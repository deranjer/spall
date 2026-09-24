use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, PlayerInput, VolumeId};
use spall_physics::{
    CharacterParams, CharacterQueryCache, CharacterState, PhysicsConfig, PhysicsOrigin,
    PhysicsRegionSet, step_character,
};
use spall_voxel::{Brick, EditPlan, Volume};

const TICKS: usize = 60;
const DT: f32 = 1.0 / 60.0;
const CELL_M: f32 = 0.25;
const SEPARATION_M: f64 = 100_000.0;

fn main() -> std::process::ExitCode {
    let volume_id = VolumeId::new(1).expect("valid volume ID");
    let mut terrain = Volume::new(volume_id, CellSizeCode::Quarter);
    let mut slabs = EditPlan::new(volume_id);
    let far_cell_x = (SEPARATION_M / f64::from(CELL_M)) as i64;
    for (x_min, x_max) in [(-1, 2), (12_499, 12_502)] {
        for y in -1..=1 {
            for z in 0..=1 {
                for x in x_min..=x_max {
                    terrain
                        .insert_brick(BrickCoord::new(x, y, z), Brick::empty())
                        .expect("resident empty query-window brick");
                }
            }
        }
    }
    for z in -16..=16 {
        for x in -16..=64 {
            slabs.set(GlobalCell::new(x, 0, z), MaterialId(1));
            slabs.set(GlobalCell::new(far_cell_x + x, 0, z), MaterialId(1));
        }
    }
    terrain
        .apply_edit(&slabs)
        .expect("build two small floor patches");

    let mut regions = PhysicsRegionSet::new();
    regions
        .create_region(10, PhysicsOrigin::ZERO, PhysicsConfig::default())
        .unwrap();
    regions
        .create_region(
            11,
            PhysicsOrigin::new([SEPARATION_M, 0.0, 0.0]).unwrap(),
            PhysicsConfig::default(),
        )
        .unwrap();
    let mut caches = [
        CharacterQueryCache::default(),
        CharacterQueryCache::default(),
    ];
    let starts = [1.0, SEPARATION_M + 1.0];
    let mut states = [
        CharacterState::at([starts[0], 0.25, 4.0]),
        CharacterState::at([starts[1], 0.25, 4.0]),
    ];
    let input = PlayerInput {
        movement: [0.0, 0.0, 1.0],
        view_dir: [1.0, 0.0, 0.0],
        buttons: 0,
    };
    let mut grounded_ticks = [0usize; 2];
    let mut finite = true;
    for _ in 0..TICKS {
        regions.step_all();
        for player in 0..2 {
            let region = 10 + player as u32;
            let state = states[player];
            states[player] = step_character(state, input, DT, |position, desired| {
                regions
                    .sweep_character(
                        region,
                        &mut caches[player],
                        &terrain,
                        CELL_M,
                        CharacterParams::DEFAULT,
                        position,
                        desired,
                        DT,
                        1,
                        &[],
                    )
                    .expect("regional terrain window and sweep")
                    .0
            });
            grounded_ticks[player] += usize::from(states[player].grounded);
            finite &= states[player].is_finite();
        }
    }
    let near_travel = states[0].position_m[0] - starts[0];
    let far_travel = states[1].position_m[0] - starts[1];
    let travel_delta = far_travel - near_travel;
    let passed =
        finite && grounded_ticks[0] > 0 && grounded_ticks[1] > 0 && travel_delta.abs() < 0.002;
    println!(
        "{{\"scenario\":\"separated_players_rebased_region_sweeps\",\"passed\":{passed},\
         \"separation_m\":{SEPARATION_M},\"ticks\":{TICKS},\"near_player_travel_m\":{near_travel},\
         \"far_player_travel_m\":{far_travel},\"travel_delta_m\":{travel_delta},\
         \"near_grounded_ticks\":{},\"far_grounded_ticks\":{},\"finite\":{finite}}}",
        grounded_ticks[0], grounded_ticks[1],
    );
    if passed {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}
