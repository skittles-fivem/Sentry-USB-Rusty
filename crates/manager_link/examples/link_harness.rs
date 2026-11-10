//! Runs the real Sentry USB link client off the Pi, against a real Sentry
//! Manager, for end-to-end tests (`tools/manager-link-e2e.sh`).
//!
//! - The client is the crate's own `spawn`, unchanged. Its files go to a
//!   state directory instead of `/root` and `/mutable`, through the same
//!   environment overrides the crate's tests use (`SENTRYUSB_CONFIG_PATH`,
//!   `SENTRYUSB_MANAGER_LINK_FILE`, `SENTRYUSB_MUTABLE_DIR`).
//! - Behind it is a stand-in router: every allowlisted route, answering
//!   JSON shaped like the Pi's, without touching this machine. The Pi's real
//!   router needs the drive database, the USB gadget and the cloud uploader,
//!   and its action handlers really run `reboot`, `poweroff` and mounts.
//!   `/api/logs/manager-link` serves the client's real security log.
//! - Every event goes to stdout as one JSON line: the client's own security
//!   log entries (`src: "pi"`, `event` = the log's `kind`: `pairing_open`,
//!   `paired`, `connected`, `disconnected`, `attempt_failed`, `network`,
//!   `request`, `refused`, `fingerprint_mismatch`, `bad_proof`,
//!   `alert_sent`, `unpaired`), link state changes (`src: "harness"`,
//!   `event: "state"`), and every call into the router (`event: "handler"`;
//!   a `GET /api/status` call with no `request` entry after it is a status
//!   push).
//! - Commands on stdin, one per line: `pair [seconds]`, `unpair`,
//!   `reconnect`, `impostor`, `status`, `log`, `quit`.
//!
//! ```text
//! cargo run -p sentryusb-manager-link --example link_harness -- \
//!     --manager nas.local:10000 --user admin --password-env WEB_PASSWORD \
//!     [--pair-window 120] [--state-dir DIR] [--fake-manager-cert] [--version v-harness]
//! cargo run -p sentryusb-manager-link --example link_harness -- --print-routes
//! ```

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path as UrlPath, RawQuery, Request};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

/// The impostor's certificate: not the one Manager pinned.
const OTHER_CERT: &[u8] = include_bytes!("../tests/fixtures/other.cert.der");
const OTHER_KEY: &[u8] = include_bytes!("../tests/fixtures/other.key.der");

static OUT: Mutex<()> = Mutex::new(());

/// One JSON line on stdout.
fn emit(mut v: Value) {
    if v.get("src").is_none() {
        v["src"] = json!("harness");
    }
    let _g = OUT.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

struct Args {
    manager: String,
    user: String,
    password: String,
    pair_window: u64,
    state_dir: Option<PathBuf>,
    fake_manager_cert: bool,
    version: String,
}

fn usage() -> ! {
    eprintln!(
        "usage: link_harness --manager HOST:PORT --user USER (--password PASS | --password-env VAR)\n\
         \x20                   [--pair-window SECONDS (0 = don't open; default 900)] [--state-dir DIR]\n\
         \x20                   [--fake-manager-cert] [--version VERSION]"
    );
    std::process::exit(2);
}

/// `--print-routes`: every allowlisted route as a JSON line with a sample
/// request (`{}` filled in, a query and body where the route wants one), so
/// a script can send each through Manager's proxy. `PUT /api/setup/config`
/// has `"body": "from-get"`: send back the masked GET, values only.
fn print_routes() -> ! {
    use sentryusb_manager_link::allowlist::{Method, ROUTES};
    for (m, pattern) in ROUTES {
        let path = pattern.replace("{}", if pattern.starts_with("/api/logs") { "manager-link" } else { "snap-000042" });
        let (query, body) = match (m, path.as_str()) {
            (Method::Put, "/api/setup/config") => ("", json!("from-get")),
            (Method::Get, "/api/config/preference") => ("key=harness", Value::Null),
            (Method::Put, "/api/config/preference") => ("", json!({"key": "harness", "value": 1})),
            (Method::Put, "/api/notifications/settings") => ("", json!({"enabled": true})),
            (Method::Put, "/api/system/keep-accessory-config") => ("", json!({"enabled": true, "minutes": 15})),
            (Method::Put, "/api/system/update-source") => ("", json!({"repo": ""})),
            (Method::Post, "/api/setup/test-archive") => {
                ("", json!({"ARCHIVE_SYSTEM": "cifs", "ARCHIVE_SERVER": "nas.local", "SHARE_PASSWORD": ""}))
            }
            (Method::Get, p) if p.ends_with("/page") || p.ends_with("/tail") => ("lines=50", Value::Null),
            _ => ("", Value::Null),
        };
        println!("{}", json!({"method": m.as_str(), "path": path, "query": query, "body": body}));
    }
    std::process::exit(0);
}

fn parse_args() -> Args {
    if std::env::args().nth(1).as_deref() == Some("--print-routes") {
        print_routes();
    }
    let mut a = Args {
        manager: String::new(),
        user: String::new(),
        password: String::new(),
        pair_window: 900,
        state_dir: None,
        fake_manager_cert: false,
        version: "v-harness".into(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| usage());
        match flag.as_str() {
            "--manager" => a.manager = val(),
            "--user" => a.user = val(),
            "--password" => a.password = val(),
            "--password-env" => a.password = std::env::var(val()).unwrap_or_else(|_| usage()),
            "--pair-window" => a.pair_window = val().parse().unwrap_or_else(|_| usage()),
            "--state-dir" => a.state_dir = Some(PathBuf::from(val())),
            "--fake-manager-cert" => a.fake_manager_cert = true,
            "--version" => a.version = val(),
            _ => usage(),
        }
    }
    if a.manager.is_empty() || a.user.is_empty() {
        usage();
    }
    a
}

fn main() {
    let args = parse_args();
    let manager = sentryusb_manager_link::store::normalize_address(&args.manager).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(2);
    });
    // Kept for the process's life when no directory was given.
    let (_tmp, dir) = match &args.state_dir {
        Some(d) => {
            std::fs::create_dir_all(d).expect("state dir");
            // It holds the web password (sentryusb.conf) and the link key.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700));
            }
            (None, d.clone())
        }
        None => {
            let t = tempfile::tempdir().expect("temp dir");
            let p = t.path().to_path_buf();
            (Some(t), p)
        }
    };
    let conf = dir.join("sentryusb.conf");
    // SAFETY: set before the runtime or any other thread starts.
    unsafe {
        std::env::set_var("SENTRYUSB_CONFIG_PATH", &conf);
        std::env::set_var("SENTRYUSB_MANAGER_LINK_FILE", dir.join("manager-link.json"));
        std::env::set_var("SENTRYUSB_MUTABLE_DIR", &dir);
    }
    write_config(&conf, &args, &manager);

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(run(args, manager, dir));
}

/// A Pi config with the web login, the link on and Manager's address.
/// Other keys a previous run (or a PUT over the link) left are kept.
fn write_config(conf: &Path, args: &Args, manager: &str) {
    if !conf.exists() {
        std::fs::write(conf, "# sentryusb.conf for link_harness\n").expect("write config");
    }
    let path = conf.to_str().expect("utf-8 path");
    let (mut active, _) = sentryusb_config::parse_file(path).expect("read config");
    let defaults = [
        ("ARCHIVE_SYSTEM", "cifs"),
        ("ARCHIVE_SERVER", "nas.local"),
        ("SHARE_NAME", "TeslaCam"),
        ("SHARE_USER", "pi"),
        ("SHARE_PASSWORD", "share-secret"),
        ("SSID", "HomeWiFi"),
        ("WIFIPASS", "wifi-secret"),
        ("TIME_ZONE", "Europe/London"),
    ];
    for (k, v) in defaults {
        active.entry(k.into()).or_insert_with(|| v.into());
    }
    active.insert("WEB_USERNAME".into(), args.user.clone());
    active.insert("WEB_PASSWORD".into(), args.password.clone());
    active.insert(sentryusb_manager_link::store::ENABLED_KEY.into(), "true".into());
    active.insert(sentryusb_manager_link::store::ADDRESS_KEY.into(), manager.into());
    sentryusb_config::write_file(path, &active).expect("write config");
}

async fn run(args: Args, manager: String, dir: PathBuf) {
    let stored = sentryusb_manager_link::store::load().expect("link file");
    emit(json!({
        "event": "ready", "state_dir": dir, "manager": manager, "device_id": stored.device_id,
        "paired": stored.paired(), "fingerprint": stored.pin(),
    }));

    tokio::spawn(tail_log(sentryusb_manager_link::audit::log_path()));
    tokio::spawn(watch_state());

    let fake = Arc::new(Mutex::new(FakePi::default()));
    sentryusb_manager_link::spawn(stand_in_router(fake), args.version.clone());

    if args.pair_window > 0 {
        let r = sentryusb_manager_link::start_pairing_for(Duration::from_secs(args.pair_window));
        emit(json!({"event": "pair", "ok": r.is_ok(), "error": r.err()}));
    }
    if args.fake_manager_cert {
        tokio::spawn({
            let manager = manager.clone();
            async move {
                wait_state(|s| s["state"] == "connected" && s["paired"] == true, Duration::from_secs(3600)).await;
                impostor(&manager).await;
            }
        });
    }

    // Blocking stdin on its own thread (tokio's `io-std` isn't enabled here).
    let (line_tx, mut lines) = tokio::sync::mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lines() {
            let Ok(line) = line else { break };
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });
    loop {
        let Some(line) = lines.recv().await else {
            // No stdin (or it closed): keep running until killed.
            std::future::pending::<()>().await;
            return;
        };
        let mut words = line.split_whitespace();
        let r: Result<(), String> = match words.next() {
            None => continue,
            Some("pair") => {
                let secs = words.next().and_then(|s| s.parse().ok()).unwrap_or(900);
                sentryusb_manager_link::start_pairing_for(Duration::from_secs(secs))
            }
            Some("unpair") => sentryusb_manager_link::unpair(),
            // Off and on again: the next connection is a fresh handshake
            // (key mode once paired).
            Some("reconnect") => sentryusb_manager_link::save_settings(false, &manager)
                .and_then(|_| sentryusb_manager_link::save_settings(true, &manager)),
            Some("impostor") => {
                tokio::spawn({
                    let manager = manager.clone();
                    async move { impostor(&manager).await }
                });
                Ok(())
            }
            Some("status") => {
                emit(json!({"event": "status", "body": sentryusb_manager_link::status_json()}));
                Ok(())
            }
            Some("log") => {
                emit(json!({"event": "log", "body": sentryusb_manager_link::log_json(200)}));
                Ok(())
            }
            Some("quit") => {
                emit(json!({"event": "quit"}));
                std::process::exit(0);
            }
            Some(other) => Err(format!("unknown command {other:?}")),
        };
        emit(json!({"event": "command", "line": line, "ok": r.is_ok(), "error": r.err()}));
    }
}

/// Prints the client's security log as it grows (`src: "pi"`).
async fn tail_log(path: PathBuf) {
    let mut offset: u64 = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let mut partial = String::new();
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if len < offset {
            // Rotated to .log.1.
            offset = 0;
            partial.clear();
        }
        if len == offset {
            continue;
        }
        let Ok(data) = std::fs::read(&path) else { continue };
        let end = (len as usize).min(data.len());
        partial.push_str(&String::from_utf8_lossy(&data[offset as usize..end]));
        offset = end as u64;
        while let Some(nl) = partial.find('\n') {
            let line: String = partial.drain(..=nl).collect();
            if let Ok(Value::Object(mut m)) = serde_json::from_str::<Value>(line.trim()) {
                let kind = m.remove("kind").unwrap_or(Value::Null);
                m.insert("event".into(), kind);
                m.insert("src".into(), json!("pi"));
                emit(Value::Object(m));
            }
        }
    }
}

/// Emits `state` whenever the status page's state, paired flag or
/// fingerprint changes.
async fn watch_state() {
    let mut last = Value::Null;
    loop {
        let s = sentryusb_manager_link::status_json();
        let now = json!({
            "state": s["state"], "paired": s["paired"], "pairing": s["pairing"], "connected": s["connected"],
            "fingerprint": s["fingerprint"], "last_error": s["last_error"],
        });
        if now != last {
            let mut e = now.clone();
            e["event"] = json!("state");
            e["message"] = s["message"].clone();
            emit(e);
            last = now;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn wait_state(f: impl Fn(&Value) -> bool, max: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + max;
    while tokio::time::Instant::now() < deadline {
        if f(&sentryusb_manager_link::status_json()) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

/// A fake Manager with another certificate on 127.0.0.1: points the link at
/// it, waits for the pin to refuse it, then points the link back at the real
/// Manager, which then gets the `alert` frame.
async fn impostor(manager: &str) {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    let cfg = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("tls versions")
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(OTHER_CERT.to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(OTHER_KEY.to_vec())),
        )
        .expect("impostor cert");
    let acc = tokio_rustls::TlsAcceptor::from(Arc::new(cfg));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind impostor");
    let addr = format!("127.0.0.1:{}", listener.local_addr().expect("addr").port());
    let fp = sentryusb_manager_link::tls::sha256_hex(OTHER_CERT);
    let server = tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else { return };
            let acc = acc.clone();
            tokio::spawn(async move {
                let _ = acc.accept(tcp).await;
            });
        }
    });
    emit(json!({"event": "impostor_start", "address": addr, "fingerprint": fp}));
    let r = sentryusb_manager_link::save_settings(true, &addr);
    let refused = r.is_ok() && wait_state(|s| s["state"] == "cert_changed", Duration::from_secs(60)).await;
    emit(json!({"event": "impostor_refused", "ok": refused, "error": r.err()}));
    server.abort();
    let r = sentryusb_manager_link::save_settings(true, manager);
    emit(json!({"event": "impostor_end", "back_to": manager, "ok": r.is_ok(), "error": r.err()}));
}

// ---------------------------------------------------------------------------
// The stand-in router.

#[derive(Default)]
struct FakePi {
    /// Snapshot ids deleted over the link.
    deleted: Vec<String>,
    preference: HashMap<String, Value>,
    notifications: Option<Value>,
    keep_accessory: Option<Value>,
    update_source: String,
    drives_active: bool,
    actions: Vec<String>,
}

type Fake = Arc<Mutex<FakePi>>;

fn ok() -> Json<Value> {
    Json(json!({"success": true}))
}

fn now_unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn fake_lock(f: &Fake) -> std::sync::MutexGuard<'_, FakePi> {
    f.lock().unwrap_or_else(|e| e.into_inner())
}

/// Records each call as a `handler` event.
async fn trace(req: Request, next: Next) -> Response {
    let (method, path, query) =
        (req.method().to_string(), req.uri().path().to_string(), req.uri().query().map(str::to_string));
    let resp = next.run(req).await;
    emit(json!({"event": "handler", "method": method, "path": path, "query": query, "status": resp.status().as_u16()}));
    resp
}

fn action(fake: &Fake, name: &'static str) -> Json<Value> {
    fake_lock(fake).actions.push(name.into());
    emit(json!({"event": "action", "name": name}));
    ok()
}

fn stand_in_router(fake: Fake) -> Router {
    let f = || fake.clone();
    macro_rules! act {
        ($name:literal) => {{
            let fk = f();
            post(move || async move { action(&fk, $name) })
        }};
    }
    Router::new()
        // Status
        .route("/api/health", get(|| async { Json(json!({"status": "ok"})) }))
        .route("/api/status", get({
            let fk = f();
            move || async move { Json(status(&fk)) }
        }))
        .route("/api/status/storage", get(|| async {
            Json(json!({"total_bytes": 256_000_000_000u64, "cam_bytes": 120_000_000_000u64,
                "music_bytes": 8_000_000_000u64, "snapshots_bytes": 40_000_000_000u64, "free_bytes": 88_000_000_000u64}))
        }))
        .route("/api/profile", get(|| async { Json(json!({"model": "Model Y", "vin": "", "nickname": "Harness"})) }))
        .route("/api/config", get(|| async {
            Json(json!({"has_cam": "yes", "has_music": "yes", "has_lightshow": "no", "has_boombox": "no",
                "uses_ble": "no", "archive_system": "cifs"}))
        }))
        .route("/api/memory", get(|| async { Json(json!({"total_kb": 467_000, "available_kb": 280_000})) }))
        .route("/api/system/version", get(|| async {
            Json(json!({"version": "v-harness", "binary_version": env!("CARGO_PKG_VERSION"),
                "sbc_model": "Raspberry Pi Zero 2 W Rev 1.0", "boot_id": "00000000-0000-4000-8000-000000000000"}))
        }))
        .route("/api/system/clock-status", get(|| async { Json(json!({"synced": true, "source": "ntp", "now": now_unix()})) }))
        .route("/api/system/rtc-status", get(|| async { Json(json!({"present": false})) }))
        .route("/api/system/ping", get(|| async { Json(json!({"pong": true})) }))
        .route("/api/backingfiles/free-space", get(|| async { Json(json!({"free": 88_000_000_000u64, "total": 256_000_000_000u64})) }))
        // Settings
        .route("/api/setup/config", get(get_setup_config).put(put_setup_config))
        .route("/api/setup/status", get(|| async { Json(json!({"setup_finished": true, "running": false})) }))
        .route("/api/setup/phases", get(|| async { Json(json!({"phases": [], "running": false})) }))
        .route("/api/setup/run", act!("setup_run"))
        .route("/api/setup/test-archive", post(|Json(b): Json<Value>| async move {
            let keys: Vec<String> = b.as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default();
            Json(json!({"success": true, "reachable": true, "keys": keys}))
        }))
        .route("/api/setup/preflight", post(|| async { Json(json!({"ok": true, "problems": []})) }))
        .route("/api/config/preference", get({
            let fk = f();
            move |RawQuery(q): RawQuery| async move {
                let key = q.unwrap_or_default().split('&').find_map(|kv| kv.strip_prefix("key=").map(str::to_string)).unwrap_or_default();
                let v = fake_lock(&fk).preference.get(&key).cloned().unwrap_or(Value::Null);
                Json(json!({"key": key, "value": v}))
            }
        }).put({
            let fk = f();
            move |Json(b): Json<Value>| async move {
                let key = b["key"].as_str().unwrap_or("").to_string();
                fake_lock(&fk).preference.insert(key, b["value"].clone());
                ok()
            }
        }))
        .route("/api/notifications/settings", get({
            let fk = f();
            move || async move {
                Json(fake_lock(&fk).notifications.clone().unwrap_or_else(|| json!({"enabled": true, "events": {"archive_done": true}})))
            }
        }).put({
            let fk = f();
            move |Json(b): Json<Value>| async move {
                fake_lock(&fk).notifications = Some(b);
                ok()
            }
        }))
        .route("/api/system/keep-accessory-config", get({
            let fk = f();
            move || async move {
                Json(fake_lock(&fk).keep_accessory.clone().unwrap_or_else(|| json!({"enabled": false, "minutes": 30})))
            }
        }).put({
            let fk = f();
            move |Json(b): Json<Value>| async move {
                fake_lock(&fk).keep_accessory = Some(b);
                ok()
            }
        }))
        .route("/api/system/update-source", get({
            let fk = f();
            move || async move {
                let repo = fake_lock(&fk).update_source.clone();
                Json(json!({"repo": repo, "official": repo.is_empty() || repo == sentryusb_manager_link::OFFICIAL_UPDATE_REPO}))
            }
        }).put({
            let fk = f();
            move |Json(b): Json<Value>| async move {
                fake_lock(&fk).update_source = b["repo"].as_str().unwrap_or("").to_string();
                ok()
            }
        }))
        // Diagnostics
        .route("/api/diagnostics", get(|| async {
            ([("content-type", "text/plain; charset=utf-8")], "Sentry USB diagnostics (link_harness)\nall good\n")
        }))
        .route("/api/diagnostics/refresh", act!("diagnostics_refresh"))
        .route("/api/diagnostics/download", post(|| async {
            // Over 1 MiB, so the answer goes out chunked.
            let body: Vec<u8> = (0..1_300_000u32).map(|i| (i % 251) as u8).collect();
            ([("content-type", "application/zip")], body)
        }))
        .route("/api/system/health-check", get(|| async { Json(json!({"ok": true, "checks": [{"name": "disk", "ok": true}]})) }))
        .route("/api/storage/health", get(|| async { Json(json!({"state": "healthy", "repair_running": false})) }))
        .route("/api/logs/{name}", get(|UrlPath(name): UrlPath<String>| async move { log_text(&name) }))
        .route("/api/logs/{name}/page", get(|UrlPath(name): UrlPath<String>| async move { log_text(&name) }))
        .route("/api/logs/{name}/tail", get(|UrlPath(name): UrlPath<String>| async move { log_text(&name) }))
        // Updates
        .route("/api/system/check-update", post(|| async {
            Json(json!({"current": "v-harness", "latest": "v-harness", "update_available": false}))
        }))
        .route("/api/system/update", act!("update"))
        .route("/api/system/update-status", get(|| async { Json(json!({"state": "idle", "progress": 0})) }))
        .route("/api/system/check-internet", get(|| async { Json(json!({"online": true})) }))
        // Actions
        .route("/api/system/reboot", act!("reboot"))
        .route("/api/system/shutdown", act!("shutdown"))
        .route("/api/system/trigger-sync", act!("trigger_sync"))
        .route("/api/system/cancel-archive", act!("cancel_archive"))
        .route("/api/system/toggle-drives", post({
            let fk = f();
            move || async move {
                let mut g = fake_lock(&fk);
                g.drives_active = !g.drives_active;
                g.actions.push("toggle_drives".into());
                emit(json!({"event": "action", "name": "toggle_drives"}));
                ok()
            }
        }))
        .route("/api/system/gadget-enable", act!("gadget_enable"))
        .route("/api/system/gadget-disable", act!("gadget_disable"))
        // Snapshots
        .route("/api/snapshots", get({
            let fk = f();
            move || async move { Json(json!({"snapshots": snapshots(&fk)})) }
        }))
        .route("/api/snapshots/live", get(|| async { Json(json!({"active": false})) }))
        .route("/api/snapshots/{id}", delete({
            let fk = f();
            move |UrlPath(id): UrlPath<String>| async move {
                let mut g = fake_lock(&fk);
                if !snapshot_ids().contains(&id.as_str()) || g.deleted.contains(&id) {
                    return (StatusCode::NOT_FOUND, Json(json!({"error": "No such snapshot"}))).into_response();
                }
                g.deleted.push(id);
                ok().into_response()
            }
        }))
        // Backups
        .route("/api/system/backup", act!("backup"))
        .route("/api/system/backups", get(|| async { Json(json!({"backups": [{"date": "2026-11-01", "size": 48211}]})) }))
        // Real Pi routes the allowlist must keep out. If one of these ever
        // answers over the link, the allowlist is broken.
        .route("/api/terminal", get(|| async { "root shell" }))
        .route("/api/auth/check", get(|| async { Json(json!({"authenticated": true})) }))
        .route("/api/manager-link", get(|| async { Json(sentryusb_manager_link::status_json()) }))
        .route("/api/system/backup/{date}", get(|| async { "whole config with every password" }))
        .layer(axum::middleware::from_fn(trace))
}

fn snapshot_ids() -> [&'static str; 3] {
    ["snap-000041", "snap-000042", "snap-000043"]
}

fn snapshots(fake: &Fake) -> Vec<Value> {
    let g = fake_lock(fake);
    snapshot_ids()
        .iter()
        .filter(|id| !g.deleted.iter().any(|d| d == *id))
        .map(|id| json!({"id": id, "date": "2026-11-0".to_string() + &id[id.len() - 1..], "size": 2_147_483_648u64}))
        .collect()
}

fn status(fake: &Fake) -> Value {
    let g = fake_lock(fake);
    let left = snapshot_ids().len() - g.deleted.len();
    json!({
        "cpu_temp": "48312", "cpu_percent": 7.5,
        "num_snapshots": left.to_string(), "snapshot_oldest": "1762300800", "snapshot_newest": "1762560000",
        "total_space": "256000000000", "free_space": "88000000000", "uptime": "86400",
        "drives_active": if g.drives_active { "yes" } else { "no" }, "udc_state": "configured",
        "cam_last_write_secs": 12, "wifi_adapters": [],
        // Not this machine's Wi-Fi, so the `net` frame's ssid stays null
        // unless the link really runs over a wl* interface.
        "wifi_ssid": "HomeWiFi", "wifi_freq": "5180000000", "wifi_strength": "-52", "wifi_ip": "",
        "ether_ip": "", "ether_speed": "", "sbc_model": "Raspberry Pi Zero 2 W Rev 1.0", "fan_speed": "",
        "wifi_rx_bps": 0, "wifi_tx_bps": 0, "ether_rx_bps": 0, "ether_tx_bps": 0,
        "wifi_rate_state": "ok", "ether_rate_state": "absent",
        "wifi_sample_age_ms": 500, "ether_sample_age_ms": null, "sampled_at": now_unix(),
        "storage_health": {"state": "healthy"},
        "harness_actions": g.actions,
    })
}

/// The real security log for `manager-link`; a line of text for the others.
fn log_text(name: &str) -> Response {
    const NAMES: &[&str] = &[
        "archiveloop", "manager-link", "setup", "diagnostics", "syslog", "kern", "auth", "daemon", "sentryusb", "sentryusb-ble",
    ];
    if !NAMES.contains(&name) {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "Unknown log"}))).into_response();
    }
    let text = if name == "manager-link" {
        std::fs::read_to_string(sentryusb_manager_link::audit::log_path()).unwrap_or_default()
    } else {
        format!("{name}: link_harness stand-in log line\n")
    };
    ([("content-type", "text/plain; charset=utf-8")], text).into_response()
}

/// Like the Pi's: every key, `{"value", "active"}`.
async fn get_setup_config() -> Response {
    match sentryusb_config::parse_file(sentryusb_config::find_config_path()) {
        Ok((active, commented)) => {
            let mut m = serde_json::Map::new();
            for (k, v) in commented {
                m.insert(k, json!({"value": v, "active": false}));
            }
            for (k, v) in active {
                m.insert(k, json!({"value": v, "active": true}));
            }
            Json(Value::Object(m)).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": format!("Failed to read config: {e}")}))).into_response(),
    }
}

/// Like the Pi's: a full replacement of the active keys (the link has
/// already checked and merged it).
async fn put_setup_config(Json(body): Json<HashMap<String, String>>) -> (StatusCode, Json<Value>) {
    match sentryusb_config::write_file(sentryusb_config::find_config_path(), &body) {
        Ok(()) => (StatusCode::OK, ok()),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": format!("Failed to write config: {e}")}))),
    }
}
