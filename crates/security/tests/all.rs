// Aggregator: each sibling file is mounted as a module so the harness links
// once. See the Cargo.toml `autotests = false` rationale.

#[path = "injection_corpus.rs"]
mod injection_corpus;
#[path = "log_redaction.rs"]
mod log_redaction;
#[path = "placeholder_roundtrip.rs"]
mod placeholder_roundtrip;
#[path = "sensitive_paths_fs.rs"]
mod sensitive_paths_fs;
