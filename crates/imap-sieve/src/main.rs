use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod daemon;

#[derive(Parser)]
#[command(
    name = "imap-sieve",
    version,
    about = "IMAP daemon that processes mail with Sieve scripts"
)]
struct Cli {
    /// Path to the TOML config file
    #[arg(short, long, default_value = "/etc/imap-sieve/config.toml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon in the foreground.
    Start,
    /// Stop the running daemon (sends SIGTERM via PID file).
    Stop,
    /// Show daemon status and connection state.
    Status,
    /// Validate the configured Sieve script without running.
    Check,
    /// Test a Sieve rule against an RFC822 message file (dry run; no IMAP/SMTP).
    TestRule {
        /// Path to a Sieve script (overrides config)
        #[arg(long)]
        script: Option<PathBuf>,
        /// Path to an RFC822 message file
        message: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Start => daemon::run(&cli.config).await,
        Command::Stop => daemon::stop(&cli.config).await,
        Command::Status => daemon::status(&cli.config).await,
        Command::Check => daemon::check(&cli.config).await,
        Command::TestRule { script, message } => {
            daemon::test_rule(&cli.config, script.as_deref(), &message).await
        }
    }
}
