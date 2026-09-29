//! `baybo device status` against a stand-in gateway: it presents the vault's
//! admin token on `GET /v1/mobile/links`, joins the answer to the approved
//! device rows, and prints the rows alone, saying why, when no gateway
//! answers.

use std::net::SocketAddr;
use std::sync::Arc;

use baybo_cli::cli::{Commands, DeviceCmd};
use baybo_cli::{CommandContext, ContextBuilder, Invocation, OutputFormat, dispatch};
use baybo_config::BayboConfig;
use baybo_gateway::AdminToken;
use baybo_gateway::api::admin::mobile::{
    DeviceLink, LastOffer, LinkCarrier, LinkClass, LiveLeg, MOBILE_LINKS_PATH, MobileLinks,
    OfferOutcome,
};
use baybo_pairing::DevicePairingService;
use baybo_security::test_support::MemorySecretStore;
use baybo_security::{EncryptionKey, SecretVault};
use baybo_storage::test_support::MemoryDeviceStore;
use baybo_store::{DeviceRow, DeviceStatus, DeviceStore};
use chrono::{TimeZone, Utc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket};
use tokio::task::JoinHandle;

const APPROVED: &str = "device-approved";
const REVOKED: &str = "device-revoked";
const IDLE: &str = "device-idle";
const REQUEST_LIMIT: usize = 16 * 1024;

fn row(device_id: &str, status: DeviceStatus) -> DeviceRow {
    DeviceRow {
        device_id: device_id.to_owned(),
        device_pubkey: vec![7; 32],
        auth_token_sha256: baybo_store::device::hash_auth_token(&format!("{device_id}-token")),
        status,
        rendezvous_id: None,
        created_at: 1_700_000_000,
        approved_at: Some(1_700_000_100),
        last_seen_at: None,
        relay_url: "wss://relay.test".to_owned(),
        push_url: "https://push.test".to_owned(),
        remote_api_key: "guest".to_owned(),
    }
}

/// A workspace whose approved device is `approved`, beside a revoked one,
/// and whose gateway listens at `gateway`. With `mint`, its vault holds an
/// admin token, which is returned.
async fn context(
    gateway: SocketAddr,
    approved: &str,
    mint: bool,
) -> (CommandContext, Option<String>) {
    let devices = Arc::new(MemoryDeviceStore::new());
    for (device_id, status) in [
        (approved, DeviceStatus::Approved),
        (REVOKED, DeviceStatus::Revoked),
    ] {
        devices.create(&row(device_id, status)).await.unwrap();
    }
    let vault = Arc::new(SecretVault::new(
        EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec()).unwrap(),
        Arc::new(MemorySecretStore::new()),
    ));
    let token = if mint {
        Some(
            AdminToken::new(Arc::clone(&vault))
                .mint_if_absent()
                .await
                .unwrap(),
        )
    } else {
        None
    };
    let mut config = BayboConfig::default();
    config.gateway.bind_address = gateway.ip().to_string();
    config.gateway.port = gateway.port();
    let ctx = ContextBuilder::new(Arc::new(config))
        .secret_vault(vault)
        .device_pairing_service(Arc::new(DevicePairingService::new(devices)))
        .build()
        .with_invocation(Invocation::Argv)
        .with_format(OutputFormat::Plain);
    (ctx, token)
}

fn links() -> MobileLinks {
    MobileLinks {
        devices: vec![
            DeviceLink {
                device_id: APPROVED.to_owned(),
                legs: vec![
                    LiveLeg {
                        class: LinkClass::Chat,
                        carrier: Some(LinkCarrier::Ipv4Punched),
                        started_at: Utc.timestamp_opt(1_700_000_200, 0).unwrap(),
                    },
                    LiveLeg {
                        class: LinkClass::Api,
                        carrier: Some(LinkCarrier::Relay),
                        started_at: Utc.timestamp_opt(1_700_000_300, 0).unwrap(),
                    },
                ],
                last_offer: Some(LastOffer {
                    at: Utc.timestamp_opt(1_700_000_190, 0).unwrap(),
                    outcome: OfferOutcome::Accepted,
                }),
            },
            DeviceLink {
                device_id: REVOKED.to_owned(),
                legs: Vec::new(),
                last_offer: Some(LastOffer {
                    at: Utc.timestamp_opt(1_700_000_050, 0).unwrap(),
                    outcome: OfferOutcome::DeclinedStale,
                }),
            },
        ],
    }
}

/// Answers one request: the link table for `GET /v1/mobile/links` carrying
/// `Bearer {token}`, `401` for anything else.
fn stand_in_gateway(listener: TcpListener, token: String) -> JoinHandle<String> {
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buf = [0u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let n = stream.read(&mut buf).await.unwrap();
            assert!(n > 0 && request.len() + n <= REQUEST_LIMIT);
            request.extend_from_slice(&buf[..n]);
        }
        let request = String::from_utf8(request).unwrap();
        let authorized = request.starts_with(&format!("GET {MOBILE_LINKS_PATH} HTTP/1.1\r\n"))
            && request
                .to_ascii_lowercase()
                .contains(&format!("authorization: bearer {token}\r\n"));
        let (status, body) = if authorized {
            ("200 OK", serde_json::to_string(&links()).unwrap())
        } else {
            ("401 Unauthorized", r#"{"error":"unauthorized"}"#.to_owned())
        };
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
        request
    })
}

async fn listener() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    (listener, address)
}

async fn status(ctx: &CommandContext) -> baybo_cli::CommandOutput {
    dispatch::run(
        ctx,
        Commands::Device {
            cmd: DeviceCmd::Status,
        },
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn a_reachable_gateway_adds_each_approved_devices_legs_and_last_offer() {
    let (listener, gateway) = listener().await;
    let (ctx, token) = context(gateway, APPROVED, true).await;
    let token = token.unwrap();
    let served = stand_in_gateway(listener, token.clone());
    let out = status(&ctx).await;
    served.await.unwrap();

    let human = &out.human;
    assert!(!human.contains("showing the device rows alone"), "{human}");
    assert!(human.starts_with(APPROVED), "{human}");
    assert!(
        human.contains("chat\tipv4_punched\tsince 2023-11-14T22:16:40Z"),
        "{human}"
    );
    assert!(
        human.contains("api\trelay\tsince 2023-11-14T22:18:20Z"),
        "{human}"
    );
    assert!(
        human.contains("last offer: accepted at 2023-11-14T22:16:30Z"),
        "{human}"
    );
    assert!(!human.contains(REVOKED), "only approved devices: {human}");
    assert!(!human.contains(&token), "{human}");

    let data = out.data.unwrap();
    assert_eq!(data["gateway"]["address"], gateway.to_string());
    assert!(data["gateway"]["error"].is_null());
    let [approved] = data["devices"].as_array().unwrap().as_slice() else {
        panic!("only the approved device: {data}");
    };
    assert_eq!(approved["device_id"], APPROVED);
    assert_eq!(approved["legs"][0]["carrier"], "ipv4_punched");
    assert_eq!(approved["last_offer"]["outcome"], "accepted");
}

#[tokio::test]
async fn an_approved_device_the_gateway_does_not_list_has_no_live_legs() {
    let (listener, gateway) = listener().await;
    let (ctx, token) = context(gateway, IDLE, true).await;
    let served = stand_in_gateway(listener, token.unwrap());
    let out = status(&ctx).await;
    served.await.unwrap();

    let human = &out.human;
    assert!(human.starts_with(IDLE), "{human}");
    assert!(human.contains("no live legs"), "{human}");
    assert!(human.contains("last offer: none"), "{human}");
    let data = out.data.unwrap();
    assert_eq!(data["devices"][0]["legs"], serde_json::json!([]));
    assert!(data["devices"][0]["last_offer"].is_null());
}

#[tokio::test]
async fn an_unreachable_gateway_leaves_the_device_rows_alone_and_says_so() {
    // Bound but never listening: a connect is refused, and no other test can
    // take the port while this socket holds it.
    let refusing = TcpSocket::new_v4().unwrap();
    refusing.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let gateway = refusing.local_addr().unwrap();
    let (ctx, _) = context(gateway, APPROVED, true).await;
    let out = status(&ctx).await;

    let human = &out.human;
    let first = human.lines().next().unwrap();
    assert!(
        first.starts_with(&format!("gateway not reachable at {gateway}")),
        "{human}"
    );
    assert!(first.ends_with("showing the device rows alone"), "{human}");
    assert!(human.contains(APPROVED), "{human}");
    assert!(!human.contains("last offer"), "{human}");
    assert!(!human.contains("live legs"), "{human}");

    let data = out.data.unwrap();
    assert!(data["gateway"]["error"].is_string());
    for device in data["devices"].as_array().unwrap() {
        assert!(
            device["legs"].is_null(),
            "legs unknown, not empty: {device}"
        );
    }
}

#[tokio::test]
async fn a_gateway_that_refuses_the_token_is_not_called_unreachable() {
    let (listener, gateway) = listener().await;
    let (ctx, _) = context(gateway, APPROVED, true).await;
    let served = stand_in_gateway(listener, "another-workspace's-token".to_owned());
    let out = status(&ctx).await;
    served.await.unwrap();

    let first = out.human.lines().next().unwrap();
    assert!(first.contains("refused the vault's admin token"), "{first}");
    assert!(!first.contains("not reachable"), "{first}");
    assert!(out.human.contains(APPROVED), "{}", out.human);
}

#[tokio::test]
async fn without_an_admin_token_the_gateway_is_not_asked() {
    let (listener, gateway) = listener().await;
    let (ctx, _) = context(gateway, APPROVED, false).await;
    let out = status(&ctx).await;

    let first = out.human.lines().next().unwrap();
    assert!(first.contains("holds no admin token"), "{first}");
    assert!(out.human.contains(APPROVED), "{}", out.human);
    let accepted =
        tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept()).await;
    assert!(accepted.is_err(), "nothing dialed the gateway");
}
