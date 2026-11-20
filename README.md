# db-s3-backup

[![CI](https://github.com/joanfabregat/db-s3-backup/actions/workflows/ci.yml/badge.svg)](https://github.com/joanfabregat/db-s3-backup/actions/workflows/ci.yml)
[![Build and push Docker image](https://github.com/joanfabregat/db-s3-backup/actions/workflows/docker-image.yml/badge.svg)](https://github.com/joanfabregat/db-s3-backup/actions/workflows/docker-image.yml)
[![GitHub release](https://img.shields.io/github/v/release/joanfabregat/db-s3-backup?include_prereleases&sort=semver)](https://github.com/joanfabregat/db-s3-backup/releases)
[![Docker Pulls](https://img.shields.io/docker/pulls/joanfabregat/mysql-s3-backup?label=pulls%20%28mysql-s3-backup%29)](https://hub.docker.com/r/joanfabregat/mysql-s3-backup)
[![Docker Image Size](https://img.shields.io/docker/image-size/joanfabregat/db-s3-backup/latest)](https://hub.docker.com/r/joanfabregat/db-s3-backup)
[![Rust](https://img.shields.io/badge/rust-1.94.1%2B-orange?logo=rust)](Cargo.toml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

A container that backs up MySQL, MariaDB and PostgreSQL databases to Amazon S3 or any S3-compatible provider (Backblaze B2, Garage, MinIO, …). It runs the database's own dump tool and streams its output to the bucket as a multipart upload, without writing the dump to disk.

**Formerly `mysql-s3-backup`.** From v4.1.0, every release is published under both `joanfabregat/db-s3-backup` and `joanfabregat/mysql-s3-backup`, with the same digest and the same tags. v4.1 is fully compatible with v4.0: an existing deployment keeps working unchanged on either name. See [Upgrading from v4.0](#upgrading-from-v40). The same images are also on the GitHub Container Registry as `ghcr.io/joanfabregat/db-s3-backup`.

## How it works

A small Rust binary drives the backup. The dump itself comes from `mariadb-dump` (Alpine's `mariadb-client`) or `pg_dump` (Alpine's `postgresql18-client`); the binary handles the rest:

1. Runs the dump tool for one database and reads its output through a pipe.
2. Compresses it with gzip into a buffer the size of one upload part (16 MiB by default). A PostgreSQL custom-format dump is compressed by `pg_dump` and passed through as is.
3. Each time the buffer fills, sends it to S3 as the next part of a multipart upload, two parts at a time.
4. Sends `CompleteMultipartUpload` only after the dump tool has exited 0.

Until step 4, S3 holds only invisible parts, not an object. If the dump fails, stalls past its timeout, or the container is stopped, the binary sends `AbortMultipartUpload` and the parts are discarded. **A truncated dump never becomes an object**, which matters when the bucket is under Object Lock and a bad object cannot be deleted.

Memory, not disk, bounds the work: at most three parts are held at once (one filling, two uploading), so 48 MiB of buffers at the default part size, whatever the database size. If the upload is slower than the dump, reading pauses and the dump tool waits on the pipe; the log reports how long, because with MySQL's default locking mode the table locks stay held while it waits (see [MySQL: consistency and locking](#mysql-consistency-and-locking)).

## Features

- MySQL and MariaDB through `mariadb-dump`, PostgreSQL 9.2 to 18 through `pg_dump`
- Streams dumps to S3 with no local disk use, and never leaves a partial object behind
- Several databases per run through a comma-separated list, or `*` for every database on the server
- PostgreSQL custom-format dumps (restore single tables with `pg_restore`) or plain SQL, and optionally roles and tablespaces through `pg_dumpall --globals-only`
- Optional MySQL `--single-transaction` dumps that do not block writes, with a warning that names tables it cannot snapshot
- TCP or Unix socket connections, individual variables or a `DATABASE_URL`
- Any S3-compatible provider through `S3_ENDPOINT_URL`, with path-style addressing
- A Content-MD5 on every part, checked by the provider
- Wall-clock timeouts that fit inside a Kubernetes Job's `activeDeadlineSeconds`
- Clean shutdown on `SIGTERM`: the dump is stopped and the upload aborted
- A built-in success ping for heartbeat monitors such as UptimeRobot, with no sidecar
- The database password goes through `MYSQL_PWD` or `PGPASSWORD`, never the command line
- Multi-architecture images (`linux/amd64`, `linux/arm64`), with a binary built by `cargo-auditable` so image scanners see its Rust dependencies

## Configuration

Everything is configured through environment variables. One run backs up one server; run one Job per server (see [Kubernetes CronJob](#kubernetes-cronjob)).

### Choosing the engine

The engine comes from the `DATABASE_URL` scheme or, without a URL, from the prefix of the variables you set:

| You set | Engine |
|---|---|
| `DATABASE_URL=mysql://…` or `mariadb://…` | MySQL / MariaDB |
| `DATABASE_URL=postgres://…` or `postgresql://…` | PostgreSQL |
| `MYSQL_*` variables | MySQL / MariaDB |
| `PG_*` variables | PostgreSQL |

These are startup errors, so a half-migrated configuration never runs:

- `MYSQL_*` and `PG_*` variables together.
- A MySQL URL with `PG_*` variables, or a PostgreSQL URL with `MYSQL_*` variables.
- A PostgreSQL URL with `PG_HOST`, `PG_PORT`, `PG_SOCKET`, `PG_USER`, `PG_PASSWORD` or `PG_DATABASE`: the URL already sets the connection. `PG_FORMAT` and `PG_GLOBALS` are fine next to it.
- Nothing at all. If libpq's own `PGHOST`-style variables are set, the error suggests the `PG_` name, since the underscore is easy to miss.

An empty variable counts as unset.

### MySQL and MariaDB

Either a `DATABASE_URL`:

```
DATABASE_URL=mysql://user:password@hostname:3306/database
DATABASE_URL=mysql://user:password@/database?unix_socket=/run/mysqld/mysqld.sock
```

or individual variables:

| Variable | Default | Description |
|---|---|---|
| `MYSQL_HOST` | | Server host name. Required unless `MYSQL_SOCKET` is set. |
| `MYSQL_PORT` | `3306` | Server port. |
| `MYSQL_SOCKET` | | Unix socket path. Takes precedence over `MYSQL_HOST`. |
| `MYSQL_USER` | | Required. |
| `MYSQL_PASSWORD` | | Passed to `mariadb-dump` through `MYSQL_PWD`. |
| `MYSQL_DATABASE` | | Required. A comma-separated list backs up each database in turn; `*` backs up every database but `mysql`, `sys`, `information_schema` and `performance_schema`. |
| `MYSQL_SINGLE_TRANSACTION` | `0` | Dump with `--single-transaction`. Accepts `1`/`0`, `true`/`false`, `yes`/`no`, `on`/`off`; anything else is rejected at startup. See [MySQL: consistency and locking](#mysql-consistency-and-locking). |

`DATABASE_URL` overrides the user, password, host and database variables. The port and socket from the URL apply only when present in it. **A MySQL URL is not percent-decoded**, as in every earlier version: write the password as it is.

### PostgreSQL

Either a `DATABASE_URL`, any [libpq connection URI](https://www.postgresql.org/docs/current/libpq-connect.html#LIBPQ-CONNSTRING-URIS) with a comma-separated database list as its path:

```
DATABASE_URL=postgres://user:password@hostname:5432/app,analytics?sslmode=require
DATABASE_URL=postgres://user:password@%2Frun%2Fpostgresql/app
```

or individual variables:

| Variable | Default | Description |
|---|---|---|
| `PG_HOST` | libpq's | Server host name. |
| `PG_PORT` | libpq's (`5432`) | Server port. |
| `PG_SOCKET` | | Unix socket directory, such as `/run/postgresql`. Takes precedence over `PG_HOST`. |
| `PG_USER` | libpq's | |
| `PG_PASSWORD` | | Passed to `pg_dump` through `PGPASSWORD`. |
| `PG_DATABASE` | | Required. A comma-separated list backs up each database in turn; `*` backs up every database that accepts connections, templates excluded (`postgres` included). |

With either form:

| Variable | Default | Description |
|---|---|---|
| `PG_FORMAT` | `custom` | `custom`: `pg_dump --format=custom`, compressed by `pg_dump`, stored as `.dump`, restored with `pg_restore`. `plain`: SQL, gzipped like a MySQL dump, stored as `.sql.gz`, restored with `psql`. |
| `PG_GLOBALS` | `0` | Also dump roles and tablespaces, which `pg_dump` leaves out, with `pg_dumpall --globals-only`, to their own object. Needs a superuser: see [PostgreSQL dumps](#postgresql-dumps). |

Unlike a MySQL URL, a PostgreSQL URL is percent-decoded, as libpq decodes it: write `@` in a password as `%40`. The password is taken out of the URL and passed through `PGPASSWORD`; put it in the user info, not in a `?password=` parameter, which would reach the `pg_dump` command line.

Variables left unset fall through to libpq, and so do libpq's own environment variables: set `PGSSLMODE`, `PGSSLROOTCERT`, `PGSSLCERT`, `PGSSLKEY`, `PGAPPNAME` and the like directly on the container. The `PG_*` variables win when both are set.

### S3

| Variable | Default | Description |
|---|---|---|
| `S3_BUCKET` | | Required. |
| `S3_PREFIX` | `/` | Key prefix, for example `backups/mysql`. |
| `S3_ENDPOINT_URL` | | Endpoint of an S3-compatible provider. Enables path-style addressing. |
| `S3_STORAGE_CLASS` | `STANDARD_IA` | Set it to an empty string for providers that reject storage classes (B2, Garage, MinIO). |
| `S3_PART_SIZE_MIB` | `16` | Upload part size, 5 to 5120. S3 allows 10,000 parts, so the default handles dumps up to about 156 GiB compressed. Memory use is about three times this value. |
| `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` | | Credentials. Any source the AWS SDK supports also works: IRSA, EKS Pod Identity, ECS task roles, instance profiles, `AWS_PROFILE`. |
| `AWS_REGION` or `AWS_DEFAULT_REGION` | `us-east-1` | Bucket region. |

### Timeouts

| Variable | Default | Description |
|---|---|---|
| `DUMP_TIMEOUT` | `600` | Seconds allowed for one attempt at one database, dump and upload together. |
| `UPLOAD_TIMEOUT` | `300` | Seconds allowed for each S3 request attempt. |
| `TIMEOUT_GRACE` | `30` | Seconds between `SIGTERM` and `SIGKILL` when stopping the dump. |

### Heartbeat

| Variable | Default | Description |
|---|---|---|
| `HEARTBEAT_URL` | | An `http://` or `https://` URL to GET once every database is uploaded, for a heartbeat monitor (UptimeRobot, Healthchecks.io, Better Stack) that alerts when the ping does not arrive. |

The ping is sent only when the whole run succeeded, so a failed, timed-out or interrupted backup alerts through the missing ping. Each attempt has 10 seconds, and a failed ping is retried twice, 2 seconds apart. **A ping that still fails does not fail the backup**: the run logs a warning and exits 0, because the backup is stored and a non-zero exit would make a Kubernetes Job dump everything again. The monitor alerts on the missing ping anyway. The URL is never logged, since it usually carries the monitor's token; keep it in a Secret.

## Timeouts, retries and Kubernetes deadlines

Without timeouts, a stalled dump or upload hangs forever, and on Kubernetes a CronJob with `concurrencyPolicy: Forbid` then skips every later run while looking idle. A Job-level `activeDeadlineSeconds` is the outer net; these timeouts are the inner one, and they say which step stalled.

- **A failed dump is not retried.** The dump tool exiting non-zero, or an attempt running past `DUMP_TIMEOUT`, ends the run with exit code 1.
- **A failed upload is retried once, with a fresh dump**, 5 seconds later. The SDK already retries each S3 request up to 3 times; this covers what that cannot.
- With `*`, a 30-second listing query runs once at the start. With `MYSQL_SINGLE_TRANSACTION` enabled, a 30-second engine check runs before each database.

Worst case per database: `2 × DUMP_TIMEOUT`, plus 30 s with `MYSQL_SINGLE_TRANSACTION`. At the defaults that is 1,230 s, and the total is that times the number of databases (the PostgreSQL globals count as one), plus 30 s with `*` and 34 s with `HEARTBEAT_URL`. **Keep it inside the Job's `activeDeadlineSeconds`**, or the deadline always fires first and these timeouts never get to say what stalled. For scale, a 3.0 GiB gzipped MySQL dump (9.4 GB raw) took 155 s to an S3-compatible server on the same LAN, so 600 s is about 4× that. Raise `DUMP_TIMEOUT` for a larger database, and raise `activeDeadlineSeconds` to match.

## MySQL: consistency and locking

By default `mariadb-dump` runs with `--lock-tables` (part of `--opt`): it takes `LOCK TABLES … READ LOCAL` over every table in a database and holds it for that database's whole dump. **Writes to those tables block for as long as the dump takes**, including any time spent waiting on a slow upload. In exchange, every table comes from the same point in time, whatever its storage engine.

`MYSQL_SINGLE_TRANSACTION=1` adds `--single-transaction`, which reads inside a transaction instead of locking. Writes are never blocked, and the backup user no longer needs `LOCK TABLES`.

The catch: **`--single-transaction` is only a consistent snapshot of transactional tables.** InnoDB tables are safe. MyISAM, Aria and MEMORY tables are read outside the transaction, so a write landing mid-dump can tear them silently. That is why it is off by default.

| | `MYSQL_SINGLE_TRANSACTION=0` (default) | `MYSQL_SINGLE_TRANSACTION=1` |
|---|---|---|
| Writes during the dump | Blocked | Not blocked |
| InnoDB consistency | Yes | Yes |
| MyISAM / Aria / MEMORY consistency | Yes | **No, can be torn** |
| Grants needed | `SELECT, SHOW VIEW, TRIGGER, LOCK TABLES` | `SELECT, SHOW VIEW, TRIGGER` |

**All-InnoDB deployment:** enable it. **Mixed engines:** convert the remaining tables to InnoDB (it has supported `FULLTEXT` since MySQL 5.6, the usual reason a MyISAM table is still around); until then, leaving it off trades a known cost for a known guarantee.

When enabled, the binary queries `information_schema` before each dump and logs every non-InnoDB table, so a mixed schema shows up in the backup log rather than in a failed restore:

```
Dumping MySQL database: mixeddb
Warning: MYSQL_SINGLE_TRANSACTION is enabled but 'mixeddb' has non-transactional tables:
Warning:   doc_recherche_index (MyISAM)
Warning: these are dumped outside the transaction and a concurrent write can tear them.
Warning: convert them to InnoDB, or unset MYSQL_SINGLE_TRANSACTION to lock instead.
```

The check is advisory: if it cannot run, it warns and the backup proceeds.

**Neither mode gives a consistent snapshot across databases.** Each database in the list gets its own dump, so its own lock or transaction window.

**MySQL servers are dumped with MariaDB's tool.** Since MariaDB 10.5.25, 10.6.18, 10.11.8 and 11.4.2, `mariadb-dump` writes a "sandbox mode" command on the first line of every dump, and Oracle's `mysql` client stops on it with `ERROR at line 1: Unknown command '\-'`. Restore with the `mariadb` client, or delete that line first: `gunzip | sed '1{/999999.*sandbox/d}' | mysql app`. `mariadb-dump` also writes no `SET @@GLOBAL.GTID_PURGED`, which matters when seeding a MySQL replica from the dump.

## PostgreSQL dumps

**Consistency.** `pg_dump` reads each database from one snapshot and takes only `ACCESS SHARE` locks: writes are never blocked, and there is no setting to choose. Schema changes (`ALTER TABLE`, `DROP TABLE`) on a table wait until that database's dump finishes. As with MySQL, each database is its own snapshot.

**Server versions.** The image ships `pg_dump` 18, which dumps servers from 9.2 through 18. A server newer than the client is refused by `pg_dump`, so a PostgreSQL 19 server needs a release of this image that ships the 19 client.

**Restoring.** A custom-format archive needs a `pg_restore` at least as new as the `pg_dump` that wrote it, so 18; this image has one (`docker run --rm -i --entrypoint pg_restore joanfabregat/db-s3-backup:v4 …`). Plain dumps start with psql's `\restrict` meta-command, added in the August 2025 minor releases: restore them with a `psql` from those releases or later. **On a server older than 17, a restore reports one error**, `unrecognized configuration parameter "transaction_timeout"`: `pg_dump` 17 and later start every dump with `SET transaction_timeout = 0`. It is harmless, and both tools carry on past it, unless you run `psql` with `ON_ERROR_STOP` or `pg_restore` with `--exit-on-error`. `pg_restore` then exits 1 with `errors ignored on restore: 1`.

**Globals.** `PG_GLOBALS=1` runs `pg_dumpall --globals-only` before the databases, to `<S3_PREFIX>/<timestamp>.globals.sql.gz`: `CREATE ROLE` statements with their password hashes, role memberships and settings, and tablespaces. Reading the password hashes needs a superuser. Managed services (RDS, Cloud SQL, Azure) give you none, so leave it off there; it fails the run otherwise.

**Grants.** The backup role needs `CONNECT` on each database and read access to everything in it. On PostgreSQL 14 and later:

```sql
CREATE ROLE backup LOGIN PASSWORD '…';
GRANT pg_read_all_data TO backup;
```

## Usage

### Docker

```bash
docker run --rm \
  -e DATABASE_URL="mysql://backup:password@db.example.com:3306/app" \
  -e S3_BUCKET="my-backup-bucket" \
  -e S3_PREFIX="mysql/daily" \
  -e AWS_ACCESS_KEY_ID="..." \
  -e AWS_SECRET_ACCESS_KEY="..." \
  -e AWS_REGION="us-west-1" \
  joanfabregat/db-s3-backup:v4
```

### Docker Compose

```yaml
services:
  mysql-backup:
    image: joanfabregat/db-s3-backup:v4
    environment:
      MYSQL_HOST: db
      MYSQL_USER: backup
      MYSQL_PASSWORD: password
      MYSQL_DATABASE: app,analytics
      S3_BUCKET: my-backup-bucket
      S3_PREFIX: backups/mysql
      AWS_ACCESS_KEY_ID: ...
      AWS_SECRET_ACCESS_KEY: ...
      AWS_REGION: us-west-1

  postgres-backup:
    image: joanfabregat/db-s3-backup:v4
    environment:
      PG_HOST: pg
      PG_USER: backup
      PG_PASSWORD: password
      PG_DATABASE: "*"
      PGSSLMODE: require
      S3_BUCKET: my-backup-bucket
      S3_PREFIX: backups/postgres
      AWS_ACCESS_KEY_ID: ...
      AWS_SECRET_ACCESS_KEY: ...
      AWS_REGION: us-west-1
```

### Backblaze B2 and other S3-compatible providers

```yaml
    environment:
      S3_ENDPOINT_URL: https://s3.us-west-004.backblazeb2.com
      S3_STORAGE_CLASS: ""
      AWS_REGION: us-west-004
```

### Kubernetes CronJob

One CronJob per database server. Each server then gets its own schedule, deadline, credentials and heartbeat monitor, and a server that is down does not hold back the others.

```yaml
apiVersion: batch/v1
kind: CronJob
metadata:
  name: mysql-backup
spec:
  schedule: "0 2 * * *"
  concurrencyPolicy: Forbid
  jobTemplate:
    spec:
      activeDeadlineSeconds: 3600
      backoffLimit: 0
      template:
        spec:
          restartPolicy: Never
          containers:
            - name: mysql-backup
              image: joanfabregat/db-s3-backup:v4
              resources:
                requests: { cpu: 250m, memory: 96Mi }
                limits: { memory: 192Mi }
              envFrom:
                - secretRef:
                    name: mysql-backup
              env:
                - name: MYSQL_SINGLE_TRANSACTION
                  value: "true"
                - name: S3_PREFIX
                  value: mysql/daily
```

No ephemeral-storage request is needed: nothing is written to disk.

## Objects

One run uses one UTC timestamp for every object:

```
<S3_PREFIX>/<YYYY-MM-DDTHHMMSSZ>.<ext>                  one database
<S3_PREFIX>/<database>/<YYYY-MM-DDTHHMMSSZ>.<ext>       several databases, or *
<S3_PREFIX>/<YYYY-MM-DDTHHMMSSZ>.globals.sql.gz         PostgreSQL globals (PG_GLOBALS=1)
```

`<ext>` is `sql.gz` for MySQL and PostgreSQL plain dumps, and `dump` for PostgreSQL custom-format archives. With `*` the per-database layout applies even when the server has a single database, so keys do not move when a second one appears. MySQL keys are identical to v3's and v4.0's, so existing lifecycle rules and restore scripts keep working.

Restore with:

```bash
# MySQL / MariaDB
aws s3 cp s3://my-backup-bucket/mysql/daily/2026-10-13T020000Z.sql.gz - | gunzip | mariadb app

# PostgreSQL, custom format (the default); pg_restore reads the archive from stdin
aws s3 cp s3://my-backup-bucket/postgres/app/2026-10-13T020000Z.dump - | pg_restore --no-owner -d app

# PostgreSQL, plain format, and the globals
aws s3 cp s3://my-backup-bucket/postgres/app/2026-10-13T020000Z.sql.gz - | gunzip | psql app
aws s3 cp s3://my-backup-bucket/postgres/2026-10-13T020000Z.globals.sql.gz - | gunzip | psql postgres
```

**Add a lifecycle rule with `AbortIncompleteMultipartUpload`** (one day is enough). Every failure path aborts its upload, but a container killed with `SIGKILL` cannot, and its parts are billed as storage until something removes them.

## Logs and exit codes

A MySQL run with `HEARTBEAT_URL` set logs:

```
db-s3-backup 4.1.0
Dumping MySQL database: app
Streaming dump to S3: s3://my-backup-bucket/mysql/daily/2026-10-13T020000Z.sql.gz
Uploaded successfully: 924.3 MiB compressed (4121.7 MiB raw) in 58 part(s), 124.6s.
mysqldump of 'app' waited 3.2s on the upload.
Backup complete.
Heartbeat sent.
```

A PostgreSQL run with `*` and `PG_GLOBALS=1`:

```
db-s3-backup 4.1.0
Found 2 database(s): app, postgres
Dumping PostgreSQL roles and tablespaces
Streaming dump to S3: s3://my-backup-bucket/postgres/2026-10-13T020000Z.globals.sql.gz
Uploaded successfully: 0.0 MiB compressed (0.0 MiB raw) in 1 part(s), 0.1s.
pg_dumpall of the PostgreSQL globals waited 0.0s on the upload.
Dumping PostgreSQL database: app
Streaming dump to S3: s3://my-backup-bucket/postgres/app/2026-10-13T020000Z.dump
Uploaded successfully: 812.6 MiB in 51 part(s), 98.2s.
pg_dump of 'app' waited 1.4s on the upload.
…
Backup complete.
```

| Exit code | Meaning |
|---|---|
| `0` | Every database was backed up. |
| `1` | Invalid configuration, `*` could not list the databases, or a database failed. Databases after it are not attempted. |
| `130`, `143` | Stopped by `SIGINT` or `SIGTERM`; the current upload was aborted. |

## Upgrading from v4.0

Nothing to change: v4.1 reads every v4.0 variable the same way, keeps the defaults, the object keys and the exit codes, and still installs `mariadb-client`. The image is published under both names; switching to `joanfabregat/db-s3-backup` is optional.

What is new or different:

- **PostgreSQL support**, through `postgres://` URLs or the `PG_*` variables.
- **`*`** in `MYSQL_DATABASE` (or a MySQL URL's path) backs up every database on the server.
- **`mariadb://`** is accepted as a MySQL URL scheme.
- **A configuration that sets both `MYSQL_*` and `PG_*` variables is now an error.** v4.0 ignored the `PG_*` ones. Check this if the backup container shares an `envFrom` with an application that uses `PG_*` names.
- **The binary is `/usr/local/bin/db-s3-backup`**, and the entrypoint points at it. `/usr/local/bin/mysql-s3-backup` remains as a symlink for deployments that override the entrypoint.
- **The first log line reads `db-s3-backup <version>`** instead of `mysql-s3-backup <version>`.
- **The image is about 5 MiB larger**, for the PostgreSQL client.

## Upgrading from v3

v4 replaced the shell script and `s5cmd` with the Rust binary. The environment variables and object keys are unchanged. What changed:

- **No temporary files.** Remove the ephemeral-storage sizing for `/tmp`; set a memory limit instead (about three times `S3_PART_SIZE_MIB` plus a few MiB).
- **`DUMP_TIMEOUT` now covers the dump and the upload together**, since they run as one stream. `UPLOAD_TIMEOUT` now applies to each S3 request. Recheck the budget in [Timeouts, retries and Kubernetes deadlines](#timeouts-retries-and-kubernetes-deadlines).
- **An upload failure retries the whole database** (2 attempts) instead of re-sending the file (3 attempts).
- **`/app/backup.sh`, `bash`, `gzip` and `s5cmd` are gone from the image.**
- **Checksums:** every part carries a Content-MD5. The AWS SDK's CRC checksums are only sent where S3 requires them, because not every compatible provider supports them.
- **New:** `S3_PART_SIZE_MIB`, and `HEARTBEAT_URL`, which replaces a sidecar container that pinged a monitor after the backup.

## Security

- Prefer workload identity (IRSA, EKS Pod Identity, instance roles) over static keys.
- Give the database user only what the dump needs: on MySQL, `SELECT, SHOW VIEW, TRIGGER, LOCK TABLES`, or `SELECT, SHOW VIEW, TRIGGER` with `MYSQL_SINGLE_TRANSACTION=1`; on PostgreSQL 14 and later, `pg_read_all_data`. Only `PG_GLOBALS=1` needs a superuser.
- Keep credentials in a secret store, and encrypt the bucket.
- Consider Object Lock on the bucket; incomplete dumps never become objects, so it cannot lock in a bad one.

## Troubleshooting

1. **`Error: dump of '…' failed: mysqldump failed (exit 2)`** or **`pg_dump failed`**: the tool's own message is printed just above. Usually a connection, grant or unknown-database error.
2. **`Access denied … when using LOCK TABLES`**: grant `LOCK TABLES`, or set `MYSQL_SINGLE_TRANSACTION=1`.
3. **Writes stall during a MySQL backup**: `mariadb-dump`'s default locking. See [MySQL: consistency and locking](#mysql-consistency-and-locking). If the log shows a long wait on the upload, the upload is extending the lock: use `MYSQL_SINGLE_TRANSACTION=1`, or a faster link to the bucket.
4. **`pg_dump: error: aborting because of server version mismatch`**: the server is newer than the image's `pg_dump` 18.
5. **`unrecognized configuration parameter "transaction_timeout"`** when restoring onto PostgreSQL 16 or older: harmless, see **Restoring** in [PostgreSQL dumps](#postgresql-dumps).
6. **`permission denied for table pg_authid`** with `PG_GLOBALS=1`: the role is not a superuser. Unset `PG_GLOBALS`.
7. **`ERROR at line 1: Unknown command '\-'`** when restoring a MySQL dump: see the end of [MySQL: consistency and locking](#mysql-consistency-and-locking).
8. **`exceeded 600s (DUMP_TIMEOUT)`**: raise `DUMP_TIMEOUT` and the Job's `activeDeadlineSeconds` together.
9. **`InvalidStorageClass` or similar on an S3-compatible provider**: set `S3_STORAGE_CLASS=""`.
10. **Container killed for memory**: raise the memory limit, or lower `S3_PART_SIZE_MIB`.

## Development

The code is a single binary crate: `src/config.rs` (environment), `src/dump.rs` (the streaming pipeline), `src/mysql.rs` and `src/postgres.rs` (the dump commands, the `*` listing, the MySQL engine check), `src/s3.rs` (multipart upload), `src/heartbeat.rs` (the success ping).

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test                     # unit tests; integration tests skip themselves
docker build -t db-s3-backup .
```

The integration tests in `tests/integration.rs` need an S3-compatible server and, for most of them, a MySQL or MariaDB server with the `mariadb` client on `PATH`, and a PostgreSQL server with `psql`, `pg_dump`, `pg_dumpall` and `pg_restore` on `PATH`:

```bash
export TEST_S3_ENDPOINT=http://127.0.0.1:7070 AWS_ACCESS_KEY_ID=... AWS_SECRET_ACCESS_KEY=...
export TEST_MYSQL_HOST=127.0.0.1 TEST_MYSQL_USER=root TEST_MYSQL_PASSWORD=...
export TEST_PG_HOST=127.0.0.1 TEST_PG_USER=postgres TEST_PG_PASSWORD=...
export TEST_REQUIRE_INTEGRATION=1
cargo test --test integration
cargo test --test integration -- --ignored --test-threads=1   # the * tests, on their own
```

Set `TEST_IMAGE=<image>` to run them through a Docker image instead of the Cargo-built binary. CI runs them against MySQL 8.4, MariaDB 11.4 and 11.8, and PostgreSQL 14, 17 and 18, with [versitygw](https://github.com/versity/versitygw) as the S3 server, and once more through the built image.

## License

[MIT](LICENSE)
