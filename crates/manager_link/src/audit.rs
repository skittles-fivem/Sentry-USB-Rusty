//! The Pi's security log for the link (contract 6b), and the alerts it
//! reports to Manager on the next good connection.
//!
//! JSON lines in `/mutable/sentryusb-manager-link.log` (the Logs page and
//! `GET /api/logs/manager-link` read it too). The file rotates to `.1` at
//! 512 KiB, so the two together stay within 1 MB. Entries never contain
//! a key, K, a password or a request body.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// One file's size limit; with the rotated copy the log stays under 1 MB.
pub const ROTATE_AT: u64 = 512 * 1024;
const MAX_FIELD: usize = 200;
/// Distinct alerts waiting for Manager, at most.
const MAX_ALERTS: usize = 20;

pub fn log_path() -> PathBuf {
    PathBuf::from(sentryusb_config::mutable_dir()).join("sentryusb-manager-link.log")
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Truncates text from the network (SSIDs, paths, error strings) so one
/// entry stays small. JSON encoding escapes the rest.
pub fn clip(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(MAX_FIELD).collect()
}

static WRITE: Mutex<()> = Mutex::new(());

/// Appends one entry: `{"at", "level", "kind", "msg", ...fields}`.
pub fn record(level: &str, kind: &str, msg: &str, fields: Value) {
    let mut entry = Map::new();
    entry.insert("at".into(), json!(now()));
    entry.insert("level".into(), json!(level));
    entry.insert("kind".into(), json!(kind));
    entry.insert("msg".into(), json!(clip(msg)));
    if let Value::Object(f) = fields {
        for (k, v) in f {
            let v = match v {
                Value::String(s) => Value::String(clip(&s)),
                other => other,
            };
            entry.entry(k).or_insert(v);
        }
    }
    let line = Value::Object(entry).to_string();
    if level == "warn" {
        tracing::warn!("[manager-link] {line}");
    }
    let _g = WRITE.lock().unwrap_or_else(|e| e.into_inner());
    let path = log_path();
    if std::fs::metadata(&path).is_ok_and(|m| m.len() >= ROTATE_AT) {
        let _ = std::fs::rename(&path, path.with_extension("log.1"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{line}");
    }
}

/// The newest `limit` entries, newest first, from both files.
pub fn recent(limit: usize) -> Vec<Value> {
    let path = log_path();
    let mut lines: Vec<Value> = Vec::new();
    for p in [path.with_extension("log.1"), path] {
        if let Ok(text) = std::fs::read_to_string(&p) {
            lines.extend(text.lines().filter_map(|l| serde_json::from_str(l).ok()));
        }
    }
    lines.reverse();
    lines.truncate(limit);
    lines
}

/// A failure Manager should hear about (fingerprint mismatch, bad proof).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Alert {
    pub kind: String,
    pub seen: String,
    pub at: i64,
}

/// A failure shown on the status page until Unpair.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Warning {
    pub kind: String,
    pub seen: String,
    pub address: String,
    pub first: i64,
    pub last: i64,
    pub count: u64,
}

const MAX_WARNINGS: usize = 10;

/// Queues an alert for the next good connection and keeps a warning for
/// the status page. Repeats of the same kind and fingerprint are kept once
/// (alerts with the first time; warnings counted), so a fake Manager can't
/// fill the file. The file is only written for a new kind and fingerprint.
pub fn queue_alert(kind: &str, seen: &str, address: &str) {
    let Ok(mut s) = crate::store::load() else { return };
    let seen = clip(seen);
    if let Some(w) = s.warnings.iter_mut().find(|w| w.kind == kind && w.seen == seen) {
        // Counted in memory only for repeats; persisted with the next new one.
        w.last = now();
        w.count += 1;
        if s.alerts.iter().any(|a| a.kind == kind && a.seen == seen) {
            return;
        }
    } else {
        if s.warnings.len() >= MAX_WARNINGS {
            s.warnings.remove(0);
        }
        s.warnings.push(Warning {
            kind: kind.into(), seen: seen.clone(), address: clip(address), first: now(), last: now(), count: 1,
        });
    }
    if !s.alerts.iter().any(|a| a.kind == kind && a.seen == seen) {
        if s.alerts.len() >= MAX_ALERTS {
            s.alerts.remove(0);
        }
        s.alerts.push(Alert { kind: kind.into(), seen, at: now() });
    }
    let _ = crate::store::save(&s);
}

/// Takes the queued alerts (called once they're about to be sent).
pub fn take_alerts() -> Vec<Alert> {
    let Ok(mut s) = crate::store::load() else { return Vec::new() };
    if s.alerts.is_empty() {
        return Vec::new();
    }
    let out = std::mem::take(&mut s.alerts);
    let _ = crate::store::save(&s);
    out
}

/// The network the Pi is on, for the `net` frame and the log.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Net {
    pub ssid: Option<String>,
    pub iface: Option<String>,
    pub ip: Option<String>,
    pub gw: Option<String>,
}

impl Net {
    pub fn frame(&self) -> Value {
        json!({"t": "net", "ssid": self.ssid, "iface": self.iface, "ip": self.ip, "gw": self.gw})
    }
}

/// Interface and gateway of the IPv4 default route, from /proc/net/route.
pub fn default_route() -> (Option<String>, Option<String>) {
    let text = std::fs::read_to_string("/proc/net/route").unwrap_or_default();
    parse_default_route(&text)
}

pub fn parse_default_route(text: &str) -> (Option<String>, Option<String>) {
    for line in text.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 3 || f[1] != "00000000" {
            continue;
        }
        let gw = u32::from_str_radix(f[2], 16)
            .ok()
            .filter(|g| *g != 0)
            // The kernel prints the address in host (little-endian) order.
            .map(|g| std::net::Ipv4Addr::from(g.swap_bytes()).to_string());
        return (Some(clip(f[0])), gw);
    }
    (None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_route_is_parsed() {
        let t = "Iface\tDestination\tGateway \tFlags\n\
                 wlan0\t0000A8C0\t00000000\t0001\n\
                 wlan0\t00000000\t0101A8C0\t0003\n";
        assert_eq!(parse_default_route(t), (Some("wlan0".into()), Some("192.168.1.1".into())));
        assert_eq!(parse_default_route("Iface\tDestination\n"), (None, None));
    }

    #[test]
    fn network_text_is_clipped_and_has_no_control_characters() {
        assert_eq!(clip("Home\n\u{1b}[31mWiFi"), "Home[31mWiFi");
        assert_eq!(clip(&"x".repeat(1000)).len(), MAX_FIELD);
    }
}
