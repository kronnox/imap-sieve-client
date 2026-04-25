//! Shared types: message context, sieve actions and events, processing results.

use std::collections::BTreeMap;
use thiserror::Error;

/// A single fetched message ready for Sieve evaluation.
#[derive(Debug, Clone)]
pub struct MessageContext {
    pub uid: u32,
    pub mailbox: String,
    /// Header name → value (lowercased keys). Multi-valued headers are
    /// concatenated with `, ` (per Sieve "header" test semantics).
    pub headers: BTreeMap<String, String>,
    /// Envelope sender (MAIL FROM). Often unavailable from IMAP — `None` if
    /// the server did not provide it.
    pub envelope_from: Option<String>,
    /// Envelope recipients (RCPT TO). Same caveat as above.
    pub envelope_to: Vec<String>,
    /// Raw RFC-822 bytes (for `redirect` action — may be `None` if not yet fetched).
    pub raw: Option<Vec<u8>>,
    /// IMAP flags currently set on the message.
    pub flags: Vec<String>,
    pub size: u32,
}

impl MessageContext {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(String::as_str)
    }
}

/// Resolved actions that the executor can perform without further interpretation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SieveAction {
    Keep,
    Discard,
    FileInto { mailbox: String, copy: bool },
    Redirect { addresses: Vec<String> },
    Reject { reason: String },
    AddFlag { flags: Vec<String> },
    RemoveFlag { flags: Vec<String> },
    SetFlag { flags: Vec<String> },
    /// `execute "name" "arg1" "arg2"` — custom action passthrough.
    Execute { name: String, args: Vec<String> },
}

/// Outcome of processing a single message.
#[derive(Debug, Clone)]
pub struct ProcessingResult {
    pub uid: u32,
    pub actions: Vec<SieveAction>,
    pub status: ProcessingStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessingStatus {
    Ok,
    SieveError(String),
    ActionError(String),
}

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("imap error: {0}")]
    Imap(String),
    #[error("smtp error: {0}")]
    Smtp(String),
    #[error("sieve error: {0}")]
    Sieve(String),
    #[error("state error: {0}")]
    State(#[from] crate::state::StateError),
    #[error("uidvalidity changed: cached={cached} server={server}")]
    UidValidityChanged { cached: u32, server: u32 },
    #[error("missing required imap extension: {0}")]
    MissingCapability(&'static str),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_lookup_is_case_insensitive() {
        let mut headers = BTreeMap::new();
        headers.insert("subject".into(), "Hello".into());
        headers.insert("from".into(), "a@b.com".into());
        let ctx = MessageContext {
            uid: 1,
            mailbox: "INBOX".into(),
            headers,
            envelope_from: None,
            envelope_to: vec![],
            raw: None,
            flags: vec![],
            size: 0,
        };
        assert_eq!(ctx.header("Subject"), Some("Hello"));
        assert_eq!(ctx.header("SUBJECT"), Some("Hello"));
        assert_eq!(ctx.header("From"), Some("a@b.com"));
        assert_eq!(ctx.header("missing"), None);
    }
}