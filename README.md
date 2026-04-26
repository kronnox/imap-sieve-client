# imap-sieve

A standalone IMAP daemon that processes incoming mail using Sieve (RFC 5228) rules.
Mail-server agnostic — connects to any IMAP server with IDLE support.

## Features

- **IDLE-based push notification** (RFC 2177) — no polling
- **Full Sieve support** via `sieve-rs`: fileinto, keep, discard, redirect, reject, imap4flags, copy, mailbox (`:create`), subaddress, regex, and more
- **SMTP relay** for redirect/reject actions (via `lettre`)
- **Script hot-reload** — watches parent directory for atomic-rename saves (works with vim, emacs, deployment tools)
- **STARTTLS and implicit TLS** (IMAPS) support, plus plain TCP for testing
- **Exponential backoff** with jitter for IMAP reconnect
- **UIDVALIDITY change detection** — exits with code 2 to prevent auto-restart by systemd
- **At-least-once processing** with per-message state persistence (atomic JSON writes)
- **CONDSTORE** support for efficient flag sync on reconnect
- **`test-rule` dry-run** subcommand for testing scripts against sample messages

## Quick Start

```bash
cargo build --release
cp config.toml.example /etc/imap-sieve/config.toml
# Edit config with your IMAP credentials and sieve script path, then:
./target/release/imap-sieve --config /etc/imap-sieve/config.toml start
```

See `examples/` for sample Sieve scripts and `config.toml.example` for all config options.

## Installation

### From Source

Requires Rust 1.75 or later.

```bash
git clone https://github.com/user/imap-sieve-client.git
cd imap-sieve-client
cargo build --release
```

The binary is at `target/release/imap-sieve`.

### Pre-built Binaries

Not yet available. Build from source for now.

## Configuration

Configuration is a TOML file. See `config.toml.example` for a commented template.

### `[imap]` — IMAP Connection

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `host` | string | **required** | IMAP server hostname |
| `port` | u16 | **required** | IMAP server port (993 for IMAPS, 143 for STARTTLS) |
| `username` | string | **required** | Login username |
| `auth_method` | `"plain"` / `"oauth2"` | `"plain"` | Authentication method. `"oauth2"` is not yet implemented |
| `password` | string | — | Plaintext password (use `password_command` instead for better security) |
| `password_command` | string | — | Shell command to retrieve password (e.g., `pass show mail/example`) |
| `token_command` | string | — | Shell command to retrieve OAuth2 token (for future OAuth2 support) |
| `tls_mode` | `"implicit"` / `"starttls"` / `"plain"` | `"implicit"` | `"implicit"` = IMAPS (port 993), `"starttls"` = STARTTLS upgrade (port 143), `"plain"` = no encryption |
| `mailbox` | string | `"INBOX"` | Mailbox to monitor |

**Validation:** `host` must not be empty, `port` must not be 0. For `auth_method = "plain"`, either `password` or `password_command` is required.

### `[sieve]` — Sieve Script

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `script_path` | path | **required** | Path to the Sieve script |
| `watch` | bool | `false` | Hot-reload script on file change |

### `[daemon]` — Daemon Behavior

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `batch_size` | usize | `10` | Max messages processed per batch |
| `reconnect_delay` | u64 | `5` | Initial reconnect delay (seconds) |
| `max_reconnect_delay` | u64 | `300` | Max reconnect delay (seconds, exponential backoff cap) |
| `reconnect_jitter` | f64 | `0.5` | Jitter factor (0.0–1.0) applied to backoff delay |
| `state_dir` | path | — | Override state directory (default: `$XDG_STATE_DIR/imap-sieve`) |

**Validation:** `reconnect_jitter` must be 0.0–1.0, `batch_size` must be > 0.

### `[logging]` — Logging

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `level` | string | `"info"` | Log level: trace, debug, info, warn, error |
| `file` | path | — | Log to file instead of stderr |

### `[smtp]` — SMTP Relay (Optional)

Required only if your Sieve scripts use `redirect` or `reject`.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `host` | string | **required** | SMTP server hostname |
| `port` | u16 | **required** | SMTP server port |
| `username` | string | **required** | SMTP login username |
| `password` | string | — | Plaintext password |
| `password_command` | string | — | Shell command to retrieve password |
| `starttls` | bool | `true` | Use STARTTLS |

**Validation:** If `smtp` is present, `host` and `username` must not be empty, and either `password` or `password_command` is required.

## Sieve Support

### Supported Actions

| Action | Behavior |
|--------|----------|
| `keep` | Leave message in current mailbox (no-op) |
| `discard` | Mark as `\Deleted` and expunge (requires UIDPLUS) |
| `fileinto` | Move to target mailbox (falls back to COPY+STORE+EXPUNGE if MOVE not available) |
| `fileinto :copy` | Copy to target mailbox, leave original |
| `fileinto :create` | Create target mailbox if it doesn't exist |
| `redirect` | Forward via SMTP relay |
| `reject` | Send rejection notice via SMTP relay |
| `addflag` / `removeflag` / `setflag` | Set/clear IMAP flags via UID STORE. Note: `addflag`/`removeflag` from `imap4flags` are folded into `setflag` on the disposition action. |
| `execute` | Custom action passthrough (logged, no handler registered) |

### Supported Extensions

`fileinto`, `copy`, `mailbox` (`:create`), `imap4flags`, `reject`, `subaddress`, `regex`, `encoded-character`, `variables`, `relational`, `date`, `index`, `envelope`

### Known Limitations

- **Mixed `:copy`/non-`:copy` fileinto**: If a non-`:copy` fileinto runs before a `:copy` fileinto in the same script, the `:copy` info is lost. This is a `sieve-rs` API limitation — `Event::FileInto` has no `copy` field. The common case (single `:copy` fileinto) works correctly.
- **OAuth2**: Not yet implemented. Config validation rejects `auth_method = "oauth2"`.
- **`stop` subcommand**: Not yet implemented. Send SIGTERM or Ctrl+C to stop the daemon.
- **`batch_size`**: Limits messages processed per batch but fetches all message bodies first. For mailboxes with thousands of pending messages, a search-then-paginate approach would be more efficient.

## CLI Reference

```
imap-sieve [OPTIONS] <COMMAND>

Options:
  -c, --config <PATH>  Path to config file [default: /etc/imap-sieve/config.toml]

Commands:
  start      Run the daemon in the foreground
  status     Show persisted state (UIDVALIDITY, last UID seen)
  check      Validate the configured Sieve script
  test-rule  Dry-run a Sieve script against an RFC822 message file
```

### `test-rule`

```bash
# Test the configured script against a message:
imap-sieve --config /etc/imap-sieve/config.toml test-rule message.eml

# Test a different script:
imap-sieve --config /etc/imap-sieve/config.toml test-rule --script /path/to/rules.sieve message.eml
```

## Operations

### Exit Codes

| Code | Meaning |
|------|---------|
| 0 | Clean shutdown (SIGTERM or Ctrl+C) |
| 2 | UIDVALIDITY changed — fatal condition requiring operator intervention |

### UIDVALIDITY Changed (Exit Code 2)

The IMAP server reassigned UIDs (usually after a mailbox rebuild). The daemon refuses to process further because cached UIDs are invalid.

**Recovery:** Verify the mailbox is correct, delete the state file, and restart:

```bash
rm ~/.local/state/imap-sieve/state.json
systemctl restart imap-sieve
```

Add `RestartPreventExitStatus=2` to your systemd unit to prevent automatic restart on this error.

### State File

Location: `$XDG_STATE_HOME/imap-sieve/state.json` (falls back to `~/.local/state/imap-sieve/`). Override with `daemon.state_dir` in config.

```json
{
  "selected_mailbox": "INBOX",
  "uidvalidity": 12345,
  "last_seen_uid": 999,
  "highestmodseq": 42
}
```

State is written after each successfully processed message (atomic write via temp file + rename). On first run, `last_seen_uid` is seeded from `UIDNEXT - 1` so existing messages are not re-processed.

### Script Hot-Reload

When `watch = true` in config, the daemon watches the **parent directory** of the script (to catch atomic-rename saves from editors). On change:
1. Recompile the script.
2. If compilation succeeds, atomically swap the running script.
3. If compilation fails, log the error and continue with the previous valid script.

### Environment Variables

| Variable | Effect |
|----------|--------|
| `RUST_LOG` | Override log level (e.g., `RUST_LOG=debug`). Overrides the `logging.level` config field. |
| `XDG_STATE_HOME` | Override state directory base (default: `~/.local/state`) |

## systemd Unit

```ini
[Unit]
Description=IMAP Sieve Daemon
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/imap-sieve --config /etc/imap-sieve/config.toml start
Restart=on-failure
RestartPreventExitStatus=2

[Install]
WantedBy=multi-user.target
```

## Architecture

Three-crate workspace with clear dependency direction:

```
config  ←  imap-sieve-core  ←  imap-sieve (binary)
  │              │                    │
  │              │                    └── CLI, signal handling, daemon loop
  │              └── IMAP connection, sieve evaluation, action execution
  └── TOML config structs, validation
```

- **`config`** — Pure data types and TOML parsing. No async dependencies.
- **`imap-sieve-core`** — The engine library. Trait-based abstractions (`SieveEngine`, `ImapClient`, `MailSender`) with production and fake implementations for testing.
- **`imap-sieve`** — Thin binary. CLI via `clap`, daemon lifecycle, signal handling.

See `docs/design.md` for the full specification.

## Building from Source

```bash
cargo build --release
```

Requires Rust 1.75+. The binary is at `target/release/imap-sieve`.

## License

AGPL-3.0 — because the embedded `sieve-rs` engine is AGPL-3.0.