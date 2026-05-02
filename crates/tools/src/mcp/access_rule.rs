//! Per-tool approval annotations carried in the rmcp `_meta` field.
//!
//! Vanilla MCP gives every tool on a server the same static
//! [`ResourceAccess`] (derived from the transport — stdio → one
//! `ExecCommand` for the whole server). Aura's embedded browser MCP
//! server needs finer per-call rules: `screenshot` always prompts (PII
//! exfil), `console` prompts only when an `expression` is non-empty,
//! `navigate` prompts only when the URL is a literal public IP.
//!
//! The server stuffs an Aura-specific block into each tool descriptor's
//! `_meta` field:
//!
//! ```jsonc
//! "_meta": {
//!   "aura": {
//!     "access_rule": { "kind": "public_ip_in_url_param", "param": "url" },
//!     "call_label":  { "kind": "from_param", "param": "url", "max_chars": 120 }
//!   }
//! }
//! ```
//!
//! Servers that don't carry `_meta.aura` get the defaults
//! ([`AccessRule::None`], [`CallLabelRule::None`]) — i.e. the historical
//! "fall back to the server-level resource list" path is preserved by
//! the caller, not by this module.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::approval::ResourceAccess;

/// Per-tool approval rule. Evaluated per-call against the request
/// params; the result feeds the executor's pre-execute approval gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../tool-src/browser/src/generated/")
)]
pub enum AccessRule {
    /// No prompt. Caller may still apply a server-level default if it
    /// wants to.
    None,
    /// Always prompt. `label_template` undergoes `{name}` substitution
    /// against the params.
    Always {
        resource: ResourceKind,
        label_template: String,
    },
    /// Prompt iff `param` is present and non-empty in the params object.
    IfParamPresent {
        param: String,
        resource: ResourceKind,
        label_template: String,
    },
    /// Prompt iff the URL in `param` resolves to a literal *public* IP
    /// address — hostnames bypass (DNS-level SSRF stays sidecar-side),
    /// blocked IP literals bypass (the sidecar's pre-flight rejects
    /// them anyway). Mirrors the rule the deleted `browser_navigate`
    /// applied in Rust.
    PublicIpInUrlParam { param: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../tool-src/browser/src/generated/")
)]
pub enum ResourceKind {
    Http,
    ExecCommand,
    ReadFile,
    WriteFile,
}

impl ResourceKind {
    fn build(&self, label: String) -> ResourceAccess {
        match self {
            Self::Http => ResourceAccess::Http { host: label },
            Self::ExecCommand => ResourceAccess::ExecCommand { command: label },
            Self::ReadFile => ResourceAccess::ReadFile {
                path: std::path::PathBuf::from(label),
            },
            Self::WriteFile => ResourceAccess::WriteFile {
                path: std::path::PathBuf::from(label),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../tool-src/browser/src/generated/")
)]
pub enum CallLabelRule {
    None,
    /// Take a string-typed `param` directly, truncate to `max_chars`.
    /// Use for tools whose primary input is itself the user-meaningful
    /// label (URL, expression, CDP method name, …).
    FromParam {
        param: String,
        max_chars: usize,
    },
    /// Render `template` against the params via the same `{name}` /
    /// `{name?then:else}` substitution the access-rule label uses.
    /// Use for tools whose label needs ternary branching on a flag
    /// (e.g. screenshot's "viewport" vs "full page") or static text
    /// the params don't carry verbatim.
    Template {
        template: String,
    },
}

/// Aura-specific block we look for inside the tool descriptor's
/// `_meta.aura`. All fields are optional: a missing block means the
/// caller falls back to the server defaults.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../tool-src/browser/src/generated/")
)]
pub struct AuraToolMeta {
    #[serde(default)]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub access_rule: Option<AccessRule>,
    #[serde(default)]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub call_label: Option<CallLabelRule>,
    /// When set, the executor injects `ctx.session_id` into the params
    /// object under this key before calling the server. Used by the
    /// embedded browser MCP server, whose handlers route per-Aura-session
    /// `BrowserContext`s by `context_id`. Vanilla MCP servers leave this
    /// `None` and never see session ids.
    #[serde(default)]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub inject_session_id_as: Option<String>,
}

impl AuraToolMeta {
    /// Pull the `aura` sub-object out of an rmcp `_meta` field. Returns
    /// `Default` (every field `None`) when the meta is missing,
    /// malformed, or doesn't carry an `aura` key.
    pub fn from_meta(meta: Option<&serde_json::Map<String, Value>>) -> Self {
        let Some(m) = meta else {
            return Self::default();
        };
        let Some(aura) = m.get("aura") else {
            return Self::default();
        };
        match serde_json::from_value(aura.clone()) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "MCP tool _meta.aura failed to parse; ignoring per-tool annotations",
                );
                Self::default()
            }
        }
    }
}

impl AccessRule {
    /// Evaluate the rule against a tool-call's params.
    pub fn evaluate(&self, params: &Value) -> Vec<ResourceAccess> {
        match self {
            Self::None => Vec::new(),
            Self::Always {
                resource,
                label_template,
            } => vec![resource.build(render_template(label_template, params))],
            Self::IfParamPresent {
                param,
                resource,
                label_template,
            } => {
                let present = params
                    .get(param)
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty());
                if present {
                    vec![resource.build(render_template(label_template, params))]
                } else {
                    Vec::new()
                }
            }
            Self::PublicIpInUrlParam { param } => params
                .get(param)
                .and_then(Value::as_str)
                .and_then(|s| url::Url::parse(s).ok())
                .and_then(|u| u.host_str().map(str::to_lowercase))
                .filter(|host| {
                    crate::builtin::url_policy::host_to_literal_ip(host)
                        .is_some_and(|ip| !aura_security::is_blocked_ip(&ip, false))
                })
                .map(|host| vec![ResourceAccess::Http { host }])
                .unwrap_or_default(),
        }
    }
}

impl CallLabelRule {
    /// Evaluate the rule against a tool-call's params. `None` when the
    /// rule produces no label.
    pub fn evaluate(&self, params: &Value) -> Option<String> {
        match self {
            Self::None => None,
            Self::FromParam { param, max_chars } => params
                .get(param)
                .and_then(Value::as_str)
                .map(|s| truncate_label(s, *max_chars)),
            Self::Template { template } => {
                let rendered = render_template(template, params);
                if rendered.trim().is_empty() {
                    None
                } else {
                    Some(rendered)
                }
            }
        }
    }
}

/// Substitution against the params object. Two forms inside `{ … }`:
///
/// * `{name}` — render the value at `params.name`. Missing or null
///   renders empty. No nested braces, no escaping.
/// * `{name?then:else}` — ternary on truthiness of `params.name`:
///   `then` when truthy, `else` when falsy. Truthy = bool true,
///   non-empty string, non-zero number, any object/array. Falsy =
///   missing, null, bool false, empty string, zero. Used by tools
///   whose label hinges on a boolean flag (e.g. screenshot's
///   "viewport" vs "full page"). The `then` / `else` branches are
///   plain literals — no nested substitution.
///
/// Bytes outside `{ … }` pass through unchanged.
fn render_template(template: &str, params: &Value) -> String {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{'
            && let Some(end_rel) = bytes[i + 1..].iter().position(|&b| b == b'}')
        {
            let inner = &template[i + 1..i + 1 + end_rel];
            render_placeholder(inner, params, &mut out);
            i += 1 + end_rel + 1;
            continue;
        }
        let Some(ch) = template[i..].chars().next() else {
            break;
        };
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn render_placeholder(inner: &str, params: &Value, out: &mut String) {
    if let Some((name, rest)) = inner.split_once('?')
        && let Some((then_branch, else_branch)) = rest.split_once(':')
    {
        let truthy = params.get(name).is_some_and(is_truthy);
        out.push_str(if truthy { then_branch } else { else_branch });
        return;
    }
    if let Some(v) = params.get(inner) {
        match v {
            Value::String(s) => out.push_str(s),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Number(n) => out.push_str(&n.to_string()),
            Value::Null => {}
            other => out.push_str(&other.to_string()),
        }
    }
}

fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn truncate_label(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    // Codepoint slice — avoids splitting a UTF-16 surrogate pair or a
    // multi-byte UTF-8 sequence at the cut. Won't preserve emoji ZWJ
    // cluster integrity (an emoji like 👨‍👩‍👧‍👦 may render as the leading
    // 👨 followed by an ellipsis); the alternative would be a
    // grapheme-aware crate dep, which is out of scope for an approval-
    // prompt label that's already best-effort. Mirrors the codepoint
    // slice in `channel-src/telegram/src/media/outbound.ts` which
    // truncates Telegram captions the same way.
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_meta_block() {
        let raw = json!({
            "aura": {
                "access_rule": { "kind": "always", "resource": "exec_command", "label_template": "x" },
                "call_label":  { "kind": "from_param", "param": "url", "max_chars": 80 }
            }
        });
        let map = raw.as_object().unwrap().clone();
        let meta = AuraToolMeta::from_meta(Some(&map));
        assert!(matches!(meta.access_rule, Some(AccessRule::Always { .. })));
        assert!(matches!(
            meta.call_label,
            Some(CallLabelRule::FromParam { .. })
        ));
    }

    #[test]
    fn missing_meta_yields_defaults() {
        let meta = AuraToolMeta::from_meta(None);
        assert!(meta.access_rule.is_none());
        assert!(meta.call_label.is_none());
    }

    #[test]
    fn malformed_meta_falls_back() {
        let raw = json!({ "aura": "not an object" });
        let map = raw.as_object().unwrap().clone();
        let meta = AuraToolMeta::from_meta(Some(&map));
        assert!(meta.access_rule.is_none());
        assert!(meta.call_label.is_none());
    }

    #[test]
    fn always_rule_renders_label_template() {
        let rule = AccessRule::Always {
            resource: ResourceKind::ExecCommand,
            label_template: "browser_screenshot: {full_page}".into(),
        };
        let p = json!({ "full_page": true });
        let r = rule.evaluate(&p);
        assert_eq!(r.len(), 1);
        match &r[0] {
            ResourceAccess::ExecCommand { command } => {
                assert_eq!(command, "browser_screenshot: true");
            }
            other => panic!("expected ExecCommand, got {other:?}"),
        }
    }

    #[test]
    fn if_param_present_skips_when_empty() {
        let rule = AccessRule::IfParamPresent {
            param: "expression".into(),
            resource: ResourceKind::ExecCommand,
            label_template: "browser_console: {expression}".into(),
        };
        assert!(rule.evaluate(&json!({})).is_empty());
        assert!(rule.evaluate(&json!({ "expression": "" })).is_empty());
        let r = rule.evaluate(&json!({ "expression": "1+1" }));
        assert_eq!(r.len(), 1);
        match &r[0] {
            ResourceAccess::ExecCommand { command } => {
                assert_eq!(command, "browser_console: 1+1");
            }
            other => panic!("expected ExecCommand, got {other:?}"),
        }
    }

    #[test]
    fn public_ip_rule_only_prompts_for_public_literal_ips() {
        let rule = AccessRule::PublicIpInUrlParam {
            param: "url".into(),
        };
        // hostname → no prompt
        assert!(
            rule.evaluate(&json!({ "url": "https://example.com" }))
                .is_empty()
        );
        // loopback (blocked) → no prompt
        assert!(
            rule.evaluate(&json!({ "url": "http://127.0.0.1/x" }))
                .is_empty()
        );
        // RFC1918 (blocked) → no prompt
        assert!(
            rule.evaluate(&json!({ "url": "http://10.0.0.1/" }))
                .is_empty()
        );
        // public IP → prompt
        let r = rule.evaluate(&json!({ "url": "http://8.8.8.8/" }));
        assert_eq!(r.len(), 1);
        match &r[0] {
            ResourceAccess::Http { host } => {
                assert_eq!(host, "8.8.8.8");
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[test]
    fn call_label_truncates() {
        let rule = CallLabelRule::FromParam {
            param: "expr".into(),
            max_chars: 5,
        };
        assert_eq!(rule.evaluate(&json!({})), None);
        assert_eq!(
            rule.evaluate(&json!({ "expr": "abcdefghi" })),
            Some("abcde…".to_string())
        );
        assert_eq!(
            rule.evaluate(&json!({ "expr": "abc" })),
            Some("abc".to_string())
        );
    }

    #[test]
    fn template_ternary_branches_on_truthiness() {
        let rule = AccessRule::Always {
            resource: ResourceKind::ExecCommand,
            label_template: "screenshot ({full_page?full page:viewport})".into(),
        };
        let r = rule.evaluate(&json!({ "full_page": true }));
        assert_eq!(r.len(), 1);
        match &r[0] {
            ResourceAccess::ExecCommand { command } => {
                assert_eq!(command, "screenshot (full page)");
            }
            other => panic!("expected ExecCommand, got {other:?}"),
        }
        let r = rule.evaluate(&json!({ "full_page": false }));
        match &r[0] {
            ResourceAccess::ExecCommand { command } => {
                assert_eq!(command, "screenshot (viewport)");
            }
            other => panic!("expected ExecCommand, got {other:?}"),
        }
        let r = rule.evaluate(&json!({}));
        match &r[0] {
            ResourceAccess::ExecCommand { command } => {
                assert_eq!(command, "screenshot (viewport)");
            }
            other => panic!("expected ExecCommand, got {other:?}"),
        }
    }

    #[test]
    fn call_label_template_renders() {
        let rule = CallLabelRule::Template {
            template: "{full_page?full page:viewport}".into(),
        };
        assert_eq!(
            rule.evaluate(&json!({ "full_page": true })),
            Some("full page".to_string())
        );
        assert_eq!(rule.evaluate(&json!({})), Some("viewport".to_string()));
    }

    #[test]
    fn call_label_template_empty_yields_none() {
        let rule = CallLabelRule::Template {
            template: "{missing}".into(),
        };
        assert_eq!(rule.evaluate(&json!({})), None);
    }

    #[test]
    fn template_unicode_is_byte_safe() {
        let rule = AccessRule::Always {
            resource: ResourceKind::ExecCommand,
            label_template: "你好 {x} 世界".into(),
        };
        let p = json!({ "x": "界" });
        let r = rule.evaluate(&p);
        assert_eq!(r.len(), 1);
        match &r[0] {
            ResourceAccess::ExecCommand { command } => assert_eq!(command, "你好 界 世界"),
            other => panic!("expected ExecCommand, got {other:?}"),
        }
    }
}
