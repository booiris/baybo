pub mod agent;
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
