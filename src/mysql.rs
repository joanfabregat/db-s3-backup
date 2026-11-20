//! MySQL and MariaDB: the mariadb-dump command, the database listing for
//! `*`, and the --single-transaction engine check.

use std::path::PathBuf;
use std::process::Stdio;

use tokio::process::Command;
use tokio::time::timeout;

use crate::config::{METADATA_QUERY_TIMEOUT, Mysql, Server};
use crate::dump::find_program;

/// Schemas `*` leaves out: the server's own, not application data.
const SYSTEM_SCHEMAS: [&str; 4] = ["information_schema", "performance_schema", "mysql", "sys"];

/// Shared by the dump, the listing and the engine check, so they all talk to
/// the same server, as the same user.
fn connection_args(cfg: &Mysql) -> Vec<String> {
    let mut args = vec!["-u".to_owned(), cfg.user.clone()];
    match &cfg.server {
        Server::Socket(socket) => args.extend(["--socket".to_owned(), socket.clone()]),
        Server::Tcp { host, port } => {
            args.extend(["-h".to_owned(), host.clone(), "-P".to_owned(), port.clone()])
        }
    }
    args
}

/// A client command with the connection arguments. The password goes through
/// MYSQL_PWD rather than `--password=` so it does not show up in `ps`.
fn client_command(program: PathBuf, cfg: &Mysql) -> Command {
    let mut command = Command::new(program);
    command
        .args(connection_args(cfg))
        .stdin(Stdio::null())
        .kill_on_drop(true);
    if let Some(password) = &cfg.password {
        command.env("MYSQL_PWD", password);
    }
    command
}

/// Alpine's mariadb-client installs `mariadb` and `mariadb-dump` and has so
/// far also shipped `mysql` and `mysqldump` compatibility symlinks; resolving
/// whichever exists avoids betting on those symlinks outliving an update.
fn client() -> Option<PathBuf> {
    find_program(&["mariadb", "mysql"])
}

/// The dump of `db`, writing SQL to stdout.
pub fn dump_command(cfg: &Mysql, db: &str) -> Result<Command, String> {
    let program = find_program(&["mariadb-dump", "mysqldump"])
        .ok_or("neither mariadb-dump nor mysqldump is on PATH")?;
    let mut command = client_command(program, cfg);
    command.arg("--no-tablespaces");
    if cfg.single_transaction {
        // --single-transaction turns --lock-tables off by itself, so there is
        // nothing else to unset here.
        command.arg("--single-transaction");
    }
    command.arg(db);
    Ok(command)
}

/// Every database on the server but the system schemas, for `*`.
pub async fn list_databases(cfg: &Mysql) -> Result<Vec<String>, String> {
    let program = client().ok_or("neither mariadb nor mysql is on PATH")?;
    let mut command = client_command(program, cfg);
    command
        .args(["--batch", "--skip-column-names", "-e", "SHOW DATABASES"])
        .stderr(Stdio::inherit());
    let output = timeout(METADATA_QUERY_TIMEOUT, command.output())
        .await
        .map_err(|_| format!("no answer in {}s", METADATA_QUERY_TIMEOUT.as_secs()))?
        .map_err(|e| format!("could not run the client: {e}"))?;
    if !output.status.success() {
        return Err(format!("SHOW DATABASES failed ({})", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|name| !name.is_empty() && !SYSTEM_SCHEMAS.contains(name))
        .map(str::to_owned)
        .collect())
}

/// Names the tables in `db` that --single-transaction cannot snapshot
/// consistently.
///
/// Advisory only: every failure path here warns and returns. A backup must not
/// be abandoned because a diagnostic query could not run -- that would turn a
/// missing warning into a missing backup, which is strictly worse than the
/// risk it warns about.
pub async fn warn_non_transactional_tables(cfg: &Mysql, db: &str) {
    let Some(client) = client() else {
        eprintln!(
            "Warning: no MySQL client found, skipping the non-transactional table check for '{db}'"
        );
        return;
    };

    // The database name is passed as an argument and read back with
    // DATABASE() rather than interpolated into the SQL, so a name containing
    // a quote cannot break out of the statement.
    let mut command = client_command(client, cfg);
    command
        .args(["--batch", "--skip-column-names", db, "-e"])
        .arg(
            "SELECT CONCAT(TABLE_NAME, ' (', IFNULL(ENGINE, 'unknown engine'), ')') \
             FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = DATABASE() \
               AND TABLE_TYPE = 'BASE TABLE' \
               AND (ENGINE IS NULL OR ENGINE <> 'InnoDB') \
             ORDER BY TABLE_NAME",
        )
        .stderr(Stdio::inherit());

    let output = match timeout(METADATA_QUERY_TIMEOUT, command.output()).await {
        Ok(Ok(output)) if output.status.success() => output,
        _ => {
            eprintln!("Warning: could not check '{db}' for non-transactional tables, continuing");
            return;
        }
    };

    let tables = String::from_utf8_lossy(&output.stdout);
    let tables: Vec<&str> = tables.lines().filter(|l| !l.is_empty()).collect();
    if tables.is_empty() {
        return;
    }
    eprintln!(
        "Warning: MYSQL_SINGLE_TRANSACTION is enabled but '{db}' has non-transactional tables:"
    );
    for table in tables {
        eprintln!("Warning:   {table}");
    }
    eprintln!(
        "Warning: these are dumped outside the transaction and a concurrent write can tear them."
    );
    eprintln!(
        "Warning: convert them to InnoDB, or unset MYSQL_SINGLE_TRANSACTION to lock instead."
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Engine};
    use std::collections::HashMap;

    fn config(extra: &[(&str, &str)]) -> Mysql {
        let mut vars: HashMap<String, String> = [
            ("MYSQL_USER", "backup"),
            ("MYSQL_HOST", "db"),
            ("MYSQL_DATABASE", "app"),
            ("S3_BUCKET", "bucket"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        vars.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        match Config::from_vars(&vars).unwrap().engine {
            Engine::Mysql(mysql) => mysql,
            Engine::Postgres(_) => panic!("not MySQL"),
        }
    }

    #[test]
    fn tcp_connection_args() {
        assert_eq!(
            connection_args(&config(&[])),
            ["-u", "backup", "-h", "db", "-P", "3306"]
        );
    }

    #[test]
    fn socket_connection_args() {
        let cfg = config(&[("MYSQL_SOCKET", "/run/mysqld/mysqld.sock")]);
        assert_eq!(
            connection_args(&cfg),
            ["-u", "backup", "--socket", "/run/mysqld/mysqld.sock"]
        );
    }

    #[test]
    fn password_is_not_an_argument() {
        let cfg = config(&[("MYSQL_PASSWORD", "hunter2")]);
        assert!(!connection_args(&cfg).iter().any(|a| a.contains("hunter2")));
        let command = client_command(PathBuf::from("/bin/true"), &cfg);
        let pwd = command
            .as_std()
            .get_envs()
            .find(|(k, _)| *k == "MYSQL_PWD")
            .and_then(|(_, v)| v);
        assert_eq!(pwd, Some(std::ffi::OsStr::new("hunter2")));
    }
}
