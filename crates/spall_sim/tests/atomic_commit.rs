//! ENG-54: an authoritative-edit commit that fails a late fallible step must
//! change nothing.
//!
//! The commit reserves every id, applies the edit to clones, and assembles and
//! validates the transaction DTO in an isolated candidate before it publishes to
//! the live world. Forcing transaction- or journal-id exhaustion (a late failure
//! that the pre-fix code hit only *after* it had already edited the world,
//! rebuilt colliders and installed bodies) must leave the world hash, cell
//! count, colliders, bodies, id-registry counters and journal exactly as they
//! were, and the request must be reported as a deterministic rejection.

use glam::DQuat;
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{EntityId, SphereBrush};
use spall_protocol::RequestId;
use spall_sim::fixtures;
use spall_sim::{BodyPose, EditIntent, EditTarget, Simulation, SimulationConfig};

fn brush_cell(x: i64, y: i64, z: i64, radius_cells: i64) -> SphereBrush {
    let h = BRUSH_UNIT / 2;
    SphereBrush::new(
        BrushPoint::from_units(x * BRUSH_UNIT + h, y * BRUSH_UNIT + h, z * BRUSH_UNIT + h),
        radius_cells * BRUSH_UNIT,
    )
    .unwrap()
}

fn actor() -> EntityId {
    EntityId::new(1).unwrap()
}

fn was_rejected(reports: &[spall_sim::TickReport], req: RequestId) -> bool {
    reports
        .iter()
        .any(|r| r.rejected.iter().any(|(id, _)| *id == req))
}

/// A non-splitting terrain cut whose commit hits transaction-id exhaustion:
/// nothing about the authoritative world or the journal may change.
#[test]
fn forced_transaction_id_exhaustion_leaves_authoritative_state_untouched() {
    let mut sim = Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();

    let (ne, nv, _nt, nj) = sim.world().registry().counters();
    // Resume with the transaction allocator one step from its exhaustion
    // sentinel, everything else intact.
    sim.world_mut()
        .resume_registry(ne, nv, u64::MAX, nj)
        .unwrap();

    let hash_before = sim.world().world_hash();
    let cells_before = sim.world().total_solid_cells();
    let bodies_before = sim.world().body_count();
    let epoch_before = sim.world().topology_epoch();
    let counters_before = sim.world().registry().counters();
    assert_eq!(sim.journal().len(), 0);

    let req = RequestId(1);
    sim.submit(EditIntent::cut(
        req,
        actor(),
        EditTarget::Terrain,
        brush_cell(12, 1, 12, 2),
    ))
    .unwrap();
    let reports = sim.run_until_idle(8).unwrap();

    assert!(
        sim.committed(req).is_none(),
        "the ID-exhausted commit must not report a transaction"
    );
    assert!(
        was_rejected(&reports, req),
        "the request must be reported as a deterministic rejection, not silently dropped"
    );

    assert_eq!(
        sim.world().world_hash(),
        hash_before,
        "world hash unchanged after the failed commit"
    );
    assert_eq!(
        sim.world().total_solid_cells(),
        cells_before,
        "no cell moved"
    );
    assert_eq!(sim.world().body_count(), bodies_before, "no body installed");
    assert_eq!(
        sim.world().topology_epoch(),
        epoch_before,
        "topology epoch not advanced"
    );
    assert_eq!(
        sim.world().registry().counters(),
        counters_before,
        "no id counter advanced — the reserved candidate ids were discarded"
    );
    assert_eq!(sim.journal().len(), 0, "no journal entry appended");
}

/// The transaction id is reservable but the *following* journal-sequence
/// allocation fails. The pre-fix code had already mutated the world and advanced
/// the live transaction counter by this point; the isolated candidate must leave
/// both untouched.
#[test]
fn late_journal_exhaustion_after_the_transaction_id_is_reserved_is_atomic() {
    let mut sim = Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();

    let (ne, nv, nt, _nj) = sim.world().registry().counters();
    sim.world_mut()
        .resume_registry(ne, nv, nt, u64::MAX)
        .unwrap();

    let hash_before = sim.world().world_hash();
    let cells_before = sim.world().total_solid_cells();

    let req = RequestId(1);
    sim.submit(EditIntent::cut(
        req,
        actor(),
        EditTarget::Terrain,
        brush_cell(12, 1, 12, 2),
    ))
    .unwrap();
    let reports = sim.run_until_idle(8).unwrap();

    assert!(sim.committed(req).is_none());
    assert!(was_rejected(&reports, req));

    let (ne_after, nv_after, nt_after, nj_after) = sim.world().registry().counters();
    assert_eq!(
        (ne_after, nv_after, nt_after, nj_after),
        (ne, nv, nt, u64::MAX),
        "the transaction counter must not advance for a candidate that never published"
    );
    assert_eq!(
        sim.world().world_hash(),
        hash_before,
        "world hash unchanged"
    );
    assert_eq!(
        sim.world().total_solid_cells(),
        cells_before,
        "no cell moved"
    );
    assert_eq!(sim.journal().len(), 0, "no journal entry appended");
}

/// A splitting cut whose commit fails late: no child body, no reserved child
/// entity/volume id leaks, no epoch bump.
#[test]
fn forced_exhaustion_on_a_splitting_cut_creates_no_body_and_no_epoch_bump() {
    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup())).unwrap();

    let (ne, nv, _nt, nj) = sim.world().registry().counters();
    sim.world_mut()
        .resume_registry(ne, nv, u64::MAX, nj)
        .unwrap();

    let hash_before = sim.world().world_hash();
    let cells_before = sim.world().total_solid_cells();
    let epoch_before = sim.world().topology_epoch();

    let req = RequestId(1);
    sim.submit(EditIntent::cut(
        req,
        actor(),
        EditTarget::Terrain,
        brush_cell(10, 4, 1, 2),
    ))
    .unwrap();
    let reports = sim.run_until_idle(12).unwrap();

    assert!(sim.committed(req).is_none());
    assert!(was_rejected(&reports, req));
    assert_eq!(sim.world().body_count(), 0, "no child body was installed");
    assert_eq!(
        sim.world().topology_epoch(),
        epoch_before,
        "a failed split does not advance the topology epoch"
    );
    assert_eq!(
        sim.world().registry().counters(),
        (ne, nv, u64::MAX, nj),
        "the child entity/volume ids reserved in the candidate were discarded"
    );
    assert_eq!(sim.world().world_hash(), hash_before);
    assert_eq!(sim.world().total_solid_cells(), cells_before);
    assert_eq!(sim.journal().len(), 0);
}

/// The authoritative terrain collider is unchanged by a failed commit: a probe
/// dropped over the cut target still lands on the intact slab in the same ticks
/// a successful cut would have let it fall through.
#[test]
fn a_failed_commit_does_not_swap_the_terrain_collider() {
    let mut sim = Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();

    let (ne, nv, _nt, nj) = sim.world().registry().counters();
    sim.world_mut()
        .resume_registry(ne, nv, u64::MAX, nj)
        .unwrap();

    let req = RequestId(1);
    sim.submit(EditIntent::cut(
        req,
        actor(),
        EditTarget::Terrain,
        brush_cell(3, 1, 3, 3),
    ))
    .unwrap();
    let reports = sim.run_until_idle(8).unwrap();
    assert!(sim.committed(req).is_none());
    assert!(was_rejected(&reports, req));

    // A fresh probe directly over where the hole would have been must come to
    // rest on the slab (top face at y = 2 cells * 0.25 m = 0.5 m), because the
    // collider still has that wall. A swapped-in "hole" collider would let it
    // fall well past y = 0 under gravity in 60 steps.
    let faller = sim
        .world_mut()
        .spawn_body(
            fixtures::solid_block(2),
            BodyPose::new(DQuat::IDENTITY, [0.6, 1.2, 0.6]),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            0,
        )
        .unwrap();
    for _ in 0..60 {
        sim.step_physics_only();
    }
    let end_y = sim.world().body(faller).unwrap().pose.translation_m[1];
    assert!(
        end_y > 0.2,
        "probe rested on the intact slab (y = {end_y}); the failed commit left the terrain collider alone"
    );
}
