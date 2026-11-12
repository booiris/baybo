//! One-connection-per-response HTTP endpoint for provider wire tests: each
//! scripted response answers the next request, whose path, headers and JSON
//! body are captured for assertions.

use std::collections::HashMap;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(crate) struct CapturedRequest {
    pub(crate) path: String,
    pub(crate) headers: HashMap<String, String>,
    pub(crate) body: Value,
}

pub(crate) async fn endpoint(
    responses: Vec<(u16, &'static str, String)>,
) -> (
    String,
    tokio::sync::mpsc::UnboundedReceiver<CapturedRequest>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        for (status, content_type, body) in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut buf = [0; 4096];
            let header_end = loop {
                let n = socket.read(&mut buf).await.unwrap();
                assert_ne!(n, 0);
                bytes.extend_from_slice(&buf[..n]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let head = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
            let mut lines = head.lines();
            let path = lines
                .next()
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap()
                .to_string();
            let headers: HashMap<_, _> = lines
                .filter_map(|line| line.split_once(':'))
                .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
                .collect();
            let len: usize = headers
                .get("content-length")
                .map(|n| n.parse().unwrap())
                .unwrap_or(0);
            while bytes.len() < header_end + len {
                let n = socket.read(&mut buf).await.unwrap();
                assert_ne!(n, 0);
                bytes.extend_from_slice(&buf[..n]);
            }
            let _ = tx.send(CapturedRequest {
                path,
                headers,
                body: if len == 0 {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes[header_end..header_end + len]).unwrap()
                },
            });
            let response = format!(
                "HTTP/1.1 {status} Test\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    (origin, rx)
}
