//! A placeholder file that fills an exFAT filesystem without writing data.
//!
//! Used by `CAM_REPORTED_SIZE`: the cam drive is formatted at the reported
//! size (say 2 TB, so the car sees a drive that size), then this module adds
//! one file, [`NAME`], that takes all but `CAM_SIZE` of the free space. The
//! car can only ever write into the space left, and recycles its own oldest
//! clips there as it does on a `CAM_SIZE` drive.
//!
//! Nothing of the file is written: it is a directory entry set plus bits in
//! the allocation bitmap, with ValidDataLength 0 (exFAT reads such a file as
//! zeros) and NoFatChain (one contiguous run, so no FAT entries either). Its
//! clusters stay holes in the sparse image, so it costs no storage, whatever
//! the kernel's exFAT driver would do on a truncate or fallocate.
//!
//! Layout follows the exFAT specification (boot sector fields, directory
//! entry types 0x85/0xC0/0xC1/0x81, the entry-set checksum and name hash).
//! Only an unmounted image is ever touched.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{bail, ensure, Context, Result};

/// The placeholder's name in the drive's root folder.
pub const NAME: &str = "SentryUSB-reserved.bin";

/// Hidden | System | ReadOnly | Archive: out of the way of file browsers.
const ATTRIBUTES: u16 = 0x0027;

struct Geometry {
    part_offset: u64,
    cluster: u64,
    fat_offset: u64,
    heap_offset: u64,
    cluster_count: u32,
    root_cluster: u32,
}

impl Geometry {
    fn read(f: &mut File, part_offset: u64) -> Result<Self> {
        let mut bs = [0u8; 512];
        f.seek(SeekFrom::Start(part_offset))?;
        f.read_exact(&mut bs).context("read exFAT boot sector")?;
        ensure!(&bs[3..11] == b"EXFAT   ", "not an exFAT filesystem");
        let u32_at = |o: usize| u32::from_le_bytes(bs[o..o + 4].try_into().unwrap());
        let sector_shift = bs[0x6C];
        let cluster_shift = bs[0x6D];
        ensure!((9..=12).contains(&sector_shift) && cluster_shift <= 25 - sector_shift, "bad exFAT geometry");
        let sector = 1u64 << sector_shift;
        Ok(Self {
            part_offset,
            cluster: sector << cluster_shift,
            fat_offset: u64::from(u32_at(0x50)) * sector,
            heap_offset: u64::from(u32_at(0x58)) * sector,
            cluster_count: u32_at(0x5C),
            root_cluster: u32_at(0x60),
        })
    }

    fn cluster_pos(&self, n: u32) -> u64 {
        self.part_offset + self.heap_offset + u64::from(n - 2) * self.cluster
    }

    /// The root directory's clusters, following the FAT chain.
    fn root_clusters(&self, f: &mut File) -> Result<Vec<u32>> {
        let mut out = vec![self.root_cluster];
        loop {
            let n = *out.last().unwrap();
            let mut e = [0u8; 4];
            f.seek(SeekFrom::Start(self.part_offset + self.fat_offset + u64::from(n) * 4))?;
            f.read_exact(&mut e)?;
            let next = u32::from_le_bytes(e);
            if !(2..self.cluster_count + 2).contains(&next) {
                return Ok(out);
            }
            ensure!(out.len() < 1024, "root directory chain too long");
            out.push(next);
        }
    }
}

/// UTF-16 code units of an ASCII name, upper-cased as exFAT's up-case table does.
fn units(name: &str) -> Vec<u16> {
    name.encode_utf16().collect()
}

fn name_hash(name: &[u16]) -> u16 {
    let mut h: u16 = 0;
    for &c in name {
        let c = if (u16::from(b'a')..=u16::from(b'z')).contains(&c) { c - 32 } else { c };
        for b in c.to_le_bytes() {
            h = (if h & 1 != 0 { 0x8000u16 } else { 0 }).wrapping_add(h >> 1).wrapping_add(u16::from(b));
        }
    }
    h
}

fn set_checksum(entries: &[u8]) -> u16 {
    let mut c: u16 = 0;
    for (i, &b) in entries.iter().enumerate() {
        if i == 2 || i == 3 {
            continue;
        }
        c = (if c & 1 != 0 { 0x8000u16 } else { 0 }).wrapping_add(c >> 1).wrapping_add(u16::from(b));
    }
    c
}

/// The directory entry set for the placeholder (File, Stream Extension, Name).
fn entry_set(first_cluster: u32, data_len: u64) -> Vec<u8> {
    let name = units(NAME);
    let name_entries = name.len().div_ceil(15);
    let mut set = vec![0u8; 32 * (2 + name_entries)];
    // a fixed, valid timestamp: 2026-01-01 00:00:00
    let ts: u32 = (((2026 - 1980) << 9 | 1 << 5 | 1) as u32) << 16;
    set[0] = 0x85;
    set[1] = (1 + name_entries) as u8;
    set[4..6].copy_from_slice(&ATTRIBUTES.to_le_bytes());
    for o in [8, 12, 16] {
        set[o..o + 4].copy_from_slice(&ts.to_le_bytes());
    }
    let s = &mut set[32..64];
    s[0] = 0xC0;
    s[1] = 0x03; // AllocationPossible | NoFatChain
    s[3] = name.len() as u8;
    s[4..6].copy_from_slice(&name_hash(&name).to_le_bytes());
    // ValidDataLength (8..16) stays 0: nothing is written
    s[20..24].copy_from_slice(&first_cluster.to_le_bytes());
    s[24..32].copy_from_slice(&data_len.to_le_bytes());
    for (i, chunk) in name.chunks(15).enumerate() {
        let e = &mut set[64 + 32 * i..96 + 32 * i];
        e[0] = 0xC1;
        for (j, c) in chunk.iter().enumerate() {
            e[2 + 2 * j..4 + 2 * j].copy_from_slice(&c.to_le_bytes());
        }
    }
    let sum = set_checksum(&set);
    set[2..4].copy_from_slice(&sum.to_le_bytes());
    set
}

/// The root directory's raw entries and where each 32-byte slot lives.
fn read_root(f: &mut File, g: &Geometry) -> Result<(Vec<u8>, Vec<u64>)> {
    let mut raw = Vec::new();
    let mut pos = Vec::new();
    for c in g.root_clusters(f)? {
        let mut buf = vec![0u8; g.cluster as usize];
        f.seek(SeekFrom::Start(g.cluster_pos(c)))?;
        f.read_exact(&mut buf)?;
        for i in 0..(g.cluster / 32) {
            pos.push(g.cluster_pos(c) + i * 32);
        }
        raw.extend_from_slice(&buf);
    }
    Ok((raw, pos))
}

/// Whether the placeholder is in the root folder of the exFAT filesystem
/// starting at `part_offset`.
pub fn has_placeholder(image: &Path, part_offset: u64) -> Result<bool> {
    let mut f = File::open(image).with_context(|| format!("open {}", image.display()))?;
    let g = Geometry::read(&mut f, part_offset)?;
    let (raw, _) = read_root(&mut f, &g)?;
    let want = units(NAME);
    for i in 0..raw.len() / 32 {
        let e = &raw[i * 32..i * 32 + 32];
        match e[0] {
            0x00 => break,
            0x85 if i + 2 < raw.len() / 32 && raw[(i + 1) * 32] == 0xC0 => {
                let len = raw[(i + 1) * 32 + 3] as usize;
                let mut name = Vec::new();
                let mut k = i + 2;
                while name.len() < len && k < raw.len() / 32 && raw[k * 32] == 0xC1 {
                    for j in 0..15 {
                        let o = k * 32 + 2 + 2 * j;
                        name.push(u16::from_le_bytes([raw[o], raw[o + 1]]));
                    }
                    k += 1;
                }
                name.truncate(len);
                if name == want {
                    return Ok(true);
                }
            }
            _ => {}
        }
    }
    Ok(false)
}

/// Add the placeholder to a freshly formatted, UNMOUNTED exFAT filesystem
/// at `part_offset` in `image`, leaving `keep_free` bytes (rounded up to
/// whole clusters) free for the car. Returns the placeholder's size in bytes
/// (0 when there is nothing to reserve). Fails, changing nothing, when the
/// root folder has no room or the clusters at the end of the heap are in use.
pub fn add_placeholder(image: &Path, part_offset: u64, keep_free: u64) -> Result<u64> {
    let mut f = OpenOptions::new().read(true).write(true).open(image)
        .with_context(|| format!("open {}", image.display()))?;
    let g = Geometry::read(&mut f, part_offset)?;
    let (raw, pos) = read_root(&mut f, &g)?;
    ensure!(!has_placeholder(image, part_offset)?, "{NAME} already exists");

    // the allocation bitmap (entry 0x81) and the first never-used slot
    let mut bitmap: Option<(u32, u64)> = None;
    let mut end_slot = None;
    for i in 0..raw.len() / 32 {
        let e = &raw[i * 32..i * 32 + 32];
        if e[0] == 0x81 && bitmap.is_none() {
            bitmap = Some((u32::from_le_bytes(e[20..24].try_into().unwrap()), u64::from_le_bytes(e[24..32].try_into().unwrap())));
        }
        if e[0] == 0x00 {
            end_slot = Some(i);
            break;
        }
    }
    let (bm_cluster, bm_len) = bitmap.context("no allocation bitmap entry")?;
    let set_len = 32 * (2 + units(NAME).len().div_ceil(15));
    let slot = end_slot.context("root folder is full")?;
    ensure!((slot + set_len / 32) * 32 <= raw.len(), "root folder has no room for {NAME}");

    let mut bits = vec![0u8; bm_len as usize];
    f.seek(SeekFrom::Start(g.cluster_pos(bm_cluster)))?;
    f.read_exact(&mut bits)?;
    let used = |i: u32| bits[(i / 8) as usize] & (1 << (i % 8)) != 0;

    // count the free clusters; reserve all but keep_free, at the END of the heap
    let free = (0..g.cluster_count).filter(|&i| !used(i)).count() as u64;
    let keep = keep_free.div_ceil(g.cluster);
    if free <= keep {
        return Ok(0);
    }
    let n = (free - keep) as u32;
    let first_index = g.cluster_count - n;
    ensure!((first_index..g.cluster_count).all(|i| !used(i)), "the end of the drive is not free");

    // bitmap first, then the directory entries: a crash in between leaves
    // clusters marked used with no file, which fsck reclaims, never a file
    // whose clusters look free
    for i in first_index..g.cluster_count {
        bits[(i / 8) as usize] |= 1 << (i % 8);
    }
    f.seek(SeekFrom::Start(g.cluster_pos(bm_cluster)))?;
    f.write_all(&bits)?;
    let set = entry_set(first_index + 2, u64::from(n) * g.cluster);
    for (k, e) in set.chunks(32).enumerate() {
        f.seek(SeekFrom::Start(pos[slot + k]))?;
        f.write_all(e)?;
    }
    f.sync_all()?;
    Ok(u64::from(n) * g.cluster)
}

/// MBR first partition: (start, end) in bytes, or `None` without a table.
pub fn first_partition(image: &Path) -> Result<Option<(u64, u64)>> {
    let mut mbr = [0u8; 512];
    let mut f = File::open(image).with_context(|| format!("open {}", image.display()))?;
    if f.read_exact(&mut mbr).is_err() || mbr[510] != 0x55 || mbr[511] != 0xAA {
        return Ok(None);
    }
    let e = &mbr[446..462];
    if e[4] == 0 || e[4] == 0xEE {
        return Ok(None);
    }
    let start = u64::from(u32::from_le_bytes(e[8..12].try_into().unwrap())) * 512;
    let sectors = u64::from(u32::from_le_bytes(e[12..16].try_into().unwrap()));
    if start == 0 || sectors == 0 {
        bail!("empty first partition");
    }
    Ok(Some((start, start + sectors * 512)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn have(tool: &str) -> bool {
        Command::new("which").arg(tool).output().is_ok_and(|o| o.status.success())
    }

    /// A sparse image with an MBR and an exFAT partition from 1 MiB, made by
    /// mkfs.exfat on an offset image file (no loop device needed).
    fn image(dir: &Path, size: u64) -> (std::path::PathBuf, u64) {
        let img = dir.join("cam_disk.bin");
        let f = File::create(&img).unwrap();
        f.set_len(size).unwrap();
        let mut mbr = [0u8; 512];
        mbr[510] = 0x55;
        mbr[511] = 0xAA;
        mbr[446 + 4] = 0x07;
        mbr[446 + 8..446 + 12].copy_from_slice(&2048u32.to_le_bytes());
        mbr[446 + 12..446 + 16].copy_from_slice(&((size / 512 - 2048) as u32).to_le_bytes());
        let mut f = OpenOptions::new().write(true).open(&img).unwrap();
        f.write_all(&mbr).unwrap();
        let part = dir.join("part.bin");
        File::create(&part).unwrap().set_len(size - (1 << 20)).unwrap();
        let ok = Command::new("mkfs.exfat").args(["-L", "CAM"]).arg(&part).output().unwrap();
        assert!(ok.status.success(), "{}", String::from_utf8_lossy(&ok.stderr));
        // copy the formatted metadata in, keeping the image sparse
        let mut src = File::open(&part).unwrap();
        let mut buf = vec![0u8; 1 << 20];
        let mut off = 0u64;
        loop {
            let n = src.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            if buf[..n].iter().any(|&b| b != 0) {
                f.seek(SeekFrom::Start((1 << 20) + off)).unwrap();
                f.write_all(&buf[..n]).unwrap();
            }
            off += n as u64;
        }
        std::fs::remove_file(&part).unwrap();
        (img, 1 << 20)
    }

    /// fsck.exfat on a copy of the partition: `-n` (report only), or `-y -s`
    /// (repair, turning clusters marked used that no file owns into rescue
    /// files) followed by a `-n` report of the result.
    fn fsck(img: &Path, off: u64, dir: &Path, rescue: bool) -> String {
        let part = dir.join("check.bin");
        let mut src = File::open(img).unwrap();
        let len = src.metadata().unwrap().len() - off;
        let mut dst = File::create(&part).unwrap();
        dst.set_len(len).unwrap();
        // sparse copy of the partition
        let mut buf = vec![0u8; 1 << 20];
        src.seek(SeekFrom::Start(off)).unwrap();
        let mut at = 0u64;
        loop {
            let n = src.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            if buf[..n].iter().any(|&b| b != 0) {
                dst.seek(SeekFrom::Start(at)).unwrap();
                dst.write_all(&buf[..n]).unwrap();
            }
            at += n as u64;
        }
        if rescue {
            let _ = Command::new("fsck.exfat").args(["-y", "-s"]).arg(&part).output().unwrap();
        }
        let out = Command::new("fsck.exfat").arg("-n").arg(&part).output().unwrap();
        std::fs::remove_file(&part).unwrap();
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
            + if out.status.success() { "\nEXIT 0" } else { "\nEXIT NONZERO" }
    }

    #[test]
    fn hashes_and_checksums_match_the_spec_examples() {
        // name hash is over the up-cased name; checksum skips bytes 2 and 3
        assert_eq!(name_hash(&units("abc")), name_hash(&units("ABC")));
        let set = entry_set(100, 1 << 30);
        assert_eq!(u16::from_le_bytes([set[2], set[3]]), set_checksum(&set));
        assert_eq!(set[1] as usize, set.len() / 32 - 1);
    }

    #[test]
    fn placeholder_fills_the_drive_passes_fsck_and_writes_no_data() {
        if !have("mkfs.exfat") || !have("fsck.exfat") {
            eprintln!("skipped: exfatprogs not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (img, off) = image(dir.path(), 1 << 30); // a 1 GiB "2 TB" drive
        assert_eq!(first_partition(&img).unwrap().map(|p| p.0), Some(off));
        assert!(!has_placeholder(&img, off).unwrap());
        #[cfg(unix)]
        let before = { use std::os::unix::fs::MetadataExt; img.metadata().unwrap().blocks() };

        let reserved = add_placeholder(&img, off, 256 << 20).unwrap(); // keep 256 MiB free
        assert!(reserved > 600 << 20 && reserved < 800 << 20, "reserved {reserved}");
        assert!(has_placeholder(&img, off).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let after = img.metadata().unwrap().blocks();
            assert!((after - before) * 512 < 1 << 20, "no data written ({} bytes)", (after - before) * 512);
        }
        let report = fsck(&img, off, dir.path(), false);
        assert!(report.ends_with("EXIT 0"), "fsck.exfat:\n{report}");
        assert!(report.contains("directories 1, files 1"), "fsck.exfat sees the placeholder:\n{report}");
        // the bitmap marks exactly the placeholder: rescue finds no orphaned
        // clusters (it would add a LOST+FOUND folder and a file)
        let rescued = fsck(&img, off, dir.path(), true);
        assert!(rescued.contains("directories 1, files 1"), "bitmap has orphaned clusters:\n{rescued}");
        eprintln!("{report}\n--- after rescue:\n{rescued}");

        // control: a placeholder cluster the bitmap calls free IS detected,
        // so the clean reports above really checked the bitmap
        let broken = dir.path().join("broken.bin");
        std::fs::copy(&img, &broken).unwrap();
        let mut f = OpenOptions::new().read(true).write(true).open(&broken).unwrap();
        let g = Geometry::read(&mut f, off).unwrap();
        let (raw, _) = read_root(&mut f, &g).unwrap();
        let bm = (0..raw.len() / 32).find(|i| raw[i * 32] == 0x81).unwrap() * 32;
        let bm_cluster = u32::from_le_bytes(raw[bm + 20..bm + 24].try_into().unwrap());
        let last = g.cluster_count - 1; // the placeholder's last cluster
        let at = g.cluster_pos(bm_cluster) + u64::from(last / 8);
        let mut b = [0u8; 1];
        f.seek(SeekFrom::Start(at)).unwrap();
        f.read_exact(&mut b).unwrap();
        assert_ne!(b[0] & (1 << (last % 8)), 0, "placeholder cluster is marked used");
        b[0] &= !(1 << (last % 8));
        f.seek(SeekFrom::Start(at)).unwrap();
        f.write_all(&b).unwrap();
        drop(f);
        let caught = fsck(&broken, off, dir.path(), false);
        eprintln!("--- control (one placeholder cluster marked free):\n{caught}");
        assert!(!caught.contains("clean."), "fsck.exfat missed a bitmap/file mismatch:\n{caught}");
        assert!(add_placeholder(&img, off, 256 << 20).is_err(), "never added twice");
    }
}

#[cfg(test)]
mod full_size {
    use super::*;
    use std::process::Command;

    /// The real thing: a 2 TB filesystem keeping 40 GB free. Slow-ish (mkfs
    /// and fsck of 2 TB of metadata), so run on demand:
    /// cargo test -p sentryusb-gadget --lib full_size -- --ignored --nocapture
    #[test]
    #[ignore]
    fn two_terabytes_keeping_forty_gigabytes() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("cam_part.bin");
        File::create(&part).unwrap().set_len(2 << 40).unwrap();
        let ok = Command::new("mkfs.exfat").args(["-L", "CAM"]).arg(&part).output().unwrap();
        assert!(ok.status.success(), "{}", String::from_utf8_lossy(&ok.stderr));
        let du = |p: &Path| { use std::os::unix::fs::MetadataExt; p.metadata().unwrap().blocks() * 512 };
        let before = du(&part);
        let t = std::time::Instant::now();
        let reserved = add_placeholder(&part, 0, 40 << 30).unwrap();
        eprintln!("reserved {} GiB in {:?}; on disk {} MiB before, {} MiB after",
            reserved >> 30, t.elapsed(), before >> 20, du(&part) >> 20);
        let out = Command::new("fsck.exfat").arg("-n").arg(&part).output().unwrap();
        let report = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
        eprintln!("{report}");
        assert!(out.status.success() && report.contains("files 1"));
        let free = (2u64 << 40) - reserved;
        assert!(free > 40 << 30 && free < 42 << 30, "about 40 GB stays free (plus metadata): {free}");
        assert!(du(&part) - before < 4 << 20, "no data written");
    }
}
