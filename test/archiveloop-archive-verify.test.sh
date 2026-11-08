#!/bin/bash
# Tests for SHA-256 archive verification and the optional auto-delete of
# snapshots verified twice (run/archiveloop). Uses the mounted (cifs)
# backend against temp directories standing in for the share, the snapshot
# drive and /mutable.
set -euo pipefail

script=${1:-run/archiveloop}
workdir=$(mktemp -d)
trap 'rm -rf "$workdir"' EXIT

extract() {
  awk -v f="$1" '$0 ~ "^function " f "[ (]" {keep=1} keep {print} keep && /^}/ {exit}' "$script"
}

# Point the hard-coded device paths at the temp tree.
for fn in sortfile prunefile archive_conf_value archive_verify_enabled archive_auto_delete_enabled \
  record_archive_link_targets queue_archived_for_verification archive_ssh_command \
  archive_remote_sha256 archive_set_aside_remote verify_archive_batch prune_archive_verify_ledger \
  queue_archive_rechecks snapshot_verified_twice auto_delete_verified_snapshots verify_archived_clips
do
  src=$(extract "$fn")
  [ -n "$src" ] || { echo "function $fn not found in $script" >&2; exit 1; }
  src=${src//\/backingfiles/$workdir/backingfiles}
  src=${src//\/mutable\//$workdir/mutable/}
  src=${src//\/root\/bin\/release_snapshot.sh/fake_release}
  src=${src//\/root\/sentryusb.conf/$workdir/sentryusb.conf}
  eval "$src"
done

failures=0
fail() { echo "FAIL: $*" >&2; failures=$((failures + 1)); }
logs=""
log() { logs+="$*"$'\n'; echo "$*" >> "$workdir/log"; }
archive_cancel_requested() { return 1; }
archive_is_reachable() { return 0; }
ionice() { shift 2; "$@"; }
nice() { shift; "$@"; }
CLOCK=1000
uptime_seconds() { echo "$CLOCK"; }
: > "$workdir/released"
fake_release() { echo "${1##*/}" >> "$workdir/released"; rm -rf "$1"; }

mkdir -p "$workdir/mutable/TeslaCam" "$workdir/share" "$workdir/backingfiles/snapshots"
export ARCHIVE_SYSTEM=cifs ARCHIVE_MOUNT="$workdir/share"
export ARCHIVE_RECENTCLIPS=true
ARCHIVE_VERIFY_QUEUE="$workdir/mutable/archive_verify_pending"
ARCHIVE_VERIFY_LEDGER="$workdir/mutable/archive_verified.tsv"
ARCHIVE_VERIFY_WORK="$workdir/work"
SNAPSHOT_LOCK_DIR="$workdir/backingfiles/snapshots"
: > "$workdir/sentryusb.conf"

# Two snapshots; each holds two 100 KB+ clips in its own image directory.
snaps="$workdir/backingfiles/snapshots"
make_snap() {  # name, clip names...
  local name=$1; shift
  mkdir -p "$snaps/$name/mnt/TeslaCam/RecentClips"
  : > "$snaps/$name/snap.bin"
  : > "$snaps/$name/snap.bin.toc"
  for c in "$@"; do
    head -c 150000 /dev/urandom > "$snaps/$name/mnt/TeslaCam/RecentClips/$c"
    echo "150000 TeslaCam/RecentClips/$c" >> "$snaps/$name/snap.bin.toc"
  done
}
make_snap snap-000001 2026-11-07_10-00-00-front.mp4 2026-11-07_10-01-00-front.mp4
make_snap snap-000002 2026-11-07_11-00-00-front.mp4
touch -d '2026-11-07 12:00' "$snaps/snap-000002/snap.bin"
touch -d '2026-11-07 11:00' "$snaps/snap-000001/snap.bin"

# Link farm, as make_snapshot.sh builds it.
lower="$workdir/mutable/TeslaCam"
mkdir -p "$lower/RecentClips/2026-11-07"
for s in snap-000001:2026-11-07_10-00-00-front.mp4 snap-000001:2026-11-07_10-01-00-front.mp4 snap-000002:2026-11-07_11-00-00-front.mp4; do
  ln -s "$snaps/${s%%:*}/mnt/TeslaCam/RecentClips/${s#*:}" "$lower/RecentClips/2026-11-07/${s#*:}"
done
list="$workdir/list"
printf '%s\n' RecentClips/2026-11-07/2026-11-07_10-00-00-front.mp4 \
  RecentClips/2026-11-07/2026-11-07_10-01-00-front.mp4 \
  RecentClips/2026-11-07/2026-11-07_11-00-00-front.mp4 > "$list"
cp "$list" "$workdir/mutable/sentry_files_archived"

# "Archive" them: copy through the links into the share, like rsync -RL.
(cd "$lower" && while IFS= read -r f; do mkdir -p "$workdir/share/${f%/*}"; cp -L "$f" "$workdir/share/$f"; done < "$list")

# --- link targets are recorded and queued -------------------------------------
record_archive_link_targets "$lower" "$list" "$workdir/targets"
[ "$(wc -l < "$workdir/targets")" = 3 ] || fail "expected 3 link targets, got: $(cat "$workdir/targets")"
queue_archived_for_verification "$list" "$workdir/targets" "$lower"
grep -q $'\t'"$snaps/snap-000001/mnt/TeslaCam/RecentClips/2026-11-07_10-00-00-front.mp4" "$ARCHIVE_VERIFY_QUEUE" ||
  fail "queue does not point at the snapshot copy"

# --- a damaged and a missing server copy --------------------------------------
printf 'X' | dd of="$workdir/share/RecentClips/2026-11-07/2026-11-07_10-01-00-front.mp4" bs=1 seek=10 conv=notrunc status=none
rm "$workdir/share/RecentClips/2026-11-07/2026-11-07_11-00-00-front.mp4"

verify_archived_clips || fail "first verification pass reported failure: $logs"
status_of() { awk -F'\t' -v r="$1" '$1 == r { s = $5 "/" $6 } END { print s }' "$ARCHIVE_VERIFY_LEDGER"; }
[ "$(status_of RecentClips/2026-11-07/2026-11-07_10-00-00-front.mp4)" = ok/1 ] || fail "intact clip should be ok/1"
[ "$(status_of RecentClips/2026-11-07/2026-11-07_10-01-00-front.mp4)" = mismatch/0 ] || fail "damaged clip should be mismatch"
[ "$(status_of RecentClips/2026-11-07/2026-11-07_11-00-00-front.mp4)" = missing/0 ] || fail "deleted clip should be missing"
ls "$workdir/share/RecentClips/2026-11-07/" | grep -q '2026-11-07_10-01-00-front.mp4.mismatch-' || fail "damaged copy was not renamed aside"
grep -q 10-01-00 "$workdir/mutable/sentry_files_archived" && fail "damaged clip still marked archived (would never be re-sent)"
grep -q 11-00-00 "$workdir/mutable/sentry_files_archived" && fail "missing clip still marked archived (would never be re-sent)"
grep -q 10-00-00 "$workdir/mutable/sentry_files_archived" || fail "verified clip dropped from the archived list"
[ ! -e "$ARCHIVE_VERIFY_QUEUE" ] || fail "queue not emptied after a complete pass"
grep -q "1 matched, 2 to re-send" <<< "$logs" || fail "summary line wrong: $logs"

# --- re-send, then second check; auto-delete off releases nothing ------------
(cd "$lower" && for f in RecentClips/2026-11-07/2026-11-07_10-01-00-front.mp4 RecentClips/2026-11-07/2026-11-07_11-00-00-front.mp4; do cp -L "$f" "$workdir/share/$f"; done)
printf '%s\n' RecentClips/2026-11-07/2026-11-07_10-01-00-front.mp4 RecentClips/2026-11-07/2026-11-07_11-00-00-front.mp4 > "$workdir/resent"
queue_archived_for_verification "$workdir/resent" "$workdir/targets" "$lower"
logs=""
verify_archived_clips || fail "second pass failed: $logs"
[ "$(status_of RecentClips/2026-11-07/2026-11-07_10-01-00-front.mp4)" = ok/1 ] || fail "re-sent clip should verify ok/1"
[ ! -s "$workdir/released" ] || fail "auto-delete ran while turned off"

# --- auto-delete on: the re-check waits for the gap, then counts twice ------
echo 'export ARCHIVE_AUTO_DELETE_VERIFIED=true' > "$workdir/sentryusb.conf"
logs=""
verify_archived_clips
[ ! -s "$ARCHIVE_VERIFY_QUEUE" ] || fail "re-check queued before the hour gap"
[ ! -s "$workdir/released" ] || fail "released a snapshot verified only once"

echo 'export ARCHIVE_AUTO_DELETE_VERIFIED=true ARCHIVE_VERIFY_RECHECK_SECONDS=0' > "$workdir/sentryusb.conf"
# The newest snapshot is never released, even fully verified. Damage the
# server copy of snap-000002's clip AFTER its first check: the second check
# must catch it.
printf 'X' | dd of="$workdir/share/RecentClips/2026-11-07/2026-11-07_11-00-00-front.mp4" bs=1 seek=10 conv=notrunc status=none
logs=""
: > "$workdir/log"
verify_archived_clips
[ "$(status_of RecentClips/2026-11-07/2026-11-07_10-00-00-front.mp4)" = ok/2 ] || fail "second check should make ok/2"
[ "$(cat "$workdir/released")" = snap-000001 ] || fail "expected only snap-000001 released, got '$(cat "$workdir/released")'"
[ -d "$snaps/snap-000002" ] || fail "newest snapshot was released"
grep -q "releasing snap-000001; all 2 clip(s) in it are verified twice" "$workdir/log" || fail "release not logged: $(cat "$workdir/log")"
[ "$(status_of RecentClips/2026-11-07/2026-11-07_11-00-00-front.mp4)" = mismatch/0 ] || fail "second check missed a copy damaged after the first check"

# --- a snapshot with a folder that isn't archived is never released ---------
: > "$workdir/released"
make_snap snap-000000 2026-11-06_09-00-00-front.mp4
mkdir -p "$snaps/snap-000000/mnt/TeslaCam/SavedClips/ev"
echo "150000 TeslaCam/SavedClips/ev/2026-11-06_09-00-00-front.mp4" >> "$snaps/snap-000000/snap.bin.toc"
printf 'RecentClips/2026-11-06/2026-11-06_09-00-00-front.mp4\t150000\tabc\t1\tok\t2\tx\n' >> "$ARCHIVE_VERIFY_LEDGER"
export ARCHIVE_SAVEDCLIPS=false
verdict=$(snapshot_verified_twice "$snaps/snap-000000/snap.bin.toc")
[ "$verdict" = "no 1 0 1" ] || fail "snapshot with un-archived SavedClips should be kept, got '$verdict'"
export ARCHIVE_SAVEDCLIPS=true
verdict=$(snapshot_verified_twice "$snaps/snap-000000/snap.bin.toc")
[ "$verdict" = "no 2 1 0" ] || fail "unverified SavedClips clip should keep the snapshot, got '$verdict'"

# --- verification off: queue is dropped, nothing runs ----------------------
echo 'export ARCHIVE_VERIFY=false' > "$workdir/sentryusb.conf"
echo "x"$'\t'"y" > "$ARCHIVE_VERIFY_QUEUE"
verify_archived_clips
[ ! -e "$ARCHIVE_VERIFY_QUEUE" ] || fail "disabled verification left the queue behind"

if [ "$failures" -gt 0 ]; then
  echo "$failures failure(s)" >&2
  exit 1
fi
echo "archive verification tests passed"
