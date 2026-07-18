#!/usr/bin/env bash
set -uo pipefail

# Hearth Smoke Test — TARGET-BEARING isolated end-to-end gate (review 5594
# B2-7). Runs a fully isolated daemon (typed isolated runtime via
# HEARTH_ISOLATED_ROOT + HEARTH_CONFIG_DIR) with fake launchable PHP CLI/FPM
# providers, and exercises discovery, Set, Show/Status, materialization,
# Sync, php exec (env + effective value), version Switch (binary/env change),
# a foreign-collision hard failure with no restart, Unset, and Unmanage.
#
# Isolation guarantees:
#   - every daemon/CLI/MCP/socket path lives under a unique temp root;
#   - provider discovery roots default beneath the isolated root — real
#     Herd/Homebrew channels are undiscoverable and unwritable;
#   - all ports are freshly allocated with dump_port+1 reserved, and the
#     daemon start retries with new ports on a bind race;
#   - known real paths are hashed before the run, and the EXIT trap re-checks
#     them on EVERY exit path (including early failures) BEFORE cleanup while
#     preserving the original exit status.
# Usage: ./scripts/smoke-test.sh

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m'
PASS=0
FAIL=0

pass() { echo -e "  ${GREEN}✓${NC} $1"; PASS=$((PASS + 1)); }
fail() { echo -e "  ${RED}✗${NC} $1"; FAIL=$((FAIL + 1)); }
info() { echo -e "\n${YELLOW}▸${NC} $1"; }

# ── Isolation root ───────────────────────────────────────────────
SMOKE_ROOT="$(mktemp -d /tmp/hearth-smoke.XXXXXX)"
# Canonicalize (macOS /tmp is a symlink to /private/tmp): channel verification
# rejects non-canonical path components by design.
SMOKE_ROOT="$(cd "$SMOKE_ROOT" && pwd -P)"
export HEARTH_ISOLATED_ROOT="$SMOKE_ROOT"
export HEARTH_CONFIG_DIR="$SMOKE_ROOT/hearth"
mkdir -p "$HEARTH_CONFIG_DIR" "$SMOKE_ROOT/herd" "$SMOKE_ROOT/homebrew"

REAL_HEARTH_DIR="$HOME/Library/Application Support/hearth"
REAL_SOCK="$REAL_HEARTH_DIR/hearth.sock"
ISOLATED_SOCK="$HEARTH_CONFIG_DIR/hearth.sock"

if [ "$ISOLATED_SOCK" = "$REAL_SOCK" ]; then
    echo "FATAL: isolation failed — isolated socket equals the real socket path"
    exit 1
fi

# ── Real-state guard (captured now; re-checked in the EXIT trap) ─
REAL_PATHS=(
    "$REAL_HEARTH_DIR/config.toml"
    "$HOME/Library/Application Support/Herd/config/valet/config.json"
    "$REAL_HEARTH_DIR/php/8.4/php.ini"
    "/usr/local/etc/php/conf.d/99-memory-limit.ini"
)
MUST_STAY_ABSENT=(
    "$HOME/Library/Application Support/Herd/config/php/84/zz-hearth.ini"
    "$HOME/Library/Application Support/Herd/config/php/85/zz-hearth.ini"
    "/opt/homebrew/etc/php/8.1/conf.d/zz-hearth.ini"
    "/opt/homebrew/etc/php/8.5/conf.d/zz-hearth.ini"
    "$REAL_HEARTH_DIR/php/manifest.toml"
)

state_line() {
    # sha256 + mode + uid + gid + inode, or "absent" — never file contents.
    if [ -e "$1" ]; then
        printf '%s %s\n' "$(shasum -a 256 "$1" 2>/dev/null | cut -d' ' -f1)" \
            "$(stat -f '%Sp %u %g %i' "$1" 2>/dev/null)"
    else
        echo "absent"
    fi
}

# Read-only snapshot of the installed daemon + its service states. The
# installed CLI is invoked with the isolation variables stripped so it talks
# to the REAL daemon (status query only — never service control).
INSTALLED_CLI="/opt/homebrew/bin/hearth"
INSTALLED_DAEMON_PATTERN="/opt/homebrew/bin/hearth-daemon"
installed_daemon_line() {
    local pid alive services
    pid="$(pgrep -f "$INSTALLED_DAEMON_PATTERN" | head -1 || true)"
    if [ -z "$pid" ]; then
        echo "daemon=absent"
        return
    fi
    alive="no"
    kill -0 "$pid" 2>/dev/null && alive="yes"
    if [ -x "$INSTALLED_CLI" ]; then
        services="$(env -u HEARTH_CONFIG_DIR -u HEARTH_ISOLATED_ROOT \
            -u HEARTH_HERD_ROOT -u HEARTH_HOMEBREW_ROOT \
            "$INSTALLED_CLI" status 2>/dev/null | tail -n +3 | awk '{print $1"="$2}' | sort | tr '\n' ' ')"
        if [ -n "$services" ]; then
            echo "daemon=pid:$pid alive:$alive responsive:yes services:[$services]"
        else
            echo "daemon=pid:$pid alive:$alive responsive:no"
        fi
    else
        echo "daemon=pid:$pid alive:$alive responsive:unknown(no installed CLI)"
    fi
}

REAL_STATE_BEFORE="$SMOKE_ROOT/real-state-before.txt"
: > "$REAL_STATE_BEFORE"
for p in "${REAL_PATHS[@]}" "${MUST_STAY_ABSENT[@]}"; do
    printf '%s => %s\n' "$p" "$(state_line "$p")" >> "$REAL_STATE_BEFORE"
done
REAL_SOCK_BEFORE="$(stat -f '%i' "$REAL_SOCK" 2>/dev/null || echo absent)"
installed_daemon_line >> "$REAL_STATE_BEFORE"

# Documented exit codes:
#   0   — all scenarios passed and the live-state guard passed
#   N   — the original failing status, preserved, when the guard passed
#   97  — GUARD FAILURE: the live-state guard detected a change (the original
#         status is logged); this code is reserved for guard mismatches only.
GUARD_FAILURE_EXIT=97

GUARD_FAIL=0
verify_real_state() {
    local after="$SMOKE_ROOT/real-state-after.txt"
    : > "$after"
    for p in "${REAL_PATHS[@]}" "${MUST_STAY_ABSENT[@]}"; do
        printf '%s => %s\n' "$p" "$(state_line "$p")" >> "$after"
    done
    local sock_after
    sock_after="$(stat -f '%i' "$REAL_SOCK" 2>/dev/null || echo absent)"
    installed_daemon_line >> "$after"
    if diff -q "$REAL_STATE_BEFORE" "$after" > /dev/null 2>&1 \
        && [ "$REAL_SOCK_BEFORE" = "$sock_after" ]; then
        echo "  real-state guard: unchanged (sha256+mode+uid/gid+inode, absence paths, socket inode, installed daemon pid/responsiveness, service states)"
    else
        GUARD_FAIL=1
        echo "  real-state guard: REAL STATE CHANGED:"
        diff "$REAL_STATE_BEFORE" "$after" || true
        [ "$REAL_SOCK_BEFORE" != "$sock_after" ] \
            && echo "    real socket inode: $REAL_SOCK_BEFORE -> $sock_after"
    fi
}

on_exit() {
    local status=$?
    trap - EXIT
    info "EXIT: verifying real user state (always, before cleanup; original status=$status)..."
    verify_real_state
    [ -n "${DAEMON_PID:-}" ] && kill "$DAEMON_PID" 2>/dev/null && wait "$DAEMON_PID" 2>/dev/null
    rm -rf "$SMOKE_ROOT" 2>/dev/null
    # Orphan check: nothing referencing the disposable root may survive.
    if pgrep -f "$SMOKE_ROOT" >/dev/null 2>&1; then
        echo "  ORPHAN fake processes detected for $SMOKE_ROOT"
        GUARD_FAIL=1
    else
        echo "  cleanup: no orphan fixture processes"
    fi
    if [ "$GUARD_FAIL" -ne 0 ]; then
        echo "  exiting with guard-failure status $GUARD_FAILURE_EXIT (original status was $status)"
        exit "$GUARD_FAILURE_EXIT"
    fi
    if [ "$status" -ne 0 ]; then
        exit "$status"
    fi
    [ "$FAIL" -gt 0 ] && exit 1
    exit 0
}
trap on_exit EXIT

# Supported deterministic fault-injection seam (review 5594 B3-3): forces an
# early exit with the requested status immediately after trap installation,
# proving the guard runs on every path and the status is preserved.
if [ -n "${HEARTH_SMOKE_FORCE_EXIT:-}" ]; then
    echo "fault-injection seam: forcing early exit ${HEARTH_SMOKE_FORCE_EXIT}"
    exit "${HEARTH_SMOKE_FORCE_EXIT}"
fi

# ── Fake launchable PHP providers (Hearth tier: resolver + discovery) ─
make_fake_php() {
    # $1 = version, $2 = binary path (conf.d sibling is the compiled scan dir)
    local version="$1" bin="$2"
    local vdir
    vdir="$(dirname "$bin")"
    mkdir -p "$vdir"
    cat > "$bin" <<'FAKEEOF'
#!/bin/sh
# Deterministic fake PHP for the Hearth smoke gate.
SELF_DIR="$(cd "$(dirname "$0")" && pwd)"
COMPILED="$SELF_DIR/conf.d"
case "${1:-}" in
  --ini|-i)
    if [ -n "${PHP_INI_SCAN_DIR:-}" ]; then
      echo "Scan for additional .ini files in: $PHP_INI_SCAN_DIR"
      CUSTOM="${PHP_INI_SCAN_DIR#:}"
      parsed=""
      for f in "$COMPILED"/*.ini "$CUSTOM"/*.ini; do
        [ -e "$f" ] && parsed="$parsed$f,"
      done
      echo "Additional .ini files parsed:      ${parsed%,}"
    else
      echo "Scan for additional .ini files in: $COMPILED"
      parsed=""
      for f in "$COMPILED"/*.ini; do
        [ -e "$f" ] && parsed="$parsed$f,"
      done
      echo "Additional .ini files parsed:      ${parsed%,}"
    fi
    ;;
  -r)
    case "${2:-}" in
      *getenv*) printf %s "${PHP_INI_SCAN_DIR:-}" ;;
      *ini_get*)
        VAL=""
        CUSTOM="${PHP_INI_SCAN_DIR#:}"
        for d in "$COMPILED" "$CUSTOM"; do
          [ -n "$d" ] && [ -f "$d/zz-hearth.ini" ] \
            && VAL="$(sed -n 's/^memory_limit=//p' "$d/zz-hearth.ini" | tail -1)"
        done
        printf %s "${VAL:-default}"
        ;;
      *) printf %s "unhandled" ;;
    esac
    ;;
  --nodaemonize*)
    # Validate the supplied FPM config like real php-fpm: refuse to run
    # without a readable config, so a missing-config restart can never be
    # masked by a blindly-succeeding fake (C1-4, review 5650).
    CONF=""
    for a in "$@"; do
      case "$a" in
        --fpm-config=*) CONF="${a#--fpm-config=}" ;;
      esac
    done
    [ -n "$CONF" ] && [ -r "$CONF" ] || exit 78
    exec sleep 300
    ;;
esac
exit 0
FAKEEOF
    chmod 755 "$bin"
    # Warm-up: absorb macOS first-exec assessment before timed probes.
    "$bin" --warmup >/dev/null 2>&1 || true
}

for v in 8.3 8.4; do
    make_fake_php "$v" "$HEARTH_CONFIG_DIR/php/$v/php"
    make_fake_php "$v" "$HEARTH_CONFIG_DIR/php/$v/php-fpm"
done
mkdir -p "$HEARTH_CONFIG_DIR/fpm"
echo "; smoke fpm config" > "$HEARTH_CONFIG_DIR/fpm/php-fpm.conf"

# ── Build ────────────────────────────────────────────────────────
info "Building workspace..."
cargo build --workspace 2>&1 | tail -1
CLI="./target/debug/hearth"
DAEMON="./target/debug/hearth-daemon"

# ── Port allocation + daemon start with bounded retry (TOCTOU-safe) ─
alloc_ports() {
    python3 - <<'PYEOF'
import socket
socks = []
def grab():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    socks.append(s)
    return s.getsockname()[1]
dump = grab()
while True:
    try:
        relay = socket.socket()
        relay.bind(("127.0.0.1", dump + 1))
        socks.append(relay)
        break
    except OSError:
        dump = grab()
others = [grab() for _ in range(5)]
print(dump, *others)
PYEOF
}

start_daemon() {
    read -r DUMP_PORT MCP_PORT DNS_PORT MYSQL_PORT POSTGRES_PORT REDIS_PORT <<EOF
$(alloc_ports)
EOF
    cat > "$HEARTH_CONFIG_DIR/config.toml" <<EOF
tld = "test"
default_php = "8.4"
dns_port = $DNS_PORT
dump_port = $DUMP_PORT
mail_smtp_port = 1025
mail_ui_port = 8025
mcp_port = $MCP_PORT
mysql_port = $MYSQL_PORT
postgres_port = $POSTGRES_PORT
redis_port = $REDIS_PORT
parked_paths = []
EOF
    : > "$SMOKE_ROOT/daemon.log"
    $DAEMON >"$SMOKE_ROOT/daemon.log" 2>&1 &
    DAEMON_PID=$!
    sleep 2
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        return 1
    fi
    if grep -q "Address already in use" "$SMOKE_ROOT/daemon.log"; then
        kill "$DAEMON_PID" 2>/dev/null
        wait "$DAEMON_PID" 2>/dev/null
        DAEMON_PID=""
        return 1
    fi
    [ -S "$ISOLATED_SOCK" ]
}

stop_daemon() {
    [ -n "${DAEMON_PID:-}" ] || return 0
    kill "$DAEMON_PID" 2>/dev/null
    wait "$DAEMON_PID" 2>/dev/null
    DAEMON_PID=""
}

boot_daemon() {
    # $1 = deterministic Herd-ownership answer for this daemon run (1|0).
    # The seam is honored only because HEARTH_ISOLATED_ROOT is set (typed
    # isolated runtime) — a production daemon ignores it.
    export HEARTH_HERD_OWNERSHIP="$1"
    STARTED=0
    for attempt in 1 2 3 4 5; do
        if start_daemon; then
            STARTED=1
            pass "Daemon started (herd=$1, attempt $attempt, PID $DAEMON_PID, dump:$DUMP_PORT mcp:$MCP_PORT)"
            break
        fi
        echo "  attempt $attempt failed — reallocating ports"
    done
    if [ "$STARTED" -ne 1 ]; then
        fail "Daemon failed to start after 5 attempts"
        sed 's/^/    /' "$SMOKE_ROOT/daemon.log" | tail -5
        exit 1
    fi
}

# ── SH. Simulated Herd ownership: boot skip + `php use` takes no FPM ─
info "SH: simulated Herd ownership — boot skip and ownership-free php use..."
boot_daemon 1
if grep -q "Herd detected — skipping" "$SMOKE_ROOT/daemon.log"; then
    pass "SH: boot skipped nginx/php-fpm/dnsmasq under simulated Herd"
else
    fail "SH: expected Herd boot skip in daemon log"
fi
if $CLI status 2>&1 | grep -q "php-fpm"; then
    fail "SH: php-fpm must not be registered under Herd ownership"
else
    pass "SH: no php-fpm registration at boot"
fi
OUTPUT=$($CLI php use 8.3 2>&1)
if echo "$OUTPUT" | grep -q "php-fpm untouched (Herd manages PHP-FPM)"; then
    pass "SH: ordinary php use is a truthful FPM skip under Herd"
else
    fail "SH: got: $OUTPUT"
fi
if $CLI status 2>&1 | grep -q "php-fpm"; then
    fail "SH: php use must not register FPM under Herd"
else
    pass "SH: still no php-fpm registration after php use"
fi
if pgrep -f "$HEARTH_CONFIG_DIR/php" >/dev/null 2>&1; then
    fail "SH: a Hearth FPM process is running — switch took ownership"
else
    pass "SH: zero Hearth FPM processes under simulated Herd"
fi
stop_daemon

# ── SH2. Runtime ownership flip: relinquish + generic control (C3-1) ─
info "SH2: Herd ownership appears at runtime — relinquish, no restarts..."
# Single-service control only: a generic `hearth start` would also launch
# real nginx/DB engine binaries against the isolated root (heavyweight and
# orphan-prone) — the generic start_all/health skip paths are covered by
# supervisor unit tests. The fake FPM `exec`s sleep, so run-state assertions
# use the supervisor's own truthful `hearth status` line, not pgrep.
HERD_FLAG="$SMOKE_ROOT/herd-flag"
echo 0 > "$HERD_FLAG"
boot_daemon "file:$HERD_FLAG"
$CLI restart php-fpm >/dev/null 2>&1
if $CLI status 2>&1 | grep -i "php-fpm" | grep -qi "running"; then
    pass "SH2: herd-absent restart launched Hearth FPM"
else
    fail "SH2: expected a running Hearth FPM before the flip"
fi
echo 1 > "$HERD_FLAG"
sleep 7   # > health interval (5s): one ownership-aware health tick
if $CLI status 2>&1 | grep -i "php-fpm" | grep -qi "running"; then
    fail "SH2: Hearth FPM child must be relinquished after Herd appears"
else
    pass "SH2: health tick relinquished Hearth's own FPM child"
fi
OUTPUT=$($CLI restart php-fpm 2>&1)
if echo "$OUTPUT" | grep -qi "owned by Herd"; then
    pass "SH2: explicit php-fpm restart names the Herd ownership"
else
    fail "SH2: got: $OUTPUT"
fi
# C4-2: invalid flag content = UNKNOWN ownership → activation fails closed.
echo garbage > "$HERD_FLAG"
OUTPUT=$($CLI restart php-fpm 2>&1)
if echo "$OUTPUT" | grep -qi "ownership is unknown"; then
    pass "SH2: unknown ownership refuses FPM activation fail-closed"
else
    fail "SH2: got: $OUTPUT"
fi
echo 1 > "$HERD_FLAG"
if $CLI status 2>&1 | grep -i "php-fpm" | grep -qi "running"; then
    fail "SH2: restart must not revive FPM under Herd"
else
    pass "SH2: no FPM revival under Herd ownership"
fi
stop_daemon

# ── Main phase: deterministic Herd-absent daemon ─────────────────
info "Starting isolated daemon (herd-absent, bounded port-collision retry)..."
boot_daemon 0

# ── Isolated daemon identity ─────────────────────────────────────
DAEMON_CMD="$(ps -o command= -p "$DAEMON_PID" | tr -d ' ')"
if [ "$DAEMON_CMD" = "./target/debug/hearth-daemon" ]; then
    pass "Isolated daemon identity: PID $DAEMON_PID runs ./target/debug/hearth-daemon"
else
    fail "Daemon identity unexpected: $DAEMON_CMD"
fi
if [ -S "$ISOLATED_SOCK" ]; then
    pass "Daemon bound the ISOLATED socket ($ISOLATED_SOCK)"
else
    fail "Isolated socket missing"
fi

OUTPUT=$($CLI status 2>&1) && pass "hearth status — connected via isolated socket" \
    || fail "hearth status — failed: $OUTPUT"

# ── S1. Discovery: rows MUST exist ───────────────────────────────
info "S1: php config --status discovers the fake providers..."
OUTPUT=$($CLI php config --status 2>&1)
if echo "$OUTPUT" | grep -q "No PHP targets discovered"; then
    fail "S1: no targets discovered — smoke must be target-bearing: $OUTPUT"
elif echo "$OUTPUT" | grep -q "hearth" && echo "$OUTPUT" | grep -q "8.3" \
    && echo "$OUTPUT" | grep -q "8.4"; then
    pass "S1: hearth 8.3 + 8.4 targets discovered"
else
    fail "S1: expected hearth 8.3/8.4 rows, got: $OUTPUT"
fi
if echo "$OUTPUT" | grep -q "Coverage: Hearth guarantees" \
    && echo "$OUTPUT" | grep -q "outside that guarantee" \
    && ! echo "$OUTPUT" | grep -qi "universal"; then
    pass "S1: coverage footer states the guarantee boundary (never universal)"
else
    fail "S1: coverage footer missing or overclaiming: $OUTPUT"
fi

# ── S2. Set + actual materialization ─────────────────────────────
info "S2: --global set materializes channel files..."
if $CLI php config --global memory_limit 1G >/dev/null 2>&1; then
    pass "S2: set --global exited 0"
else
    fail "S2: set --global failed"
fi
for v in 8.3 8.4; do
    f="$HEARTH_CONFIG_DIR/php/$v/conf.d/zz-hearth.ini"
    if [ -f "$f" ] && grep -q '^memory_limit=1G$' "$f"; then
        pass "S2: materialized $v channel file with memory_limit=1G"
    else
        fail "S2: missing/wrong channel file for $v"
    fi
done

# ── S3. Show ─────────────────────────────────────────────────────
info "S3: --show reports the configured value..."
OUTPUT=$($CLI php config --show memory_limit 2>&1)
if echo "$OUTPUT" | grep -q "configured=1G"; then
    pass "S3: show reports configured=1G"
else
    fail "S3: got: $OUTPUT"
fi
if echo "$OUTPUT" | grep -q "observed=1G (launch-probed)"; then
    pass "S3: CLI targets report the launch-probed effective value"
else
    fail "S3: expected observed=1G (launch-probed), got: $OUTPUT"
fi
# C1-3: keyed --status carries per-target configured + launch-probed values.
OUTPUT=$($CLI php config --status memory_limit 2>&1)
if echo "$OUTPUT" | grep -q "configured=1G" \
    && echo "$OUTPUT" | grep -q "observed=1G (launch-probed)" \
    && echo "$OUTPUT" | grep -q "Coverage: Hearth guarantees"; then
    pass "S3b: keyed --status shows configured + launch-probed values with the footer"
else
    fail "S3b: got: $OUTPUT"
fi

# ── S4. Sync idempotent ──────────────────────────────────────────
info "S4: --sync is clean/idempotent..."
OUTPUT=$($CLI php config --sync 2>&1)
if [ $? -eq 0 ] && echo "$OUTPUT" | grep -q "unchanged"; then
    pass "S4: sync exited 0 with unchanged files"
else
    fail "S4: got: $OUTPUT"
fi

# ── S5/S6. php exec: env boundary + effective value ──────────────
info "S5/S6: hearth php exec env + effective value..."
OUTPUT=$($CLI php exec -- -r 'echo getenv("PHP_INI_SCAN_DIR");' 2>/dev/null)
if [ "$OUTPUT" = ":$HEARTH_CONFIG_DIR/php/8.4/conf.d" ]; then
    pass "S5: exec child sees scan-dir env for the active version (8.4)"
else
    fail "S5: got: $OUTPUT"
fi
OUTPUT=$($CLI php exec -- -r 'echo ini_get("memory_limit");' 2>/dev/null)
if [ "$OUTPUT" = "1G" ]; then
    pass "S6: exec child observes the materialized effective value 1G"
else
    fail "S6: got: $OUTPUT"
fi

# ── S7. Switch: new binary + new env ─────────────────────────────
info "S7: version switch rebuilds FPM and moves the exec env..."
OUTPUT=$($CLI php use 8.3 2>&1)
if echo "$OUTPUT" | grep -q "Switched to PHP 8.3; php-fpm restarted"; then
    pass "S7: switch to 8.3 restarted the fake FPM"
else
    fail "S7: got: $OUTPUT"
fi
STATUS_OUT=$($CLI status 2>&1)
if echo "$STATUS_OUT" | grep -q "php-fpm"; then
    pass "S7: php-fpm registered after switch"
else
    fail "S7: php-fpm missing from status: $STATUS_OUT"
fi
OUTPUT=$($CLI php exec -- -r 'echo getenv("PHP_INI_SCAN_DIR");' 2>/dev/null)
if [ "$OUTPUT" = ":$HEARTH_CONFIG_DIR/php/8.3/conf.d" ]; then
    pass "S7: exec env follows the switched version (8.3)"
else
    fail "S7: got: $OUTPUT"
fi

# ── S8. Foreign collision → hard failure, no restart ─────────────
info "S8: foreign channel collision hard-fails with no restart..."
TARGET_FILE="$HEARTH_CONFIG_DIR/php/8.3/conf.d/zz-hearth.ini"
cp "$TARGET_FILE" "$SMOKE_ROOT/ours.ini.bak"
echo "; foreign file" > "$TARGET_FILE"
OUTPUT=$($CLI php config --global memory_limit 2G 2>&1)
STATUS=$?
if [ "$STATUS" -ne 0 ] && echo "$OUTPUT" | grep -qi "refused"; then
    pass "S8: collision → nonzero exit + refused outcome"
else
    fail "S8: exit=$STATUS output: $OUTPUT"
fi
if echo "$OUTPUT" | grep -q "php-fpm restarted"; then
    fail "S8: FPM must NOT restart on hard failure"
else
    pass "S8: no FPM restart on hard failure"
fi
if [ "$(cat "$TARGET_FILE")" = "; foreign file" ]; then
    pass "S8: foreign file untouched"
else
    fail "S8: foreign file was modified"
fi
cp "$SMOKE_ROOT/ours.ini.bak" "$TARGET_FILE"

# ── S9. Unset + Unmanage clean up ────────────────────────────────
info "S9: unset + unmanage remove owned artifacts..."
$CLI php config --sync >/dev/null 2>&1   # re-own after restore (2G refused round left pending intent)
if $CLI php config --global --unset memory_limit >/dev/null 2>&1; then
    pass "S9: unset exited 0"
else
    fail "S9: unset failed"
fi
$CLI php config --unmanage >/dev/null 2>&1
LEFT=0
for v in 8.3 8.4; do
    [ -e "$HEARTH_CONFIG_DIR/php/$v/conf.d/zz-hearth.ini" ] && LEFT=1
done
if [ "$LEFT" -eq 0 ]; then
    pass "S9: all owned channel files removed"
else
    fail "S9: leftover channel files"
fi

# ── S9b. Missing FPM config on a FRESH boot: unconditional gate ──
# stop → remove config → fresh Herd-absent daemon: no stale registration
# can mask the failure, and the LAUNCH-BLOCKED assertions are deterministic
# on every host (C1-4, review 5650).
info "S9b: conf-missing fresh boot — unconditional LAUNCH-BLOCKED + set exit 0..."
stop_daemon
mv "$HEARTH_CONFIG_DIR/fpm/php-fpm.conf" "$SMOKE_ROOT/php-fpm.conf.bak"
boot_daemon 0
if $CLI status 2>&1 | grep -q "php-fpm"; then
    fail "S9b: FPM must not register at boot without its config"
else
    pass "S9b: no stale FPM registration to mask the missing config"
fi
OUTPUT=$($CLI php config --status 2>&1)
if echo "$OUTPUT" | grep -q "LAUNCH-BLOCKED" && echo "$OUTPUT" | grep -q "#2343"; then
    pass "S9b: --status renders the LAUNCH-BLOCKED row citing todo #2343 (unconditional)"
else
    fail "S9b: expected LAUNCH-BLOCKED row, got: $OUTPUT"
fi
if echo "$OUTPUT" | grep -q "Coverage: Hearth guarantees"; then
    pass "S9b: coverage footer present with FPM config missing"
else
    fail "S9b: footer missing: $OUTPUT"
fi
OUTPUT=$($CLI php config --global memory_limit 1G 2>&1)
STATUS=$?
if [ "$STATUS" -eq 0 ] && echo "$OUTPUT" | grep -q "LAUNCH-BLOCKED"; then
    pass "S9b: --global set exits 0 and truthfully reports the launch-blocked FPM"
else
    fail "S9b: exit=$STATUS output: $OUTPUT"
fi
$CLI php config --global --unset memory_limit >/dev/null 2>&1
$CLI php config --unmanage >/dev/null 2>&1
mv "$SMOKE_ROOT/php-fpm.conf.bak" "$HEARTH_CONFIG_DIR/fpm/php-fpm.conf"

# ── S10. No privileged paths in any scenario output ──────────────
info "S10: no privileged-path writes..."
if $CLI php config --status 2>&1 | grep -q "usr/local.*Written"; then
    fail "S10: privileged path appears as written"
else
    pass "S10: no privileged writes reported"
fi

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo -e " Results: ${GREEN}$PASS passed${NC}, ${RED}$FAIL failed${NC} (real-state guard runs in EXIT trap)"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "Isolated root: $SMOKE_ROOT  ports: dump=$DUMP_PORT mcp=$MCP_PORT"

[ "$FAIL" -eq 0 ] && exit 0 || exit 1
