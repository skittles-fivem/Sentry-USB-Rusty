//! Disk throughput for the dashboard's System tile, from `/proc/diskstats`.
//!
//! The device measured is the one holding `/backingfiles` (the XFS partition
//! with the recordings, snapshots and disk images): `mmcblk0p3` on an SD-card
//! install, `sda1`/`nvme0n1p1` on an SSD. It is resolved from
//! `/proc/self/mountinfo` by the mount's major:minor (so `/dev/root` or a
//! by-uuid source still matches) and cached for five minutes, since mounts
//! rarely change. If that partition has no full statistics line, the parent
//! disk is measured instead.
//!
//! Sampling follows `cpu_usage`: each status request compares against the
//! previous reading, requests under a second apart reuse the last figure, and
//! with no reading in the last 30 s the request takes two readings 250 ms
//! apart. Busy % is the share of wall time the device had I/O in flight
//! (`io_ticks`), capped at 100.

use serde::Serialize;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MIN_INTERVAL: Duration = Duration::from_secs(1);
const STALE_AFTER: Duration = Duration::from_secs(30);
const FRESH_WINDOW: Duration = Duration::from_millis(250);
const DEVICE_TTL: Duration = Duration::from_secs(300);
const MOUNT_POINT: &str = "/backingfiles";
/// `/proc/diskstats` always counts 512-byte sectors, whatever the device's
/// real sector size.
const SECTOR_BYTES: u64 = 512;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DiskIo {
    /// Kernel block device name, e.g. `mmcblk0p3`.
    pub device: String,
    pub read_bps: u64,
    pub write_bps: u64,
    /// Share of wall time with I/O in flight, 0–100, one decimal.
    pub busy_percent: f64,
    /// Bytes read since boot.
    pub read_total_bytes: u64,
    /// Bytes written since boot.
    pub write_total_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Counters {
    sectors_read: u64,
    sectors_written: u64,
    io_ticks_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Rates {
    read_bps: u64,
    write_bps: u64,
    busy_percent: f64,
}

struct Sample {
    device: String,
    counters: Counters,
    taken_at: Instant,
    rates: Option<Rates>,
}

static SAMPLE: Mutex<Option<Sample>> = Mutex::new(None);
static DEVICE: Mutex<Option<(String, Instant)>> = Mutex::new(None);

/// One parsed `/proc/diskstats` line.
#[derive(Debug, PartialEq, Eq)]
struct Line<'a> {
    major: u32,
    minor: u32,
    name: &'a str,
    counters: Counters,
}

/// Parses a line: `major minor name` then reads, reads merged, sectors read,
/// ms reading, writes, writes merged, sectors written, ms writing, in flight,
/// io_ticks, weighted ms, and (4.18+) discard and (5.5+) flush fields.
/// Lines with fewer than the ten classic counters (old kernels' short
/// partition lines) are rejected, so the caller falls back to the disk.
fn parse_line(line: &str) -> Option<Line<'_>> {
    let mut f = line.split_whitespace();
    let major = f.next()?.parse().ok()?;
    let minor = f.next()?.parse().ok()?;
    let name = f.next()?;
    let stats: Vec<u64> = f.take(10).map(|v| v.parse().ok()).collect::<Option<_>>()?;
    if stats.len() < 10 {
        return None;
    }
    Some(Line {
        major,
        minor,
        name,
        counters: Counters { sectors_read: stats[2], sectors_written: stats[6], io_ticks_ms: stats[9] },
    })
}

/// The `/backingfiles` mount from mountinfo: (major, minor, source basename).
/// The last matching line wins, as that is the mount on top.
fn backing_mount(mountinfo: &str) -> Option<(u32, u32, String)> {
    mountinfo
        .lines()
        .rev()
        .find_map(|line| {
            let mut f = line.split_whitespace();
            let dev = f.nth(2)?;
            let _root = f.next()?;
            if f.next()? != MOUNT_POINT {
                return None;
            }
            let (major, minor) = dev.split_once(':')?;
            // After the optional fields: "- fstype source superopts".
            let source = f.skip_while(|v| *v != "-").nth(2).unwrap_or("");
            let base = source.rsplit('/').next().unwrap_or("").to_string();
            Some((major.parse().ok()?, minor.parse().ok()?, base))
        })
}

/// Whole-disk name for a partition, from sysfs (a partition's directory sits
/// inside its disk's), falling back to [`parent_disk_name`].
fn parent_disk(name: &str) -> Option<String> {
    let dir = std::path::Path::new("/sys/class/block").join(name);
    if dir.join("partition").exists()
        && let Ok(real) = std::fs::canonicalize(&dir)
        && let Some(parent) = real.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str())
    {
        return Some(parent.to_string());
    }
    parent_disk_name(name).map(str::to_string)
}

/// Whole-disk name guessed from a partition name: `mmcblk0p3`/`nvme0n1p1` →
/// `mmcblk0`/`nvme0n1` (a `p` separates the number when the disk name ends in
/// a digit), `sda1`/`vdb2` → `sda`/`vdb`. Whole-disk names give `None`.
fn parent_disk_name(name: &str) -> Option<&str> {
    let stem = name.trim_end_matches(|c: char| c.is_ascii_digit());
    if stem.len() == name.len() {
        return None;
    }
    if let Some(disk) = stem.strip_suffix('p')
        && disk.ends_with(|c: char| c.is_ascii_digit())
    {
        return Some(disk);
    }
    let lettered = ["sd", "vd", "hd", "xvd"].iter().any(|p| stem.starts_with(p));
    (lettered && stem.chars().all(|c| c.is_ascii_lowercase())).then_some(stem)
}

/// The diskstats name to measure for the `/backingfiles` mount: the line with
/// the mount's major:minor, else one named like its source, else the parent disk.
fn resolve_device(
    mountinfo: &str,
    diskstats: &str,
    parent: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let (major, minor, source) = backing_mount(mountinfo)?;
    let lines: Vec<&str> = diskstats.lines().collect();
    let any_line = |want: &dyn Fn(&str, u32, u32) -> bool| {
        lines.iter().find_map(|l| {
            let mut f = l.split_whitespace();
            let (ma, mi, name) = (f.next()?.parse().ok()?, f.next()?.parse().ok()?, f.next()?);
            want(name, ma, mi).then(|| name.to_string())
        })
    };
    let full = |name: &str| lines.iter().filter_map(|l| parse_line(l)).any(|l| l.name == name);
    let partition = any_line(&|_, ma, mi| ma == major && mi == minor)
        .or_else(|| any_line(&|name, _, _| !source.is_empty() && name == source))?;
    if full(&partition) {
        return Some(partition);
    }
    parent(&partition).filter(|d| full(d))
}

/// Rates over `elapsed_ms`, or `None` for a zero interval or a counter that
/// went backwards (device replaced or counters reset).
fn rates(prev: Counters, cur: Counters, elapsed_ms: f64) -> Option<Rates> {
    if elapsed_ms.is_nan() || elapsed_ms <= 0.0 {
        return None;
    }
    let read = cur.sectors_read.checked_sub(prev.sectors_read)?;
    let written = cur.sectors_written.checked_sub(prev.sectors_written)?;
    let ticks = cur.io_ticks_ms.checked_sub(prev.io_ticks_ms)?;
    let per_sec = |sectors: u64| ((sectors * SECTOR_BYTES) as f64 * 1000.0 / elapsed_ms).round() as u64;
    let busy = (ticks as f64 * 100.0 / elapsed_ms).min(100.0);
    Some(Rates { read_bps: per_sec(read), write_bps: per_sec(written), busy_percent: (busy * 10.0).round() / 10.0 })
}

fn device() -> Option<String> {
    if let Ok(cached) = DEVICE.lock()
        && let Some((name, at)) = cached.as_ref()
        && at.elapsed() < DEVICE_TTL
    {
        return Some(name.clone());
    }
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let diskstats = std::fs::read_to_string("/proc/diskstats").ok()?;
    let name = resolve_device(&mountinfo, &diskstats, parent_disk)?;
    if let Ok(mut cached) = DEVICE.lock() {
        *cached = Some((name.clone(), Instant::now()));
    }
    Some(name)
}

fn read(device: &str) -> Option<Counters> {
    let stats = std::fs::read_to_string("/proc/diskstats").ok()?;
    stats.lines().filter_map(parse_line).find(|l| l.name == device).map(|l| l.counters)
}

fn output(device: String, counters: Counters, rates: Rates) -> DiskIo {
    DiskIo {
        device,
        read_bps: rates.read_bps,
        write_bps: rates.write_bps,
        busy_percent: rates.busy_percent,
        read_total_bytes: counters.sectors_read.saturating_mul(SECTOR_BYTES),
        write_total_bytes: counters.sectors_written.saturating_mul(SECTOR_BYTES),
    }
}

/// Current throughput of the `/backingfiles` device, or `None` when it isn't
/// mounted or `/proc/diskstats` can't be read.
pub async fn get() -> Option<DiskIo> {
    let device = device()?;
    let now = Instant::now();
    let base = {
        let prev = SAMPLE.lock().ok()?;
        match prev.as_ref().filter(|s| s.device == device) {
            Some(s) if now.duration_since(s.taken_at) < MIN_INTERVAL => {
                return s.rates.map(|r| output(device, s.counters, r));
            }
            Some(s) if now.duration_since(s.taken_at) < STALE_AFTER => Some((s.counters, s.taken_at)),
            _ => None,
        }
    };
    let (base, base_at) = match base {
        Some(b) => b,
        None => {
            let first = read(&device)?;
            let at = Instant::now();
            tokio::time::sleep(FRESH_WINDOW).await;
            (first, at)
        }
    };
    let cur = read(&device)?;
    let taken_at = Instant::now();
    let r = rates(base, cur, taken_at.duration_since(base_at).as_secs_f64() * 1000.0);
    if let Ok(mut state) = SAMPLE.lock() {
        *state = Some(Sample { device: device.clone(), counters: cur, taken_at, rates: r });
    }
    r.map(|r| output(device, cur, r))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 4.18+ (18 fields), 5.5+ (20 fields), pre-4.18 (14 fields) and an old
    // short partition line (7 fields).
    const DISKSTATS: &str = "\
   7       0 loop0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0
 179       0 mmcblk0 9000 100 800000 4000 20000 300 1600000 90000 0 50000 94000 0 0 0 0 1200 300
 179       1 mmcblk0p1 200 0 4000 100 2 0 8 1 0 120 101 0 0 0 0
 179       3 mmcblk0p3 5000 50 600000 3000 18000 250 1500000 85000 2 45000 88000 0 0 0 0 1100 280
   8       0 sda 100 0 2000 50 10 0 80 5 0 60 55
   8       1 sda1 50 1000 5 40
";

    fn counters(r: u64, w: u64, t: u64) -> Counters {
        Counters { sectors_read: r, sectors_written: w, io_ticks_ms: t }
    }

    #[test]
    fn parses_old_and_new_diskstats_formats() {
        let lines: Vec<_> = DISKSTATS.lines().filter_map(parse_line).collect();
        let p3 = lines.iter().find(|l| l.name == "mmcblk0p3").unwrap();
        assert_eq!((p3.major, p3.minor), (179, 3));
        assert_eq!(p3.counters, counters(600000, 1500000, 45000), "20-field line");
        let p1 = lines.iter().find(|l| l.name == "mmcblk0p1").unwrap();
        assert_eq!(p1.counters, counters(4000, 8, 120), "18-field line");
        let sda = lines.iter().find(|l| l.name == "sda").unwrap();
        assert_eq!(sda.counters, counters(2000, 80, 60), "14-field line");
        assert!(lines.iter().all(|l| l.name != "sda1"), "short partition lines lack io_ticks");
        assert_eq!(parse_line("garbage"), None);
        assert_eq!(parse_line(""), None);
    }

    const MOUNTINFO: &str = "\
22 27 0:21 / /proc rw,relatime - proc proc rw
27 1 179:2 / / rw,noatime shared:1 - ext4 /dev/root rw
31 27 179:3 / /backingfiles rw,noatime shared:5 - xfs /dev/mmcblk0p3 rw,attr2,inode64
40 27 7:0 / /mnt/cam ro,relatime - vfat /dev/loop0 ro
";

    fn guess(name: &str) -> Option<String> {
        parent_disk_name(name).map(str::to_string)
    }

    #[test]
    fn resolves_the_backingfiles_mount() {
        assert_eq!(backing_mount(MOUNTINFO), Some((179, 3, "mmcblk0p3".into())));
        assert_eq!(resolve_device(MOUNTINFO, DISKSTATS, guess).as_deref(), Some("mmcblk0p3"));
        assert_eq!(backing_mount("27 1 179:2 / / rw - ext4 /dev/root rw\n"), None, "not mounted");
        let stacked = format!("{MOUNTINFO}50 31 8:1 / /backingfiles rw - xfs /dev/sda1 rw\n");
        assert_eq!(backing_mount(&stacked), Some((8, 1, "sda1".into())), "the mount on top wins");
        // /backingfiles2 must not match.
        assert_eq!(backing_mount("31 27 8:1 / /backingfiles2 rw - xfs /dev/sda1 rw\n"), None);
    }

    #[test]
    fn matches_by_number_when_the_source_name_differs() {
        let info = "31 27 179:3 / /backingfiles rw - xfs /dev/disk/by-uuid/abcd rw\n";
        assert_eq!(resolve_device(info, DISKSTATS, guess).as_deref(), Some("mmcblk0p3"));
        let info = "31 27 0:99 / /backingfiles rw - xfs /dev/mmcblk0p3 rw\n";
        assert_eq!(resolve_device(info, DISKSTATS, guess).as_deref(), Some("mmcblk0p3"), "falls back to the source name");
    }

    #[test]
    fn falls_back_to_the_parent_disk() {
        let info = "31 27 8:1 / /backingfiles rw - xfs /dev/sda1 rw\n";
        assert_eq!(resolve_device(info, DISKSTATS, guess).as_deref(), Some("sda"));
        let info = "31 27 9:9 / /backingfiles rw - xfs /dev/nvme0n1p1 rw\n";
        assert_eq!(resolve_device(info, DISKSTATS, guess), None, "nothing in diskstats at all");
    }

    #[test]
    fn parent_disk_names() {
        assert_eq!(parent_disk_name("mmcblk0p3"), Some("mmcblk0"));
        assert_eq!(parent_disk_name("nvme0n1p1"), Some("nvme0n1"));
        assert_eq!(parent_disk_name("sda1"), Some("sda"));
        assert_eq!(parent_disk_name("sdb12"), Some("sdb"));
        assert_eq!(parent_disk_name("sda"), None);
        assert_eq!(parent_disk_name("mmcblk0"), None);
        assert_eq!(parent_disk_name("nvme0n1"), None);
    }

    #[test]
    fn rates_and_busy_share() {
        let a = counters(1000, 2000, 500);
        // 2048 sectors read (1 MiB), 4096 written (2 MiB), 250 ms busy over 1 s.
        assert_eq!(
            rates(a, counters(3048, 6096, 750), 1000.0),
            Some(Rates { read_bps: 1_048_576, write_bps: 2_097_152, busy_percent: 25.0 })
        );
        // Half a second: rates double; busy over the shorter window.
        assert_eq!(
            rates(a, counters(2000, 2000, 625), 500.0),
            Some(Rates { read_bps: 1_024_000, write_bps: 0, busy_percent: 25.0 })
        );
        assert_eq!(rates(a, a, 1000.0), Some(Rates { read_bps: 0, write_bps: 0, busy_percent: 0.0 }), "idle");
        assert_eq!(rates(a, counters(1000, 2000, 2000), 1000.0).unwrap().busy_percent, 100.0, "capped");
        assert_eq!(rates(a, counters(1000, 2000, 533), 300.0).unwrap().busy_percent, 11.0);
        assert_eq!(rates(a, counters(999, 2000, 500), 1000.0), None, "counter went backwards");
        assert_eq!(rates(a, counters(1000, 2000, 499), 1000.0), None, "io_ticks went backwards");
        assert_eq!(rates(a, counters(2000, 3000, 600), 0.0), None, "zero interval");
    }

    #[test]
    fn output_includes_since_boot_totals() {
        let out = output("sda".into(), counters(2, 4, 0), Rates { read_bps: 1, write_bps: 2, busy_percent: 3.0 });
        assert_eq!((out.read_total_bytes, out.write_total_bytes), (1024, 2048));
    }
}
