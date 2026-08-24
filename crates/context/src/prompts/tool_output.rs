//! Byte-budget cap and content-addressed spill for tool output entering the
//! LLM transcript.
//!
//! The `<tool_output>` envelope itself lives in `baybo-model`
//! ([`baybo_model::wrap_tool_output`]), beside the delimiter constants it keys
//! off, because `baybo-tools` needs the same framing for its judge prompts and
//! cannot depend on this crate. Detection and secret sanitization stay in
//! `baybo-security`.

use std::path::{Path, PathBuf};

/// Truncate `content` to at most [`baybo_model::MAX_TOOL_OUTPUT_BYTES`] at a UTF-8 char
/// boundary, appending a notice when truncation happened. When `spill_path`
/// is set the notice points the model at the full payload (readable back via
/// the `Read` tool).
pub fn cap_tool_output(content: String, spill_path: Option<&Path>) -> String {
    let limit = baybo_model::MAX_TOOL_OUTPUT_BYTES;
    if content.len() <= limit {
        return content;
    }
    let mut cut = limit;
    while cut > 0 && !content.is_char_boundary(cut) {
        cut -= 1;
    }
    let limit_kib = limit / 1024;
    let notice = match spill_path {
        Some(path) => format!(
            "\n\n[... truncated: {shown}/{total} bytes shown (per-tool-result cap is {kib} KiB / {limit} bytes). Full output written to {path} — call the `Read` tool with that absolute path (use `offset`/`limit` for ranges) to fetch the rest, or re-run the original tool with a narrower scope.]",
            shown = cut,
            total = content.len(),
            kib = limit_kib,
            limit = limit,
            path = path.display(),
        ),
        None => format!(
            "\n\n[... truncated: {shown}/{total} bytes shown (per-tool-result cap is {kib} KiB / {limit} bytes). Re-run the tool with a narrower scope for the rest.]",
            shown = cut,
            total = content.len(),
            kib = limit_kib,
            limit = limit,
        ),
    };
    let mut out = String::with_capacity(cut + notice.len());
    out.push_str(&content[..cut]);
    out.push_str(&notice);
    out
}

/// Write `bytes` to `<dir>/<sha256-hex>.txt`, creating `dir` if missing.
/// Content-addressed so identical spills dedupe automatically. Returns the
/// absolute path on success; errors are logged and turn into `None` so the
/// caller falls back to the plain truncation notice.
pub(crate) async fn spill_tool_output(dir: &Path, bytes: &[u8]) -> Option<PathBuf> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let hex = hex::encode(hasher.finalize());
    let path = dir.join(format!("{hex}.txt"));

    if let Err(e) = tokio::fs::create_dir_all(dir).await {
        tracing::warn!(
            error = %e,
            dir = %dir.display(),
            "failed to create tool-spill directory; truncation notice will omit the path"
        );
        return None;
    }
    // Idempotent write: if the same content has already been spilled (same
    // sha256), reuse the existing file rather than rewriting it.
    match tokio::fs::metadata(&path).await {
        Ok(meta) if meta.is_file() => return Some(path),
        _ => {}
    }
    if let Err(e) = tokio::fs::write(&path, bytes).await {
        tracing::warn!(
            error = %e,
            path = %path.display(),
            "failed to write tool-spill file; truncation notice will omit the path"
        );
        return None;
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_preserves_short_content() {
        let s = "hello".to_string();
        assert_eq!(cap_tool_output(s.clone(), None), s);
    }

    #[test]
    fn cap_truncates_long_content() {
        let big = "x".repeat(baybo_model::MAX_TOOL_OUTPUT_BYTES + 1024);
        let out = cap_tool_output(big.clone(), None);
        assert!(out.len() < big.len());
        assert!(out.contains("[... truncated"));
    }

    #[test]
    fn cap_notice_references_spill_path() {
        let big = "x".repeat(baybo_model::MAX_TOOL_OUTPUT_BYTES + 1024);
        let out = cap_tool_output(big, Some(Path::new("/tmp/spill/abc.txt")));
        assert!(out.contains("Full output written to /tmp/spill/abc.txt"));
        assert!(out.contains("`Read` tool"));
        assert!(out.contains(&format!("{} bytes", baybo_model::MAX_TOOL_OUTPUT_BYTES)));
    }

    #[test]
    fn cap_respects_char_boundary() {
        // 4-byte chars so most byte indices are non-boundaries.
        let big = "🐙".repeat(baybo_model::MAX_TOOL_OUTPUT_BYTES);
        let out = cap_tool_output(big, None);
        assert!(out.len() >= baybo_model::MAX_TOOL_OUTPUT_BYTES - 4);
        assert!(out.contains("[... truncated"));
    }

    #[tokio::test]
    async fn spill_writes_full_payload_and_dedups() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bytes = "x"
            .repeat(baybo_model::MAX_TOOL_OUTPUT_BYTES + 1024)
            .into_bytes();

        let p1 = spill_tool_output(dir.path(), &bytes)
            .await
            .expect("spill path");
        let written = tokio::fs::read(&p1).await.expect("read spill");
        assert_eq!(
            written.len(),
            bytes.len(),
            "spill must hold the full payload"
        );

        // Identical content → same content-addressed path, one file on disk.
        let p2 = spill_tool_output(dir.path(), &bytes)
            .await
            .expect("spill path");
        assert_eq!(p1, p2);

        let mut entries = tokio::fs::read_dir(dir.path()).await.expect("read dir");
        let mut count = 0;
        while entries.next_entry().await.expect("entry").is_some() {
            count += 1;
        }
        assert_eq!(count, 1, "identical content should produce one spill file");
    }
}
