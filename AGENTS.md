# Implementation agent contract

This repository holds the Spall voxel engine and its sandbox game. Read README.md, docs/architecture.md, docs/protocol.md, docs/validation.md, and your assigned ticket before implementing. Then read **Proportionality** below: it overrides any habit of building more than the ticket needs.

## Spall and worker coordination

- Engine name: Spall. Engine libraries use `spall_*` in `crates/`; package `sandbox` in `examples/sandbox` provides the shipped game example and thin server/client binaries. Engine libraries never depend on game packages. See README.md for the boundary.
- The coordinator may delegate independent ready assignments to worker sessions while keeping architecture and integration review. Give each implementation ticket explicit context and its own Git worktree, and give no two parallel workers the same files.
- Respect the dependency graph. Do not start dependent implementation early merely to keep slots occupied.
- The coordinator owns Loopira status/log updates unless an assignment explicitly delegates them. Workers return evidence and corrections; only the coordinator marks a ticket done after review/integration and required checks.
- Keep reviewable commits and do not push or merge into the main checkout from a worker. The coordinator handles integration. No automatic publication is required by this guide.

## Loopira

Project and work tracking for this engine lives in Loopira (project "Voxel Engine"), reachable through the Loopira MCP server. Every implementation session must:

- **Check the agent guide.** Call `get_project_guide` at the start of the session for stack, conventions, and guardrails.
- **Pull the ticket.** Use `list_issues` / `get_issue`. Work tickets are `ENG-*` (the original `T00`–`T25` plan lives in `docs/tasks.md`). Take one ticket per pass and respect its listed dependencies.
- **Track status.** Call `update_issue_status` when you start and when you finish a ticket.
- **Log work.** After finishing a task or session, call `add_work_log` on the project with changed files, exact checks run, results (measured vs. target), remaining risks, and the next unblocked ticket. Work-log entries are permanent.

If a ticket and the repo docs disagree, resolve it explicitly and update both; do not silently diverge.

## Engine invariants

- Full-world destruction and multiplayer are user requirements. Never downgrade them to destructible props or single-player-first architecture.
- Implement the assigned task and its prerequisites only. If a prerequisite is absent, report the missing task rather than inventing a competing interface.
- Do not claim a proposed command or test already works. Planned behavior is labeled as planned.
- Keep server authority, voxel/body ownership, per-brick revision validation, transaction ordering, and durable recovery intact.
- Keep dependencies acyclic. No GPU/window/network runtime dependencies in voxel algorithms or core simulation.
- No individual voxel entities, per-voxel rigid bodies, global mutable singletons, or a second physics engine.
- Workers receive immutable snapshots. Only the owning thread applies validated results at a tick boundary.
- Use stable IDs and explicit DTOs. Never persist runtime ECS IDs, library handles, Rust enum layout, or native-endian struct bytes. Versioning follows **Proportionality** rule 3.
- Every behavioral change needs tests for the invariant it changes (see **Proportionality** rule 6). Do not add superficial tests that merely duplicate implementation.
- Keep fixtures small enough for CPU CI. GPU capture and hardware performance gates are separate and must be reported as unrun if unavailable.
- Preserve existing work. Do not rewrite shared interfaces or dependency versions as a convenience; explain necessary contract changes and update callers, docs, and fixtures together.
- Do not bypass a failed feasibility gate with placeholder physics, approximate topology, hidden anchors, or a reduced workload reported under the original scenario name. A *declared* approximation, named as such in the ticket and docs, is not a bypass.
- No editor/UI framework, platform services, or additional game systems unless assigned.
- Use structured logs, bounded runs, and the xtask interfaces. Clean up only processes/resources launched by that run.

## Proportionality

The 2026-10-20 audit (`docs/reviews/2026-10-20-overengineering-audit.md`) found that most accidental complexity came from building to standards the engine does not need. These rules are defaults. Breaking one needs a stated reason in the ticket or the final report, and for rules 1–3, user approval.

1. **Build for a caller that exists.** Do not add a public API, module, crate, cargo feature, config field, or protocol record unless non-test code in the same change uses it. Infrastructure for a later milestone (more physics regions, future persistence, future creature behavior) needs explicit user approval first, or it waits. Deleting unused code is a valid, encouraged change, and git history is the archive.
2. **Exact only where authority needs it.** The server is the only authority, and physics is not lockstep. Required exactness is: same per-brick `(revision, content hash)` on server and client for bricks a transaction touches, and same-binary determinism where a replay test depends on it. Do not require cross-machine bit-identical math, integer-only algorithms, canonical ordering, or world-scale hashes unless the value is persisted, hashed, or compared across the wire. Default to `HashMap`/`HashSet` and `f32`/`f64`. Use `BTreeMap` or canonical order only when order is observable.
3. **Pre-release versioning is two numbers.** Until the user declares a public release, there is one wire version (`spall_protocol::PROTOCOL_VERSION`) and one save version (`spall_store::STORE_SCHEMA_VERSION`). Do not add per-record schema constants, new hash-domain strings, legacy decoders, migration paths, or "frozen / never edit" files. A breaking change bumps the relevant number once, and an older save or peer fails with a clear error. On-disk asset formats (`.spvox`) are the exception and keep their own version.
4. **Bound untrusted input; trust your own.** Strict count/byte limits with preflight belong on client→server records and imported asset files. Server→client payloads, the server's own save DB, and in-process handoffs get coarse caps at most. Put limits in `spall_protocol::limits` (or the asset crate), not as scattered per-module `MAX_*` constants.
5. **Scaffolding stays out of production paths.** Fixtures, fault injection, loss proxies, render capture/evidence tooling, and benchmarks live behind a cargo feature, in a test crate, or in xtask. Production config structs do not grow test-run fields (tick limits, quiescence, fault plans, evidence output paths), and engine enums do not list gate scenes. Put those in the harness config.
6. **Tests follow risk, not ceremony.** Test the invariant the ticket changes, at the cheapest level that proves it. Do not encode review transcripts as `*_review.rs` files. Add a differential oracle only when the algorithm itself is the risk. Do not add a new scenario fixture when an existing one covers the case.
7. **Keep files reviewable.** Do not let a non-test source file grow past about 2,000 lines. If you need to add to a file already over that, split it (moves only) in a separate commit first.
8. **One concurrency story.** CPU parallelism uses rayon or the existing job pool, and async I/O uses tokio inside net/server/client. Do not add another thread pool, scheduler, lane system, or ad-hoc `std::thread` without a measurement showing the existing ones are insufficient. Run cheap work inline before reaching for optimistic concurrency and retries.
9. **Docs describe the present.** `README.md`, `docs/architecture.md`, and `docs/validation.md` state current behavior. Dated history and superseded experiments go under `docs/reports/`. Commit a short summary report for evidence; keep raw evidence (`*.jsonl`, `*.log`, capture dumps) out of git (see `.gitignore`).
10. **Formal gates are for research risk.** Gate rigor (G0–G5, frozen workloads, measured-vs-target evidence) applies to destruction + replication, lighting, persistence/streaming, and the multiplayer slice. Content features (weather, audio, snow, vegetation looks, animal visuals) are ordinary tickets with ordinary tests and a playtest note.

## Finishing a ticket

Finish with changed files, exact checks run, results, remaining risks, and the next unblocked ticket. Distinguish measured results from targets. Also list:

- anything added under rule 1 (new public API, module, feature, config field, record) and its non-test caller;
- any version bump (rule 3);
- any proportionality rule you broke, and why.

`cargo xtask check` enforces Proportionality rules 3, 7 and 9 mechanically (and warns on rule 5) via `tools/xtask/proportionality-allowlist.txt`. That allowlist may only shrink or have entries removed; adding to it needs user approval.

No special approval step is imposed for ordinary authorized implementation. Gate failures require evidence and an integration decision; agents should finish independent, in-scope work and report the precise blocker.
