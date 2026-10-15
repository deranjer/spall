# ENG-209 actual-animal collision workload

## Result

The bounded actual-asset probe now passes its exact source-cell checks for both collider representations. It decodes the checked-in trout, crow, and deer SPVX assets and sends their authored part grids and sampled articulated transforms through the same `spall_physics::PhysicsWorld` Rapier adapter used by the engine. It does not add a second solver or make `spall_physics` depend on the sandbox.

Each asset is tested as one articulated body with all parts present, including hidden animation parts. The workloads contain 136 trout cells across 5 parts, 217 crow cells across 6 parts, and 285 deer cells across 9 parts. Every part has negative-origin coordinates. Grid cell size is the authored 6.25 cm; authored pivots are converted once into assembly-root translation. The occupancy-only probe makes no material-catalog physics claim.

The probe samples both clips per animal at 0%, 25%, 50%, 75%, and 100% of clip duration. For each pose it independently intersects rays from every occupied cell center along three orthogonal axes against the union of exact transformed source-cell cubes, plus a guaranteed miss ray. This oracle does not invoke the production hit mapper. It also removes and restores one source cell per part and checks rays against the changed cell union. Across both representations, this yields 38,520 ray checks with zero mismatches and 66 guaranteed miss checks with zero false hits. All six timed matrices also report zero ray misses.

## Bounded release measurements

The table reports microseconds per operation. `pose + rays + step` includes one pose update, one ray per part (including query refresh), and one fixed physics step. `grid rebuild` measures one source-part grid replacement; the measured sequence removes and restores cells on the live assembly. `initial attach` measures attachment in a fresh world. Initial attachment and pose timing each have 10 warmups and 100 measured samples. Grid replacement has 10 warmup removal/restoration pairs and 200 measured operations (one removal and one restoration per measured iteration). Percentiles are descriptive single-assembly diagnostics only; there is no accepted population or performance target for these measurements.

| Animal | Representation | Cells / parts | Colliders / primitives | Initial attach p50 / p95 / p99 | Grid rebuild p50 / p95 / p99 | Pose + rays + step p50 / p95 / p99 |
|---|---|---:|---:|---:|---:|---:|
| Trout | NativeVoxels | 136 / 5 | 5 / 5 | 19.6 / 20.7 / 24.3 | 2.9 / 7.9 / 8.0 | 6.3 / 6.6 / 8.6 |
| Trout | MergedCuboids | 136 / 5 | 5 / 17 | 5.9 / 7.0 / 9.2 | 1.2 / 2.3 / 2.4 | 5.8 / 6.1 / 6.3 |
| Crow | NativeVoxels | 217 / 6 | 6 / 6 | 23.1 / 24.5 / 27.8 | 3.8 / 5.0 / 5.1 | 7.6 / 7.9 / 9.1 |
| Crow | MergedCuboids | 217 / 6 | 6 / 14 | 6.3 / 6.6 / 9.1 | 1.3 / 1.6 / 1.7 | 6.7 / 7.1 / 9.3 |
| Deer | NativeVoxels | 285 / 9 | 9 / 9 | 33.8 / 34.3 / 42.1 | 3.1 / 9.6 / 9.8 | 11.9 / 12.6 / 15.8 |
| Deer | MergedCuboids | 285 / 9 | 9 / 21 | 9.3 / 10.5 / 17.4 | 1.0 / 2.1 / 2.2 | 10.4 / 11.0 / 13.0 |

These values describe this machine and workload, not an acceptance threshold or a multi-body scene estimate. The structured results, including exact sampled clip ticks and per-case counts, are in [ENG-209-animal-workload.json](ENG-209-animal-workload.json).

## Earlier failures and resolution

An earlier run before the physics pose-cache correction reported timed ray misses after a step followed by a part-local pose update: trout 11, crow 65, and deer 32 per representation. The independent exhaustive phase rays passed. The coordinator isolated the cause to stale collider world poses after `PhysicsWorld::step` cleared body dirty flags; the physics-side correction refreshes the collider pose from the current validated root and part-local pose during installation. The final unchanged matrix reports zero timed misses.

The first source-cell removal diagnostic also reported false mismatches because the fixture expected source revision 1 for every hit on a part other than the one currently edited. Earlier parts had already undergone remove/restore and had advanced to revision 3. The fixture now reads the expected hit part's current revision from its revision map. After correction, removal/restoration cell, part, TOI, topology, pose, and source revision all agree with the independent oracle.

## Reproduction and environment

Environment: Windows x86_64, `AMD64 Family 26 Model 68 Stepping 0, AuthenticAMD`, 16 reported logical processors; optimized Rust release profile. No GPU, window, network, or store was used.

Checks and exact bounded run:

```powershell
rustfmt --edition 2024 --check examples/sandbox/examples/animal_collision_probe.rs
cargo check -p sandbox --example animal-collision-probe
cargo build -p sandbox --release --example animal-collision-probe
cargo run -p sandbox --release --example animal-collision-probe -- --bounded
```

The final run exited 0 after the physics adapter was frozen, including its live-volume ownership and numeric admission guards, direct slice-sweep exclusions, and post-step part-pose refresh. The executable decodes the included real assets; asset decoding and world construction are outside the operation timing. Pose timing excludes clip sampling and ray generation, which are prepared before the timed interval. Live grid-rebuild timing starts after cloning an already materialized source grid; it includes adapter validation, shape construction and installation, but not canonical volume edits or occupancy extraction. Initial attachment includes source-grid cloning and adapter attachment. The release run is deliberately bounded and CPU-only. These results establish fixture-level exactness and provide diagnostic timing evidence; they do not establish target-scale performance.
