# imap-sieve

A standalone IMAP daemon that processes incoming mail using Sieve (RFC 5228) rules.
Mail-server agnostic — connects to any IMAP server with IDLE support.

## Quickstart

```bash
cargo build --release
cp config.toml.example /etc/imap-sieve/config.toml
# edit config, then:
./target/release/imap-sieve --config /etc/imap-sieve/config.toml start
```

## Subcommands

- `start` — run the daemon in the foreground
- `check` — validate the configured Sieve script
- `test-rule <message>` — dry-run the script against an RFC822 file
- `status` — show persisted state (last UID seen, etc.)

## Architecture

See `docs/superpowers/specs/2026-04-25-imap-sieve-client-design.md` for the design,
and `docs/superpowers/plans/2026-04-26-imap-sieve-client.md` for the implementation
plan.

## Operations

Exit code 2 means the mailbox UIDVALIDITY changed (the server reassigned
UIDs — usually after a mailbox rebuild). The daemon refuses to process
further. Delete the state file after verifying the mailbox and restart.
Add `RestartPreventExitStatus=2` to the systemd unit file to suppress
auto-restart.

## License

AGPL-3.0 (because the embedded `sieve` engine is AGPL-3.0).