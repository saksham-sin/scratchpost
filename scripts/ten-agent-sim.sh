#!/usr/bin/env bash
# Ten-agent parallel stress simulation.
#
# Case 1 (control): ten agents share one working directory with plain git. Branch
#                   hijacking, silent Liquibase/Flyway collisions and port
#                   collisions are reproduced and measured.
# Case 2 (ScratchPost): the same ten agents run in isolated worktrees with
#                   daemon-arbitrated sequence allocation. Every collision is
#                   blocked deterministically and self-corrected.
#
# Usage: bash scripts/ten-agent-sim.sh [--keep]
#   --keep   leave the workspace and daemon running for UI capture
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

SIM="${SP_SIM_ROOT:-/tmp/sp-ten-agent-sim}"
CTRL="${SP_CONTROL_ROOT:-/tmp/sp-ten-agent-control}"
ART="$ROOT/demo-artifacts"
LOGS="$ART/logs"
BIN="$ROOT/target/release/sp"
UI_PORT="${SP_SIM_UI_PORT:-19200}"
AGENTS=10
KEEP=0
[ "${1:-}" = "--keep" ] && KEEP=1

fail() { echo "SIM FAIL: $*" >&2; exit 1; }
pass() { echo "  ok: $*"; }
step() { echo; echo "==> $*"; }

for tool in git curl python3 ps; do
  command -v "$tool" >/dev/null 2>&1 || fail "required tool missing: $tool"
done

mkdir -p "$LOGS"

# ---------------------------------------------------------------- fixtures

# A polyrepo master folder: two microservices, one Liquibase, one Flyway.
seed_workspace() {
  local root="$1"
  rm -rf "$root"
  mkdir -p "$root/auth-service/src/main/resources/db" \
           "$root/auth-service/src/main/java/com/demo/auth" \
           "$root/payment-service/src/main/resources/db/migration" \
           "$root/payment-service/src/main/java/com/demo/payment" \
           "$root/scripts"

  cat >"$root/auth-service/src/main/resources/db/dbchangelog.xml" <<'XML'
<?xml version="1.0" encoding="UTF-8"?>
<databaseChangeLog xmlns="http://www.liquibase.org/xml/ns/dbchangelog">
  <changeSet id="100" author="platform">
    <createTable tableName="users">
      <column name="id" type="bigint"/>
    </createTable>
  </changeSet>
  <changeSet id="101" author="platform">
    <addColumn tableName="users"><column name="created_at" type="timestamp"/></addColumn>
  </changeSet>
</databaseChangeLog>
XML
  cat >"$root/auth-service/src/main/java/com/demo/auth/User.java" <<'JAVA'
package com.demo.auth;

public record User(String id, String email) {}
JAVA
  printf 'CREATE TABLE payments (id BIGSERIAL PRIMARY KEY);\n' \
    >"$root/payment-service/src/main/resources/db/migration/V100__create_payments.sql"
  cat >"$root/payment-service/src/main/java/com/demo/payment/PaymentService.java" <<'JAVA'
package com.demo.payment;

public class PaymentService {}
JAVA
  cat >"$root/.ai-context.md" <<'MD'
# Shared workspace context

| Service | Stack | Migrations |
|---|---|---|
| `auth-service` | Spring Boot + Liquibase | `src/main/resources/db/dbchangelog.xml` |
| `payment-service` | Spring Boot + Flyway | `src/main/resources/db/migration/V<n>__*.sql` |

Migrations are sequenced. Two agents must never claim the same `N`.
MD
  cp "$ROOT/scripts/validate-migrations.py" "$root/scripts/validate-migrations.py"

  for repo in auth-service payment-service; do
    git -C "$root/$repo" init -q -b main
    git -C "$root/$repo" config user.name "Demo Platform"
    git -C "$root/$repo" config user.email "platform@demo.example"
    git -C "$root/$repo" config commit.gpgsign false
    git -C "$root/$repo" add .
    git -C "$root/$repo" commit -q -m "baseline $repo"
  done
}

# Append a Liquibase changeSet. `where` is append|prepend so two agents editing
# the same file produce disjoint hunks that git merges without a conflict.
write_changeset() {
  local file="$1" author="$2" key="$3" where="${4:-append}"
  python3 - "$file" "$author" "$key" "$where" <<'PY'
from pathlib import Path
import sys
path, author, key, where = Path(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
text = path.read_text()
block = (
    f'  <changeSet id="{key}" author="{author}">\n'
    f'    <sql>select {key};</sql>\n'
    '  </changeSet>\n'
)
if where == "prepend":
    marker = '  <changeSet id="100"'
    text = text.replace(marker, block + marker, 1)
else:
    text = text.replace("</databaseChangeLog>", block + "</databaseChangeLog>", 1)
path.write_text(text)
PY
}

# ================================================================ CASE 1
# Ten agents, one shared working directory, plain git. No ScratchPost.

step "CASE 1 — ten agents, one shared working directory, plain git"
seed_workspace "$CTRL"
pass "control workspace seeded at $CTRL"

C1="$LOGS/90-control-10-agents.log"
: >"$C1"
log1() { echo "$*" | tee -a "$C1"; }

log1 "=== FAILURE 1: branch hijacking across 10 agents (shared .git/HEAD) ==="
for n in $(seq 1 "$AGENTS"); do
  git -C "$CTRL/auth-service" checkout -q -b "feat/agent-$n" 2>/dev/null || true
  log1 "agent_$n ran 'git checkout -b feat/agent-$n' -> shared HEAD is now $(git -C "$CTRL/auth-service" rev-parse --abbrev-ref HEAD)"
done
log1 "RESULT: 10 agents believe they own a branch; the repository has exactly one HEAD."
log1 "        Every agent after the first is committing onto someone else's branch."

log1 ""
log1 "=== FAILURE 2: silent Liquibase sequence collision (10 agents claim id=200) ==="
git -C "$CTRL/auth-service" checkout -q main
for n in $(seq 1 "$AGENTS"); do
  git -C "$CTRL/auth-service" checkout -q -b "seq/agent-$n" main
  if [ $((n % 2)) -eq 0 ]; then
    write_changeset "$CTRL/auth-service/src/main/resources/db/dbchangelog.xml" "agent_$n" 200 prepend
  else
    write_changeset "$CTRL/auth-service/src/main/resources/db/dbchangelog.xml" "agent_$n" 200 append
  fi
  git -C "$CTRL/auth-service" commit -q -am "agent_$n adds changeSet 200"
  log1 "agent_$n committed changeSet id=200 on seq/agent-$n"
done

git -C "$CTRL/auth-service" checkout -q main
MERGED=0
STUCK=0
for n in $(seq 1 "$AGENTS"); do
  set +e
  OUT="$(git -C "$CTRL/auth-service" merge --no-edit "seq/agent-$n" 2>&1)"
  CODE=$?
  set -e
  case "$CODE" in
    0) MERGED=$((MERGED + 1)) ;;
    *) STUCK=$((STUCK + 1)) ;;
  esac
  # Run the real CI check after every merge, exactly as a pipeline would.
  set +e
  VOUT="$(python3 "$CTRL/scripts/validate-migrations.py" 2>&1)"
  VCODE=$?
  set -e
  log1 "git merge seq/agent-$n exit=$CODE  |  migration validator exit=$VCODE"
  if [ "$CODE" -ne 0 ]; then
    log1 "    $(echo "$OUT" | head -1)"
  fi
  if [ "$VCODE" -ne 0 ] && [ "$MERGED" -le 2 ]; then
    echo "$VOUT" | sed 's/^/    /' | tee -a "$C1"
  fi
  # A textual conflict leaves the tree unmerged; abort so the next agent can try.
  if [ "$CODE" -ne 0 ]; then
    git -C "$CTRL/auth-service" merge --abort 2>/dev/null || true
  fi
done
DUPES="$(grep -c 'changeSet id="200"' "$CTRL/auth-service/src/main/resources/db/dbchangelog.xml" || true)"
log1 ""
log1 "merges that reported success: $MERGED/$AGENTS"
log1 "merges that hit a textual conflict: $STUCK/$AGENTS"
log1 "duplicate changeSet id=200 occurrences on main: $DUPES"
log1 "RESULT: git merged $MERGED branches with exit 0 and zero warnings, and main was"
log1 "        already corrupt after the second one. The remaining $STUCK agents then hit a"
log1 "        textual conflict and are stuck: their work cannot land at all."
log1 "        Nothing in the git output ever mentioned a sequence number."

log1 ""
log1 "=== FAILURE 3: port collision across 10 concurrent test runs ==="
python3 - <<'PY' | tee -a "$C1"
import socket
held = []
try:
    for n in range(1, 11):
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            sock.bind(("127.0.0.1", 8080))
            sock.listen(1)
            held.append(sock)
            print(f"agent_{n} bound 127.0.0.1:8080 (test server)")
        except OSError as err:
            print(f"agent_{n} bind 127.0.0.1:8080 failed: {err.strerror} (EADDRINUSE)")
            sock.close()
finally:
    for sock in held:
        sock.close()
PY
log1 "RESULT: 1 of 10 agents could start its test server; 9 crashed on EADDRINUSE."

# ================================================================ CASE 2
# The same ten agents, coordinated by ScratchPost.

step "CASE 2 — the same ten agents under ScratchPost"
seed_workspace "$SIM"
pass "simulation workspace seeded at $SIM"

step "sp init"
SP_MASTER_ROOT="$SIM" "$BIN" init >"$LOGS/91-sim-init.log" 2>&1
grep -q "discovered 2 repository(ies)" "$LOGS/91-sim-init.log" || fail "expected 2 discovered repositories"
pass "2 microservice repositories discovered and hooked"

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

step "starting scratchpostd"
# `sp init` provisions a daemon with its stderr detached. Restart it under our
# own redirection so the run leaves a complete daemon log behind.
if SP_MASTER_ROOT="$SIM" "$BIN" status --json >/dev/null 2>&1; then
  INIT_PID="$(cat "$SIM/.scratchpost/run/daemon.pid" 2>/dev/null || true)"
  if [ -n "$INIT_PID" ]; then
    kill "$INIT_PID" 2>/dev/null || true
    for _ in $(seq 1 100); do
      kill -0 "$INIT_PID" 2>/dev/null || break
      sleep 0.05
    done
  fi
fi
SP_MASTER_ROOT="$SIM" "$BIN" daemon >"$LOGS/92-sim-daemon.log" 2>&1 &
for _ in $(seq 1 200); do
  SP_MASTER_ROOT="$SIM" "$BIN" status --json >/dev/null 2>&1 && break
  sleep 0.05
done
SP_MASTER_ROOT="$SIM" "$BIN" status --json >/dev/null 2>&1 || fail "daemon did not become ready"
DAEMON_PID="$(cat "$SIM/.scratchpost/run/daemon.pid")"
pass "daemon ready (pid $DAEMON_PID)"

sp() { SP_MASTER_ROOT="$SIM" "$BIN" "$@"; }

step "attaching $AGENTS agent sessions"
for n in $(seq 1 "$AGENTS"); do
  sp run --agent "agent_$n" -- /usr/bin/true >"$LOGS/93-session-agent_$n.log" 2>&1
done
sp status --json >"$LOGS/94-sessions.json"
python3 - "$LOGS/94-sessions.json" "$AGENTS" <<'PY'
import json, sys
state = json.load(open(sys.argv[1]))
expected = int(sys.argv[2])
sessions = {s["agent"]: s for s in state["sessions"]}
assert len(sessions) == expected, sorted(sessions)
for field in ("workspace", "port_base", "slot"):
    values = {s[field] for s in sessions.values()}
    assert len(values) == expected, (field, values)
for agent, session in sorted(sessions.items(), key=lambda kv: kv[1]["slot"]):
    print(f"  {agent}: slot={session['slot']} SP_PORT_BASE={session['port_base']} workspace={session['workspace']}")
PY
pass "$AGENTS isolated worktrees, $AGENTS disjoint port bands, $AGENTS distinct slots"

step "verifying copy-on-write context isolation"
A1WS="$SIM/.scratchpost/sessions/agent_1/workspace"
printf '\n- agent_1 local note: must not leak to master\n' >>"$A1WS/.ai-context.md"
grep -q "agent_1 local note" "$SIM/.ai-context.md" && fail "agent_1 context write leaked into canonical master"
pass "agent_1 context mutation did not corrupt canonical master"

commit_as() {
  local agent="$1" repo="$2" message="$3"
  sp run --agent "$agent" -- /bin/sh -c "cd $repo && git commit -m '$message'"
}

step "scenario 1: ten agents race for Liquibase changeSet 200"
C2="$LOGS/95-scratchpost-10-agents.log"
: >"$C2"
log2() { echo "$*" | tee -a "$C2"; }

log2 "=== SEQUENCE GATING: 10 agents race for changeSet id=200 ==="
for n in $(seq 1 "$AGENTS"); do
  WS="$SIM/.scratchpost/sessions/agent_$n/workspace"
  write_changeset "$WS/auth-service/src/main/resources/db/dbchangelog.xml" "agent_$n" 200
  sp run --agent "agent_$n" -- /usr/bin/git -C auth-service add src/main/resources/db/dbchangelog.xml >/dev/null
done
pass "all $AGENTS agents staged changeSet id=200"

# agent_1 wins the race and locks 200 in the seqlock ledger.
set +e
commit_as agent_1 auth-service "agent_1 adds changeSet 200" \
  >"$LOGS/96-agent_1-commit.out" 2>"$LOGS/96-agent_1-commit.err"
A1_CODE=$?
set -e
[ "$A1_CODE" -eq 0 ] || { cat "$LOGS/96-agent_1-commit.err" >&2; fail "agent_1 commit exited $A1_CODE"; }
log2 "agent_1 committed changeSet id=200 (exit 0) — 200 is now locked in the ledger"
pass "agent_1 claimed 200 (exit 0)"

# Agents 2..10 must all be blocked deterministically.
BLOCKED=0
for n in $(seq 2 "$AGENTS"); do
  set +e
  commit_as "agent_$n" auth-service "agent_$n adds changeSet 200" \
    >"$LOGS/97-agent_${n}-blocked.out" 2>"$LOGS/97-agent_${n}-blocked.err"
  CODE=$?
  set -e
  [ "$CODE" -eq 1 ] || fail "agent_$n conflicting commit exited $CODE, expected 1"
  grep -Fq 'SP_BLOCKED v1' "$LOGS/97-agent_${n}-blocked.err" || fail "agent_$n missing SP_BLOCKED"
  grep -Fq 'SP_SEQUENCE_COLLISION' "$LOGS/97-agent_${n}-blocked.err" || fail "agent_$n missing SP_SEQUENCE_COLLISION"
  grep -Fq 'SP_CONFLICT_JSON=' "$LOGS/97-agent_${n}-blocked.err" || fail "agent_$n missing SP_CONFLICT_JSON"
  BLOCKED=$((BLOCKED + 1))
  log2 "agent_$n blocked: SP_SEQUENCE_COLLISION on id=200 (exit 1)"
done
[ "$BLOCKED" -eq $((AGENTS - 1)) ] || fail "expected $((AGENTS - 1)) blocked agents, got $BLOCKED"
pass "$BLOCKED agents blocked deterministically with the SP_BLOCKED contract"

# Each blocked agent self-corrects to the next free key and retries.
log2 ""
log2 "=== SELF-CORRECTION: agents 2..10 resequence and retry ==="
for n in $(seq 2 "$AGENTS"); do
  NEXT="$(sp sequence next --namespace auth-service:src/main/resources/db/dbchangelog.xml --after 200 2>/dev/null | tail -1)"
  sp resequence --agent "agent_$n" --file auth-service/src/main/resources/db/dbchangelog.xml \
    --from 200 --to "$NEXT" >"$LOGS/98-agent_${n}-resequence.out" 2>&1
  set +e
  commit_as "agent_$n" auth-service "agent_$n adds changeSet $NEXT" \
    >"$LOGS/99-agent_${n}-retry.out" 2>"$LOGS/99-agent_${n}-retry.err"
  CODE=$?
  set -e
  [ "$CODE" -eq 0 ] || { cat "$LOGS/99-agent_${n}-retry.err" >&2; fail "agent_$n retry exited $CODE"; }
  log2 "agent_$n resequenced 200 -> $NEXT and committed (exit 0)"
done
pass "all $AGENTS agents committed with unique Liquibase ids"

step "scenario 2: ten agents race for Flyway V101"
log2 ""
log2 "=== FLYWAY GATING: 10 agents race for V101 ==="
for n in $(seq 1 "$AGENTS"); do
  WS="$SIM/.scratchpost/sessions/agent_$n/workspace"
  printf 'ALTER TABLE payments ADD COLUMN c%s int;\n' "$n" \
    >"$WS/payment-service/src/main/resources/db/migration/V101__agent_${n}.sql"
  sp run --agent "agent_$n" -- /usr/bin/git -C payment-service add \
    "src/main/resources/db/migration/V101__agent_${n}.sql" >/dev/null
done

set +e
commit_as agent_1 payment-service "agent_1 adds V101" \
  >"$LOGS/9A-agent_1-flyway.out" 2>"$LOGS/9A-agent_1-flyway.err"
CODE=$?
set -e
[ "$CODE" -eq 0 ] || { cat "$LOGS/9A-agent_1-flyway.err" >&2; fail "agent_1 flyway commit exited $CODE"; }
log2 "agent_1 committed Flyway V101 (exit 0)"

FLY_BLOCKED=0
for n in $(seq 2 "$AGENTS"); do
  set +e
  commit_as "agent_$n" payment-service "agent_$n adds V101" \
    >"$LOGS/9B-agent_${n}-flyway.out" 2>"$LOGS/9B-agent_${n}-flyway.err"
  CODE=$?
  set -e
  [ "$CODE" -eq 1 ] || fail "agent_$n flyway commit exited $CODE, expected 1"
  grep -Fq 'SP_SEQUENCE_COLLISION' "$LOGS/9B-agent_${n}-flyway.err" || fail "agent_$n missing SP_SEQUENCE_COLLISION"
  FLY_BLOCKED=$((FLY_BLOCKED + 1))
  NEXT="$(sp sequence next --namespace payment-service:src/main/resources/db/migration --after 101 2>/dev/null | tail -1)"
  sp run --agent "agent_$n" -- /usr/bin/git -C payment-service mv \
    "src/main/resources/db/migration/V101__agent_${n}.sql" \
    "src/main/resources/db/migration/V${NEXT}__agent_${n}.sql" >/dev/null
  set +e
  commit_as "agent_$n" payment-service "agent_$n adds V$NEXT" \
    >"$LOGS/9C-agent_${n}-flyway-retry.out" 2>"$LOGS/9C-agent_${n}-flyway-retry.err"
  CODE=$?
  set -e
  [ "$CODE" -eq 0 ] || { cat "$LOGS/9C-agent_${n}-flyway-retry.err" >&2; fail "agent_$n flyway retry exited $CODE"; }
  log2 "agent_$n blocked on V101, renamed to V$NEXT, committed (exit 0)"
done
[ "$FLY_BLOCKED" -eq $((AGENTS - 1)) ] || fail "expected $((AGENTS - 1)) flyway blocks, got $FLY_BLOCKED"
pass "$FLY_BLOCKED agents blocked on V101 and self-corrected"

step "scenario 3: path claims across ten agents"
log2 ""
log2 "=== PATH CLAIMS: advisory + mandatory claims on a shared file ==="
sp claim --agent agent_1 --repo auth-service \
  --path src/main/java/com/demo/auth/User.java --strength mandatory --note "agent_1 owns User.java" \
  >"$LOGS/9D-claim-agent_1.out" 2>&1
sp claim --agent agent_2 --repo auth-service \
  --path src/main/java/com/demo/auth/User.java --strength advisory --note "agent_2 reads User.java" \
  >"$LOGS/9D-claim-agent_2.out" 2>&1
sp status --json >"$LOGS/9E-claims.json"
python3 - "$LOGS/9E-claims.json" <<'PY'
import json, sys
state = json.load(open(sys.argv[1]))
claims = state["path_claims"]
assert len(claims) == 2, claims
strengths = {c["agent"]: c["strength"] for c in claims}
assert strengths == {"agent_1": "mandatory", "agent_2": "advisory"}, strengths
print("  ok: agent_1 holds a mandatory claim, agent_2 an advisory claim on the same path")
PY
log2 "agent_1 holds a mandatory claim on User.java; agent_2 an advisory claim"
pass "path claims registered and visible to every agent"

step "scenario 4: adversarial hook bypass and stale lease recovery"
REAL_GIT="$(command -v git)"
WS2="$SIM/.scratchpost/sessions/agent_2/workspace"
write_changeset "$WS2/auth-service/src/main/resources/db/dbchangelog.xml" agent_2 500
sp run --agent agent_2 -- /usr/bin/git -C auth-service add src/main/resources/db/dbchangelog.xml >/dev/null
set +e
sp run --agent agent_2 -- "$REAL_GIT" -C auth-service commit -m "bypass attempt" \
  >"$LOGS/9F-bypass.out" 2>"$LOGS/9F-bypass.err"
BYPASS_CODE=$?
set -e
[ "$BYPASS_CODE" -eq 1 ] || fail "direct $REAL_GIT commit exited $BYPASS_CODE, expected 1"
grep -Fq 'commits must originate from the managed ScratchPost git shim' "$LOGS/9F-bypass.err" \
  || fail "hook bypass rejection message missing"
log2 "direct $REAL_GIT commit rejected by managed core.hooksPath (exit 1)"
pass "managed core.hooksPath blocked a direct git commit"

sp run --agent agent_11 -- /usr/bin/true >"$LOGS/9G-agent_11.log" 2>&1
sp status --json >"$LOGS/9H-before-evict.json"
sp evict --agent agent_11 >"$LOGS/9I-evict.out" 2>&1
sp status --json >"$LOGS/9J-after-evict.json"
python3 - "$LOGS/9H-before-evict.json" "$LOGS/9J-after-evict.json" <<'PY'
import json, sys
before = {s["agent"]: s["status"] for s in json.load(open(sys.argv[1]))["sessions"]}
after = {s["agent"]: s["status"] for s in json.load(open(sys.argv[2]))["sessions"]}
assert before.get("agent_11") == "active", before
assert after.get("agent_11") == "abandoned", after
print("  ok: stalled agent_11 lease revoked without blocking peers")
PY
pass "abandoned session recovery verified"

step "scenario 5: barrier contention for the mergeability radar"
log2 ""
log2 "=== BARRIER CONTENTION: three agents converge on auth-service ==="
sp config --mode barrier-stacked --barrier-scope component >"$LOGS/9L-mode.log" 2>&1
for n in 1 2 3; do
  WS="$SIM/.scratchpost/sessions/agent_$n/workspace"
  write_changeset "$WS/auth-service/src/main/resources/db/dbchangelog.xml" "agent_$n" "$((600 + n))"
  sp run --agent "agent_$n" -- /usr/bin/git -C auth-service add \
    src/main/resources/db/dbchangelog.xml >/dev/null
done
set +e
commit_as agent_1 auth-service "agent_1 barrier contention" \
  >"$LOGS/9M-agent_1-barrier.out" 2>"$LOGS/9M-agent_1-barrier.err"
CODE=$?
set -e
[ "$CODE" -eq 0 ] || { cat "$LOGS/9M-agent_1-barrier.err" >&2; fail "agent_1 barrier commit exited $CODE"; }
for n in 2 3; do
  set +e
  commit_as "agent_$n" auth-service "agent_$n barrier contention" \
    >"$LOGS/9M-agent_${n}-barrier.out" 2>"$LOGS/9M-agent_${n}-barrier.err"
  CODE=$?
  set -e
  [ "$CODE" -eq 1 ] || fail "agent_$n barrier commit exited $CODE, expected 1"
  grep -Fq 'SP_BARRIER_WAIT' "$LOGS/9M-agent_${n}-barrier.err" || fail "agent_$n missing SP_BARRIER_WAIT"
  log2 "agent_$n parked at the auth-service component barrier (SP_BARRIER_WAIT)"
done
sp status --json >"$LOGS/10-agents-status.json"
python3 - "$LOGS/10-agents-status.json" <<'RADAR'
import json, sys
state = json.load(open(sys.argv[1]))
components = state["components"]
assert components, "expected a conflict component for the radar"
agents = sorted(components[0]["agents"])
assert agents == ["agent_1", "agent_2", "agent_3"], agents
print(f"  ok: mergeability radar shows component 0 = {' <-> '.join(agents)}")
RADAR
pass "three agents share one conflict component; radar is populated"
sp barrier release >"$LOGS/9N-barrier-release.json" 2>&1
sp config --mode fcfs >/dev/null
log2 "operator released the barrier; all three agents proceed"

step "validating the merged tree"
set +e
VALIDATE_OUT="$(python3 "$SIM/scripts/validate-migrations.py" 2>&1)"
VALIDATE_CODE=$?
set -e
echo "$VALIDATE_OUT" | sed 's/^/  /' | tee -a "$C2"
[ "$VALIDATE_CODE" -eq 0 ] || fail "migration validator failed on the ScratchPost workspace"
log2 "post-merge migration validator exit=0 — main is valid"
pass "migration validator passes: every sequence number is unique"

step "capturing final ledger state"
sp status --json >"$LOGS/10-agents-final-status.json"
sp claims --json >"$LOGS/10-agents-claims.json"
sp notices --json >"$LOGS/10-agents-notices.json"
python3 - "$LOGS/10-agents-claims.json" "$AGENTS" <<'PY'
import json, sys
expected = int(sys.argv[2])
claims = json.load(open(sys.argv[1]))
committed = [c for c in claims if c["state"] == "committed"]
assert len(committed) >= expected, (len(committed), expected)
keys = sorted({c["key"] for c in committed if c["namespace"].startswith("auth-service")}, key=int)
assert len(keys) == len(set(keys)), keys
print(f"  ok: {len(committed)} committed sequence claims, auth-service keys {keys}")
PY
pass "sequence registry captured"

step "measuring daemon memory"
sleep 0.3
RSS_KB="$(ps -o rss= -p "$DAEMON_PID" | tr -d ' ')"
echo "$RSS_KB" >"$LOGS/9K-daemon-rss-kb.txt"
[ "$RSS_KB" -lt 25600 ] || fail "daemon RSS ${RSS_KB} KB exceeds 25600 KB"
pass "daemon RSS ${RSS_KB} KB < 25600 KB"

echo
echo "SIM PASS: 10-agent parallel stress simulation complete."
echo "daemon_pid=$DAEMON_PID master=$SIM ui_port=$UI_PORT"
if [ "$KEEP" -eq 1 ]; then
  echo "workspace kept for UI capture: SP_MASTER_ROOT=$SIM $BIN ui"
else
  echo "workspace retained at $SIM (daemon pid $DAEMON_PID)"
fi
