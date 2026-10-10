//! The topology hash is computed from cached per-chunk digests that are reused while a chunk's
//! stamps are unchanged. Whatever happens to the world between two hash calls, the cached hash
//! must equal the hash computed from scratch with the reference function. A stale digest would
//! make the server accept or reject the wrong transactions.

use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{BrickCoord, CellSizeCode, EntityId, GlobalCell, Revision, SphereBrush, VolumeId};
use spall_protocol::{RequestId, canonical_topology_hash};
use spall_sim::{EditIntent, EditTarget, Simulation, SimulationConfig, fixtures};
use spall_voxel::{Brick, Volume};

/// Bricks per side of the floor: 24 x 24 x 3 spans 3 x 3 x 1 hash chunks.
const SIDE: i64 = 24;

fn floor_sim() -> Simulation {
    let id = VolumeId::new(1).unwrap();
    let mut volume = Volume::new(id, CellSizeCode::Quarter);
    for z in 0..SIDE {
        for x in 0..SIDE {
            for y in 0..3 {
                volume
                    .insert_brick(
                        BrickCoord::new(x, y, z),
                        Brick::uniform(
                            fixtures::STONE,
                            Revision(1 + (x + SIDE * (z + SIDE * y)) as u64),
                        ),
                    )
                    .unwrap();
            }
        }
    }
    let mut setup = fixtures::flat_terrain_setup();
    setup.terrain = volume;
    setup.terrain_collider_region = (
        GlobalCell::new(0, 0, 0),
        GlobalCell::new(SIDE * 32 - 1, 3 * 32 - 1, SIDE * 32 - 1),
    );
    setup.physics.disable_ccd = true;
    Simulation::new(SimulationConfig::new(setup)).unwrap()
}

fn brush_cell(x: i64, y: i64, z: i64, radius_cells: i64) -> SphereBrush {
    let h = BRUSH_UNIT / 2;
    SphereBrush::new(
        BrushPoint::from_units(x * BRUSH_UNIT + h, y * BRUSH_UNIT + h, z * BRUSH_UNIT + h),
        radius_cells * BRUSH_UNIT,
    )
    .unwrap()
}

/// The world's topology hash from scratch: whole canonical volumes, no cache.
fn reference_world_hash(sim: &Simulation) -> spall_protocol::Hash32 {
    let world = sim.world();
    let mut volumes = vec![world.canonical_volume(world.terrain_volume_id()).unwrap()];
    for body in world.bodies() {
        volumes.push(world.canonical_volume(body.volume_id).unwrap());
    }
    canonical_topology_hash(&volumes)
}

struct Rng(u64);
impl Rng {
    fn below(&mut self, n: u64) -> i64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n) as i64
    }
}

#[test]
fn the_cached_hash_equals_the_reference_after_any_mix_of_cuts_and_evictions() {
    let mut sim = floor_sim();
    let terrain = sim.world().terrain_volume_id();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut next_request = 1u64;
    assert_eq!(sim.world().world_hash(), reference_world_hash(&sim));

    for step in 0..60 {
        match rng.below(5) {
            // A cut somewhere on the floor (it may span a brick or chunk boundary).
            0..=2 => {
                let (x, y, z) = (
                    2 + rng.below(SIDE as u64 * 32 - 4),
                    1 + rng.below(80),
                    2 + rng.below(SIDE as u64 * 32 - 4),
                );
                let request = RequestId(next_request);
                next_request += 1;
                sim.submit(EditIntent::cut(
                    request,
                    EntityId::new(1).unwrap(),
                    EditTarget::Terrain,
                    brush_cell(x, y, z, 2 + rng.below(5)),
                ))
                .unwrap();
                sim.run_until_idle(24).unwrap();
            }
            // Evict a resident brick: its digest moves to the retained set.
            _ => {
                let resident = sim
                    .world()
                    .volume_ref(terrain)
                    .unwrap()
                    .resident_brick_coords();
                let coord = resident[rng.below(resident.len() as u64) as usize];
                sim.world_mut().evict_brick(terrain, coord).ok();
            }
        }
        // Several mutations may pass between hash calls; the cache must not care.
        if step % 3 == 0 {
            assert_eq!(
                sim.world().world_hash(),
                reference_world_hash(&sim),
                "step {step}"
            );
            assert_eq!(
                sim.world().volume_hash(terrain).unwrap(),
                canonical_topology_hash(&[sim.world().canonical_volume(terrain).unwrap()]),
                "step {step}"
            );
        }
    }
    assert_eq!(sim.world().world_hash(), reference_world_hash(&sim));
}

#[test]
fn a_split_that_creates_bodies_hashes_the_same_cached_or_from_scratch() {
    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup())).unwrap();
    assert_eq!(sim.world().world_hash(), reference_world_hash(&sim));
    sim.submit(EditIntent::cut(
        RequestId(1),
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        brush_cell(10, 4, 1, 2),
    ))
    .unwrap();
    sim.run_until_idle(24).unwrap();
    assert_eq!(sim.world().body_count(), 1, "the beam detached");
    assert_eq!(sim.world().world_hash(), reference_world_hash(&sim));
    // And the transaction's own result hashes are the cached hashes of the resulting volumes.
    let committed = sim.committed(RequestId(1)).unwrap();
    for result in &committed.topology.result_hashes {
        assert_eq!(sim.world().volume_hash(result.volume).unwrap(), result.hash);
    }
}

#[test]
fn a_second_world_with_the_same_contents_hashes_the_same() {
    let (a, b) = (floor_sim(), floor_sim());
    assert_eq!(a.world().world_hash(), b.world().world_hash());
}
