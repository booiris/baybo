//! Offline pass that walks each bench's `trace/` dir and writes a tiny
//! `<trace>.tools.json` sidecar next to every `trace.json` — the per-tool
//! call counts the list view reads cheaply instead of re-parsing traces
//! that can run to hundreds of MB. Run it for backfill and after each
//! bench finishes (the harness can invoke the `precompute-tool-counts`
//! bin). Writing sidecars next to the (gitignored) traces keeps the
//! viewer itself read-only on the request path.

use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::adapters::BENCHES;
use crate::model::ToolCount;
use crate::trace::{count_tools_in_trace, tool_sidecar_rel};

/// Write/refresh tool-count sidecars for every trace under each known
/// bench's `trace/` dir below `root`. `only_bench` restricts the sweep to
/// a single bench id (its dir name) — what a per-bench `consolidate.sh`
/// passes so it touches only its own traces; `None` sweeps all (the
/// viewer launcher). Returns how many were written. Best-effort per file:
/// a read/parse failure is logged and skipped so one bad trace can't abort
/// the sweep.
pub fn write_tool_count_sidecars(root: &Path, only_bench: Option<&str>) -> anyhow::Result<usize> {
    let mut written = 0;
    for spec in BENCHES {
        if only_bench.is_some_and(|b| b != spec.id) {
            continue;
        }
        let trace_dir = root.join(spec.id).join("trace");
        if !trace_dir.is_dir() {
            continue;
        }
        let mut traces = Vec::new();
        collect_traces(&trace_dir, &mut traces);
        for trace in traces {
            match write_one(&trace) {
                Ok(true) => written += 1,
                Ok(false) => {} // sidecar already up to date
                Err(e) => {
                    tracing::warn!(path = %trace.display(), error = %e, "tool sidecar skipped")
                }
            }
        }
    }
    Ok(written)
}

/// Returns `true` if a sidecar was (re)written, `false` if the existing
/// one is already current — so re-running is cheap (only new/changed
/// traces are parsed, the rest are a stat).
fn write_one(trace: &Path) -> anyhow::Result<bool> {
    let sidecar = PathBuf::from(tool_sidecar_rel(&trace.to_string_lossy()));
    if sidecar_is_current(&sidecar, trace) {
        return Ok(false);
    }
    let raw =
        std::fs::read_to_string(trace).with_context(|| format!("read {}", trace.display()))?;
    let val: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", trace.display()))?;
    let tools: Vec<ToolCount> = count_tools_in_trace(&val);
    let body = serde_json::to_string(&tools).context("serialize tool counts")?;
    std::fs::write(&sidecar, body).with_context(|| format!("write {}", sidecar.display()))?;
    Ok(true)
}

/// A sidecar is current when it exists and is no older than its trace —
/// the cheap mtime check that makes a re-sweep a no-op (a stat per trace,
/// never a parse of the up-to-date ones).
fn sidecar_is_current(sidecar: &Path, trace: &Path) -> bool {
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    match (mtime(sidecar), mtime(trace)) {
        (Some(s), Some(t)) => s >= t,
        _ => false,
    }
}

/// Recursively collect trace files: file names ending in `trace.json`
/// (swe `<item>.trace.json`, tb `agent-logs/trace.json`, memory
/// `<sid>.trace.json`) — never the `messages.json` siblings nor the
/// `*.tools.json` sidecars we write. Symlinked entries (e.g.
/// `trace/latest`) are skipped so a dir isn't swept twice.
fn collect_traces(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(reader) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in reader.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_symlink() {
            continue;
        }
        let path = entry.path();
        if ft.is_dir() {
            collect_traces(&path, out);
        } else if ft.is_file()
            && path
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|n| n.ends_with("trace.json"))
        {
            out.push(path);
        }
    }
}
