# ScratchPost RFC v0.1 — Architecture Review and Technical Contracts

## 0. Verdict and non-negotiable invariants

The coordination model is sound. The proposed isolation boundary is not.

A `git` shim is a **policy-enforcement point**, not a filesystem isolation boundary. It cannot stop an IDE, build tool, test runner, Docker build, or absolute-path invocation from mutating the master working tree. ScratchPost must isolate the **session workspace first**, then use the shim to enforce Git and publication policy.

Before coding, adopt these invariants:

1. **The master workspace is never an agent’s working directory.** Each session gets a canonical isolated workspace root. No symlink trickery from the master path.
2. **The daemon is the single writer for ledger state, refs, fetches, and pushes.** Shims may read a memory-mapped snapshot; they mutate only through IPC.
3. **A commit decision is bound to an immutable index snapshot**, not to whatever happens to be in the index milliseconds later.
4. **Barriers are scoped to conflict components, not “all agents globally.”** A global barrier is an over-synchronization deadlock generator.
5. **Same-UID agents cannot be securely contained by this design.** Treat the shim as correctness tooling, not a hostile-agent sandbox. A real security boundary requires separate UIDs, filesystem permissions, and/or mount namespaces.

---

# 1. Brutal Architectural Critique and Top Failure Modes

## 1.1 Failure Mode 1 — The shim creates an isolation illusion; path resolution breaks first

### The core problem

If an agent process has `cwd=/master/auth-service`, it writes `/master/auth-service/UserDTO.java`. Setting `GIT_WORK_TREE` or `GIT_INDEX_FILE` does not change that. It only changes what `git` thinks the tree is. You then get the worst possible state:

- The editor/build tool mutates the shared master tree.
- The Git shim stages files from an isolated tree.
- Tests run against a mixture of both.
- The daemon validates one content set while the agent commits another.

`GIT_WORK_TREE` and `GIT_INDEX_FILE` are useful **inside a single shim invocation**, with a scoped environment. They must never be exported globally to agent processes.

### Path strategy comparison

| Strategy | Result |
|---|---|
| Export `GIT_WORK_TREE` / `GIT_INDEX_FILE` | Does not isolate file writes; leaks into hooks, build plugins, and child processes. Rejected. |
| Replace `/master/auth-service` with a symlink per agent | Destroys the shared master path, breaks `getcwd()`, `realpath`, IDE indexes, Docker contexts, and all other agents. Rejected. |
| FUSE overlay per agent | Strong path transparency, but high syscall latency, memory overhead, cache incoherence, and fragile behavior with mmap, `O_DIRECT`, databases, and build tools. Rejected for the 15–25 MB budget. |
| Linux mount namespace/bind mount per session | Best path transparency on Linux. Requires privileges or user-namespace setup; unavailable on macOS. Optional advanced mode only. |
| `git worktree` per agent | Good for Git isolation and object sharing; still requires agents to actually run in the worktree. Shared refs, fetches, GC, and hooks must be daemon-owned. Viable. |
| Full CoW session workspace (`clonefile`/`FICLONE` or overlay upperdir) | Best practical default. The session root is real, canonical, and independent. More provisioning work, but no lying to the OS. Recommended. |

### Recommended topology

Use a **session workspace root**:

```text
/master/                                  # canonical, daemon-owned, agents do not write
  auth-service/
  payment-service/
  .ai-context.md
  .scratchpost/
    sessions/<session-uuid>/
      workspace/
        auth-service/                     # CoW-cloned or worktree
        payment-service/
        .ai-context.md                    # CoW copy, never hardlink
      caches/                             # per-session caches
```

`sp shell`, `sp run`, and `sp --mcp` all launch the agent with:

```text
cwd       = /master/.scratchpost/sessions/<id>/workspace
SP_ROOT   = that path
SP_AGENT  = <stable-session-id>
PATH      = <sp-shim-dir>:$PATH
```

The master path is not symlinked. The MCP server reports only sandbox paths. If an IDE is used, the IDE workspace must be opened at the session root. That is an explicit product requirement, not an inconvenience to hide.

The shim should reject any invocation whose physical cwd resolves outside the session root:

```text
SP_PATH_OUTSIDE_SESSION
requested=/master/auth-service
session_root=/master/.scratchpost/sessions/...
correct_path=/master/.scratchpost/sessions/.../auth-service
```

Use `realpath`/`getcwd` semantics, not `$PWD`. Check symlinks: a repository can contain a relative or absolute symlink escaping the session root. Scan for escape symlinks at session creation and block commits containing them.

### The bypass surface is larger than “git”

The following do not necessarily honor a `git` PATH shim:

- IDE Git integrations using libgit2/JGit.
- Maven/Gradle/SBT plugins invoking JGit or absolute Git paths.
- `gh`, GitHub API calls via `curl`, custom release scripts.
- `pre-commit`, commit hooks, and build scripts.
- Docker build contexts and test fixtures using absolute master paths.
- `command git`, `/usr/bin/git`, shell functions, aliases, or `xcrun git`.

Therefore:

- Filesystem isolation must protect file contents.
- The shim protects the **normal Git control path** and produces deterministic refusal messages.
- Publication (`git push`, PR creation) must be daemon-owned; otherwise agents can bypass ordering by invoking `gh` directly.
- If hostile-agent containment is required, this architecture must run agents under distinct UIDs and the daemon under a separate privileged UID, with the session workspace owned by the agent UID and the ledger not writable by it.

### TOCTOU: validate-then-commit is unsafe

Even with perfect path isolation, this sequence is racy:

1. Shim asks daemon to validate staged content.
2. Daemon approves.
3. Agent or a hook modifies the index.
4. Shim runs real `git commit`.
5. The committed tree is not the validated tree.

Fix this with an **immutable index snapshot and commit permit**:

1. Shim copies the index into a private snapshot file.
2. Shim computes the staged tree OID and sends it to the daemon.
3. Daemon validates that exact tree and issues `CommitPermit`.
4. Shim runs `git commit` with `GIT_INDEX_FILE=<snapshot>`, scoped and sanitized.
5. Shim sends `RecordCommit` with the resulting commit OID and tree OID.
6. Daemon verifies the commit’s tree and parent match the permit; a mismatch produces `SP_COMMIT_POSTCONDITION_FAILED`, blocks push, and marks the intent suspect.

Do not rely on a pre-commit hook as the enforcement mechanism; hooks are bypassable and may mutate the index.

---

## 1.2 Failure Mode 2 — Barrier mode deadlocks or stalls on abandoned agents

“Wait for all active agents” has no bounded termination condition. Real scenarios:

- Agent 3 is in a 12-minute LLM reasoning/tool call and cannot heartbeat.
- Agent 3’s terminal is closed; its process dies.
- Agent 3 is alive but the user abandoned the session.
- Agent 3 has no changes but never calls `sp barrier arrive`.
- A new agent starts while the barrier is open, extending “all” indefinitely.
- An agent waits for a file owned by another agent, while that agent waits at the barrier. The barrier does not know about this semantic dependency.

### Required barrier semantics

- Barrier participants are snapshotted at `barrier open`; late arrivals join the next epoch.
- Agents with no registered mutation intent auto-arrive or are excluded.
- Each participant has a lease and heartbeat. `sp shell`/`sp run` supervises the child process group and reports exit. A heartbeat is also sent on Git/MCP operations.
- The daemon never holds a repository lock while waiting for a barrier.
- A barrier has a hard deadline. On expiry, apply a configured policy:
  - `SP_BARRIER_TIMEOUT` and degrade to FCFS for ready components.
  - `release_ready` and publish non-conflicting components.
  - `fail_closed` and require operator action.
- Never default to “wait forever.”
- A crashed session’s claims and intents move to `Abandoned`; tombstoned sequence IDs remain tombstoned until safely reclaimable.
- Barrier scopes should be per conflict component. If Agent 3’s work has no graph edge to Agents 1 and 2, it must not block them.

Use monotonic time for deadlines. Wall-clock changes must not extend or expire barriers arbitrarily.

### Deadlock prevention rules

1. Wait-for edges are only `agent -> barrier` or `agent -> publication queue`; barriers are resolved by the daemon, never by agents waiting on each other.
2. No shim holds a Git lock, index lock, or repo mutex while waiting for IPC.
3. Daemon lock ordering is deterministic, e.g. acquire repo locks by ascending `RepoId`, never nest a second repo lock while holding one.
4. Every queued operation has a deadline and an idempotency key.
5. Cycles in the conflict graph are explicit `SP_INTENT_CONFLICT` states, not silent waits.

---

## 1.3 Failure Mode 3 — Stacked PRs become a rebase/CI cascade

The proposed 1A behavior — “bump Agent 2 to 201, rebase onto Agent 1’s branch, open PR #2” — is dangerous if performed after commits exist:

- Rewriting a committed changelog changes the tree and commit hash.
- The force-push invalidates CI and often review approvals.
- If PR #1 fails CI or receives review changes, every dependent branch needs restacking; each restack invalidates CI again.
- If PR #1 is squashed or dropped, PR #2’s base and sequence assumptions change.
- If PR #1 is merged out of order, a “clean” stack can still break `main`.
- If the sequence claim is in an earlier commit, rewriting the tip is insufficient.

### Recommendation

Do not make branch stacking the primary correctness mechanism. Prefer:

1. **Integration branch per service or change set.** All validated intents merge into `integration/<change-set>`; one PR to `main`. This is boring, robust, and matches how CI actually behaves.
2. If stacked PRs are required, use a **publication DAG** keyed by content/patch identity, not mutable branch names.
3. Reserve sequence IDs **before** commit creation. Never auto-rewrite after commit without explicit policy and a new commit.
4. Mark dependent PRs draft until the parent is green.
5. On parent restack, record old/new head OIDs; do not silently force-push. Notify dependents and require a restack command.
6. Use a merge queue or merge train for final ordering. The daemon should not emulate a merge queue with ad hoc branch rebases.

The correct ordering unit is the **change set**, not the individual commit.

---

## 1.4 Failure Mode 4 — Multi-repository partial failure

Agent 1 modifies `auth-service` and `payment-service`. `auth-service` has a sequence collision; `payment-service` is clean. The naive implementation commits and pushes payment, then blocks auth. The result is a half-published change set: payment consumers see a new API before auth is available, or a PR exists for only half the feature.

Remote Git has no atomic multi-repository transaction. You need an explicit two-phase publication protocol:

```text
ChangeSet: Draft
  -> all intents staged locally
  -> all repositories validated
  -> all commits created locally
  -> publication plan computed (topological order)
  -> push all branches / create all draft PRs
  -> if any push fails: PartiallyPublished, no PR is marked ready
  -> all checks green: Published
  -> merge queue / integration branch: Merged
```

A failure after the first push cannot always be rolled back safely. Mark `SP_CHANGESET_PARTIAL_FAILURE`, preserve the pushed branch, do not mark the change set ready, and emit a recovery prompt listing:

- succeeded repositories and branch OIDs,
- failed repositories and error codes,
- safe retry command,
- whether deletion of the remote branch is supported/authorized.

Per-repository barriers are insufficient when repositories have API or migration dependencies. The ledger needs a `ChangeSet` object with dependency edges and a single publication state.

---

## 1.5 Additional high-probability failure modes

| Failure | Consequence | Required treatment |
|---|---|---|
| Shared `.git` worktree concurrency | Ref/index/packed-refs races, `git gc` deleting objects, fetch races | Daemon owns fetch, worktree administration, push, and GC. Disable per-agent auto-GC. Serialize mutations. |
| FCFS `bulletin.md` read/write race | Agent reads stale notice; last writer loses data | Append-only notice log with monotonic sequence; `bulletin.md` is a generated projection only. |
| Agent bypasses shim with `gh pr create` or `/usr/bin/git` | Unordered PR or unpoliced push | Daemon-owned publication; shim `gh` too or document explicitly that only `sp publish` is supported. Remote branch protection. |
| Hardlinked shared files/caches | One agent edits `.ai-context.md`, all agents change; caches mutate shared inodes | CoW copy, not hardlinks. Per-session cache directories. |
| Sequence parser false negatives | Collision reaches `main` | High-confidence rules only; parse structured formats; fail closed for known rule namespaces. |
| Sequence parser false positives | Agents blocked unnecessarily | Rule-specific confidence and severity; `sp sequence explain`; explicit allowlist. |
| Long-running tests race with rebase | Test reads files while branch changes | Session mutation lease; no auto-rebase while tests hold a lease. |
| Daemon crash midway through publication | Orphaned intents, stale locks, half-pushed branch | Persistent journal, startup reconciliation, `PartiallyPublished` recovery state. |
| Reused `AGENT_ID=agent_1` across sessions | Stale worktree/branch/claims | Stable session UUID plus human label; reject duplicate active labels or attach explicitly. |
| Absolute symlink inside repo | Agent writes through to master | Scan and reject escape symlinks; document same-UID limitation. |
| Non-UTF-8 paths / case-insensitive FS | Repo lookup mismatch, path collisions | Normalize with platform rules, store byte paths in IPC, compare with filesystem identity. |
| MCP notification assumptions | Agent never sees broadcast | Piggyback notices on every MCP response and every shim command; never depend on unsolicited notifications. |

---

# 2. Original Architectural Enhancements

## 2.1 Per-component adaptive coordination with a speculative mergeability graph

A global barrier is the wrong default. Build a conflict graph from the current intents:

- textual merge conflicts from `merge-tree`,
- same-file overlap or adjacent-hunk overlap,
- sequence-claim namespace collisions,
- API contract and migration dependency edges,
- declared change-set edges.

Then:

- Split the graph into connected components.
- Run a barrier only inside each conflicting component.
- Publish independent components immediately under FCFS.
- Compute a deterministic topological order for each component.
- Continuously run pairwise `merge-tree` checks against the moving base and other intents, producing a **mergeability radar**.

Benefits:

- No global stall when Agent 3 is unrelated.
- Conflicts are surfaced before an agent finishes.
- FCFS and barrier become policy decisions over the same graph, not separate implementations.
- The daemon can tell an agent early: “Your change is currently mergeable, but sequence namespace `dbchangelog` is contended by Agent 2.”

The radar must be incremental: cache by `(base_tree, intent_tree, rule_set_version, patch_id)`, not recompute the full graph on every command.

## 2.2 Resource lease broker and hermetic test environment

Two agents running `mvn test` or `npm test` will collide on:

- fixed application ports,
- Testcontainers ports,
- Docker Compose project names and networks,
- shared database schemas,
- Maven/Gradle/npm/Cargo caches,
- local temp directories and sockets.

Add a per-session resource lease broker:

```text
sp run --profile test -- mvn test
```

The daemon allocates and exports, for example:

```text
SP_SESSION_ID
COMPOSE_PROJECT_NAME=sp_<session>
SP_PORT_BASE=41000
TESTCONTAINERS_REUSE_ENABLE=false
TESTCONTAINERS_RYUK_DISABLED=false
MAVEN_OPTS=-Dmaven.repo.local=<session>/caches/m2
GRADLE_USER_HOME=<session>/caches/gradle
npm_config_cache=<session>/caches/npm
CARGO_TARGET_DIR=<session>/caches/target
TMPDIR=<session>/tmp
SP_TEST_DATABASE_SCHEMA=sp_<session>
```

Resource leases are bounded, tracked in the ledger, and reclaimed on session exit or lease expiry. The daemon can also enforce a test concurrency cap to protect the host. This is higher leverage than merely isolating Git: most “parallel agent” failures on a developer workstation are actually port, cache, and database contention.

## 2.3 Cross-repository contract fingerprint graph and change-set publication gate

For microservices, Git mergeability is not semantic compatibility. Build deterministic contract fingerprints for:

- OpenAPI/JSON Schema,
- Protobuf/gRPC descriptors,
- Avro schemas,
- public Java/Kotlin types where a parser is available,
- database migration lineage and schema versions,
- event topics and message envelopes.

Store a fingerprint per published artifact and an edge for each consumer. On commit/publication:

- Detect breaking changes to a consumed contract.
- Require the producer and known consumers to be in the same change set, or require an explicit versioning decision.
- Detect migration ordering dependencies across services.
- Block publication if a consumer would observe an incompatible producer.

This does not require an LLM. It is a deterministic parser plus fingerprint/diff engine. It also gives the mergeability graph real semantic edges instead of merely textual ones.

### Optional but valuable: fencing tokens on all state transitions

Every session and intent carries a monotonically increasing fencing token. Any daemon operation on behalf of a session validates:

```text
request.session_id + request.fencing_token == current grant
```

A stale shell or duplicated MCP process cannot mutate the ledger after session replacement. This avoids the classic “old agent process wakes up and commits into the new session” failure.

---

# 3. Exact Rust Crate Stack and Memory Footprint

## 3.1 Recommendation

### Git

Use a **hybrid**:

- `gix` for read-only local operations: repository discovery, object database reads, refs, trees, index inspection, revision walking, status, diff metadata.
- The system `git` binary for mutating/plumbing operations: `fetch`, `worktree add/remove`, `merge-tree`, `rebase`, `push`, and final commit creation.

Why not pure `gix` for everything? Git’s mutating surface is enormous and compatibility-sensitive. Reimplementing fetch, worktree administration, hooks, signing, and rebase in-process increases risk and resident memory. A short-lived system `git` subprocess is the correct trade-off for an ultra-low-memory daemon.

Why not `git2`? `git2` is mature for merge/rebase, but it links libgit2 and adds C runtime/build complexity and resident memory. If the project mandates “no subprocess Git plumbing,” use `git2` with vendored libgit2 and accept roughly 1–2 MB additional RSS. The recommended default remains `gix` plus supervised system Git for mutations.

Sanitize every subprocess:

```text
GIT_TERMINAL_PROMPT=0
GIT_CONFIG_NOSYSTEM=1
GIT_OPTIONAL_LOCKS=0
GIT_PAGER=cat
LC_ALL=C
```

and run with:

```text
git --no-optional-locks -c gc.auto=0 -c maintenance.auto=false ...
```

Never pass agent-controlled config overrides into daemon-owned operations.

### IPC

Use `std::os::unix::net::UnixListener` / `UnixStream` with a small custom thread pool. Do **not** use Tokio.

- Five clients do not justify an async runtime.
- Tokio adds dependency tree and roughly 1–3 MB RSS.
- Blocking UDS with a 2–4 worker pool and 256 KB thread stacks is simpler and easier to audit.

Use `SOCK_STREAM` with length-prefixed frames, because macOS AF_UNIX `SOCK_SEQPACKET` support is not portable. If available, `SOCK_SEQPACKET` may be used as an optimization behind a transport abstraction.

### Serialization

- **IPC:** `serde` + `postcard`. Compact, deterministic enough for versioned enums, no schema runtime.
- **Memory-mapped ledger:** `rkyv` + `memmap2`. `rkyv` gives zero-copy archived reads from the mmap. `bincode` would require deserializing into owned structures in every shim, defeating the zero-copy goal and increasing RSS.
- **MCP:** `serde_json` is required by JSON-RPC/MCP. Keep it in the MCP bridge path; do not make the daemon’s primary state JSON.
- **Ledger durability:** append-only journal using fixed-width records and CRC32C; mmap is a published snapshot, not the source of truth.

### Parsing and policy

- `quick-xml` for Liquibase/POM XML. Avoid `tree-sitter`; grammar binaries and parsers add memory and complexity.
- `regex` or `regex-lite` for filename rules. Keep regex sets static and bounded.
- `globset` or a small custom path matcher for rule scopes.
- `lexopt` for the CLI. Avoid `clap` if binary size/RSS matters.
- Custom tiny structured logger. Avoid `tracing` + subscriber in the daemon.

### File watching

Do not use a persistent watcher by default. The shim generates the important events. On session attach and commit, scan the workspace and relevant root context files.

If watching is required for `.ai-context.md` or root configuration, use `notify` only behind a feature flag, or a direct `inotify`/`kqueue` implementation. `notify` adds a watcher thread, event buffers, and platform backends that count against the budget.

### Suggested dependency set

Pin exact versions in `Cargo.lock`; do not use floating `*` ranges.

```toml
[dependencies]
gix = { version = "<pinned>", default-features = false, features = ["index", "status", "revision", "dirwalk", "blob-diff"] }
serde = { version = "1", features = ["derive"] }
postcard = { version = "1", features = ["use-std"] }
rkyv = { version = "0.8", features = ["bytecheck", "alloc"] }
memmap2 = "0.9"
quick-xml = "<pinned>"
regex = "<pinned>"          # or regex-lite if the rules permit
globset = "<pinned>"
serde_json = "1"
lexopt = "<pinned>"
libc = "0.2"                # peer credentials, flock, low-level UDS details
# Optional: socket2 for transport control; rustix/nix only if justified.
```

Rejected by default:

```text
tokio, notify, clap, tracing-subscriber, jemalloc,
reqwest/openssl, tree-sitter, rayon, serde_yaml
```

## 3.2 Memory budget

A realistic accounting:

| Component | Idle RSS target | Notes |
|---|---:|---|
| `scratchpostd` | 6–9 MB | Binary text, bounded heap, ledger mmap pages, 2–4 small threads |
| 5 × `sp --mcp` bridges | 1.0–1.6 MB each = 5–8 MB | Same multi-call binary; text pages shared in page cache/PSS |
| 5 idle agent shells | 0 MB ScratchPost | Not owned by ScratchPost |
| Transient Git shims | 1.2–2.5 MB each | Only during a command; not idle |
| Idle total | **11–17 MB** | Feasible |
| Peak during five simultaneous commits | Add 6–12 MB | 17–29 MB; usually serialized by daemon operations |

Therefore:

- **15 MB idle is achievable**, but tight.
- **15 MB peak including five concurrent shims and five MCP bridges is not realistic** without deleting the MCP bridge processes or accepting a much smaller daemon feature set.
- **25 MB peak is realistic** and should be the stated engineering target.

Measure PSS, not the sum of RSS, because the same binary text pages are shared across processes. Report daemon RSS, bridge PSS, shim peak RSS, and mmap resident pages separately.

### Techniques to stay under budget

- `panic = "abort"`, `lto = "fat"`, `codegen-units = 1`, `strip = "symbols"`, `opt-level = "s"`, no debug info.
- Static musl build if distribution constraints allow.
- No jemalloc/mimalloc unless measured better; system allocator is usually lower for this workload.
- Thread stacks 256–512 KB, not the default 2 MB.
- Bounded ledger: fixed-capacity slots, interned strings, 64–128 KB string arena, no unbounded vectors.
- Snapshot ledger size target under 512 KB; two slots under 1 MB.
- Do not cache every repository object. One lazy repo handle per active repo, close idle handles.
- No persistent file watcher.
- No resident TLS stack. Fetch through system `git`.
- Publish ledger snapshots on state change with debounce, not on every heartbeat.
- Cap notices and prompt payloads; truncate with a reference to `sp conflict show`.
- Never keep full diffs or file contents in ledger.

---

# 4. IPC Wire Protocol and Ledger Schema

## 4.1 Transport

Use a Unix domain socket. Because macOS limits UDS path length to roughly 104 bytes, do not assume `.scratchpost/run/daemon.sock` is short enough:

```text
/tmp/sp/<uid>/<workspace-hash>.sock
```

with directory mode `0700`. Store the endpoint and a per-boot capability token in:

```text
<master>/.scratchpost/run/endpoint
<master>/.scratchpost/run/token        # mode 0600
```

Validate peer credentials where available:

- Linux: `SO_PEERCRED`
- macOS: `getpeereid()`

The token deters accidental cross-workspace connections; it is not a security boundary against the same UID.

### Frame format

```text
u32 magic             0x53504950 ("SPIP")
u16 wire_version
u16 flags
u64 request_id
u32 deadline_ms       relative timeout, not wall-clock
u32 payload_len
[u8; payload_len]     postcard-encoded ShimRequest or DaemonResponse
```

Rules:

- Maximum frame: 1 MiB; conflict prompts target < 16 KiB.
- One in-flight request per connection is sufficient; shims may open one connection per command.
- `request_id` is monotonic per process; daemon caches idempotent results for a short TTL.
- All timeouts use monotonic time on the same host.
- The daemon rejects protocol or schema mismatches with `SP_PROTOCOL_MISMATCH`.

## 4.2 State authority and memory mapping

The daemon owns authoritative mutable state in its heap. The mmap is a **published, read-only snapshot** for observability and fast reads. Do not build a mutable cross-process lock-free ledger unless you accept a much harder correctness problem.

Use a double-buffer seqlock:

```rust
#[repr(C, align(4096))]
pub struct LedgerFile {
    pub header: LedgerHeader,
    pub slots: [LedgerSlot; 2],
}

#[repr(C, align(64))]
pub struct LedgerHeader {
    pub magic: [u8; 8],          // b"SPLEDGER"
    pub layout_version: u16,
    pub flags: u16,
    pub schema_hash: u32,
    pub active_slot: AtomicU8,
    pub generation: AtomicU64,
    pub daemon_boot_id: u128,
    pub published_at_ms: u64,
}

#[repr(C, align(64))]
pub struct LedgerSlot {
    pub seq: AtomicU64,          // even = stable, odd = writer active
    pub len: AtomicU32,
    pub payload: [u8; MAX_LEDGER_BYTES],
}
```

Writer:

1. Serialize the new `WorkspaceLedger` into the inactive slot.
2. Increment its `seq` to odd before writing; write payload and length.
3. Increment `seq` to even and publish with `Release`.
4. Store the new `active_slot` and increment `generation`.

Reader:

1. Read `active_slot`.
2. Read `seq`; retry if odd.
3. Validate header, length, CRC32C, and schema hash.
4. Access the archived ledger with `rkyv::access`.
5. Re-read `seq`; retry if changed.
6. Copy only the small fields needed by the shim; do not hold references across daemon restart.

If the shim cannot read a consistent snapshot, it falls back to a UDS `Status` request. The mmap is never a source of truth for sequence claims or commit authorization.

## 4.3 Ledger types

The following are contracts, not the full implementation. Derives and bounds checks are omitted where obvious.

```rust
pub type RepoId = u16;
pub type AgentId = u16;
pub type IntentId = u32;
pub type ClaimId = u32;
pub type ChangeSetId = u32;
pub type BarrierId = u32;
pub type UnixMs = u64;

pub const MAX_AGENTS: usize = 32;
pub const MAX_REPOS: usize = 32;
pub const MAX_INTENTS: usize = 256;
pub const MAX_CLAIMS: usize = 2048;
pub const MAX_CHANGESETS: usize = 64;
pub const MAX_NOTICES: usize = 512;
pub const STRING_ARENA_BYTES: usize = 128 * 1024;
pub const MAX_LEDGER_BYTES: usize = 512 * 1024;

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct GitOid {
    pub bytes: [u8; 32],
    pub len: u8,                 // 20 = SHA-1, 32 = SHA-256
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct InternedStr {
    pub offset: u32,
    pub len: u16,
    pub flags: u8,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct StringArena {
    pub used: u32,
    pub bytes: [u8; STRING_ARENA_BYTES],
}

#[derive(Archive, Serialize, Deserialize)]
pub struct WorkspaceLedger {
    pub workspace_id: u64,
    pub schema_version: u16,
    pub policy: ResolutionPolicy,
    pub barrier: BarrierSlot,

    pub agent_count: u16,
    pub agents: [AgentSlot; MAX_AGENTS],

    pub repo_count: u16,
    pub repos: [RepoSlot; MAX_REPOS],

    pub intent_count: u16,
    pub intents: [IntentSlot; MAX_INTENTS],

    pub claim_count: u32,
    pub claims: [SequenceClaim; MAX_CLAIMS],

    pub changeset_count: u16,
    pub changesets: [ChangeSetSlot; MAX_CHANGESETS],

    pub notice_head: u64,
    pub notices: [NoticeSlot; MAX_NOTICES],

    pub strings: StringArena,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolutionMode {
    Barrier,
    Fcfs,
    Manual,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct ResolutionPolicy {
    pub mode: ResolutionMode,
    pub barrier_timeout_ms: u32,
    pub on_barrier_timeout: TimeoutAction,
    pub sequence_policy: SequencePolicy,
    pub auto_rebase_clean_worktree: bool,
    pub max_auto_rewrite_edits: u8,
    pub max_parallel_tests: u8,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug)]
pub enum TimeoutAction {
    DegradeToFcfs,
    ReleaseReadyComponents,
    FailClosed,
    NotifyOperator,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug)]
pub enum SequencePolicy {
    Block,
    ReserveOnCommit,
    RewriteBeforeCommit,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct AgentSlot {
    pub state: AgentState,
    pub agent_id: AgentId,
    pub pid: u32,
    pub session_generation: u64,
    pub lease_expires_ms: UnixMs,
    pub last_heartbeat_ms: UnixMs,
    pub label: InternedStr,
    pub session_root: InternedStr,
    pub active_repo: Option<RepoId>,
    pub active_intent: Option<IntentId>,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug)]
pub enum AgentState {
    Vacant,
    Starting,
    Active,
    AwaitingBarrier,
    Committing,
    Publishing,
    Completed,
    Abandoned,
    LeaseExpired,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct RepoSlot {
    pub state: RepoState,
    pub repo_id: RepoId,
    pub name: InternedStr,
    pub relative_path: InternedStr,
    pub default_branch: InternedStr,
    pub base_oid: GitOid,        // last observed origin/<default>
    pub local_head_oid: GitOid,
    pub fetch_epoch: u64,
    pub last_fetch_ms: UnixMs,
    pub active_intents: u16,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug)]
pub enum RepoState {
    Vacant,
    Discovered,
    Fetching,
    Ready,
    Degraded,
    Unavailable,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct IntentSlot {
    pub state: IntentState,
    pub intent_id: IntentId,
    pub agent_id: AgentId,
    pub repo_id: RepoId,
    pub branch: InternedStr,
    pub base_oid: GitOid,
    pub head_oid: GitOid,
    pub staged_tree_oid: GitOid,
    pub patch_id: [u8; 20],
    pub claims: [ClaimId; 8],
    pub claim_count: u8,
    pub paths: [InternedStr; 16],
    pub path_count: u8,
    pub changeset_id: Option<ChangeSetId>,
    pub pr_number: u32,              // 0 = none
    pub created_ms: UnixMs,
    pub updated_ms: UnixMs,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug)]
pub enum IntentState {
    Proposed,
    Staged,
    Validated,
    Reserved,
    Committed,
    Pushed,
    PrOpen,
    Merged,
    Closed,
    Blocked,
    Superseded,
    Abandoned,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct SequenceClaim {
    pub state: ClaimState,
    pub claim_id: ClaimId,
    pub repo_id: RepoId,
    pub rule_id: RuleId,
    pub key: SequenceKey,
    pub namespace: InternedStr,
    pub source_path: InternedStr,
    pub owner_intent: Option<IntentId>,
    pub owner_agent: AgentId,
    pub base_oid: GitOid,
    pub created_ms: UnixMs,
    pub expires_ms: UnixMs,
    pub tombstone_until_ms: UnixMs,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuleId(pub [u8; 16]);

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimState {
    Free,
    Reserved,
    Committed,
    Released,
    Tombstoned,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct SequenceKey {
    pub kind: SequenceKeyKind,
    pub numeric: u64,
    pub text: InternedStr,
    pub ordinal: u32,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SequenceKeyKind {
    Numeric,
    Version,
    Lexical,
    Composite,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct BarrierSlot {
    pub state: BarrierState,
    pub barrier_id: BarrierId,
    pub epoch: u64,
    pub opened_ms: UnixMs,
    pub deadline_ms: UnixMs,
    pub participant_count: u16,
    pub arrived_count: u16,
    pub aborted_count: u16,
    pub conflict_component: u32,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug)]
pub enum BarrierState {
    Closed,
    Open,
    Resolving,
    Published,
    Degraded,
    Aborted,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct ChangeSetSlot {
    pub state: ChangeSetState,
    pub changeset_id: ChangeSetId,
    pub agent_id: AgentId,
    pub intents: [IntentId; 16],
    pub intent_count: u8,
    pub dependency_edges: u16,
    pub target: PublicationTarget,
    pub published_count: u8,
    pub failed_count: u8,
    pub created_ms: UnixMs,
    pub updated_ms: UnixMs,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug)]
pub enum ChangeSetState {
    Draft,
    Staged,
    Validated,
    Publishing,
    PartiallyPublished,
    Published,
    Merged,
    Failed,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug)]
pub enum PublicationTarget {
    IndividualPrs,
    StackedPrs,
    IntegrationBranch,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug)]
pub struct NoticeSlot {
    pub seq: u64,
    pub kind: NoticeKind,
    pub severity: Severity,
    pub target_agent: Option<AgentId>,
    pub intent_id: Option<IntentId>,
    pub repo_id: Option<RepoId>,
    pub changeset_id: Option<ChangeSetId>,
    pub created_ms: UnixMs,
    pub code: SpErrorCode,
    pub message: InternedStr,
    pub detail: InternedStr,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug)]
pub enum NoticeKind {
    MutationPublished,
    SequenceReserved,
    ConflictDetected,
    BarrierStateChanged,
    LeaseExpired,
    PublicationBlocked,
    OperatorActionRequired,
}

#[derive(Archive, Serialize, Deserialize, Clone, Copy, Debug)]
pub enum Severity {
    Info,
    Warning,
    Blocking,
}
```

Do not store raw diffs, env values, credentials, or full commit messages in this ledger. Store bounded summaries and OIDs; resolve details from Git on demand.

## 4.4 IPC message contracts

```rust
#[derive(Serialize, Deserialize)]
pub struct ShimContext {
    pub protocol: u16,
    pub agent_id: Option<AgentId>,
    pub session_token: Option<u64>,
    pub pid: u32,
    pub ppid: u32,
    pub cwd: PathBuf,
    pub argv0: String,
    pub tty: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct ShimRequest {
    pub context: ShimContext,
    pub request: ShimToDaemonRequest,
}

#[derive(Serialize, Deserialize)]
pub enum ShimToDaemonRequest {
    Hello {
        shim_version: String,
    },
    AttachSession {
        label: String,
        requested_root: PathBuf,
        capabilities: SessionCapabilities,
    },
    Heartbeat {
        agent_id: AgentId,
        state: AgentRuntimeState,
        active_repo: Option<RepoId>,
    },
    ResolveRepository {
        cwd: PathBuf,
    },
    AuthorizeGit {
        invocation: GitInvocation,
    },
    BeginIntent {
        agent_id: AgentId,
        repo_id: RepoId,
        branch: String,
        base_oid: GitOid,
        paths: Vec<PathBuf>,
    },
    PrepareCommit {
        agent_id: AgentId,
        intent_id: IntentId,
        repo_id: RepoId,
        branch: String,
        expected_head: GitOid,
        index_snapshot_tree: GitOid,
        staged_paths: Vec<PathBuf>,
        message_subject: String,
    },
    RecordCommit {
        agent_id: AgentId,
        permit_id: [u8; 16],
        commit_oid: GitOid,
        tree_oid: GitOid,
        parent_oid: GitOid,
    },
    BarrierArrive {
        agent_id: AgentId,
        intent_id: Option<IntentId>,
        timeout_ms: u32,
    },
    BarrierStatus {
        agent_id: AgentId,
        barrier_id: BarrierId,
    },
    FetchLatest {
        agent_id: AgentId,
        repo_id: RepoId,
    },
    MergeCheck {
        agent_id: AgentId,
        repo_id: RepoId,
        base_oid: GitOid,
        head_oid: GitOid,
        paths: Vec<PathBuf>,
    },
    ReserveSequence {
        agent_id: AgentId,
        intent_id: IntentId,
        repo_id: RepoId,
        rule_id: RuleId,
        namespace: String,
        requested: Option<SequenceKey>,
    },
    ReleaseSequence {
        agent_id: AgentId,
        claim_id: ClaimId,
        reason: ReleaseReason,
    },
    PublishChangeSet {
        agent_id: AgentId,
        changeset_id: ChangeSetId,
    },
    DrainNotices {
        agent_id: AgentId,
        after_seq: u64,
        limit: u16,
    },
    AckNotices {
        agent_id: AgentId,
        through_seq: u64,
    },
    AbandonSession {
        agent_id: AgentId,
        reason: AbandonReason,
    },
    Status {
        scope: StatusScope,
    },
}

#[derive(Serialize, Deserialize)]
pub struct GitInvocation {
    pub raw_argv: Vec<String>,
    pub cwd: PathBuf,
    pub operation: GitOperation,
    pub repo_selector: Option<PathBuf>, // -C / --git-dir
    pub env_fingerprint: u64,
}

#[derive(Serialize, Deserialize)]
pub enum GitOperation {
    Status,
    Add,
    Commit,
    Checkout,
    Switch,
    Branch,
    Rebase,
    Merge,
    Push,
    Fetch,
    Worktree,
    Other,
}

#[derive(Serialize, Deserialize)]
pub enum DaemonToShimResponse {
    Hello {
        daemon_version: String,
        boot_id: u128,
        workspace_id: u64,
        wire_version: u16,
        ledger_generation: u64,
    },
    SessionGranted {
        agent_id: AgentId,
        session_root: PathBuf,
        policy_hash: u64,
    },
    RepositoryResolved {
        repo_id: RepoId,
        worktree_root: PathBuf,
        expected_branch: String,
        base_oid: GitOid,
    },
    GitAuthorized {
        operation: GitOperation,
    },
    GitRejected {
        error: SpError,
    },
    CommitDecision {
        decision: CommitDecision,
    },
    CommitRecorded {
        permit_id: [u8; 16],
        state: IntentState,
    },
    BarrierUpdate {
        barrier_id: BarrierId,
        epoch: u64,
        state: BarrierState,
        ready_count: u16,
        participant_count: u16,
        retry_after_ms: Option<u32>,
    },
    FetchResult {
        repo_id: RepoId,
        old_base: GitOid,
        new_base: GitOid,
        fetch_epoch: u64,
    },
    MergeCheckResult {
        repo_id: RepoId,
        mergeable: bool,
        conflict_paths: Vec<PathBuf>,
        result_tree: Option<GitOid>,
    },
    SequenceReserved {
        claim_id: ClaimId,
        key: SequenceKey,
        expires_ms: UnixMs,
    },
    ChangeSetStatus {
        changeset_id: ChangeSetId,
        state: ChangeSetState,
        published: Vec<RepoId>,
        failed: Vec<RepoId>,
    },
    NoticeBatch {
        notices: Vec<Notice>,
        next_after_seq: u64,
    },
    Status {
        ledger_generation: u64,
        agents: u16,
        repos: u16,
        blocked_intents: u16,
        barrier_state: BarrierState,
    },
    Error {
        error: SpError,
    },
}

#[derive(Serialize, Deserialize)]
pub enum CommitDecision {
    Allowed {
        permit: CommitPermit,
    },
    Blocked {
        conflict: ConflictReport,
    },
    Deferred {
        retry_after_ms: u32,
        queue_position: Option<u16>,
    },
}

#[derive(Serialize, Deserialize)]
pub struct CommitPermit {
    pub permit_id: [u8; 16],
    pub intent_id: IntentId,
    pub repo_id: RepoId,
    pub expected_head: GitOid,
    pub index_tree: GitOid,
    pub claim_fingerprint: [u8; 32],
    pub expires_ms: UnixMs,
    pub nonce: u64,
}

#[derive(Serialize, Deserialize)]
pub struct SpError {
    pub code: SpErrorCode,
    pub message: String,
    pub retryable: bool,
    pub conflict_id: Option<[u8; 16]>,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[repr(u16)]
pub enum SpErrorCode {
    PathOutsideSession = 1001,
    SessionNotAttached = 1002,
    AgentLeaseExpired = 1003,
    SessionIdentityConflict = 1004,

    RepositoryNotRegistered = 2001,
    RepositoryBusy = 2002,
    GitInvocationRejected = 2003,
    StaleBase = 2004,
    FetchFailed = 2005,

    SequenceCollision = 3001,
    SequenceReservationInvalid = 3002,
    SemanticRuleViolation = 3003,
    RuleParseFailure = 3004,

    BarrierTimeout = 4001,
    BarrierAborted = 4002,
    BarrierNotOpen = 4003,

    IntentConflict = 5001,
    CommitPermitInvalid = 5002,
    ChangeSetPartialFailure = 5003,
    CommitPostconditionFailed = 5004,

    StackParentNotReady = 6001,
    PublicationTargetInvalid = 6002,

    ContractViolation = 7001,

    TransportTimeout = 8001,
    ProtocolMismatch = 8002,

    Internal = 9001,
}
```

`SessionCapabilities`, `AgentRuntimeState`, `StatusScope`, `ReleaseReason`, and `AbandonReason` are small policy enums omitted here; they should not contain free-form strings.

### Conflict and resolution types used by both stderr and MCP

```rust
#[derive(Serialize, Deserialize)]
pub struct ConflictReport {
    pub schema: String,                  // "sp.conflict.v1"
    pub conflict_id: [u8; 16],
    pub code: SpErrorCode,
    pub operation: BlockedOperation,
    pub agent_id: AgentId,
    pub agent_label: String,
    pub repo_id: RepoId,
    pub repo_name: String,
    pub branch: String,
    pub base_oid: GitOid,
    pub head_oid: GitOid,
    pub paths: Vec<PathBuf>,
    pub evidence: ConflictEvidence,
    pub resolution: ResolutionPlan,
    pub retry: RetryContract,
}

#[derive(Serialize, Deserialize)]
pub enum BlockedOperation {
    GitCommit,
    GitPush,
    ChangeSetPublish,
    Rebase,
    BarrierArrive,
    BranchCheckout,
}

#[derive(Serialize, Deserialize)]
pub enum ConflictEvidence {
    SequenceCollision {
        rule_id: RuleId,
        rule_name: String,
        namespace: String,
        path: PathBuf,
        line: u32,
        requested: SequenceKey,
        conflicting_claim: ClaimId,
        conflicting_agent: AgentId,
        lowest_free: Option<SequenceKey>,
    },
    MergeConflict {
        paths: Vec<PathBuf>,
        base_oid: GitOid,
        target_oid: GitOid,
        merge_tree_exit: i32,
    },
    StaleBase {
        expected: GitOid,
        observed: GitOid,
        fetch_epoch: u64,
    },
    IntentConflict {
        competing_intent: IntentId,
        competing_agent: AgentId,
        competing_paths: Vec<PathBuf>,
    },
    StackParentNotReady {
        parent_intent: IntentId,
        parent_state: IntentState,
    },
    ChangeSetPartial {
        published_repos: Vec<String>,
        failed_repos: Vec<String>,
    },
    ContractViolation {
        producer: String,
        consumer: String,
        contract: String,
        breaking_change: String,
    },
}

#[derive(Serialize, Deserialize)]
pub struct ResolutionPlan {
    pub summary: String,
    pub steps: Vec<ResolutionStep>,
    pub safe_retry_command: Option<String>,
    pub forbidden_actions: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct ResolutionStep {
    pub ordinal: u8,
    pub action: String,
    pub command: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct RetryContract {
    pub mode: RetryMode,                 // RerunOriginalCommand
    pub idempotent: bool,
    pub retry_after_ms: Option<u32>,
    pub max_attempts: u8,
    pub validation: ValidationMode,      // AutomaticOnRetry
}

#[derive(Serialize, Deserialize)]
pub enum RetryMode {
    RerunOriginalCommand,
    RunSafeCommand,
    WaitForDaemon,
    OperatorRequired,
}

#[derive(Serialize, Deserialize)]
pub enum ValidationMode {
    AutomaticOnRetry,
    RequiresNewCommitPermit,
    RequiresManualReview,
}
```

## 4.5 SequenceRule plugin contract

Rules are compiled into the daemon and registered in a static registry. Do not load dynamic libraries; it adds unsafe ABI, memory, and supply-chain risk.

```rust
pub trait SequenceRule: Send + Sync {
    fn id(&self) -> RuleId;
    fn name(&self) -> &'static str;

    /// Cheap path/glob check before reading blobs.
    fn matches(&self, repo: &RepoView<'_>, path: &RepoPath) -> bool;

    /// Deterministic scan of changed files in base and candidate trees.
    /// Must not perform network, LLM, or environment-dependent work.
    fn scan(&self, ctx: &ScanContext<'_>) -> Result<ScanResult, RuleError>;

    /// Compare observations with ledger claims and base-branch history.
    fn validate(
        &self,
        scan: &ScanResult,
        ledger: &LedgerView<'_>,
        intent: &IntentView<'_>,
    ) -> Result<(), Vec<RuleViolation>>;

    /// Return the lowest legal key after the requested key, or the next
    /// deterministic allocation in this rule's namespace.
    fn allocate(
        &self,
        requested: &SequenceKey,
        ledger: &LedgerView<'_>,
    ) -> Result<SequenceKey, RuleError>;

    /// Optional textual rewrite, expressed as a byte-range edit.
    /// The daemon applies it only when policy explicitly permits rewriting.
    fn rewrite(
        &self,
        source: &[u8],
        observation: &Observation,
        replacement: &SequenceKey,
    ) -> Result<Option<TextEdit>, RuleError>;
}

pub struct ScanContext<'a> {
    pub repo: &'a RepoView<'a>,
    pub base_tree: &'a TreeView<'a>,
    pub candidate_tree: &'a TreeView<'a>,
    pub changed_paths: &'a [RepoPath],
    pub blob_size_limit: usize,
}

pub struct ScanResult {
    pub observations: Vec<Observation>,
    pub malformed: Vec<RuleParseFailure>,
}

pub struct Observation {
    pub rule_id: RuleId,
    pub path: RepoPath,
    pub key: SequenceKey,
    pub byte_span: (u32, u32),
    pub line: u32,
    pub confidence: u8,
}

pub struct RuleViolation {
    pub code: SpErrorCode,
    pub rule_id: RuleId,
    pub path: RepoPath,
    pub observation: Observation,
    pub conflicting_claim: Option<ClaimId>,
    pub suggested_replacement: Option<SequenceKey>,
    pub message: String,
}

pub struct TextEdit {
    pub start: u32,
    pub end: u32,
    pub replacement: Vec<u8>,
}

pub struct RuleError {
    pub code: SpErrorCode,
    pub message: String,
    pub path: Option<RepoPath>,
}
```

Initial built-in rules:

| Rule | Scope | Detection |
|---|---|---|
| `liquibase.changeset_id` | `**/db.changelog*.xml`, configured changelogs | Namespace-aware XML parse of `<changeSet id=...>`; uniqueness per changelog namespace |
| `flyway.versioned_migration` | `**/db/migration/V*__*.sql` | Filename version parser; uniqueness/order per configured migration path |
| `contract.openapi_operation` | configured OpenAPI documents | Operation ID/path/method uniqueness and breaking-change fingerprint |
| `contract.protobuf_symbol` | configured `.proto` roots | Symbol/field compatibility fingerprint |

For Liquibase, do not regex XML if a structured parse is feasible. For Flyway, filename parsing is deterministic and cheap. Keep rule scope configurable per repository.

## 4.6 State machines

### Session/agent

```text
Vacant
  -> Starting          attach request accepted
  -> Active            session grant delivered, lease valid
  -> AwaitingBarrier   barrier arrive accepted
  -> Committing        commit permit issued
  -> Publishing        change set publication started
  -> Completed         session closed normally

Active/AwaitingBarrier/Committing
  -> LeaseExpired      heartbeat deadline passed
  -> Abandoned         supervisor exit or explicit abandon
```

Guards:

- `CommitPermit` expires; an expired permit cannot record a commit.
- A session with an expired lease cannot acquire new intents.
- An abandoned session’s uncommitted claims are released only under the claim-recovery policy; committed/pushed claims become tombstones.

### Intent

```text
Proposed
  -> Staged           immutable index snapshot captured
  -> Validated        merge, sequence, and contract checks passed
  -> Reserved         permits and sequence claims held
  -> Committed        RecordCommit matches permit
  -> Pushed           daemon push succeeded
  -> PrOpen           publication succeeded
  -> Merged           external merge observed
  -> Closed           external close observed

Staged/Validated/Reserved
  -> Blocked          deterministic rule violation
  -> Superseded       base moved or intent replaced by restack
  -> Abandoned        session abandoned
```

### Sequence claim

```text
Free
  -> Reserved         daemon allocates atomically
  -> Committed        commit receipt recorded
  -> Tombstoned       pushed or retained to prevent unsafe reuse
  -> Released         only before commit, with policy permission
```

Never reuse an ID below a repository’s high-water mark unless the policy can prove it is absent from all remote branches and tags.

### Barrier

```text
Closed
  -> Open             participant snapshot fixed, deadline set
  -> Resolving        all required participants ready
  -> Published        publication plan emitted
  -> Closed

Open
  -> Degraded         deadline expired, policy permits partial progress
  -> Aborted          operator or fatal error
```

### Change set

```text
Draft
  -> Staged           all intents committed locally
  -> Validated        all repository checks and contract edges pass
  -> Publishing       push/PR phase begins
  -> PartiallyPublished  one or more operations failed
  -> Published        all remote branches/PRs exist
  -> Merged           merge queue/integration branch completed
```

---

# 5. Passive Conflict Resolution Prompt Specification

## 5.1 Requirements

The prompt must be:

- **Deterministic:** same conflict state produces byte-identical ordering and field names.
- **Bounded:** target 4–8 KiB; never dump a full diff or full file.
- **Machine-readable:** a stable schema and conflict ID.
- **Actionable:** exact file, line, current value, next legal value, and retry command.
- **Non-authoritative:** it does not ask the agent to modify the ledger or bypass checks.
- **Sanitized:** escape control characters, ANSI sequences, newlines in user-controlled names, and truncate untrusted fields. Never include credentials or environment values.
- **Same data for stderr and MCP:** one renderer, two projections.

Exit status for a blocked `git` operation: `1`. Internal daemon/IPC failure: `2`. The blocked commit must not exist.

Do not ask the LLM to reason about a conflict using hidden daemon state. Put the facts in the prompt.

## 5.2 stderr template

The first line is stable and greppable. The final line is a single-line JSON projection for agents that parse machine input. Human-readable actions appear between them.

```text
SP_BLOCKED v1 code=SP_SEQUENCE_COLLISION conflict_id=6c1f2a9e4b7d0f31
agent=agent_2 repo=auth-service branch=sp/agent_2/feat-b operation=git.commit

Your commit was NOT created. A deterministic repository check failed.
This is a coordination conflict, not a transient Git error.

EVIDENCE
  rule: liquibase.changeset_id
  namespace: dbchangelog
  file: src/main/resources/db/dbchangelog.xml
  line: 142
  requested value: 200
  reserved by: agent_1 (intent 17, claim 42, commit 9f3a1c2)
  lowest currently free value: 201

WHY THIS MATTERS
  Another agent has already reserved this sequence value. Committing it
  would produce two changesets with the same semantic sequence number.

RESOLUTION
  1. Change the changeSet id at the location above from 200 to 201.
     If you prefer the daemon to allocate the next value, run:
       sp sequence next --rule liquibase.changeset_id --namespace dbchangelog
  2. Re-stage the edited file:
       git add src/main/resources/db/dbchangelog.xml
  3. Retry the original command:
       git commit -m "<your original message>"
  4. If a new conflict is reported, follow the new conflict ID. Do not
     reuse this conflict ID after a successful retry.

RETRY CONTRACT
  mode: rerun_original_command
  validation: automatic_on_retry
  max_attempts: 3
  idempotent: true

FORBIDDEN
  - Do not use git commit --no-verify.
  - Do not edit, delete, or truncate .scratchpost state.
  - Do not force-push or rewrite another agent's branch.
  - Do not retry the unchanged commit; it will produce the same conflict.
  - Do not ask the user to resolve this unless the retry contract requires it.

SP_CONFLICT_JSON={"schema":"sp.conflict.v1","code":"SP_SEQUENCE_COLLISION","conflict_id":"6c1f2a9e4b7d0f31","operation":"git.commit","agent":{"id":2,"label":"agent_2"},"repository":{"id":3,"name":"auth-service"},"branch":"sp/agent_2/feat-b","evidence":{"kind":"sequence_collision","rule":"liquibase.changeset_id","namespace":"dbchangelog","path":"src/main/resources/db/dbchangelog.xml","line":142,"requested":200,"reserved_by":{"agent":"agent_1","intent_id":17,"claim_id":42},"lowest_free":201},"resolution":{"summary":"Reassign the duplicate sequence value and retry the commit.","steps":[{"ordinal":1,"action":"Change changeSet id 200 to 201.","command":null},{"ordinal":2,"action":"Re-stage the file.","command":"git add src/main/resources/db/dbchangelog.xml"},{"ordinal":3,"action":"Retry the original commit.","command":"git commit -m <original-message>"}],"safe_retry_command":null,"forbidden_actions":["git commit --no-verify","editing .scratchpost state","force-pushing another agent branch"]},"retry":{"mode":"rerun_original_command","idempotent":true,"max_attempts":3,"validation":"automatic_on_retry"}}

Exit status: 1
```

For a merge conflict, the same shell is used with:

```text
EVIDENCE
  kind: merge_conflict
  base: <oid>
  target: <oid>
  conflicting paths: <max 20 paths; "+N more via sp conflict show">

RESOLUTION
  1. Run: sp sync --rebase
     The command refuses if the worktree is dirty or a test lease is held.
  2. Resolve the listed files in your session workspace.
  3. Stage the resolved files and retry the original commit.
```

For a multi-repository partial publication:

```text
SP_BLOCKED v1 code=SP_CHANGESET_PARTIAL_FAILURE ...
published: auth-service@sp/agent_1/feat-a
failed: payment-service (SP_SEQUENCE_COLLISION)
state: partially_published; no PR is marked ready

RESOLUTION
  1. Resolve the payment-service conflict using conflict ID ...
  2. Run: sp publish --changeset 9 --resume
  3. Do not manually delete the published auth-service branch.
```

## 5.3 MCP response

For MCP, return a tool result with `isError: true`. Do not return a JSON-RPC protocol error for a normal conflict; the agent must see structured data, not a transport failure.

```json
{
  "isError": true,
  "content": [
    {
      "type": "text",
      "text": "SP_BLOCKED ... [same human-readable prompt]"
    }
  ],
  "structuredContent": {
    "schema": "sp.conflict.v1",
    "code": "SP_SEQUENCE_COLLISION",
    "conflictId": "6c1f2a9e4b7d0f31",
    "operation": "git.commit",
    "repository": {
      "id": 3,
      "name": "auth-service"
    },
    "agent": {
      "id": 2,
      "label": "agent_2"
    },
    "evidence": {
      "kind": "sequence_collision",
      "rule": "liquibase.changeset_id",
      "path": "src/main/resources/db/dbchangelog.xml",
      "line": 142,
      "requested": 200,
      "lowestFree": 201
    },
    "resolution": {
      "summary": "Reassign the duplicate sequence value and retry the commit.",
      "steps": [
        {
          "ordinal": 1,
          "action": "Change changeSet id 200 to 201."
        },
        {
          "ordinal": 2,
          "action": "Re-stage the file.",
          "command": "git add src/main/resources/db/dbchangelog.xml"
        },
        {
          "ordinal": 3,
          "action": "Retry the original commit."
        }
      ],
      "retry": {
        "mode": "rerun_original_command",
        "idempotent": true,
        "maxAttempts": 3,
        "validation": "automatic_on_retry"
      }
    }
  }
}
```

Every MCP response, including successful ones, should include pending notices:

```json
{
  "notices": [
    {
      "seq": 1042,
      "severity": "warning",
      "code": "SP_MUTATION_BROADCAST",
      "message": "agent_1 published auth-service migration sequence 200.",
      "repository": "auth-service",
      "createdAt": "2026-11-15T00:31:12Z"
    }
  ]
}
```

If the MCP client does not support `structuredContent`, include the same JSON in the text content. Do not depend on unsolicited server notifications.

## 5.4 Prompt safety rules

- Paths and branch names from another agent are untrusted input. JSON-escape them and cap lengths.
- Strip ANSI escapes, carriage returns, and control characters before rendering.
- Never include raw file contents, diff bodies, environment variables, tokens, or remote URLs containing credentials.
- Commands shown to the agent must come from fixed templates. Do not interpolate unquoted paths into shell strings. If a path is shown as a command argument, shell-quote it correctly.
- Never say “ignore previous instructions” or otherwise embed policy text from repository files.
- Do not tell the agent to modify `.scratchpost`, disable hooks, use `--no-verify`, force-push, or delete a worktree.
- A blocked operation is idempotent: retrying without changes returns the same conflict ID or a newer one. It never partially creates a commit.

---

# 6. Concrete changes I would make before implementation

1. **Move isolation to the filesystem/session layer.** Master folder is canonical and daemon-owned. Agents run only in a session workspace. The Git shim is a control-plane client, not a sandbox.
2. **Make the daemon the sole writer for fetches, refs, pushes, GC, and worktree administration.** Per-agent Git maintenance must be disabled.
3. **Replace the global barrier with per-conflict-component coordination.** Use the mergeability graph; degrade timed-out components to FCFS rather than blocking unrelated work.
4. **Implement commit permits bound to immutable index snapshots.** Validate before commit and verify after commit; otherwise semantic checks are advisory only.
5. **Make publication a change-set transaction with two-phase behavior.** No partial “ready” PRs across repositories; explicit `PartiallyPublished` recovery.
6. **Do not auto-rewrite sequence IDs after commits.** Reserve before commit or block. Prefer integration branches/merge queues over mutable stacked PR branches.
7. **Treat `.scratchpost/bulletin.md` as a generated view.** The append-only notice log and ledger are authoritative.
8. **Define the memory budget by measurement.** Target ≤17 MB idle PSS for daemon plus five MCP bridges; target ≤25 MB during representative commit activity. Drop Tokio, persistent watchers, and resident TLS to make that credible.

This keeps the system deterministic, auditable, and within the stated memory envelope without pretending a PATH shim is a security or isolation boundary.
