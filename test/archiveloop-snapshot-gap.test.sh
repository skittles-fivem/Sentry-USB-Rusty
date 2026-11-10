#!/bin/bash
# Tests for the snapshot gap check (run/archiveloop: snapshot_gap_check):
# when the newest snapshot is over an hour old, log why once per gap and
# alert, telling apart "the car wrote but nothing kept it", "the drive isn't
# presented / the car isn't using it" and "the car didn't record".
set -euo pipefail

script=${1:-run/archiveloop}
workdir=$(mktemp -d)
trap 'rm -rf "$workdir"' EXIT

extract() {
  awk -v f="$1" '$0 ~ "^function " f "[ (]" {keep=1} keep {print} keep && /^}/ {exit}' "$script"
}
for fn in newest_snapshot_unix snapshot_gap_check
do
  src=$(extract "$fn")
  [ -n "$src" ] || { echo "function $fn not found in $script" >&2; exit 1; }
  src=${src//\/backingfiles/$workdir/backingfiles}
  eval "$src"
done
grep -q '^SNAPSHOT_GAP_MARKER=' "$script" || { echo "SNAPSHOT_GAP_MARKER not defined" >&2; exit 1; }
grep -q 'snapshot_gap_check "$snap_rc" || true' "$script" || { echo "snapshotloop doesn't call snapshot_gap_check" >&2; exit 1; }

failures=0
fail() { echo "FAIL: $*" >&2; failures=$((failures + 1)); }
logs=""
log() { logs+="$*"$'\n'; }
mkdir -p "$workdir/bin"
cat > "$workdir/bin/send-push-message" <<EOF
#!/bin/bash
echo "\$2" >> "$workdir/pushes"
EOF
chmod +x "$workdir/bin/send-push-message"
# The script calls /root/bin/send-push-message; point it at the stub.
eval "$(declare -f snapshot_gap_check | sed "s|/root/bin/send-push-message|$workdir/bin/send-push-message|")"
pushes_count() { [ -e "$workdir/pushes" ] && wc -l < "$workdir/pushes" || echo 0; }

SNAPSHOT_GAP_MARKER="$workdir/gap-marker"
GADGET=active UDC=configured AWAKE=no
usb_gadget_is_active() { [ "$GADGET" = active ]; }
snapshot_gap_udc_state() { echo "$UDC"; }
car_is_awake_via_ble() { [ "$AWAKE" = yes ]; }

snaps="$workdir/backingfiles/snapshots"
mkdir -p "$snaps"
cam="$workdir/backingfiles/cam_disk.bin"
: > "$cam"
now=$(date +%s)
snap_at() {  # n, unix time
  mkdir -p "$snaps/snap-$(printf %06d "$1")"
  : > "$snaps/snap-$(printf %06d "$1")/snap.bin.toc"
  touch -d "@$2" "$snaps/snap-$(printf %06d "$1")/snap.bin.toc"
}

# No snapshots at all: nothing to say.
snapshot_gap_check
[ -z "$logs" ] || fail "no snapshot should log nothing: $logs"

# Newest snapshot 30 min old: no gap, and a stale marker is cleared.
echo stale > "$SNAPSHOT_GAP_MARKER"
snap_at 0 $((now - 7200))
snap_at 1 $((now - 1800))
touch -d "@$((now - 60))" "$cam"
snapshot_gap_check
[ -z "$logs" ] || fail "a 30 min old snapshot is no gap: $logs"
[ ! -e "$SNAPSHOT_GAP_MARKER" ] || fail "marker not cleared when there's no gap"

# Newest 70 min old, the car touched its drive 10 min ago, but the runs
# succeeded (discarded as identical): housekeeping, not footage. Logged
# only (the 2026-11-10 11:22 false alarm).
snap_at 2 $((now - 4200))
touch -d "@$((now - 600))" "$cam"
logs=""
snapshot_gap_check 0
grep -q "WARNING: snapshot gap: no snapshot for 70 min.*every run since .* found no new clip on the drive.*was housekeeping" <<< "$logs" || fail "identical runs with a recent drive write should be housekeeping: $logs"
[ "$(pushes_count)" = 0 ] || fail "identical runs must not alert, even if the car touched its drive"
# The same gap when a run fails: footage may be lost.
rm -f "$SNAPSHOT_GAP_MARKER"
logs=""
snapshot_gap_check 1
grep -q "ERROR: snapshot gap: no snapshot for 70 min" <<< "$logs" || fail "gap not logged as an error: $logs"
grep -q "the last snapshot run failed (exit 1)" <<< "$logs" || fail "wrong reason: $logs"
[ "$(pushes_count)" = 1 ] || fail "a failed run: one alert expected"
# Reported once per gap, however many runs see it.
logs=""
snapshot_gap_check 1
[ -z "$logs" ] || fail "the same gap was reported again: $logs"
[ "$(pushes_count)" = 1 ] || fail "the same gap alerted again"

# A new gap (a newer snapshot, then over an hour again): the drive isn't
# presented to the car.
snap_at 3 $((now - 3700))
touch -d "@$((now - 3800))" "$cam"
GADGET=inactive
logs=""
snapshot_gap_check
grep -q "ERROR: snapshot gap: .*the USB drive isn't presented to the car" <<< "$logs" || fail "unpresented drive not reported: $logs"
[ "$(pushes_count)" = 2 ] || fail "unpresented drive should alert"
GADGET=active

# The car isn't using the drive (USB link not configured).
snap_at 4 $((now - 3900))
touch -d "@$((now - 4000))" "$cam"
UDC=suspended
logs=""
snapshot_gap_check
grep -q "WARNING: snapshot gap: .*the car isn't using the USB drive (USB state 'suspended')" <<< "$logs" || fail "USB link state not reported: $logs"
[ "$(pushes_count)" = 2 ] || fail "a suspended USB link with the car asleep (or unknown) is logged only"
# The same while the car is awake: that's trouble, so it alerts.
snap_at 7 $((now - 3950))
AWAKE=yes
logs=""
snapshot_gap_check
grep -q "USB state 'suspended'.*car awake over BLE: yes" <<< "$logs" || fail "awake + suspended not reported: $logs"
[ "$(pushes_count)" = 3 ] || fail "an awake car not using the drive should alert"
AWAKE=no
UDC=configured

# The car didn't record (drive presented and in use, nothing written): logged
# only, awake or not (parked in Sentry Mode keeps the car awake).
snap_at 8 $((now - 4000))
touch -d "@$((now - 4100))" "$cam"
logs=""
snapshot_gap_check
grep -q "WARNING: snapshot gap: .*found no new clip on the drive.*car awake over BLE: unknown" <<< "$logs" || fail "idle car not reported: $logs"
[ "$(pushes_count)" = 3 ] || fail "an idle car with unknown wake state should not alert"
snap_at 9 $((now - 4000 + 1))
AWAKE=yes
logs=""
snapshot_gap_check
grep -q "car awake over BLE: yes" <<< "$logs" || fail "awake state not reported: $logs"
[ "$(pushes_count)" = 3 ] || fail "a car that isn't recording is logged only, even when awake (parked in Sentry)"

if [ "$failures" -gt 0 ]
then
  echo "$failures failure(s)" >&2
  exit 1
fi
echo "snapshot gap tests passed"
