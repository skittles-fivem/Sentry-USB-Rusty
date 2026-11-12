#!/bin/bash
# The archive Live Activity must stay up through drive processing: no end
# when the copy finishes, processing progress while the processor runs, and
# exactly one end per started activity on every way out of the cycle.
# The stubs and variables below are used by the archiveloop and
# post-archive-process.sh code eval'd in, which shellcheck can't see.
# shellcheck disable=SC2016,SC2034,SC2329
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
log() { :; }
# Cancellation has its own test (archive-cancel.test.sh); run commands directly.
archive_run_command() { "$@"; }
archive_cancel_requested() { return 1; }

# The copy step itself never ends the activity; it hands over to processing.
copy_step=$(awk '/^function archive_teslacam_clips / {keep=1} /^function / && !/archive_teslacam_clips/ && keep {exit} keep {print}' "$root/run/archiveloop")
! grep -q 'send-live-activity end' <<< "$copy_step" || { echo 'copy step ends the Live Activity'; exit 1; }
grep -q 'send-live-activity update processing 0 0 0 0 high' <<< "$copy_step"

# ── archive_clips with hardware and network boundaries stubbed ──
eval "$(awk '/^function archive_wait_for_processor / {keep=1} /^function rtc_sync_systohc / {exit} keep {print}' "$root/run/archiveloop" | sed "s@/root/bin/@$work/bin/@g; s@/mutable/@$work/mutable/@g")"
mkdir -p "$work/bin" "$work/mutable"
for cmd in connect-archive.sh disconnect-archive.sh; do
  printf '#!/bin/bash\n' > "$work/bin/$cmd"
  chmod +x "$work/bin/$cmd"
done
printf '#!/bin/bash\necho "activity $*" >> "%s"\n' "$work/calls" > "$work/bin/send-live-activity"
printf '#!/bin/bash\necho "processing active=${LIVE_ACTIVITY_ACTIVE:-unset}" >> "%s"\n' "$work/calls" > "$work/bin/post-archive-process.sh"
chmod +x "$work/bin/send-live-activity" "$work/bin/post-archive-process.sh"
has_cam_disk() { return 0; }
archive_teslacam_clips() { LIVE_ACTIVITY_ACTIVE=1; echo copied >> "$work/calls"; return 0; }
archive_is_reachable() { return 0; }
clear_archive_status() { :; }
cloud_upload_step() { echo cloud >> "$work/calls"; }
travel_mode_active() { return 1; }
# Archive verification and the share status file have their own tests
# (archiveloop-archive-verify.test.sh); here they only need to exist.
verify_archived_clips() { :; }
write_share_status() { :; }
write_device_status() { :; }
SHARE_STATUS_DIR=.sentryusb SHARE_STATUS_NAME=archive-status.json
ensure_usb_drives_connected() { :; }
MUSIC_MOUNT="$work/no-music"
LOG_FILE="$work/log"

run_cycle() {
  : > "$work/calls"
  local rc=0
  archive_clips || rc=$?
  return "$rc"
}

ARCHIVE_RETRY_ATTEMPT=0
run_cycle
expected=$'copied\nprocessing active=1\ncloud\nactivity end complete'
[[ $(cat "$work/calls") == "$expected" ]] || { echo "unexpected cycle:"; cat "$work/calls"; exit 1; }

# Driving away mid-archive skips processing, so the activity ends there.
archive_is_reachable() { return 1; }
run_cycle || true
[[ $(cat "$work/calls") == $'copied\nactivity end complete' ]] || { echo 'drove-away cycle left the activity up'; cat "$work/calls"; exit 1; }

# Retry attempts never started an activity, so they never end one.
archive_is_reachable() { return 0; }
archive_teslacam_clips() { echo copied >> "$work/calls"; return 0; }
ARCHIVE_RETRY_ATTEMPT=1
run_cycle
[[ $(cat "$work/calls") == $'copied\nprocessing active=0\ncloud' ]] || { echo 'retry cycle sent an unmatched end'; cat "$work/calls"; exit 1; }

# ── post-archive-process.sh progress updates, against a scripted processor ──
# The script runs with `set -uo pipefail` (no -e); match it.
set +e
eval "$(awk '/^ACTIVITY_MIN_GAP=/ {keep=1} /^# Process RecentClips/ {exit} keep {print}' "$root/run/post-archive-process.sh" | sed "s@/tmp/@$work/@g; s@/root/bin/@$work/bin/@g")"
API_URL=http://api
ARCHIVE_CYCLE_QUERY=
clock=1000
date() { echo "$clock"; }
sleep() { clock=$((clock + $1)); }
# One status per 5 s poll: counting (total 0), then 0/10, 3/10 held for 35 s, done.
statuses=(
  '{"running":true,"process_total":0}'
  '{"running":true,"process_total":0}'
  '{"running":true,"process_current":0,"process_total":10,"process_eta_seconds":50}'
  '{"running":true,"process_current":3,"process_total":10,"process_eta_seconds":35}'
  '{"running":true,"process_current":3,"process_total":10,"process_eta_seconds":30}'
  '{"running":true,"process_current":3,"process_total":10,"process_eta_seconds":25}'
  '{"running":true,"process_current":3,"process_total":10,"process_eta_seconds":20}'
  '{"running":true,"process_current":3,"process_total":10,"process_eta_seconds":15}'
  '{"running":true,"process_current":3,"process_total":10,"process_eta_seconds":10}'
  '{"running":true,"process_current":3,"process_total":10,"process_eta_seconds":5}'
  '{"running":false,"archive_work_running":false,"process_current":10,"process_total":10}'
)
curl() {
  if [[ " $* " == *" -X POST "* ]]; then
    echo '{"total":10}' > "$work/drive_process_response.json"
    echo 202
    return 0
  fi
  local i
  i=$(cat "$work/poll" 2>/dev/null || echo 0)
  echo $((i + 1)) > "$work/poll"
  echo "${statuses[$i]}"
}

: > "$work/calls"
LIVE_ACTIVITY_ACTIVE=1
process_clips_dir /clips
# Counting shows "Preparing…" (total 0); each count change goes out; the
# unchanged 3/10 stretch sends only the 30 s heartbeat; then the final count.
# A new total goes at high priority.
expected=$'activity update processing 0 0 0 high\nactivity update processing 0 10 50 high\nactivity update processing 3 10 35\nactivity update processing 3 10 5\nactivity update processing 10 10 0'
[[ $(cat "$work/calls") == "$expected" ]] || { echo "unexpected processing updates:"; cat "$work/calls"; exit 1; }

# No activity started this cycle: no updates at all.
: > "$work/calls"
rm -f "$work/poll"
activity_last_sent=0 activity_last_counts=""
LIVE_ACTIVITY_ACTIVE=0
process_clips_dir /clips
[[ ! -s "$work/calls" ]] || { echo 'updates sent without an activity'; exit 1; }

echo 'live activity processing tests passed'
