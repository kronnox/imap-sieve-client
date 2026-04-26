# IMAP Sieve Client — Design Specification

**Date:** 2026-04-25  
**Status:** Draft  
**Author:** Freya + Claude

---

## Overview

An IMAP client daemon that processes incoming mail using Sieve (RFC 5228) rules. It operates as a mail-server-independent complement to server-side Sieve — running on the client side, connecting to any IMAP server that supports IDLE, and applying Sieve rules to new messages as they arrive.

Key differentiator from server-side Sieve: it's mail-server agnostic and can be run per-account as a standalone daemon. While server-side Sieve only runs at delivery time, this client can process messages already in the mailbox.

## Goals

- Monitor an IMAP mailbox for new messages using IDLE
- Evaluate Sieve scripts (RFC 5228 + extensions) against each new message
- Execute resulting actions (fileinto, keep, redirect, discard, reject, flag, etc.)
- Be resilient to network failures and IMAP server disconnections
- Process only new mail (messages arriving after daemon start), not retroactively

## Non-Goals

- Multi-account support within a single instance (run one instance per account)
- Retroactive processing of existing messages on startup
- Email sending beyond redirect/reject (no full SMTP client)
- Server-side Sieve script management (ManageSieve protocol)
- Web dashboard or Prometheus metrics (logging only via `tracing`)
- Plugin system (deferred to v2 — see Future Work)

## Architecture

### Approach: `async-imap` + `sieve-rs`

- `async-imap` for IMAP client (mature, async, IDLE support, tokio-compatible)
- `sieve-rs` for Sieve engine (full RFC 5228 + 27 extensions, compiler + interpreter, event-driven API)
- `clap` for CLI, `toml` for config, `tracing` for observability
- `lettre` for SMTP (redirect/reject actions)

**License consideration:** `sieve-rs` is AGPL-3.0. For a self-hosted daemon (not distributed), this is fine. If distribution becomes needed, a custom Sieve engine can replace it behind the `SieveEngine` trait.

### Crate Structure

Three crates with clear dependency direction (no circular dependencies):

```
config  ←  imap-sieve-core  ←  imap-sieve (binary)
  │              │                    │
  │              │                    └── CLI, signal handling, daemon loop
  │              └── IMAP connection, sieve evaluation, action execution
  └── TOML config structs, validation
```

- **`config`** — Pure data types and TOML parsing. No dependency on any other crate.
- **`imap-sieve-core`** — The engine library. Depends on `config`, `async-imap`, `sieve-rs`, `lettre`. Owns all shared types (`SieveAction`, `MessageContext`, `ProcessingResult`).
- **`imap-sieve`** — Thin binary. Depends on `imap-sieve-core`, `clap`. CLI parsing, daemon lifecycle, signal handling.

This avoids the circular dependency risk of finer-grained crates and keeps shared types in one place (`imap-sieve-core`). Crates can be split later when boundaries stabilize.

### Layered Architecture

```
┌─────────────────────────────────┐
│           CLI (clap)            │  start, stop, status, test-rule
├─────────────────────────────────┤
│         Daemon Manager          │  signal handling, graceful shutdown
├─────────────────────────────────┤
│       IMAP Session Manager      │  async-imap, IDLE, reconnect, UID tracking
├─────────────────────────────────┤
│      Message Processor          │  fetch → sieve evaluate → act
├──────────┬──────────────────────┤
│ Sieve    │   Action Executor    │  fileinto, keep, redirect, discard
│ Engine   │                      │  (IMAP + SMTP operations)
│(sieve-rs)│                      │
├──────────┴──────────────────────┤
│         Config (TOML)           │  IMAP creds, script paths, settings
└─────────────────────────────────┘
```

### Key Architectural Decisions

1. **Single-account daemon**: One instance per IMAP account. Multi-account achieved by running multiple instances.
2. **IMAP IDLE-based**: Daemon sleeps via IDLE, wakes on EXISTS notification, fetches new messages only.
3. **UID-based IMAP operations**: All IMAP commands use UID variants (`UID FETCH`, `UID STORE`, `UID MOVE`, `UID EXPUNGE`) to avoid sequence-number fragility.
4. **Compiled script cache**: Sieve scripts compiled once on load, recompiled only when the file changes. Atomic swap via `Arc<CompiledScript>` to avoid mid-batch inconsistency.
5. **Trait-based Sieve engine abstraction**: `SieveEngine` trait isolates `sieve-rs` behind an interface, enabling future replacement.
6. **sieve-rs Event API**: Use `Event::Execute` for extensibility and sieve-rs's event-driven runtime (not error interception) — see Sieve Engine section.
7. **Single IMAP connection**: One TCP connection for both IDLE and commands. IDLE is interrupted to issue commands, then re-entered. This is the standard IMAP pattern and avoids the complexity of dual sessions.

## Data Flow

### Processing Pipeline

```
IMAP Server
    │
    ▼
┌──────────────┐    EXISTS response     ┌──────────────┐
│  IMAP IDLE   │ ──────────────────────▶ │  Interrupt   │
│  (waiting)   │                         │  IDLE,       │
└──────────────┘                          │  fetch new   │
                                          │  UIDs        │
                                          └──────┬───────┘
                                                 │
                                                 ▼
                                        ┌───────────────┐
                                        │  Fetch batch  │
                                        │  of new msgs  │
                                        │  by UID range │
                                        └───────┬────────┘
                                                │
                                                ▼
                                        ┌───────────────┐
                                        │  For each msg │
                                        │  in batch:    │
                                        └───────┬────────┘
                                                │
                                    ┌───────────┴───────────┐
                                    ▼                       ▼
                              ┌──────────┐           ┌────────────┐
                              │ Build    │           │ Evaluate   │
                              │ Sieve    │           │ message    │
                              │ runtime  │           │ against    │
                              │ context  │           │ compiled   │
                              └────┬─────┘           │ script     │
                                   │                 └─────┬──────┘
                                   │                       │
                                   │                       ▼
                                   │                 ┌────────────┐
                                   │                 │ Collect    │
                                   │                 │ actions +  │
                                   │                 │ events     │
                                   │                 └─────┬──────┘
                                   │                       │
                                   ▼                       ▼
                             ┌─────────────────────────────────┐
                             │       Execute actions            │
                             │  (IMAP MOVE/STORE/EXPUNGE,       │
                             │   SMTP for redirect/reject)      │
                             └──────────────┬──────────────────┘
                                            │
                                            ▼
                                   ┌────────────────┐
                                   │ Update          │
                                   │ last_seen_uid   │
                                   │ (persist state)  │
                                   └────────────────┘
```

### Concurrency Model

IMAP is fundamentally serial on a single connection — commands must be issued sequentially. The processing model is:

1. **IDLE interrupts for batch processing**: When an EXISTS notification arrives, the daemon interrupts IDLE (sends DONE), fetches all new messages by UID, processes the entire batch, then re-enters IDLE.
2. **If more EXISTS arrive during processing**: They are queued. After the current batch finishes, the daemon checks for queued notifications, fetches the new messages, processes them, then re-enters IDLE.
3. **Sieve evaluation is sequential per message**: Each message is fetched and evaluated one at a time. This keeps the model simple and avoids IMAP connection contention.
4. **IDLE timeout per RFC 2177**: Re-enter IDLE every 29 minutes (recommended maximum) even without activity, to satisfy server-side IDLE timeouts and detect stale connections.

### New Message Discovery

When IDLE pushes an `* N EXISTS` response, the daemon does not know which UIDs are new. The discovery algorithm:

1. Maintain `last_seen_uid` (persisted to state file).
2. On EXISTS notification, interrupt IDLE.
3. Issue `UID FETCH (last_seen_uid + 1):* (UID ENVELOPE BODY.PEEK[HEADER.FIELDS (FROM TO CC SUBJECT DATE MESSAGE-ID LIST-ID X-SPAM-STATUS)])` to get all new messages.
4. Process each message in the batch.
5. Update `last_seen_uid` to the highest UID in the batch after successful processing.
6. Re-enter IDLE.

Using UID range `(last_seen_uid + 1):*` ensures we only fetch messages we haven't seen, even if multiple arrived during processing.

### Message Processing Steps

1. **IDLE wake**: IMAP server pushes EXISTS notification. Daemon interrupts IDLE.
2. **Fetch**: Retrieve new messages by UID using `UID FETCH`. Fetch headers and envelope data needed for Sieve evaluation.
3. **Build runtime context**: Construct `MessageContext` with headers, envelope, flags, and IMAP metadata.
4. **Sieve evaluation**: `sieve-rs` evaluates the script against the `MessageContext`. Produces actions and events via the event-driven API.
5. **Execute actions**: Translate Sieve actions to IMAP/SMTP operations:
   - `fileinto` → IMAP `UID MOVE` to target folder (falls back to `UID COPY` + `UID STORE \Deleted` + `UID EXPUNGE` if server lacks MOVE)
   - `keep` → leave in current folder (no-op)
   - `discard` → IMAP `UID STORE \Deleted` + `UID EXPUNGE` (requires UIDPLUS)
   - `redirect` → Forward via SMTP (requires SMTP relay config)
   - `reject` → Send rejection notice via SMTP (requires SMTP relay config)
   - `imap4flags` actions → IMAP `UID STORE` to set/clear flags
6. **Update state**: Persist `last_seen_uid` after each successfully processed message.

## Sieve Engine

### Integration with sieve-rs

`sieve-rs` provides:
- **Compiler**: Parses `.sieve` files into bytecode
- **Runtime**: Evaluates bytecode against a message, produces actions and events
- **Extensions**: Supports 27+ RFC extensions out of the box (imap4flags, vacation, notify, subaddress, regex, etc.)
- **Event-driven API**: The runtime emits `Event` objects during evaluation, including `Event::Execute` for custom actions and `Event::Keep` / `Event::FileInto` / `Event::Redirect` for standard actions

### Event-Driven Evaluation

Rather than intercepting unknown identifiers (which doesn't work — sieve-rs rejects them at compile time), we use sieve-rs's built-in extensibility:

- **Standard actions** (`fileinto`, `keep`, `redirect`, `discard`, `reject`): Handled natively by sieve-rs, emitted as `Event::FileInto`, `Event::Keep`, `Event::Redirect`, etc.
- **Custom actions** via `execute`: Sieve scripts use `execute "action_name" "arg1" "arg2"`. The runtime emits `Event::Execute { command, arguments }`. Our `ActionExecutor` maps these command strings to actions.
- **Custom tests** via `FunctionMap`: sieve-rs supports registering custom functions for expression evaluation via `FunctionMap`. Custom tests that can be expressed as functions (e.g., `spam_score :above 80`) register their logic here.

This is the canonical way to extend sieve-rs — no pre-processing or error interception needed.

### SieveEngine Trait

```rust
trait SieveEngine {
    /// Compile a sieve script from file content
    fn compile(&self, script: &str) -> Result<CompiledScript, SieveError>;

    /// Evaluate a compiled script against a message context.
    /// Returns a list of actions and events to execute.
    fn evaluate(
        &self,
        script: &CompiledScript,
        context: &MessageContext,
    ) -> Result<Vec<SieveEvent>, SieveError>;
}
```

The `SieveEngineImpl` wraps `sieve-rs`. If AGPL becomes an issue, a custom implementation can replace it without touching the rest of the codebase.

### Script Hot-Reload

When `watch = true` in config, the daemon watches the sieve script file for changes:
- On change, recompile the script.
- If compilation succeeds, atomically swap the `Arc<CompiledScript>` so the next batch uses the new script.
- If compilation fails, log the error and continue using the old script. Never leave the daemon without a valid script.
- A batch always uses a consistent script version — no mid-batch script swaps.

## Configuration

### Format: TOML

```toml
[imap]
host = "imap.example.com"
port = 993
username = "user@example.com"
# Auth method: "plain" (default) or "oauth2"
auth_method = "plain"
password = "secret"                    # for auth_method = "plain"
# password_command = "pass show mail/example"  # fetch from password manager
# For OAuth2/XOAUTH2:
# auth_method = "oauth2"
# token_command = "oauth2cmd get-token user@example.com"  # command to refresh token
# TLS mode: "implicit" (default, IMAPS on port 993), "starttls" (STARTTLS on port 143), "plain" (no encryption)
tls_mode = "implicit"

[sieve]
script_path = "/etc/imap-sieve/rules.sieve"
# Watch for file changes and hot-reload
watch = true

[daemon]
# How many messages to fetch at once
batch_size = 10
# Seconds to wait between reconnect attempts (initial delay)
reconnect_delay = 5
# Maximum reconnect delay (exponential backoff cap)
max_reconnect_delay = 300
# Reconnect delay jitter factor (0.0-1.0, applied as random multiplier)
reconnect_jitter = 0.5
# Override state directory (default: $XDG_STATE_HOME/imap-sieve or ~/.local/state/imap-sieve)
# state_dir = "/var/lib/imap-sieve"

[logging]
level = "info"
# Optional: log to file
# file = "/var/log/imap-sieve/daemon.log"

# SMTP relay for redirect/reject actions (optional — required only if sieve scripts use redirect/reject)
[smtp]
host = "smtp.example.com"
port = 587
username = "user@example.com"
password = "secret"
# password_command = "pass show smtp/example"
starttls = true
```

## IMAP Extension Requirements

### Required

| Extension | RFC | Purpose |
|-----------|-----|---------|
| IDLE | 2177 | Push notifications for new mail |

### Strongly Recommended

| Extension | RFC | Purpose | Fallback |
|-----------|-----|---------|----------|
| UIDPLUS | 4315 | `UID EXPUNGE` for safe discard | `STORE \Deleted` without EXPUNGE (flags message as deleted, requires manual cleanup) |
| MOVE | 6851 | Atomic `fileinto` via `UID MOVE` | `UID COPY` + `UID STORE \Deleted` + `UID EXPUNGE` (three commands, non-atomic) |

### Recommended

| Extension | RFC | Purpose |
|-----------|-----|---------|
| CONDSTORE | 7162 | Efficient reconnect after IDLE break (only fetch changes since last HIGHESTMODSEQ) |
| QRESYNC | 7162 | Quick mailbox resynchronization after reconnect |

At startup, the daemon checks the IMAP server's CAPABILITY response and logs which extensions are available. If a required extension is missing, the daemon fails with a clear error. If a recommended extension is missing, the daemon logs a warning and uses the fallback behavior.

## State Persistence

### What is Persisted

- `last_seen_uid`: Highest UID processed for the monitored mailbox
- `uidvalidity`: The UIDVALIDITY of the mailbox at last processing
- `highestmodseq`: The HIGHESTMODSEQ (if CONDSTORE is supported)
- `selected_mailbox`: The mailbox name being monitored (e.g., "INBOX")

### Format and Location

- **Format**: JSON file (simple, human-readable, easy to debug)
- **Location**: `$XDG_STATE_DIR/imap-sieve/state.json` (falls back to `~/.local/state/imap-sieve/` if `XDG_STATE_DIR` is unset)
- **Write timing**: After each successfully processed message (not just at shutdown — crashes must not lose progress)
- **Atomic writes**: Write to a temp file, then rename over the old file (prevents corruption on crash)

### UIDVALIDITY Change Handling

If `UIDVALIDITY` changes on reconnect (mailbox was recreated), the daemon:
1. Logs a warning: "UIDVALIDITY changed, cached state is invalid"
2. Refuses to process messages and enters an error state
3. Alerts the operator via logging (and optionally via a notification hook in v2)
4. Requires manual intervention: the operator must verify the mailbox is correct and delete the state file to reset

This is the safest approach — a UIDVALIDITY change means all cached UIDs are invalid, and processing based on stale UIDs could move or delete the wrong messages. The operator must confirm the new mailbox is legitimate before processing resumes.

## Error Handling & Resilience

### Connection Resilience

- **Auto-reconnect** with exponential backoff + jitter on IMAP connection loss:
  - Initial delay: `reconnect_delay` seconds
  - Cap: `max_reconnect_delay` seconds
  - Jitter: `delay * (1 - jitter/2 + random(0..jitter))` — centers around the base delay to avoid thundering herd
- **IDLE re-establishment** after reconnect
- **UID tracking**: Track `last_seen_uid` + `UIDVALIDITY` to avoid re-processing messages after reconnect
- **IDLE timeout**: Re-enter IDLE every 29 minutes (RFC 2177 recommendation) to detect stale connections and satisfy server-side IDLE timeouts. This replaces NOOP-based health checks — IDLE has its own keepalive mechanism.

### Message Processing Resilience

- At-least-once processing: batch stops on first error so failed UIDs are retried
- Failed actions logged with context (message UID, error)
- On crash, some messages may be re-processed on restart. Most Sieve actions are idempotent:
  - `fileinto` / `keep`: Moving to a folder is idempotent if the message is already there (UID MOVE is idempotent, COPY may create duplicates — log a warning)
  - `discard`: Re-discarding an already-deleted message is a no-op
  - `redirect`: **Non-idempotent** — sends duplicate emails on re-processing. Log a warning on re-processed redirect actions. Operators should be aware of this limitation.
  - `reject`: **Non-idempotent** — sends duplicate rejection notices. Same mitigation as redirect.

### Sieve Script Errors

- **Compilation errors**: Fail fast at startup. Daemon refuses to start with an invalid script.
- **Runtime errors**: Log and skip the message. Default action is `keep` (leave message in place).
- **Script hot-reload errors**: If the new script fails to compile, log the error and continue using the previous valid script.

### Graceful Shutdown

- SIGTERM/SIGINT → stop accepting new messages, finish processing current message, persist state, disconnect cleanly
- State is persisted after each message, so shutdown simply stops the processing loop and disconnects

## CLI Interface

```
imap-sieve start          # Start the daemon (foreground)
imap-sieve stop           # Not yet implemented; send SIGTERM or Ctrl+C
imap-sieve status         # Show daemon status and connection state
imap-sieve test-rule      # Test a sieve rule against a message (dry-run)
imap-sieve check          # Validate sieve script without running
```

## Testing Strategy

### Unit Tests
- Sieve script compilation and evaluation
- Action executor translation (SieveAction → IMAP commands)
- Config parsing and validation
- State persistence (write, read, corrupt-file recovery)
- UIDVALIDITY change detection

### Integration Tests
- Full IDLE → process → act cycle against a real IMAP server (Dovecot in Docker)
- Reconnection after server restart
- UIDVALIDITY change handling
- Graceful shutdown and state persistence
- IMAP extension fallbacks (test with/without MOVE, UIDPLUS)
- Redirect action end-to-end (with mock SMTP server)

### Sieve Compliance Tests
- RFC 5228 mandatory test cases
- Extension test cases (imap4flags, subaddress, regex, etc.)
- Event::Execute handling for custom actions
- FunctionMap custom functions

## Project Structure

```
imap-sieve-client/
├── Cargo.toml                  # Workspace root
├── config.toml.example         # Example configuration
├── crates/
│   ├── config/                 # Configuration parsing (leaf crate)
│   │   └── src/
│   │       └── lib.rs           # TOML config structs, validation
│   ├── imap-sieve-core/        # Engine library
│   │   └── src/
│   │       ├── lib.rs           # Re-exports, shared types (SieveAction, MessageContext, etc.)
│   │       ├── sieve_engine.rs  # SieveEngine trait + sieve-rs implementation
│   │       ├── action_executor.rs # Sieve actions → IMAP/SMTP operations
│   │       ├── session.rs       # IMAP session manager (IDLE, reconnect, UID tracking)
│   │       ├── processor.rs     # Message processing pipeline
│   │       ├── state.rs         # State persistence (JSON file)
│   │       ├── imap_client.rs   # ImapClient trait + AsyncImapClient + fake impl
│   │       ├── smtp_sender.rs   # MailSender trait + LettreMailSender + fake impl
│   │       ├── script_loader.rs # Script loading + hot-reload watcher
│   │       └── types.rs         # MessageContext, SieveAction, CoreError, parse_headers
│   └── imap-sieve/             # Binary (thin CLI wrapper)
│       └── src/
│           ├── main.rs          # CLI entry point (clap)
│           └── daemon.rs        # Daemon lifecycle, signal handling, init_tracing
├── examples/                   # Example sieve scripts
├── tests/
│   └── integration/            # Integration tests (Dovecot Docker)
└── docs/
    └── design.md               # This design specification
```

## Dependencies (Key Crates)

| Crate | Purpose | License |
|-------|---------|---------|
| `async-imap` | Async IMAP client with IDLE | MIT/Apache |
| `sieve-rs` | Sieve script compiler + interpreter (event-driven API) | AGPL-3.0 |
| `tokio` | Async runtime | MIT |
| `clap` | CLI argument parsing | MIT/Apache |
| `toml` | Configuration file parsing | MIT/Apache |
| `serde` | Serialization (state file, config) | MIT/Apache |
| `tracing` | Structured logging | MIT |
| `lettre` | SMTP client (for redirect/reject) | MIT |
| `notify` | File system watcher (sieve script hot-reload) | CC0/Apache/MIT |

## Open Questions

1. **Redirect idempotency**: `redirect` is non-idempotent and may send duplicate emails on re-processing. Should the daemon track recently-redirected message UIDs and skip re-redirects? This adds complexity but prevents duplicate sends.
2. **SMTP relay for redirect/reject**: Current design assumes an external SMTP relay via config. Should the daemon support direct SMTP delivery? Current decision: external relay only, using `lettre`.
3. **Password security**: Storing passwords in TOML is not ideal. The `password_command` option allows fetching from password managers. Keyring integration could be added later.

## Future Work (v2)

- **Plugin system**: WASM-based custom Sieve tests and actions, with sandboxed execution, capability-scoped host functions, and resource limits. The `Event::Execute` API in sieve-rs provides the natural integration point for custom actions. Custom tests would use sieve-rs's `FunctionMap` API or a script pre-processing approach.
- **Multiple mailbox monitoring**: Watch folders beyond INBOX (e.g., for server-side Sieve rules that file into subfolders before our daemon sees them).
- **Retroactive processing**: Process existing messages on startup or on demand (e.g., `imap-sieve process --all`).
- **Prometheus metrics**: Processing counts, latency histograms, error rates.
- **Notification hooks**: Alert the operator on persistent failures via email, webhook, or system notifications.
- **ManageSieve support**: Upload/manage sieve scripts on the server via the ManageSieve protocol.