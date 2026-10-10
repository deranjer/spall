//! A player's collision window is rebuilt only when a terrain edit can reach it. Before this,
//! every committed terrain edit anywhere bumped one global revision and every player rebuilt
//! its window (about 100 ms each on a full-size world), which alone held the server at a
//! fraction of its tick rate with eight connected players.

use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{
    BrickCoord, CellSizeCode, EntityId, GlobalCell, MaterialId, Revision, SphereBrush, VolumeId,
    player_entity_for,
};
use spall_protocol::RequestId;
use spall_sim::{EditIntent, EditTarget, Simulation, SimulationConfig, fixtures};
use spall_voxel::{Brick, Volume};

/// Bricks per side of the floor; it is 3 bricks (24 m) thick, so its top is at y = 24 m.
const SIDE: i64 = 12;

fn floor_sim() -> Simulation {
    let id = VolumeId::new(1).unwrap();
    let mut volume = Volume::new(id, CellSizeCode::Quarter);
    for z in 0..SIDE {
        for x in 0..SIDE {
            // Three bricks of stone, then two of resident air so a window above the floor is
            // built entirely from authored cells.
            for y in 0..5 {
                let material = if y < 3 {
                    fixtures::STONE
                } else {
                    MaterialId::AIR
                };
                volume
                    .insert_brick(
                        BrickCoord::new(x, y, z),
                        Brick::uniform(material, Revision(1)),
                    )
                    .unwrap();
            }
        }
    }
    let mut setup = fixtures::flat_terrain_setup();
    setup.terrain = volume;
    setup.terrain_collider_region = (
        GlobalCell::new(0, 0, 0),
        GlobalCell::new(SIDE * 32 - 1, 5 * 32 - 1, SIDE * 32 - 1),
    );
    setup.physics.disable_ccd = true;
    Simulation::new(SimulationConfig::new(setup)).unwrap()
}

fn brush_at_cell(x: i64, y: i64, z: i64, radius_cells: i64) -> SphereBrush {
    let h = BRUSH_UNIT / 2;
    SphereBrush::new(
        BrushPoint::from_units(x * BRUSH_UNIT + h, y * BRUSH_UNIT + h, z * BRUSH_UNIT + h),
        radius_cells * BRUSH_UNIT,
    )
    .unwrap()
}

fn cut(sim: &mut Simulation, request: u64, cell: (i64, i64, i64)) {
    sim.submit(EditIntent::cut(
        RequestId(request),
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        brush_at_cell(cell.0, cell.1, cell.2, 2),
    ))
    .unwrap();
    sim.run_until_idle(24).unwrap();
    for _ in 0..3 {
        sim.tick().unwrap();
    }
}

#[test]
fn an_edit_far_from_a_player_does_not_rebuild_its_window_but_a_near_one_does() {
    let mut sim = floor_sim();
    let player = player_entity_for(0);
    // Mid-floor, feet on the 24 m surface (cell y 96).
    sim.add_player(player, [48.0, 24.1, 48.0]);
    for _ in 0..5 {
        sim.tick().unwrap();
    }
    let built = sim.world().window_stats();
    assert!(
        built.window_rebuilds >= 1,
        "the arena is large enough for the player to have a window: {built:?}"
    );

    // About 40 m from the player, far outside a 4 m window.
    cut(&mut sim, 1, (360, 94, 360));
    let after_far = sim.world().window_stats();
    assert_eq!(
        after_far.window_rebuilds, built.window_rebuilds,
        "a distant edit must not rebuild the window"
    );

    // Right under the player.
    cut(&mut sim, 2, (192, 94, 192));
    let after_near = sim.world().window_stats();
    assert!(
        after_near.window_rebuilds > after_far.window_rebuilds,
        "an edit inside the window must rebuild it: {after_far:?} -> {after_near:?}"
    );
}
