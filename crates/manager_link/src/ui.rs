//! Routes for Settings → Sentry Manager on the Pi's own web UI. They sit
//! under `/api/`, behind the Pi's normal web login, and are never on the
//! link's allowlist: Manager can't unpair, re-point or pair the Pi.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

fn err(status: StatusCode, msg: String) -> Response {
    (status, Json(json!({"error": msg}))).into_response()
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, Response> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "Internal error".into()))
}

/// GET /api/manager-link
pub async fn get_status() -> Response {
    match blocking(crate::status_json).await {
        Ok(v) => ([(axum::http::header::CACHE_CONTROL, "no-store")], Json(v)).into_response(),
        Err(r) => r,
    }
}

#[derive(Deserialize)]
pub struct SettingsBody {
    enabled: bool,
    #[serde(default)]
    address: String,
}

/// PUT /api/manager-link `{"enabled": bool, "address": "host[:port]" | ""}`
pub async fn put_settings(Json(body): Json<SettingsBody>) -> Response {
    if body.address.len() > 300 {
        return err(StatusCode::BAD_REQUEST, "Address is too long".into());
    }
    match blocking(move || crate::save_settings(body.enabled, &body.address)).await {
        Ok(Ok(())) => get_status().await,
        Ok(Err(e)) => err(StatusCode::BAD_REQUEST, e),
        Err(r) => r,
    }
}

/// POST /api/manager-link/pair: opens the 15-minute pairing window.
pub async fn post_pair() -> Response {
    match blocking(crate::start_pairing).await {
        Ok(Ok(())) => get_status().await,
        Ok(Err(e)) => err(StatusCode::BAD_REQUEST, e),
        Err(r) => r,
    }
}

#[derive(Deserialize)]
pub struct ConfirmBody {
    fingerprint: String,
}

/// POST /api/manager-link/confirm `{"fingerprint": "<the one shown>"}`: the
/// owner checked it matches Sentry Manager's, so pin it and pair.
///
/// Like pair and unpair it needs the web login, whose cookie is
/// `SameSite=Strict`, so another site can't send it. It also takes only a
/// JSON body (`Content-Type: application/json`, which a plain form can't
/// send cross-site without a CORS preflight the Pi never allows), and the
/// fingerprint must be the one being shown, which another site can't read.
pub async fn post_confirm(Json(body): Json<ConfirmBody>) -> Response {
    if body.fingerprint.len() > 200 {
        return err(StatusCode::BAD_REQUEST, "That isn't a fingerprint".into());
    }
    match blocking(move || crate::confirm_pairing(&body.fingerprint)).await {
        Ok(Ok(())) => get_status().await,
        Ok(Err(e)) => err(StatusCode::CONFLICT, e),
        Err(r) => r,
    }
}

/// POST /api/manager-link/unpair: forgets the key and the fingerprint.
pub async fn post_unpair() -> Response {
    match blocking(crate::unpair).await {
        Ok(Ok(())) => get_status().await,
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(r) => r,
    }
}

/// GET /api/manager-link/log: the Pi's security log for the link.
pub async fn get_log() -> Response {
    match blocking(|| crate::log_json(200)).await {
        Ok(v) => ([(axum::http::header::CACHE_CONTROL, "no-store")], Json(v)).into_response(),
        Err(r) => r,
    }
}
