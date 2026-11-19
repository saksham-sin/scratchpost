# Changelog

All notable changes to ScratchPost are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project adheres
to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] — 2026-11-18

Reliability release driven by two production agents that ran ScratchPost
against a real multi-service workspace. The headline fix is that a lease
expiring mid-task no longer destroys uncommitted work.

### Added

- **The installer verifies the release checksum.** It downloads the published
  `.sha256` alongside the binary and refuses to install on a mismatch, so a
  corrupt or tampered download cannot reach `$PATH`. `SP_REQUIRE_CHECKSUM=1`
  additionally refuses releases that publish no checksum. Falls back to
  `sha256sum`, `shasum`, or `openssl`.

### Fixed

- **Re-attaching no longer destroys uncommitted work.** A lease can expire while
  an agent is still mid-task. `sp run` used to re-provision the session, wiping
  the workspace and minting a new branch and session id. An expired session whose
  workspace is still on disk is now revived in place, so the branch, port band
  and session id stay stable across re-attaches. A `session-revived` notice is
  recorded when the workspace still holds unpreserved work.
- **`sp evict` now reclaims disk and unregisters worktrees.** It previously only
  marked the session abandoned, leaving the workspace and its `git worktree`
  registrations behind; those stale registrations later blocked
  `git worktree add` and `gh pr checkout` for other agents. Eviction now removes
  the worktrees, deletes the session directory, and reports `reclaimed_bytes`.
- **Single-repository workspaces unregister their worktree on teardown.** The
  `relative_path == "."` case was skipped, leaving a prunable registration.
- **Gitignored local configuration is copied into new worktrees.** A worktree
  only materialises tracked files, so `application-dev.yaml`, `.env` and local
  keystores were absent and services failed with errors that looked like code
  bugs (`UnknownHostException: ${DB_HOST}`). Ignored files matching a narrow
  config heuristic are now copied in, and the list is written to
  `local-config.txt` in the session directory.
- **Compose files that pin `container_name:` are reported at attach time.**
  `COMPOSE_PROJECT_NAME` cannot isolate containers pinned by name, so two
  sessions silently share the same database. The daemon now emits a
  `compose-isolation-degraded` warning naming the offending files and containers.
- **`sp status` reports lease state.** Sessions now carry `lease_remaining_ms`
  and `has_unpreserved_work`, and the human view shows the remaining lease and
  flags unpreserved work, so "idle" is distinguishable from "has work in flight".
- **Actionable errors for unattached sessions and bypassed shims.**
  `SP_SESSION_NOT_ATTACHED` now names the `sp run` invocation that fixes it, and
  `SP_GIT_INVOCATION_REJECTED` on push explains that a login shell (`bash -lc`)
  reset `PATH` past the managed git shim.

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

- Stripped release binary: 889,536 bytes (869 KB) on macOS arm64, ~1.13 MB on
  Linux x86_64 (glibc links more than libSystem).
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
- **Windows is not supported.** The source does not compile for
  `x86_64-pc-windows-msvc`; the IPC layer, daemon lock, token permissions, and
  `git` shim all use Unix-only APIs. macOS and Linux only. See
  [README.md § Platform support](README.md#platform-support).

[Unreleased]: https://github.com/saksham-sin/scratchpost/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/saksham-sin/scratchpost/releases/tag/v0.2.0
[0.1.0]: https://github.com/saksham-sin/scratchpost/releases/tag/v0.1.0
