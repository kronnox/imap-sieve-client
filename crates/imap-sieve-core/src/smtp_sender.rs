//! SMTP sender trait + lettre-backed implementation (Phase 9) + fake for tests.

use crate::types::CoreError;
use async_trait::async_trait;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingMail {
    pub envelope_from: String,
    pub envelope_to: Vec<String>,
    pub raw: Vec<u8>,
}

#[async_trait]
pub trait MailSender: Send + Sync {
    async fn send(&self, mail: OutgoingMail) -> Result<(), CoreError>;
}

#[cfg(any(test, feature = "test-support"))]
pub mod fake {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Default, Clone)]
    pub struct FakeSender {
        pub sent: Arc<Mutex<Vec<OutgoingMail>>>,
    }

    impl FakeSender {
        pub fn new() -> Self {
            Self::default()
        }
        pub fn sent(&self) -> Vec<OutgoingMail> {
            self.sent.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl MailSender for FakeSender {
        async fn send(&self, mail: OutgoingMail) -> Result<(), CoreError> {
            self.sent.lock().unwrap().push(mail);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;

    #[tokio::test]
    async fn fake_records_sent_mail() {
        let s = FakeSender::new();
        s.send(OutgoingMail {
            envelope_from: "a@b".into(),
            envelope_to: vec!["c@d".into()],
            raw: b"Subject: x\r\n\r\nbody".to_vec(),
        })
        .await
        .unwrap();
        assert_eq!(s.sent().len(), 1);
    }
}