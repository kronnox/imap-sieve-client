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
    ///
    /// Note: sieve-rs parses headers from `raw` directly; this field is for
    /// consumer inspection (e.g. `test-rule` output) only.
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
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

/// Resolved actions that the executor can perform without further interpretation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SieveAction {
    Keep,
    Discard,
    FileInto {
        mailbox: String,
        copy: bool,
        create: bool,
    },
    Redirect {
        addresses: Vec<String>,
    },
    Reject {
        reason: String,
    },
    AddFlag {
        flags: Vec<String>,
    },
    RemoveFlag {
        flags: Vec<String>,
    },
    SetFlag {
        flags: Vec<String>,
    },
    /// `execute "name" "arg1" "arg2"` — custom action passthrough.
    Execute {
        name: String,
        args: Vec<String>,
    },
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

/// Parse RFC 5322 headers from raw email bytes.
///
/// Lowercases header names and concatenates duplicate headers with `, `
/// (per Sieve "header" test semantics). Handles RFC 5322 header field
/// folding (continuation lines starting with whitespace).
///
/// The initial `\n` in the first pass is intentional: it ensures the
/// first header line is parsed correctly by `split_once(':')` in the
/// second pass, even though the input doesn't start with a newline.
pub fn parse_headers(raw: &[u8]) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::new();
    let text = String::from_utf8_lossy(raw);

    // First pass: unfold continuation lines (RFC 5322 §2.2.3).
    // A continuation line starts with whitespace; it's appended to the
    // previous logical line by removing the CRLF + whitespace, replacing
    // with a single space.
    let mut unfolded = String::with_capacity(text.len());
    let mut prev_was_header = false;
    for line in text.lines() {
        if line.is_empty() {
            break;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            // Continuation line — fold into previous header
            if prev_was_header {
                unfolded.push(' ');
                unfolded.push_str(line.trim_start());
            }
        } else {
            unfolded.push('\n');
            unfolded.push_str(line);
            prev_was_header = true;
        }
    }

    // Second pass: extract name: value pairs
    for line in unfolded.lines() {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers
                .entry(name.trim().to_ascii_lowercase())
                .and_modify(|v: &mut String| {
                    v.push_str(", ");
                    v.push_str(value.trim());
                })
                .or_insert_with(|| value.trim().to_string());
        }
    }
    headers
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

    #[test]
    fn parse_headers_basic() {
        let raw = b"Subject: hello\r\nFrom: a@b.com\r\n\r\nbody";
        let h = parse_headers(raw);
        assert_eq!(h.get("subject"), Some(&"hello".to_string()));
        assert_eq!(h.get("from"), Some(&"a@b.com".to_string()));
    }

    #[test]
    fn parse_headers_concatenates_duplicates() {
        let raw = b"Received: by mx1\r\nReceived: from mx2\r\n\r\n";
        let h = parse_headers(raw);
        assert_eq!(h.get("received"), Some(&"by mx1, from mx2".to_string()));
    }

    #[test]
    fn parse_headers_unfolds_continuation() {
        let raw = b"Subject: this is\r\n a folded\r\n header\r\n\r\n";
        let h = parse_headers(raw);
        assert_eq!(
            h.get("subject"),
            Some(&"this is a folded header".to_string())
        );
    }

    #[test]
    fn parse_headers_return_path_preserves_brackets() {
        let raw = b"Return-Path: <user@example.com>\r\nFrom: a@b.com\r\n\r\n";
        let h = parse_headers(raw);
        // parse_headers returns the raw RFC 5321 reverse-path including angle
        // brackets. Consumers (imap_client::fetch_uid_range) strip them.
        assert_eq!(
            h.get("return-path"),
            Some(&"<user@example.com>".to_string())
        );
    }

    #[test]
    fn strip_return_path_angle_brackets() {
        fn strip_brackets(s: &str) -> &str {
            let s = s.trim();
            s.strip_prefix('<')
                .and_then(|s| s.strip_suffix('>'))
                .unwrap_or(s)
        }
        assert_eq!(strip_brackets("<user@example.com>"), "user@example.com");
        assert_eq!(strip_brackets("user@example.com"), "user@example.com");
        assert_eq!(strip_brackets(" <>"), "");
        assert_eq!(strip_brackets("<>"), "");
    }
}
