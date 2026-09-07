# Implementation agent contract

This repository is a plan for one custom voxel game engine. Read README.md, docs/architecture.md, docs/protocol.md, docs/validation.md, and your assigned task in docs/tasks.md before implementing.

## Spall and worker coordination

- Engine name: Spall. Engine libraries use `spall_*` in `crates/`; package `sandbox` in `examples/sandbox` provides the shipped game example and thin server/client binaries. Engine libraries never depend on game packages. See README.md for the boundary.
- The coordinator may delegate up to three independent ready assignments to GPT-5.6 Terra workers with High reasoning, while retaining architecture and integration review. Use explicit ticket context and a dedicated Git worktree per implementation ticket.
- Respect the dependency graph: T00 and then T01 precede useful broader fan-out. Do not start dependent implementation early merely to keep slots occupied.
- The coordinator owns Loopira status/log updates unless an assignment explicitly delegates them. Workers return evidence and corrections; only the coordinator marks a ticket done after review/integration and required checks.
- Keep reviewable commits and do not push or merge into the main checkout from a worker. The coordinator handles integration. No automatic publication is required by this guide.

## Loopira

Project and work tracking for this engine lives in Loopira (project "Voxel Engine"), reachable through the Loopira MCP server. Every implementation session must:

- **Check the agent guide** — call `get_project_guide` at the start of the session for stack, conventions, and guardrails.
- **Pull the ticket** — `list_issues` / `get_issue`; work tickets are `T00`–`T25` and mirror `docs/tasks.md`. Take one task ID per pass and respect its listed dependencies.
- **Track status** — `update_issue_status` when you start and when you finish a ticket.
- **Log work** — after finishing a task or session, `add_work_log` on the project with changed files, exact checks run, results (measured vs. target), remaining risks, and the next unblocked task ID. Work-log entries are permanent.

If a ticket and the repo docs disagree, resolve it explicitly and update both; do not silently diverge.

- Full-world destruction and multiplayer are user requirements. Never downgrade them to destructible props or single-player-first architecture.
- Implement the assigned task and its prerequisites only. If a prerequisite is absent, report the missing task rather than inventing a competing interface.
- The architecture describes target behavior, not existing code. Do not claim a proposed command or test already works.
- Keep server authority, voxel/body ownership, revision validation, transaction ordering, and durable recovery intact.
- Keep dependencies acyclic. No GPU/window/network runtime dependencies in voxel algorithms or core simulation.
- No individual voxel entities, per-voxel rigid bodies, global mutable singletons, or a second physics engine.
- Workers receive immutable snapshots. Only the owning thread applies validated results at a tick boundary.
- Use stable IDs and explicit versioned DTOs. Never persist runtime ECS IDs, library handles, Rust enum layout, or native-endian struct bytes.
- Every behavioral change needs the relevant invariant/scenario tests from the assigned task. Do not add superficial tests that merely duplicate implementation.
- Keep fixtures small enough for CPU CI. GPU capture and hardware performance gates are separate and must be reported as unrun if unavailable.
- Preserve existing work. Do not rewrite shared interfaces or dependency versions as a convenience; explain necessary contract changes and update callers, docs, and fixtures together.
- Do not bypass a failed feasibility gate with placeholder physics, approximate topology, hidden anchors, or a reduced workload reported under the original scenario name.
- No editor/UI framework, platform services, or additional game systems unless assigned.
- Use structured logs, bounded runs, and the xtask interfaces once implemented. Clean up only processes/resources launched by that run.
- Finish with changed files, exact checks run, results, remaining risks, and the next unblocked task ID. Distinguish measured results from targets.

No special approval step is imposed for ordinary authorized implementation. Gate failures require evidence and an integration decision; agents should finish independent, in-scope work and report the precise blocker.
