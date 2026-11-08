//! Archive verification status for the Snapshots page, and its settings.
//!
//! archiveloop checks every clip it archives by SHA-256 (the Pi's snapshot
//! copy against the archive server's copy) and records the result in
//! `/mutable/archive_verified.tsv`, one line per check:
//!
//! ```text
//! <archive relpath>\t<size>\t<sha256>\t<unix>\t<status>\t<times verified>\t<Pi source>
//! ```
//!
//! `status` is `ok`, `converted`, `converted_unconfirmed`, `mismatch`,
//! `missing` or `nosource`. `converted`: Sentry Manager re-encoded the server
//! copy in place (H.265/AV1) and the original is confirmed by Manager's
//! checksum record or an earlier match; it counts as verified.
//! `converted_unconfirmed`: re-encoded before anything confirmed the
//! original; kept on the server but never counted as verified; `times verified`
//! counts consecutive `ok` checks of the same bytes. Clips waiting to be
//! checked are listed in `/mutable/archive_verify_pending`.
//!
//! This module matches those results against each snapshot's TOC
//! (`snap.bin.toc`, `"<size> <path>"` per file) so the Snapshots page can tag
//! a snapshot "Verified on archive". The matching rules are the same ones
//! archiveloop's `snapshot_verified_twice` uses before auto-deleting, so the
//! page shows exactly what auto-delete will decide.
//!
//!   * `GET  /api/archive-verify/settings` — the two toggles + context
//!   * `POST /api/archive-verify/settings` — `{"verify"?: bool, "auto_delete"?: bool}`

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::router::AppState;

pub const LEDGER: &str = "/mutable/archive_verified.tsv";
pub const QUEUE: &str = "/mutable/archive_verify_pending";
/// Relpaths archiveloop has moved to the archive (one per line). A clip on
/// this list with no ledger line was sent before verification existed (or
/// while it was off) and is queued for its first check at the next cycle.
pub const ARCHIVED: &str = "/mutable/sentry_files_archived";
/// The archive skips `.mp4` files below this size as short recordings.
const SHORT_RECORDING_BYTES: u64 = 100_000;

/// Archive-related settings, read from the ACTIVE lines of sentryusb.conf
/// only. A commented-out sample line (`#export ARCHIVE_RECENTCLIPS=true`)
/// is not a setting archiveloop sees, so it must not count here either.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ArchiveSettings {
    pub archive_system: String,
    pub verify: bool,
    pub auto_delete: bool,
    pub saved: bool,
    pub sentry: bool,
    pub recent: bool,
    pub track: bool,
}

fn flag(config: &sentryusb_config::SetupConfig, key: &str, default: bool) -> bool {
    match config.get(key).map(|v| v.trim().to_ascii_lowercase()) {
        Some(v) if matches!(v.as_str(), "true" | "yes" | "1" | "on") => true,
        Some(v) if matches!(v.as_str(), "false" | "no" | "0" | "off") => false,
        _ => default,
    }
}

impl ArchiveSettings {
    /// Defaults match archiveloop's `${VAR:-default}` expansions.
    pub fn from_config(active: &sentryusb_config::SetupConfig) -> Self {
        Self {
            archive_system: active
                .get("ARCHIVE_SYSTEM")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "none".to_string()),
            verify: flag(active, "ARCHIVE_VERIFY", true),
            auto_delete: flag(active, "ARCHIVE_AUTO_DELETE_VERIFIED", false),
            saved: flag(active, "ARCHIVE_SAVEDCLIPS", true),
            sentry: flag(active, "ARCHIVE_SENTRYCLIPS", true),
            recent: flag(active, "ARCHIVE_RECENTCLIPS", false),
            track: flag(active, "ARCHIVE_TRACKMODECLIPS", true),
        }
    }

    pub fn read() -> Option<Self> {
        Self::read_at(sentryusb_config::find_config_path())
    }

    fn read_at(path: &str) -> Option<Self> {
        sentryusb_config::parse_file(path)
            .ok()
            .map(|(active, _)| Self::from_config(&active))
    }

    /// Verification only runs with a real archive destination.
    pub fn verifying(&self) -> bool {
        self.verify && self.archive_system != "none"
    }

    fn archives(&self, category: &str) -> bool {
        match category {
            "SavedClips" => self.saved,
            "SentryClips" => self.sentry,
            "RecentClips" => self.recent,
            "TeslaTrackMode" => self.track,
            _ => false,
        }
    }
}

/// One snapshot's footage, as seen by the archive server.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Coverage {
    /// `verified` — every clip verified on the archive server;
    /// `pending` — sent, checksum check still queued;
    /// `partial` — some clips verified, others not yet archived;
    /// `failed` — a clip's server copy didn't match or was missing (it is
    ///   being re-sent); `unverified` — nothing verified yet;
    /// `nothing` — no clips in this snapshot.
    pub state: &'static str,
    /// Clips (`.mp4` of at least 100000 bytes) in the snapshot.
    pub clips: usize,
    /// Clips whose server copy matched by SHA-256 at least once.
    pub verified: usize,
    /// Clips verified at least twice (what auto-delete requires).
    pub verified_twice: usize,
    /// Of `verified`, clips Sentry Manager re-encoded in place on the server
    /// (H.265/AV1), with the original confirmed by checksum.
    pub converted: usize,
    /// Clips Sentry Manager re-encoded before the Pi or Manager's checksum
    /// record could confirm the original. Not verified; they keep the
    /// snapshot from being auto-deleted.
    pub unconfirmed: usize,
    /// Clips sent and waiting for their checksum check.
    pub pending: usize,
    /// Clips whose latest check failed (mismatch or missing).
    pub failed: usize,
    /// Clips in folders this device doesn't archive (never verifiable).
    pub not_archived: usize,
    /// True when auto-delete would release this snapshot (it never releases
    /// the newest one; the caller clears this flag for it).
    pub ready_to_delete: bool,
    /// When the most recent check of one of these clips ran.
    pub last_checked_unix: Option<i64>,
}

#[derive(Clone, Debug)]
struct LedgerEntry {
    size: u64,
    status: String,
    times: u32,
    unix: i64,
}

/// Map an archive relpath or a TOC path to the key both share.
/// TOC paths are CAM-filesystem paths (`TeslaCam/RecentClips/<file>`); the
/// archive groups RecentClips by day (`RecentClips/<date>/<file>`).
pub fn clip_key(path: &str) -> String {
    let p = path.strip_prefix("TeslaCam/").unwrap_or(path);
    if let Some(rest) = p.strip_prefix("RecentClips/") {
        let name = rest.rsplit('/').next().unwrap_or(rest);
        return format!("RecentClips/{name}");
    }
    p.to_string()
}

/// Latest ledger entry per clip key.
fn parse_ledger(text: &str) -> HashMap<String, LedgerEntry> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 6 {
            continue;
        }
        map.insert(
            clip_key(f[0]),
            LedgerEntry {
                size: f[1].parse().unwrap_or(0),
                status: f[4].to_string(),
                times: f[5].parse().unwrap_or(0),
                unix: f[3].parse().unwrap_or(0),
            },
        );
    }
    map
}

/// Clip keys from the queue (`relpath\t…` lines) or the archived list
/// (bare relpath lines): the key is the text before the first tab.
fn parse_queue(text: &str) -> HashSet<String> {
    text.lines()
        .filter_map(|l| l.split('\t').next())
        .filter(|r| !r.is_empty())
        .map(clip_key)
        .collect()
}

/// Coverage of one snapshot from its TOC text.
fn coverage(
    toc: &str,
    ledger: &HashMap<String, LedgerEntry>,
    queue: &HashSet<String>,
    archived: &HashSet<String>,
    settings: &ArchiveSettings,
) -> Coverage {
    let mut c = Coverage {
        state: "nothing",
        clips: 0,
        verified: 0,
        verified_twice: 0,
        converted: 0,
        unconfirmed: 0,
        pending: 0,
        failed: 0,
        not_archived: 0,
        ready_to_delete: false,
        last_checked_unix: None,
    };
    for line in toc.lines() {
        let Some((size, path)) = line.split_once(' ') else { continue };
        let Ok(size) = size.parse::<u64>() else { continue };
        if !path.to_ascii_lowercase().ends_with(".mp4") || size < SHORT_RECORDING_BYTES {
            continue;
        }
        let p = path.strip_prefix("TeslaCam/").unwrap_or(path);
        let category = p.split('/').next().unwrap_or("");
        c.clips += 1;
        if !settings.archives(category) {
            c.not_archived += 1;
            continue;
        }
        let key = clip_key(path);
        match ledger.get(&key) {
            // The server copy must be at least as large as this snapshot's:
            // an older snapshot can hold a clip the car was still writing.
            Some(e) if (e.status == "ok" || e.status == "converted") && e.size >= size => {
                c.verified += 1;
                if e.status == "converted" {
                    c.converted += 1;
                }
                if e.times >= 2 {
                    c.verified_twice += 1;
                }
                c.last_checked_unix = c.last_checked_unix.max(Some(e.unix));
            }
            Some(e) if e.status == "converted_unconfirmed" => {
                c.unconfirmed += 1;
                c.last_checked_unix = c.last_checked_unix.max(Some(e.unix));
            }
            Some(e) if (e.status == "mismatch" || e.status == "missing") && !queue.contains(&key) => {
                c.failed += 1;
                c.last_checked_unix = c.last_checked_unix.max(Some(e.unix));
            }
            _ if queue.contains(&key) => c.pending += 1,
            // Sent before verification existed; its first check is queued
            // at the next archive cycle.
            None if archived.contains(&key) => c.pending += 1,
            _ => {}
        }
    }
    c.state = if c.clips == 0 {
        "nothing"
    } else if c.failed > 0 {
        "failed"
    } else if c.verified == c.clips {
        "verified"
    } else if c.verified + c.pending == c.clips {
        "pending"
    } else if c.verified > 0 || c.pending > 0 {
        "partial"
    } else {
        "unverified"
    };
    c.ready_to_delete = settings.auto_delete && c.clips > 0 && c.verified_twice == c.clips;
    c
}

/// What the computed coverage depends on: file identities plus settings.
type CacheKey = (Vec<(PathBuf, Option<(SystemTime, u64)>)>, ArchiveSettings);

fn identity(path: &Path) -> Option<(SystemTime, u64)> {
    std::fs::metadata(path).ok().and_then(|m| Some((m.modified().ok()?, m.len())))
}

/// The last computed coverage and what it was computed from.
type CoverageCache = Mutex<Option<(CacheKey, HashMap<String, Coverage>)>>;

fn cache() -> &'static CoverageCache {
    static CACHE: OnceLock<CoverageCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Coverage for each snapshot id under `snapshots_dir`, or `None` when
/// verification isn't running on this device. Blocking; call from
/// `spawn_blocking`. Cached until a TOC, the ledger, the queue or the
/// settings change.
pub fn coverage_for(snapshots_dir: &Path, ids: &[String]) -> Option<HashMap<String, Coverage>> {
    let settings = ArchiveSettings::read()?;
    coverage_with(snapshots_dir, Path::new(LEDGER), Path::new(QUEUE), Path::new(ARCHIVED), ids, settings)
}

fn coverage_with(
    snapshots_dir: &Path,
    ledger_path: &Path,
    queue_path: &Path,
    archived_path: &Path,
    ids: &[String],
    settings: ArchiveSettings,
) -> Option<HashMap<String, Coverage>> {
    if !settings.verifying() {
        return None;
    }
    let mut files: Vec<PathBuf> = ids.iter().map(|id| snapshots_dir.join(id).join("snap.bin.toc")).collect();
    files.push(ledger_path.to_path_buf());
    files.push(queue_path.to_path_buf());
    files.push(archived_path.to_path_buf());
    let key: CacheKey = (files.iter().map(|p| (p.clone(), identity(p))).collect(), settings.clone());
    if let Ok(guard) = cache().lock() {
        if let Some((k, v)) = guard.as_ref() {
            if *k == key {
                return Some(v.clone());
            }
        }
    }

    let ledger = parse_ledger(&std::fs::read_to_string(ledger_path).unwrap_or_default());
    let queue = parse_queue(&std::fs::read_to_string(queue_path).unwrap_or_default());
    let archived = parse_queue(&std::fs::read_to_string(archived_path).unwrap_or_default());
    let mut out = HashMap::new();
    for id in ids {
        // No TOC: the snapshot isn't complete (or is being made); say nothing.
        if let Ok(toc) = std::fs::read_to_string(snapshots_dir.join(id).join("snap.bin.toc")) {
            out.insert(id.clone(), coverage(&toc, &ledger, &queue, &archived, &settings));
        }
    }
    if let Ok(mut guard) = cache().lock() {
        *guard = Some((key, out.clone()));
    }
    Some(out)
}

#[derive(Serialize)]
struct SettingsBody {
    verify: bool,
    auto_delete: bool,
    archive_system: String,
    /// False when RecentClips aren't archived: snapshots holding them can
    /// never be fully verified, so auto-delete keeps them.
    recent_clips_archived: bool,
}

fn settings_body(s: &ArchiveSettings) -> SettingsBody {
    SettingsBody {
        verify: s.verify,
        auto_delete: s.auto_delete,
        archive_system: s.archive_system.clone(),
        recent_clips_archived: s.recent,
    }
}

/// GET /api/archive-verify/settings
pub async fn get_settings(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    match tokio::task::spawn_blocking(ArchiveSettings::read).await {
        Ok(Some(s)) => (StatusCode::OK, Json(serde_json::json!(settings_body(&s)))),
        _ => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, "Could not read the configuration"),
    }
}

#[derive(Deserialize)]
pub struct SettingsUpdate {
    #[serde(default)]
    verify: Option<bool>,
    #[serde(default)]
    auto_delete: Option<bool>,
}

/// POST /api/archive-verify/settings — `{"verify"?: bool, "auto_delete"?: bool}`.
///
/// Writes `ARCHIVE_VERIFY` / `ARCHIVE_AUTO_DELETE_VERIFIED` to sentryusb.conf.
/// archiveloop reads both fresh each cycle, so no restart is needed.
/// Turning verification off also turns auto-delete off: auto-delete must
/// never act on results that are no longer being kept up to date.
pub async fn set_settings(
    State(_s): State<AppState>,
    Json(body): Json<SettingsUpdate>,
) -> (StatusCode, Json<serde_json::Value>) {
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<ArchiveSettings> {
        let path = sentryusb_config::find_config_path();
        let (mut active, _) = sentryusb_config::parse_file(path)?;
        if let Some(v) = body.verify {
            active.insert("ARCHIVE_VERIFY".into(), v.to_string());
            if !v {
                active.insert("ARCHIVE_AUTO_DELETE_VERIFIED".into(), "false".into());
            }
        }
        if let Some(v) = body.auto_delete {
            let verify_on = flag(&active, "ARCHIVE_VERIFY", true);
            anyhow::ensure!(!v || verify_on, "Turn on archive verification before auto-delete");
            active.insert("ARCHIVE_AUTO_DELETE_VERIFIED".into(), v.to_string());
        }
        let _ = std::process::Command::new("bash").args(["-c", "/root/bin/remountfs_rw"]).status();
        sentryusb_config::write_file(path, &active)?;
        ArchiveSettings::read_at(path).ok_or_else(|| anyhow::anyhow!("config unreadable after write"))
    })
    .await;
    match result {
        Ok(Ok(s)) => (StatusCode::OK, Json(serde_json::json!(settings_body(&s)))),
        Ok(Err(e)) => crate::json_error(StatusCode::BAD_REQUEST, &format!("{e}")),
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("config task failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(recent: bool, auto_delete: bool) -> ArchiveSettings {
        ArchiveSettings {
            archive_system: "cifs".into(),
            verify: true,
            auto_delete,
            saved: true,
            sentry: true,
            recent,
            track: true,
        }
    }

    fn ledger(lines: &[&str]) -> HashMap<String, LedgerEntry> {
        parse_ledger(&lines.join("\n"))
    }

    const TOC: &str = "150000 TeslaCam/RecentClips/2026-11-07_10-00-00-front.mp4\n\
                       150000 TeslaCam/SentryClips/ev/2026-11-07_10-00-00-back.mp4\n\
                       500 TeslaCam/RecentClips/2026-11-07_10-05-00-front.mp4\n\
                       900 TeslaCam/SentryClips/ev/event.json\n";

    #[test]
    fn keys_match_between_archive_and_toc_paths() {
        assert_eq!(
            clip_key("RecentClips/2026-11-07/2026-11-07_10-00-00-front.mp4"),
            clip_key("TeslaCam/RecentClips/2026-11-07_10-00-00-front.mp4")
        );
        assert_eq!(clip_key("TeslaCam/SentryClips/ev/a.mp4"), "SentryClips/ev/a.mp4");
        assert_eq!(clip_key("TeslaTrackMode/a.mp4"), "TeslaTrackMode/a.mp4");
    }

    #[test]
    fn all_clips_verified_twice_is_verified_and_ready() {
        let l = ledger(&[
            "RecentClips/2026-11-07/2026-11-07_10-00-00-front.mp4\t150000\th\t10\tok\t2\tx",
            "SentryClips/ev/2026-11-07_10-00-00-back.mp4\t150000\th\t20\tok\t2\tx",
        ]);
        let c = coverage(TOC, &l, &HashSet::new(), &HashSet::new(), &settings(true, true));
        assert_eq!(c.state, "verified");
        assert_eq!((c.clips, c.verified, c.verified_twice), (2, 2, 2), "short clip and json ignored");
        assert!(c.ready_to_delete);
        assert_eq!(c.last_checked_unix, Some(20));
        // Same data with auto-delete off: verified, never "ready".
        assert!(!coverage(TOC, &l, &HashSet::new(), &HashSet::new(), &settings(true, false)).ready_to_delete);
    }

    #[test]
    fn verified_once_is_not_ready_to_delete() {
        let l = ledger(&[
            "RecentClips/2026-11-07/2026-11-07_10-00-00-front.mp4\t150000\th\t10\tok\t1\tx",
            "SentryClips/ev/2026-11-07_10-00-00-back.mp4\t150000\th\t20\tok\t2\tx",
        ]);
        let c = coverage(TOC, &l, &HashSet::new(), &HashSet::new(), &settings(true, true));
        assert_eq!(c.state, "verified");
        assert!(!c.ready_to_delete);
    }

    #[test]
    fn unarchived_folder_blocks_verified_and_auto_delete() {
        let l = ledger(&["SentryClips/ev/2026-11-07_10-00-00-back.mp4\t150000\th\t20\tok\t2\tx"]);
        let c = coverage(TOC, &l, &HashSet::new(), &HashSet::new(), &settings(false, true));
        assert_eq!(c.state, "partial");
        assert_eq!(c.not_archived, 1);
        assert!(!c.ready_to_delete);
    }

    #[test]
    fn failed_pending_and_short_copies() {
        let failed = ledger(&["SentryClips/ev/2026-11-07_10-00-00-back.mp4\t150000\th\t20\tmismatch\t0\tx"]);
        assert_eq!(coverage(TOC, &failed, &HashSet::new(), &HashSet::new(), &settings(true, false)).state, "failed");

        // Re-queued after the failure: being re-sent, not failed.
        let queue = parse_queue("RecentClips/2026-11-07/2026-11-07_10-00-00-front.mp4\tsrc\n\
                                 SentryClips/ev/2026-11-07_10-00-00-back.mp4\tsrc\n");
        assert_eq!(coverage(TOC, &failed, &queue, &HashSet::new(), &settings(true, false)).state, "pending");

        // A server copy smaller than this snapshot's doesn't cover it.
        let small = ledger(&[
            "RecentClips/2026-11-07/2026-11-07_10-00-00-front.mp4\t100\th\t10\tok\t2\tx",
            "SentryClips/ev/2026-11-07_10-00-00-back.mp4\t150000\th\t20\tok\t2\tx",
        ]);
        let c = coverage(TOC, &small, &HashSet::new(), &HashSet::new(), &settings(true, true));
        assert_eq!((c.state, c.verified), ("partial", 1));
        assert!(!c.ready_to_delete);

        assert_eq!(coverage("", &small, &HashSet::new(), &HashSet::new(), &settings(true, true)).state, "nothing");
        assert_eq!(coverage(TOC, &HashMap::new(), &HashSet::new(), &HashSet::new(), &settings(true, true)).state, "unverified");
    }

    #[test]
    fn clips_archived_before_verification_show_as_pending() {
        // Both clips sent before verification existed: on the archived
        // list, no ledger line yet. They're "sent, check queued", never
        // "not archived".
        let archived = parse_queue("RecentClips/2026-11-07/2026-11-07_10-00-00-front.mp4\n\
                                    SentryClips/ev/2026-11-07_10-00-00-back.mp4\n");
        let c = coverage(TOC, &HashMap::new(), &HashSet::new(), &archived, &settings(true, true));
        assert_eq!((c.state, c.pending, c.verified), ("pending", 2, 0));
        assert!(!c.ready_to_delete);
        // Once checked, the ledger decides, not the archived list.
        let l = ledger(&["SentryClips/ev/2026-11-07_10-00-00-back.mp4\t150000\th\t20\tmismatch\t0\tx"]);
        assert_eq!(coverage(TOC, &l, &HashSet::new(), &archived, &settings(true, true)).state, "failed");
    }

    #[test]
    fn clips_re_encoded_by_sentry_manager_count_as_verified() {
        let l = ledger(&[
            "RecentClips/2026-11-07/2026-11-07_10-00-00-front.mp4\t150000\th\t10\tconverted\t2\tx",
            "SentryClips/ev/2026-11-07_10-00-00-back.mp4\t150000\th\t20\tok\t2\tx",
        ]);
        let c = coverage(TOC, &l, &HashSet::new(), &HashSet::new(), &settings(true, true));
        assert_eq!((c.state, c.verified, c.converted, c.verified_twice), ("verified", 2, 1, 2));
        assert!(c.ready_to_delete);
    }

    #[test]
    fn clips_re_encoded_before_any_check_are_not_verified() {
        let l = ledger(&[
            "RecentClips/2026-11-07/2026-11-07_10-00-00-front.mp4\t150000\th\t10\tconverted_unconfirmed\t0\tx",
            "SentryClips/ev/2026-11-07_10-00-00-back.mp4\t150000\th\t20\tok\t2\tx",
        ]);
        let c = coverage(TOC, &l, &HashSet::new(), &HashSet::new(), &settings(true, true));
        assert_eq!((c.state, c.verified, c.unconfirmed), ("partial", 1, 1));
        assert!(!c.ready_to_delete);
    }

    #[test]
    fn latest_ledger_line_wins() {
        let l = ledger(&[
            "SentryClips/ev/a.mp4\t150000\th\t10\tmismatch\t0\tx",
            "SentryClips/ev/a.mp4\t150000\th\t20\tok\t1\tx",
        ]);
        assert_eq!(l["SentryClips/ev/a.mp4"].status, "ok");
    }

    #[test]
    fn settings_read_active_lines_only_with_archiveloop_defaults() {
        let mut active = sentryusb_config::SetupConfig::new();
        active.insert("ARCHIVE_SYSTEM".into(), "rsync".into());
        let s = ArchiveSettings::from_config(&active);
        assert!(s.verify && !s.auto_delete && !s.recent && s.saved && s.sentry && s.track);
        assert!(s.verifying());
        active.insert("ARCHIVE_VERIFY".into(), "false".into());
        assert!(!ArchiveSettings::from_config(&active).verifying());
        let none = ArchiveSettings::from_config(&sentryusb_config::SetupConfig::new());
        assert!(!none.verifying(), "no archive system, nothing to verify");
    }

    #[test]
    fn coverage_files_are_read_and_cached() {
        let d = tempfile::tempdir().unwrap();
        let snaps = d.path().join("snapshots");
        std::fs::create_dir_all(snaps.join("snap-000001")).unwrap();
        std::fs::create_dir_all(snaps.join("snap-000002")).unwrap();
        std::fs::write(snaps.join("snap-000001/snap.bin.toc"), TOC).unwrap();
        let ledger_path = d.path().join("ledger");
        let queue_path = d.path().join("queue");
        std::fs::write(&ledger_path, "SentryClips/ev/2026-11-07_10-00-00-back.mp4\t150000\th\t20\tok\t1\tx\n").unwrap();
        let ids = vec!["snap-000001".to_string(), "snap-000002".to_string()];
        let out = coverage_with(&snaps, &ledger_path, &queue_path, &d.path().join("archived"), &ids, settings(true, false)).unwrap();
        assert_eq!(out["snap-000001"].state, "partial");
        assert!(!out.contains_key("snap-000002"), "no TOC, no claim");
        let off = ArchiveSettings { verify: false, ..settings(true, false) };
        assert!(coverage_with(&snaps, &ledger_path, &queue_path, &d.path().join("archived"), &ids, off).is_none());
    }
}
