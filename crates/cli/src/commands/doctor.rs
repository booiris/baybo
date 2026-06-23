use serde_json::json;

use crate::context::CommandContext;
use crate::error::Result;
use crate::format::CommandOutput;

/// Run a set of non-invasive health checks and return a structured report.
///
/// Each check contributes one row of the form
/// `{ name, status: ok|warn|error, detail }`.
pub async fn handle(ctx: &CommandContext) -> Result<CommandOutput> {
    let mut rows: Vec<serde_json::Value> = Vec::new();
    let mut worst = CheckStatus::Ok;

    // 1. Config presence.
    match &ctx.config_path {
        Some(p) => {
            rows.push(row(
                "config.path",
                CheckStatus::Ok,
                format!("using {}", p.display()),
            ));
        }
        None => {
            rows.push(row(
                "config.path",
                CheckStatus::Warn,
                "no baybo.json resolved — running with defaults".into(),
            ));
            worst = worst.worse(CheckStatus::Warn);
        }
    }

    // 2. Encryption key file is readable.
    match &ctx.config.security.encryption_key_file {
        Some(path) => match std::fs::metadata(path) {
            Ok(_) => rows.push(row(
                "security.encryption_key_file",
                CheckStatus::Ok,
                format!("readable at {path}"),
            )),
            Err(e) => {
                rows.push(row(
                    "security.encryption_key_file",
                    CheckStatus::Error,
                    format!("{path}: {e}"),
                ));
                worst = worst.worse(CheckStatus::Error);
            }
        },
        None => {
            rows.push(row(
                "security.encryption_key_file",
                CheckStatus::Error,
                "not set in baybo.json — run `baybo setup` to mint a key".into(),
            ));
            worst = worst.worse(CheckStatus::Error);
        }
    }

    // 3. LLM client wired up.
    match ctx.llm.as_ref() {
        Some(c) => rows.push(row(
            "llm.client",
            CheckStatus::Ok,
            format!("{} / {}", c.model_info().provider, c.model_info().id),
        )),
        None => {
            rows.push(row(
                "llm.client",
                CheckStatus::Error,
                "no LLM client configured".into(),
            ));
            worst = worst.worse(CheckStatus::Error);
        }
    }

    // 4. At least one channel registered.
    if ctx.channels.is_empty() {
        rows.push(row(
            "channels.registered",
            CheckStatus::Warn,
            "no channels registered — no way to receive messages".into(),
        ));
        worst = worst.worse(CheckStatus::Warn);
    } else {
        rows.push(row(
            "channels.registered",
            CheckStatus::Ok,
            format!("{} channel(s) registered", ctx.channels.len()),
        ));
    }

    let value = json!({
        "status": worst.as_str(),
        "checks": rows,
    });
    let mut human = format!("overall: {}\n", worst.as_str());
    if let Some(arr) = value["checks"].as_array() {
        for check in arr {
            human.push_str(&format!(
                "  [{}] {}: {}\n",
                check["status"].as_str().unwrap_or("?"),
                check["name"].as_str().unwrap_or("?"),
                check["detail"].as_str().unwrap_or(""),
            ));
        }
    }
    Ok(CommandOutput {
        human: human.trim_end().to_string(),
        data: Some(value),
    })
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum CheckStatus {
    Ok,
    Warn,
    Error,
}

impl CheckStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
    fn worse(self, other: Self) -> Self {
        self.max(other)
    }
}

fn row(name: &str, status: CheckStatus, detail: String) -> serde_json::Value {
    json!({ "name": name, "status": status.as_str(), "detail": detail })
}
