#!/usr/bin/env bash
# End-to-end check of the Sentry Manager link without a Pi: the real link
# client (crates/manager_link, run by examples/link_harness.rs) against a
# real Sentry Manager.
#
#   tools/manager-link-e2e.sh <manager-host:port>     # a real Manager's device port (10000)
#   tools/manager-link-e2e.sh --self-test             # against examples/fake_manager.rs
#
# Environment:
#   E2E_PASSWORD        the "Pi" web password you also type into Manager (required unless --self-test)
#   E2E_CODE            the pairing code Manager shows after Start pairing, e.g. K7QP-M2XD-9WRT
#                       (asked for when not set; --self-test uses K7QP-M2XD-9WRT). Never logged.
#   E2E_USER            the web username (default admin)
#   E2E_STATE_DIR       where the fake Pi keeps its files (default: a new temp dir)
#   MANAGER_API         Manager's admin URL, e.g. https://nas.local:9000. With it, step 5
#                       sends every allowlisted route through Manager's proxy with curl.
#   MANAGER_CURL        extra curl arguments for MANAGER_API, e.g.
#                       "--cacert manager.pem -H 'Cookie: session=...'" (word-split, then eval'd)
#   MANAGER_DEVICE_ID   the Pi's id in Manager's routes (default: the Pi's device id)
#   E2E_YES=1           don't wait for Enter at the Manager-side steps
#
# What it does, in order (each step says what to do in Manager first):
#   1. pairing with a wrong password, then with a wrong code: Manager says
#      "denied", the Pi stops at once with one message for both, nothing is saved
#      (two of Manager's five tries per slot)
#   2. pairing with the right password and code: no fingerprint step, the link
#      key and the fingerprint seen are saved (0600), and the code is nowhere
#      in the Pi's files or events
#   3. status push: right after connecting, then every 30 s
#   4. key-mode reconnect: off/on, and a fresh process on the same files
#   5. every allowlisted route through Manager's proxy (MANAGER_API), and a refused one
#   6. net frame, then pin mismatch with an impostor certificate, then the alert frame
#   7. unpair on the Pi (no reconnect), pair again, unpair in Manager (key mode denied)
#
# Every event of the fake Pi is a JSON line in $E2E_STATE_DIR/events.jsonl.
# The router behind the link is a stand-in (see link_harness.rs): actions
# such as reboot are only logged, nothing happens to this machine.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
self_test=0
if [[ "${1:-}" == "--self-test" ]]; then
    self_test=1
    port="${E2E_FAKE_PORT:-18443}"
    manager="127.0.0.1:$port"
    E2E_PASSWORD="${E2E_PASSWORD:-correct horse battery staple}"
    E2E_CODE="${E2E_CODE:-K7QP-M2XD-9WRT}"
    E2E_YES=1
elif [[ $# -eq 1 ]]; then
    manager="$1"
else
    sed -n '2,36p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
fi
user="${E2E_USER:-admin}"
: "${E2E_PASSWORD:?set E2E_PASSWORD to the web password you give Manager}"
export E2E_PASSWORD
E2E_WRONG_PASSWORD="wrong-${RANDOM}${RANDOM}"
export E2E_WRONG_PASSWORD
# normalize_code CODE: as both sides do (uppercase, no spaces or dashes, O->0, I/L->1).
normalize_code() { printf '%s' "$1" | tr a-z A-Z | tr -d ' -' | tr OIL 011; }
set_code() { # set_code CODE: the right code and a wrong one (its last character changed)
    E2E_CODE="$1"
    local n
    n=$(normalize_code "$E2E_CODE")
    [[ "$n" =~ ^[0-9A-HJKMNP-TV-Z]{12}$ ]] || { echo "that isn't a valid pairing code" >&2; exit 2; }
    E2E_CODE_NORM="$n"
    if [[ "${n:11:1}" == 0 ]]; then E2E_WRONG_CODE="${n:0:11}1"; else E2E_WRONG_CODE="${n:0:11}0"; fi
    export E2E_CODE E2E_WRONG_CODE
}
if [[ -z "${E2E_CODE:-}" ]]; then
    echo "In Manager: Settings -> Sentry USB -> Add a Sentry USB, username '$user' and the RIGHT password (E2E_PASSWORD), then Start pairing."
    read -r -p "Pairing code Manager shows: " E2E_CODE </dev/tty
fi
set_code "$E2E_CODE"

need() { command -v "$1" >/dev/null || { echo "needs $1" >&2; exit 2; }; }
need cargo; need jq; need curl

echo "== building link_harness and fake_manager"
(cd "$here" && cargo build -q -p sentryusb-manager-link --examples)
bin="${CARGO_TARGET_DIR:-$here/target}/debug/examples"

state="${E2E_STATE_DIR:-$(mktemp -d)}"
mkdir -p "$state"
chmod 700 "$state"
events="$state/events.jsonl"
: >"$events"
fifo="$state/cmd"
rm -f "$fifo"
mkfifo "$fifo"
pids=()
cleanup() {
    exec 9>&- 2>/dev/null || true
    for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
    rm -f "$fifo"
}
trap cleanup EXIT

pass=0
fail=0
ok() { echo "  PASS $*"; pass=$((pass + 1)); }
bad() { echo "  FAIL $*"; fail=$((fail + 1)); }
step() { echo; echo "== $*"; }
ask() {
    echo "  -> $*"
    if [[ "${E2E_YES:-}" != 1 ]]; then read -r -p "     press Enter when done " _ </dev/tty; fi
}

# Events after the current mark.
mark() { wc -l <"$events" | tr -d ' '; }
since() { tail -n "+$(($1 + 1))" "$events"; }
# wait_ev MARK SECONDS JQ-FILTER: succeeds once an event after MARK matches.
wait_ev() {
    local m="$1" t="$2" f="$3" end=$((SECONDS + $2))
    while ((SECONDS < end)); do
        if [[ -n "$(since "$m" | jq -c "select($f)" 2>/dev/null | head -1)" ]]; then return 0; fi
        sleep 0.3
    done
    return 1
}
# no_ev MARK SECONDS JQ-FILTER: succeeds if nothing matches for SECONDS.
no_ev() { ! wait_ev "$@"; }
# Status pushes after MARK: in-process GET /api/status calls that were not
# a request from Manager (both go through the same router).
pushes() {
    local h r
    h=$(since "$1" | jq -c 'select(.event=="handler" and .path=="/api/status")' 2>/dev/null | wc -l)
    r=$(since "$1" | jq -c 'select(.event=="request" and .path=="/api/status")' 2>/dev/null | wc -l)
    echo $((h - r))
}
wait_push() { # wait_push MARK SECONDS
    local end=$((SECONDS + $2))
    while ((SECONDS < end)); do
        (($(pushes "$1") >= 1)) && return 0
        sleep 0.5
    done
    return 1
}

harness_pid=""
start_harness() { # start_harness PASSWORD-VAR CODE-VAR PAIR-WINDOW [extra args]
    local pw="$1" code="$2" win="$3"
    shift 3
    stop_harness
    exec 9<>"$fifo"
    "$bin/link_harness" --manager "$manager" --user "$user" --password-env "$pw" --code-env "$code" \
        --pair-window "$win" --state-dir "$state/pi" "$@" <&9 >>"$events" 2>>"$state/harness.stderr" &
    harness_pid=$!
    pids+=("$harness_pid")
}
stop_harness() {
    if [[ -n "$harness_pid" ]]; then
        kill "$harness_pid" 2>/dev/null || true
        wait "$harness_pid" 2>/dev/null || true
        harness_pid=""
    fi
    exec 9>&- 2>/dev/null || true
}
cmd() { echo "$*" >&9; }

if ((self_test)); then
    "$bin/fake_manager" --port "$port" --user "$user" --password "$E2E_PASSWORD" --code "$E2E_CODE" >"$state/manager.jsonl" 2>&1 &
    pids+=("$!")
    sleep 1
fi
echo "state: $state  (events: $events)"

# -------------------------------------------------------------------------
step "1. pairing with a wrong password, then a wrong code"
ask "In Manager: a pairing slot must be open for username '$user' and the RIGHT password (E2E_PASSWORD), showing the code you gave."
nothing_saved() {
    if [[ ! -s "$state/pi/manager-link.json" ]] || jq -e '.link_key == null and .pin == null' "$state/pi/manager-link.json" >/dev/null; then
        ok "nothing saved"
    else
        bad "a key or pin was saved"
    fi
}
msgs=()
for wrong in "E2E_WRONG_PASSWORD E2E_CODE" "E2E_PASSWORD E2E_WRONG_CODE"; do
    read -r pw_var code_var <<<"$wrong"
    m=$(mark)
    start_harness "$pw_var" "$code_var" 60
    if wait_ev "$m" 40 '.event=="attempt_failed" and .mode=="pair"'; then ok "Manager refused ($pw_var, $code_var)"; else bad "no refused pairing attempt ($pw_var, $code_var)"; fi
    if wait_ev "$m" 10 '.event=="state" and .state=="pair_failed"'; then ok "the Pi stopped pairing"; else bad "no pair_failed state"; fi
    msgs+=("$(since "$m" | jq -r 'select(.event=="state" and .state=="pair_failed") | .message' | head -1)")
    n=$(since "$m" | jq -s '[.[] | select(.event=="attempt_failed" and .mode=="pair")] | length')
    sleep 3
    n2=$(since "$m" | jq -s '[.[] | select(.event=="attempt_failed" and .mode=="pair")] | length')
    ((n == 1 && n2 == 1)) && ok "one try, no retries" || bad "$n2 pairing tries"
    if no_ev "$m" 1 '.event=="paired"'; then ok "not paired"; else bad "paired with a wrong password or code"; fi
    stop_harness
    nothing_saved
done
[[ "${msgs[0]}" == "${msgs[1]}" && "${msgs[0]}" == "Wrong pairing code or password"* ]] && ok "the same message for both: ${msgs[0]%%.*}" || bad "messages differ: ${msgs[*]}"
echo "  (Manager's security log should show two failed pairing proofs; its slot allows five)"

# -------------------------------------------------------------------------
step "2. pairing with the right password and code"
ask "Manager's pairing slot must still be open (if it closed, start again: a new slot has a new code)."
m=$(mark)
start_harness E2E_PASSWORD E2E_CODE 300
if wait_ev "$m" 60 '.event=="paired"'; then ok "paired"; else bad "not paired"; fi
order=$(since "$m" | jq -r 'select(.event=="pairing_open" or (.event=="connected" and .mode=="pair") or .event=="paired") | .event' | awk '!s[$0]++' | paste -sd, -)
[[ "$order" == pairing_open,paired,connected ]] && ok "Pair opened the window, then paired and connected" || bad "pairing order: $order"
since "$m" | jq -e 'select(.event=="fingerprint_read" or .event=="fingerprint_confirmed")' >/dev/null && bad "a fingerprint step ran" || ok "no fingerprint step"
if wait_ev "$m" 10 '.event=="connected" and .mode=="pair"'; then ok "connected in pair mode"; else bad "no pair-mode connection"; fi
id=$(jq -r 'select(.event=="ready") | .device_id' "$events" | tail -1)
fp=$(jq -r '.pin' "$state/pi/manager-link.json")
mode=$(stat -c %a "$state/pi/manager-link.json")
[[ "$mode" == 600 ]] && ok "link file is 0600" || bad "link file mode $mode"
jq -e '.link_key | test("^[0-9a-f]{64}$")' "$state/pi/manager-link.json" >/dev/null && ok "link key saved" || bad "no link key"
if grep -rqiF -e "$E2E_CODE" -e "$E2E_CODE_NORM" "$state" --exclude=manager.jsonl; then bad "the pairing code is in the Pi's files or events"; else ok "the pairing code is nowhere in the Pi's files or events"; fi
echo "  Pi id $id"
echo "  Pinned fingerprint (for reference): $(echo "$fp" | tr a-f A-F | sed 's/..../& /g')"
ask "Check the Sentry USB is listed as paired and online in Manager."

# -------------------------------------------------------------------------
step "3. status push"
if wait_push "$m" 10; then ok "status pushed right after connecting (after net)"; else bad "no status push after connecting"; fi
m=$(mark)
if wait_push "$m" 40; then ok "status pushed again (every 30 s, or after a change)"; else bad "no further status push"; fi
echo "  (Manager's device list should show the status and a fresh 'last seen')"

# -------------------------------------------------------------------------
step "4. key-mode reconnect"
m=$(mark)
cmd reconnect
if wait_ev "$m" 30 '.event=="connected" and .mode=="key"'; then ok "reconnected in key mode (off/on)"; else bad "no key-mode reconnect"; fi
m=$(mark)
start_harness E2E_WRONG_PASSWORD E2E_WRONG_CODE 0
if wait_ev "$m" 30 '.event=="connected" and .mode=="key"'; then ok "new process, same files, wrong web password: key mode still connects"; else bad "restart did not connect in key mode"; fi
if no_ev "$m" 1 '.event=="pairing_open"'; then ok "no pairing window opened"; else bad "pairing window opened"; fi
m=$(mark)
start_harness E2E_PASSWORD E2E_CODE 0
wait_ev "$m" 30 '.event=="connected" and .mode=="key"' || bad "restart with the right password did not connect"

# -------------------------------------------------------------------------
step "5. allowlisted routes through Manager's proxy"
dev="${MANAGER_DEVICE_ID:-$id}"
proxy() { # proxy METHOD PATH QUERY BODY -> prints "status body"
    # Manager's proxy path leaves out the Pi's /api: .../proxy/status is GET /api/status.
    local url="$MANAGER_API/api/sentryusb/$dev/proxy${2#/api}"
    [[ -n "$3" ]] && url="$url?$3"
    local args=(-sS -o "$state/proxy.out" -w '%{http_code}' -X "$1")
    [[ "$4" != null ]] && args+=(-H 'Content-Type: application/json' --data-binary "$4")
    eval "curl ${MANAGER_CURL:-} \"\${args[@]}\" \"\$url\""
}
if ((self_test)); then
    # fake_manager sends every route itself on each connection.
    if wait_ev "$m" 60 '.event=="refused" and .path=="/api/logs/../../etc/shadow"'; then ok "fake Manager's request run finished"; fi
    n_ok=$(since "$m" | jq -s '[.[] | select(.event=="request")] | length')
    n_routes=$("$bin/link_harness" --print-routes | wc -l)
    ((n_ok >= n_routes)) && ok "$n_ok requests served ($n_routes routes)" || bad "only $n_ok requests served of $n_routes routes"
    since "$m" | jq -e 'select(.event=="refused" and .path=="/api/terminal")' >/dev/null && ok "/api/terminal refused" || bad "/api/terminal not refused"
elif [[ -n "${MANAGER_API:-}" ]]; then
    while IFS= read -r r; do
        meth=$(jq -r .method <<<"$r"); path=$(jq -r .path <<<"$r"); q=$(jq -r .query <<<"$r"); body=$(jq -c .body <<<"$r")
        if [[ "$body" == '"from-get"' ]]; then
            proxy GET "$path" "" null >/dev/null
            body=$(jq -c 'to_entries | map(select((.key | endswith("__set") | not) and .value.active == true)) | map({key, value: .value.value}) | from_entries' "$state/proxy.out")
        fi
        m=$(mark)
        code=$(proxy "$meth" "$path" "$q" "$body" || echo 000)
        if [[ "$code" =~ ^2 ]] && wait_ev "$m" 10 ".event==\"request\" and .method==\"$meth\" and .path==\"$path\""; then
            ok "$meth $path -> $code"
        else
            bad "$meth $path -> $code (Pi event: $(since "$m" | jq -c 'select(.path?=="'"$path"'") | {event,status}' | head -1))"
        fi
    done < <("$bin/link_harness" --print-routes)
    # Allowlisted on Manager's side, refused by the Pi: not the official repo.
    m=$(mark)
    code=$(proxy PUT /api/system/update-source "" '{"repo":"someone/else"}' || echo 000)
    if [[ "$code" == 403 ]] && wait_ev "$m" 10 '.event=="refused" and .path=="/api/system/update-source"'; then ok "Pi refused another update repo (403)"; else bad "update-source to another repo -> $code"; fi
    # Not on the allowlist at all: Manager must refuse without asking the Pi.
    m=$(mark)
    code=$(proxy GET /api/terminal "" null || echo 000)
    if [[ "$code" =~ ^4 ]] && no_ev "$m" 3 '.path=="/api/terminal"'; then ok "Manager refused /api/terminal ($code), the Pi never saw it"; else bad "/api/terminal -> $code"; fi
else
    echo "  MANAGER_API not set: send these by hand (curl \$MANAGER_API/api/sentryusb/$dev/proxy<path without /api>) and watch $events for \"request\":"
    "$bin/link_harness" --print-routes | jq -r '"    " + .method + " " + (.path | ltrimstr("/api")) + (if .query != "" then "?" + .query else "" end) + (if .body != null then "  " + (.body|tostring) else "" end)'
    echo "    PUT /system/update-source  {\"repo\":\"someone/else\"}   -> 403 from the Pi (event \"refused\")"
    echo "    GET /terminal                                        -> refused by Manager, no Pi event"
fi
echo "  (Manager's log should list each proxied action: who, which Pi, which route, the result)"

# -------------------------------------------------------------------------
step "6. net frame, pin mismatch, alert frame"
jq -e 'select(.event=="network")' "$events" >/dev/null && ok "net frame sent ($(jq -c 'select(.event=="network") | {iface,ip,gw,ssid}' "$events" | tail -1))" || bad "no net frame"
m=$(mark)
cmd impostor
if wait_ev "$m" 60 '.event=="fingerprint_mismatch"'; then ok "impostor certificate refused by the pin"; else bad "impostor not refused"; fi
if wait_ev "$m" 60 '.event=="impostor_refused" and .ok==true'; then ok "status page shows cert_changed"; else bad "no cert_changed"; fi
if wait_ev "$m" 60 '.event=="alert_sent"'; then ok "alert sent to the real Manager"; else bad "no alert sent"; fi
if wait_ev "$m" 30 '.event=="connected" and .mode=="key"'; then ok "back on the real Manager"; else bad "did not reconnect to the real Manager"; fi
echo "  (Manager's security log should show the net frame and a 'possible hijack attempt' for the alert,"
echo "   with fingerprint $(jq -r 'select(.event=="impostor_start") | .fingerprint' "$events" | tail -1))"

# -------------------------------------------------------------------------
step "7. unpair"
m=$(mark)
cmd unpair
if wait_ev "$m" 10 '.event=="unpaired"'; then ok "unpaired on the Pi"; else bad "no unpaired event"; fi
jq -e '.link_key == null and .pin == null' "$state/pi/manager-link.json" >/dev/null && ok "key and pin forgotten" || bad "key or pin still saved"
if no_ev "$m" 20 '.event=="connected"'; then ok "no reconnect without a pairing window"; else bad "reconnected after unpair"; fi
if ((self_test)); then
    m=$(mark)
    cmd "pair $E2E_CODE 300"
    if wait_ev "$m" 60 '.event=="paired"'; then ok "paired again with a code typed on stdin"; else bad "did not pair again"; fi
    echo "  (fake_manager has no admin API: Manager-side unpair not checked)"
else
    ask "In Manager the Sentry USB should now fail its next handshake as 'not paired'. Delete it there, then Add a Sentry USB again with the right password and Start pairing."
    read -r -p "     New pairing code Manager shows: " code2 </dev/tty
    set_code "$code2"
    m=$(mark)
    cmd "pair $E2E_CODE 300"
    if wait_ev "$m" 60 '.event=="paired"'; then ok "paired again"; else bad "did not pair again"; fi
    ask "Now Unpair (delete) the Sentry USB in Manager."
    m=$(mark)
    if wait_ev "$m" 30 '.event=="disconnected"'; then ok "Manager closed the link"; else bad "link not closed by Manager"; fi
    if wait_ev "$m" 60 '.event=="state" and .state=="not_paired"'; then ok "Pi shows not_paired (key-mode handshake denied)"; else bad "Pi does not show not_paired"; fi
fi

if grep -rqiF -e "$E2E_CODE" -e "$E2E_CODE_NORM" "$state" --exclude=manager.jsonl; then bad "a pairing code is in the Pi's files or events"; else ok "no pairing code in the Pi's files or events"; fi
cmd quit || true
echo
echo "== $pass passed, $fail failed  (events: $events)"
((fail == 0))
