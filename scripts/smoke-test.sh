#!/usr/bin/env bash
set -uo pipefail

# Hearth Smoke Test — verifies daemon ↔ CLI ↔ dump server end-to-end in a
# FULLY ISOLATED environment (review 5594 F1):
#   - HEARTH_CONFIG_DIR points every daemon/CLI/MCP/socket/run/log/data path
#     into a unique temp root; the real user config and socket are never read,
#     written, or removed.
#   - HEARTH_HERD_ROOT / HEARTH_HOMEBREW_ROOT point provider discovery at
#     empty temp dirs so php-config reconciliation can never touch real
#     Herd/Homebrew channel directories.
#   - All ports (dump, MCP, DB engines, DNS) are OS-assigned free ports baked
#     into the isolated config.toml.
#   - Known real paths are hashed before and after; any change fails the run.
# Usage: ./scripts/smoke-test.sh

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m'
PASS=0
FAIL=0

pass() { echo -e "  ${GREEN}✓${NC} $1"; PASS=$((PASS + 1)); }
fail() { echo -e "  ${RED}✗${NC} $1"; FAIL=$((FAIL + 1)); }
warn() { echo -e "  ${YELLOW}⚠${NC} $1"; }
info() { echo -e "\n${YELLOW}▸${NC} $1"; }

# ── Isolation root ───────────────────────────────────────────────
SMOKE_ROOT="$(mktemp -d /tmp/hearth-smoke.XXXXXX)"
export HEARTH_CONFIG_DIR="$SMOKE_ROOT/hearth"
export HEARTH_HERD_ROOT="$SMOKE_ROOT/herd"
export HEARTH_HOMEBREW_ROOT="$SMOKE_ROOT/homebrew"
mkdir -p "$HEARTH_CONFIG_DIR" "$HEARTH_HERD_ROOT" "$HEARTH_HOMEBREW_ROOT"

REAL_HEARTH_DIR="$HOME/Library/Application Support/hearth"
REAL_SOCK="$REAL_HEARTH_DIR/hearth.sock"
ISOLATED_SOCK="$HEARTH_CONFIG_DIR/hearth.sock"

if [ "$ISOLATED_SOCK" = "$REAL_SOCK" ]; then
    echo "FATAL: isolation failed — isolated socket equals the real socket path"
    exit 1
fi

# ── Free ports ───────────────────────────────────────────────────
# One allocation pass so all six ports are distinct AND dump_port+1 (the dump
# subscriber relay binds it implicitly) is reserved and free.
read -r DUMP_PORT MCP_PORT DNS_PORT MYSQL_PORT POSTGRES_PORT REDIS_PORT <<EOF
$(python3 - <<'PYEOF'
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
)
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

echo "Isolated root: $SMOKE_ROOT (dump:$DUMP_PORT mcp:$MCP_PORT)"

# ── Real-state guard (hash/existence before, re-check after) ─────
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
    # <sha256+mode> or "absent" — never file contents.
    if [ -e "$1" ]; then
        printf '%s %s\n' "$(shasum -a 256 "$1" 2>/dev/null | cut -d' ' -f1)" \
            "$(stat -f '%Sp' "$1" 2>/dev/null)"
    else
        echo "absent"
    fi
}

REAL_STATE_BEFORE="$SMOKE_ROOT/real-state-before.txt"
: > "$REAL_STATE_BEFORE"
for p in "${REAL_PATHS[@]}" "${MUST_STAY_ABSENT[@]}"; do
    printf '%s => %s\n' "$p" "$(state_line "$p")" >> "$REAL_STATE_BEFORE"
done
REAL_SOCK_BEFORE="$(stat -f '%i' "$REAL_SOCK" 2>/dev/null || echo absent)"

check_real_state_unchanged() {
    local after="$SMOKE_ROOT/real-state-after.txt"
    : > "$after"
    for p in "${REAL_PATHS[@]}" "${MUST_STAY_ABSENT[@]}"; do
        printf '%s => %s\n' "$p" "$(state_line "$p")" >> "$after"
    done
    local sock_after
    sock_after="$(stat -f '%i' "$REAL_SOCK" 2>/dev/null || echo absent)"
    if diff -q "$REAL_STATE_BEFORE" "$after" > /dev/null 2>&1 \
        && [ "$REAL_SOCK_BEFORE" = "$sock_after" ]; then
        pass "real config/socket/ambient files unchanged (hash+mode+inode)"
    else
        fail "REAL STATE CHANGED during smoke run:"
        diff "$REAL_STATE_BEFORE" "$after" || true
        [ "$REAL_SOCK_BEFORE" != "$sock_after" ] && echo "    real socket inode: $REAL_SOCK_BEFORE -> $sock_after"
    fi
}

cleanup() {
    info "Cleaning up..."
    [ -n "${DAEMON_PID:-}" ] && kill "$DAEMON_PID" 2>/dev/null && wait "$DAEMON_PID" 2>/dev/null
    [ -n "${DUMP_PID:-}" ] && kill "$DUMP_PID" 2>/dev/null
    rm -rf "$SMOKE_ROOT" 2>/dev/null
}
trap cleanup EXIT

# Build first
info "Building workspace..."
cargo build --workspace 2>&1 | tail -1

CLI="./target/debug/hearth"
DAEMON="./target/debug/hearth-daemon"

# ── 1. Daemon starts (isolated) ──────────────────────────────────
info "Starting daemon under HEARTH_CONFIG_DIR=$HEARTH_CONFIG_DIR ..."
$DAEMON 2>"$SMOKE_ROOT/daemon.log" &
DAEMON_PID=$!
sleep 2

if kill -0 "$DAEMON_PID" 2>/dev/null; then
    pass "Daemon started (PID $DAEMON_PID)"
else
    fail "Daemon failed to start"
    sed 's/^/    /' "$SMOKE_ROOT/daemon.log" | tail -5
    exit 1
fi

if [ -S "$ISOLATED_SOCK" ]; then
    pass "Daemon bound the ISOLATED socket"
else
    fail "Isolated socket missing at $ISOLATED_SOCK"
fi

# ── 2. CLI connectivity ──────────────────────────────────────────
info "Testing CLI ↔ daemon communication..."

OUTPUT=$($CLI status 2>&1) && pass "hearth status — connected" || fail "hearth status — connection failed"

if echo "$OUTPUT" | grep -q "SERVICE\|nginx\|php-fpm\|dnsmasq"; then
    pass "Status output contains service table"
else
    fail "Status output missing service table: $OUTPUT"
fi

# ── 3. Sites command ─────────────────────────────────────────────
info "Testing site enumeration..."

if OUTPUT=$($CLI sites 2>&1); then
    pass "hearth sites — command succeeded"
else
    fail "hearth sites — failed"
fi

# ── 4. PHP version listing ───────────────────────────────────────
info "Testing PHP version discovery..."

if OUTPUT=$($CLI php list 2>&1); then
    pass "hearth php list — command succeeded"
else
    fail "hearth php list — failed"
fi

# ── 5. Restart unregistered service → error ──────────────────────
info "Testing error on unregistered service restart..."

OUTPUT=$($CLI restart dump 2>&1) || true

if echo "$OUTPUT" | grep -qi "not registered"; then
    pass "hearth restart dump — correctly returned 'not registered' error"
else
    fail "hearth restart dump — expected 'not registered', got: $OUTPUT"
fi

# ── 6. Dump server broadcast (isolated port) ─────────────────────
info "Testing dump server broadcast on :$DUMP_PORT ..."

$CLI dump > "$SMOKE_ROOT/subscriber.txt" 2>&1 &
DUMP_PID=$!
sleep 0.5

echo "HEARTH_SMOKE_TEST_PAYLOAD_$(date +%s)" | nc -w 1 localhost "$DUMP_PORT" 2>/dev/null

sleep 1

if grep -q "HEARTH_SMOKE_TEST_PAYLOAD" "$SMOKE_ROOT/subscriber.txt" 2>/dev/null; then
    pass "Dump broadcast — subscriber received VarDumper payload"
else
    fail "Dump broadcast — subscriber did not receive payload"
    sed 's/^/    /' "$SMOKE_ROOT/subscriber.txt" 2>/dev/null | tail -3
fi

kill "$DUMP_PID" 2>/dev/null
DUMP_PID=""

# ── 7. hearth add subcommand wired ───────────────────────────────
info "Testing hearth add subcommand parser..."

OUTPUT=$($CLI add --help 2>&1)
if echo "$OUTPUT" | grep -q "horizon" && echo "$OUTPUT" | grep -q "telescope" && \
   echo "$OUTPUT" | grep -q "pulse" && echo "$OUTPUT" | grep -q "reverb"; then
    pass "hearth add — all 4 packages registered"
else
    fail "hearth add --help missing expected packages"
fi

# ── 8. hearth add dry-run against an explicit Laravel path ───────
info "Testing hearth add dry-run resolution..."

mkdir -p "$SMOKE_ROOT/laravel"
cat > "$SMOKE_ROOT/laravel/composer.json" <<EOF
{"require":{"laravel/framework":"^11.0","laravel/telescope":"^5.0"}}
EOF
echo "APP_ENV=local" > "$SMOKE_ROOT/laravel/.env"

# Explicit --site paths that look like Laravel apps are accepted (synthesized
# site) — dry-run must succeed without touching anything; a clean linked-site
# error is also acceptable when PHP resolution fails in isolation.
OUTPUT=$($CLI add telescope --site="$SMOKE_ROOT/laravel" --yes --dry-run 2>&1) || true
if echo "$OUTPUT" | grep -qiE "done|dry-run|not.*linked|could not resolve PHP"; then
    pass "hearth add — dry-run resolution behaves (got: $(echo "$OUTPUT" | head -1))"
else
    fail "hearth add dry-run unexpected: $OUTPUT"
fi

# ── 9. MCP server reachable (isolated port) ──────────────────────
info "Testing MCP Streamable HTTP endpoint on :$MCP_PORT ..."

sleep 1

# The Streamable HTTP response may be an SSE stream that stays open, so cap
# the read time and inspect whatever body arrived instead of trusting curl's
# exit code.
MCP_BODY=$(curl -s -N -m 5 "http://127.0.0.1:$MCP_PORT/mcp" -X POST \
    -H "Content-Type: application/json" \
    -H "Accept: application/json, text/event-stream" \
    -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"smoke-test","version":"0.1.0"}}}' \
    2>/dev/null || true)
if echo "$MCP_BODY" | grep -q '"jsonrpc"'; then
    pass "MCP endpoint responded (initialize answered)"
else
    fail "MCP endpoint not reachable at :$MCP_PORT (body: $(echo "$MCP_BODY" | head -c 120))"
fi

# ── 10. DB engines (isolated ports/dirs) ─────────────────────────
info "Testing DB engine commands..."

if OUTPUT=$($CLI db status 2>&1); then
    pass "hearth db status — command succeeded"
else
    fail "hearth db status — failed"
fi

if command -v python3 >/dev/null 2>&1; then
    if $CLI db status --json 2>/dev/null | python3 -c 'import sys, json; json.load(sys.stdin)'; then
        pass "hearth db status --json — valid JSON"
    else
        fail "hearth db status --json — output is not valid JSON"
    fi
fi

OUTPUT=$($CLI db stop 2>&1) && pass "hearth db stop — accepted" || fail "hearth db stop — failed: $OUTPUT"

# ── 11. php config surface (stage B, fully isolated) ─────────────
info "Testing php config V2 surface against the isolated daemon..."

if OUTPUT=$($CLI php config --global memory_limit 1G 2>&1); then
    pass "hearth php config --global — exit 0 regardless of FPM registration"
else
    fail "hearth php config --global — failed: $OUTPUT"
fi

if OUTPUT=$($CLI php config --status 2>&1); then
    # Isolated provider roots are empty, so either the coverage footer (rows
    # present) or the explicit no-targets line (rows empty) is truthful.
    if echo "$OUTPUT" | grep -qE "Coverage: Hearth guarantees|No PHP targets discovered"; then
        pass "hearth php config --status — truthful coverage output"
    else
        fail "hearth php config --status — unexpected output: $OUTPUT"
    fi
else
    fail "hearth php config --status — failed: $OUTPUT"
fi

if OUTPUT=$($CLI php config --unmanage 2>&1); then
    pass "hearth php config --unmanage — accepted"
else
    fail "hearth php config --unmanage — failed: $OUTPUT"
fi

# ── 12. Real state unchanged ─────────────────────────────────────
info "Verifying real user state is untouched..."
check_real_state_unchanged

# ── Summary ──────────────────────────────────────────────────────
echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo -e " Results: ${GREEN}$PASS passed${NC}, ${RED}$FAIL failed${NC}"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

[ "$FAIL" -eq 0 ] && exit 0 || exit 1
