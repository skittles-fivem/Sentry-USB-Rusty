# Sentry USB ↔ Sentry Manager: the archive share contract

Sentry USB (the Pi) uploads dashcam clips to an archive share. Sentry Manager
reads the same share, builds its footage inventory, and can re-encode clips.
This document is what each side may write to the share, and what each side
must do so they never undo each other's work.

Sentry USB side: `run/archiveloop` (sections "Archive verification" and
"Status file on the archive share"), `run/rsync_archive/archive-clips.sh`.

## 1. Layout (unchanged)

Clips keep the Tesla layout and names: `RecentClips/<YYYY-MM-DD>/…`,
`SentryClips/<event>/…`, `SavedClips/<event>/…`, `TeslaTrackMode/…`, with
clip names `YYYY-MM-DD_HH-MM-SS-<camera>.mp4`. Neither side renames folders
or clips without updating this document.

## 2. What Sentry USB writes besides clips (Manager: ignore all of it)

| Path | What it is |
|---|---|
| `.sentryusb/archive-status.json` | Status file (section 4) |
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

### How Sentry USB decides

| Share copy | Manager's record line | Result on the Pi |
|---|---|---|
| matches the Pi's hash | (not read) | `ok`: verified |
| re-encoded | equals the Pi's hash | `converted`: verified, counts toward auto-delete |
| re-encoded | differs from the Pi's hash | `mismatch`: Manager converted a bad upload. Its output is renamed to `.mismatch-<time>` and the original is re-uploaded, so expect it back to convert again. |
| re-encoded | none, but the clip matched before it was converted | `converted`: verified |
| re-encoded | none, and it never matched before conversion | `converted_unconfirmed`: kept and never re-sent, but that Pi snapshot is never auto-deleted |
| differs, not a re-encode | (not read) | `mismatch`: renamed aside, re-uploaded |

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
  "transfer": null
}
```

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

## 5. Checking it works end to end

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
