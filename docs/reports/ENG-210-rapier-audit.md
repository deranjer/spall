# ENG-210 Rapier rig candidate audit

## Decision

Carry forward a bounded **one kinematic rigid body per authored source part**
candidate inside the existing `PhysicsWorld`/Rapier solver, with a single
conceptual assembly owner and the existing exact source-volume table. Use
position-based kinematics for clip-sampled targets. Each body receives the
world-space transform of its own flattened source part at the tick boundary;
Rapier can then derive that body's linear and angular velocity from the
previous and next poses. This directly addresses ENG-209's missing child
surface velocity without claiming that SPVX parent links define physical
joints. There are no impulse or multibody joints in this candidate.

This is a candidate selection, not a production-interface acceptance. The
current prototype still has a single kinematic root and attached colliders.
The experiment has not yet measured the per-part-body candidate against the
ENG-209 0.6 m / 60-step path, the one-step 0.5 m case, or actual asset clips.

## Pinned Rapier audit

The workspace pins `rapier3d = 0.35.3`. Its source exposes
`RigidBodyBuilder::kinematic_position_based` and
`RigidBody::set_next_kinematic_position`; the latter derives a kinematic
velocity from the current pose to the requested next pose during stepping.
The pinned `ActiveCollisionTypes::default()` enables dynamic/dynamic,
dynamic/kinematic, and dynamic/fixed pairs. It does not enable kinematic/fixed
pairs (nor kinematic/kinematic pairs). The bit flags expose those pairs for
explicit activation.

Enabling kinematic/fixed pairs is necessary for terrain contact observations,
but is not a movement-blocking contract: a position-based kinematic target is
prescribed, so solver contacts do not rewrite the target to stop the animated
body. Root movement therefore needs a bounded sweep against the current
authoritative terrain shapes before scheduling all part targets. The contract
must state whether the sweep is exact against the represented collider or
conservative relative to voxel geometry; it must not call the existing
character capsule sweep exact for an articulated source-cell union.

Per-part kinematic bodies remain infinite-mass prescribed animation. They can
transfer solver impulse to dynamic bodies, but are not ragdolls and do not
yield to impacts. If the product requires animals to recoil or deform from
impacts, that is a different, explicitly authored physical model and cannot
be inferred from the SPVX hierarchy.

## Retained evidence and limits

ENG-209 remains the baseline. Its measured local-part path produced about
0.674 N·s over 60 steps versus about 10.10 N·s for the same root-driven path;
the fast one-step target produced a contact pair and zero impulse. The root
crossed the 0.25 m fixed wall over its 2 m / 2 s target, while the existing
character sweep advanced 0.6799 m. These failures are not renamed, scaled down,
or treated as passed by this audit.

ENG-209's actual-asset test remains useful evidence for exact geometry,
topology revisions, pose tokens, ray provenance, and workload size. It does
not measure per-part body costs. Its checked assets contain 5 trout parts / 136
cells, 6 crow parts / 217 cells, and 9 deer parts / 285 cells, including hidden
parts and nested antlers. The all-clips, all-representations matrix must be
rerun with the candidate and report the added body count, collider count,
primitive count, update/query/step costs, and source-cell oracle results.

No source cell may be selected by rounding a manifold point or by trusting the
nearest cell alone. A later damage rule must first identify the source part
through the collider-to-volume binding, transform the actual contact witness
into that part's source coordinates, and prove the selected occupied cell is
consistent with the contact surface. Ambiguous or interior witnesses must be
reported as unattributed (or resolved with a separately validated geometric
query); this audit does not accept contact-cell attribution.

## Outstanding feasibility work

- Implement a bounded experimental per-part-body path without changing the
  production animal lifecycle contract.
- Preserve assembly ownership and atomic stale-pose rejection across every
  part body. Prove a rejected or partial pose cannot move only some parts.
- Run the original ENG-209 trajectory and wall scenarios unchanged, plus
  current-terrain movement, support removal/fall, fast contact, and stale-update
  isolation scenarios.
- Validate all trout, crow, and deer clips, including hidden parts and nested
  antlers, against the independent occupied-cell oracle.
- Measure complete attachment, pose update, query, and solver step costs and
  report body/collider/primitive counts. There is no accepted population target.
- Leave source-cell damage attribution, dynamic corpse ownership, runtime
  actors, replication, persistence, and host integration gated until their
  independent contracts are reviewed.

This report records the pinned-source API audit and chosen experiment only. It
does not pass ENG-210 or ENG-209's known movement/contact gaps.
