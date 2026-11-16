# Contributing to ScratchPost

Thanks for looking. ScratchPost is a small, dependency-light Rust project; the
bar for a change is that it keeps the daemon under its memory budget and keeps
the commit gate deterministic.

## Before you start

```sh
git clone https://github.com/saksham-sin/scratchpost
cd scratchpost
cargo build --release
cargo test
bash scripts/verify-e2e.sh     # prints "E2E PASS"
```

Prerequisites: a Rust toolchain (stable, 1.75+), `git`, and `python3` for the
simulation scripts. No Node, no network access, no external services.

## What CI checks

Every pull request runs, on Linux and macOS:

| Check | Command |
|---|---|
| Formatting | `cargo fmt --all --check` |
| Lints | `cargo clippy --all-targets -- -D warnings` |
| Build | `cargo build --release --locked` |
| Unit tests | `cargo test --locked` |
| End-to-end | `bash scripts/verify-e2e.sh` |
| Binary budget | release binary < 1 MB |
| Stress | `bash scripts/ten-agent-sim.sh` |

Run the same commands locally before pushing. `RUSTFLAGS=-D warnings` is set in
CI, so a warning is a failure.

## Ground rules

**The daemon never calls an LLM.** Conflict detection and sequence allocation are
deterministic Rust. If a change would require a model call to decide whether a
commit is safe, the design is wrong.

**Machine-readable output is a contract.** `SP_BLOCKED v1`, `SP_CONFLICT_JSON=`,
`lowest_free:`, and `discovered N repository(ies)` are parsed by agents and
scripts. They must stay byte-identical. Human-facing output may change freely;
machine-facing output may not, without a schema version bump.

**The memory budget is a feature.** Idle RSS must stay under 15 MB with several
agents attached. Avoid unbounded caches, per-connection threads, and buffering
whole trees in memory.

**State is written only by the daemon.** Clients send RPCs. If you find yourself
writing to `.scratchpost/` from the CLI or the shim, stop.

**Never weaken the gate to make a test pass.** If a test fails, the gate is
probably right and the test fixture is wrong.

## Adding a rule engine

Sequence rules live in `src/rules.rs`. A rule needs to:

1. Recognise its file type (path or content heuristic).
2. Extract the sequence key from a blob.
3. Report the byte offset so the conflict prompt can point at a line.

Add a unit test with a real fixture, and extend `scripts/ten-agent-sim.sh` with a
collision scenario so the end-to-end path is covered.

## Adding an MCP tool

Tools live in `src/mcp.rs`. A tool must return the same `sp.conflict.v1` contract
as the CLI when it blocks, with `isError: true` and the payload in
`structuredContent`. Do not invent a second error shape.

## Commit messages

Conventional commits, imperative mood, one logical change per commit:

```text
feat(rules): recognise Flyway repeatable migrations
fix(shim): reject commits when the index changes mid-gate
docs(readme): document the ten-agent simulation
```

## Reporting bugs

Include the ScratchPost version (`sp --version`), the platform, the exact command,
and the full `SP_BLOCKED` output if the gate was involved. The
`SP_CONFLICT_JSON` line is usually enough to reproduce the decision.

## License

By contributing you agree that your contribution is licensed under the
[Apache License 2.0](LICENSE).
