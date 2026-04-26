//! Configuration types and loading for the imap-sieve daemon.

use serde::Deserialize;
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub imap: ImapConfig,
    pub sieve: SieveConfig,
    #[serde(default)]
    pub daemon: DaemonConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    pub smtp: Option<SmtpConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ImapConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    #[serde(default)]
    pub auth_method: AuthMethod,
    pub password: Option<String>,
    pub password_command: Option<String>,
    pub token_command: Option<String>,
    #[serde(default = "default_tls_mode")]
    pub tls_mode: ImapTlsMode,
    #[serde(default = "default_mailbox")]
    pub mailbox: String,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum AuthMethod {
    #[default]
    Plain,
    Oauth2,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ImapTlsMode {
    /// Implicit TLS (IMAPS, typically port 993).
    #[default]
    Implicit,
    /// STARTTLS upgrade from plain (RFC 3207, typically port 143).
    Starttls,
    /// Plain TCP, no encryption.
    Plain,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SieveConfig {
    pub script_path: PathBuf,
    #[serde(default)]
    pub watch: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DaemonConfig {
    /// Batch size for UID FETCH. Limits the number of messages processed per
    /// batch to avoid unbounded memory usage and IMAP timeouts.
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
    #[serde(default = "default_reconnect_delay")]
    pub reconnect_delay: u64,
    #[serde(default = "default_max_reconnect_delay")]
    pub max_reconnect_delay: u64,
    #[serde(default = "default_reconnect_jitter")]
    pub reconnect_jitter: f64,
    pub state_dir: Option<PathBuf>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            batch_size: default_batch_size(),
            reconnect_delay: default_reconnect_delay(),
            max_reconnect_delay: default_max_reconnect_delay(),
            reconnect_jitter: default_reconnect_jitter(),
            state_dir: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoggingConfig {
    #[serde(default = "default_log_level")]
    pub level: String,
    /// Optional log file path. When set, logs are appended to this file
    /// instead of stderr.
    pub file: Option<PathBuf>,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            file: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: Option<String>,
    pub password_command: Option<String>,
    #[serde(default = "default_true")]
    pub starttls: bool,
}

fn default_true() -> bool {
    true
}
fn default_tls_mode() -> ImapTlsMode {
    ImapTlsMode::Implicit
}
fn default_mailbox() -> String {
    "INBOX".into()
}
fn default_batch_size() -> usize {
    10
}
fn default_reconnect_delay() -> u64 {
    5
}
fn default_max_reconnect_delay() -> u64 {
    300
}
fn default_reconnect_jitter() -> f64 {
    0.5
}
fn default_log_level() -> String {
    "info".into()
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("io error reading config: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml parse error: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid config: {0}")]
    Invalid(String),
    #[error("password_command failed: {0}")]
    PasswordCommand(String),
}

impl Config {
    /// Load and validate config from a TOML file.
    pub fn load(path: &std::path::Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)?;
        let cfg: Config = toml::from_str(&text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.imap.host.is_empty() {
            return Err(ConfigError::Invalid("imap.host must not be empty".into()));
        }
        if self.imap.port == 0 {
            return Err(ConfigError::Invalid("imap.port must not be 0".into()));
        }
        if matches!(self.imap.auth_method, AuthMethod::Plain)
            && self.imap.password.is_none()
            && self.imap.password_command.is_none()
        {
            return Err(ConfigError::Invalid(
                "imap.password or imap.password_command required for auth_method = plain".into(),
            ));
        }
        if matches!(self.imap.auth_method, AuthMethod::Oauth2) {
            return Err(ConfigError::Invalid(
                "auth_method = oauth2 is not yet implemented".into(),
            ));
        }
        if let Some(ref smtp) = self.smtp {
            if smtp.host.is_empty() {
                return Err(ConfigError::Invalid("smtp.host must not be empty".into()));
            }
            if smtp.username.is_empty() {
                return Err(ConfigError::Invalid(
                    "smtp.username must not be empty".into(),
                ));
            }
            if smtp.password.is_none() && smtp.password_command.is_none() {
                return Err(ConfigError::Invalid(
                    "smtp.password or smtp.password_command required".into(),
                ));
            }
        }
        if !(0.0..=1.0).contains(&self.daemon.reconnect_jitter) {
            return Err(ConfigError::Invalid(
                "daemon.reconnect_jitter must be between 0.0 and 1.0".into(),
            ));
        }
        if self.daemon.batch_size == 0 {
            return Err(ConfigError::Invalid("daemon.batch_size must be > 0".into()));
        }
        Ok(())
    }
}

impl ImapConfig {
    pub fn resolve_password(&self) -> Result<String, ConfigError> {
        resolve_secret(self.password.as_deref(), self.password_command.as_deref())
    }

    pub fn resolve_oauth_token(&self) -> Result<String, ConfigError> {
        match &self.token_command {
            Some(cmd) => run_secret_command(cmd),
            None => Err(ConfigError::Invalid("token_command not configured".into())),
        }
    }
}

impl SmtpConfig {
    pub fn resolve_password(&self) -> Result<String, ConfigError> {
        resolve_secret(self.password.as_deref(), self.password_command.as_deref())
    }
}

fn resolve_secret(literal: Option<&str>, command: Option<&str>) -> Result<String, ConfigError> {
    if let Some(s) = literal {
        return Ok(s.to_string());
    }
    if let Some(cmd) = command {
        return run_secret_command(cmd);
    }
    Err(ConfigError::Invalid(
        "no secret available (literal or command)".into(),
    ))
}

fn run_secret_command(cmd: &str) -> Result<String, ConfigError> {
    let output = if cfg!(windows) {
        std::process::Command::new("cmd").args(["/C", cmd]).output()
    } else {
        std::process::Command::new("sh").args(["-c", cmd]).output()
    };
    let output = output.map_err(|e| ConfigError::PasswordCommand(e.to_string()))?;
    if !output.status.success() {
        return Err(ConfigError::PasswordCommand(format!(
            "command exited with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let secret = String::from_utf8_lossy(&output.stdout)
        .trim_end_matches(['\n', '\r'])
        .to_string();
    if secret.is_empty() {
        return Err(ConfigError::PasswordCommand("empty output".into()));
    }
    Ok(secret)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_CONFIG: &str = r#"
[imap]
host = "imap.example.com"
port = 993
username = "user@example.com"
auth_method = "plain"
password = "secret"
tls_mode = "implicit"

[sieve]
script_path = "/etc/imap-sieve/rules.sieve"
watch = true

[daemon]
batch_size = 10
reconnect_delay = 5
max_reconnect_delay = 300
reconnect_jitter = 0.5

[logging]
level = "info"

[smtp]
host = "smtp.example.com"
port = 587
username = "user@example.com"
password = "secret"
starttls = true
"#;

    #[test]
    fn parses_full_config() {
        let cfg: Config = toml::from_str(FULL_CONFIG).expect("parse");
        assert_eq!(cfg.imap.host, "imap.example.com");
        assert_eq!(cfg.imap.port, 993);
        assert_eq!(cfg.imap.auth_method, AuthMethod::Plain);
        assert_eq!(
            cfg.sieve.script_path.to_str().unwrap(),
            "/etc/imap-sieve/rules.sieve"
        );
        assert!(cfg.sieve.watch);
        assert_eq!(cfg.daemon.batch_size, 10);
        assert_eq!(cfg.logging.level, "info");
        assert!(cfg.smtp.is_some());
    }

    #[test]
    fn defaults_apply_when_omitted() {
        let minimal = r#"
[imap]
host = "imap.example.com"
port = 993
username = "u"
password = "p"

[sieve]
script_path = "rules.sieve"
"#;
        let cfg: Config = toml::from_str(minimal).expect("parse");
        assert_eq!(cfg.imap.auth_method, AuthMethod::Plain);
        assert_eq!(cfg.imap.tls_mode, ImapTlsMode::Implicit);
        assert!(!cfg.sieve.watch);
        assert_eq!(cfg.daemon.batch_size, 10);
        assert_eq!(cfg.daemon.reconnect_delay, 5);
        assert_eq!(cfg.daemon.max_reconnect_delay, 300);
        assert!((cfg.daemon.reconnect_jitter - 0.5).abs() < f64::EPSILON);
        assert_eq!(cfg.logging.level, "info");
        assert!(cfg.smtp.is_none());
    }

    #[test]
    fn resolve_password_returns_static_password() {
        let imap = ImapConfig {
            host: "h".into(),
            port: 993,
            username: "u".into(),
            auth_method: AuthMethod::Plain,
            password: Some("static".into()),
            password_command: None,
            token_command: None,
            tls_mode: ImapTlsMode::Implicit,
            mailbox: "INBOX".into(),
        };
        assert_eq!(imap.resolve_password().unwrap(), "static");
    }

    #[test]
    #[cfg(unix)]
    fn resolve_password_runs_command() {
        let imap = ImapConfig {
            host: "h".into(),
            port: 993,
            username: "u".into(),
            auth_method: AuthMethod::Plain,
            password: None,
            password_command: Some("printf 'from-cmd'".into()),
            token_command: None,
            tls_mode: ImapTlsMode::Implicit,
            mailbox: "INBOX".into(),
        };
        assert_eq!(imap.resolve_password().unwrap(), "from-cmd");
    }
}
