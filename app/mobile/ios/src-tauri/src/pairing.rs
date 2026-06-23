//! The `pair` command's transport: dial the gateway's `/v1/device/pair` WS and
//! drive the 4-message SPAKE2 handshake through `aura_mobile_core::PairingClient`.
//!
//! The crypto + state machine live in the host-tested core; this file is just
//! the WebSocket pump (msgpack `PairFrame`s as binary frames) + the bits the
//! shell owns: generating the device's Noise keypair and a device id.

use aura_device_proto::noise::StaticKeypair;
use aura_device_proto::pairing::{self, ApnsEnv, PairFrame};
use aura_mobile_core::{PairedGateway, PairingClient, PairingRequest};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

/// What the UI renders after a successful (operator-pending) pairing. The
/// secrets (`auth_token`, `push_key`, the Noise static secret) are persisted by
/// the shell, never returned to the webview.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairedSummary {
    pub user_id: String,
    pub relay_node_id: String,
    pub direct_candidates: Vec<String>,
    pub pairing_code: String,
    pub pending_approval: bool,
}

impl From<&PairedGateway> for PairedSummary {
    fn from(p: &PairedGateway) -> Self {
        Self {
            user_id: p.user_id.clone(),
            relay_node_id: p.relay_node_id.clone(),
            direct_candidates: p.direct_candidates.clone(),
            pairing_code: p.pairing_code.clone(),
            // The token is inert until the operator runs `aura device approve`.
            pending_approval: true,
        }
    }
}

/// Run the full pairing handshake against `endpoint` (`ws://` or `wss://`).
pub async fn run_pairing(
    endpoint: &str,
    code: &str,
    label: &str,
) -> Result<PairedSummary, String> {
    let url = format!("{}/v1/device/pair", endpoint.trim_end_matches('/'));
    let (mut ws, _) = connect_async(&url)
        .await
        .map_err(|e| format!("connect {url}: {e}"))?;

    // The app's long-term Noise identity. TODO(persist): the secret belongs in
    // the keychain so content sessions reuse it across launches.
    let keypair = StaticKeypair::generate().map_err(|e| e.to_string())?;
    let device_id = format!("ios-{}", hex::encode(&keypair.public()[..8]));

    let req = PairingRequest {
        code: code.to_string(),
        device_id,
        label: label.to_string(),
        static_pubkey: keypair.public(),
        // TODO(apns): the real APNs token comes from the app's
        // didRegisterForRemoteNotifications; empty until that's wired.
        apns_token: String::new(),
        apns_env: ApnsEnv::Sandbox,
    };

    let (mut client, hello) = PairingClient::start(req);
    send(&mut ws, &hello).await?;

    let PairFrame::PakeReply { pake } = recv(&mut ws).await? else {
        return Err("expected PakeReply".into());
    };
    let sealed = client.on_pake_reply(&pake).map_err(|e| e.to_string())?;
    send(&mut ws, &sealed).await?;

    let PairFrame::Sealed { nonce, ciphertext } = recv(&mut ws).await? else {
        return Err("expected sealed GatewayWelcome".into());
    };
    let paired = client
        .on_welcome(&nonce, &ciphertext)
        .map_err(|e| e.to_string())?;

    // TODO(persist): store `paired` (auth_token, gateway static key, relay/direct)
    // and write `paired.push_key` to the shared App-Group keychain for the NSE.
    Ok(PairedSummary::from(&paired))
}

type Ws = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

async fn send(ws: &mut Ws, frame: &PairFrame) -> Result<(), String> {
    let bytes = pairing::encode(frame).map_err(|e| format!("encode: {e}"))?;
    ws.send(Message::Binary(bytes))
        .await
        .map_err(|e| format!("send: {e}"))
}

async fn recv(ws: &mut Ws) -> Result<PairFrame, String> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Binary(b))) => {
                return pairing::decode(&b).map_err(|e| format!("decode: {e}"));
            }
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            Some(Ok(Message::Close(_))) | None => return Err("connection closed".into()),
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(format!("ws: {e}")),
        }
    }
}
