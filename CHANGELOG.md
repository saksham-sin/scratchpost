# Changelog

All notable changes to ScratchPost are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project adheres
to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] — 2026-11-16

First public release. ScratchPost is a single-binary Rust workspace coordinator
for parallel coding agents.

### Added

**Isolation**

- Per-agent physical session workspaces under
  `.scratchpost/sessions/<agent>/workspace/`, each with isolated `git worktree`
  instances sharing one object database (`gc.auto=0`).
- Copy-on-write root context files (`clonefile` on APFS, `FICLONE` on Linux,
  hardlink fallback).
- Deterministic resource bands per session: `SP_PORT_BASE`, `SERVER_PORT`,
  `COMPOSE_PROJECT_NAME`, `TMPDIR`, and per-session `MAVEN_OPTS`,
  `GRADLE_USER_HOME`, `npm_config_cache`, `CARGO_TARGET_DIR`.

**The commit gate**

- TOCTOU-safe gate: the shim snapshots the index, the daemon validates the
  immutable tree, and the issued `CommitPermit` is bound to that exact tree.
- Managed `core.hooksPath` re-checks inside Git, so a direct call to the real
  `git` binary is still rejected.
- Deterministic Liquibase `<changeSet id>` and Flyway `V<n>__` sequence
  extraction and allocation.
- `SP_BLOCKED v1` stderr contract with an `EVIDENCE` block, copy-pasteable
  `RESOLUTION` steps, and a single-line `SP_CONFLICT_JSON={...}` footer
  (`sp.conflict.v1`).

**Coordination**

- Three modes: `fcfs`, `barrier-stacked`, `barrier-integration`.
- Barrier scope `workspace` or `component`, so orthogonal agents never wait on
  each other.
- Advisory and mandatory path claims with TTL.
- Lease expiry and operator eviction for abandoned sessions.
- Mutation broadcast to peers on every commit.

**Interfaces**

- `sp` CLI: `init`, `daemon`, `shell`, `run`, `status`, `claims`, `claim`,
  `notices`, `instructions`, `config`, `barrier`, `sequence`, `resequence`,
  `sync`, `evict`, `ui`, `--mcp`.
- `sp --mcp`: MCP JSON-RPC over stdio with `sp_status`, `sp_claim`,
  `sp_barrier_arrive`, `sp_commit`, `sp_notices`, `sp_sequence_next`,
  `sp_resequence`, `sp_sync`, `sp_config_set`, `sp_evict`. Blocked tools return
  `isError: true` with the identical conflict contract in `structuredContent`.
- `sp ui`: embedded Mission Control dashboard over HTTP + SSE on
  `127.0.0.1:19200`. No external assets, no CDN.

**Agent-first scaffolding**

- `sp init` generates `scratchpost-agent-prompt.md` (paste once into any agent
  chat), `AGENT_INSTRUCTIONS.md`, `.cursorrules`,
  `.github/copilot-instructions.md`, and `.claude/skills/scratchpost/`.
- `RUNNING_REPORT.md`: a live audit ledger the daemon rewrites on every
  coordination action, so a developer can see what happened while they were away.

**Distribution**

- `install.sh`: one-line installer with OS/arch detection, binary magic-byte
  verification, and `/usr/local/bin` → `~/.local/bin` fallback.

### Performance

- Stripped release binary: 889,536 bytes (869 KB).
- Daemon RSS: ~3.6 MB idle, ~4.5 MB with ten concurrent agents.
- Seqlock mmap status reads: ~2.9 ms.
- Zero external LLM API calls.

### Known limitations

- The commit gate adds ~105 ms p50 to a commit (atomic subprocess plumbing and
  WAL fsync). This is above the original 50 ms target and is documented honestly
  in [DEMO_REPORT.md](DEMO_REPORT.md).
- `git commit --no-verify` bypasses hooks, as with any user-level Git hook. Run
  agents inside `sp shell`.
- No kernel-level sandboxing. ScratchPost coordinates cooperating agents; it does
  not contain hostile ones.

[Unreleased]: https://github.com/saksham-sin/scratchpost/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/saksham-sin/scratchpost/releases/tag/v0.1.0
