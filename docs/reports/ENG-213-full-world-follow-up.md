# ENG-213 follow-up: direct exact casts and warmed full-world timings

This follow-up profiles the same natural seed-1 generated world (4096 cells/edge, 1024 m, 123,892 authoritative per-brick fixed terrain colliders), same habitat poses, all authored trout/crow/deer phase transitions, and both NativeVoxels and MergedCuboids representations. It does not include weather stepping or change the transition cap, source-cell geometry, or full-world server collider set.

## Exact-cast hot path

The localized transition path was rebuilding a Rapier `QueryPipeline` for every occupied-source-cell/candidate-collider pair. Each filtered view traversed/constructed against the full collider set, even when the broadphase had already returned one or two relevant terrain candidates. The path now keeps the same freshness sync and conservative full-world broadphase, then calls Parry 0.30.2's nonlinear shape cast directly using the selected live collider's exact shape and world pose. Source cells remain exact cuboids, follow the same linear per-part-origin plus shortest-arc rotation trajectory, and retain the same part/volume mapping. Errors/unsupported pairs remain non-hits as with the pipeline adapter. The one-million source-cell × candidate cap remains unchanged.

The transition result now reports three stage durations: query sync, candidate query construction/lookup, and exact source-cell casts. On the complete authoritative world, median stage times were:

| Animal | Cells | Candidates / exact casts | Query sync | Candidate lookup | Exact casts | Full transition p50 (p95) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Trout | 136 | 0 / 0 | <0.1 µs | 0.7–0.8 µs | 1.6–1.9 µs | 7–10 µs |
| Crow | 217 | 1 / 217 | <0.1 µs | 1.2–1.3 µs | 824–849 µs | 832–855 µs (931–936 µs) |
| Deer | 285 | 2 / 570 | 0.1 µs | 1.9–2.2 µs | 5.72–5.73 ms | 5.74 ms (5.82 ms) |

The earlier same-world run measured 499 ms crow and 1.31 s deer median transitions. This follow-up reduced those medians by about 580× and 228×, respectively, while the cast count and candidates remained identical. This confirms the prior excess was per-cast whole-world query-pipeline reconstruction. Raw stage results are in [ENG-213-full-world-after-direct-cast.json](ENG-213-full-world-after-direct-cast.json), with generation/setup logs in [ENG-213-full-world-after-direct-cast.stderr.txt](ENG-213-full-world-after-direct-cast.stderr.txt).

## Step breakdown and warmed samples

The one-shot 32-substep global trio sample remained a cold sample: 502 ms outer step, of which 502 ms was in Rapier's pipeline. In separate 30-step runs at 32 substeps and unchanged outer dt (1/60 s), the first sample was cold at about 493 ms (no rigs) and 523 ms (three rigs). The remaining 29 samples, including the 10 designated warmup samples, yield these whole-run medians:

| Full-world setup | Fixed terrain | Articulated bodies / colliders | Dynamic bodies | Step wall p50 / p95 | Pipeline p50 / p95 | Contact witness capture p50 / p95 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| No animals + falling dynamic probe | 123,892 | 0 / 0 | 1 | 329 / 646 µs | 59 / 70 µs | 0.7 / 1.0 µs |
| Trout + crow + deer + dynamic probe | 123,892 | 23 / 20 | 1 | 371 / 843 µs | 127 / 180 µs | 1.4 / 1.8 µs |

All 30 raw samples and setup counts are in the same JSON. The first full-world step pays a roughly half-second initialization/refit cost; timings after it are hundreds of microseconds for this bounded fixture. Cold startup remains a material one-time cost and the measured steady sample is not a population-throughput acceptance target. Weather, water simulation, CPU-fleet counts, and a target population/tick SLA are still outside this physics measurement.

## Validation and limits

- `cargo test -p spall_physics --lib`: 91 passed, 1 ignored; includes exact-wall sweeps, midarc/off-center rotation, removal/replacement/edit freshness, and full-world distant-collider locality coverage.
- `cargo clippy -p spall_physics -p sandbox --all-targets -- -D warnings`: passed.
- `cargo fmt --all -- --check`: passed after formatting.
- `cargo run --release -p sandbox --example animal-collision-probe -- --generated-1km`: passed against the complete seed-1 server world; all 60 authored transitions were clear under both collider representations. No reduced/client-local terrain subset was used.
- Earlier independent source-cell collision oracle and small-world broadphase-vs-exhaustive checks remain unchanged and passed in the prior ENG-213 commit.

These measurements establish a substantial narrowphase query overhead fix and warmed CPU-step feasibility for one three-animal fixture. They do not establish one-kilometre weather plus a large fully simulated animal population; the actual population target, weather cost, shared-server actor path, integrated ENG-212 artifacts, and an explicit latency target still need acceptance measurements.
