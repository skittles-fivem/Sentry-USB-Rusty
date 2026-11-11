//! The Sentry USB side of the management link to Sentry Manager.
//!
//! The Pi connects out to Manager's device port over WSS, both sides prove
//! they share a key, and Manager's requests then run in-process against the
//! allowlisted part of the Pi's API router. Contract:
//! `docs/sentry-manager-link.md`.
//!
//! Pairing (first connection) is only attempted inside a 15-minute pairing
//! window the owner opens on the Pi by entering Manager's address and the
//! one-time pairing code Manager shows, then pressing Pair
//! (`start_pairing`). Outside it the Pi only ever connects in key mode, so
//! nothing a network or a fake Manager does can make it fall back to
//! pairing.
//!
//! The pair-mode secret is P = HMAC(K, "pair-code|" + code): the code is
//! held in memory only, for the window's life, and never stored or logged.
//! A first pairing accepts whatever certificate is served; the proof covers
//! the fingerprint seen, and the one that pairs is pinned.

pub mod allowlist;
pub mod audit;
pub mod car;
pub mod handshake;
pub mod secrets;
pub mod serve;
pub mod session;
pub mod store;
pub mod tls;
pub mod ui;

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::{watch, Notify};

use handshake::{PairingCode, Secret};
use session::{LinkError, Mode};

pub use secrets::OFFICIAL_UPDATE_REPO;

/// How long a pairing window stays open (matches Manager's pairing slot).
pub const PAIRING_WINDOW: Duration = Duration::from_secs(15 * 60);
const BACKOFF_START: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(5 * 60);
/// While pairing, retry often enough that pairing completes soon after the
/// owner adds the Pi in Manager, yet under Manager's 10 failures a minute.
const PAIRING_BACKOFF_MAX: Duration = Duration::from_secs(15);

/// What the link is doing, for the status page.
#[derive(Debug, Clone, PartialEq)]
enum Phase {
    Idle,
    Connecting,
    Connected,
    Retrying,
}

/// Why the last pairing stopped without pairing. Cleared by Pair and Unpair.
#[derive(Debug, Clone, PartialEq)]
enum PairFailure {
    /// Manager refused the proof: the code or the web password is wrong
    /// (one message for both, so neither is revealed).
    WrongCode,
    /// The window ran out here, or Manager had no pairing slot open.
    Expired,
    /// The other side speaks another link version.
    Version(String),
}

#[derive(Debug, Clone, Default)]
struct Runtime {
    phase: Option<Phase>,
    mode: Option<Mode>,
    last_error: Option<String>,
    /// The last refusal was "not paired" (Manager doesn't know our key).
    not_paired: bool,
    cert_mismatch: bool,
    /// The version message, when Manager speaks another link version.
    version_mismatch: Option<String>,
    last_connected: Option<i64>,
    next_retry: Option<Instant>,
    pair_failure: Option<PairFailure>,
}

/// An open pairing window: when it closes, and the code the owner entered.
/// Memory only; dropping it wipes the code.
struct PairWindow {
    until: Instant,
    code: PairingCode,
}

pub struct Link {
    runtime: Mutex<Runtime>,
    pairing: Mutex<Option<PairWindow>>,
    wake: Notify,
    /// Bumped to end the current connection (unpair, settings change).
    stop_tx: watch::Sender<u64>,
    started: OnceLock<()>,
    version: OnceLock<String>,
}

fn link() -> &'static Link {
    static LINK: OnceLock<Link> = OnceLock::new();
    LINK.get_or_init(|| Link {
        runtime: Mutex::new(Runtime::default()),
        pairing: Mutex::new(None),
        wake: Notify::new(),
        stop_tx: watch::channel(0).0,
        started: OnceLock::new(),
        version: OnceLock::new(),
    })
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Link {
    fn set(&self, f: impl FnOnce(&mut Runtime)) {
        f(&mut lock(&self.runtime));
    }

    /// The window is open. One that just ran out is dropped (with its code)
    /// and reported as expired.
    fn pairing_open(&self) -> bool {
        let expired = {
            let mut w = lock(&self.pairing);
            match &*w {
                Some(p) if Instant::now() < p.until => return true,
                Some(_) => {
                    *w = None;
                    true
                }
                None => false,
            }
        };
        if expired {
            audit::record("info", "pairing_closed", "Pairing window expired", json!({}));
            self.set(|r| {
                r.pair_failure.get_or_insert(PairFailure::Expired);
            });
        }
        false
    }

    fn pairing_left(&self) -> Option<Duration> {
        lock(&self.pairing).as_ref().and_then(|p| p.until.checked_duration_since(Instant::now()))
    }

    /// P for the open window's code, or `None` when no window is open.
    fn pair_secret(&self, k: &Secret) -> Option<Secret> {
        lock(&self.pairing).as_ref().map(|p| handshake::derive_pair_secret(k, &p.code))
    }

    fn open_pairing_for(&self, window: Duration, code: PairingCode, address: &str) {
        let window = window.min(PAIRING_WINDOW);
        *lock(&self.pairing) = Some(PairWindow { until: Instant::now() + window, code });
        let msg = match window.as_secs() {
            60 => "Pairing window opened for 1 minute".to_string(),
            s if s % 60 == 0 => format!("Pairing window opened for {} minutes", s / 60),
            s => format!("Pairing window opened for {s} seconds"),
        };
        // The code is never logged: only that a window opened, and where to.
        audit::record("info", "pairing_open", &msg, json!({"address": address}));
        self.set(|r| {
            r.not_paired = false;
            r.last_error = None;
            r.pair_failure = None;
            r.version_mismatch = None;
            r.cert_mismatch = false;
        });
        self.kick(true);
    }

    /// Closes the window and forgets its code.
    fn close_pairing(&self) {
        *lock(&self.pairing) = None;
    }

    /// Re-reads settings now; with `restart`, also drops the current link.
    fn kick(&self, restart: bool) {
        if restart {
            self.stop_tx.send_modify(|g| *g = g.wrapping_add(1));
        }
        self.wake.notify_one();
    }
}

/// Starts the link task. `router` is the Pi's API router with its state,
/// before any auth layer: the link is its own gate (allowlist). Call once.
pub fn spawn(router: axum::Router, version: String) {
    let l = link();
    if l.started.set(()).is_err() {
        return;
    }
    let _ = l.version.set(version);
    tokio::spawn(run(router));
}

/// The Sentry USB version passed to `spawn` (empty before that).
pub(crate) fn running_version() -> String {
    link().version.get().cloned().unwrap_or_default()
}

/// A default route exists (Wi-Fi or Ethernet is up).
fn network_up() -> bool {
    std::fs::read_to_string("/proc/net/route")
        .map(|t| t.lines().skip(1).any(|l| l.split_whitespace().nth(1) == Some("00000000")))
        .unwrap_or(true)
}

/// Sleeps `d`, waking early when nudged or when the network comes up.
async fn wait(d: Duration) {
    let l = link();
    let deadline = tokio::time::Instant::now() + d;
    let mut was_up = network_up();
    loop {
        let step = tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + Duration::from_secs(3)));
        tokio::select! {
            _ = l.wake.notified() => return,
            _ = step => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return;
        }
        let up = network_up();
        if up && !was_up {
            return;
        }
        was_up = up;
    }
}

fn jitter(d: Duration) -> Duration {
    use ring::rand::SecureRandom;
    let mut b = [0u8; 2];
    let _ = ring::rand::SystemRandom::new().fill(&mut b);
    // 80% .. 120%
    let f = 0.8 + (u16::from_le_bytes(b) as f64 / u16::MAX as f64) * 0.4;
    d.mul_f64(f)
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().chars().take(64).collect())
        .unwrap_or_else(|_| "sentryusb".into())
}

/// The pairing key for the current web login, computed off the runtime
/// and cached so retries don't redo 200,000 PBKDF2 rounds.
struct KCache {
    tag: Option<[u8; 32]>,
    k: Option<Secret>,
}

impl KCache {
    async fn get(&mut self, user: &str, pass: &str) -> Secret {
        let tag_in = format!("{}\0{}", user, pass);
        let mut tag = [0u8; 32];
        tag.copy_from_slice(ring::digest::digest(&ring::digest::SHA256, tag_in.as_bytes()).as_ref());
        if self.tag == Some(tag) {
            if let Some(k) = &self.k {
                return k.clone();
            }
        }
        let (u, p) = (user.to_string(), pass.to_string());
        let k = tokio::task::spawn_blocking(move || handshake::derive_pairing_key(&u, &p))
            .await
            .expect("pbkdf2 task");
        self.tag = Some(tag);
        self.k = Some(k.clone());
        k
    }
}

async fn run(router: axum::Router) {
    let l = link();
    let mut backoff = BACKOFF_START;
    let mut kcache = KCache { tag: None, k: None };
    loop {
        let active = store::read_config();
        let settings = store::Settings::from_config(&active);
        let stored = match store::load() {
            Ok(s) => s,
            Err(e) => {
                l.set(|r| {
                    r.phase = Some(Phase::Retrying);
                    r.last_error = Some(format!("Can't read the link file: {e}"));
                });
                wait(BACKOFF_MAX).await;
                continue;
            }
        };
        let pairing = l.pairing_open();
        let address = settings.effective_address().and_then(|a| store::split_host_port(&a).ok());
        let mode = if pairing && settings.web_password_set {
            Some(Mode::Pair)
        } else if stored.paired() {
            Some(Mode::Key)
        } else {
            None
        };
        let (Some((host, port)), Some(mode), true) = (address, mode, settings.enabled) else {
            l.set(|r| {
                r.phase = Some(Phase::Idle);
                r.next_retry = None;
            });
            // Re-check now and then so a pairing window's end shows.
            wait(Duration::from_secs(30)).await;
            backoff = BACKOFF_START;
            continue;
        };

        let target = format!("{host}:{port}");
        // Pair mode keeps a saved pin (pairing again only works with the
        // same Manager); a first pairing has none and accepts any
        // certificate, bound into the proof.
        let pin = stored.pin().map(str::to_string);
        let secret = match mode {
            Mode::Pair => {
                let pass = active.get("WEB_PASSWORD").cloned().unwrap_or_default();
                let k = kcache.get(&settings.web_username, &pass).await;
                // The window can close while K is derived.
                let Some(p) = l.pair_secret(&k) else { continue };
                p
            }
            Mode::Key => stored.link_key().expect("paired"),
        };
        let attempt = session::Attempt {
            host,
            port,
            mode,
            secret,
            pin,
            device_id: stored.device_id.clone(),
            name: hostname(),
            version: l.version.get().cloned().unwrap_or_default(),
        };
        l.set(|r| {
            r.phase = Some(Phase::Connecting);
            r.mode = Some(mode);
            r.next_retry = None;
        });
        let stop = l.stop_tx.subscribe();
        let mode_name = match mode { Mode::Pair => "pair", Mode::Key => "key" };
        match session::connect(&attempt).await {
            Ok((ws, est)) => {
                if mode == Mode::Pair {
                    if let Err(e) = session::save_pairing(&est) {
                        tracing::warn!("[manager-link] pairing succeeded but could not be saved: {e}");
                        l.set(|r| {
                            r.phase = Some(Phase::Retrying);
                            r.last_error = Some("Paired, but the key could not be saved on the Sentry USB".into());
                        });
                        wait(jitter(backoff)).await;
                        continue;
                    }
                    l.close_pairing();
                    l.set(|r| r.pair_failure = None);
                    tracing::info!("[manager-link] paired with Sentry Manager, fingerprint {}", est.fp);
                    audit::record("info", "paired", "Paired with Sentry Manager",
                        json!({"address": target, "fingerprint": est.fp}));
                }
                audit::record("info", "connected", "Connected to Sentry Manager",
                    json!({"address": target, "mode": mode_name, "fingerprint": est.fp}));
                let local_ip = est.local_ip.clone();
                drop(est);
                l.set(|r| {
                    r.phase = Some(Phase::Connected);
                    r.last_error = None;
                    r.not_paired = false;
                    r.cert_mismatch = false;
                    r.version_mismatch = None;
                    r.last_connected = Some(now_unix());
                });
                tracing::info!("[manager-link] connected to {}:{}", attempt.host, attempt.port);
                let reason = session::run(ws, serve::Server::new(router.clone()), stop, local_ip).await;
                tracing::info!("[manager-link] disconnected: {reason}");
                audit::record("info", "disconnected", "Disconnected from Sentry Manager",
                    json!({"address": target, "reason": reason}));
                l.set(|r| {
                    r.last_connected = Some(now_unix());
                    r.last_error = Some(reason);
                    r.phase = Some(Phase::Retrying);
                });
                backoff = BACKOFF_START;
            }
            Err(e) => {
                tracing::info!("[manager-link] connection failed ({mode_name} mode): {e}");
                match &e {
                    LinkError::CertMismatch(seen) => {
                        audit::record("warn", "fingerprint_mismatch",
                            if attempt.pin.is_some() {
                                "Possible fake Sentry Manager: the certificate doesn't match the paired one"
                            } else {
                                "Possible man in the middle: the certificate isn't the one Sentry Manager named; no proof was sent"
                            },
                            json!({"address": target, "mode": mode_name, "seen": seen, "expected": attempt.pin}));
                        audit::queue_alert("fingerprint", seen, &target);
                    }
                    LinkError::BadProof(seen) => {
                        audit::record("warn", "bad_proof",
                            "Sentry Manager's proof failed: the server doesn't hold the key",
                            json!({"address": target, "mode": mode_name, "seen": seen}));
                        audit::queue_alert("proof", seen, &target);
                    }
                    other => audit::record("info", "attempt_failed", "Connection attempt failed",
                        json!({"address": target, "mode": mode_name, "error": other.to_string()})),
                }
                // A pairing Manager refused: retrying with the same code
                // would only use up Manager's tries, so the window closes
                // and the owner is told in plain words.
                let failure = match (&e, mode) {
                    (LinkError::Version(_), _) => Some(PairFailure::Version(e.to_string())),
                    (LinkError::Denied(why), Mode::Pair) => match why.as_str() {
                        "rate limited" | "timeout" => None,
                        "no pairing open" | "no slot" | "expired" => Some(PairFailure::Expired),
                        _ => Some(PairFailure::WrongCode),
                    },
                    _ => None,
                };
                if let (Some(f), Mode::Pair) = (&failure, mode) {
                    l.close_pairing();
                    audit::record("info", "pairing_closed", match f {
                        PairFailure::WrongCode => "Pairing failed: wrong pairing code or password",
                        PairFailure::Expired => "Pairing failed: Sentry Manager's pairing window has closed",
                        PairFailure::Version(_) => "Pairing failed: Sentry Manager uses another link version",
                    }, json!({"address": target}));
                }
                l.set(|r| {
                    r.phase = Some(Phase::Retrying);
                    r.last_error = Some(e.to_string());
                    r.not_paired = matches!(e, LinkError::Denied(_) | LinkError::BadProof(_)) && mode == Mode::Key;
                    r.cert_mismatch = matches!(e, LinkError::CertMismatch(_));
                    r.version_mismatch = matches!(e, LinkError::Version(_)).then(|| e.to_string());
                    if mode == Mode::Pair && failure.is_some() {
                        r.pair_failure = failure.clone();
                    }
                });
                if mode == Mode::Pair && failure.is_some() {
                    l.set(|r| {
                        r.phase = Some(Phase::Idle);
                        r.next_retry = None;
                    });
                    backoff = BACKOFF_START;
                    continue;
                }
            }
        }
        let cap = if mode == Mode::Pair { PAIRING_BACKOFF_MAX } else { BACKOFF_MAX };
        let d = jitter(backoff.min(cap));
        l.set(|r| r.next_retry = Some(Instant::now() + d));
        wait(d).await;
        backoff = (backoff * 2).min(cap);
    }
}

/// The status page's view of the link (`GET /api/manager-link`). Never
/// carries the pairing code, K, P or the link key.
pub fn status_json() -> Value {
    let l = link();
    let active = store::read_config();
    let settings = store::Settings::from_config(&active);
    let stored = store::load().unwrap_or_default();
    let pairing = l.pairing_open();
    let rt = lock(&l.runtime).clone();
    let left = l.pairing_left().map(|d| d.as_secs()).unwrap_or(0);
    let paired = stored.paired();
    let effective = settings.effective_address();
    let next_retry = rt.next_retry.and_then(|t| t.checked_duration_since(Instant::now())).map(|d| d.as_secs());
    let target = effective.as_deref().and_then(|a| store::split_host_port(a).ok()).map(|(h, p)| format!("{h}:{p}"));
    let pin = stored.pin();
    let retry_in = match next_retry {
        Some(s) => format!("Trying again in {s} s."),
        None => "Trying again shortly.".into(),
    };

    let (state, message): (&str, String) = if !settings.enabled {
        ("off", "Off. Turn this on to let Sentry Manager on your NAS see and manage this Sentry USB.".into())
    } else if !paired && !settings.web_password_set {
        ("needs_password", "Set a web password first. Sentry Manager pairs using this Sentry USB's web username and password.".into())
    } else if pairing && rt.phase != Some(Phase::Connected) {
        let head = format!(
            "Pairing with Sentry Manager at {} ({} min left)…",
            target.as_deref().unwrap_or("its address"),
            left.div_ceil(60).max(1)
        );
        ("pairing", match (&rt.last_error, rt.phase.as_ref()) {
            (Some(e), Some(Phase::Retrying)) => format!("{head} Can't reach it yet: {e}. Check the address. {retry_in}"),
            _ => head,
        })
    } else if let (false, Some(f)) = (paired, &rt.pair_failure) {
        ("pair_failed", match f {
            PairFailure::WrongCode => "Wrong pairing code or password. Check the web username and password you \
                entered in Sentry Manager, press Start pairing there for a new code, then enter it here and press Pair.".into(),
            PairFailure::Expired => "Pairing window expired. Press Start pairing in Sentry Manager for a new code, \
                then enter it here and press Pair.".into(),
            PairFailure::Version(m) => format!("{m}."),
        })
    } else if let Some(m) = &rt.version_mismatch {
        ("update_needed", format!("{m}."))
    } else if rt.cert_mismatch && paired {
        ("cert_changed", "Sentry Manager's certificate changed, so this Sentry USB won't connect. If you reinstalled or replaced Manager, press Unpair, then pair again. If not, someone may be pretending to be Manager.".into())
    } else if !paired {
        ("needs_pairing", "Not paired. In Sentry Manager, open Settings → Sentry USB → Add a Sentry USB and press \
            Start pairing. Then enter Sentry Manager's address and the pairing code it shows, and press Pair.".into())
    } else if effective.is_none() {
        ("needs_address", "Enter Sentry Manager's address: the computer it runs on, with port 10000 (for example 192.168.1.5:10000).".into())
    } else if rt.phase == Some(Phase::Connected) {
        ("connected", "Connected to Sentry Manager.".into())
    } else if rt.not_paired {
        ("not_paired", "Sentry Manager doesn't recognize this Sentry USB any more (it may have been removed there). To pair again, press Start pairing in Sentry Manager, then enter the new code here and press Pair. Retrying automatically.".into())
    } else if rt.phase == Some(Phase::Connecting) || rt.phase.is_none() {
        ("connecting", "Connecting to Sentry Manager…".into())
    } else {
        ("retrying", format!("Can't reach Sentry Manager right now. This is normal while the car is away from home. {retry_in}"))
    };

    json!({
        "enabled": settings.enabled,
        "address": settings.address,
        "effective_address": effective,
        "web_password_set": settings.web_password_set,
        "paired": paired,
        "pairing": pairing,
        "pairing_seconds_left": if pairing { left } else { 0 },
        // The pinned fingerprint, for reference: nothing has to be compared.
        "fingerprint": pin,
        "fingerprint_display": pin.map(store::fingerprint_display),
        "device_id": stored.device_id,
        "state": state,
        "message": message,
        "connected": rt.phase == Some(Phase::Connected),
        "last_error": rt.last_error,
        "last_connected": rt.last_connected,
        "next_retry_seconds": next_retry,
    })
}

/// Writes on/off and the address to sentryusb.conf. Returns whether the
/// link was on before, and the address before.
fn write_settings(enabled: bool, address: &str) -> Result<(bool, String), String> {
    let path = sentryusb_config::find_config_path();
    let (mut active, _) = sentryusb_config::parse_file(path).map_err(|_| "Could not read the settings".to_string())?;
    let was_enabled = store::Settings::from_config(&active).enabled;
    let old_address = active.get(store::ADDRESS_KEY).cloned().unwrap_or_default();
    if was_enabled == enabled && old_address == address {
        return Ok((was_enabled, old_address));
    }
    active.insert(store::ENABLED_KEY.into(), if enabled { "true" } else { "false" }.into());
    if address.is_empty() {
        active.remove(store::ADDRESS_KEY);
    } else {
        active.insert(store::ADDRESS_KEY.into(), address.into());
    }
    if std::env::var_os("SENTRYUSB_MANAGER_LINK_FILE").is_none() {
        let _ = std::process::Command::new("/root/bin/remountfs_rw").status();
    }
    sentryusb_config::write_file(path, &active).map_err(|_| "Could not save the settings".to_string())?;
    Ok((was_enabled, old_address))
}

/// Saves on/off and the address. Pairing only starts with Pair
/// (`start_pairing`); turning the link off closes a pairing window.
pub fn save_settings(enabled: bool, address: &str) -> Result<(), String> {
    let address = if address.trim().is_empty() { String::new() } else { store::normalize_address(address)? };
    let (was_enabled, old_address) = write_settings(enabled, &address)?;
    let l = link();
    if !enabled {
        l.close_pairing();
    }
    l.kick(enabled != was_enabled || old_address != address);
    Ok(())
}

/// Pair: saves Manager's address (when given) and opens the 15-minute
/// pairing window with the code Manager shows. The code stays in memory
/// only, until the window closes. A pin already saved stays in force, so
/// pairing again only works with the same Manager.
pub fn start_pairing(address: &str, code: &str) -> Result<(), String> {
    start_pairing_for(address, code, PAIRING_WINDOW)
}

/// `start_pairing` with a shorter window (capped at 15 minutes). For the
/// off-Pi test harness (`examples/link_harness.rs`); the Pi's own routes
/// always use the full window.
pub fn start_pairing_for(address: &str, code: &str, window: Duration) -> Result<(), String> {
    let settings = store::Settings::from_config(&store::read_config());
    if !settings.web_password_set {
        return Err("Set a web username and password first".into());
    }
    if !settings.enabled {
        return Err("Turn on \"Managed by Sentry Manager\" first".into());
    }
    let address = match address.trim() {
        "" => settings.effective_address().ok_or_else(|| {
            "Enter Sentry Manager's address: the computer it runs on, with port 10000 (for example 192.168.1.5:10000)".to_string()
        })?,
        a => store::normalize_address(a)?,
    };
    let code = PairingCode::parse(code).map_err(str::to_string)?;
    write_settings(true, &address)?;
    link().open_pairing_for(window, code, &address);
    Ok(())
}

/// Forgets the link key and the pinned fingerprint and drops the link.
pub fn unpair() -> Result<(), String> {
    let mut s = store::load().map_err(|_| "Could not read the link file".to_string())?;
    s.link_key = None;
    s.pin = None;
    s.paired_at = None;
    s.warnings.clear();
    store::save(&s).map_err(|_| "Could not save the link file".to_string())?;
    let l = link();
    l.close_pairing();
    l.set(|r| {
        r.not_paired = false;
        r.cert_mismatch = false;
        r.last_error = None;
        r.phase = Some(Phase::Idle);
        r.pair_failure = None;
        r.version_mismatch = None;
    });
    l.kick(true);
    tracing::info!("[manager-link] unpaired on the Sentry USB");
    audit::record("info", "unpaired", "Unpaired on the Sentry USB", json!({}));
    Ok(())
}

/// The security log for the status page (`GET /api/manager-link/log`):
/// newest first, plus the fingerprint and proof warnings pulled out so the
/// page can show them prominently.
pub fn log_json(limit: usize) -> Value {
    let entries = audit::recent(limit);
    // From the link file, not the log: a flood of log lines can't hide them.
    let warnings: Vec<Value> = store::load()
        .map(|s| s.warnings)
        .unwrap_or_default()
        .iter()
        .rev()
        .map(|w| {
            json!({
                "at": w.last, "first": w.first, "count": w.count, "level": "warn",
                "kind": if w.kind == "fingerprint" { "fingerprint_mismatch" } else { "bad_proof" },
                "msg": if w.kind == "fingerprint" { "Certificate doesn't match the paired Sentry Manager" }
                       else { "Server couldn't prove it is the paired Sentry Manager" },
                "address": w.address, "seen": w.seen,
            })
        })
        .collect();
    json!({"entries": entries, "warnings": warnings})
}
