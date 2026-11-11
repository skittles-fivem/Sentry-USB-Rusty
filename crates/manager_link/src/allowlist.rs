//! Section 5 of the contract: the routes Manager may call, deny by default.
//!
//! A request is matched against this table only after its path and query
//! pass a strict canonical-form check, so no spelling (`..`, `//`,
//! percent-encoding, a trailing slash, a smuggled `?` or `#`) can reach a
//! handler the table doesn't name. The link builds the request itself and
//! copies no header from Manager, so method-override headers have nothing
//! to ride on.

/// HTTP methods the link carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Delete,
}

impl Method {
    pub fn parse(s: &str) -> Option<Method> {
        // Exact, uppercase: "get" or "GET " is not a method here.
        match s {
            "GET" => Some(Method::Get),
            "POST" => Some(Method::Post),
            "PUT" => Some(Method::Put),
            "DELETE" => Some(Method::Delete),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
        }
    }
}

use Method::*;

/// The allowlist. `{}` is one path segment of `[A-Za-z0-9._-]`, never
/// `.` or `..`. Each entry is checked against `crates/api/src/router.rs` by
/// a test, so a route that doesn't exist can't sit here unnoticed.
///
/// Left out on purpose, although the contract's table listed them:
/// - `GET /api/logs`: no such route exists in the router.
/// - `GET /api/system/backup/{date}`: a backup holds the whole config with
///   every password, the SSH private key and the Tesla BLE private key
///   (which opens the car). It never leaves the Pi over the link.
pub const ROUTES: &[(Method, &str)] = &[
    // Status
    (Get, "/api/health"),
    (Get, "/api/status"),
    (Get, "/api/status/storage"),
    (Get, "/api/profile"),
    (Get, "/api/config"),
    (Get, "/api/memory"),
    (Get, "/api/system/version"),
    (Get, "/api/system/clock-status"),
    (Get, "/api/system/rtc-status"),
    (Get, "/api/system/ping"),
    (Get, "/api/backingfiles/free-space"),
    // Settings
    (Get, "/api/setup/config"),
    (Put, "/api/setup/config"),
    (Get, "/api/setup/status"),
    (Get, "/api/setup/phases"),
    (Post, "/api/setup/run"),
    (Post, "/api/setup/test-archive"),
    (Post, "/api/setup/preflight"),
    (Get, "/api/config/preference"),
    (Put, "/api/config/preference"),
    (Get, "/api/notifications/settings"),
    (Put, "/api/notifications/settings"),
    (Get, "/api/system/keep-accessory-config"),
    (Put, "/api/system/keep-accessory-config"),
    (Get, "/api/system/update-source"),
    // PUT only back to the official source; see `secrets::check_write`.
    (Put, "/api/system/update-source"),
    // Diagnostics
    (Get, "/api/diagnostics"),
    (Post, "/api/diagnostics/refresh"),
    (Post, "/api/diagnostics/download"),
    (Get, "/api/system/health-check"),
    (Get, "/api/storage/health"),
    (Get, "/api/logs/{}"),
    (Get, "/api/logs/{}/page"),
    (Get, "/api/logs/{}/tail"),
    // Updates
    (Post, "/api/system/check-update"),
    (Post, "/api/system/update"),
    (Get, "/api/system/update-status"),
    (Get, "/api/system/check-internet"),
    // Actions
    (Post, "/api/system/reboot"),
    (Post, "/api/system/shutdown"),
    (Post, "/api/system/trigger-sync"),
    (Post, "/api/system/cancel-archive"),
    (Post, "/api/system/toggle-drives"),
    (Post, "/api/system/gadget-enable"),
    (Post, "/api/system/gadget-disable"),
    // Snapshots
    (Get, "/api/snapshots"),
    (Get, "/api/snapshots/live"),
    (Delete, "/api/snapshots/{}"),
    // Backups
    (Post, "/api/system/backup"),
    (Get, "/api/system/backups"),
];

pub const MAX_PATH_LEN: usize = 256;
pub const MAX_QUERY_LEN: usize = 2048;
const MAX_SEGMENT_LEN: usize = 128;

fn is_segment_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')
}

/// A path is canonical when it is `/api/` followed by non-empty segments
/// of `[A-Za-z0-9._-]`, none of them `.` or `..`, with no trailing slash.
/// Anything else (percent-escapes, backslashes, `//`, `?`, `#`, spaces,
/// control characters, non-ASCII) is refused before any matching.
pub fn path_is_canonical(path: &str) -> bool {
    if path.len() > MAX_PATH_LEN || !path.starts_with("/api/") {
        return false;
    }
    path[1..].split('/').all(|seg| {
        !seg.is_empty()
            && seg.len() <= MAX_SEGMENT_LEN
            && seg != "."
            && seg != ".."
            && seg.bytes().all(is_segment_char)
    })
}

/// A query is plain `key=value&...` text: unreserved characters plus
/// `%`-escapes, `=`, `&`, `+`, `,` and `:`. No `#`, `?`, `/`, spaces or
/// control characters, so it can never change the path once joined.
pub fn query_is_safe(query: &str) -> bool {
    if query.len() > MAX_QUERY_LEN {
        return false;
    }
    let b = query.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'%' {
            // A percent-escape must be complete.
            if i + 2 >= b.len() || !b[i + 1].is_ascii_hexdigit() || !b[i + 2].is_ascii_hexdigit() {
                return false;
            }
            i += 3;
            continue;
        }
        if !(c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'~' | b'=' | b'&' | b'+' | b',' | b':')) {
            return false;
        }
        i += 1;
    }
    true
}

fn pattern_matches(pattern: &str, path: &str) -> bool {
    let mut p = pattern.split('/');
    let mut s = path.split('/');
    loop {
        match (p.next(), s.next()) {
            (None, None) => return true,
            (Some(ps), Some(ss)) => {
                if ps == "{}" {
                    // Path is already canonical, so the segment is safe.
                    continue;
                }
                if ps != ss {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

/// Logs Manager may read. The router falls back to `/var/log/<name>` for
/// any other name; over the link only the named logs are served, so no
/// file that happens to sit in /var/log goes out.
pub const LINK_LOGS: &[&str] = &[
    "archiveloop", "manager-link", "setup", "diagnostics", "syslog", "kern", "auth", "daemon",
    "sentryusb", "sentryusb-ble", "bluetooth",
];

fn log_name_ok(path: &str) -> bool {
    match path.strip_prefix("/api/logs/") {
        Some(rest) => LINK_LOGS.contains(&rest.split('/').next().unwrap_or("")),
        None => true,
    }
}

/// Whether the link may carry this request. The caller must also have
/// checked `path_is_canonical` and `query_is_safe`; this re-checks them.
pub fn is_allowed(method: Method, path: &str, query: &str) -> bool {
    path_is_canonical(path)
        && query_is_safe(query)
        && log_name_ok(path)
        && ROUTES.iter().any(|(m, p)| *m == method && pattern_matches(p, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listed_routes_pass() {
        assert!(is_allowed(Get, "/api/status", ""));
        assert!(is_allowed(Get, "/api/logs/archiveloop/page", "lines=200"));
        assert!(is_allowed(Get, "/api/logs/manager-link", ""));
        assert!(is_allowed(Delete, "/api/snapshots/snap-000123", ""));
        assert!(is_allowed(Put, "/api/setup/config", ""));
        assert!(is_allowed(Post, "/api/system/check-update", "include_prerelease=true"));
    }

    #[test]
    fn wrong_method_or_unlisted_route_is_denied() {
        assert!(!is_allowed(Post, "/api/status", ""));
        assert!(!is_allowed(Delete, "/api/setup/config", ""));
        assert!(!is_allowed(Get, "/api/snapshots/x/y", ""));
        for path in [
            "/api/terminal",
            "/api/auth/login",
            "/api/auth/check",
            "/api/files/ls",
            "/api/files",
            "/api/files/upload",
            "/api/system/ssh-keygen",
            "/api/system/ssh-pubkey",
            "/api/system/restore",
            "/api/system/backup/2026-01-01",
            "/api/system/ble-pair",
            "/api/system/ble-reset-pair",
            "/api/wraps/admin/validate",
            "/api/lockchime/community/admin/validate",
            "/api/system/after-archive",
            "/api/manager-link",
            "/api/manager-link/unpair",
            "/api/manager-link/pair",
            "/api/manager-link/confirm",
            "/api/ws",
            "/api/logs",
            "/api/logs/bluetooth/bundle",
        ] {
            for m in [Get, Post, Put, Delete] {
                assert!(!is_allowed(m, path, ""), "{} {path} must be denied", m.as_str());
            }
        }
    }

    #[test]
    fn path_tricks_are_refused() {
        for path in [
            "/api/status/",
            "//api/status",
            "/api//status",
            "/api/./status",
            "/api/setup/../terminal",
            "/api/logs/../terminal",
            "/api/logs/..",
            "/api/logs/.",
            "/api/logs/%2e%2e/page",
            "/api/logs/a%2Fb",
            "/api/%73tatus",
            "/api/status?x=1",
            "/api/status#frag",
            "/api/status;x",
            "/api\\status",
            "/api/logs/a\\b",
            "/api/st atus",
            "/api/status\0",
            "/api/status\n",
            "/API/status",
            "api/status",
            "/api/snapshots/é",
            "",
            "/",
            "/api/",
            "http://x/api/status",
        ] {
            assert!(!path_is_canonical(path) || !is_allowed(Get, path, ""), "{path:?} slipped through");
            for m in [Get, Post, Put, Delete] {
                assert!(!is_allowed(m, path, ""), "{path:?}");
            }
        }
        let long = format!("/api/logs/{}", "a".repeat(300));
        assert!(!is_allowed(Get, &long, ""));
    }

    #[test]
    fn query_smuggling_is_refused() {
        for q in [
            "a=1#x",
            "a=1?b=2",
            "a=/api/terminal",
            "a=1 b",
            "a=1\r\nX-HTTP-Method-Override: DELETE",
            "a=%zz",
            "a=%2",
            "a=%",
            "a=\u{0}",
        ] {
            assert!(!query_is_safe(q), "{q:?}");
            assert!(!is_allowed(Get, "/api/status", q));
        }
        assert!(query_is_safe("offset=10&limit=200&q=a%20b+c"));
        assert!(!query_is_safe(&"a".repeat(MAX_QUERY_LEN + 1)));
    }

    #[test]
    fn only_named_logs_are_readable() {
        for name in ["shadow", "btmp", "wtmp", "sentryusb-cloud.log", "archiveloop.log", "journal"] {
            for tail in ["", "/page", "/tail"] {
                assert!(!is_allowed(Get, &format!("/api/logs/{name}{tail}"), ""), "{name}{tail}");
            }
        }
        for name in LINK_LOGS {
            assert!(is_allowed(Get, &format!("/api/logs/{name}/tail"), ""), "{name}");
        }
    }

    #[test]
    fn methods_parse_exactly() {
        assert_eq!(Method::parse("GET"), Some(Get));
        for bad in ["get", "HEAD", "PATCH", "OPTIONS", "CONNECT", "GET ", "TRACE", ""] {
            assert_eq!(Method::parse(bad), None, "{bad}");
        }
    }

    /// Every allowlisted route exists in the router, with that method.
    #[test]
    fn every_listed_route_exists_in_the_router() {
        let src = include_str!("../../api/src/router.rs");
        for (m, pattern) in ROUTES {
            let wanted = pattern.replace("{}", "{");
            // Find the `.route("<path>", ...)` registration.
            let found = src.match_indices(".route(").any(|(i, _)| {
                let rest = &src[i + 7..];
                let rest = rest.trim_start();
                let Some(rest) = rest.strip_prefix('"') else { return false };
                let Some(end) = rest.find('"') else { return false };
                let path = &rest[..end];
                let segs_eq = path.split('/').count() == wanted.split('/').count()
                    && path.split('/').zip(wanted.split('/')).all(|(a, b)| {
                        if b == "{" { a.starts_with('{') && a.ends_with('}') } else { a == b }
                    });
                if !segs_eq {
                    return false;
                }
                let tail = &rest[end..];
                let call = &tail[..tail.find("\n        .route(").unwrap_or(tail.len()).min(tail.len())];
                let m = match m { Get => "get(", Post => "post(", Put => "put(", Delete => "delete(" };
                call.contains(m)
            });
            assert!(found, "{} {pattern} is not a route in router.rs", m.as_str());
        }
    }
}
