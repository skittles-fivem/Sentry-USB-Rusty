//! Serving Manager's requests (contract 4): in-process against the Pi's own
//! API router, allowlisted, with secrets masked and answers chunked.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use base64::Engine;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::{mpsc, Notify, Semaphore};
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;

use crate::allowlist::{self, Method};
use crate::secrets;

/// Answers up to this size go in one `res` frame.
pub const CHUNK: usize = 1024 * 1024;
/// Hard cap on one answer.
pub const MAX_ANSWER: usize = 64 * 1024 * 1024;
/// Requests served at once (Manager allows 8 in flight).
pub const MAX_IN_FLIGHT: usize = 8;
/// Manager times out at 60 s; answer a little before that.
const HANDLER_TIMEOUT: Duration = Duration::from_secs(55);
const MAX_ID_LEN: usize = 64;

pub const NOT_ALLOWED: &str = "Not allowed over the Sentry Manager link";

/// Request serving for one connection.
pub struct Server {
    router: axum::Router,
    in_flight: Mutex<HashMap<String, tokio::task::AbortHandle>>,
    slots: Arc<Semaphore>,
    /// Fired after a successful write so fresh status goes out at once.
    pub changed: Arc<Notify>,
}

fn text(v: Value) -> Message {
    Message::text(v.to_string())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

fn res_json(id: &str, status: u16, body: Value) -> Message {
    text(json!({"t": "res", "id": id, "status": status, "type": "application/json", "body": body}))
}

impl Server {
    pub fn new(router: axum::Router) -> Arc<Self> {
        Arc::new(Server {
            router,
            in_flight: Mutex::new(HashMap::new()),
            slots: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
            changed: Arc::new(Notify::new()),
        })
    }

    /// Handles one text frame after the handshake. Returns a frame to send
    /// straight back, if any. Unknown types (including a stray `res`) and
    /// garbage are ignored.
    pub fn handle_frame(self: &Arc<Self>, frame: &str, tx: &mpsc::Sender<Message>) -> Option<Message> {
        let Ok(Value::Object(mut m)) = serde_json::from_str::<Value>(frame) else {
            tracing::debug!("[manager-link] ignored a frame that is not a JSON object");
            return None;
        };
        match m.get("t").and_then(Value::as_str) {
            Some("req") => {
                let id = m.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                if !valid_id(&id) {
                    tracing::warn!("[manager-link] request with an invalid id ignored");
                    return None;
                }
                let Ok(permit) = self.slots.clone().try_acquire_owned() else {
                    return Some(res_json(&id, 429, json!({"error": "Too many requests in flight"})));
                };
                let mut inflight = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
                if inflight.contains_key(&id) {
                    tracing::warn!("[manager-link] duplicate request id ignored");
                    return None;
                }
                let req = Value::Object(std::mem::take(&mut m));
                let me = self.clone();
                let tx = tx.clone();
                let id2 = id.clone();
                let task = tokio::spawn(async move {
                    me.serve(&id2, &req, &tx).await;
                    me.in_flight.lock().unwrap_or_else(|e| e.into_inner()).remove(&id2);
                    drop(permit);
                });
                inflight.insert(id, task.abort_handle());
                None
            }
            Some("ping") => Some(text(json!({"t": "pong"}))),
            _ => None,
        }
    }

    /// Aborts every request still running (the link went away).
    pub fn abort_all(&self) {
        for (_, h) in self.in_flight.lock().unwrap_or_else(|e| e.into_inner()).drain() {
            h.abort();
        }
    }

    /// `GET /api/status`, in-process.
    pub async fn status_body(&self) -> Option<Value> {
        let req = Request::get("/api/status").body(Body::empty()).expect("static request");
        let Ok(Ok(resp)) = tokio::time::timeout(HANDLER_TIMEOUT, self.router.clone().oneshot(req)).await else {
            return None;
        };
        if !resp.status().is_success() {
            return None;
        }
        let bytes = axum::body::to_bytes(resp.into_body(), CHUNK).await.ok()?;
        serde_json::from_slice::<Value>(&bytes).ok()
    }

    /// Sends `{"t":"status","body":<GET /api/status>}`.
    pub async fn push_status(&self, tx: &mpsc::Sender<Message>) {
        if let Some(body) = self.status_body().await {
            let _ = tx.send(text(json!({"t": "status", "body": body}))).await;
        }
    }

    async fn serve(&self, id: &str, req: &Value, tx: &mpsc::Sender<Message>) {
        let (status, frames) = self.execute(id, req, tx).await;
        if let Some(f) = frames {
            let _ = tx.send(f).await;
        }
        let method = crate::audit::clip(req.get("method").and_then(Value::as_str).unwrap_or("?"));
        let path = crate::audit::clip(req.get("path").and_then(Value::as_str).unwrap_or("?"));
        // Method, path and status only; bodies can hold secrets.
        let fields = json!({"method": method, "path": path, "status": status});
        if status == 403 {
            crate::audit::record("warn", "refused", "Request refused by the allowlist", fields);
        } else {
            crate::audit::record("info", "request", "Request served", fields);
        }
    }

    /// Runs one request. Returns its status for the log and, for small
    /// answers, the frame to send (large ones are sent while streaming).
    async fn execute(&self, id: &str, req: &Value, tx: &mpsc::Sender<Message>) -> (u16, Option<Message>) {
        let deny = || (403, Some(res_json(id, 403, json!({"error": NOT_ALLOWED}))));
        let Some(method) = req.get("method").and_then(Value::as_str).and_then(Method::parse) else {
            return deny();
        };
        let Some(path) = req.get("path").and_then(Value::as_str) else { return deny() };
        let query = match req.get("query") {
            None | Some(Value::Null) => "",
            Some(Value::String(q)) => q.as_str(),
            Some(_) => return deny(),
        };
        if !allowlist::is_allowed(method, path, query) {
            return deny();
        }
        let mut body = req.get("body").cloned().unwrap_or(Value::Null);
        if matches!(method, Method::Get | Method::Delete) {
            body = Value::Null;
        } else {
            let active = crate::store::read_config();
            match secrets::check_write(method, path, &body, &active, &crate::running_version()) {
                Ok(Some(b)) => body = b,
                Ok(None) => {}
                Err((status, msg)) => return (status, Some(res_json(id, status, json!({"error": msg})))),
            }
        }

        let uri = if query.is_empty() { path.to_string() } else { format!("{path}?{query}") };
        // Built from scratch: no header from Manager is copied, so nothing
        // like X-HTTP-Method-Override can reach the router. No ConnectInfo
        // either, so loopback-only handlers stay closed.
        let mut builder = Request::builder().method(method.as_str()).uri(&uri);
        let http_body = if body.is_null() {
            Body::empty()
        } else {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(body.to_string())
        };
        let Ok(request) = builder.body(http_body) else { return deny() };
        // The built URI must be exactly what was checked.
        if request.uri().path() != path || request.uri().query().unwrap_or("") != query {
            return deny();
        }

        let resp = match tokio::time::timeout(HANDLER_TIMEOUT, self.router.clone().oneshot(request)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => match e {},
            Err(_) => return (504, Some(res_json(id, 504, json!({"error": "Timed out on the Sentry USB"})))),
        };
        let status = resp.status();
        if method != Method::Get && status.is_success() {
            self.changed.notify_one();
        }
        let mask = method == Method::Get && path == "/api/setup/config";
        (status.as_u16(), send_answer(id, resp, mask, tx).await)
    }
}

fn is_json(ct: &str) -> bool {
    let ct = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    ct == "application/json" || ct.ends_with("+json")
}

fn res_frame(id: &str, status: StatusCode, ct: &str, bytes: &[u8]) -> Message {
    let mut frame = json!({"t": "res", "id": id, "status": status.as_u16(), "type": ct});
    if bytes.is_empty() {
        frame["body"] = Value::Null;
    } else if let (true, Ok(v)) = (is_json(ct), serde_json::from_slice::<Value>(bytes)) {
        frame["body"] = v;
    } else {
        frame["bodyB64"] = Value::String(base64::engine::general_purpose::STANDARD.encode(bytes));
    }
    text(frame)
}

/// Sends the handler's answer: one `res` frame up to 1 MiB, otherwise
/// `res` with `chunked: true`, then `chunk` frames of at most 1 MiB each
/// and a closing `end`. Streams, so a 64 MiB answer never sits in memory
/// whole. Returns the single frame for small answers.
async fn send_answer(
    id: &str,
    resp: axum::response::Response,
    mask: bool,
    tx: &mpsc::Sender<Message>,
) -> Option<Message> {
    let status = resp.status();
    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .chars()
        .take(100)
        .collect::<String>();

    if mask {
        // The setup config is small; read it whole and mask it. If it can't
        // be read as JSON it is not sent at all.
        return Some(match axum::body::to_bytes(resp.into_body(), CHUNK).await {
            Ok(b) => match serde_json::from_slice::<Value>(&b) {
                Ok(v) => text(json!({"t": "res", "id": id, "status": status.as_u16(),
                    "type": "application/json", "body": secrets::mask_setup_config(v)})),
                Err(_) => res_json(id, 502, json!({"error": "Could not read the settings"})),
            },
            Err(_) => res_json(id, 502, json!({"error": "Could not read the settings"})),
        });
    }

    let mut stream = resp.into_body().into_data_stream();
    let mut buf: Vec<u8> = Vec::new();
    // Fill up to one chunk; if the body ends first, it's a single frame.
    loop {
        match stream.next().await {
            Some(Ok(bytes)) => {
                buf.extend_from_slice(&bytes);
                if buf.len() > CHUNK {
                    break;
                }
            }
            Some(Err(_)) => return Some(res_json(id, 502, json!({"error": "The answer could not be read"}))),
            None => return Some(res_frame(id, status, &ct, &buf)),
        }
    }

    let header = json!({"t": "res", "id": id, "status": status.as_u16(), "type": ct, "chunked": true});
    if tx.send(text(header)).await.is_err() {
        return None;
    }
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut seq: u64 = 0;
    let mut total = 0usize;
    let mut error: Option<&str> = None;
    let mut done = false;
    while !done || !buf.is_empty() {
        while !done && buf.len() < CHUNK {
            match stream.next().await {
                Some(Ok(bytes)) => buf.extend_from_slice(&bytes),
                Some(Err(_)) => {
                    error = Some("read failed");
                    done = true;
                }
                None => done = true,
            }
        }
        if error.is_some() {
            break;
        }
        let take = buf.len().min(CHUNK);
        if take == 0 {
            break;
        }
        if total + take > MAX_ANSWER {
            error = Some("too large");
            break;
        }
        let piece: Vec<u8> = buf.drain(..take).collect();
        total += take;
        let frame = json!({"t": "chunk", "id": id, "seq": seq, "data": b64.encode(&piece)});
        if tx.send(text(frame)).await.is_err() {
            return None;
        }
        seq += 1;
    }
    let mut end = json!({"t": "end", "id": id});
    if let Some(e) = error {
        // The answer is incomplete; Manager must treat it as failed.
        end["error"] = Value::String(e.into());
    }
    let _ = tx.send(text(end)).await;
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::{get, post, put};

    fn router() -> axum::Router {
        axum::Router::new()
            .route("/api/status", get(|| async { axum::Json(json!({"ok": true})) }))
            .route("/api/terminal", get(|| async { "pwned" }))
            .route("/api/system/version", get(|| async { "v1" }))
            .route("/api/snapshots", get(|q: axum::extract::RawQuery| async move { q.0.unwrap_or_default() }))
            .route(
                "/api/system/backups",
                get(|q: axum::extract::RawQuery| async move {
                    // Answer as many bytes as asked for.
                    let n: usize = q.0.unwrap_or_default().trim_start_matches("n=").parse().unwrap_or(0);
                    (
                        [(header::CONTENT_TYPE, "application/octet-stream")],
                        (0..n).map(|i| (i % 251) as u8).collect::<Vec<u8>>(),
                    )
                }),
            )
            .route("/api/setup/run", post(|| async { axum::Json(json!({"status": "started"})) }))
            .route(
                "/api/system/update-source",
                put(|b: String| async move { axum::Json(json!({"got": b})) }),
            )
            .route(
                "/api/setup/config",
                get(|| async {
                    axum::Json(json!({
                        "WIFIPASS": {"value": "hunter2", "active": true},
                        "SSID": {"value": "Home", "active": true},
                    }))
                }),
            )
    }

    async fn call(server: &Arc<Server>, req: Value) -> Vec<Value> {
        let (tx, mut rx) = mpsc::channel(256);
        let mut out = Vec::new();
        if let Some(m) = server.handle_frame(&req.to_string(), &tx) {
            out.push(m);
        }
        drop(tx);
        while let Some(m) = rx.recv().await {
            out.push(m);
        }
        // Wait for the task to drop its sender.
        out.into_iter()
            .filter_map(|m| match m {
                Message::Text(t) => serde_json::from_str(t.as_str()).ok(),
                _ => None,
            })
            .collect()
    }

    fn req(method: &str, path: &str, query: Value, body: Value) -> Value {
        json!({"t": "req", "id": "r1", "method": method, "path": path, "query": query, "body": body})
    }

    #[tokio::test]
    async fn allowed_request_runs_in_process() {
        let s = Server::new(router());
        let out = call(&s, req("GET", "/api/status", Value::Null, Value::Null)).await;
        assert_eq!(out, vec![json!({"t":"res","id":"r1","status":200,"type":"application/json","body":{"ok":true}})]);
        let out = call(&s, req("GET", "/api/system/version", Value::Null, Value::Null)).await;
        assert_eq!(out[0]["bodyB64"], base64::engine::general_purpose::STANDARD.encode("v1"));
    }

    #[tokio::test]
    async fn anything_else_gets_the_contract_403() {
        let s = Server::new(router());
        for (m, p, q) in [
            ("GET", "/api/terminal", ""),
            ("GET", "/api/status/../terminal", ""),
            ("GET", "//api/terminal", ""),
            ("GET", "/api/%74erminal", ""),
            ("POST", "/api/status", ""),
            ("HEAD", "/api/status", ""),
            ("get", "/api/status", ""),
            ("GET", "/api/status", "x=1#/../../terminal"),
            ("GET", "/api/snapshots", "a=/api/terminal"),
        ] {
            let out = call(&s, req(m, p, json!(q), Value::Null)).await;
            assert_eq!(out.len(), 1, "{m} {p}");
            assert_eq!(out[0]["status"], 403, "{m} {p} {q}");
            assert_eq!(out[0]["body"]["error"], NOT_ALLOWED);
        }
        // A non-string query or path is refused too.
        let out = call(&s, json!({"t":"req","id":"r1","method":"GET","path":"/api/status","query":{"a":1}})).await;
        assert_eq!(out[0]["status"], 403);
        let out = call(&s, json!({"t":"req","id":"r1","method":"GET","path":["/api/status"]})).await;
        assert_eq!(out[0]["status"], 403);
    }

    #[tokio::test]
    async fn setup_config_secrets_are_masked() {
        let s = Server::new(router());
        let out = call(&s, req("GET", "/api/setup/config", Value::Null, Value::Null)).await;
        assert_eq!(out[0]["body"]["WIFIPASS"]["value"], "");
        assert_eq!(out[0]["body"]["WIFIPASS__set"], true);
        assert_eq!(out[0]["body"]["SSID"]["value"], "Home");
        assert!(!out[0].to_string().contains("hunter2"));
    }

    #[tokio::test]
    async fn update_source_write_is_restricted() {
        let s = Server::new(router());
        let out = call(&s, req("PUT", "/api/system/update-source", Value::Null, json!({"repo": "evil/x"}))).await;
        assert_eq!(out[0]["status"], 403);
        let out = call(&s, req("PUT", "/api/system/update-source", Value::Null, json!({"repo": ""}))).await;
        assert_eq!(out[0]["status"], 200);
    }

    #[tokio::test]
    async fn large_answers_are_chunked() {
        let s = Server::new(router());
        // Exactly 1 MiB: one frame.
        let out = call(&s, req("GET", "/api/system/backups", json!(format!("n={CHUNK}")), Value::Null)).await;
        assert_eq!(out.len(), 1);
        assert!(out[0]["bodyB64"].is_string());
        // 2.5 MiB: res + 3 chunks + end.
        let n = CHUNK * 5 / 2;
        let out = call(&s, req("GET", "/api/system/backups", json!(format!("n={n}")), Value::Null)).await;
        assert_eq!(out[0]["chunked"], true);
        assert!(out[0].get("body").is_none() && out[0].get("bodyB64").is_none());
        let chunks: Vec<&Value> = out.iter().filter(|f| f["t"] == "chunk").collect();
        assert_eq!(chunks.len(), 3);
        let mut all = Vec::new();
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c["seq"], i as u64);
            let d = base64::engine::general_purpose::STANDARD.decode(c["data"].as_str().unwrap()).unwrap();
            assert!(d.len() <= CHUNK);
            all.extend(d);
        }
        assert_eq!(all, (0..n).map(|i| (i % 251) as u8).collect::<Vec<u8>>());
        assert_eq!(out.last().unwrap(), &json!({"t": "end", "id": "r1"}));
    }

    #[tokio::test]
    async fn oversized_answers_are_cut_and_marked() {
        let s = Server::new(router());
        let n = MAX_ANSWER + 10;
        let out = call(&s, req("GET", "/api/system/backups", json!(format!("n={n}")), Value::Null)).await;
        let end = out.last().unwrap();
        assert_eq!(end["t"], "end");
        assert_eq!(end["error"], "too large");
        let sent: usize = out
            .iter()
            .filter(|f| f["t"] == "chunk")
            .map(|c| base64::engine::general_purpose::STANDARD.decode(c["data"].as_str().unwrap()).unwrap().len())
            .sum();
        assert!(sent <= MAX_ANSWER);
    }

    #[tokio::test]
    async fn pings_garbage_and_stray_frames() {
        let s = Server::new(router());
        let (tx, _rx) = mpsc::channel(4);
        assert!(matches!(s.handle_frame(r#"{"t":"ping"}"#, &tx), Some(Message::Text(t)) if t.as_str() == r#"{"t":"pong"}"#));
        for junk in [
            "", "not json", "[]", "{}", r#"{"t":"res","id":"nope","status":200}"#, r#"{"t":"pong"}"#,
            r#"{"t":"req","id":"","method":"GET","path":"/api/status"}"#,
            r#"{"t":"req","id":"has space","method":"GET","path":"/api/status"}"#,
        ] {
            assert!(s.handle_frame(junk, &tx).is_none(), "{junk}");
        }
    }

    #[tokio::test]
    async fn in_flight_requests_are_capped() {
        let r = axum::Router::new().route(
            "/api/status",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                "slow"
            }),
        );
        let s = Server::new(r);
        let (tx, _rx) = mpsc::channel(64);
        for i in 0..MAX_IN_FLIGHT {
            let f = json!({"t":"req","id":format!("a{i}"),"method":"GET","path":"/api/status"}).to_string();
            assert!(s.handle_frame(&f, &tx).is_none());
        }
        let f = json!({"t":"req","id":"over","method":"GET","path":"/api/status"}).to_string();
        let Some(Message::Text(t)) = s.handle_frame(&f, &tx) else { panic!("expected 429") };
        assert!(t.as_str().contains("429"));
        // A duplicate id of a running request is ignored.
        s.abort_all();
    }
}
