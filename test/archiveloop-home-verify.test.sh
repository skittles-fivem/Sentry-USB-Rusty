#!/bin/bash
# Tests for home_verify_backlog (run/archiveloop): keep checking the
# verification queue while parked at home, in cancellable slices.
set -euo pipefail

script=${1:-run/archiveloop}
workdir=$(mktemp -d)
trap 'rm -rf "$workdir"' EXIT
src=$(awk '$0 ~ "^function home_verify_backlog[ (]" {keep=1} keep {print} keep && /^}/ {exit}' "$script")
[ -n "$src" ] || { echo "home_verify_backlog not found in $script" >&2; exit 1; }
src=${src//\/root\/bin\/connect-archive.sh/fake_connect}
src=${src//\/root\/bin\/disconnect-archive.sh/fake_disconnect}
eval "$src"

failures=0
fail() { echo "FAIL: $*" >&2; failures=$((failures + 1)); }
LOG_FILE="$workdir/log"
log() { echo "$*" >> "$LOG_FILE"; }
ARCHIVE_VERIFY_QUEUE="$workdir/queue"
SHARE_STATUS_DIR=.sentryusb SHARE_STATUS_NAME=archive-status.json

# Stubs: each verify slice checks up to $per_slice queue lines.
verify_on=true reachable=true cancel_after=0 connect_ok=true per_slice=3 slices=0 cycles=0 status_writes=0 disconnects=0
archive_verify_enabled() { $verify_on; }
work_shown="" work_cleared=""
archive_work_progress() { work_shown="$1:$2:$4"; }
archive_work_done() { work_cleared+="$1 "; }
archive_is_reachable() { $reachable; }
archive_run_command() { "$@"; }
fake_connect() { $connect_ok; }
fake_disconnect() { disconnects=$((disconnects + 1)); }
archive_cycle_begin() { cycles=$((cycles + 1)); }
archive_cycle_end() { [ "$cancel_after" = 0 ] || [ "$slices" -lt "$cancel_after" ]; }
write_share_status() { status_writes=$((status_writes + 1)); }
sleeps=0
sleep() { sleeps=$((sleeps + 1)); }
waiting_slices=0   # this many first slices find only clips waiting for Manager
verify_archived_clips() {
  slices=$((slices + 1))
  if [ "$slices" -le "$waiting_slices" ]; then
    ARCHIVE_VERIFY_MATCHED=0 ARCHIVE_VERIFY_WAITING=$(wc -l < "$ARCHIVE_VERIFY_QUEUE")
    return 0
  fi
  local n
  n=$(wc -l < "$ARCHIVE_VERIFY_QUEUE")
  [ "$n" -le "$per_slice" ] || n=$per_slice
  ARCHIVE_VERIFY_MATCHED=$n
  sed -i "1,${n}d" "$ARCHIVE_VERIFY_QUEUE"
  [ -s "$ARCHIVE_VERIFY_QUEUE" ] || rm -f "$ARCHIVE_VERIFY_QUEUE"
}
reset() { seq 1 "$1" > "$ARCHIVE_VERIFY_QUEUE"; slices=0 cycles=0 status_writes=0 disconnects=0; : > "$LOG_FILE"; }

# Drains the whole backlog in slices, one cycle each, status file updated.
reset 8
home_verify_backlog
[ ! -e "$ARCHIVE_VERIFY_QUEUE" ] || fail "queue not drained: $(wc -l < "$ARCHIVE_VERIFY_QUEUE") left"
[ "$slices" = 3 ] && [ "$cycles" = 3 ] && [ "$disconnects" = 3 ] && [ "$status_writes" = 3 ] ||
  fail "expected 3 slices/cycles/disconnects/status writes, got $slices/$cycles/$disconnects/$status_writes"
grep -q "Parked at home: checking more of the archive backlog (8 file(s) queued)" "$LOG_FILE" || fail "no start line: $(cat "$LOG_FILE")"

# The car leaves: stops before starting another slice.
reset 8
reachable=false
home_verify_backlog
[ "$slices" = 0 ] || fail "checked while the archive was unreachable"
reachable=true

# Cancel from the web UI stops after the current slice.
reset 8
cancel_after=1
home_verify_backlog
[ "$slices" = 1 ] || fail "kept going after cancel ($slices slices)"
grep -q "stopped by user" "$LOG_FILE" || fail "cancel not logged"
cancel_after=0

# A slice that checks nothing (server unreadable) doesn't spin.
reset 8
per_slice=0
home_verify_backlog
[ "$slices" = 1 ] || fail "spun on a slice with no progress ($slices slices)"
grep -q "made no progress" "$LOG_FILE" || fail "no-progress stop not logged"
per_slice=3

# Only clips waiting for Sentry Manager's checksum: waits and tries again
# (instead of giving up until the next trip home), then drains.
reset 4
waiting_slices=1 sleeps=0
ARCHIVE_VERIFY_MANAGER_POLL_SECONDS=60
home_verify_backlog
[ ! -e "$ARCHIVE_VERIFY_QUEUE" ] || fail "didn't come back for the waiting clips"
[ "$sleeps" = 2 ] || fail "expected two 30 s waits, got $sleeps"
grep -q "Waiting for Sentry Manager's checksums for 4 file(s)" "$LOG_FILE" || fail "wait not logged: $(cat "$LOG_FILE")"
[ "$work_shown" = "waiting_manager:Waiting for Sentry Manager's checksums:4" ] || fail "dashboard not told about the wait: $work_shown"
[[ "$work_cleared" == *waiting_manager* ]] || fail "wait status not cleared after the wait"
# ...but leaves if the car does while waiting.
reset 4
waiting_slices=5 sleeps=0
sleep() { sleeps=$((sleeps + 1)); reachable=false; }
home_verify_backlog
[ "$slices" = 1 ] || fail "kept going after the car left during the wait"
reachable=true waiting_slices=0
sleep() { sleeps=$((sleeps + 1)); }

# Can't connect: stops, retries next cycle.
reset 8
connect_ok=false
home_verify_backlog
[ "$slices" = 0 ] || fail "verified without a connection"
connect_ok=true

# Verification off, or nothing queued: does nothing.
reset 8
verify_on=false
home_verify_backlog
[ "$cycles" = 0 ] || fail "ran with verification off"
verify_on=true
rm -f "$ARCHIVE_VERIFY_QUEUE"; slices=0
home_verify_backlog
[ "$slices" = 0 ] || fail "ran with an empty queue"

if [ "$failures" -gt 0 ]; then
  echo "$failures failure(s)" >&2
  exit 1
fi
echo "home backlog verification tests passed"
