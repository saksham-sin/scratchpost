#!/usr/bin/env bash
# Enterprise polyrepo simulation: 5 concurrent agents across 3 microservices.
# Produces demo-artifacts/logs/* and leaves the workspace running for UI capture.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

SIM="${SP_SIM_ROOT:-/tmp/sp-enterprise-sim}"
ART="$ROOT/demo-artifacts"
LOGS="$ART/logs"
BIN="$ROOT/target/release/sp"
UI_PORT="${SP_SIM_UI_PORT:-19200}"

fail() { echo "SIM FAIL: $*" >&2; exit 1; }
pass() { echo "  ok: $*"; }

for tool in git curl python3 ps; do
  command -v "$tool" >/dev/null 2>&1 || fail "required tool missing: $tool"
done

echo "==> building release binary"
cargo build --release --quiet
[ -x "$BIN" ] || fail "release binary missing"

echo "==> resetting simulation workspace at $SIM"
rm -rf "$SIM"
mkdir -p "$SIM/auth-service/src/main/resources/db" \
         "$SIM/auth-service/src/main/java/com/health/auth" \
         "$SIM/billing-service/db/migration" \
         "$SIM/billing-service/src/main/java/com/health/billing" \
         "$SIM/notification-service/src"

cat >"$SIM/auth-service/src/main/resources/db/dbchangelog.xml" <<'XML'
<?xml version="1.0" encoding="UTF-8"?>
<databaseChangeLog xmlns="http://www.liquibase.org/xml/ns/dbchangelog">
  <changeSet id="199" author="core">
    <createTable tableName="users">
      <column name="id" type="BIGINT"/>
    </createTable>
  </changeSet>
</databaseChangeLog>
XML
cat >"$SIM/auth-service/src/main/java/com/health/auth/UserDTO.java" <<'JAVA'
package com.health.auth;

public record UserDTO(String id, String email) {}
JAVA
cat >"$SIM/billing-service/db/migration/V41__baseline.sql" <<'SQL'
CREATE TABLE invoices (id BIGSERIAL PRIMARY KEY, total NUMERIC(12,2));
SQL
cat >"$SIM/billing-service/src/main/java/com/health/billing/InvoiceController.java" <<'JAVA'
package com.health.billing;

public class InvoiceController {}
JAVA
cat >"$SIM/notification-service/src/NotificationDispatcher.ts" <<'TS'
export class NotificationDispatcher {
  dispatch(channel: string, payload: unknown): void {
    void channel;
    void payload;
  }
}
TS
cat >"$SIM/.ai-context.md" <<'MD'
# Philips Health Platform - Shared Agent Context

- Polyrepo workspace: auth-service, billing-service, notification-service.
- Database migrations are strictly ordered: Liquibase changeSet ids and Flyway V<num> prefixes.
- Never reuse a migration sequence number that already exists on main.
MD
cat >"$SIM/.env.shared" <<'ENV'
PLATFORM_REGION=eu-west-1
DB_POOL_SIZE=8
ENV

for repo in auth-service billing-service notification-service; do
  git -C "$SIM/$repo" init -q -b main
  git -C "$SIM/$repo" config user.name "Philips Platform"
  git -C "$SIM/$repo" config user.email "platform@philips.example"
  git -C "$SIM/$repo" config commit.gpgsign false
  git -C "$SIM/$repo" add .
  git -C "$SIM/$repo" commit -q -m "baseline ${repo}"
done
pass "3 microservice repositories + shared context created"

echo "==> sp init"
SP_MASTER_ROOT="$SIM" "$BIN" init >"$LOGS/10-sim-init.log" 2>&1
grep -q "discovered 3 repository(ies)" "$LOGS/10-sim-init.log" || fail "expected 3 discovered repositories"

python3 - "$SIM/.scratchpost/config.json" "$UI_PORT" <<'PY'
import json, sys
path, port = sys.argv[1], int(sys.argv[2])
with open(path) as handle:
    config = json.load(handle)
config["ui_port"] = port
config["barrier_timeout_ms"] = 20000
with open(path, "w") as handle:
    json.dump(config, handle, indent=2)
PY

echo "==> starting scratchpostd"
# `sp init` already provisions the daemon; only start one if it is not live.
if ! SP_MASTER_ROOT="$SIM" "$BIN" status --json >/dev/null 2>&1; then
  SP_MASTER_ROOT="$SIM" "$BIN" daemon >"$LOGS/11-sim-daemon.log" 2>&1 &
  for _ in $(seq 1 200); do
    SP_MASTER_ROOT="$SIM" "$BIN" status --json >/dev/null 2>&1 && break
    sleep 0.05
  done
fi
SP_MASTER_ROOT="$SIM" "$BIN" status --json >/dev/null 2>&1 || fail "daemon did not become ready"
DAEMON_PID="$(cat "$SIM/.scratchpost/run/daemon.pid")"
pass "daemon ready (pid $DAEMON_PID)"

sp() { SP_MASTER_ROOT="$SIM" "$BIN" "$@"; }

echo "==> attaching 5 agent sessions"
for n in 1 2 3 4 5; do
  sp run --agent "agent_$n" -- /usr/bin/true >"$LOGS/12-session-agent_$n.log" 2>&1
done
sp status --json >"$LOGS/13-sessions.json"
python3 - "$LOGS/13-sessions.json" <<'PY'
import json, sys
state = json.load(open(sys.argv[1]))
sessions = {s["agent"]: s for s in state["sessions"]}
assert len(sessions) == 5, sorted(sessions)
workspaces = {s["workspace"] for s in sessions.values()}
ports = {s["port_base"] for s in sessions.values()}
slots = {s["slot"] for s in sessions.values()}
assert len(workspaces) == 5, workspaces
assert len(ports) == 5, ports
assert len(slots) == 5, slots
for agent, session in sorted(sessions.items()):
    print(f"  {agent}: slot={session['slot']} SP_PORT_BASE={session['port_base']} workspace={session['workspace']}")
PY
pass "5 isolated sessions with distinct port bands"

echo "==> verifying copy-on-write context isolation"
A1WS="$SIM/.scratchpost/sessions/agent_1/workspace"
printf '\n- agent_1 local note: do not leak to master\n' >>"$A1WS/.ai-context.md"
if grep -q "agent_1 local note" "$SIM/.ai-context.md"; then
  fail "agent_1 context write leaked into canonical master"
fi
pass "agent_1 context mutation did not corrupt canonical master"

write_changeset() {
  local file="$1" author="$2" key="$3"
  python3 - "$file" "$author" "$key" <<'PY'
from pathlib import Path
import sys
path, author, key = Path(sys.argv[1]), sys.argv[2], sys.argv[3]
text = path.read_text()
marker = "</databaseChangeLog>"
block = (
    f'  <changeSet id="{key}" author="{author}">\n'
    f'    <sql>select {key};</sql>\n'
    '  </changeSet>\n'
    '</databaseChangeLog>'
)
path.write_text(text.replace(marker, block, 1))
PY
}

commit_as() {
  local agent="$1" repo="$2" message="$3"
  sp run --agent "$agent" -- /bin/sh -c "cd $repo && git commit -m '$message'"
}

echo "==> scenario 1: Liquibase changeSet 200 collision (agent_1 vs agent_2)"
A1WS="$SIM/.scratchpost/sessions/agent_1/workspace"
A2WS="$SIM/.scratchpost/sessions/agent_2/workspace"
write_changeset "$A1WS/auth-service/src/main/resources/db/dbchangelog.xml" agent_1 200
write_changeset "$A2WS/auth-service/src/main/resources/db/dbchangelog.xml" agent_2 200
sp run --agent agent_1 -- /usr/bin/git -C auth-service add src/main/resources/db/dbchangelog.xml >/dev/null
sp run --agent agent_2 -- /usr/bin/git -C auth-service add src/main/resources/db/dbchangelog.xml >/dev/null

set +e
commit_as agent_1 auth-service "agent_1 adds changeSet 200" >"$LOGS/20-agent_1-commit.out" 2>"$LOGS/20-agent_1-commit.err"
A1_CODE=$?
set -e
[ "$A1_CODE" -eq 0 ] || { cat "$LOGS/20-agent_1-commit.err" >&2; fail "agent_1 commit exited $A1_CODE"; }
pass "agent_1 committed changeSet 200 (exit 0)"

set +e
commit_as agent_2 auth-service "agent_2 adds changeSet 200" >"$LOGS/21-agent_2-blocked.out" 2>"$LOGS/02-liquibase-collision-prompt.txt"
A2_CODE=$?
set -e
[ "$A2_CODE" -eq 1 ] || fail "agent_2 conflicting commit exited $A2_CODE, expected 1"
for needle in 'SP_BLOCKED v1' 'SP_SEQUENCE_COLLISION' 'lowest_free: 201' 'SP_CONFLICT_JSON='; do
  grep -Fq "$needle" "$LOGS/02-liquibase-collision-prompt.txt" || fail "blocked prompt missing: $needle"
done
pass "agent_2 blocked with SP_SEQUENCE_COLLISION (exit 1)"

sp resequence --agent agent_2 --file auth-service/src/main/resources/db/dbchangelog.xml --from 200 --to 201 >"$LOGS/22-resequence.out" 2>&1
set +e
commit_as agent_2 auth-service "agent_2 adds changeSet 201" >"$LOGS/23-agent_2-retry.out" 2>"$LOGS/23-agent_2-retry.err"
A2_RETRY=$?
set -e
[ "$A2_RETRY" -eq 0 ] || { cat "$LOGS/23-agent_2-retry.err" >&2; fail "agent_2 retry exited $A2_RETRY"; }
pass "agent_2 resequenced to 201 and committed (exit 0)"

echo "==> scenario 2: Flyway V42 collision (agent_3 vs agent_4)"
A3WS="$SIM/.scratchpost/sessions/agent_3/workspace"
A4WS="$SIM/.scratchpost/sessions/agent_4/workspace"
printf 'CREATE INDEX idx_users_upi ON users (upi);\n' >"$A3WS/billing-service/db/migration/V42__add_upi_index.sql"
printf 'ALTER TABLE invoices ADD COLUMN vat NUMERIC(12,2);\n' >"$A4WS/billing-service/db/migration/V42__add_vat_column.sql"
sp run --agent agent_3 -- /usr/bin/git -C billing-service add db/migration/V42__add_upi_index.sql >/dev/null
sp run --agent agent_4 -- /usr/bin/git -C billing-service add db/migration/V42__add_vat_column.sql >/dev/null

set +e
commit_as agent_3 billing-service "agent_3 adds V42 upi index" >"$LOGS/30-agent_3-commit.out" 2>"$LOGS/30-agent_3-commit.err"
A3_CODE=$?
set -e
[ "$A3_CODE" -eq 0 ] || { cat "$LOGS/30-agent_3-commit.err" >&2; fail "agent_3 commit exited $A3_CODE"; }
pass "agent_3 committed Flyway V42 (exit 0)"

set +e
commit_as agent_4 billing-service "agent_4 adds V42 vat column" >"$LOGS/31-agent_4-blocked.out" 2>"$LOGS/31-agent_4-blocked.err"
A4_CODE=$?
set -e
[ "$A4_CODE" -eq 1 ] || fail "agent_4 conflicting commit exited $A4_CODE, expected 1"
grep -Fq 'SP_SEQUENCE_COLLISION' "$LOGS/31-agent_4-blocked.err" || fail "agent_4 missing SP_SEQUENCE_COLLISION"
grep -Fq 'lowest_free: 43' "$LOGS/31-agent_4-blocked.err" || fail "agent_4 did not recommend version 43"
pass "agent_4 blocked with recommended version 43 (exit 1)"

sp run --agent agent_4 -- /usr/bin/git -C billing-service mv db/migration/V42__add_vat_column.sql db/migration/V43__add_vat_column.sql >/dev/null
set +e
commit_as agent_4 billing-service "agent_4 adds V43 vat column" >"$LOGS/32-agent_4-retry.out" 2>"$LOGS/32-agent_4-retry.err"
A4_RETRY=$?
set -e
[ "$A4_RETRY" -eq 0 ] || { cat "$LOGS/32-agent_4-retry.err" >&2; fail "agent_4 retry exited $A4_RETRY"; }
pass "agent_4 renamed to V43 and committed (exit 0)"

echo "==> scenario 3: orthogonal fast lane (agent_5) under an active barrier"
sp config --mode barrier-stacked --barrier-scope component >"$LOGS/40-mode.log" 2>&1

# Build a genuine contending component in auth-service: agent_1 and agent_2 both
# stage overlapping changes so they share a conflict component.
write_changeset "$A1WS/auth-service/src/main/resources/db/dbchangelog.xml" agent_1 400
write_changeset "$A2WS/auth-service/src/main/resources/db/dbchangelog.xml" agent_2 401
sp run --agent agent_1 -- /usr/bin/git -C auth-service add src/main/resources/db/dbchangelog.xml >/dev/null
sp run --agent agent_2 -- /usr/bin/git -C auth-service add src/main/resources/db/dbchangelog.xml >/dev/null

# agent_1 arrives at the component barrier; agent_2 has not, so the barrier stays open.
set +e
commit_as agent_1 auth-service "agent_1 waits at component barrier" >"$LOGS/41-agent_1-barrier.out" 2>"$LOGS/41-agent_1-barrier.err"
A1_BARRIER=$?
set -e
[ "$A1_BARRIER" -eq 1 ] || fail "agent_1 barrier commit exited $A1_BARRIER, expected 1"
grep -Fq 'SP_BARRIER_WAIT' "$LOGS/41-agent_1-barrier.err" || fail "agent_1 did not report SP_BARRIER_WAIT"
sp barrier status >"$LOGS/41-barrier-open.json" 2>&1
python3 - "$LOGS/41-barrier-open.json" <<'PY'
import json, sys
state = json.load(open(sys.argv[1]))
open_barriers = [b for b in state.get("barriers", []) if b.get("open")]
assert open_barriers, state
assert any("agent_1" in b.get("arrived", []) for b in open_barriers), open_barriers
print(f"  ok: component barrier open with {len(open_barriers)} pending component(s)")
PY

A5WS="$SIM/.scratchpost/sessions/agent_5/workspace"
printf 'export class NotificationDispatcher { dispatch(): void {} }\n' >"$A5WS/notification-service/src/NotificationDispatcher.ts"
sp run --agent agent_5 -- /usr/bin/git -C notification-service add src/NotificationDispatcher.ts >/dev/null
set +e
commit_as agent_5 notification-service "agent_5 dispatcher refactor" >"$LOGS/42-agent_5-commit.out" 2>"$LOGS/42-agent_5-commit.err"
A5_CODE=$?
set -e
[ "$A5_CODE" -eq 0 ] || { cat "$LOGS/42-agent_5-commit.err" >&2; fail "agent_5 orthogonal commit exited $A5_CODE"; }
grep -Fq 'SP_BARRIER_WAIT' "$LOGS/42-agent_5-commit.err" && fail "agent_5 was blocked by an unrelated component barrier"
pass "agent_5 committed in an independent component while auth-service barrier was open (exit 0)"

# Release the auth-service barrier and let both peers commit.
sp barrier release >"$LOGS/43-barrier-release.json" 2>&1
set +e
commit_as agent_2 auth-service "agent_2 releases component barrier" >"$LOGS/44-agent_2.out" 2>"$LOGS/44-agent_2.err"
A2_BARRIER=$?
set -e
[ "$A2_BARRIER" -eq 0 ] || { cat "$LOGS/44-agent_2.err" >&2; fail "agent_2 post-release commit exited $A2_BARRIER"; }
set +e
commit_as agent_1 auth-service "agent_1 retries after release" >"$LOGS/45-agent_1-retry.out" 2>"$LOGS/45-agent_1-retry.err"
A1_RETRY=$?
set -e
[ "$A1_RETRY" -eq 0 ] || { cat "$LOGS/45-agent_1-retry.err" >&2; fail "agent_1 retry exited $A1_RETRY"; }
pass "component barrier released and both peers committed"
sp config --mode fcfs >/dev/null

echo "==> scenario 4: adversarial hook bypass and stale lease recovery"
REAL_GIT="$(command -v git)"
write_changeset "$A2WS/auth-service/src/main/resources/db/dbchangelog.xml" agent_2 200
sp run --agent agent_2 -- /usr/bin/git -C auth-service add src/main/resources/db/dbchangelog.xml >/dev/null
set +e
sp run --agent agent_2 -- "$REAL_GIT" -C auth-service commit -m "bypass attempt" >"$LOGS/50-bypass.out" 2>"$LOGS/50-bypass.err"
BYPASS_CODE=$?
set -e
[ "$BYPASS_CODE" -eq 1 ] || fail "direct /usr/bin/git commit exited $BYPASS_CODE, expected 1"
grep -Fq 'commits must originate from the managed ScratchPost git shim' "$LOGS/50-bypass.err" || fail "hook bypass rejection message missing"
pass "managed core.hooksPath blocked direct /usr/bin/git commit (exit 1)"

sp run --agent agent_6 -- /usr/bin/true >"$LOGS/51-agent_6.log" 2>&1
sp status --json >"$LOGS/52-before-evict.json"
sp evict --agent agent_6 >"$LOGS/53-evict.out" 2>&1
sp status --json >"$LOGS/54-after-evict.json"
python3 - "$LOGS/52-before-evict.json" "$LOGS/54-after-evict.json" <<'PY'
import json, sys
before = {s["agent"]: s["status"] for s in json.load(open(sys.argv[1]))["sessions"]}
after = {s["agent"]: s["status"] for s in json.load(open(sys.argv[2]))["sessions"]}
assert before.get("agent_6") == "active", before
assert after.get("agent_6") == "abandoned", after
print("  ok: stalled agent_6 lease revoked without blocking peers")
PY
pass "abandoned session recovery verified"

echo "==> capturing final ledger state"
sp status --json >"$LOGS/60-final-status.json"
sp claims --json >"$LOGS/61-final-claims.json"
sp notices --json >"$LOGS/62-final-notices.json"
python3 - "$LOGS/61-final-claims.json" <<'PY'
import json, sys
claims = json.load(open(sys.argv[1]))
pairs = {(c["agent"], c["key"], c["state"]) for c in claims}
for expected in [("agent_1","200","committed"),("agent_2","201","committed"),
                 ("agent_3","42","committed"),("agent_4","43","committed")]:
    assert expected in pairs, (expected, sorted(pairs))
print("  ok: sequence registry contains 200, 201, 42, 43 as committed")
PY

echo "==> measuring daemon memory"
sleep 0.3
RSS_KB="$(ps -o rss= -p "$DAEMON_PID" | tr -d ' ')"
echo "$RSS_KB" >"$LOGS/70-daemon-rss-kb.txt"
[ "$RSS_KB" -lt 25600 ] || fail "daemon RSS ${RSS_KB} KB exceeds 25600 KB"
pass "daemon RSS ${RSS_KB} KB < 25600 KB"

echo
echo "SIM PASS: 5-agent enterprise simulation complete."
echo "daemon_pid=$DAEMON_PID master=$SIM ui_port=$UI_PORT"
