#!/bin/bash
# Tests for the snapshot health check (run/archiveloop: snapshot_health_check,
# run by the car file inventory worker, outside the snapshot loop) and its
# "snapshot_health" status object: a stuck snapshot loop, or runs that miss
# clips the car wrote (the 2026-11-12 stale mount), are caught, reported
# once and alerted.
# Functions are loaded with eval and stubbed, so shellcheck sees them as unused or unreachable.
# shellcheck disable=SC2034,SC2015,SC2016,SC2317
set -euo pipefail

script=${1:-run/archiveloop}
workdir=$(mktemp -d)
trap 'rm -rf "$workdir"' EXIT

extract() {
  awk -v f="$1" '$0 ~ "^function " f "[ (]" {keep=1} keep {print} keep && /^}/ {exit}' "$script"
}
for fn in json_string json_int snapshot_health_check snapshot_health_json
do
  src=$(extract "$fn")
  [ -n "$src" ] || { echo "function $fn not found in $script" >&2; exit 1; }
  src=${src//\/backingfiles/$workdir/backingfiles}
  src=${src//\/root\/bin\/send-push-message/push_stub}
  eval "$src"
done
failures=0
fail() { echo "FAIL: $*" >&2; failures=$((failures + 1)); }
grep -q 'snapshot_health_check "$(date +%s)" || true' "$script" || fail "the inventory worker doesn't run the health check"
grep -q '"snapshot_health": %s' "$script" || fail "the status file doesn't include snapshot_health"

logs=""; log() { logs+="$*"$'\n'; }
pushes=0; push_stub() { pushes=$((pushes + 1)); }
SNAPSHOT_LOG="$workdir/snapshot_log.tsv"
CAR_FILES_LOG="$workdir/car_files.tsv"
SNAPSHOT_HEALTH_FILE="$workdir/health"
SNAPSHOT_HEALTH_ALERTED="$workdir/alerted"
SNAPSHOT_INTERVAL=300
now=2000000000
kept=$((now - 3600))
snap="$workdir/backingfiles/snapshots/snap-000080"
mkdir -p "$snap"
printf '100 TeslaCam/RecentClips/a-front.mp4\n' > "$snap/snap.bin.toc"
touch -d "@$kept" "$snap/snap.bin.toc"
state() { cut -f2 "$SNAPSHOT_HEALTH_FILE"; }

# No snapshot log: nothing judged.
snapshot_health_check $now
[ ! -e "$SNAPSHOT_HEALTH_FILE" ] || fail "judged without a snapshot log"

# Runs every 5 min, the car wrote only what the snapshot holds: ok.
printf 'snap-000080\t%s\t1\n-\t%s\t0\n' $kept $((now - 200)) > "$SNAPSHOT_LOG"
printf '%s\tRecentClips/a-front.mp4\t100\t%s\n' $((kept - 100)) $((kept - 200)) > "$CAR_FILES_LOG"
snapshot_health_check $now
[ "$(state)" = ok ] || fail "expected ok, got $(state)"
[ "$pushes" = 0 ] && [ -z "$logs" ] || fail "ok should be quiet: $logs"
json=$(snapshot_health_json)
[[ $json == *'"state": "ok"'*'"last_run_unix": '$((now - 200))*'"newest_kept_unix": '$kept*'"clips_not_kept": 0, "example_clip": null'* ]] || fail "bad json: $json"
python3 -c 'import json,sys; json.loads(sys.argv[1])' "$json" || fail "invalid json: $json"

# A clip seen after the snapshot but after the latest run too: not judged yet.
printf '%s\tRecentClips/b-front.mp4\t50\t%s\n' $((now - 100)) $((now - 110)) >> "$CAR_FILES_LOG"
snapshot_health_check $now
[ "$(state)" = ok ] || fail "a clip newer than the latest run counted as missed"

# The 11/12 case: clips seen long before discarded runs, not in the snapshot.
printf '%s\tRecentClips/c-front.mp4\t50\t%s\n%s\tRecentClips/c-front.mp4\t90\t%s\n' \
  $((now - 1800)) $((now - 1810)) $((now - 1200)) $((now - 1210)) >> "$CAR_FILES_LOG"
printf '%s\tSentryClips/2033-05-18_00-00-00/event.json\t5\t%s\n' $((now - 1800)) $((now - 1810)) >> "$CAR_FILES_LOG"
snapshot_health_check $now
[ "$(state)" = missed ] || fail "expected missed, got $(state)"
[ "$(cut -f5 "$SNAPSHOT_HEALTH_FILE")" = 1 ] || fail "a grown clip counted twice, or a non-clip counted: $(cat "$SNAPSHOT_HEALTH_FILE")"
grep -q "ERROR: snapshot health: the car wrote 1 clip(s) since the newest kept snapshot .* (e.g. RecentClips/c-front.mp4)" <<< "$logs" || fail "miss not logged: $logs"
[ "$pushes" = 1 ] || fail "a miss should alert once"
json=$(snapshot_health_json)
python3 -c 'import json,sys; d=json.loads(sys.argv[1]); assert d["state"]=="missed" and d["clips_not_kept"]==1 and d["example_clip"]=="RecentClips/c-front.mp4"' "$json" || fail "bad missed json: $json"
# Reported once per problem.
logs=""
snapshot_health_check $now
[ "$pushes" = 1 ] && [ -z "$logs" ] || fail "the same miss reported again"

# A new kept snapshot holding the clip: back to ok, and the alert re-arms.
printf '100 TeslaCam/RecentClips/a-front.mp4\n90 TeslaCam/RecentClips/c-front.mp4\n' > "$snap/snap.bin.toc"
touch -d "@$kept" "$snap/snap.bin.toc"
snapshot_health_check $now
[ "$(state)" = ok ] || fail "clip in the snapshot still counted missed"
[ ! -e "$SNAPSHOT_HEALTH_ALERTED" ] || fail "alert marker not cleared"

# No run for over twice the interval: stalled.
logs=""
snapshot_health_check $((now - 200 + 2 * 300 + 301))
[ "$(state)" = stalled ] || fail "expected stalled, got $(state)"
grep -q "ERROR: snapshot health: no snapshot run since .*the snapshot loop looks stuck" <<< "$logs" || fail "stall not logged: $logs"
[ "$pushes" = 2 ] || fail "a stall should alert"

# A garbled health file leaves the field out.
echo junk > "$SNAPSHOT_HEALTH_FILE"
! snapshot_health_json >/dev/null || fail "garbled health file produced json"

if [ "$failures" -gt 0 ]
then
  echo "$failures failure(s)" >&2
  exit 1
fi
echo "snapshot health tests passed"
