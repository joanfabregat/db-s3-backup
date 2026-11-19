//! PostgreSQL: the pg_dump and pg_dumpall commands and the database listing
//! for `*`.
//!
//! Every value from the configuration reaches libpq through `--dbname`, as a
//! URI or a quoted conninfo string, so a database name containing `=` or
//! starting with `postgres://` is never reinterpreted as connection options.
//! The password goes through PGPASSWORD, never the command line. Anything
//! left unset falls through to libpq's own environment (PGSSLMODE,
//! PGSSLROOTCERT, ...) and defaults.

use std::path::PathBuf;
use std::process::Stdio;

use tokio::process::Command;
use tokio::time::timeout;

use crate::config::{METADATA_QUERY_TIMEOUT, PgConnection, PgFormat, Postgres};
use crate::dump::find_program;

/// The database pg_dumpall and the listing connect to. pg_dumpall ignores the
/// database in --dbname anyway; the listing needs one to exist.
const MAINTENANCE_DB: &str = "postgres";

/// Percent-encodes `name` for the path of a URI: everything but RFC 3986's
/// unreserved characters.
fn uri_encode(name: &str) -> String {
    name.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Quotes a conninfo value: single quotes, with `\` and `'` escaped.
fn conninfo_quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// The `--dbname` value that connects to `db`.
fn dbname_arg(cfg: &Postgres, db: &str) -> String {
    match &cfg.connection {
        PgConnection::Url { base, query } => format!("{base}/{}{query}", uri_encode(db)),
        PgConnection::Params { host, port, user } => {
            let mut parts = Vec::new();
            for (key, value) in [("host", host), ("port", port), ("user", user)] {
                if let Some(value) = value {
                    parts.push(format!("{key}={}", conninfo_quote(value)));
                }
            }
            parts.push(format!("dbname={}", conninfo_quote(db)));
            parts.join(" ")
        }
    }
}

fn command(program: PathBuf, cfg: &Postgres, db: &str) -> Command {
    let mut command = Command::new(program);
    command
        // Never prompt: a missing password is an error, not a hang.
        .arg("--no-password")
        .arg(format!("--dbname={}", dbname_arg(cfg, db)))
        .stdin(Stdio::null())
        .kill_on_drop(true);
    if let Some(password) = &cfg.password {
        command.env("PGPASSWORD", password);
    }
    command
}

fn program(name: &str) -> Result<PathBuf, String> {
    find_program(&[name]).ok_or_else(|| format!("{name} is not on PATH"))
}

/// The dump of `db`: custom format (compressed by pg_dump) or plain SQL.
pub fn dump_command(cfg: &Postgres, db: &str) -> Result<Command, String> {
    let mut command = command(program("pg_dump")?, cfg, db);
    command.arg(match cfg.format {
        PgFormat::Custom => "--format=custom",
        PgFormat::Plain => "--format=plain",
    });
    Ok(command)
}

/// Roles and tablespaces, which pg_dump leaves out, as plain SQL.
pub fn globals_command(cfg: &Postgres) -> Result<Command, String> {
    let mut command = command(program("pg_dumpall")?, cfg, MAINTENANCE_DB);
    command.arg("--globals-only");
    Ok(command)
}

/// Every database that accepts connections, templates excluded, for `*`.
pub async fn list_databases(cfg: &Postgres) -> Result<Vec<String>, String> {
    let mut command = command(program("psql")?, cfg, MAINTENANCE_DB);
    command
        .args([
            "--no-psqlrc",
            "--no-align",
            "--tuples-only",
            "--quiet",
            "--set=ON_ERROR_STOP=1",
            "--command=SELECT datname FROM pg_database \
             WHERE datallowconn AND NOT datistemplate ORDER BY datname",
        ])
        .stderr(Stdio::inherit());
    let output = timeout(METADATA_QUERY_TIMEOUT, command.output())
        .await
        .map_err(|_| format!("no answer in {}s", METADATA_QUERY_TIMEOUT.as_secs()))?
        .map_err(|e| format!("could not run psql: {e}"))?;
    if !output.status.success() {
        return Err(format!("listing pg_database failed ({})", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(host: Option<&str>, port: Option<&str>, user: Option<&str>) -> Postgres {
        Postgres {
            connection: PgConnection::Params {
                host: host.map(str::to_owned),
                port: port.map(str::to_owned),
                user: user.map(str::to_owned),
            },
            password: Some("hunter2".into()),
            format: PgFormat::Custom,
            globals: false,
        }
    }

    #[test]
    fn conninfo_from_variables() {
        let cfg = params(Some("db"), Some("5433"), Some("back'up"));
        assert_eq!(
            dbname_arg(&cfg, r"a=b\c"),
            r"host='db' port='5433' user='back\'up' dbname='a=b\\c'"
        );
        assert_eq!(dbname_arg(&params(None, None, None), "app"), "dbname='app'");
    }

    #[test]
    fn uri_from_database_url() {
        let cfg = Postgres {
            connection: PgConnection::Url {
                base: "postgres://u@h:5432".into(),
                query: "?sslmode=require".into(),
            },
            ..params(None, None, None)
        };
        assert_eq!(
            dbname_arg(&cfg, "my app/é"),
            "postgres://u@h:5432/my%20app%2F%C3%A9?sslmode=require"
        );
    }

    #[test]
    fn password_is_not_an_argument() {
        let cfg = params(Some("db"), None, Some("u"));
        let command = command(PathBuf::from("/bin/true"), &cfg, "app");
        let std = command.as_std();
        assert!(
            !std.get_args()
                .any(|a| a.to_string_lossy().contains("hunter2"))
        );
        let pwd = std
            .get_envs()
            .find(|(k, _)| *k == "PGPASSWORD")
            .and_then(|(_, v)| v);
        assert_eq!(pwd, Some(std::ffi::OsStr::new("hunter2")));
    }
}
