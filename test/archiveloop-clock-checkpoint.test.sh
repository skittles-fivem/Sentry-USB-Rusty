#!/bin/bash
# Tests for save_clock_checkpoint (run/archiveloop): keeps fake-hwclock's
# saved time current on Pis without a battery-backed RTC, never moving it
# backwards.
set -euo pipefail

script=${1:-run/archiveloop}
workdir=$(mktemp -d)
trap 'rm -rf "$workdir"' EXIT
for fn in save_clock_checkpoint restore_clock_from_checkpoint; do
  src=$(awk -v f="$fn" '$0 ~ "^function " f "[ (]" {keep=1} keep {print} keep && /^}/ {exit}' "$script")
  [ -n "$src" ] || { echo "$fn not found in $script" >&2; exit 1; }
  eval "$src"
done
CLOCK_CHECKPOINT_FILE="$workdir/mutable/clock_checkpoint"
SNAPSHOT_LOG="$workdir/mutable/snapshot_log.tsv"
logs=""
log() { logs+="$*"$'\n'; }

failures=0
fail() { echo "FAIL: $*" >&2; failures=$((failures + 1)); }

mkdir -p "$workdir/mutable/etc"
ln -s "$workdir/mutable/etc/fake-hwclock.data" "$workdir/fake-hwclock.data"
FAKE_HWCLOCK_DATA="$workdir/fake-hwclock.data"
data="$workdir/mutable/etc/fake-hwclock.data"
export RTC_BATTERY_ENABLED=false

# Stale file (the Sep 17 case) is moved forward to now, through the link,
# in fake-hwclock's format.
echo "2026-09-17 20:07:20" > "$data"
save_clock_checkpoint
grep -Eq '^[0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}:[0-9]{2}$' "$data" || fail "bad format: $(cat "$data")"
saved=$(date -u -d "$(cat "$data")" +%s); now=$(date -u +%s)
[ $((now - saved)) -le 5 ] || fail "checkpoint not current: $(cat "$data")"
[ -L "$FAKE_HWCLOCK_DATA" ] || fail "the /etc link was replaced by a file"

# A newer saved time (clock not synced yet this boot) is never overwritten.
echo "2099-01-01 00:00:00" > "$data"
save_clock_checkpoint
[ "$(cat "$data")" = "2099-01-01 00:00:00" ] || fail "moved the checkpoint backwards: $(cat "$data")"

# Missing file: created.
rm -f "$data"
save_clock_checkpoint
[ -s "$data" ] || fail "missing checkpoint not created"

# Battery-backed RTC: left alone.
echo "2026-09-17 20:07:20" > "$data"
RTC_BATTERY_ENABLED=true save_clock_checkpoint
[ "$(cat "$data")" = "2026-09-17 20:07:20" ] || fail "touched the checkpoint with an RTC"

# Unwritable location: fails quietly, never errors out of archiveloop.
FAKE_HWCLOCK_DATA="$workdir/nonexistent-dir/fake-hwclock.data" save_clock_checkpoint || true

# --- archiveloop's own checkpoint in /mutable ------------------------------
# Written even when fake-hwclock's file can't be (the 11/9 case: every boot at
# Sep 17 because /etc/fake-hwclock.data wasn't writable), forward only.
rm -f "$CLOCK_CHECKPOINT_FILE"
FAKE_HWCLOCK_DATA="$workdir/nonexistent-dir/fake-hwclock.data" save_clock_checkpoint || true
[ "$(( $(date -u +%s) - $(cat "$CLOCK_CHECKPOINT_FILE") ))" -le 5 ] || fail "own checkpoint not written when fake-hwclock's can't be"
echo 4102444800 > "$CLOCK_CHECKPOINT_FILE"   # 2100-01-01
save_clock_checkpoint
[ "$(cat "$CLOCK_CHECKPOINT_FILE")" = 4102444800 ] || fail "own checkpoint moved backwards"
echo garbage > "$CLOCK_CHECKPOINT_FILE"
save_clock_checkpoint
[[ "$(cat "$CLOCK_CHECKPOINT_FILE")" =~ ^[0-9]+$ ]] || fail "a corrupt own checkpoint isn't replaced"

# --- restore at startup ------------------------------------------------------
set_to=""
set_system_clock() { set_to=$1; }
now=$(date -u +%s)
# Behind the checkpoint by a day: moved forward to it, and logged.
echo $((now + 86400 + 3600)) > "$CLOCK_CHECKPOINT_FILE"; : > "$SNAPSHOT_LOG"; logs=""
restore_clock_from_checkpoint
[ "$set_to" = $((now + 86400 + 3600)) ] || fail "clock not moved forward to the checkpoint ($set_to)"
grep -q "Clock was 1 day(s) 1 h behind its last checkpoint" <<< "$logs" || fail "restore not logged: $logs"
# The newest snapshot in the snapshot log is a floor too.
set_to=""; echo $((now - 100)) > "$CLOCK_CHECKPOINT_FILE"
printf 'snap-000001\t%s\t1\n-\t%s\t0\n' $((now + 7200)) $((now + 600)) > "$SNAPSHOT_LOG"
restore_clock_from_checkpoint
[ "$set_to" = $((now + 7200)) ] || fail "newest snapshot time not used as the floor ($set_to)"
# Clock already ahead: never moved back.
set_to=""; echo $((now - 3600)) > "$CLOCK_CHECKPOINT_FILE"; : > "$SNAPSHOT_LOG"
restore_clock_from_checkpoint
[ -z "$set_to" ] || fail "clock moved backwards"
# More than a year ahead (corrupt): ignored, with a warning.
set_to=""; logs=""; echo $((now + 400 * 86400)) > "$CLOCK_CHECKPOINT_FILE"
restore_clock_from_checkpoint
[ -z "$set_to" ] || fail "followed a checkpoint more than a year ahead"
grep -q "more than a year ahead" <<< "$logs" || fail "corrupt checkpoint not warned about"
# No files at all: nothing happens.
set_to=""; rm -f "$CLOCK_CHECKPOINT_FILE" "$SNAPSHOT_LOG"
restore_clock_from_checkpoint
[ -z "$set_to" ] || fail "moved the clock with no checkpoint"
# Battery-backed RTC: left alone.
set_to=""; echo $((now + 86400)) > "$CLOCK_CHECKPOINT_FILE"
RTC_BATTERY_ENABLED=true restore_clock_from_checkpoint
[ -z "$set_to" ] || fail "moved the clock despite a battery-backed RTC"
# Runs before archiveloop's first log line.
restore_line=$(grep -n '^restore_clock_from_checkpoint' "$script" | cut -d: -f1)
start_line=$(grep -n '^log "Starting archiveloop' "$script" | cut -d: -f1)
if [ -z "$restore_line" ] || [ "$restore_line" -ge "$start_line" ]; then
  fail "restore doesn't run before archiveloop's first log line"
fi

if [ "$failures" -gt 0 ]; then
  echo "$failures failure(s)" >&2
  exit 1
fi
echo "clock checkpoint tests passed"
