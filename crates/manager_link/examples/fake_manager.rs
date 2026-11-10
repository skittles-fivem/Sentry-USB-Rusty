//! A minimal stand-in for Sentry Manager's device port, to check
//! `link_harness` and `tools/manager-link-e2e.sh` before the real Manager
//! is available. Not Manager: no admin API, no database, no rate limit.
//!
//! - Serves `wss://127.0.0.1:<port>/api/sentryusb/link` with the test
//!   fixture certificate (`tests/fixtures/manager.cert.der`).
//! - Pairs with a Pi whose web login is `--user`/`--password` (the slot is
//!   always open), hands out a random link key and keeps it in memory, so
//!   later connections use key mode.
//! - After each handshake it sends every allowlisted route once, then a few
//!   requests the Pi must refuse, waiting for each answer.
//! - Prints each frame from the Pi, and each answer's summary, as JSON lines.
//!
//! ```text
//! cargo run -p sentryusb-manager-link --example fake_manager -- --port 10000 --user admin --password pw
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use sentryusb_manager_link::allowlist::{Method, ROUTES};
use sentryusb_manager_link::handshake::{self, Secret};

const CERT: &[u8] = include_bytes!("../tests/fixtures/manager.cert.der");
const KEY: &[u8] = include_bytes!("../tests/fixtures/manager.key.der");

type Ws = tokio_tungstenite::WebSocketStream<tokio_rustls::server::TlsStream<tokio::net::TcpStream>>;

fn emit(mut v: Value) {
    v["src"] = json!("manager");
    println!("{v}");
}

#[tokio::main]
async fn main() {
    let mut port = 10000u16;
    let (mut user, mut pass) = (String::new(), String::new());
    let mut it = std::env::args().skip(1);
    while let Some(f) = it.next() {
        let v = it.next().expect("value");
        match f.as_str() {
            "--port" => port = v.parse().expect("port"),
            "--user" => user = v,
            "--password" => pass = v,
            _ => panic!("unknown flag {f}"),
        }
    }
    let k = handshake::derive_pairing_key(&user, &pass);
    let fp = sentryusb_manager_link::tls::sha256_hex(CERT);
    let keys: Arc<Mutex<HashMap<String, Secret>>> = Default::default();

    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    let cfg = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(CERT.to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(KEY.to_vec())),
        )
        .unwrap();
    let acc = tokio_rustls::TlsAcceptor::from(Arc::new(cfg));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.expect("bind");
    emit(json!({"event": "listening", "port": listener.local_addr().unwrap().port(), "fingerprint": fp}));
    loop {
        let (tcp, peer) = listener.accept().await.expect("accept");
        let (acc, k, fp, keys) = (acc.clone(), k.clone(), fp.clone(), keys.clone());
        tokio::spawn(async move {
            let Ok(tls) = acc.accept(tcp).await else {
                emit(json!({"event": "tls_failed", "peer": peer.to_string()}));
                return;
            };
            let Ok(ws) = tokio_tungstenite::accept_async(tls).await else { return };
            session(ws, &k, &fp, &keys).await;
        });
    }
}

async fn recv(ws: &mut Ws) -> Option<Value> {
    loop {
        match ws.next().await? {
            Ok(Message::Text(t)) => return serde_json::from_str(t.as_str()).ok(),
            Ok(Message::Ping(_) | Message::Pong(_)) => continue,
            _ => return None,
        }
    }
}

async fn send(ws: &mut Ws, v: Value) {
    let _ = ws.send(Message::text(v.to_string())).await;
}

async fn session(mut ws: Ws, k: &Secret, fp: &str, keys: &Mutex<HashMap<String, Secret>>) {
    let mn = handshake::random_hex32().unwrap();
    send(&mut ws, json!({"t": "hello", "v": 1, "nonce": mn, "fp": fp})).await;
    let Some(auth) = recv(&mut ws).await else { return };
    let (pn, id, mode) = (
        auth["nonce"].as_str().unwrap_or("").to_string(),
        auth["id"].as_str().unwrap_or("").to_string(),
        auth["mode"].as_str().unwrap_or("").to_string(),
    );
    let secret = if mode == "pair" { Some(k.clone()) } else { keys.lock().unwrap().get(&id).cloned() };
    let good = secret.as_ref().is_some_and(|s| auth["proof"].as_str() == Some(handshake::pi_proof(s, &mn, &pn, fp, &id).as_str()));
    if !good {
        emit(json!({"event": "denied", "id": id, "mode": mode}));
        send(&mut ws, json!({"t": "denied", "why": "not paired"})).await;
        let _ = ws.close(None).await;
        return;
    }
    let s = secret.unwrap();
    let mut ok = json!({"t": "ok", "proof": handshake::mgr_proof(&s, &mn, &pn, fp, &id)});
    if mode == "pair" {
        let mut b = [0u8; 32];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b).unwrap();
        let lk = Secret::from_bytes(b);
        ok["linkKey"] = json!(lk.to_hex());
        keys.lock().unwrap().insert(id.clone(), lk);
    }
    send(&mut ws, ok).await;
    emit(json!({"event": "authenticated", "id": id, "mode": mode, "name": auth["name"], "version": auth["version"]}));

    // Frames until the first status, then the requests.
    while let Some(f) = recv(&mut ws).await {
        let t = f["t"].as_str().unwrap_or("").to_string();
        emit(json!({"event": "frame", "t": t, "frame": summarize(&f)}));
        if t == "status" {
            break;
        }
    }
    let mut n = 0u32;
    for (m, pattern) in ROUTES {
        let path = pattern.replace("{}", if pattern.starts_with("/api/logs") { "manager-link" } else { "snap-000042" });
        let (query, body) = sample(*m, &path, &mut ws, &mut n).await;
        n += 1;
        request(&mut ws, &format!("r{n}"), m.as_str(), &path, &query, body).await;
    }
    for (m, path, body) in [
        ("GET", "/api/terminal", Value::Null),
        ("GET", "/api/manager-link", Value::Null),
        ("GET", "/api/system/backup/2026-11-01", Value::Null),
        ("PUT", "/api/system/update-source", json!({"repo": "someone/else"})),
        ("GET", "/api/logs/../../etc/shadow", Value::Null),
    ] {
        n += 1;
        request(&mut ws, &format!("x{n}"), m, path, "", body).await;
    }
    emit(json!({"event": "requests_done", "count": n}));
    // Then just log whatever comes (status pushes, pings answered by tungstenite).
    while let Some(f) = recv(&mut ws).await {
        emit(json!({"event": "frame", "t": f["t"], "frame": summarize(&f)}));
    }
    emit(json!({"event": "closed", "id": id}));
}

/// Query and body for one allowlisted route.
async fn sample(m: Method, path: &str, ws: &mut Ws, n: &mut u32) -> (String, Value) {
    match (m, path) {
        (Method::Put, "/api/setup/config") => {
            // Full replacement from the masked GET, secrets left "".
            *n += 1;
            let cfg = request(ws, &format!("r{n}"), "GET", "/api/setup/config", "", Value::Null).await;
            let mut out = serde_json::Map::new();
            for (k, v) in cfg["body"].as_object().cloned().unwrap_or_default() {
                if k.ends_with("__set") || v["active"] != true {
                    continue;
                }
                out.insert(k, v["value"].clone());
            }
            out.insert("TIME_ZONE".into(), json!("Europe/Paris"));
            (String::new(), Value::Object(out))
        }
        (Method::Put, "/api/config/preference") => (String::new(), json!({"key": "harness", "value": 1})),
        (Method::Get, "/api/config/preference") => ("key=harness".into(), Value::Null),
        (Method::Put, "/api/notifications/settings") => (String::new(), json!({"enabled": true})),
        (Method::Put, "/api/system/keep-accessory-config") => (String::new(), json!({"enabled": true, "minutes": 15})),
        (Method::Put, "/api/system/update-source") => (String::new(), json!({"repo": ""})),
        (Method::Post, "/api/setup/test-archive") => (String::new(), json!({"ARCHIVE_SYSTEM": "cifs", "ARCHIVE_SERVER": "nas.local", "SHARE_PASSWORD": ""})),
        (Method::Get, p) if p.ends_with("/page") || p.ends_with("/tail") => ("lines=50".into(), Value::Null),
        _ => (String::new(), Value::Null),
    }
}

/// Sends one request and waits for its answer (joining chunks).
async fn request(ws: &mut Ws, id: &str, method: &str, path: &str, query: &str, body: Value) -> Value {
    send(ws, json!({"t": "req", "id": id, "method": method, "path": path, "query": query, "body": body})).await;
    let mut res = Value::Null;
    let mut chunks: Vec<u8> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let Ok(Some(f)) = tokio::time::timeout_at(deadline, recv(ws)).await else { break };
        if f["id"] != id {
            emit(json!({"event": "frame", "t": f["t"], "frame": summarize(&f)}));
            continue;
        }
        match f["t"].as_str() {
            Some("res") if f["chunked"] == true => res = f,
            Some("res") => {
                res = f;
                break;
            }
            Some("chunk") => {
                chunks.extend(base64::engine::general_purpose::STANDARD.decode(f["data"].as_str().unwrap_or("")).unwrap_or_default())
            }
            Some("end") => break,
            _ => {}
        }
    }
    let bytes = if res["chunked"] == true {
        chunks.len()
    } else {
        res["bodyB64"].as_str().map(|b| b.len() * 3 / 4).unwrap_or(0)
    };
    emit(json!({"event": "answer", "id": id, "method": method, "path": path, "status": res["status"],
        "chunked": res["chunked"] == true, "bytes": bytes, "body": summarize(&res["body"])}));
    res
}

/// Short form of a frame for the log.
fn summarize(v: &Value) -> Value {
    let s = v.to_string();
    if s.len() <= 300 { v.clone() } else { json!(format!("{}…", &s[..s.char_indices().nth(300).map(|c| c.0).unwrap_or(s.len())])) }
}
