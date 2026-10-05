//! The phone (P) of the netns matrix (`app/ios/ffi/tests/netns_matrix.rs`):
//! the real ffi client, driven one command per stdin line, answering one JSON
//! object per stdout line. Runs inside the matrix's phone namespace, with the
//! in-memory keychain seeded from the pairing the gateway's seed example
//! printed.
//!
//! Commands:
//! - `seed <path>`: seed the keychain with the paired record JSON at `<path>`.
//! - `network <interface> <wifi|wired|cellular>`: report the path, as
//!   `NWPathMonitor` would, with the families the interface has addresses of.
//! - `preconnect`: dial the relay chat leg, which asks for a probe.
//! - `wait_carrier <secs>`: wait until a direct carrier is live.
//! - `status`: the current carrier state.
//! - `api`: one REST request over the API tunnel (`list_sessions`).
//! - `create <session>` / `chat <session>`: create a session; subscribe to one.
//! - `hold_rotation <on|off>`.
//! - `sleep <secs>`.
//! - `quit`.

use std::io::BufRead;
use std::sync::Arc;
use std::time::Duration;

use baybo_ffi::{
    BayboClient, CarrierLabel, CarrierSink, CarrierStatus, ClientConfig, FrameSink,
    NetworkInterfaceKind, NetworkPath,
};
use parking_lot::Mutex;
use serde_json::{Value, json};

#[derive(Default)]
struct Watch {
    status: Mutex<Option<CarrierStatus>>,
    changed: tokio::sync::Notify,
}

struct StatusSink(Arc<Watch>);

impl CarrierSink for StatusSink {
    fn on_carrier(&self, status: CarrierStatus) {
        *self.0.status.lock() = Some(status);
        self.0.changed.notify_waiters();
    }
}

#[derive(Default)]
struct Frames {
    received: Mutex<Vec<String>>,
}

impl FrameSink for Frames {
    fn on_frame(&self, frame_json: String) {
        self.received.lock().push(frame_json);
    }

    fn on_disconnected(&self, _session_id: String) {}
}

fn label(label: CarrierLabel) -> &'static str {
    match label {
        CarrierLabel::Relay => "relay",
        CarrierLabel::Lan => "lan",
        CarrierLabel::Ipv6 => "ipv6",
        CarrierLabel::Ipv4 => "ipv4",
        CarrierLabel::Ipv4Punched => "ipv4_punched",
    }
}

fn status_json(watch: &Watch) -> Value {
    let status = watch.status.lock().clone();
    let Some(status) = status else {
        return json!({ "carrier": "relay", "last_probe": null });
    };
    let probe = status.last_probe.map(|probe| {
        let tiers: serde_json::Map<String, Value> = probe
            .tiers
            .iter()
            .map(|tier| {
                (
                    label(tier.tier).to_owned(),
                    json!(format!("{:?}", tier.outcome)),
                )
            })
            .collect();
        json!({ "ended_on": label(probe.ended_on), "tiers": tiers })
    });
    json!({ "carrier": label(status.carrier), "last_probe": probe })
}

/// The path `NWPathMonitor` would report for `interface`: the families it
/// routes are the families it has addresses of.
fn path(interface: &str, kind: &str) -> NetworkPath {
    let addresses: Vec<_> = carrier::interfaces::enumerate()
        .into_iter()
        .filter(|address| address.interface == interface)
        .collect();
    NetworkPath {
        satisfied: true,
        interface_kind: match kind {
            "wifi" => NetworkInterfaceKind::Wifi,
            "wired" => NetworkInterfaceKind::Wired,
            "cellular" => NetworkInterfaceKind::Cellular,
            _ => NetworkInterfaceKind::Other,
        },
        interface_name: interface.to_owned(),
        gateways: Vec::new(),
        supports_ipv4: addresses.iter().any(|address| address.ip.is_ipv4()),
        supports_ipv6: addresses
            .iter()
            .any(|address| address.ip.is_ipv6() && !address.unusable),
        available_interfaces: vec![interface.to_owned()],
        is_expensive: false,
        is_constrained: false,
    }
}

fn reply(value: Value) {
    println!("{value}");
}

fn outcome<T>(result: Result<T, baybo_ffi::BayboError>) -> Value {
    match result {
        Ok(_) => json!({ "ok": true }),
        Err(error) => json!({ "ok": false, "error": error.to_string() }),
    }
}

#[tokio::main]
async fn main() {
    let client = BayboClient::new(ClientConfig {
        log_dir: std::env::var("NETNS_PHONE_LOG_DIR").ok(),
        blob_cache_dir: None,
    });
    let watch = Arc::new(Watch::default());
    client.set_carrier_sink(Arc::new(StatusSink(watch.clone())));
    let frames = Arc::new(Frames::default());

    let (lines_tx, mut lines) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { return };
            if lines_tx.send(line).is_err() {
                return;
            }
        }
    });

    while let Some(line) = lines.recv().await {
        let mut words = line.split_whitespace();
        let command = words.next().unwrap_or_default();
        let argument = words.next().unwrap_or_default().to_owned();
        let response = match command {
            "seed" => match std::fs::read_to_string(&argument)
                .map_err(|e| e.to_string())
                .and_then(|json| baybo_ffi::test_support::seed_relay_pairing(&json))
            {
                Ok(()) => json!({ "ok": true }),
                Err(error) => json!({ "ok": false, "error": error }),
            },
            "network" => {
                let kind = words.next().unwrap_or("wifi");
                let path = path(&argument, kind);
                let families = json!({ "ipv4": path.supports_ipv4, "ipv6": path.supports_ipv6 });
                client.network_changed(path);
                json!({ "ok": true, "families": families })
            }
            "preconnect" => outcome(client.clone().relay_preconnect().await),
            "wait_carrier" => {
                let secs: u64 = argument.parse().unwrap_or(30);
                let live = tokio::time::timeout(Duration::from_secs(secs), async {
                    loop {
                        let changed = watch.changed.notified();
                        let live = watch
                            .status
                            .lock()
                            .as_ref()
                            .is_some_and(|status| status.carrier != CarrierLabel::Relay);
                        if live {
                            return;
                        }
                        changed.await;
                    }
                })
                .await
                .is_ok();
                json!({ "ok": live, "status": status_json(&watch) })
            }
            "status" => json!({ "ok": true, "status": status_json(&watch) }),
            "api" => outcome(client.clone().chat_list_sessions().await),
            "create" => match client.clone().chat_create_session(argument.clone()).await {
                Ok(session_id) => json!({ "ok": true, "session_id": session_id }),
                Err(error) => json!({ "ok": false, "error": error.to_string() }),
            },
            "chat" => {
                let result = client
                    .clone()
                    .chat_connect(argument.clone(), frames.clone())
                    .await;
                let mut value = outcome(result);
                value["frames"] = json!(frames.received.lock().len());
                value
            }
            "sleep" => {
                let secs: u64 = argument.parse().unwrap_or(1);
                tokio::time::sleep(Duration::from_secs(secs)).await;
                json!({ "ok": true })
            }
            "hold_rotation" => {
                baybo_ffi::test_support::hold_chat_rotation(argument == "on");
                json!({ "ok": true })
            }
            "quit" => {
                reply(json!({ "ok": true }));
                return;
            }
            other => json!({ "ok": false, "error": format!("unknown command {other:?}") }),
        };
        reply(response);
    }
}
