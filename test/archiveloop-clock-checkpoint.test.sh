#!/bin/bash
# Tests for save_clock_checkpoint (run/archiveloop): keeps fake-hwclock's
# saved time current on Pis without a battery-backed RTC, never moving it
# backwards.
set -euo pipefail

script=${1:-run/archiveloop}
workdir=$(mktemp -d)
trap 'rm -rf "$workdir"' EXIT
src=$(awk '$0 ~ "^function save_clock_checkpoint[ (]" {keep=1} keep {print} keep && /^}/ {exit}' "$script")
[ -n "$src" ] || { echo "save_clock_checkpoint not found in $script" >&2; exit 1; }
eval "$src"

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

if [ "$failures" -gt 0 ]; then
  echo "$failures failure(s)" >&2
  exit 1
fi
echo "clock checkpoint tests passed"
