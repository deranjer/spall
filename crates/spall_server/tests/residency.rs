//! T18 streamed-residency acceptance fixtures.

use glam::DQuat;
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{
    BrickCoord, CellSizeCode, EntityId, GlobalCell, MaterialId, SphereBrush, VolumeId,
};
use spall_physics::PhysicsConfig;
use spall_server::{
    BodySpatialIndex, MemoryBrickBacking, PartitionCoord, PersistConfig, ResidencyController,
    StructuralResolution,
};
use spall_sim::{
    BodyPose, EditIntent, EditTarget, Simulation, SimulationConfig, WorldSetup, fixtures,
};
use spall_structure::AnchorPlane;
use spall_voxel::{
    BrickBounds, BrickCacheKey, CacheBudget, EditPlan, InterestRadii, Sample, Volume,
};

const WORLD_MIN: BrickCoord = BrickCoord::new(0, 0, 0);
// 256 x 128 x 256 metres at 25 cm/cell = 32 x 16 x 32 bricks.
const WORLD_MAX: BrickCoord = BrickCoord::new(31, 15, 31);

fn terrain_id() -> VolumeId {
    VolumeId::new(1).unwrap()
}

fn bounded_world(mut terrain: Volume, collider_hi: GlobalCell) -> Simulation {
    // The setup volume id is normalised to the registry's terrain id (1).
    assert_eq!(terrain.id(), terrain_id());
    // Keep at least one solid cell so the initial fixed collider is valid.
    if terrain.resident_brick_count() == 0 {
        terrain
            .apply_edit(&EditPlan::filled_box(
                terrain_id(),
                GlobalCell::new(0, 0, 0),
                GlobalCell::new(0, 0, 0),
                fixtures::STONE,
            ))
            .unwrap();
    }
    Simulation::new(SimulationConfig::new(WorldSetup {
        terrain,
        terrain_collider_region: (GlobalCell::new(0, 0, 0), collider_hi),
        materials: fixtures::stone_manifest(),
        anchor: AnchorPlane::at(0),
        physics: PhysicsConfig::default(),
    }))
    .unwrap()
}

fn terrain() -> Volume {
    Volume::bounded(
        terrain_id(),
        CellSizeCode::Quarter,
        BrickBounds::new(WORLD_MIN, WORLD_MAX).unwrap(),
    )
}

fn one_cell_cut(request: u64, cell: GlobalCell) -> EditIntent {
    let half = BRUSH_UNIT / 2;
    let brush = SphereBrush::new(
        BrushPoint::from_units(
            cell.x * BRUSH_UNIT + half,
            cell.y * BRUSH_UNIT + half,
            cell.z * BRUSH_UNIT + half,
        ),
        half,
    )
    .unwrap();
    EditIntent::cut(
        spall_protocol::RequestId(request),
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        brush,
    )
}

#[test]
fn structural_dependencies_load_past_visibility_before_releasing_a_component() {
    let mut volume = terrain();
    // A 64-cell beam spans three bricks. Its only anchor is at the remote end
    // in brick x=2; the visible neighborhood initially retains only brick 0.
    volume
        .apply_edit(&EditPlan::filled_box(
            terrain_id(),
            GlobalCell::new(1, 1, 1),
            GlobalCell::new(64, 1, 1),
            fixtures::STONE,
        ))
        .unwrap();
    volume
        .apply_edit(&EditPlan::filled_box(
            terrain_id(),
            GlobalCell::new(64, 0, 1),
            GlobalCell::new(64, 0, 1),
            fixtures::STONE,
        ))
        .unwrap();
    let mut sim = bounded_world(volume, GlobalCell::new(64, 2, 2));
    let mut residency = ResidencyController::new(
        CacheBudget::new(1, usize::MAX),
        64,
        MemoryBrickBacking::default(),
    );
    residency.register_world(sim.world(), false);
    residency
        .persist_volume(&sim.world().terrain().volume)
        .unwrap();
    residency.cache.update_interest(
        terrain_id(),
        BrickCoord::new(0, 0, 0),
        InterestRadii::new(0, 0).unwrap(),
    );
    let evicted = residency.enforce_budget(sim.world_mut()).unwrap();
    assert_eq!(sim.world().terrain().volume.resident_brick_count(), 1);
    assert!(evicted.iter().any(|k| k.coord.x == 2));
    assert!(
        residency
            .graph_metadata(BrickCacheKey::new(terrain_id(), BrickCoord::new(2, 0, 0)))
            .is_some(),
        "graph metadata remains resident after voxel eviction"
    );
    // Admit the complete three-brick structural working set; the initial
    // one-brick budget existed specifically to force the eviction boundary.
    residency.cache.set_budget(CacheBudget::new(
        3,
        3 * spall_voxel::MemoryReport::DENSE_BRICK_BYTES,
    ));

    let loaded = match residency
        .resolve_structure(sim.world_mut(), terrain_id(), 8)
        .unwrap()
    {
        StructuralResolution::Ready { index, loaded } => {
            assert_eq!(index.report().supported_cells, 65);
            loaded
        }
        StructuralResolution::Pending { missing, .. } => {
            panic!("durable structural dependencies stayed unavailable: {missing:?}")
        }
    };
    assert!(loaded.iter().any(|k| k.coord.x == 2));

    sim.submit(one_cell_cut(1, GlobalCell::new(64, 0, 1)))
        .unwrap();
    sim.run_until_idle(32).unwrap();
    assert_eq!(
        sim.world().body_count(),
        1,
        "removing the remote anchor releases the cross-partition beam"
    );
}

#[test]
fn modified_air_is_persisted_before_eviction_and_reloads_without_regrowth() {
    let mut volume = terrain();
    let cell = GlobalCell::new(4, 4, 4);
    volume
        .apply_edit(&EditPlan::filled_box(
            terrain_id(),
            cell,
            cell,
            fixtures::STONE,
        ))
        .unwrap();
    volume
        .apply_edit(&EditPlan::filled_box(
            terrain_id(),
            cell,
            cell,
            MaterialId::AIR,
        ))
        .unwrap();
    // Keep a separate anchor brick so SimWorld can create its collider.
    volume
        .apply_edit(&EditPlan::filled_box(
            terrain_id(),
            GlobalCell::new(32, 0, 0),
            GlobalCell::new(32, 0, 0),
            fixtures::STONE,
        ))
        .unwrap();
    let mut sim = bounded_world(volume, GlobalCell::new(32, 4, 4));
    let full_hash = sim.world().world_hash().0;
    let key = BrickCacheKey::new(terrain_id(), BrickCoord::new(0, 0, 0));
    let mut residency = ResidencyController::new(
        CacheBudget::new(1, usize::MAX),
        64,
        MemoryBrickBacking::default(),
    );
    residency.register_world(sim.world(), false);
    residency.cache.update_interest(
        terrain_id(),
        BrickCoord::new(1, 0, 0),
        InterestRadii::new(0, 0).unwrap(),
    );
    assert!(
        residency
            .enforce_budget(sim.world_mut())
            .unwrap()
            .contains(&key)
    );
    assert!(residency.backing().get(key).unwrap().edited);
    let checkpoint = residency
        .capture_checkpoint(
            &sim,
            &PersistConfig {
                world_id: 1,
                seed: 2,
                generator_version: 1,
            },
            0,
        )
        .unwrap();
    assert!(
        checkpoint
            .bricks
            .iter()
            .any(|brick| brick.volume_id == 1 && brick.coord == [0, 0, 0] && brick.edited)
    );
    assert_eq!(checkpoint.world_hash, full_hash);
    residency.cache.set_budget(CacheBudget::new(2, usize::MAX));
    assert!(residency.load_brick(sim.world_mut(), key).unwrap());
    assert_eq!(
        sim.world().terrain().volume.sample(cell).unwrap(),
        Sample::Empty { modified: true }
    );
}

#[test]
fn one_body_keeps_one_identity_while_crossing_partition_boundaries() {
    let mut base = terrain();
    base.apply_edit(&EditPlan::filled_box(
        terrain_id(),
        GlobalCell::new(0, 0, 0),
        GlobalCell::new(1, 0, 1),
        fixtures::STONE,
    ))
    .unwrap();
    let mut sim = bounded_world(base, GlobalCell::new(2, 2, 2));
    let entity = sim
        .world_mut()
        .spawn_body(
            |id| {
                let mut v = Volume::new(id, CellSizeCode::Quarter);
                v.apply_edit(&EditPlan::filled_box(
                    id,
                    GlobalCell::new(0, 0, 0),
                    GlobalCell::new(63, 3, 3),
                    fixtures::STONE,
                ))
                .unwrap();
                v
            },
            BodyPose::new(DQuat::IDENTITY, [28.0, 4.0, 0.0]),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            0,
        )
        .unwrap();
    let volume_id = sim.world().body(entity).unwrap().volume_id;
    let mut index = BodySpatialIndex::new(16.0).unwrap();
    index.rebuild(sim.world());
    let before = index.partitions_of(entity).unwrap().clone();
    assert!(before.len() >= 2);
    assert!(before.iter().all(|p| index.bodies_in(*p).eq([entity])));

    sim.world_mut()
        .volume_body_mut(volume_id)
        .unwrap()
        .pose
        .translation_m[0] += 20.0;
    index.rebuild(sim.world());
    let after = index.partitions_of(entity).unwrap();
    assert_ne!(&before, after);
    assert!(after.iter().all(|p| index.bodies_in(*p).eq([entity])));
    assert_eq!(
        sim.world().body_count(),
        1,
        "partition crossing never clones a body"
    );

    // Whole body geometry is pinned even under a zero-brick terrain budget.
    let mut residency =
        ResidencyController::new(CacheBudget::new(0, 0), 64, MemoryBrickBacking::default());
    residency.register_world(sim.world(), true);
    residency.enforce_budget(sim.world_mut()).unwrap();
    assert_eq!(
        sim.world()
            .body(entity)
            .unwrap()
            .volume
            .resident_brick_count(),
        2
    );
}

#[test]
fn fast_collision_entry_is_deferred_until_every_swept_brick_is_ready() {
    let mut residency = ResidencyController::new(
        CacheBudget::new(8, usize::MAX),
        8,
        MemoryBrickBacking::default(),
    );
    let first = BrickCacheKey::new(terrain_id(), BrickCoord::new(0, 0, 0));
    let second = BrickCacheKey::new(terrain_id(), BrickCoord::new(1, 0, 0));
    residency
        .collision
        .mark_ready(first, spall_core::Revision(1));
    assert!(matches!(
        residency.collision.admit_sweep(
            terrain_id(),
            CellSizeCode::Quarter,
            [1.0, 1.0, 1.0],
            [12.0, 1.0, 1.0],
            0.3,
        ),
        spall_voxel::CollisionAdmission::Blocked { .. }
    ));
    residency
        .collision
        .mark_ready(second, spall_core::Revision(1));
    assert_eq!(
        residency.collision.admit_sweep(
            terrain_id(),
            CellSizeCode::Quarter,
            [1.0, 1.0, 1.0],
            [12.0, 1.0, 1.0],
            0.3,
        ),
        spall_voxel::CollisionAdmission::Ready
    );
}

#[test]
fn steady_traversal_residency_plateaus_under_the_configured_budget() {
    let mut volume = terrain();
    for brick_x in 0..20 {
        let x = brick_x * 32;
        volume
            .apply_edit(&EditPlan::filled_box(
                terrain_id(),
                GlobalCell::new(x, 0, 0),
                GlobalCell::new(x, 0, 0),
                fixtures::STONE,
            ))
            .unwrap();
    }
    let mut sim = bounded_world(volume, GlobalCell::new(20 * 32, 1, 1));
    let mut residency = ResidencyController::new(
        CacheBudget::new(3, 3 * spall_voxel::MemoryReport::DENSE_BRICK_BYTES),
        64,
        MemoryBrickBacking::default(),
    );
    residency.register_world(sim.world(), false);
    residency
        .persist_volume(&sim.world().terrain().volume)
        .unwrap();

    let radii = InterestRadii::new(0, 1).unwrap();
    let mut observed_max = 0usize;
    for x in 0..20 {
        let coord = BrickCoord::new(x, 0, 0);
        let key = BrickCacheKey::new(terrain_id(), coord);
        residency.cache.update_interest(terrain_id(), coord, radii);
        residency.enforce_budget(sim.world_mut()).unwrap();
        if sim
            .world()
            .terrain()
            .volume
            .brick_revision(coord)
            .unwrap()
            .is_none()
        {
            assert!(residency.load_brick(sim.world_mut(), key).unwrap());
        }
        // A newly loaded entry was not present during the first interest pass.
        residency.cache.update_interest(terrain_id(), coord, radii);
        residency.enforce_budget(sim.world_mut()).unwrap();
        observed_max = observed_max.max(residency.cache.resident_bricks());
        assert!(residency.cache.resident_bricks() <= 3);
        assert!(
            residency.cache.resident_dense_bytes()
                <= 3 * spall_voxel::MemoryReport::DENSE_BRICK_BYTES
        );
    }
    assert_eq!(observed_max, 3);
    assert!(residency.cache.resident_bricks() <= 3);
    // The named fixture is the required 256 x 128 x 256 m bounded world.
    assert_eq!(
        sim.world().terrain().volume.bounds().unwrap().max,
        WORLD_MAX
    );
    assert_eq!(
        BodySpatialIndex::new(16.0)
            .unwrap()
            .bodies_in(PartitionCoord::new(0, 0, 0))
            .count(),
        0
    );
}
