#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

fail() {
  echo "E2E FAIL: $*" >&2
  exit 1
}

pass() {
  echo "  ok: $*"
}

for tool in git curl python3 ps; do
  command -v "$tool" >/dev/null 2>&1 || fail "required tool missing: $tool"
done

echo "==> building release binary"
cargo build --release --quiet
BIN="$ROOT/target/release/sp"
[ -x "$BIN" ] || fail "release binary was not produced"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/sp-e2e.XXXXXX")"
MASTER="$TMP/workspace"
DAEMON_PID=""
UI_PID=""
cleanup() {
  if [ -n "$UI_PID" ]; then kill "$UI_PID" >/dev/null 2>&1 || true; fi
  if [ -n "$DAEMON_PID" ]; then kill "$DAEMON_PID" >/dev/null 2>&1 || true; fi
  if [ -n "$UI_PID" ]; then wait "$UI_PID" >/dev/null 2>&1 || true; fi
  if [ -n "$DAEMON_PID" ]; then wait "$DAEMON_PID" >/dev/null 2>&1 || true; fi
  rm -rf "$TMP"
}
trap cleanup EXIT

sp() {
  SP_MASTER_ROOT="$MASTER" "$BIN" "$@"
}

wait_for_daemon() {
  local deadline=$((SECONDS + 15))
  while [ "$SECONDS" -lt "$deadline" ]; do
    if sp status --json >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.05
  done
  return 1
}

echo "==> creating temporary master workspace with two microservice repositories"
mkdir -p "$MASTER/auth-service" "$MASTER/payment-service"
for repo in auth-service payment-service; do
  git -C "$MASTER/$repo" init -q -b main
  git -C "$MASTER/$repo" config user.name "ScratchPost E2E"
  git -C "$MASTER/$repo" config user.email "e2e@scratchpost.local"
  git -C "$MASTER/$repo" config commit.gpgsign false
  cat >"$MASTER/$repo/dbchangelog.xml" <<'XML'
<databaseChangeLog>
  <changeSet id="100" author="base">
    <sql>select 100;</sql>
  </changeSet>
</databaseChangeLog>
XML
  printf 'repository %s\n' "$repo" >"$MASTER/$repo/README.md"
  git -C "$MASTER/$repo" add .
  git -C "$MASTER/$repo" commit -q -m "initial ${repo}"
done
printf '# shared workspace context for parallel agents\n' >"$MASTER/.ai-context.md"
pass "workspace at $MASTER"

echo "==> initializing ScratchPost"
sp init >"$TMP/init.out"
grep -q "discovered 2 repository(ies)" "$TMP/init.out" || fail "expected two discovered repositories"
[ -d "$MASTER/.scratchpost" ] || fail "state directory missing"
[ -x "$MASTER/.scratchpost/bin/git" ] || fail "git shim missing"
[ -x "$MASTER/.scratchpost/hooks/pre-commit" ] || fail "pre-commit hook missing"
[ -x "$MASTER/.scratchpost/hooks/pre-push" ] || fail "pre-push hook missing"
pass "managed state, shim, and hooks installed"

UI_PORT=19200
python3 - "$MASTER/.scratchpost/config.json" "$UI_PORT" <<'PY'
import json, sys
path, port = sys.argv[1], int(sys.argv[2])
with open(path) as handle:
    config = json.load(handle)
config["ui_port"] = port
with open(path, "w") as handle:
    json.dump(config, handle, indent=2)
PY

echo "==> starting scratchpostd"
# `sp init` already provisions the daemon; only start one if it is not live.
if ! sp status --json >/dev/null 2>&1; then
  SP_MASTER_ROOT="$MASTER" "$BIN" daemon >"$TMP/daemon.log" 2>&1 &
  DAEMON_PID=$!
  wait_for_daemon || {
    cat "$TMP/daemon.log" >&2
    fail "daemon did not become ready"
  }
  grep -q "daemon.ready" "$TMP/daemon.log" || fail "daemon readiness event missing"
fi
pass "daemon ready (pid ${DAEMON_PID:-from init})"

echo "==> provisioning two isolated agent sessions"
sp run --agent agent_1 -- /usr/bin/true >"$TMP/agent_1.provision"
sp run --agent agent_2 -- /usr/bin/true >"$TMP/agent_2.provision"
A1WS="$MASTER/.scratchpost/sessions/agent_1/workspace"
A2WS="$MASTER/.scratchpost/sessions/agent_2/workspace"
[ -d "$A1WS/auth-service" ] || fail "agent_1 worktree missing"
[ -d "$A2WS/auth-service" ] || fail "agent_2 worktree missing"
[ "$A1WS" != "$A2WS" ] || fail "agent workspaces are not isolated"
[ -f "$A1WS/.ai-context.md" ] || fail "shared context was not copied into agent_1 workspace"
[ -f "$A2WS/.ai-context.md" ] || fail "shared context was not copied into agent_2 workspace"
sp status --json >"$TMP/status-initial.json"
python3 - "$TMP/status-initial.json" <<'PY'
import json, sys
with open(sys.argv[1]) as handle:
    state = json.load(handle)
sessions = {session["agent"]: session for session in state["sessions"]}
assert sessions["agent_1"]["workspace"] != sessions["agent_2"]["workspace"], "workspace collision"
assert sessions["agent_1"]["port_base"] != sessions["agent_2"]["port_base"], "port band collision"
assert sessions["agent_1"]["slot"] != sessions["agent_2"]["slot"], "session slot collision"
print("  ok: agents use distinct slots, port bands, and workspaces")
PY
grep -q "SP_PORT_BASE=31000" "$TMP/agent_1.provision" || true
pass "session isolation and resource leases verified"

write_changeset() {
  local file="$1" author="$2" key="${3:-200}"
  python3 - "$file" "$author" "$key" <<'PY'
from pathlib import Path
import sys
path, author, key = Path(sys.argv[1]), sys.argv[2], sys.argv[3]
text = path.read_text()
marker = "</databaseChangeLog>"
if marker not in text:
    raise SystemExit(f"missing {marker} in {path}")
block = (
    f'  <changeSet id="{key}" author="{author}">\n'
    f'    <sql>select {key};</sql>\n'
    '  </changeSet>\n'
    '</databaseChangeLog>'
)
path.write_text(text.replace(marker, block, 1))
PY
}

echo "==> agent_1 adds Liquibase changeSet id=200"
write_changeset "$A1WS/auth-service/dbchangelog.xml" agent_1
sp run --agent agent_1 -- /usr/bin/git -C auth-service add dbchangelog.xml >/dev/null
REAL_GIT="$(command -v git)"
set +e
sp run --agent agent_1 -- "$REAL_GIT" -C auth-service commit -m "unmanaged bypass" >"$TMP/bypass.out" 2>"$TMP/bypass.err"
BYPASS_CODE=$?
set -e
[ "$BYPASS_CODE" -eq 1 ] || fail "direct unmanaged git commit exited $BYPASS_CODE, expected hook rejection"
grep -Fq 'commits must originate from the managed ScratchPost git shim' "$TMP/bypass.err" || fail "hook bypass rejection message missing"
pass "managed hooks reject direct unmanaged git commits"
set +e
sp run --agent agent_1 -- /bin/sh -c 'cd auth-service && git commit -m "agent_1 adds changeSet 200"' >"$TMP/agent_1.commit.out" 2>"$TMP/agent_1.commit.err"
A1_CODE=$?
set -e
[ "$A1_CODE" -eq 0 ] || {
  cat "$TMP/agent_1.commit.out" "$TMP/agent_1.commit.err" >&2
  fail "agent_1 commit exited $A1_CODE, expected 0"
}
sp claims --json >"$TMP/claims-after-agent-1.json"
python3 - "$TMP/claims-after-agent-1.json" <<'PY'
import json, sys
with open(sys.argv[1]) as handle:
    claims = json.load(handle)
match = [c for c in claims if c["agent"] == "agent_1" and c["key"] == "200"]
assert match, "agent_1 sequence claim 200 missing"
assert match[0]["state"] == "committed", match[0]["state"]
assert match[0]["namespace"] == "auth-service:dbchangelog.xml", match[0]["namespace"]
PY
pass "agent_1 committed and sequence claim 200 is committed"

echo "==> agent_2 independently adds Liquibase changeSet id=200"
write_changeset "$A2WS/auth-service/dbchangelog.xml" agent_2
sp run --agent agent_2 -- /usr/bin/git -C auth-service add dbchangelog.xml >/dev/null
set +e
sp run --agent agent_2 -- /bin/sh -c 'cd auth-service && git commit -m "agent_2 adds changeSet 200"' >"$TMP/agent_2.commit.out" 2>"$TMP/agent_2.commit.err"
A2_CODE=$?
set -e
[ "$A2_CODE" -eq 1 ] || {
  cat "$TMP/agent_2.commit.out" "$TMP/agent_2.commit.err" >&2
  fail "agent_2 conflicting commit exited $A2_CODE, expected 1"
}
for needle in 'SP_BLOCKED v1' 'SP_SEQUENCE_COLLISION' 'lowest_free: 201' 'SP_CONFLICT_JSON='; do
  grep -Fq "$needle" "$TMP/agent_2.commit.err" || fail "blocked stderr missing: $needle"
done
python3 - "$TMP/agent_2.commit.err" <<'PY'
import json, sys
lines = [line for line in open(sys.argv[1]) if line.startswith("SP_CONFLICT_JSON=")]
assert len(lines) == 1, f"expected exactly one SP_CONFLICT_JSON line, got {len(lines)}"
report = json.loads(lines[0].split("=", 1)[1])
assert report["schema"] == "sp.conflict.v1", report.get("schema")
assert report["code"] == "SP_SEQUENCE_COLLISION", report.get("code")
assert report["agent"] == "agent_2", report.get("agent")
assert report["evidence"]["requested"] == "200", report["evidence"]
assert report["evidence"]["lowest_free"] == "201", report["evidence"]
assert report["evidence"]["namespace"] == "auth-service:dbchangelog.xml", report["evidence"]
assert report["retry"]["mode"] == "rerun_original_command", report["retry"]
assert report["resolution"]["steps"], "missing resolution steps"
print("  ok: SP_CONFLICT_JSON schema, code, evidence, and retry contract verified")
PY
pass "agent_2 was blocked with deterministic passive resolution prompt"

echo "==> applying the suggested resequence and retrying agent_2"
grep -Fq 'sp resequence --agent agent_2 --file auth-service/dbchangelog.xml --from 200 --to 201' "$TMP/agent_2.commit.err" || fail "suggested resequence command missing"
sp resequence --agent agent_2 --file auth-service/dbchangelog.xml --from 200 --to 201 >"$TMP/resequence.out"
grep -q 'id="201"' "$A2WS/auth-service/dbchangelog.xml" || fail "resequence did not rewrite changeSet id to 201"
set +e
sp run --agent agent_2 -- /bin/sh -c 'cd auth-service && git commit -m "agent_2 adds changeSet 201"' >"$TMP/agent_2.retry.out" 2>"$TMP/agent_2.retry.err"
A2_RETRY_CODE=$?
set -e
[ "$A2_RETRY_CODE" -eq 0 ] || {
  cat "$TMP/agent_2.retry.out" "$TMP/agent_2.retry.err" >&2
  fail "agent_2 retry exited $A2_RETRY_CODE, expected 0"
}
sp claims --json >"$TMP/claims-final.json"
python3 - "$TMP/claims-final.json" <<'PY'
import json, sys
with open(sys.argv[1]) as handle:
    claims = json.load(handle)
pairs = {(c["agent"], c["key"], c["state"]) for c in claims if c["namespace"] == "auth-service:dbchangelog.xml"}
assert ("agent_1", "200", "committed") in pairs, pairs
assert ("agent_2", "201", "committed") in pairs, pairs
PY
pass "resequenced commit 201 recorded as committed"

echo "==> smoke testing MCP JSON-RPC over stdio"
cat >"$TMP/mcp.in" <<'JSON'
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"verify-e2e","version":"1"}}}
{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"sp_status","arguments":{}}}
JSON
SP_MASTER_ROOT="$MASTER" "$BIN" --mcp <"$TMP/mcp.in" >"$TMP/mcp.out" 2>"$TMP/mcp.err"
python3 - "$TMP/mcp.out" <<'PY'
import json, sys
messages = [json.loads(line) for line in open(sys.argv[1]) if line.strip()]
by_id = {message["id"]: message for message in messages}
assert by_id[1]["result"]["protocolVersion"] == "2024-11-05", by_id[1]
names = {tool["name"] for tool in by_id[2]["result"]["tools"]}
for required in ("sp_status", "sp_notices", "sp_sequence_next", "sp_resequence"):
    assert required in names, (required, names)
status = by_id[3]["result"]
assert status["isError"] is False, status
payload = status["structuredContent"]
assert len(payload["sessions"]) >= 2, payload["sessions"]
assert any(claim["key"] == "201" for claim in payload["claims"]), payload["claims"]
print("  ok: MCP initialize, tools/list, and sp_status verified")
PY
pass "stdio MCP server responses valid"

echo "==> smoke testing embedded Web UI and SSE"
SP_MASTER_ROOT="$MASTER" "$BIN" ui >"$TMP/ui.log" 2>&1 &
UI_PID=$!
UI_READY=0
for _ in $(seq 1 100); do
  if curl -fsS "http://127.0.0.1:${UI_PORT}/" >"$TMP/dashboard.html" 2>/dev/null; then
    UI_READY=1
    break
  fi
  sleep 0.05
done
[ "$UI_READY" -eq 1 ] || {
  cat "$TMP/ui.log" >&2
  fail "UI did not become ready on port $UI_PORT"
}
grep -q 'ScratchPost Mission Control' "$TMP/dashboard.html" || fail "dashboard HTML missing"
grep -q 'Live Agent Lanes' "$TMP/dashboard.html" || fail "dashboard lane panel missing"
grep -q 'Sequence Registry' "$TMP/dashboard.html" || fail "dashboard sequence panel missing"
curl -fsS "http://127.0.0.1:${UI_PORT}/api/status" >"$TMP/ui-status.json"
python3 - "$TMP/ui-status.json" <<'PY'
import json, sys
with open(sys.argv[1]) as handle:
    state = json.load(handle)
assert len(state["sessions"]) >= 2, state["sessions"]
assert any(claim["key"] == "201" for claim in state["claims"]), state["claims"]
assert "config" in state, state
PY
curl -sS --max-time 1 "http://127.0.0.1:${UI_PORT}/events" >"$TMP/sse.out" 2>/dev/null || true
grep -q '^data:' "$TMP/sse.out" || fail "SSE stream did not emit a data frame"
pass "dashboard, /api/status, and /events SSE verified"

echo "==> smoke testing barrier control surface"
sp config --mode barrier-stacked >/dev/null
sp barrier arrive --agent agent_1 >"$TMP/barrier-1.json"
sp barrier arrive --agent agent_2 >"$TMP/barrier-2.json"
python3 - "$TMP/barrier-2.json" <<'PY'
import json, sys
with open(sys.argv[1]) as handle:
    barrier = json.load(handle)
assert barrier["released"] is True, barrier
assert set(barrier["participants"]) == {"agent_1", "agent_2"}, barrier
PY
sp barrier release >/dev/null
sp config --mode fcfs >/dev/null
pass "barrier arrive/release and mode switching verified"

echo "==> verifying barrier mode gates commits until peers arrive"
sp config --mode barrier-stacked >/dev/null
write_changeset "$A1WS/auth-service/dbchangelog.xml" agent_1 300
sp run --agent agent_1 -- /usr/bin/git -C auth-service add dbchangelog.xml >/dev/null
set +e
sp run --agent agent_1 -- /bin/sh -c 'cd auth-service && git commit -m "agent_1 barrier 300"' >"$TMP/barrier-a1.out" 2>"$TMP/barrier-a1.err"
BARRIER_A1_CODE=$?
set -e
[ "$BARRIER_A1_CODE" -eq 1 ] || fail "first barrier commit exited $BARRIER_A1_CODE, expected 1"
grep -Fq 'SP_BARRIER_WAIT' "$TMP/barrier-a1.err" || fail "barrier wait conflict code missing"
write_changeset "$A2WS/auth-service/dbchangelog.xml" agent_2 301
sp run --agent agent_2 -- /usr/bin/git -C auth-service add dbchangelog.xml >/dev/null
set +e
sp run --agent agent_2 -- /bin/sh -c 'cd auth-service && git commit -m "agent_2 releases barrier 301"' >"$TMP/barrier-a2.out" 2>"$TMP/barrier-a2.err"
BARRIER_A2_CODE=$?
set -e
[ "$BARRIER_A2_CODE" -eq 0 ] || {
  cat "$TMP/barrier-a2.out" "$TMP/barrier-a2.err" >&2
  fail "peer barrier commit exited $BARRIER_A2_CODE, expected 0"
}
set +e
sp run --agent agent_1 -- /bin/sh -c 'cd auth-service && git commit -m "agent_1 retry barrier 300"' >"$TMP/barrier-a1-retry.out" 2>"$TMP/barrier-a1-retry.err"
BARRIER_RETRY_CODE=$?
set -e
[ "$BARRIER_RETRY_CODE" -eq 0 ] || {
  cat "$TMP/barrier-a1-retry.out" "$TMP/barrier-a1-retry.err" >&2
  fail "retry after barrier release exited $BARRIER_RETRY_CODE, expected 0"
}
sp claims --json >"$TMP/claims-barrier.json"
python3 - "$TMP/claims-barrier.json" <<'PY'
import json, sys
with open(sys.argv[1]) as handle:
    claims = json.load(handle)
pairs = {(c["agent"], c["key"], c["state"]) for c in claims if c["namespace"] == "auth-service:dbchangelog.xml"}
assert ("agent_1", "300", "committed") in pairs, pairs
assert ("agent_2", "301", "committed") in pairs, pairs
PY
sp config --mode fcfs >/dev/null
pass "barrier mode blocked first commit, released on peer arrival, and admitted retry"

echo "==> measuring scratchpostd resident memory"
sleep 0.2
if [ -z "$DAEMON_PID" ]; then
  # The daemon was provisioned by `sp init`; it publishes its own pid.
  DAEMON_PID="$(cat "$MASTER/.scratchpost/run/daemon.pid")"
fi
RSS_KB="$(ps -o rss= -p "$DAEMON_PID" | tr -d ' ')"
[ -n "$RSS_KB" ] || fail "cannot read daemon RSS"
[ "$RSS_KB" -lt 15360 ] || fail "daemon RSS ${RSS_KB} KB exceeds 15360 KB"
pass "daemon RSS ${RSS_KB} KB < 15360 KB"

echo
echo "E2E PASS: ScratchPost commit gate, sequence resequencing, MCP, UI/SSE, barrier controls, and memory budget verified."
