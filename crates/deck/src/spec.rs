//! The per-card OpenAPI admission contract.
//!
//! Every call that crosses the gateway — user-initiated ops and dry-run
//! invocations — is validated against the card's own `openapi.json`
//! *before* anything reaches agent-written code. The document wrapper is
//! a deliberately constrained OpenAPI subset (op existence, `get`/`post`
//! only, mandatory `x-baybo-retryable`); the per-op parameter check is
//! **standard JSON Schema**, compiled once at install by the
//! `jsonschema` crate into a validator per op: each declared parameter's
//! `schema` is taken verbatim as a subschema inside
//! `{type: object, properties, required, additionalProperties: false}`,
//! so required-param presence, typing, enums, and unknown-param
//! rejection all fall out of the library. `$ref` (and `$dynamicRef`) are
//! rejected at admission — the crate's remote resolvers are compiled out
//! (`default-features = false`), and an agent-written schema must never
//! reference anything outside its own document. Off-schema requests die
//! at the gateway.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use serde_json::{Map, Value, json};

use crate::error::{DeckError, Result};

/// Byte cap on `openapi.json` itself.
pub const MAX_SPEC_BYTES: usize = 64 * 1024;
/// Ops per card.
pub const MAX_OPS: usize = 32;
/// Params per op.
pub const MAX_PARAMS_PER_OP: usize = 32;
/// Byte cap on a serialized op-call params object.
pub const MAX_CALL_PARAMS_BYTES: usize = 16 * 1024;
/// Validation errors quoted back to the caller per rejected call.
const MAX_REPORTED_ERRORS: usize = 3;

#[derive(Clone)]
struct OpSpec {
    validator: Arc<jsonschema::Validator>,
    /// The author's MANDATORY `x-baybo-retryable` declaration: running this
    /// op twice is harmless (pure read / recompute), so a transport that
    /// lost the response mid-flight may replay it. No default on purpose —
    /// every op author must decide replay-safety explicitly; an op with side
    /// effects must never be silently re-executed.
    retryable: bool,
}

/// Compiled admission contract for one card.
#[derive(Clone)]
pub struct CardSpec {
    ops: BTreeMap<String, OpSpec>,
    /// The raw document, re-served at
    /// `GET /v1/deck/services/{uuid}/openapi.json`.
    raw: Value,
}

impl fmt::Debug for CardSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CardSpec")
            .field("ops", &self.ops.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

fn op_name_ok(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    name.len() <= 64
        && first.is_ascii_lowercase()
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Reject `$ref` / `$dynamicRef` anywhere in an author-supplied schema.
/// The compiled-out resolvers would fail such a schema anyway, but with a
/// resolver error; this check is the admission contract stated plainly.
fn reject_refs(schema: &Value, ctx: &str) -> Result<()> {
    match schema {
        Value::Object(map) => {
            for (k, v) in map {
                if k == "$ref" || k == "$dynamicRef" {
                    return Err(DeckError::InvalidBundle(format!(
                        "{ctx}: `{k}` is not supported — inline the schema"
                    )));
                }
                reject_refs(v, ctx)?;
            }
            Ok(())
        }
        Value::Array(items) => {
            for v in items {
                reject_refs(v, ctx)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

impl CardSpec {
    /// Parse + compile `openapi.json`. Rejection messages are agent-facing.
    pub fn parse(raw_bytes: &[u8]) -> Result<Self> {
        if raw_bytes.len() > MAX_SPEC_BYTES {
            return Err(DeckError::InvalidBundle(format!(
                "openapi.json exceeds {MAX_SPEC_BYTES} bytes"
            )));
        }
        let raw: Value = serde_json::from_slice(raw_bytes)
            .map_err(|e| DeckError::InvalidBundle(format!("openapi.json is not JSON: {e}")))?;
        let paths = raw
            .get("paths")
            .and_then(Value::as_object)
            .ok_or_else(|| DeckError::InvalidBundle("openapi.json has no `paths` object".into()))?;
        if paths.len() > MAX_OPS {
            return Err(DeckError::InvalidBundle(format!(
                "openapi.json declares more than {MAX_OPS} ops"
            )));
        }

        let mut ops = BTreeMap::new();
        for (path, item) in paths {
            let op_name = path.trim_start_matches('/');
            if !op_name_ok(op_name) {
                return Err(DeckError::InvalidBundle(format!(
                    "op path `{path}` must be /<name> with name matching [a-z][a-z0-9_]*, max 64 chars"
                )));
            }
            let item = item.as_object().ok_or_else(|| {
                DeckError::InvalidBundle(format!("paths.{path} is not an object"))
            })?;
            // One method per op; `get` and `post` are accepted and
            // equivalent (the wire call is always POST .../{op}).
            let method_item = item
                .get("get")
                .or_else(|| item.get("post"))
                .ok_or_else(|| {
                    DeckError::InvalidBundle(format!(
                        "paths.{path} declares neither `get` nor `post`"
                    ))
                })?;

            let retryable = match method_item.get("x-baybo-retryable") {
                Some(Value::Bool(b)) => *b,
                Some(_) => {
                    return Err(DeckError::InvalidBundle(format!(
                        "paths.{path}: x-baybo-retryable must be a boolean"
                    )));
                }
                None => {
                    return Err(DeckError::InvalidBundle(format!(
                        "paths.{path}: x-baybo-retryable (boolean) is required — declare \
                         whether replaying this op is harmless (true only for pure \
                         reads/recomputes; false for anything with side effects)"
                    )));
                }
            };

            let mut properties = Map::new();
            let mut required = Vec::new();
            if let Some(list) = method_item.get("parameters") {
                let list = list.as_array().ok_or_else(|| {
                    DeckError::InvalidBundle(format!("paths.{path}.parameters is not an array"))
                })?;
                if list.len() > MAX_PARAMS_PER_OP {
                    return Err(DeckError::InvalidBundle(format!(
                        "paths.{path} declares more than {MAX_PARAMS_PER_OP} parameters"
                    )));
                }
                for p in list {
                    let name = p
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            DeckError::InvalidBundle(format!(
                                "paths.{path}: parameter without a `name`"
                            ))
                        })?
                        .to_string();
                    let schema = p.get("schema").cloned().ok_or_else(|| {
                        DeckError::InvalidBundle(format!(
                            "paths.{path}.parameters.{name}: `schema` is required"
                        ))
                    })?;
                    if !schema.is_object() {
                        return Err(DeckError::InvalidBundle(format!(
                            "paths.{path}.parameters.{name}: `schema` must be an object"
                        )));
                    }
                    reject_refs(&schema, &format!("paths.{path}.parameters.{name}"))?;
                    if p.get("required").and_then(Value::as_bool).unwrap_or(false) {
                        required.push(Value::String(name.clone()));
                    }
                    properties.insert(name, schema);
                }
            }
            let op_schema = json!({
                "type": "object",
                "properties": properties,
                "required": required,
                "additionalProperties": false,
            });
            let validator = jsonschema::validator_for(&op_schema).map_err(|e| {
                DeckError::InvalidBundle(format!(
                    "paths.{path}: parameter schema does not compile: {e}"
                ))
            })?;
            ops.insert(
                op_name.to_string(),
                OpSpec {
                    validator: Arc::new(validator),
                    retryable,
                },
            );
        }
        if ops.is_empty() {
            return Err(DeckError::InvalidBundle(
                "openapi.json declares no ops".into(),
            ));
        }
        Ok(Self { ops, raw })
    }

    pub fn raw(&self) -> &Value {
        &self.raw
    }

    pub fn has_op(&self, op: &str) -> bool {
        self.ops.contains_key(op)
    }

    /// Ops the author declared replay-safe (`x-baybo-retryable: true`).
    /// Served to clients on the deck view so the phone's relay transport
    /// can pick its replay policy per call.
    pub fn retryable_ops(&self) -> Vec<String> {
        self.ops
            .iter()
            .filter(|(_, spec)| spec.retryable)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Validate one gateway-crossing call. `params` must be a JSON object
    /// (or null, treated as `{}`).
    pub fn validate_call(&self, op: &str, params: &Value) -> Result<()> {
        let spec = self
            .ops
            .get(op)
            .ok_or_else(|| DeckError::OpRejected(format!("unknown op `{op}`")))?;
        let empty;
        let instance: &Value = match params {
            Value::Null => {
                empty = Value::Object(Map::new());
                &empty
            }
            Value::Object(_) => params,
            other => {
                return Err(DeckError::OpRejected(format!(
                    "params must be an object, got {}",
                    json_type_name(other)
                )));
            }
        };
        if serde_json::to_vec(params).map(|b| b.len()).unwrap_or(0) > MAX_CALL_PARAMS_BYTES {
            return Err(DeckError::OpRejected(format!(
                "params exceed {MAX_CALL_PARAMS_BYTES} bytes"
            )));
        }
        let errors: Vec<String> = spec
            .validator
            .iter_errors(instance)
            .take(MAX_REPORTED_ERRORS)
            .map(|e| {
                let at = e.instance_path().to_string();
                if at.is_empty() {
                    e.to_string()
                } else {
                    format!("{at}: {e}")
                }
            })
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(DeckError::OpRejected(format!(
                "op `{op}`: {}",
                errors.join("; ")
            )))
        }
    }
}

fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn quota_spec() -> CardSpec {
        CardSpec::parse(
            json!({
                "openapi": "3.1.0",
                "paths": {
                    "/quota": {
                        "get": {
                            "x-baybo-retryable": true,
                            "parameters": [
                                {"name": "provider", "required": true,
                                 "schema": {"type": "string", "enum": ["anthropic", "openai"]}},
                                {"name": "days", "schema": {"type": "integer"}}
                            ]
                        }
                    },
                    "/history": {"post": {"x-baybo-retryable": false}}
                }
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap()
    }

    #[test]
    fn accepts_well_formed_calls() {
        let spec = quota_spec();
        spec.validate_call("quota", &json!({"provider": "anthropic", "days": 7}))
            .unwrap();
        spec.validate_call("history", &Value::Null).unwrap();
    }

    #[test]
    fn rejects_off_schema_calls() {
        let spec = quota_spec();
        // Unknown op.
        assert!(matches!(
            spec.validate_call("ghost", &json!({})),
            Err(DeckError::OpRejected(_))
        ));
        // Missing required.
        assert!(spec.validate_call("quota", &json!({})).is_err());
        // Unknown param.
        assert!(
            spec.validate_call("quota", &json!({"provider": "anthropic", "x": 1}))
                .is_err()
        );
        // Wrong type.
        assert!(
            spec.validate_call("quota", &json!({"provider": 3}))
                .is_err()
        );
        // Enum violation.
        assert!(
            spec.validate_call("quota", &json!({"provider": "google"}))
                .is_err()
        );
        // Non-object params.
        assert!(spec.validate_call("quota", &json!([1, 2])).is_err());
    }

    #[test]
    fn rejects_malformed_specs() {
        assert!(CardSpec::parse(b"not json").is_err());
        assert!(CardSpec::parse(b"{}").is_err());
        assert!(
            CardSpec::parse(json!({"paths": {}}).to_string().as_bytes()).is_err(),
            "no ops"
        );
        assert!(
            CardSpec::parse(
                json!({"paths": {"/Bad-Name": {"get": {"x-baybo-retryable": true}}}})
                    .to_string()
                    .as_bytes()
            )
            .is_err()
        );
        assert!(
            CardSpec::parse(
                json!({"paths": {"/op": {"put": {"x-baybo-retryable": true}}}})
                    .to_string()
                    .as_bytes()
            )
            .is_err(),
            "only get/post"
        );
    }

    #[test]
    fn retryable_declaration_is_mandatory_and_surfaced() {
        // Missing declaration refuses the whole spec — the author must
        // decide replay-safety per op, there is no default.
        let err = CardSpec::parse(
            json!({"paths": {"/op": {"get": {}}}})
                .to_string()
                .as_bytes(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("x-baybo-retryable"), "{err}");
        // Non-boolean values refuse too.
        assert!(
            CardSpec::parse(
                json!({"paths": {"/op": {"get": {"x-baybo-retryable": "yes"}}}})
                    .to_string()
                    .as_bytes()
            )
            .is_err()
        );
        // Declarations surface for the deck view.
        assert_eq!(quota_spec().retryable_ops(), vec!["quota".to_string()]);
    }

    /// Parameter schemas are full JSON Schema now — a typed array param
    /// validates for free, something the old hand-rolled checker refused.
    #[test]
    fn rich_parameter_schemas_validate_as_json_schema() {
        let spec = CardSpec::parse(
            json!({"paths": {"/watch": {"get": {
                "x-baybo-retryable": true,
                "parameters": [
                    {"name": "hosts", "required": true,
                     "schema": {"type": "array", "items": {"type": "string"}, "maxItems": 4}},
                    {"name": "window", "schema": {"type": "integer", "minimum": 1}},
                    {"name": "thresholds", "schema": {
                        "type": "object",
                        "properties": {"warn": {"type": "number"}, "crit": {"type": "number"}},
                        "additionalProperties": false
                    }}
                ]
            }}}})
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        spec.validate_call(
            "watch",
            &json!({"hosts": ["a.example", "b.example"], "thresholds": {"warn": 0.8}}),
        )
        .unwrap();
        assert!(
            spec.validate_call("watch", &json!({"hosts": "a.example"}))
                .is_err(),
            "scalar where array declared"
        );
        assert!(
            spec.validate_call("watch", &json!({"hosts": [], "window": 0}))
                .is_err(),
            "minimum enforced"
        );
        assert!(
            spec.validate_call(
                "watch",
                &json!({"hosts": [], "thresholds": {"warn": "hot"}})
            )
            .is_err(),
            "nested map property typing enforced"
        );
    }

    /// `$ref` is an admission error with a plain message, not a resolver
    /// failure — the compiled-out remote resolvers must never be reachable
    /// by an agent-written schema.
    #[test]
    fn ref_is_rejected_at_admission() {
        let err = CardSpec::parse(
            json!({"paths": {"/op": {"get": {
                "x-baybo-retryable": true,
                "parameters": [
                    {"name": "p", "schema": {"$ref": "https://evil.example/schema.json"}}
                ]
            }}}})
            .to_string()
            .as_bytes(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("$ref"), "{err}");
        // Nested inside a combinator too.
        assert!(
            CardSpec::parse(
                json!({"paths": {"/op": {"get": {
                    "x-baybo-retryable": true,
                    "parameters": [
                        {"name": "p", "schema": {"anyOf": [{"$ref": "#/x"}]}}
                    ]
                }}}})
                .to_string()
                .as_bytes()
            )
            .is_err()
        );
    }

    /// A parameter must carry an object `schema` — the wrapper still
    /// refuses shapeless declarations before compilation.
    #[test]
    fn parameter_schema_is_mandatory() {
        assert!(
            CardSpec::parse(
                json!({"paths": {"/op": {"get": {
                    "x-baybo-retryable": true,
                    "parameters": [{"name": "p"}]
                }}}})
                .to_string()
                .as_bytes()
            )
            .is_err()
        );
        assert!(
            CardSpec::parse(
                json!({"paths": {"/op": {"get": {
                    "x-baybo-retryable": true,
                    "parameters": [{"name": "p", "schema": "string"}]
                }}}})
                .to_string()
                .as_bytes()
            )
            .is_err()
        );
    }
}
