use crate::ToolCallInfo;
use regex::Regex;
use serde_json::Value;

/// Extracts tool call JSON from free-text model responses.
/// Supports tag-based extraction and JSON block detection.
pub struct JsonExtractor;

impl JsonExtractor {
    pub fn new() -> Self {
        Self
    }

    /// Attempt to extract tool calls from the given text.
    /// Looks for `<tool_call>...</tool_call>` tags first, then falls back
    /// to JSON code blocks.
    pub fn extract(&self, text: &str) -> Vec<ToolCallInfo> {
        let calls = self.extract_tagged(text);
        if !calls.is_empty() {
            return calls;
        }
        self.extract_json_blocks(text)
    }

    /// Extract content between `<tool_call>` and `</tool_call>` tags,
    /// parsing each as a JSON object with `name` and `arguments` fields.
    fn extract_tagged(&self, text: &str) -> Vec<ToolCallInfo> {
        let re = match Regex::new(r"(?s)<tool_call>\s*(.*?)\s*</tool_call>") {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };

        let mut calls = Vec::new();
        for (idx, cap) in re.captures_iter(text).enumerate() {
            let json_str = &cap[1];
            if let Some(call) = parse_tool_call_json(json_str, idx) {
                calls.push(call);
            }
        }
        calls
    }

    /// Look for ````json ... ``` `` blocks or bare JSON objects that match
    /// the tool call structure (`name` + `arguments` fields).
    fn extract_json_blocks(&self, text: &str) -> Vec<ToolCallInfo> {
        let re = match Regex::new(r"(?s)```(?:json)?\s*(\{.*?\})\s*```") {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };

        let mut calls = Vec::new();
        for (idx, cap) in re.captures_iter(text).enumerate() {
            let json_str = &cap[1];
            if let Some(call) = parse_tool_call_json(json_str, idx) {
                calls.push(call);
            }
        }
        calls
    }
}

impl Default for JsonExtractor {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse a JSON string into a `ToolCallInfo`, expecting `name` and `arguments` fields.
fn parse_tool_call_json(json_str: &str, index: usize) -> Option<ToolCallInfo> {
    let val: Value = serde_json::from_str(json_str).ok()?;
    let obj = val.as_object()?;
    let name = obj.get("name")?.as_str()?.to_string();
    let arguments = obj
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    Some(ToolCallInfo {
        id: format!("extracted-{index}"),
        name,
        arguments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_tagged_extraction() {
        let text = r#"Some text before
<tool_call>
{"name": "get_weather", "arguments": {"location": "Tokyo"}}
</tool_call>
Some text after"#;

        let extractor = JsonExtractor::new();
        let calls = extractor.extract(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "get_weather");
        assert_eq!(calls[0].arguments["location"], "Tokyo");
    }

    #[test]
    fn test_multiple_tagged_extractions() {
        let text = r#"
<tool_call>
{"name": "tool_a", "arguments": {"x": 1}}
</tool_call>
middle text
<tool_call>
{"name": "tool_b", "arguments": {"y": 2}}
</tool_call>
"#;

        let extractor = JsonExtractor::new();
        let calls = extractor.extract(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "tool_a");
        assert_eq!(calls[1].name, "tool_b");
    }

    #[test]
    fn test_json_code_block_extraction() {
        let text = r#"Here is the tool call:

```json
{"name": "search", "arguments": {"query": "rust lang"}}
```
"#;

        let extractor = JsonExtractor::new();
        let calls = extractor.extract(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "search");
        assert_eq!(calls[0].arguments["query"], "rust lang");
    }

    #[test]
    fn test_code_block_without_json_tag() {
        let text = r#"
```
{"name": "calc", "arguments": {"expr": "2+2"}}
```
"#;

        let extractor = JsonExtractor::new();
        let calls = extractor.extract(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "calc");
    }

    #[test]
    fn test_no_tool_calls_returns_empty() {
        let text = "This is just a normal response with no tool calls.";
        let extractor = JsonExtractor::new();
        let calls = extractor.extract(text);
        assert!(calls.is_empty());
    }

    #[test]
    fn test_malformed_json_returns_empty() {
        let text = r#"
<tool_call>
{not valid json at all}
</tool_call>
"#;

        let extractor = JsonExtractor::new();
        let calls = extractor.extract(text);
        assert!(calls.is_empty());
    }

    #[test]
    fn test_missing_name_field_returns_empty() {
        let text = r#"
<tool_call>
{"arguments": {"x": 1}}
</tool_call>
"#;

        let extractor = JsonExtractor::new();
        let calls = extractor.extract(text);
        assert!(calls.is_empty());
    }

    #[test]
    fn test_missing_arguments_defaults_to_empty_object() {
        let text = r#"
<tool_call>
{"name": "no_args_tool"}
</tool_call>
"#;

        let extractor = JsonExtractor::new();
        let calls = extractor.extract(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "no_args_tool");
        assert!(calls[0].arguments.is_object());
    }

    #[test]
    fn test_tagged_takes_priority_over_code_blocks() {
        let text = r#"
<tool_call>
{"name": "tagged_tool", "arguments": {}}
</tool_call>

```json
{"name": "block_tool", "arguments": {}}
```
"#;

        let extractor = JsonExtractor::new();
        let calls = extractor.extract(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "tagged_tool");
    }

    #[test]
    fn test_extracted_ids_are_sequential() {
        let text = r#"
<tool_call>{"name": "a", "arguments": {}}</tool_call>
<tool_call>{"name": "b", "arguments": {}}</tool_call>
"#;

        let extractor = JsonExtractor::new();
        let calls = extractor.extract(text);
        assert_eq!(calls[0].id, "extracted-0");
        assert_eq!(calls[1].id, "extracted-1");
    }
}
