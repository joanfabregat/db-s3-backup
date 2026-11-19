//! Running a dump tool and streaming its output to S3.

use std::fmt;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use flate2::Compression;
use flate2::write::GzEncoder;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;
use tokio::time::timeout;

use crate::config::{Config, Databases, Engine, Target};
use crate::s3::{Bucket, Upload};
use crate::{mysql, postgres};

/// Parts uploading at once. With the one being filled, memory holds at most
/// three parts: 48 MiB at the default 16 MiB part size.
const PARALLEL_PARTS: usize = 2;

const READ_CHUNK: usize = 64 * 1024;

/// Room for the compressed output of the read that pushes a part past its
/// size, so the buffer never reallocates to double its capacity.
const PART_SLACK: usize = 256 * 1024;

/// Finds the first of `names` on PATH.
pub fn find_program(names: &[&str]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    names.iter().find_map(|name| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| {
                candidate
                    .metadata()
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
    })
}

/// The databases this run backs up: the configured list, or for `*`, the
/// server's, listed now.
pub async fn databases(cfg: &Config) -> Result<Vec<String>, String> {
    match &cfg.databases {
        Databases::Named(names) => Ok(names.clone()),
        Databases::All => match &cfg.engine {
            Engine::Mysql(mysql) => mysql::list_databases(mysql).await,
            Engine::Postgres(pg) => postgres::list_databases(pg).await,
        },
    }
}

/// The dump command for `target`, and the tool name log lines use for it.
/// MySQL dumps are still called mysqldump in the logs, as in v4, whichever
/// binary runs.
fn dump_command(cfg: &Config, target: &Target) -> Result<(Command, &'static str), String> {
    match (&cfg.engine, target) {
        (Engine::Mysql(mysql), Target::Database(db)) => {
            Ok((mysql::dump_command(mysql, db)?, "mysqldump"))
        }
        (Engine::Postgres(pg), Target::Database(db)) => {
            Ok((postgres::dump_command(pg, db)?, "pg_dump"))
        }
        (Engine::Postgres(pg), Target::PgGlobals) => {
            Ok((postgres::globals_command(pg)?, "pg_dumpall"))
        }
        (Engine::Mysql(_), Target::PgGlobals) => {
            Err("PostgreSQL globals requested from a MySQL server".into())
        }
    }
}

pub struct Summary {
    /// The dump tool, for the log.
    pub tool: &'static str,
    pub gzipped: bool,
    pub parts: i32,
    pub raw_bytes: u64,
    /// Bytes uploaded: compressed, by the binary or by the dump tool.
    pub compressed_bytes: u64,
    pub elapsed: Duration,
    /// Time the dump spent blocked because the upload had not kept up. While
    /// it is blocked, its table locks (or transaction) stay open.
    pub upload_wait: Duration,
}

#[derive(Debug)]
pub enum Failure {
    /// The dump tool could not run or did not exit 0. Not retried.
    Dump(String),
    /// S3 rejected or dropped a request. Retried with a fresh dump.
    Upload(anyhow::Error),
    /// The attempt ran past DUMP_TIMEOUT. Not retried.
    TimedOut,
    /// A TERM or INT arrived; carries the signal number.
    Interrupted(i32),
}
impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Dump(message) => f.write_str(message),
            Failure::Upload(e) => write!(f, "{e:#}"),
            Failure::TimedOut => f.write_str("timed out"),
            Failure::Interrupted(signal) => write!(f, "interrupted by signal {signal}"),
        }
    }
}

fn describe_exit(status: ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exit {code}"),
        (None, Some(signal)) => format!("killed by signal {signal}"),
        (None, None) => status.to_string(),
    }
}

/// Sends TERM, then KILL if the dump tool has not exited after `grace`.
async fn stop(child: &mut Child, grace: Duration) {
    if let Some(pid) = child.id() {
        // SAFETY: kill(2) with a PID we spawned and have not yet reaped, so it
        // cannot have been recycled for another process.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
        if timeout(grace, child.wait()).await.is_ok() {
            return;
        }
    }
    let _ = child.kill().await;
}

/// One attempt at one target: dump, compress and upload as a single stream,
/// bounded by DUMP_TIMEOUT. On any failure the multipart upload is aborted.
pub async fn backup_once(
    cfg: &Config,
    bucket: &Bucket,
    target: &Target,
    key: &str,
    shutdown: &mut watch::Receiver<Option<i32>>,
) -> Result<Summary, Failure> {
    if let Some(signal) = *shutdown.borrow() {
        return Err(Failure::Interrupted(signal));
    }

    let (mut command, tool) = dump_command(cfg, target).map_err(Failure::Dump)?;
    let gzipped = cfg.gzipped(target);
    let content_type = if gzipped {
        "application/gzip"
    } else {
        "application/octet-stream"
    };
    let upload = bucket
        .start_upload(key, content_type)
        .await
        .map_err(Failure::Upload)?;

    command.stdout(Stdio::piped()).stderr(Stdio::inherit());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            upload.abort().await;
            return Err(Failure::Dump(format!("could not start {tool}: {e}")));
        }
    };
    let stdout = child.stdout.take().expect("stdout is piped");

    let started = Instant::now();
    let pipeline = Pipeline {
        tool,
        gzipped,
        upload: &upload,
        part_size: cfg.part_size,
    };
    let outcome = tokio::select! {
        result = timeout(cfg.dump_timeout, pipeline.stream(stdout, &mut child)) => {
            result.unwrap_or(Err(Failure::TimedOut))
        }
        signal = wait_for_signal(shutdown) => Err(Failure::Interrupted(signal)),
    };

    match outcome {
        Ok(mut summary) => {
            summary.elapsed = started.elapsed();
            Ok(summary)
        }
        Err(failure) => {
            stop(&mut child, cfg.timeout_grace).await;
            upload.abort().await;
            Err(failure)
        }
    }
}

async fn wait_for_signal(shutdown: &mut watch::Receiver<Option<i32>>) -> i32 {
    match shutdown.wait_for(Option::is_some).await {
        Ok(signal) => signal.expect("waited for Some"),
        // The sender lives for the whole process; if it is gone, never fire.
        Err(_) => std::future::pending().await,
    }
}

/// The part being filled: gzipped here, or passed through when the dump tool
/// compresses its own output.
enum PartBuffer {
    Gzip(GzEncoder<Vec<u8>>),
    Raw(Vec<u8>),
}

impl PartBuffer {
    fn new(gzipped: bool, part_size: usize) -> Self {
        let buffer = Vec::with_capacity(part_size + PART_SLACK);
        if gzipped {
            PartBuffer::Gzip(GzEncoder::new(buffer, Compression::default()))
        } else {
            PartBuffer::Raw(buffer)
        }
    }

    fn write(&mut self, data: &[u8]) {
        match self {
            PartBuffer::Gzip(encoder) => encoder
                .write_all(data)
                .expect("writing to a Vec cannot fail"),
            PartBuffer::Raw(buffer) => buffer.extend_from_slice(data),
        }
    }

    fn len(&self) -> usize {
        match self {
            PartBuffer::Gzip(encoder) => encoder.get_ref().len(),
            PartBuffer::Raw(buffer) => buffer.len(),
        }
    }

    /// The bytes ready so far, leaving an empty buffer in their place. The
    /// gzip stream carries on across parts: the object is one gzip member.
    fn take(&mut self, part_size: usize) -> Vec<u8> {
        let fresh = Vec::with_capacity(part_size + PART_SLACK);
        match self {
            PartBuffer::Gzip(encoder) => std::mem::replace(encoder.get_mut(), fresh),
            PartBuffer::Raw(buffer) => std::mem::replace(buffer, fresh),
        }
    }

    fn finish(self) -> Vec<u8> {
        match self {
            PartBuffer::Gzip(encoder) => encoder.finish().expect("writing to a Vec cannot fail"),
            PartBuffer::Raw(buffer) => buffer,
        }
    }
}

struct Pipeline<'a> {
    tool: &'static str,
    gzipped: bool,
    upload: &'a Upload,
    part_size: usize,
}

impl Pipeline<'_> {
    async fn stream(&self, mut stdout: ChildStdout, child: &mut Child) -> Result<Summary, Failure> {
        let tool = self.tool;
        let permits = Arc::new(Semaphore::new(PARALLEL_PARTS));
        let mut in_flight = JoinSet::new();
        let mut completed = Vec::new();
        let mut part = PartBuffer::new(self.gzipped, self.part_size);
        let mut buf = vec![0u8; READ_CHUNK];
        let mut summary = Summary {
            tool,
            gzipped: self.gzipped,
            parts: 0,
            raw_bytes: 0,
            compressed_bytes: 0,
            elapsed: Duration::ZERO,
            upload_wait: Duration::ZERO,
        };

        // Waits for an upload slot, then sends `body` as the next part. Waiting
        // here stops reading, which blocks the dump on the pipe: memory stays
        // bounded and the time is recorded as upload_wait.
        macro_rules! submit {
            ($body:expr) => {{
                let body: Vec<u8> = $body;
                let waiting = Instant::now();
                let permit = permits
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("semaphore is never closed");
                summary.upload_wait += waiting.elapsed();
                summary.parts += 1;
                summary.compressed_bytes += body.len() as u64;
                let number = summary.parts;
                let upload = self.upload.clone();
                in_flight.spawn(async move {
                    let _permit = permit;
                    upload.upload_part(number, body).await
                });
            }};
        }

        loop {
            let n = stdout
                .read(&mut buf)
                .await
                .map_err(|e| Failure::Dump(format!("could not read {tool} output: {e}")))?;
            if n == 0 {
                break;
            }
            summary.raw_bytes += n as u64;
            part.write(&buf[..n]);

            if part.len() >= self.part_size {
                submit!(part.take(self.part_size));
            }

            // Surface an upload failure now rather than after the whole dump.
            while let Some(joined) = in_flight.try_join_next() {
                completed.push(
                    joined
                        .expect("upload tasks do not panic")
                        .map_err(Failure::Upload)?,
                );
            }
        }

        let status = child
            .wait()
            .await
            .map_err(|e| Failure::Dump(format!("could not wait for {tool}: {e}")))?;
        if !status.success() {
            return Err(Failure::Dump(format!(
                "{tool} failed ({})",
                describe_exit(status)
            )));
        }

        // Only now is the dump known to be complete. The last part may be
        // under the 5 MiB minimum; S3 allows that for the final part.
        submit!(part.finish());

        while let Some(joined) = in_flight.join_next().await {
            completed.push(
                joined
                    .expect("upload tasks do not panic")
                    .map_err(Failure::Upload)?,
            );
        }
        self.upload
            .complete(completed)
            .await
            .map_err(Failure::Upload)?;
        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn exit_descriptions() {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(describe_exit(ExitStatus::from_raw(2 << 8)), "exit 2");
        assert_eq!(describe_exit(ExitStatus::from_raw(9)), "killed by signal 9");
    }

    #[test]
    fn gzip_parts_concatenate_into_one_stream() {
        let mut part = PartBuffer::new(true, 1024);
        part.write(b"hello ");
        let first = part.take(1024);
        part.write(b"world");
        let mut all = first;
        all.extend(part.finish());
        let mut out = String::new();
        flate2::read::GzDecoder::new(&all[..])
            .read_to_string(&mut out)
            .unwrap();
        assert_eq!(out, "hello world");
    }

    #[test]
    fn raw_parts_pass_through() {
        let mut part = PartBuffer::new(false, 1024);
        part.write(b"PGDMP");
        assert_eq!(part.len(), 5);
        assert_eq!(part.take(1024), b"PGDMP");
        part.write(b"rest");
        assert_eq!(part.finish(), b"rest");
    }
}
