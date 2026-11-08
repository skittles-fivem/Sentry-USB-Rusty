# Building SentryUSB (Rust)

## Prerequisites

- Rust stable (1.89+; some locked dependencies need it)
- Node `^20.19.0 || >=22.12.0` and npm
- `cross` for cross-compilation: `cargo install cross`
- Docker (for `cross` and image builds)

## Web UI

The web UI is React + Vite. Source lives in `web/`; the build output is embedded
into the `sentryusb` binary at compile time via `rust_embed`.

```
cd web
npm ci --no-audit --no-fund
npm run build
cd ..
rm -rf crates/sentryusb/static
cp -r web/dist crates/sentryusb/static
```

## Rust binaries

Three binaries ship with the project:
- `sentryusb` — main daemon (HTTP + WebSocket + setup orchestrator)
- `sentryusb-tesla-telemetry` — BLE telemetry sampler (lazy-started after pairing)
- `sentryusb-ble-action` — Tesla BLE action CLI (from the `sentryusb-tesla-ble` package)

### Cross-compile for the Pi

64-bit (Pi 3A+/4/5/Zero 2):
```
cross build --release --target aarch64-unknown-linux-gnu -p sentryusb
cross build --release --target aarch64-unknown-linux-gnu -p sentryusb-tesla-telemetry
cross build --release --target aarch64-unknown-linux-gnu -p sentryusb-tesla-ble --bin sentryusb-ble-action
```

32-bit (armhf — Pi 3A+ with 32-bit Pi OS):
```
cross build --release --target armv7-unknown-linux-gnueabihf -p sentryusb
```
(Build the other two packages the same way with the armv7 target.)

Binaries land in `target/<target>/release/`.

### Native (Linux dev box)

```
cargo build --release
```

## SD card image

`image/build-image.sh` takes the newest official Raspberry Pi OS Lite
(64-bit) image, installs one Sentry USB release into it by running
`install-pi.sh` in image mode inside a chroot, and writes the image, its
checksum and a Raspberry Pi Imager manifest. It needs Linux, root, and an
arm64 host (or qemu-user binfmt for aarch64):

```
sudo image/build-image.sh --tag v3.29.0 --out deploy
```

The image keeps Pi OS's first boot as it ships, so the Wi-Fi, user,
hostname and SSH settings chosen in Imager apply on the user's card. Imager
2 only offers those settings for images opened through a manifest that
declares how the image is configured (`init_format`), which is why users
open the `.rpi-imager-manifest` rather than the `.img.xz` itself;
`image/imager.py` copies `init_format` and the board list from Raspberry
Pi's own entry for the base image.

## Deploy to an existing Pi

```
# Copy binary to the Pi and run install-pi.sh with its local path
scp target/aarch64-unknown-linux-gnu/release/sentryusb pi@<ip>:/tmp/
ssh pi@<ip> sudo -i
bash install-pi.sh /tmp/sentryusb
```

## Testing

- Rust unit tests: `cargo test --workspace --locked` (CI runs them on Linux; a few tests need Linux-only `/proc` and shell tools)
- Clippy: `cargo +1.99.0 clippy --workspace --all-targets --locked -- -D warnings` (CI pins Rust 1.99.0)
- Shell and Python harnesses: `test/*.test.sh`, `test/*.test.py`
- Web: `cd web && npm test && npm run lint`
- Lint: `bash check.sh` (runs ShellCheck against selected runtime scripts)

## Releasing

GitHub Releases are expected to host these artifacts (naming consumed by
`install-pi.sh`):

- `sentryusb-linux-arm64-a53` / `-a72` / `-a76` (per-CPU aarch64 variants)
- `sentryusb-linux-arm64` (backward-compat alias = a72 build)
- `sentryusb-linux-armv7`
- `sentryusb-tesla-telemetry-linux-*` and `sentryusb-ble-action-linux-*` (one
  per CPU variant, plus the `-linux-arm64` alias)

The `Build & Release` workflow (`.github/workflows/build.yml`) builds these on
native arm64 runners (armv7 is cross-compiled with GCC) and attaches them to
the release.

Once a stable release is out, the `SD Card Image` workflow
(`.github/workflows/image.yml`) builds the image for it and attaches three
more files and a first line in the release notes pointing to the manifest.
Publishing the release doesn't wait for it.

- `Sentry-USB-for-Raspberry-Pi-Imager.rpi-imager-manifest`: what users
  download and double-click. Its name puts it at the top of the release
  page, which lists files by name.
- `sentryusb-raspberry-pi-image.img.xz` and its `.sha256`: the image the
  manifest points to. Its name sorts it in among the binaries, so people
  don't flash it without Imager's Wi-Fi and user settings.
- `sentryusb-imager-os-list.json`: the same entry in the format Raspberry
  Pi's own Imager list loads from us, at
  `releases/latest/download/sentryusb-imager-os-list.json`.

Automatic builds run only while the repository variable `SENTRYUSB_SD_IMAGE`
is `auto`. Pre-releases are skipped, but promoting one to a full release
builds its image. To rebuild or test, run the workflow by hand with a tag;
turn off "publish" to keep the result as a workflow artifact instead of
attaching it.

### Test builds from a fork

A Pi can update from a fork instead of official: in Settings > Software
Updates > Update source, choose **Custom repository** and enter the fork as
`owner/name` (the field starts at `skittles-fivem/Sentry-USB-Rusty`). This
writes `REPO` in `sentryusb.conf`; `REPO=owner` (the repository name stays
`Sentry-USB-Rusty`) and `REPO=owner/name` both work. Choosing **Official**
removes `REPO` again. A custom source always lists its pre-releases, since
that is what test builds are.

**Tag format.** Fork releases must use `v<semver>` tags, and test builds take
the *next* official patch version plus `-test.<n>`. With official at `v3.28.9`,
tag `v3.28.10-test.1`, then `v3.28.10-test.2`, and so on. Semver ranks a release
above its own pre-releases, so the official `v3.28.10` is newer than every
`v3.28.10-test.N`: after switching the source back to Official, the Pi offers
`v3.28.10` (or whatever official latest is by then) as one update. Before
official `v3.28.10` exists, the Settings page offers "Revert to Stable" back to
`v3.28.9`. Never tag a fork release with a plain `vX.Y.Z` version the official
project hasn't shipped: it would outrank, or collide with, the real release.
Tags that aren't semver at all (`1.22`, `latest`) are ignored: the Pi never
offers or installs them.

**Publishing a fork release the Pi can install.**

1. In the fork, enable GitHub Actions (forks start with workflows disabled)
   and allow `GITHUB_TOKEN` read and write access (Settings > Actions >
   General > Workflow permissions) so the workflow can create the release.
2. Commit to the fork's `main`, then push the tag:
   `git tag v3.28.10-test.1 && git push origin v3.28.10-test.1`.
3. `Build & Release` (`.github/workflows/build.yml`) runs on the `v*` tag,
   builds every variant and creates the GitHub Release with the assets listed
   above. A tag containing `-` is marked as a pre-release automatically, and
   the `SD Card Image` workflow skips pre-releases.
4. The Pi needs, from that release: `sentryusb-<variant>` for its board
   (required), and `sentryusb-tesla-telemetry-<variant>` and
   `sentryusb-ble-action-<variant>` (optional; without them those components
   stay on the previous version). It also fetches the tag's source archive
   and `setup/pi/apply-runtime-patches.sh` at that tag from the same
   repository, which GitHub serves for every tag. Don't attach binaries by
   hand under other names.
5. On the Pi, check for updates and install the build from the Pre-release
   card, which names the repository it comes from.

Downloads from a fork are verified the same way as official ones: the file
size and, when GitHub lists one, the SHA-256 digest of each release asset. Official
releases aren't signed, so a fork needs no key. Automatic installation only
takes stable releases, so `-test.N` builds are always installed by hand. A
device on a test build is moved to the next official stable release
automatically once the source is Official again (if automatic installation is
on).

armv6 (Pi Zero W / Pi 1) is no longer built — the board is too underpowered
to run the daemon comfortably, and dropping the matrix entry keeps the
release artifact count manageable.
