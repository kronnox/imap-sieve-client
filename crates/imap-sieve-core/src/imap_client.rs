//! IMAP client trait + production async-imap implementation (Phase 9) + fake for tests.

use crate::types::{parse_headers, CoreError, MessageContext};
use async_trait::async_trait;
use futures::StreamExt;
use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

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
    /// The server pushed an update (EXISTS, RECENT, etc.).
    /// The count field is always 0 with the current AsyncImapClient implementation;
    /// the session manager calls process_pending() which fetches all new messages
    /// regardless of the count.
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
        async fn fetch_uid_range(
            &mut self,
            _: &str,
            _: u32,
        ) -> Result<Vec<MessageContext>, CoreError> {
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
            self.ops
                .lock()
                .unwrap()
                .push(Op::Store(uid, op, flags.to_vec()));
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

// ---------------------------------------------------------------------------
// Stream type-erasure: TLS vs plain TCP
// ---------------------------------------------------------------------------

/// Wraps either a TLS stream or a plain TCP stream so that both can be stored
/// behind a single `Session` type.
enum ImapStream {
    Tls(Box<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>),
    Plain(tokio::net::TcpStream),
}

impl AsyncRead for ImapStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            ImapStream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
            ImapStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for ImapStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            ImapStream::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
            ImapStream::Plain(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            ImapStream::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
            ImapStream::Plain(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            ImapStream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
            ImapStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

impl fmt::Debug for ImapStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImapStream::Tls(_) => write!(f, "TlsStream"),
            ImapStream::Plain(_) => write!(f, "TcpStream"),
        }
    }
}

// Safety: both TlsStream<TcpStream> and TcpStream are Send + Unpin, so the enum
// is automatically Send + Unpin.

// ---------------------------------------------------------------------------
// AsyncImapClient – production IMAP adapter over async-imap
// ---------------------------------------------------------------------------

/// Helper to build a TLS stream using `tokio-rustls`.
async fn tls_connect(tcp: tokio::net::TcpStream, host: &str) -> Result<ImapStream, CoreError> {
    let root_store = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = rustls::client::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    let config = std::sync::Arc::new(config);
    let connector = tokio_rustls::TlsConnector::from(config);
    let server_name = rustls_pki_types::ServerName::try_from(host)
        .map_err(|e| CoreError::Imap(format!("invalid server name: {e}")))?
        .to_owned();
    let tls = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| CoreError::Imap(format!("TLS handshake: {e}")))?;
    Ok(ImapStream::Tls(Box::new(tls)))
}

/// Production IMAP client wrapping `async-imap`.
///
/// The inner stream type is `ImapStream` – an enum that erases TLS vs plain
/// TCP so that `Session<ImapStream>` has a single concrete type.
pub struct AsyncImapClient {
    session: Option<async_imap::Session<ImapStream>>,
}

impl AsyncImapClient {
    /// Connect to an IMAP server and authenticate.
    pub async fn connect(
        host: &str,
        port: u16,
        username: &str,
        password: &str,
        tls: bool,
    ) -> Result<Self, CoreError> {
        let tcp = tokio::net::TcpStream::connect((host, port))
            .await
            .map_err(|e| CoreError::Imap(format!("tcp connect to {host}:{port}: {e}")))?;

        let stream = if tls {
            tls_connect(tcp, host).await?
        } else {
            ImapStream::Plain(tcp)
        };

        let client = async_imap::Client::new(stream);

        // Read the server greeting before attempting login.
        // The Client::new() constructor does not consume the greeting; we rely
        // on login() to read it as part of the response loop.
        let session = client
            .login(username, password)
            .await
            .map_err(|(e, _)| CoreError::Imap(format!("login failed: {e}")))?;

        Ok(Self {
            session: Some(session),
        })
    }

    fn session(&mut self) -> Result<&mut async_imap::Session<ImapStream>, CoreError> {
        self.session
            .as_mut()
            .ok_or_else(|| CoreError::Imap("session not available".into()))
    }

    /// Create a mailbox. Used by integration tests. Not on the `ImapClient` trait.
    pub async fn session_create_mailbox(&mut self, name: &str) -> Result<(), CoreError> {
        self.session()?
            .create(name)
            .await
            .map_err(|e| CoreError::Imap(e.to_string()))
    }

    /// Append a raw message to a mailbox. Used by integration tests.
    pub async fn session_append(&mut self, mailbox: &str, raw: &[u8]) -> Result<(), CoreError> {
        self.session()?
            .append(mailbox, raw)
            .await
            .map_err(|e| CoreError::Imap(e.to_string()))
    }
}

/// Convert an `async_imap::types::Flag` to its string representation.
fn flag_to_string(flag: async_imap::types::Flag<'_>) -> String {
    match flag {
        async_imap::types::Flag::Seen => "\\Seen".to_string(),
        async_imap::types::Flag::Answered => "\\Answered".to_string(),
        async_imap::types::Flag::Flagged => "\\Flagged".to_string(),
        async_imap::types::Flag::Deleted => "\\Deleted".to_string(),
        async_imap::types::Flag::Draft => "\\Draft".to_string(),
        async_imap::types::Flag::Recent => "\\Recent".to_string(),
        async_imap::types::Flag::MayCreate => "\\*".to_string(),
        async_imap::types::Flag::Custom(s) => s.into_owned(),
    }
}

#[async_trait]
impl ImapClient for AsyncImapClient {
    async fn capabilities(&mut self) -> Result<Capabilities, CoreError> {
        let resp = self
            .session()?
            .capabilities()
            .await
            .map_err(|e| CoreError::Imap(e.to_string()))?;
        let mut caps = Capabilities::default();
        if resp.has_str("IDLE") {
            caps.idle = true;
        }
        if resp.has_str("UIDPLUS") {
            caps.uidplus = true;
        }
        if resp.has_str("MOVE") {
            caps.supports_move = true;
        }
        if resp.has_str("CONDSTORE") {
            caps.condstore = true;
        }
        if resp.has_str("QRESYNC") {
            caps.qresync = true;
        }
        Ok(caps)
    }

    async fn select(&mut self, mailbox: &str) -> Result<MailboxStatus, CoreError> {
        let m = self
            .session()?
            .select(mailbox)
            .await
            .map_err(|e| CoreError::Imap(e.to_string()))?;
        Ok(MailboxStatus {
            uidvalidity: m.uid_validity.unwrap_or(0),
            uidnext: m.uid_next.unwrap_or(0),
            exists: m.exists,
            highestmodseq: m.highest_modseq,
        })
    }

    async fn fetch_uid_range(
        &mut self,
        _mailbox: &str,
        start_uid: u32,
    ) -> Result<Vec<MessageContext>, CoreError> {
        let query = format!("{}:*", start_uid);
        let mut out = Vec::new();
        let mut stream = self
            .session()?
            .uid_fetch(query, "(UID FLAGS RFC822.SIZE BODY.PEEK[])")
            .await
            .map_err(|e| CoreError::Imap(e.to_string()))?;
        while let Some(item) = stream.next().await {
            let m = item.map_err(|e| CoreError::Imap(e.to_string()))?;
            let uid = m.uid.unwrap_or(0);
            let raw = m.body().map(|b| b.to_vec());
            let size = m.size.unwrap_or(0);
            let flags: Vec<String> = m.flags().map(flag_to_string).collect();
            let headers = parse_headers(raw.as_deref().unwrap_or_default());
            out.push(MessageContext {
                uid,
                mailbox: String::new(),
                headers,
                envelope_from: None,
                envelope_to: vec![],
                raw,
                flags,
                size,
            });
        }
        Ok(out)
    }

    async fn uid_move(&mut self, uid: u32, target: &str) -> Result<(), CoreError> {
        self.session()?
            .uid_mv(uid.to_string(), target)
            .await
            .map_err(|e| CoreError::Imap(e.to_string()))
    }

    async fn uid_copy(&mut self, uid: u32, target: &str) -> Result<(), CoreError> {
        self.session()?
            .uid_copy(uid.to_string(), target)
            .await
            .map_err(|e| CoreError::Imap(e.to_string()))
    }

    async fn uid_store_flags(
        &mut self,
        uid: u32,
        op: FlagOp,
        flags: &[String],
    ) -> Result<(), CoreError> {
        let prefix = match op {
            FlagOp::Add => "+FLAGS",
            FlagOp::Remove => "-FLAGS",
            FlagOp::Set => "FLAGS",
        };
        let cmd = format!("{} ({})", prefix, flags.join(" "));
        let results: Vec<_> = self
            .session()?
            .uid_store(uid.to_string(), cmd)
            .await
            .map_err(|e| CoreError::Imap(e.to_string()))?
            .collect()
            .await;
        for result in results {
            result.map_err(|e| CoreError::Imap(e.to_string()))?;
        }
        Ok(())
    }

    async fn uid_expunge(&mut self, uids: &[u32]) -> Result<(), CoreError> {
        let arg = uids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let results: Vec<_> = self
            .session()?
            .uid_expunge(arg)
            .await
            .map_err(|e| CoreError::Imap(e.to_string()))?
            .collect()
            .await;
        for result in results {
            result.map_err(|e| CoreError::Imap(e.to_string()))?;
        }
        Ok(())
    }

    async fn idle(&mut self, timeout: std::time::Duration) -> Result<IdleEvent, CoreError> {
        // idle() takes the Session by value, so we must take it out of the Option.
        // If init() fails, the Session is lost inside the Handle. The supervisor
        // will reconnect from scratch.
        let session = self
            .session
            .take()
            .ok_or_else(|| CoreError::Imap("session not available".into()))?;
        let mut handle = session.idle();

        // Send the IDLE command and wait for the continuation from the server.
        handle
            .init()
            .await
            .map_err(|e| CoreError::Imap(format!("idle init: {e}")))?;

        // Wait for a server push or timeout.
        let (fut, _stop_source) = handle.wait_with_timeout(timeout);
        let result = fut.await;

        // Best-effort: send DONE to cleanly exit IDLE. If the connection is
        // already dead, done() will fail — that's fine, the supervisor will
        // reconnect.
        match handle.done().await {
            Ok(session) => {
                self.session = Some(session);
            }
            Err(_) => {
                // Session is lost; supervisor will reconnect.
            }
        }

        match result {
            // NewData indicates the server pushed an update (typically EXISTS).
            // The count is not extracted from the response; the session manager
            // calls process_pending which fetches all new messages regardless.
            Ok(async_imap::extensions::idle::IdleResponse::NewData(_)) => Ok(IdleEvent::Exists(0)),
            Ok(async_imap::extensions::idle::IdleResponse::ManualInterrupt) => {
                Ok(IdleEvent::Interrupted)
            }
            Ok(async_imap::extensions::idle::IdleResponse::Timeout) => Ok(IdleEvent::Interrupted),
            Err(e) => Err(CoreError::Imap(format!("idle wait: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;

    #[tokio::test]
    async fn move_with_fallback_uses_native_move_when_available() {
        let caps = Capabilities {
            supports_move: true,
            uidplus: true,
            idle: true,
            ..Default::default()
        };
        let mut fake = FakeImap::new().with_caps(caps.clone());
        move_with_fallback(&mut fake, &caps, 5, "Junk")
            .await
            .unwrap();
        assert_eq!(fake.ops(), vec![Op::Move(5, "Junk".into())]);
    }

    #[tokio::test]
    async fn move_with_fallback_falls_back_to_copy_store_expunge() {
        let caps = Capabilities {
            supports_move: false,
            uidplus: true,
            idle: true,
            ..Default::default()
        };
        let mut fake = FakeImap::new().with_caps(caps.clone());
        move_with_fallback(&mut fake, &caps, 5, "Junk")
            .await
            .unwrap();
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
        let caps = Capabilities {
            supports_move: false,
            uidplus: false,
            idle: true,
            ..Default::default()
        };
        let mut fake = FakeImap::new().with_caps(caps.clone());
        move_with_fallback(&mut fake, &caps, 5, "Junk")
            .await
            .unwrap();
        assert_eq!(
            fake.ops(),
            vec![
                Op::Copy(5, "Junk".into()),
                Op::Store(5, FlagOp::Add, vec!["\\Deleted".into()]),
            ]
        );
    }
}
