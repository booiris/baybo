use crate::ToolDefinitionForLlm;
use serde_json::Value;

/// Converts Aura tool definitions into the OpenAI-style function calling format.
pub fn to_openai_tools(tools: &[ToolDefinitionForLlm]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters_schema,
                }
            })
        })
        .collect()
}

/// Converts Aura tool definitions into the Anthropic tool use format.
pub fn to_anthropic_tools(tools: &[ToolDefinitionForLlm]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.parameters_schema,
            })
        })
        .collect()
}

/// Builds a prompt string describing available tools for prompt-guided mode.
///
/// The resulting prompt instructs the model to emit tool calls inside
/// `<tool_call>` tags so that `JsonExtractor` can parse them.
pub fn build_tool_schema_prompt(tools: &[ToolDefinitionForLlm]) -> String {
    if tools.is_empty() {
        return String::new();
    }

    let mut prompt = String::from(
        "You have access to the following tools. To call a tool, respond with a \
         <tool_call> tag containing a JSON object with \"name\" and \"arguments\" fields.\n\n\
         Available tools:\n",
    );

    for tool in tools {
        prompt.push_str(&format!("\n### {}\n", tool.name));
        prompt.push_str(&format!("Description: {}\n", tool.description));
        prompt.push_str(&format!(
            "Parameters: {}\n",
            serde_json::to_string_pretty(&tool.parameters_schema).unwrap_or_default()
        ));
    }

    prompt.push_str(
        "\nExample usage:\n\
         <tool_call>\n\
         {\"name\": \"tool_name\", \"arguments\": {\"param\": \"value\"}}\n\
         </tool_call>\n",
    );

    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tools() -> Vec<ToolDefinitionForLlm> {
        vec![
            ToolDefinitionForLlm {
                name: "get_weather".to_string(),
                description: "Get current weather for a location".to_string(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "location": {"type": "string"}
                    },
                    "required": ["location"]
                }),
            },
            ToolDefinitionForLlm {
                name: "search".to_string(),
                description: "Search the web".to_string(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"}
                    }
                }),
            },
        ]
    }

    #[test]
    fn test_to_openai_tools() {
        let tools = sample_tools();
        let result = to_openai_tools(&tools);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0]["type"], "function");
        assert_eq!(result[0]["function"]["name"], "get_weather");
        assert_eq!(
            result[0]["function"]["description"],
            "Get current weather for a location"
        );
        assert!(result[0]["function"]["parameters"]["properties"]["location"].is_object());
    }

    #[test]
    fn test_to_anthropic_tools() {
        let tools = sample_tools();
        let result = to_anthropic_tools(&tools);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0]["name"], "get_weather");
        assert_eq!(
            result[0]["description"],
            "Get current weather for a location"
        );
        assert!(result[0]["input_schema"]["properties"]["location"].is_object());
        // Anthropic format should not have "type": "function" wrapper
        assert!(result[0].get("type").is_none());
    }

    #[test]
    fn test_to_openai_tools_empty() {
        let result = to_openai_tools(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_to_anthropic_tools_empty() {
        let result = to_anthropic_tools(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_build_tool_schema_prompt_empty() {
        let result = build_tool_schema_prompt(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_build_tool_schema_prompt_contains_tool_info() {
        let tools = sample_tools();
        let prompt = build_tool_schema_prompt(&tools);

        assert!(prompt.contains("get_weather"));
        assert!(prompt.contains("Get current weather for a location"));
        assert!(prompt.contains("search"));
        assert!(prompt.contains("Search the web"));
        assert!(prompt.contains("<tool_call>"));
        assert!(prompt.contains("</tool_call>"));
    }

    #[test]
    fn test_build_tool_schema_prompt_includes_parameters() {
        let tools = vec![ToolDefinitionForLlm {
            name: "test_tool".to_string(),
            description: "A test".to_string(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "foo": {"type": "string"}
                }
            }),
        }];
        let prompt = build_tool_schema_prompt(&tools);
        assert!(prompt.contains("foo"));
        assert!(prompt.contains("Parameters:"));
    }
}
