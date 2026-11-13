#!/bin/bash
# Tests for the stale snapshot mount fix (run/make_snapshot.sh,
# run/release_snapshot.sh): a discarded run reuses its slot number, so the
# autofs mount must come down by mountpoint (the map mounts the image at an
# offset, which "umount <image>" doesn't find), and a run must never list
# its files through a mount of an older image (2026-11-12: 236 runs in a row
# read a stale mount and were discarded as identical).
# Functions are loaded with eval and stubbed, so shellcheck sees them as unused or unreachable.
# shellcheck disable=SC2034,SC2015,SC2016,SC2317
set -euo pipefail

ms=${1:-run/make_snapshot.sh}
rs=${2:-run/release_snapshot.sh}
failures=0
fail() { echo "FAIL: $*" >&2; failures=$((failures + 1)); }

extract() {
  awk -v f="$2" '$0 ~ "^function " f "[ (]" {keep=1} keep {print} keep && /^}/ {exit}' "$1"
}
for fn in snapshot_mounted unmount_snapshot_mount snapshot_mount_is_current
do
  src=$(extract "$ms" "$fn")
  [ -n "$src" ] || { echo "function $fn not found in $ms" >&2; exit 1; }
  eval "$src"
done

# Wiring: unmount a leftover before reuse, re-check after listing, unmount on discard.
grep -q 'if snapshot_mounted "$newsnapmnt"' "$ms" || fail "make_snapshot doesn't unmount a leftover mount before reusing the slot"
grep -q '! snapshot_mount_is_current "$newsnapmnt"' "$ms" || fail "make_snapshot doesn't check the listed mount is this snapshot's image"
discard=$(awk '/release_snapshot.sh/ && /newsnapdir|snapname/ {keep=1} keep {print} keep && /rm -rf/ {exit}' "$ms")
[[ $discard == *'unmount_snapshot_mount "$newsnapmnt"'* ]] || fail "the discard path doesn't unmount the slot's mount"
grep -q 'umount "/tmp/snapshots/$NAME"' "$rs" || fail "release_snapshot doesn't unmount by the autofs mountpoint"
grep -q 'unmount_snapshot_mount(&snap_mnt)' crates/usb_gadget/src/snapshot.rs || fail "the Rust snapshot code doesn't unmount the slot's mount"

# Not mounted: nothing to do, and a plain directory counts as current.
dir=$(mktemp -d); trap 'rm -rf "$dir"' EXIT
! snapshot_mounted "$dir" || fail "a plain directory reported as mounted"
unmount_snapshot_mount "$dir" || fail "unmounting something not mounted should succeed"
snapshot_mount_is_current "$dir" "$dir/snap.bin" || fail "a non-loop mount must count as current"

# A loop mount: current only when its backing inode is this image's.
losetup() { echo " $FAKE_INO"; }
grep() { [ "$*" = "-q  /fake/mnt  /proc/mounts" ] && return 0; command grep "$@"; }
awk() { if [ "${2:-}" = "m=/fake/mnt" ]; then echo /dev/loop7; else command awk "$@"; fi; }
: > "$dir/snap.bin"
FAKE_INO=$(stat -c %i "$dir/snap.bin")
snapshot_mount_is_current /fake/mnt "$dir/snap.bin" || fail "a loop mount of this image reported stale"
FAKE_INO=$((FAKE_INO + 1))
! snapshot_mount_is_current /fake/mnt "$dir/snap.bin" || fail "a loop mount of an older image reported current"
unset -f losetup grep awk

if [ "$failures" -gt 0 ]
then
  echo "$failures failure(s)" >&2
  exit 1
fi
echo "stale snapshot mount tests passed"
