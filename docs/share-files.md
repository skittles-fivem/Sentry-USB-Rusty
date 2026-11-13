# Files Sentry USB writes on the archive share

Besides the footage itself, Sentry USB writes status files into a
`.sentryusb/` folder at the root of the archive share (the folder that holds
`RecentClips/` or `TeslaCam/`). Programs that read the share, such as
Sentry Six Manager, use them to show the Pi's state. The share is the only
channel: no pairing, no network link to the Pi and no extra ports.

Readers must ignore files and keys they don't know: newer versions may add
them.

## `.sentryusb/device-status.json`

The Pi's version, storage, drives, CPU temperature and, when the Pi reads
car data, the car's latest readings. Built by `crates/api/src/device_status`.

### When it is written

- At the start and at the end of every archive cycle (`write_device_status`
  in `run/archiveloop`).
- Every 5 minutes while an SMB or NFS share is mounted.
- While mounted, at most once a minute when a car reading changes.

It is best effort: writing it never blocks or slows archiving. A failed or
slow write (20 seconds at most) is logged and dropped, and a stuck write is
never followed by a second one. A reader may find a file that is minutes or
days old (the Pi was away from home).

### How it is written

- **SMB / NFS:** the Pi writes `.sentryusb/device-status.json.tmp`, then
  renames it over `device-status.json`. A reader sees the old file or the
  new one, never half of one. Ignore the `.tmp` name.
- **rsync:** the file is sent with each cycle; rsync itself writes a hidden
  temporary name and renames it into place.
- **rclone:** the file is copied with each cycle (`rclone copyto`); how the
  replacement happens depends on the remote.

### Format

JSON, UTF-8, at most 16 KiB (16384 bytes). If the content would be larger,
`car` is left out first.

```json
{
  "schema": 1,
  "written_unix": 1762800000,
  "clock_synced": true,
  "version": "v3.28.10",
  "storage": { "total_bytes": 256060514304, "used_bytes": 98100000000, "free_bytes": 157960514304 },
  "drives": [ { "name": "cam", "size_bytes": 64424509440 }, { "name": "music", "size_bytes": 4294967296 } ],
  "cpu_temp_c": 51.2,
  "car": {
    "battery":  { "pct": 89, "charging": false, "at": 1762800000 },
    "interior": { "c": 9.4, "at": 1762800000 },
    "exterior": { "c": 7.8, "at": 1762799940 },
    "tires": {
      "fl": { "kpa": 290.3, "at": 1762792800 }, "fr": { "kpa": 289.6, "at": 1762792800 },
      "rl": { "kpa": 288.9, "at": 1762771200 }, "rr": { "kpa": 291.6, "at": 1762771200 },
      "state": "optimal"
    },
    "drive": { "state": "parked", "since": 1762791000, "at": 1762799990 }
  }
}
```

Only `schema` and `written_unix` are always there. Every other key is left
out when it is unknown; the Pi never writes `null` or placeholder values.

| Key | Meaning |
| --- | --- |
| `schema` | Always `1` for this format. |
| `written_unix` | When the file was written, Unix seconds, by the Pi's clock (see [The Pi's clock](#the-pis-clock)). |
| `clock_synced` | `true` once the Pi's clock was corrected from a time server since it started; `false` before that. |
| `version` | The Sentry USB version, at most 64 characters. |
| `storage` | The Pi's backing-files filesystem (drive images and snapshots), in bytes: the same total, used and free space the Pi's own web page shows. |
| `drives` | Up to 8 entries, one per drive image that exists. `name` is one of `cam`, `music`, `lightshow`, `boombox`; `size_bytes` is the drive's size as the car sees it. |
| `cpu_temp_c` | Pi CPU temperature, °C, -40 to 125, one decimal. |
| `car` | Only when the Pi reads car data (Tesla BLE telemetry); otherwise left out entirely. |

`car`:

| Key | Meaning |
| --- | --- |
| `battery.pct` | State of charge, a whole percent 0 to 100. |
| `battery.charging` | `true` or `false`. |
| `interior.c` / `exterior.c` | Temperatures in °C, one decimal: interior -60 to 100, exterior -70 to 70. |
| `tires.fl` / `fr` / `rl` / `rr` | `kpa`: pressure in kPa, 0 to 1000, one decimal (kPa = psi × 6.894757). Each tire's `at` is when the car last heard that sensor. |
| `tires.state` | `optimal`, `check` or `unsafe`: the worst tire against the owner's psi bands (`TIRE_PSI_BANDS`). |
| `drive.state` | `parked` or `driving`. |
| `drive.since` | When the last drive ended. |
| `at` (any reading) | When that reading was taken, Unix seconds, by the Pi's clock. |

Never included: location, position, odometer, VIN, or display units (`units`).

### The Pi's clock

Most Pis have no clock battery. After starting up away from the internet,
the clock begins from the last time it saved and stays wrong until the Pi
reaches a time server, which can take a whole drive. The file is still
written then, and says so:

- `clock_synced: true`: the clock was corrected this boot; `written_unix`
  and every `at` can be trusted.
- `clock_synced: false`: they may be off by minutes to days.
- Missing (older Sentry USB versions): unknown; treat like `false`.

When `clock_synced` is not `true`, a reader can estimate the error itself:

1. Take the time the share recorded for the file (its modification time).
   On SMB, NFS and rsync it is set by the NAS when the file arrives, by the
   NAS's own clock.
2. `offset = file modification time − written_unix`.
3. Add `offset` to `written_unix` and to every `at` and `since` in the file.

The offset is only an estimate (it includes the few seconds the write took)
and only valid for this copy of the file: the next write may carry a
different error, until `clock_synced` turns `true`. rclone may give the
remote file the Pi's own time instead of the arrival time; there, compare
`written_unix` with the time the reader first saw the new content.
