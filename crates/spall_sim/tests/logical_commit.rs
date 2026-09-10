//! T23 / G3 row 7, slice B — cache placement must not move logical topology.
//!
//! Evicting a terrain brick that a cut neither writes nor structurally depends
//! on must not change:
//!   - the transaction's parent `result_hash`,
//!   - `SimWorld::world_hash()`,
//!   - `SimWorld::total_solid_cells()` (conservation).
//!
//! And staging must **refuse** (without mutating the snapshot) a cut whose
//! structural analysis would read an evicted brick as empty.

use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{BrickCoord, EntityId, SphereBrush};
use spall_protocol::RequestId;
use spall_sim::{
    EditIntent, EditTarget, Simulation, SimulationConfig, StageError, StageInput, fixtures,
    stage_edit,
};
use spall_structure::AnchorPlane;
use spall_voxel::{BrickDigest, EvictedBricks, Volume};

fn cut(req: u64, cell: [i64; 3], radius: i64) -> EditIntent {
    let h = BRUSH_UNIT / 2;
    let brush = SphereBrush::new(
        BrushPoint::from_units(
            cell[0] * BRUSH_UNIT + h,
            cell[1] * BRUSH_UNIT + h,
            cell[2] * BRUSH_UNIT + h,
        ),
        radius * BRUSH_UNIT,
    )
    .unwrap();
    EditIntent::cut(
        RequestId(req),
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        brush,
    )
}

/// Runs one west-column cut on `separated_regions_setup` and returns the
/// committed transaction's parent (terrain) `result_hash`, the post-cut
/// `world_hash`, and the post-cut `total_solid_cells`.
fn run_west_cut(
    evict: Option<BrickCoord>,
) -> (spall_protocol::Hash32, spall_protocol::Hash32, u64) {
    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::separated_regions_setup())).unwrap();
    let terrain = sim.world().terrain_volume_id();

    if let Some(coord) = evict {
        let did = sim.world_mut().evict_brick(terrain, coord).unwrap();
        assert!(did, "brick {coord:?} was resident and got evicted");
    }

    sim.submit(cut(1, [10, 6, 3], 2)).unwrap();
    sim.run_until_idle(24).unwrap();

    let committed = sim.committed(RequestId(1)).expect("the west cut committed");
    let parent_hash = committed
        .topology
        .result_hashes
        .iter()
        .find(|vh| vh.volume == terrain)
        .expect("terrain result hash")
        .hash;

    (
        parent_hash,
        sim.world().world_hash(),
        sim.world().total_solid_cells(),
    )
}

#[test]
fn evicting_an_untouched_brick_does_not_move_the_transaction_or_world_hash() {
    let full = run_west_cut(None);

    // Pick a resident terrain brick in the far (east) region — the west cut
    // neither writes it nor structurally depends on it.
    let east_brick = {
        let sim =
            Simulation::new(SimulationConfig::new(fixtures::separated_regions_setup())).unwrap();
        sim.world()
            .terrain()
            .volume
            .resident_brick_coords()
            .into_iter()
            .find(|c| c.x >= 2 && c.z >= 2)
            .expect("separated_regions has an east-region brick")
    };

    let evicted = run_west_cut(Some(east_brick));

    assert_eq!(
        evicted.0, full.0,
        "parent result_hash moved when an untouched brick was evicted"
    );
    assert_eq!(evicted.1, full.1, "world_hash moved under eviction");
    assert_eq!(
        evicted.2, full.2,
        "total_solid_cells (conservation) moved under eviction"
    );
}

#[test]
fn world_hash_and_conservation_are_stable_across_eviction_order() {
    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::separated_regions_setup())).unwrap();
    let terrain = sim.world().terrain_volume_id();
    let baseline_hash = sim.world().world_hash();
    let baseline_solid = sim.world().total_solid_cells();

    let coords: Vec<BrickCoord> = sim.world().terrain().volume.resident_brick_coords();
    assert!(coords.len() >= 4);

    // Evict a few bricks in a deliberately non-canonical order.
    for &c in [coords[2], coords[0], coords[3]].iter() {
        assert!(sim.world_mut().evict_brick(terrain, c).unwrap());
    }
    assert!(sim.world().has_evicted());
    assert_eq!(
        sim.world().world_hash(),
        baseline_hash,
        "world_hash moved after out-of-order eviction"
    );
    assert_eq!(sim.world().total_solid_cells(), baseline_solid);

    // The reload lifecycle refuses to clear a digest while the brick is still
    // absent (no geometry to verify against).
    assert!(
        sim.world_mut()
            .clear_evicted_after_reload(terrain, coords[0])
            .is_err()
    );
    assert_eq!(sim.world().world_hash(), baseline_hash);
}

#[test]
fn staging_refuses_a_cut_that_needs_an_evicted_brick_without_touching_the_snapshot() {
    // A column + beam crossing the x = 32 brick boundary: the beam's support
    // path runs through brick x = 1.
    let volume: Volume = fixtures::cross_brick_bridged_setup().terrain;
    let vid = volume.id();
    let before_digest = spall_voxel::fixtures::digest_hex(&volume);

    // Evict brick (1, 0, 0) — part of the floor + beam live there.
    let mut evicted = EvictedBricks::new();
    let victim = BrickCoord::new(1, 0, 0);
    evicted
        .record(victim, BrickDigest::capture(&volume, victim).unwrap())
        .unwrap();
    let mut sim_volume = volume.clone();
    sim_volume.evict_brick(victim);

    let intent = cut(1, [31, 4, 1], 2); // sever the seam column
    let input = StageInput::new(
        &intent,
        vid,
        sim_volume,
        evicted,
        AnchorPlane::at(0),
        Default::default(),
        Default::default(),
    );

    match stage_edit(&input) {
        Err(StageError::EvictedGeometryRequired(bricks)) => {
            assert!(
                bricks.contains(&victim),
                "the error names the evicted brick it needs: {bricks:?}"
            );
        }
        other => panic!("expected EvictedGeometryRequired, got {other:?}"),
    }

    // The un-evicted source volume is untouched.
    assert_eq!(spall_voxel::fixtures::digest_hex(&volume), before_digest);
}

#[test]
fn staging_still_proceeds_when_the_evicted_brick_is_clear_of_the_edit() {
    // Same scene, but evict a brick far from the cut and its support path.
    let volume: Volume = fixtures::separated_regions_setup().terrain;
    let vid = volume.id();

    let far = volume
        .resident_brick_coords()
        .into_iter()
        .find(|c| c.x >= 2 && c.z >= 2)
        .unwrap();
    let mut evicted = EvictedBricks::new();
    evicted
        .record(far, BrickDigest::capture(&volume, far).unwrap())
        .unwrap();
    let mut sim_volume = volume.clone();
    sim_volume.evict_brick(far);

    let intent = cut(1, [10, 6, 3], 2); // west column, nowhere near `far`
    let input = StageInput::new(
        &intent,
        vid,
        sim_volume,
        evicted,
        AnchorPlane::at(0),
        Default::default(),
        Default::default(),
    );

    let staged = stage_edit(&input).expect("a cut clear of the evicted brick still stages");
    assert!(
        staged.splits(),
        "the west column cut detaches the west beam"
    );
    assert!(staged.ledger.check().is_ok());
}
