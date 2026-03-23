#!/usr/bin/env bash
set -uo pipefail

# Hearth Smoke Test — verifies daemon ↔ CLI ↔ dump server end-to-end
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

cleanup() {
    info "Cleaning up..."
    [ -n "${DAEMON_PID:-}" ] && kill "$DAEMON_PID" 2>/dev/null && wait "$DAEMON_PID" 2>/dev/null
    [ -n "${DUMP_PID:-}" ] && kill "$DUMP_PID" 2>/dev/null
    rm -rf /tmp/hearth-smoke-test 2>/dev/null
    rm -f ~/Library/Application\ Support/hearth/hearth.sock 2>/dev/null
}
trap cleanup EXIT

# Build first
info "Building workspace..."
cargo build --workspace 2>&1 | tail -1

CLI="./target/debug/hearth"
DAEMON="./target/debug/hearth-daemon"

# ── 1. Daemon starts ──────────────────────────────────────────────
info "Starting daemon..."
$DAEMON 2>/dev/null &
DAEMON_PID=$!
sleep 2

if kill -0 "$DAEMON_PID" 2>/dev/null; then
    pass "Daemon started (PID $DAEMON_PID)"
else
    fail "Daemon failed to start"
    exit 1
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

if echo "$OUTPUT" | grep -qE "[0-9]+\.[0-9]+"; then
    pass "PHP versions found in output"
else
    warn "No PHP versions found (expected if PHP not installed)"
fi

# ── 5. Restart unregistered service → error ──────────────────────
info "Testing error on unregistered service restart..."

OUTPUT=$($CLI restart dump 2>&1) || true

if echo "$OUTPUT" | grep -qi "not registered"; then
    pass "hearth restart dump — correctly returned 'not registered' error"
else
    fail "hearth restart dump — expected 'not registered', got: $OUTPUT"
fi

# ── 6. Restart registered service ────────────────────────────────
info "Testing restart of registered service..."

if OUTPUT=$($CLI restart nginx 2>&1); then
    pass "hearth restart nginx — command accepted"
else
    warn "nginx restart returned error (expected if nginx binary not installed)"
fi

# ── 7. Dump server broadcast ─────────────────────────────────────
info "Testing dump server broadcast..."

DUMP_OUTPUT="/tmp/hearth-smoke-test"
mkdir -p "$DUMP_OUTPUT"

# Connect as subscriber in background
$CLI dump > "$DUMP_OUTPUT/subscriber.txt" 2>&1 &
DUMP_PID=$!
sleep 0.5

# Send test payload as VarDumper client
echo "HEARTH_SMOKE_TEST_PAYLOAD_$(date +%s)" | nc -w 1 localhost 9912 2>/dev/null

sleep 1

# Check if subscriber received the payload
if [ -f "$DUMP_OUTPUT/subscriber.txt" ] && grep -q "HEARTH_SMOKE_TEST_PAYLOAD" "$DUMP_OUTPUT/subscriber.txt" 2>/dev/null; then
    pass "Dump broadcast — subscriber received VarDumper payload"
else
    fail "Dump broadcast — subscriber did not receive payload"
    [ -f "$DUMP_OUTPUT/subscriber.txt" ] && echo "    Subscriber output: $(cat "$DUMP_OUTPUT/subscriber.txt")"
fi

kill "$DUMP_PID" 2>/dev/null
DUMP_PID=""

# ── 8. Park command ──────────────────────────────────────────────
info "Testing park command..."

mkdir -p /tmp/hearth-smoke-test/sites

if OUTPUT=$($CLI park /tmp/hearth-smoke-test/sites 2>&1); then
    pass "hearth park — command accepted"
else
    warn "park failed (expected if Valet not installed)"
fi

# ── 9. MCP server reachable ──────────────────────────────────────
info "Testing MCP Streamable HTTP endpoint..."

sleep 1  # give MCP server time to bind

if curl -sf http://127.0.0.1:9900/mcp -X POST \
    -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"smoke-test","version":"0.1.0"}}}' \
    -o /tmp/hearth-smoke-test/mcp-response.json 2>/dev/null; then
    pass "MCP endpoint responded"
else
    fail "MCP endpoint not reachable at :9900"
fi

# ── Summary ──────────────────────────────────────────────────────
echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo -e " Results: ${GREEN}$PASS passed${NC}, ${RED}$FAIL failed${NC}"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

[ "$FAIL" -eq 0 ] && exit 0 || exit 1
