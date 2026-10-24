//! Creature death: a strike finds the creature from its posed voxels, and a
//! kill despawns it and leaves those voxels as ordinary, editable terrain.
use glam::{DQuat, DVec3};
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{EntityId, GlobalCell, SphereBrush};
use spall_protocol::{CreatureRecord, RequestId};
use spall_sim::intent::{EditIntent, EditTarget};
use spall_sim::{CreatureShape, CreatureVoxel, Simulation, SimulationConfig, fixtures};
use spall_voxel::Sample;

const EDGE_M: f64 = 0.0625;

/// A 1 m x 0.25 m x 0.25 m slab along the creature's local X, its underside at
/// the root: a stand-in for an animal's body with no legs.
fn slab_shape() -> CreatureShape {
    CreatureShape::new(|record: &CreatureRecord| {
        let [x, y, z, w] = record.rotation_xyzw;
        let rotation = DQuat::from_xyzw(x.into(), y.into(), z.into(), w.into());
        let root = DVec3::from_array(record.translation_m);
        let mut voxels = Vec::new();
        for i in 0..16 {
            for j in 0..4 {
                for k in 0..4 {
                    let local = DVec3::new(
                        -0.5 + (f64::from(i) + 0.5) * EDGE_M,
                        (f64::from(j) + 0.5) * EDGE_M,
                        -0.125 + (f64::from(k) + 0.5) * EDGE_M,
                    );
                    voxels.push(CreatureVoxel {
                        centre_m: (root + rotation * local).to_array(),
                        edge_m: EDGE_M as f32,
                        material: fixtures::STONE,
                    });
                }
            }
        }
        voxels
    })
}

fn sim_with_creature() -> Simulation {
    let mut sim = Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();
    sim.initialize_creature_host().unwrap();
    sim.set_creature_shape(slab_shape()).unwrap();
    // Turned a quarter about +Y, so its length runs along world Z; standing
    // 1 m above the ground (top of the slab floor is y = 0.5 m).
    let half = std::f32::consts::FRAC_PI_4;
    sim.spawn_creatures(vec![CreatureRecord {
        creature_id: 1,
        species: "slab".into(),
        translation_m: [3.0, 1.5, 3.0],
        rotation_xyzw: [0.0, half.sin(), 0.0, half.cos()],
        locomotion_tag: 0,
        clip_phase_tick: 0,
    }])
    .unwrap();
    sim
}

fn run_until_idle(sim: &mut Simulation) {
    // Off-thread staging finishes in wall time, so pace the ticks a little.
    for _ in 0..2000 {
        sim.tick().unwrap();
        if sim.is_idle() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    panic!("edits never finished");
}

fn filled(sim: &Simulation, cell: GlobalCell) -> bool {
    matches!(
        sim.world().terrain().volume.sample(cell),
        Ok(Sample::Filled(_))
    )
}

#[test]
fn a_strike_hits_the_posed_voxels_only() {
    let sim = sim_with_creature();
    // Along +X at body height: the slab is 0.25 m thick in X after the turn.
    let (id, t) = sim
        .creature_struck([0.5, 1.6, 3.4], [1.0, 0.0, 0.0], 12.0)
        .expect("the ray crosses the slab");
    assert_eq!(id, 1);
    assert!((2.3..2.4).contains(&t), "hit at {t} m");
    // Past the slab's end in Z, short of reach, or above it: no hit.
    assert!(
        sim.creature_struck([0.5, 1.6, 3.6], [1.0, 0.0, 0.0], 12.0)
            .is_none()
    );
    assert!(
        sim.creature_struck([0.5, 1.6, 3.4], [1.0, 0.0, 0.0], 2.0)
            .is_none()
    );
    assert!(
        sim.creature_struck([0.5, 2.0, 3.0], [1.0, 0.0, 0.0], 12.0)
            .is_none()
    );
}

#[test]
fn a_kill_leaves_the_voxels_on_the_ground_as_editable_terrain() {
    let mut sim = sim_with_creature();
    let actor = EntityId::new(9).unwrap();
    let kill = sim.kill_creature(1, Some(RequestId(7)), actor).unwrap();
    assert!(sim.creature_records().is_empty(), "the creature is gone");
    assert_eq!(kill.record.creature_id, 1);

    // Quantised to 0.25 m: two cells across X (2.875..3.125 m), four along Z
    // (2.5..3.5 m), one high, dropped from y = 1.5 m onto the floor (cell 2).
    let mut expected = Vec::new();
    for z in 10..14 {
        for x in 11..13 {
            expected.push(GlobalCell::new(x, 2, z));
        }
    }
    let mut placed: Vec<GlobalCell> = kill.carcass.iter().map(|(cell, _)| *cell).collect();
    placed.sort_by_key(|c| (c.x, c.y, c.z));
    expected.sort_by_key(|c| (c.x, c.y, c.z));
    assert_eq!(placed, expected);
    assert!(kill.carcass.iter().all(|(_, m)| *m == fixtures::STONE));

    run_until_idle(&mut sim);
    assert!(
        sim.committed(RequestId(7)).is_some(),
        "the strike's request commits"
    );
    for cell in &expected {
        assert!(filled(&sim, *cell), "carcass cell {cell:?}");
    }
    assert!(
        !filled(&sim, GlobalCell::new(11, 6, 10)),
        "nothing left in the air"
    );

    // Cut one carcass cell away through the normal edit path.
    let target = GlobalCell::new(11, 2, 12);
    let h = BRUSH_UNIT / 2;
    let brush = SphereBrush::new(
        BrushPoint::from_units(
            target.x * BRUSH_UNIT + h,
            target.y * BRUSH_UNIT + h,
            target.z * BRUSH_UNIT + h,
        ),
        0,
    )
    .unwrap();
    sim.submit(EditIntent::cut(
        RequestId(8),
        actor,
        EditTarget::Terrain,
        brush,
    ))
    .unwrap();
    run_until_idle(&mut sim);
    assert!(sim.committed(RequestId(8)).is_some());
    assert!(!filled(&sim, target), "the cut cell is gone");
    assert!(
        filled(&sim, GlobalCell::new(12, 2, 12)),
        "its neighbour stays"
    );
}

#[test]
fn killing_needs_a_hosted_creature() {
    let mut sim = sim_with_creature();
    let actor = EntityId::new(9).unwrap();
    assert!(sim.kill_creature(2, None, actor).is_err());
    sim.kill_creature(1, None, actor).unwrap();
    assert!(sim.kill_creature(1, None, actor).is_err());
}

/// ENG-238: one creature kills another, then clears and fills single terrain
/// cells through committed server edits.
#[test]
fn a_creature_kills_another_and_edits_single_cells() {
    let mut sim = sim_with_creature();
    sim.spawn_creatures(vec![CreatureRecord {
        creature_id: 2,
        species: "slab".into(),
        translation_m: [4.5, 1.5, 4.5],
        rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
        locomotion_tag: 0,
        clip_phase_tick: 0,
    }])
    .unwrap();
    assert!(sim.kill_creature_by(1, 1).is_err(), "not itself");
    assert!(sim.kill_creature_by(3, 1).is_err(), "an unknown killer");
    let kill = sim.kill_creature_by(2, 1).unwrap();
    assert_eq!(sim.creature_records().len(), 1, "the prey is gone");
    run_until_idle(&mut sim);
    let (cell, material) = kill.carcass[0];
    assert!(filled(&sim, cell));

    // Clearing a solid cell queues a cut; clearing it again is a no-op.
    assert_eq!(sim.creature_edit_cell(2, cell, None), Ok(true));
    run_until_idle(&mut sim);
    assert!(!filled(&sim, cell), "the bitten cell is gone");
    assert_eq!(sim.creature_edit_cell(2, cell, None), Ok(false));

    // Filling an empty cell places; filling a solid one changes nothing.
    assert_eq!(sim.creature_edit_cell(2, cell, Some(material)), Ok(true));
    run_until_idle(&mut sim);
    assert!(filled(&sim, cell), "the placed cell is there");
    assert_eq!(sim.creature_edit_cell(2, cell, Some(material)), Ok(false));

    // Only a hosted creature edits.
    assert!(sim.creature_edit_cell(1, cell, None).is_err());
}

/// Local terrain cells of a four-legged animal, one 0.25 m voxel per cell:
/// legs two cells high under a 6 x 3 body, a neck and head sticking out ahead
/// of the front legs, and a tail behind. Its body and head are held up only
/// through the legs.
fn legged_cells() -> Vec<[i64; 3]> {
    let mut cells = Vec::new();
    for (x, z) in [(1, 0), (1, 2), (4, 0), (4, 2)] {
        for y in 0..2 {
            cells.push([x, y, z]);
        }
    }
    for x in 0..6 {
        for z in 0..3 {
            cells.push([x, 2, z]);
        }
    }
    // Neck and head ahead (-X), tail behind (+X).
    cells.extend([[-1, 2, 1], [-1, 3, 1], [-2, 3, 1], [-2, 4, 1], [-3, 4, 1]]);
    cells.push([6, 2, 1]);
    cells
}

fn legged_shape() -> CreatureShape {
    CreatureShape::new(|record: &CreatureRecord| {
        let root = DVec3::from_array(record.translation_m);
        legged_cells()
            .into_iter()
            .map(|[x, y, z]| CreatureVoxel {
                centre_m: (root + (DVec3::new(x as f64, y as f64, z as f64) + 0.5) * 0.25)
                    .to_array(),
                edge_m: 0.25,
                material: fixtures::STONE,
            })
            .collect()
    })
}

fn legged_kill_lands_as_terrain(off_thread: bool) {
    let mut sim = Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();
    if off_thread {
        sim.enable_off_thread_staging();
    }
    sim.initialize_creature_host().unwrap();
    sim.set_creature_shape(legged_shape()).unwrap();
    // Root corner at cell (8, 3, 10): its feet hover one cell above the floor
    // (top solid cell y = 1), so the carcass drops one cell.
    sim.spawn_creatures(vec![CreatureRecord {
        creature_id: 1,
        species: "legged".into(),
        translation_m: [2.0, 0.75, 2.5],
        rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
        locomotion_tag: 0,
        clip_phase_tick: 0,
    }])
    .unwrap();
    let bodies_before = sim.world().body_count();
    let kill = sim
        .kill_creature(1, Some(RequestId(7)), EntityId::new(9).unwrap())
        .unwrap();
    assert_eq!(kill.carcass.len(), legged_cells().len());
    assert_eq!(
        kill.first_status.as_ref().map(|s| s.request_id),
        Some(RequestId(7))
    );
    run_until_idle(&mut sim);
    assert!(sim.committed(RequestId(7)).is_some(), "the strike commits");
    let missing = kill
        .carcass
        .iter()
        .filter(|(cell, _)| !filled(&sim, *cell))
        .count();
    let new_bodies = sim.world().body_count() - bodies_before;
    assert!(
        missing == 0 && new_bodies == 0,
        "{missing} of {} carcass cells are not terrain; {new_bodies} new bodies",
        kill.carcass.len()
    );
    for [x, y, z] in legged_cells() {
        assert!(filled(&sim, GlobalCell::new(8 + x, 2 + y, 10 + z)));
    }
}

/// ENG-242: a carcass held up through its legs lands as connected terrain,
/// not as loose single-cell bodies, whatever order its cells commit in.
#[test]
fn a_legged_carcass_lands_as_connected_terrain() {
    legged_kill_lands_as_terrain(false);
}

/// The same with staging on its worker thread, as the server runs it.
#[test]
fn a_legged_carcass_lands_as_connected_terrain_off_thread() {
    legged_kill_lands_as_terrain(true);
}
