# Spall progress and PR audit — 2026-09-09

Reviewed Loopira's 26 returned work-log entries and 54 project tickets, GitHub open/merged PR metadata, the open PR source and tests, and the merged Vulkan diagnostic diff. This is a focused review, not full engine acceptance. No implementation or PR branch was changed.

## Open PRs

- [#53](https://github.com/deranjer/spall/pull/53), T13, head `88845c460fff0acb296d509b1654c93b6cf481c6`: both hosted foundation checks pass; GitHub reports CONFLICTING. Resolve against current main and rerun checks. The decision document usefully separates single-capture D3D12 timing from G2 percentiles and admits omitted sub-0.5 m wall leakage. However, its GPU test renders only the open room and checks a broad bright-pixel count; closed/open and wall-leak assertions use the CPU trace copy. It does not validate those outcomes through GPU denoising and surface sampling. Add rendered non-emissive receiver comparisons/controls before treating GPU quality as accepted. The decision's 'accepted' wording is premature while review remains pending. No T13 completion log was present in the returned log history.
- [#54](https://github.com/deranjer/spall/pull/54), T22 increment 1, head `5e61c6fdac5ac0a7b634608c27ca69a99eabab94`: both hosted foundation checks pass; mergeable, no submitted reviews. Scope separation is appropriate, but the public canonical damage API has a correctness gap: `BrokenBond` exposes public endpoints, while `DamageState::from_bonds` and `insert` trust their ordering. A reversed struct literal is accepted, hashes differently, and is not found by `contains`, which normalizes the query. Normalize/validate bonds at ingestion or make invalid construction impossible, with a regression for reversed raw endpoints. Existing tests call `BrokenBond::new`, so do not cover this. Spec approval remains outstanding. Refeeding an in-memory report proves evaluation idempotence, not a disk restart: DTOs, persistence, replication hashes and simulation integration remain increment 2. The log's 'reviewed-spec' wording and 'restart met' claim should be read with these qualifications. Workspace-flake cause attribution is unproven by passing isolation alone.

## Vulkan reporting

[#52](https://github.com/deranjer/spall/pull/52) merged; ENG-60 correctly remains backlog. Updated the ticket title and appended a reporting correction:

- Observed native failure is during T12 pipeline creation on recorded Windows/NVIDIA/wgpu 24 configuration, before draws. Exact root cause remains unresolved. D3D12 success and naga validation do not prove the application/translation layers innocent; upgrade is an experiment, not a verified fix.
- The crash watch checks only failure of `cargo run`, so build/capability errors and ordinary panics can pass it. Require a separately built, bounded child run and exact native status/last-marker verification.
- Four reported D3D12 test passes include one inactive watch returning early; distinguish three exercised GPU tests from that watch. Vulkan remains unaccepted; D3D12 is the measured Windows path.
- These corrections qualify the stronger claims in the permanent diagnosis log and merged report; the code/report themselves were not modified in this audit.

## Progress and tracking

Loopira reports 68% across 54 tickets, including bugs; this is not engine or gate readiness. Fourteen of the 26 planned tasks are marked done: T00–T10, T12, T16 and T17. T11, T13 and T22 remain in progress; nine planned tasks remain backlog. G1 has no complete acceptance, and later G2–G5 gates remain outstanding.

The earlier G1 success log is qualified by the subsequent failed reproduction and coverage review. Merging #50 does not establish resolution of that recorded Rapier panic, scripted-run truncation, cross-brick coverage or moving-body recut gaps. Keep ENG-18 open until new evidence addresses them.

ENG-41, ENG-42, ENG-50 and ENG-56 still say in_progress although their fixes merged as #47, #48, #37 and #49 respectively. Reconcile acceptance evidence before closing; this audit did not rerun those full suites. Older T06 'feasibility PASSED' and collision-inflation claims are historical and must be read alongside the later exact-collision/measurement repairs. The project metadata still says planned; the initial guide's specification-only text and docs/tasks.md's 'Every task is unstarted' are stale. Added a superseding Loopira guide fragment for current-state interpretation.

## Checks and limitations

- `cargo test -p spall_structure` at #54 head: PASS, 35 unit/scenario tests; 0 doc tests.
- `cargo test -p spall_render --lib` in `.local/worktrees/eng-20` at #53 head: PASS, 19 CPU tests.
- `git diff HEAD --check`: PASS for tracked current-checkout changes.
- `gh pr list --state open ...`, `gh pr view 50/52/53/54 ...`, `gh pr list --state merged ...`, `gh pr diff 52 --patch`: read current states, CI and diagnostic changes; final open heads unchanged.
- GPU runs, native crash reproduction, full workspace suite, integration-conflict resolution and performance gates were not run. GPU numbers quoted in PRs remain prior author measurements, not fresh measurements here.

Next priority is T11 / ENG-18 acceptance repair. Independently dependency-ready planned work includes T18 and T19; T13 review and T22 increment 2 remain unfinished. T14 depends on accepted T13; T20 depends on T18/T19. T21's listed dependency includes T18, so T22 increment 2 alone does not unblock it.
