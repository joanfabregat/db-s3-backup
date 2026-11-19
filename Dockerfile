# The builder always runs on the runner's own architecture and cross-compiles
# for the target with xx (Docker's cross-compilation helpers), so the arm64
# image does not compile Rust under QEMU emulation.
FROM --platform=$BUILDPLATFORM tonistiigi/xx:1.9.0 AS xx

# The rust:1 tag is deliberate: every rebuild picks up the latest stable
# toolchain, so a fix only needs a new image, not a Dockerfile edit.
FROM --platform=$BUILDPLATFORM rust:1-alpine3.24 AS builder

COPY --from=xx / /

# clang and lld are the cross linker and C compiler xx-cargo points Cargo at.
# cmake and perl are aws-lc-sys's fallback build path, used when its bundled
# build script does not cover the target.
RUN apk add --no-cache build-base clang lld cmake perl

# cargo-auditable embeds the dependency list in the binary, so Trivy can
# report advisories in the Rust crates inside the image, not only in Alpine.
RUN cargo install cargo-auditable --locked --version 0.7.7

ARG TARGETPLATFORM
# The target's C runtime, for aws-lc-sys and the final link.
RUN xx-apk add --no-cache musl-dev gcc

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN xx-cargo auditable build --release --locked \
    && target="target/$(xx-cargo --print-target-triple)/release/db-s3-backup" \
    && xx-verify --static "$target" \
    && cp "$target" /db-s3-backup

FROM alpine:3.24

# mariadb-client provides mariadb-dump (mysqldump) and the mariadb client the
# MYSQL_SINGLE_TRANSACTION engine check and `*` use. postgresql18-client
# provides pg_dump, pg_dumpall and psql; pg_dump dumps servers from 9.2 up
# to its own major version, so it tracks the newest PostgreSQL release.
# The version checks fail the build if a tool is missing from PATH.
RUN apk upgrade --no-cache \
    && apk add --no-cache mariadb-client postgresql18-client \
    && mariadb-dump --version && pg_dump --version && pg_dumpall --version && psql --version

COPY --from=builder /db-s3-backup /usr/local/bin/db-s3-backup
# v4.0 and earlier shipped the binary under this name; keep it for
# deployments that override the entrypoint with that path.
RUN ln -s db-s3-backup /usr/local/bin/mysql-s3-backup

USER nobody

ENTRYPOINT ["/usr/local/bin/db-s3-backup"]
