# ScratchPost RFC Review — Principal Architect Response

*Review of: "Architectural RFC & Co-Design Task: `ScratchPost` (`sp` CLI / `scratchpostd` Daemon)" — 2026-11-15*

---

## 1. Brutal Critique & Top 4 Failure Modes

**The single most important correction: the `$PATH` shim is not an enforcement point. Git hooks are.** Agents routinely bypass `PATH` (absolute `/usr/bin/git`, `gh` CLI, editor-integrated git, tools that sanitize env). A determined-or-ignorant agent voids every guarantee. Flip the design: install managed `pre-commit`/`pre-push` hooks (via `core.hooksPath` pointing at `.scratchpost/hooks/`, which you control) as the *hard* choke point, and keep the shim as *convenience* (cwd bootstrap, env injection, nicer output). The hook talks to the daemon; even `--no-verify` on commit is caught at `pre-push`, and the daemon also verifies on `sp push`/PR-open. Design every invariant so the last line of defense is a hook, never `PATH`.

### Failure Mode 1 — Path identity split-brain (the topology is wrong as written)

Running agents "in" `/master/auth-service` while their real worktree is `.scratchpost/sandboxes/agent_1/auth-service` does not work with any of the three proposed mechanisms:

- **Symlinks**: `getcwd()` returns the physical path; LSPs, Java/Maven path resolution, and most agent file tools canonicalize. You get two logical views of one byte-store — the exact collision you're trying to kill, now invisible.
- **`GIT_WORK_TREE`/`GIT_INDEX_FILE` redirection**: fixes git only. The collision is in **file bytes**, not git metadata — two processes editing the same inode in the master folder still clobber each other. Env vars don't virtualize writes.
- **FUSE/virtual FS**: macFUSE requires a kernel extension approval flow no enterprise fleet will accept; watchers (IDE, `fswatch`), `spawn_blocking` stat semantics, and inode identity all leak. Rejected.

**Fix: make the sandbox the literal cwd.** `sp shell` execs a login shell with `cwd = .scratchpost/sandboxes/<agent>/<repo>` and exports `SP_REPO_ROOT`, `SP_LOGICAL_PATH`. The MCP server exposes a `logical_path`/`real_path` pair on every tool result so agents never guess. For IDE users, `sp open --agent` emits the correct `vscode://file/...sandbox...` URI. Additionally: make "editing in the wrong place" *detectable* — a daemon watchdog detects writes to the master-folder worktrees and flags the offending session.

Also fix the sandbox primitive: use **`git worktree`** (shared object DB, per-worktree HEAD/index — exactly the isolation wanted, near-zero disk) and reserve CoW (`clonefile`/`FICLONE`) for *non-git payloads*: `node_modules`, `target/`, gradle caches. Hardlinking `.ai-context.md` is a trap: the first agent edit either diverges silently or corrupts the shared file. Use **app-level COW**: hardlink + `chmod 0444`; write attempt fails → agent runs `sp fork .ai-context.md` → daemon `clonefile`s a private copy and registers a write-intent. Enforcement via the failure itself, zero polling.

### Failure Mode 2 — Barrier liveness (LLM agents are non-cooperative processes)

Agent 3 is not a well-behaved participant; it hits context limits, gets killed, or the user closes the tab. "All active agents reach barrier" is an unbounded wait on a set you can't even define ("active" by what predicate?). Design:

- **Explicit cohorts**: `sp cohort join feat-x --agents 1,2,3`. Barrier is over cohort members only, never "everyone with a lease."
- **Lease + TTL**: each session holds a 30s lease renewed by hook invocations / MCP pings. No heartbeat ⇒ `SUSPECT` ⇒ `ABANDONED`.
- **Fallback policy is config, per cohort**: `CLOSE_WITHOUT_ABSENTEES` (barrier fires over live members, abandoned members' sandboxes frozen), `CANCEL`, or `ESCALATE` (surface to human). Never `WAIT_FOREVER`.
- **Crash consistency**: the ledger is single-writer daemon state backed by an append-only WAL; on daemon restart, `epoch` increments, every shim/MCP client sees the epoch change and re-registers. A barrier that survives daemon restart is a *deterministic function of the WAL*, not in-memory state.

### Failure Mode 3 — Restack cascade & CI amplification in stacked mode

If PR #1 gets review feedback (i.e., its commits are *amended*, not appended), every downstream patch must be replayed. Naive auto-restack creates: conflict ping-pong between auto-rebase and human review, orphaned merge commits, and N× CI runs per review round. Design:

- Restack is **idempotent and patch-id-aware**: skip patches whose `git patch-id` already exists upstream (same trick `git rerere`/stacked-tools use); track `upstream_commit → replayed_commit` mappings in the ledger.
- Auto-restack **only when the merge is mechanical** (`gix-merge` reports clean in-memory). Real conflicts ⇒ freeze the stack segment, emit a Passive Prompt (Section 5) to the *downstream* agent, never guess.
- **CI debounce**: coalesce restack pushes in a 60–120s window; keep PRs draft until the stack head is stable; document alignment with GitHub merge queue (the stack order should feed the queue, not fight it).
- The daemon holds a **landing plan**: total order over the commit DAG (declared deps ∪ conflict-graph edges). Merging out of order is a *pre-merge check failure*, deterministically detected.

### Failure Mode 4 — Cross-repo partial failure / non-transactional landing

Agent 1's change spans `auth-service` (producer) and `payment-service` (consumer); only auth conflicts. Two real problems:

1. **Resolution mode granularity** — a global `sp config` mode is too coarse; mode, cohort, and stack must be **per-repo, joined by a cross-repo `PublishGroup`**.
2. **Landing order is a deploy contract** — API changes must land consumer-compatible-first. The daemon enforces: a PR in group G with a dependency edge cannot merge until its prereq is merged (pre-merge status check + `pre-push` verdict). A partial revert (auth merged, payment reverted) is surfaced as a **group health violation** in the bulletin/`sp status`, because it is a broken deploy contract, not a git conflict.

**Honorable mentions (design corrections, not top-4):**

- `bulletin.md` as state is wrong — it must be a *rendered view* of the ledger (atomic write-tmp-then-rename), never the source of truth.
- FCFS "notice on next git command" misses agents that make no git calls for 20 minutes — the MCP server should also deliver notices on *any* `sp` MCP tool call and on `git status` (via `post-index-change` hook or a cheap FSEvents watch on the sandbox `.git`).
- Sequence-conflict detection must run on **merge-base diffs** (what's actually being introduced), not working-tree scans, or every long-lived branch false-positives.

---

## 2. Original Enhancements

### 2.1 Sandbox resource namespaces (the mvn/npm problem)

Two agents running `mvn test` collide on Tomcat/Spring ports, Docker container names, Redis indexes, Gradle daemons, and temp dirs. The daemon owns a **ResourceLease ledger** and `sp shell` injects a per-agent band:

- `SP_PORT_BASE=21000+1000·n` (exported as `SERVER_PORT`, plus a tiny `sp exec` wrapper that rewrites `-Dserver.port=`)
- `COMPOSE_PROJECT_NAME=sp_a{n}` (container-name prefixing for free)
- `REDIS_DB={n}`
- per-agent `TMPDIR` and Gradle daemon registry (`-Dorg.gradle.daemon.registry=`)
- shared *read-only* dependency caches (`~/.m2` fine; per-agent `~/.gradle/caches/daemon`)

Cost: ~200 lines of deterministic code; eliminates the most common "works in my sandbox" failure.

### 2.2 Pre-write intent leases ("declarative occupancy")

Add MCP tool `sp_claim(paths[], sequence_keys[])` + a cheap hook-verified rule: claiming is advisory at edit-time but **mandatory at commit-time** — committing touched paths you never claimed is itself a violation. Daemon intersects claims against merge-base diffs *before* bytes are written, collapsing most conflicts from "resolve after the fact" to "never happened." Detection remains the backstop; leases make the common case free.

Bonus signal: `sp claim` yields the cohort/stack dependency edges for free (an agent claiming `dbchangelog.xml` with `id=200` while another holds 200 *is* the sequencing conflict, discovered at minute 2 instead of PR time).

### 2.3 Deterministic cross-repo contract gate

On `pre-commit`, when a diff touches contract surfaces (`*.proto`, `openapi.yaml`, `schema.graphql`, config-matched `dto/` paths), extract the public-shape delta (textual, schema-aware — no LLM) and record it on the commit as a `ContractDelta` ledger entry. When any group member's PR is about to merge, the daemon checks consumer repos' *current* main against the delta (do referenced symbols/paths exist?). This is a heuristic gate, not proof — but false positives cost one cheap Passive Prompt, and one caught producer-before-consumer landing pays for the system.

### 2.4 Shared-object stores + `git gc` arbitration

Sandboxes must share object DBs via worktrees/`objects/info/alternates` (else CoW clones drift into multi-GB disk and slow fetches). Consequence: `git gc`/repack must be daemon-arbitrated (single flock, run only when no sandbox has refs into loose objects). Easy to forget, catastrophic (`gc` pruning an object another sandbox's HEAD needs).

---

## 3. Crate Stack & Memory Budget

| Concern | Choice | Why |
|---|---|---|
| Git | **`gix` (gitoxide): `gix-object`, `gix-diff`, `gix-traverse`, `gix-index`, `gix-merge`, `gix-protocol`** | Pure Rust ⇒ Rust-controlled allocations (the whole point of a 15 MB budget). `git2`/libgit2 keeps arenas in C malloc you can't bound or introspect. `gix-merge` gives in-memory merge-tree (the conflict oracle) without materializing a checkout. Fallback: shell out to `git merge-tree --write-tree` (git ≥ 2.38) for pathological cases — git's own C is fast and short-lived. |
| IPC | **std `UnixListener` + `mio` (or tokio current-thread)** | Traffic is ~10 msg/s, 64 KB frames. Multi-thread tokio buys nothing here; current-thread `tokio` is the pragmatic pick (timers for leases/barrier are real and it avoids `mio` boilerplate). Budget it: ~0.5 MB. |
| Ledger serialization | **bincode snapshots + atomic rename + `mmap` for readers. Not rkyv (yet).** | Engineering honesty: at 5 agents the ledger is **tens of KB** (agents × repos × leases). A full bincode re-serialize on mutation is sub-millisecond; readers `mmap` the snapshot and check a header checksum + epoch. `rkyv` zero-copy earns its complexity only if you later embed diff-stats/symbol tables (MB-scale). Design the API (`SnapshotReader`) so rkyv slots in behind it. |
| Hashing/ids | `blake3` (content ids, patch keys) | Fast, one-shot, tiny. |
| XML/YAML rules | `roxmltree` (Liquibase), `serde_yaml` | Deterministic AST, not regex, for `<changeSet id>`. |
| Watching | `notify` (FSEvents on macOS / inotify on Linux), **watch only `.scratchpost/` and sandbox `.git` dirs** | FSEvents is one coalescing stream regardless of file count — O(1) memory. Primary event source is hooks+MCP; the watcher is only for staleness/abandon detection. |
| Allocator | system default (or `mimalloc` if fragmentation is measured) | jemalloc adds ~1–2 MB of resident arenas; wrong direction for this budget. |

**RSS budget (measured target ≈ 12 MB idle):**

```text
binary text+rodata (stripped, lazy-loaded)   ~3.0 MB
tokio current-thread + mio/fsevents threads  ~0.5 MB
gix object cache (LRU-capped, hard cap)      ~4.0 MB  ← the knob that enforces the budget
ledger + WAL (mmap, file-backed)             reclaimable page cache, not dirty RSS
per-session state (≤16 sessions × ~4 KB)     ~0.1 MB
hooks/handshake buffers (16 × 64 KB frames)  ~0.2 MB
rule engine + misc (blake3, notify, slab)    ~1.0 MB
margin                                     + ~3.0 MB
```

Enforce with a hard LRU cap on the object cache and a `getrusage`-based self-check that logs `rss_kb` every 60s (CI can assert `< 15_000`). One daemon *per master folder* — a fleet of 5 agents is one daemon, not five.

---

## 4. Wire Protocol & Ledger Schema

**Transport:** per-daemon UDS at `.scratchpost/run/daemon.sock`. One request/response per connection (connect–write–read–close; no session keepalive needed except MCP, which is its own process). Frame: `[u16 version][u16 msg_kind][u32 len_le][bincode payload]`, `len ≤ 65536`, read deadline 250 ms.

**Failure policy:** read-only requests fail *open* (shim proceeds, warns); mutation-gate requests (`PreCommitCheck`, `PrePushCheck`) fail *closed* past a 5 s grace (hook exits 1 with "daemon unavailable" Passive Prompt) — commits are cheap to retry, silent conflicts are not.

```rust
// ---------- Ledger (daemon-owned; readers mmap immutable snapshots) ----------
pub const LEDGER_MAGIC: [u8; 4] = *b"SP01";

#[repr(u8)] pub enum AgentStatus { Active, Suspended, BarrierParked, Abandoned, Retired }
#[repr(u8)] pub enum RepoMode { BarrierStacked, BarrierIntegration, Fcfs }

#[derive(Serialize, Deserialize)]
pub struct AgentSlot {
    pub agent_id: AgentId,               // interned u32
    pub session_ids: SmallVec<[SessionId; 4]>,
    pub lease_expires_ms: u64,           // monotonic ms; renew = hook/MCP touch
    pub status: AgentStatus,
    pub sandbox: SandboxBinding,         // { repo: RepoId, worktree_path_hash, branch: InternedStr, head: ObjectId }
    pub claims: SmallVec<[ClaimKey; 8]>, // sp_claim'd paths + sequence keys
}

#[derive(Serialize, Deserialize)]
pub struct RepoSlot {
    pub repo_id: RepoId,
    pub git_dir_fingerprint: [u8; 16],   // blake3 of canonical main-repo identity
    pub mode: RepoMode,
    pub origin_main: ObjectId,           // last fetched tip (staleness = detected here)
    pub stack: Vec<StackEntry>,          // landing plan, topologically ordered
    pub gc_arbitration: GcState,         // flock holder + generation
}

#[derive(Serialize, Deserialize)]
pub struct SequenceClaim {
    pub rule_id: RuleId,                 // e.g. b"liquibase-changeset", b"flyway-filename"
    pub key: i128,                       // 200; negative = range reservation
    pub repo_id: RepoId, pub path: InternedStr,
    pub owner: AgentId, pub commit: Option<ObjectId>,
}

#[derive(Serialize, Deserialize)]
pub struct WorkspaceLedger {              // snapshot file format
    pub magic: [u8; 4], pub format_version: u16,
    pub epoch: u64,                      // +1 per daemon restart; clients re-register on change
    pub generation: u64,                 // monotonic; bumped per mutation (seqlock for readers)
    pub checksum: u32,                   // crc32 of payload
    pub agents: Slab<AgentSlot>,
    pub repos: Slab<RepoSlot>,
    pub sequences: BTreeMap<(RepoId, RuleId, i128), SequenceClaim>,
    pub intents: IntentTrie,             // path-prefix occupancy per repo
    pub barrier: Option<BarrierState>,   // { cohort, arrivals: BitVec, deadline_ms, fallback: FallbackPolicy }
    pub broadcasts: RingBuffer<BroadcastEntry, 64>, // FCFS notices; ring, newest wins
    pub groups: Slab<PublishGroup>,      // cross-repo landing contracts + dependency edges
}
// Durability: mutations append to .scratchpost/ledger.wal (bincode frames, fsync on
// gate decisions only); snapshot rewritten atomically (tmp+rename) every N muts / 250ms.

// ---------- IPC ----------
#[derive(Serialize, Deserialize)]
pub enum ShimToDaemonRequest {
    Hello      { session: SessionInfo, epoch_seen: u64 },   // { agent_id, tty_pgid, cwd_hash, shim_ver }
    Heartbeat  { session: SessionId },
    Claim      { agent: AgentId, keys: Vec<ClaimKey> },     // paths + sequence keys
    PreCommitCheck { session: SessionId, repo: RepoId, staged: CommitDigest }, // tree hash + touched paths
    PrePushCheck   { session: SessionId, repo: RepoId, refs: Vec<(String, ObjectId)> },
    SequenceAllocate { repo: RepoId, rule: RuleId, desired: i128, count: u16 },
    BarrierSignal  { cohort: CohortId, agent: AgentId, kind: ArrivalKind },
    NoticePoll     { session: SessionId, since_generation: u64 },
}

#[derive(Serialize, Deserialize)]
pub enum DaemonToShimResponse {
    Ack { generation: u64, granted: Vec<i128> },
    Blocked { verdict: BlockVerdict, prompt: Box<PassivePrompt>, nonce: u64 }, // hook exits 1
    Stale { daemon_epoch: u64 },          // client must re-Hello
    Notice(Vec<BroadcastEntry>),          // e.g. "origin/main moved; agent_2 merged #141; rebase advised"
}

pub enum BlockVerdict {
    PathContention { holder: AgentId, overlap: Vec<(PathBuf, PathBuf)> },
    SequenceCollision { rule: RuleId, requested: i128, holder: AgentId, next_free: i128,
                        auto_resequence_cmd: Option<String> },
    BarrierWait { cohort: CohortId, pending: Vec<AgentId>, deadline_ms: u64 },
    StackOutOfOrder { group: GroupId, prereq_unmerged: Vec<PrRef> },
    DaemonUnavailable,
}

// ---------- Sequence rule plugin (compiled-in set + config, not dlopen) ----------
pub trait SequenceRule: Send + Sync {
    fn id(&self) -> RuleId;
    fn applies_to(&self, ctx: &RepoCtx, path: &RepoPath) -> bool;
    /// Deterministic extraction from a blob (parse, don't regex-match — regex only
    /// for filename-shaped sequences like Flyway's V<id>__name.sql).
    fn extract(&self, blob: &[u8]) -> Result<Vec<SequenceKey>, RuleParseError>;
    /// Keys introduced by this change = extract(new_blob) − extract(merge_base_blob).
    fn diff_keys(&self, base: &[u8], new: &[u8]) -> Vec<i128> { /* default via extract */ }
    /// Lowest legal key ≥ desired that no live claim holds.
    fn propose(&self, taken: &BTreeSet<i128>, desired: i128) -> Option<i128>;
    /// Mechanical rewrite for the daemon's auto-resequence (1A). Must be
    /// byte-deterministic and idempotent; anything nontrivial ⇒ Err(NeedsAgent).
    fn rewrite(&self, blob: &mut Vec<u8>, from: i128, to: i128) -> Result<(), RuleError>;
}
// Implementations: LiquibaseChangeSet (roxmltree, id + author attrs), FlywayFilename
// (path rename + content header), SemverMigration, ConfigGated custom.
```

**Key invariant:** `PreCommitCheck` carries the **staged tree hash**, so the daemon computes introduced sequences and content overlap against `merge_base(origin_main)` purely from objects — no worktree materialization, no races with the agent's still-running editor.

---

## 5. Passive Prompt Wrapper Specification

**Design rules:** fixed machine-readable grammar (agents pattern-match it), ≤ 50 lines, exactly **one** next action, copy-pasteable commands, deterministic (same input ⇒ byte-identical output), `SP-*` prefix so agents (and their log scrubbers) recognize it. Delivery: hook `stderr` (exit 1 ⇒ `git commit` fails) and/or MCP `tool_result` with `isError: true` carrying the identical text. The shim forwards hook stderr verbatim.

```text
SCRATCHPOST-BLOCK v1
verdict: SEQUENCE_COLLISION
repo: auth-service
rule: liquibase-changeset
conflict: <changeSet id="200"> already claimed
claimed_by: agent_2 (branch sandbox/agent_2, commit a1f9c2d, pushed 14:22Z, PR #141)
detected_in: db/changelog/dbchangelog.xml (staged diff introduces id=200)
basis: origin/main@3e7b21a + 1 broadcast (generation 841)

resolution (single next action — run this, then retry your commit):
    sp resequence db/changelog/dbchangelog.xml --rule liquibase-changeset --from 200 --to 201
    git commit -m "<your original message>"

verify (optional, before commit):
    sp check --staged

constraints:
  - do NOT change the id to an unsuggested value; claim first with:
      sp claim --rule liquibase-changeset --id <n>
  - if your changeSet semantically duplicates agent_2's (same table/column DDL),
    abandon yours and note it in your PR description instead of resequencing.
nonce: 841:0x3f7d   daemon-epoch: 7   ledger-gen: 843
```

**Content-conflict variant** replaces the middle block with `verdict: PATH_CONTENTION`, the overlapping path ranges (from the `gix-diff` blob diff of staged vs holder's pushed commit), and the resolution command `sp rebase --onto sandbox/agent_2 --auth <path>` or `sp hold <path> until agent_2:merged`.

**Barrier variant:** `verdict: BARRIER_WAIT` with the pending cohort list and "your commit is parked as a signed local commit; it will be replayed automatically at barrier release — do NOT retry in a loop; poll `sp status` at most every 60s". Loop-prevention lines matter: LLM agents hammer retries otherwise; every template gets an explicit anti-retry clause where relevant.

`nonce` + `ledger-gen` let the daemon dedupe prompt deliveries and let the shim discard notices older than what the agent already saw.

---

## Suggested Build Order

1. Ledger + WAL + UDS protocol
2. Worktree sandboxes + hook enforcement + `sp shell`
3. `PreCommitCheck` with content contention
4. `SequenceRule` engine + resequence
5. Barrier / stack orchestration
6. FCFS broadcast polish
7. Publish groups (cross-repo landing contracts)

*Sections 1 and 2 corrections (cwd contract, hooks-not-shim, lease/TTL, per-repo modes) are architectural and must land before coding starts.*
