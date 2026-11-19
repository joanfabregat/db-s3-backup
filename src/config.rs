//! Configuration from the environment.
//!
//! The engine comes from the `DATABASE_URL` scheme or, without one, from the
//! prefix of the variables that are set: `MYSQL_*` or `PG_*`. The MySQL
//! variable names, defaults, `DATABASE_URL` parsing and object keys match v3
//! and v4.0, so an existing deployment only changes its image tag.

use std::collections::HashMap;
use std::time::Duration;

use hyper::Uri;

use crate::heartbeat;

/// Whole-database attempts. Only a failed upload is retried; a failed or
/// timed-out dump is not, matching v3.
pub const MAX_ATTEMPTS: u32 = 2;

/// Deliberately not tunable: the engine check and the database listing are
/// metadata lookups, not data reads. If one has not answered in 30s the
/// server is in no state to be dumped, and DUMP_TIMEOUT would say so a moment
/// later.
pub const METADATA_QUERY_TIMEOUT: Duration = Duration::from_secs(30);

const MIB: usize = 1024 * 1024;

/// S3 rejects non-final parts under 5 MiB and any part over 5 GiB.
const MIN_PART_SIZE_MIB: usize = 5;
const MAX_PART_SIZE_MIB: usize = 5 * 1024;

const MYSQL_VARS: [&str; 7] = [
    "MYSQL_HOST",
    "MYSQL_PORT",
    "MYSQL_SOCKET",
    "MYSQL_USER",
    "MYSQL_PASSWORD",
    "MYSQL_DATABASE",
    "MYSQL_SINGLE_TRANSACTION",
];

const PG_CONNECTION_VARS: [&str; 6] = [
    "PG_HOST",
    "PG_PORT",
    "PG_SOCKET",
    "PG_USER",
    "PG_PASSWORD",
    "PG_DATABASE",
];

const PG_OPTION_VARS: [&str; 2] = ["PG_FORMAT", "PG_GLOBALS"];

/// libpq's own variables. pg_dump reads them from the environment, but they
/// do not select the engine: only named in the error when nothing else is
/// configured, since `PGHOST` for `PG_HOST` is the likely typo.
const LIBPQ_VARS: [&str; 5] = ["PGHOST", "PGPORT", "PGUSER", "PGPASSWORD", "PGDATABASE"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Server {
    Tcp { host: String, port: String },
    Socket(String),
}

#[derive(Debug, Clone)]
pub struct Mysql {
    pub user: String,
    pub password: Option<String>,
    pub server: Server,
    pub single_transaction: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgFormat {
    /// `pg_dump -Fc`: compressed by pg_dump, restored with pg_restore.
    Custom,
    /// Plain SQL, gzipped by the binary like a MySQL dump.
    Plain,
}

/// Where pg_dump connects. The password is kept apart, in [`Postgres`], and
/// reaches pg_dump through PGPASSWORD, never its command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PgConnection {
    /// `DATABASE_URL` with its password and database list removed: `base` is
    /// `scheme://[user@]hosts`, `query` is empty or starts with `?`.
    Url { base: String, query: String },
    /// `PG_*` variables. Unset fields are left to libpq, which falls back to
    /// PGHOST, PGPORT, PGUSER and its defaults. `host` is a socket directory
    /// when PG_SOCKET is set.
    Params {
        host: Option<String>,
        port: Option<String>,
        user: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct Postgres {
    pub connection: PgConnection,
    pub password: Option<String>,
    pub format: PgFormat,
    /// Also dump roles and tablespaces with `pg_dumpall --globals-only`.
    pub globals: bool,
}

#[derive(Debug, Clone)]
pub enum Engine {
    Mysql(Mysql),
    Postgres(Postgres),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Databases {
    Named(Vec<String>),
    /// `*`: every database on the server, listed at the start of the run.
    All,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub engine: Engine,
    pub databases: Databases,
    pub bucket: String,
    pub prefix: String,
    pub endpoint_url: Option<String>,
    pub storage_class: Option<String>,
    pub part_size: usize,
    /// Wall-clock limit for one attempt at one database: dump and upload
    /// together, since they run as one stream.
    pub dump_timeout: Duration,
    /// Limit for each S3 request attempt.
    pub upload_timeout: Duration,
    /// Time between TERM and KILL when stopping the dump.
    pub timeout_grace: Duration,
    /// Pinged once every database is uploaded.
    pub heartbeat_url: Option<Uri>,
}

/// One object a run uploads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Database(String),
    /// PostgreSQL roles and tablespaces, from `pg_dumpall --globals-only`.
    PgGlobals,
}

impl Target {
    /// How log lines name it: `'app'`, with quotes, as in v4.
    pub fn label(&self) -> String {
        match self {
            Target::Database(db) => format!("'{db}'"),
            Target::PgGlobals => "the PostgreSQL globals".to_owned(),
        }
    }
}

struct Vars<'a>(&'a HashMap<String, String>);

impl Vars<'_> {
    /// The value if set and non-empty, like `${VAR:-}` in the shell.
    fn get(&self, name: &str) -> Option<&str> {
        self.0
            .get(name)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    /// The value if set, even when empty, like `${VAR-default}`.
    fn raw(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    /// The names in `names` that are set and non-empty.
    fn set<'n>(&self, names: &[&'n str]) -> Vec<&'n str> {
        names
            .iter()
            .copied()
            .filter(|name| self.get(name).is_some())
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Mysql,
    Postgres,
}

fn url_kind(url: &str) -> Result<Kind, String> {
    if url.starts_with("mysql://") || url.starts_with("mariadb://") {
        Ok(Kind::Mysql)
    } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        Ok(Kind::Postgres)
    } else {
        Err(
            "DATABASE_URL must use the mysql://, mariadb://, postgres:// or postgresql:// scheme"
                .into(),
        )
    }
}

/// Connection fields as a MySQL `DATABASE_URL` sets them. `None` leaves the
/// matching `MYSQL_*` variable in charge.
#[derive(Debug, Default, PartialEq, Eq)]
struct UrlParts {
    user: String,
    password: String,
    host: String,
    port: Option<String>,
    database: String,
    socket: Option<String>,
}

/// The v3 parser, unchanged: no percent-decoding, so a password containing
/// `%` keeps working.
fn parse_mysql_url(url: &str) -> Result<UrlParts, String> {
    let rest = url
        .strip_prefix("mysql://")
        .or_else(|| url.strip_prefix("mariadb://"))
        .ok_or("DATABASE_URL must use mysql:// scheme")?;
    let (userinfo, rest) = rest
        .split_once('@')
        .ok_or("DATABASE_URL must include a user: mysql://user[:password]@host[:port]/database")?;
    let (user, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
    let (hostport_path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (hostport, database) = hostport_path.split_once('/').unwrap_or((hostport_path, ""));
    let (host, port) = match hostport.split_once(':') {
        Some((host, port)) => (host, Some(port.to_owned())),
        None => (hostport, None),
    };
    let socket = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("unix_socket="))
        .map(str::to_owned);

    Ok(UrlParts {
        user: user.to_owned(),
        password: password.to_owned(),
        host: host.to_owned(),
        port,
        database: database.to_owned(),
        socket,
    })
}

/// A PostgreSQL `DATABASE_URL`: the connection without password or
/// database, the decoded password, and the database list, still encoded.
#[derive(Debug, PartialEq, Eq)]
struct PgUrl {
    connection: PgConnection,
    password: Option<String>,
    databases: String,
}

/// Splits a libpq URI as libpq does: the user info ends at the first `@`
/// before the path, and the password at the first `:` in it. Percent-escapes
/// in the password and database names are decoded, as libpq decodes them.
/// Hosts and query parameters are passed through untouched.
fn parse_postgres_url(url: &str) -> Result<PgUrl, String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or("DATABASE_URL must use postgres:// scheme")?;
    let (before_query, query) = match rest.find('?') {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    let (authority, path) = before_query.split_once('/').unwrap_or((before_query, ""));
    let (userinfo, hosts) = match authority.split_once('@') {
        Some((userinfo, hosts)) => (Some(userinfo), hosts),
        None => (None, authority),
    };
    let (user, password) = match userinfo {
        Some(userinfo) => match userinfo.split_once(':') {
            Some((user, password)) => (Some(user), Some(password)),
            None => (Some(userinfo), None),
        },
        None => (None, None),
    };
    let password = password
        .map(|p| percent_decode(p, "the DATABASE_URL password"))
        .transpose()?
        .filter(|p| !p.is_empty());
    let user = user.filter(|u| !u.is_empty());
    let base = match user {
        Some(user) => format!("{scheme}://{user}@{hosts}"),
        None => format!("{scheme}://{hosts}"),
    };
    // Still percent-encoded: the list is split on commas before decoding, so
    // an encoded comma (%2C) stays inside its name.
    let databases = path.to_owned();
    Ok(PgUrl {
        connection: PgConnection::Url {
            base,
            query: query.to_owned(),
        },
        password,
        databases,
    })
}

fn percent_decode(value: &str, what: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or_else(|| format!("{what} has an invalid percent-escape"))?;
            out.push(hex);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| format!("{what} is not valid UTF-8 once decoded"))
}

/// Splits a comma-separated list. With `decode`, each name is percent-decoded
/// after the split, for a libpq URI path.
fn parse_databases(name: &str, value: &str, decode: bool) -> Result<Databases, String> {
    let databases: Vec<String> = value
        .split(',')
        .map(str::trim)
        .filter(|db| !db.is_empty())
        .map(|db| match decode {
            true => percent_decode(db, "a DATABASE_URL database name"),
            false => Ok(db.to_owned()),
        })
        .collect::<Result<_, _>>()?;
    if databases.is_empty() {
        return Err(format!("{name} has no database names: '{value}'"));
    }
    if databases.iter().any(|db| db == "*") {
        if databases.len() > 1 {
            return Err(format!(
                "{name} cannot combine '*' with database names: '{value}'"
            ));
        }
        return Ok(Databases::All);
    }
    Ok(Databases::Named(databases))
}

fn parse_bool(name: &str, value: &str) -> Result<bool, String> {
    match value.to_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(format!(
            "{name} must be a boolean (1/0, true/false, yes/no, on/off), got '{value}'"
        )),
    }
}

fn parse_seconds(
    name: &str,
    value: Option<&str>,
    default: u64,
    allow_zero: bool,
) -> Result<Duration, String> {
    let Some(value) = value else {
        return Ok(Duration::from_secs(default));
    };
    match value.parse::<u64>() {
        Ok(0) if !allow_zero => Err(format!("{name} must be greater than 0")),
        Ok(secs) => Ok(Duration::from_secs(secs)),
        Err(_) => Err(format!(
            "{name} must be a whole number of seconds, got '{value}'"
        )),
    }
}

fn parse_part_size(value: Option<&str>) -> Result<usize, String> {
    let Some(value) = value else {
        return Ok(16 * MIB);
    };
    match value.parse::<usize>() {
        Ok(mib) if (MIN_PART_SIZE_MIB..=MAX_PART_SIZE_MIB).contains(&mib) => Ok(mib * MIB),
        _ => Err(format!(
            "S3_PART_SIZE_MIB must be a whole number from {MIN_PART_SIZE_MIB} to {MAX_PART_SIZE_MIB}, got '{value}'"
        )),
    }
}

/// Picks the engine and rejects configurations that mix the two.
fn engine_kind(vars: &Vars) -> Result<Kind, String> {
    let mysql_set = vars.set(&MYSQL_VARS);
    let pg_set = vars.set(&[PG_CONNECTION_VARS.as_slice(), &PG_OPTION_VARS].concat());
    if !mysql_set.is_empty() && !pg_set.is_empty() {
        return Err(format!(
            "Both MySQL and PostgreSQL variables are set ({}; {}): configure one engine",
            mysql_set.join(", "),
            pg_set.join(", ")
        ));
    }

    if let Some(url) = vars.get("DATABASE_URL") {
        let kind = url_kind(url)?;
        match kind {
            Kind::Mysql if !pg_set.is_empty() => Err(format!(
                "DATABASE_URL is a MySQL URL but PostgreSQL variables are set ({})",
                pg_set.join(", ")
            )),
            Kind::Postgres if !mysql_set.is_empty() => Err(format!(
                "DATABASE_URL is a PostgreSQL URL but MySQL variables are set ({})",
                mysql_set.join(", ")
            )),
            Kind::Postgres if !vars.set(&PG_CONNECTION_VARS).is_empty() => Err(format!(
                "DATABASE_URL already sets the PostgreSQL connection; unset {}",
                vars.set(&PG_CONNECTION_VARS).join(", ")
            )),
            _ => Ok(kind),
        }
    } else if !pg_set.is_empty() {
        Ok(Kind::Postgres)
    } else if !mysql_set.is_empty() {
        Ok(Kind::Mysql)
    } else {
        let mut message =
            "No database configured: set DATABASE_URL, the MYSQL_* variables or the PG_* variables"
                .to_owned();
        if let Some(found) = vars.set(&LIBPQ_VARS).first() {
            let suggestion = format!("PG_{}", &found[2..]);
            message.push_str(&format!(" (found {found}: did you mean {suggestion}?)"));
        }
        Err(message)
    }
}

/// The MySQL connection, exactly as v4 read it.
fn mysql_config(vars: &Vars) -> Result<(Mysql, Databases), String> {
    let single_transaction = match vars.get("MYSQL_SINGLE_TRANSACTION") {
        Some(value) => parse_bool("MYSQL_SINGLE_TRANSACTION", value)?,
        None => false,
    };

    let mut user = vars.get("MYSQL_USER").map(str::to_owned);
    let mut password = vars.get("MYSQL_PASSWORD").map(str::to_owned);
    let mut host = vars.get("MYSQL_HOST").map(str::to_owned);
    let mut port = vars.get("MYSQL_PORT").unwrap_or("3306").to_owned();
    let mut database = vars.get("MYSQL_DATABASE").map(str::to_owned);
    let mut socket = vars.get("MYSQL_SOCKET").map(str::to_owned);

    if let Some(url) = vars.get("DATABASE_URL") {
        let parts = parse_mysql_url(url)?;
        let non_empty = |s: String| Some(s).filter(|s| !s.is_empty());
        user = non_empty(parts.user);
        password = non_empty(parts.password);
        host = non_empty(parts.host);
        database = non_empty(parts.database);
        if let Some(p) = parts.port {
            port = p;
        }
        if let Some(s) = parts.socket {
            socket = non_empty(s);
        }
    }

    let database = database.ok_or("Missing required environment variable MYSQL_DATABASE")?;
    let user = user.ok_or("Missing required environment variable MYSQL_USER")?;
    let server = match (socket, host) {
        (Some(socket), _) => Server::Socket(socket),
        (None, Some(host)) => Server::Tcp { host, port },
        (None, None) => {
            return Err("Missing required environment variable MYSQL_HOST or MYSQL_SOCKET".into());
        }
    };
    let databases = parse_databases("MYSQL_DATABASE", &database, false)?;
    Ok((
        Mysql {
            user,
            password,
            server,
            single_transaction,
        },
        databases,
    ))
}

fn postgres_config(vars: &Vars) -> Result<(Postgres, Databases), String> {
    let format = match vars.get("PG_FORMAT").map(str::to_lowercase).as_deref() {
        None | Some("custom") => PgFormat::Custom,
        Some("plain") => PgFormat::Plain,
        Some(_) => {
            return Err(format!(
                "PG_FORMAT must be custom or plain, got '{}'",
                vars.get("PG_FORMAT").unwrap_or_default()
            ));
        }
    };
    let globals = match vars.get("PG_GLOBALS") {
        Some(value) => parse_bool("PG_GLOBALS", value)?,
        None => false,
    };

    let (connection, password, databases) = match vars.get("DATABASE_URL") {
        Some(url) => {
            let url = parse_postgres_url(url)?;
            (
                url.connection,
                url.password,
                parse_databases("DATABASE_URL", &url.databases, true)?,
            )
        }
        None => {
            let database = vars
                .get("PG_DATABASE")
                .ok_or("Missing required environment variable PG_DATABASE")?;
            let connection = PgConnection::Params {
                // A socket directory goes where libpq takes it: in host.
                host: vars
                    .get("PG_SOCKET")
                    .or(vars.get("PG_HOST"))
                    .map(str::to_owned),
                port: vars.get("PG_PORT").map(str::to_owned),
                user: vars.get("PG_USER").map(str::to_owned),
            };
            (
                connection,
                vars.get("PG_PASSWORD").map(str::to_owned),
                parse_databases("PG_DATABASE", database, false)?,
            )
        }
    };
    Ok((
        Postgres {
            connection,
            password,
            format,
            globals,
        },
        databases,
    ))
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let vars = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .collect();
        Self::from_vars(&vars)
    }

    pub fn from_vars(vars: &HashMap<String, String>) -> Result<Self, String> {
        let vars = Vars(vars);

        let (engine, databases) = match engine_kind(&vars)? {
            Kind::Mysql => {
                let (mysql, databases) = mysql_config(&vars)?;
                (Engine::Mysql(mysql), databases)
            }
            Kind::Postgres => {
                let (postgres, databases) = postgres_config(&vars)?;
                (Engine::Postgres(postgres), databases)
            }
        };

        let bucket = vars
            .get("S3_BUCKET")
            .ok_or("Missing required environment variable S3_BUCKET")?
            .to_owned();

        Ok(Config {
            engine,
            databases,
            bucket,
            prefix: vars.get("S3_PREFIX").unwrap_or("/").to_owned(),
            endpoint_url: vars.get("S3_ENDPOINT_URL").map(str::to_owned),
            // Set-but-empty disables the storage class, for providers that
            // reject STANDARD_IA.
            storage_class: Some(vars.raw("S3_STORAGE_CLASS").unwrap_or("STANDARD_IA"))
                .filter(|c| !c.is_empty())
                .map(str::to_owned),
            part_size: parse_part_size(vars.get("S3_PART_SIZE_MIB"))?,
            dump_timeout: parse_seconds("DUMP_TIMEOUT", vars.get("DUMP_TIMEOUT"), 600, false)?,
            upload_timeout: parse_seconds(
                "UPLOAD_TIMEOUT",
                vars.get("UPLOAD_TIMEOUT"),
                300,
                false,
            )?,
            timeout_grace: parse_seconds("TIMEOUT_GRACE", vars.get("TIMEOUT_GRACE"), 30, true)?,
            heartbeat_url: vars
                .get("HEARTBEAT_URL")
                .map(heartbeat::parse_url)
                .transpose()?,
        })
    }

    /// "MySQL" or "PostgreSQL", for log lines.
    pub fn engine_name(&self) -> &'static str {
        match self.engine {
            Engine::Mysql(_) => "MySQL",
            Engine::Postgres(_) => "PostgreSQL",
        }
    }

    /// Whether `target` is gzipped by the binary. A custom-format pg_dump is
    /// compressed already.
    pub fn gzipped(&self, target: &Target) -> bool {
        match (&self.engine, target) {
            (Engine::Postgres(pg), Target::Database(_)) => pg.format == PgFormat::Plain,
            _ => true,
        }
    }

    /// Keys go under a directory per database with several databases, or
    /// with `*`, where the count can change from one run to the next and a
    /// single-database layout would move the keys when a second appears.
    fn per_database_keys(&self) -> bool {
        match &self.databases {
            Databases::Named(names) => names.len() > 1,
            Databases::All => true,
        }
    }

    /// The object key for `target`. MySQL keys are byte-for-byte compatible
    /// with v3, double slashes from a trailing-slash prefix included, so
    /// existing lifecycle rules and restore tooling keep matching.
    pub fn object_key(&self, target: &Target, timestamp: &str) -> String {
        let (dir, file) = match target {
            Target::Database(db) => {
                let extension = if self.gzipped(target) {
                    "sql.gz"
                } else {
                    "dump"
                };
                (
                    self.per_database_keys().then_some(db.as_str()),
                    format!("{timestamp}.{extension}"),
                )
            }
            Target::PgGlobals => (None, format!("{timestamp}.globals.sql.gz")),
        };
        let prefix = self.prefix.strip_prefix('/').unwrap_or(&self.prefix);
        let key = match dir {
            Some(dir) => format!("{prefix}/{dir}/{file}"),
            None => format!("{prefix}/{file}"),
        };
        match key.strip_prefix('/') {
            Some(stripped) => stripped.to_owned(),
            None => key,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn base() -> Vec<(&'static str, &'static str)> {
        vec![
            ("MYSQL_HOST", "db"),
            ("MYSQL_USER", "backup"),
            ("MYSQL_PASSWORD", "secret"),
            ("MYSQL_DATABASE", "app"),
            ("S3_BUCKET", "bucket"),
        ]
    }

    fn with(extra: &[(&str, &str)]) -> HashMap<String, String> {
        let mut all = vars(&base());
        all.extend(vars(extra));
        all
    }

    fn mysql(cfg: &Config) -> &Mysql {
        match &cfg.engine {
            Engine::Mysql(mysql) => mysql,
            Engine::Postgres(_) => panic!("not MySQL"),
        }
    }

    fn postgres(cfg: &Config) -> &Postgres {
        match &cfg.engine {
            Engine::Postgres(pg) => pg,
            Engine::Mysql(_) => panic!("not PostgreSQL"),
        }
    }

    fn named(cfg: &Config) -> &[String] {
        match &cfg.databases {
            Databases::Named(names) => names,
            Databases::All => panic!("not a list"),
        }
    }

    // ---- MySQL: the v4 behavior, unchanged ----

    #[test]
    fn defaults() {
        let cfg = Config::from_vars(&with(&[])).unwrap();
        assert_eq!(
            mysql(&cfg).server,
            Server::Tcp {
                host: "db".into(),
                port: "3306".into()
            }
        );
        assert_eq!(mysql(&cfg).password.as_deref(), Some("secret"));
        assert_eq!(named(&cfg), ["app"]);
        assert_eq!(cfg.prefix, "/");
        assert_eq!(cfg.storage_class.as_deref(), Some("STANDARD_IA"));
        assert_eq!(cfg.part_size, 16 * MIB);
        assert_eq!(cfg.dump_timeout, Duration::from_secs(600));
        assert_eq!(cfg.upload_timeout, Duration::from_secs(300));
        assert_eq!(cfg.timeout_grace, Duration::from_secs(30));
        assert!(!mysql(&cfg).single_transaction);
        assert!(cfg.endpoint_url.is_none());
        assert!(cfg.heartbeat_url.is_none());
    }

    #[test]
    fn heartbeat_url() {
        let cfg = Config::from_vars(&with(&[("HEARTBEAT_URL", "https://hb.example/x")])).unwrap();
        assert_eq!(cfg.heartbeat_url.unwrap().host(), Some("hb.example"));
        let err = Config::from_vars(&with(&[("HEARTBEAT_URL", "hb.example/x")])).unwrap_err();
        assert!(err.starts_with("HEARTBEAT_URL must be"), "{err}");
    }

    #[test]
    fn empty_storage_class_disables_it() {
        let cfg = Config::from_vars(&with(&[("S3_STORAGE_CLASS", "")])).unwrap();
        assert_eq!(cfg.storage_class, None);
    }

    #[test]
    fn empty_variables_count_as_unset() {
        let cfg = Config::from_vars(&with(&[
            ("MYSQL_PORT", ""),
            ("S3_PREFIX", ""),
            ("MYSQL_PASSWORD", ""),
        ]))
        .unwrap();
        assert_eq!(
            mysql(&cfg).server,
            Server::Tcp {
                host: "db".into(),
                port: "3306".into()
            }
        );
        assert_eq!(cfg.prefix, "/");
        assert_eq!(mysql(&cfg).password, None);
    }

    #[test]
    fn socket_wins_over_host() {
        let cfg = Config::from_vars(&with(&[("MYSQL_SOCKET", "/run/mysqld.sock")])).unwrap();
        assert_eq!(
            mysql(&cfg).server,
            Server::Socket("/run/mysqld.sock".into())
        );
    }

    #[test]
    fn single_transaction_values() {
        for value in ["1", "true", "YES", "On"] {
            let cfg = Config::from_vars(&with(&[("MYSQL_SINGLE_TRANSACTION", value)])).unwrap();
            assert!(mysql(&cfg).single_transaction, "{value}");
        }
        for value in ["0", "false", "No", "OFF"] {
            let cfg = Config::from_vars(&with(&[("MYSQL_SINGLE_TRANSACTION", value)])).unwrap();
            assert!(!mysql(&cfg).single_transaction, "{value}");
        }
        let err = Config::from_vars(&with(&[("MYSQL_SINGLE_TRANSACTION", "maybe")])).unwrap_err();
        assert_eq!(
            err,
            "MYSQL_SINGLE_TRANSACTION must be a boolean (1/0, true/false, yes/no, on/off), got 'maybe'"
        );
    }

    #[test]
    fn missing_variables() {
        let cases = [
            (
                "MYSQL_DATABASE",
                "Missing required environment variable MYSQL_DATABASE",
            ),
            (
                "MYSQL_USER",
                "Missing required environment variable MYSQL_USER",
            ),
            (
                "MYSQL_HOST",
                "Missing required environment variable MYSQL_HOST or MYSQL_SOCKET",
            ),
            (
                "S3_BUCKET",
                "Missing required environment variable S3_BUCKET",
            ),
        ];
        for (name, message) in cases {
            let mut v = with(&[]);
            v.remove(name);
            assert_eq!(Config::from_vars(&v).unwrap_err(), message);
        }
    }

    #[test]
    fn database_list_is_trimmed() {
        let cfg = Config::from_vars(&with(&[("MYSQL_DATABASE", " app , analytics ,,")])).unwrap();
        assert_eq!(named(&cfg), ["app", "analytics"]);
        let err = Config::from_vars(&with(&[("MYSQL_DATABASE", " , ")])).unwrap_err();
        assert_eq!(err, "MYSQL_DATABASE has no database names: ' , '");
    }

    #[test]
    fn timeouts_and_part_size() {
        let cfg = Config::from_vars(&with(&[
            ("DUMP_TIMEOUT", "1200"),
            ("UPLOAD_TIMEOUT", "60"),
            ("TIMEOUT_GRACE", "0"),
            ("S3_PART_SIZE_MIB", "64"),
        ]))
        .unwrap();
        assert_eq!(cfg.dump_timeout, Duration::from_secs(1200));
        assert_eq!(cfg.upload_timeout, Duration::from_secs(60));
        assert_eq!(cfg.timeout_grace, Duration::ZERO);
        assert_eq!(cfg.part_size, 64 * MIB);

        assert!(Config::from_vars(&with(&[("DUMP_TIMEOUT", "0")])).is_err());
        assert!(Config::from_vars(&with(&[("DUMP_TIMEOUT", "10m")])).is_err());
        assert!(Config::from_vars(&with(&[("S3_PART_SIZE_MIB", "4")])).is_err());
        assert!(Config::from_vars(&with(&[("S3_PART_SIZE_MIB", "5121")])).is_err());
    }

    #[test]
    fn database_url_overrides_individual_variables() {
        let cfg =
            Config::from_vars(&with(&[("DATABASE_URL", "mysql://u:p%40ss@h:3307/d1,d2")])).unwrap();
        assert_eq!(mysql(&cfg).user, "u");
        // Not percent-decoded, as in v3.
        assert_eq!(mysql(&cfg).password.as_deref(), Some("p%40ss"));
        assert_eq!(
            mysql(&cfg).server,
            Server::Tcp {
                host: "h".into(),
                port: "3307".into()
            }
        );
        assert_eq!(named(&cfg), ["d1", "d2"]);
    }

    #[test]
    fn database_url_variants() {
        assert_eq!(
            parse_mysql_url("mysql://u@h/d").unwrap(),
            UrlParts {
                user: "u".into(),
                host: "h".into(),
                database: "d".into(),
                ..Default::default()
            }
        );
        // The password may contain colons: only the first one splits.
        assert_eq!(
            parse_mysql_url("mysql://u:a:b@h/d").unwrap().password,
            "a:b"
        );
        assert_eq!(
            parse_mysql_url("mysql://u:p@/d?charset=utf8&unix_socket=/s.sock").unwrap(),
            UrlParts {
                user: "u".into(),
                password: "p".into(),
                database: "d".into(),
                socket: Some("/s.sock".into()),
                ..Default::default()
            }
        );
        assert!(parse_mysql_url("mysql://h/d").is_err());
        // mariadb:// is the same parser.
        assert_eq!(
            parse_mysql_url("mariadb://u:p@h:3307/d").unwrap(),
            parse_mysql_url("mysql://u:p@h:3307/d").unwrap()
        );
    }

    #[test]
    fn database_url_keeps_port_and_socket_from_env_when_absent() {
        let cfg = Config::from_vars(&with(&[
            ("MYSQL_PORT", "3310"),
            ("DATABASE_URL", "mysql://u:p@h/d"),
        ]))
        .unwrap();
        assert_eq!(
            mysql(&cfg).server,
            Server::Tcp {
                host: "h".into(),
                port: "3310".into()
            }
        );

        let cfg = Config::from_vars(&with(&[
            ("MYSQL_SOCKET", "/env.sock"),
            ("DATABASE_URL", "mysql://u:p@/d"),
        ]))
        .unwrap();
        assert_eq!(mysql(&cfg).server, Server::Socket("/env.sock".into()));
    }

    fn key(cfg: &Config, ts: &str) -> String {
        let db = match &cfg.databases {
            Databases::Named(names) => names[0].clone(),
            Databases::All => "app".into(),
        };
        cfg.object_key(&Target::Database(db), ts)
    }

    #[test]
    fn object_keys_match_v3() {
        let ts = "2026-10-13T120000Z";
        let key = |prefix: &str, dbs: &str| {
            let cfg = Config::from_vars(&with(&[("S3_PREFIX", prefix), ("MYSQL_DATABASE", dbs)]))
                .unwrap();
            key(&cfg, ts)
        };
        assert_eq!(key("/", "app"), "2026-10-13T120000Z.sql.gz");
        assert_eq!(
            key("backups/mysql", "app"),
            "backups/mysql/2026-10-13T120000Z.sql.gz"
        );
        assert_eq!(key("/backups", "app"), "backups/2026-10-13T120000Z.sql.gz");
        assert_eq!(key("backups/", "app"), "backups//2026-10-13T120000Z.sql.gz");
        assert_eq!(key("/", "app,other"), "app/2026-10-13T120000Z.sql.gz");
        assert_eq!(
            key("daily", "app,other"),
            "daily/app/2026-10-13T120000Z.sql.gz"
        );
    }

    // ---- New in v4.1 ----

    #[test]
    fn star_means_every_database_and_always_uses_directories() {
        let cfg = Config::from_vars(&with(&[("MYSQL_DATABASE", " * ")])).unwrap();
        assert_eq!(cfg.databases, Databases::All);
        assert_eq!(
            cfg.object_key(&Target::Database("app".into()), "T"),
            "app/T.sql.gz"
        );
        let err = Config::from_vars(&with(&[("MYSQL_DATABASE", "*,app")])).unwrap_err();
        assert_eq!(
            err,
            "MYSQL_DATABASE cannot combine '*' with database names: '*,app'"
        );
    }

    fn pg(extra: &[(&str, &str)]) -> Result<Config, String> {
        let mut all = vars(&[("PG_DATABASE", "app"), ("S3_BUCKET", "bucket")]);
        all.extend(vars(extra));
        Config::from_vars(&all)
    }

    #[test]
    fn postgres_from_variables() {
        let cfg = pg(&[
            ("PG_HOST", "db"),
            ("PG_PORT", "5433"),
            ("PG_USER", "backup"),
            ("PG_PASSWORD", "secret"),
        ])
        .unwrap();
        let pg = postgres(&cfg);
        assert_eq!(
            pg.connection,
            PgConnection::Params {
                host: Some("db".into()),
                port: Some("5433".into()),
                user: Some("backup".into()),
            }
        );
        assert_eq!(pg.password.as_deref(), Some("secret"));
        assert_eq!(pg.format, PgFormat::Custom);
        assert!(!pg.globals);
        assert_eq!(cfg.engine_name(), "PostgreSQL");
    }

    #[test]
    fn postgres_leaves_unset_fields_to_libpq() {
        let cfg = pg(&[]).unwrap();
        assert_eq!(
            postgres(&cfg).connection,
            PgConnection::Params {
                host: None,
                port: None,
                user: None
            }
        );
        assert_eq!(postgres(&cfg).password, None);
    }

    #[test]
    fn postgres_socket_wins_over_host() {
        let cfg = pg(&[("PG_HOST", "db"), ("PG_SOCKET", "/run/postgresql")]).unwrap();
        let PgConnection::Params { host, .. } = &postgres(&cfg).connection else {
            panic!()
        };
        assert_eq!(host.as_deref(), Some("/run/postgresql"));
    }

    #[test]
    fn postgres_options() {
        let cfg = pg(&[("PG_FORMAT", "Plain"), ("PG_GLOBALS", "yes")]).unwrap();
        assert_eq!(postgres(&cfg).format, PgFormat::Plain);
        assert!(postgres(&cfg).globals);
        assert_eq!(
            pg(&[("PG_FORMAT", "tar")]).unwrap_err(),
            "PG_FORMAT must be custom or plain, got 'tar'"
        );
        assert!(pg(&[("PG_GLOBALS", "maybe")]).is_err());
    }

    #[test]
    fn postgres_requires_a_database() {
        let err = Config::from_vars(&vars(&[("PG_HOST", "db"), ("S3_BUCKET", "b")])).unwrap_err();
        assert_eq!(err, "Missing required environment variable PG_DATABASE");
    }

    #[test]
    fn postgres_keys() {
        let ts = "T";
        let custom = pg(&[("S3_PREFIX", "daily")]).unwrap();
        assert_eq!(key(&custom, ts), "daily/T.dump");
        assert_eq!(
            custom.object_key(&Target::PgGlobals, ts),
            "daily/T.globals.sql.gz"
        );
        assert!(!custom.gzipped(&Target::Database("app".into())));
        assert!(custom.gzipped(&Target::PgGlobals));

        let plain = pg(&[("PG_FORMAT", "plain"), ("PG_DATABASE", "a,b")]).unwrap();
        assert_eq!(key(&plain, ts), "a/T.sql.gz");
        assert_eq!(plain.object_key(&Target::PgGlobals, ts), "T.globals.sql.gz");
    }

    #[test]
    fn postgres_url() {
        let cfg = Config::from_vars(&vars(&[
            (
                "DATABASE_URL",
                "postgresql://backup:p%40ss:w@db1:5432,db2:5433/app,%2Fodd%2Cname?sslmode=require",
            ),
            ("S3_BUCKET", "b"),
            ("PG_FORMAT", "plain"),
        ]))
        .unwrap();
        let pg = postgres(&cfg);
        assert_eq!(
            pg.connection,
            PgConnection::Url {
                base: "postgresql://backup@db1:5432,db2:5433".into(),
                query: "?sslmode=require".into(),
            }
        );
        // Decoded, as libpq decodes it; only the first colon splits.
        assert_eq!(pg.password.as_deref(), Some("p@ss:w"));
        assert_eq!(named(&cfg), ["app", "/odd,name"]);
    }

    #[test]
    fn postgres_url_variants() {
        let url = parse_postgres_url("postgres://h/d").unwrap();
        assert_eq!(
            url.connection,
            PgConnection::Url {
                base: "postgres://h".into(),
                query: String::new()
            }
        );
        assert_eq!(url.password, None);
        assert_eq!(url.databases, "d");

        let url = parse_postgres_url("postgres://u:@%2Frun%2Fpostgresql/d").unwrap();
        assert_eq!(url.password, None);
        let PgConnection::Url { base, .. } = url.connection else {
            panic!()
        };
        assert_eq!(base, "postgres://u@%2Frun%2Fpostgresql");

        assert_eq!(
            parse_postgres_url("postgres://u:%zz@h/d").unwrap_err(),
            "the DATABASE_URL password has an invalid percent-escape"
        );

        let err = Config::from_vars(&vars(&[
            ("DATABASE_URL", "postgres://u@h"),
            ("S3_BUCKET", "b"),
        ]))
        .unwrap_err();
        assert_eq!(err, "DATABASE_URL has no database names: ''");
    }

    #[test]
    fn unknown_scheme() {
        let err = Config::from_vars(&with(&[("DATABASE_URL", "sqlite://x")])).unwrap_err();
        assert_eq!(
            err,
            "DATABASE_URL must use the mysql://, mariadb://, postgres:// or postgresql:// scheme"
        );
    }

    #[test]
    fn engines_cannot_be_mixed() {
        let err = Config::from_vars(&with(&[("PG_FORMAT", "plain")])).unwrap_err();
        assert_eq!(
            err,
            "Both MySQL and PostgreSQL variables are set (MYSQL_HOST, MYSQL_USER, MYSQL_PASSWORD, MYSQL_DATABASE; PG_FORMAT): configure one engine"
        );

        let err = Config::from_vars(&vars(&[
            ("DATABASE_URL", "mysql://u@h/d"),
            ("PG_GLOBALS", "1"),
            ("S3_BUCKET", "b"),
        ]))
        .unwrap_err();
        assert_eq!(
            err,
            "DATABASE_URL is a MySQL URL but PostgreSQL variables are set (PG_GLOBALS)"
        );

        let err = Config::from_vars(&vars(&[
            ("DATABASE_URL", "postgres://u@h/d"),
            ("MYSQL_SINGLE_TRANSACTION", "1"),
            ("S3_BUCKET", "b"),
        ]))
        .unwrap_err();
        assert_eq!(
            err,
            "DATABASE_URL is a PostgreSQL URL but MySQL variables are set (MYSQL_SINGLE_TRANSACTION)"
        );

        let err = Config::from_vars(&vars(&[
            ("DATABASE_URL", "postgres://u@h/d"),
            ("PG_HOST", "other"),
            ("S3_BUCKET", "b"),
        ]))
        .unwrap_err();
        assert_eq!(
            err,
            "DATABASE_URL already sets the PostgreSQL connection; unset PG_HOST"
        );

        // Options are fine next to a URL.
        let cfg = Config::from_vars(&vars(&[
            ("DATABASE_URL", "postgres://u@h/d"),
            ("PG_GLOBALS", "1"),
            ("S3_BUCKET", "b"),
        ]))
        .unwrap();
        assert!(postgres(&cfg).globals);
    }

    #[test]
    fn nothing_configured_points_at_libpq_typos() {
        let err = Config::from_vars(&vars(&[("S3_BUCKET", "b")])).unwrap_err();
        assert_eq!(
            err,
            "No database configured: set DATABASE_URL, the MYSQL_* variables or the PG_* variables"
        );
        let err = Config::from_vars(&vars(&[("PGHOST", "db"), ("S3_BUCKET", "b")])).unwrap_err();
        assert_eq!(
            err,
            "No database configured: set DATABASE_URL, the MYSQL_* variables or the PG_* variables (found PGHOST: did you mean PG_HOST?)"
        );
    }

    #[test]
    fn libpq_variables_do_not_select_or_mix_engines() {
        let cfg = Config::from_vars(&with(&[("PGSSLMODE", "require"), ("PGHOST", "x")])).unwrap();
        assert_eq!(cfg.engine_name(), "MySQL");
    }
}
