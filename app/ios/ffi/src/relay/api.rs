//! Relay-mode JSON API calls over an interactive API tunnel leg.

use device_proto::noise::StaticKeypair;
use serde::Deserialize;

use super::pairing::load_paired_record;
use super::tunnel::{
    REQUEST_ID, collect_response_body, dial_tunnel_leg, expect_response_head, send_request,
};
use crate::api::ChatSessionSummary;
use crate::core::TunnelRequest;

#[derive(Deserialize)]
struct ChatSessionsList {
    items: Vec<SessionSummary>,
}

#[derive(Deserialize)]
struct SessionSummary {
    session_id: String,
    created_at: String,
    last_active: String,
    #[serde(default)]
    last_user_text: Option<String>,
    pinned: bool,
}

pub(crate) async fn sessions_list() -> Result<Vec<ChatSessionSummary>, String> {
    let record = load_paired_record()?.ok_or("not paired; pair a gateway first")?;
    let local = StaticKeypair::from_parts(record.noise_public, record.noise_secret);
    let (mut ws, mut session) =
        dial_tunnel_leg(&record, &local, remote_host_protocol::relay::LegClass::Api).await?;

    send_request(
        &mut ws,
        &mut session,
        &TunnelRequest::Head {
            request_id: REQUEST_ID,
            method: "GET".into(),
            path: "/v1/chat/sessions".into(),
            headers: Vec::new(),
            body_len: None,
        },
    )
    .await?;

    let (status, _headers, body_len) = expect_response_head(&mut ws, &mut session).await?;
    if !(200..300).contains(&status) {
        return Err(format!("session list failed: HTTP {status}"));
    }
    let body = collect_response_body(&mut ws, &mut session, body_len).await?;
    let _ = ws.close(None).await;
    let list: ChatSessionsList =
        serde_json::from_slice(&body).map_err(|e| format!("decode sessions: {e}"))?;
    Ok(list
        .items
        .into_iter()
        .map(|s| ChatSessionSummary {
            session_id: s.session_id,
            created_at: s.created_at,
            last_active: s.last_active,
            last_user_text: s.last_user_text,
            pinned: s.pinned,
        })
        .collect())
}
