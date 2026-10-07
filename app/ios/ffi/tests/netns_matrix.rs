//! The direct-carrier NAT matrix (`docs/modules/mobile/direct-carriers.md`
//! § Testing): C, A and P run as real processes — the `remote-host` binary,
//! the `baybo` gateway and the ffi client — each in its own network namespace
//! behind a NAT profile, and every cell asserts the carrier P ends
//! on, and that it carries API and chat traffic.
//!
//! `#[ignore]`d: it needs root, `ip`, `iptables`, `tc` and `sqlite3`, and the binaries
//! `scripts/netns-matrix.sh` builds and names through `NETNS_*_BIN`.
//! `NETNS_CELLS` (space-separated cell names) narrows the run; `NETNS_RUNS`
//! (default 5) and `NETNS_IDLE_SECS` (default 40) size it.
//!
//! ```text
//!  ns:a ── ns:nat-a ──┐                        ┌── ns:nat-p ── ns:p
//!  10.0.1.2 · 2001:2:0:a::2   ns:inet (bridge)  10.0.2.2 · 2001:2:0:b::2
//!                     ├──── 198.18.0.0/24 ─────┤
//!                     │     2001:2::/64        │
//!                     └──────── ns:c ──────────┘   198.18.0.10
//!  same-LAN variant: ns:p on ns:a's LAN (10.0.1.3)
//! ```

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const RELAY_KEY: &str = "netns-matrix-key";
const C_WAN: &str = "198.18.0.10";
const C_WAN_V6: &str = "2001:2::10";
const C_PORT: u16 = 7777;
const GATEWAY_ADMIN_PORT: u16 = 18888;
const SESSION: &str = "netns-matrix-session";
/// How long P may take to land a carrier: the settle delay, the offer's POST,
/// `PROBE_BUDGET` and the proof leg.
const CARRIER_WAIT_SECS: u64 = 25;
/// How long a chat rotation may take once released: `CHAT_ROTATION_QUIET`
/// plus the dial.
const ROTATION_WAIT: Duration = Duration::from_secs(12);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Profile {
    /// Routed, no NAT, no filter: the host has a public address.
    Open,
    /// Endpoint-independent mapping and filtering.
    Cone,
    /// Endpoint-independent mapping, address-and-port-dependent filtering.
    PortRestricted,
    /// Address-and-port-dependent mapping.
    Symmetric,
}

impl Profile {
    const ALL: [Self; 4] = [
        Self::Open,
        Self::Cone,
        Self::PortRestricted,
        Self::Symmetric,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Cone => "cone",
            Self::PortRestricted => "port-restricted",
            Self::Symmetric => "symmetric",
        }
    }
}

/// One cell of the matrix.
#[derive(Debug, Clone)]
struct Cell {
    name: String,
    phone: Profile,
    gateway: Profile,
    /// Both sides get routed GUAs behind a stateful IPv6 firewall.
    ipv6: bool,
    relay_ipv6: bool,
    /// P sits on A's LAN.
    same_lan: bool,
    udp_blocked_phone: bool,
    udp_blocked_gateway: bool,
    /// C runs its UDP rendezvous.
    rendezvous: bool,
    /// `tc netem loss 5%` on both WANs.
    loss: bool,
    expected: &'static str,
}

impl Cell {
    fn base(phone: Profile, gateway: Profile, expected: &'static str) -> Self {
        Self {
            name: format!("{}/{}", phone.name(), gateway.name()),
            phone,
            gateway,
            ipv6: false,
            relay_ipv6: false,
            same_lan: false,
            udp_blocked_phone: false,
            udp_blocked_gateway: false,
            rendezvous: true,
            loss: false,
            expected,
        }
    }

    fn named(mut self, name: &str) -> Self {
        self.name = name.to_owned();
        self
    }
}

/// The expected IPv4 results, with no IPv6 and different networks.
fn ipv4_expectation(phone: Profile, gateway: Profile) -> &'static str {
    use Profile::{Open, Symmetric};
    match (phone, gateway) {
        (_, Open) => "ipv4",
        (_, Symmetric) => "relay",
        (Symmetric, Profile::PortRestricted) => "relay",
        _ => "ipv4_punched",
    }
}

fn cells() -> Vec<Cell> {
    let mut cells = Vec::new();
    for phone in Profile::ALL {
        for gateway in Profile::ALL {
            cells.push(Cell::base(phone, gateway, ipv4_expectation(phone, gateway)));
        }
    }
    let pr = Profile::PortRestricted;
    cells.push(Cell {
        same_lan: true,
        ..Cell::base(pr, pr, "lan").named("same-lan")
    });
    cells.push(Cell {
        ipv6: true,
        ..Cell::base(pr, pr, "ipv6").named("ipv6")
    });
    cells.push(Cell {
        ipv6: true,
        relay_ipv6: true,
        ..Cell::base(pr, pr, "ipv6").named("ipv6-relay")
    });
    cells.push(Cell {
        udp_blocked_gateway: true,
        ..Cell::base(pr, pr, "relay").named("udp-blocked-gateway")
    });
    cells.push(Cell {
        udp_blocked_phone: true,
        ..Cell::base(pr, pr, "relay").named("udp-blocked-phone")
    });
    cells.push(Cell {
        rendezvous: false,
        ..Cell::base(pr, Profile::Open, "ipv4").named("no-rendezvous/open")
    });
    cells.push(Cell {
        rendezvous: false,
        ..Cell::base(pr, Profile::Cone, "relay").named("no-rendezvous/cone")
    });
    cells.push(Cell {
        loss: true,
        ..Cell::base(pr, pr, "ipv4_punched").named("loss")
    });
    cells
}

fn env_path(name: &str) -> PathBuf {
    PathBuf::from(std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set")))
}

fn env_number(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// Runs `program args` and fails the test on a non-zero exit.
fn run(program: &str, args: &[&str]) {
    let output = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("{program} {args:?}: {e}"));
    assert!(
        output.status.success(),
        "{program} {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn ip(args: &[&str]) {
    run("ip", args);
}

fn in_ns(ns: &str, args: &[&str]) {
    let mut full = vec!["netns", "exec", ns];
    full.extend_from_slice(args);
    run("ip", &full);
}

/// The namespaces of one run. Dropping it deletes them, panic or not.
struct Topology {
    prefix: String,
}

impl Topology {
    fn ns(&self, role: &str) -> String {
        format!("{}-{role}", self.prefix)
    }

    fn build(cell: &Cell, run_index: u64) -> Self {
        let topology = Self {
            prefix: format!("bnm{run_index}"),
        };
        let [a, nat_a, inet, nat_p, p, c] =
            ["a", "nata", "inet", "natp", "p", "c"].map(|role| topology.ns(role));
        for ns in [&a, &nat_a, &inet, &nat_p, &p, &c] {
            let _ = Command::new("ip").args(["netns", "del", ns]).output();
            ip(&["netns", "add", ns]);
            ip(&["-n", ns, "link", "set", "lo", "up"]);
            in_ns(ns, &["sysctl", "-qw", "net.ipv6.conf.all.accept_dad=0"]);
            in_ns(ns, &["sysctl", "-qw", "net.ipv6.conf.default.accept_dad=0"]);
        }

        // The WAN: one bridge in inet joining both NAT boxes and C.
        ip(&["-n", &inet, "link", "add", "br0", "type", "bridge"]);
        ip(&["-n", &inet, "link", "set", "br0", "up"]);
        for (peer, port, wan) in [
            (&nat_a, "to-nata", "wan"),
            (&nat_p, "to-natp", "wan"),
            (&c, "to-c", "eth0"),
        ] {
            ip(&[
                "-n", &inet, "link", "add", port, "type", "veth", "peer", "name", wan, "netns",
                peer,
            ]);
            ip(&["-n", &inet, "link", "set", port, "master", "br0"]);
            ip(&["-n", &inet, "link", "set", port, "up"]);
            ip(&["-n", peer, "link", "set", wan, "up"]);
        }
        addr(&nat_a, "wan", "198.18.0.1/24");
        addr(&nat_p, "wan", "198.18.0.2/24");
        addr(&c, "eth0", &format!("{C_WAN}/24"));
        if cell.ipv6 {
            addr(&nat_a, "wan", "2001:2::1/64");
            addr(&nat_p, "wan", "2001:2::2/64");
            addr(&c, "eth0", &format!("{C_WAN_V6}/64"));
            for (subnet, router) in [
                ("2001:2:0:a::/64", "2001:2::1"),
                ("2001:2:0:b::/64", "2001:2::2"),
            ] {
                ip(&["-n", &c, "-6", "route", "add", subnet, "via", router]);
            }
        }

        // A's LAN, and P's (or P on A's).
        let (a_net, a_host) = lan(cell.gateway, 1);
        ip(&["-n", &nat_a, "link", "add", "br-lan", "type", "bridge"]);
        ip(&["-n", &nat_a, "link", "set", "br-lan", "up"]);
        attach(&nat_a, "lan-a", &a, "eth0");
        addr(&nat_a, "br-lan", &format!("{a_net}.1/24"));
        addr(&a, "eth0", &format!("{a_net}.{a_host}/24"));
        ip(&[
            "-n",
            &a,
            "route",
            "add",
            "default",
            "via",
            &format!("{a_net}.1"),
        ]);
        if cell.same_lan {
            attach(&nat_a, "lan-p", &p, "eth0");
            addr(&p, "eth0", &format!("{a_net}.3/24"));
            ip(&[
                "-n",
                &p,
                "route",
                "add",
                "default",
                "via",
                &format!("{a_net}.1"),
            ]);
        } else {
            let (p_net, p_host) = lan(cell.phone, 2);
            ip(&["-n", &nat_p, "link", "add", "br-lan", "type", "bridge"]);
            ip(&["-n", &nat_p, "link", "set", "br-lan", "up"]);
            attach(&nat_p, "lan-p", &p, "eth0");
            addr(&nat_p, "br-lan", &format!("{p_net}.1/24"));
            addr(&p, "eth0", &format!("{p_net}.{p_host}/24"));
            ip(&[
                "-n",
                &p,
                "route",
                "add",
                "default",
                "via",
                &format!("{p_net}.1"),
            ]);
        }
        if cell.ipv6 {
            addr(&nat_a, "br-lan", "2001:2:0:a::1/64");
            addr(&a, "eth0", "2001:2:0:a::2/64");
            ip(&[
                "-n",
                &a,
                "-6",
                "route",
                "add",
                "default",
                "via",
                "2001:2:0:a::1",
            ]);
            addr(&nat_p, "br-lan", "2001:2:0:b::1/64");
            addr(&p, "eth0", "2001:2:0:b::2/64");
            ip(&[
                "-n",
                &p,
                "-6",
                "route",
                "add",
                "default",
                "via",
                "2001:2:0:b::1",
            ]);
            ip(&[
                "-n",
                &nat_a,
                "-6",
                "route",
                "add",
                "2001:2:0:b::/64",
                "via",
                "2001:2::2",
            ]);
            ip(&[
                "-n",
                &nat_p,
                "-6",
                "route",
                "add",
                "2001:2:0:a::/64",
                "via",
                "2001:2::1",
            ]);
        }

        // Public LANs are routed across the WAN; private ones never are.
        if cell.gateway == Profile::Open {
            for ns in [&nat_p, &c] {
                ip(&[
                    "-n",
                    ns,
                    "route",
                    "add",
                    "198.18.1.0/24",
                    "via",
                    "198.18.0.1",
                ]);
            }
        }
        if cell.phone == Profile::Open && !cell.same_lan {
            for ns in [&nat_a, &c] {
                ip(&[
                    "-n",
                    ns,
                    "route",
                    "add",
                    "198.18.2.0/24",
                    "via",
                    "198.18.0.2",
                ]);
            }
        }

        for (ns, profile, host, blocked) in [
            (
                &nat_a,
                cell.gateway,
                format!("{a_net}.{a_host}"),
                cell.udp_blocked_gateway,
            ),
            (
                &nat_p,
                cell.phone,
                format!("{}.2", lan(cell.phone, 2).0),
                cell.udp_blocked_phone,
            ),
        ] {
            in_ns(ns, &["sysctl", "-qw", "net.ipv4.ip_forward=1"]);
            in_ns(ns, &["sysctl", "-qw", "net.ipv6.conf.all.forwarding=1"]);
            for rule in rules(profile, &host, cell.ipv6, blocked) {
                let words: Vec<&str> = rule.split_whitespace().collect();
                in_ns(ns, &words);
            }
            // Short UDP timeouts, so the idle phase exercises the keepalive.
            in_ns(
                ns,
                &["sysctl", "-qw", "net.netfilter.nf_conntrack_udp_timeout=30"],
            );
            in_ns(
                ns,
                &[
                    "sysctl",
                    "-qw",
                    "net.netfilter.nf_conntrack_udp_timeout_stream=30",
                ],
            );
            if cell.loss {
                in_ns(
                    ns,
                    &[
                        "tc", "qdisc", "add", "dev", "wan", "root", "netem", "loss", "5%",
                    ],
                );
            }
        }
        topology
    }
}

impl Drop for Topology {
    fn drop(&mut self) {
        for role in ["a", "nata", "inet", "natp", "p", "c"] {
            let _ = Command::new("ip")
                .args(["netns", "del", &self.ns(role)])
                .output();
        }
    }
}

fn addr(ns: &str, dev: &str, address: &str) {
    if address.contains(':') {
        ip(&["-n", ns, "addr", "add", address, "dev", dev, "nodad"]);
    } else {
        ip(&["-n", ns, "addr", "add", address, "dev", dev]);
    }
}

/// A veth from `bridge_ns`'s LAN bridge to `host_ns`'s `host_dev`.
fn attach(bridge_ns: &str, port: &str, host_ns: &str, host_dev: &str) {
    ip(&[
        "-n", bridge_ns, "link", "add", port, "type", "veth", "peer", "name", host_dev, "netns",
        host_ns,
    ]);
    ip(&["-n", bridge_ns, "link", "set", port, "master", "br-lan"]);
    ip(&["-n", bridge_ns, "link", "set", port, "up"]);
    ip(&["-n", host_ns, "link", "set", host_dev, "up"]);
}

/// A side's LAN prefix (/24) and host octet: public and routed for `Open`,
/// private behind NAT otherwise.
fn lan(profile: Profile, side: u8) -> (String, u8) {
    match profile {
        Profile::Open => (format!("198.18.{side}"), 2),
        _ => (format!("10.0.{side}"), 2),
    }
}

/// One NAT box's packet filter (iptables, on its nftables backend): the
/// profile's NAT for IPv4, the consumer-router input chain that drops
/// unsolicited WAN traffic before conntrack confirms it — without it an early
/// inbound punch pins a conntrack entry and masquerade remaps the host's port
/// — and the stateful IPv6 firewall.
fn rules(profile: Profile, host: &str, ipv6: bool, udp_blocked: bool) -> Vec<String> {
    let mut rules = vec!["iptables -A INPUT -i wan -m conntrack --ctstate NEW -j DROP".to_owned()];
    match profile {
        Profile::Open => {}
        Profile::Cone => {
            rules.push(format!(
                "iptables -t nat -A PREROUTING -i wan -p udp --dport 1024:65535 -j DNAT --to-destination {host}"
            ));
            rules.push(format!(
                "iptables -t nat -A POSTROUTING -o wan -j SNAT --to-source 198.18.0.{}",
                wan_octet(host)
            ));
        }
        Profile::PortRestricted => {
            rules.push("iptables -t nat -A POSTROUTING -o wan -j MASQUERADE".to_owned());
        }
        Profile::Symmetric => {
            rules.push(
                "iptables -t nat -A POSTROUTING -o wan -j MASQUERADE --random-fully".to_owned(),
            );
        }
    }
    if udp_blocked {
        rules.push("iptables -A FORWARD -p udp -j DROP".to_owned());
        rules.push("ip6tables -A FORWARD -p udp -j DROP".to_owned());
    }
    if ipv6 {
        rules.push("ip6tables -A INPUT -i wan -m conntrack --ctstate NEW -j DROP".to_owned());
        rules.push("ip6tables -A FORWARD -i wan -m conntrack --ctstate NEW -j DROP".to_owned());
    }
    rules
}

/// The WAN address's last octet of the NAT box in front of `host`.
fn wan_octet(host: &str) -> u8 {
    if host.starts_with("10.0.1.") || host.starts_with("198.18.1.") {
        1
    } else {
        2
    }
}

/// A child process killed when dropped.
struct Process {
    child: Child,
    name: &'static str,
    log: PathBuf,
}

impl Process {
    fn spawn(name: &'static str, ns: &str, command: &mut Command, log: PathBuf) -> Self {
        let log_file = std::fs::File::create(&log).expect("log file");
        let mut wrapped = Command::new("ip");
        wrapped
            .args(["netns", "exec", ns])
            .arg(command.get_program())
            .args(command.get_args())
            .envs(command.get_envs().filter_map(|(k, v)| Some((k, v?))))
            .stdout(log_file.try_clone().expect("log"))
            .stderr(log_file);
        let child = wrapped
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {name}: {e}"));
        Self { child, name, log }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if std::thread::panicking() {
            eprintln!("--- {} log ({}) ---", self.name, self.log.display());
            if let Ok(log) = std::fs::read_to_string(&self.log) {
                let tail: Vec<&str> = log.lines().rev().take(40).collect();
                for line in tail.iter().rev() {
                    eprintln!("{line}");
                }
            }
        }
    }
}

/// The phone driver (`examples/netns_phone.rs`).
struct Phone {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    log_dir: PathBuf,
}

impl Phone {
    fn spawn(ns: &str, dir: &Path) -> Self {
        let log_dir = dir.join("phone");
        std::fs::create_dir_all(&log_dir).expect("phone log dir");
        let mut child = Command::new("ip")
            .args(["netns", "exec", ns])
            .arg(env_path("NETNS_PHONE_BIN"))
            .env("NETNS_PHONE_LOG_DIR", &log_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the phone");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Self {
            child,
            stdin,
            stdout,
            log_dir,
        }
    }

    fn command(&mut self, line: &str) -> Value {
        writeln!(self.stdin, "{line}").expect("phone stdin");
        let mut reply = String::new();
        self.stdout.read_line(&mut reply).expect("phone stdout");
        serde_json::from_str(&reply).unwrap_or_else(|e| panic!("{line}: {e}: {reply:?}"))
    }

    fn ok(&mut self, line: &str) -> Value {
        let reply = self.command(line);
        assert_eq!(reply["ok"], true, "{line}: {reply}");
        reply
    }
}

impl Drop for Phone {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if std::thread::panicking() {
            let log = self.log_dir.join("baybo.log");
            eprintln!("--- phone log ({}) ---", log.display());
            if let Ok(log) = std::fs::read_to_string(&log) {
                let tail: Vec<&str> = log.lines().rev().take(60).collect();
                for line in tail.iter().rev() {
                    eprintln!("{line}");
                }
            }
        }
    }
}

/// A's link table, through `baybo device status --json` in A's namespace:
/// `(class, carrier)` per live leg.
fn gateway_legs(ns: &str, config: &Path) -> Vec<(String, String)> {
    let output = Command::new("ip")
        .args(["netns", "exec", ns])
        .arg(env_path("NETNS_GATEWAY_BIN"))
        .arg("--config")
        .arg(config)
        .args(["device", "status", "--json"])
        .env("NETNS_FAKE_LLM_KEY", "sk-fake")
        .output()
        .expect("device status");
    let status: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "device status: {e}: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    status["devices"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|device| device["legs"].as_array().cloned().unwrap_or_default())
        .map(|leg| {
            (
                leg["class"].as_str().unwrap_or_default().to_owned(),
                leg["carrier"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

fn until(what: &str, timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// One run of `cell`, asserting in order: the first chat leg rides the
/// relay; the final carrier, and an API request on it; after the idle phase,
/// the same carrier and a second API request with no relay fallback; then the
/// chat rotates and a chat frame round-trips on the carrier.
fn run_cell(cell: &Cell, run_index: u64, idle: Duration) {
    let topology = Topology::build(cell, run_index);
    let dir = tempfile_dir(&format!("{}-{run_index}", cell.name.replace('/', "_")));

    // C, with its admission table.
    let admission = dir.join("admission.db");
    run(
        env_path("NETNS_SQLITE3_BIN").to_str().expect("utf-8"),
        &[
            admission.to_str().expect("utf-8"),
            &format!(
                "CREATE TABLE IF NOT EXISTS remote_api_keys (remote_api_key TEXT PRIMARY KEY, label TEXT, \
                 max_conns INTEGER, max_bps INTEGER, per_server_max_bps INTEGER, expires_at TEXT, \
                 created_at TEXT NOT NULL DEFAULT (datetime('now')), \
                 CHECK (max_conns IS NOT NULL AND max_bps IS NOT NULL)); \
                 INSERT INTO remote_api_keys(remote_api_key, label, max_conns, max_bps) \
                 VALUES ('{RELAY_KEY}', 'netns', 64, 100000000);"
            ),
        ],
    );
    let mut relay = Command::new(env_path("NETNS_RELAY_BIN"));
    let relay_host = if cell.relay_ipv6 {
        format!("[{C_WAN_V6}]")
    } else {
        C_WAN.to_owned()
    };
    relay
        .env("BIND_ADDR", format!("{relay_host}:{C_PORT}"))
        .env("ADMISSION_DB_PATH", &admission)
        .env("ADMISSION_POLL_SECS", "1")
        .env("TRAFFIC_DB_PATH", "")
        .env("LOG_DIR", &dir)
        .env("RUST_LOG", "remote_host=info,remote_host_relay=info");
    if cell.rendezvous {
        relay.env("UDP_PUBLIC_ADDR", format!("{C_WAN}:{C_PORT}"));
    }
    let _c = Process::spawn("C", &topology.ns("c"), &mut relay, dir.join("c.log"));

    // A: a seeded workspace, then the gateway.
    let key = dir.join("key");
    std::fs::write(&key, hex32()).expect("key file");
    let config = dir.join("baybo.json");
    std::fs::write(
        &config,
        serde_json::to_vec_pretty(&serde_json::json!({
            "llm": [{"name": "fake", "provider": "openai", "model": "gpt-4o-mini",
                     "api_key_env": "NETNS_FAKE_LLM_KEY"}],
            "default-llm": "fake",
            "security": {"encryption_key_file": key},
            "workspace": {"path": dir.join("workspace")},
            "gateway": {"enabled": true, "bind_address": "127.0.0.1", "port": GATEWAY_ADMIN_PORT,
                        "direct_udp": {"enabled": true, "ipv4_bind": "0.0.0.0:0", "ipv6_bind": "[::]:0"}},
        }))
        .expect("config"),
    )
    .expect("write config");
    let record = dir.join("record.json");
    run(
        env_path("NETNS_SEED_BIN").to_str().expect("utf-8"),
        &[
            config.to_str().expect("utf-8"),
            &format!("ws://{relay_host}:{C_PORT}"),
            RELAY_KEY,
            record.to_str().expect("utf-8"),
        ],
    );
    // Any `baybo` command bootstraps the workspace (its git-backed agents
    // directory among it); do it once, before the gateway and the status
    // polls below could race each other through it.
    let _ = gateway_legs(&topology.ns("a"), &config);
    let mut gateway = Command::new(env_path("NETNS_GATEWAY_BIN"));
    gateway
        .arg("--config")
        .arg(&config)
        .args(["gateway", "start"])
        .env("NETNS_FAKE_LLM_KEY", "sk-fake")
        .env("RUST_LOG", "baybo_gateway=info,carrier=info");
    let _a = Process::spawn("A", &topology.ns("a"), &mut gateway, dir.join("a.log"));
    let a = topology.ns("a");
    until("A's relay binding is up", Duration::from_secs(30), || {
        Command::new("ip")
            .args(["netns", "exec", &a])
            .arg(env_path("NETNS_GATEWAY_BIN"))
            .arg("--config")
            .arg(&config)
            .args(["device", "status", "--json"])
            .env("NETNS_FAKE_LLM_KEY", "sk-fake")
            .output()
            .is_ok_and(|output| {
                serde_json::from_slice::<Value>(&output.stdout)
                    .is_ok_and(|status| status["gateway"]["error"].is_null())
            })
    });

    // P.
    let mut phone = Phone::spawn(&topology.ns("p"), &dir);
    phone.ok(&format!("seed {}", record.display()));
    phone.ok("hold_rotation on");
    phone.ok("network eth0 wifi");
    phone.ok("preconnect");

    // 1. The first chat leg went over the relay.
    let legs = gateway_legs(&a, &config);
    assert!(
        legs.contains(&("chat".to_owned(), "relay".to_owned())),
        "{}: the first chat leg rides the relay: {legs:?}",
        cell.name
    );

    // 2. The final carrier, and one API request on it.
    let waited = phone.command(&format!("wait_carrier {CARRIER_WAIT_SECS}"));
    let carrier = waited["status"]["carrier"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert_eq!(
        carrier, cell.expected,
        "{} run {run_index}: {waited}",
        cell.name
    );
    if cell.expected == "relay" {
        return;
    }
    phone.ok("api");
    let legs = gateway_legs(&a, &config);
    assert!(
        legs.iter()
            .any(|(class, kind)| class == "api" && kind != "relay"),
        "{}: an API leg rides the carrier: {legs:?}",
        cell.name
    );

    // 3. Idle with rotation held, so only QUIC keepalives cross the NATs.
    std::thread::sleep(idle);
    let status = phone.ok("status");
    assert_eq!(
        status["status"]["carrier"], cell.expected,
        "{}: the carrier survived the idle phase: {status}",
        cell.name
    );
    phone.ok("api");
    let legs = gateway_legs(&a, &config);
    assert!(
        !legs
            .iter()
            .any(|(class, kind)| class == "api" && kind == "relay"),
        "{}: no API leg fell back to the relay: {legs:?}",
        cell.name
    );

    // 4. Released, the chat rotates and a chat frame round-trips on it.
    phone.ok("hold_rotation off");
    until("the chat leg rotates", ROTATION_WAIT, || {
        gateway_legs(&a, &config)
            .iter()
            .any(|(class, kind)| class == "chat" && kind != "relay")
    });
    phone.ok(&format!("create {SESSION}"));
    phone.ok(&format!("chat {SESSION}"));
    let legs = gateway_legs(&a, &config);
    assert!(
        !legs
            .iter()
            .any(|(class, kind)| class == "chat" && kind == "relay"),
        "{}: the relay chat leg is gone: {legs:?}",
        cell.name
    );
}

fn tempfile_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("netns-matrix-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("run dir");
    dir
}

/// A fresh 32-byte vault key, hex-encoded as `baybo setup` writes it.
fn hex32() -> String {
    let mut bytes = [0u8; 32];
    let mut urandom = std::fs::File::open("/dev/urandom").expect("urandom");
    std::io::Read::read_exact(&mut urandom, &mut bytes).expect("urandom");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
#[ignore = "needs root, ip, iptables, tc and sqlite3; run scripts/netns-matrix.sh"]
fn direct_carrier_nat_matrix() {
    let selected: Option<Vec<String>> = std::env::var("NETNS_CELLS")
        .ok()
        .map(|cells| cells.split_whitespace().map(str::to_owned).collect());
    let runs = env_number("NETNS_RUNS", 5);
    let idle = Duration::from_secs(env_number("NETNS_IDLE_SECS", 40));
    let cells = cells();
    if let Some(selected) = &selected {
        for name in selected {
            assert!(
                cells.iter().any(|cell| cell.name == *name),
                "unknown NETNS_CELLS entry: {name}"
            );
        }
    }
    let mut ran = 0;
    for cell in cells {
        if selected
            .as_ref()
            .is_some_and(|selected| !selected.contains(&cell.name))
        {
            continue;
        }
        for run_index in 0..runs {
            eprintln!("netns-matrix: {} run {run_index}", cell.name);
            run_cell(&cell, run_index, idle);
            ran += 1;
        }
    }
    assert!(ran > 0, "NETNS_CELLS selected no cell");
}
