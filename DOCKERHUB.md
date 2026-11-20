# db-s3-backup

Backs up MySQL, MariaDB and PostgreSQL databases to Amazon S3 or any S3-compatible provider (Backblaze B2, Garage, MinIO, …). It runs the database's own dump tool and streams its output to the bucket as a multipart upload, without writing the dump to disk. The upload is completed only after the dump tool exits 0, and aborted otherwise, so **a truncated dump never becomes an object**.

**Formerly `mysql-s3-backup`.** From v4.1.0, every release is published under both [`joanfabregat/db-s3-backup`](https://hub.docker.com/r/joanfabregat/db-s3-backup) and [`joanfabregat/mysql-s3-backup`](https://hub.docker.com/r/joanfabregat/mysql-s3-backup), with the same digest and the same tags. v4.1 is fully compatible with v4.0: an existing deployment keeps working unchanged on either name.

Full documentation, source and changelog: [github.com/joanfabregat/db-s3-backup](https://github.com/joanfabregat/db-s3-backup).

## Tags

| Tag | Follows |
|---|---|
| `v4` | The latest 4.x release. Recommended. |
| `v4.1` | The latest 4.1.x release. |
| `v4.1.0` | One release, never moved. |
| `latest` | The latest release, including the next major version. |

Images are built for `linux/amd64` and `linux/arm64`, on Alpine, with `mariadb-dump` and `pg_dump` 18 (which dumps PostgreSQL 9.2 to 18).

## Features

- MySQL and MariaDB through `mariadb-dump`, PostgreSQL through `pg_dump`
- Streaming to S3 with no local disk use; memory stays around three upload parts (48 MiB by default) whatever the database size
- Several databases per run, or `*` for every database on the server
- PostgreSQL custom-format dumps (restore single tables with `pg_restore`) or plain SQL, and optionally roles and tablespaces
- Optional MySQL `--single-transaction` dumps that do not block writes
- Wall-clock timeouts and retries that fit inside a Kubernetes Job deadline, and a clean abort on `SIGTERM`
- A success ping for heartbeat monitors (UptimeRobot, Healthchecks.io, Better Stack)
- The database password goes through `MYSQL_PWD` or `PGPASSWORD`, never the command line

## Quick start

MySQL or MariaDB:

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

PostgreSQL, every database on the server:

```bash
docker run --rm \
  -e DATABASE_URL="postgres://backup:password@pg.example.com:5432/*?sslmode=require" \
  -e S3_BUCKET="my-backup-bucket" \
  -e S3_PREFIX="postgres/daily" \
  -e AWS_ACCESS_KEY_ID="..." \
  -e AWS_SECRET_ACCESS_KEY="..." \
  -e AWS_REGION="us-west-1" \
  joanfabregat/db-s3-backup:v4
```

## Configuration

The engine comes from the `DATABASE_URL` scheme (`mysql://`, `mariadb://`, `postgres://`, `postgresql://`) or, without a URL, from the prefix of the variables: `MYSQL_*` or `PG_*`. Setting both prefixes is a startup error.

| Variable | Default | Description |
|---|---|---|
| `DATABASE_URL` | | Connection URL. The path is the database, a comma-separated list, or `*`. |
| `MYSQL_HOST`, `MYSQL_PORT`, `MYSQL_SOCKET`, `MYSQL_USER`, `MYSQL_PASSWORD`, `MYSQL_DATABASE` | port `3306` | MySQL connection, instead of a URL. |
| `MYSQL_SINGLE_TRANSACTION` | `0` | Dump with `--single-transaction`. |
| `PG_HOST`, `PG_PORT`, `PG_SOCKET`, `PG_USER`, `PG_PASSWORD`, `PG_DATABASE` | libpq's | PostgreSQL connection, instead of a URL. libpq's own variables (`PGSSLMODE`, …) also apply. |
| `PG_FORMAT` | `custom` | `custom` (`.dump`, for `pg_restore`) or `plain` (`.sql.gz`, for `psql`). |
| `PG_GLOBALS` | `0` | Also dump roles and tablespaces with `pg_dumpall --globals-only`. Needs a superuser. |
| `S3_BUCKET` | | Required. |
| `S3_PREFIX` | `/` | Key prefix. |
| `S3_ENDPOINT_URL` | | Endpoint of an S3-compatible provider. Enables path-style addressing. |
| `S3_STORAGE_CLASS` | `STANDARD_IA` | Set it empty for providers that reject storage classes (B2, Garage, MinIO). |
| `S3_PART_SIZE_MIB` | `16` | Upload part size, 5 to 5120. |
| `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION` | region `us-east-1` | Credentials and region. IRSA, EKS Pod Identity, instance profiles and `AWS_PROFILE` also work. |
| `DUMP_TIMEOUT` | `600` | Seconds for one attempt at one database. |
| `UPLOAD_TIMEOUT` | `300` | Seconds for each S3 request attempt. |
| `HEARTBEAT_URL` | | URL to GET once every database is uploaded. |

A MySQL URL is not percent-decoded; a PostgreSQL URL is, as libpq does. The [full README](https://github.com/joanfabregat/db-s3-backup#configuration) covers every variable, locking and consistency, retries, and Kubernetes CronJob examples.

## Objects and restoring

```
<S3_PREFIX>/<YYYY-MM-DDTHHMMSSZ>.<ext>                  one database
<S3_PREFIX>/<database>/<YYYY-MM-DDTHHMMSSZ>.<ext>       several databases, or *
<S3_PREFIX>/<YYYY-MM-DDTHHMMSSZ>.globals.sql.gz         PostgreSQL globals
```

```bash
# MySQL / MariaDB
aws s3 cp s3://my-backup-bucket/mysql/daily/2026-10-13T020000Z.sql.gz - | gunzip | mariadb app

# PostgreSQL, custom format (the default)
aws s3 cp s3://my-backup-bucket/postgres/daily/app/2026-10-13T020000Z.dump - | pg_restore --no-owner -d app
```

Add a lifecycle rule with `AbortIncompleteMultipartUpload` on the bucket: a container killed with `SIGKILL` cannot abort its upload, and the parts are billed until something removes them.

## License

MIT
