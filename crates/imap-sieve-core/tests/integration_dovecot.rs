//! Integration tests against a Dovecot server. Skipped unless
//! `DOVECOT_TEST_HOST` is set.

use imap_sieve_core::imap_client::{AsyncImapClient, ImapClient};
use imap_sieve_core::smtp_sender::{MailSender, OutgoingMail};
use imap_sieve_core::types::CoreError;
use async_trait::async_trait;

/// A no-op mail sender for integration tests that don't need SMTP.
struct NopMailSender;

#[async_trait]
impl MailSender for NopMailSender {
    async fn send(&self, _mail: OutgoingMail) -> Result<(), CoreError> {
        Ok(())
    }
}

fn dovecot_host() -> Option<(String, u16)> {
    let host = std::env::var("DOVECOT_TEST_HOST").ok()?;
    let port = std::env::var("DOVECOT_TEST_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(143);
    Some((host, port))
}

#[tokio::test]
#[ignore = "requires Dovecot running locally"]
async fn login_and_select_inbox() {
    let Some((host, port)) = dovecot_host() else {
        return;
    };
    let mut client = AsyncImapClient::connect(&host, port, "testuser", "testpass", false)
        .await
        .expect("connect");
    let caps = client.capabilities().await.expect("capabilities");
    assert!(caps.idle, "Dovecot should advertise IDLE");
    let status = client.select("INBOX").await.expect("select");
    assert!(status.uidvalidity > 0);
}

#[tokio::test]
#[ignore = "requires Dovecot running locally"]
async fn fileinto_action_moves_message() {
    use imap_sieve_core::processor::MessageProcessor;
    use imap_sieve_core::script_loader::ScriptLoader;
    use imap_sieve_core::sieve_engine::SieveEngineImpl;
    use imap_sieve_core::state::StateStore;
    use tempfile::TempDir;

    let Some((host, port)) = dovecot_host() else {
        return;
    };

    // 1. Append a test message via a fresh client
    let mut client = AsyncImapClient::connect(&host, port, "testuser", "testpass", false)
        .await
        .unwrap();
    // Ensure the Junk folder exists (ignore error if it already does):
    let _ = client.session_create_mailbox("Junk").await;
    let test_msg = b"From: a@b.com\r\nTo: c@d.com\r\nSubject: integration spam\r\n\r\nbody\r\n";
    client.session_append("INBOX", test_msg).await.unwrap();

    // 2. Set up engine + processor
    let dir = TempDir::new().unwrap();
    let script_path = dir.path().join("rules.sieve");
    std::fs::write(
        &script_path,
        "require \"fileinto\";\nif header :contains \"Subject\" \"integration spam\" { fileinto \"Junk\"; }",
    )
    .unwrap();
    let engine = SieveEngineImpl::new();
    let (_loader, script) = ScriptLoader::load(SieveEngineImpl::new(), &script_path).unwrap();

    let mut state = StateStore::open(dir.path().join("state.json")).unwrap();
    let caps = client.capabilities().await.unwrap();
    let status = client.select("INBOX").await.unwrap();
    state
        .update(|s| {
            s.uidvalidity = Some(status.uidvalidity);
            s.selected_mailbox = Some("INBOX".into());
        })
        .unwrap();

    let smtp = NopMailSender;
    let mut processor = MessageProcessor {
        engine: &engine,
        script: &script,
        imap: &mut client,
        smtp: Some(&smtp),
        caps: &caps,
        state: &mut state,
        mailbox: "INBOX",
    };
    let results = processor.run_batch().await.unwrap();
    assert!(results
        .iter()
        .any(|r| matches!(r.status, imap_sieve_core::types::ProcessingStatus::Ok)));

    // 3. Verify the message landed in Junk by selecting Junk and checking EXISTS
    let junk = client.select("Junk").await.unwrap();
    assert!(junk.exists >= 1, "Junk should contain the moved message");
}