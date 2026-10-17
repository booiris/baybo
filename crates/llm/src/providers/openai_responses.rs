use futures::StreamExt;
use rig::completion::{AssistantContent, CompletionError, CompletionRequest, Message};
use rig::message::UserContent;
use rig::streaming::StreamedAssistantContent;
use serde::{Deserialize, Serialize};

use crate::{LlmStream, ProviderCallExtras};

const CALL_ID_PREFIX: &str = "responses:";

// The transcript has one opaque tool id; Responses needs both the output
// item id and the call id on the next turn. Keep the pair in that handle.
#[derive(Serialize, Deserialize)]
struct CallId {
    item_id: String,
    call_id: String,
}

fn retain_call_id(call: &mut rig::message::ToolCall) -> Result<(), CompletionError> {
    if let Some(call_id) = call.call_id.take() {
        let ids = CallId {
            item_id: call.id.clone(),
            call_id,
        };
        call.id = format!("{CALL_ID_PREFIX}{}", serde_json::to_string(&ids)?);
    }
    Ok(())
}

fn decode_call_id(id: &str) -> Result<CallId, CompletionError> {
    match id.strip_prefix(CALL_ID_PREFIX) {
        Some(encoded) => Ok(serde_json::from_str(encoded)?),
        None => Ok(CallId {
            item_id: id.to_string(),
            call_id: id.to_string(),
        }),
    }
}

fn prepare_request(
    mut request: CompletionRequest,
    extras: ProviderCallExtras<'_>,
) -> Result<CompletionRequest, CompletionError> {
    request.temperature = None;
    for message in request.chat_history.iter_mut() {
        match message {
            Message::Assistant { content, .. } => {
                for part in content.iter_mut() {
                    if let AssistantContent::ToolCall(call) = part {
                        let ids = decode_call_id(&call.id)?;
                        call.id = ids.item_id;
                        call.call_id = Some(ids.call_id);
                    }
                }
            }
            Message::User { content } => {
                for part in content.iter_mut() {
                    if let UserContent::ToolResult(result) = part {
                        result.call_id = Some(decode_call_id(&result.id)?.call_id);
                    }
                }
            }
            _ => {}
        }
    }
    let params = request
        .additional_params
        .get_or_insert_with(|| serde_json::json!({}));
    params["store"] = false.into();
    params["include"] = serde_json::json!(["reasoning.encrypted_content"]);
    if let Some(key) = extras.prompt_cache_key {
        params["prompt_cache_key"] = key.into();
    }
    Ok(request)
}

pub(crate) async fn completion(
    model: &rig::providers::openai::responses_api::ResponsesCompletionModel,
    request: CompletionRequest,
    extras: ProviderCallExtras<'_>,
) -> Result<rig::completion::CompletionResponse<()>, CompletionError> {
    use rig::completion::CompletionModel;
    let mut response =
        crate::repack_completion(model.completion(prepare_request(request, extras)?).await?);
    for part in response.choice.iter_mut() {
        if let AssistantContent::ToolCall(call) = part {
            retain_call_id(call)?;
        }
    }
    Ok(response)
}

pub(crate) async fn stream(
    model: &rig::providers::openai::responses_api::ResponsesCompletionModel,
    request: CompletionRequest,
    extras: ProviderCallExtras<'_>,
) -> Result<LlmStream, CompletionError> {
    use rig::completion::CompletionModel;
    let stream = model.stream(prepare_request(request, extras)?).await?;
    Ok(LlmStream::from_stream(stream.filter_map(|result| {
        futures::future::ready(match result {
            Err(error) => Some(Err(crate::rig_completion_to_error(error))),
            Ok(mut event) => {
                if let StreamedAssistantContent::ToolCall { tool_call, .. } = &mut event
                    && let Err(error) = retain_call_id(tool_call)
                {
                    return futures::future::ready(Some(Err(crate::rig_completion_to_error(
                        error,
                    ))));
                }
                crate::convert_stream_event(event)
            }
        })
    })))
}
