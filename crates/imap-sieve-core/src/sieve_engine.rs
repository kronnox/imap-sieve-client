//! Sieve engine abstraction and `sieve-rs` implementation.

use crate::types::{MessageContext, SieveAction};
use std::sync::Arc;
use thiserror::Error;

// The crate on crates.io is called `sieve-rs`, but the Rust library name is `sieve`.
use sieve::{Compiler, Envelope, Event, Input, Runtime, Sieve};

#[derive(Debug, Error)]
pub enum SieveError {
    #[error("compile error: {0}")]
    Compile(String),
    #[error("runtime error: {0}")]
    Runtime(String),
}

/// Compiled sieve script. Opaque to consumers; only `SieveEngine::evaluate` reads it.
///
/// Holds the bytecode in an `Arc` so hot-reload swaps are cheap. We never
/// clone the inner value — `as_sieve()` returns a reference.
#[derive(Clone)]
pub struct CompiledScript {
    inner: Arc<Sieve>,
}

impl std::fmt::Debug for CompiledScript {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledScript").finish_non_exhaustive()
    }
}

impl CompiledScript {
    pub fn as_sieve(&self) -> &Sieve {
        &self.inner
    }
}

pub trait SieveEngine: Send + Sync {
    fn compile(&self, script: &str) -> Result<CompiledScript, SieveError>;
    fn evaluate(
        &self,
        script: &CompiledScript,
        context: &MessageContext,
    ) -> Result<Vec<SieveAction>, SieveError>;
}

/// Production engine wrapping the `sieve-rs` crate.
///
/// `Runtime` is configuration-only (it does not hold per-evaluation state) so
/// `&self` is sufficient. If a future version requires `&mut self`, wrap it
/// in `Mutex` rather than changing the trait.
pub struct SieveEngineImpl {
    compiler: Compiler,
    runtime: Runtime,
}

impl SieveEngineImpl {
    pub fn new() -> Self {
        Self {
            compiler: Compiler::new(),
            runtime: Runtime::new(),
        }
    }
}

impl Default for SieveEngineImpl {
    fn default() -> Self {
        Self::new()
    }
}

impl SieveEngine for SieveEngineImpl {
    fn compile(&self, script: &str) -> Result<CompiledScript, SieveError> {
        let bytecode = self
            .compiler
            .compile(script.as_bytes())
            .map_err(|e| SieveError::Compile(format!("{e:?}")))?;
        Ok(CompiledScript {
            inner: Arc::new(bytecode),
        })
    }

    fn evaluate(
        &self,
        script: &CompiledScript,
        context: &MessageContext,
    ) -> Result<Vec<SieveAction>, SieveError> {
        let raw = context
            .raw
            .as_deref()
            .ok_or_else(|| SieveError::Runtime("MessageContext.raw is required".into()))?;

        let mut instance = self.runtime.filter(raw);
        if let Some(from) = &context.envelope_from {
            instance.set_envelope(Envelope::From, from.as_str());
        }
        for to in &context.envelope_to {
            instance.set_envelope(Envelope::To, to.as_str());
        }

        let mut actions = Vec::new();
        // Bootstrap with the compiled script, then thread Input::True through
        // subsequent iterations unless an event demands a specific response.
        let mut next_input: Input = Input::script("main", script.as_sieve().clone());

        loop {
            let event = match instance.run(next_input) {
                Some(Ok(ev)) => ev,
                Some(Err(e)) => {
                    return Err(SieveError::Runtime(format!("{e:?}")));
                }
                None => break,
            };

            next_input = match event {
                Event::IncludeScript { .. } => {
                    // Include not supported — skip and continue.
                    Input::True
                }
                Event::Keep { flags, .. } => {
                    if !flags.is_empty() {
                        actions.push(SieveAction::SetFlag { flags });
                    }
                    actions.push(SieveAction::Keep);
                    Input::True
                }
                Event::Discard => {
                    actions.push(SieveAction::Discard);
                    Input::True
                }
                Event::FileInto {
                    folder,
                    flags,
                    create,
                    ..
                } => {
                    if !flags.is_empty() {
                        actions.push(SieveAction::SetFlag { flags });
                    }
                    actions.push(SieveAction::FileInto {
                        mailbox: folder,
                        copy: false, // :copy is handled by sieve-rs emitting Keep alongside FileInto
                        create,
                    });
                    Input::True
                }
                Event::SendMessage { recipient, .. } => {
                    match recipient {
                        sieve::Recipient::Address(addr) => {
                            actions.push(SieveAction::Redirect {
                                addresses: vec![addr],
                            });
                        }
                        _ => {
                            tracing::warn!("unsupported recipient type in redirect; skipping");
                        }
                    }
                    Input::True
                }
                Event::Reject { reason, .. } => {
                    actions.push(SieveAction::Reject { reason });
                    Input::True
                }
                Event::Function { id, arguments } => {
                    let name = id.to_string();
                    let args: Vec<String> = arguments
                        .iter()
                        .map(|v| v.to_string().into_owned())
                        .collect();
                    actions.push(SieveAction::Execute { name, args });
                    Input::True
                }
                _ => Input::True,
            };
        }

        // RFC 5228 implicit keep: if no disposition action, leave message in place.
        let touched = actions.iter().any(|a| {
            matches!(
                a,
                SieveAction::Keep
                    | SieveAction::Discard
                    | SieveAction::FileInto { .. }
                    | SieveAction::Redirect { .. }
                    | SieveAction::Reject { .. }
            )
        });
        if !touched {
            actions.push(SieveAction::Keep);
        }

        // :copy detection: sieve-rs conveys `:copy` through the implicit Keep
        // event. A non-:copy fileinto clears final_event; :copy preserves it.
        // When both FileInto and Keep are present, every FileInto must have
        // preserved the Keep (either via :copy or because an explicit `keep;`
        // restored it). In both cases copy=true is correct: the message should
        // stay in INBOX (COPY instead of MOVE). The redundant Keep is removed
        // since the original stays via the COPY.
        //
        // Known limitation: if a non-:copy fileinto runs BEFORE a :copy one,
        // the Keep is cleared and never restored — sieve-rs emits no Keep, so
        // the :copy info is lost. This is a sieve-rs API limitation (Event::FileInto
        // has no `copy` field). The mixed case is uncommon in practice.
        let has_fileinto = actions
            .iter()
            .any(|a| matches!(a, SieveAction::FileInto { .. }));
        let has_keep = actions.iter().any(|a| matches!(a, SieveAction::Keep));
        if has_fileinto && has_keep {
            for action in actions.iter_mut() {
                if let SieveAction::FileInto { copy, .. } = action {
                    *copy = true;
                }
            }
            actions.retain(|a| !matches!(a, SieveAction::Keep));
        }

        Ok(actions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MessageContext;
    use std::collections::BTreeMap;

    fn ctx(subject: &str) -> MessageContext {
        let raw = format!(
            "Subject: {subject}\r\nFrom: alice@example.com\r\nTo: bob@example.com\r\n\r\nbody"
        );
        let mut headers = BTreeMap::new();
        headers.insert("subject".into(), subject.into());
        headers.insert("from".into(), "alice@example.com".into());
        headers.insert("to".into(), "bob@example.com".into());
        MessageContext {
            uid: 1,
            mailbox: "INBOX".into(),
            headers,
            envelope_from: Some("alice@example.com".into()),
            envelope_to: vec!["bob@example.com".into()],
            raw: Some(raw.clone().into_bytes()),
            flags: vec![],
            size: raw.len() as u32,
        }
    }

    #[test]
    fn keep_when_no_match() {
        let engine = SieveEngineImpl::new();
        let script = engine.compile(r#"keep;"#).expect("compile");
        let actions = engine.evaluate(&script, &ctx("anything")).expect("eval");
        assert!(actions.contains(&SieveAction::Keep), "got {:?}", actions);
    }

    #[test]
    fn fileinto_on_subject_match() {
        let engine = SieveEngineImpl::new();
        let script = r#"
require "fileinto";
if header :contains "Subject" "spam" {
    fileinto "Junk";
}
"#;
        let script = engine.compile(script).expect("compile");
        let actions = engine
            .evaluate(&script, &ctx("buy spam now"))
            .expect("eval");
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, SieveAction::FileInto { mailbox, .. } if mailbox == "Junk")),
            "got {:?}",
            actions
        );
    }

    #[test]
    fn discard_action_returned() {
        let engine = SieveEngineImpl::new();
        let script = r#"discard;"#;
        let compiled = engine.compile(script).expect("compile");
        let actions = engine.evaluate(&compiled, &ctx("x")).expect("eval");
        assert!(actions.contains(&SieveAction::Discard), "got {:?}", actions);
    }

    #[test]
    fn invalid_script_returns_compile_error() {
        let engine = SieveEngineImpl::new();
        let err = engine.compile("this is not sieve").unwrap_err();
        assert!(matches!(err, SieveError::Compile(_)));
    }

    #[test]
    fn fileinto_copy_sets_copy_flag() {
        let engine = SieveEngineImpl::new();
        let script = r#"
require "fileinto";
require "copy";
fileinto :copy "Archive";
"#;
        let script = engine.compile(script).expect("compile");
        let actions = engine.evaluate(&script, &ctx("anything")).expect("eval");
        // :copy produces FileInto with copy=true; the implicit Keep is stripped
        // since copy semantics mean the original stays anyway.
        let fileinto = actions
            .iter()
            .find(|a| matches!(a, SieveAction::FileInto { .. }));
        assert!(fileinto.is_some(), "got {:?}", actions);
        match fileinto.unwrap() {
            SieveAction::FileInto { copy, .. } => {
                assert!(copy, "copy should be true for :copy");
            }
            _ => panic!("expected FileInto"),
        }
        // No Keep should be present (it was stripped as redundant with copy=true).
        assert!(
            !actions.iter().any(|a| matches!(a, SieveAction::Keep)),
            "got {:?}",
            actions
        );
    }

    #[test]
    fn fileinto_create_sets_create_flag() {
        let engine = SieveEngineImpl::new();
        let script = r#"
require "fileinto";
require "mailbox";
fileinto :create "NewFolder";
"#;
        let script = engine.compile(script).expect("compile");
        let actions = engine.evaluate(&script, &ctx("anything")).expect("eval");
        let fileinto = actions
            .iter()
            .find(|a| matches!(a, SieveAction::FileInto { .. }));
        assert!(fileinto.is_some(), "got {:?}", actions);
        match fileinto.unwrap() {
            SieveAction::FileInto { create, .. } => {
                assert!(create, "create should be true for :create");
            }
            _ => panic!("expected FileInto"),
        }
    }

    #[test]
    fn imap4flags_set_on_keep() {
        let engine = SieveEngineImpl::new();
        let script = r#"
require "imap4flags";
addflag "\\Seen";
keep;
"#;
        let script = engine.compile(script).expect("compile");
        let actions = engine.evaluate(&script, &ctx("anything")).expect("eval");
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, SieveAction::SetFlag { flags } if flags.contains(&"\\Seen".to_string()))),
            "expected SetFlag with \\Seen, got {:?}",
            actions
        );
        assert!(actions.iter().any(|a| matches!(a, SieveAction::Keep)));
    }

    #[test]
    fn imap4flags_set_on_fileinto() {
        let engine = SieveEngineImpl::new();
        let script = r#"
require "fileinto";
require "imap4flags";
addflag "\\Flagged";
fileinto "Important";
"#;
        let script = engine.compile(script).expect("compile");
        let actions = engine.evaluate(&script, &ctx("anything")).expect("eval");
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, SieveAction::SetFlag { flags } if flags.contains(&"\\Flagged".to_string()))),
            "expected SetFlag with \\Flagged, got {:?}",
            actions
        );
        let fileinto = actions
            .iter()
            .find(|a| matches!(a, SieveAction::FileInto { mailbox, .. } if mailbox == "Important"));
        assert!(fileinto.is_some(), "got {:?}", actions);
    }

    /// Known limitation: when a non-:copy fileinto runs before a :copy one,
    /// sieve-rs clears the implicit Keep, so our heuristic cannot detect the
    /// :copy modifier. This is a sieve-rs API limitation — Event::FileInto has
    /// no `copy` field. Both FileInto actions get copy=false, which is wrong
    /// for "Archive" (should be copy=true). This case is uncommon in practice.
    #[test]
    fn mixed_copy_fileinto_known_limitation() {
        let engine = SieveEngineImpl::new();
        let script = r#"
require "fileinto";
require "copy";
fileinto "Log";
fileinto :copy "Archive";
"#;
        let script = engine.compile(script).expect("compile");
        let actions = engine.evaluate(&script, &ctx("anything")).expect("eval");
        // sieve-rs emits FileInto("Log") + FileInto("Archive") with NO Keep,
        // because the non-:copy "Log" fileinto clears final_event before
        // the :copy "Archive" fileinto runs.
        let log = actions
            .iter()
            .find(|a| matches!(a, SieveAction::FileInto { mailbox, .. } if mailbox == "Log"));
        let archive = actions
            .iter()
            .find(|a| matches!(a, SieveAction::FileInto { mailbox, .. } if mailbox == "Archive"));
        assert!(log.is_some(), "Log fileinto missing, got {:?}", actions);
        assert!(
            archive.is_some(),
            "Archive fileinto missing, got {:?}",
            actions
        );
        // Log correctly gets copy=false
        if let Some(SieveAction::FileInto { copy, .. }) = log {
            assert!(!copy, "Log should have copy=false");
        }
        // Archive gets copy=false due to the limitation — ideally copy=true,
        // but sieve-rs provides no way to recover this information.
        if let Some(SieveAction::FileInto { copy, .. }) = archive {
            assert!(!copy, "Archive gets copy=false (known limitation)");
        }
        // No Keep present since it was cleared
        assert!(
            !actions.iter().any(|a| matches!(a, SieveAction::Keep)),
            "no Keep expected in mixed case, got {:?}",
            actions
        );
    }

    #[test]
    fn reject_action_returned() {
        let engine = SieveEngineImpl::new();
        let script = r#"
require "reject";
reject "message not accepted";
"#;
        let script = engine.compile(script).expect("compile");
        let actions = engine.evaluate(&script, &ctx("anything")).expect("eval");
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, SieveAction::Reject { .. })),
            "got {:?}",
            actions
        );
        // Reject should NOT produce a spurious implicit Keep.
        assert!(
            !actions.iter().any(|a| matches!(a, SieveAction::Keep)),
            "got {:?}",
            actions
        );
    }
}
