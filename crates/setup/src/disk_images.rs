//! Disk image creation — replaces `create-backingfiles.sh`.
//!
//! Creates FAT32/exFAT disk images for cam, music, lightshow, and boombox
//! drives in /backingfiles/. Wraps & License Plates live as folders on the
//! cam drive — no dedicated partition.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::env::SetupEnv;
use crate::SetupEmitter;

const BACKINGFILES: &str = "/backingfiles";

/// Disk image spec.
struct DriveSpec {
    name: &'static str,
    label: &'static str,
    config_key: &'static str,
    default_fallback: &'static str,
}

const DRIVE_SPECS: &[DriveSpec] = &[
    DriveSpec { name: "cam", label: "CAM", config_key: "CAM_SIZE", default_fallback: "30G" },
    DriveSpec { name: "music", label: "MUSIC", config_key: "MUSIC_SIZE", default_fallback: "4G" },
    DriveSpec { name: "lightshow", label: "LIGHTSHOW", config_key: "LIGHTSHOW_SIZE", default_fallback: "1G" },
    DriveSpec { name: "boombox", label: "BOOMBOX", config_key: "BOOMBOX_SIZE", default_fallback: "100M" },
];

/// One-time cleanup for installs that previously had a dedicated wraps disk.
/// The 4 GB image is no longer used — Wraps & LicensePlate are now folders
/// on the cam drive. Reclaim the space on the next setup re-run.
fn purge_legacy_wraps_disk() {
    let _ = std::fs::remove_file(format!("{}/wraps_disk.bin", BACKINGFILES));
    let _ = std::fs::remove_file(format!("{}/wraps_disk.bin.opts", BACKINGFILES));
    let _ = std::fs::remove_dir("/mnt/wraps");
}

/// Parse a human-readable size like "30G", "4G", "100M" into KB.
pub fn dehumanize(s: &str) -> Result<u64> {
    let s = s.trim().to_uppercase()
        .replace("GB", "G")
        .replace("MB", "M")
        .replace("KB", "K");

    if s == "0" || s.is_empty() {
        return Ok(0);
    }

    if s.ends_with('G') {
        let n: f64 = s.trim_end_matches('G').parse()?;
        Ok((n * 1024.0 * 1024.0) as u64) // KB
    } else if s.ends_with('M') {
        let n: f64 = s.trim_end_matches('M').parse()?;
        Ok((n * 1024.0) as u64)
    } else if s.ends_with('K') {
        let n: f64 = s.trim_end_matches('K').parse()?;
        Ok(n as u64)
    } else {
        // Assume bytes
        let n: u64 = s.parse()?;
        Ok(n / 1024)
    }
}

/// Get available space in KB on /backingfiles, minus a safety margin.
async fn available_space_kb() -> Result<u64> {
    let output = sentryusb_shell::run(
        "df", &["--output=size", "--block-size=1K", &format!("{}/", BACKINGFILES)],
    ).await?;
    let total: u64 = output.lines().last().unwrap_or("0").trim().parse().unwrap_or(0);

    // Reserve 10% capped between 2GB and 10GB
    let ten_pct = total / 10;
    let min_pad = 2 * 1024 * 1024; // 2GB in KB
    let max_pad = 10 * 1024 * 1024; // 10GB in KB
    let padding = ten_pct.max(min_pad).min(max_pad);
    Ok(total.saturating_sub(padding))
}

/// Check if an existing image file matches the requested size (within 10MB).
fn image_matches(file: &str, requested_kb: u64) -> bool {
    if requested_kb == 0 {
        return !Path::new(file).exists();
    }
    let Ok(meta) = std::fs::metadata(file) else { return false };
    let within = |kb: u64| (kb as i64 - requested_kb as i64).unsigned_abs() < 10240;
    if within(meta.len() / 1024) {
        return true;
    }
    // A cam image grown by CAM_REPORTED_SIZE is larger than its partition: it
    // still matches when the partition inside is the requested size, so a setup
    // re-run never wipes the footage over the reported size alone.
    meta.len() / 1024 > requested_kb
        && sentryusb_gadget::reported_size::partition_end(Path::new(file))
            .ok()
            .flatten()
            .is_some_and(|end| within(end / 1024))
}

/// `CAM_REPORTED_SIZE` in KB when it applies: set, a valid size larger than
/// `CAM_SIZE`, and an exFAT cam drive (the placeholder is exFAT-only).
fn cam_reported_kb(env: &SetupEnv, cam_kb: u64, use_exfat: bool, emitter: &SetupEmitter) -> Option<u64> {
    let raw = env.get(sentryusb_gadget::reported_size::KEY, "");
    if raw.trim().is_empty() || cam_kb == 0 {
        return None;
    }
    let Some(bytes) = sentryusb_gadget::reported_size::parse_size_bytes(&raw) else {
        emitter.progress(&format!("CAM_REPORTED_SIZE={raw} is not a size like 2T; ignoring it"));
        return None;
    };
    if !use_exfat {
        emitter.progress("CAM_REPORTED_SIZE needs an exFAT cam drive (USE_EXFAT=true); ignoring it");
        return None;
    }
    (bytes / 1024 > cam_kb).then_some(bytes / 1024)
}

/// Whether the cam image is already what the settings ask for. With
/// `CAM_REPORTED_SIZE`: a partition of the reported size holding the
/// placeholder file (so the car sees that size but only `CAM_SIZE` free).
fn cam_matches(file: &str, cam_kb: u64, reported_kb: Option<u64>) -> bool {
    let Some(reported_kb) = reported_kb else { return image_matches(file, cam_kb) };
    let path = Path::new(file);
    let Ok(Some((start, end))) = sentryusb_gadget::exfat_reserve::first_partition(path) else { return false };
    // the partition spans the image from 1 MiB, so compare its end to the size
    (end / 1024).abs_diff(reported_kb) < 10240
        && sentryusb_gadget::exfat_reserve::has_placeholder(path, start).unwrap_or(false)
}

/// Format the cam drive at the reported size and fill all but `CAM_SIZE` of
/// it with the placeholder. Any failure falls back to a plain `CAM_SIZE`
/// drive: a reported-size drive without the placeholder would let the car
/// write far more than `CAM_SIZE` to the storage.
async fn create_reported_cam(
    label: &str,
    cam_kb: u64,
    reported_kb: u64,
    use_exfat: bool,
    emitter: &SetupEmitter,
) -> Result<()> {
    let filename = format!("{}/cam_disk.bin", BACKINGFILES);
    let attempt: Result<u64> = async {
        create_drive("cam", label, reported_kb, use_exfat, emitter).await?;
        let path = Path::new(&filename);
        let (start, _) = sentryusb_gadget::exfat_reserve::first_partition(path)?
            .context("cam image has no partition table")?;
        sentryusb_gadget::exfat_reserve::add_placeholder(path, start, cam_kb * 1024)
    }
    .await;
    match attempt {
        Ok(reserved) => {
            emitter.progress(&format!(
                "Cam drive reports {} GB to the car; {} GB of it is a placeholder, so the car records into about {} GB",
                reported_kb / 1024 / 1024,
                reserved >> 30,
                cam_kb / 1024 / 1024,
            ));
            Ok(())
        }
        Err(e) => {
            emitter.progress(&format!("CAM_REPORTED_SIZE could not be set up ({e:#}); creating a normal cam drive"));
            create_drive("cam", label, cam_kb, use_exfat, emitter).await
        }
    }
}

/// Create a single drive image file with a partition table and filesystem.
async fn create_drive(
    name: &str,
    label: &str,
    size_kb: u64,
    use_exfat: bool,
    emitter: &SetupEmitter,
) -> Result<()> {
    let filename = format!("{}/{}_disk.bin", BACKINGFILES, name);
    let mountpoint = format!("/mnt/{}", name);

    if size_kb == 0 {
        let _ = std::fs::remove_file(&filename);
        let _ = std::fs::remove_file(format!("{}.opts", filename));
        let _ = std::fs::remove_dir(&mountpoint);
        return Ok(());
    }

    emitter.progress(&format!("Allocating {}K for {}...", size_kb, filename));
    let _ = std::fs::remove_file(&filename);
    sentryusb_shell::run("truncate", &["--size", &format!("{}K", size_kb), &filename]).await
        .context("truncate failed")?;

    // Create partition table
    let sfdisk_type = if use_exfat { "type=7" } else { "type=c" };
    sentryusb_shell::run(
        "bash", &["-c", &format!("echo '{}' | sfdisk '{}'", sfdisk_type, filename)],
    ).await.context("sfdisk failed on disk image")?;

    // Find partition offset
    let offset = get_partition_offset(&filename).await?;

    // Set up loop device
    let loopdev = sentryusb_shell::run(
        "losetup", &["-f", "--show", "-o", &offset.to_string(), &filename],
    ).await.context("losetup failed")?.trim().to_string();

    let _ = sentryusb_shell::run("udevadm", &["settle", "--timeout=5"]).await;

    // Format
    emitter.progress(&format!("Creating filesystem with label '{}'", label));
    let format_result = if use_exfat {
        sentryusb_shell::run("mkfs.exfat", &[&loopdev, "-L", label]).await
    } else {
        sentryusb_shell::run("mkfs.vfat", &[&loopdev, "-F", "32", "-n", label]).await
    };

    let _ = sentryusb_shell::run("losetup", &["-d", &loopdev]).await;
    format_result.context("filesystem creation failed")?;

    let _ = std::fs::create_dir_all(&mountpoint);
    emitter.progress(&format!("Drive image {} ready.", filename));
    Ok(())
}

/// Get the byte offset of the first partition in a disk image.
async fn get_partition_offset(filename: &str) -> Result<u64> {
    let bytes_out = sentryusb_shell::run(
        "bash", &["-c", &format!("sfdisk -l -o Size -q --bytes '{}' | tail -1", filename)],
    ).await?;
    let size_in_bytes: u64 = bytes_out.trim().parse().context("parse size")?;

    let sectors_out = sentryusb_shell::run(
        "bash", &["-c", &format!("sfdisk -l -o Sectors -q '{}' | tail -1", filename)],
    ).await?;
    let size_in_sectors: u64 = sectors_out.trim().parse().context("parse sectors")?;

    let sector_size = size_in_bytes / size_in_sectors;

    let start_out = sentryusb_shell::run(
        "bash", &["-c", &format!("sfdisk -l -o Start -q '{}' | tail -1", filename)],
    ).await?;
    let start_sector: u64 = start_out.trim().parse().context("parse start")?;

    Ok(start_sector * sector_size)
}

/// Release all loop devices and unmount all drive image mount points.
async fn release_all_images() {
    let _ = sentryusb_shell::run("bash", &["-c", "killall archiveloop 2>/dev/null"]).await;
    // Use the usb_gadget crate to disable
    let _ = sentryusb_gadget::disable();
    // /mnt/wraps stays in the list to drain any leftover mount from a
    // pre-migration install before purge_legacy_wraps_disk runs.
    for mount in &["/mnt/cam", "/mnt/music", "/mnt/lightshow", "/mnt/boombox", "/mnt/wraps"] {
        let _ = sentryusb_shell::run("umount", &["-d", mount]).await;
    }
    let _ = sentryusb_shell::run(
        "bash", &["-c", "umount -d /backingfiles/snapshots/snap*/mnt 2>/dev/null"],
    ).await;
}

/// Ensure exfat tools are available if needed.
async fn ensure_exfat_tools(use_exfat: bool, emitter: &SetupEmitter) -> Result<bool> {
    if !use_exfat {
        return Ok(false);
    }

    // Check kernel support
    let has_kernel = sentryusb_shell::run(
        "bash", &["-c", "grep -q exfat /proc/filesystems || modprobe -n exfat"],
    ).await.is_ok();

    if !has_kernel {
        // Surface to the wizard log — a silent fallback would let the
        // user think they got an exFAT cam disk when they actually
        // got FAT32 (and FAT32's 4 GB per-file cap silently truncates
        // long Tesla clips).
        emitter.progress("WARNING: kernel does not support ExFAT — falling back to FAT32");
        return Ok(false);
    }

    // Install exfatprogs if needed
    if sentryusb_shell::run("which", &["mkfs.exfat"]).await.is_err() {
        if crate::apt::apt_install(
            |m| emitter.progress(m),
            &["exfatprogs"],
            Duration::from_secs(600),
        ).await.is_err() {
            emitter.progress("WARNING: could not install exfatprogs — falling back to FAT32");
            return Ok(false);
        }
    }

    Ok(true)
}

/// Ensure dosfstools is available.
async fn ensure_vfat_tools(emitter: &SetupEmitter) -> Result<()> {
    if sentryusb_shell::run("which", &["mkfs.vfat"]).await.is_err() {
        crate::apt::apt_install(
            |m| emitter.progress(m),
            &["dosfstools"],
            Duration::from_secs(600),
        ).await.context("failed to install dosfstools")?;
    }
    Ok(())
}

/// Create all disk images based on config settings. Returns true if any work was performed.
pub async fn create_disk_images(env: &SetupEnv, emitter: &SetupEmitter) -> Result<bool> {
    let use_exfat_cfg = env.get_bool("USE_EXFAT", false);

    // Calculate requested sizes first (before any heavy work) so we can
    // short-circuit when everything already matches.
    let mut sizes: Vec<(String, String, u64, String)> = Vec::new();
    for spec in DRIVE_SPECS {
        let raw = env.get(spec.config_key, "0");
        let size_kb = if raw.contains('%') {
            dehumanize(spec.default_fallback)?
        } else {
            dehumanize(&raw)?
        };
        let filename = format!("{}/{}_disk.bin", BACKINGFILES, spec.name);
        sizes.push((spec.name.to_string(), spec.label.to_string(), size_kb, filename));
    }

    // Reclaim the 4 GB the dedicated wraps disk used to occupy. Runs before
    // the all-match early exit so a pre-migration install gets cleaned up
    // even when the user hasn't changed any sizes.
    let legacy_wraps_path = format!("{}/wraps_disk.bin", BACKINGFILES);
    let legacy_wraps = Path::new(&legacy_wraps_path).exists();
    if legacy_wraps {
        emitter.progress("Removing legacy wraps disk image — using cam drive folders now...");
        let _ = sentryusb_shell::run("umount", &["-d", "/mnt/wraps"]).await;
        purge_legacy_wraps_disk();
    }

    let cam_reported = cam_reported_kb(env, sizes[0].2, use_exfat_cfg, emitter);
    let matches = |i: usize| {
        let (_, _, sz, f) = &sizes[i];
        if i == 0 { cam_matches(f, *sz, cam_reported) } else { image_matches(f, *sz) }
    };
    let all_match = (0..sizes.len()).all(matches);
    if all_match && !legacy_wraps {
        return Ok(false);
    }

    emitter.begin_phase("disk_images", "Disk images");
    emitter.progress("Creating disk images...");

    let use_exfat = ensure_exfat_tools(use_exfat_cfg, emitter).await?;
    ensure_vfat_tools(emitter).await?;

    // Space check. teslausb auto-shrinks because it has no UI to ask
    // the user; we have a UI, so we reject explicitly with a clear
    // breakdown. The wizard pre-flight surfaces the same calculation
    // before submit (see verify::verify_disk_space). Never auto-delete
    // snapshots as a side effect of a settings change.
    let total_requested: u64 = sizes.iter().map(|(_, _, sz, _)| sz).sum();
    let available = available_space_kb().await.unwrap_or(0);
    if total_requested > available {
        let need_gb = (total_requested - available) / 1024 / 1024;
        let req_gb = total_requested / 1024 / 1024;
        let avail_gb = available / 1024 / 1024;
        bail!(
            "Disk images need {} GB but backingfiles has only {} GB free \
             (after safety reserve). Free at least {} GB by deleting \
             snapshots from the snapshot management page, then re-run setup.",
            req_gb, avail_gb, need_gb,
        );
    }

    // Release everything that might be using the images
    release_all_images().await;

    // Create/update each drive
    let cam_changed = !matches(0);
    for (i, (name, label, size_kb, _)) in sizes.iter().enumerate() {
        if matches(i) {
            continue;
        }
        emitter.progress(&format!("Recreating {} drive ({}K)...", name, size_kb));
        match (i, cam_reported) {
            (0, Some(reported_kb)) => create_reported_cam(label, *size_kb, reported_kb, use_exfat, emitter).await?,
            _ => create_drive(name, label, *size_kb, use_exfat, emitter).await?,
        }
    }

    // Clean up stale /mutable/TeslaCam symlinks when cam drive was
    // changed/removed — those symlinks point into the old cam_disk and
    // are dangling after the recreate. Snapshots are intentionally NOT
    // touched: they live independently on backingfiles and represent
    // the user's archived footage history. Wiping them on a CAM_SIZE
    // change is the same "I changed a setting, why did I lose data"
    // failure mode the partition wipe used to cause.
    if sizes[0].2 == 0 || cam_changed {
        if Path::new("/mutable/TeslaCam").is_dir() {
            for dir in &["RecentClips", "SavedClips", "SentryClips", "TeslaTrackMode"] {
                let _ = std::fs::remove_dir_all(format!("/mutable/TeslaCam/{}", dir));
            }
            let _ = std::fs::remove_file("/mutable/sentry_files_archived");
        }
    }

    emitter.progress("Disk image creation complete.");
    Ok(true)
}

#[cfg(test)]
mod reported_size_tests {
    use super::image_matches;

    /// A sparse image of `len` bytes whose MBR has one partition ending at `end`.
    fn image(dir: &std::path::Path, len: u64, end: u64) -> String {
        let mut mbr = [0u8; 512];
        mbr[510] = 0x55;
        mbr[511] = 0xAA;
        mbr[446 + 4] = 0x07;
        mbr[446 + 8..446 + 12].copy_from_slice(&2048u32.to_le_bytes());
        mbr[446 + 12..446 + 16].copy_from_slice(&((end / 512) as u32 - 2048).to_le_bytes());
        let path = dir.join("cam_disk.bin");
        std::fs::write(&path, mbr).unwrap();
        std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(len).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn a_cam_image_grown_for_cam_reported_size_is_not_recreated() {
        let dir = std::env::temp_dir().join(format!("sentryusb-reported-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let gib = 1u64 << 30;
        let requested_kb = 4 * gib / 1024; // CAM_SIZE=4G
        // as created: matches
        assert!(image_matches(&image(&dir, 4 * gib, 4 * gib), requested_kb));
        // grown to 2 TB around the same 4 GB partition: still matches (no wipe)
        assert!(image_matches(&image(&dir, 2u64 << 40, 4 * gib), requested_kb));
        // CAM_SIZE really changed (partition is 8 GB): recreated as before
        assert!(!image_matches(&image(&dir, 2u64 << 40, 8 * gib), requested_kb));
        assert!(!image_matches(&image(&dir, 8 * gib, 8 * gib), requested_kb));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod cam_reported_tests {
    use super::cam_matches;
    use std::io::{Read, Seek, SeekFrom, Write};

    /// An image of `size` bytes: MBR + an exFAT partition from 1 MiB.
    fn exfat_image(dir: &std::path::Path, size: u64) -> String {
        let img = dir.join("cam_disk.bin");
        let mut f = std::fs::File::create(&img).unwrap();
        f.set_len(size).unwrap();
        let mut mbr = [0u8; 512];
        mbr[510] = 0x55;
        mbr[511] = 0xAA;
        mbr[446 + 4] = 0x07;
        mbr[446 + 8..446 + 12].copy_from_slice(&2048u32.to_le_bytes());
        mbr[446 + 12..446 + 16].copy_from_slice(&((size / 512 - 2048) as u32).to_le_bytes());
        f.write_all(&mbr).unwrap();
        let part = dir.join("part.bin");
        std::fs::File::create(&part).unwrap().set_len(size - (1 << 20)).unwrap();
        assert!(std::process::Command::new("mkfs.exfat").arg(&part).output().unwrap().status.success());
        let mut data = Vec::new();
        std::fs::File::open(&part).unwrap().read_to_end(&mut data).unwrap();
        f.seek(SeekFrom::Start(1 << 20)).unwrap();
        f.write_all(&data).unwrap();
        std::fs::remove_file(&part).unwrap();
        img.to_string_lossy().into_owned()
    }

    #[test]
    fn cam_matches_needs_the_reported_partition_and_the_placeholder() {
        if !std::process::Command::new("which").arg("mkfs.exfat").output().is_ok_and(|o| o.status.success()) {
            eprintln!("skipped: exfatprogs not installed");
            return;
        }
        let dir = std::env::temp_dir().join(format!("sentryusb-camrep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mib = 1024u64; // in KB
        let (cam_kb, reported_kb) = (32 * mib, 128 * mib);

        let img = exfat_image(&dir, 128 << 20);
        // formatted at the reported size but no placeholder yet: not done
        assert!(!cam_matches(&img, cam_kb, Some(reported_kb)));
        let (start, _) = sentryusb_gadget::exfat_reserve::first_partition(std::path::Path::new(&img)).unwrap().unwrap();
        sentryusb_gadget::exfat_reserve::add_placeholder(std::path::Path::new(&img), start, cam_kb * 1024).unwrap();
        assert!(cam_matches(&img, cam_kb, Some(reported_kb)), "reported-size drive with its placeholder");
        // the setting removed: a reported-size drive is recreated at CAM_SIZE
        assert!(!cam_matches(&img, cam_kb, None));

        // a CAM_SIZE drive (or one only grown on the outside) is recreated
        // once CAM_REPORTED_SIZE is set
        let img = exfat_image(&dir, 32 << 20);
        assert!(cam_matches(&img, cam_kb, None));
        assert!(!cam_matches(&img, cam_kb, Some(reported_kb)));
        std::fs::OpenOptions::new().write(true).open(&img).unwrap().set_len(128 << 20).unwrap();
        assert!(cam_matches(&img, cam_kb, None), "grown on the outside still matches CAM_SIZE");
        assert!(!cam_matches(&img, cam_kb, Some(reported_kb)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
