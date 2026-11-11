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
Every handshake, pairing included, runs with a pin. A Pi with no fingerprint
saved only reads the certificate's fingerprint (TLS, then close) and pins it
once the owner confirms it on the Pi (section 2, step 3).

## 2. Pairing: the Pi's username and password, never stored by Manager

The Pi already has a web username and password: `WEB_USERNAME` and
`WEB_PASSWORD` from its setup. Pairing uses them once, then both sides switch to
a random link key. Manager never stores the Pi's password, and neither side ever
sends it over the link.

1. **On the Pi, under Settings → Sentry Manager:**
   - Turn on "Managed by Sentry Manager".
   - Enter Manager's address: the computer it runs on, with port 10000.
     Nothing is filled in automatically and the network is never scanned.
   - A Pi with no web password set cannot pair; the UI says to set one first.
2. **In Manager, under Settings → Sentry USB → Add a Sentry USB:**
   - Enter a name, plus the Pi's web username and password.
   - These go to Manager over Manager's HTTPS, like every password.
   - Manager derives the pairing key at once:
     `K = PBKDF2-HMAC-SHA256(password, salt = "sentryusb-link|" + username, 200000 iterations, 32 bytes)`.
   - Manager keeps K **in memory only**, for at most 15 minutes, and forgets
     the password at once.
   - While this pairing slot is open, the device port is open.
3. **The owner confirms Manager's fingerprint on the Pi** (first pairing
   only, when the Pi has no fingerprint saved):
   - The Pi makes a TLS connection to Manager's address only to read the
     certificate's SHA-256 fingerprint, then closes it: no WebSocket upgrade,
     no `auth`, no proof. Manager sees a TLS connection that ends after the
     TLS handshake, and needs nothing new for it.
   - The Pi's Sentry Manager settings show that fingerprint with "Check this
     matches the fingerprint on Sentry Manager's Sentry USB page", the first
     and last groups of four shown large, and an "It matches, pair" button.
   - Only after that tap does the Pi pin the fingerprint (in memory, for this
     pairing) and run the pair handshake. A server with any other
     certificate fails TLS against the pin before anything is sent, so it
     never gets a proof to guess the web password against offline.
   - The confirm route (`POST /api/manager-link/confirm`
     `{"fingerprint":"<the one shown>"}`) is behind the Pi's web login, like
     Pair and Unpair, and takes only a JSON body. It is refused if the
     fingerprint sent isn't the one being shown (it changed after the page
     loaded), or the pairing window has closed.
   - If the fingerprint can't be read (Manager's device port isn't open yet,
     a wrong address), the page says so with the reason and keeps trying
     every 5 to 15 seconds.
   - If the certificate presented when pairing isn't the confirmed one, the
     Pi sends nothing, drops that pin, and shows the new fingerprint flagged
     "changed after you confirmed it", to be checked again.
   - The confirmed fingerprint is not written to the link file; only a
     successful pairing saves a pin.
4. **The Pi connects** to the device port, and both sides prove they hold K
   (section 3).
   - The Pi only tries pair mode during a 15-minute pairing window that the
     owner opens on the Pi: turning the link on, or pressing "Pair". Outside
     it, it only uses key mode, so nothing on the network can make a paired
     Pi fall back to pairing, and a Pi that was never paired doesn't offer its
     pairing proof to whoever answers at Manager's address.
   - While pairing it retries every 5 to 15 seconds, so pairing completes
     within seconds of the owner adding the Pi in Manager (and stays under
     Manager's 10 failures a minute).
   - "Pair again" on an already paired Pi keeps the saved fingerprint, so it
     can only re-pair with the same Manager. "Unpair" clears both.
   - On success, Manager generates a random 32-byte **link key**.
   - Manager sends it inside the TLS channel, and both sides save it.
   - Manager forgets K.
   - The Pi saves Manager's certificate fingerprint (the confirmed one) and
     shows it in its UI, and Manager shows its own fingerprint, so you can
     compare the two.
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
   `{"t":"hello","v":1,"nonce":"<32 random bytes, hex>","fp":"<Manager cert sha256, hex>"}`
2. **The Pi replies with `auth`:**

   ```
   {"t":"auth","v":1,"id":"<Pi device id>","mode":"pair"|"key","nonce":"<32 random bytes hex>",
    "name":"<hostname>","version":"<Sentry USB version>",
    "proof":hex(HMAC-SHA256(S, "pi|" + managerNonce + "|" + piNonce + "|" + fp + "|" + id))}
   ```

   - `id` is a stable random id the Pi generates once and keeps.
   - `S` is K in pair mode and the link key in key mode.
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
UTF-8 bytes with no normalization. Test vectors are in Appendix A.

**Pi-side checks:**

- The `fp` in `hello` must equal the fingerprint the Pi saw, or the Pi closes
  (someone else terminated TLS).
- A `denied` instead of `hello` (rate limit, no slot) is accepted and reported.
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
  `{"t":"status","body":<the same JSON as GET /api/status>}`. "A change" is
  any successful non-GET request over the link.
- Manager keeps the last one, with its time, so the page shows status
  instantly and a "last seen" time when the Pi is away.

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
| `POST /api/sentryusb/pair` | Takes `{name, username, password}` and opens a 15-minute pairing slot. Fresh admin password confirmation is needed, as for other risky actions. |
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
`pairing_open`, `fingerprint_read` (the fingerprint read before the first
pairing, with `address` and `fingerprint`), `fingerprint_confirmed` (the owner
pressed "It matches, pair"), `paired`, `unpaired`, `connected`,
`disconnected`, `attempt_failed` (`mode` is `pair`, `key`, or `check` for a
fingerprint read that failed), `fingerprint_mismatch` (warn; also when the
certificate at pairing isn't the confirmed one), `bad_proof` (warn),
`network`, `request`, `refused` (warn) and `alert_sent`. Text from the network is
clipped to 200 characters with control characters removed. No key, K,
password or request body is ever logged.

- **What it records:**
  - every connection attempt to Manager, fingerprint reads included
  - the fingerprint read before the first pairing, and the owner's confirm
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

- `v` in `hello` and `auth` is 1.
- A side that gets a newer `v` it doesn't know closes the connection with
  `{"t":"denied","why":"version"}`, and both UIs say to update.

## Appendix A. Test vectors

Both sides must reproduce these exactly. They are checked by the Pi's unit
tests (`crates/manager_link/src/handshake.rs`) and were also computed
independently with Python's `hashlib.pbkdf2_hmac` and `hmac`.

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

**Pair mode (S = K):**

- Pi proof, `HMAC(K, "pi|" + managerNonce + "|" + piNonce + "|" + fp + "|" + id)`:
  `1425df08a1362d43a7d44a6059ba7f078df99375d282bc35221ccd364acebbcf`
- Manager proof, `HMAC(K, "mgr|" + piNonce + "|" + managerNonce + "|" + fp + "|" + id)`:
  `2f864f5224cad380d328c3b8fa62104855ca1ce1eaaccf5dc12a49535cd2ce1e`

**Key mode (S = linkKey):**

- Pi proof: `852e6aa89c5f0fd2bd95f2ab6471d17353ffa98b3782d8b014db20c9bf865e50`
- Manager proof: `cc691f70debff4e37baa54f32bdd250a5fcdfa8efcffeca8f1c7de92a2c54bef`

**Fingerprint display:** both UIs show fingerprints as 16 groups of 4
uppercase hex digits, for example
`61EC B687 2005 D76F 9C85 B9AB EE8C 635C 4133 8110 3BF5 DF25 1EC2 63FC 331D 6C27`.
