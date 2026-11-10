#!/bin/bash
# Tests for the car file log (run/archiveloop): car_files_record keeps one
# line per clip the car wrote (and again when it grew), car_files_day_summary
# sums up a day, car_files_daily_summary logs yesterday once, and the
# inventory worker is supervised with the other cam-disk workers.
# Literal code patterns are grepped; date is stubbed and called indirectly.
# shellcheck disable=SC2016,SC2317,SC2015
set -euo pipefail

script=${1:-run/archiveloop}
workdir=$(mktemp -d)
trap 'rm -rf "$workdir"' EXIT
extract() {
  awk -v f="$1" '$0 ~ "^function " f "[ (]" {keep=1} keep {print} keep && /^}/ {exit}' "$script"
}
for fn in car_files_record car_files_day_summary car_files_daily_summary; do
  src=$(extract "$fn")
  [ -n "$src" ] || { echo "$fn not found in $script" >&2; exit 1; }
  eval "$src"
done
failures=0
fail() { echo "FAIL: $*" >&2; failures=$((failures + 1)); }
logs=""
log() { logs+="$*"$'\n'; }
CAR_FILES_LOG="$workdir/car_files.tsv"
CAR_FILES_SUMMARY_STAMP="$workdir/car_files_summary_day"

grep -q '^CAR_FILES_LOG=/mutable/car_files.tsv$' "$script" || fail "log path changed"
grep -q 'car_file_inventory_loop &' "$script" || fail "the inventory worker isn't started"
grep -q 'kill "$snap_pid" "$cfi_pid"' "$script" || fail "the inventory worker isn't restarted with the others"
grep -q '/root/bin/mountimage "$img" "$mnt" -o ro' "$script" || fail "the inventory copy must be mounted read-only"

now=1762800000
list="$workdir/list"
printf '%s\t%s\t%s\n' \
  RecentClips/2026-11-10_12-00-00-front.mp4 30000000 1762790000.5 \
  RecentClips/2026-11-10_12-00-00-back.mp4 31000000 1762790000.5 \
  RecentClips/2026-11-10_12-01-00-front.mp4 1000 1762790060.0 \
  RecentClips/thumb.png 13000 1762790000.0 \
  SentryClips/2026-11-10_13-00-00/2026-11-10_12-55-00-front.mp4 20000000 1762793700.0 \
  SentryClips/2026-11-10_13-00-00/event.json 192 1762793700.0 \
  SavedClips/2026-11-10_14-00-00/2026-11-10_13-59-00-front.mp4 9000000 1762797540.0 > "$list"
car_files_record "$list" "$now"
[ "$(wc -l < "$CAR_FILES_LOG")" = 5 ] || fail "expected 5 clip lines, got $(wc -l < "$CAR_FILES_LOG")"
grep -q 'thumb.png\|event.json' "$CAR_FILES_LOG" && fail "non-clip files logged"
awk -F'\t' -v n="$now" '$1 != n || $4 !~ /^[0-9]+$/ { bad = 1 } END { exit bad }' "$CAR_FILES_LOG" || fail "bad line format: $(cat "$CAR_FILES_LOG")"

# The same listing again: nothing new. A clip that grew: one more line.
car_files_record "$list" $((now + 600))
[ "$(wc -l < "$CAR_FILES_LOG")" = 5 ] || fail "an unchanged listing added lines"
sed -i 's/12-01-00-front.mp4\t1000\t/12-01-00-front.mp4\t32000000\t/' "$list"
car_files_record "$list" $((now + 1200))
[ "$(wc -l < "$CAR_FILES_LOG")" = 6 ] || fail "a grown clip should add one line"
grep -q $'^'"$((now + 1200))"$'\tRecentClips/2026-11-10_12-01-00-front.mp4\t32000000\t' "$CAR_FILES_LOG" || fail "grown clip line wrong"
# A clip the car deleted stays logged (the log is what it wrote).
printf '%s\t%s\t%s\n' RecentClips/2026-11-10_12-02-00-front.mp4 30000000 1762790120.0 > "$list"
car_files_record "$list" $((now + 1800))
grep -q '12-00-00-front.mp4' "$CAR_FILES_LOG" || fail "a clip no longer on the drive was dropped from the log"

# Day summary by the clips' own names.
summary=$(car_files_day_summary 2026-11-10)
[ "$summary" = "Car files for 2026-11-10: 4 RecentClips file(s) covering 3 minute(s), 1 Sentry event(s), 1 saved clip(s)" ] ||
  fail "summary wrong: $summary"
[ "$(car_files_day_summary 2026-11-09)" = "Car files for 2026-11-09: 0 RecentClips file(s) covering 0 minute(s), 0 Sentry event(s), 0 saved clip(s)" ] ||
  fail "empty day summary wrong"

# Yesterday is summed up once, on the first pass of a new day.
echo 2026-11-10 > "$CAR_FILES_SUMMARY_STAMP"
date() { if [ "${1:-}" = +%F ]; then echo 2026-11-11; else command date "$@"; fi; }
logs=""
car_files_daily_summary
grep -q "Car files for 2026-11-10: 4 RecentClips" <<< "$logs" || fail "yesterday not summed up: $logs"
[ "$(cat "$CAR_FILES_SUMMARY_STAMP")" = 2026-11-11 ] || fail "stamp not moved to today"
logs=""
car_files_daily_summary
[ -z "$logs" ] || fail "summed up twice: $logs"
# First run ever: no summary, just the stamp.
rm -f "$CAR_FILES_SUMMARY_STAMP"; logs=""
car_files_daily_summary
[ -z "$logs" ] && [ -s "$CAR_FILES_SUMMARY_STAMP" ] || fail "first run should only set the stamp"
unset -f date

# Lines older than 8 days are dropped.
printf '%s\tRecentClips/2026-10-01_00-00-00-front.mp4\t1\t1\n' $((now - 9 * 86400)) > "$CAR_FILES_LOG.old"
cat "$CAR_FILES_LOG" >> "$CAR_FILES_LOG.old"; mv "$CAR_FILES_LOG.old" "$CAR_FILES_LOG"
: > "$list"
car_files_record "$list" "$now"
grep -q '2026-10-01' "$CAR_FILES_LOG" && fail "a line older than 8 days was kept"

[ "$failures" -eq 0 ] || { echo "$failures failure(s)" >&2; exit 1; }
echo "car file log tests passed"
