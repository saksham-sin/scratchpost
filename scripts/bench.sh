#!/usr/bin/env bash
# Empirical benchmarks: mmap read latency, cold start, commit-gate overhead, peak RSS.
# Requires a running simulation workspace (scripts/enterprise-sim.sh leaves one behind).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

SIM="${SP_SIM_ROOT:-/tmp/sp-enterprise-sim}"
LOGS="$ROOT/demo-artifacts/logs"
BIN="$ROOT/target/release/sp"
AGENT="${SP_BENCH_AGENT:-agent_1}"

fail() { echo "BENCH FAIL: $*" >&2; exit 1; }
pass() { echo "  ok: $*"; }

[ -x "$BIN" ] || fail "release binary missing; run cargo build --release"
[ -d "$SIM/.scratchpost" ] || fail "no simulation workspace at $SIM; run scripts/enterprise-sim.sh first"
mkdir -p "$LOGS"

export SP_MASTER_ROOT="$SIM"
DAEMON_PID="$(cat "$SIM/.scratchpost/run/daemon.pid" 2>/dev/null || true)"
[ -n "$DAEMON_PID" ] && kill -0 "$DAEMON_PID" 2>/dev/null || fail "scratchpostd is not running for $SIM"

echo "==> re-attaching $AGENT (leases expire after 300s)"
"$BIN" run --agent "$AGENT" -- /usr/bin/true >/dev/null 2>&1 || fail "cannot attach $AGENT"

echo "==> measuring mmap read latency and cold start"
python3 - "$BIN" "$LOGS/71-latency-metrics.json" <<'PY'
import json, statistics, subprocess, sys, time

bin_path, out = sys.argv[1], sys.argv[2]

def bench(cmd, n=30):
    samples = []
    for _ in range(n):
        start = time.perf_counter()
        subprocess.run(cmd, capture_output=True, check=False)
        samples.append((time.perf_counter() - start) * 1000.0)
    samples.sort()
    return {
        "n": n,
        "p50_ms": round(statistics.median(samples), 2),
        "p99_ms": round(samples[min(len(samples) - 1, int(len(samples) * 0.99))], 2),
        "min_ms": round(samples[0], 2),
        "max_ms": round(samples[-1], 2),
    }

result = {
    "status_mmap_read": bench([bin_path, "status", "--json"]),
    "claims_read": bench([bin_path, "claims", "--json"]),
    "version_cold_start": bench([bin_path, "--version"]),
}
json.dump(result, open(out, "w"), indent=2)
for name, stats in result.items():
    print(f"  {name}: p50={stats['p50_ms']}ms p99={stats['p99_ms']}ms")
PY
pass "latency metrics written"

echo "==> measuring commit-gate overhead"
python3 - "$BIN" "$SIM" "$AGENT" "$LOGS/72-commit-gate-latency.json" <<'PY'
import json, os, shutil, statistics, subprocess, sys, tempfile, time

bin_path, sim, agent, out = sys.argv[1:5]
session_root = os.path.join(sim, ".scratchpost", "sessions", agent, "workspace")
repo = os.path.join(session_root, "auth-service")
shim_dir = os.path.join(sim, ".scratchpost", "bin")

env = dict(os.environ)
env.update({
    "SP_MASTER_ROOT": sim,
    "SP_AGENT": agent,
    "SP_SESSION_ROOT": session_root,
    "PATH": shim_dir + os.pathsep + env.get("PATH", ""),
})

def timed(cmd, cwd, e):
    start = time.perf_counter()
    proc = subprocess.run(cmd, cwd=cwd, env=e, capture_output=True, text=True)
    return (time.perf_counter() - start) * 1000.0, proc

def summarize(samples):
    samples.sort()
    return {
        "n": len(samples),
        "p50_ms": round(statistics.median(samples), 2),
        "p99_ms": round(samples[min(len(samples) - 1, int(len(samples) * 0.99))], 2),
        "min_ms": round(samples[0], 2),
        "max_ms": round(samples[-1], 2),
    }

gated, plain = [], []
for i in range(15):
    target = os.path.join(repo, "src", "main", "resources", "db", "dbchangelog.xml")
    with open(target, "a") as handle:
        handle.write(f"<!-- bench {i} -->\n")
    subprocess.run(["git", "add", "-A"], cwd=repo, env=env, capture_output=True)
    elapsed, proc = timed(["git", "commit", "-m", f"bench gated {i}"], repo, env)
    if proc.returncode != 0:
        raise SystemExit(f"gated commit failed: {proc.stderr[-400:]}")
    gated.append(elapsed)

# The plain baseline must run outside a ScratchPost session, otherwise the managed
# pre-commit hook rejects it. Use a throwaway repository with no SP_* environment.
plain_repo = os.path.join(tempfile.mkdtemp(prefix="sp-bench-plain-"), "repo")
os.makedirs(plain_repo)
plain_env = {k: v for k, v in os.environ.items() if not k.startswith("SP_")}
subprocess.run(["git", "init", "-q", "-b", "main"], cwd=plain_repo, env=plain_env, check=True)
subprocess.run(["git", "config", "user.email", "bench@example.com"], cwd=plain_repo, env=plain_env)
subprocess.run(["git", "config", "user.name", "bench"], cwd=plain_repo, env=plain_env)
plain_file = os.path.join(plain_repo, "dbchangelog.xml")
for i in range(15):
    with open(plain_file, "a") as handle:
        handle.write(f"<!-- bench plain {i} -->\n")
    subprocess.run(["git", "add", "-A"], cwd=plain_repo, env=plain_env, capture_output=True)
    elapsed, proc = timed(["/usr/bin/git", "commit", "-m", f"bench plain {i}"], plain_repo, plain_env)
    if proc.returncode != 0:
        raise SystemExit(f"plain commit failed: {proc.stderr[-400:]}")
    plain.append(elapsed)
shutil.rmtree(os.path.dirname(plain_repo), ignore_errors=True)

result = {
    "gated_commit": summarize(gated),
    "plain_git_commit": summarize(plain),
    "gate_overhead_p50_ms": round(statistics.median(gated) - statistics.median(plain), 2),
}
json.dump(result, open(out, "w"), indent=2)
print(f"  gated p50={result['gated_commit']['p50_ms']}ms "
      f"plain p50={result['plain_git_commit']['p50_ms']}ms "
      f"overhead={result['gate_overhead_p50_ms']}ms")
PY
pass "commit-gate latency written"

echo "==> measuring peak daemon RSS under concurrent load"
python3 - "$BIN" "$SIM" "$DAEMON_PID" "$LOGS/73-peak-rss-kb.txt" <<'PY'
import json, os, subprocess, sys, threading, time

bin_path, sim, daemon_pid, out = sys.argv[1:5]
peak = [0]
stop = threading.Event()

def sample():
    while not stop.is_set():
        try:
            rss = int(subprocess.run(["ps", "-o", "rss=", "-p", daemon_pid],
                                      capture_output=True, text=True).stdout.strip() or 0)
            peak[0] = max(peak[0], rss)
        except ValueError:
            pass
        time.sleep(0.02)

def hammer():
    for _ in range(40):
        subprocess.run([bin_path, "status", "--json"], capture_output=True)
        subprocess.run([bin_path, "claims", "--json"], capture_output=True)
        subprocess.run([bin_path, "notices", "--json"], capture_output=True)

watcher = threading.Thread(target=sample, daemon=True)
watcher.start()
workers = [threading.Thread(target=hammer) for _ in range(6)]
for worker in workers:
    worker.start()
for worker in workers:
    worker.join()
stop.set()
watcher.join(timeout=1)

open(out, "w").write(str(peak[0]))
print(f"  peak daemon RSS {peak[0]} KB")
PY
pass "peak RSS written"

echo
echo "BENCH PASS: metrics written to $LOGS"
