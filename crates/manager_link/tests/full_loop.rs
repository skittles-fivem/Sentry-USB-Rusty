//! The whole client: settings in a config file, the pairing window, the
//! fingerprint check before the first pairing, pairing against a fake
//! Manager, the saved 0600 file, status, and unpairing.
//! Its own test binary because it sets process-wide environment variables.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

use sentryusb_manager_link::handshake::{self, Secret};

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
async fn flip_the_switch_pair_and_unpair() {
    let dir = tempfile::tempdir().unwrap();
    let conf = dir.path().join("sentryusb.conf");
    let linkfile = dir.path().join("link.json");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    std::fs::write(
        &conf,
        "export WEB_USERNAME=admin\nexport WEB_PASSWORD='correct horse battery staple'\nexport ARCHIVE_SERVER=127.0.0.1\n",
    )
    .unwrap();
    // SAFETY: set before any thread of this test binary reads them.
    unsafe {
        std::env::set_var("SENTRYUSB_CONFIG_PATH", &conf);
        std::env::set_var("SENTRYUSB_MANAGER_LINK_FILE", &linkfile);
        std::env::set_var("SENTRYUSB_MUTABLE_DIR", dir.path());
    }

    // Fake Manager with an open pairing slot: accepts any number of
    // connections, pairs, then answers with the key it handed out.
    let k = handshake::derive_pairing_key("admin", "correct horse battery staple");
    let link_key = Secret::from_bytes([0x42; 32]);
    let mfp = sentryusb_manager_link::tls::sha256_hex(MANAGER_CERT);
    let acc = acceptor();
    // Flipped to make the same address present another certificate.
    let impostor = Arc::new(AtomicBool::new(false));
    let other_acc = acceptor_for(OTHER_CERT, OTHER_KEY);
    let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    {
        let mfp = mfp.clone();
        let impostor = impostor.clone();
        tokio::spawn(async move {
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let acc = if impostor.load(Ordering::SeqCst) { other_acc.clone() } else { acc.clone() };
                let (k, lk, mfp, seen) = (k.clone(), link_key.clone(), mfp.clone(), seen_tx.clone());
                tokio::spawn(async move {
                    let Ok(tls) = acc.accept(tcp).await else { return };
                    let Ok(mut ws) = tokio_tungstenite::accept_async(tls).await else { return };
                    let mn = handshake::random_hex32().unwrap();
                    ws.send(Message::text(json!({"t":"hello","v":1,"nonce":mn,"fp":mfp}).to_string())).await.unwrap();
                    let Some(Ok(Message::Text(t))) = ws.next().await else { return };
                    let auth: Value = serde_json::from_str(t.as_str()).unwrap();
                    let (pn, id) = (auth["nonce"].as_str().unwrap(), auth["id"].as_str().unwrap());
                    let s = if auth["mode"] == "pair" { k.clone() } else { lk.clone() };
                    assert_eq!(auth["proof"].as_str().unwrap(), handshake::pi_proof(&s, &mn, pn, &mfp, id));
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

    // The one switch (plus the test's port instead of 10000). With no pin
    // yet, the Pi only reads the fingerprint and waits for the owner.
    sentryusb_manager_link::save_settings(true, &format!("127.0.0.1:{port}")).unwrap();
    let s = wait_for("confirm_fingerprint").await;
    assert_eq!(s["check"]["fingerprint"], mfp);
    let display = s["check"]["display"].as_str().unwrap().to_string();
    assert_eq!(s["check"]["first"], display[..4]);
    assert_eq!(s["check"]["last"], display[display.len() - 4..]);
    assert_eq!(s["fingerprint"], Value::Null, "nothing pinned yet");
    assert!(s["message"].as_str().unwrap().contains("Check this matches the fingerprint on Sentry Manager's Sentry USB page"));
    assert!(
        tokio::time::timeout(Duration::from_secs(3), seen_rx.recv()).await.is_err(),
        "no proof before the owner confirms"
    );
    // Only the fingerprint being shown is accepted.
    let other_fp = sentryusb_manager_link::tls::sha256_hex(OTHER_CERT);
    let e = sentryusb_manager_link::confirm_pairing(&other_fp).unwrap_err();
    assert!(e.contains("changed"), "{e}");
    assert!(sentryusb_manager_link::confirm_pairing("1234").is_err());
    // "It matches, pair" (the grouped form works too).
    sentryusb_manager_link::confirm_pairing(&display).unwrap();
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
    for k in ["pairing_open", "fingerprint_read", "fingerprint_confirmed", "paired", "connected", "network", "refused", "fingerprint_mismatch", "alert_sent"] {
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
    let s = wait_for("pairing_expired").await;
    while seen_rx.try_recv().is_ok() {}
    assert_eq!(s["paired"], false);
    let saved: Value = serde_json::from_slice(&std::fs::read(&linkfile).unwrap()).unwrap();
    assert!(saved.get("link_key").is_none() && saved.get("pin").is_none());
    assert!(
        tokio::time::timeout(Duration::from_secs(3), seen_rx.recv()).await.is_err(),
        "an unpaired Pi must not reconnect without a new pairing window"
    );

    // Pair again: the fingerprint is read afresh and must be confirmed.
    sentryusb_manager_link::start_pairing().unwrap();
    let s = wait_for("confirm_fingerprint").await;
    assert_eq!(s["check"]["fingerprint"], mfp);
    assert_eq!(s["fingerprint_changed"], false);
    // The certificate changes between the read and the pairing: nothing is
    // sent, and the new one is shown, flagged, to be checked again.
    impostor.store(true, Ordering::SeqCst);
    sentryusb_manager_link::confirm_pairing(&mfp).unwrap();
    let mut s = wait_for("confirm_fingerprint").await;
    for _ in 0..100 {
        if s["fingerprint_changed"] == true {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        s = sentryusb_manager_link::status_json();
    }
    assert_eq!(s["fingerprint_changed"], true, "{s}");
    assert_eq!(s["check"]["fingerprint"], other_fp);
    assert!(s["message"].as_str().unwrap().contains("changed after you confirmed it"));
    assert_eq!(s["fingerprint"], Value::Null, "the confirmed pin is dropped");
    assert!(
        tokio::time::timeout(Duration::from_secs(2), seen_rx.recv()).await.is_err(),
        "the other certificate got no proof"
    );
    // Back to the real certificate; Pair reads it again.
    impostor.store(false, Ordering::SeqCst);
    sentryusb_manager_link::start_pairing().unwrap();
    let mut s = wait_for("confirm_fingerprint").await;
    for _ in 0..100 {
        if s["check"]["fingerprint"] == mfp.as_str() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        s = sentryusb_manager_link::status_json();
    }
    assert_eq!(s["check"]["fingerprint"], mfp);
    sentryusb_manager_link::confirm_pairing(&mfp).unwrap();
    let again = loop {
        let f = tokio::time::timeout(Duration::from_secs(20), seen_rx.recv()).await.unwrap().unwrap();
        if f.get("mode").is_some() {
            break f;
        }
    };
    assert_eq!(again["mode"], "pair");
    wait_for("connected").await;

    // Turning it off stops the link.
    sentryusb_manager_link::save_settings(false, "").unwrap();
    wait_for("off").await;
}
