//! The one outbound dial path for every relay WebSocket the gateway opens —
//! the control connection, content / API / blob data legs, and the pairing host
//! leg.
//!
//! The WebSocket upgrade rides a `reqwest` client, so relay dials share the
//! egress policy of every other outbound HTTP call: the operator's `proxy`
//! block (else the ambient `HTTPS_PROXY` / `ALL_PROXY`) is used for HTTP
//! `CONNECT` and SOCKS5 tunnels, and `socks5h://` resolves the relay host at the
//! proxy. Certificates are checked against the system trust store, the same way
//! the LLM calls check them. Once a proxy applies, a failed dial is an error; it
//! never falls back to a direct connection.

use std::sync::Arc;
use std::time::Duration;

use baybo_security::http::ProxySettings;
use hyper_util::client::proxy::matcher::Matcher;
use reqwest::header::{
    CONNECTION, HeaderValue, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_VERSION,
    UPGRADE,
};
use reqwest::{StatusCode, Upgraded, Url};
use thiserror::Error;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::client::generate_key;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;

use remote_host_protocol::REMOTE_API_KEY_HEADER;

/// An established relay WebSocket, whatever path (direct / proxied) it took.
pub(crate) type RelayWs = WebSocketStream<Upgraded>;

/// TCP connect budget — to the relay, or to the proxy when one applies.
const RELAY_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Budget for the whole dial: connect, proxy tunnel, TLS, and the `101` upgrade.
const RELAY_DIAL_TIMEOUT: Duration = Duration::from_secs(30);

/// Cap on how much of a rejected upgrade's body is read for the error message.
const REJECT_BODY_READ_MAX: usize = 4 * 1024;

const WEBSOCKET_VERSION: &str = "13";

/// Why a relay dial failed. Every string here has proxy credentials scrubbed
/// and never carries the dialed URL (its path holds the relay leg key).
#[derive(Debug, Clone, Error)]
pub enum RelayDialError {
    /// The dialer couldn't be built (a malformed proxy URL, an unreadable system
    /// trust store), so no relay dial can succeed until that is fixed.
    #[error("relay dialer unavailable: {0}")]
    Unavailable(String),
    #[error("bad relay url: {0}")]
    BadUrl(String),
    #[error("bad instance key header")]
    BadApiKey,
    /// Connect, proxy tunnel, or TLS failed. `via` names the egress route.
    #[error("connect via {via}: {reason}")]
    Connect { via: String, reason: String },
    #[error("relay dial timed out after {0:?}")]
    Timeout(Duration),
    /// The relay answered the upgrade with a non-`101` status — its admission
    /// verdict (e.g. a 401/403 for an unadmitted key) is in the body.
    #[error("http {status}{}", fmt_body(.body))]
    Rejected { status: StatusCode, body: String },
    #[error("websocket handshake: {0}")]
    Handshake(String),
}

fn fmt_body(body: &str) -> String {
    if body.is_empty() {
        String::new()
    } else {
        format!(": {body}")
    }
}

/// Where relay dials go out. Cheap to clone; build once per process.
#[derive(Clone)]
pub struct RelayDialer {
    inner: Arc<DialerInner>,
}

struct DialerInner {
    client: Result<reqwest::Client, String>,
    egress: Egress,
}

enum Egress {
    Direct,
    Proxy {
        /// The proxy URL with credentials redacted.
        display: String,
        /// Credential fragments to scrub from any error text before it leaves.
        secrets: Vec<String>,
        /// The same matcher reqwest builds from these settings, so the dialer's
        /// idea of which targets are proxied can't drift from reqwest's.
        matcher: Box<Matcher>,
        /// An `http` / `https` proxy forwards a plain-http request (absolute-form
        /// `GET`, path and headers in the clear) instead of tunnelling it; SOCKS
        /// tunnels everything.
        forwards_plain_http: bool,
    },
}

/// How one dial leaves the host.
enum Route<'a> {
    Direct,
    /// A proxy is configured but the target matches its no-proxy list.
    Bypassed,
    Proxied(&'a str),
}

impl Route<'_> {
    fn describe(&self) -> String {
        match self {
            Self::Direct => "direct".to_string(),
            Self::Bypassed => "direct (no_proxy)".to_string(),
            Self::Proxied(display) => format!("proxy {display}"),
        }
    }
}

impl Egress {
    fn of(proxy: Option<&ProxySettings>) -> Self {
        let Some(p) = proxy else {
            return Self::Direct;
        };
        // reqwest reads a schemeless proxy URL as `http://`.
        let with_scheme = if p.url.contains("://") {
            p.url.clone()
        } else {
            format!("http://{}", p.url)
        };
        let mut secrets = Vec::new();
        if let Ok(url) = Url::parse(&with_scheme)
            && let Some(password) = url.password().filter(|s| !s.is_empty())
        {
            secrets.push(password.to_string());
            let decoded = percent_decode(password);
            if decoded != password {
                secrets.push(decoded);
            }
        }
        let scheme = with_scheme
            .split_once("://")
            .map(|(s, _)| s.to_ascii_lowercase())
            .unwrap_or_default();
        Self::Proxy {
            display: p.display_url(),
            secrets,
            matcher: Box::new(
                Matcher::builder()
                    .all(p.url.clone())
                    .no(p.no_proxy_list())
                    .build(),
            ),
            forwards_plain_http: matches!(scheme.as_str(), "http" | "https"),
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Direct => Route::Direct.describe(),
            Self::Proxy { display, .. } => Route::Proxied(display).describe(),
        }
    }

    fn route(&self, target: &Url) -> Route<'_> {
        let Self::Proxy {
            display, matcher, ..
        } = self
        else {
            return Route::Direct;
        };
        // An unparseable target is assumed proxied — the conservative answer
        // for the plain-http guard.
        match target.as_str().parse::<hyper::Uri>() {
            Ok(uri) if matcher.intercept(&uri).is_none() => Route::Bypassed,
            _ => Route::Proxied(display),
        }
    }

    fn scrub(&self, text: String) -> String {
        match self {
            Self::Direct => text,
            Self::Proxy { secrets, .. } => secrets
                .iter()
                .fold(text, |acc, s| acc.replace(s.as_str(), "[REDACTED]")),
        }
    }
}

impl RelayDialer {
    /// The production dialer: the operator's `proxy` block when set, else the
    /// ambient `HTTPS_PROXY` / `ALL_PROXY` (see [`ProxySettings::from_env`]),
    /// else direct. Never fails — a build error is kept and returned by
    /// [`Self::ready`] and every [`Self::dial`], so a broken proxy setting
    /// disables the relay without stopping the rest of the gateway.
    pub fn new(configured: Option<ProxySettings>) -> Self {
        let proxy = configured.or_else(|| ProxySettings::from_env(|k| std::env::var(k).ok()));
        Self::build(proxy, Vec::new())
    }

    /// A dialer that always connects directly and ignores the environment.
    #[cfg(any(test, feature = "test-support"))]
    pub fn direct() -> Self {
        Self::build(None, Vec::new())
    }

    /// A dialer with an explicit proxy (no env) that also trusts `extra_roots`
    /// on top of the system store.
    #[cfg(test)]
    fn with_roots(proxy: Option<ProxySettings>, extra_roots: Vec<reqwest::Certificate>) -> Self {
        Self::build(proxy, extra_roots)
    }

    fn build(proxy: Option<ProxySettings>, extra_roots: Vec<reqwest::Certificate>) -> Self {
        let egress = Egress::of(proxy.as_ref());
        let client = match proxy.as_ref().map(ProxySettings::to_proxy) {
            // reqwest's own message for a bad proxy URL embeds the raw URL,
            // credentials included, so it is never rendered.
            Some(Err(_)) => Err(format!(
                "invalid proxy url {}",
                proxy
                    .as_ref()
                    .map(ProxySettings::display_url)
                    .unwrap_or_default()
            )),
            _ => build_client(proxy.as_ref(), extra_roots)
                .map_err(|e| egress.scrub(error_chain(&e.without_url()))),
        };
        Self {
            inner: Arc::new(DialerInner { client, egress }),
        }
    }

    /// `Err` when the dialer couldn't be built, so a caller can fail fast before
    /// promising the user a relay connection.
    pub fn ready(&self) -> Result<(), RelayDialError> {
        self.client().map(|_| ())
    }

    /// The egress route relay dials take, safe to log: `direct`, or
    /// `proxy <url>` with credentials redacted.
    pub fn describe(&self) -> String {
        self.inner.egress.describe()
    }

    fn client(&self) -> Result<&reqwest::Client, RelayDialError> {
        self.inner
            .client
            .as_ref()
            .map_err(|e| RelayDialError::Unavailable(e.clone()))
    }

    /// Open a WebSocket to `url` (`ws://` / `wss://`), presenting
    /// `remote_api_key` as the relay admission header.
    pub(crate) async fn dial(
        &self,
        url: &str,
        remote_api_key: &str,
    ) -> Result<RelayWs, RelayDialError> {
        let client = self.client()?;
        let target = http_target(url)?;
        let route = self.inner.egress.route(&target);
        if matches!(route, Route::Proxied(_))
            && target.scheme() == "http"
            && matches!(
                self.inner.egress,
                Egress::Proxy {
                    forwards_plain_http: true,
                    ..
                }
            )
        {
            // The proxy would receive the leg key (in the path) and the
            // admission key in the clear.
            return Err(RelayDialError::BadUrl(
                "a ws:// relay can't be reached through an http(s) egress proxy; use wss://, \
                 a SOCKS proxy, or list the relay host in no_proxy"
                    .into(),
            ));
        }
        let api_key =
            HeaderValue::from_str(remote_api_key).map_err(|_| RelayDialError::BadApiKey)?;
        let key = generate_key();
        let request = client
            .get(target)
            .header(CONNECTION, "Upgrade")
            .header(UPGRADE, "websocket")
            .header(SEC_WEBSOCKET_VERSION, WEBSOCKET_VERSION)
            .header(SEC_WEBSOCKET_KEY, &key)
            .header(REMOTE_API_KEY_HEADER, api_key);

        let handshake = async {
            let resp = request
                .send()
                .await
                .map_err(|e| self.connect_error(e, &route))?;
            let status = resp.status();
            if status != StatusCode::SWITCHING_PROTOCOLS {
                return Err(RelayDialError::Rejected {
                    status,
                    body: read_reject_body(resp).await,
                });
            }
            let expected = derive_accept_key(key.as_bytes());
            let accepted = resp
                .headers()
                .get(SEC_WEBSOCKET_ACCEPT)
                .is_some_and(|v| v.as_bytes() == expected.as_bytes());
            if !accepted {
                return Err(RelayDialError::Handshake(
                    "missing or wrong Sec-WebSocket-Accept".into(),
                ));
            }
            let upgraded = resp.upgrade().await.map_err(|e| {
                RelayDialError::Handshake(self.inner.egress.scrub(error_chain(&e.without_url())))
            })?;
            Ok(WebSocketStream::from_raw_socket(upgraded, Role::Client, None).await)
        };
        tokio::time::timeout(RELAY_DIAL_TIMEOUT, handshake)
            .await
            .map_err(|_| RelayDialError::Timeout(RELAY_DIAL_TIMEOUT))?
    }

    fn connect_error(&self, e: reqwest::Error, route: &Route<'_>) -> RelayDialError {
        let mut reason = self.inner.egress.scrub(error_chain(&e.without_url()));
        if reason.contains("UnknownIssuer") {
            reason.push_str(
                " (the relay's certificate chain isn't trusted by the system CA store; \
                 check /etc/ssl/certs or SSL_CERT_FILE / SSL_CERT_DIR)",
            );
        }
        RelayDialError::Connect {
            via: route.describe(),
            reason,
        }
    }
}

fn build_client(
    proxy: Option<&ProxySettings>,
    extra_roots: Vec<reqwest::Certificate>,
) -> reqwest::Result<reqwest::Client> {
    let builder = match proxy {
        Some(p) => baybo_security::http::client_builder(Some(p))?,
        // The env was already consulted by `RelayDialer::new`; don't let reqwest
        // pick up a different variable on its own.
        None => baybo_security::http::client_builder(None)?.no_proxy(),
    };
    builder
        // The upgrade is HTTP/1.1-only; ALPN must not negotiate h2.
        .http1_only()
        .connect_timeout(RELAY_CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .pool_max_idle_per_host(0)
        .tls_certs_merge(extra_roots)
        .build()
}

/// Map a `ws://` / `wss://` URL to the `http://` / `https://` form reqwest dials.
/// Errors never echo the URL: its path carries the relay leg key.
fn http_target(url: &str) -> Result<Url, RelayDialError> {
    let mut target =
        Url::parse(url).map_err(|e| RelayDialError::BadUrl(format!("unparseable: {e}")))?;
    let scheme = match target.scheme() {
        "wss" => "https",
        "ws" => "http",
        other => {
            return Err(RelayDialError::BadUrl(format!(
                "scheme {other}:// is not ws:// or wss://"
            )));
        }
    };
    target
        .set_scheme(scheme)
        .map_err(|()| RelayDialError::BadUrl("cannot map to an http(s) url".into()))?;
    Ok(target)
}

/// The error and its sources joined with `: ` — reqwest's own `Display` stops
/// at "error sending request", which hides the actual TLS / proxy cause.
fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        let text = s.to_string();
        if !out.ends_with(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        source = s.source();
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn read_reject_body(mut resp: reqwest::Response) -> String {
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = resp.chunk().await {
        body.extend_from_slice(&chunk);
        if body.len() >= REJECT_BODY_READ_MAX {
            body.truncate(REJECT_BODY_READ_MAX);
            break;
        }
    }
    crate::http_body_snippet(&String::from_utf8_lossy(&body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use parking_lot::Mutex;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::handshake::server::{
        Callback, ErrorResponse, Request, Response,
    };

    const TEST_CA: &[u8] = include_bytes!("testdata/test-ca.pem");
    const LEAF_CERT: &[u8] = include_bytes!("testdata/relay-test-cert.pem");
    const LEAF_KEY: &[u8] = include_bytes!("testdata/relay-test-key.pem");
    const API_KEY: &str = "inst-A";
    const PROXY_PASSWORD: &str = "s3cret";

    fn test_ca() -> Vec<reqwest::Certificate> {
        vec![reqwest::Certificate::from_pem(TEST_CA).unwrap()]
    }

    fn proxy(url: String) -> Option<ProxySettings> {
        Some(ProxySettings {
            url,
            no_proxy: None,
        })
    }

    fn proxy_bypassing(url: String, host: &str) -> Option<ProxySettings> {
        Some(ProxySettings {
            url,
            no_proxy: Some(vec![host.to_string()]),
        })
    }

    /// A WS echo server on a loopback alias outside the always-direct list, so
    /// it stands in for a LAN relay the proxy matcher doesn't exempt.
    const LAN_HOST: &str = "127.0.0.2";

    /// A port nothing listens on.
    async fn dead_port() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    }

    fn tls_acceptor() -> tokio_rustls::TlsAcceptor {
        use rustls::pki_types::pem::PemObject;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer};
        let certs = CertificateDer::pem_slice_iter(LEAF_CERT)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let key = PrivateKeyDer::from_pem_slice(LEAF_KEY).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
        tokio_rustls::TlsAcceptor::from(Arc::new(config))
    }

    /// Request headers the WS server saw, one entry per upgrade.
    type SeenHeaders = Arc<Mutex<Vec<reqwest::header::HeaderMap>>>;

    /// A WS echo server on loopback, TLS (`relay.test` cert) when `tls`.
    async fn ws_server(tls: bool) -> (u16, SeenHeaders) {
        ws_server_on("127.0.0.1", tls).await
    }

    async fn ws_server_on(host: &str, tls: bool) -> (u16, SeenHeaders) {
        let listener = TcpListener::bind((host, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = SeenHeaders::default();
        let acceptor = tls.then(tls_acceptor);
        let seen_in = Arc::clone(&seen);
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let seen = Arc::clone(&seen_in);
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    match acceptor {
                        Some(a) => {
                            if let Ok(stream) = a.accept(tcp).await {
                                echo_ws(stream, seen).await;
                            }
                        }
                        None => echo_ws(tcp, seen).await,
                    }
                });
            }
        });
        (port, seen)
    }

    struct RecordHeaders(SeenHeaders);

    impl Callback for RecordHeaders {
        fn on_request(self, req: &Request, resp: Response) -> Result<Response, ErrorResponse> {
            self.0.lock().push(req.headers().clone());
            Ok(resp)
        }
    }

    async fn echo_ws<S: AsyncRead + AsyncWrite + Unpin>(stream: S, seen: SeenHeaders) {
        let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(stream, RecordHeaders(seen)).await
        else {
            return;
        };
        while let Some(Ok(msg)) = ws.next().await {
            if msg.is_binary() && ws.send(msg).await.is_err() {
                break;
            }
        }
    }

    async fn read_head(tcp: &mut TcpStream) -> String {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if tcp.read(&mut byte).await.unwrap_or(0) == 0 {
                break;
            }
            head.push(byte[0]);
        }
        String::from_utf8_lossy(&head).into_owned()
    }

    /// An HTTP CONNECT proxy that records each request head and tunnels every
    /// target — `relay.test` is not resolvable here — to `127.0.0.1:{port}`.
    async fn connect_proxy() -> (u16, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let heads = Arc::new(Mutex::new(Vec::new()));
        let heads_in = Arc::clone(&heads);
        tokio::spawn(async move {
            while let Ok((mut client, _)) = listener.accept().await {
                let heads = Arc::clone(&heads_in);
                tokio::spawn(async move {
                    let head = read_head(&mut client).await;
                    heads.lock().push(head.clone());
                    let target_port = head
                        .split_whitespace()
                        .nth(1)
                        .and_then(|t| t.rsplit_once(':'))
                        .and_then(|(_, p)| p.parse::<u16>().ok());
                    let Some(target_port) = target_port else {
                        return;
                    };
                    let Ok(mut upstream) = TcpStream::connect(("127.0.0.1", target_port)).await
                    else {
                        return;
                    };
                    let _ = client
                        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                        .await;
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                });
            }
        });
        (port, heads)
    }

    /// A no-auth SOCKS5 proxy recording `(address type, host)` per request and
    /// tunnelling to the requested port.
    async fn socks5_proxy() -> (u16, Arc<Mutex<Vec<(u8, String)>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_in = Arc::clone(&seen);
        tokio::spawn(async move {
            while let Ok((mut client, _)) = listener.accept().await {
                let seen = Arc::clone(&seen_in);
                tokio::spawn(async move {
                    let mut greet = [0u8; 2];
                    client.read_exact(&mut greet).await.unwrap();
                    let mut methods = vec![0u8; greet[1] as usize];
                    client.read_exact(&mut methods).await.unwrap();
                    client.write_all(&[5, 0]).await.unwrap();
                    let mut req = [0u8; 4];
                    client.read_exact(&mut req).await.unwrap();
                    let atyp = req[3];
                    let host = match atyp {
                        3 => {
                            let len = client.read_u8().await.unwrap();
                            let mut name = vec![0u8; len as usize];
                            client.read_exact(&mut name).await.unwrap();
                            String::from_utf8_lossy(&name).into_owned()
                        }
                        1 => {
                            let mut ip = [0u8; 4];
                            client.read_exact(&mut ip).await.unwrap();
                            std::net::Ipv4Addr::from(ip).to_string()
                        }
                        _ => return,
                    };
                    let target_port = client.read_u16().await.unwrap();
                    // Names (`relay.test`) map to loopback; IPs are dialed as given.
                    let upstream_host = if atyp == 1 {
                        host.clone()
                    } else {
                        "127.0.0.1".to_string()
                    };
                    seen.lock().push((atyp, host));
                    let mut upstream = TcpStream::connect((upstream_host.as_str(), target_port))
                        .await
                        .unwrap();
                    client
                        .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])
                        .await
                        .unwrap();
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                });
            }
        });
        (port, seen)
    }

    async fn assert_echo(ws: &mut RelayWs) {
        ws.send(Message::Binary(b"ping".to_vec())).await.unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("echo timed out")
            .expect("stream open")
            .expect("frame");
        assert_eq!(reply, Message::Binary(b"ping".to_vec()));
    }

    #[tokio::test]
    async fn direct_ws_dial_presents_the_admission_key() {
        let (port, seen) = ws_server(false).await;
        let mut ws = RelayDialer::direct()
            .dial(&format!("ws://127.0.0.1:{port}/control"), API_KEY)
            .await
            .expect("dial");
        assert_echo(&mut ws).await;
        let headers = seen.lock()[0].clone();
        assert_eq!(headers[REMOTE_API_KEY_HEADER], API_KEY);
    }

    /// The reported setup: no usable DNS, egress only via an authenticated HTTP
    /// proxy, TLS verified against a CA the Mozilla bundle doesn't carry.
    #[tokio::test]
    async fn wss_tunnels_through_http_connect_with_remote_resolution() {
        let (relay_port, seen) = ws_server(true).await;
        let (proxy_port, heads) = connect_proxy().await;
        let dialer = RelayDialer::with_roots(
            proxy(format!(
                "http://alice:{PROXY_PASSWORD}@127.0.0.1:{proxy_port}"
            )),
            test_ca(),
        );
        let mut ws = dialer
            .dial(
                &format!("wss://relay.test:{relay_port}/content/host/k"),
                API_KEY,
            )
            .await
            .expect("dial through CONNECT");
        assert_echo(&mut ws).await;

        let head = heads.lock()[0].clone();
        assert!(
            head.starts_with(&format!("CONNECT relay.test:{relay_port} ")),
            "proxy must resolve the relay host: {head}"
        );
        assert!(
            head.to_ascii_lowercase()
                .contains("proxy-authorization: basic "),
            "credentials go to the proxy: {head}"
        );
        let relay_headers = seen.lock()[0].clone();
        assert!(relay_headers.get("proxy-authorization").is_none());
        assert_eq!(relay_headers[REMOTE_API_KEY_HEADER], API_KEY);
    }

    #[tokio::test]
    async fn wss_tunnels_through_socks5h_with_remote_resolution() {
        let (relay_port, _) = ws_server(true).await;
        let (proxy_port, seen) = socks5_proxy().await;
        let dialer = RelayDialer::with_roots(
            proxy(format!("socks5h://127.0.0.1:{proxy_port}")),
            test_ca(),
        );
        let mut ws = dialer
            .dial(&format!("wss://relay.test:{relay_port}/control"), API_KEY)
            .await
            .expect("dial through socks5h");
        assert_echo(&mut ws).await;
        assert_eq!(seen.lock()[0], (3, "relay.test".to_string()));
    }

    #[tokio::test]
    async fn untrusted_chain_is_rejected() {
        let (relay_port, seen) = ws_server(true).await;
        let (proxy_port, _) = connect_proxy().await;
        let dialer =
            RelayDialer::with_roots(proxy(format!("http://127.0.0.1:{proxy_port}")), Vec::new());
        let err = dialer
            .dial(&format!("wss://relay.test:{relay_port}/control"), API_KEY)
            .await
            .expect_err("test CA is not in the system store");
        let text = err.to_string();
        assert!(matches!(err, RelayDialError::Connect { .. }), "{text}");
        assert!(text.contains("UnknownIssuer"), "{text}");
        assert!(
            seen.lock().is_empty(),
            "no upgrade over an unverified chain"
        );
    }

    #[tokio::test]
    async fn certificate_for_another_host_is_rejected() {
        let (relay_port, _) = ws_server(true).await;
        let (proxy_port, _) = connect_proxy().await;
        let dialer =
            RelayDialer::with_roots(proxy(format!("http://127.0.0.1:{proxy_port}")), test_ca());
        let err = dialer
            .dial(&format!("wss://other.test:{relay_port}/control"), API_KEY)
            .await
            .expect_err("cert names relay.test only");
        assert!(err.to_string().contains("not valid for name"), "{err}");
    }

    #[tokio::test]
    async fn dead_proxy_fails_closed_without_leaking_credentials() {
        let proxy_port = dead_port().await;
        let dialer = RelayDialer::with_roots(
            proxy(format!(
                "http://alice:{PROXY_PASSWORD}@127.0.0.1:{proxy_port}"
            )),
            test_ca(),
        );
        let err = dialer
            .dial("wss://relay.test/control", API_KEY)
            .await
            .expect_err("proxy is down");
        let text = err.to_string();
        let RelayDialError::Connect { via, .. } = &err else {
            panic!("expected a connect error, got {text}");
        };
        assert_eq!(
            via,
            &format!("proxy http://[REDACTED]@127.0.0.1:{proxy_port}")
        );
        assert!(!text.contains(PROXY_PASSWORD), "{text}");
    }

    #[tokio::test]
    async fn loopback_relay_bypasses_the_proxy() {
        let (port, _) = ws_server(false).await;
        let dialer = RelayDialer::with_roots(
            proxy(format!("http://127.0.0.1:{}", dead_port().await)),
            Vec::new(),
        );
        let mut ws = dialer
            .dial(&format!("ws://127.0.0.1:{port}/control"), API_KEY)
            .await
            .expect("loopback stays direct");
        assert_echo(&mut ws).await;
    }

    #[tokio::test]
    async fn plain_ws_to_a_remote_relay_is_refused_while_proxied() {
        let dialer = RelayDialer::with_roots(
            proxy(format!("http://127.0.0.1:{}", dead_port().await)),
            Vec::new(),
        );
        let err = dialer
            .dial("ws://relay.test/control", API_KEY)
            .await
            .expect_err("ws:// can't be tunnelled");
        assert!(matches!(err, RelayDialError::BadUrl(_)), "{err}");
        // A loopback alias outside the always-direct list would be proxied too,
        // so it is refused rather than forwarded with the keys in the clear.
        let err = dialer
            .dial(&format!("ws://{LAN_HOST}:1/control"), API_KEY)
            .await
            .expect_err("127.0.0.2 is not exempt");
        assert!(matches!(err, RelayDialError::BadUrl(_)), "{err}");
    }

    #[tokio::test]
    async fn plain_ws_relay_listed_in_no_proxy_goes_direct() {
        let (port, _) = ws_server_on(LAN_HOST, false).await;
        let dialer = RelayDialer::with_roots(
            proxy_bypassing(format!("http://127.0.0.1:{}", dead_port().await), LAN_HOST),
            Vec::new(),
        );
        let mut ws = dialer
            .dial(&format!("ws://{LAN_HOST}:{port}/control"), API_KEY)
            .await
            .expect("no_proxy host dials direct");
        assert_echo(&mut ws).await;
    }

    #[tokio::test]
    async fn plain_ws_tunnels_through_socks() {
        let (relay_port, _) = ws_server_on(LAN_HOST, false).await;
        let (proxy_port, seen) = socks5_proxy().await;
        let dialer = RelayDialer::with_roots(
            proxy(format!("socks5://127.0.0.1:{proxy_port}")),
            Vec::new(),
        );
        let mut ws = dialer
            .dial(&format!("ws://{LAN_HOST}:{relay_port}/control"), API_KEY)
            .await
            .expect("SOCKS tunnels plain http");
        assert_echo(&mut ws).await;
        assert_eq!(seen.lock()[0], (1, LAN_HOST.to_string()));
    }

    #[tokio::test]
    async fn bypassed_target_errors_name_the_direct_route() {
        let port = dead_port().await;
        let dialer = RelayDialer::with_roots(
            proxy_bypassing(format!("http://127.0.0.1:{}", dead_port().await), LAN_HOST),
            Vec::new(),
        );
        let err = dialer
            .dial(&format!("wss://{LAN_HOST}:{port}/control"), API_KEY)
            .await
            .expect_err("nothing listens");
        let RelayDialError::Connect { via, .. } = &err else {
            panic!("expected a connect error, got {err}");
        };
        assert_eq!(via, "direct (no_proxy)");
    }

    #[tokio::test]
    async fn malformed_proxy_leaves_the_dialer_unavailable() {
        for url in [
            format!("http://alice:{PROXY_PASSWORD}@[bad"),
            // Schemeless: reqwest's own error would echo this verbatim.
            format!("alice:{PROXY_PASSWORD}@proxy.corp:99999"),
        ] {
            let err = RelayDialer::with_roots(proxy(url), Vec::new())
                .ready()
                .expect_err("unparseable proxy url");
            assert!(matches!(err, RelayDialError::Unavailable(_)), "{err}");
            assert!(!err.to_string().contains(PROXY_PASSWORD), "{err}");
        }
        let dialer = RelayDialer::with_roots(
            proxy(format!("http://alice:{PROXY_PASSWORD}@[bad")),
            Vec::new(),
        );
        let err = dialer
            .dial("wss://relay.test/control", API_KEY)
            .await
            .expect_err("no dial without a dialer");
        assert!(matches!(err, RelayDialError::Unavailable(_)), "{err}");
    }

    #[tokio::test]
    async fn rejected_upgrade_surfaces_status_and_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut tcp, _) = listener.accept().await.unwrap();
            read_head(&mut tcp).await;
            let _ = tcp
                .write_all(
                    b"HTTP/1.1 403 Forbidden\r\ncontent-length: 14\r\nconnection: close\r\n\r\nunadmitted key",
                )
                .await;
        });
        let err = RelayDialer::direct()
            .dial(&format!("ws://127.0.0.1:{port}/control"), API_KEY)
            .await
            .expect_err("403");
        assert!(
            matches!(&err, RelayDialError::Rejected { status, body }
                if *status == StatusCode::FORBIDDEN && body == "unadmitted key"),
            "{err}"
        );
        assert_eq!(err.to_string(), "http 403 Forbidden: unadmitted key");
    }

    #[test]
    fn non_websocket_schemes_are_bad_urls() {
        assert!(matches!(
            http_target("https://relay.test/control"),
            Err(RelayDialError::BadUrl(_))
        ));
        assert_eq!(
            http_target("wss://relay.test/control").unwrap().as_str(),
            "https://relay.test/control"
        );
    }

    #[test]
    fn percent_encoded_passwords_are_scrubbed_in_both_forms() {
        let egress = Egress::of(proxy("http://u:p%40ss@h:1".into()).as_ref());
        assert_eq!(
            egress.scrub("a p%40ss b p@ss".into()),
            "a [REDACTED] b [REDACTED]"
        );
    }
}
