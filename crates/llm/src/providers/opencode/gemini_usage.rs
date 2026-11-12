//! OpenCode's Gemini surface sends `usageMetadata` objects without
//! `totalTokenCount` (often `{}`) on content chunks. rig 0.37 declares that
//! field required in both the streaming `PartialUsage` and the unary
//! `UsageMetadata`, so it drops every such SSE event — text and function
//! calls included — and rejects a unary body outright. This client fills the
//! missing total in before rig parses anything.

use bytes::Bytes;
use futures::StreamExt;
use rig::http_client::sse::BoxedStream;
use rig::http_client::{
    self, HttpClientExt, LazyBody, MultipartForm, Request, Response, StreamingResponse,
};
use rig::wasm_compat::WasmCompatSend;
use serde_json::Value;

const USAGE_FIELD: &str = "usageMetadata";
const TOTAL_FIELD: &str = "totalTokenCount";
const COUNTED_FIELDS: [&str; 4] = [
    "promptTokenCount",
    "candidatesTokenCount",
    "thoughtsTokenCount",
    "toolUsePromptTokenCount",
];
const DATA_FIELD: &[u8] = b"data:";

/// `reqwest::Client` behind rig's [`HttpClientExt`], rewriting Gemini
/// response bodies so every `usageMetadata` object carries a
/// `totalTokenCount`.
#[derive(Debug, Clone)]
pub(crate) struct GeminiUsageClient {
    inner: reqwest::Client,
}

impl GeminiUsageClient {
    pub(crate) fn new(inner: reqwest::Client) -> Self {
        Self { inner }
    }
}

impl HttpClientExt for GeminiUsageClient {
    fn send<T, U>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        T: Into<Bytes> + WasmCompatSend,
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        let pending = self.inner.send::<T, Bytes>(req);
        async move {
            let (parts, body) = pending.await?.into_parts();
            let body: LazyBody<U> = Box::pin(async move {
                let bytes = body.await?;
                Ok(U::from(normalize_json_body(bytes)))
            });
            Ok(Response::from_parts(parts, body))
        }
    }

    fn send_multipart<U>(
        &self,
        req: Request<MultipartForm>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        self.inner.send_multipart(req)
    }

    fn send_streaming<T>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = http_client::Result<StreamingResponse>> + WasmCompatSend
    where
        T: Into<Bytes> + WasmCompatSend,
    {
        let pending = self.inner.send_streaming(req);
        async move {
            let (parts, body) = pending.await?.into_parts();
            Ok(Response::from_parts(parts, normalize_sse(body)))
        }
    }
}

fn normalize_json_body(body: Bytes) -> Bytes {
    normalize_payload(&body).map(Bytes::from).unwrap_or(body)
}

fn normalize_sse(mut inner: BoxedStream) -> BoxedStream {
    Box::pin(async_stream::stream! {
        let mut normalizer = SseUsageNormalizer::default();
        while let Some(chunk) = inner.next().await {
            match chunk {
                Ok(bytes) => {
                    let out = normalizer.push(&bytes);
                    if !out.is_empty() {
                        yield Ok(Bytes::from(out));
                    }
                }
                Err(error) => yield Err(error),
            }
        }
        let rest = normalizer.finish();
        if !rest.is_empty() {
            yield Ok(Bytes::from(rest));
        }
    })
}

/// Returns the re-serialized payload only when it had to change.
fn normalize_payload(payload: &[u8]) -> Option<Vec<u8>> {
    let mut value: Value = serde_json::from_slice(payload).ok()?;
    let usage = value.get_mut(USAGE_FIELD)?.as_object_mut()?;
    if usage.get(TOTAL_FIELD).is_some_and(Value::is_number) {
        return None;
    }
    let total = COUNTED_FIELDS
        .iter()
        .filter_map(|field| usage.get(*field).and_then(Value::as_i64))
        .fold(0_i64, i64::saturating_add);
    usage.insert(TOTAL_FIELD.into(), total.into());
    serde_json::to_vec(&value).ok()
}

/// Line-buffered SSE rewriter. Events are held until their blank-line
/// terminator so a payload spread over several `data:` lines is judged as
/// the one JSON document SSE joins it into.
#[derive(Default)]
struct SseUsageNormalizer {
    partial_line: Vec<u8>,
    event: Vec<Vec<u8>>,
}

impl SseUsageNormalizer {
    fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.partial_line.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut start = 0;
        while let Some(offset) = self.partial_line[start..].iter().position(|&b| b == b'\n') {
            let end = start + offset + 1;
            let line = self.partial_line[start..end].to_vec();
            start = end;
            if line_content(&line).is_empty() {
                self.flush_event(&mut out);
                out.extend_from_slice(&line);
            } else {
                self.event.push(line);
            }
        }
        self.partial_line.drain(..start);
        out
    }

    fn finish(&mut self) -> Vec<u8> {
        if !self.partial_line.is_empty() {
            self.event.push(std::mem::take(&mut self.partial_line));
        }
        let mut out = Vec::new();
        self.flush_event(&mut out);
        out
    }

    fn flush_event(&mut self, out: &mut Vec<u8>) {
        let lines = std::mem::take(&mut self.event);
        let data: Vec<&[u8]> = lines.iter().filter_map(|line| data_value(line)).collect();
        let normalized = if data.is_empty() {
            None
        } else {
            normalize_payload(&data.join(&b'\n'))
        };
        let Some(payload) = normalized else {
            lines.iter().for_each(|line| out.extend_from_slice(line));
            return;
        };
        let mut written = false;
        for line in &lines {
            if data_value(line).is_none() {
                out.extend_from_slice(line);
            } else if !written {
                written = true;
                out.extend_from_slice(DATA_FIELD);
                out.push(b' ');
                out.extend_from_slice(&payload);
                out.extend_from_slice(line_ending(line));
            }
        }
    }
}

fn line_content(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn line_ending(line: &[u8]) -> &[u8] {
    &line[line_content(line).len()..]
}

fn data_value(line: &[u8]) -> Option<&[u8]> {
    let value = line_content(line).strip_prefix(DATA_FIELD)?;
    Some(value.strip_prefix(b" ").unwrap_or(value))
}

#[cfg(test)]
mod tests {
    use futures::stream;
    use serde_json::json;

    use super::*;

    fn events(body: &[u8]) -> Vec<Value> {
        String::from_utf8(body.to_vec())
            .unwrap()
            .split("\n\n")
            .flat_map(|event| event.split("\r\n\r\n"))
            .filter(|event| !event.trim().is_empty())
            .map(|event| {
                let data: Vec<&str> = event
                    .lines()
                    .filter_map(|line| line.strip_prefix("data:"))
                    .map(|v| v.strip_prefix(' ').unwrap_or(v))
                    .collect();
                serde_json::from_str(&data.join("\n")).unwrap_or(Value::Null)
            })
            .collect()
    }

    fn run(chunks: &[&[u8]]) -> Vec<u8> {
        let mut normalizer = SseUsageNormalizer::default();
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend(normalizer.push(chunk));
        }
        out.extend(normalizer.finish());
        out
    }

    fn multi_event_body() -> String {
        let text = json!({"candidates":[{"content":{"role":"model","parts":[{"text":"héllo — 世界"}]}}],"usageMetadata":{}});
        let call = json!({"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"read_file","args":{}}}]}}],"usageMetadata":{"promptTokenCount":7,"thoughtsTokenCount":2}});
        let done = json!({"candidates":[{"content":{"role":"model","parts":[]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3,"totalTokenCount":10}});
        format!("data: {text}\n\ndata: {call}\r\n\r\n: keepalive\n\ndata: {done}")
    }

    #[test]
    fn fills_missing_totals_across_every_chunk_split() {
        let body = multi_event_body();
        let bytes = body.as_bytes();
        let whole = run(&[bytes]);
        let parsed = events(&whole);
        assert_eq!(parsed[0]["usageMetadata"]["totalTokenCount"], 0);
        assert_eq!(
            parsed[0]["candidates"][0]["content"]["parts"][0]["text"],
            "héllo — 世界"
        );
        assert_eq!(parsed[1]["usageMetadata"]["totalTokenCount"], 9);
        assert_eq!(parsed[2], Value::Null);
        assert_eq!(parsed[3]["usageMetadata"]["totalTokenCount"], 10);
        assert!(whole.ends_with(b"}"), "trailing partial event is flushed");
        for split in 0..=bytes.len() {
            assert_eq!(run(&[&bytes[..split], &bytes[split..]]), whole, "{split}");
        }
        let singles: Vec<&[u8]> = bytes.chunks(1).collect();
        assert_eq!(run(&singles), whole);
    }

    #[test]
    fn keeps_crlf_endings_and_untouched_events_byte_identical() {
        let complete = r#"{"usageMetadata":{"promptTokenCount":1,"totalTokenCount":1}}"#;
        let absent = r#"{"candidates":[]}"#;
        let body = format!(
            "event: x\r\ndata: {complete}\r\n\r\ndata: {absent}\r\n\r\ndata: [DONE]\r\n\r\ndata: not json\n\n"
        );
        assert_eq!(run(&[body.as_bytes()]), body.as_bytes());

        let rewritten = run(&[b"id: 1\r\ndata: {\"usageMetadata\":{}}\r\n\r\n"]);
        assert_eq!(
            rewritten,
            b"id: 1\r\ndata: {\"usageMetadata\":{\"totalTokenCount\":0}}\r\n\r\n"
        );
    }

    #[test]
    fn joins_multi_line_data_fields_before_judging() {
        let body = "data: {\"usageMetadata\":\ndata: {\"promptTokenCount\":4}}\n\n";
        let out = run(&[body.as_bytes()]);
        assert_eq!(
            events(&out)[0]["usageMetadata"],
            json!({"promptTokenCount":4,"totalTokenCount":4})
        );
        assert!(out.ends_with(b"\n\n"));
    }

    #[test]
    fn unary_bodies_are_rewritten_only_when_usage_lacks_a_total() {
        let empty = Bytes::from_static(br#"{"candidates":[],"usageMetadata":{}}"#);
        let fixed: Value = serde_json::from_slice(&normalize_json_body(empty)).unwrap();
        assert_eq!(fixed["usageMetadata"]["totalTokenCount"], 0);
        for unchanged in [
            &br#"{"usageMetadata":{"totalTokenCount":3}}"#[..],
            br#"{"candidates":[]}"#,
            b"not json",
        ] {
            let body = Bytes::from_static(unchanged);
            assert_eq!(normalize_json_body(body.clone()), body);
        }
    }

    #[tokio::test]
    async fn stream_errors_pass_through_and_tail_is_flushed() {
        let inner: BoxedStream = Box::pin(stream::iter(vec![
            Ok(Bytes::from_static(b"data: {\"usageMeta")),
            Ok(Bytes::from_static(b"data\":{}}\n\ndata: {\"a\":1}")),
            Err(http_client::Error::StreamEnded),
        ]));
        let items: Vec<_> = normalize_sse(inner).collect().await;
        assert_eq!(items.len(), 3);
        assert_eq!(
            items[0].as_ref().unwrap().as_ref(),
            b"data: {\"usageMetadata\":{\"totalTokenCount\":0}}\n\n"
        );
        assert!(matches!(items[1], Err(http_client::Error::StreamEnded)));
        assert_eq!(items[2].as_ref().unwrap().as_ref(), b"data: {\"a\":1}");
    }
}
