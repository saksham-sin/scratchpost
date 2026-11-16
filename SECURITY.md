# Security Policy

## Supported versions

ScratchPost is pre-1.0. Security fixes land on `main` and in the latest release
only.

| Version | Supported |
|---|---|
| `0.1.x` | yes |
| older | no |

## Reporting a vulnerability

Please **do not open a public issue** for a security problem. Use GitHub's
private reporting:

<https://github.com/saksham-sin/scratchpost/security/advisories/new>

Include the version, platform, a description of the impact, and a reproduction if
you have one. Expect an acknowledgement within a few days.

## Threat model

ScratchPost is a **coordination** tool, not a sandbox. It is designed to stop
cooperating agents from corrupting each other's work by accident. It is not
designed to contain a hostile process running as the same user.

### What ScratchPost defends against

| Threat | Defence |
|---|---|
| An agent commits a duplicate migration sequence | Deterministic rule engines + seqlock sequence registry; commit blocked with a repair contract |
| An agent commits onto another agent's branch | Physical worktrees; there is no shared `HEAD` |
| An agent's index changes between validation and commit (TOCTOU) | Immutable index snapshot + permit bound to the validated tree |
| An agent bypasses the shim by calling the real `git` binary | Managed `core.hooksPath` re-checks inside Git |
| Two agents bind the same test port | Disjoint `SP_PORT_BASE` bands per session |
| A crashed agent blocks its peers forever | Lease expiry and operator eviction |

### What ScratchPost does **not** defend against

- **A hostile process running as the same user.** It can read the capability
  token in `.scratchpost/run/token`, write to the socket, or edit the ledger
  directly. Run untrusted code in a container or VM.
- **`git commit --no-verify`.** As with any user-level Git hook, an agent that
  invokes the real Git binary with `--no-verify` bypasses hooks. Run agents
  inside `sp shell` so the shim is authoritative on `$PATH`.
- **Kernel-level isolation.** ScratchPost intentionally does not attempt
  namespaces, seccomp, or cgroups.
- **Malicious rule content.** The Liquibase and Flyway parsers read repository
  files. A pathological file could be slow to parse; it cannot execute code.

## Hardening notes

- The capability token is written with mode `0600`.
- The Unix socket lives at `/tmp/sp/<uid>/<hash>.sock`, inside a per-uid
  directory, so another user cannot reach it.
- The daemon never makes network calls. `sp ui` binds `127.0.0.1` only.
- The daemon never calls an external LLM API. There is no outbound traffic to
  audit.
- Git subprocesses are invoked with an explicit argument vector; no shell is
  involved, so repository paths cannot inject commands.

## Dependency policy

The dependency set is deliberately small (`libc`, `memmap2`, `quick-xml`,
`serde`, `serde_json`). New dependencies need a justification in the pull
request. `cargo audit` findings are treated as bugs.
