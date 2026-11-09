//! Snapshot management API.
//!
//! XFS reflink snapshots live at
//! `/backingfiles/snapshots/snap-<id>/snap.bin`. This module provides:
//!
//!   * `GET    /api/snapshots`               — list with size/timestamp
//!   * `DELETE /api/snapshots/:id`           — delete one snapshot
//!   * `GET    /api/backingfiles/free-space` — total/used/avail in KB
//!   * `GET    /api/snapshots/live`          — the Live row: when the car
//!     last wrote to its drive and when the next scheduled snapshot is due
//!
//! Deletion uses `/root/bin/release_snapshot.sh` when available so it shares
//! the runtime's unmount and symlink-cleanup behavior.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;

use crate::router::AppState;

const SNAPSHOTS_DIR: &str = "/backingfiles/snapshots";
const RELEASE_SNAPSHOT_SCRIPT: &str = "/root/bin/release_snapshot.sh";
/// The live image retains shared extents independently of snapshots.
const CAM_DISK: &str = "/backingfiles/cam_disk.bin";

/// One snapshot entry in the listing response.
#[derive(serde::Serialize)]
struct SnapshotEntry {
    /// `snap-<id>` directory name. Used as the path parameter for delete.
    id: String,
    /// Estimated bytes freed by deleting THIS snapshot **and every older
    /// one**, not this snapshot alone.
    ///
    /// Reflinked blocks are freed only when their final holder is deleted, so
    /// reclaim is meaningful for an oldest-first prefix rather than one file.
    /// The final row should equal the independently derived total footprint.
    ///
    /// `None` means "not measured yet, or could not be measured" — render
    /// as pending/unavailable, NEVER as `0 B`. A measured zero is
    /// legitimate (a run that frees nothing) and does render as `0 B`.
    cumulative_reclaim_bytes: Option<u64>,
    /// Number of older snapshots in the reclaim prefix.
    older_count: usize,
    /// Unix epoch seconds from `snap.bin` mtime.
    created_unix: i64,
    /// How much of this snapshot's footage is verified (SHA-256) on the
    /// archive server. `None` when verification is off, there is no archive
    /// server, or the snapshot has no TOC yet.
    archive: Option<crate::archive_verify::Coverage>,
}

/// How long a measurement stays usable before we re-measure.
const SIZE_MAX_AGE_SECS: u64 = 15 * 60;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn size_cache() -> &'static std::sync::Mutex<sentryusb_gadget::reflink::SizeCache> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<sentryusb_gadget::reflink::SizeCache>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(sentryusb_gadget::reflink::SizeCache::new()))
}

static REFRESH_IN_FLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Clears the in-flight flag even if measurement panics.
struct RefreshGuard;
impl Drop for RefreshGuard {
    fn drop(&mut self) {
        REFRESH_IN_FLIGHT.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Measure every snapshot off the request path.
///
/// Extent-map walks run off the request path, one refresh at a time.
/// `ids` must be OLDEST-FIRST: the cumulative figure for a snapshot is
/// "delete this and everything older", so the prefix order is the meaning.
fn spawn_size_refresh(ids: Vec<String>, generation: u64) {
    use std::sync::atomic::Ordering;
    if REFRESH_IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return;
    }
    tokio::task::spawn_blocking(move || {
        let _guard = RefreshGuard;

        // One missing extent map invalidates every later cumulative value.
        let mut maps: Vec<Vec<sentryusb_gadget::reflink::PhysicalRange>> =
            Vec::with_capacity(ids.len());
        let mut idents = Vec::with_capacity(ids.len());
        let mut failed: Option<(String, String)> = None;
        for id in &ids {
            let bin = format!("{}/{}/snap.bin", SNAPSHOTS_DIR, id);
            let p = std::path::Path::new(&bin);
            match sentryusb_gadget::reflink::extent_map(p)
                .and_then(|m| Ok((m, sentryusb_gadget::reflink::file_identity(p)?)))
            {
                Ok((m, ident)) => {
                    maps.push(m);
                    idents.push(ident);
                }
                Err(e) => {
                    failed = Some((id.clone(), e.to_string()));
                    break;
                }
            }
        }

        // Extents retained by the live image are not reclaimable.
        let external = if failed.is_none() {
            match sentryusb_gadget::reflink::extent_map(std::path::Path::new(CAM_DISK)) {
                Ok(m) => Some(m),
                Err(e) => {
                    failed = Some((CAM_DISK.to_string(), e.to_string()));
                    None
                }
            }
        } else {
            None
        };

        let entries = match (failed, external) {
            (None, Some(external)) => {
                let curve = sentryusb_gadget::reflink::cumulative_reclaim(&maps, &external);
                ids.iter()
                    .zip(curve)
                    .zip(idents)
                    .map(|((id, bytes), ident)| (id.clone(), (bytes, ident)))
                    .collect()
            }
            (failed, _) => {
                if let Some((id, e)) = failed {
                    tracing::warn!("cumulative snapshot sizing failed at {}: {}", id, e);
                }
                Vec::new()
            }
        };

        if let Ok(mut cache) = size_cache().lock() {
            // Do not republish values invalidated by a concurrent deletion.
            if !cache.publish(generation, ids, entries, now_secs()) {
                tracing::debug!("discarded a snapshot-size measurement invalidated mid-flight");
            }
        }
    });
}

/// GET /api/snapshots
///
/// Returns the list of snapshot directories under `/backingfiles/snapshots/`.
/// Sorted oldest-first so callers can default to that ordering — the
/// user typically wants to delete the oldest to free space.
pub async fn list_snapshots(
    State(_s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut entries: Vec<SnapshotEntry> = Vec::new();

    let dir = match std::fs::read_dir(SNAPSHOTS_DIR) {
        Ok(d) => d,
        Err(_) => {
            return (StatusCode::OK, Json(serde_json::json!({
                "snapshots": entries,
            })));
        }
    };

    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("snap-") {
            continue;
        }
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        // Use `snap.bin` mtime because autofs metadata updates the directory
        // mtime when an old snapshot is viewed.
        let created_unix = std::fs::symlink_metadata(path.join("snap.bin"))
            .or_else(|_| entry.metadata())
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        entries.push(SnapshotEntry {
            id: name,
            // Filled from the extent-map cache below.
            cumulative_reclaim_bytes: None,
            older_count: 0,
            created_unix,
            archive: None,
        });
    }

    // Cumulative reclaim values are defined against oldest-first order.
    entries.sort_by_key(|e| e.created_unix);
    for (i, e) in entries.iter_mut().enumerate() {
        e.older_count = i;
    }

    // Archive verification status. Auto-delete never releases the newest
    // snapshot (by slot or by time), so never mark that one as ready.
    let ids_for_coverage: Vec<String> = entries.iter().map(|e| e.id.clone()).collect();
    if let Ok(Some(mut coverage)) = tokio::task::spawn_blocking(move || {
        crate::archive_verify::coverage_for(std::path::Path::new(SNAPSHOTS_DIR), &ids_for_coverage)
    })
    .await
    {
        let newest_slot = entries.iter().map(|e| e.id.clone()).max();
        let newest_time = entries.last().map(|e| e.id.clone());
        for id in [newest_slot, newest_time].into_iter().flatten() {
            if let Some(c) = coverage.get_mut(&id) {
                c.ready_to_delete = false;
            }
        }
        for e in entries.iter_mut() {
            e.archive = coverage.remove(&e.id);
        }
    }

    // A stale value may describe a different snapshot set, so omit it and refresh.
    let now = now_secs();
    let ids: Vec<String> = entries.iter().map(|e| e.id.clone()).collect();
    let (current, computed_at, generation) = match size_cache().lock() {
        Ok(cache) => {
            let current = cache.is_current_for(&ids, now, SIZE_MAX_AGE_SECS);
            if current {
                for e in entries.iter_mut() {
                    // Verify the file is still the one that was measured.
                    let bin = format!("{}/{}/snap.bin", SNAPSHOTS_DIR, e.id);
                    e.cumulative_reclaim_bytes =
                        sentryusb_gadget::reflink::file_identity(std::path::Path::new(&bin))
                            .ok()
                            .and_then(|ident| cache.get_if_same(&e.id, ident));
                }
            }
            (current, cache.computed_at(), cache.generation())
        }
        Err(_) => (false, None, 0),
    };
    if !current && !ids.is_empty() {
        spawn_size_refresh(ids, generation);
    }
    // Derive pending state from the cache to avoid racing worker publication.
    // Failed measurements still count as completed attempts.
    let pending = !current && !entries.is_empty();

    // `du` double-counts reflinked files. Derive the aggregate exclusive
    // footprint as filesystem-used bytes minus non-snapshot file usage.
    let total_allocated_bytes: u64 = if entries.is_empty() {
        0
    } else {
        let df_out = sentryusb_shell::run(
            "df", &["--output=used", "--block-size=1", "/backingfiles/"],
        ).await.unwrap_or_default();
        let used_bytes: u64 = df_out
            .lines()
            .last()
            .and_then(|l| l.split_whitespace().next())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        let non_snap_out = sentryusb_shell::run(
            "du", &["-sB1", "--exclude=snapshots", "/backingfiles/"],
        ).await.unwrap_or_default();
        let non_snap_bytes: u64 = non_snap_out
            .split_whitespace()
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        used_bytes.saturating_sub(non_snap_bytes)
    };

    // Cross-check FIEMAP attribution against the independent df-minus-du total.
    if let Some(last) = entries.last().and_then(|e| e.cumulative_reclaim_bytes) {
        let hi = total_allocated_bytes.max(last);
        let lo = total_allocated_bytes.min(last);
        // Allow metadata, journal, and in-flight-write noise.
        if hi > 0 && (hi - lo) * 10 > hi * 3 {
            tracing::warn!(
                "snapshot accounting disagreement: cumulative curve ends at {} bytes but \
                 df-based total is {} — one of these is wrong",
                last,
                total_allocated_bytes
            );
        }
    }

    let verification = tokio::task::spawn_blocking(|| {
        verify_progress(
            std::path::Path::new(VERIFY_PROGRESS_FILE),
            std::path::Path::new(crate::archive_verify::QUEUE),
            now_secs() as i64,
        )
    })
    .await
    .ok();

    (StatusCode::OK, Json(serde_json::json!({
        "snapshots": entries,
        "verification": verification,
        "total_allocated_bytes": total_allocated_bytes,
        // Distinguish an active measurement from unavailable data.
        "sizes_computed_at": computed_at,
        "sizes_pending": pending,
    })))
}

/// DELETE /api/snapshots/:id
///
/// Calls `release_snapshot.sh` to umount the snap.bin loop image and
/// remove the directory + dangling /mutable/TeslaCam symlinks. The
/// id must be a `snap-*` name; reject anything else to prevent
/// arbitrary path traversal.
pub async fn delete_snapshot(
    State(_s): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    if !id.starts_with("snap-") || id.contains('/') || id.contains("..") {
        return crate::json_error(
            StatusCode::BAD_REQUEST,
            "Invalid snapshot id (expected snap-<digits>)",
        );
    }

    let path = format!("{}/{}", SNAPSHOTS_DIR, id);
    if !std::path::Path::new(&path).is_dir() {
        return crate::json_error(StatusCode::NOT_FOUND, "Snapshot not found");
    }

    // Run teardown out of process so its blocking lock wait cannot exhaust API
    // workers. Pass the validated bare ID expected by the installed script;
    // partially installed systems fall back to removing the snapshot directory.
    let script_exists = std::path::Path::new(RELEASE_SNAPSHOT_SCRIPT).exists();
    let result = if script_exists {
        sentryusb_shell::run(RELEASE_SNAPSHOT_SCRIPT, &[id.as_str()]).await
    } else {
        sentryusb_shell::run("rm", &["-rf", &path]).await
    };

    match result {
        Ok(_) => {
            // Deleting one holder changes every cumulative reclaim value.
            if let Ok(mut cache) = size_cache().lock() {
                cache.invalidate();
            }
            (StatusCode::OK, Json(serde_json::json!({"deleted": id})))
        }
        Err(e) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Failed to delete snapshot: {}", e),
        ),
    }
}

/// Written by archiveloop during a verification pass.
const VERIFY_PROGRESS_FILE: &str = "/tmp/archive-verify-progress.json";

/// The archive check's progress, for the Snapshots page.
#[derive(serde::Serialize, Debug, PartialEq, Eq)]
pub struct VerifyProgress {
    /// A pass is running now (its progress was updated in the last 15 min).
    running: bool,
    /// Clips checked so far in the current (or last) pass, of `total`.
    checked: u64,
    total: u64,
    /// Why the last pass stopped early, if it did.
    stopped: Option<String>,
    /// Clips waiting in the queue overall (including ones waiting for Sentry
    /// Manager's checksum), not just this pass.
    queued: u64,
}

fn verify_progress(progress_file: &std::path::Path, queue_file: &std::path::Path, now: i64) -> VerifyProgress {
    let v: serde_json::Value = std::fs::read_to_string(progress_file)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(serde_json::Value::Null);
    let updated = v.get("updated_unix").and_then(|x| x.as_i64()).unwrap_or(0);
    // A "running" pass that hasn't reported for 15 minutes died with the
    // loop (or the Pi restarted): don't claim it's still running.
    let running = v.get("running").and_then(|x| x.as_bool()).unwrap_or(false) && now - updated < 900;
    let queued = std::fs::read_to_string(queue_file)
        .map(|t| t.lines().filter(|l| !l.trim().is_empty()).count() as u64)
        .unwrap_or(0);
    VerifyProgress {
        running,
        checked: v.get("checked").and_then(|x| x.as_u64()).unwrap_or(0),
        total: v.get("total").and_then(|x| x.as_u64()).unwrap_or(0),
        stopped: v.get("stopped").and_then(|x| x.as_str()).map(String::from),
        queued,
    }
}

/// Written by archiveloop's snapshotloop: unix time the next scheduled
/// snapshot is due.
const NEXT_SNAPSHOT_FILE: &str = "/tmp/sentryusb-next-snapshot";

#[derive(serde::Serialize, Debug, PartialEq, Eq)]
pub struct LiveStatus {
    /// When the car last wrote to its drive (`cam_disk.bin` mtime), unix
    /// seconds; null if unknown. Recent means the car is recording now.
    car_last_write_ts: Option<i64>,
    /// When the next scheduled snapshot is due; null if unknown or stale
    /// (the snapshot loop isn't running, or its time is long past).
    next_scheduled_ts: Option<i64>,
    /// The Pi's clock, so ages don't depend on the browser's clock.
    server_ts: i64,
}

fn live_status(cam_disk: &std::path::Path, next_file: &std::path::Path, now: i64) -> LiveStatus {
    let car_last_write_ts = std::fs::metadata(cam_disk)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64);
    // A due time more than 10 minutes past means the loop isn't keeping it
    // current (stopped, or the clock was stepped); don't show it.
    let next_scheduled_ts = std::fs::read_to_string(next_file)
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|&ts| ts > now - 600);
    LiveStatus { car_last_write_ts, next_scheduled_ts, server_ts: now }
}

/// GET /api/snapshots/live
pub async fn get_live() -> (StatusCode, Json<LiveStatus>) {
    let now = now_secs() as i64;
    let status = tokio::task::spawn_blocking(move || {
        live_status(std::path::Path::new(CAM_DISK), std::path::Path::new(NEXT_SNAPSHOT_FILE), now)
    })
    .await
    .unwrap_or(LiveStatus { car_last_write_ts: None, next_scheduled_ts: None, server_ts: now });
    (StatusCode::OK, Json(status))
}

/// GET /api/backingfiles/free-space
///
/// Returns total/used/available bytes for the backingfiles partition.
/// Used by the snapshot management UI's space gauge and by the wizard
/// pre-flight to render context alongside any size-rejection error.
pub async fn get_free_space(
    State(_s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let health = tokio::task::spawn_blocking(crate::status::managed_storage_health).await;
    match health {
        Ok(health) => (StatusCode::OK, Json(serde_json::json!({
            "total_bytes": health.total_bytes,
            "used_bytes": health.total_bytes.saturating_sub(health.free_bytes),
            "available_bytes": health.free_bytes,
            "mounted": health.total_bytes > 0,
            "storage_health": health,
        }))),
        Err(_) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, "Storage status unavailable"),
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    #[test]
    fn live_status_reads_the_car_write_time_and_a_current_schedule() {
        let d = tempfile::tempdir().unwrap();
        let cam = d.path().join("cam_disk.bin");
        let next = d.path().join("next");
        std::fs::write(&cam, b"x").unwrap();
        let written = std::fs::metadata(&cam).unwrap().modified().unwrap()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        let now = written + 30;
        std::fs::write(&next, format!("{}\n", now + 1200)).unwrap();
        assert_eq!(live_status(&cam, &next, now), LiveStatus {
            car_last_write_ts: Some(written), next_scheduled_ts: Some(now + 1200), server_ts: now,
        });
        // Long past: the loop isn't keeping it current, so it's hidden.
        std::fs::write(&next, format!("{}", now - 3600)).unwrap();
        assert_eq!(live_status(&cam, &next, now).next_scheduled_ts, None);
        // A little past (snapshot running now) still shows.
        std::fs::write(&next, format!("{}", now - 60)).unwrap();
        assert_eq!(live_status(&cam, &next, now).next_scheduled_ts, Some(now - 60));
        // Missing files: nulls, never an error.
        let gone = d.path().join("missing");
        assert_eq!(live_status(&gone, &gone, now), LiveStatus {
            car_last_write_ts: None, next_scheduled_ts: None, server_ts: now,
        });
    }

    #[test]
    fn verify_progress_reports_a_running_pass_and_the_queue() {
        let d = tempfile::tempdir().unwrap();
        let prog = d.path().join("p.json");
        let queue = d.path().join("q");
        std::fs::write(&queue, "a\tx\nb\tx\n\nc\tx\n").unwrap();
        std::fs::write(&prog, r#"{"running": true, "checked": 150, "total": 400, "stopped": null, "updated_unix": 1000}"#).unwrap();
        assert_eq!(verify_progress(&prog, &queue, 1060), VerifyProgress {
            running: true, checked: 150, total: 400, stopped: None, queued: 3,
        });
        // No update for 15 minutes: not running any more.
        assert!(!verify_progress(&prog, &queue, 1000 + 900).running);
        std::fs::write(&prog, r#"{"running": false, "checked": 50, "total": 400, "stopped": "archive server unreachable", "updated_unix": 1000}"#).unwrap();
        let p = verify_progress(&prog, &queue, 1010);
        assert_eq!((p.running, p.stopped.as_deref()), (false, Some("archive server unreachable")));
        // Nothing written yet: idle, empty.
        let gone = d.path().join("missing");
        assert_eq!(verify_progress(&gone, &gone, 1000), VerifyProgress {
            running: false, checked: 0, total: 0, stopped: None, queued: 0,
        });
    }
}
