//! Exercise the candidate gate against a small real generated world and live water.

use std::sync::Arc;

use glam::DVec3;
use sandbox::animal_habitat::{
    AnimalHabitat, CandidateEnvelope, RejectReason, ValidationLimits, validate_candidate,
};
use spall_core::{GlobalCell, MaterialId};
use spall_fluid::columns::{ColumnConfig, ColumnWorld, VolumeTerrain};
use spall_voxel::{EditPlan, Sample};

#[test]
fn generated_habitats_accept_each_category_and_reject_lost_water_or_support() {
    let scene = sandbox::worldgen_scene::generate("gallery", 1, 256).unwrap();
    let generated = scene.world();
    let terrain = &generated.terrain;
    let source = VolumeTerrain::new(
        Arc::new(terrain.clone()),
        generated.region.0.y as i32,
        (generated.region.1.y + 1) as i32,
    );
    let mut water = ColumnWorld::with_terrain(Arc::new(source), ColumnConfig::default());
    let levels: Vec<_> = generated
        .water
        .columns
        .iter()
        .map(|c| {
            (
                i64::from(c.x),
                i64::from(c.z),
                i64::from(c.bed),
                i64::from(c.top),
            )
        })
        .collect();
    assert!(water.seed_levels(&levels).is_empty());
    let limits = ValidationLimits::default();

    // The trout's authored rest envelope; candidates still use current water.
    let fish = generated
        .water
        .columns
        .iter()
        .take(4096)
        .find_map(|c| {
            let candidate = CandidateEnvelope {
                center_m: DVec3::new(
                    (f64::from(c.x) + 0.5) * 0.25,
                    f64::from(c.top + 1) * 0.25 - 0.5,
                    (f64::from(c.z) + 0.5) * 0.25,
                ),
                size_m: DVec3::new(0.9375, 0.4375, 0.4375),
            };
            validate_candidate(
                terrain,
                Some(&water),
                AnimalHabitat::Aquatic,
                candidate,
                limits,
            )
            .ok()
            .map(|_| candidate)
        })
        .expect("gallery must offer a bounded deep-water trout candidate");
    // Drain one real solver column inside its footprint, without changing biome data.
    water.fill_column_to(
        (fish.center_m.x / 0.25).floor() as i64,
        (fish.center_m.z / 0.25).floor() as i64,
        f64::from(generated.region.0.y as i32),
    );
    assert_eq!(
        validate_candidate(terrain, Some(&water), AnimalHabitat::Aquatic, fish, limits),
        Err(RejectReason::WaterTooShallow)
    );

    let deer = scene
        .player_spawns()
        .iter()
        .take(16)
        .find_map(|spawn| {
            let x = (spawn[0] / 0.25).floor() as i64;
            let z = (spawn[2] / 0.25).floor() as i64;
            let start_y = (spawn[1] / 0.25).floor() as i64;
            (start_y - 12..=start_y).rev().find_map(|y| {
                if !matches!(
                    terrain.sample(GlobalCell::new(x, y, z)),
                    Ok(Sample::Filled(_))
                ) {
                    return None;
                }
                let candidate = CandidateEnvelope {
                    center_m: DVec3::new(
                        (x as f64 + 0.5) * 0.25,
                        (y + 1) as f64 * 0.25 + 1.1875 * 0.5,
                        (z as f64 + 0.5) * 0.25,
                    ),
                    size_m: DVec3::new(0.875, 1.1875, 0.1875),
                };
                validate_candidate(terrain, None, AnimalHabitat::Land, candidate, limits)
                    .ok()
                    .map(|_| candidate)
            })
        })
        .expect("gallery must offer a grounded deer candidate near a spawn");
    let crow = CandidateEnvelope {
        center_m: deer.center_m + DVec3::Y * 4.0,
        size_m: DVec3::new(0.875, 0.625, 0.8125),
    };
    assert!(validate_candidate(terrain, None, AnimalHabitat::Flying, crow, limits).is_ok());

    let mut edited = terrain.clone();
    let foot = deer.center_m - deer.size_m * 0.5;
    let support = GlobalCell::new(
        (deer.center_m.x / 0.25).floor() as i64,
        (foot.y / 0.25).round() as i64 - 1,
        (deer.center_m.z / 0.25).floor() as i64,
    );
    let mut cut = EditPlan::new(edited.id());
    cut.set(support, MaterialId::AIR);
    edited.apply_edit(&cut).unwrap();
    assert_eq!(
        validate_candidate(&edited, None, AnimalHabitat::Land, deer, limits),
        Err(RejectReason::MissingSupport)
    );
}
