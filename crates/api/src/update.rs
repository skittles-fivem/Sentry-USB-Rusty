//! OTA update: check for updates, run update, version info.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;

use crate::router::AppState;
use crate::status::get_sbc_model;

/// Persists the last update check for `get_update_status`.
const UPDATE_CHECK_CACHE: &str = "/tmp/sentryusb-update-check.json";

static UPDATE_RUNNING: AtomicBool = AtomicBool::new(false);

pub(crate) struct UpdateGuard {
    _lock: sentryusb_drives::archive_mount_lock::ArchiveMountGuard,
}
impl Drop for UpdateGuard {
    fn drop(&mut self) { UPDATE_RUNNING.store(false, Ordering::SeqCst); }
}
pub(crate) async fn acquire_update() -> anyhow::Result<UpdateGuard> {
    if UPDATE_RUNNING.swap(true, Ordering::SeqCst) { anyhow::bail!("Update already in progress"); }
    let result = tokio::task::spawn_blocking(|| sentryusb_drives::archive_mount_lock::acquire_path(
        std::path::Path::new("/tmp/sentryusb-update.lock"), std::time::Duration::ZERO)).await;
    match result {
        Ok(Ok(lock)) => Ok(UpdateGuard { _lock: lock }),
        _ => { UPDATE_RUNNING.store(false, Ordering::SeqCst); anyhow::bail!("Update already in progress or lock unavailable") }
    }
}

/// Keep fixed to preserve pseudonymous fingerprint continuity.
const TELEMETRY_SALT: &str = "SENTRYUSB_2026_PROD";

/// Cached hardware identity; installation IDs would double-count reinstalls.
pub(crate) fn get_fingerprint() -> &'static str {
    static CACHED: OnceLock<String> = OnceLock::new();
    CACHED.get_or_init(|| {
        let device_tree: Vec<_> = ["/sys/firmware/devicetree/base/serial-number", "/proc/device-tree/serial-number"]
            .iter().filter_map(|path| std::fs::read_to_string(path).ok()).collect();
        let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
        let serial = hardware_serial(device_tree.iter().map(String::as_str), &cpuinfo);
        match serial {
            Some(serial) => hash_serial(&serial),
            None => {
                tracing::warn!("[device-report] no stable hardware serial; device analytics skipped");
                String::new()
            }
        }
    }).as_str()
}

fn hardware_serial<'a>(device_tree: impl IntoIterator<Item = &'a str>, cpuinfo: &'a str) -> Option<String> {
    let cpu_serial = cpuinfo.lines().filter_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key.trim() == "Serial").then_some(value)
    });
    device_tree.into_iter().chain(cpu_serial).map(|value| value.trim_matches(|c: char| c == '\0' || c.is_whitespace()))
        .find(|value| {
            let digits = value.strip_prefix("0x").or_else(|| value.strip_prefix("0X")).unwrap_or(value);
            !value.is_empty() && !digits.chars().all(|c| c == '0')
                && !value.eq_ignore_ascii_case("unknown") && value.len() <= 128
                && value.chars().all(|c| c.is_ascii_alphanumeric())
        })
        .map(str::to_owned)
}

fn hash_serial(serial: &str) -> String {
    use ring::digest::{SHA256, digest};
    hex::encode(digest(&SHA256, format!("{}{}", serial, TELEMETRY_SALT).as_bytes()).as_ref())
}

/// GET /api/system/check-internet
pub async fn check_internet(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    use futures_util::future::select_ok;
    use std::time::Duration;
    use tokio::net::TcpStream;

    // Race HTTPS probes because some Pi-hole networks block direct DNS probes.
    let t = Duration::from_secs(2);
    let probes: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send>>> = vec![
        Box::pin(async move {
            tokio::time::timeout(t, TcpStream::connect("8.8.8.8:443")).await
                .map_err(|_| anyhow::anyhow!("timeout"))?.map_err(anyhow::Error::from)?;
            Ok(())
        }),
        Box::pin(async move {
            tokio::time::timeout(t, TcpStream::connect("1.1.1.1:443")).await
                .map_err(|_| anyhow::anyhow!("timeout"))?.map_err(anyhow::Error::from)?;
            Ok(())
        }),
    ];
    let connected = select_ok(probes).await.is_ok();
    (StatusCode::OK, Json(serde_json::json!({"connected": connected})))
}

/// Starts an OTA update. An optional `{"version":"vX.Y.Z"}` body selects a
/// release; an empty body installs the latest release.
pub async fn run_update(
    State(s): State<AppState>,
    body: String,
) -> (StatusCode, Json<serde_json::Value>) {
    let guard = match acquire_update().await {
        Ok(guard) => guard,
        Err(e) => return crate::json_error(StatusCode::CONFLICT, &e.to_string()),
    };
    match crate::update_attempt::load(&crate::update_attempt::current_path()) {
        Ok(Some(a)) if !a.terminal() => return crate::json_error(StatusCode::CONFLICT, "Previous automatic update is being verified at startup"),
        Err(_) => {
            if let Err(e) = crate::update_attempt::preserve_before_manual_repair(&crate::update_attempt::current_path()) {
                return crate::json_error(StatusCode::CONFLICT, &format!("Cannot preserve update recovery record before manual repair: {e}"));
            }
            tracing::warn!("Preserved unreadable automatic-update receipt; explicit manual repair may proceed");
        },
        _ => {},
    }

    // The client omits the body when installing the latest release.
    let target_version: Option<String> = if body.trim().is_empty() {
        None
    } else {
        serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("version").and_then(|s| s.as_str()).map(String::from))
            .filter(|s| !s.is_empty())
    };

    let hub = s.hub.clone();
    tokio::spawn(async move {
        let _guard = guard;
        hub.broadcast("update_status", serde_json::json!({"status": "running"}));

        let result = self_update(&hub, target_version).await;

        match result {
            Ok(msg) => {
                // A successful explicit manual update supersedes a failed
                // automatic attempt; its outcome queue remains independent.
                if let Err(e) = crate::update_attempt::clear_after_manual_repair(&crate::update_attempt::current_path()) {
                    hub.broadcast("update_status", serde_json::json!({"status":"error", "error":format!("Installed files but could not clear superseded recovery state: {e}")}));
                    return;
                }
                hub.broadcast("update_status", serde_json::json!({
                    "status": "complete",
                    "output": msg
                }));

                // Allow each WebSocket status to reach the client before rebooting.
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                hub.broadcast("update_status", serde_json::json!({
                    "status": "restarting",
                    "message": "Restarting Pi to apply update…"
                }));
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;

                match sentryusb_shell::run("reboot", &[]).await {
                    Ok(_) => {
                        let e = await_reboot().await;
                        hub.broadcast("update_status", serde_json::json!({"status":"error", "error":e.to_string()}));
                    }
                    Err(e) => hub.broadcast("update_status", serde_json::json!({"status":"error", "error":e.to_string()})),
                }
            }
            Err(e) => hub.broadcast("update_status", serde_json::json!({
                "status": "error",
                "error": e.to_string()
            })),
        }
    });

    (StatusCode::OK, Json(serde_json::json!({"status": "started"})))
}

/// Returns only if a requested reboot never got under way. Shutdown can stop
/// this server long after the deadline (its graceful drain waits on open
/// connections), so a system still stopping is progressing, not failed.
pub(crate) async fn await_reboot() -> anyhow::Error {
    await_reboot_with(std::time::Duration::from_secs(90), std::time::Duration::from_secs(5), system_stopping).await
}

async fn await_reboot_with<F: std::future::Future<Output = bool>>(
    deadline: std::time::Duration, poll: std::time::Duration, mut stopping: impl FnMut() -> F,
) -> anyhow::Error {
    tokio::time::sleep(deadline).await;
    while stopping().await { tokio::time::sleep(poll).await; }
    anyhow::anyhow!("Device did not reboot after update")
}

async fn system_stopping() -> bool {
    // is-system-running exits non-zero for every state except "running".
    let probe = tokio::process::Command::new("systemctl").arg("is-system-running").kill_on_drop(true).output();
    matches!(tokio::time::timeout(std::time::Duration::from_secs(10), probe).await,
        Ok(Ok(out)) if String::from_utf8_lossy(&out.stdout).trim() == "stopping")
}

/// Default OTA source.
const DEFAULT_UPDATE_REPO_NAME: &str = "Sentry-USB-Rusty";
pub const OFFICIAL_UPDATE_REPO: &str = "Sentry-Six/Sentry-USB-Rusty";

/// Resolves the OTA release repository from `REPO` in sentryusb.conf.
/// `REPO=owner` keeps the original repository name (owner/Sentry-USB-Rusty);
/// `REPO=owner/name` names a whole repository, such as a test fork with a
/// different name. A missing, empty or malformed value means official.
pub fn update_repo() -> String {
    let path = sentryusb_config::find_config_path();
    let (active, _commented) = sentryusb_config::parse_file(path).unwrap_or_default();
    match active.get("REPO").map(|s| s.trim()).filter(|s| !s.is_empty()) {
        None => OFFICIAL_UPDATE_REPO.to_string(),
        Some(value) => normalize_repo(value).unwrap_or_else(|| {
            tracing::warn!("REPO={value:?} is not a GitHub owner or owner/name; updating from {OFFICIAL_UPDATE_REPO}");
            OFFICIAL_UPDATE_REPO.to_string()
        }),
    }
}

/// `owner` or `owner/name` as `owner/name`, or `None` unless both parts use
/// only characters GitHub allows (owner: letters, digits and inner hyphens,
/// at most 39; name: letters, digits, `-`, `_`, `.`, at most 100, not `.`/`..`).
pub(crate) fn normalize_repo(value: &str) -> Option<String> {
    let value = value.trim();
    let (owner, name) = value.split_once('/').unwrap_or((value, DEFAULT_UPDATE_REPO_NAME));
    let owner_ok = (1..=39).contains(&owner.len()) && !owner.starts_with('-') && !owner.ends_with('-')
        && owner.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-');
    let name_ok = (1..=100).contains(&name.len()) && name != "." && name != ".."
        && name.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'));
    (owner_ok && name_ok).then(|| format!("{owner}/{name}"))
}

/// GET /api/system/update-source
pub async fn get_update_source() -> (StatusCode, Json<serde_json::Value>) {
    let repo = update_repo();
    (StatusCode::OK, Json(serde_json::json!({
        "repo": repo,
        "official": repo.eq_ignore_ascii_case(OFFICIAL_UPDATE_REPO),
        "official_repo": OFFICIAL_UPDATE_REPO,
    })))
}

/// PUT /api/system/update-source — `{"repo": "owner/name"}`; an empty repo
/// (or the official one) removes `REPO` so updates come from official again.
pub async fn set_update_source(body: String) -> (StatusCode, Json<serde_json::Value>) {
    let requested = serde_json::from_str::<serde_json::Value>(&body).ok()
        .and_then(|v| v.get("repo").and_then(|r| r.as_str()).map(|r| r.trim().to_string()));
    let Some(requested) = requested else {
        return crate::json_error(StatusCode::BAD_REQUEST, "invalid request body");
    };
    let repo = if requested.is_empty() { None } else {
        match normalize_repo(&requested) {
            Some(repo) if repo.eq_ignore_ascii_case(OFFICIAL_UPDATE_REPO) => None,
            Some(repo) => Some(repo),
            None => return crate::json_error(StatusCode::BAD_REQUEST,
                "Enter a GitHub repository as owner/name, for example skittles-fivem/Sentry-USB-Rusty."),
        }
    };
    let persist = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let config_path = sentryusb_config::find_config_path();
        let (mut active, _) = sentryusb_config::parse_file(config_path)?;
        match repo {
            Some(repo) => { active.insert("REPO".to_string(), repo); }
            // Omitting the key comments it out, restoring the official default.
            None => { active.remove("REPO"); }
        }
        let _ = std::process::Command::new("bash").args(["-c", "/root/bin/remountfs_rw"]).status();
        sentryusb_config::write_file(config_path, &active)
    }).await;
    match persist {
        Ok(Ok(())) => {
            // A cached check describes the previous source's releases.
            let _ = std::fs::remove_file(UPDATE_CHECK_CACHE);
            get_update_source().await
        }
        Ok(Err(e)) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("config write failed: {e}")),
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("config task failed: {e}")),
    }
}

/// Detects the release suffix selected by the boot picker or equivalent live
/// CPU rules. Userspace architecture takes precedence over kernel architecture.
pub(crate) async fn detect_release_suffix() -> anyhow::Result<String> {
    // Older pickers recorded fallback names, so accept only release suffixes.
    const KNOWN_SUFFIXES: &[&str] = &[
        "linux-arm64-a53",
        "linux-arm64-a72",
        "linux-arm64-a76",
        "linux-armv7",
        "linux-amd64",
    ];
    if let Ok(s) = std::fs::read_to_string("/opt/sentryusb/active-variant") {
        let trimmed = s.trim();
        if KNOWN_SUFFIXES.contains(&trimmed) {
            return Ok(trimmed.to_string());
        }
        if !trimmed.is_empty() {
            tracing::warn!(
                "active-variant contains {:?} (not a release suffix) — \
                 ignoring it and re-detecting from CPU",
                trimmed
            );
        }
    }

    // dpkg reflects userspace on systems with a 64-bit kernel and 32-bit rootfs.
    let family = if let Ok(out) = sentryusb_shell::run("dpkg", &["--print-architecture"]).await {
        match out.trim() {
            "arm64" => "aarch64",
            "armhf" => return Ok("linux-armv7".to_string()),
            "armel" => anyhow::bail!(
                "armv6 (armel / Pi Zero W / Pi 1) is no longer supported — \
                 SentryUSB requires Pi Zero 2 W or newer"
            ),
            "amd64" => return Ok("linux-amd64".to_string()),
            other => anyhow::bail!("unsupported userspace architecture: {}", other),
        }
    } else {
        let arch = sentryusb_shell::run("uname", &["-m"]).await?;
        match arch.trim() {
            "aarch64" => "aarch64",
            "armv7l" => return Ok("linux-armv7".to_string()),
            "armv6l" => anyhow::bail!(
                "armv6 (Pi Zero W / Pi 1) is no longer supported — \
                 SentryUSB requires Pi Zero 2 W or newer"
            ),
            "x86_64" => return Ok("linux-amd64".to_string()),
            other => anyhow::bail!("unsupported architecture: {}", other),
        }
    };

    // Mirror the boot picker's aarch64 rules on pre-picker installations.
    debug_assert_eq!(family, "aarch64");
    if let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo") {
        // The a76 build requires both LSE atomics and the crypto extension.
        let has_hwcap = |cap: &str| {
            cpuinfo.lines().any(|line| {
                line.starts_with("Features")
                    && line.split_whitespace().any(|w| w == cap)
            })
        };
        if has_hwcap("atomics") && has_hwcap("aes") {
            return Ok("linux-arm64-a76".to_string());
        }
        // 0xD08 = Cortex-A72 (Pi 4 / RK3399 perf cluster).
        for line in cpuinfo.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("CPU part") {
                let part = trimmed.split(':').nth(1).unwrap_or("").trim().to_ascii_lowercase();
                if part == "0xd08" {
                    return Ok("linux-arm64-a72".to_string());
                }
            }
        }
    }
    // Default for aarch64: Cortex-A53 (Pi 3, Pi Zero 2 W, Allwinner H618).
    Ok("linux-arm64-a53".to_string())
}

/// Streams a release file to staging and broadcasts advisory download progress.
async fn download_with_progress(
    hub: &sentryusb_ws::Hub,
    source: &crate::release_download::Source,
    dest: &str,
) -> Result<(), crate::release_download::FetchError> {
    let notify = |message: &str| {
        hub.broadcast(
            "update_status",
            serde_json::json!({"status": "downloading", "message": message, "percent": null}),
        );
    };
    let mut last_percent: i64 = -1;
    let mut last_emit = std::time::Instant::now();
    // CDN responses may omit Content-Length; the UI then uses indeterminate progress.
    let mut progress = |done: u64, total: Option<u64>| {
        let percent = total.map(|t| ((done as f64 / t as f64) * 100.0) as i64);
        let emit = match percent {
            Some(p) => p != last_percent,
            None => last_emit.elapsed() >= std::time::Duration::from_millis(500),
        };
        if emit {
            if let Some(p) = percent {
                last_percent = p;
            }
            last_emit = std::time::Instant::now();
            hub.broadcast(
                "update_status",
                serde_json::json!({
                    "status": "downloading",
                    "message": "Downloading update…",
                    "percent": percent,
                    "bytes_done": done,
                    "bytes_total": total,
                }),
            );
        }
    };
    crate::release_download::download(source, std::path::Path::new(dest), &notify, &mut progress).await
}

/// Downloads an optional companion binary. `Ok(false)` means the release
/// doesn't include it; `Err` is a warning for the update summary, because a
/// failed download leaves the old companion beside the new main binary.
async fn fetch_optional(
    source: Option<crate::release_download::Source>,
    name: &str,
    dest: &str,
    notify: &(dyn Fn(&str) + Sync),
) -> Result<bool, String> {
    use crate::release_download::FetchError;
    let Some(source) = source else { return Ok(false) };
    match crate::release_download::download(&source, std::path::Path::new(dest), notify, &mut |_, _| {}).await {
        Ok(()) => Ok(true),
        Err(FetchError::NotFound) => Ok(false),
        Err(e) => Err(format!(
            "{name} wasn't updated and still runs the previous version: {}",
            e.sentence()
        )),
    }
}

async fn self_update(
    hub: &sentryusb_ws::Hub,
    target_version: Option<String>,
) -> anyhow::Result<String> {
    hub.broadcast(
        "update_status",
        serde_json::json!({"status": "checking", "message": "Checking release…"}),
    );
    use crate::release_download::{self as rd, FetchError};

    let suffix = detect_release_suffix().await?;
    let repo = update_repo();
    let checking = |message: &str| {
        hub.broadcast("update_status", serde_json::json!({"status": "checking", "message": message}));
    };

    // The release listing pins every file to one tag (even for "latest"),
    // says which files the release has, and supplies the API download links
    // tried when GitHub's release links fail. A requested version supports
    // both prerelease installs and stable reverts.
    let release = match rd::lookup_release(&repo, target_version.as_deref(), &checking).await {
        Ok(r) => Some(r),
        Err(FetchError::NotFound) => anyhow::bail!(match &target_version {
            Some(v) => format!("GitHub has no release {v}. Check for updates again to see the current releases."),
            None => "GitHub has no published release to update to.".to_string(),
        }),
        Err(e) => {
            // The plain release links may still work.
            tracing::warn!("update: release listing unavailable ({}); using release links", e.reason());
            None
        }
    };
    // A non-semver tag (a fork's old `1.22`) can't be ordered against later
    // releases, so it is never installed, even as "latest".
    if let Some(tag) = release.as_ref().map(|r| r.tag.as_str()).or(target_version.as_deref())
        && parse_semver(tag).is_none()
    {
        anyhow::bail!("Release {tag} isn't tagged vX.Y.Z, so it can't be installed. {UNCHANGED}");
    }
    let file_tag = release.as_ref().map(|r| r.tag.clone()).or_else(|| target_version.clone());
    let source_for = |name: &str| match &release {
        Some(r) => r.source(&repo, name),
        None => Some(rd::Source::release_link(&repo, file_tag.as_deref(), name)),
    };

    const UNCHANGED: &str = "Your installed version wasn't changed.";
    let main_name = format!("sentryusb-{suffix}");
    let missing_build = || {
        let release = match &file_tag {
            Some(t) => format!("Release {t}"),
            None => "The latest release".to_string(),
        };
        anyhow::anyhow!("{release} doesn't include a build for this board ({main_name}). {UNCHANGED}")
    };
    let main = source_for(&main_name).ok_or_else(missing_build)?;
    // Fail before remounting anything if GitHub can't serve the binary.
    match rd::probe(&main, &checking).await {
        Ok(()) => {}
        Err(FetchError::NotFound) => return Err(missing_build()),
        Err(e) => anyhow::bail!("Couldn't reach the update on GitHub. {} {UNCHANGED}", e.sentence()),
    }

    // Try overlay-aware, standard, and legacy remount forms for supported images.
    hub.broadcast(
        "update_status",
        serde_json::json!({"status": "remounting", "message": "Preparing filesystem…"}),
    );
    let _ = sentryusb_shell::run("/root/bin/remountfs_rw", &[]).await;
    let _ = sentryusb_shell::run("mount", &["-o", "remount,rw", "/"]).await;
    let _ = sentryusb_shell::run("mount", &["/", "-o", "remount,rw"]).await;

    // Same-filesystem staging makes the final rename atomic across power loss.
    sentryusb_shell::run("mkdir", &["-p", "/opt/sentryusb"]).await?;
    let tmp = "/opt/sentryusb/.sentryusb-update.new";
    download_with_progress(hub, &main, tmp)
        .await
        .map_err(|e| anyhow::anyhow!("Couldn't download the update. {} {UNCHANGED}", e.sentence()))?;

    hub.broadcast(
        "update_status",
        serde_json::json!({"status": "installing", "message": "Installing update…"}),
    );
    sentryusb_shell::run("chmod", &["+x", tmp]).await?;
    // Check the staged executable before replacing the working binary.
    let target_help = sentryusb_shell::run_with_timeout(
        std::time::Duration::from_secs(15), tmp, &["--help"],
    ).await?;

    // Preserve the picker's multi-binary layout:
    //   /opt/sentryusb/sentryusb-{suffix}            ← we write here
    //   /opt/sentryusb/sentryusb-current → ↑         ← picker symlink
    //   /opt/sentryusb/sentryusb         → -current  ← back-compat symlink
    //
    // Pre-multi-binary installations still require the legacy destination.
    let dest = if std::path::Path::new("/opt/sentryusb/sentryusb-current").exists() {
        format!("/opt/sentryusb/sentryusb-{}", suffix)
    } else {
        "/opt/sentryusb/sentryusb".to_string()
    };
    sentryusb_shell::run("mv", &[tmp, &dest]).await?;

    // Activate the target's bundled archive scripts for the next boot without
    // depending on GitHub again or restarting the currently running archive.
    let mut install_warnings: Vec<String> = Vec::new();
    if target_help.contains("refresh-archive-scripts") {
        hub.broadcast("update_status", serde_json::json!({
            "status": "updating_scripts", "message": "Updating archive scripts…"
        }));
        if let Err(error) = sentryusb_shell::run_with_timeout(
            std::time::Duration::from_secs(30), &dest, &["refresh-archive-scripts"],
        ).await {
            // Finish recording the installed release so startup migration can
            // retry its scripts; do not leave a new binary with the old tag.
            install_warnings.push(format!("Bundled archive-script refresh failed: {error}; runtime patches and startup migration will retry."));
        }
    }

    // Keep the optional telemetry sampler on the main binary's release and ABI.
    hub.broadcast(
        "update_status",
        serde_json::json!({"status": "installing", "message": "Installing components…"}),
    );
    let installing = |message: &str| {
        hub.broadcast("update_status", serde_json::json!({"status": "installing", "message": message}));
    };
    let telemetry_name = format!("sentryusb-tesla-telemetry-{suffix}");
    // Stage beside the destination for an atomic rename.
    let telemetry_tmp = "/opt/sentryusb/.sentryusb-tesla-telemetry-update.new";
    match fetch_optional(source_for(&telemetry_name), &telemetry_name, telemetry_tmp, &installing).await {
        Ok(true) => {
            // The picker repairs this per-CPU symlink when an SD card moves hosts.
            if let Err(e) =
                sentryusb_shell::run("mkdir", &["-p", "/root/bin"]).await
            {
                install_warnings.push(format!(
                    "telemetry: mkdir /root/bin failed: {e}"
                ));
            }
            if let Err(e) =
                sentryusb_shell::run("mkdir", &["-p", "/opt/sentryusb"]).await
            {
                install_warnings.push(format!(
                    "telemetry: mkdir /opt/sentryusb failed: {e}"
                ));
            }
            if let Err(e) =
                sentryusb_shell::run("chmod", &["+x", telemetry_tmp]).await
            {
                install_warnings.push(format!(
                    "telemetry: chmod +x failed: {e}"
                ));
            }
            let telemetry_dest =
                format!("/opt/sentryusb/sentryusb-tesla-telemetry-{}", suffix);
            // Move first so a failed install cannot leave a dangling symlink.
            let install_result =
                match sentryusb_shell::run("mv", &[telemetry_tmp, &telemetry_dest])
                    .await
                {
                    Ok(_) => {
                        sentryusb_shell::run(
                            "ln",
                            &[
                                "-sfn",
                                &telemetry_dest,
                                "/root/bin/sentryusb-tesla-telemetry",
                            ],
                        )
                        .await
                        .map(|_| ())
                    }
                    Err(e) => Err(e),
                };
            match install_result {
                Ok(_) => {
                    // Activate the new sampler without waiting for reboot.
                    let _ = sentryusb_shell::run(
                        "systemctl",
                        &["daemon-reload"],
                    )
                    .await;
                    let _ = sentryusb_shell::run(
                        "systemctl",
                        &["restart", "sentryusb-telemetry"],
                    )
                    .await;
                }
                Err(e) => {
                    install_warnings.push(format!(
                        "telemetry binary install FAILED \
                         (install to {telemetry_dest} + symlink from \
                         /root/bin/sentryusb-tesla-telemetry): \
                         {e} — likely read-only rootfs; if your image \
                         uses an overlay you may need to remount manually \
                         or wait for the next reboot"
                    ));
                    // Remove the failed staging file.
                    let _ = sentryusb_shell::run("rm", &["-f", telemetry_tmp]).await;
                }
            }
        }
        Err(warning) => install_warnings.push(warning),
        // Older releases may not include the optional sampler.
        Ok(false) => tracing::info!("release does not include {telemetry_name}; skipping"),
    }

    // Keep the optional BLE-action CLI on the main binary's protocol version.
    let action_name = format!("sentryusb-ble-action-{suffix}");
    // Stage beside the destination for an atomic rename.
    let action_tmp = "/opt/sentryusb/.sentryusb-ble-action-update.new";
    match fetch_optional(source_for(&action_name), &action_name, action_tmp, &installing).await {
        Ok(true) => {
            if let Err(e) =
                sentryusb_shell::run("mkdir", &["-p", "/root/bin"]).await
            {
                install_warnings.push(format!(
                    "ble-action: mkdir /root/bin failed: {e}"
                ));
            }
            if let Err(e) =
                sentryusb_shell::run("mkdir", &["-p", "/opt/sentryusb"]).await
            {
                install_warnings.push(format!(
                    "ble-action: mkdir /opt/sentryusb failed: {e}"
                ));
            }
            if let Err(e) =
                sentryusb_shell::run("chmod", &["+x", action_tmp]).await
            {
                install_warnings.push(format!(
                    "ble-action: chmod +x failed: {e}"
                ));
            }
            // Move first so a failed install cannot leave a dangling symlink.
            let action_dest =
                format!("/opt/sentryusb/sentryusb-ble-action-{}", suffix);
            let install_result =
                match sentryusb_shell::run("mv", &[action_tmp, &action_dest])
                    .await
                {
                    Ok(_) => {
                        sentryusb_shell::run(
                            "ln",
                            &[
                                "-sfn",
                                &action_dest,
                                "/root/bin/sentryusb-ble-action",
                            ],
                        )
                        .await
                        .map(|_| ())
                    }
                    Err(e) => Err(e),
                };
            if let Err(e) = install_result {
                install_warnings.push(format!(
                    "ble-action binary install FAILED \
                     (install to {action_dest} + symlink from \
                     /root/bin/sentryusb-ble-action): {e} — \
                     likely read-only rootfs"
                ));
                let _ = sentryusb_shell::run("rm", &["-f", action_tmp]).await;
            }
        }
        Err(warning) => install_warnings.push(warning),
        Ok(false) => tracing::info!("release does not include {action_name}; skipping"),
    }

    // Record the listed tag, the requested one, or resolve the tag backing `/latest`.
    let tag = match file_tag {
        Some(v) => v,
        None => {
            let api_url = format!(
                "https://api.github.com/repos/{}/releases/latest",
                repo
            );
            match crate::http_client()
                .get(&api_url)
                .header("User-Agent", "sentryusb-updater")
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await
            {
                Ok(resp) => resp
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|v| {
                        v.get("tag_name")
                            .and_then(|t| t.as_str())
                            .map(|s| s.trim().to_string())
                    })
                    .unwrap_or_default(),
                Err(_) => String::new(),
            }
        }
    };

    if !tag.is_empty() {
        let _ = std::fs::write("/opt/sentryusb/version", &tag);
    }

    // Refresh and reapply hardware-specific runtime patches after the binary swap.
    hub.broadcast(
        "update_status",
        serde_json::json!({"status": "updating_scripts", "message": "Updating scripts…"}),
    );
    let patches_path = "/usr/local/bin/sentryusb-apply-runtime-patches";
    // Pin the helper to the installed tag. With no tag, retain an existing
    // helper or bootstrap from main only when no helper exists.
    let patches_ref = if !tag.is_empty() {
        Some(tag.clone())
    } else if std::path::Path::new(patches_path).exists() {
        None
    } else {
        Some("main".to_string())
    };
    let patches_url = patches_ref.as_ref().map(|r| {
        format!(
            "https://raw.githubusercontent.com/{}/{}/setup/pi/apply-runtime-patches.sh",
            repo, r
        )
    });
    // Stage on the target filesystem so replacement is atomic.
    let patches_tmp = "/usr/local/bin/.sentryusb-apply-runtime-patches.new";
    // `None` keeps the installed helper when the release tag is unknown.
    let download = match &patches_url {
        Some(url) => {
            tracing::info!("update.rs: refreshing runtime-patches script from {}", url);
            Some(
                sentryusb_shell::run_with_timeout(
                    std::time::Duration::from_secs(20),
                    "curl",
                    &["-fsSL", "--max-time", "15", "-o", patches_tmp, url],
                )
                .await,
            )
        }
        None => {
            tracing::warn!(
                "update.rs: release tag unknown — keeping the existing runtime-patches \
                 script rather than pulling an unpinned copy from main"
            );
            install_warnings.push(
                "could not determine the release tag, so the existing runtime-patches \
                 script was kept; fixes added in this release may not have applied."
                    .to_string(),
            );
            None
        }
    };
    match download {
        Some(Ok(_)) => {
            // Never replace the live helper with an empty response.
            if std::fs::metadata(patches_tmp)
                .map(|m| m.len() > 0)
                .unwrap_or(false)
            {
                // Validate permissions and syntax before replacing the live helper.
                let staged_ok = sentryusb_shell::run("chmod", &["+x", patches_tmp])
                    .await
                    .is_ok()
                    && sentryusb_shell::run("bash", &["-n", patches_tmp]).await.is_ok();
                if !staged_ok {
                    let _ = std::fs::remove_file(patches_tmp);
                    tracing::error!(
                        "update.rs: staged runtime-patches script failed chmod/syntax check; \
                         keeping the existing one"
                    );
                    install_warnings.push(
                        "the downloaded runtime-patches script was not executable or failed a \
                         syntax check, so the previous one was kept; fixes added in this \
                         release may not have applied."
                            .to_string(),
                    );
                }
                match if staged_ok {
                    std::fs::rename(patches_tmp, patches_path)
                } else {
                    // Validation failure leaves the live helper untouched.
                    Ok(())
                } {
                    Ok(()) if staged_ok => {
                        tracing::info!("update.rs: runtime-patches script refreshed");
                    }
                    Ok(()) => {}
                    Err(e) => {
                        let _ = std::fs::remove_file(patches_tmp);
                        tracing::error!(
                            "update.rs: runtime-patches swap FAILED ({e}); keeping existing script"
                        );
                        install_warnings.push(format!(
                            "runtime-patches script could not be replaced ({e}): this device will \
                             re-run its EXISTING patch script, so fixes added in this release may \
                             not apply. Re-run install-pi.sh manually."
                        ));
                    }
                }
            } else {
                let _ = std::fs::remove_file(patches_tmp);
                if !std::path::Path::new(patches_path).exists() {
                    install_warnings.push(
                        "runtime-patches download empty AND no existing script: board-specific \
                         fixes won't apply this update. Re-run install-pi.sh manually."
                            .to_string(),
                    );
                }
            }
        }
        Some(Err(e)) => {
            let _ = std::fs::remove_file(patches_tmp);
            if !std::path::Path::new(patches_path).exists() {
                install_warnings.push(format!(
                    "runtime-patches download FAILED ({e}) AND no existing script: board-specific \
                     fixes (BCM4345C0 BLE on Rock 4C+, EATT disable, etc.) won't auto-reapply \
                     after this update. Re-run install-pi.sh manually if BLE pairing breaks."
                ));
            } else {
                tracing::warn!(
                    "update.rs: runtime-patches refresh failed ({e}), falling back to existing on-disk script"
                );
            }
        }
        // Tag unknown; retain the existing helper.
        None => {}
    }

    if std::path::Path::new(patches_path).exists() {
        match sentryusb_shell::run_with_timeout(
            std::time::Duration::from_secs(30),
            patches_path,
            &[],
        )
        .await
        {
            Ok(_) => tracing::info!("update.rs: runtime-patches re-applied successfully"),
            Err(e) => install_warnings.push(format!(
                "runtime-patches re-apply FAILED: {e} — board-specific fixes \
                 (BCM4345C0 BLE on Rock 4C+, etc.) may not survive this update; \
                 if BLE pairing is broken after this update, re-run install-pi.sh"
            )),
        }
    }

    if install_warnings.is_empty() {
        Ok(format!(
            "Updated to {}.",
            if tag.is_empty() { "latest".to_string() } else { tag }
        ))
    } else {
        // Keep full journal detail but cap the WebSocket response at 4 KiB.
        for w in &install_warnings {
            tracing::warn!("update.rs: {}", w);
        }
        let joined = install_warnings.join("\n  • ");
        let mut msg = format!(
            "Updated to {} — but with warnings:\n  • {}",
            if tag.is_empty() {
                "latest".to_string()
            } else {
                tag.clone()
            },
            joined
        );
        if msg.len() > 4096 {
            msg.truncate(4093);
            msg.push_str("...");
        }
        Ok(msg)
    }
}

/// Reads the per-boot identifier used to verify that an update rebooted.
pub(crate) fn read_boot_id() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub async fn get_version() -> (StatusCode, Json<serde_json::Value>) {
    let version = env!("CARGO_PKG_VERSION");
    let sbc_model = get_sbc_model();

    let installed = std::fs::read_to_string("/opt/sentryusb/version")
        .or_else(|_| std::fs::read_to_string("/root/.sentryusb_version"))
        .unwrap_or_else(|_| version.to_string());

    (StatusCode::OK, Json(serde_json::json!({
        "version": installed.trim(),
        "binary_version": version,
        "sbc_model": sbc_model,
        // The version file changes before reboot, so clients compare boot IDs.
        "boot_id": read_boot_id(),
    })))
}

/// Parses a semver tag, handling prerelease and edge cases. Build metadata
/// (`+...`) is dropped: semver gives it no precedence.
pub(crate) fn parse_semver(v: &str) -> Option<(u32, u32, u32, String)> {
    let v = v.trim().trim_start_matches('v');
    let v = v.split_once('+').map_or(v, |(v, _)| v);
    let (base, pre) = match v.find('-') {
        Some(i) => (&v[..i], v[i + 1..].to_string()),
        None => (v, String::new()),
    };
    let parts: Vec<&str> = base.split('.').collect();
    if parts.len() < 3 {
        return None;
    }
    let mut nums = [0u32; 3];
    for (i, p) in parts.iter().take(3).enumerate() {
        if p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        nums[i] = p.parse().ok()?;
    }
    Some((nums[0], nums[1], nums[2], pre))
}

/// Semver 2.0 precedence (section 11), or `None` if either side isn't semver.
/// A release outranks every prerelease of the same version, so `4.98.1` is
/// newer than `4.98.1-test.N` for every N. Prerelease identifiers compare
/// numerically when both are numbers (`test.10` > `test.9`), numbers rank
/// below words, and a longer list wins when one is a prefix of the other.
pub(crate) fn compare_versions(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    let (a, b) = (parse_semver(a)?, parse_semver(b)?);
    let base = (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2));
    if base != Ordering::Equal {
        return Some(base);
    }
    Some(match (a.3.is_empty(), b.3.is_empty()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => {
            let numeric = |s: &str| (!s.is_empty() && s.bytes().all(|c| c.is_ascii_digit())).then(|| s.parse::<u64>().ok()).flatten();
            let (mut x, mut y) = (a.3.split('.'), b.3.split('.'));
            loop {
                let ord = match (x.next(), y.next()) {
                    (None, None) => break Ordering::Equal,
                    (Some(_), None) => Ordering::Greater,
                    (None, Some(_)) => Ordering::Less,
                    (Some(p), Some(q)) => match (numeric(p), numeric(q)) {
                        (Some(p), Some(q)) => p.cmp(&q),
                        (Some(_), None) => Ordering::Less,
                        (None, Some(_)) => Ordering::Greater,
                        (None, None) => p.cmp(q),
                    },
                };
                if ord != Ordering::Equal {
                    break ord;
                }
            }
        }
    })
}

/// True if `candidate` is newer than `current` by semver precedence: stable
/// beats prerelease at the same base version. A candidate tag that isn't
/// semver (`1.22`, `latest`) is never newer; a semver candidate is offered
/// over an unreadable installed version so the device can recover.
pub(crate) fn is_version_newer(candidate: &str, current: &str) -> bool {
    if parse_semver(candidate).is_none() {
        return false;
    }
    match compare_versions(candidate, current) {
        Some(ord) => ord == std::cmp::Ordering::Greater,
        None => true,
    }
}


/// Prereleases are tagged like stable releases (`v3.27.3`) and flagged only on
/// GitHub, so the installed tag's own release record decides. A tag missing
/// from the fetched page is not assumed to be a prerelease.
fn is_prerelease_install(current: &str, releases: &[ReleaseInfo]) -> bool {
    let has_suffix = parse_semver(current).is_some_and(|(_, _, _, pre)| !pre.is_empty());
    let tag = current.trim().trim_start_matches('v');
    has_suffix
        || releases
            .iter()
            .any(|r| !r.draft && r.prerelease && r.tag_name.trim().trim_start_matches('v') == tag)
}

/// The running Sentry USB version, as the status and update pages show it.
pub fn current_version() -> String {
    read_current_version()
}

pub(crate) fn read_current_version() -> String {
    std::fs::read_to_string("/opt/sentryusb/version")
        .or_else(|_| std::fs::read_to_string("/root/.sentryusb_version"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string())
}

/// Checks GitHub releases and returns both legacy and current response fields.
pub async fn check_for_update(
    State(_s): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let current = read_current_version();
    let can_update = !current.is_empty() && current != "dev";

    // The request, the saved channel, or a custom source (whose releases are
    // test builds, tagged as prereleases) enables prerelease discovery.
    let mut include_prerelease = params.get("include_prerelease").map(String::as_str) == Some("true")
        || !update_repo().eq_ignore_ascii_case(OFFICIAL_UPDATE_REPO);
    if !include_prerelease {
        let prefs = crate::preferences::load_prefs();
        if prefs.get("update_channel").and_then(|v| v.as_str()) == Some("prerelease") {
            include_prerelease = true;
        }
    }

    let releases = match fetch_releases().await {
        Ok(rs) => rs,
        Err(msg) => {
            // Opt-in device reporting is independent of GitHub availability.
            crate::device_reporting::nudge();

            return (
                StatusCode::OK,
                Json(serde_json::json!({
                    "available": false,
                    "update_available": false,
                    "error": msg,
                })),
            );
        }
    };

    let (latest_stable, latest_prerelease) = find_latest_releases(&releases);

    let mut result = serde_json::json!({
        "current_version": current,
        // Offered releases all come from this repository.
        "source_repo": update_repo(),
        "checked_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    });

    // A prerelease installation may always revert to the latest stable.
    let on_prerelease = is_prerelease_install(&current, &releases);

    if let Some(stable) = latest_stable {
        let stable_available = can_update && is_version_newer(&stable.tag_name, &current);
        result["update_available"] = serde_json::Value::Bool(stable_available);
        result["latest_version"] = serde_json::Value::String(stable.tag_name.clone());
        result["release_url"] = serde_json::Value::String(stable.html_url.clone());
        result["release_notes"] = serde_json::Value::String(stable.body.clone());
        result["stable"] = serde_json::json!({
            "version": stable.tag_name,
            "release_url": stable.html_url,
            "release_notes": stable.body,
            "available": stable_available,
        });

        if on_prerelease && can_update && !stable_available {
            result["revert_stable"] = serde_json::json!({
                "version": stable.tag_name,
                "release_url": stable.html_url,
                "release_notes": stable.body,
            });
        }
    } else {
        result["update_available"] = serde_json::Value::Bool(false);
    }

    if include_prerelease {
        if let Some(pre) = latest_prerelease {
            let pre_available = can_update && is_version_newer(&pre.tag_name, &current);
            result["prerelease"] = serde_json::json!({
                "version": pre.tag_name,
                "release_url": pre.html_url,
                "release_notes": pre.body,
                "available": pre_available,
            });
        }
    }

    // Cache the result for subsequent Settings loads.
    if let Ok(data) = serde_json::to_vec(&result) {
        let _ = std::fs::write(UPDATE_CHECK_CACHE, data);
    }

    crate::device_reporting::nudge();

    (StatusCode::OK, Json(result))
}

#[derive(Clone)]
struct ReleaseInfo {
    tag_name: String,
    html_url: String,
    body: String,
    prerelease: bool,
    draft: bool,
}

/// Fetches recent stable and prerelease records from GitHub.
async fn fetch_releases() -> Result<Vec<ReleaseInfo>, String> {
    let url = format!("https://api.github.com/repos/{}/releases?per_page=20", update_repo());
    let body = crate::release_download::get_json(&url, &|_| {})
        .await
        .map_err(|e| format!("Couldn't check GitHub for updates. {}", e.sentence()))?;

    let arr = body
        .as_array()
        .ok_or_else(|| "Couldn't check GitHub for updates. GitHub sent an unexpected reply; try again in a few minutes.".to_string())?;

    Ok(arr
        .iter()
        .map(|v| ReleaseInfo {
            tag_name: v.get("tag_name").and_then(|s| s.as_str()).unwrap_or("").to_string(),
            html_url: v.get("html_url").and_then(|s| s.as_str()).unwrap_or("").to_string(),
            body: v.get("body").and_then(|s| s.as_str()).unwrap_or("").to_string(),
            prerelease: v.get("prerelease").and_then(|s| s.as_bool()).unwrap_or(false),
            draft: v.get("draft").and_then(|s| s.as_bool()).unwrap_or(false),
        })
        .filter(|r| !r.tag_name.is_empty())
        .collect())
}

/// Selects the highest non-draft stable release and the highest prerelease
/// by version. GitHub's list isn't in version order (a fork's releases came
/// back as test.8, test.7, test.6, test.10, test.5), so taking the first of
/// each kind left a device on test.8 "up to date" while test.10 existed.
/// Releases whose tag isn't semver (a fork's old `1.22`) are skipped: they
/// can't be ordered against installed versions.
fn find_latest_releases(releases: &[ReleaseInfo]) -> (Option<&ReleaseInfo>, Option<&ReleaseInfo>) {
    let higher = |best: Option<&ReleaseInfo>, r: &'_ ReleaseInfo| -> bool {
        best.is_none_or(|b| compare_versions(&r.tag_name, &b.tag_name) == Some(std::cmp::Ordering::Greater))
    };
    let mut stable: Option<&ReleaseInfo> = None;
    let mut prerelease: Option<&ReleaseInfo> = None;
    for r in releases {
        if r.draft || parse_semver(&r.tag_name).is_none() {
            continue;
        }
        if r.prerelease {
            if higher(prerelease, r) {
                prerelease = Some(r);
            }
        } else if higher(stable, r) {
            stable = Some(r);
        }
    }
    (stable, prerelease)
}

/// Returns the last cached update check; live progress uses the WebSocket channel.
pub async fn get_update_status(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    match std::fs::read_to_string(UPDATE_CHECK_CACHE) {
        Ok(s) => match serde_json::from_str::<serde_json::Value>(&s) {
            Ok(v) => (StatusCode::OK, Json(v)),
            Err(_) => (
                StatusCode::OK,
                Json(serde_json::json!({"update_available": false})),
            ),
        },
        Err(_) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "update_available": false,
                "checked_at": "",
            })),
        ),
    }
}

#[cfg(test)]
mod hardware_identity_tests {
    use super::*;

    #[test]
    fn board_identity_survives_reinstall_and_matches_existing_hash() {
        let first = hardware_serial(["10000000abcdef01\0"], "Serial : 10000000abcdef01").unwrap();
        let reinstalled = hardware_serial([], "Hardware : BCM2712\nSerial : 10000000abcdef01\n").unwrap();
        assert_eq!(hash_serial(&first), hash_serial(&reinstalled));
        assert_eq!(hash_serial(&first), "ae2bb2e75d744f73ee3bcd95ac63b5fb70cbbb1cb2f0e69aff5d21c8e41ef841");
    }

    #[test]
    fn empty_and_placeholder_serials_are_not_devices() {
        assert!(hardware_serial(["\0", "0000000000000000"], "Hardware : BCM2712\nSerial : 00000000").is_none());
        assert!(hardware_serial(["unknown", "0x0000"], "").is_none());
        assert_eq!(hardware_serial(["0000000000000000"], "Serial : 123456789abcdef0").as_deref(), Some("123456789abcdef0"));
    }

    #[tokio::test]
    async fn slow_shutdown_is_not_reported_as_a_failed_reboot() {
        use std::time::Duration;
        let ms = Duration::from_millis;
        // Still stopping at the deadline: keep waiting for systemd to end us.
        let pending = await_reboot_with(ms(1), ms(1), || async { true });
        assert!(tokio::time::timeout(ms(50), pending).await.is_err());
        // Shutdown never began, or was abandoned: report the failure.
        let polls = std::cell::Cell::new(0);
        let e = await_reboot_with(ms(1), ms(1), || { polls.set(polls.get() + 1); async { polls.get() < 3 } }).await;
        assert_eq!(e.to_string(), "Device did not reboot after update");
        assert_eq!(polls.get(), 3);
    }
}

#[cfg(test)]
mod prerelease_install_tests {
    use super::*;

    fn release(tag: &str, prerelease: bool) -> ReleaseInfo {
        ReleaseInfo { tag_name: tag.into(), html_url: String::new(), body: String::new(), prerelease, draft: false }
    }

    fn releases() -> Vec<ReleaseInfo> {
        vec![release("v3.27.3", true), release("v3.27.2", true), release("v3.27.1", true), release("v3.27.0", false), release("v3.26.8", false)]
    }

    #[test]
    fn plain_tagged_prerelease_uses_the_github_flag() {
        assert!(is_prerelease_install("v3.27.1", &releases()));
        assert!(is_prerelease_install("3.27.3", &releases()));
    }

    #[test]
    fn stable_installs_are_not_prereleases() {
        assert!(!is_prerelease_install("v3.27.0", &releases()));
        assert!(!is_prerelease_install("v3.26.8", &releases()));
    }

    #[test]
    fn a_tag_missing_from_the_page_is_not_assumed_prerelease() {
        assert!(!is_prerelease_install("v3.20.0", &releases()));
        assert!(!is_prerelease_install("v3.28.0", &releases()));
    }

    #[test]
    fn suffixed_prerelease_still_counts() {
        assert!(is_prerelease_install("v3.27.0-beta.1", &releases()));
        assert!(is_prerelease_install("v3.27.1-rc1", &[]));
    }

    #[test]
    fn drafts_never_count() {
        let mut list = releases();
        list.push(ReleaseInfo { draft: true, ..release("v3.29.0", true) });
        assert!(!is_prerelease_install("v3.29.0", &list));
    }

    #[test]
    fn the_highest_version_wins_whatever_order_github_lists() {
        // The order GitHub really returned for the owner's fork.
        let list = vec![
            release("v3.28.10-test.8", true), release("v3.28.10-test.7", true), release("v3.28.10-test.6", true),
            release("v3.28.10-test.10", true), release("v3.28.10-test.5", true),
            release("v3.28.8", false), release("v3.28.9", false),
        ];
        let (stable, pre) = find_latest_releases(&list);
        assert_eq!(pre.map(|r| r.tag_name.as_str()), Some("v3.28.10-test.10"));
        assert_eq!(stable.map(|r| r.tag_name.as_str()), Some("v3.28.9"));
        assert!(is_version_newer("v3.28.10-test.10", "v3.28.10-test.8"));
        // Drafts still never count, even when they'd be highest.
        let mut with_draft = list.clone();
        with_draft.push(ReleaseInfo { draft: true, ..release("v3.28.10-test.99", true) });
        assert_eq!(find_latest_releases(&with_draft).1.map(|r| r.tag_name.as_str()), Some("v3.28.10-test.10"));
    }

    #[test]
    fn non_semver_releases_are_never_offered() {
        let list = vec![release("1.22", false), release("latest", true), release("", false), release("v3.28.9", false), release("v3.29.0-beta.1", true)];
        let (stable, pre) = find_latest_releases(&list);
        assert_eq!(stable.map(|r| r.tag_name.as_str()), Some("v3.28.9"));
        assert_eq!(pre.map(|r| r.tag_name.as_str()), Some("v3.29.0-beta.1"));
        assert_eq!(find_latest_releases(&[release("1.22", false)]).0.map(|r| r.tag_name.clone()), None);
        for bad in ["1.22", "latest", "", "v1", "1.2.x", "main"] {
            assert!(!is_version_newer(bad, "v3.28.9"), "{bad:?}");
            assert!(!is_version_newer(bad, "dev"), "{bad:?}");
        }
        // A real release still repairs an unreadable installed version.
        assert!(is_version_newer("v3.28.9", "unknown"));
    }

    #[test]
    fn a_fork_test_build_counts_as_prerelease_so_official_stable_is_offered() {
        assert!(is_prerelease_install("v4.98.1-test.2", &[]));
    }
}

#[cfg(test)]
mod version_order_tests {
    use super::*;
    use std::cmp::Ordering::*;

    #[test]
    fn official_release_outranks_every_test_build_of_it() {
        for n in ["1", "2", "9", "10", "123"] {
            let test = format!("v4.98.1-test.{n}");
            assert!(is_version_newer("v4.98.1", &test), "{test}");
            assert!(!is_version_newer(&test, "v4.98.1"), "{test}");
            assert!(is_version_newer(&test, "v4.98.0"), "{test}");
            assert!(!is_version_newer(&test, "v4.98.2"), "{test}");
        }
    }

    #[test]
    fn prerelease_identifiers_follow_semver_precedence() {
        // Numeric identifiers compare as numbers, not strings.
        assert_eq!(compare_versions("v4.98.1-test.10", "v4.98.1-test.9"), Some(Greater));
        assert_eq!(compare_versions("1.0.0-alpha", "1.0.0-alpha.1"), Some(Less));
        assert_eq!(compare_versions("1.0.0-alpha.1", "1.0.0-alpha.beta"), Some(Less));
        assert_eq!(compare_versions("1.0.0-beta.2", "1.0.0-beta.11"), Some(Less));
        assert_eq!(compare_versions("1.0.0-rc.1", "1.0.0"), Some(Less));
        assert_eq!(compare_versions("v1.0.0", "1.0.0+build.7"), Some(Equal));
        assert_eq!(compare_versions("v3.10.0", "v3.9.9"), Some(Greater));
        assert_eq!(compare_versions("dev", "v1.0.0"), None);
    }

    #[test]
    fn repo_setting_accepts_owner_or_owner_and_name() {
        assert_eq!(normalize_repo("Sentry-Six").as_deref(), Some("Sentry-Six/Sentry-USB-Rusty"));
        assert_eq!(normalize_repo(" skittles-fivem ").as_deref(), Some("skittles-fivem/Sentry-USB-Rusty"));
        assert_eq!(normalize_repo("skittles-fivem/Sentry-USB-Rusty-testing").as_deref(),
            Some("skittles-fivem/Sentry-USB-Rusty-testing"));
        assert_eq!(normalize_repo("a/b_c.d").as_deref(), Some("a/b_c.d"));
    }

    #[test]
    fn repo_setting_rejects_anything_github_would_not_name() {
        let long_owner = "x".repeat(40);
        let long_name = format!("owner/{}", "x".repeat(101));
        for bad in ["", "/", "owner/", "/name", "a/b/c", "-owner", "owner-", "own_er", "own.er",
            "owner/..", "owner/.", "owner/na me", "https://github.com/a/b", "owner/name?x=1",
            "o\nwner", &long_owner, &long_name] {
            assert_eq!(normalize_repo(bad), None, "{bad:?}");
        }
    }
}
