//! IMAP client trait + production async-imap implementation (Phase 9) + fake for tests.

use crate::types::{CoreError, MessageContext};
use async_trait::async_trait;

/// Capability flags relevant to our processing.
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    pub idle: bool,
    pub uidplus: bool,
    pub supports_move: bool,
    pub condstore: bool,
    pub qresync: bool,
}

#[derive(Debug, Clone, Default)]
pub struct MailboxStatus {
    pub uidvalidity: u32,
    pub uidnext: u32,
    pub exists: u32,
    pub highestmodseq: Option<u64>,
}

/// Outcome of waiting in IDLE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdleEvent {
    Exists(u32),
    Interrupted,
    Disconnected,
}

#[async_trait]
pub trait ImapClient: Send + Sync {
    async fn capabilities(&mut self) -> Result<Capabilities, CoreError>;
    async fn select(&mut self, mailbox: &str) -> Result<MailboxStatus, CoreError>;
    async fn fetch_uid_range(
        &mut self,
        mailbox: &str,
        start_uid: u32,
    ) -> Result<Vec<MessageContext>, CoreError>;
    async fn uid_move(&mut self, uid: u32, target: &str) -> Result<(), CoreError>;
    async fn uid_copy(&mut self, uid: u32, target: &str) -> Result<(), CoreError>;
    async fn uid_store_flags(
        &mut self,
        uid: u32,
        op: FlagOp,
        flags: &[String],
    ) -> Result<(), CoreError>;
    async fn uid_expunge(&mut self, uids: &[u32]) -> Result<(), CoreError>;
    async fn idle(&mut self, timeout: std::time::Duration) -> Result<IdleEvent, CoreError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagOp {
    Add,
    Remove,
    Set,
}

/// Helper for fallback: implement MOVE as COPY + STORE \Deleted + (optional) EXPUNGE.
pub async fn move_with_fallback<C: ImapClient + ?Sized>(
    client: &mut C,
    caps: &Capabilities,
    uid: u32,
    target: &str,
) -> Result<(), CoreError> {
    if caps.supports_move {
        client.uid_move(uid, target).await
    } else {
        client.uid_copy(uid, target).await?;
        client
            .uid_store_flags(uid, FlagOp::Add, &["\\Deleted".into()])
            .await?;
        if caps.uidplus {
            client.uid_expunge(&[uid]).await?;
        }
        Ok(())
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod fake {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Op {
        Move(u32, String),
        Copy(u32, String),
        Store(u32, FlagOp, Vec<String>),
        Expunge(Vec<u32>),
    }

    #[derive(Default)]
    pub struct FakeImap {
        pub caps: Capabilities,
        pub status: MailboxStatus,
        pub fetch_responses: Mutex<Vec<MessageContext>>,
        pub ops: Arc<Mutex<Vec<Op>>>,
    }

    impl FakeImap {
        pub fn new() -> Self {
            Self {
                caps: Capabilities {
                    idle: true,
                    uidplus: true,
                    supports_move: true,
                    condstore: false,
                    qresync: false,
                },
                status: MailboxStatus {
                    uidvalidity: 1,
                    uidnext: 1,
                    exists: 0,
                    highestmodseq: None,
                },
                fetch_responses: Mutex::new(vec![]),
                ops: Arc::new(Mutex::new(vec![])),
            }
        }

        pub fn with_caps(mut self, caps: Capabilities) -> Self {
            self.caps = caps;
            self
        }

        pub fn ops(&self) -> Vec<Op> {
            self.ops.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ImapClient for FakeImap {
        async fn capabilities(&mut self) -> Result<Capabilities, CoreError> {
            Ok(self.caps.clone())
        }
        async fn select(&mut self, _: &str) -> Result<MailboxStatus, CoreError> {
            Ok(self.status.clone())
        }
        async fn fetch_uid_range(&mut self, _: &str, _: u32) -> Result<Vec<MessageContext>, CoreError> {
            Ok(std::mem::take(&mut *self.fetch_responses.lock().unwrap()))
        }
        async fn uid_move(&mut self, uid: u32, target: &str) -> Result<(), CoreError> {
            self.ops.lock().unwrap().push(Op::Move(uid, target.into()));
            Ok(())
        }
        async fn uid_copy(&mut self, uid: u32, target: &str) -> Result<(), CoreError> {
            self.ops.lock().unwrap().push(Op::Copy(uid, target.into()));
            Ok(())
        }
        async fn uid_store_flags(
            &mut self,
            uid: u32,
            op: FlagOp,
            flags: &[String],
        ) -> Result<(), CoreError> {
            self.ops.lock().unwrap().push(Op::Store(uid, op, flags.to_vec()));
            Ok(())
        }
        async fn uid_expunge(&mut self, uids: &[u32]) -> Result<(), CoreError> {
            self.ops.lock().unwrap().push(Op::Expunge(uids.to_vec()));
            Ok(())
        }
        async fn idle(&mut self, _: std::time::Duration) -> Result<IdleEvent, CoreError> {
            // Yield to the runtime so that other tasks (e.g. shutdown
            // notifications) get a chance to run. A real IDLE command
            // would block on network I/O and naturally yield.
            tokio::task::yield_now().await;
            Ok(IdleEvent::Interrupted)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;

    #[tokio::test]
    async fn move_with_fallback_uses_native_move_when_available() {
        let caps = Capabilities { supports_move: true, uidplus: true, idle: true, ..Default::default() };
        let mut fake = FakeImap::new().with_caps(caps.clone());
        move_with_fallback(&mut fake, &caps, 5, "Junk").await.unwrap();
        assert_eq!(fake.ops(), vec![Op::Move(5, "Junk".into())]);
    }

    #[tokio::test]
    async fn move_with_fallback_falls_back_to_copy_store_expunge() {
        let caps = Capabilities { supports_move: false, uidplus: true, idle: true, ..Default::default() };
        let mut fake = FakeImap::new().with_caps(caps.clone());
        move_with_fallback(&mut fake, &caps, 5, "Junk").await.unwrap();
        assert_eq!(
            fake.ops(),
            vec![
                Op::Copy(5, "Junk".into()),
                Op::Store(5, FlagOp::Add, vec!["\\Deleted".into()]),
                Op::Expunge(vec![5]),
            ]
        );
    }

    #[tokio::test]
    async fn move_with_fallback_skips_expunge_without_uidplus() {
        let caps = Capabilities { supports_move: false, uidplus: false, idle: true, ..Default::default() };
        let mut fake = FakeImap::new().with_caps(caps.clone());
        move_with_fallback(&mut fake, &caps, 5, "Junk").await.unwrap();
        assert_eq!(
            fake.ops(),
            vec![
                Op::Copy(5, "Junk".into()),
                Op::Store(5, FlagOp::Add, vec!["\\Deleted".into()]),
            ]
        );
    }
}