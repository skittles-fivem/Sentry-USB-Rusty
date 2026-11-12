//! `.sentryusb/device-status.json` on the archive share: what Sentry
//! Manager shows about the Pi (version, storage, drives, CPU temperature and
//! the car's latest readings), read from the share the Pi archives to.
//! The format is in [`document`] and the tests below; the repo has no
//! separate share-contract document.
//!
//! When it is written:
//! - at the start and the end of every archive cycle: archiveloop asks for it
//!   (`POST /api/system/device-status/write` for a mounted SMB/NFS share,
//!   `GET /api/system/device-status` to send it itself over rsync/rclone);
//! - every 5 minutes while the SMB/NFS share is mounted at /mnt/archive;
//! - at most once a minute, while mounted, when a car reading changes.
//!
//! The file is replaced atomically (`device-status.json.tmp` in the same
//! folder, then a rename). It is best effort: a failure is logged and never
//! holds up the archive, and nothing waits on the share lock.

pub mod car;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};
use tower::ServiceExt;

/// Folder at the share root, shared with archive-status.json.
pub const DIR: &str = ".sentryusb";
pub const NAME: &str = "device-status.json";
pub const TMP_NAME: &str = "device-status.json.tmp";
/// Largest file written; Manager refuses bigger ones.
pub const MAX_BYTES: usize = 16 * 1024;
pub const SCHEMA: u64 = 1;
const MAX_VERSION_LEN: usize = 64;
const MAX_DRIVES: usize = 8;
/// The SMB/NFS archive share's mount point (ARCHIVE_MOUNT in archiveloop).
const ARCHIVE_MOUNT: &str = "/mnt/archive";
const BACKING_DIR: &str = "/backingfiles";
const CPU_TEMP_PATH: &str = "/sys/class/thermal/thermal_zone0/temp";
/// Drive images, in the contract's names.
const DRIVES: [(&str, &str); 4] = [
    ("cam", "cam_disk.bin"),
    ("music", "music_disk.bin"),
    ("lightshow", "lightshow_disk.bin"),
    ("boombox", "boombox_disk.bin"),
];

const TICK: Duration = Duration::from_secs(30);
const REFRESH: Duration = Duration::from_secs(5 * 60);
const CAR_MIN_GAP: Duration = Duration::from_secs(60);
/// A write that hasn't finished by then is given up on (a dead share).
const WRITE_TIMEOUT: Duration = Duration::from_secs(20);
/// Each in-process GET.
const GET_TIMEOUT: Duration = Duration::from_secs(15);

/// Everything the file is built from. `None` and empty mean unknown.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub written_unix: i64,
    pub version: Option<String>,
    /// Backing-files filesystem: (total, free) bytes.
    pub storage: Option<(u64, u64)>,
    /// (name, size the car sees) for each drive image that exists.
    pub drives: Vec<(String, u64)>,
    pub cpu_temp_c: Option<f64>,
    pub car: Option<Value>,
}

fn round1(n: f64) -> f64 {
    (n * 10.0).round() / 10.0
}

/// The JSON document. Unknown fields are left out.
pub fn document(f: &Facts) -> Value {
    let mut m = Map::new();
    m.insert("schema".into(), SCHEMA.into());
    m.insert("written_unix".into(), f.written_unix.into());
    if let Some(v) = f
        .version
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty() && v.chars().count() <= MAX_VERSION_LEN)
    {
        m.insert("version".into(), v.into());
    }
    if let Some((total, free)) = f.storage.filter(|(total, free)| *total > 0 && free <= total) {
        m.insert(
            "storage".into(),
            json!({"total_bytes": total, "used_bytes": total - free, "free_bytes": free}),
        );
    }
    let drives: Vec<Value> = f
        .drives
        .iter()
        .filter(|(name, size)| *size > 0 && DRIVES.iter().any(|(n, _)| n == name))
        .take(MAX_DRIVES)
        .map(|(name, size)| json!({"name": name, "size_bytes": size}))
        .collect();
    if !drives.is_empty() {
        m.insert("drives".into(), Value::Array(drives));
    }
    if let Some(t) = f
        .cpu_temp_c
        .filter(|t| t.is_finite() && (-40.0..=125.0).contains(t))
    {
        m.insert("cpu_temp_c".into(), round1(t).into());
    }
    if let Some(car) = f.car.as_ref().filter(|c| c.as_object().is_some_and(|o| !o.is_empty())) {
        m.insert("car".into(), car.clone());
    }
    Value::Object(m)
}

/// The file's bytes, at most MAX_BYTES: when too big, the car object goes
/// first, then the drive list. None if even that doesn't fit.
pub fn render(f: &Facts) -> Option<Vec<u8>> {
    let mut f = f.clone();
    loop {
        let bytes = serde_json::to_vec_pretty(&document(&f)).ok()?;
        if bytes.len() <= MAX_BYTES {
            return Some(bytes);
        }
        if f.car.is_some() {
            f.car = None;
        } else if !f.drives.is_empty() {
            f.drives.clear();
        } else {
            return None;
        }
    }
}

/// Write `<root>/.sentryusb/device-status.json`: the bytes go to
/// `device-status.json.tmp` in the same folder, then it is renamed over the
/// real name, so a reader sees the old file or the new one, never half.
pub fn write_atomic(root: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = root.join(DIR);
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(TMP_NAME);
    let result = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, dir.join(NAME))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// thermal_zone temp (millidegrees) in °C.
pub fn parse_cpu_temp(raw: &str) -> Option<f64> {
    let milli = raw.trim().parse::<f64>().ok()?;
    let c = milli / 1000.0;
    (c.is_finite() && (-40.0..=125.0).contains(&c)).then(|| round1(c))
}

/// The drive images under `root`, each at its full size (what the car sees).
pub fn drives_in(root: &Path) -> Vec<(String, u64)> {
    DRIVES
        .iter()
        .filter_map(|(name, file)| {
            let meta = std::fs::metadata(root.join(file)).ok()?;
            (meta.is_file() && meta.len() > 0).then(|| (name.to_string(), meta.len()))
        })
        .collect()
}

/// True when `path` is a mounted SMB or NFS share in /proc/mounts.
pub fn share_mounted(mounts: &str, path: &str) -> bool {
    mounts.lines().any(|line| {
        let mut fields = line.split_whitespace();
        let (_, Some(mnt), Some(fstype)) = (fields.next(), fields.next(), fields.next()) else {
            return false;
        };
        mnt == path && matches!(fstype, "cifs" | "smb3" | "nfs" | "nfs4")
    })
}

fn archive_share_mounted() -> bool {
    std::fs::read_to_string("/proc/mounts")
        .map(|m| share_mounted(&m, ARCHIVE_MOUNT))
        .unwrap_or(false)
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── Gathering, in-process ───────────────────────────────────────────────

static ROUTER: OnceLock<axum::Router> = OnceLock::new();

/// One GET against the router, its JSON answer (None on any failure).
async fn get_json(router: &axum::Router, path: &str) -> Option<Value> {
    let req = Request::get(path).body(Body::empty()).ok()?;
    let Ok(Ok(resp)) = tokio::time::timeout(GET_TIMEOUT, router.clone().oneshot(req)).await else {
        return None;
    };
    if !resp.status().is_success() {
        return None;
    }
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024).await.ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()
}

/// The car object from the Pi's own telemetry answers and settings.
async fn car_object(router: &axum::Router) -> Option<Value> {
    let sample = get_json(router, "/api/system/ble-latest-sample").await?;
    let charge = get_json(router, "/api/charging/current").await;
    let last_drive_end = get_json(router, "/api/drives/stats").await.and_then(|s| {
        s.get("latest_drive_end")
            .and_then(Value::as_str)
            .and_then(car::parse_local_time)
    });
    let config = tokio::task::spawn_blocking(|| {
        sentryusb_config::parse_file(sentryusb_config::find_config_path())
            .map(|(active, _)| active)
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default();
    car::build(&car::Inputs {
        sample: Some(&sample),
        charge: charge.as_ref(),
        last_drive_end,
        config: &config,
        now: now_unix(),
    })
}

async fn collect(router: &axum::Router) -> Facts {
    let local = tokio::task::spawn_blocking(|| {
        let health = crate::status::managed_storage_health();
        let storage = (health.total_bytes > 0).then_some((health.total_bytes, health.free_bytes));
        let cpu = std::fs::read_to_string(CPU_TEMP_PATH)
            .ok()
            .and_then(|raw| parse_cpu_temp(&raw));
        (storage, drives_in(Path::new(BACKING_DIR)), cpu)
    })
    .await;
    let (storage, drives, cpu_temp_c) = local.unwrap_or_default();
    Facts {
        written_unix: now_unix(),
        version: crate::device_reporting::running_version(
            option_env!("SENTRYUSB_RELEASE_VERSION"),
            &crate::update::read_current_version(),
        ),
        storage,
        drives,
        cpu_temp_c,
        car: car_object(router).await,
    }
}

// ── Writing to the mounted share ────────────────────────────────────────

#[derive(Default)]
struct Written {
    at: Option<Instant>,
    /// car::signature of the car object last written.
    car: Option<Value>,
}

fn written() -> &'static Mutex<Written> {
    static W: OnceLock<Mutex<Written>> = OnceLock::new();
    W.get_or_init(|| Mutex::new(Written::default()))
}

/// Set while a write runs on a blocking thread, cleared when it really ends
/// (even after its timeout), so a hung share never gets a second writer.
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Builds the file and writes it to the share root `root`.
async fn write_to(root: PathBuf, facts: Facts) -> anyhow::Result<usize> {
    let bytes = render(&facts).ok_or_else(|| anyhow::anyhow!("device status over {MAX_BYTES} bytes"))?;
    if IN_FLIGHT.swap(true, Ordering::SeqCst) {
        anyhow::bail!("an earlier write to the share hasn't finished");
    }
    let n = bytes.len();
    let task = tokio::task::spawn_blocking(move || {
        let r = write_atomic(&root, &bytes);
        IN_FLIGHT.store(false, Ordering::SeqCst);
        r
    });
    match tokio::time::timeout(WRITE_TIMEOUT, task).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(e))) => return Err(e.into()),
        Ok(Err(e)) => anyhow::bail!("write task failed: {e}"),
        Err(_) => anyhow::bail!("share didn't answer in {}s", WRITE_TIMEOUT.as_secs()),
    }
    if let Ok(mut w) = written().lock() {
        w.at = Some(Instant::now());
        w.car = facts.car.as_ref().map(car::signature);
    }
    Ok(n)
}

/// Whether the loop should write now, given the last write and the car now.
fn due(last_at: Option<Instant>, last_car: Option<&Value>, car_now: Option<&Value>, now: Instant) -> bool {
    let Some(at) = last_at else { return true };
    let since = now.saturating_duration_since(at);
    since >= REFRESH || (since >= CAR_MIN_GAP && last_car != car_now)
}

async fn tick(router: &axum::Router) {
    if !tokio::task::spawn_blocking(archive_share_mounted).await.unwrap_or(false) {
        return;
    }
    let (last_at, last_car) = written()
        .lock()
        .map(|w| (w.at, w.car.clone()))
        .unwrap_or_default();
    let fresh = last_at.is_some_and(|at| at.elapsed() < CAR_MIN_GAP);
    if fresh {
        return;
    }
    let facts = collect(router).await;
    let car_now = facts.car.as_ref().map(car::signature);
    if !due(last_at, last_car.as_ref(), car_now.as_ref(), Instant::now()) {
        return;
    }
    if let Err(e) = write_to(PathBuf::from(ARCHIVE_MOUNT), facts).await {
        tracing::warn!("[device-status] could not write {DIR}/{NAME} to the archive share: {e:#}");
    }
}

/// Starts the 5-minute / car-change writer for the mounted share, and makes
/// the router available to the two routes. Call once with the full router
/// (or the degraded one); later calls are ignored.
pub fn spawn(router: axum::Router) {
    if ROUTER.set(router.clone()).is_err() {
        return;
    }
    tokio::spawn(async move {
        let mut every = tokio::time::interval(TICK);
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            every.tick().await;
            tick(&router).await;
        }
    });
}

/// GET /api/system/device-status: the file as it would be written now.
pub async fn get_document() -> Response {
    let Some(router) = ROUTER.get() else {
        return crate::json_error(StatusCode::SERVICE_UNAVAILABLE, "starting").into_response();
    };
    match render(&collect(router).await) {
        Some(bytes) => ([(header::CONTENT_TYPE, "application/json")], bytes).into_response(),
        None => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, "device status too large").into_response(),
    }
}

/// POST /api/system/device-status/write: write it to the mounted SMB/NFS
/// share now (archiveloop, at the start and end of a cycle).
pub async fn post_write() -> Response {
    let Some(router) = ROUTER.get() else {
        return crate::json_error(StatusCode::SERVICE_UNAVAILABLE, "starting").into_response();
    };
    if !tokio::task::spawn_blocking(archive_share_mounted).await.unwrap_or(false) {
        return crate::json_error(StatusCode::CONFLICT, "archive share not mounted").into_response();
    }
    match write_to(PathBuf::from(ARCHIVE_MOUNT), collect(router).await).await {
        Ok(n) => (StatusCode::OK, Json(json!({"written": true, "bytes": n}))).into_response(),
        Err(e) => {
            tracing::warn!("[device-status] could not write {DIR}/{NAME} to the archive share: {e:#}");
            crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}")).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_762_800_000;

    fn full() -> Facts {
        Facts {
            written_unix: NOW,
            version: Some("v3.28.10".into()),
            storage: Some((256_060_514_304, 157_960_514_304)),
            drives: vec![("cam".into(), 64_424_509_440), ("music".into(), 4_294_967_296)],
            cpu_temp_c: Some(51.23),
            car: Some(json!({"battery": {"pct": 89.0, "charging": false, "at": NOW}})),
        }
    }

    #[test]
    fn document_matches_the_contract() {
        assert_eq!(
            document(&full()),
            json!({
                "schema": 1,
                "written_unix": NOW,
                "version": "v3.28.10",
                "storage": {"total_bytes": 256_060_514_304u64, "used_bytes": 98_100_000_000u64, "free_bytes": 157_960_514_304u64},
                "drives": [{"name": "cam", "size_bytes": 64_424_509_440u64}, {"name": "music", "size_bytes": 4_294_967_296u64}],
                "cpu_temp_c": 51.2,
                "car": {"battery": {"pct": 89.0, "charging": false, "at": NOW}},
            })
        );
    }

    #[test]
    fn unknown_fields_are_left_out() {
        let bare = Facts { written_unix: NOW, ..Facts::default() };
        assert_eq!(document(&bare), json!({"schema": 1, "written_unix": NOW}));
        let odd = Facts {
            written_unix: NOW,
            version: Some("x".repeat(65)),
            storage: Some((0, 0)),
            drives: vec![("usb".into(), 10), ("cam".into(), 0)],
            cpu_temp_c: Some(f64::NAN),
            car: Some(json!({})),
        };
        assert_eq!(document(&odd), json!({"schema": 1, "written_unix": NOW}));
        let hot = Facts { cpu_temp_c: Some(130.0), ..bare.clone() };
        assert!(document(&hot).get("cpu_temp_c").is_none());
        let too_many = Facts {
            drives: (0..20).map(|_| ("cam".to_string(), 1)).collect(),
            ..bare
        };
        assert_eq!(document(&too_many)["drives"].as_array().unwrap().len(), MAX_DRIVES);
    }

    #[test]
    fn size_is_capped_by_dropping_the_car_first() {
        let fits = render(&full()).unwrap();
        assert!(fits.len() <= MAX_BYTES);
        let doc: Value = serde_json::from_slice(&fits).unwrap();
        assert!(doc.get("car").is_some());

        let mut big = full();
        big.car = Some(json!({"battery": {"pct": 1.0, "pad": "x".repeat(MAX_BYTES)}}));
        let bytes = render(&big).unwrap();
        assert!(bytes.len() <= MAX_BYTES);
        let doc: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(doc.get("car").is_none());
        assert_eq!(doc["schema"], 1);
        assert_eq!(doc["drives"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn write_is_atomic_through_the_tmp_name() {
        let root = tempfile::tempdir().unwrap();
        // An existing file and a leftover temp file from an interrupted write.
        std::fs::create_dir_all(root.path().join(DIR)).unwrap();
        std::fs::write(root.path().join(DIR).join(NAME), b"old").unwrap();
        std::fs::write(root.path().join(DIR).join(TMP_NAME), b"half").unwrap();
        // Its neighbour is never touched.
        std::fs::write(root.path().join(DIR).join("archive-status.json"), b"{}").unwrap();

        let bytes = render(&full()).unwrap();
        write_atomic(root.path(), &bytes).unwrap();
        assert_eq!(std::fs::read(root.path().join(DIR).join(NAME)).unwrap(), bytes);
        let mut names: Vec<String> = std::fs::read_dir(root.path().join(DIR))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["archive-status.json", NAME], "no temp file left behind");

        // The folder is created when missing.
        let fresh = tempfile::tempdir().unwrap();
        write_atomic(fresh.path(), b"{}").unwrap();
        assert_eq!(std::fs::read(fresh.path().join(DIR).join(NAME)).unwrap(), b"{}");
    }

    #[test]
    fn a_failed_write_keeps_the_old_file() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(NAME), b"old").unwrap();
        // The temp name is taken by a folder: the write fails.
        std::fs::create_dir(dir.join(TMP_NAME)).unwrap();
        assert!(write_atomic(root.path(), b"new").is_err());
        assert_eq!(std::fs::read(dir.join(NAME)).unwrap(), b"old");
    }

    #[test]
    fn cpu_temp_and_drives_are_read_from_the_system_files() {
        assert_eq!(parse_cpu_temp("51234\n"), Some(51.2));
        assert_eq!(parse_cpu_temp("-5000"), Some(-5.0));
        assert_eq!(parse_cpu_temp("200000"), None);
        assert_eq!(parse_cpu_temp("hot"), None);

        let root = tempfile::tempdir().unwrap();
        std::fs::File::create(root.path().join("cam_disk.bin")).unwrap().set_len(1 << 30).unwrap();
        std::fs::File::create(root.path().join("boombox_disk.bin")).unwrap().set_len(1 << 20).unwrap();
        std::fs::write(root.path().join("music_disk.bin"), b"").unwrap();
        std::fs::create_dir(root.path().join("lightshow_disk.bin")).unwrap();
        assert_eq!(
            drives_in(root.path()),
            vec![("cam".to_string(), 1 << 30), ("boombox".to_string(), 1 << 20)]
        );
    }

    #[test]
    fn only_a_mounted_network_share_counts() {
        let mounts = "/dev/sda1 /backingfiles xfs rw 0 0\n//nas/cam /mnt/archive cifs rw,vers=3.0 0 0\n";
        assert!(share_mounted(mounts, "/mnt/archive"));
        assert!(share_mounted("nas:/cam /mnt/archive nfs4 rw 0 0", "/mnt/archive"));
        assert!(!share_mounted("/dev/root / ext4 rw 0 0\n", "/mnt/archive"));
        assert!(!share_mounted("/dev/sdb1 /mnt/archive ext4 rw 0 0", "/mnt/archive"));
        assert!(!share_mounted("//nas/m /mnt/musicarchive cifs rw 0 0", "/mnt/archive"));
    }

    #[test]
    fn writes_every_five_minutes_or_on_a_car_change_once_a_minute() {
        let t0 = Instant::now();
        let a = json!({"battery": {"pct": 80.0}});
        let b = json!({"battery": {"pct": 79.0}});
        assert!(due(None, None, None, t0), "first write");
        let at = |s: u64| t0 + Duration::from_secs(s);
        assert!(!due(Some(t0), Some(&a), Some(&a), at(120)));
        assert!(due(Some(t0), Some(&a), Some(&a), at(300)));
        assert!(!due(Some(t0), Some(&a), Some(&b), at(30)), "a change waits a minute");
        assert!(due(Some(t0), Some(&a), Some(&b), at(60)));
        assert!(due(Some(t0), None, Some(&a), at(61)), "first car reading");
    }

    #[tokio::test]
    async fn car_object_comes_from_the_routers_answers() {
        use axum::routing::get;
        let r = axum::Router::new().route(
            "/api/system/ble-latest-sample",
            get(|| async {
                axum::Json(json!({"ts": 1, "battery_pct": 80.0, "location_name": "Home",
                    "vin": "5YJ3E1EA0KF000000", "field_secs_ago": {"battery_pct": 3}}))
            }),
        );
        let car = car_object(&r).await.unwrap();
        assert_eq!(car["battery"]["pct"], 80.0);
        let text = car.to_string();
        assert!(!text.contains("Home") && !text.contains("5YJ3"));

        let r = axum::Router::new()
            .route("/api/system/ble-latest-sample", get(|| async { axum::Json(json!({"ts": null})) }));
        assert!(car_object(&r).await.is_none(), "no readings: no car key");
    }
}
