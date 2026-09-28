# Water physics: agent starting brief

Status: design handoff from the user discussion, not an implemented feature or
an accepted feasibility gate. No Loopira tickets were created or updated for
this document. The implementing agent owns the subsequent tracking work.

## Agreed gameplay

The user wants physically interactive water in Spall, including:

- Building dams, accumulating water behind them, and breaking dams.
- Digging canals that redirect water and change connected water levels.
- Swimming, including interaction with currents.
- Building boats and sailing on water.
- Experimenting with water appearance independently of the physical model.

Containers that can be picked up and tipped are lower priority, not a permanent
exclusion. Rivers and lakes are the proposed first operating envelope.
Open-ocean scale and large ocean waves have not been decided. Do not interpret
this brief as a promise of unlimited water volume, resolution, or active area.

Full-world destruction, authoritative multiplayer, and CPU-only dedicated
servers remain requirements. A visual water surface with scripted buoyancy
alone does not satisfy the requested gameplay.

## Read and inspect first

Read `AGENTS.md`, `README.md`, `docs/architecture.md`, `docs/protocol.md`,
`docs/validation.md`, and relevant assignments in `docs/tasks.md`. Inspect the
current implementation and evidence rather than relying on historical status
paragraphs. Preserve existing uncommitted work.

Follow the repository's Loopira workflow when implementation starts: inspect
the project guide and current tickets, establish a bounded assignment and its
dependencies, update status, and record evidence. This handoff is not a ticket
ID and does not assign the entire water system to one implementation pass.

## Proposed technical direction

Keep Rapier as the sole rigid-body solver. Add a fluid subsystem that exchanges
forces and boundary information with it; do not introduce a competing solid
physics world. Explicitly document this extension to the existing physics
boundary when integrating it.

The leading candidate is a sparse three-dimensional water grid containing
water amount and velocity, with gravity, conservative transport, and a pressure
solve. This is a hypothesis to test, not a frozen solver choice. Simple
downward transfer and neighbor equalization are insufficient evidence for
dam-break momentum, pressure-driven flow, or convincing boat interaction.

Evaluate representation, free-surface treatment, pressure solver, and solid
boundary handling together. A standard single-heightfield shallow-water
solver cannot alone cover a river above a flooded tunnel or stacked pools.
Particle-based water is another candidate, particularly for moving boundaries
and eventual pouring, but its cost at reservoir scale must be measured.

Salva is worth inspecting as an existing Rust fluid library with Rapier
coupling. Verify released dependency compatibility, capabilities, licensing,
and maintenance before adopting it. Do not upgrade shared dependencies merely
to make a prototype convenient. A custom grid solver and Salva need not both
be fully implemented before making a decision; compare the most important
risks with bounded evidence.

## Integration constraints

- **Ownership:** Keep water state distinct from solid material occupancy.
  Partial cells must be possible. Water is not a collection of voxel rigid
  bodies or ECS entities. Any new engine crate should follow `spall_*` naming
  and remain independent of game packages, graphics, and network runtimes.
- **Conservation:** Account for initial water, explicit sources/sinks, boundary
  outflow, and numerical error. Excavation, placement, moving bodies, and
  streaming must not silently create or delete water. Define what happens when
  a solid is placed into occupied water and displacement cannot be resolved.
- **Geometry:** Water resolution may differ from solid resolution, but intact
  thin walls must block flow and supported openings must pass it. Record the
  minimum supported wall/opening sizes and measure resolution sensitivity.
- **Tick ordering:** Integrate fluid boundary changes with committed terrain
  edits and body ownership changes. Do not flow through an intact wall using
  stale geometry. Specify the order of fluid stepping, force exchange, and
  Rapier stepping, including any coupling lag. Keep the existing fixed server
  tick; document fluid substeps, stability limits, and overload behavior.
- **Jobs:** Workers consume immutable snapshots. The owning simulation thread
  validates dependencies and applies results at tick boundaries. Pressure
  solves spanning connected regions require an explicit consistency strategy.
- **Bodies:** Bodies obstruct and displace water; water applies forces and
  torque back to bodies. Avoid double-counting buoyancy if pressure coupling
  already supplies it. Handle sleeping/waking and geometry changes explicitly.
- **Scale:** Sparse storage alone does not make a filled lake cheap. Measure
  active cells, pressure iterations, memory, and total server cost. Quiet water
  needs a conservative dormancy/wake strategy. Missing regions are not empty
  space, drains, or permanent walls; define boundary exchange before streaming.
- **Multiplayer and recovery:** The server owns gameplay water and forces.
  Plan versioned water DTOs, late-join baselines, repair, bounded interest
  updates, checkpointing, and crash recovery. Separate canonical saved state
  from interpolated visual state. Do not rely on client fluid lockstep or on
  replaying floating-point simulation from edit commands to recover water.

## Boats, swimming, and appearance

Boat buoyancy needs displaced volume and its distribution, not just a force
at the body center. An open-topped hull below its gunwale can exclude water;
counting only its solid wood voxels misses that displacement. A breach or
overtopping must let water enter. Prototype watertightness, orientation,
off-center loading, flooding, and fluid/body coupling. A sealed-air simulation
is not automatically required: state the initial trapped-air approximation
and its limits rather than implying air pressure is simulated.

Swimming needs authoritative submersion and local water velocity queries,
with buoyancy, drag, and movement controls. Sailing additionally needs a
bounded wind model and sail, keel, and rudder forces. A motorized test body
does not establish sailing. Stage these after basic fluid feasibility; avoid
adding unrelated weather or game systems.

Keep appearance replaceable: blocky, smooth stylized, or more realistic water
can share the same physical state. Fine ripples, foam, and spray may be client
effects. Waves that materially move boats or players need an authoritative
physical representation. A pretty capture is not a physics acceptance test.

## First bounded assignment: establish fluid feasibility

Audit integration points and build a small CPU fixture with two finite
reservoirs at different elevations, an editable dam, and an excavatable
connecting canal/tunnel. Use actual Spall voxel boundary data. Start with one
water type and no implicit infinite source.

Before running it, record dimensions, water resolution, timestep/substeps,
initial volume, boundary conditions, hardware, and proposed numerical and
performance thresholds. Thresholds are targets until measured. Reserve water
time within the existing total server budget; do not treat that entire budget
as available to fluids.

Required evidence for this first assignment:

1. Resting water remains stable and does not leak through intact walls,
   including a wall crossing a brick boundary.
2. Opening a canal transfers water toward hydrostatic equilibrium; closing
   the passage blocks exchange without deleting water.
3. Breaking the dam creates a moving surge and downstream accumulation.
   Report flow/momentum behavior, not just final equalized levels.
4. A flooded tunnel beneath another water body is represented correctly.
5. Water accounting closes within a declared tolerance throughout edits and
   flow; report absolute and relative error and any explicit outflow.
6. Report fluid-step and total-step timings, memory, active cells, pressure
   convergence, and response to a larger workload. Include a resolution or
   timestep comparison to expose numerical artifacts.

Use bounded runs and structured metrics through the existing harness where
practical. Add meaningful invariant/scenario tests. New water commands are
proposed interfaces until implemented and exercised; do not claim they exist.

Deliver the prototype, reproducible commands, evidence, and a solver decision
with limitations. If it misses feasibility targets, report the actual failure
and finish independent in-scope work. Do not silently shrink the scenario or
substitute visual effects for simulated flow.

## Subsequent acceptance stages

These are the roadmap, not requirements to finish in the first assignment:

1. **Rigid-body coupling:** floating and sinking debris, displacement, drag,
   rotating hulls, off-center loads, breached-hull flooding, and stable resting
   contacts. Test two-way interaction rather than scripted object motion.
2. **Gameplay:** swimming with currents; a player-built boat with wind-driven
   propulsion and steering. Define how authored voxel pieces form one hull
   using existing body/assembly capabilities or an explicitly scoped prerequisite.
3. **Multiplayer and recovery:** two clients observe a dam breach, late join
   during flow, packet-loss recovery, save/restart, and matching canonical
   water baselines. Test durability across geometry and water state changes.
4. **Scale and visuals:** conservative streaming/dormancy, several active water
   regions, sustained server/bandwidth measurements, and alternative visual
   treatments of the same physical fixture.

Plan authority and persistence from the first prototype, even though their
integrated checks land later. Local solver evidence alone does not establish
multiplayer readiness. End each assignment with changed files, exact checks,
measured results versus targets, remaining risks, and the next unblocked task.

## Starting references

- [Salva](https://salva.rs/) and its
  [source repository](https://github.com/dimforge/salva): particle fluids and
  Rapier coupling candidate.
- [Bridson's fluid simulation course](https://www.cs.ubc.ca/~rbridson/fluidsimulation/2006/):
  background on grid, pressure, and particle/grid methods.
- [Real-time Simulation of Large Bodies of Water with Small Scale Details](https://matthias-research.github.io/pages/publications/hfFluid.pdf):
  background on shallow-water representations and their role in larger systems.

References are starting points, not evidence of Spall compatibility or performance.
