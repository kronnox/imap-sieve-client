use anyhow::{Context, Result};
use config::Config;
use imap_sieve_core::imap_client::AsyncImapClient;
use imap_sieve_core::script_loader::ScriptLoader;
use imap_sieve_core::session::{BackoffConfig, ConnectionFactory, IDLE_TIMEOUT, Supervisor};
use imap_sieve_core::sieve_engine::{SieveEngine, SieveEngineImpl};
use imap_sieve_core::smtp_sender::LettreMailSender;
use imap_sieve_core::state::StateStore;
use imap_sieve_core::types::{CoreError, MessageContext};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Notify;

/// Exit code returned when UIDVALIDITY changes — signals a fatal
/// condition that should *not* be auto-restarted by a supervisor like
/// systemd without operator intervention.
const EXIT_UIDVALIDITY: i32 = 2;

pub async fn run(config_path: &Path) -> Result<()> {
    let cfg = Config::load(config_path).context("loading config")?;
    init_tracing(&cfg.logging.level);

    let engine = SieveEngineImpl::new();
    let (loader, script) =
        ScriptLoader::load(SieveEngineImpl::new(), &cfg.sieve.script_path)
            .context("loading sieve script")?;
    let _watcher = if cfg.sieve.watch {
        Some(loader.spawn_watcher().context("spawning sieve watcher")?)
    } else {
        None
    };

    let state_path = state_path(&cfg)?;
    let state = StateStore::open(&state_path).context("opening state store")?;

    let shutdown = Arc::new(Notify::new());
    install_signal_handlers(shutdown.clone());

    tracing::info!(
        host = %cfg.imap.host,
        mailbox = %cfg.imap.mailbox,
        "imap-sieve daemon starting"
    );

    let factory = AsyncImapFactory::new(cfg.imap.clone());
    let smtp = match cfg.smtp.clone() {
        Some(smtp_cfg) => Some(make_smtp(&smtp_cfg)?),
        None => None,
    };

    let supervisor = Supervisor {
        factory,
        engine,
        script,
        smtp,
        state,
        mailbox: cfg.imap.mailbox.clone(),
        backoff_cfg: BackoffConfig {
            initial: std::time::Duration::from_secs(cfg.daemon.reconnect_delay),
            max: std::time::Duration::from_secs(cfg.daemon.max_reconnect_delay),
            jitter: cfg.daemon.reconnect_jitter,
        },
        idle_timeout: IDLE_TIMEOUT,
        shutdown,
    };

    match supervisor.run().await {
        Ok(()) => {
            tracing::info!("daemon shut down cleanly");
            Ok(())
        }
        Err(CoreError::UidValidityChanged { cached, server }) => {
            tracing::error!(
                cached_uidvalidity = cached,
                server_uidvalidity = server,
                "UIDVALIDITY changed; operator must verify mailbox and delete the state file. Refusing to process."
            );
            std::process::exit(EXIT_UIDVALIDITY);
        }
        Err(e) => Err(e).context("supervisor exited"),
    }
}

pub async fn stop(_config_path: &Path) -> Result<()> {
    anyhow::bail!("`stop` subcommand requires a PID file integration; not yet implemented")
}

pub async fn status(config_path: &Path) -> Result<()> {
    let cfg = Config::load(config_path)?;
    let state_path = state_path(&cfg)?;
    let store = StateStore::open(&state_path)?;
    let s = store.state();
    println!("config:     {}", config_path.display());
    println!("state:      {}", state_path.display());
    println!("mailbox:    {}", cfg.imap.mailbox);
    println!(
        "uidvalidity:{}",
        s.uidvalidity
            .map(|u| u.to_string())
            .unwrap_or_else(|| "n/a".into())
    );
    println!(
        "last_uid:   {}",
        s.last_seen_uid
            .map(|u| u.to_string())
            .unwrap_or_else(|| "n/a".into())
    );
    Ok(())
}

pub async fn check(config_path: &Path) -> Result<()> {
    let cfg = Config::load(config_path)?;
    let engine = SieveEngineImpl::new();
    let text =
        std::fs::read_to_string(&cfg.sieve.script_path).context("reading sieve script")?;
    engine.compile(&text).context("compiling sieve script")?;
    println!(
        "OK: sieve script at {} compiled cleanly",
        cfg.sieve.script_path.display()
    );
    Ok(())
}

pub async fn test_rule(
    config_path: &Path,
    script_override: Option<&Path>,
    message_path: &Path,
) -> Result<()> {
    let cfg = Config::load(config_path)?;
    let script_path = script_override.unwrap_or(&cfg.sieve.script_path);
    let engine = SieveEngineImpl::new();
    let script_text = std::fs::read_to_string(script_path)?;
    let compiled = engine.compile(&script_text)?;

    let raw = std::fs::read(message_path)?;
    let headers = parse_headers(&raw);
    let ctx = MessageContext {
        uid: 0,
        mailbox: "test".into(),
        headers,
        envelope_from: None,
        envelope_to: vec![],
        raw: Some(raw),
        flags: vec![],
        size: 0,
    };
    let actions = engine.evaluate(&compiled, &ctx)?;
    println!("Actions ({}):", actions.len());
    for a in &actions {
        println!("  - {a:?}");
    }
    Ok(())
}

fn state_path(cfg: &Config) -> Result<PathBuf> {
    if let Some(dir) = &cfg.daemon.state_dir {
        return Ok(dir.join("state.json"));
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(dirs::state_dir)
        .or_else(dirs::data_local_dir)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "cannot determine state directory; set daemon.state_dir in config"
            )
        })?;
    Ok(base.join("imap-sieve").join("state.json"))
}

fn init_tracing(level: &str) {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    fmt().with_env_filter(filter).init();
}

fn install_signal_handlers(shutdown: Arc<Notify>) {
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut term = tokio::signal::unix::signal(
                tokio::signal::unix::SignalKind::terminate(),
            )
            .expect("install SIGTERM handler");
            tokio::select! {
                _ = ctrl_c => {}
                _ = term.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = ctrl_c.await;
        }
        tracing::info!("signal received; initiating shutdown");
        shutdown.notify_one();
    });
}

fn parse_headers(raw: &[u8]) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::new();
    let text = String::from_utf8_lossy(raw);
    for line in text.lines() {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers
                .entry(name.trim().to_ascii_lowercase())
                .or_insert_with(|| value.trim().to_string());
        }
    }
    headers
}

// === inline ConnectionFactory for AsyncImapClient ===

use async_trait::async_trait;
use config::ImapConfig;

struct AsyncImapFactory {
    cfg: ImapConfig,
}

impl AsyncImapFactory {
    fn new(cfg: ImapConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl ConnectionFactory for AsyncImapFactory {
    type Client = AsyncImapClient;
    async fn connect(&self) -> Result<Self::Client, CoreError> {
        let password = self
            .cfg
            .resolve_password()
            .map_err(|e| CoreError::Imap(format!("password: {e}")))?;
        AsyncImapClient::connect(
            &self.cfg.host,
            self.cfg.port,
            &self.cfg.username,
            &password,
            self.cfg.tls,
        )
        .await
    }
}

fn make_smtp(cfg: &config::SmtpConfig) -> Result<LettreMailSender> {
    let password = cfg.resolve_password().context("resolving smtp password")?;
    LettreMailSender::new(&cfg.host, cfg.port, &cfg.username, &password, cfg.starttls)
        .context("building smtp transport")
}