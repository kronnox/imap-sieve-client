# CLAUDE.md

Project conventions for Claude sessions working on imap-sieve.

## Project Overview

Rust workspace implementing an IMAP daemon that processes mail with Sieve (RFC 5228) rules. AGPL-3.0 license (due to `sieve-rs`). Three crates, single binary.

## Crate Structure

```
config              ← TOML config structs, validation, password resolution
imap-sieve-core     ← Engine library (IMAP, sieve, actions, state, SMTP)
imap-sieve          ← Binary (CLI, daemon lifecycle, signal handling)
```

### Key Files

- `crates/config/src/lib.rs` — All config types, validation, password resolution
- `crates/imap-sieve-core/src/sieve_engine.rs` — `SieveEngine` trait + `sieve-rs` implementation
- `crates/imap-sieve-core/src/action_executor.rs` — `SieveAction` → IMAP/SMTP operations
- `crates/imap-sieve-core/src/processor.rs` — Batch message processing pipeline
- `crates/imap-sieve-core/src/session.rs` — IDLE loop, `SessionManager`, `Supervisor`, `Backoff`
- `crates/imap-sieve-core/src/imap_client.rs` — `ImapClient` trait, `AsyncImapClient`, fake impl, `move_with_fallback`
- `crates/imap-sieve-core/src/script_loader.rs` — Compile, reload, file watcher (`Arc<ArcSwap<CompiledScript>>`)
- `crates/imap-sieve-core/src/state.rs` — Atomic JSON state persistence (`StateStore`)
- `crates/imap-sieve-core/src/smtp_sender.rs` — `MailSender` trait + `LettreMailSender` + fake
- `crates/imap-sieve-core/src/types.rs` — `MessageContext`, `SieveAction`, `CoreError`, `parse_headers`
- `crates/imap-sieve/src/main.rs` — CLI (`clap`)
- `crates/imap-sieve/src/daemon.rs` — `run()`, `status()`, `check()`, `test_rule()`, signal handlers, `AsyncImapFactory`

## How to Run Tests

```bash
cargo test                              # All tests
cargo test -p imap-sieve-core           # Core library only
cargo test -p config                    # Config only
cargo clippy --workspace --all-targets  # Lint
cargo fmt --check                       # Format check
```

Integration tests require a local Dovecot container: see `tests/integration/README.md`.

## Architectural Patterns

- **Trait-based DI**: `SieveEngine`, `ImapClient`, `MailSender` — each has a production impl and a fake impl for testing. Inject via constructor args, not globals.
- **Hot-reload**: `ScriptLoader` + `Arc<ArcSwap<CompiledScript>>` for atomic script swaps. Watcher monitors parent directory (catches atomic renames from editors).
- **State persistence**: `StateStore` writes JSON atomically (temp file + rename). Written after each successful message, not just at shutdown.
- **Backoff**: Exponential with jitter in `session.rs`. `initial * 2^n`, capped at `max`, jitter centers around the base delay: `delay * (1 - jitter/2 + random(0..jitter))`.
- **IDLE loop**: `SessionManager::run()` enters IDLE, wakes on EXISTS, processes batch, re-enters IDLE. 29-minute IDLE timeout per RFC 2177.
- **At-least-once**: Batch stops on first error so failed UIDs are retried. State is updated per-message.

## Common Tasks

### Add a new Sieve action

1. Add variant to `SieveAction` in `crates/imap-sieve-core/src/types.rs`
2. Handle the corresponding `Event` variant in `sieve_engine.rs` evaluate loop
3. Implement the action in `action_executor.rs`
4. Add tests in `sieve_engine.rs` (evaluation) and `action_executor.rs` (execution)

### Add a new config field

1. Add field to the appropriate struct in `crates/config/src/lib.rs` with `#[serde(default = ...)]` if needed
2. Add to `Config::validate()` if the field has constraints
3. Update `config.toml.example`
4. Update README config reference table

### Add a new IMAP capability

1. Add field to `Capabilities` in `imap_client.rs`
2. Detect in `AsyncImapClient::connect()` from CAPABILITY response
3. Check in `session.rs` or `action_executor.rs` where the capability affects behavior
4. Add fallback behavior when capability is absent

## Known Limitations

- Mixed `:copy`/non-`:copy` fileinto in same script: `:copy` info lost when non-`:copy` runs first (sieve-rs API limitation)
- OAuth2 auth method not yet implemented
- `stop` subcommand not yet implemented
- `batch_size` fetches all message bodies then truncates; doesn't reduce network I/O