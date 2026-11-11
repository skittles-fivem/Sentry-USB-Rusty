#!/usr/bin/env bash
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# The keep-awake webhook pin, taken from the real scripts (both copies must match)
sed -n '/^function keep_awake_webhook_pin () {/,/^}/p' "$repo/run/awake_start" > "$tmp/start.sh"
sed -n '/^function keep_awake_webhook_pin () {/,/^}/p' "$repo/run/awake_stop" > "$tmp/stop.sh"
cmp -s "$tmp/start.sh" "$tmp/stop.sh" || { echo "awake_start and awake_stop differ"; exit 1; }
[ -s "$tmp/start.sh" ] || { echo "function not found"; exit 1; }
source "$tmp/start.sh"

# name resolution is faked: these names answer with the address given
getent() {
  case "$2" in
    ha.lan) echo "192.168.1.20    STREAM ha.lan" ;;
    sneaky.example) echo "127.0.0.1       STREAM sneaky.example" ;;
    sneaky6.example) echo "::1             STREAM sneaky6.example" ;;
    ll.example) echo "169.254.10.1    STREAM ll.example" ;;
    v6.example) echo "2001:db8::5     STREAM v6.example" ;;
    *) return 2 ;;
  esac
}

fail=0
ok() { local out; if out=$(keep_awake_webhook_pin "$1"); then [ "$out" = "$2" ] || { echo "FAIL $1: got '$out' want '$2'"; fail=1; }; else echo "FAIL $1: refused"; fail=1; fi; }
no() { if keep_awake_webhook_pin "$1" >/dev/null; then echo "FAIL $1: allowed"; fail=1; fi; }

ok 'http://ha.lan:8123/api/webhook/x' 'ha.lan:8123:192.168.1.20'
ok 'https://HA.lan/hook' 'ha.lan:443:192.168.1.20'
ok 'https://v6.example/x' 'v6.example:443:[2001:db8::5]'
ok 'http://192.168.1.20:8123/x' ''
no 'http://sneaky.example/x'
no 'http://sneaky6.example/x'
no 'http://ll.example/x'
no 'http://127.0.0.1/api/system/reboot'
no 'http://[::1]:80/x'
no 'http://0.0.0.0/x'
no 'http://2130706433/x'
no 'http://0x7f.1/x'
no 'http://user@ha.lan/x'
no 'ftp://ha.lan/x'
no 'file:///etc/passwd'
no 'http://unknown.example/x'
no 'http://ha.lan:99999999/x'

[ "$fail" -eq 0 ] && echo "keep-awake webhook pin: ok"
exit "$fail"
