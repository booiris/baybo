//! Trace + raw-file endpoints. Trace files are treated as opaque JSON
//! (the same "untyped on the Rust side by design" stance the gateway
//! takes): we only reshape the envelope so the frontend's ported viewer
//! types line up — `{session, turns}` + `{messages}` → `{session_id,
//! session_messages, turns}`.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use axum::Json;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::error::ApiError;
use crate::model::ToolCount;

/// Read an item's precomputed tool-count sidecar (`<trace>.tools.json`,
/// written by the `precompute-tool-counts` bin). Cheap by design — the
/// list view reads one tiny JSON per item and NEVER the trace, which can
/// run to hundreds of MB (a single terminal-bench trace has hit 166 MB).
/// Missing/malformed sidecar → empty (best-effort; tool stats are a
/// nicety, never load-bearing).
pub(crate) fn tool_counts(bench_dir: &Path, trace_rel: &str) -> Vec<ToolCount> {
    let Ok(path) = safe_join(bench_dir, &tool_sidecar_rel(trace_rel)) else {
        return Vec::new();
    };
    let Ok(val) = read_json(&path) else {
        return Vec::new();
    };
    serde_json::from_value(val).unwrap_or_default()
}

/// Sidecar path for a trace: swap the trailing `.json` for `.tools.json`
/// (`…/trace.json` → `…/trace.tools.json`). Shared by the reader above and
/// the precompute writer so they always agree.
pub(crate) fn tool_sidecar_rel(trace_rel: &str) -> String {
    match trace_rel.strip_suffix(".json") {
        Some(stem) => format!("{stem}.tools.json"),
        None => format!("{trace_rel}.tools.json"),
    }
}

/// Count `tool_call` spans by `tool_name` in a parsed `trace.json`,
/// highest first (ties broken by name). Walks the `turns[].steps[].spans[]`
/// shape `baybo session export` emits. The precompute bin runs this once
/// per trace and writes the result as the sidecar [`tool_counts`] reads.
pub(crate) fn count_tools_in_trace(val: &Value) -> Vec<ToolCount> {
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    let turns = val.get("turns").and_then(Value::as_array);
    for turn in turns.into_iter().flatten() {
        let steps = turn.get("steps").and_then(Value::as_array);
        for step in steps.into_iter().flatten() {
            let spans = step.get("spans").and_then(Value::as_array);
            for span in spans.into_iter().flatten() {
                let kind = span.get("kind");
                if kind.and_then(|k| k.get("kind")).and_then(Value::as_str) != Some("tool_call") {
                    continue;
                }
                if let Some(name) = kind
                    .and_then(|k| k.get("begin"))
                    .and_then(|b| b.get("tool_name"))
                    .and_then(Value::as_str)
                {
                    *counts.entry(name.to_string()).or_insert(0) += 1;
                }
            }
        }
    }
    let mut out: Vec<ToolCount> = counts
        .into_iter()
        .map(|(name, count)| ToolCount { name, count })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
    out
}

/// Reshape an item's `trace.json` (+ optional `messages.json`) into the
/// `{ session_id, session_messages, turns }` shape the frontend viewer
/// consumes.
pub(crate) fn trace_response(
    bench_dir: &Path,
    trace_rel: &str,
    messages_rel: Option<&str>,
) -> Result<Response, ApiError> {
    let trace_path = safe_join(bench_dir, trace_rel)?;
    let trace_val = read_json(&trace_path)?;
    let session = trace_val.get("session").cloned().unwrap_or(Value::Null);
    let turns = trace_val
        .get("turns")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));

    let session_messages = match messages_rel {
        Some(rel) => {
            let mp = safe_join(bench_dir, rel)?;
            read_json(&mp)?
                .get("messages")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new()))
        }
        None => Value::Array(Vec::new()),
    };

    let body = json!({
        "session_id": session,
        "session_messages": session_messages,
        "turns": turns,
    });
    Ok(Json(body).into_response())
}

/// Serve a raw side-artifact. A `<file>#<instance_id>` path extracts that
/// instance's `model_patch` from a SWE predictions `.jsonl`; otherwise
/// the file is returned verbatim with a content-type by extension.
pub(crate) fn file_response(bench_dir: &Path, spec_path: &str) -> Result<Response, ApiError> {
    if let Some((file_rel, fragment)) = spec_path.split_once('#') {
        let path = safe_join(bench_dir, file_rel)?;
        let content = std::fs::read_to_string(&path).map_err(|e| ApiError::Io(e.to_string()))?;
        for line in content.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if v.get("instance_id").and_then(Value::as_str) == Some(fragment) {
                let patch = v
                    .get("model_patch")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                return Ok(text_response(patch, "text/plain; charset=utf-8"));
            }
        }
        return Err(ApiError::NotFound);
    }

    let path = safe_join(bench_dir, spec_path)?;
    let bytes = std::fs::read(&path).map_err(|e| ApiError::Io(e.to_string()))?;
    let mime = if spec_path.ends_with(".json") {
        "application/json"
    } else {
        "text/plain; charset=utf-8"
    };
    Ok(([(header::CONTENT_TYPE, mime)], bytes).into_response())
}

fn text_response(body: String, mime: &'static str) -> Response {
    ([(header::CONTENT_TYPE, mime)], body).into_response()
}

fn read_json(path: &Path) -> Result<Value, ApiError> {
    let raw = std::fs::read_to_string(path).map_err(|e| ApiError::Io(e.to_string()))?;
    serde_json::from_str(&raw).map_err(|e| ApiError::Parse(e.to_string()))
}

/// Join a client-supplied relative path under `base`, rejecting any
/// traversal (`..`, absolute, prefix/root components) and — after
/// canonicalization — any symlink that escapes `base`. The target must
/// exist (these are read endpoints).
pub(crate) fn safe_join(base: &Path, rel: &str) -> Result<PathBuf, ApiError> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return Err(ApiError::BadPath);
    }
    for component in rel_path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ApiError::BadPath);
            }
        }
    }
    let joined = base.join(rel_path);
    let canon = joined.canonicalize().map_err(|_| ApiError::NotFound)?;
    let base_canon = base.canonicalize().map_err(|_| ApiError::NotFound)?;
    if !canon.starts_with(&base_canon) {
        return Err(ApiError::BadPath);
    }
    Ok(canon)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_accepts_contained_file_and_rejects_escapes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path();
        std::fs::write(base.join("ok.txt"), "hi").expect("write");

        assert!(safe_join(base, "ok.txt").is_ok());
        assert!(matches!(
            safe_join(base, "../etc/passwd"),
            Err(ApiError::BadPath)
        ));
        assert!(matches!(
            safe_join(base, "/etc/passwd"),
            Err(ApiError::BadPath)
        ));
        // Contained but non-existent → NotFound (canonicalize fails).
        assert!(matches!(
            safe_join(base, "nope.txt"),
            Err(ApiError::NotFound)
        ));
    }
}
