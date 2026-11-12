//! Report a larger cam drive to the car than the partition inside it.
//!
//! `CAM_REPORTED_SIZE` (for example `2T`) grows `cam_disk.bin` to that size
//! as a SPARSE file just before the drive is presented to the car. The
//! partition table and the exFAT/FAT partition inside it are never touched:
//! the car keeps recording into the same `CAM_SIZE` partition (and keeps
//! recycling its own oldest clips there), snapshots copy it as before, and
//! the extra space is a hole that costs nothing on /backingfiles. Only the
//! drive's capacity, as the car reads it over USB, changes.
//!
//! Why: Tesla software 2026.14+ only offers its 24-hour Dashcam/Sentry
//! buffer on a drive of 1 TB or more, and a car with a smaller drive keeps
//! no continuous footage while parked in Sentry Mode.
//!
//! Safety rules:
//! * Nothing happens without a readable MBR partition table. A GPT disk, or a
//!   drive the car repartitioned, is left exactly as it is.
//! * The file never shrinks into a partition, and it shrinks (the setting was
//!   removed or lowered) only while the gadget is not bound to the car.
//! * Without the setting, a normal install is never touched: only a file left
//!   larger than `CAM_SIZE` by an earlier `CAM_REPORTED_SIZE` is shrunk back.

use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};

/// The config key.
pub const KEY: &str = "CAM_REPORTED_SIZE";

/// Largest size accepted (16 TiB): far beyond what any car checks for.
const MAX_BYTES: u64 = 16 << 40;

/// Slack below which a size difference is left alone (sfdisk alignment).
const SLACK_BYTES: u64 = 64 << 20;

/// Parse "2T", "2TB", "1.5T", "40G", "500M", "64K" or plain bytes. `None` for
/// empty, zero, unparsable or out-of-range values.
pub fn parse_size_bytes(raw: &str) -> Option<u64> {
    let s = raw.trim().trim_matches(|c| c == '\'' || c == '"').to_ascii_uppercase();
    let s = s.strip_suffix("IB").or_else(|| s.strip_suffix('B')).unwrap_or(&s);
    let (num, mult): (&str, u64) = match s.chars().last()? {
        'T' => (&s[..s.len() - 1], 1 << 40),
        'G' => (&s[..s.len() - 1], 1 << 30),
        'M' => (&s[..s.len() - 1], 1 << 20),
        'K' => (&s[..s.len() - 1], 1 << 10),
        c if c.is_ascii_digit() => (s, 1),
        _ => return None,
    };
    let n: f64 = num.trim().parse().ok()?;
    if !n.is_finite() || n <= 0.0 {
        return None;
    }
    let bytes = n * mult as f64;
    (bytes >= 1.0 && bytes <= MAX_BYTES as f64).then_some(bytes as u64)
}

/// End (in bytes) of the last partition in an MBR partition table, or `None`
/// when the image has no MBR, has no partitions, or is GPT (protective 0xEE
/// entry), so callers leave it alone.
pub fn partition_end(image: &Path) -> Result<Option<u64>> {
    let mut mbr = [0u8; 512];
    let mut f = std::fs::File::open(image).with_context(|| format!("open {}", image.display()))?;
    if f.read_exact(&mut mbr).is_err() {
        return Ok(None);
    }
    Ok(mbr_partition_end(&mbr))
}

fn mbr_partition_end(mbr: &[u8; 512]) -> Option<u64> {
    if mbr[510] != 0x55 || mbr[511] != 0xAA {
        return None;
    }
    let mut end = 0u64;
    for i in 0..4 {
        let e = &mbr[446 + i * 16..446 + (i + 1) * 16];
        let kind = e[4];
        if kind == 0 {
            continue;
        }
        if kind == 0xEE {
            return None; // GPT: never touch
        }
        let start = u64::from(u32::from_le_bytes([e[8], e[9], e[10], e[11]]));
        let sectors = u64::from(u32::from_le_bytes([e[12], e[13], e[14], e[15]]));
        end = end.max((start + sectors) * 512);
    }
    (end > 0).then_some(end)
}

/// What to do with the image file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    Keep,
    Grow(u64),
    Shrink(u64),
}

/// Decide the image's size. `reported` is `CAM_REPORTED_SIZE`, `cam_size` is
/// `CAM_SIZE` (both in bytes, `None` when unset or unparsable).
pub fn plan(file_len: u64, part_end: Option<u64>, cam_size: Option<u64>, reported: Option<u64>) -> Plan {
    let Some(end) = part_end else { return Plan::Keep };
    let target = match reported {
        Some(r) => r.max(end),
        // Off: only undo an earlier grow, back to CAM_SIZE.
        None => match cam_size {
            Some(cs) if file_len > cs.saturating_add(SLACK_BYTES) => cs.max(end),
            _ => return Plan::Keep,
        },
    };
    if target > file_len {
        Plan::Grow(target)
    } else if file_len - target > SLACK_BYTES {
        Plan::Shrink(target)
    } else {
        Plan::Keep
    }
}

/// Apply `CAM_REPORTED_SIZE` to `image`. `bound` says whether the gadget is
/// currently bound to the car (a shrink then waits). Returns the plan carried
/// out (`Keep` when nothing changed).
pub fn apply(image: &Path, cam_size: Option<&str>, reported: Option<&str>, bound: bool) -> Result<Plan> {
    let meta = match std::fs::metadata(image) {
        Ok(m) if m.is_file() => m,
        _ => return Ok(Plan::Keep),
    };
    let reported_bytes = match reported.map(str::trim).filter(|s| !s.is_empty()) {
        Some(r) => match parse_size_bytes(r) {
            Some(b) => Some(b),
            None => {
                tracing::warn!("{KEY}={r:?} is not a size like 2T; leaving the cam drive as it is");
                return Ok(Plan::Keep);
            }
        },
        None => None,
    };
    let p = plan(meta.len(), partition_end(image)?, cam_size.and_then(parse_size_bytes), reported_bytes);
    match p {
        Plan::Keep => {}
        Plan::Shrink(_) if bound => {
            tracing::info!("cam drive is attached to the car; its reported size goes back on the next reconnect");
            return Ok(Plan::Keep);
        }
        Plan::Grow(n) | Plan::Shrink(n) => {
            let f = std::fs::OpenOptions::new().write(true).open(image)
                .with_context(|| format!("open {} for resize", image.display()))?;
            f.set_len(n).with_context(|| format!("resize {} to {n} bytes", image.display()))?;
            tracing::info!("cam drive reports {} to the car ({:?}); the partition inside is unchanged", crate::format_size(n), p);
        }
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: u64 = 1 << 30;
    const T: u64 = 1 << 40;

    /// An MBR with one partition from sector 2048 to `end_bytes`.
    fn mbr(end_bytes: u64, kind: u8) -> [u8; 512] {
        let mut m = [0u8; 512];
        m[510] = 0x55;
        m[511] = 0xAA;
        let start = 2048u32;
        let sectors = (end_bytes / 512) as u32 - start;
        m[446 + 4] = kind;
        m[446 + 8..446 + 12].copy_from_slice(&start.to_le_bytes());
        m[446 + 12..446 + 16].copy_from_slice(&sectors.to_le_bytes());
        m
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size_bytes("2T"), Some(2 * T));
        assert_eq!(parse_size_bytes("2TB"), Some(2 * T));
        assert_eq!(parse_size_bytes("'2T'"), Some(2 * T));
        assert_eq!(parse_size_bytes("1.5t"), Some(T + T / 2));
        assert_eq!(parse_size_bytes("40G"), Some(40 * G));
        assert_eq!(parse_size_bytes("40GiB"), Some(40 * G));
        assert_eq!(parse_size_bytes("1024"), Some(1024));
        for bad in ["", "0", "0T", "-1T", "abc", "T", "17T", "2X"] {
            assert_eq!(parse_size_bytes(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn reads_the_partition_end_and_refuses_gpt_or_no_table() {
        assert_eq!(mbr_partition_end(&mbr(40 * G, 0x07)), Some(40 * G));
        assert_eq!(mbr_partition_end(&mbr(40 * G, 0xEE)), None, "GPT");
        assert_eq!(mbr_partition_end(&[0u8; 512]), None, "no table");
        let mut empty = [0u8; 512];
        empty[510] = 0x55;
        empty[511] = 0xAA;
        assert_eq!(mbr_partition_end(&empty), None, "no partitions");
    }

    #[test]
    fn plans() {
        let end = Some(40 * G);
        // spoof on: grow once, then keep
        assert_eq!(plan(40 * G, end, Some(40 * G), Some(2 * T)), Plan::Grow(2 * T));
        assert_eq!(plan(2 * T, end, Some(40 * G), Some(2 * T)), Plan::Keep);
        // lowered: shrink, but never into the partition
        assert_eq!(plan(2 * T, end, Some(40 * G), Some(T)), Plan::Shrink(T));
        assert_eq!(plan(2 * T, end, Some(40 * G), Some(10 * G)), Plan::Shrink(40 * G));
        // off: undo an earlier grow; a normal install is never touched
        assert_eq!(plan(2 * T, end, Some(40 * G), None), Plan::Shrink(40 * G));
        assert_eq!(plan(40 * G, end, Some(40 * G), None), Plan::Keep);
        assert_eq!(plan(40 * G + 1024 * 1024, end, Some(40 * G), None), Plan::Keep, "alignment slack");
        assert_eq!(plan(2 * T, end, None, None), Plan::Keep, "no CAM_SIZE to go back to");
        // the car repartitioned the whole 2 TB: its partition is never cut
        assert_eq!(plan(2 * T, Some(2 * T), Some(40 * G), None), Plan::Keep);
        // unknown layout: never touched
        assert_eq!(plan(40 * G, None, Some(40 * G), Some(2 * T)), Plan::Keep);
    }

    #[test]
    fn grows_sparse_keeps_the_partition_and_shrinks_only_when_unbound() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("cam_disk.bin");
        let small = 64u64 << 20; // a 64 MiB "CAM_SIZE" drive
        std::fs::write(&img, mbr(small, 0x07)).unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(&img).unwrap();
        f.set_len(small).unwrap();
        drop(f);
        let before = std::fs::read(&img).unwrap()[..512].to_vec();

        assert_eq!(apply(&img, Some("64M"), Some("2G"), true).unwrap(), Plan::Grow(2 * G));
        let m = std::fs::metadata(&img).unwrap();
        assert_eq!(m.len(), 2 * G);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert!(m.blocks() * 512 < 64 << 20, "the grown space is a hole");
        }
        assert_eq!(std::fs::read(&img).unwrap()[..512], before[..], "partition table untouched");
        assert_eq!(apply(&img, Some("64M"), Some("2G"), true).unwrap(), Plan::Keep);

        // setting removed: no shrink while the car has the drive, then back to CAM_SIZE
        assert_eq!(apply(&img, Some("64M"), None, true).unwrap(), Plan::Keep);
        assert_eq!(std::fs::metadata(&img).unwrap().len(), 2 * G);
        assert_eq!(apply(&img, Some("64M"), None, false).unwrap(), Plan::Shrink(small));
        assert_eq!(std::fs::metadata(&img).unwrap().len(), small);

        // a bad value changes nothing; a missing image is fine
        assert_eq!(apply(&img, Some("64M"), Some("lots"), false).unwrap(), Plan::Keep);
        assert_eq!(apply(&dir.path().join("none.bin"), Some("64M"), Some("2G"), false).unwrap(), Plan::Keep);
    }
}
