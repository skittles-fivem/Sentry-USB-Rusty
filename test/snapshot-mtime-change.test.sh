#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)

# make_snapshot.sh discards a run as identical only when no file is new or
# longer AND no file was modified since the previous snapshot (the newest
# file time, kept in snap.bin.newest beside each TOC). Same harness as
# snapshot-toc-recovery.test.sh: paths rewritten into a fixture tree,
# mounts stubbed by directories.
work=$(mktemp -d)
trap 'chmod -R u+w "$work" 2>/dev/null; rm -rf "$work"' EXIT
snaps=$work/snapshots
autofs=$work/autofs
farm=$work/mutable/TeslaCam
mkdir -p "$snaps" "$autofs" "$work/bin" "$farm"
: > "$work/mutable/.recentclips_events_purged"
: > "$work/cam_disk.bin"
paths () {
  tr -d '\r' | sed -e "s@/backingfiles/snapshots@$snaps@g" -e "s@/backingfiles@$work@g" \
      -e "s@/tmp/snapshots@$autofs@g" -e "s@/mutable@$work/mutable@g" \
      -e "s@/root/bin@$work/bin@g" -e "s@/tmp/\.sentryusb-@$work/.sentryusb-@g" \
      -e "s@/tmp/toc-recover@$work/toc-recover@g"
}
stub () { printf '#!/bin/bash\n%s\n' "$2" > "$work/bin/$1"; chmod +x "$work/bin/$1"; }
stub ionice 'while [[ ${1:-} == -* ]]; do [[ $1 == -p ]] && exit 0; shift 2; done; exec "$@"'
stub cp 'args=(); for a; do [[ $a == --reflink=* ]] || args+=("$a"); done; exec /bin/cp "${args[@]}"'
stub losetup 'case "$1" in -d) exit 0 ;; *) echo /dev/fakeloop ;; esac'
stub fsck 'exit 0'
stub systemctl 'exit 0'
stub release_snapshot.sh 'rm -rf "$1"'
export PATH=$work/bin:$PATH LOG=$work/log
log () { echo "$*" >> "$LOG"; }
# archiveloop exports these to make_snapshot.sh.
losetup_find_show () { echo /dev/fakeloop; }
export -f log losetup_find_show

paths < "$root/run/make_snapshot.sh" > "$work/make_snapshot.sh"
run_snapshot () {
  FLOCKED=$work/make_snapshot.sh bash -eu "$work/make_snapshot.sh" nofsck
}
clip () {  # clip SNAP SIZE NAME : a RecentClips file inside a snapshot's mount
  mkdir -p "$autofs/$1/TeslaCam/RecentClips"
  head -c "$2" /dev/zero > "$autofs/$1/TeslaCam/RecentClips/$3"
}
committed () {  # committed SNAP : image + TOC listing what its mount holds
  mkdir -p "$snaps/$1"
  : > "$snaps/$1/snap.bin"
  (cd "$autofs/$1" && find . -type f -printf '%s %P\n') > "$snaps/$1/snap.bin.toc"
}
recent () { readlink "$farm/RecentClips/2026-10-30/$1"; }

a=2026-10-30_20-00-00-front.mp4

# A first snapshot records its newest file time.
clip snap-000000 5 "$a"
run_snapshot
[[ -e $snaps/snap-000000/snap.bin.toc && -s $snaps/snap-000000/snap.bin.newest ]]

# The same files with the same times: identical, discarded.
: > "$LOG"
clip snap-000001 5 "$a"
touch -r "$autofs/snap-000000/TeslaCam/RecentClips/$a" "$autofs/snap-000001/TeslaCam/RecentClips/$a"
run_snapshot
[[ ! -e $snaps/snap-000001 ]]
grep -q 'identical to previous one' "$LOG"

# Same name and size, but rewritten since (newer time): kept, and said why.
: > "$LOG"
clip snap-000001 5 "$a"
touch -d "@$(( $(stat -c %Y "$autofs/snap-000000/TeslaCam/RecentClips/$a") + 60 ))" "$autofs/snap-000001/TeslaCam/RecentClips/$a"
run_snapshot
[[ -e $snaps/snap-000001/snap.bin.toc ]]
grep -q 'a file was modified in place since the previous snapshot' "$LOG"
awk -v a="$(cat "$snaps/snap-000001/snap.bin.newest")" -v b="$(cat "$snaps/snap-000000/snap.bin.newest")" 'BEGIN { exit !(a + 0 > b + 0) }'

# A previous snapshot from before .newest files: names and sizes alone decide.
rm -f "$snaps/snap-000001/snap.bin.newest"
: > "$LOG"
clip snap-000002 5 "$a"
touch -d "@$(( $(stat -c %Y "$autofs/snap-000001/TeslaCam/RecentClips/$a") + 60 ))" "$autofs/snap-000002/TeslaCam/RecentClips/$a"
run_snapshot
[[ ! -e $snaps/snap-000002 ]]
grep -q 'identical to previous one' "$LOG"

echo "snapshot mtime-change tests passed"
