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
        let mut next_input: Input =
            Input::script("main", script.as_sieve().clone());

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
                Event::Keep { .. } => {
                    actions.push(SieveAction::Keep);
                    Input::True
                }
                Event::Discard => {
                    actions.push(SieveAction::Discard);
                    Input::True
                }
                Event::FileInto {
                    folder, create, ..
                } => {
                    actions.push(SieveAction::FileInto {
                        mailbox: folder,
                        copy: create,
                    });
                    Input::True
                }
                Event::SendMessage { recipient, .. } => {
                    let addr = match recipient {
                        sieve::Recipient::Address(addr) => addr,
                        _ => String::new(),
                    };
                    actions.push(SieveAction::Redirect {
                        addresses: vec![addr],
                    });
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
            )
        });
        if !touched {
            actions.push(SieveAction::Keep);
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
        let raw = format!("Subject: {subject}\r\nFrom: alice@example.com\r\nTo: bob@example.com\r\n\r\nbody");
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
        let actions = engine.evaluate(&script, &ctx("buy spam now")).expect("eval");
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
        assert!(
            actions.contains(&SieveAction::Discard),
            "got {:?}",
            actions
        );
    }

    #[test]
    fn invalid_script_returns_compile_error() {
        let engine = SieveEngineImpl::new();
        let err = engine.compile("this is not sieve").unwrap_err();
        assert!(matches!(err, SieveError::Compile(_)));
    }
}