//! The whole client: settings in a config file, Pair with an address and a
//! pairing code, wrong codes and passwords, pairing against a fake Manager,
//! the saved 0600 file, status, unpairing, and that the code never reaches
//! disk or a log.
//! Its own test binary because it sets process-wide environment variables.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

use sentryusb_manager_link::handshake::{self, PairingCode, Secret};

/// The code Manager shows; the fake Manager's slot holds P for it.
const CODE: &str = "K7QP-M2XD-9WRT";
const PASSWORD: &str = "correct horse battery staple";

/// Every `tracing` event of the process, formatted, so the test can check
/// the code never reaches a log line.
static TRACED: Mutex<String> = Mutex::new(String::new());

struct Capture;

struct Fields<'a>(&'a mut String);

impl tracing::field::Visit for Fields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        let _ = write!(self.0, " {}={:?}", field.name(), value);
    }
}

impl tracing::Subscriber for Capture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        let mut out = TRACED.lock().unwrap();
        span.record(&mut Fields(&mut out));
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        values.record(&mut Fields(&mut TRACED.lock().unwrap()));
    }
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut out = TRACED.lock().unwrap();
        event.record(&mut Fields(&mut out));
        out.push('\n');
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

fn write_conf(conf: &std::path::Path, password: &str) {
    // Keep what save_settings wrote (on/off, address); change the password.
    let text = std::fs::read_to_string(conf).unwrap_or_default();
    let kept: String = text.lines().filter(|l| !l.contains("WEB_PASSWORD")).map(|l| format!("{l}\n")).collect();
    std::fs::write(conf, format!("{kept}export WEB_PASSWORD='{password}'\n")).unwrap();
}

/// Every file under `dir`, as text.
fn all_files(dir: &std::path::Path) -> String {
    let mut out = String::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.push_str(&all_files(&p));
        } else {
            out.push_str(&String::from_utf8_lossy(&std::fs::read(&p).unwrap()));
        }
    }
    out
}

fn assert_no_code(what: &str, text: &str) {
    for spelling in ["K7QP", "k7qp", "M2XD", "9WRT", "K7QPM2XD9WRT"] {
        assert!(!text.contains(spelling), "the pairing code reached {what}");
    }
}

const MANAGER_CERT: &[u8] = include_bytes!("fixtures/manager.cert.der");
const MANAGER_KEY: &[u8] = include_bytes!("fixtures/manager.key.der");
const OTHER_CERT: &[u8] = include_bytes!("fixtures/other.cert.der");
const OTHER_KEY: &[u8] = include_bytes!("fixtures/other.key.der");

fn acceptor() -> tokio_rustls::TlsAcceptor {
    acceptor_for(MANAGER_CERT, MANAGER_KEY)
}

fn acceptor_for(cert: &'static [u8], key: &'static [u8]) -> tokio_rustls::TlsAcceptor {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    let cfg = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert.to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.to_vec())),
        )
        .unwrap();
    tokio_rustls::TlsAcceptor::from(Arc::new(cfg))
}

async fn next(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Value>) -> Value {
    tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap()
}

async fn wait_for(state: &str) -> Value {
    for _ in 0..200 {
        let s = sentryusb_manager_link::status_json();
        if s["state"] == state {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("state never became {state}: {}", sentryusb_manager_link::status_json());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pair_with_a_code_then_unpair() {
    tracing::subscriber::set_global_default(Capture).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let conf = dir.path().join("sentryusb.conf");
    let linkfile = dir.path().join("link.json");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    std::fs::write(
        &conf,
        format!("export WEB_USERNAME=admin\nexport WEB_PASSWORD='{PASSWORD}'\nexport ARCHIVE_SERVER=127.0.0.1\n"),
    )
    .unwrap();
    // SAFETY: set before any thread of this test binary reads them.
    unsafe {
        std::env::set_var("SENTRYUSB_CONFIG_PATH", &conf);
        std::env::set_var("SENTRYUSB_MANAGER_LINK_FILE", &linkfile);
        std::env::set_var("SENTRYUSB_MUTABLE_DIR", dir.path());
    }

    // Fake Manager with an open pairing slot holding P for CODE: accepts
    // any number of connections, pairs, then answers with the key it handed
    // out. A wrong proof is denied as the real one does.
    let k = handshake::derive_pair_secret(&handshake::derive_pairing_key("admin", PASSWORD), &PairingCode::parse(CODE).unwrap());
    let link_key = Secret::from_bytes([0x42; 32]);
    let mfp = sentryusb_manager_link::tls::sha256_hex(MANAGER_CERT);
    let acc = acceptor();
    // Flipped to make the same address present another certificate.
    let impostor = Arc::new(AtomicBool::new(false));
    let other_acc = acceptor_for(OTHER_CERT, OTHER_KEY);
    let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let connections = Arc::new(AtomicUsize::new(0));
    let denials = Arc::new(AtomicUsize::new(0));
    {
        let mfp = mfp.clone();
        let impostor = impostor.clone();
        let (connections, denials) = (connections.clone(), denials.clone());
        tokio::spawn(async move {
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                connections.fetch_add(1, Ordering::SeqCst);
                let denials = denials.clone();
                let acc = if impostor.load(Ordering::SeqCst) { other_acc.clone() } else { acc.clone() };
                let (k, lk, mfp, seen) = (k.clone(), link_key.clone(), mfp.clone(), seen_tx.clone());
                tokio::spawn(async move {
                    let Ok(tls) = acc.accept(tcp).await else { return };
                    let Ok(mut ws) = tokio_tungstenite::accept_async(tls).await else { return };
                    let mn = handshake::random_hex32().unwrap();
                    ws.send(Message::text(json!({"t":"hello","v":2,"nonce":mn,"fp":mfp}).to_string())).await.unwrap();
                    let Some(Ok(Message::Text(t))) = ws.next().await else { return };
                    let auth: Value = serde_json::from_str(t.as_str()).unwrap();
                    assert_eq!(auth["v"], 2);
                    let (pn, id) = (auth["nonce"].as_str().unwrap(), auth["id"].as_str().unwrap());
                    let s = if auth["mode"] == "pair" { k.clone() } else { lk.clone() };
                    if auth["proof"].as_str().unwrap() != handshake::pi_proof(&s, &mn, pn, &mfp, id) {
                        denials.fetch_add(1, Ordering::SeqCst);
                        let _ = ws.send(Message::text(json!({"t":"denied","why":"wrong password"}).to_string())).await;
                        return;
                    }
                    let mut ok = json!({"t":"ok","proof": handshake::mgr_proof(&s, &mn, pn, &mfp, id)});
                    if auth["mode"] == "pair" {
                        ok["linkKey"] = json!(lk.to_hex());
                    }
                    ws.send(Message::text(ok.to_string())).await.unwrap();
                    seen.send(auth).unwrap();
                    // One request the allowlist must refuse.
                    let req = json!({"t":"req","id":"x1","method":"GET","path":"/api/terminal"});
                    let _ = ws.send(Message::text(req.to_string())).await;
                    // Everything the Pi sends after the handshake.
                    while let Some(Ok(m)) = ws.next().await {
                        match m {
                            Message::Text(t) => {
                                let _ = seen.send(serde_json::from_str(t.as_str()).unwrap());
                            }
                            Message::Close(f) => {
                                let code = f.map(|f| u16::from(f.code));
                                let _ = seen.send(json!({"t": "__close", "code": code}));
                            }
                            _ => {}
                        }
                    }
                });
            }
        });
    }

    let router = axum::Router::new()
        .route("/api/status", axum::routing::get(|| async { axum::Json(json!({"ok": true})) }));
    sentryusb_manager_link::spawn(router, "v-test".into());

    // Off by default, and no address is filled in from the archive server.
    let s = wait_for("off").await;
    assert!(s.get("default_address").is_none());
    assert!(s["effective_address"].is_null());
    assert_eq!(s["web_password_set"], true);

    // The switch and the address alone don't pair, and send nothing.
    let addr = format!("127.0.0.1:{port}");
    sentryusb_manager_link::save_settings(true, &addr).unwrap();
    let s = wait_for("needs_pairing").await;
    assert!(s["message"].as_str().unwrap().contains("pairing code"), "{s}");
    assert_eq!(s["pairing"], false);
    assert!(s.get("check").is_none(), "no fingerprint to confirm any more");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(connections.load(Ordering::SeqCst), 0, "no connection before Pair, not even TLS");

    // A malformed code is refused with one plain message; nothing opens.
    for bad in ["", "1234", "K7QP-M2XD-9WRU", "K7QP-M2XD-9WRTX"] {
        assert_eq!(sentryusb_manager_link::start_pairing(&addr, bad).unwrap_err(), "That isn't a valid pairing code");
    }
    assert_eq!(sentryusb_manager_link::status_json()["pairing"], false);

    // A wrong code with the right password: denied once, the window closes
    // (retrying would only use up Manager's tries), and a plain message.
    sentryusb_manager_link::start_pairing(&addr, "K7QP-M2XD-9WRV").unwrap();
    let wrong_code = wait_for("pair_failed").await;
    assert!(wrong_code["message"].as_str().unwrap().starts_with("Wrong pairing code or password"), "{wrong_code}");
    assert_eq!(wrong_code["pairing"], false);
    assert_eq!(wrong_code["paired"], false);
    assert_eq!(denials.load(Ordering::SeqCst), 1);
    // The right code with a wrong password: the same message.
    write_conf(&conf, "not the password");
    sentryusb_manager_link::start_pairing(&addr, CODE).unwrap();
    wait_for("pairing").await;
    let wrong_pass = wait_for("pair_failed").await;
    assert_eq!(wrong_pass["message"], wrong_code["message"], "neither is revealed");
    assert_eq!(denials.load(Ordering::SeqCst), 2);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(denials.load(Ordering::SeqCst), 2, "no retries after a refusal");
    assert!(seen_rx.try_recv().is_err());
    write_conf(&conf, PASSWORD);

    // Pair: the address (already saved) and the code, typed loosely.
    sentryusb_manager_link::start_pairing("", " k7qp m2xd 9wrt ").unwrap();
    let s = sentryusb_manager_link::status_json();
    assert_eq!(s["pairing"], true);
    assert!(s["pairing_seconds_left"].as_u64().unwrap() > 14 * 60);
    let first = tokio::time::timeout(Duration::from_secs(20), seen_rx.recv()).await.unwrap().unwrap();
    assert_eq!(first["mode"], "pair");
    // After the handshake: net first, then status; the refused request.
    let net = next(&mut seen_rx).await;
    assert_eq!(net["t"], "net");
    assert_eq!(net["ip"], "127.0.0.1");
    let mut frames = [next(&mut seen_rx).await, next(&mut seen_rx).await];
    frames.sort_by_key(|f| f["t"].as_str().unwrap().to_string());
    assert_eq!(frames[0]["t"], "res");
    assert_eq!(frames[0]["status"], 403);
    assert_eq!(frames[1]["t"], "status");
    let s = wait_for("connected").await;
    assert_eq!(s["paired"], true);
    assert_eq!(s["fingerprint"], mfp);
    assert_eq!(s["fingerprint_display"].as_str().unwrap().len(), 16 * 4 + 15);
    assert!(!s.to_string().contains(&"42".repeat(32)), "status must not show the key");

    let saved: Value = serde_json::from_slice(&std::fs::read(&linkfile).unwrap()).unwrap();
    assert_eq!(saved["link_key"], "42".repeat(32));
    assert_eq!(saved["pin"], mfp);
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&linkfile).unwrap().permissions().mode() & 0o777, 0o600);
    }
    // The key never lands in the user-visible config.
    let conf_text = std::fs::read_to_string(&conf).unwrap();
    assert!(!conf_text.contains(&"42".repeat(32)));
    assert!(conf_text.contains("SENTRY_MANAGER_ENABLED=true"));

    // A fake Manager on another address: the pin refuses it, the log warns
    // with the fingerprint seen, and the real Manager hears about it on the
    // next good connection.
    let fake = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fake_port = fake.local_addr().unwrap().port();
    let fake_acc = acceptor_for(OTHER_CERT, OTHER_KEY);
    let other_fp = sentryusb_manager_link::tls::sha256_hex(OTHER_CERT);
    tokio::spawn(async move {
        loop {
            let (tcp, _) = fake.accept().await.unwrap();
            let _ = fake_acc.accept(tcp).await;
        }
    });
    sentryusb_manager_link::save_settings(true, &format!("127.0.0.1:{fake_port}")).unwrap();
    let s = wait_for("cert_changed").await;
    assert!(s["message"].as_str().unwrap().contains("certificate changed"));
    let log = sentryusb_manager_link::log_json(200);
    let warn = &log["warnings"][0];
    assert_eq!(warn["kind"], "fingerprint_mismatch");
    assert_eq!(warn["level"], "warn");
    assert_eq!(warn["seen"], other_fp);
    // Back to the real Manager.
    while seen_rx.try_recv().is_ok() {}
    sentryusb_manager_link::save_settings(true, &format!("127.0.0.1:{port}")).unwrap();
    let mut alert = None;
    for _ in 0..10 {
        let f = next(&mut seen_rx).await;
        if f["t"] == "alert" {
            alert = Some(f);
            break;
        }
    }
    let alert = alert.expect("alert frame");
    assert_eq!(alert["kind"], "fingerprint");
    assert_eq!(alert["seen"], other_fp);
    assert!(alert["at"].as_i64().unwrap() > 0);
    wait_for("connected").await;
    let saved: Value = serde_json::from_slice(&std::fs::read(&linkfile).unwrap()).unwrap();
    assert!(saved.get("alerts").is_none(), "alerts are cleared once sent");

    // The log has the whole story, newest first, without secrets.
    let log = sentryusb_manager_link::log_json(200);
    let kinds: Vec<&str> = log["entries"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect();
    for k in ["pairing_open", "pairing_closed", "paired", "connected", "network", "refused", "fingerprint_mismatch", "alert_sent"] {
        assert!(kinds.contains(&k), "{k} missing from {kinds:?}");
    }
    let text = std::fs::read_to_string(dir.path().join("sentryusb-manager-link.log")).unwrap();
    assert!(!text.contains("correct horse") && !text.contains(&"42".repeat(32)));

    // Unpair drops the link and forgets key and pin; it never pairs again
    // by itself.
    sentryusb_manager_link::unpair().unwrap();
    // ...closing with 4001 ("unpaired"), so Manager forgets this Pi too.
    loop {
        let f = next(&mut seen_rx).await;
        if f["t"] == "__close" {
            assert_eq!(f["code"], 4001, "{f}");
            break;
        }
    }
    let s = wait_for("needs_pairing").await;
    while seen_rx.try_recv().is_ok() {}
    assert_eq!(s["paired"], false);
    let saved: Value = serde_json::from_slice(&std::fs::read(&linkfile).unwrap()).unwrap();
    assert!(saved.get("link_key").is_none() && saved.get("pin").is_none());
    assert!(
        tokio::time::timeout(Duration::from_secs(3), seen_rx.recv()).await.is_err(),
        "an unpaired Pi must not reconnect without a new pairing window"
    );

    // Pair again (no pin now) while something else answers at the address
    // with its own certificate, relaying Manager's hello: refused before
    // any proof, and the window stays open for the real Manager.
    impostor.store(true, Ordering::SeqCst);
    sentryusb_manager_link::start_pairing(&addr, CODE).unwrap();
    for _ in 0..100 {
        if sentryusb_manager_link::status_json()["last_error"].is_string() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let s = sentryusb_manager_link::status_json();
    assert_eq!(s["state"], "pairing", "{s}");
    assert_eq!(s["fingerprint"], Value::Null);
    assert!(
        tokio::time::timeout(Duration::from_secs(1), seen_rx.recv()).await.is_err(),
        "the other certificate got no proof"
    );
    impostor.store(false, Ordering::SeqCst);
    let again = loop {
        let f = tokio::time::timeout(Duration::from_secs(30), seen_rx.recv()).await.unwrap().unwrap();
        if f.get("mode").is_some() {
            break f;
        }
    };
    assert_eq!(again["mode"], "pair");
    let s = wait_for("connected").await;
    assert_eq!(s["fingerprint"], mfp, "the certificate that paired is pinned");

    // The code never reached disk, a log line, the status or the log page.
    assert_no_code("a file", &all_files(dir.path()));
    assert!(TRACED.lock().unwrap().contains("paired with Sentry Manager"), "log lines are captured");
    assert_no_code("a log line", &TRACED.lock().unwrap());
    assert_no_code("the status", &sentryusb_manager_link::status_json().to_string());
    assert_no_code("the log page", &sentryusb_manager_link::log_json(500).to_string());
    assert!(!TRACED.lock().unwrap().contains(PASSWORD));

    // Turning it off stops the link.
    sentryusb_manager_link::save_settings(false, "").unwrap();
    wait_for("off").await;
}
