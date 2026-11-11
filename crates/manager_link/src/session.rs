//! One connection to Manager: TCP, TLS with the pin, WebSocket upgrade, the
//! mutual handshake (contract 3), then the request loop (contract 4).
//!
//! Key mode always runs pinned to the saved fingerprint. Pair mode runs
//! pinned too when a fingerprint is saved; on a first pairing it accepts
//! whatever certificate is served, which is safe because the proof is keyed
//! by P (the web password and the one-time pairing code) and covers the
//! fingerprint seen, so a server with another certificate can neither
//! complete nor relay the handshake.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

use crate::handshake::{self, is_hex64, Secret};
use crate::serve::Server;
use crate::store;

/// Everything up to and including Manager's `ok` must finish in this time.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Ping and status-push interval.
pub const PING_INTERVAL: Duration = Duration::from_secs(30);
/// How often the car's readings are checked; a change is pushed at once,
/// so this is also the most often a car change sends status.
pub const CAR_CHECK_INTERVAL: Duration = Duration::from_secs(15);
/// A connection silent this long is closed.
pub const SILENCE_TIMEOUT: Duration = Duration::from_secs(90);
/// A single outgoing frame that can't be written in this time ends the link.
const SEND_TIMEOUT: Duration = Duration::from_secs(60);
/// Largest frame accepted before the handshake completes.
pub const MAX_HANDSHAKE_FRAME: usize = 8 * 1024;
/// Largest frame or message accepted at all (checked while reading, before
/// any parsing).
pub const MAX_FRAME: usize = 2 * 1024 * 1024;
pub const PATH: &str = "/api/sentryusb/link";
/// WebSocket close code for "unpaired", sent by either side.
pub const UNPAIRED_CLOSE: u16 = 4001;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Pair,
    Key,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Pair => "pair",
            Mode::Key => "key",
        }
    }
}

/// One connection attempt.
pub struct Attempt {
    pub host: String,
    pub port: u16,
    pub mode: Mode,
    /// P in pair mode, the link key in key mode.
    pub secret: Secret,
    /// The pinned fingerprint saved at pairing: any other certificate fails
    /// TLS before a proof is sent. Required in key mode. `None` only for a
    /// first pairing, which accepts any certificate.
    pub pin: Option<String>,
    pub device_id: String,
    pub name: String,
    pub version: String,
}

#[derive(Debug, PartialEq)]
pub enum LinkError {
    /// Manager refused us (`denied`); the reason it gave.
    Denied(String),
    /// Manager speaks another protocol version: the `v` its hello carried,
    /// or `None` when it refused ours with `denied`/`version`.
    Version(Option<u64>),
    /// The certificate didn't match the pin (or Manager's own claim); the
    /// fingerprint seen.
    CertMismatch(String),
    /// The handshake didn't finish in time.
    Timeout,
    /// Manager's proof was wrong: it doesn't hold the key. The fingerprint
    /// of the server that sent it.
    BadProof(String),
    Protocol(String),
    Io(String),
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkError::Denied(why) => write!(f, "Sentry Manager refused the connection ({why})"),
            LinkError::Version(Some(v)) if *v < handshake::VERSION => {
                f.write_str("Sentry Manager needs updating: update Sentry Manager to the newest version")
            }
            LinkError::Version(_) => {
                f.write_str("Sentry Manager uses another link version: update Sentry USB to test.30 or later")
            }
            LinkError::CertMismatch(_) => f.write_str("the certificate presented isn't the expected Sentry Manager certificate"),
            LinkError::Timeout => f.write_str("Sentry Manager didn't answer in time"),
            LinkError::BadProof(_) => f.write_str("Sentry Manager couldn't prove it holds the key"),
            LinkError::Protocol(m) => write!(f, "unexpected message from Sentry Manager: {m}"),
            LinkError::Io(m) => f.write_str(m),
        }
    }
}

/// What a finished handshake hands back.
pub struct Established {
    /// Fingerprint seen in this TLS session.
    pub fp: String,
    /// The new link key (pair mode only).
    pub link_key: Option<Secret>,
    /// The Pi's own address on this connection.
    pub local_ip: Option<String>,
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<TcpStream>>;

fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME))
        .max_frame_size(Some(MAX_FRAME))
        .max_write_buffer_size(16 * 1024 * 1024)
}

fn io(e: impl std::fmt::Display) -> LinkError {
    LinkError::Io(e.to_string())
}

/// Reads one handshake frame: text, at most `MAX_HANDSHAKE_FRAME` bytes, a
/// JSON object. Anything else ends the attempt.
async fn read_handshake_frame(ws: &mut Ws) -> Result<serde_json::Map<String, Value>, LinkError> {
    loop {
        let msg = ws.next().await.ok_or_else(|| io("connection closed"))?.map_err(io)?;
        let text = match msg {
            Message::Text(t) => t,
            // Control frames are fine; tungstenite answers pings itself.
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => return Err(io("connection closed")),
            _ => return Err(LinkError::Protocol("non-text frame during handshake".into())),
        };
        if text.len() > MAX_HANDSHAKE_FRAME {
            return Err(LinkError::Protocol("oversized handshake frame".into()));
        }
        return match serde_json::from_str::<Value>(text.as_str()) {
            Ok(Value::Object(m)) => Ok(m),
            _ => Err(LinkError::Protocol("handshake frame is not a JSON object".into())),
        };
    }
}

async fn send_json(ws: &mut Ws, v: Value) -> Result<(), LinkError> {
    ws.send(Message::text(v.to_string())).await.map_err(io)
}

/// Connects and runs the handshake. On success the socket is ready for the
/// request loop.
pub async fn connect(a: &Attempt) -> Result<(Ws, Established), LinkError> {
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, connect_inner(a)).await {
        Ok(r) => r,
        Err(_) => Err(LinkError::Timeout),
    }
}

async fn connect_inner(a: &Attempt) -> Result<(Ws, Established), LinkError> {
    let tcp = TcpStream::connect((a.host.as_str(), a.port)).await.map_err(io)?;
    let _ = tcp.set_nodelay(true);
    let local_ip = tcp.local_addr().ok().map(|s| s.ip().to_canonical().to_string());
    let pin = match (&a.pin, a.mode) {
        (Some(p), _) if is_hex64(p) => Some(p.as_str()),
        (Some(_), _) | (None, Mode::Key) => return Err(io("no saved fingerprint")),
        // A first pairing: any certificate, bound into the proof below.
        (None, Mode::Pair) => None,
    };
    let seen: crate::tls::SeenSlot = Default::default();
    let cfg = match pin {
        Some(p) => crate::tls::client_config(p, seen.clone()),
        None => crate::tls::any_cert_config(seen.clone()),
    }
    .map_err(io)?;
    let server_name = rustls::pki_types::ServerName::try_from(a.host.clone())
        .map_err(|_| io("invalid host name"))?;
    let tls = tokio_rustls::TlsConnector::from(cfg)
        .connect(server_name, tcp)
        .await
        .map_err(|e| {
            let text = e.to_string();
            if text.contains("ApplicationVerificationFailure") {
                LinkError::CertMismatch(seen.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default())
            } else {
                LinkError::Io(format!("TLS: {text}"))
            }
        })?;
    let fp = tls
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|c| c.first())
        .map(|c| crate::tls::sha256_hex(c.as_ref()))
        .ok_or_else(|| io("no server certificate"))?;
    // The verifier already enforced the pin; checked again here because
    // everything below depends on it.
    if pin.is_some_and(|p| !handshake::ct_eq(&fp, p)) {
        return Err(LinkError::CertMismatch(fp));
    }

    let host = if a.host.contains(':') { format!("[{}]", a.host) } else { a.host.clone() };
    let url = format!("wss://{host}:{}{PATH}", a.port);
    let (mut ws, _resp) = tokio_tungstenite::client_async_with_config(url, tls, Some(ws_config()))
        .await
        .map_err(|e| io(format!("WebSocket: {e}")))?;

    // 1. hello
    let hello = read_handshake_frame(&mut ws).await?;
    match hello.get("t").and_then(Value::as_str) {
        Some("hello") => {}
        // Manager may refuse at once (rate limit, no pairing slot).
        Some("denied") => {
            let why = hello.get("why").and_then(Value::as_str).unwrap_or("").chars().take(120).collect::<String>();
            return Err(if why == "version" { LinkError::Version(None) } else { LinkError::Denied(why) });
        }
        _ => return Err(LinkError::Protocol("expected hello".into())),
    }
    // Only v2: a v1 Manager would key pair mode with K alone.
    match hello.get("v").and_then(Value::as_u64) {
        Some(handshake::VERSION) => {}
        Some(v) => {
            let _ = send_json(&mut ws, json!({"t": "denied", "why": "version"})).await;
            let _ = ws.close(None).await;
            return Err(LinkError::Version(Some(v)));
        }
        None => return Err(LinkError::Protocol("bad hello version".into())),
    }
    let manager_nonce = hello
        .get("nonce")
        .and_then(Value::as_str)
        .filter(|n| is_hex64(n))
        .ok_or_else(|| LinkError::Protocol("bad hello nonce".into()))?
        .to_string();
    // Manager states its own fingerprint; a different one means someone
    // else terminated TLS.
    let claimed = hello.get("fp").and_then(Value::as_str).unwrap_or("");
    if !handshake::ct_eq(claimed, &fp) {
        return Err(LinkError::CertMismatch(fp));
    }

    // 2. auth
    let pi_nonce = handshake::random_hex32().map_err(io)?;
    if handshake::ct_eq(&pi_nonce, &manager_nonce) {
        return Err(LinkError::Protocol("nonce reuse".into()));
    }
    let proof = handshake::pi_proof(&a.secret, &manager_nonce, &pi_nonce, &fp, &a.device_id);
    send_json(
        &mut ws,
        json!({
            "t": "auth", "v": handshake::VERSION, "id": a.device_id, "mode": a.mode.as_str(),
            "nonce": pi_nonce, "name": a.name, "version": a.version, "proof": proof,
        }),
    )
    .await?;

    // 3. ok or denied
    let answer = read_handshake_frame(&mut ws).await?;
    match answer.get("t").and_then(Value::as_str) {
        Some("ok") => {}
        Some("denied") => {
            let why = answer.get("why").and_then(Value::as_str).unwrap_or("").chars().take(120).collect::<String>();
            let _ = ws.close(None).await;
            return Err(if why == "version" { LinkError::Version(None) } else { LinkError::Denied(why) });
        }
        _ => return Err(LinkError::Protocol("expected ok or denied".into())),
    }
    let mgr = answer.get("proof").and_then(Value::as_str).unwrap_or("");
    if !handshake::verify_mgr_proof(&a.secret, &manager_nonce, &pi_nonce, &fp, &a.device_id, mgr) {
        let _ = ws.close(None).await;
        return Err(LinkError::BadProof(fp));
    }
    let link_key = match a.mode {
        Mode::Pair => Some(
            answer
                .get("linkKey")
                .and_then(Value::as_str)
                .and_then(Secret::from_hex)
                .filter(|k| k.to_hex() != "0".repeat(64))
                .ok_or_else(|| LinkError::Protocol("ok without a valid linkKey".into()))?,
        ),
        // In key mode a linkKey is not expected and is never used.
        Mode::Key => None,
    };
    Ok((ws, Established { fp, link_key, local_ip }))
}

/// Saves a fresh pairing: the link key and the fingerprint seen.
pub fn save_pairing(est: &Established) -> anyhow::Result<()> {
    let mut s = store::load()?;
    s.link_key = est.link_key.as_ref().map(Secret::to_hex);
    s.pin = Some(est.fp.clone());
    s.paired_at = Some(chrono_now());
    store::save(&s)
}

fn chrono_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The network as seen now: default route interface and gateway, the Pi's
/// address on this link, and the Wi-Fi name when the link runs over Wi-Fi.
fn current_net(status: Option<&Value>, local_ip: &Option<String>) -> crate::audit::Net {
    let (iface, gw) = crate::audit::default_route();
    let wifi_ssid = status.and_then(|s| s.get("wifi_ssid")).and_then(Value::as_str).unwrap_or("");
    let wifi_ip = status.and_then(|s| s.get("wifi_ip")).and_then(Value::as_str).unwrap_or("");
    let over_wifi = (!wifi_ip.is_empty() && local_ip.as_deref() == Some(wifi_ip))
        || iface.as_deref().is_some_and(|i| i.starts_with("wl"));
    let ssid = (over_wifi && !wifi_ssid.is_empty()).then(|| crate::audit::clip(wifi_ssid));
    crate::audit::Net { ssid, iface, ip: local_ip.clone(), gw }
}

fn log_net(net: &crate::audit::Net) {
    crate::audit::record(
        "info",
        "network",
        "Network in use",
        json!({"ssid": net.ssid, "iface": net.iface, "ip": net.ip, "gw": net.gw}),
    );
}

/// Runs the request loop until the link drops or `stop` fires. First sends
/// `net`, then any queued alerts, then status (contract 6b).
pub async fn run(
    ws: Ws,
    server: Arc<Server>,
    mut stop: tokio::sync::watch::Receiver<u64>,
    local_ip: Option<String>,
) -> String {
    let (mut sink, mut stream) = ws.split();
    let (tx, mut rx) = mpsc::channel::<Message>(16);

    // The only writer. A frame that can't go out within SEND_TIMEOUT ends
    // the link, so a Manager that stops reading can't pin memory.
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let closing = matches!(msg, Message::Close(_));
            match tokio::time::timeout(SEND_TIMEOUT, sink.send(msg)).await {
                Ok(Ok(())) if !closing => {}
                _ => break,
            }
        }
        let _ = sink.close().await;
    });

    let status = server.status_body().await;
    let mut net = current_net(status.as_ref(), &local_ip);
    log_net(&net);
    let _ = tx.send(Message::text(net.frame().to_string())).await;
    for a in crate::audit::take_alerts() {
        crate::audit::record("info", "alert_sent", "Reported an earlier failure to Sentry Manager",
            json!({"alert": a.kind, "seen": a.seen}));
        let _ = tx.send(Message::text(json!({"t": "alert", "kind": a.kind, "seen": a.seen, "at": a.at}).to_string())).await;
    }
    if let Some(body) = status {
        let _ = tx.send(Message::text(json!({"t": "status", "body": body}).to_string())).await;
    }
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + PING_INTERVAL, PING_INTERVAL);
    let mut silence = Box::pin(tokio::time::sleep(SILENCE_TIMEOUT));
    let mut car_check = tokio::time::interval_at(tokio::time::Instant::now() + CAR_CHECK_INTERVAL, CAR_CHECK_INTERVAL);
    car_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let changed = server.changed.clone();
    let reason = loop {
        tokio::select! {
            msg = stream.next() => {
                silence.as_mut().reset(tokio::time::Instant::now() + SILENCE_TIMEOUT);
                match msg {
                    Some(Ok(Message::Text(t))) => {
                        if let Some(reply) = server.handle_frame(t.as_str(), &tx) {
                            if tx.send(reply).await.is_err() { break "writer closed".to_string(); }
                        }
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | None => break "Sentry Manager closed the link".to_string(),
                    Some(Ok(_)) => {} // binary frames are not part of the protocol; ignored
                    Some(Err(e)) => break format!("link error: {e}"),
                }
            }
            _ = ping.tick() => {
                if tx.send(Message::Ping(Vec::new().into())).await.is_err() { break "writer closed".into(); }
                let status = server.status_body().await;
                let now_net = current_net(status.as_ref(), &local_ip);
                if now_net != net {
                    net = now_net;
                    log_net(&net);
                    let _ = tx.send(Message::text(net.frame().to_string())).await;
                }
                if let Some(body) = status {
                    let _ = tx.send(Message::text(json!({"t": "status", "body": body}).to_string())).await;
                }
            }
            _ = changed.notified() => {
                server.push_status(&tx).await;
            }
            _ = car_check.tick() => {
                if server.car_changed().await {
                    server.push_status(&tx).await;
                }
            }
            _ = &mut silence => break "no answer from Sentry Manager for 90 seconds".into(),
            _ = stop.changed() => {
                // Unpaired here (the key is already gone): close with 4001,
                // as Manager does, so Manager marks this Sentry USB unpaired
                // too instead of waiting for a handshake that never comes.
                if store::load().map(|s| s.link_key.is_none()).unwrap_or(false) {
                    let _ = tx.send(Message::Close(Some(CloseFrame { code: CloseCode::from(UNPAIRED_CLOSE), reason: "unpaired".into() }))).await;
                    break "unpaired on the Sentry USB".into();
                }
                let _ = tx.send(Message::Close(None)).await;
                break "stopped on the Sentry USB".into();
            }
        }
        if writer.is_finished() {
            break "link write failed".into();
        }
    };
    drop(tx);
    server.abort_all();
    let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
    reason
}
