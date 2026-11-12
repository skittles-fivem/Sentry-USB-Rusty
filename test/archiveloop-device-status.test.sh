#!/bin/bash
# Tests for write_device_status (run/archiveloop): .sentryusb/device-status.json
# on the archive share, built by the sentryusb service.
set -euo pipefail

script=${1:-run/archiveloop}
workdir=$(mktemp -d)
trap 'rm -rf "$workdir"' EXIT
src=$(awk '$0 ~ "^function write_device_status[ (]" {keep=1} keep {print} keep && /^}/ {exit}' "$script")
[ -n "$src" ] || { echo "write_device_status not found in $script" >&2; exit 1; }
src=${src//\/tmp\/device-status-stage/$workdir/stage}
eval "$src"
eval "$(grep -E '^DEVICE_STATUS_NAME=' "$script")"
[ "$DEVICE_STATUS_NAME" = device-status.json ] || { echo "unexpected DEVICE_STATUS_NAME" >&2; exit 1; }

failures=0
fail() { echo "FAIL: $*" >&2; failures=$((failures + 1)); }
# shellcheck disable=SC2034 # read by the eval'd function
SHARE_STATUS_DIR=.sentryusb
calls="$workdir/calls"
api_ok=true
DOC='{"schema": 1, "written_unix": 1762800000}'

# Stubs: record each call; curl -o writes the document like the service.
curl() {
  echo "curl $*" >> "$calls"
  $api_ok || return 22
  local out=""
  while [ $# -gt 0 ]; do
    if [ "$1" = -o ]; then out=$2; shift; fi
    shift
  done
  [ -z "$out" ] || printf '%s' "$DOC" > "$out"
}
timeout() { shift; "$@"; }
# shellcheck disable=SC2034 # read by the eval'd function
archive_ssh_command() { ssh_cmd=(ssh -p 22); }
rsync() {
  echo "rsync $*" >> "$calls"
  local args=("$@")
  local src=${args[${#args[@]}-2]}
  [ "$(cat "$src/$DEVICE_STATUS_NAME")" = "$DOC" ] || fail "rsync sent the wrong document"
}
rclone() {
  echo "rclone $*" >> "$calls"
  [ "$(cat "$4")" = "$DOC" ] || fail "rclone sent the wrong document"
}
reset() { : > "$calls"; api_ok=true; }

# No archive configured: nothing happens.
reset
ARCHIVE_SYSTEM=none write_device_status || fail "none should succeed"
[ ! -s "$calls" ] || fail "none made calls: $(cat "$calls")"

# SMB/NFS: the service writes it to the mounted share (tmp + rename there).
for sys in cifs nfs; do
  reset
  ARCHIVE_SYSTEM=$sys SENTRYUSB_PORT=8080 write_device_status || fail "$sys write failed"
  grep -qx 'curl -sf -m 30 -X POST http://127.0.0.1:8080/api/system/device-status/write' "$calls" ||
    fail "$sys didn't ask the service to write: $(cat "$calls")"
  reset; api_ok=false
  if ARCHIVE_SYSTEM=$sys write_device_status; then fail "$sys service failure not reported"; fi
done

# rsync: fetched from the service, sent into .sentryusb/ at the archive root.
reset
ARCHIVE_SYSTEM=rsync RSYNC_USER=pi RSYNC_SERVER=nas RSYNC_PATH=/cam write_device_status || fail "rsync write failed"
grep -q '^curl -sf -m 30 -o .*/stage/.sentryusb/device-status.json http://127.0.0.1:80/api/system/device-status$' "$calls" ||
  fail "rsync didn't fetch the document: $(cat "$calls")"
grep -q '^rsync -r --no-perms --omit-dir-times -e ssh -p 22 .*/stage/.sentryusb pi@nas:/cam/$' "$calls" ||
  fail "rsync didn't send .sentryusb: $(cat "$calls")"
[ ! -e "$workdir/stage" ] || fail "stage left behind"

# rclone: copied to the same name under the remote path.
reset
ARCHIVE_SYSTEM=rclone RCLONE_DRIVE=gd RCLONE_PATH=cam write_device_status || fail "rclone write failed"
grep -q ' gd:cam/.sentryusb/device-status.json$' "$calls" || fail "rclone target wrong: $(cat "$calls")"

# The service unreachable: reported, nothing sent, nothing left behind.
reset; api_ok=false
if ARCHIVE_SYSTEM=rclone RCLONE_DRIVE=gd RCLONE_PATH=cam write_device_status; then fail "fetch failure not reported"; fi
grep -q '^rclone' "$calls" && fail "sent without a document"
[ ! -e "$workdir/stage" ] || fail "stage left behind after a failure"

# Called at the start and end of the archive cycle and of the home backlog check.
[ "$(grep -c 'write_device_status || log' "$script")" = 4 ] || fail "expected 4 call sites in $script"

if [ "$failures" -gt 0 ]; then
  echo "$failures failure(s)" >&2
  exit 1
fi
echo "archiveloop-device-status: all tests passed"
