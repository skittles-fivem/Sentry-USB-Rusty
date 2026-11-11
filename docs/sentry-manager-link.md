# Sentry USB ↔ Sentry Manager: the management link

Sentry Manager (on the NAS) manages a Sentry USB (the Pi in the car). From
Manager you can:

- see the Pi's status
- change its settings
- pull diagnostics and logs
- check for and install updates
- reboot it, start an archive, and so on

This file is the contract both sides build to. Both repositories keep a copy:
`docs/sentry-usb-link.md` in Sentry Manager and `docs/sentry-manager-link.md` in
Sentry USB.

## 1. Who connects to whom

The **Pi connects out to Manager**; Manager never connects to the Pi:

- The Pi is only home now and then, gets a new address from DHCP, and has no
  HTTPS of its own. Its web UI is plain HTTP, and the owner's rule is
  "passwords only over HTTPS".
- The owner enters Manager's address on the Pi once; nothing is filled in
  automatically, because Manager need not run on the archive server.
- No port is opened on the Pi, and the Pi's own web login is never used over
  the network by Manager.

**Port:** Manager's **device port, 10000 by default (`CAN_PORT`)**:

- It's the port the CAN Bridge already uploads to, using Manager's own TLS
  certificate.
- It serves only a small fixed set of device routes, never the admin UI on 9000.
- It listens only while a device feature is on: CAN uploads, or at least one
  Sentry USB (paired or pairing).

**Endpoint:** `wss://<manager host>:10000/api/sentryusb/link`, WebSocket over
TLS. TLS 1.2 is the minimum, and 1.3 is preferred.

The Pi pins Manager's certificate by its SHA-256 fingerprint (section 2).
Every key-mode handshake runs with the pin. A first pairing accepts whatever
certificate is served, but only inside the pairing window the owner opens on
the Pi by entering Manager's pairing code; the pair proof covers the
fingerprint the Pi saw and is keyed by the code as well as the web password,
and the certificate that pairs is then pinned.

## 2. Pairing: the Pi's username and password plus a one-time code

The Pi already has a web username and password: `WEB_USERNAME` and
`WEB_PASSWORD` from its setup. Pairing uses them once, together with a
one-time **pairing code** that Manager shows and the owner types on the Pi,
then both sides switch to a random link key. Manager never stores the Pi's
password, and neither side ever sends the password or the code over the link.

(Protocol v1 had the owner compare Manager's certificate fingerprint on the
Pi instead. Only 8 hex characters were shown large, which an attacker can
match by generating certificates, and the pairing proof was then keyed by the
web password alone, so it could be cracked offline. The code has 60 random
bits, is used once and never crosses the network. v1 is gone; no device ever
paired with it.)

1. **In Manager, on the Sentry USB page (in the main navigation) → Add a Sentry USB:**
   - Enter a name, plus the Pi's web username and password, then press
     Start pairing.
   - These go to Manager over Manager's HTTPS, like every password.
   - Manager derives the pairing key at once:
     `K = PBKDF2-HMAC-SHA256(password, salt = "sentryusb-link|" + username, 200000 iterations, 32 bytes)`,
     generates the pairing code, and derives the pair-mode secret
     `P = HMAC-SHA256(key = K, message = "pair-code|" + normalizedCode)`.
   - Manager keeps P (and the code, to show it) **in memory only**, for at
     most 15 minutes, and forgets the password at once.
   - It shows the code very large, monospace, in three groups (for example
     `K7QP-M2XD-9WRT`), with a copy button, the countdown and: "On the Sentry
     USB, open Settings → System → Sentry Manager, enter this Sentry
     Manager's address (the computer it runs on, port 10000, for example
     192.168.1.5:10000) and this code, then press Pair."
   - While this pairing slot is open, the device port is open.
2. **On the Pi, under Settings → System → Sentry Manager** (behind the Pi's
   web login):
   - Turn on "Managed by Sentry Manager".
   - Enter Manager's address (the field starts empty, with the hint
     `192.168.1.5:10000`; nothing is filled in automatically and the network
     is never scanned) and the pairing code (dashes, spaces and lower case
     are fine), then press **Pair**.
   - Pair (`POST /api/manager-link/pair` `{"address":"host[:port]","code":"..."}`,
     JSON only, like Unpair) saves the address and opens the Pi's 15-minute
     **pairing window**. The code is kept in memory only, for the window's
     life.
   - A Pi with no web password set cannot pair; the UI says to set one first.
   - A malformed code is refused with "That isn't a valid pairing code".
3. **The Pi connects** to the device port, and both sides prove they hold P
   (section 3).
   - The Pi only tries pair mode during the pairing window. Outside it, it
     only uses key mode, so nothing on the network can make a paired Pi fall
     back to pairing, and a Pi that was never paired never connects at all.
   - On a first pairing the Pi accepts whatever certificate is served. A man
     in the middle with its own certificate can't finish: the `fp` in the
     Pi's proof is its certificate's, so the real Manager refuses the proof,
     and it can't compute a proof of its own without P. Playing Manager
     outright, it gets a Pi proof keyed by P; cracking that needs the 60-bit
     code as well as the password, which isn't feasible.
   - Pairing again while paired keeps the saved fingerprint, so it only
     works with the same Manager. "Unpair" clears it.
   - While pairing it retries every 5 to 15 seconds while Manager can't be
     reached. When Manager **refuses** the pair proof (a wrong code or
     password), or has no slot open, the Pi closes its window at once and
     forgets the code: retrying would only use up Manager's 5 tries per slot.
   - On success, Manager generates a random 32-byte **link key**.
   - Manager sends it inside the TLS channel, and both sides save it.
   - Manager forgets P and the code. The Pi forgets the code and closes its
     window.
   - The Pi pins the certificate fingerprint it paired with and shows "Paired
     with Sentry Manager at <address>", the fingerprint small "for
     reference", and Unpair. Nothing has to be compared.
4. **Failure messages** (plain words, both UIs):
   - "Wrong pairing code or password": the same text for both, so neither
     is revealed.
   - "Pairing window expired" (the 15 minutes ran out, or Manager's slot
     closed).
   - A version mismatch says which side to update (section 7).

**The code:**

- Manager generates 12 characters from Crockford base32
  (`0123456789ABCDEFGHJKMNPQRSTVWXYZ`, 60 bits) with `crypto.randomInt`, and
  shows it as three groups of 4 joined by `-`.
- **Normalization on both sides before use:** uppercase; drop spaces and
  `-`; map `O` to `0`, and `I` and `L` to `1`. The result must be exactly 12
  characters from the alphabet, otherwise "That isn't a valid pairing code".
- **Lifetime:** one code per open slot, and a new slot gets a new code. The
  code is forgotten when the slot closes: success, cancel, 15 minutes, or 5
  failed proofs.
- **Never stored or logged:** not on disk, not in a database, not in a
  snapshot or status, and not in a log on either side. Manager shows it only
  to the admin who opened the slot (in the response to
  `POST /api/sentryusb/pair` and on the page while the slot is open); a
  watch-only account never sees it.

5. **Afterwards, every connection uses the link key** and never the password.
   - Changing the Pi's web password doesn't unpair it.
   - "Unpair" on either side deletes the key on that side, and the other side's
     next handshake fails. Unpair on the Pi while connected also closes the
     link with WebSocket code 4001 (`unpaired`, the code Manager uses), so
     Manager marks the Pi unpaired at once: a Pi without a key never connects
     again to fail a handshake.
   - A failed handshake is reported as "not paired" on both sides.

**Storage:**

- **Manager:** stores the link key with the device record in its database,
  which is in the data folder only the app can read. The key is never shown
  and never returned by any API.
- **Pi:** stores the link key and the pinned fingerprint in a root-only file
  (0600), next to its other secrets. It is not in the user-visible config.

## 3. Handshake: mutual, on every connection

Every message is one JSON text frame. Binary data travels as base64 inside JSON.

1. **Manager sends `hello`:**
   `{"t":"hello","v":2,"nonce":"<32 random bytes, hex>","fp":"<Manager cert sha256, hex>"}`
2. **The Pi replies with `auth`:**

   ```
   {"t":"auth","v":2,"id":"<Pi device id>","mode":"pair"|"key","nonce":"<32 random bytes hex>",
    "name":"<hostname>","version":"<Sentry USB version>",
    "proof":hex(HMAC-SHA256(S, "pi|" + managerNonce + "|" + piNonce + "|" + fp + "|" + id))}
   ```

   - `id` is a stable random id the Pi generates once and keeps.
   - `S` is P in pair mode (`P = HMAC-SHA256(K, "pair-code|" + normalizedCode)`,
     section 2) and the link key in key mode. In v1, pair mode used K itself.
   - `fp` is the fingerprint the Pi actually saw in the TLS session. A
     man-in-the-middle with another certificate therefore fails, even in pair
     mode.
3. **Manager checks the proof** in constant time and answers with `ok`:

   ```
   {"t":"ok","proof":hex(HMAC-SHA256(S, "mgr|" + piNonce + "|" + managerNonce + "|" + fp + "|" + id)),
    "linkKey":"<hex, pair mode only>"}
   ```

   - On failure Manager sends `{"t":"denied","why":"..."}` and closes.
   - Failures are rate-limited per address: at most 10 a minute.
4. **The Pi checks Manager's proof.** If it's wrong, the Pi closes and doesn't
   save anything.

**Encoding (both sides):** nonces, fingerprints, proofs and the link key are
exactly 64 **lowercase** hex characters; anything else fails. The fingerprint
is the SHA-256 of the DER certificate. The HMAC message is the UTF-8 string
shown, with the hex strings as text. The device `id` is 32 lowercase hex
characters (16 random bytes). K's password and username are used as their
UTF-8 bytes with no normalization. The pairing code is normalized as in
section 2 before P is derived, and `"pair-code|" + normalizedCode` is ASCII.
Test vectors are in Appendix A.

**Pi-side checks:**

- The `fp` in `hello` must equal the fingerprint the Pi saw, or the Pi closes
  (someone else terminated TLS).
- A `hello` with any `v` other than 2 is answered with
  `{"t":"denied","why":"version"}` and the connection closed.
- A `denied` instead of `hello` (rate limit, no slot) is accepted and reported.
- In pair mode, a `denied` after `auth` other than `rate limited`, `timeout`
  or `version` ends the pairing window: `no pairing open` is shown as
  "Pairing window expired", anything else (Manager says `wrong password`) as
  "Wrong pairing code or password".
- Handshake frames are at most 8 KiB, text, and JSON objects; anything else
  closes the connection.
- In key mode, a `linkKey` in `ok` is ignored.

**Timing and liveness:**

- Any frame before the handshake completes, other than the ones above, closes
  the connection.
- The handshake must finish within 10 seconds.
- After that, both sides ping every 30 seconds, and a connection silent for 90
  seconds is closed. The Pi sends WebSocket ping control frames, answers
  WebSocket pings with pongs, and also answers a JSON `{"t":"ping"}` with
  `{"t":"pong"}`. Any frame, control frames included, counts as not silent.
- Frames over 2 MiB are refused while reading, before any parsing.
- The Pi reconnects with backoff (5 s doubling up to 5 min, with jitter)
  whenever it's on a network. It reconnects at once when Wi-Fi comes up.
- Each Pi id has only one connection; a new one replaces the old.

## 4. After the handshake: requests from Manager, answers from the Pi

**Request, Manager → Pi:**
`{"t":"req","id":"<request id>","method":"GET"|"POST"|"PUT"|"DELETE","path":"/api/...","query":"a=1&b=2","body":<JSON or null>}`

**Answer, Pi → Manager:**
`{"t":"res","id":"...","status":200,"type":"application/json","body":<JSON>}`

- **Large or binary answers** (a diagnostics zip, a backup, a long log page) use
  `"bodyB64":"..."` instead of `body`.
- **Answers over 1 MiB** are split into chunks, each at most 1 MiB:
  `{"t":"chunk","id":"...","seq":0,"data":"<b64>"}`, ending with
  `{"t":"end","id":"..."}`.
  - In this case the `res` frame carries `"chunked":true` and no body.
  - At most 64 MiB is sent per answer. An answer cut at that limit, or one
    whose reading failed midway, ends with `{"t":"end","id":"...","error":"too large"|"read failed"}`;
    Manager must treat it as failed.
  - Chunk `data` is at most 1 MiB once decoded, so a frame is up to about
    1.4 MiB: Manager's maximum frame size must be at least 2 MiB.
  - A non-chunked `res` uses `body` when the handler answered JSON, otherwise
    `bodyB64` (logs and diagnostics are `text/plain`, so they come as
    `bodyB64`). An empty answer has `"body": null`.
- **Request limits:** Manager allows at most 8 requests in flight per Pi, and
  times each one out after 60 seconds. Update and setup runs answer at once and
  are then polled through their status routes.

**How the Pi serves a request:**

- It runs the request **in-process against its own API router**, the same
  handlers its web UI uses, as a trusted local caller.
- It runs only paths and methods on the allowlist below. Anything else is
  answered with `{"status":403,"body":{"error":"Not allowed over the Sentry Manager link"}}`.

**Status pushed by the Pi, without being asked:**

- Every 30 seconds while connected, and at once after a change, the Pi sends
  `{"t":"status","body":<the same JSON as GET /api/status, plus "car">}`.
  "A change" is any successful non-GET request over the link, or a change in
  the car's readings (below).
- Manager keeps the last one, with its time, so the page shows status
  instantly and a "last seen" time when the Pi is away.

### The car (`car` in the status body)

Sentry USB test.31 and later add a `car` object: the car's battery,
temperatures, tires and whether it is parked, from the Tesla BLE telemetry the
Pi already reads (the same readings as its own dashboard's car card). It is
only what the Pi has: there is no new request to the car, and never the car's
place, position, odometer or VIN.

```json
"car": {
  "battery":  {"pct": 89, "charging": false, "at": 1762800000},
  "interior": {"c": 9.4, "at": 1762800000},
  "exterior": {"c": 7.8, "at": 1762799940},
  "tires": {
    "fl": {"kpa": 290.3, "at": 1762792800}, "fr": {"kpa": 289.6, "at": 1762792800},
    "rl": {"kpa": 288.9, "at": 1762771200}, "rr": {"kpa": 291.6, "at": 1762771200},
    "state": "optimal"
  },
  "drive": {"state": "parked", "since": 1762791000, "at": 1762799990},
  "units": {"temp": "F", "pressure": "psi"}
}
```

| Field | Type, unit, range | Notes |
|---|---|---|
| `battery.pct` | number, %, 0 to 100 | state of charge |
| `battery.charging` | boolean, optional | true while charging; left out when the Pi doesn't know |
| `interior.c`, `exterior.c` | number, °C, -60 to 100 inside, -70 to 70 outside | one decimal |
| `tires.fl`, `.fr`, `.rl`, `.rr` | `{kpa}`: number, kPa, 0 to 1000 | front left, front right, rear left, rear right; one decimal (the Pi reads psi: kPa = psi × 6.894757) |
| `tires.state` | `"optimal"`, `"check"` or `"unsafe"`, optional | the worst tire against the owner's bands on the Pi (`TIRE_PSI_BANDS`, default 28, 36, 45, 50 psi), as the Pi's dashboard shows it ("Optimal", "Check tires", "Unsafe") |
| `drive.state` | `"parked"` or `"driving"` | driving: a gear reading at most 2 minutes old says D, R or N; parked: it says P, or there is none and a drive has ended |
| `drive.since` | integer, Unix seconds, optional | parked: when the last drive ended |
| `units.temp`, `units.pressure` | `"F"`/`"C"`, `"psi"`/`"bar"` | how the Pi's own dashboard shows them (its TEMPERATURE_UNIT and PRESSURE_UNIT); display only, the values above stay °C and kPa |
| `at` (in each reading) | integer, Unix seconds, optional | when that reading was taken; for a tire, when the car last heard its sensor (parked, the sensors sleep and the car keeps the last pressure) |

- **Everything is optional.** A reading the Pi doesn't have is left out, and a
  Pi with no reading at all (no Tesla BLE set up) sends no `car`.
- **How often:** with every status push (30 s), and the Pi checks the readings
  every 15 s and pushes status at once when a value changed (a newer reading
  of the same value is not a change). So a car change sends at most one
  extra status every 15 s.
- **Manager checks it** (server/sentryUsbCar.js): only the keys above are
  kept, each with its type and range; a reading with a wrong type, an
  out-of-range value or a bad time (before 2017, or more than a day ahead
  of Manager's clock) is dropped, the others are kept; a `car` with no valid
  reading left is not kept. It is stored with the rest of the status and
  shown on the Status tab's Car card to every signed-in account, watch-only
  ones included. Manager has no unit settings of its own, so the card uses
  `units`, else °F and psi.

## 5. The allowlist (Pi side, deny by default)

**Allowed:**

| Area | Routes |
|---|---|
| Status | `GET /api/health`, `/api/status`, `/api/status/storage`, `/api/profile`, `/api/config`, `/api/memory`, `GET /api/system/version`, `/api/system/clock-status`, `/api/system/rtc-status`, `/api/system/ping`, `/api/backingfiles/free-space` |
| Settings | `GET/PUT /api/setup/config`, `GET /api/setup/status`, `GET /api/setup/phases`, `POST /api/setup/run`, `POST /api/setup/test-archive`, `POST /api/setup/preflight`, `GET/PUT /api/config/preference`, `GET/PUT /api/notifications/settings`, `GET /api/system/keep-accessory-config` and its PUT, `GET/PUT /api/system/update-source` |
| Diagnostics | `GET /api/diagnostics`, `POST /api/diagnostics/refresh`, `POST /api/diagnostics/download`, `GET /api/system/health-check`, `GET /api/storage/health`, `GET /api/logs/{name}`, `/page`, `/tail` |
| Updates | `POST /api/system/check-update`, `POST /api/system/update`, `GET /api/system/update-status`, `GET /api/system/check-internet` |
| Actions | `POST /api/system/reboot`, `/shutdown`, `/trigger-sync`, `/cancel-archive`, `/toggle-drives`, `/gadget-enable`, `/gadget-disable` |
| Snapshots | `GET /api/snapshots`, `GET /api/snapshots/live`, `DELETE /api/snapshots/{id}` |
| Backups | `POST /api/system/backup`, `GET /api/system/backups` |

Removed from the first draft of this table:

- `GET /api/logs`: no such route exists on the Pi (the log names are fixed:
  `archiveloop`, `manager-link`, `setup`, `diagnostics`, `syslog`, `kern`,
  `auth`, `daemon`, `sentryusb`, `sentryusb-ble`).
- `GET /api/system/backup/{date}`: a backup holds the whole sentryusb.conf with
  every password, the Pi's SSH private key and the Tesla BLE private key,
  which unlocks the car. It never leaves the Pi over the link. Backups made
  with `POST /api/system/backup` are written to the archive share, where
  Manager can already read them.

**Never allowed:**

- the terminal
- `/api/auth/*`
- `/api/files/*` writes and deletes
- SSH key routes
- backup restore
- community admin routes
- BLE pairing and reset
- the Pi's own Sentry Manager routes (`/api/manager-link*`)
- anything not listed above

Each side keeps its own copy of this list. Manager offers only the allowed
routes, and the Pi enforces the list.

**Secrets in settings:** the setup config contains passwords (Wi-Fi, archive
share, the web password itself), and the Pi always masks them over the link.

- **Read:** a secret field comes back as `""` with a companion
  `"<FIELD>__set": true` when a value exists.
- **Write:** an empty or missing secret leaves the stored one unchanged, and a
  non-empty value replaces it.
- **The web login can't be changed over the link.** `WEB_USERNAME` and
  `WEB_PASSWORD` are read-only there (the password is masked like every
  secret). Whoever sets the web password can sign in to the Pi's web UI,
  which has a root terminal, so a compromised Manager could take over the
  Pi. Change it on the Pi itself.

### 5a. How the Pi checks requests (Sentry USB side)

- **Canonical form first.** `path` must be `/api/` followed by segments of
  `[A-Za-z0-9._-]`, none `.` or `..`, no empty segment, no trailing `/`, at most
  256 characters. No `%`, `\`, `?`, `#`, spaces or control characters. `query`
  may hold only `[A-Za-z0-9._~=&+,:-]` and complete `%XX` escapes, at most 2048
  characters. `method` must be exactly `GET`, `POST`, `PUT` or `DELETE`. `{id}`,
  `{name}` and `{date}` match one such segment. Anything else gets the 403.
- **No headers from Manager.** The Pi builds the request itself, with only
  `Content-Type: application/json` when there is a body, so method-override
  headers have nothing to ride on. It carries no client address, so the Pi's
  loopback-only routes stay closed.
- **Request ids** are 1 to 64 characters of `[A-Za-z0-9._:-]`. A `req` with an
  invalid id, or the id of a request still running, is ignored. At most 8
  requests run at once; a ninth is answered `429` at once.
- **`PUT /api/setup/config` is a full replacement** of the active settings, as
  in the Pi's own wizard: send every active key (from the masked `GET`), with
  `""` for secrets you don't change. `<FIELD>__set` companions are ignored. On
  top of the secret rules:
  - Only listed keys may change over the link: Wi-Fi, hotspot, hostname, time
    zone, archive settings, archive verification, notification providers,
    temperature settings and the like. Every other key is written back exactly
    as stored, whatever was sent; sending it back unchanged is fine, sending a
    different value is refused with `400` naming the key. Read-only keys include
    the web login, `NOTIFICATION_COMMAND_*`, `INSTALL_USER_REQUESTED_PACKAGES`,
    `REPO`, `BRANCH`, `RCLONE_FLAGS`, drive sizes, `USE_EXFAT`, `DATA_DRIVE`,
    `INCREASE_ROOT_SIZE`, `SAMBA_*`, `SENTRY_MANAGER_*`, and any key the Pi
    doesn't know (a new key could be `PATH` or `LD_PRELOAD`, since
    sentryusb.conf is exported into root shells).
  - A stored secret can't follow a changed destination: changing
    `ARCHIVE_SYSTEM`, `ARCHIVE_SERVER`, `SHARE_USER` or `SHARE_DOMAIN` needs
    `SHARE_PASSWORD` sent again, `SSID` needs `WIFIPASS`, and likewise for the
    Matrix, Gotify, ntfy and AWS credentials. Otherwise `400`. This stops the
    stored password being sent to a server of someone else's choosing.
  - Values that become command arguments (hosts, users, paths, share names,
    SSIDs, hostname, time zone, VINs) can't start with `-`; `ARCHIVE_SERVER` and
    `RSYNC_SERVER` must be host names or addresses; `RSYNC_USER` is a plain user
    name; `SHARE_USER`, `SHARE_DOMAIN`, `CIFS_VERSION` and `CIFS_SEC` can't hold
    `,` or `=` (mount options). No value may hold control characters.
- **`POST /api/setup/test-archive`** gets the same value checks. An empty
  secret is filled with the stored one only when the destination is the stored
  one.
- **`PUT /api/system/update-source`** only accepts `{"repo": ""}` or the
  official repository, which both mean "official". Any other repository is
  `403`: it would install someone else's binaries as root. Changing to a test
  repository is done on the Pi.
- **`POST /api/system/update`** takes no body or `{"version": "<tag>"}`, the tag
  being 1 to 64 characters of `[A-Za-z0-9._+-]` without `..`.

## 6. Manager's own routes (admin only, main port 9000)

| Route | What it does |
|---|---|
| `GET /api/sentryusb` | The devices, each with: id, name, paired, online, last seen, version, last status. Never the key. |
| `POST /api/sentryusb/pair` | Takes `{name, username, password}`, opens a 15-minute pairing slot and answers with its pairing code (section 2), shown only to that admin. Fresh admin password confirmation is needed, as for other risky actions. |
| `DELETE /api/sentryusb/pair` | Cancels the slot. |
| `PATCH /api/sentryusb/{id}` | Renames the device. |
| `DELETE /api/sentryusb/{id}` | Unpairs: deletes the key and closes the link. |
| `ANY /api/sentryusb/{id}/proxy/<allowlisted path>` | Sends one request over the link and returns the Pi's answer. It is checked against Manager's copy of the allowlist. Writes need an admin, and reads are allowed for any signed-in account. |

**Logging:** Manager logs each action it sends (who, which Pi, which route),
never bodies containing secrets.

## 6a. Queued actions (Manager side, no new Pi routes)

While the Pi is away, an admin can queue actions in Manager. They run over the
normal link the next time the Pi connects, using the same allowlisted routes.

- **What can be queued:**
  - install update: check, then update if newer
  - reboot
  - sync now
  - fetch diagnostics: refresh, then download
  - create a backup (it is written to the archive share; the Pi never sends
    backups over the link, section 5)
  - apply settings changes
- **Settings changes:** a queued change stores **only the changed fields**. On
  connect, Manager reads the current config, applies just those fields, saves,
  and re-runs setup if asked. A change made on the Pi meanwhile to another
  field is not overwritten. If the same field was changed on the Pi too,
  Manager skips that field and reports it.
- **Never a password:** password changes need the Sentry USB online. A queued
  change refuses every secret field (the list Manager masks, section 5) and
  every field whose secret must be sent with it (5a: a new SSID needs
  WIFIPASS, a new archive server SHARE_PASSWORD). A mix saved while the Pi is
  away queues the other fields and leaves those untouched on the page, with
  "Password changes need the Sentry USB online. Save them while it's
  connected." Nothing secret is stored in the queue.
- **Order and timing:**
  - Actions run in the order queued, one at a time, after the handshake and
    the first status push.
  - Update and reboot wait until the Pi's status shows no archive or upload
    running, checking for at most 2 hours. Then they're reported as "waited
    too long, still queued".
  - Reboot and update are always last in a batch.
- **Files:** fetched diagnostics and backups are saved in Manager's data folder,
  so they can be downloaded later. The newest 5 per Pi are kept.
- **Persistence:** queued actions survive a Manager restart. Each runs at most
  once: it's marked "running" before it's sent, and a restart in the middle
  reports it as "interrupted", not run twice.
- **Expiry and cancelling:** an action expires after 14 days, which is
  adjustable, and can be cancelled while queued.
- **Results:** each result is kept: done, failed with the Pi's error, expired or
  cancelled. Manager raises a notification when a batch finishes, using its
  existing notification system.
- **Who can queue:** queueing and cancelling are admin-only, with the same rules
  as the proxy writes.

## 6b. Security audit log (both sides)

Every connection and attempt is logged, so a hijack attempt or an unexpected
network shows up.

### What the Pi tells Manager about its network

Right after the handshake, before any status, the Pi sends one frame:

```
{"t":"net","ssid":"<Wi-Fi name or null>","iface":"wlan0|eth0|usb0|...","ip":"<its own address>","gw":"<gateway address>"}
```

It's sent again whenever any of these change. Manager treats every value as
untrusted text: it escapes them and never uses them in decisions about who
the Pi is. They only add detail to the log.

### Manager's log

- **Every device-port event is recorded:**
  - a connection opened
  - a handshake failed, with its kind: bad proof, unknown Pi id, wrong
    version, pairing with no open slot, timed out, garbage or oversized frame,
    or rate limited
  - a pairing succeeded or failed
  - a Pi connected or disconnected, with the source address and port
  - a Pi id replaced by a newer connection
  - unpairing
  - every proxied or queued action: who, which Pi, which route, and the result
- **Flags:**
  - **"New address":** a paired Pi connects from a source IP it hasn't used in
    the last 90 days, or a new Wi-Fi name or gateway appears in its net frame.
  - **"Outside home":** the source IP is not a private or loopback address, or
    the Wi-Fi name or gateway differs from the Pi's usual home network. The
    home network is the one seen most in the last 30 days, or the one set by
    hand in the device settings.
  - **"Possible hijack attempt":** a failed handshake using a paired Pi's id, a
    burst of failed proofs, a wrong-fingerprint report from the Pi, or a
    pairing attempt while no slot is open.
- **Notifications** use Manager's existing notification system and rules:
  - a possible hijack attempt is a warning, and is always on
  - a new address or outside home is a notice, which can be turned off per Pi
- **Storage:**
  - The log is kept 180 days, adjustable.
  - A cap on rows means a flood of attempts can't fill the disk. Repeated
    identical failures from one address are counted on one row with a count
    and the last time.
  - It's included in Manager's log search and support bundle, never with
    secrets.
- **Display:** a "Security log" tab on the Sentry USB page, with filters (all,
  attempts, new or outside, actions) and CSV export.

### The Pi's log

The Pi keeps a security log too, in its own log folder with a 1 MB cap and
rotation, shown in its Sentry Manager settings section.

On the Pi: JSON lines in `/mutable/sentryusb-manager-link.log`, rotated to
`.log.1` at 512 KiB (so both stay within 1 MB). Manager can read it over the
link as `GET /api/logs/manager-link`. Each line is
`{"at":<unix>,"level":"info"|"warn","kind":"...","msg":"...",...}` with kinds
`pairing_open` (with `address`), `pairing_closed` (the window expired, or
pairing failed: wrong code or password, no slot, version), `paired` (with the
fingerprint pinned), `unpaired`, `connected`, `disconnected`,
`attempt_failed` (`mode` is `pair` or `key`), `fingerprint_mismatch` (warn;
also when a server's `hello` names another certificate than the one it
presented), `bad_proof` (warn), `network`, `request`, `refused` (warn) and
`alert_sent`. Text from the network is clipped to 200 characters with control
characters removed. No key, K, P, pairing code, password or request body is
ever logged.

- **What it records:**
  - every connection attempt to Manager
  - the pairing window opening and closing, and why it closed
  - a certificate fingerprint mismatch: a possible fake Manager, logged as a
    warning and shown prominently with the fingerprint seen
  - a Manager proof that failed
  - pairing and unpairing
  - every request served over the link, with method, path and status
  - every request refused by the allowlist
  - the network it was on (Wi-Fi name and gateway)
- **Reports to Manager:** a fingerprint mismatch or a failed Manager proof is
  also sent to Manager the next time a good connection is made, using
  `{"t":"alert","kind":"fingerprint"|"proof","seen":"...","at":<unix>}`.
  Manager logs these as possible hijack attempts.
  - `seen` is the SHA-256 fingerprint (64 lowercase hex) of the certificate
    that was presented; `at` is when it was first seen.
  - Alerts are sent after `net` and before the first `status`, then cleared.
    The Pi queues at most 20, once per kind and fingerprint, so a fake
    Manager can't flood it.
- **The `net` frame:** `ip` is the Pi's own address on this connection, `iface`
  and `gw` come from its IPv4 default route, and `ssid` is set only when the
  link runs over Wi-Fi (else `null`). The Pi checks for changes every 30 s.

## 7. Versioning

- `v` in `hello` and `auth` is **2** (pairing code). Version 1 (owner
  compares the fingerprint; pair mode keyed by K) is not accepted: no device
  ever paired with it, so there is no migration.
- A side that gets any other `v` (1, or a newer one it doesn't know) answers
  `{"t":"denied","why":"version"}` and closes. Both UIs then say to update
  the other side: Sentry USB to test.30 or later, or Sentry Manager to the
  newest version.

## Appendix A. Test vectors

Both sides must reproduce these exactly. They are checked by the Pi's unit
tests (`crates/manager_link/src/handshake.rs`) and were also computed
independently with Python's `hashlib.pbkdf2_hmac` and `hmac` (P included).

| Input | Value |
|---|---|
| username | `admin` |
| password | `correct horse battery staple` |
| salt | `sentryusb-link|admin` |
| iterations | 200000, 32-byte output |
| K | `42dbcc8ba07c3b03314a2cec3c9cae2adf011049eb445394ef1d3a11b94f6205` |
| managerNonce | `000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f` |
| piNonce | `202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f` |
| fp | `61ecb6872005d76f9c85b9abee8c635c413381103bf5df251ec263fc331d6c27` (SHA-256 of the ASCII text `sentry-manager test certificate`) |
| id | `0123456789abcdef0123456789abcdef` |
| linkKey | `404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f` |

**Pair mode (protocol v2, S = P):**

| Input | Value |
|---|---|
| pairing code as typed | `K7QP-M2XD-9WRT` (`k7qp m2xd 9wrt` normalizes the same) |
| normalized code | `K7QPM2XD9WRT` |
| P, `HMAC(K, "pair-code|" + normalizedCode)` | `ae8f34df1f97c33455555600e278f5e25fa728f6cb4d612e0f4b6befe88cb7cf` |

- Pi proof, `HMAC(P, "pi|" + managerNonce + "|" + piNonce + "|" + fp + "|" + id)`:
  `a81633f8e74b24df86d3b5064bab2a7cd6705f884f8cf1d3baef7347e1ace662`
- Manager proof, `HMAC(P, "mgr|" + piNonce + "|" + managerNonce + "|" + fp + "|" + id)`:
  `d50cde97a5b18afcc5e8221803b93e8dc9163a9ec5bddf3e06aa04436ecbf6f0`

(The v1 pair-mode proofs, keyed by K, were `1425df08…bbcf` and `2f864f52…ce1e`;
a v2 side never accepts them.)

**Normalization cases:** `k7qp-m2xd-9wrt`, `K7QPM2XD9WRT` and
` k7qp m2xd 9wrt ` give `K7QPM2XD9WRT`; `OOOO-IIII-LLLL` gives `000011111111`;
`K7QP-M2XD-9WR` (11), `K7QP-M2XD-9WRTX` (13), `K7QP-M2XD-9WRU` (U isn't in the
alphabet) and `K7QP_M2XD_9WRT` are invalid.

**Key mode (S = linkKey):**

- Pi proof: `852e6aa89c5f0fd2bd95f2ab6471d17353ffa98b3782d8b014db20c9bf865e50`
- Manager proof: `cc691f70debff4e37baa54f32bdd250a5fcdfa8efcffeca8f1c7de92a2c54bef`

**Fingerprint display:** both UIs show fingerprints as 16 groups of 4
uppercase hex digits, for example
`61EC B687 2005 D76F 9C85 B9AB EE8C 635C 4133 8110 3BF5 DF25 1EC2 63FC 331D 6C27`.

## Sentry Manager's side: details both sides must match

What Manager does where the sections above leave a choice (server/sentryUsbLink.js,
server/sentryUsbQueue.js, server/sentryUsbAudit.js, web/sentryUsbLink.js).

**Using it.** Every interface has a **Sentry USB** page, in the main navigation
in the place Policies had (after Encoder; Policies moved further along): the guided "Add a Sentry USB" steps, the pairing code (large, in
three groups, with a copy button and the countdown) and the reason when pairing
fails ("Wrong pairing code or password", the same for both; "Pairing window
expired"; "The Sentry USB needs updating"), the device list (online, or away with when it was last
seen), and per device the tabs Status, Settings, Diagnostics, Logs, Updates,
Actions, Snapshots, Backups, Waiting for the Pi, Security log, and Name and unpair.
While a Pi is away, Update, Reboot, Sync, Diagnostics and Backup become
"when home" buttons, and Settings offers "Save and apply when home".

**Encodings.** Appendix A's vectors, P and the normalization cases
included, are checked by Manager's `test/sentryUsbLink.test.js`.

- `fp` is the SHA-256 of the DER certificate the device port served on *that*
  TLS connection, as **64 lowercase hex digits, no colons**. People see it as
  16 groups of 4 upper-case hex digits (Appendix A), in four rows.
- `nonce`s are 64 lowercase hex digits. Manager refuses a Pi nonce equal to its own.
- `proof` is 64 lowercase hex digits; the HMAC key `S` is the raw 32 bytes
  (P in pair mode, or the link key decoded from hex), and the message is UTF-8.
- K: password and `"sentryusb-link|" + username` as UTF-8 bytes, exactly as
  typed: nothing is trimmed or normalized.
- `id`: Manager accepts 8 to 128 characters of `A-Z a-z 0-9 _ -` (the Pi's are
  32 lowercase hex). Its request ids are 24 lowercase hex digits.
- The link key is 32 random bytes, never all zero.
- An `end` frame with `"error"` fails that request (502).
- `v` must be a number; anything else is `bad message`.

**`denied` reasons** (the `why` Manager sends): `not paired` (key mode: unknown
id or wrong key), `wrong password` (pair mode: the proof did not match P, so a wrong
pairing code or a wrong password, which are not told apart; it also covers a
man in the middle with another certificate; the name stays as in v1), `no pairing open`,
`version`, `timeout` (no `auth` within 10 s), `bad message`. After `denied`
Manager closes with WebSocket code 1008.

**Close codes.** 4000 replaced by a newer connection for the same id; 4001
unpaired (from either side: a Pi unpaired on itself closes with 4001, and
Manager then forgets it, its queue too, logs it and raises a notice); 1001 silent for 90 s or Manager shutting down; 1008 denied or a
protocol violation; 1009 a frame over the limit; 1002 an unmasked or malformed
frame; 1003 a binary frame; 1007 text that is not UTF-8.

**Liveness.** Manager sends a WebSocket **ping control frame** every 30 s and
closes a connection it has heard nothing from (no frame at all) for 90 s. It
answers pings with pongs, and also answers a JSON `{"t":"ping"}` with
`{"t":"pong"}`.

**Limits.** Before the handshake a frame may be at most 16 KiB; after it 2 MiB
(a 1 MiB chunk in base64 inside JSON fits). Sizes are checked from the frame
header, before the payload is read. A chunk decoding to more than 1 MiB, chunks
out of order, or a second handshake closes the link (1008); more than 20 frames
answering request ids Manager never sent close it too. An answer over 64 MiB
fails that request (502). Statuses over 256 KiB are ignored. At most 6
connections per address and 256 in all on the device port (shared with the CAN
Bridge), and a connection that sends nothing for 15 s is closed. After 10
failed handshakes in a minute an address gets HTTP 429 on the upgrade.

**Pairing slot.** One slot at a time (a new one replaces it, with a new code).
It closes after the first success, after 15 minutes, or after 5 wrong proofs
(any address), and forgets P and the code. A `version` refusal while it is open
is shown on the page and does not count as a wrong proof.
Pairing an id that is already paired replaces its key (the admin typed that
Pi's password).

**Requests.** `query` is passed raw, with the Pi's rule: only
`A-Z a-z 0-9 . _ ~ = & + , : -` and whole `%XX` escapes, at most 2048 characters; `body` is the browser's JSON
(or `null` for GET). The Pi's `type` is reduced to one of `application/json`,
`text/plain`, `application/zip`, `application/gzip`, `application/x-tar`,
`application/octet-stream` before it reaches a browser. A Pi `401` reaches the
browser as `502` (it is not Manager's sign-in).

**Section 6, in detail.** In `GET /api/sentryusb` the address a Pi connects
from and the networks it reports are shown to admins only, and the slot's
`pairing.code` is there only for the admin who opened the slot (as in the
answer to `POST /api/sentryusb/pair`). Of the proxied reads, the Pi's logs,
diagnostics, setup and notification settings, update source, keep-accessory
config and backups list are for admins only (as Manager's own logs are); the
other reads are allowed for any signed-in account.

**Manager's own routes, beyond section 6:**

| Route | Who | What |
|---|---|---|
| `GET /api/sentryusb/:id/last-config` | admin | the last setup config read from it (masked), for editing while away |
| `GET/POST /api/sentryusb/:id/queue`, `DELETE /api/sentryusb/:id/queue/:qid` | read: signed in; write: admin | queued actions (6a) |
| `GET /api/sentryusb/:id/files/:name` | admin | a fetched diagnostics report |
| `GET /api/sentryusb/security`, `GET /api/sentryusb/security.csv`, `PUT /api/sentryusb/security/settings` | admin | the security log (6b) |
| `PATCH /api/sentryusb/:id` | admin | also `{notifyNetwork, home: {ssid, gw}}` |

Fresh password confirmation is needed for: pairing, unpairing, proxied
`PUT /api/setup/config`, `POST /api/setup/run` and `PUT /api/system/update-source`,
and queueing settings changes. Manager's allowlist matches the Pi's final one:
no `GET /api/logs` (the page lists the Pi's fixed log names) and no
`GET /api/system/backup/{date}` (backups are created, never fetched), and
`/api/manager-link*` is refused.

**Secrets in settings.** Manager also masks every field the Pi counts as a
secret (its `is_secret_key`: `PASS`, `TOKEN`, `SECRET`, `…_KEY`, `…_KEY_ID`,
`…WEBHOOK_URL`, `NTFY_URL`, `IFTTT_KEY`) in `GET /api/setup/config` before it
reaches a browser or is kept. A Pi that sent
any such value unmasked is marked `__unmasked: true`, and the page will not
save its settings (an empty field would wipe the secret on a Pi that does not
treat empty as "keep"). Masked fields may come as `"<KEY>__set": true` at the
top level of the config object. The editor shows keys off the Pi's settable
list (section 5a) as "on the Pi only" and never changes them; before saving or
queueing it asks for the secret a changed destination needs (5a; queued, that
field waits for the Pi to be online with its secret), and a `400`
from the Pi that names a key outlines that field. Queueing checks the same two
rules on the server.

**Queued actions (6a).** Archive busy is read from the last status: any of
`archiving`, `archive_running`, `archive_work_running`, `uploading`,
`upload_running` true (or `"yes"`), or `archive.running`/`archive.active`.
The Pi's status sends `archiving` (archive work running: copying,
uploading, verifying), so update and reboot wait while it is true. Settings changes store `{from, to}` per field
(`null` = commented out), never a secret: `POST …/queue` holds those back and
answers `held` (the field names) and `note`; with nothing else left it is a
`400` (`SECRET_ONLINE`). A queue saved by an older Manager has its secret values
dropped at start-up. The PUT sends every active line, with secrets empty
(keep), plus the changes, since the Pi's PUT replaces the whole config.
The Updates tab sends `{"version": latest_version}` with Install when the tag
is a plain version tag (else no body); the queued update posts no body (latest
release). A queued backup is created on the Pi only (written to the archive
share); it is not fetched. `trigger-sync` answering 409
counts as done ("an archive was already running").

**Security log (6b).** Kept in its own SQLite table (`sentryusb_audit`),
180 days by default (7 to 3650), at most 20,000 rows. Failures, `net` and
`open` rows with the same kind, address, Pi, detail and flags within an hour
share a row (count, last time). A burst is 3 failed proofs (bad key or wrong
password) from one address within 10 minutes. A possible hijack attempt raises
a warning notification of kind `security`, which the notification rules
cannot turn off; new address and outside home raise an `info` notice of kind
`sentryusb`, which each Pi's Security log settings can turn off. Reads (GET)
through the proxy are not logged there (the page polls); every change, every
backup download and every queued action and its result are. Each row also goes
to Manager's log (log search) and the newest 1000 to the support bundle as
`sentryusb-security.csv`, with addresses replaced. CSV cells starting with
`= + - @`, tab or CR get a leading `'`.

**Where the key and the code are not.** No log line, support bundle,
security log, database row or snapshot holds a pairing code (it lives only in
the slot, in memory), and no API answer holds it except for the admin who
opened the slot. No API answer, log line, support bundle, security
log or database snapshot holds a link key: a snapshot drops the keys (a
restored one shows the Pi unpaired) and the queue (a restored one would run
actions again that already ran).

**Where this differs from the first draft.** The Pi's final contract above
replaced the draft: no log list, no backup download, the web login read-only
over the link, settable keys only, update-source and update body rules, the
Pi's pairing window, and the fingerprint shown in groups of 4. Manager follows
all of it.
