//! Dumps MySQL, MariaDB or PostgreSQL databases and streams them to S3 or an
//! S3-compatible provider. Configured entirely from the environment; see the
//! README.

mod config;
mod dump;
mod heartbeat;
mod mysql;
mod postgres;
mod s3;

use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aws_smithy_types::DateTime;
use aws_smithy_types::date_time::Format;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;

use crate::config::{Config, Databases, Engine, MAX_ATTEMPTS, Target};
use crate::dump::{Failure, Summary, backup_once};
use crate::s3::Bucket;

const RETRY_DELAY: Duration = Duration::from_secs(5);

/// `2026-10-13T120000Z`: RFC 3339 in UTC to the second, without colons so it
/// is safe in object keys and sorts lexically.
fn timestamp(now: SystemTime) -> String {
    let secs = now
        .duration_since(UNIX_EPOCH)
        .expect("clock is after 1970")
        .as_secs();
    DateTime::from_secs(secs as i64)
        .fmt(Format::DateTime)
        .expect("a current time formats")
        .replace(':', "")
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn report(target: &Target, summary: &Summary) {
    if summary.gzipped {
        println!(
            "Uploaded successfully: {:.1} MiB compressed ({:.1} MiB raw) in {} part(s), {:.1}s.",
            mib(summary.compressed_bytes),
            mib(summary.raw_bytes),
            summary.parts,
            summary.elapsed.as_secs_f64(),
        );
    } else {
        println!(
            "Uploaded successfully: {:.1} MiB in {} part(s), {:.1}s.",
            mib(summary.compressed_bytes),
            summary.parts,
            summary.elapsed.as_secs_f64(),
        );
    }
    println!(
        "{} of {} waited {:.1}s on the upload.",
        summary.tool,
        target.label(),
        summary.upload_wait.as_secs_f64()
    );
}

/// Forwards the first TERM or INT into `tx`, so the running attempt can stop
/// the dump and abort its upload instead of dying with parts left behind.
fn watch_signals(tx: watch::Sender<Option<i32>>) {
    let mut term = signal(SignalKind::terminate()).expect("can install a TERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("can install an INT handler");
    tokio::spawn(async move {
        let signal = tokio::select! {
            _ = term.recv() => libc::SIGTERM,
            _ = int.recv() => libc::SIGINT,
        };
        eprintln!("Received signal {signal}, stopping");
        let _ = tx.send(Some(signal));
    });
}

/// Backs up one target, retrying a failed upload with a fresh dump.
async fn backup_target(
    cfg: &Config,
    bucket: &Bucket,
    target: &Target,
    key: &str,
    shutdown: &mut watch::Receiver<Option<i32>>,
) -> Result<(), ExitCode> {
    let label = target.label();
    let mut attempt = 1;
    loop {
        match backup_once(cfg, bucket, target, key, shutdown).await {
            Ok(summary) => {
                report(target, &summary);
                return Ok(());
            }
            Err(Failure::Upload(e)) if attempt < MAX_ATTEMPTS => {
                eprintln!("Upload attempt {attempt} failed: {e:#}");
                eprintln!(
                    "Retrying with a fresh dump in {}s...",
                    RETRY_DELAY.as_secs()
                );
                attempt += 1;
                tokio::select! {
                    _ = tokio::time::sleep(RETRY_DELAY) => {}
                    _ = shutdown.changed() => {}
                }
            }
            Err(Failure::Upload(e)) => {
                eprintln!("Upload attempt {attempt} failed: {e:#}");
                eprintln!("Error: S3 upload failed after {MAX_ATTEMPTS} attempts");
                return Err(ExitCode::FAILURE);
            }
            Err(Failure::TimedOut) => {
                eprintln!(
                    "Error: dump and upload of {label} exceeded {}s (DUMP_TIMEOUT) and were stopped",
                    cfg.dump_timeout.as_secs()
                );
                return Err(ExitCode::FAILURE);
            }
            Err(Failure::Dump(message)) => {
                eprintln!("Error: dump of {label} failed: {message}");
                return Err(ExitCode::FAILURE);
            }
            Err(Failure::Interrupted(signal)) => {
                eprintln!(
                    "Error: backup of {label} was interrupted; the incomplete upload was aborted"
                );
                return Err(ExitCode::from(128 + signal as u8));
            }
        }
    }
}

async fn run(cfg: &Config) -> Result<(), ExitCode> {
    let (tx, mut shutdown) = watch::channel(None);
    watch_signals(tx);

    let bucket = Bucket::new(cfg).await;
    let timestamp = timestamp(SystemTime::now());

    let databases = dump::databases(cfg).await.map_err(|message| {
        eprintln!("Error: could not list the databases on the server: {message}");
        ExitCode::FAILURE
    })?;
    if cfg.databases == Databases::All {
        if databases.is_empty() {
            eprintln!("Error: the server has no databases to back up");
            return Err(ExitCode::FAILURE);
        }
        println!(
            "Found {} database(s): {}",
            databases.len(),
            databases.join(", ")
        );
    }

    // Globals first: they are small, and a missing privilege shows up before
    // the long dumps rather than after them.
    if let Engine::Postgres(pg) = &cfg.engine
        && pg.globals
    {
        println!("Dumping PostgreSQL roles and tablespaces");
        let target = Target::PgGlobals;
        let key = cfg.object_key(&target, &timestamp);
        println!("Streaming dump to S3: s3://{}/{key}", cfg.bucket);
        backup_target(cfg, &bucket, &target, &key, &mut shutdown).await?;
    }

    for db in databases {
        println!("Dumping {} database: {db}", cfg.engine_name());
        if let Engine::Mysql(mysql) = &cfg.engine
            && mysql.single_transaction
        {
            mysql::warn_non_transactional_tables(mysql, &db).await;
        }

        let target = Target::Database(db);
        let key = cfg.object_key(&target, &timestamp);
        println!("Streaming dump to S3: s3://{}/{key}", cfg.bucket);
        backup_target(cfg, &bucket, &target, &key, &mut shutdown).await?;
    }
    Ok(())
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> ExitCode {
    let cfg = match Config::from_env() {
        Ok(cfg) => cfg,
        Err(message) => {
            eprintln!("Error: {message}");
            return ExitCode::FAILURE;
        }
    };
    println!("db-s3-backup {}", env!("CARGO_PKG_VERSION"));

    if let Err(code) = run(&cfg).await {
        return code;
    }

    println!("Backup complete.");
    if let Some(url) = &cfg.heartbeat_url {
        heartbeat::ping(url).await;
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_format() {
        let t = UNIX_EPOCH + Duration::from_millis(1_791_892_800_250);
        assert_eq!(timestamp(t), "2026-10-13T120000Z");
    }
}
