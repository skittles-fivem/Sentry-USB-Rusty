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
  archive_quick_fingerprint archive_remote_quick_fingerprint archive_quick_result archive_verify_quick_mode \
  archive_cached_local_sum precompute_local_hashes archive_local_quick_fingerprint archive_verify_progress \
  archive_work_progress archive_work_done archive_verify_methods archive_verify_gone_through \
  archive_basic_describe archive_current_source archive_resolve_batch_sources requeue_nosource_with_copy archive_verify_parallel archive_remote_quick_fingerprints archive_lowprio_fn archive_verify_basic_mode archive_basic_fingerprints archive_basic_result verify_archive_batch_basic archive_verify_finish_batch \
  archive_stream_is_reencoded archive_local_is_reencoded archive_remote_is_reencoded archive_manager_original_sums archive_manager_record_present \
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
ionice() { while [ "${1:-}" != "${1#-}" ]; do shift; done; "$@"; }
nice() { while [ "${1:-}" != "${1#-}" ]; do shift; done; "$@"; }
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
ARCHIVE_REENCODED_SAMPLE_ENTRY=' 73 74 73 64( [0-9a-f]{2}){12} (68 76 63 31|68 65 76 31|61 76 30 31)'
# An MP4 stsd box whose one sample entry is $1 (hvc1, avc1, ...).
stsd_entry() { printf 'stsd\000\000\000\000\000\000\000\001\000\000\000\126%s' "$1"; }
ARCHIVE_MANAGER_CHECKSUMS=.sentrymanager/original-checksums.sha256
export ARCHIVE_VERIFY_ABSENT_RECHECK_SECONDS=0
ARCHIVE_QUICK_WHOLE_BELOW=2097152
ARCHIVE_LOCAL_SUMS="$workdir/mutable/archive_local_sums.tsv"
ARCHIVE_VERIFY_PROGRESS="$workdir/progress.json"
ARCHIVE_WORK_PROGRESS="$workdir/work-progress.json"
ARCHIVE_CONTROL_DIR="$workdir/ctl"; mkdir -p "$ARCHIVE_CONTROL_DIR"
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

# Damage a file: change byte 10 to a different value. (Writing a fixed byte
# left the file unchanged whenever the random data already had it there,
# about 1 run in 256.)
corrupt_byte() {
  local b
  b=$(od -An -tu1 -j10 -N1 "$1" | tr -d ' ')
  printf "\\$(printf %03o $(( (b + 1) % 256 )))" | dd of="$1" bs=1 seek=10 conv=notrunc status=none
}

# --- link targets are recorded and queued -------------------------------------
record_archive_link_targets "$lower" "$list" "$workdir/targets"
[ "$(wc -l < "$workdir/targets")" = 3 ] || fail "expected 3 link targets, got: $(cat "$workdir/targets")"
queue_archived_for_verification "$list" "$workdir/targets" "$lower"
grep -q $'\t'"$snaps/snap-000001/mnt/TeslaCam/RecentClips/2026-11-07_10-00-00-front.mp4" "$ARCHIVE_VERIFY_QUEUE" ||
  fail "queue does not point at the snapshot copy"

# --- a damaged and a missing server copy --------------------------------------
corrupt_byte "$workdir/share/RecentClips/2026-11-07/2026-11-07_10-01-00-front.mp4"
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
grep -q "1 matched, 0 matched by quick check, 0 matched by size and frames, 0 re-encoded on the server by Sentry Manager, 2 to re-send" <<< "$logs" || fail "summary line wrong: $logs"

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
corrupt_byte "$workdir/share/RecentClips/2026-11-07/2026-11-07_11-00-00-front.mp4"
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
{ head -c 4000 /dev/urandom; stsd_entry hvc1; head -c 4000 /dev/urandom; } > "$workdir/share/$conv_rel"
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
  { head -c 4000 /dev/urandom; stsd_entry hvc1; } > "$workdir/share/$rel"
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

# --- Manager's record verifies without reading the clip back ----------------
# The record now exists. A clip with a matching line is verified from it:
# the share copy is removed here to prove nothing is read back.
mr=RecentClips/2026-11-07/2026-11-07_19-00-00-front.mp4
make_snap snap-000019 2026-11-07_19-00-00-front.mp4
ln -s "$snaps/snap-000019/mnt/TeslaCam/RecentClips/2026-11-07_19-00-00-front.mp4" "$lower/$mr"
echo "$mr" >> "$workdir/mutable/sentry_files_archived"
printf '%s  %s\n' "$(pi_sum 19)" "$mr" >> "$workdir/share/$ARCHIVE_MANAGER_CHECKSUMS"
rm -f "$workdir/share/$mr"
verify_archived_clips
[ "$(status_of $mr)" = converted/1 ] || fail "clip with a matching Manager line should be converted/1 without read-back, got $(status_of $mr)"

# No line yet: waits in the queue (not read back, not missing), stamped.
mw=RecentClips/2026-11-07/2026-11-07_20-00-00-front.mp4
make_snap snap-000020 2026-11-07_20-00-00-front.mp4
ln -s "$snaps/snap-000020/mnt/TeslaCam/RecentClips/2026-11-07_20-00-00-front.mp4" "$lower/$mw"
cp -L "$lower/$mw" "$workdir/share/$mw"
echo "$mw" >> "$workdir/mutable/sentry_files_archived"
: > "$workdir/log"
verify_archived_clips
[ -z "$(status_of $mw)" ] || fail "clip without a Manager line was checked instead of waiting: $(status_of $mw)"
grep -q "^$mw"$'\t' "$ARCHIVE_VERIFY_QUEUE" || fail "waiting clip left the queue"
awk -F'\t' -v r="$mw" '$1 == r && $5 ~ /^[0-9]+$/ { ok = 1 } END { exit !ok }' "$ARCHIVE_VERIFY_QUEUE" || fail "waiting clip not stamped with its queue time"
grep -q "1 waiting for Sentry Manager's checksum" "$workdir/log" || fail "waiting not reported: $(cat "$workdir/log")"
# Still waiting on the next pass (the stamp is kept, not reset).
verify_archived_clips
[ -z "$(status_of $mw)" ] || fail "waiting clip checked on the second pass"

# Past the wait limit (0 = don't wait): read back as before.
echo 'export ARCHIVE_AUTO_DELETE_VERIFIED=true ARCHIVE_VERIFY_MANAGER_WAIT_HOURS=0' > "$workdir/sentryusb.conf"
verify_archived_clips
[ "$(status_of $mw)" = ok/1 ] || fail "clip past the wait should be read back and ok/1, got $(status_of $mw)"

# Already re-encoded with no line (converted before the record existed),
# past the wait: classified from its codec, never downloaded. Proved by a
# sha256sum on the share copy that would fail the test if it ran.
mc=RecentClips/2026-11-07/2026-11-07_21-00-00-front.mp4
make_snap snap-000021 2026-11-07_21-00-00-front.mp4
ln -s "$snaps/snap-000021/mnt/TeslaCam/RecentClips/2026-11-07_21-00-00-front.mp4" "$lower/$mc"
{ head -c 4000 /dev/urandom; stsd_entry hvc1; } > "$workdir/share/$mc"
echo "$mc" >> "$workdir/mutable/sentry_files_archived"
# The Pi hashes a share copy with `timeout … sha256sum < file`, so watch
# timeout: a sha256sum whose stdin is this clip's share copy is a download.
# It runs in a $( ) subshell, so it leaves a marker file rather than calling fail.
timeout() {
  if [[ "$*" == *sha256sum* ]] &&
     [ "$(readlink -f "/proc/$BASHPID/fd/0" 2>/dev/null)" = "$(readlink -f "$workdir/share/$mc")" ]
  then
    touch "$workdir/downloaded"
  fi
  command timeout "$@"
}
verify_archived_clips
unset -f timeout
[ ! -e "$workdir/downloaded" ] || fail "downloaded a re-encoded clip to hash it"
[ "$(status_of $mc)" = converted_unconfirmed/0 ] || fail "re-encoded clip without a line should be converted_unconfirmed, got $(status_of $mc)"
ls "$workdir/share/RecentClips/2026-11-07/" | grep -q '21-00-00-front.mp4.mismatch' && fail "re-encoded clip without a line was renamed aside"

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
stsd_entry hvc1 >> "$snaps/snap-000004/mnt/TeslaCam/RecentClips/2026-11-07_14-00-00-front.mp4"
archive_local_is_reencoded "$snaps/snap-000004/mnt/TeslaCam/RecentClips/2026-11-07_14-00-00-front.mp4" || fail "local hvc1 not detected"
# The four letters on their own, or an H.264 (avc1) entry, are not a re-encode.
printf 'xxhvc1xxav01xxhev1xx' > "$workdir/loose.mp4"
archive_local_is_reencoded "$workdir/loose.mp4" && fail "loose hvc1/av01 bytes taken for a re-encode"
stsd_entry avc1 > "$workdir/h264.mp4"
archive_local_is_reencoded "$workdir/h264.mp4" && fail "an avc1 (H.264) sample entry taken for a re-encode"
stsd_entry av01 > "$workdir/av1.mp4"
archive_local_is_reencoded "$workdir/av1.mp4" || fail "an av01 sample entry not detected"

# --- SMB quick check (no Manager record): size + moov + video slices -------
# A synthetic MP4 above the 2 MiB whole-file limit: ftyp, mdat (3 MiB), moov.
make_mp4() {  # file
  { printf '\000\000\000\030ftypmp42\000\000\000\000mp42isom'
    printf '\000\060\000\000mdat'; head -c $((3145728 - 8)) /dev/urandom
    printf '\000\000\020\000moov'; head -c $((4096 - 8)) /dev/urandom; } > "$1"
}
rm -f "$workdir/share/$ARCHIVE_MANAGER_CHECKSUMS"   # no Manager on this share now
echo 'export ARCHIVE_AUTO_DELETE_VERIFIED=true' > "$workdir/sentryusb.conf"
quick_clip() {  # hh -> relpath; Pi copy in a snapshot, identical copy on the share
  local name="2026-11-07_$1-00-00-front.mp4" rel="RecentClips/2026-11-07/2026-11-07_$1-00-00-front.mp4"
  mkdir -p "$snaps/snap-0000$1/mnt/TeslaCam/RecentClips"
  make_mp4 "$snaps/snap-0000$1/mnt/TeslaCam/RecentClips/$name"
  echo "3149848 TeslaCam/RecentClips/$name" > "$snaps/snap-0000$1/snap.bin.toc"; : > "$snaps/snap-0000$1/snap.bin"
  ln -s "$snaps/snap-0000$1/mnt/TeslaCam/RecentClips/$name" "$lower/$rel"
  cp -L "$lower/$rel" "$workdir/share/$rel"
  echo "$rel" >> "$workdir/mutable/sentry_files_archived"
  echo "$rel"
}
q1=$(quick_clip 22)
verify_archived_clips
[ "$(status_of "$q1")" = ok_quick/1 ] || fail "identical MP4 should pass the quick check, got $(status_of "$q1")"
# Damage inside a sampled slice (40% into mdat): mismatch.
q2=$(quick_clip 23)
off=$(( 32 + 3145728 * 2 / 5 + 100 ))
printf 'ZZZZZZZZ' | dd of="$workdir/share/$q2" bs=1 seek=$off conv=notrunc status=none
verify_archived_clips
[ "$(status_of "$q2")" = mismatch/0 ] || fail "damaged slice should be a mismatch, got $(status_of "$q2")"
# Cut off (no moov): mismatch.
q3=$(quick_clip 24)
truncate -s 2500000 "$workdir/share/$q3"
verify_archived_clips
[ "$(status_of "$q3")" = mismatch/0 ] || fail "cut-off copy should be a mismatch, got $(status_of "$q3")"
# Forced full read-back: plain ok.
echo 'export ARCHIVE_VERIFY_MODE=full' > "$workdir/sentryusb.conf"
q4=$(quick_clip 25)
verify_archived_clips
[ "$(status_of "$q4")" = ok/1 ] || fail "full mode should read back and be ok/1, got $(status_of "$q4")"

# --- low-priority runs use the real ionice/nice programs --------------------
# The stubs above are shell functions, which hid that "ionice ... some_shell_function"
# fails on the Pi (programs can't run shell functions). Run without them.
if command -v ionice >/dev/null && command -v nice >/dev/null
then
  saved_ionice=$(declare -f ionice); saved_nice=$(declare -f nice)
  unset -f ionice nice
  make_mp4 "$workdir/lowprio.mp4"
  fp=$(archive_lowprio_fn low archive_quick_fingerprint "$workdir/lowprio.mp4") || fail "quick fingerprint at low priority failed"
  [[ "$fp" == mp4:3149848:* ]] || fail "quick fingerprint at low priority wrong: $fp"
  fp=$(archive_lowprio_fn idle archive_quick_fingerprint "$workdir/lowprio.mp4") || fail "quick fingerprint at idle priority failed"
  [[ "$fp" == mp4:* ]] || fail "idle-priority fingerprint wrong: $fp"
  fp=$(archive_local_quick_fingerprint "$workdir/lowprio.mp4") || fail "local quick fingerprint failed with real ionice"
  [[ "$fp" == mp4:* ]] || fail "local quick fingerprint wrong: $fp"
  echo "$workdir/lowprio.mp4" > "$workdir/lowprio.list"
  fp=$(ARCHIVE_LOWPRIO_TIMEOUT=60 archive_lowprio_fn low archive_basic_fingerprints "$workdir/lowprio.list") ||
    fail "basic fingerprints at low priority failed"
  [ -n "$fp" ] || fail "basic fingerprints at low priority printed nothing"
  eval "$saved_ionice"; eval "$saved_nice"
fi

# --- parallel checks ------------------------------------------------------------
echo 'export ARCHIVE_VERIFY_PARALLEL=3' > "$workdir/sentryusb.conf"
[ "$(archive_verify_parallel)" = 3 ] || fail "parallel setting not read"
echo 'export ARCHIVE_VERIFY_PARALLEL=500' > "$workdir/sentryusb.conf"
[ "$(archive_verify_parallel)" = 32 ] || fail "parallel setting not capped at 32"
echo 'export ARCHIVE_VERIFY_PARALLEL=0' > "$workdir/sentryusb.conf"
[ "$(archive_verify_parallel)" = 1 ] || fail "parallel setting below 1 not raised to 1"
: > "$workdir/sentryusb.conf"
[ "$(archive_verify_parallel)" = 32 ] || fail "parallel default should be 32"
# Full read-back of many clips at once: every present clip hashed once, an
# absent one left out (the caller treats it as absent).
mkdir -p "$workdir/share/par"
: > "$workdir/par-list"
for n in $(seq 1 20); do
  head -c 5000 /dev/urandom > "$workdir/share/par/c$n.mp4"
  echo "par/c$n.mp4" >> "$workdir/par-list"
done
echo "par/not-there.mp4" >> "$workdir/par-list"
got=$(archive_remote_sha256 "$workdir/par-list" | sort -k2)
want=$(cd "$workdir/share" && for n in $(seq 1 20); do s=$(sha256sum < "par/c$n.mp4"); printf '%s\t%s\n' "${s%% *}" "par/c$n.mp4"; done | sort -k2)
[ "$got" = "$want" ] || fail "parallel read-back hashes wrong"
# Quick-check fingerprints of the share, several at once, in any order.
: > "$workdir/qpar"
for n in 1 2 3; do printf 'par/c%s.mp4\tL%s\t-\n' "$n" "$n" >> "$workdir/qpar"; done
printf 'par/not-there.mp4\tL9\t-\n' >> "$workdir/qpar"
out=$(archive_remote_quick_fingerprints "$workdir/qpar" | sort)
[ "$(grep -c $'\twhole:5000:' <<< "$out")" = 3 ] || fail "parallel quick fingerprints wrong: $out"
grep -q $'^par/not-there.mp4\tL9\t-\tABSENT$' <<< "$out" || fail "absent clip should be ABSENT: $out"
rm -rf "$workdir/share/par"

# --- basic check (ARCHIVE_VERIFY_MODE=basic): size and frames --------------
# A minimal MP4: ftyp, mdat, and a moov with one video track whose stsd entry
# is $2, $3 frames, $4 ms long (timescale 1000), frame sizes seeded by $5.
basic_mp4() {  # file codec frames ms seed
  python3 - "$@" <<'PYMP4'
import random, struct, sys
path, codec, frames, ms, seed = sys.argv[1], sys.argv[2].encode(), int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5])
def box(t, body): return struct.pack(">I", 8 + len(body)) + t + body
rnd = random.Random(seed)
sizes = [rnd.randint(500, 5000) for _ in range(frames)]
stsz = box(b"stsz", b"\0" * 4 + struct.pack(">II", 0, frames) + b"".join(struct.pack(">I", x) for x in sizes))
stsd = box(b"stsd", b"\0" * 4 + struct.pack(">I", 1) + box(codec, b"\0" * 78))
mdhd = box(b"mdhd", b"\0" * 12 + struct.pack(">II", 1000, ms) + b"\0" * 4)
hdlr = box(b"hdlr", b"\0" * 8 + b"vide" + b"\0" * 13)
moov = box(b"moov", box(b"trak", box(b"mdia", mdhd + hdlr + box(b"minf", box(b"stbl", stsd + stsz)))))
mdat = box(b"mdat", bytes(rnd.getrandbits(8) for _ in range(150000)))
open(path, "wb").write(box(b"ftyp", b"isom\0\0\0\0isom") + mdat + moov)
PYMP4
}
basic_clip() {  # hh -> relpath; Pi copy in a snapshot, identical copy on the share
  local name="2026-11-06_$1-00-00-front.mp4" rel="RecentClips/2026-11-06/2026-11-06_$1-00-00-front.mp4"
  mkdir -p "$snaps/snap-0001$1/mnt/TeslaCam/RecentClips" "$lower/RecentClips/2026-11-06"
  basic_mp4 "$snaps/snap-0001$1/mnt/TeslaCam/RecentClips/$name" avc1 100 3000 "$1"
  echo "$(stat -c %s "$snaps/snap-0001$1/mnt/TeslaCam/RecentClips/$name") TeslaCam/RecentClips/$name" > "$snaps/snap-0001$1/snap.bin.toc"
  : > "$snaps/snap-0001$1/snap.bin"
  ln -s "$snaps/snap-0001$1/mnt/TeslaCam/RecentClips/$name" "$lower/$rel"
  mkdir -p "$workdir/share/${rel%/*}"
  cp -L "$lower/$rel" "$workdir/share/$rel"
  echo "$rel" >> "$workdir/mutable/sentry_files_archived"
  echo "$rel"
}
echo 'export ARCHIVE_VERIFY_MODE=basic ARCHIVE_AUTO_DELETE_VERIFIED=true' > "$workdir/sentryusb.conf"
archive_verify_basic_mode || fail "basic mode not detected"
# Even with Manager's record on the share, basic mode doesn't wait for it.
mkdir -p "$workdir/share/.sentrymanager"; : > "$workdir/share/$ARCHIVE_MANAGER_CHECKSUMS"
logs=""
b1=$(basic_clip 01)   # identical
b2=$(basic_clip 02)   # one frame size changed in the server copy's index
b3=$(basic_clip 03)   # cut off
b4=$(basic_clip 04)   # Manager's H.265 re-encode, same frames and duration
b5=$(basic_clip 05)   # re-encode that dropped frames
b6=$(basic_clip 06)   # not on the server
basic_mp4 "$workdir/share/$b2" avc1 100 3000 99
truncate -s 100000 "$workdir/share/$b3"
basic_mp4 "$workdir/share/$b4" hvc1 100 3040 7
basic_mp4 "$workdir/share/$b5" hvc1 90 2700 7
rm -f "$workdir/share/$b6"
# Manager's one-frame cases (100 frames, 30 ms each): one frame fewer with the
# total duration kept passes; one frame fewer and a frame's length short (a
# lost last frame) doesn't; two frames fewer doesn't.
[ "$(archive_basic_result mp4:1:avc1:100:3000:a mp4:2:hvc1:99:3000:b)" = converted_basic ] || fail "one frame fewer, same duration, should pass"
[ "$(archive_basic_result mp4:1:avc1:100:3000:a mp4:2:hvc1:101:3010:b)" = converted_basic ] || fail "one frame more within half a frame should pass"
[ "$(archive_basic_result mp4:1:avc1:100:3000:a mp4:2:hvc1:99:2970:b)" = converted_unconfirmed ] || fail "lost last frame should not pass"
[ "$(archive_basic_result mp4:1:avc1:100:3000:a mp4:2:hvc1:98:3000:b)" = converted_unconfirmed ] || fail "two frames apart should not pass"
[ "$(archive_basic_result mp4:1:avc1:100:3000:a mp4:2:avc1:99:3000:b)" = mismatch ] || fail "same codec with a frame missing is damage, not a re-encode"
: > "$ARCHIVE_LOCAL_SUMS"
verify_archived_clips
[ "$(status_of "$b1")" = ok_basic/1 ] || fail "identical clip should pass the basic check, got $(status_of "$b1")"
[ "$(status_of "$b2")" = mismatch/0 ] || fail "changed frame sizes should be a mismatch, got $(status_of "$b2")"
[ "$(status_of "$b3")" = mismatch/0 ] || fail "cut-off copy should be a mismatch, got $(status_of "$b3")"
[ "$(status_of "$b4")" = converted_basic/1 ] || fail "re-encode with the same frames should be converted_basic, got $(status_of "$b4")"
[ "$(status_of "$b5")" = converted_unconfirmed/0 ] || fail "re-encode with fewer frames should be unconfirmed, got $(status_of "$b5")"
[ "$(status_of "$b6")" = missing/0 ] || fail "absent clip should be missing, got $(status_of "$b6")"
ls "$workdir/share/${b2}".mismatch-* >/dev/null 2>&1 || fail "mismatched copy not renamed aside"
ls "$workdir/share/${b4}".mismatch-* >/dev/null 2>&1 && fail "a re-encode must never be renamed aside"
grep -q "$b2" "$workdir/mutable/sentry_files_archived" && fail "mismatched clip should be re-sent"
grep -q "$b5" "$workdir/mutable/sentry_files_archived" || fail "unconfirmed re-encode must not be re-sent"
grep -q "basic:mp4:" "$ARCHIVE_VERIFY_LEDGER" || fail "basic fingerprint not stored in the ledger"
[ ! -s "$ARCHIVE_LOCAL_SUMS" ] || fail "basic mode should not pre-hash or read clips whole"
grep -q "matched by size and frames" <<< "$logs" || fail "summary should count basic matches: $logs"
grep -q "frames don't match the Pi's copy (Pi: avc1, 100 frames, 3.000 s, [0-9]* bytes; server: hvc1, 90 frames, 2.700 s, [0-9]* bytes)" <<< "$logs" ||
  fail "unconfirmed re-encode should log both sides' frames: $logs"
grep -q "match the Pi's copy (size and frames)" <<< "$logs" || fail "mismatch log should name the basic check: $logs"
grep -q "against the archive server (size and frames)" <<< "$logs" || fail "start log should name the basic check: $logs"
# Second check an hour later counts twice; with the Pi copy gone, the stored
# fingerprint stands in for it.
CLOCK=$((CLOCK + 10))
awk -F'\t' -v OFS='\t' -v r="$b1" '$1 == r { $4 = $4 - 7200 } { print }' "$ARCHIVE_VERIFY_LEDGER" > "$ARCHIVE_VERIFY_LEDGER.t" && mv "$ARCHIVE_VERIFY_LEDGER.t" "$ARCHIVE_VERIFY_LEDGER"
rm -f "$snaps/snap-000101/mnt/TeslaCam/RecentClips/2026-11-06_01-00-00-front.mp4"
queue_archive_rechecks
grep -q "$b1" "$ARCHIVE_VERIFY_QUEUE" || fail "basic match not queued for its second check"
verify_archived_clips
[ "$(status_of "$b1")" = ok_basic/2 ] || fail "second basic check should count twice, got $(status_of "$b1")"
# A clip first checked when the snapshot it was sent from was already gone
# ("nosource") is checked again from its copy in a newer snapshot.
b7=$(basic_clip 07)
b7_new="$snaps/snap-000107/mnt/TeslaCam/RecentClips/2026-11-06_07-00-00-front.mp4"
printf '%s\t-\t-\t%s\tnosource\t0\t%s\n' "$b7" "$(date +%s)" "$snaps/snap-000099/gone/2026-11-06_07-00-00-front.mp4" >> "$ARCHIVE_VERIFY_LEDGER"
# One that really has no copy left anywhere stays as it is.
printf '%s\t-\t-\t%s\tnosource\t0\t%s\n' "RecentClips/2026-11-06/2026-11-06_08-00-00-front.mp4" "$(date +%s)" "$snaps/snap-000099/gone/x.mp4" >> "$ARCHIVE_VERIFY_LEDGER"
[ "$(archive_current_source "$b7")" = "$b7_new" ] || fail "current source not found through the link farm: $(archive_current_source "$b7" || true)"
archive_current_source RecentClips/2026-11-06/2026-11-06_08-00-00-front.mp4 >/dev/null && fail "a clip with no link has no current source"
grep -v "^$b7"$'\t' "$workdir/mutable/sentry_files_archived" > "$workdir/sfa" && mv "$workdir/sfa" "$workdir/mutable/sentry_files_archived"
echo "$b7" >> "$workdir/mutable/sentry_files_archived"
rm -f "$ARCHIVE_VERIFY_QUEUE"
logs=""
requeue_nosource_with_copy
grep -q "^$b7"$'\t'"$b7_new"$'\t' "$ARCHIVE_VERIFY_QUEUE" || fail "nosource clip with a newer copy not re-queued"
grep -q "08-00-00" "$ARCHIVE_VERIFY_QUEUE" && fail "nosource clip with no copy re-queued"
grep -q "still has them in a newer snapshot" <<< "$logs" || fail "re-queue not logged: $logs"
verify_archived_clips
[ "$(status_of "$b7")" = ok_basic/1 ] || fail "re-queued clip should verify from its newer copy, got $(status_of "$b7")"
[ "$(awk -F'\t' -v r="$b7" '$1 == r { s = $7 } END { print s }' "$ARCHIVE_VERIFY_LEDGER")" = "$b7_new" ] || fail "ledger should record the newer copy as the source"
# A queue line whose source snapshot is gone is pointed at the current copy.
printf '%s\t%s\t-\t-\t1\nRecentClips/x/none.mp4\t/nowhere/none.mp4\t-\t-\t1\nbare-rel-only\n' "$b7" "$snaps/snap-000099/gone/y.mp4" > "$workdir/rb-batch"
archive_resolve_batch_sources "$workdir/rb-batch"
[ "$(sed -n 1p "$workdir/rb-batch")" = "$b7"$'\t'"$b7_new"$'\t-\t-\t1' ] || fail "batch source not resolved: $(sed -n 1p "$workdir/rb-batch")"
[ "$(sed -n 2p "$workdir/rb-batch")" = $'RecentClips/x/none.mp4\t/nowhere/none.mp4\t-\t-\t1' ] || fail "unresolvable source should stay as it was"
[ "$(sed -n 3p "$workdir/rb-batch")" = "bare-rel-only" ] || fail "a line without a source should stay as it was"
# Two basic matches release the snapshot (auto-delete is on).
grep -qx snap-000101 "$workdir/released" || fail "snapshot verified twice by the basic check wasn't released"
# Switching to full doesn't treat a stored basic fingerprint as a SHA-256:
# with the Pi copy gone there's nothing to compare, so nothing is renamed.
rm -f "$workdir/share/$ARCHIVE_MANAGER_CHECKSUMS"   # (or the clip would wait for Manager)
echo 'export ARCHIVE_VERIFY_MODE=full' > "$workdir/sentryusb.conf"
logs=""
printf '%s\t%s\t1\tbasic:mp4:1:avc1:1:1:x\t%s\n' "$b1" "$snaps/snap-000101/mnt/TeslaCam/RecentClips/2026-11-06_01-00-00-front.mp4" "$(date +%s)" > "$ARCHIVE_VERIFY_QUEUE"
verify_archived_clips
grep -q "1 with no Pi copy left to compare" <<< "$logs" || fail "full mode with only a basic fingerprint should be nosource: $logs"
ls "$workdir/share/${b1}".mismatch-* >/dev/null 2>&1 && fail "full mode renamed a clip aside over a basic fingerprint"
echo 'export ARCHIVE_AUTO_DELETE_VERIFIED=true' > "$workdir/sentryusb.conf"

# --- pre-hashing: the Pi's half is done after the snapshot, then reused ---
echo 'export ARCHIVE_AUTO_DELETE_VERIFIED=true' > "$workdir/sentryusb.conf"
mkdir -p "$lower/RecentClips/2026-11-08" "$workdir/share/RecentClips/2026-11-08"
pre_rel=RecentClips/2026-11-08/2026-11-08_01-00-00-front.mp4
make_snap snap-000030 2026-11-08_01-00-00-front.mp4
pre_src="$snaps/snap-000030/mnt/TeslaCam/RecentClips/2026-11-08_01-00-00-front.mp4"
ln -s "$pre_src" "$lower/$pre_rel"
precompute_local_hashes
grep -qF "$pre_src" "$ARCHIVE_LOCAL_SUMS" || fail "new clip not pre-hashed"
[ "$(awk -F'\t' -v p="$pre_src" '$1 == p { print $3 }' "$ARCHIVE_LOCAL_SUMS")" = "$(sha256sum < "$pre_src" | cut -c1-64)" ] ||
  fail "pre-hashed checksum is wrong"
# A second run doesn't hash it again.
before=$(wc -l < "$ARCHIVE_LOCAL_SUMS"); precompute_local_hashes
[ "$(wc -l < "$ARCHIVE_LOCAL_SUMS")" = "$before" ] || fail "clip hashed twice"
# While an archive cycle runs, pre-hashing stops.
pre2_rel=RecentClips/2026-11-08/2026-11-08_02-00-00-front.mp4
make_snap snap-000031 2026-11-08_02-00-00-front.mp4
ln -s "$snaps/snap-000031/mnt/TeslaCam/RecentClips/2026-11-08_02-00-00-front.mp4" "$lower/$pre2_rel"
: > "$ARCHIVE_CONTROL_DIR/archive-cycle"
precompute_local_hashes
grep -q "snap-000031" "$ARCHIVE_LOCAL_SUMS" && fail "pre-hashed during an archive cycle"
rm -f "$ARCHIVE_CONTROL_DIR/archive-cycle"
# Verification uses the stored checksum and doesn't read the Pi's copy:
# a stored hash that differs from the real one decides the result, and a
# guard catches any sha256sum of that file.
cp -L "$lower/$pre_rel" "$workdir/share/$pre_rel"
echo "$pre_rel" >> "$workdir/mutable/sentry_files_archived"
echo 'export ARCHIVE_VERIFY_MODE=full' > "$workdir/sentryusb.conf"
sed -i "s|^\($pre_src\t[0-9]*\t\)[0-9a-f]*|\1$(printf 'a%.0s' {1..64})|" "$ARCHIVE_LOCAL_SUMS"
rm -f "$workdir/read-pi-copy"
ionice() {
  while [ "${1:-}" != "${1#-}" ]; do shift; done
  if [ "$1" = nice ] && [ "${3:-}" = sha256sum ] && [ "$(readlink -f "/proc/$BASHPID/fd/0")" = "$(readlink -f "$pre_src")" ]
  then touch "$workdir/read-pi-copy"; fi
  "$@"
}
verify_archived_clips
ionice() { while [ "${1:-}" != "${1#-}" ]; do shift; done; "$@"; }
[ ! -e "$workdir/read-pi-copy" ] || fail "re-read the Pi's copy although it was pre-hashed"
[ "$(status_of $pre_rel | cut -d/ -f1)" = mismatch ] || fail "stored checksum wasn't used (got $(status_of $pre_rel))"
# A released snapshot's lines are dropped on the next run.
rm -rf "$snaps/snap-000030"
precompute_local_hashes
grep -q "snap-000030" "$ARCHIVE_LOCAL_SUMS" && fail "lines for a released snapshot kept"
echo 'export ARCHIVE_AUTO_DELETE_VERIFIED=true' > "$workdir/sentryusb.conf"

# --- progress file: written during and after a pass -------------------------
prog=$(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print(d["running"], d["checked"], d["total"], d["stopped"])' "$ARCHIVE_VERIFY_PROGRESS") ||
  fail "progress file missing or not JSON"
[ "${prog%% *}" = False ] || fail "progress should say not running after a pass: $prog"

# --- Dashboard work status --------------------------------------------------
[ ! -e "$ARCHIVE_WORK_PROGRESS" ] || fail "work status left behind after a verification pass"
wp() { python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print(d[sys.argv[2]])' "$ARCHIVE_WORK_PROGRESS" "$1"; }
ARCHIVE_WORK_PHASE=""
archive_work_progress verifying "Verifying archived clips" 0 400
[ "$(wp eta_seconds)" = None ] || fail "no estimate before anything is done"
ARCHIVE_WORK_STARTED=$(( $(date +%s) - 100 ))
archive_work_progress verifying "Verifying archived clips" 100 400 "100 via Sentry Manager's record"
[ "$(wp done)/$(wp total)" = "100/400" ] || fail "work status counts wrong"
[ "$(wp eta_seconds)" -ge 299 ] && [ "$(wp eta_seconds)" -le 301 ] || fail "estimate should be ~300 s, got $(wp eta_seconds)"
[ "$(wp detail)" = "100 via Sentry Manager's record" ] || fail "detail wrong: $(wp detail)"
# Another phase finishing doesn't erase this one's status.
archive_work_done prehashing
[ -e "$ARCHIVE_WORK_PROGRESS" ] || fail "pre-hash finishing erased the verification status"
archive_work_done verifying
[ ! -e "$ARCHIVE_WORK_PROGRESS" ] || fail "verification status not cleared"
printf 'a\t1\th\t1\tconverted\t1\tx\nb\t1\th\t1\tok_quick\t1\tx\nc\t1\th\t1\tok_quick\t1\tx\nd\t1\th\t1\tmismatch\t0\tx\n' > "$workdir/methods"
[ "$(archive_verify_methods "$workdir/methods")" = "1 via Sentry Manager's record · 2 by quick check · 1 to re-send" ] ||
  fail "methods summary wrong: $(archive_verify_methods "$workdir/methods")"
# Clips waiting for Manager's line are named in the summary and count as gone through.
printf 'e\tsrc\t1\th\t1\nf\tsrc\t1\th\t1\n' > "$workdir/waiting"
[ "$(archive_verify_methods "$workdir/methods" "$workdir/waiting")" = "1 via Sentry Manager's record · 2 by quick check · 1 to re-send · 2 waiting for Sentry Manager's checksum" ] ||
  fail "summary with waiting clips wrong: $(archive_verify_methods "$workdir/methods" "$workdir/waiting")"
[ "$(archive_verify_methods /dev/null "$workdir/waiting")" = "2 waiting for Sentry Manager's checksum" ] ||
  fail "summary with only waiting clips wrong"
[ "$(archive_verify_methods "$workdir/methods" "$workdir/no-such-file")" = "1 via Sentry Manager's record · 2 by quick check · 1 to re-send" ] ||
  fail "a missing waiting list should add nothing"
saved_work=$ARCHIVE_VERIFY_WORK
ARCHIVE_VERIFY_WORK="$workdir/gone-through"
mkdir -p "$ARCHIVE_VERIFY_WORK"
[ "$(archive_verify_gone_through)" = 0 ] || fail "empty pass should count 0"
cp "$workdir/methods" "$ARCHIVE_VERIFY_WORK/results-all"
cp "$workdir/waiting" "$ARCHIVE_VERIFY_WORK/deferred"
[ "$(archive_verify_gone_through)" = 6 ] || fail "checked + waiting should be 6, got $(archive_verify_gone_through)"
ARCHIVE_VERIFY_WORK=$saved_work

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
