# db-s3-backup

Formerly `mysql-s3-backup`. From v5, every release is published to both `joanfabregat/db-s3-backup` and `joanfabregat/mysql-s3-backup` on Docker Hub, and to `ghcr.io/joanfabregat/db-s3-backup`. Deployments on the old name's `latest` get v5, so the MySQL side stays as v4 read it: the `MYSQL_*` variables, the MySQL `DATABASE_URL` parsing (no percent-decoding), the defaults, the MySQL object keys and the exit codes don't change without a new major version. The "Upgrading from v4" section of the README lists the deliberate exceptions.

## Code Execution

Never run code locally. Use the `/run-rust` skill for every Cargo command (build, test, clippy, fmt). The integration tests need an S3 server and the databases: run versitygw, MariaDB and PostgreSQL with `run-podman --group`, then the test binary in that group (see `tests/integration.rs` for the variables). Run the PostgreSQL tests from a `postgres:18` image and the MySQL tests from a `mariadb` image, so the clients are on `PATH`; the `*` tests are `#[ignore]`d and run in a second pass with `--ignored --test-threads=1`.

## Before Committing

All of these must pass with no warnings:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## Docker Hub

The Hub overview is `dockerhub/db-s3-backup.md`, not `README.md`, which is over the Hub's 25,000-byte limit; the release workflow puts `dockerhub/mysql-s3-backup-notice.md` above it for the old name. When a change adds or renames a variable, a tag or a default, update the README and the Hub page.
