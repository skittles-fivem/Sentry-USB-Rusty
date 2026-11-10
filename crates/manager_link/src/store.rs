//! Where the link keeps its state.
//!
//! - Device id, link key and pinned fingerprint: a root-only (0600) JSON
//!   file next to the Pi's other secrets (`/root/.sentryusb-manager-link.json`,
//!   beside `/root/.sentryusb-sessions.json`). Never in sentryusb.conf, which
//!   the web UI shows and exports.
//! - On/off and Manager's address: sentryusb.conf, as
//!   `SENTRY_MANAGER_ENABLED` and `SENTRY_MANAGER_ADDRESS` (empty means
//!   "the archive server's host, port 10000").

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::handshake::{is_hex64, Secret};

pub const ENABLED_KEY: &str = "SENTRY_MANAGER_ENABLED";
pub const ADDRESS_KEY: &str = "SENTRY_MANAGER_ADDRESS";
pub const DEFAULT_PORT: u16 = 10000;

/// The secrets file. `SENTRYUSB_MANAGER_LINK_FILE` moves it for tests and
/// off-Pi development.
pub fn secrets_path() -> PathBuf {
    if let Ok(p) = std::env::var("SENTRYUSB_MANAGER_LINK_FILE") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    PathBuf::from("/root/.sentryusb-manager-link.json")
}

#[derive(Default, Serialize, Deserialize, Clone)]
pub struct Stored {
    #[serde(default)]
    pub device_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paired_at: Option<i64>,
    /// Fingerprint mismatches and failed proofs not yet reported to Manager.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alerts: Vec<crate::audit::Alert>,
    /// The same failures, kept for the status page until Unpair (the log
    /// file rotates, and requests must not be able to push these out).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<crate::audit::Warning>,
}

impl std::fmt::Debug for Stored {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stored")
            .field("device_id", &self.device_id)
            .field("link_key", &self.link_key.as_ref().map(|_| "<redacted>"))
            .field("pin", &self.pin)
            .finish()
    }
}

impl Stored {
    pub fn link_key(&self) -> Option<Secret> {
        self.link_key.as_deref().and_then(Secret::from_hex)
    }

    pub fn pin(&self) -> Option<&str> {
        self.pin.as_deref().filter(|p| is_hex64(p))
    }

    /// Paired means both halves are there and well-formed.
    pub fn paired(&self) -> bool {
        self.link_key().is_some() && self.pin().is_some()
    }
}

fn valid_device_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Loads the file, creating the device id on first use. A damaged file is
/// treated as unpaired (keeping a valid device id when there is one).
pub fn load() -> anyhow::Result<Stored> {
    let path = secrets_path();
    let mut s: Stored = match std::fs::read(&path) {
        Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Stored::default(),
        Err(e) => return Err(e.into()),
    };
    if !valid_device_id(&s.device_id) {
        let mut id = crate::handshake::random_hex32()?;
        id.truncate(32);
        s.device_id = id;
        s.link_key = None;
        s.pin = None;
        save(&s)?;
    }
    Ok(s)
}

/// Writes the file atomically: a 0600 temp file, fsync, rename. The temp
/// file is created with mode 0600, so the key is never readable by others,
/// not even for a moment.
pub fn save(s: &Stored) -> anyhow::Result<()> {
    let path = secrets_path();
    remount_rw();
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_vec(s)?;
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        #[cfg(unix)]
        {
            // An older temp file could carry other bits.
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(&data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(())
}

/// Root is mounted read-only on the Pi; the repo's writers remount first.
fn remount_rw() {
    if std::env::var_os("SENTRYUSB_MANAGER_LINK_FILE").is_some() {
        return;
    }
    let _ = std::process::Command::new("/root/bin/remountfs_rw")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Settings from sentryusb.conf.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    /// What the user typed; empty means the default.
    pub address: String,
    /// The archive server's host with port 10000, if one is set up.
    pub default_address: Option<String>,
    pub web_username: String,
    pub web_password_set: bool,
}

impl Settings {
    pub fn from_config(active: &HashMap<String, String>) -> Self {
        let get = |k: &str| active.get(k).map(|v| v.trim().to_string()).unwrap_or_default();
        Settings {
            enabled: matches!(get(ENABLED_KEY).to_ascii_lowercase().as_str(), "true" | "yes" | "1" | "on"),
            address: get(ADDRESS_KEY),
            default_address: default_address(active),
            web_username: active.get("WEB_USERNAME").cloned().unwrap_or_default(),
            web_password_set: !get("WEB_USERNAME").is_empty()
                && active.get("WEB_PASSWORD").is_some_and(|p| !p.is_empty()),
        }
    }

    /// The address to connect to: what the user typed, else the default.
    pub fn effective_address(&self) -> Option<String> {
        if self.address.is_empty() { self.default_address.clone() } else { Some(self.address.clone()) }
    }
}

/// Active settings from sentryusb.conf (empty when it can't be read).
pub fn read_config() -> HashMap<String, String> {
    sentryusb_config::parse_file(sentryusb_config::find_config_path())
        .map(|(a, _)| a)
        .unwrap_or_default()
}

/// The archive server's host on port 10000. rclone remotes are not hosts.
pub fn default_address(active: &HashMap<String, String>) -> Option<String> {
    let system = active.get("ARCHIVE_SYSTEM").map(|s| s.trim()).unwrap_or("");
    if system == "rclone" || system == "none" {
        return None;
    }
    let server = active
        .get("ARCHIVE_SERVER")
        .filter(|s| !s.trim().is_empty())
        .or_else(|| active.get("RSYNC_SERVER"))
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())?;
    let (host, _) = split_host_port(server).ok()?;
    Some(join_host_port(&host, DEFAULT_PORT))
}

fn join_host_port(host: &str, port: u16) -> String {
    if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") }
}

fn valid_host(host: &str) -> bool {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
}

/// Splits `host`, `host:port`, `[v6]` or `[v6]:port`, tolerating a leading
/// `wss://` or `https://` and a trailing `/`. The port defaults to 10000.
/// Anything with a path, user info or odd characters is refused.
pub fn split_host_port(input: &str) -> Result<(String, u16), String> {
    let mut s = input.trim();
    for scheme in ["wss://", "https://"] {
        if let Some(rest) = s.strip_prefix(scheme) {
            s = rest;
        }
    }
    let s = s.strip_suffix('/').unwrap_or(s);
    let bad = || format!("{input:?} is not a host name, or host:port");
    if s.contains(['/', '@', '?', '#', ' ', '\\']) {
        return Err(bad());
    }
    let (host, port) = if let Some(rest) = s.strip_prefix('[') {
        let (h, after) = rest.split_once(']').ok_or_else(bad)?;
        h.parse::<std::net::Ipv6Addr>().map_err(|_| bad())?;
        let port = match after {
            "" => DEFAULT_PORT,
            p => p.strip_prefix(':').and_then(|p| p.parse().ok()).ok_or_else(bad)?,
        };
        (h.to_string(), port)
    } else if s.parse::<std::net::Ipv6Addr>().is_ok() {
        (s.to_string(), DEFAULT_PORT)
    } else if let Some((h, p)) = s.rsplit_once(':') {
        (h.to_string(), p.parse::<u16>().map_err(|_| bad())?)
    } else {
        (s.to_string(), DEFAULT_PORT)
    };
    if port == 0 || !valid_host(&host) {
        return Err(bad());
    }
    Ok((host.to_ascii_lowercase(), port))
}

/// Normalized `host:port` for storing.
pub fn normalize_address(input: &str) -> Result<String, String> {
    let (h, p) = split_host_port(input)?;
    Ok(join_host_port(&h, p))
}

/// `61EC B687 2005 ...`: 16 groups of 4 uppercase hex digits, the format
/// both UIs show so the two are easy to compare.
pub fn fingerprint_display(fp: &str) -> String {
    fp.as_bytes()
        .chunks(4)
        .map(|c| String::from_utf8_lossy(c).to_ascii_uppercase())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn default_address_comes_from_the_archive_server() {
        assert_eq!(default_address(&cfg(&[("ARCHIVE_SERVER", "nas.local")])).as_deref(), Some("nas.local:10000"));
        assert_eq!(default_address(&cfg(&[("ARCHIVE_SERVER", "192.168.1.5")])).as_deref(), Some("192.168.1.5:10000"));
        assert_eq!(
            default_address(&cfg(&[("ARCHIVE_SYSTEM", "rsync"), ("RSYNC_SERVER", "NAS")])).as_deref(),
            Some("nas:10000")
        );
        assert_eq!(default_address(&cfg(&[("ARCHIVE_SYSTEM", "rclone"), ("ARCHIVE_SERVER", "x")])), None);
        assert_eq!(default_address(&cfg(&[])), None);
        assert_eq!(default_address(&cfg(&[("ARCHIVE_SERVER", "fd00::5")])).as_deref(), Some("[fd00::5]:10000"));
    }

    #[test]
    fn addresses_parse_strictly() {
        assert_eq!(split_host_port("nas.local").unwrap(), ("nas.local".into(), 10000));
        assert_eq!(split_host_port("wss://nas.local:10443/").unwrap(), ("nas.local".into(), 10443));
        assert_eq!(split_host_port("[fd00::1]:9").unwrap(), ("fd00::1".into(), 9));
        for bad in ["", "nas.local:0", "nas.local:x", "a/b", "u@nas", "nas local", "-nas", "nas.local:10000/api", "[zz]"] {
            assert!(split_host_port(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn settings_read_from_config() {
        let s = Settings::from_config(&cfg(&[
            (ENABLED_KEY, "true"),
            ("ARCHIVE_SERVER", "nas"),
            ("WEB_USERNAME", "admin"),
            ("WEB_PASSWORD", "pw"),
        ]));
        assert!(s.enabled && s.web_password_set);
        assert_eq!(s.effective_address().as_deref(), Some("nas:10000"));
        let s = Settings::from_config(&cfg(&[("WEB_USERNAME", "admin")]));
        assert!(!s.enabled && !s.web_password_set);
    }

    #[test]
    fn fingerprint_is_grouped() {
        let fp = "61ecb6872005d76f9c85b9abee8c635c413381103bf5df251ec263fc331d6c27";
        assert_eq!(
            fingerprint_display(fp),
            "61EC B687 2005 D76F 9C85 B9AB EE8C 635C 4133 8110 3BF5 DF25 1EC2 63FC 331D 6C27"
        );
    }

    #[test]
    fn secrets_file_is_private_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("link.json");
        // SAFETY: tests that touch this variable all set the same value.
        unsafe { std::env::set_var("SENTRYUSB_MANAGER_LINK_FILE", &path) };
        let mut s = load().unwrap();
        assert_eq!(s.device_id.len(), 32);
        assert!(!s.paired());
        s.link_key = Some("40".repeat(32));
        s.pin = Some("ab".repeat(32));
        save(&s).unwrap();
        let again = load().unwrap();
        assert!(again.paired());
        assert_eq!(again.device_id, s.device_id);
        assert!(!format!("{again:?}").contains(&"40".repeat(32)));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
