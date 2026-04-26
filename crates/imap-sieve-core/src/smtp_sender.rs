//! SMTP sender trait + lettre-backed implementation (Phase 9) + fake for tests.

use crate::types::CoreError;
use async_trait::async_trait;
use lettre::{transport::smtp::authentication::Credentials, AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

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

// ---------------------------------------------------------------------------
// LettreMailSender – production SMTP adapter
// ---------------------------------------------------------------------------

pub struct LettreMailSender {
    transport: AsyncSmtpTransport<Tokio1Executor>,
}

impl LettreMailSender {
    pub fn new(
        host: &str,
        port: u16,
        username: &str,
        password: &str,
        starttls: bool,
    ) -> Result<Self, CoreError> {
        let builder = if starttls {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host)
                .map_err(|e| CoreError::Smtp(e.to_string()))?
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::relay(host)
                .map_err(|e| CoreError::Smtp(e.to_string()))?
        };
        let transport = builder
            .port(port)
            .credentials(Credentials::new(username.into(), password.into()))
            .build();
        Ok(Self { transport })
    }
}

#[async_trait]
impl MailSender for LettreMailSender {
    async fn send(&self, mail: OutgoingMail) -> Result<(), CoreError> {
        let from = if mail.envelope_from.is_empty() {
            None
        } else {
            Some(
                mail.envelope_from
                    .parse()
                    .map_err(|e: lettre::address::AddressError| CoreError::Smtp(e.to_string()))?,
            )
        };
        let to = mail
            .envelope_to
            .iter()
            .map(|s| {
                s.parse::<lettre::Address>()
                    .map_err(|e: lettre::address::AddressError| CoreError::Smtp(e.to_string()))
            })
            .collect::<Result<Vec<_>, CoreError>>()?;
        let envelope = lettre::address::Envelope::new(from, to)
            .map_err(|e| CoreError::Smtp(e.to_string()))?;
        self.transport
            .send_raw(&envelope, &mail.raw)
            .await
            .map_err(|e| CoreError::Smtp(e.to_string()))?;
        Ok(())
    }
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