//! The `AskUser` tool: asks the human 1–4 structured multiple-choice questions
//! and returns their structured answer as the tool result.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use baybo_model::human_ask::{
    ASK_USER_TOOL_NAME, MAX_HEADER_CHARS, MAX_OPTIONS, MAX_QUESTIONS, MIN_OPTIONS, Question,
    QuestionOption, QuestionRequest,
};
use baybo_model::{ChannelType, TrustLevel};
use baybo_tools::{
    QuestionOutcome, Tool, ToolConcurrency, ToolContext, ToolManifest, ToolOutput, ToolTriggerScope,
};
use serde_json::{Value, json};

const ASK_USER_DESCRIPTION: &str = r#"Ask the user 1 to 4 structured multiple-choice clarifying questions.

Each question renders as a tab in the UI with 2 to 4 predefined options plus a fixed free-text "Other" input (always provided by the UI — do not add an "Other" option yourself).

Rules:
- Use this only when a genuine ambiguity or user preference must be settled before acting; do not ask trivial yes/no questions or anything you can determine by reading files.
- `AskUser` MUST be the only tool call in your response. Put all questions into one call's `questions` array and wait for the user's answer before calling any other tool.
- A {"cancelled": true} result means the user cancelled this request. Leave its questions unanswered and do not repeat the request."#;

/// Tool that raises a durable question ask through [`ToolContext::question`].
pub struct AskUserTool;

/// Build the `AskUser` tool and its owner-only manifest for runtime registration.
pub fn agent_tools() -> Vec<(Arc<dyn Tool>, ToolManifest)> {
    let tool = AskUserTool;
    let manifest = ToolManifest {
        name: tool.name().to_string(),
        description: tool.description(),
        trust_level: TrustLevel::Trusted,
        parameters_schema: tool.parameters_schema(),
        capabilities: Vec::new(),
        channels: vec![ChannelType::owner()],
    };
    vec![(Arc::new(tool), manifest)]
}

/// Validate and normalize raw `AskUser` parameters into a [`QuestionRequest`].
pub(crate) fn validate_question_request(
    params: Value,
) -> std::result::Result<QuestionRequest, String> {
    let raw: QuestionRequest =
        serde_json::from_value(params).map_err(|e| format!("invalid AskUser parameters: {e}"))?;

    if raw.questions.is_empty() || raw.questions.len() > MAX_QUESTIONS {
        return Err(format!(
            "questions must contain between 1 and {MAX_QUESTIONS} items (got {})",
            raw.questions.len()
        ));
    }

    let mut seen_headers: HashSet<String> = HashSet::with_capacity(raw.questions.len());
    let mut questions = Vec::with_capacity(raw.questions.len());

    for (idx, q) in raw.questions.into_iter().enumerate() {
        let header = q.header.trim().to_owned();
        if header.is_empty() {
            return Err(format!("questions[{idx}].header must not be empty"));
        }
        let header_chars = header.chars().count();
        if header_chars > MAX_HEADER_CHARS {
            return Err(format!(
                "questions[{idx}].header exceeds {MAX_HEADER_CHARS} characters (got {header_chars})"
            ));
        }
        if !seen_headers.insert(header.to_lowercase()) {
            return Err(format!("duplicate question header {header:?}"));
        }

        let question = q.question.trim().to_owned();
        if question.is_empty() {
            return Err(format!("questions[{idx}].question must not be empty"));
        }

        if q.options.len() < MIN_OPTIONS || q.options.len() > MAX_OPTIONS {
            return Err(format!(
                "questions[{idx}].options must contain between {MIN_OPTIONS} and {MAX_OPTIONS} items (got {})",
                q.options.len()
            ));
        }

        let mut seen_labels: HashSet<String> = HashSet::with_capacity(q.options.len());
        let mut options = Vec::with_capacity(q.options.len());
        for (opt_idx, opt) in q.options.into_iter().enumerate() {
            let label = opt.label.trim().to_owned();
            if label.is_empty() {
                return Err(format!(
                    "questions[{idx}].options[{opt_idx}].label must not be empty"
                ));
            }
            if !seen_labels.insert(label.to_lowercase()) {
                return Err(format!(
                    "questions[{idx}] has duplicate option label {label:?}"
                ));
            }
            let description = opt.description.trim().to_owned();
            if description.is_empty() {
                return Err(format!(
                    "questions[{idx}].options[{opt_idx}].description must not be empty"
                ));
            }
            options.push(QuestionOption { label, description });
        }

        questions.push(Question {
            header,
            question,
            options,
            multi_select: q.multi_select,
        });
    }

    Ok(QuestionRequest { questions })
}

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        ASK_USER_TOOL_NAME
    }

    fn description(&self) -> String {
        ASK_USER_DESCRIPTION.to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["questions"],
            "additionalProperties": false,
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_QUESTIONS,
                    "description": "1 to 4 questions, shown as tabs.",
                    "items": {
                        "type": "object",
                        "required": ["header", "question", "options"],
                        "additionalProperties": false,
                        "properties": {
                            "header": {
                                "type": "string",
                                "maxLength": MAX_HEADER_CHARS,
                                "description": "Short unique tab label (at most 12 characters)."
                            },
                            "question": {
                                "type": "string",
                                "description": "The question shown to the user."
                            },
                            "options": {
                                "type": "array",
                                "minItems": MIN_OPTIONS,
                                "maxItems": MAX_OPTIONS,
                                "description": "2 to 4 choices. Do not include an Other option; the UI always offers one.",
                                "items": {
                                    "type": "object",
                                    "required": ["label", "description"],
                                    "additionalProperties": false,
                                    "properties": {
                                        "label": {
                                            "type": "string",
                                            "description": "Short choice label."
                                        },
                                        "description": {
                                            "type": "string",
                                            "description": "Brief explanation or trade-off of this choice."
                                        }
                                    }
                                }
                            },
                            "multi_select": {
                                "type": "boolean",
                                "default": false,
                                "description": "Allow picking more than one option."
                            }
                        }
                    }
                }
            }
        })
    }

    fn progress_label(&self, params: &Value) -> Option<String> {
        crate::summary::first_question_header(params)
    }

    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Exclusive
    }

    fn trigger_scope(&self) -> ToolTriggerScope {
        ToolTriggerScope::UserConversation
    }

    fn sole_call(&self) -> bool {
        true
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        let request = match validate_question_request(params) {
            Ok(req) => req,
            Err(msg) => return Ok(ToolOutput::Error(msg)),
        };

        let Some(handle) = &ctx.question else {
            return Ok(ToolOutput::Json(crate::result::unattended()));
        };

        let outcome = handle.ask(request).await;
        let payload = match outcome {
            QuestionOutcome::Answered(answers) => crate::result::answered(&answers),
            QuestionOutcome::Declined => crate::result::declined(),
            QuestionOutcome::Cancelled => crate::result::cancelled(),
            QuestionOutcome::Abandoned => crate::result::abandoned(),
        };
        Ok(ToolOutput::Json(payload))
    }
}

#[cfg(test)]
mod tests {
    use baybo_model::human_ask::{QuestionAnswer, QuestionOutcomeKind};
    use baybo_tools::{
        AskCancel, AskSite, ParkGauge, QuestionAsk, QuestionGate, QuestionHandle,
        QuestionHandleConfig,
    };

    use super::*;

    struct FixedGate(QuestionOutcome);

    #[async_trait]
    impl QuestionGate for FixedGate {
        async fn ask(
            &self,
            _ask: QuestionAsk,
            _site: AskSite,
            _cancel: AskCancel,
        ) -> QuestionOutcome {
            self.0.clone()
        }
    }

    fn valid_params() -> Value {
        json!({
            "questions": [
                {
                    "header": "Auth",
                    "question": "Which auth scheme?",
                    "options": [
                        { "label": "JWT", "description": "stateless" },
                        { "label": "Session", "description": "server-side" }
                    ],
                    "multi_select": false
                }
            ]
        })
    }

    fn ctx_with_gate(outcome: QuestionOutcome) -> (ToolContext, QuestionHandle) {
        let handle = QuestionHandle::from_config(QuestionHandleConfig {
            gate: Arc::new(FixedGate(outcome)),
            tool_call_id: "call-1".into(),
            session_id: "s1".into(),
            channel: ChannelType::owner(),
            turn_id: baybo_model::TurnId::default(),
            park: ParkGauge::new(),
            cancel: AskCancel::never(),
        });
        let ctx = ToolContext {
            question: Some(handle.clone()),
            ..ToolContext::for_test()
        };
        (ctx, handle)
    }

    #[test]
    fn validation_table_rejects_bad_inputs() {
        let cases: Vec<(&str, Value)> = vec![
            ("empty questions", json!({ "questions": [] })),
            (
                "too many questions",
                json!({
                    "questions": (0..=MAX_QUESTIONS).map(|i| json!({
                        "header": format!("Q{i}"),
                        "question": "?",
                        "options": [
                            { "label": "A", "description": "a" },
                            { "label": "B", "description": "b" }
                        ]
                    })).collect::<Vec<_>>()
                }),
            ),
            (
                "empty header",
                json!({
                    "questions": [{
                        "header": "   ",
                        "question": "?",
                        "options": [
                            { "label": "A", "description": "a" },
                            { "label": "B", "description": "b" }
                        ]
                    }]
                }),
            ),
            (
                "header too long",
                json!({
                    "questions": [{
                        "header": "1234567890123",
                        "question": "?",
                        "options": [
                            { "label": "A", "description": "a" },
                            { "label": "B", "description": "b" }
                        ]
                    }]
                }),
            ),
            (
                "duplicate header",
                json!({
                    "questions": [
                        {
                            "header": "Auth",
                            "question": "q1",
                            "options": [
                                { "label": "A", "description": "a" },
                                { "label": "B", "description": "b" }
                            ]
                        },
                        {
                            "header": "auth",
                            "question": "q2",
                            "options": [
                                { "label": "C", "description": "c" },
                                { "label": "D", "description": "d" }
                            ]
                        }
                    ]
                }),
            ),
            (
                "empty question",
                json!({
                    "questions": [{
                        "header": "Auth",
                        "question": "",
                        "options": [
                            { "label": "A", "description": "a" },
                            { "label": "B", "description": "b" }
                        ]
                    }]
                }),
            ),
            (
                "too few options",
                json!({
                    "questions": [{
                        "header": "Auth",
                        "question": "?",
                        "options": [{ "label": "A", "description": "a" }]
                    }]
                }),
            ),
            (
                "too many options",
                json!({
                    "questions": [{
                        "header": "Auth",
                        "question": "?",
                        "options": (0..=MAX_OPTIONS).map(|i| json!({
                            "label": format!("L{i}"),
                            "description": "d"
                        })).collect::<Vec<_>>()
                    }]
                }),
            ),
            (
                "empty option label",
                json!({
                    "questions": [{
                        "header": "Auth",
                        "question": "?",
                        "options": [
                            { "label": "", "description": "a" },
                            { "label": "B", "description": "b" }
                        ]
                    }]
                }),
            ),
            (
                "duplicate option label",
                json!({
                    "questions": [{
                        "header": "Auth",
                        "question": "?",
                        "options": [
                            { "label": "JWT", "description": "a" },
                            { "label": "jwt", "description": "b" }
                        ]
                    }]
                }),
            ),
            (
                "empty option description",
                json!({
                    "questions": [{
                        "header": "Auth",
                        "question": "?",
                        "options": [
                            { "label": "A", "description": " " },
                            { "label": "B", "description": "b" }
                        ]
                    }]
                }),
            ),
        ];

        for (label, params) in cases {
            assert!(
                validate_question_request(params).is_err(),
                "expected validation failure for {label}"
            );
        }
    }

    #[tokio::test]
    async fn validation_failure_returns_tool_error_without_asking() {
        let (ctx, handle) = ctx_with_gate(QuestionOutcome::Declined);
        let out = AskUserTool
            .execute(json!({ "questions": [] }), &ctx)
            .await
            .expect("ok");
        assert!(matches!(out, ToolOutput::Error(_)));
        assert_eq!(handle.last_outcome(), None);
    }

    #[tokio::test]
    async fn missing_question_handle_returns_declined_unattended() {
        let ctx = ToolContext::for_test();
        assert!(ctx.question.is_none());
        let out = AskUserTool.execute(valid_params(), &ctx).await.expect("ok");
        let ToolOutput::Json(val) = out else {
            panic!("expected Json, got {out:?}");
        };
        assert_eq!(val["declined"], true);
        assert!(
            val["note"]
                .as_str()
                .is_some_and(|n| n.contains("unattended"))
        );
    }

    #[tokio::test]
    async fn answered_declined_and_abandoned_map_to_result_envelopes() {
        let (ctx, handle) = ctx_with_gate(QuestionOutcome::Answered(vec![QuestionAnswer {
            header: "Auth".into(),
            selected: vec!["JWT".into()],
            other: None,
        }]));
        let out = AskUserTool.execute(valid_params(), &ctx).await.expect("ok");
        let ToolOutput::Json(val) = out else {
            panic!("expected Json");
        };
        assert_eq!(val["answers"][0]["header"], "Auth");
        assert_eq!(val["answers"][0]["selected"][0], "JWT");
        assert_eq!(handle.last_outcome(), Some(QuestionOutcomeKind::Answered));

        let (ctx, handle) = ctx_with_gate(QuestionOutcome::Declined);
        let out = AskUserTool.execute(valid_params(), &ctx).await.expect("ok");
        assert!(
            matches!(out, ToolOutput::Json(v) if v["declined"] == true && v.get("note").is_some())
        );
        assert_eq!(handle.last_outcome(), Some(QuestionOutcomeKind::Declined));

        let (ctx, handle) = ctx_with_gate(QuestionOutcome::Abandoned);
        let out = AskUserTool.execute(valid_params(), &ctx).await.expect("ok");
        assert!(
            matches!(out, ToolOutput::Json(v) if v["declined"] == true && v.get("reason").is_some())
        );
        assert_eq!(handle.last_outcome(), Some(QuestionOutcomeKind::Abandoned));
    }

    #[test]
    fn manifest_is_owner_only_user_conversation_sole_call() {
        let tools = agent_tools();
        assert_eq!(tools.len(), 1);
        let (tool, manifest) = &tools[0];
        assert_eq!(tool.name(), ASK_USER_TOOL_NAME);
        assert!(tool.sole_call());
        assert_eq!(tool.concurrency(), ToolConcurrency::Exclusive);
        assert_eq!(tool.trigger_scope(), ToolTriggerScope::UserConversation);
        assert_eq!(manifest.channels, vec![ChannelType::owner()]);
        assert_eq!(
            tool.progress_label(&valid_params()).as_deref(),
            Some("Auth")
        );
    }
}
