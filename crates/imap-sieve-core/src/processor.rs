//! Per-batch message processing pipeline.

use crate::action_executor::ActionExecutor;
use crate::imap_client::{Capabilities, ImapClient};
use crate::script_loader::ScriptHandle;
use crate::sieve_engine::SieveEngine;
use crate::smtp_sender::MailSender;
use crate::state::StateStore;
use crate::types::{CoreError, ProcessingResult, ProcessingStatus, SieveAction};

/// Lifetimes: `'i` for per-iteration `&mut` borrows, `'s` for shared refs.
pub struct MessageProcessor<'i, 's, E: SieveEngine, C: ImapClient + ?Sized, S: MailSender + ?Sized> {
    pub engine: &'s E,
    pub script: &'s ScriptHandle,
    pub imap: &'i mut C,
    pub smtp: Option<&'s S>,
    pub caps: &'s Capabilities,
    pub state: &'i mut StateStore,
    pub mailbox: &'s str,
}

impl<'i, 's, E: SieveEngine, C: ImapClient + ?Sized, S: MailSender + ?Sized>
    MessageProcessor<'i, 's, E, C, S>
{
    /// Fetch all UIDs newer than `state.last_seen_uid` and process each one.
    ///
    /// State update policy (at-least-once with operator-visible failures):
    /// - On `ProcessingStatus::Ok`, advance `last_seen_uid` to this message's UID.
    /// - On `SieveError` or `ActionError`, **do not advance** `last_seen_uid` —
    ///   the message will be re-processed on the next batch (or after restart).
    /// - If `last_seen_uid` is `None`, returns an error — the SessionManager
    ///   must seed it from UIDNEXT-1 on first SELECT.
    pub async fn run_batch(&mut self) -> Result<Vec<ProcessingResult>, CoreError> {
        let start = match self.state.state().last_seen_uid {
            Some(uid) => uid + 1,
            None => {
                return Err(CoreError::Imap(
                    "last_seen_uid not initialized; SessionManager must seed it from UIDNEXT".into(),
                ));
            }
        };
        let messages = self.imap.fetch_uid_range(self.mailbox, start).await?;
        let script = self.script.current();
        let mut results = Vec::with_capacity(messages.len());

        for msg in messages {
            let actions_result = self.engine.evaluate(&script, &msg);
            let (actions, status) = match actions_result {
                Ok(a) => (a, ProcessingStatus::Ok),
                Err(e) => {
                    tracing::error!(uid = msg.uid, error = %e, "sieve evaluation failed; falling back to keep");
                    (vec![SieveAction::Keep], ProcessingStatus::SieveError(e.to_string()))
                }
            };

            let mut exec = ActionExecutor {
                imap: self.imap,
                smtp: self.smtp,
                caps: self.caps,
                source_mailbox: self.mailbox,
            };
            let final_status = match exec.execute(&msg, &actions).await {
                Ok(()) => status,
                Err(e) => {
                    tracing::error!(uid = msg.uid, error = %e, "action execution failed");
                    ProcessingStatus::ActionError(e.to_string())
                }
            };

            // Only advance `last_seen_uid` on successful processing.
            if matches!(final_status, ProcessingStatus::Ok) {
                self.state.update(|s| {
                    s.last_seen_uid = Some(msg.uid.max(s.last_seen_uid.unwrap_or(0)));
                })?;
            }

            results.push(ProcessingResult { uid: msg.uid, actions, status: final_status });
        }

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imap_client::fake::{FakeImap, Op};
    use crate::script_loader::ScriptLoader;
    use crate::sieve_engine::SieveEngineImpl;
    use crate::smtp_sender::fake::FakeSender;
    use crate::types::MessageContext;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn msg(uid: u32, subject: &str) -> MessageContext {
        let mut headers = BTreeMap::new();
        headers.insert("subject".into(), subject.into());
        MessageContext {
            uid,
            mailbox: "INBOX".into(),
            headers,
            envelope_from: Some("a@b".into()),
            envelope_to: vec!["c@d".into()],
            raw: Some(format!("Subject: {subject}\r\n\r\nbody").into_bytes()),
            flags: vec![],
            size: 0,
        }
    }

    #[tokio::test]
    async fn processes_batch_and_updates_state() {
        let dir = TempDir::new().unwrap();
        let script_path = dir.path().join("rules.sieve");
        std::fs::write(
            &script_path,
            "require \"fileinto\";\nif header :contains \"Subject\" \"spam\" { fileinto \"Junk\"; }",
        )
        .unwrap();
        let engine = SieveEngineImpl::new();
        let (_loader, handle) = ScriptLoader::load(SieveEngineImpl::new(), &script_path).unwrap();

        let mut imap = FakeImap::new();
        *imap.fetch_responses.lock().unwrap() = vec![msg(10, "buy spam"), msg(11, "hello")];
        let smtp = FakeSender::new();
        let caps = imap.caps.clone();
        let mut state = StateStore::open(dir.path().join("state.json")).unwrap();
        state.update(|s| s.last_seen_uid = Some(9)).unwrap();

        let mut processor = MessageProcessor {
            engine: &engine,
            script: &handle,
            imap: &mut imap,
            smtp: Some(&smtp),
            caps: &caps,
            state: &mut state,
            mailbox: "INBOX",
        };
        let results = processor.run_batch().await.unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(state.state().last_seen_uid, Some(11));
        assert!(imap.ops().contains(&Op::Move(10, "Junk".into())));
        assert_eq!(imap.ops().len(), 1);
    }

    #[tokio::test]
    async fn sieve_error_falls_back_to_keep_without_advancing_state() {
        let dir = TempDir::new().unwrap();
        let script_path = dir.path().join("rules.sieve");
        std::fs::write(&script_path, "keep;").unwrap();
        let engine = SieveEngineImpl::new();
        let (_l, handle) = ScriptLoader::load(SieveEngineImpl::new(), &script_path).unwrap();

        // Message with no raw body — engine will error
        let mut bad = msg(1, "x");
        bad.raw = None;
        let mut imap = FakeImap::new();
        *imap.fetch_responses.lock().unwrap() = vec![bad];
        let smtp = FakeSender::new();
        let caps = imap.caps.clone();
        let mut state = StateStore::open(dir.path().join("state.json")).unwrap();
        state.update(|s| s.last_seen_uid = Some(0)).unwrap();

        let mut processor = MessageProcessor {
            engine: &engine,
            script: &handle,
            imap: &mut imap,
            smtp: Some(&smtp),
            caps: &caps,
            state: &mut state,
            mailbox: "INBOX",
        };
        let results = processor.run_batch().await.unwrap();
        assert!(matches!(results[0].status, ProcessingStatus::SieveError(_)));
        // last_seen_uid stays at 0 — failed message will be retried
        assert_eq!(state.state().last_seen_uid, Some(0));
    }

    #[tokio::test]
    async fn uninitialized_state_returns_error() {
        let dir = TempDir::new().unwrap();
        let script_path = dir.path().join("rules.sieve");
        std::fs::write(&script_path, "keep;").unwrap();
        let engine = SieveEngineImpl::new();
        let (_l, handle) = ScriptLoader::load(SieveEngineImpl::new(), &script_path).unwrap();
        let mut imap = FakeImap::new();
        let smtp = FakeSender::new();
        let caps = imap.caps.clone();
        let mut state = StateStore::open(dir.path().join("state.json")).unwrap();
        // Intentionally do NOT seed last_seen_uid.

        let mut processor = MessageProcessor {
            engine: &engine,
            script: &handle,
            imap: &mut imap,
            smtp: Some(&smtp),
            caps: &caps,
            state: &mut state,
            mailbox: "INBOX",
        };
        let err = processor.run_batch().await.unwrap_err();
        assert!(matches!(err, CoreError::Imap(_)));
    }
}