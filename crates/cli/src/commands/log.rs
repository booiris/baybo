use std::path::Path;
use std::time::Duration;

use baybo_workspace::paths::{LOG_FILE_PREFIX, TUI_LOG_FILE_PREFIX};
use chrono::{NaiveDate, Utc};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::cli::LogCmd;
use crate::commands::parse_date_arg;
use crate::context::CommandContext;
use crate::error::{CliError, Result};
use crate::format::{CommandOutput, OutputFormat};

const FOLLOW_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Backwards-tail chunk size. 8 KiB keeps the average daily log
/// (~200 entries × ~150 B per line) to a single read while staying
/// small enough that `--limit 1` doesn't pull megabytes off disk.
const TAIL_CHUNK_BYTES: u64 = 8 * 1024;

pub async fn handle(ctx: &CommandContext, cmd: LogCmd) -> Result<CommandOutput> {
    match cmd {
        LogCmd::Main {
            date,
            limit,
            follow,
        } => read_main(ctx, date.as_deref(), limit, follow).await,
        LogCmd::Tui {
            date,
            limit,
            follow,
        } => read_tui(ctx, date.as_deref(), limit, follow).await,
        LogCmd::Channel {
            channel,
            date,
            limit,
            follow,
        } => read_channel(ctx, &channel, date.as_deref(), limit, follow).await,
    }
}

fn resolve_date(raw: Option<&str>) -> Result<NaiveDate> {
    match raw {
        Some(s) => parse_date_arg(s, "--date"),
        None => Ok(Utc::now().date_naive()),
    }
}

async fn read_main(
    ctx: &CommandContext,
    date: Option<&str>,
    limit: usize,
    follow: bool,
) -> Result<CommandOutput> {
    let date = resolve_date(date)?;
    let path = ctx
        .workspace
        .logs_dir()
        .join(format!("{LOG_FILE_PREFIX}.{date}"));
    read_log_file(ctx, "main", &path, limit, follow).await
}

async fn read_tui(
    ctx: &CommandContext,
    date: Option<&str>,
    limit: usize,
    follow: bool,
) -> Result<CommandOutput> {
    let date = resolve_date(date)?;
    let path = ctx
        .workspace
        .logs_dir()
        .join(format!("{TUI_LOG_FILE_PREFIX}.{date}"));
    read_log_file(ctx, "tui", &path, limit, follow).await
}

async fn read_channel(
    ctx: &CommandContext,
    channel: &str,
    date: Option<&str>,
    limit: usize,
    follow: bool,
) -> Result<CommandOutput> {
    let date = resolve_date(date)?;
    let path = ctx
        .workspace
        .channel_logs_dir()
        .join(format!("{channel}.log.{date}"));
    read_log_file(ctx, channel, &path, limit, follow).await
}

async fn read_log_file(
    ctx: &CommandContext,
    kind: &str,
    path: &Path,
    limit: usize,
    follow: bool,
) -> Result<CommandOutput> {
    if follow && matches!(ctx.format, OutputFormat::Json) {
        return Err(CliError::Parse(
            "--follow streams plain text and is incompatible with --json".into(),
        ));
    }

    let (tail, file_len) = tail_lines(path, limit, kind).await?;

    if follow {
        // Print the initial tail directly, then stream new lines until
        // Ctrl-C. We bypass `CommandOutput` here because the loop
        // produces an unbounded stream of text the caller never wraps.
        for line in &tail {
            println!("{line}");
        }
        follow_loop(path, file_len).await?;
        return Ok(CommandOutput {
            human: String::new(),
            data: None,
        });
    }

    let human = tail.join("\n");
    let line_values: Vec<Value> = tail.iter().cloned().map(Value::String).collect();

    Ok(CommandOutput {
        human,
        data: Some(json!({
            "path": path.display().to_string(),
            "returned_lines": tail.len(),
            "lines": line_values,
        })),
    })
}

/// Return up to `limit` trailing lines plus the file length at read
/// time. Reads chunks backwards from EOF until `limit + 1` newlines
/// are buffered (so the first line — which may be a partial mid-line
/// when we didn't seek all the way to byte 0 — can be dropped). This
/// avoids the obvious `read_to_end` + `lines().rev().take(N)` shape,
/// which on a multi-MB daily log allocates the whole file just to
/// keep 200 lines.
async fn tail_lines(path: &Path, limit: usize, kind: &str) -> Result<(Vec<String>, u64)> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| io_err(kind, path, e))?;
    let file_len = file
        .metadata()
        .await
        .map_err(|e| io_err(kind, path, e))?
        .len();

    if limit == 0 || file_len == 0 {
        return Ok((Vec::new(), file_len));
    }

    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut newlines: usize = 0;
    let mut pos = file_len;
    while pos > 0 && newlines <= limit {
        let read_size = std::cmp::min(TAIL_CHUNK_BYTES, pos);
        pos -= read_size;
        file.seek(std::io::SeekFrom::Start(pos))
            .await
            .map_err(|e| io_err(kind, path, e))?;
        let mut chunk = vec![0u8; read_size as usize];
        file.read_exact(&mut chunk)
            .await
            .map_err(|e| io_err(kind, path, e))?;
        newlines += chunk.iter().filter(|&&b| b == b'\n').count();
        chunks.push(chunk);
    }
    chunks.reverse();
    let buf: Vec<u8> = chunks.into_iter().flatten().collect();
    let text = String::from_utf8_lossy(&buf);

    // If we stopped before byte 0, the first "line" in `buf` is a
    // partial line — the tail of whatever began before our seek
    // window. Drop it so callers only see complete lines.
    let lines: Vec<String> = if pos > 0 {
        text.lines().skip(1).map(String::from).collect()
    } else {
        text.lines().map(String::from).collect()
    };
    let start = lines.len().saturating_sub(limit);
    Ok((lines[start..].to_vec(), file_len))
}

fn io_err(kind: &str, path: &Path, err: std::io::Error) -> CliError {
    if err.kind() == std::io::ErrorKind::NotFound {
        CliError::Io(format!(
            "no {kind} log at {} (wrong --date, or nothing logged that day)",
            path.display()
        ))
    } else {
        CliError::Io(format!("read {}: {err}", path.display()))
    }
}

/// Polling tail-f. Wakes every `FOLLOW_POLL_INTERVAL`, re-reads from
/// `pos`, and prints any new bytes. Exits cleanly on Ctrl-C. Handles
/// truncation by resetting `pos` to 0 — covers operator `> file` and
/// the appender's midnight rotation (after midnight the new lines land
/// in tomorrow's file; we keep watching today's so the operator sees
/// the rotation gap explicitly rather than silently switching files).
async fn follow_loop(path: &Path, mut pos: u64) -> Result<()> {
    let mut ticker = tokio::time::interval(FOLLOW_POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            _ = ticker.tick() => {}
        }
        let meta = match tokio::fs::metadata(path).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(CliError::Io(format!("stat {}: {e}", path.display()))),
        };
        let len = meta.len();
        if len < pos {
            pos = 0;
        }
        if len > pos {
            let mut file = tokio::fs::File::open(path)
                .await
                .map_err(|e| CliError::Io(format!("open {}: {e}", path.display())))?;
            file.seek(std::io::SeekFrom::Start(pos))
                .await
                .map_err(|e| CliError::Io(format!("seek {}: {e}", path.display())))?;
            let mut buf = Vec::with_capacity((len - pos) as usize);
            file.read_to_end(&mut buf)
                .await
                .map_err(|e| CliError::Io(format!("read {}: {e}", path.display())))?;
            print!("{}", String::from_utf8_lossy(&buf));
            use std::io::Write as _;
            let _ = std::io::stdout().flush();
            pos = len;
        }
    }
}
