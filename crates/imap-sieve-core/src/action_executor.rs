//! Translate SieveAction into IMAP/SMTP operations.

use crate::imap_client::{move_with_fallback, Capabilities, FlagOp, ImapClient};
use crate::smtp_sender::{MailSender, OutgoingMail};
use crate::types::{CoreError, MessageContext, SieveAction};

/// Two lifetimes: `'i` for the per-iteration `&mut ImapClient` borrow,
/// `'s` for the longer-lived shared refs (smtp/caps/mailbox).
pub struct ActionExecutor<'i, 's, C: ImapClient + ?Sized, S: MailSender + ?Sized> {
    pub imap: &'i mut C,
    pub smtp: Option<&'s S>,
    pub caps: &'s Capabilities,
    pub source_mailbox: &'s str,
}

impl<'i, 's, C: ImapClient + ?Sized, S: MailSender + ?Sized> ActionExecutor<'i, 's, C, S> {
    pub async fn execute(
        &mut self,
        ctx: &MessageContext,
        actions: &[SieveAction],
    ) -> Result<(), CoreError> {
        let mut filed = false;
        let mut discarded = false;

        // Process flag and execute actions first (they don't affect disposition).
        for action in actions {
            match action {
                SieveAction::AddFlag { flags } => {
                    self.imap.uid_store_flags(ctx.uid, FlagOp::Add, flags).await?;
                }
                SieveAction::RemoveFlag { flags } => {
                    self.imap.uid_store_flags(ctx.uid, FlagOp::Remove, flags).await?;
                }
                SieveAction::SetFlag { flags } => {
                    self.imap.uid_store_flags(ctx.uid, FlagOp::Set, flags).await?;
                }
                SieveAction::Execute { name, args } => {
                    tracing::info!(action = %name, args = ?args, uid = ctx.uid, "execute action (no handler registered)");
                }
                _ => {}
            }
        }

        // Process disposition actions.
        for action in actions {
            match action {
                SieveAction::FileInto { mailbox, copy } => {
                    if *copy {
                        self.imap.uid_copy(ctx.uid, mailbox).await?;
                    } else {
                        move_with_fallback(self.imap, self.caps, ctx.uid, mailbox).await?;
                        filed = true;
                    }
                }
                SieveAction::Discard => {
                    discarded = true;
                }
                SieveAction::Redirect { addresses } => {
                    let smtp = self.smtp.ok_or_else(|| {
                        CoreError::Smtp("redirect action requires SMTP configuration".into())
                    })?;
                    let raw = ctx.raw.as_deref().ok_or_else(|| {
                        CoreError::Smtp("redirect requires raw message body".into())
                    })?;
                    smtp.send(OutgoingMail {
                        envelope_from: ctx
                            .envelope_from
                            .clone()
                            .unwrap_or_else(|| "<>".into()),
                        envelope_to: addresses.clone(),
                        raw: raw.to_vec(),
                    })
                    .await?;
                }
                SieveAction::Reject { reason } => {
                    let smtp = self.smtp.ok_or_else(|| {
                        CoreError::Smtp("reject action requires SMTP configuration".into())
                    })?;
                    let to = ctx
                        .envelope_from
                        .as_deref()
                        .ok_or_else(|| CoreError::Smtp("reject requires envelope-from".into()))?;
                    let body = format!(
                        "Subject: Mail rejected\r\nTo: {to}\r\n\r\n{reason}\r\n"
                    );
                    smtp.send(OutgoingMail {
                        envelope_from: String::new(),
                        envelope_to: vec![to.into()],
                        raw: body.into_bytes(),
                    })
                    .await?;
                    discarded = true;
                }
                _ => {}
            }
        }

        if discarded {
            self.imap
                .uid_store_flags(ctx.uid, FlagOp::Add, &["\\Deleted".into()])
                .await?;
            if self.caps.uidplus {
                self.imap.uid_expunge(&[ctx.uid]).await?;
            }
        }

        let _ = filed;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imap_client::fake::{FakeImap, Op};
    use crate::smtp_sender::fake::FakeSender;
    use crate::types::MessageContext;
    use std::collections::BTreeMap;

    fn ctx(uid: u32) -> MessageContext {
        let mut headers = BTreeMap::new();
        headers.insert("subject".into(), "x".into());
        MessageContext {
            uid,
            mailbox: "INBOX".into(),
            headers,
            envelope_from: Some("a@b.com".into()),
            envelope_to: vec!["b@c.com".into()],
            raw: Some(b"Subject: x\r\n\r\nbody".to_vec()),
            flags: vec![],
            size: 16,
        }
    }

    #[tokio::test]
    async fn keep_action_is_a_noop() {
        let mut imap = FakeImap::new();
        let smtp = FakeSender::new();
        let caps = imap.caps.clone();
        let mut exec = ActionExecutor {
            imap: &mut imap,
            smtp: Some(&smtp),
            caps: &caps,
            source_mailbox: "INBOX",
        };
        exec.execute(&ctx(1), &[SieveAction::Keep]).await.unwrap();
        assert!(imap.ops().is_empty());
        assert!(smtp.sent().is_empty());
    }

    #[tokio::test]
    async fn fileinto_uses_move() {
        let mut imap = FakeImap::new();
        let smtp = FakeSender::new();
        let caps = imap.caps.clone();
        let mut exec = ActionExecutor {
            imap: &mut imap,
            smtp: Some(&smtp),
            caps: &caps,
            source_mailbox: "INBOX",
        };
        exec.execute(
            &ctx(7),
            &[SieveAction::FileInto { mailbox: "Junk".into(), copy: false }],
        )
        .await
        .unwrap();
        assert_eq!(imap.ops(), vec![Op::Move(7, "Junk".into())]);
    }

    #[tokio::test]
    async fn discard_marks_deleted_and_expunges() {
        let mut imap = FakeImap::new();
        let smtp = FakeSender::new();
        let caps = imap.caps.clone();
        let mut exec = ActionExecutor {
            imap: &mut imap,
            smtp: Some(&smtp),
            caps: &caps,
            source_mailbox: "INBOX",
        };
        exec.execute(&ctx(9), &[SieveAction::Discard]).await.unwrap();
        assert_eq!(
            imap.ops(),
            vec![
                Op::Store(9, FlagOp::Add, vec!["\\Deleted".into()]),
                Op::Expunge(vec![9]),
            ]
        );
    }

    #[tokio::test]
    async fn redirect_calls_smtp() {
        let mut imap = FakeImap::new();
        let smtp = FakeSender::new();
        let caps = imap.caps.clone();
        let mut exec = ActionExecutor {
            imap: &mut imap,
            smtp: Some(&smtp),
            caps: &caps,
            source_mailbox: "INBOX",
        };
        exec.execute(
            &ctx(2),
            &[SieveAction::Redirect { addresses: vec!["dest@x.com".into()] }],
        )
        .await
        .unwrap();
        assert_eq!(smtp.sent().len(), 1);
        assert_eq!(smtp.sent()[0].envelope_to, vec!["dest@x.com".to_string()]);
    }

    #[tokio::test]
    async fn redirect_without_smtp_errors() {
        let mut imap = FakeImap::new();
        let caps = imap.caps.clone();
        let mut exec: ActionExecutor<'_, '_, _, crate::smtp_sender::fake::FakeSender> =
            ActionExecutor {
                imap: &mut imap,
                smtp: None,
                caps: &caps,
                source_mailbox: "INBOX",
            };
        let err = exec
            .execute(
                &ctx(3),
                &[SieveAction::Redirect { addresses: vec!["x@y".into()] }],
            )
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::Smtp(_)));
    }

    #[tokio::test]
    async fn add_flag_issues_store() {
        let mut imap = FakeImap::new();
        let smtp = FakeSender::new();
        let caps = imap.caps.clone();
        let mut exec = ActionExecutor {
            imap: &mut imap,
            smtp: Some(&smtp),
            caps: &caps,
            source_mailbox: "INBOX",
        };
        exec.execute(
            &ctx(4),
            &[SieveAction::AddFlag { flags: vec!["\\Seen".into()] }],
        )
        .await
        .unwrap();
        assert_eq!(
            imap.ops(),
            vec![Op::Store(4, FlagOp::Add, vec!["\\Seen".into()])]
        );
    }
}