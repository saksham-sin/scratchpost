#!/usr/bin/env bash
# Control run: what happens when 5 agents share one folder with plain git.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CTRL="${SP_CONTROL_ROOT:-/tmp/sp-chaos-control}"
LOGS="$ROOT/demo-artifacts/logs"

rm -rf "$CTRL"
mkdir -p "$CTRL/auth-service/src/main/resources/db" "$CTRL/billing-service/db/migration"
cat >"$CTRL/auth-service/src/main/resources/db/dbchangelog.xml" <<'XML'
<?xml version="1.0" encoding="UTF-8"?>
<databaseChangeLog xmlns="http://www.liquibase.org/xml/ns/dbchangelog">
  <changeSet id="199" author="core">
    <createTable tableName="users"><column name="id" type="BIGINT"/></createTable>
  </changeSet>
</databaseChangeLog>
XML
printf 'CREATE TABLE invoices (id BIGSERIAL PRIMARY KEY);\n' >"$CTRL/billing-service/db/migration/V41__baseline.sql"

for repo in auth-service billing-service; do
  git -C "$CTRL/$repo" init -q -b main
  git -C "$CTRL/$repo" config user.name "Control"
  git -C "$CTRL/$repo" config user.email "control@example"
  git -C "$CTRL/$repo" config commit.gpgsign false
  git -C "$CTRL/$repo" add .
  git -C "$CTRL/$repo" commit -q -m "baseline $repo"
done

echo "=== CONTROL 1: branch hijacking (shared .git/HEAD) ===" | tee "$LOGS/80-control-branch-hijack.log"
git -C "$CTRL/auth-service" checkout -q -b feat/oauth
echo "agent_1 created feat/oauth; HEAD -> $(git -C "$CTRL/auth-service" rev-parse --abbrev-ref HEAD)" | tee -a "$LOGS/80-control-branch-hijack.log"
git -C "$CTRL/auth-service" checkout -q -b feat/mfa
echo "agent_2 created feat/mfa; HEAD -> $(git -C "$CTRL/auth-service" rev-parse --abbrev-ref HEAD)" | tee -a "$LOGS/80-control-branch-hijack.log"
echo "agent_1 now believes it is on feat/oauth but the shared HEAD says: $(git -C "$CTRL/auth-service" rev-parse --abbrev-ref HEAD)" | tee -a "$LOGS/80-control-branch-hijack.log"

echo "=== CONTROL 2: silent Liquibase sequence collision ===" | tee "$LOGS/81-control-sequence-collision.log"
# Both agents add changeSet id=200 but at different offsets in the file, so Git
# sees two disjoint hunks and merges cleanly with zero warnings.
git -C "$CTRL/auth-service" checkout -q main
git -C "$CTRL/auth-service" checkout -q -b feat/agent-1
python3 - "$CTRL/auth-service/src/main/resources/db/dbchangelog.xml" agent_1 append <<'PY'
from pathlib import Path
import sys
path, author, where = Path(sys.argv[1]), sys.argv[2], sys.argv[3]
text = path.read_text()
block = f'  <changeSet id="200" author="{author}">\n    <sql>select 200;</sql>\n  </changeSet>\n'
path.write_text(text.replace("</databaseChangeLog>", block + "</databaseChangeLog>", 1))
PY
git -C "$CTRL/auth-service" commit -q -am "agent_1 changeSet 200"

git -C "$CTRL/auth-service" checkout -q main
git -C "$CTRL/auth-service" checkout -q -b feat/agent-2
python3 - "$CTRL/auth-service/src/main/resources/db/dbchangelog.xml" agent_2 prepend <<'PY'
from pathlib import Path
import sys
path, author, where = Path(sys.argv[1]), sys.argv[2], sys.argv[3]
text = path.read_text()
block = f'  <changeSet id="200" author="{author}">\n    <sql>select 200;</sql>\n  </changeSet>\n'
marker = '  <changeSet id="199"'
path.write_text(text.replace(marker, block + marker, 1))
PY
git -C "$CTRL/auth-service" commit -q -am "agent_2 changeSet 200"

set +e
MERGE_OUT="$(git -C "$CTRL/auth-service" merge --no-edit feat/agent-1 2>&1)"
MERGE_CODE=$?
set -e
echo "git merge feat/agent-1 exit=$MERGE_CODE" | tee -a "$LOGS/81-control-sequence-collision.log"
echo "$MERGE_OUT" | sed 's/^/  /' | tee -a "$LOGS/81-control-sequence-collision.log"
DUPLICATES="$(grep -c 'changeSet id="200"' "$CTRL/auth-service/src/main/resources/db/dbchangelog.xml")"
echo "duplicate changeSet id=200 occurrences after merge: $DUPLICATES" | tee -a "$LOGS/81-control-sequence-collision.log"
if [ "$MERGE_CODE" -eq 0 ] && [ "$DUPLICATES" -eq 2 ]; then
  echo "RESULT: merge reported ZERO conflicts while the Liquibase changelog now has two id=200 changeSets." | tee -a "$LOGS/81-control-sequence-collision.log"
  echo "        Liquibase will fail at deploy time (or silently skip one migration)." | tee -a "$LOGS/81-control-sequence-collision.log"
else
  echo "RESULT: merge exit=$MERGE_CODE duplicates=$DUPLICATES" | tee -a "$LOGS/81-control-sequence-collision.log"
fi

echo "=== CONTROL 3: port collision on concurrent test runs ===" | tee "$LOGS/82-control-port-collision.log"
python3 - <<'PY' | tee -a "$LOGS/82-control-port-collision.log"
import socket, sys
socks = []
try:
    first = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    first.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    first.bind(("127.0.0.1", 8080))
    first.listen(1)
    socks.append(first)
    print("agent_1 bound 127.0.0.1:8080 (test server)")
    second = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        second.bind(("127.0.0.1", 8080))
        print("agent_2 also bound 8080 (unexpected)")
    except OSError as err:
        print(f"agent_2 bind 127.0.0.1:8080 failed: {err.strerror} (EADDRINUSE)")
finally:
    for sock in socks:
        sock.close()
PY

echo "CONTROL RUN COMPLETE: 3 failure modes reproduced without ScratchPost."
