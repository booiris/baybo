pub mod agent;
pub mod agents;
pub mod channel;
pub mod completion;
pub mod config;
pub mod cost;
pub mod cron;
pub mod doctor;
pub mod job;
pub mod llm;
pub mod log;
pub mod mcp;
pub mod pair;
pub(crate) mod prompt;
pub(crate) mod secret_input;
pub(crate) mod select;
pub mod session;
pub mod skills;
pub mod status;

/// Parse a `YYYY-MM-DD` CLI argument with a consistent error message
/// across every command that accepts a date flag (`--date`, `--since`,
/// `--until`, …). The `flag` parameter is interpolated into the error
/// so the user sees which flag they botched.
pub(crate) fn parse_date_arg(raw: &str, flag: &str) -> crate::error::Result<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d").map_err(|e| {
        crate::error::CliError::Parse(format!("invalid {flag} {raw:?}: expected YYYY-MM-DD ({e})"))
    })
}

/// Maximum byte length for a channel-name CLI argument. Real channel
/// types (`telegram`, `slack`, `discord`, `tui`, `http`, …) are all
/// well under this; the cap is purely a defensive bound against
/// pathological inputs.
pub(crate) const MAX_CHANNEL_NAME_LEN: usize = 64;

/// Wired as a clap `value_parser` on `<channel>` positionals (notably
/// `aura log channel <channel>`) so the validator runs before any
/// handler builds a path from the raw input.
///
/// Restricts to `[a-z0-9_-]+` so the value can land in a filename
/// component (`<workspace>/logs/channel/<channel>.log.<date>`) with
/// **no** way to escape the directory:
///
/// * No `.` → cannot become `..` or `.git`/`.ssh`/etc.
/// * No `/` or `\` → cannot insert a path separator.
/// * No uppercase or Unicode → forces canonical casing for the
///   sidecar filename, which the tracing appender writes in lower
///   case anyway.
///
/// Rejecting at parse time also closes the slash-mode path: the same
/// clap tree validates slash dispatch, so `/log channel ../../etc`
/// fails before the whitelist gate even runs.
pub(crate) fn parse_channel_name(raw: &str) -> Result<String, String> {
    if raw.is_empty() {
        return Err("channel name must not be empty".into());
    }
    if raw.len() > MAX_CHANNEL_NAME_LEN {
        return Err(format!(
            "channel name too long ({} bytes; max {MAX_CHANNEL_NAME_LEN})",
            raw.len()
        ));
    }
    if !raw
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return Err(format!(
            "channel name {raw:?} must match [a-z0-9_-]+ (no path separators, no dots)"
        ));
    }
    Ok(raw.to_string())
}
