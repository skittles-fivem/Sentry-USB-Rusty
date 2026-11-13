# Sentry USB ↔ Sentry Manager: the archive share contract

Sentry USB (the Pi) uploads dashcam clips to an archive share. Sentry Manager
reads the same share, builds its footage inventory, and can re-encode clips.
This document is what each side may write to the share, and what each side
must do so they never undo each other's work.

Sentry USB side: `run/archiveloop` (sections "Archive verification",
"Status file on the archive share" and "Device status file on the archive
share"), `run/rsync_archive/archive-clips.sh`, `crates/api/src/device_status`.

The share is the only connection between the two: Manager never connects to
the Pi, and the Pi never connects to Manager. What Manager shows about the Pi
comes from the files in section 2.

## 1. Layout (unchanged)

Clips keep the Tesla layout and names: `RecentClips/<YYYY-MM-DD>/…`,
`SentryClips/<event>/…`, `SavedClips/<event>/…`, `TeslaTrackMode/…`, with
clip names `YYYY-MM-DD_HH-MM-SS-<camera>.mp4`. Neither side renames folders
or clips without updating this document.

## 2. What Sentry USB writes besides clips (Manager: ignore all of it)

| Path | What it is |
|---|---|
| `.sentryusb/archive-status.json` | Status file (section 4) |
| `.sentryusb/car-files.tsv` | Every clip the car wrote to its USB drive in the last 7 days (section 4) |
| `.sentryusb/device-status.json` | The Pi's version, storage, drives, CPU temperature and the car's latest readings (section 5) |
| `<clip folder>/.sentryusb-partial/<clip>.mp4` | An interrupted rsync upload, waiting to resume. Moved to its real name when complete; the folder is removed when empty. |
| `.sentryusbtmp/` | SMB/NFS upload temp folder |
| `<clip>.mp4.mismatch-<unix time>` | A server copy that failed its SHA-256 check, renamed aside (never deleted). The clip is re-uploaded under its real name. |

Rule for Manager: skip anything dot-prefixed and anything not ending in
`.mp4`. Don't index, count, convert or flag those as gaps.

The Pi in turn ignores everything it didn't upload. Manager's
`TEMP-H265-<name>.mp4` working copies and `.sentrymanager/` are never read
or touched, except the checksum record (section 3).

Sentry USB never deletes anything on the share.

## 3. Checksum verification and Manager's re-encodes

After each upload, Sentry USB computes the SHA-256 of the clip on the Pi and of
the copy on the share and compares them. With auto-delete on, it checks again
at least an hour later, then deletes Pi snapshots whose clips all passed
twice.

Manager may re-encode a clip **in place** ("replace" output mode). The result
can't match the Pi's hash, so Sentry USB recognises re-encodes:

- **Detected as a re-encode:** the share copy's video sample entry is `hvc1`
  (or `hev1`) for H.265, or `av01` for AV1, while the Pi's copy is the car's
  H.264. Only the first and last MiB of the file are read; Manager's output
  keeps `moov` in the last MiB.
- **Never treated as damage:** a re-encode is never renamed aside or
  re-uploaded.

### What Manager must do (required for in-place conversion)

1. **Write a checksum record of the original before replacing it.**
   - File: `.sentrymanager/original-checksums.sha256` in the folder that holds
     `RecentClips/`. That folder is the Pi's share root: the Pi uploads to
     `<share root>/RecentClips/…`. If the NAS share has a `TeslaCam/` level,
     the record is `TeslaCam/.sentrymanager/original-checksums.sha256`, and
     the Pi's archive path points at that `TeslaCam/` folder.
   - Format: `sha256sum` style, one line per clip:
     `<64 lowercase hex>  <relpath>` (two spaces). The relpath is from the
     share root with no leading `./`, e.g.
     `RecentClips/2026-11-08/2026-11-08_12-59-00-front.mp4`.
   - Order: hash the original bytes, append the line, flush or fsync, and
     only then replace the file. If the replace then fails, the line is
     harmless.
   - Append only; a later line for the same path wins. Keep at least 30 days.
     Pruning older lines is fine if you rewrite to a temp file and rename.
2. **Replace atomically.** Write the output under any name other than the
   clip's final name, then rename it over the original. A half-written file
   under the real name looks like damage. Manager uses
   `TEMP-H265-<name>.mp4` in the same folder, which is fine: the Pi only ever
   opens the exact paths it uploaded.
   Manager's replace is gap-free on shares with hard links. Without them it
   falls back to removing the original a moment before renaming the
   converted copy in. The Pi tolerates that: a clip absent at its first
   look is asked for again after 20 s (ARCHIVE_VERIFY_ABSENT_RECHECK_SECONDS)
   and only counts as missing if it's still absent. A longer gap would make
   the Pi re-send the original, so on the first test, watch the share for
   `*.mismatch-*` files and the Pi's log for "isn't on the archive server"
   lines.
3. **Keep the output recognisable.** Same relpath and name, an MP4 whose video
   sample entry is `hvc1` (H.265) or `av01` (AV1), with the `moov` box in the
   first or last MiB. Manager writes `hvc1`, never `hev1`, with `moov` at the
   end. The Pi also accepts `hev1`, but nothing on the Manager side relies on
   that.
4. **Don't convert clips that are still arriving.** See section 4.

"Separate" output mode (converted copies in another folder) leaves originals
untouched and needs none of this.

### Manager's record comes first (no read-back)

On an SMB/NFS share, the Pi's only other way to hash the share's copy is to
read all of it back over the network. So when the record file exists, the
Pi looks there first:

- **A clip with a line:** the Pi compares the line with its own hash, and
  reads nothing back. An equal line means the share held exactly the
  uploaded bytes when Manager converted them.
- **A clip without a line yet:** it waits in the Pi's queue for Manager to
  convert it, for up to 24 hours (`ARCHIVE_VERIFY_MANAGER_WAIT_HOURS`; 0 =
  don't wait). Only then is it read back. While parked at home, the Pi
  checks the record again every 15 minutes.
- **Clips Manager never converts** fall back to read-back after the wait.
  Optional, to save that traffic: Manager could also append a line for clips
  it decides not to convert, in the same format. The Pi treats any line as
  the uploaded bytes' hash.

### How Sentry USB decides

| Share copy | Manager's record line | Result on the Pi |
|---|---|---|
| (not read) | equals the Pi's hash | `converted`: verified, no read-back |
| (not read) | none yet, under 24 h | waits in the queue |
| matches the Pi's hash | none, or no record file | `ok`: verified (read back) |
| re-encoded | equals the Pi's hash | `converted`: verified, counts toward auto-delete |
| re-encoded | differs from the Pi's hash | `mismatch`: Manager converted a bad upload. Its output is renamed to `.mismatch-<time>` and the original is re-uploaded, so expect it back to convert again. |
| re-encoded | none, but the clip matched before it was converted | `converted`: verified |
| re-encoded | none, and it never matched before conversion | `converted_unconfirmed`: kept and never re-sent, but that Pi snapshot is never auto-deleted |
| differs, not a re-encode | (not read) | `mismatch`: renamed aside, re-uploaded |

### Basic mode (`ARCHIVE_VERIFY_MODE=basic`, SMB/NFS only)

The user can choose a basic check instead of SHA-256 in Settings. Then the
Pi neither reads Manager's record nor waits for it. For each clip it reads
only the MP4 index of both copies (box headers and the `moov` box) and
compares the size, the video codec, the number of frames, the duration and
every frame's size (`stsz`).

| Share copy | Result on the Pi |
|---|---|
| same size, codec and frames | `ok_basic`: verified |
| Manager's `hvc1`/`hev1`/`av01` re-encode of the Pi's H.264, same frame count, duration within 100 ms | `converted_basic`: verified |
| Manager's re-encode, frame count differs by exactly one, duration within three quarters of a frame (average frame length; real re-encodes land about 0.6 frame off) | `converted_basic`: verified. Covers Manager's two known one-frame cases, where the total duration is kept: Tesla's encoder skipping a frame_num across the wrap before an IDR, and variable-frame-rate clips |
| Manager's re-encode with a different frame count or duration | `converted_unconfirmed`: kept and never re-sent, but that snapshot is never auto-deleted |
| anything else (cut off, different frames, not on the share) | `mismatch` or `missing`: renamed aside / re-sent as usual |

What this asks of Manager: keep each video frame of the original and its
timing in a re-encode (`-fps_mode passthrough`, no `-r` or fps filter). Where
FFmpeg can't output one picture, keep the total duration by giving its time to
the previous frame. Keep `moov` within the file's top-level boxes. Manager
already does all of this. Writing the checksum record does no harm; basic mode
just doesn't use it.

## 4. Status file: `.sentryusb/archive-status.json`

The Pi replaces this file atomically twice per archive cycle: when a
transfer starts, and when the cycle's archive and checksum steps finish.
Schema 1:

```json
{
  "schema": 1,
  "device": "sentryusb",
  "written_unix": 1762646400,
  "archive": {
    "result": "in_progress | ok | failed | nothing_new",
    "started_unix": 1762646000, "finished_unix": 1762646390,
    "files_archived": 120, "files_not_archived": 0,
    "last_clip": "RecentClips/2026-11-08/2026-11-08_12-59-00-front.mp4"
  },
  "verification": {
    "enabled": true, "ran": true,
    "matched": 118, "converted": 2, "resending": 0,
    "no_pi_copy": 0, "still_queued": 0, "stopped": null
  },
  "auto_delete_enabled": false,
  "state": "transferring | idle",
  "snapshot_interval_s": 3480,
  "snapshots": [
    { "name": "snap-000011", "started_unix": 1762731609, "kept": true },
    { "started_unix": 1762735089, "kept": false },
    { "name": "snap-000012", "started_unix": 1762738569, "kept": true }
  ],
  "snapshot_health": {
    "state": "ok | stalled | missed", "checked_unix": 1762739000,
    "last_run_unix": 1762738569, "newest_kept_unix": 1762738569,
    "clips_not_kept": 0, "example_clip": null
  },
  "transfer": null
}
```

`snapshot_interval_s` and `snapshots` are optional (absent from files
written by older Sentry USB builds), and `schema` stays 1:
- `snapshot_interval_s`: the SNAPSHOT_INTERVAL the Pi's snapshot loop uses,
  in seconds. Written only when it's within 60-86400; otherwise left out.
- `snapshots`: every snapshot run in the last 7 days, oldest first, at
  most 1000 (the newest), `started_unix` being when the run copied the
  car's drive.
  - A kept snapshot has `name` (`snap-NNNNNN`, always matching
    `^[A-Za-z0-9._-]{1,64}$`) and `"kept": true`, and stays listed after
    it's released or deleted. Snapshots taken before the Pi had this log are
    listed with their image's time, and only while they're still on the Pi.
  - A run discarded as identical to the previous snapshot has no `name` and
    `"kept": false`: the car wrote nothing new to the drive since the last
    snapshot, so there was nothing to keep. It still counts as a snapshot
    point.

  The list survives reboots (it's kept in `/mutable/snapshot_log.tsv`), and
  1000 entries take about 73 KB, so the file stays well under 256 KB.

  The car keeps about 60 min of RecentClips, so footage can only be lost
  when the car was driving and no run (kept or not) started within about
  60 min of it. A parked stretch shows regular unkept runs every
  `snapshot_interval_s`.

`snapshot_health` is optional too (left out by older builds, or before the
first check). The Pi checks it every 10 minutes, after listing the car's
drive for `car-files.tsv`, and from outside the snapshot loop, so a stuck
loop or a run that read the wrong image can't hide it:
- `ok`: snapshot runs are happening and the newest kept snapshot holds
  every clip the car wrote since it.
- `stalled`: no snapshot run (kept or not) for over twice
  `snapshot_interval_s` plus 5 minutes. The snapshot loop is stuck; footage
  older than about 60 min is being lost.
- `missed`: `clips_not_kept` clips (`.mp4`, counted once per path) were
  first seen in `car-files.tsv` after `newest_kept_unix` and at least a
  minute before `last_run_unix`, yet the newest kept snapshot doesn't hold
  them. A run should have kept them and didn't. `example_clip` is one of
  them (path under TeslaCam, as in `car-files.tsv`). This is what happened on
  2026-11-12: runs read a stale mount of an old image and discarded
  themselves as identical for 19 hours while the car recorded.

`newest_kept_unix` is null when the Pi holds no kept snapshot. Show
`stalled` and `missed` as a Sentry USB problem (not missing car footage),
using `checked_unix` for "as of". The Pi also logs and alerts once per
problem.

When a transfer is in progress, `transfer` is:
`{"started_unix", "files", "bytes", "first_clip", "last_clip", "expected_finish_unix", "give_up_unix"}`.

How Manager should use it:
- **`state` is `transferring`:** clips between `first_clip` and `last_clip`
  (ordered by clip name) are still arriving. Don't flag gaps or convert in
  that range yet.
- **Still `transferring` after `give_up_unix`:** the car drove off or the Pi
  lost its connection mid-transfer. Nothing is lost: the clips stay on the Pi
  and the transfer resumes at the next archive. Show "waiting for Sentry USB"
  instead of reporting missing footage.
- **`archive.result` is `failed`:** `files_not_archived` files are retried
  next cycle.
- **Display:** it's worth showing last archive time, `last_clip` and the
  verification counts in Manager.

### `.sentryusb/car-files.tsv`: what the car wrote

Written beside the status file, whenever it's written. Every 10 minutes the
Pi lists the clips on the car's drive (from an instant read-only copy,
independent of the snapshots) and records each clip it hasn't seen, or that
has grown since. The car keeps RecentClips about an hour, so every clip it
writes is seen at least once. Tab-separated, one header line starting with
`#`, then:

```
<first seen unix>	<path under TeslaCam>	<size bytes>	<mtime unix>
```

e.g. `1762800000	RecentClips/2026-11-10_12-00-00-front.mp4	30000000	1762790000`
(RecentClips paths are the car's flat layout, without the date folder used on
the share). A clip appears again with a later first-seen time when it grew;
take the largest size per path. Lines from the last 7 days only.

Use it to tell where footage went missing:
- In this file but not on the share: the car wrote it and the Pi didn't keep
  or upload it (a Sentry USB problem).
- Expected (the car was driving) but not in this file: the car never wrote it
  to the USB drive (a car or USB problem).

The Pi also logs a daily line in its archiveloop log, e.g. "Car files for
2026-11-10: 1234 RecentClips file(s) covering 310 minute(s), 3 Sentry
event(s), 1 saved clip(s)".

## 5. Device status: `.sentryusb/device-status.json`

Beside `archive-status.json`, at the same share root. What Manager shows
about the Pi itself, without ever connecting to it.

**When it's written:**
- at the start and the end of every archive cycle (also the parked-at-home
  backlog checks);
- every 5 minutes while the share is mounted on the Pi (SMB/NFS);
- at most once a minute, while mounted, when a car reading changes (a newer
  reading of the same value is not a change).

Writing it is best effort and never holds up or slows the archive; a failed
write is logged on the Pi and tried again next time. On rsync and rclone
archives it's written only at the start and end of a cycle (the Pi has
nothing mounted between them).

**How:** atomically. The Pi writes `.sentryusb/device-status.json.tmp` in the
same folder and renames it over `device-status.json`, so a reader sees the
old file or the new one, never half of one. (rsync uses its own dot-named
temp file, then renames.) At most 16 KiB.

**Schema 1:**

```json
{
  "schema": 1,
  "written_unix": 1762800000,
  "version": "v3.28.10-test.32",
  "storage": { "total_bytes": 256060514304, "used_bytes": 98100000000, "free_bytes": 157960514304 },
  "drives": [ { "name": "cam", "size_bytes": 64424509440 }, { "name": "music", "size_bytes": 4294967296 } ],
  "cpu_temp_c": 51.2,
  "car": {
    "battery":  { "pct": 89, "charging": false, "at": 1762800000 },
    "interior": { "c": 9.4, "at": 1762800000 },
    "exterior": { "c": 7.8, "at": 1762799940 },
    "tires": { "fl": { "kpa": 290.3, "at": 1762792800 }, "fr": { "kpa": 289.6, "at": 1762792800 }, "rl": { "kpa": 288.9, "at": 1762771200 }, "rr": { "kpa": 291.6, "at": 1762771200 }, "state": "optimal" },
    "drive": { "state": "parked", "since": 1762791000, "at": 1762799990 }
  }
}
```

`schema` and `written_unix` (Unix seconds) are always there. Every other
field is optional and left out when the Pi doesn't know it:

- `version`: the Sentry USB version (at most 64 characters).
- `storage`: the Pi's own storage that holds the car's drive images and
  snapshots (the backing-files filesystem), in bytes. The same numbers the
  Pi's dashboard shows as used and free.
- `drives`: up to 8 `{name, size_bytes}`, `name` one of `cam`, `music`,
  `lightshow`, `boombox`, `size_bytes` the size the car sees. Only drives
  that exist.
- `cpu_temp_c`: the Pi's CPU temperature in °C, one decimal, -40 to 125.
- `car`: the car's latest readings from the Pi's Tesla BLE telemetry. No
  `car` key when there are none; inside it, unknown readings are left out.
  - `battery`: `pct` (0-100), `charging` (when known), `at`.
  - `interior`, `exterior`: `c`, °C with one decimal, and `at`.
  - `tires`: `fl`, `fr`, `rl`, `rr`, each `kpa` (one decimal) and `at`, when
    the car last heard that sensor; `state` is `optimal`, `check` or
    `unsafe`, the worst tire against the owner's `TIRE_PSI_BANDS`.
  - `drive`: `state` `parked` or `driving`; `since` (parked: when the last
    drive ended) and `at` (when the gear was read), each when known.
  - `at` and `since` are Unix seconds. Never the car's place, position,
    odometer or VIN, and no display units: Manager converts for display.

Treat a file whose `written_unix` is old as "the Pi hasn't reached the share
since then", not as current values.

## 6. Checking it works end to end

1. Manager writes `.sentrymanager/original-checksums.sha256` before each
   in-place conversion.
2. The Pi runs Sentry USB `v3.28.10-test.5` or later.
3. After an archive with auto-convert on, the Pi's `archiveloop` log shows
   `Archive verification: N matched, M re-encoded on the server by Sentry
   Manager, 0 to re-send, …`.
4. On the Pi's Snapshots page, those snapshots show "Verified on archive",
   not "unconfirmed".
5. No `*.mismatch-*` files appear on the share, unless an upload really was
   damaged.
