//! End-to-end tests against a small local TLS WebSocket server that plays
//! Sentry Manager.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

use sentryusb_manager_link::handshake::{self, Secret};
use sentryusb_manager_link::serve::Server;
use sentryusb_manager_link::session::{self, Attempt, LinkError, Mode};

const MANAGER_CERT: &[u8] = include_bytes!("fixtures/manager.cert.der");
const MANAGER_KEY: &[u8] = include_bytes!("fixtures/manager.key.der");
const OTHER_CERT: &[u8] = include_bytes!("fixtures/other.cert.der");
const OTHER_KEY: &[u8] = include_bytes!("fixtures/other.key.der");

const USER: &str = "admin";
const PASS: &str = "correct horse battery staple";
const DEVICE: &str = "0123456789abcdef0123456789abcdef";

type ServerWs = tokio_tungstenite::WebSocketStream<tokio_rustls::server::TlsStream<tokio::net::TcpStream>>;

fn fp(cert: &[u8]) -> String {
    sentryusb_manager_link::tls::sha256_hex(cert)
}

fn acceptor(cert: &'static [u8], key: &'static [u8]) -> tokio_rustls::TlsAcceptor {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ServerConfig::builder_with_provider(provider)
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

/// Starts a fake Manager on a random port that runs `behave` on the first
/// connection and returns its result.
async fn manager<F, Fut, T>(
    cert: &'static [u8],
    key: &'static [u8],
    behave: F,
) -> (u16, tokio::task::JoinHandle<T>)
where
    F: FnOnce(ServerWs) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + Send,
    T: Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let acc = acceptor(cert, key);
    let h = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let tls = acc.accept(tcp).await.unwrap();
        let ws = tokio_tungstenite::accept_async(tls).await.unwrap();
        behave(ws).await
    });
    (port, h)
}

async fn recv(ws: &mut ServerWs) -> Value {
    loop {
        match ws.next().await.unwrap().unwrap() {
            Message::Text(t) => return serde_json::from_str(t.as_str()).unwrap(),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected {other:?}"),
        }
    }
}

async fn send(ws: &mut ServerWs, v: Value) {
    ws.send(Message::text(v.to_string())).await.unwrap();
}

fn attempt(port: u16, mode: Mode, secret: Secret, pin: String) -> Attempt {
    Attempt {
        host: "127.0.0.1".into(),
        port,
        mode,
        secret,
        pin,
        device_id: DEVICE.into(),
        name: "sentryusb".into(),
        version: "v-test".into(),
    }
}

/// Plays a correct Manager handshake. `s` is the secret Manager holds and
/// `own_fp` the fingerprint it computes with (its real certificate's).
async fn honest_handshake(ws: &mut ServerWs, s: &Secret, own_fp: &str, link_key: Option<&Secret>) -> Value {
    let mn = handshake::random_hex32().unwrap();
    send(ws, json!({"t":"hello","v":1,"nonce":mn,"fp":own_fp})).await;
    let auth = recv(ws).await;
    let pn = auth["nonce"].as_str().unwrap().to_string();
    let id = auth["id"].as_str().unwrap().to_string();
    let expect = handshake::pi_proof(s, &mn, &pn, own_fp, &id);
    if auth["proof"].as_str() != Some(expect.as_str()) {
        send(ws, json!({"t":"denied","why":"not paired"})).await;
        return auth;
    }
    let mut ok = json!({"t":"ok","proof": handshake::mgr_proof(s, &mn, &pn, own_fp, &id)});
    if let Some(k) = link_key {
        ok["linkKey"] = json!(k.to_hex());
    }
    send(ws, ok).await;
    auth
}

fn router() -> axum::Router {
    use axum::routing::get;
    axum::Router::new()
        .route("/api/status", get(|| async { axum::Json(json!({"uptime": "1"})) }))
        .route("/api/terminal", get(|| async { "shell" }))
}

#[tokio::test]
async fn pairing_then_requests_end_to_end() {
    let k = handshake::derive_pairing_key(USER, PASS);
    let link_key = Secret::from_bytes([7u8; 32]);
    let mfp = fp(MANAGER_CERT);
    let (port, mgr) = manager(MANAGER_CERT, MANAGER_KEY, {
        let (k, lk, mfp) = (k.clone(), link_key.clone(), mfp.clone());
        move |mut ws| async move {
            let auth = honest_handshake(&mut ws, &k, &mfp, Some(&lk)).await;
            assert_eq!(auth["mode"], "pair");
            assert_eq!(auth["v"], 1);
            assert!(auth.get("password").is_none());
            // The Pi tells Manager its network first, then pushes status.
            let net = recv(&mut ws).await;
            assert_eq!(net["t"], "net");
            assert_eq!(net["ip"], "127.0.0.1");
            let status = recv(&mut ws).await;
            assert_eq!(status, json!({"t":"status","body":{"uptime":"1"}}));
            send(&mut ws, json!({"t":"req","id":"1","method":"GET","path":"/api/status","query":"","body":null})).await;
            send(&mut ws, json!({"t":"req","id":"2","method":"GET","path":"/api/terminal","query":"","body":null})).await;
            let mut got = vec![recv(&mut ws).await, recv(&mut ws).await];
            got.sort_by_key(|v| v["id"].as_str().unwrap().to_string());
            // A JSON ping is answered.
            send(&mut ws, json!({"t":"ping"})).await;
            let pong = recv(&mut ws).await;
            ws.close(None).await.ok();
            (got, pong)
        }
    })
    .await;

    // The owner confirmed Manager's fingerprint: pairing runs pinned to it.
    let (ws, est) = session::connect(&attempt(port, Mode::Pair, k, mfp.clone())).await.expect("pairing");
    assert_eq!(est.fp, mfp);
    assert_eq!(est.link_key.as_ref().unwrap().to_hex(), link_key.to_hex());
    let (_tx, stop) = tokio::sync::watch::channel(0u64);
    let reason = tokio::time::timeout(Duration::from_secs(10), session::run(ws, Server::new(router()), stop, est.local_ip.clone()))
        .await
        .unwrap();
    assert!(reason.contains("closed"), "{reason}");
    let (got, pong) = mgr.await.unwrap();
    assert_eq!(got[0], json!({"t":"res","id":"1","status":200,"type":"application/json","body":{"uptime":"1"}}));
    assert_eq!(got[1]["status"], 403);
    assert_eq!(got[1]["body"]["error"], "Not allowed over the Sentry Manager link");
    assert_eq!(pong, json!({"t":"pong"}));
}

#[tokio::test]
async fn key_mode_with_the_pin_connects() {
    let lk = Secret::from_bytes([9u8; 32]);
    let mfp = fp(MANAGER_CERT);
    let (port, mgr) = manager(MANAGER_CERT, MANAGER_KEY, {
        let (lk, mfp) = (lk.clone(), mfp.clone());
        move |mut ws| async move { honest_handshake(&mut ws, &lk, &mfp, None).await }
    })
    .await;
    let (_, est) = session::connect(&attempt(port, Mode::Key, lk, mfp)).await.expect("key mode");
    assert!(est.link_key.is_none());
    assert_eq!(mgr.await.unwrap()["mode"], "key");
}

#[tokio::test]
async fn another_certificate_fails_the_pin() {
    // A fake Manager (another certificate) on the same address.
    let (port, _mgr) = manager(OTHER_CERT, OTHER_KEY, |mut ws| async move {
        let _ = ws.next().await;
    })
    .await;
    let r = session::connect(&attempt(port, Mode::Key, Secret::from_bytes([1; 32]), fp(MANAGER_CERT))).await;
    assert!(matches!(r.err(), Some(LinkError::CertMismatch(seen)) if seen == fp(OTHER_CERT)));
}

#[tokio::test]
async fn a_pinned_pi_does_not_pair_with_a_fake_manager_either() {
    // Pairing again (pin kept) refuses a different certificate even though
    // the fake knows K.
    let k = handshake::derive_pairing_key(USER, PASS);
    let (port, _mgr) = manager(OTHER_CERT, OTHER_KEY, |mut ws| async move {
        let _ = ws.next().await;
    })
    .await;
    let r = session::connect(&attempt(port, Mode::Pair, k, fp(MANAGER_CERT))).await;
    assert!(matches!(r.err(), Some(LinkError::CertMismatch(seen)) if seen == fp(OTHER_CERT)));
}

/// What a fake Manager saw: TLS handshakes completed, WebSocket upgrades,
/// and every `auth` frame (each carries a proof).
#[derive(Default)]
struct Seen {
    tls: AtomicUsize,
    upgrades: AtomicUsize,
    auths: Mutex<Vec<Value>>,
}

/// A Manager on a random port that accepts any number of connections and
/// records what each sends. It knows K and plays the handshake honestly
/// over `claimed_fp`, so a proof is all it would need to guess the
/// password offline.
async fn recording_manager(cert: &'static [u8], key: &'static [u8], claimed_fp: String) -> (u16, Arc<Seen>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let acc = acceptor(cert, key);
    let seen = Arc::new(Seen::default());
    let k = handshake::derive_pairing_key(USER, PASS);
    tokio::spawn({
        let seen = seen.clone();
        async move {
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let (acc, seen, k, claimed) = (acc.clone(), seen.clone(), k.clone(), claimed_fp.clone());
                tokio::spawn(async move {
                    let Ok(tls) = acc.accept(tcp).await else { return };
                    seen.tls.fetch_add(1, Ordering::SeqCst);
                    let Ok(mut ws) = tokio_tungstenite::accept_async(tls).await else { return };
                    seen.upgrades.fetch_add(1, Ordering::SeqCst);
                    let mn = handshake::random_hex32().unwrap();
                    send(&mut ws, json!({"t":"hello","v":1,"nonce":mn,"fp":claimed})).await;
                    let Some(Ok(Message::Text(t))) = ws.next().await else { return };
                    let auth: Value = serde_json::from_str(t.as_str()).unwrap();
                    seen.auths.lock().unwrap().push(auth.clone());
                    let pn = auth["nonce"].as_str().unwrap_or("");
                    let proof = handshake::mgr_proof(&k, &mn, pn, &claimed, DEVICE);
                    send(&mut ws, json!({"t":"ok","proof":proof,"linkKey":"33".repeat(32)})).await;
                });
            }
        }
    });
    (port, seen)
}

#[tokio::test]
async fn reading_the_fingerprint_sends_no_proof() {
    // Before the owner confirms anything, the Pi only reads the certificate.
    let (port, seen) = recording_manager(OTHER_CERT, OTHER_KEY, fp(OTHER_CERT)).await;
    let got = session::read_fingerprint("127.0.0.1", port).await.expect("fingerprint");
    assert_eq!(got, fp(OTHER_CERT));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(seen.tls.load(Ordering::SeqCst), 1, "one TLS handshake");
    assert_eq!(seen.upgrades.load(Ordering::SeqCst), 0, "no WebSocket upgrade");
    assert!(seen.auths.lock().unwrap().is_empty(), "no auth, so no proof");
}

#[tokio::test]
async fn a_handshake_never_runs_without_a_confirmed_pin() {
    let (port, seen) = recording_manager(MANAGER_CERT, MANAGER_KEY, fp(MANAGER_CERT)).await;
    let k = handshake::derive_pairing_key(USER, PASS);
    for pin in ["", "not a fingerprint"] {
        let r = session::connect(&attempt(port, Mode::Pair, k.clone(), pin.into())).await;
        assert!(matches!(r.err(), Some(LinkError::Io(_))));
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(seen.tls.load(Ordering::SeqCst), 0, "not even TLS");
    assert!(seen.auths.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_server_with_another_certificate_than_the_confirmed_one_gets_no_proof() {
    // The owner confirmed the real Manager's fingerprint; an impostor that
    // knows K (so it could check guesses) answers at the address, claiming
    // the real fingerprint or its own. The pin refuses it during TLS.
    let k = handshake::derive_pairing_key(USER, PASS);
    let real_fp = fp(MANAGER_CERT);
    for claimed in [real_fp.clone(), fp(OTHER_CERT)] {
        let (port, seen) = recording_manager(OTHER_CERT, OTHER_KEY, claimed).await;
        let r = session::connect(&attempt(port, Mode::Pair, k.clone(), real_fp.clone())).await;
        assert!(matches!(r.err(), Some(LinkError::CertMismatch(s)) if s == fp(OTHER_CERT)));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(seen.upgrades.load(Ordering::SeqCst), 0, "no WebSocket upgrade");
        assert!(seen.auths.lock().unwrap().is_empty(), "no proof sent to the impostor");
    }
}

#[tokio::test]
async fn a_confirmed_pin_pairs() {
    let (port, seen) = recording_manager(MANAGER_CERT, MANAGER_KEY, fp(MANAGER_CERT)).await;
    let confirmed = session::read_fingerprint("127.0.0.1", port).await.unwrap();
    let k = handshake::derive_pairing_key(USER, PASS);
    let (_ws, est) = session::connect(&attempt(port, Mode::Pair, k, confirmed)).await.expect("pairs");
    assert_eq!(est.fp, fp(MANAGER_CERT));
    assert_eq!(est.link_key.unwrap().to_hex(), "33".repeat(32));
    let auths = seen.auths.lock().unwrap();
    assert_eq!(auths.len(), 1);
    assert_eq!(auths[0]["mode"], "pair");
}

#[tokio::test]
async fn a_confirmed_server_claiming_another_fingerprint_fails() {
    // TLS terminated with the confirmed certificate, but hello names
    // another: someone else's handshake is relayed. Refused before auth.
    let (port, seen) = recording_manager(MANAGER_CERT, MANAGER_KEY, fp(OTHER_CERT)).await;
    let k = handshake::derive_pairing_key(USER, PASS);
    let r = session::connect(&attempt(port, Mode::Pair, k, fp(MANAGER_CERT))).await;
    assert!(matches!(r.err(), Some(LinkError::CertMismatch(_))));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(seen.auths.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_manager_without_the_key_is_rejected() {
    // Knows the protocol but not K (wrong password): it can only guess.
    let wrong = handshake::derive_pairing_key(USER, "not the password");
    let mfp = fp(MANAGER_CERT);
    let (port, _m) = manager(MANAGER_CERT, MANAGER_KEY, {
        let mfp = mfp.clone();
        move |mut ws| async move {
            let mn = handshake::random_hex32().unwrap();
            send(&mut ws, json!({"t":"hello","v":1,"nonce":mn,"fp":mfp})).await;
            let auth = recv(&mut ws).await;
            let pn = auth["nonce"].as_str().unwrap();
            let proof = handshake::mgr_proof(&wrong, &mn, pn, &mfp, DEVICE);
            send(&mut ws, json!({"t":"ok","proof":proof,"linkKey":"11".repeat(32)})).await;
        }
    })
    .await;
    let k = handshake::derive_pairing_key(USER, PASS);
    assert!(matches!(session::connect(&attempt(port, Mode::Pair, k, mfp)).await.err(), Some(LinkError::BadProof(_))));
}

#[tokio::test]
async fn a_replayed_ok_is_rejected() {
    // An `ok` recorded from an earlier connection carries a proof over the
    // earlier Pi nonce; this connection's nonce is new.
    let lk = Secret::from_bytes([5; 32]);
    let mfp = fp(MANAGER_CERT);
    let (port, _m) = manager(MANAGER_CERT, MANAGER_KEY, {
        let (lk, mfp) = (lk.clone(), mfp.clone());
        move |mut ws| async move {
            let mn = "00".repeat(32);
            send(&mut ws, json!({"t":"hello","v":1,"nonce":mn,"fp":mfp})).await;
            let _auth = recv(&mut ws).await;
            let old_pn = "ab".repeat(32);
            let proof = handshake::mgr_proof(&lk, &mn, &old_pn, &mfp, DEVICE);
            send(&mut ws, json!({"t":"ok","proof":proof})).await;
        }
    })
    .await;
    let r = session::connect(&attempt(port, Mode::Key, lk, mfp)).await;
    assert!(matches!(r.err(), Some(LinkError::BadProof(_))));
}

#[tokio::test]
async fn a_denied_answer_and_a_newer_version_are_reported() {
    let mfp = fp(MANAGER_CERT);
    let (port, _m) = manager(MANAGER_CERT, MANAGER_KEY, {
        let mfp = mfp.clone();
        move |mut ws| async move {
            send(&mut ws, json!({"t":"hello","v":1,"nonce":"00".repeat(32),"fp":mfp})).await;
            let _ = recv(&mut ws).await;
            send(&mut ws, json!({"t":"denied","why":"not paired"})).await;
        }
    })
    .await;
    let r = session::connect(&attempt(port, Mode::Key, Secret::from_bytes([1; 32]), mfp.clone())).await;
    assert_eq!(r.err(), Some(LinkError::Denied("not paired".into())));

    let (port, m) = manager(MANAGER_CERT, MANAGER_KEY, {
        let mfp = mfp.clone();
        move |mut ws| async move {
            send(&mut ws, json!({"t":"hello","v":2,"nonce":"00".repeat(32),"fp":mfp})).await;
            recv(&mut ws).await
        }
    })
    .await;
    let r = session::connect(&attempt(port, Mode::Key, Secret::from_bytes([1; 32]), mfp)).await;
    assert_eq!(r.err(), Some(LinkError::Version));
    assert_eq!(m.await.unwrap(), json!({"t":"denied","why":"version"}));
}

#[tokio::test]
async fn garbage_and_oversized_handshake_frames_end_the_attempt() {
    let mfp = fp(MANAGER_CERT);
    for bad in [
        Message::text("x".repeat(9 * 1024)),
        Message::text("not json"),
        Message::text(r#"{"t":"req","id":"1","method":"GET","path":"/api/status"}"#),
        Message::text(json!({"t":"hello","v":1,"nonce":"short","fp":mfp}).to_string()),
        Message::text(json!({"t":"hello","v":1,"nonce":"AA".repeat(32),"fp":mfp}).to_string()),
        Message::binary(vec![0u8; 16]),
    ] {
        let (port, _m) = manager(MANAGER_CERT, MANAGER_KEY, move |mut ws| async move {
            let _ = ws.send(bad).await;
            let _ = ws.next().await;
        })
        .await;
        let r = session::connect(&attempt(port, Mode::Key, Secret::from_bytes([1; 32]), mfp.clone())).await;
        assert!(matches!(r.err(), Some(LinkError::Protocol(_))));
    }

    // A frame over the 2 MiB read limit is refused while reading.
    let (port, _m) = manager(MANAGER_CERT, MANAGER_KEY, move |mut ws| async move {
        let _ = ws.send(Message::text("y".repeat(3 * 1024 * 1024))).await;
        let _ = ws.next().await;
    })
    .await;
    let r = session::connect(&attempt(port, Mode::Key, Secret::from_bytes([1; 32]), mfp)).await;
    let e = r.err();
    assert!(matches!(e, Some(LinkError::Io(_))), "{e:?}");
}

#[tokio::test]
async fn a_silent_manager_times_the_handshake_out() {
    // Slowloris: accept TLS and the upgrade, then never say hello.
    let (port, _m) = manager(MANAGER_CERT, MANAGER_KEY, |ws| async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        drop(ws);
    })
    .await;
    let t0 = std::time::Instant::now();
    let r = session::connect(&attempt(port, Mode::Key, Secret::from_bytes([1; 32]), fp(MANAGER_CERT))).await;
    assert_eq!(r.err(), Some(LinkError::Timeout));
    assert!(t0.elapsed() < session::HANDSHAKE_TIMEOUT + Duration::from_secs(2));
}

#[tokio::test]
async fn a_tcp_peer_that_never_speaks_tls_times_out() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let _h = tokio::spawn(async move {
        let (_s, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(30)).await;
    });
    let r = session::connect(&attempt(port, Mode::Key, Secret::from_bytes([1; 32]), fp(MANAGER_CERT))).await;
    assert_eq!(r.err(), Some(LinkError::Timeout));
}
