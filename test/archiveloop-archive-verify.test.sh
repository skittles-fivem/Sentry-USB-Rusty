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
  archive_local_is_reencoded archive_remote_is_reencoded archive_manager_original_sums \
  queue_unverified_archived queue_archive_rechecks snapshot_verified_twice auto_delete_verified_snapshots verify_archived_clips \
  json_string json_int json_bool build_share_status_json plan_share_transfer record_archive_throughput \
  write_share_status
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
# Like release_snapshot.sh: remove the snapshot and the links into it.
fake_release() {
  echo "${1##*/}" >> "$workdir/released"
  rm -rf "$1"
  find "$workdir/mutable/TeslaCam" -lname "*/${1##*/}/*" -delete
}

mkdir -p "$workdir/mutable/TeslaCam" "$workdir/share" "$workdir/backingfiles/snapshots"
export ARCHIVE_SYSTEM=cifs ARCHIVE_MOUNT="$workdir/share"
export ARCHIVE_RECENTCLIPS=true
ARCHIVE_VERIFY_QUEUE="$workdir/mutable/archive_verify_pending"
ARCHIVE_VERIFY_LEDGER="$workdir/mutable/archive_verified.tsv"
ARCHIVE_VERIFY_WORK="$workdir/work"
ARCHIVE_REENCODED_TAGS='hvc1|hev1|av01'
ARCHIVE_MANAGER_CHECKSUMS=.sentrymanager/original-checksums.sha256
export ARCHIVE_VERIFY_ABSENT_RECHECK_SECONDS=0
# The "missing" re-check pauses with sleep; a test can hook that pause to
# simulate the share changing in between.
on_pause=""
sleep() { if [ -n "$on_pause" ]; then eval "$on_pause"; on_pause=""; fi; }
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
grep -q "1 matched, 0 re-encoded on the server by Sentry Manager, 2 to re-send" <<< "$logs" || fail "summary line wrong: $logs"

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

# --- clips archived before verification existed get checked ---------------
# On the archived list (sent by an older version), no ledger line, no queue
# entry: the next pass must queue and verify it.
make_snap snap-000003 2026-11-07_13-00-00-front.mp4
touch -d '2026-11-07 13:00' "$snaps/snap-000003/snap.bin"
old_rel=RecentClips/2026-11-07/2026-11-07_13-00-00-front.mp4
ln -s "$snaps/snap-000003/mnt/TeslaCam/RecentClips/2026-11-07_13-00-00-front.mp4" "$lower/$old_rel"
cp -L "$lower/$old_rel" "$workdir/share/$old_rel"
echo "$old_rel" >> "$workdir/mutable/sentry_files_archived"
echo 'export ARCHIVE_VERIFY=true' > "$workdir/sentryusb.conf"
: > "$workdir/log"
verify_archived_clips || fail "backlog pass failed: $(cat "$workdir/log")"
grep -q "Queueing 1 previously archived file(s)" "$workdir/log" || fail "backlog clip not queued: $(cat "$workdir/log")"
[ "$(status_of "$old_rel")" = ok/1 ] || fail "backlog clip should verify ok/1, got $(status_of "$old_rel")"
# Already checked: the next pass doesn't queue it again.
: > "$workdir/log"
verify_archived_clips
grep -q "previously archived" "$workdir/log" && fail "backlog clip queued twice: $(cat "$workdir/log")"

# --- Sentry Manager re-encodes a clip in place on the share -----------------
# First check matches; then the share copy is replaced by an "H.265" file
# (same name, different bytes, an hvc1 sample entry). The second check must
# call it converted: kept, not renamed aside, not re-sent, and still counted
# toward auto-delete.
conv_rel=RecentClips/2026-11-07/2026-11-07_13-00-00-front.mp4
echo 'export ARCHIVE_AUTO_DELETE_VERIFIED=true ARCHIVE_VERIFY_RECHECK_SECONDS=0' > "$workdir/sentryusb.conf"
{ head -c 4000 /dev/urandom; printf 'stsd....hvc1....'; head -c 4000 /dev/urandom; } > "$workdir/share/$conv_rel"
: > "$workdir/log"
verify_archived_clips
[ "$(status_of "$conv_rel")" = converted/2 ] || fail "re-encoded copy should be converted/2, got $(status_of "$conv_rel"): $(cat "$workdir/log")"
grep -q "1 re-encoded on the server by Sentry Manager" "$workdir/log" || fail "summary should count the conversion: $(cat "$workdir/log")"
ls "$workdir/share/RecentClips/2026-11-07/" | grep -q '13-00-00-front.mp4.mismatch' && fail "re-encoded copy was renamed aside"
grep -q 13-00-00 "$workdir/mutable/sentry_files_archived" || fail "re-encoded clip dropped from the archived list (would be re-sent)"
grep -q "doesn't match" "$workdir/log" && fail "a conversion was logged as a mismatch"
[ "$(snapshot_verified_twice "$snaps/snap-000003/snap.bin.toc")" = "yes 1" ] || fail "converted clips should count toward auto-delete"

# A server copy that differs and is NOT a re-encode is still a mismatch,
# even when its name says .mp4.
mismatch_rel=RecentClips/2026-11-07/2026-11-07_14-00-00-front.mp4
make_snap snap-000004 2026-11-07_14-00-00-front.mp4
ln -s "$snaps/snap-000004/mnt/TeslaCam/RecentClips/2026-11-07_14-00-00-front.mp4" "$lower/$mismatch_rel"
head -c 150000 /dev/urandom > "$workdir/share/$mismatch_rel"
echo "$mismatch_rel" >> "$workdir/mutable/sentry_files_archived"
: > "$workdir/log"
verify_archived_clips
[ "$(status_of "$mismatch_rel")" = mismatch/0 ] || fail "garbage server copy should be a mismatch, got $(status_of "$mismatch_rel")"

# --- re-encoded BEFORE the Pi's first check --------------------------------
# new_reencoded <hh>: a snapshot clip whose share copy is already "H.265".
new_reencoded() {
  local name="2026-11-07_$1-00-00-front.mp4" rel="RecentClips/2026-11-07/2026-11-07_$1-00-00-front.mp4"
  make_snap "snap-0000$1" "$name"
  ln -s "$snaps/snap-0000$1/mnt/TeslaCam/RecentClips/$name" "$lower/$rel"
  { head -c 4000 /dev/urandom; printf 'stsd....hvc1....'; } > "$workdir/share/$rel"
  echo "$rel" >> "$workdir/mutable/sentry_files_archived"
}
pi_sum() { sha256sum < "$snaps/snap-0000$1/mnt/TeslaCam/RecentClips/2026-11-07_$1-00-00-front.mp4" | cut -c1-64; }
echo 'export ARCHIVE_AUTO_DELETE_VERIFIED=true' > "$workdir/sentryusb.conf"

# No Manager record: kept, not re-sent, but unconfirmed (no auto-delete).
new_reencoded 15
: > "$workdir/log"
verify_archived_clips
r15=RecentClips/2026-11-07/2026-11-07_15-00-00-front.mp4
[ "$(status_of $r15)" = converted_unconfirmed/0 ] || fail "early re-encode without a record should be unconfirmed, got $(status_of $r15)"
ls "$workdir/share/RecentClips/2026-11-07/" | grep -q '15-00-00-front.mp4.mismatch' && fail "unconfirmed re-encode was renamed aside"
grep -q 15-00-00 "$workdir/mutable/sentry_files_archived" || fail "unconfirmed re-encode would be re-sent"
[ "$(snapshot_verified_twice "$snaps/snap-000015/snap.bin.toc")" = "no 1 1 0" ] || fail "unconfirmed re-encode must block auto-delete"
# Opt-in trust counts it, and only while the setting is on.
echo 'export ARCHIVE_AUTO_DELETE_VERIFIED=true ARCHIVE_TRUST_UNCONFIRMED_CONVERSIONS=true' > "$workdir/sentryusb.conf"
[ "$(snapshot_verified_twice "$snaps/snap-000015/snap.bin.toc")" = "yes 1" ] || fail "trusted unconfirmed re-encode should allow auto-delete"
echo 'export ARCHIVE_AUTO_DELETE_VERIFIED=true' > "$workdir/sentryusb.conf"

# Manager's record matches the Pi's copy: converted, confirmed by checksum.
new_reencoded 16
r16=RecentClips/2026-11-07/2026-11-07_16-00-00-front.mp4
mkdir -p "$workdir/share/.sentrymanager"
printf '%s  %s\n' "$(pi_sum 16)" "$r16" >> "$workdir/share/$ARCHIVE_MANAGER_CHECKSUMS"
# A substring of another path must not match (grep -F alone would).
printf '%s  %s\n' "$(printf 'f%.0s' {1..64})" "x$r16" >> "$workdir/share/$ARCHIVE_MANAGER_CHECKSUMS"
verify_archived_clips
[ "$(status_of $r16)" = converted/1 ] || fail "early re-encode with a matching record should be converted/1, got $(status_of $r16)"

# Manager's record doesn't match: Manager converted a bad upload; re-send.
new_reencoded 17
r17=RecentClips/2026-11-07/2026-11-07_17-00-00-front.mp4
printf '%s  %s\n' "$(printf '0%.0s' {1..64})" "$r17" >> "$workdir/share/$ARCHIVE_MANAGER_CHECKSUMS"
: > "$workdir/log"
verify_archived_clips
[ "$(status_of $r17)" = mismatch/0 ] || fail "re-encode of a bad copy should be a mismatch, got $(status_of $r17)"
grep -q "re-encoded $r17 from a copy that doesn't match" "$workdir/log" || fail "bad-source re-encode not explained: $(cat "$workdir/log")"
ls "$workdir/share/RecentClips/2026-11-07/" | grep -q '17-00-00-front.mp4.mismatch' || fail "bad-source re-encode not renamed aside"
grep -q 17-00-00 "$workdir/mutable/sentry_files_archived" && fail "bad-source re-encode not queued for re-send"

# --- briefly absent (Manager's no-hard-link replace) is not missing ---------
# The clip is gone at the first look and back by the re-check: verified,
# not re-sent.
gap_rel=RecentClips/2026-11-07/2026-11-07_18-00-00-front.mp4
make_snap snap-000018 2026-11-07_18-00-00-front.mp4
ln -s "$snaps/snap-000018/mnt/TeslaCam/RecentClips/2026-11-07_18-00-00-front.mp4" "$lower/$gap_rel"
echo "$gap_rel" >> "$workdir/mutable/sentry_files_archived"
on_pause='cp -L "$lower/$gap_rel" "$workdir/share/$gap_rel"'
: > "$workdir/log"
verify_archived_clips
[ "$(status_of $gap_rel)" = ok/1 ] || fail "clip back after a brief gap should be ok/1, got $(status_of $gap_rel)"
grep -q 18-00-00 "$workdir/mutable/sentry_files_archived" || fail "briefly absent clip was queued for re-send"
grep -q "isn't on the archive server" "$workdir/log" && fail "briefly absent clip was reported missing"

# A Pi copy that is itself H.265 proves nothing about a differing server copy.
printf 'hvc1' >> "$snaps/snap-000004/mnt/TeslaCam/RecentClips/2026-11-07_14-00-00-front.mp4"
archive_local_is_reencoded "$snaps/snap-000004/mnt/TeslaCam/RecentClips/2026-11-07_14-00-00-front.mp4" || fail "local hvc1 not detected"

# --- verification off: queue is dropped, nothing runs ----------------------
echo 'export ARCHIVE_VERIFY=false' > "$workdir/sentryusb.conf"
echo "x"$'\t'"y" > "$ARCHIVE_VERIFY_QUEUE"
verify_archived_clips
[ ! -e "$ARCHIVE_VERIFY_QUEUE" ] || fail "disabled verification left the queue behind"

# --- status file on the share ---------------------------------------------
echo 'export ARCHIVE_AUTO_DELETE_VERIFIED=true' > "$workdir/sentryusb.conf"
travel_mode_active() { return 1; }
SHARE_STATUS_DIR=.sentryusb SHARE_STATUS_NAME=archive-status.json
ARCHIVE_THROUGHPUT_FILE="$workdir/mutable/archive_throughput"
validate_json() { python3 -c 'import json,sys; print(json.dumps(json.load(open(sys.argv[1])), sort_keys=True))' "$1"; }
status_json="$ARCHIVE_MOUNT/.sentryusb/archive-status.json"

# Start of a transfer: range, size and deadlines from the default 2 MB/s.
printf '%s\n' RecentClips/2026-11-07/2026-11-07_10-00-00-front.mp4 'SentryClips/ev "1"/2026-11-07_09-59-00-back.mp4' > "$workdir/plan"
mkdir -p "$workdir/planroot/RecentClips/2026-11-07" "$workdir/planroot/SentryClips/ev \"1\""
head -c 3000000 /dev/zero > "$workdir/planroot/RecentClips/2026-11-07/2026-11-07_10-00-00-front.mp4"
head -c 1000000 /dev/zero > "$workdir/planroot/SentryClips/ev \"1\"/2026-11-07_09-59-00-back.mp4"
plan_share_transfer "$workdir/plan" "$workdir/planroot"
[ "$ARCHIVE_PLAN_BYTES" = 4000000 ] || fail "planned bytes $ARCHIVE_PLAN_BYTES"
[ $((ARCHIVE_PLAN_FINISH - ARCHIVE_PLAN_START)) = 62 ] || fail "expected 4 MB / 2 MB/s + 60 s"
[ $((ARCHIVE_PLAN_GIVE_UP - ARCHIVE_PLAN_FINISH)) = 900 ] || fail "give-up grace should be at least 15 min"
ARCHIVE_SHARE_STATE=transferring ARCHIVE_LAST_RESULT=in_progress
write_share_status || fail "status write failed"
out=$(validate_json "$status_json") || fail "status file is not valid JSON: $(cat "$status_json")"
for want in '"state": "transferring"' '"result": "in_progress"' '"files": 2' \
  '"first_clip": "SentryClips/ev \"1\"/2026-11-07_09-59-00-back.mp4"' \
  '"last_clip": "RecentClips/2026-11-07/2026-11-07_10-00-00-front.mp4"' '"auto_delete_enabled": true'
do
  grep -qF "$want" <<< "$out" || fail "status file lacks $want: $out"
done
ls -A "$ARCHIVE_MOUNT/.sentryusb" | grep -qv '^archive-status.json$' && fail "temp file left beside the status file"

# End of the cycle: idle with results; throughput learned for next time.
ARCHIVE_SHARE_STATE=idle ARCHIVE_LAST_RESULT=ok ARCHIVE_LAST_ARCHIVED=2 ARCHIVE_LAST_LEFT=0
ARCHIVE_LAST_END=$((ARCHIVE_PLAN_START + 40)) ARCHIVE_LAST_CLIP=RecentClips/2026-11-07/2026-11-07_10-00-00-front.mp4
ARCHIVE_VERIFY_RAN=1 ARCHIVE_VERIFY_MATCHED=2 ARCHIVE_VERIFY_STOPPED=""
record_archive_throughput
[ "$(cat "$ARCHIVE_THROUGHPUT_FILE")" = 100000 ] || fail "throughput should be 4 MB / 40 s"
write_share_status
out=$(validate_json "$status_json") || fail "idle status file is not valid JSON"
for want in '"state": "idle"' '"result": "ok"' '"files_archived": 2' '"matched": 2' '"ran": true' '"stopped": null'; do
  grep -qF "$want" <<< "$out" || fail "idle status lacks $want: $out"
done
ARCHIVE_PLAN_START="" ; write_share_status
grep -q '"transfer": null' "$status_json" || fail "no transfer this cycle should write transfer: null"

if [ "$failures" -gt 0 ]; then
  echo "$failures failure(s)" >&2
  exit 1
fi
echo "archive verification tests passed"
