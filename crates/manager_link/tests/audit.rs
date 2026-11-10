//! The Pi's security log: size cap and rotation, alerts and warnings.
//! Its own test binary because it sets process-wide environment variables.

use serde_json::json;
use sentryusb_manager_link::audit;

#[test]
fn log_rotation_alerts_and_warnings() {
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: set before anything in this binary reads them.
    unsafe {
        std::env::set_var("SENTRYUSB_MUTABLE_DIR", dir.path());
        std::env::set_var("SENTRYUSB_MANAGER_LINK_FILE", dir.path().join("link.json"));
    }

    // A warning first, then a flood of request lines (a busy or hostile
    // Manager) that rotates the log several times.
    audit::queue_alert("fingerprint", &"ab".repeat(32), "nas.local:10000");
    audit::record("warn", "fingerprint_mismatch", "Possible fake Sentry Manager", json!({"seen": "ab".repeat(32)}));
    for i in 0..12_000 {
        audit::record("info", "request", "Request served", json!({"method": "GET", "path": format!("/api/logs/x{i}"), "status": 200}));
    }
    let main = std::fs::metadata(audit::log_path()).unwrap().len();
    let old = std::fs::metadata(audit::log_path().with_extension("log.1")).unwrap().len();
    assert!(main <= audit::ROTATE_AT + 512, "{main}");
    assert!(old <= audit::ROTATE_AT + 512, "{old}");
    assert!(main + old <= 1024 * 1024 + 1024, "the log stays within 1 MB");

    // Newest first.
    let recent = audit::recent(3);
    assert_eq!(recent[0]["path"], "/api/logs/x11999");
    assert_eq!(recent[1]["path"], "/api/logs/x11998");

    // The warning is gone from the rotated log, but still on the status page.
    let page = sentryusb_manager_link::log_json(200);
    assert_eq!(page["warnings"][0]["kind"], "fingerprint_mismatch");
    assert_eq!(page["warnings"][0]["seen"], "ab".repeat(32));
    assert_eq!(page["warnings"][0]["address"], "nas.local:10000");

    // Hostile text is clipped and stripped of control characters.
    audit::record("info", "network", "Network in use", json!({"ssid": format!("evil\n{}", "x".repeat(500))}));
    let e = &audit::recent(1)[0];
    let ssid = e["ssid"].as_str().unwrap();
    assert!(!ssid.contains('\n') && ssid.chars().count() == 200);

    // Alerts: once per kind and fingerprint, at most 20, cleared once taken.
    for _ in 0..5 {
        audit::queue_alert("fingerprint", &"ab".repeat(32), "nas.local:10000");
    }
    for i in 0..30 {
        audit::queue_alert("proof", &format!("{i:064x}"), "nas.local:10000");
    }
    let alerts = audit::take_alerts();
    assert_eq!(alerts.len(), 20);
    assert!(alerts.iter().all(|a| a.at > 0));
    assert!(audit::take_alerts().is_empty());
    let warnings = sentryusb_manager_link::log_json(10)["warnings"].as_array().unwrap().len();
    assert_eq!(warnings, 10, "warnings are capped");
}
