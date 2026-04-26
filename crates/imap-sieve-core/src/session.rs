//! IMAP session manager: drives IDLE, handles reconnects, fires the processor.

use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct BackoffConfig {
    pub initial: Duration,
    pub max: Duration,
    pub jitter: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    cfg: BackoffConfig,
    attempt: u32,
}

impl Backoff {
    pub fn new(cfg: BackoffConfig) -> Self {
        Self { cfg, attempt: 0 }
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// Returns the next delay and increments the attempt counter.
    pub fn next_delay(&mut self, rng: &mut impl rand::Rng) -> Duration {
        let exp = 2u32.saturating_pow(self.attempt) as u64;
        let base = self.cfg.initial.as_secs().saturating_mul(exp);
        let capped = base.min(self.cfg.max.as_secs());
        let jitter_factor = 1.0 + rng.gen_range(0.0..=self.cfg.jitter);
        let jittered = (capped as f64 * jitter_factor).min(self.cfg.max.as_secs() as f64);
        self.attempt = self.attempt.saturating_add(1);
        Duration::from_secs(jittered as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    #[test]
    fn first_delay_is_at_least_initial() {
        let cfg = BackoffConfig {
            initial: Duration::from_secs(5),
            max: Duration::from_secs(300),
            jitter: 0.5,
        };
        let mut b = Backoff::new(cfg);
        let mut rng = rand::rngs::StdRng::seed_from_u64(0);
        let d = b.next_delay(&mut rng);
        assert!(d >= Duration::from_secs(5), "got {d:?}");
        assert!(d <= Duration::from_secs(8), "got {d:?}"); // 5 * (1+0.5)
    }

    #[test]
    fn delay_grows_exponentially_then_caps() {
        let cfg = BackoffConfig {
            initial: Duration::from_secs(5),
            max: Duration::from_secs(300),
            jitter: 0.0,
        };
        let mut b = Backoff::new(cfg);
        let mut rng = rand::rngs::StdRng::seed_from_u64(0);
        assert_eq!(b.next_delay(&mut rng), Duration::from_secs(5));
        assert_eq!(b.next_delay(&mut rng), Duration::from_secs(10));
        assert_eq!(b.next_delay(&mut rng), Duration::from_secs(20));
        // Run forward to verify cap
        for _ in 0..20 {
            let _ = b.next_delay(&mut rng);
        }
        assert_eq!(b.next_delay(&mut rng), Duration::from_secs(300));
    }

    #[test]
    fn reset_returns_to_initial() {
        let cfg = BackoffConfig {
            initial: Duration::from_secs(5),
            max: Duration::from_secs(300),
            jitter: 0.0,
        };
        let mut b = Backoff::new(cfg);
        let mut rng = rand::rngs::StdRng::seed_from_u64(0);
        let _ = b.next_delay(&mut rng);
        let _ = b.next_delay(&mut rng);
        b.reset();
        assert_eq!(b.next_delay(&mut rng), Duration::from_secs(5));
    }
}

use crate::imap_client::{IdleEvent, ImapClient};
use crate::processor::MessageProcessor;
use crate::script_loader::ScriptHandle;
use crate::sieve_engine::SieveEngine;
use crate::smtp_sender::MailSender;
use crate::state::StateStore;
use crate::types::CoreError;
use std::sync::Arc;
use tokio::sync::Notify;

pub const IDLE_TIMEOUT: Duration = Duration::from_secs(29 * 60);

/// Lifetimes: `'i` for per-iteration mutable borrows, `'s` for shared refs.
pub struct SessionManager<'i, 's, E: SieveEngine, C: ImapClient + ?Sized, S: MailSender + ?Sized> {
    pub engine: &'s E,
    pub script: &'s ScriptHandle,
    pub imap: &'i mut C,
    pub smtp: Option<&'s S>,
    pub state: &'i mut StateStore,
    pub mailbox: &'s str,
    pub idle_timeout: Duration,
    pub shutdown: Arc<Notify>,
}

impl<'i, 's, E: SieveEngine, C: ImapClient + ?Sized, S: MailSender + ?Sized>
    SessionManager<'i, 's, E, C, S>
{
    /// Run the IDLE → process → repeat loop until `shutdown` is notified or a
    /// non-recoverable error occurs.
    pub async fn run(&mut self) -> Result<(), CoreError> {
        let caps = self.imap.capabilities().await?;
        if !caps.idle {
            return Err(CoreError::MissingCapability("IDLE"));
        }

        let status = self.imap.select(self.mailbox).await?;

        // Validate or persist UIDVALIDITY.
        match self.state.state().uidvalidity {
            Some(cached) if cached != status.uidvalidity => {
                return Err(CoreError::UidValidityChanged {
                    cached,
                    server: status.uidvalidity,
                });
            }
            _ => {
                self.state.update(|s| {
                    s.uidvalidity = Some(status.uidvalidity);
                    s.selected_mailbox = Some(self.mailbox.to_string());
                })?;
            }
        }

        // First-run UID seeding: per spec, the daemon processes only mail
        // arriving *after* startup. If no `last_seen_uid` is persisted yet,
        // anchor at `UIDNEXT - 1` so we skip every existing message.
        // (UIDNEXT is the UID the *next* arrival will get.)
        if self.state.state().last_seen_uid.is_none() {
            let seed = status.uidnext.saturating_sub(1);
            self.state.update(|s| s.last_seen_uid = Some(seed))?;
            tracing::info!(
                seed_uid = seed,
                uidnext = status.uidnext,
                "first run: anchored last_seen_uid to UIDNEXT-1; existing messages will not be processed"
            );
        }

        // Drain any messages that arrived between shutdown and restart
        // (or, on first run with a non-empty mailbox, none — the seed
        // above ensures the fetch range is empty).
        self.process_pending(&caps).await?;

        loop {
            // Cancellation safety: if `shutdown` fires while `idle` is in
            // flight, the IDLE future is dropped *without* sending DONE. The
            // `ImapClient::idle` implementation must therefore use a
            // best-effort `done()` in its own cleanup path (see Phase 9.1).
            // This loop assumes `idle()` is cancel-safe in the sense that
            // dropping the future leaves the connection unusable but not
            // catastrophically broken — the supervisor will reconnect.
            tokio::select! {
                biased;
                _ = self.shutdown.notified() => {
                    tracing::info!("shutdown requested; exiting session loop");
                    return Ok(());
                }
                event = self.imap.idle(self.idle_timeout) => {
                    match event? {
                        IdleEvent::Exists(_) => {
                            self.process_pending(&caps).await?;
                        }
                        IdleEvent::Interrupted => {
                            // IDLE timed out (29 min keepalive per RFC 2177);
                            // also poll for any messages that arrived during
                            // the IDLE window in case the server didn't push
                            // EXISTS, then re-enter IDLE.
                            self.process_pending(&caps).await?;
                            continue;
                        }
                        IdleEvent::Disconnected => {
                            return Err(CoreError::Imap("connection lost during IDLE".into()));
                        }
                    }
                }
            }
        }
    }

    async fn process_pending(
        &mut self,
        caps: &crate::imap_client::Capabilities,
    ) -> Result<(), CoreError> {
        let mut processor = MessageProcessor {
            engine: self.engine,
            script: self.script,
            imap: self.imap,
            smtp: self.smtp,
            caps,
            state: self.state,
            mailbox: self.mailbox,
        };
        processor.run_batch().await?;
        Ok(())
    }
}

#[cfg(test)]
mod session_loop_tests {
    use super::*;
    use crate::imap_client::fake::FakeImap;
    use crate::script_loader::ScriptLoader;
    use crate::sieve_engine::SieveEngineImpl;
    use crate::smtp_sender::fake::FakeSender;
    use crate::types::MessageContext;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn msg(uid: u32) -> MessageContext {
        let mut headers = BTreeMap::new();
        headers.insert("subject".into(), "x".into());
        MessageContext {
            uid,
            mailbox: "INBOX".into(),
            headers,
            envelope_from: Some("a@b".into()),
            envelope_to: vec!["c@d".into()],
            raw: Some(b"Subject: x\r\n\r\nbody".to_vec()),
            flags: vec![],
            size: 0,
        }
    }

    #[tokio::test]
    async fn processes_pending_then_shuts_down() {
        let dir = TempDir::new().unwrap();
        let script_path = dir.path().join("rules.sieve");
        std::fs::write(&script_path, "keep;").unwrap();
        let engine = SieveEngineImpl::new();
        let (_l, handle) = ScriptLoader::load(SieveEngineImpl::new(), &script_path).unwrap();

        let mut imap = FakeImap::new();
        // Simulate UIDNEXT = 3 so first-run seeding sets last_seen_uid = 2
        imap.status.uidnext = 3;
        *imap.fetch_responses.lock().unwrap() = vec![msg(1), msg(2)];
        let smtp = FakeSender::new();
        let mut state = StateStore::open(dir.path().join("state.json")).unwrap();
        let shutdown = Arc::new(Notify::new());

        let s = shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            s.notify_one();
        });

        let mut sm = SessionManager {
            engine: &engine,
            script: &handle,
            imap: &mut imap,
            smtp: Some(&smtp),
            state: &mut state,
            mailbox: "INBOX",
            idle_timeout: Duration::from_secs(60),
            shutdown,
        };
        sm.run().await.unwrap();
        // With at-least-once semantics, successful processing advances last_seen_uid.
        assert_eq!(state.state().last_seen_uid, Some(2));
    }

    #[tokio::test]
    async fn uidvalidity_change_returns_error() {
        let dir = TempDir::new().unwrap();
        let script_path = dir.path().join("rules.sieve");
        std::fs::write(&script_path, "keep;").unwrap();
        let engine = SieveEngineImpl::new();
        let (_l, handle) = ScriptLoader::load(SieveEngineImpl::new(), &script_path).unwrap();

        let mut imap = FakeImap::new();
        imap.status.uidvalidity = 99;
        let smtp = FakeSender::new();
        let mut state = StateStore::open(dir.path().join("state.json")).unwrap();
        state.update(|s| s.uidvalidity = Some(7)).unwrap();
        let shutdown = Arc::new(Notify::new());

        let mut sm = SessionManager {
            engine: &engine,
            script: &handle,
            imap: &mut imap,
            smtp: Some(&smtp),
            state: &mut state,
            mailbox: "INBOX",
            idle_timeout: Duration::from_secs(60),
            shutdown,
        };
        let err = sm.run().await.unwrap_err();
        assert!(matches!(
            err,
            CoreError::UidValidityChanged {
                cached: 7,
                server: 99
            }
        ));
    }

    #[tokio::test]
    async fn first_run_seeds_last_seen_uid_from_uidnext() {
        let dir = TempDir::new().unwrap();
        let script_path = dir.path().join("rules.sieve");
        std::fs::write(&script_path, "keep;").unwrap();
        let engine = SieveEngineImpl::new();
        let (_l, handle) = ScriptLoader::load(SieveEngineImpl::new(), &script_path).unwrap();

        let mut imap = FakeImap::new();
        // Mailbox has 42 existing messages; UIDNEXT = 43 means the next
        // arriving message will get UID 43. Seeding at 42 skips them all.
        imap.status.uidnext = 43;
        imap.status.exists = 42;
        // No new messages to fetch — the batch should be empty.
        *imap.fetch_responses.lock().unwrap() = vec![];
        let smtp = FakeSender::new();
        let mut state = StateStore::open(dir.path().join("state.json")).unwrap();
        let shutdown = Arc::new(Notify::new());
        shutdown.notify_one(); // immediate shutdown

        let mut sm = SessionManager {
            engine: &engine,
            script: &handle,
            imap: &mut imap,
            smtp: Some(&smtp),
            state: &mut state,
            mailbox: "INBOX",
            idle_timeout: Duration::from_secs(60),
            shutdown,
        };
        sm.run().await.unwrap();
        // last_seen_uid seeded to UIDNEXT-1 = 42; no messages processed.
        assert_eq!(state.state().last_seen_uid, Some(42));
    }
}

use async_trait::async_trait;

/// Constructs a fresh `ImapClient` on demand. Used by `Supervisor` for reconnects.
#[async_trait]
pub trait ConnectionFactory: Send + Sync {
    type Client: ImapClient + Send + 'static;
    async fn connect(&self) -> Result<Self::Client, CoreError>;
}

pub struct Supervisor<F: ConnectionFactory, E: SieveEngine, S: MailSender> {
    pub factory: F,
    pub engine: E,
    pub script: ScriptHandle,
    pub smtp: Option<S>,
    pub state: StateStore,
    pub mailbox: String,
    pub backoff_cfg: BackoffConfig,
    pub idle_timeout: Duration,
    pub shutdown: Arc<Notify>,
}

impl<F, E, S> Supervisor<F, E, S>
where
    F: ConnectionFactory,
    E: SieveEngine,
    S: MailSender,
{
    /// Run the reconnect loop: connect → session → reconnect on error.
    ///
    /// Returns `Ok(())` on graceful shutdown, `Err(UidValidityChanged)` on
    /// UIDVALIDITY mismatch (fatal — operator must intervene), or `Err` on
    /// an unrecoverable error that exhausted retries.
    pub async fn run(mut self) -> Result<(), CoreError> {
        let mut backoff = Backoff::new(self.backoff_cfg);
        let mut rng = rand::thread_rng();

        loop {
            // Connect, with cancellation-aware retry.
            let mut client = loop {
                tokio::select! {
                    biased;
                    _ = self.shutdown.notified() => {
                        tracing::info!("shutdown requested during connect; exiting");
                        return Ok(());
                    }
                    result = self.factory.connect() => {
                        match result {
                            Ok(c) => {
                                backoff.reset();
                                break c;
                            }
                            Err(e) => {
                                let delay = backoff.next_delay(&mut rng);
                                tracing::warn!(error = %e, ?delay, "connect failed; will retry");
                                // Sleep before retrying, but bail out if shutdown fires.
                                tokio::select! {
                                    _ = tokio::time::sleep(delay) => continue,
                                    _ = self.shutdown.notified() => return Ok(()),
                                }
                            }
                        }
                    }
                }
            };

            // Run the session to completion (or until an error occurs).
            let mut session = SessionManager {
                engine: &self.engine,
                script: &self.script,
                imap: &mut client,
                smtp: self.smtp.as_ref(),
                state: &mut self.state,
                mailbox: &self.mailbox,
                idle_timeout: self.idle_timeout,
                shutdown: self.shutdown.clone(),
            };
            match session.run().await {
                Ok(()) => return Ok(()), // graceful shutdown
                Err(CoreError::UidValidityChanged { cached, server }) => {
                    // Fatal — UIDVALIDITY changed. Log prominently and propagate.
                    tracing::error!(
                        cached_uidvalidity = cached,
                        server_uidvalidity = server,
                        "UIDVALIDITY changed; operator must verify mailbox and reset state. Refusing to process."
                    );
                    return Err(CoreError::UidValidityChanged { cached, server });
                }
                Err(e) => {
                    let delay = backoff.next_delay(&mut rng);
                    tracing::warn!(error = %e, ?delay, "session error; reconnecting");
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {},
                        _ = self.shutdown.notified() => return Ok(()),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod supervisor_tests {
    use super::*;
    use crate::imap_client::fake::FakeImap;
    use crate::script_loader::ScriptLoader;
    use crate::sieve_engine::SieveEngineImpl;
    use crate::smtp_sender::fake::FakeSender;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tempfile::TempDir;

    struct CountingFactory {
        count: AtomicU32,
        fail_first_n: u32,
    }

    #[async_trait]
    impl ConnectionFactory for CountingFactory {
        type Client = FakeImap;
        async fn connect(&self) -> Result<Self::Client, CoreError> {
            let n = self.count.fetch_add(1, Ordering::SeqCst);
            if n < self.fail_first_n {
                Err(CoreError::Imap(format!("simulated failure {n}")))
            } else {
                Ok(FakeImap::new())
            }
        }
    }

    #[tokio::test]
    async fn supervisor_retries_then_succeeds_then_shuts_down() {
        let dir = TempDir::new().unwrap();
        let script_path = dir.path().join("rules.sieve");
        std::fs::write(&script_path, "keep;").unwrap();
        let engine = SieveEngineImpl::new();
        let (_l, handle) = ScriptLoader::load(SieveEngineImpl::new(), &script_path).unwrap();

        let smtp: FakeSender = FakeSender::new();
        let mut state = StateStore::open(dir.path().join("state.json")).unwrap();
        // Seed state so the processor doesn't reject the batch.
        state.update(|s| s.last_seen_uid = Some(0)).unwrap();
        let shutdown = Arc::new(Notify::new());

        let s = shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            s.notify_one();
        });

        let supervisor = Supervisor {
            factory: CountingFactory {
                count: AtomicU32::new(0),
                fail_first_n: 2,
            },
            engine,
            script: handle,
            smtp: Some(smtp),
            state,
            mailbox: "INBOX".into(),
            backoff_cfg: BackoffConfig {
                initial: Duration::from_millis(10),
                max: Duration::from_millis(50),
                jitter: 0.0,
            },
            idle_timeout: Duration::from_millis(100),
            shutdown,
        };
        supervisor.run().await.unwrap();
    }
}
