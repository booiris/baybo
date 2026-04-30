//! Lifecycle loop for embedded sidecars.
//!
//! Lazy spawn model: an embedded channel type only gets a supervised
//! restart loop once at least one bot is registered for it. The
//! supervisor polls [`ChannelBotStore`] on a short tick, and the first
//! time a channel reports live bots it materialises a
//! `tokio::process::Command` pointed at `bun <bundle>` (with `bun`
//! resolved from `PATH` or [`BUN_BINARY_ENV`]), hands it to
//! [`ChannelSpawner`] (which injects the channel WS URL + a fresh
//! capability token via env vars — see `spawn.rs`), and starts waiting
//! on the child. On exit, back off and restart. On shutdown, SIGKILL
//! the child and exit.
//!
//! A channel that never gains a bot never spawns. A channel that
//! gains one at runtime spawns within one discovery tick. The supervisor
//! does not currently tear a sidecar down when its bot count drops back
//! to zero — the [`crate::channel::ChannelBotReconciler`] keeps the
//! sidecar's roster in sync, so an idle sidecar costs only the node
//! process; explicit stop-on-empty would just kill a process that's
//! already doing nothing.
//!
//! Backoff is exponential with a 30s cap. A child that ran for ≥60s
//! before dying resets backoff so a long-stable sidecar that finally
//! crashes restarts immediately.
//!
//! Child stdout/stderr are (a) pushed into the shared [`LogBuffer`]
//! for the live admin dashboard, (b) appended to a per-channel
//! daily-rolling file at `<channel_log_dir>/<channel_type>.log.<date>`
//! through [`RedactingMakeWriter`], and (c) tee'd to the gateway's
//! own stdout/stderr so running `aura gateway start` in a terminal
//! still shows every sidecar line live. Sidecar output never enters
//! `aura.log`.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aura_agent::service::ShutdownSignal;
use aura_model::ChannelType;
use aura_security::{LeakDetector, RedactingMakeWriter};
use aura_storage::ChannelBotStore;
use tokio::io::{AsyncRead, BufReader};
use tokio::process::Command;
use tokio::time::sleep;
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::fmt::MakeWriter;

use crate::log_buffer::{LogBuffer, LogLevel};
use crate::sidecar::pipe_pump::{LineOutcome, drain_until_newline, read_capped_line};
use crate::spawn::ChannelSpawner;

use super::assets::SidecarRuntime;

const SIDECAR_PIPE_LINE_MAX_BYTES: usize = 1024;

/// Override for the `bun` binary used to run sidecars. Resolved
/// lazily at spawn time so a bun install elsewhere on disk can be
/// pointed at without rebuilding. Defaults to `bun` from `PATH`.
pub const BUN_BINARY_ENV: &str = "AURA_BUN_BIN";

const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Uptime above which a child's next crash resets backoff to the
/// minimum. Prevents a sidecar that's been stable for hours from
/// taking 30s to come back after a transient failure.
const UPTIME_RESET_THRESHOLD: Duration = Duration::from_secs(60);
/// How often the supervisor polls the bot store for embedded channel
/// types that have gained their first registered bot. Matches the
/// reconciler tick so an `aura channel add` propagates uniformly.
const BOT_DISCOVERY_INTERVAL: Duration = Duration::from_secs(2);

type ChannelLogWriter = Arc<RedactingMakeWriter<NonBlocking>>;

/// Resolve the `bun` executable path used to run sidecars. Honours
/// the `AURA_BUN_BIN` override; otherwise relies on `PATH` resolution
/// inside `Command::new`.
pub(crate) fn bun_binary() -> PathBuf {
    std::env::var_os(BUN_BINARY_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("bun"))
}

/// Drives the restart loop for every embedded sidecar that has at
/// least one registered bot. Clone-cheap (everything is `Arc` / small
/// value).
#[derive(Clone)]
pub struct SidecarSupervisor {
    runtime: Arc<SidecarRuntime>,
    spawner: ChannelSpawner,
    log_buffer: Arc<LogBuffer>,
    channel_log_dir: PathBuf,
    leak_detector: Arc<LeakDetector>,
    bot_store: Arc<dyn ChannelBotStore>,
}

impl SidecarSupervisor {
    pub fn new(
        runtime: Arc<SidecarRuntime>,
        spawner: ChannelSpawner,
        log_buffer: Arc<LogBuffer>,
        channel_log_dir: PathBuf,
        leak_detector: Arc<LeakDetector>,
        bot_store: Arc<dyn ChannelBotStore>,
    ) -> Self {
        Self {
            runtime,
            spawner,
            log_buffer,
            channel_log_dir,
            leak_detector,
            bot_store,
        }
    }

    /// Poll the bot store and spawn a supervised restart loop for each
    /// embedded channel type the first time it reports ≥ 1 live bot.
    /// Channels with no bots are never spawned. Returns when `shutdown`
    /// fires and every child has been signalled + awaited.
    pub async fn run(self, shutdown: ShutdownSignal) {
        if let Err(e) = std::fs::create_dir_all(&self.channel_log_dir) {
            tracing::warn!(
                dir = %self.channel_log_dir.display(),
                error = %e,
                "failed to create channel log dir; sidecar output will stay in-memory only",
            );
        }
        let this = Arc::new(self);
        let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
        let mut guards: Vec<WorkerGuard> = Vec::new();
        let mut started: HashSet<String> = HashSet::new();

        let embedded: Vec<String> = this
            .runtime
            .names_in_domain(crate::sidecar::domains::CHANNEL)
            .map(String::from)
            .collect();
        if embedded.is_empty() {
            shutdown.wait().await;
            return;
        }

        let mut ticker = tokio::time::interval(BOT_DISCOVERY_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = shutdown.wait() => break,
                _ = ticker.tick() => {
                    for ct_str in &embedded {
                        if started.contains(ct_str) {
                            continue;
                        }
                        let ct = ChannelType::from(ct_str.as_str());
                        let has_live = match this.bot_store.list_live(&ct).await {
                            Ok(rows) => !rows.is_empty(),
                            Err(e) => {
                                tracing::warn!(
                                    channel_type = %ct_str,
                                    error = %format!("{e:#}"),
                                    "list live bots failed; will retry next discovery tick",
                                );
                                continue;
                            }
                        };
                        if !has_live {
                            continue;
                        }
                        let writer = build_channel_log_writer(
                            &this.channel_log_dir,
                            ct_str,
                            Arc::clone(&this.leak_detector),
                            &mut guards,
                        );
                        started.insert(ct_str.clone());
                        let sv = Arc::clone(&this);
                        let sd = shutdown.clone();
                        let ct_owned = ct_str.clone();
                        tracing::info!(
                            channel_type = %ct_str,
                            "registered bot detected; starting sidecar",
                        );
                        tasks.push(tokio::spawn(async move {
                            sv.supervise_one(ct_owned, sd, writer).await;
                        }));
                    }
                }
            }
        }
        for t in tasks {
            let _ = t.await;
        }
        drop(guards);
    }

    async fn supervise_one(
        self: Arc<Self>,
        channel_type: String,
        shutdown: ShutdownSignal,
        writer: Option<ChannelLogWriter>,
    ) {
        let bundle = match self.runtime.bundle_for(&channel_type) {
            Some(p) => p.to_owned(),
            None => return,
        };
        let bun = bun_binary();

        let mut backoff = BACKOFF_MIN;
        while !shutdown.is_shutdown() {
            let mut cmd = Command::new(&bun);
            cmd.arg(&bundle);
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());
            let handle = match self.spawner.spawn(
                cmd,
                format!("sidecar-{channel_type}"),
                channel_type.clone(),
            ) {
                Ok(h) => h,
                Err(e) => {
                    tracing::error!(
                        %channel_type,
                        error = %e,
                        "spawn sidecar failed; backing off",
                    );
                    if !wait_or_shutdown(&shutdown, backoff).await {
                        return;
                    }
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                    continue;
                }
            };
            let pid = handle.pid();
            tracing::info!(
                %channel_type,
                pid,
                bundle = %bundle.display(),
                "sidecar spawned",
            );

            let started = Instant::now();
            let mut handle = handle;
            let stdout_task = handle.child_mut().stdout.take().map(|out| {
                tokio::spawn(drain_pipe(
                    out,
                    Arc::clone(&self.log_buffer),
                    writer.clone(),
                    channel_type.clone(),
                    format!("sidecar::{channel_type}::stdout"),
                    Stream::Stdout,
                    LogLevel::Info,
                ))
            });
            let stderr_task = handle.child_mut().stderr.take().map(|err| {
                tokio::spawn(drain_pipe(
                    err,
                    Arc::clone(&self.log_buffer),
                    writer.clone(),
                    channel_type.clone(),
                    format!("sidecar::{channel_type}::stderr"),
                    Stream::Stderr,
                    LogLevel::Warn,
                ))
            });
            tokio::select! {
                _ = shutdown.wait() => {
                    if let Err(e) = handle.child_mut().start_kill() {
                        tracing::debug!(%channel_type, pid, error = %e, "start_kill failed");
                    }
                    let _ = handle.child_mut().wait().await;
                    tracing::info!(%channel_type, pid, "sidecar stopped on shutdown");
                    join_pipe_readers(stdout_task, stderr_task).await;
                    return;
                }
                status = handle.child_mut().wait() => {
                    match status {
                        Ok(s) => tracing::warn!(
                            %channel_type,
                            pid,
                            exit = ?s,
                            uptime_ms = started.elapsed().as_millis() as u64,
                            "sidecar exited; restarting after backoff",
                        ),
                        Err(e) => tracing::warn!(
                            %channel_type,
                            pid,
                            error = %e,
                            "sidecar wait failed; restarting",
                        ),
                    }
                }
            }
            join_pipe_readers(stdout_task, stderr_task).await;
            drop(handle); // revokes the token early

            if started.elapsed() >= UPTIME_RESET_THRESHOLD {
                backoff = BACKOFF_MIN;
            }
            if !wait_or_shutdown(&shutdown, backoff).await {
                return;
            }
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }
}

/// Build the per-channel-type daily-rolling appender and wrap it in
/// the shared leak-detector's redactor. Pushes the `WorkerGuard` into
/// `guards` so the caller keeps flushing alive for the supervisor's
/// lifetime. Returns `None` only if the caller should skip the file
/// sink entirely (currently never — left as a future opt-out).
fn build_channel_log_writer(
    dir: &Path,
    channel_type: &str,
    leak_detector: Arc<LeakDetector>,
    guards: &mut Vec<WorkerGuard>,
) -> Option<ChannelLogWriter> {
    let appender = tracing_appender::rolling::daily(dir, format!("{channel_type}.log"));
    let (nb, guard) = tracing_appender::non_blocking(appender);
    guards.push(guard);
    Some(Arc::new(RedactingMakeWriter::new(leak_detector, nb)))
}

/// Sleep `dur` but wake early on shutdown. Returns `false` if shutdown
/// fired (caller should stop looping).
async fn wait_or_shutdown(shutdown: &ShutdownSignal, dur: Duration) -> bool {
    tokio::select! {
        _ = shutdown.wait() => false,
        _ = sleep(dur) => true,
    }
}

#[derive(Copy, Clone)]
enum Stream {
    Stdout,
    Stderr,
}

impl Stream {
    fn tag(self) -> &'static str {
        match self {
            Stream::Stdout => "stdout",
            Stream::Stderr => "stderr",
        }
    }
}

async fn drain_pipe<R>(
    reader: R,
    buffer: Arc<LogBuffer>,
    file_writer: Option<ChannelLogWriter>,
    channel_type: String,
    target: String,
    stream: Stream,
    level: LogLevel,
) where
    R: AsyncRead + Unpin + Send + 'static,
{
    // Two-stage cap. `read_capped_line` keeps the read buffer
    // bounded at PIPE_LINE_READ_BUF_MAX (64 KiB) — a sidecar that
    // emits a multi-GB unbroken stream without a newline can no
    // longer OOM the supervisor. The display-side cap below
    // (SIDECAR_PIPE_LINE_MAX_BYTES = 1024) trims what we write to
    // the LogBuffer / file / terminal. Cap-hits get a dedicated
    // suffix so an operator sees the difference between a normal
    // long line and a runaway stream.
    let mut reader = BufReader::new(reader);
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    loop {
        buf.clear();
        let outcome = match read_capped_line(&mut reader, &mut buf).await {
            Ok(LineOutcome::Eof) => break,
            Ok(o) => o,
            Err(e) => {
                let msg = format!("[aura] sidecar pipe read error: {e}");
                if let Some(mw) = file_writer.as_ref() {
                    write_line_to_file(mw, stream.tag(), &msg);
                }
                tee_to_terminal(stream, &channel_type, &msg);
                buffer.push_external(LogLevel::Warn, target.clone(), msg);
                break;
            }
        };
        // Strip trailing CRLF/LF for cleaner log output (mirrors
        // `BufReader::lines`'s behaviour the previous impl relied on).
        if buf.last() == Some(&b'\n') {
            buf.pop();
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
        }
        let raw = String::from_utf8_lossy(&buf);
        let cap_hit = outcome == LineOutcome::CapHit;
        let suffix = if cap_hit {
            " [...truncated, line exceeded read cap]"
        } else if raw.len() > SIDECAR_PIPE_LINE_MAX_BYTES {
            " [...truncated]"
        } else {
            ""
        };
        let cut_at = if raw.len() > SIDECAR_PIPE_LINE_MAX_BYTES {
            let mut cut = SIDECAR_PIPE_LINE_MAX_BYTES;
            while cut > 0 && !raw.is_char_boundary(cut) {
                cut -= 1;
            }
            cut
        } else {
            raw.len()
        };
        let mut trimmed = raw[..cut_at].to_owned();
        trimmed.push_str(suffix);
        if let Some(mw) = file_writer.as_ref() {
            write_line_to_file(mw, stream.tag(), &trimmed);
        }
        tee_to_terminal(stream, &channel_type, &trimmed);
        buffer.push_external(level, target.clone(), trimmed);

        // After a cap-hit flush, resync at the next newline so the
        // remainder of the unbounded line isn't emitted as if it
        // were a fresh line.
        if cap_hit {
            let _ = drain_until_newline(&mut reader).await;
        }
    }
}

fn write_line_to_file(mw: &ChannelLogWriter, stream_tag: &str, line: &str) {
    let mut w = mw.make_writer();
    let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z");
    let _ = writeln!(w, "{ts} [{stream_tag}] {line}");
}

fn tee_to_terminal(stream: Stream, channel_type: &str, line: &str) {
    match stream {
        Stream::Stdout => {
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "[{channel_type}] {line}");
        }
        Stream::Stderr => {
            let mut err = std::io::stderr().lock();
            let _ = writeln!(err, "[{channel_type}] {line}");
        }
    }
}

async fn join_pipe_readers(
    stdout: Option<tokio::task::JoinHandle<()>>,
    stderr: Option<tokio::task::JoinHandle<()>>,
) {
    if let Some(t) = stdout {
        let _ = t.await;
    }
    if let Some(t) = stderr {
        let _ = t.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log_buffer::LogQuery;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn pipe_forwards_each_line_to_buffer() {
        let buf = LogBuffer::new(8);
        let (reader, mut writer) = tokio::io::duplex(64);
        let target = "sidecar::telegram::stdout".to_string();
        let drain = tokio::spawn(drain_pipe(
            reader,
            Arc::clone(&buf),
            None,
            "telegram".to_string(),
            target.clone(),
            Stream::Stdout,
            LogLevel::Info,
        ));
        writer
            .write_all(b"first line\nsecond line\n")
            .await
            .unwrap();
        drop(writer);
        drain.await.unwrap();

        let page = buf.query(&LogQuery {
            limit: 10,
            ..Default::default()
        });
        assert_eq!(page.total, 2);
        assert_eq!(page.items[0].message, "second line");
        assert_eq!(page.items[1].message, "first line");
        assert!(page.items.iter().all(|r| r.target == target));
    }

    #[tokio::test]
    async fn pipe_writes_redacted_lines_to_per_channel_file() {
        let dir = tempfile::tempdir().unwrap();
        let leak = Arc::new(LeakDetector::with_default_rules());
        let mut guards = Vec::new();
        let mw = build_channel_log_writer(dir.path(), "telegram", leak, &mut guards).unwrap();

        let buf = LogBuffer::new(8);
        let (reader, mut writer) = tokio::io::duplex(4096);
        let drain = tokio::spawn(drain_pipe(
            reader,
            Arc::clone(&buf),
            Some(mw),
            "telegram".to_string(),
            "sidecar::telegram::stdout".to_string(),
            Stream::Stdout,
            LogLevel::Info,
        ));
        let secret_line = b"bootstrapping with key AKIAIOSFODNN7EXAMPLE now\n";
        writer.write_all(secret_line).await.unwrap();
        drop(writer);
        drain.await.unwrap();
        drop(guards); // flush the non-blocking appender

        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1);
        let name = entries[0].file_name().into_string().unwrap();
        assert!(name.starts_with("telegram.log."), "got {name}");
        let contents = std::fs::read_to_string(entries[0].path()).unwrap();
        assert!(contents.contains("[stdout]"));
        assert!(!contents.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(contents.contains("[{REDACTED_LOG_aws_access_key}]"));
    }

    #[tokio::test]
    async fn pipe_truncates_oversized_line() {
        let buf = LogBuffer::new(4);
        let (reader, mut writer) = tokio::io::duplex(4096);
        let drain = tokio::spawn(drain_pipe(
            reader,
            Arc::clone(&buf),
            None,
            "t".to_string(),
            "sidecar::t::stdout".to_string(),
            Stream::Stdout,
            LogLevel::Info,
        ));
        let big = "x".repeat(SIDECAR_PIPE_LINE_MAX_BYTES + 128);
        writer.write_all(big.as_bytes()).await.unwrap();
        writer.write_all(b"\n").await.unwrap();
        drop(writer);
        drain.await.unwrap();

        let page = buf.query(&LogQuery {
            limit: 10,
            ..Default::default()
        });
        assert_eq!(page.total, 1);
        let rec = &page.items[0];
        assert!(rec.message.ends_with("[...truncated]"));
        assert!(rec.message.len() <= SIDECAR_PIPE_LINE_MAX_BYTES + " [...truncated]".len());
    }

    #[tokio::test]
    async fn pipe_bounds_unbroken_stream_at_read_cap() {
        // Regression: a sidecar that emits a huge unbroken stream
        // (no newline) must not OOM the supervisor. Pre-migration
        // `BufReader::lines()` had no per-line cap; with pipe_pump
        // every read is bounded by PIPE_LINE_READ_BUF_MAX (64 KiB)
        // and a cap-hit gets emitted as a truncated line with the
        // dedicated suffix so an operator sees the real cause.
        use crate::sidecar::pipe_pump::PIPE_LINE_READ_BUF_MAX;
        let buf = LogBuffer::new(8);
        let (reader, mut writer) = tokio::io::duplex(PIPE_LINE_READ_BUF_MAX * 4);
        let drain = tokio::spawn(drain_pipe(
            reader,
            Arc::clone(&buf),
            None,
            "t".to_string(),
            "sidecar::t::stdout".to_string(),
            Stream::Stdout,
            LogLevel::Info,
        ));
        // Two cap-fulls + a final "tail\n" so the drain returns.
        let blast = "y".repeat(PIPE_LINE_READ_BUF_MAX * 2);
        writer.write_all(blast.as_bytes()).await.unwrap();
        writer.write_all(b"tail\n").await.unwrap();
        drop(writer);
        drain.await.unwrap();

        let page = buf.query(&LogQuery {
            limit: 10,
            ..Default::default()
        });
        // Each cap-hit emits exactly one record + the trailing
        // "tail" line after we drain past the next newline. So
        // we expect 1 cap-hit record (since the drain after the
        // first cap consumes the rest of the unbroken blast and
        // the "tail" newline) — but the suffix tells us either
        // way. Assert there is at least one cap-hit record.
        let cap_hit = page
            .items
            .iter()
            .any(|r| r.message.contains("exceeded read cap"));
        assert!(
            cap_hit,
            "expected a cap-hit truncation record, got: {:?}",
            page.items.iter().map(|r| &r.message).collect::<Vec<_>>(),
        );
        // No record exceeds the display cap (1024) plus the
        // suffix, even though the input was 128 KiB.
        for rec in &page.items {
            assert!(
                rec.message.len()
                    <= SIDECAR_PIPE_LINE_MAX_BYTES
                        + " [...truncated, line exceeded read cap]".len(),
                "record exceeded display cap: len={}",
                rec.message.len(),
            );
        }
    }
}
