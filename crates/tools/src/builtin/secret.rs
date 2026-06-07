//! User-secret management tools: `SecretAdd`, `SecretList`, `SecretCheck`.
//!
//! All three route through [`crate::SecretAccess`] on the tool context (the
//! agent layer binds it over the security gateway). They are deliberately
//! value-blind: `SecretAdd` writes a value but never echoes it, and neither
//! `SecretList` nor `SecretCheck` returns any secret material — only names and
//! booleans. Deleting a secret is intentionally CLI-only (no agent tool); see
//! `docs/secret-management.md`.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::{SecretAccess, Tool, ToolConcurrency, ToolContext, ToolError, ToolOutput};

/// Borrow the secret accessor or fail closed when the context has none wired.
fn secrets(ctx: &ToolContext) -> crate::Result<&dyn SecretAccess> {
    ctx.secrets
        .as_deref()
        .ok_or_else(|| ToolError::Execution("secret store is not available in this context".into()))
}

pub struct SecretAddTool;

#[derive(Deserialize)]
struct AddParams {
    name: String,
    value: String,
    #[serde(default)]
    overwrite: bool,
}

#[async_trait]
impl Tool for SecretAddTool {
    fn name(&self) -> &str {
        "SecretAdd"
    }

    fn description(&self) -> String {
        "Store a user secret (e.g. an API token) under an environment-variable name so shell \
         commands can use it via the Bash tool's `secret_env` without you ever seeing the value. \
         Pass the secret as `value`; if it arrived in chat as a redacted placeholder, pass that \
         placeholder verbatim — it resolves to the real value at the execution boundary and is \
         never written to the transcript. Set `overwrite: true` to replace an existing name. \
         Values are write-only: no tool ever returns them."
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Env-var-style name, e.g. OPENAI_API_KEY (must match [A-Za-z_][A-Za-z0-9_]*)" },
                "value": { "type": "string", "description": "The secret value, or a redacted placeholder that resolves to it" },
                "overwrite": { "type": "boolean", "default": false, "description": "Replace the value if the name already exists" }
            },
            "required": ["name", "value"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: AddParams =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        let outcome = secrets(ctx)?
            .add(&p.name, p.value.as_bytes(), p.overwrite)
            .await?;
        let status = match outcome {
            aura_security::AddOutcome::Created => "created",
            aura_security::AddOutcome::Replaced => "replaced",
        };
        Ok(ToolOutput::Json(
            json!({ "name": p.name, "status": status }),
        ))
    }
}

pub struct SecretListTool;

#[async_trait]
impl Tool for SecretListTool {
    fn name(&self) -> &str {
        "SecretList"
    }

    /// Read-only (names only) — safe to run concurrently.
    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }

    fn description(&self) -> String {
        "List the names of stored user secrets (names only — values are never returned). Use \
         these names with SecretCheck or the Bash tool's `secret_env`."
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    async fn execute(&self, _params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let names = secrets(ctx)?.list_names().await?;
        Ok(ToolOutput::Json(json!({ "names": names })))
    }
}

pub struct SecretCheckTool;

#[derive(Deserialize)]
struct CheckParams {
    names: Vec<String>,
}

#[async_trait]
impl Tool for SecretCheckTool {
    fn name(&self) -> &str {
        "SecretCheck"
    }

    /// Read-only existence check — safe to run concurrently.
    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }

    fn description(&self) -> String {
        "Check whether named user secrets exist before relying on them (e.g. in the Bash tool's \
         `secret_env`). Returns a name→boolean map; never returns values."
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "names": { "type": "array", "items": { "type": "string" }, "description": "Secret names to check" }
            },
            "required": ["names"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: CheckParams =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        let results = secrets(ctx)?.exists(&p.names).await?;
        let map: Map<String, Value> = results
            .into_iter()
            .map(|(name, exists)| (name, Value::Bool(exists)))
            .collect();
        Ok(ToolOutput::Json(Value::Object(map)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_model::{ChannelType, User};
    use aura_security::AddOutcome;
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    #[derive(Default)]
    struct FakeSecrets {
        store: Mutex<HashMap<String, String>>,
    }

    #[async_trait]
    impl SecretAccess for FakeSecrets {
        async fn resolve_env(&self, names: &[String]) -> crate::Result<Vec<(String, String)>> {
            let store = self.store.lock();
            names
                .iter()
                .map(|n| {
                    store
                        .get(n)
                        .map(|v| (n.clone(), v.clone()))
                        .ok_or_else(|| ToolError::InvalidParams(format!("missing {n}")))
                })
                .collect()
        }

        async fn redact(&self, text: &str, values: &[String]) -> crate::Result<String> {
            let mut out = text.to_string();
            for v in values {
                out = out.replace(v, "[REDACTED]");
            }
            Ok(out)
        }

        async fn add(
            &self,
            name: &str,
            value: &[u8],
            overwrite: bool,
        ) -> crate::Result<AddOutcome> {
            let mut store = self.store.lock();
            let existed = store.contains_key(name);
            if existed && !overwrite {
                return Err(ToolError::Execution(format!(
                    "secret '{name}' already exists"
                )));
            }
            store.insert(
                name.to_string(),
                String::from_utf8_lossy(value).into_owned(),
            );
            Ok(if existed {
                AddOutcome::Replaced
            } else {
                AddOutcome::Created
            })
        }

        async fn list_names(&self) -> crate::Result<Vec<String>> {
            let mut names: Vec<String> = self.store.lock().keys().cloned().collect();
            names.sort();
            Ok(names)
        }

        async fn exists(&self, names: &[String]) -> crate::Result<Vec<(String, bool)>> {
            let store = self.store.lock();
            Ok(names
                .iter()
                .map(|n| (n.clone(), store.contains_key(n)))
                .collect())
        }
    }

    fn ctx_with(secrets: Option<Arc<dyn SecretAccess>>) -> ToolContext {
        ToolContext {
            session_id: "t".into(),
            job_id: aura_model::JobId::default(),
            span_id: aura_model::SpanId::default(),
            user: User {
                id: "u".into(),
                name: None,
                channel: ChannelType::tui(),
            },
            timeout: Duration::from_secs(1),
            cancellation_token: CancellationToken::new(),
            workspace_root: std::path::PathBuf::from("/tmp"),
            workspace_paths: aura_workspace::WorkspacePaths::new("/tmp"),
            sandbox: None,
            approval: None,
            notifier: None,
            events: crate::noop_event_sink(),
            llm: None,
            secrets,
            virtual_reads: None,
            background_jobs: None,
        }
    }

    #[tokio::test]
    async fn add_then_list_and_check() {
        let fake = Arc::new(FakeSecrets::default());
        let ctx = ctx_with(Some(fake.clone()));

        let out = SecretAddTool
            .execute(json!({ "name": "FOO", "value": "sk-123" }), &ctx)
            .await
            .unwrap();
        let ToolOutput::Json(v) = out else {
            panic!("json")
        };
        assert_eq!(v["status"], "created");

        let out = SecretListTool.execute(json!({}), &ctx).await.unwrap();
        let ToolOutput::Json(v) = out else {
            panic!("json")
        };
        assert_eq!(v["names"], json!(["FOO"]));

        let out = SecretCheckTool
            .execute(json!({ "names": ["FOO", "BAR"] }), &ctx)
            .await
            .unwrap();
        let ToolOutput::Json(v) = out else {
            panic!("json")
        };
        assert_eq!(v, json!({ "FOO": true, "BAR": false }));
    }

    #[tokio::test]
    async fn add_overwrite_guard() {
        let ctx = ctx_with(Some(Arc::new(FakeSecrets::default())));
        SecretAddTool
            .execute(json!({ "name": "FOO", "value": "a" }), &ctx)
            .await
            .unwrap();
        assert!(
            SecretAddTool
                .execute(json!({ "name": "FOO", "value": "b" }), &ctx)
                .await
                .is_err()
        );
        let out = SecretAddTool
            .execute(
                json!({ "name": "FOO", "value": "b", "overwrite": true }),
                &ctx,
            )
            .await
            .unwrap();
        let ToolOutput::Json(v) = out else {
            panic!("json")
        };
        assert_eq!(v["status"], "replaced");
    }

    #[tokio::test]
    async fn fails_closed_without_secret_store() {
        let ctx = ctx_with(None);
        assert!(SecretListTool.execute(json!({}), &ctx).await.is_err());
    }
}
