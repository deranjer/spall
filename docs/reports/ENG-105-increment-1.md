# ENG-105 increment 1: authoritative water

Status: historical evidence for the first ENG-105 increment. PR #167 subsequently
merged full-domain keyframes and client presentation. Persistence, body
interaction, dormancy, and streaming remain open. The current audit and exact
acceptance gaps are in [replication validation](ENG-105-replication-validation.md);
the remaining-work paragraphs below describe the original increment's baseline.

## Server ownership and tick order

`Simulation` may own one `AuthoritativeWater` over an explicitly bounded,
fully resident `DomainSpec`. `WaterSetup` seeds global-cell fractions from the
scene. On each tick, committed terrain edits refresh the strict voxel boundary;
new solids displace water breadth-first through face-connected open cells in a
stable direction order. Capacity errors do not mutate the fluid grid. The
solver then advances at the fixed simulation dt, followed by Rapier. Solver
stability-budget overflow skips that water step and records skipped ticks and
time; it does not change the simulation dt.

The valley showcase no longer turns its authored water cells into blue solid
voxels. Its custom server world receives them as water fractions. Dynamic
bodies are not fluid obstacles and receive no buoyancy or drag in this
increment.

## Checks

- `cargo test -p spall_fluid boundary_ -- --nocapture`: 6 passed, including
  fully submerged displacement, sealed-pocket displacement, atomic capacity
  failure, and no displacement through solid walls.
- `cargo test -p spall_sim --test authoritative_water -- --nocapture`: 2
  passed. The server-tick scenario uses real canal and dam-breach `EditIntent`s,
  checks downstream flow, and accounts for conserved volume.
- `cargo test -p sandbox --test editor_scene authored_valley_water_is_server_state_not_solid_terrain -- --nocapture`:
  passed; authored water cells are empty in terrain and are retained by the
  server custom-world setup.
- `cargo check -p spall_client -p spall_server -p sandbox`: passed.

## Uncapped run observation

Command: `cargo xtask play --editor-scene fixtures/valley-showcase --ticks 120 --uncapped`.
The window selected Immediate presentation. Once terrain loaded (20,180 cube
instances), reported render rates were approximately 425–473 FPS, with about
0.88–0.90 ms scene GPU time per frame on this development machine. The server
completed its 120 requested ticks in roughly 35 seconds of the observed run,
far slower than the 2 seconds of simulated time at the 60 Hz target. This is a
measured integration performance failure for the current valley water region;
the renderer FPS does not demonstrate a 60 Hz authoritative water budget.
These measurements are hardware- and debug-build-specific.

## Remaining risks and next work

Water currently has no versioned network snapshot or client replica, so this
server-side simulation is not yet visible as flowing water to a networked
client. ENG-105 increment 2 must add bounded snapshots/deltas, late-join repair,
and presentation-only client rendering. Increment 3 must make canonical water
state part of saves before scenes with water are safe to restart from a durable
world. The valley's measured server throughput also needs a full-region
performance investigation before claiming the 60 Hz target.
