#!/bin/bash
# Tests for the drive-absent safety net (run/archiveloop: gadget_absent_check,
# called by gadget_stall_watchdog while the drive isn't presented): a drive
# left disconnected from the car outside an archive is re-presented after
# 5 min, with one alert per outage, but never during an archive cycle, while
# the user has turned the drives off, or during a slow boot.
# The checked function is loaded with eval, so shellcheck sees its globals as unused.
# shellcheck disable=SC2034,SC2015,SC2016
set -euo pipefail

script=${1:-run/archiveloop}
workdir=$(mktemp -d)
trap 'rm -rf "$workdir"' EXIT

extract() {
  awk -v f="$1" '$0 ~ "^function " f "[ (]" {keep=1} keep {print} keep && /^}/ {exit}' "$script"
}
src=$(extract gadget_absent_check)
[ -n "$src" ] || { echo "gadget_absent_check not found in $script" >&2; exit 1; }
eval "${src//\/root\/bin\/send-push-message/push_stub}"
watchdog=$(extract gadget_stall_watchdog)

failures=0
fail() { echo "FAIL: $*" >&2; failures=$((failures + 1)); }

# The watchdog calls the check while the drive is down, and resets the outage
# (and clears a stale "drives off" marker) once the drive is presented.
[[ $watchdog == *'if ! usb_gadget_is_active'*'gadget_absent_check'*'continue'* ]] ||
  fail "the watchdog doesn't call gadget_absent_check while the drive is down"
[[ $watchdog == *'GADGET_PRESENTED_ONCE=1 GADGET_ABSENT_SINCE=0 GADGET_ABSENT_ALERTED=0'* ]] ||
  fail "the watchdog doesn't reset the outage when the drive is presented"
[[ $watchdog == *'rm -f "$DRIVES_USER_OFF_MARKER"'* ]] ||
  fail "the watchdog doesn't clear the drives-off marker when the drive is presented"
grep -q '^DRIVES_USER_OFF_MARKER=/tmp/sentryusb_drives_user_off$' "$script" ||
  fail "marker path differs from the web UI's (crates/api/src/system.rs)"
grep -q 'const DRIVES_USER_OFF_MARKER: &str = "/tmp/sentryusb_drives_user_off";' crates/api/src/system.rs ||
  fail "the web UI doesn't write the same drives-off marker"

logs=""
log() { logs+="$*"$'\n'; }
pushes=0
push_stub() { pushes=$((pushes + 1)); }
present=false
connect_works=true
connects=0
usb_gadget_is_active() { [ "$present" = true ]; }
ensure_usb_drives_connected() { connects=$((connects + 1)); if [ "$connect_works" = true ]; then present=true; else return 1; fi; }
ARCHIVE_CONTROL_DIR="$workdir/ctl"; mkdir -p "$ARCHIVE_CONTROL_DIR"
DRIVES_USER_OFF_MARKER="$workdir/drives_user_off"

reset() { present=false; connects=0; pushes=0; logs=""; GADGET_ABSENT_SINCE=0 GADGET_ABSENT_ALERTED=0; }
t=1000000

# Slow boot: never presented yet, inside the 15 min boot allowance: nothing.
reset; GADGET_PRESENTED_ONCE=0 GADGET_WATCHDOG_STARTED=$t
for dt in 0 300 600 899; do gadget_absent_check $((t + dt)); done
[ "$connects" = 0 ] || fail "a slow boot was interrupted"
# Past the allowance, a drive that was never presented is caught (after the
# usual 5 min of watching).
gadget_absent_check $((t + 900))
gadget_absent_check $((t + 1200))
[ "$connects" = 1 ] && [ "$present" = true ] || fail "a boot that never presented the drive wasn't fixed ($connects)"

# Presented, then disconnected outside an archive: re-presented after 5 min.
reset; GADGET_PRESENTED_ONCE=1
gadget_absent_check $t
gadget_absent_check $((t + 299))
[ "$connects" = 0 ] || fail "re-presented before 5 min"
gadget_absent_check $((t + 300))
[ "$connects" = 1 ] && [ "$present" = true ] || fail "not re-presented after 5 min"
[ "$pushes" = 1 ] || fail "expected one alert, got $pushes"
grep -q "ERROR: the USB drive has been disconnected from the car for 5 min outside an archive; re-presenting it" <<< "$logs" || fail "outage not logged: $logs"
grep -q "USB drive re-presented to the car" <<< "$logs" || fail "recovery not logged: $logs"

# During an archive cycle: left alone however long, timed from its end.
reset; GADGET_PRESENTED_ONCE=1
echo 42 > "$ARCHIVE_CONTROL_DIR/archive-cycle"
for dt in 0 300 3600; do gadget_absent_check $((t + dt)); done
[ "$connects" = 0 ] || fail "re-presented during an archive cycle"
rm -f "$ARCHIVE_CONTROL_DIR/archive-cycle"
gadget_absent_check $((t + 3700))
gadget_absent_check $((t + 3999))
[ "$connects" = 0 ] || fail "not timed from the end of the archive cycle"
gadget_absent_check $((t + 4000))
[ "$connects" = 1 ] || fail "not re-presented 5 min after the archive cycle ended"

# The user turned the drives off: left alone.
reset; GADGET_PRESENTED_ONCE=1
: > "$DRIVES_USER_OFF_MARKER"
for dt in 0 300 3600; do gadget_absent_check $((t + dt)); done
[ "$connects" = 0 ] || fail "re-presented while the user had the drives off"
rm -f "$DRIVES_USER_OFF_MARKER"

# Reconnect keeps failing: retried every 5 min, alerted once.
reset; GADGET_PRESENTED_ONCE=1 connect_works=false
for dt in 0 300 450 600 900; do gadget_absent_check $((t + dt)); done
[ "$connects" = 3 ] || fail "expected retries at 5, 10 and 15 min, got $connects"
[ "$pushes" = 1 ] || fail "a failing outage should alert once, got $pushes"
grep -q "ERROR: could not re-present the USB drive to the car" <<< "$logs" || fail "failed reconnect not logged"
connect_works=true

if [ "$failures" -gt 0 ]
then
  echo "$failures failure(s)" >&2
  exit 1
fi
echo "drive-absent safety net tests passed"
