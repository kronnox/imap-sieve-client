# Integration tests

These require a running Dovecot. Bring it up with:

    docker compose -f tests/integration/dovecot/docker-compose.yml up -d

Then:

    DOVECOT_TEST_HOST=localhost DOVECOT_TEST_PORT=14300 cargo test -p imap-sieve-core --features test-support -- --ignored