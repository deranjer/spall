use std::time::Instant;

use spall_physics::fixtures::{CELL_M, STONE_DENSITY, debris_pieces};
use spall_physics::{
    BodyKind, BodySpec, OccupancyGrid, PhysicsConfig, PhysicsOrigin, PhysicsRegionSet,
    Representation,
};

const REGIONS: u32 = 8;
const BODIES_PER_REGION: usize = 8;
const STEPS: usize = 120;
const REGION_SPACING_M: f64 = 100_000.0;
const TERRAIN_CELLS_PER_REGION: u64 = 256 * 32;

fn main() -> std::process::ExitCode {
    let (_, volume) = debris_pieces(903, 1, 4)
        .pop()
        .expect("64-cell body fixture");
    let grid = OccupancyGrid::from_volume(&volume)
        .expect("occupancy extraction")
        .expect("solid body");
    let cells_per_body = grid.solid_count();
    let mut regions = PhysicsRegionSet::new();
    let mut body_ids = Vec::new();
    for region in 0..REGIONS {
        regions
            .create_region(
                region,
                PhysicsOrigin::new([f64::from(region) * REGION_SPACING_M, 0.0, 0.0]).unwrap(),
                PhysicsConfig::default(),
            )
            .unwrap();
        regions
            .add_body(
                region,
                floor_spec(),
                [f64::from(region) * REGION_SPACING_M, 0.0, 0.0],
            )
            .unwrap();
        for body in 0..BODIES_PER_REGION {
            let position = [
                f64::from(region) * REGION_SPACING_M + body as f64 * 8.0,
                3.0,
                4.0,
            ];
            body_ids.push(
                regions
                    .add_body(region, body_spec(grid.clone()), position)
                    .unwrap(),
            );
        }
    }
    let started = Instant::now();
    let mut finite = true;
    for _ in 0..STEPS {
        regions.step_all();
        for id in &body_ids {
            finite &= regions
                .state(*id)
                .is_ok_and(|state| state.local.is_finite());
        }
    }
    let elapsed = started.elapsed();
    let bodies = REGIONS as usize * BODIES_PER_REGION;
    let exact_population = (0..REGIONS)
        .all(|region| regions.active_body_count(region).ok() == Some(BODIES_PER_REGION + 1));
    let contact_regions = (0..REGIONS)
        .filter(|region| {
            regions
                .contact_pair_count(*region)
                .is_ok_and(|pairs| pairs >= BODIES_PER_REGION)
        })
        .count();
    let passed = finite && exact_population && contact_regions == REGIONS as usize;
    println!(
        "{{\"scenario\":\"bounded_rebased_region_debris_envelope\",\"passed\":{passed},\
         \"physics_regions\":{REGIONS},\"regions_spacing_m\":{REGION_SPACING_M},\
         \"active_dynamic_bodies\":{bodies},\"terrain_colliders\":{REGIONS},\"solid_voxel_cells\":{},\"steps_per_region\":{STEPS},\
         \"elapsed_ms\":{},\"elapsed_per_step_ms\":{},\"finite\":{finite},\
         \"exact_population\":{exact_population},\"regions_with_body_contacts\":{contact_regions}}}",
        cells_per_body * bodies as u64 + TERRAIN_CELLS_PER_REGION * u64::from(REGIONS),
        elapsed.as_secs_f64() * 1_000.0,
        elapsed.as_secs_f64() * 1_000.0 / STEPS as f64,
    );
    if passed {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

fn floor_spec() -> BodySpec {
    BodySpec {
        kind: BodyKind::Fixed,
        representation: Representation::MergedCuboids,
        grid: OccupancyGrid::from_solid_mask(
            spall_core::GlobalCell::new(0, 0, 0),
            [256, 1, 32],
            vec![true; 256 * 32],
            vec![spall_core::MaterialId(1); 256 * 32],
        )
        .expect("bounded region floor"),
        cell_m: CELL_M,
        density_kg_m3: 1.0,
        mass_properties: None,
        translation_m: [0.0; 3],
        linvel_m_s: [0.0; 3],
    }
}

fn body_spec(grid: OccupancyGrid) -> BodySpec {
    BodySpec {
        kind: BodyKind::Dynamic { ccd: false },
        representation: Representation::MergedCuboids,
        grid,
        cell_m: CELL_M,
        density_kg_m3: STONE_DENSITY,
        mass_properties: None,
        translation_m: [0.0; 3],
        linvel_m_s: [0.0; 3],
    }
}
