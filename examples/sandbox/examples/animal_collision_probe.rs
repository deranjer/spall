//! Bounded CPU feasibility probe for exact articulated animal colliders.
//!
//! Run with `cargo run -p sandbox --release --example animal-collision-probe -- --bounded`.
//! The source-cell ray oracle below intersects each authored occupied cell
//! independently; it does not call the production hit-mapping helper.

use std::{
    collections::BTreeMap,
    env,
    time::{Duration, Instant},
};

use glam::{Quat, Vec3};
use serde::Serialize;
use spall_asset::{Asset, Part, Transform, decode, sample_clip};
use spall_core::{GlobalCell, MaterialId, VolumeId};
use spall_physics::{
    ArticulatedBodyId, ArticulatedPose, BodyKind, BodySpec, OccupancyGrid, PartTransform,
    PhysicsConfig, PhysicsWorld, Representation, SourcePart, build_collider,
};

const CELL_M: f32 = 0.0625;
const TOPOLOGY_TOKEN: u64 = 0x209;
const REPEATS: usize = 100;
const WARMUP: usize = 10;
const ANIMALS: [(&str, &[u8]); 3] = [
    ("trout", include_bytes!("../assets/animals/trout.spvox")),
    ("crow", include_bytes!("../assets/animals/crow.spvox")),
    ("deer", include_bytes!("../assets/animals/deer.spvox")),
];

#[derive(Clone)]
struct PartGeometry {
    part: Part,
    grid: OccupancyGrid,
    cells: Vec<[i32; 3]>,
}

#[derive(Clone)]
struct Animal {
    asset: Asset,
    parts: Vec<PartGeometry>,
    clips: Vec<(String, Vec<f64>)>,
    pivot_m: Vec3,
    cell_count: usize,
}

#[derive(Clone, Copy)]
struct RefCell {
    part_id: u32,
    cell: [i32; 3],
    center: Vec3,
    rotation: Quat,
}

struct AssemblyTokens {
    topology: u64,
    pose: u64,
}

#[derive(Debug, Clone, Copy)]
struct RayCase {
    part_id: u32,
    origin: Vec3,
    direction: Vec3,
}

#[derive(Serialize)]
struct Summary {
    p50_us: f64,
    p95_us: f64,
    p99_us: f64,
}

#[derive(Serialize)]
struct CaseResult {
    animal: String,
    representation: &'static str,
    solver_substeps: usize,
    clips: usize,
    sampled_phases: usize,
    source_cells: usize,
    source_parts: usize,
    negative_origin_parts: usize,
    sampled_clip_phases: Vec<String>,
    body_count: usize,
    collider_count: usize,
    collider_primitives: usize,
    repeats: usize,
    warmup: usize,
    initial_attachment: Summary,
    source_cell_rebuild: Summary,
    pose_refresh_ray_step: Summary,
    terrain_transition_sweeps: Option<TerrainTransitionSummary>,
    timed_ray_queries_per_pose: usize,
    ray_checks: usize,
    miss_checks: usize,
    ray_mismatches: usize,
    timed_ray_miss_count: usize,
    timed_ray_misses: Vec<String>,
}

#[derive(Serialize)]
struct TerrainTransitionSummary {
    same_clip_transitions: usize,
    accepted: usize,
    blocked: usize,
    complete_transition_query_step: Summary,
    cases: Vec<TerrainTransitionCase>,
}

#[derive(Serialize)]
struct TerrainTransitionCase {
    clip: String,
    from_tick: f64,
    to_tick: f64,
    blocked: bool,
    hit_part_id: Option<u32>,
    hit_time_of_impact: Option<f32>,
    independent_clearance_proven: bool,
    complete_update_query_step_us: f64,
    source_part_count: usize,
    terrain_collider_count: usize,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = env::args().skip(1).collect();
    let kinematic_parts = args.iter().any(|arg| arg == "--kinematic-parts");
    let solver_substeps = args
        .iter()
        .find_map(|arg| arg.strip_prefix("--solver-substeps="))
        .map(str::parse::<usize>)
        .transpose()?
        .unwrap_or(1);
    if args.iter().filter(|arg| *arg == "--bounded").count() != 1
        || args.iter().any(|arg| {
            arg != "--bounded"
                && arg != "--kinematic-parts"
                && !arg.starts_with("--solver-substeps=")
        })
        || !(1..=64).contains(&solver_substeps)
        || (solver_substeps > 1 && !kinematic_parts)
    {
        return Err(
            "usage: animal-collision-probe --bounded [--kinematic-parts] [--solver-substeps=1..64]"
                .into(),
        );
    }
    let mut results = Vec::new();
    for (name, bytes) in ANIMALS {
        let animal = load_animal(decode(bytes)?)?;
        for representation in [Representation::NativeVoxels, Representation::MergedCuboids] {
            results.push(run_case(
                name,
                &animal,
                representation,
                kinematic_parts,
                solver_substeps,
            )?);
        }
    }
    let transition_representation_disagreements =
        validate_transition_parity(&results, kinematic_parts)?;
    let mismatch_count = results
        .iter()
        .map(|result| result.ray_mismatches)
        .sum::<usize>();
    let output = serde_json::json!({
        "scenario": if kinematic_parts { "eng210_actual_animal_kinematic_parts_bounded" } else { "eng209_actual_animal_collision_bounded" },
        "rig": if kinematic_parts { "one_position_kinematic_body_per_source_part" } else { "single_position_kinematic_root_with_part_colliders" },
        "physics": "same spall_physics PhysicsWorld Rapier adapter",
        "source": "authored SPVX trout/crow/deer, exact 6.25 cm cells, all clips/phases",
        "terrain_transition_contract": if kinematic_parts {
            serde_json::Value::String("continuous casts of every occupied source-cell Cuboid; linear part-body origin and shortest-arc rotation; 10 degree root yaw; bounded by one million cell-terrain casts".to_owned())
        } else {
            serde_json::Value::Null
        },
        "transition_representation_disagreements": transition_representation_disagreements,
        "solver_substeps": solver_substeps,
        "hardware": hardware(),
        "results": results,
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    if mismatch_count != 0 {
        return Err(format!(
            "independent exact-cell oracle reported {mismatch_count} mismatch(es)"
        )
        .into());
    }
    Ok(())
}

fn validate_transition_parity(
    results: &[CaseResult],
    kinematic_parts: bool,
) -> Result<usize, Box<dyn std::error::Error>> {
    if !kinematic_parts {
        return Ok(0);
    }
    let mut disagreement_count = 0;
    for animal in ["trout", "crow", "deer"] {
        let native = results
            .iter()
            .find(|result| result.animal == animal && result.representation == "native_voxels")
            .and_then(|result| result.terrain_transition_sweeps.as_ref())
            .ok_or_else(|| format!("missing NativeVoxels transition matrix for {animal}"))?;
        let merged = results
            .iter()
            .find(|result| result.animal == animal && result.representation == "merged_cuboids")
            .and_then(|result| result.terrain_transition_sweeps.as_ref())
            .ok_or_else(|| format!("missing MergedCuboids transition matrix for {animal}"))?;
        if native.cases.len() != merged.cases.len() {
            return Err(format!("transition matrix length differs for {animal}").into());
        }
        for (native_case, merged_case) in native.cases.iter().zip(&merged.cases) {
            if native_case.clip != merged_case.clip
                || native_case.from_tick != merged_case.from_tick
                || native_case.to_tick != merged_case.to_tick
            {
                return Err(format!("transition matrix ordering differs for {animal}").into());
            }
            if native_case.blocked != merged_case.blocked {
                disagreement_count += 1;
            }
        }
    }
    if disagreement_count != 0 {
        return Err(format!(
            "continuous source-cell transition casts disagree between collider representations in {disagreement_count} cases"
        )
        .into());
    }
    for &(from_tick, to_tick) in &[(0.0, 12.0), (24.0, 36.0)] {
        for representation in ["native_voxels", "merged_cuboids"] {
            let accepted = results.iter().any(|result| {
                result.animal == "deer"
                    && result.representation == representation
                    && result
                        .terrain_transition_sweeps
                        .as_ref()
                        .is_some_and(|summary| {
                            summary.cases.iter().any(|case| {
                                case.clip == "walk"
                                    && case.from_tick == from_tick
                                    && case.to_tick == to_tick
                                    && !case.blocked
                            })
                        })
            });
            if !accepted {
                return Err(format!(
                    "deer walk {from_tick}->{to_tick} must be accepted by cell-union sweep for {representation}"
                )
                .into());
            }
        }
    }
    Ok(disagreement_count)
}

fn load_animal(asset: Asset) -> Result<Animal, Box<dyn std::error::Error>> {
    if asset.cell_size_code != 1 {
        return Err(format!("{} is not authored at 6.25 cm", asset.name).into());
    }
    let mut cells_by_part: BTreeMap<u32, Vec<[i32; 3]>> = asset
        .parts
        .iter()
        .map(|part| (part.id, Vec::new()))
        .collect();
    for run in &asset.runs {
        let cells = cells_by_part
            .get_mut(&run.part_id)
            .ok_or("SPVX run references missing part")?;
        for dx in 0..run.length_x {
            cells.push([
                i32::try_from(i64::from(run.start[0]) + i64::from(dx))?,
                run.start[1],
                run.start[2],
            ]);
        }
    }
    let mut parts = Vec::with_capacity(asset.parts.len());
    let mut cell_count = 0;
    for part in &asset.parts {
        let cells = cells_by_part
            .remove(&part.id)
            .ok_or("missing part occupancy")?;
        if cells.is_empty() {
            return Err(format!("{} part {} has no source cells", asset.name, part.id).into());
        }
        let mut lo = [i32::MAX; 3];
        let mut hi = [i32::MIN; 3];
        for cell in &cells {
            for axis in 0..3 {
                lo[axis] = lo[axis].min(cell[axis]);
                hi[axis] = hi[axis].max(cell[axis]);
            }
        }
        let dims = [
            u32::try_from(i64::from(hi[0]) - i64::from(lo[0]) + 1)?,
            u32::try_from(i64::from(hi[1]) - i64::from(lo[1]) + 1)?,
            u32::try_from(i64::from(hi[2]) - i64::from(lo[2]) + 1)?,
        ];
        let volume = dims.iter().map(|d| *d as usize).product::<usize>();
        let mut solid = vec![false; volume];
        let mut materials = vec![spall_core::MaterialId(1); volume];
        for cell in &cells {
            let x = usize::try_from(cell[0] - lo[0])?;
            let y = usize::try_from(cell[1] - lo[1])?;
            let z = usize::try_from(cell[2] - lo[2])?;
            let index = x + dims[0] as usize * (y + dims[1] as usize * z);
            solid[index] = true;
        }
        // Material identity is deliberately irrelevant to collision, but every
        // occupied source cell receives an ordinary non-air slot.
        for (index, is_solid) in solid.iter().enumerate() {
            if !is_solid {
                materials[index] = spall_core::MaterialId::AIR;
            }
        }
        let grid = OccupancyGrid::from_solid_mask(
            GlobalCell::new(i64::from(lo[0]), i64::from(lo[1]), i64::from(lo[2])),
            dims,
            solid,
            materials,
        )?;
        cell_count += cells.len();
        parts.push(PartGeometry {
            part: part.clone(),
            grid,
            cells,
        });
    }
    parts.sort_by_key(|part| part.part.id);
    let clips = asset
        .clips
        .iter()
        .map(|clip| {
            let end = f64::from(clip.duration_ticks);
            (
                clip.name.clone(),
                vec![0.0, end * 0.25, end * 0.5, end * 0.75, end],
            )
        })
        .collect::<Vec<_>>();
    if clips.is_empty() {
        return Err(format!("{} has no authored animation clips", asset.name).into());
    }
    let pivot_m = Vec3::new(
        asset.pivot_subcells[0] as f32,
        asset.pivot_subcells[1] as f32,
        asset.pivot_subcells[2] as f32,
    ) * (CELL_M / 256.0);
    Ok(Animal {
        asset,
        parts,
        clips,
        pivot_m,
        cell_count,
    })
}

fn run_case(
    name: &str,
    animal: &Animal,
    representation: Representation,
    kinematic_parts: bool,
    solver_substeps: usize,
) -> Result<CaseResult, Box<dyn std::error::Error>> {
    let mut samples = pose_samples(animal)?;
    let root_translation = -animal.pivot_m;
    for sample in &mut samples {
        sample.ref_cells = reference_cells(animal, &sample.locals, root_translation)?;
        sample.rays = rays_for_cells(&sample.ref_cells);
    }
    let primitives = animal
        .parts
        .iter()
        .map(|part| build_collider(&part.grid, CELL_M, representation).primitives)
        .sum();
    let mut attachment_times = Vec::with_capacity(REPEATS);
    let mut source_rebuild_times = Vec::with_capacity(REPEATS * 2);
    let mut operation_times = Vec::with_capacity(REPEATS);
    let mut ray_checks = 0;
    let mut miss_checks = 0;
    let mut ray_mismatches = 0;
    let mut timed_ray_misses = Vec::new();
    let mut timed_ray_miss_count = 0;

    // World construction and SPVX decoding are outside timing. This measures
    // repeated initial attachment separately from live source-grid rebuilds.
    for iteration in 0..(WARMUP + REPEATS) {
        let mut rebuild_world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            solver_substeps,
            ..Default::default()
        });
        let mut elapsed = Vec::new();
        let _ = rebuild(
            &mut rebuild_world,
            animal,
            representation,
            root_translation,
            &mut elapsed,
            kinematic_parts,
        )?;
        if iteration >= WARMUP {
            attachment_times.push(elapsed[0]);
        }
    }

    // Exhaustively validate every source cell along three independent axes at
    // every sampled animation phase. The cell-union oracle is outside timing.
    let mut validation_world = PhysicsWorld::new(PhysicsConfig {
        gravity_m_s2: [0.0; 3],
        solver_substeps,
        ..Default::default()
    });
    let mut ignored = Vec::new();
    let validation_id = rebuild(
        &mut validation_world,
        animal,
        representation,
        root_translation,
        &mut ignored,
        kinematic_parts,
    )?;
    let mut pose_token = 0u64;
    for sample in &samples {
        pose_token += 1;
        let update = ArticulatedPose {
            topology_token: TOPOLOGY_TOKEN,
            base_pose_token: pose_token - 1,
            pose_token,
            parts: sample.locals.clone(),
        };
        if kinematic_parts {
            let transition = validation_world.update_articulated_pose_with_swept_root_pose(
                validation_id,
                update,
                PartTransform {
                    translation_m: root_translation.to_array(),
                    ..PartTransform::IDENTITY
                },
            ).map_err(|error| format!(
                "substep admission rejected sampled pose animal={} clip={} tick={} substeps={}: {error:?}",
                animal.asset.name, sample.clip, sample.tick, solver_substeps
            ))?;
            if transition.blocked {
                return Err(format!(
                    "terrain-free transition unexpectedly blocked animal={} clip={} tick={} part={:?} toi={:?}",
                    animal.asset.name,
                    sample.clip,
                    sample.tick,
                    transition.part_id,
                    transition.time_of_impact,
                )
                .into());
            }
        } else {
            validation_world.update_articulated_pose(validation_id, update)?;
        }
        if kinematic_parts {
            validation_world.step();
        }
        for ray in &sample.rays {
            let expected = reference_hit_cells(&sample.ref_cells, ray.origin, ray.direction, 8.0);
            let actual = validation_world.raycast_articulated(
                validation_id,
                ray.origin.to_array(),
                ray.direction.to_array(),
                8.0,
            );
            ray_checks += 1;
            if !same_hit(&actual, expected, TOPOLOGY_TOKEN, pose_token, 1) {
                eprintln!(
                    "oracle mismatch animal={} representation={} clip={} tick={} ray={:?} expected={:?} actual={:?}",
                    animal.asset.name,
                    representation.label(),
                    sample.clip,
                    sample.tick,
                    ray,
                    expected,
                    actual
                );
                ray_mismatches += 1;
            }
        }
        let miss = validation_world.raycast_articulated(
            validation_id,
            (root_translation + Vec3::splat(24.0)).to_array(),
            Vec3::X.to_array(),
            1.0,
        );
        miss_checks += 1;
        if miss.is_ok() {
            ray_mismatches += 1;
        }
    }

    let (rebuild_checks, rebuild_misses, rebuild_mismatches) = check_source_cell_removal(
        animal,
        representation,
        root_translation,
        &mut validation_world,
        validation_id,
        AssemblyTokens {
            topology: TOPOLOGY_TOKEN,
            pose: pose_token,
        },
        &samples.last().ok_or("no sampled phases")?.locals,
    )?;
    ray_checks += rebuild_checks;
    miss_checks += rebuild_misses;
    ray_mismatches += rebuild_mismatches;

    // Timed operation: one complete sampled-pose update, one independently
    // chosen ray per source part (raycast refreshes queries internally), and a
    // fixed step; then time a real source-cell removal and restoration.
    let mut world = PhysicsWorld::new(PhysicsConfig {
        gravity_m_s2: [0.0; 3],
        solver_substeps,
        ..Default::default()
    });
    let mut ignored = Vec::new();
    let id = rebuild(
        &mut world,
        animal,
        representation,
        root_translation,
        &mut ignored,
        kinematic_parts,
    )?;
    let edit_targets = animal
        .parts
        .iter()
        .map(|part| {
            if part.cells.len() < 2 {
                return Err(
                    format!("part {} cannot survive a one-cell removal", part.part.id).into(),
                );
            }
            let cell = part.cells[0];
            Ok((part.part.id, grid_without_cell(&part.grid, cell)?))
        })
        .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
    let mut topology_token = TOPOLOGY_TOKEN;
    let mut source_revisions = vec![1u64; animal.parts.len()];
    let mut timed_pose_token = 0u64;
    for iteration in 0..(WARMUP + REPEATS) {
        let sample = &samples[iteration % samples.len()];
        let timed_rays = representative_rays(&sample.ref_cells);
        timed_pose_token += 1;
        let before = Instant::now();
        let update = ArticulatedPose {
            topology_token,
            base_pose_token: timed_pose_token - 1,
            pose_token: timed_pose_token,
            parts: sample.locals.clone(),
        };
        if kinematic_parts {
            let transition = world.update_articulated_pose_with_swept_root_pose(
                id,
                update,
                PartTransform {
                    translation_m: root_translation.to_array(),
                    ..PartTransform::IDENTITY
                },
            )?;
            if transition.blocked {
                return Err(format!(
                    "timed terrain-free transition blocked animal={} clip={} tick={} part={:?}",
                    animal.asset.name, sample.clip, sample.tick, transition.part_id
                )
                .into());
            }
        } else {
            world.update_articulated_pose(id, update)?;
        }
        if kinematic_parts {
            world.step();
        }
        for ray in &timed_rays {
            if let Err(error) =
                world.raycast_articulated(id, ray.origin.to_array(), ray.direction.to_array(), 8.0)
            {
                let diagnostic = format!(
                    "{} {} {}@{} part={} origin={:?} direction={:?} expected={:?} actual_error={error}",
                    animal.asset.name,
                    representation.label(),
                    sample.clip,
                    sample.tick,
                    ray.part_id,
                    ray.origin,
                    ray.direction,
                    reference_hit_cells(&sample.ref_cells, ray.origin, ray.direction, 8.0)
                );
                if timed_ray_misses.len() < 24 {
                    timed_ray_misses.push(diagnostic);
                }
                timed_ray_miss_count += 1;
                ray_mismatches += 1;
            }
        }
        if !kinematic_parts {
            world.step();
        }
        if iteration >= WARMUP {
            operation_times.push(before.elapsed());
        }

        let part_index = iteration % animal.parts.len();
        let (part_id, removed_grid) = &edit_targets[part_index];
        let revision = &mut source_revisions[part_index];
        for restoring in [false, true] {
            let grid = if restoring {
                animal.parts[part_index].grid.clone()
            } else {
                removed_grid.clone()
            };
            let start = Instant::now();
            world.rebuild_articulated_part_grid(
                id,
                topology_token,
                topology_token + 1,
                *part_id,
                *revision,
                *revision + 1,
                grid,
            )?;
            if iteration >= WARMUP {
                source_rebuild_times.push(start.elapsed());
            }
            topology_token += 1;
            *revision += 1;
        }
    }
    let terrain_transition_sweeps = if kinematic_parts {
        Some(measure_terrain_transitions(
            animal,
            &samples,
            representation,
            root_translation,
            solver_substeps,
        )?)
    } else {
        None
    };
    Ok(CaseResult {
        animal: name.to_owned(),
        representation: representation.label(),
        solver_substeps,
        clips: animal.clips.len(),
        sampled_phases: samples.len(),
        source_cells: animal.cell_count,
        source_parts: animal.parts.len(),
        negative_origin_parts: animal
            .parts
            .iter()
            .filter(|part| {
                let origin = part.grid.origin();
                origin.x < 0 || origin.y < 0 || origin.z < 0
            })
            .count(),
        sampled_clip_phases: samples
            .iter()
            .map(|sample| format!("{}@{}", sample.clip, sample.tick))
            .collect(),
        body_count: world.articulated_rigid_body_count(id),
        collider_count: world.articulated_collider_count(id),
        collider_primitives: primitives,
        repeats: REPEATS,
        warmup: WARMUP,
        initial_attachment: summarize(&attachment_times),
        source_cell_rebuild: summarize(&source_rebuild_times),
        pose_refresh_ray_step: summarize(&operation_times),
        terrain_transition_sweeps,
        timed_ray_queries_per_pose: animal.parts.len(),
        ray_checks,
        miss_checks,
        ray_mismatches,
        timed_ray_miss_count,
        timed_ray_misses,
    })
}

fn measure_terrain_transitions(
    animal: &Animal,
    samples: &[PoseSample],
    representation: Representation,
    root_translation: Vec3,
    solver_substeps: usize,
) -> Result<TerrainTransitionSummary, Box<dyn std::error::Error>> {
    let mut cases = Vec::new();
    let mut elapsed = Vec::new();
    let root_rotation = Quat::from_rotation_y(10.0_f32.to_radians());
    for pair in samples
        .windows(2)
        .filter(|pair| pair[0].clip == pair[1].clip)
    {
        let from = &pair[0];
        let to = &pair[1];
        let target_cells = to
            .ref_cells
            .iter()
            .map(|cell| RefCell {
                center: root_translation + root_rotation * (cell.center - root_translation),
                rotation: (root_rotation * cell.rotation).normalize(),
                ..*cell
            })
            .collect::<Vec<_>>();
        let pose_cells = from
            .ref_cells
            .iter()
            .chain(&target_cells)
            .copied()
            .collect::<Vec<_>>();
        let (min, max) = pose_cells.iter().fold(
            (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY)),
            |(min, max), cell| {
                let x = cell.rotation * Vec3::X;
                let y = cell.rotation * Vec3::Y;
                let z = cell.rotation * Vec3::Z;
                let half = Vec3::new(
                    x.x.abs() + y.x.abs() + z.x.abs(),
                    x.y.abs() + y.y.abs() + z.y.abs(),
                    x.z.abs() + y.z.abs() + z.z.abs(),
                ) * (CELL_M * 0.5);
                (min.min(cell.center - half), max.max(cell.center + half))
            },
        );
        if pose_cells.is_empty() {
            return Err("asset clip has no occupied source cells".into());
        }
        let floor_top = min.y - 0.02;
        let wall_x = max.x + 0.02;
        let independent_clearance_proven =
            independent_cell_union_clearance(&from.ref_cells, &target_cells, floor_top, wall_x);
        if !independent_clearance_proven {
            return Err(format!(
                "independent continuous source-cell clearance could not certify {} {} {}->{}",
                animal.asset.name, to.clip, from.tick, to.tick
            )
            .into());
        }
        let material = MaterialId(1);
        let floor_grid = OccupancyGrid::from_solid_mask(
            GlobalCell::new(0, 0, 0),
            [128, 1, 128],
            vec![true; 128 * 128],
            vec![material; 128 * 128],
        )?;
        let wall_grid = OccupancyGrid::from_solid_mask(
            GlobalCell::new(0, 0, 0),
            [1, 128, 128],
            vec![true; 128 * 128],
            vec![material; 128 * 128],
        )?;
        let mut world = PhysicsWorld::new(PhysicsConfig {
            gravity_m_s2: [0.0; 3],
            solver_substeps,
            ..Default::default()
        });
        let mut attachment = Vec::new();
        let id = rebuild(
            &mut world,
            animal,
            representation,
            root_translation,
            &mut attachment,
            true,
        )?;
        world.update_articulated_pose(
            id,
            ArticulatedPose {
                topology_token: TOPOLOGY_TOKEN,
                base_pose_token: 0,
                pose_token: 1,
                parts: from.locals.clone(),
            },
        )?;
        world.step();
        world.add_body(BodySpec {
            kind: BodyKind::Fixed,
            representation,
            grid: floor_grid,
            cell_m: CELL_M,
            density_kg_m3: 0.0,
            mass_properties: None,
            translation_m: [min.x - 4.0, floor_top - CELL_M, min.z - 4.0],
            linvel_m_s: [0.0; 3],
        });
        world.add_body(BodySpec {
            kind: BodyKind::Fixed,
            representation,
            grid: wall_grid,
            cell_m: CELL_M,
            density_kg_m3: 0.0,
            mass_properties: None,
            translation_m: [wall_x, floor_top, min.z - 4.0],
            linvel_m_s: [0.0; 3],
        });
        let start = Instant::now();
        let transition = world.update_articulated_pose_with_swept_root_pose(
            id,
            ArticulatedPose {
                topology_token: TOPOLOGY_TOKEN,
                base_pose_token: 1,
                pose_token: 2,
                parts: to.locals.clone(),
            },
            PartTransform {
                translation_m: root_translation.to_array(),
                rotation_xyzw: root_rotation.to_array(),
            },
        ).map_err(|error| format!(
            "substep admission rejected terrain transition animal={} clip={} from={} to={} substeps={}: {error:?}",
            animal.asset.name, to.clip, from.tick, to.tick, solver_substeps
        ))?;
        if !transition.blocked {
            world.step();
            for ray in representative_rays(&target_cells) {
                let expected = reference_hit_cells(&target_cells, ray.origin, ray.direction, 8.0);
                let actual = world.raycast_articulated(
                    id,
                    ray.origin.to_array(),
                    ray.direction.to_array(),
                    8.0,
                );
                if !same_hit(&actual, expected, TOPOLOGY_TOKEN, 2, 1) {
                    return Err(format!(
                        "terrain transition query mismatch animal={} clip={} tick={} part={} expected={expected:?} actual={actual:?}",
                        animal.asset.name, to.clip, to.tick, ray.part_id
                    )
                    .into());
                }
            }
        }
        let duration = start.elapsed();
        elapsed.push(duration);
        cases.push(TerrainTransitionCase {
            clip: to.clip.clone(),
            from_tick: from.tick,
            to_tick: to.tick,
            blocked: transition.blocked,
            hit_part_id: transition.part_id,
            hit_time_of_impact: transition.time_of_impact,
            independent_clearance_proven,
            complete_update_query_step_us: duration.as_secs_f64() * 1_000_000.0,
            source_part_count: animal.parts.len(),
            terrain_collider_count: 2,
        });
    }
    let blocked = cases.iter().filter(|case| case.blocked).count();
    let accepted = cases.len() - blocked;
    Ok(TerrainTransitionSummary {
        same_clip_transitions: cases.len(),
        accepted,
        blocked,
        complete_transition_query_step: summarize(&elapsed),
        cases,
    })
}

/// Conservative continuous proof against the two infinite planes containing
/// the finite floor and wall fixtures in this workload. For each occupied
/// authored cell, the OBB's exact support extent is evaluated at the midpoint
/// of an interval. Every point in that cell can move at most
/// `(|linear origin delta| + angular delta * radius) * interval_half_width`
/// from the midpoint. If both plane clearances exceed that bound, the complete
/// interval is proven clear. Otherwise the interval is bisected to depth 24;
/// failure to prove is conservatively reported as not clear. This independent
/// oracle is not a finite-sample claim and can only overblock; it does not use
/// Rapier's nonlinear cast implementation.
fn independent_cell_union_clearance(
    start_cells: &[RefCell],
    end_cells: &[RefCell],
    floor_top: f32,
    wall_x: f32,
) -> bool {
    let end_by_cell: BTreeMap<_, _> = end_cells
        .iter()
        .map(|cell| ((cell.part_id, cell.cell), *cell))
        .collect();
    start_cells.iter().all(|start| {
        let Some(end) = end_by_cell.get(&(start.part_id, start.cell)) else {
            return false;
        };
        let local_center = (Vec3::new(
            start.cell[0] as f32,
            start.cell[1] as f32,
            start.cell[2] as f32,
        ) + Vec3::splat(0.5))
            * CELL_M;
        let pivot_start = start.center - start.rotation * local_center;
        let pivot_end = end.center - end.rotation * local_center;
        let angular_delta = (end.rotation * start.rotation.inverse()).to_scaled_axis();
        let reach = local_center.length() + (3.0_f32).sqrt() * CELL_M * 0.5;
        prove_cell_interval_clear(
            pivot_start,
            pivot_end,
            start.rotation,
            angular_delta,
            local_center,
            reach,
            floor_top,
            wall_x,
            0.0,
            1.0,
            0,
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn prove_cell_interval_clear(
    pivot_start: Vec3,
    pivot_end: Vec3,
    rotation_start: Quat,
    angular_delta: Vec3,
    local_center: Vec3,
    reach: f32,
    floor_top: f32,
    wall_x: f32,
    t0: f32,
    t1: f32,
    depth: u8,
) -> bool {
    let mid = (t0 + t1) * 0.5;
    let pivot = pivot_start.lerp(pivot_end, mid);
    let rotation = (Quat::from_scaled_axis(angular_delta * mid) * rotation_start).normalize();
    let center = pivot + rotation * local_center;
    let x_axis = rotation * Vec3::X;
    let y_axis = rotation * Vec3::Y;
    let z_axis = rotation * Vec3::Z;
    let half = CELL_M * 0.5;
    let radius_y = half * (x_axis.y.abs() + y_axis.y.abs() + z_axis.y.abs());
    let radius_x = half * (x_axis.x.abs() + y_axis.x.abs() + z_axis.x.abs());
    let floor_clearance = center.y - radius_y - floor_top;
    let wall_clearance = wall_x - center.x - radius_x;
    let half_interval = (t1 - t0) * 0.5;
    let path_bound = (pivot_end - pivot_start).length() * half_interval
        + angular_delta.length() * reach * half_interval;
    const FLOAT_MARGIN_M: f32 = 2.0e-5;
    if floor_clearance.min(wall_clearance) > path_bound + FLOAT_MARGIN_M {
        return true;
    }
    if depth >= 24 {
        return false;
    }
    prove_cell_interval_clear(
        pivot_start,
        pivot_end,
        rotation_start,
        angular_delta,
        local_center,
        reach,
        floor_top,
        wall_x,
        t0,
        mid,
        depth + 1,
    ) && prove_cell_interval_clear(
        pivot_start,
        pivot_end,
        rotation_start,
        angular_delta,
        local_center,
        reach,
        floor_top,
        wall_x,
        mid,
        t1,
        depth + 1,
    )
}

fn rebuild(
    world: &mut PhysicsWorld,
    animal: &Animal,
    representation: Representation,
    root_translation: Vec3,
    times: &mut Vec<Duration>,
    kinematic_parts: bool,
) -> Result<ArticulatedBodyId, Box<dyn std::error::Error>> {
    let start = Instant::now();
    let sources = animal
        .parts
        .iter()
        .map(|part| {
            let t = part.part.rest;
            if (t.scale - Vec3::ONE).abs().max_element() > 1.0e-6 {
                return Err(format!(
                    "{} part {} has non-unit source scale",
                    animal.asset.name, part.part.id
                )
                .into());
            }
            Ok(SourcePart {
                part_id: part.part.id,
                parent_id: (part.part.parent_id != 0).then_some(part.part.parent_id),
                volume_id: VolumeId::new(
                    u64::from(part.part.id) + u64::from(name_salt(&animal.asset.name)) * 1000,
                )?,
                source_revision: 1,
                grid: part.grid.clone(),
                cell_m: CELL_M,
                local_transform: local_transform(t),
            })
        })
        .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
    let id = if kinematic_parts {
        world.add_articulated_kinematic_parts(
            root_translation.to_array(),
            Quat::IDENTITY.to_array(),
            TOPOLOGY_TOKEN,
            sources,
            representation,
        )?
    } else {
        world.add_articulated_body(
            root_translation.to_array(),
            Quat::IDENTITY.to_array(),
            TOPOLOGY_TOKEN,
            sources,
            representation,
        )?
    };
    times.push(start.elapsed());
    Ok(id)
}

struct PoseSample {
    clip: String,
    tick: f64,
    locals: Vec<(u32, PartTransform)>,
    ref_cells: Vec<RefCell>,
    rays: Vec<RayCase>,
}

fn pose_samples(animal: &Animal) -> Result<Vec<PoseSample>, Box<dyn std::error::Error>> {
    let mut output = Vec::new();
    for (clip, phases) in &animal.clips {
        for tick in phases {
            let sampled = sample_clip(&animal.asset, clip, *tick)?;
            let mut locals = animal
                .parts
                .iter()
                .map(|part| {
                    let pose = sampled.get(&part.part.id).ok_or("clip omitted a part")?;
                    if (pose.transform.scale - Vec3::ONE).abs().max_element() > 1.0e-6 {
                        return Err(format!(
                            "{} clip {clip} has non-unit scale",
                            animal.asset.name
                        )
                        .into());
                    }
                    Ok((part.part.id, local_transform(pose.transform)))
                })
                .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
            locals.sort_by_key(|(id, _)| *id);
            output.push(PoseSample {
                clip: clip.clone(),
                tick: *tick,
                locals,
                ref_cells: Vec::new(),
                rays: Vec::new(),
            });
        }
    }
    Ok(output)
}

fn local_transform(transform: Transform) -> PartTransform {
    PartTransform {
        translation_m: (transform.translation * CELL_M).to_array(),
        rotation_xyzw: transform.rotation.normalize().to_array(),
    }
}

fn rays_for_cells(cells: &[RefCell]) -> Vec<RayCase> {
    let axes = [Vec3::X, Vec3::Y, Vec3::Z];
    cells
        .iter()
        .flat_map(|cell| {
            axes.map(|direction| RayCase {
                part_id: cell.part_id,
                origin: cell.center - direction,
                direction,
            })
        })
        .collect()
}

fn representative_rays(cells: &[RefCell]) -> Vec<RayCase> {
    let mut parts = BTreeMap::<u32, RefCell>::new();
    for cell in cells {
        parts.entry(cell.part_id).or_insert(*cell);
    }
    parts
        .into_values()
        .map(|cell| RayCase {
            part_id: cell.part_id,
            origin: cell.center - Vec3::X,
            direction: Vec3::X,
        })
        .collect()
}

fn reference_cells(
    animal: &Animal,
    locals: &[(u32, PartTransform)],
    root: Vec3,
) -> Result<Vec<RefCell>, Box<dyn std::error::Error>> {
    let local_map: BTreeMap<_, _> = locals.iter().copied().collect();
    let mut flattened = BTreeMap::<u32, (Vec3, Quat)>::new();
    while flattened.len() < animal.parts.len() {
        let mut changed = false;
        for geometry in &animal.parts {
            if flattened.contains_key(&geometry.part.id) {
                continue;
            }
            let local = local_map
                .get(&geometry.part.id)
                .ok_or("pose missing part")?;
            let t = Vec3::from_array(local.translation_m);
            let q = Quat::from_array(local.rotation_xyzw).normalize();
            if geometry.part.parent_id == 0 {
                flattened.insert(geometry.part.id, (t, q));
            } else if let Some((parent_t, parent_q)) =
                flattened.get(&geometry.part.parent_id).copied()
            {
                flattened.insert(
                    geometry.part.id,
                    (parent_t + parent_q * t, (parent_q * q).normalize()),
                );
            }
            changed |= flattened.contains_key(&geometry.part.id);
        }
        if !changed {
            return Err("part graph cannot be flattened".into());
        }
    }
    let mut cells = Vec::with_capacity(animal.cell_count);
    for geometry in &animal.parts {
        let (translation, rotation) = flattened[&geometry.part.id];
        for cell in &geometry.cells {
            let local_center = (Vec3::new(cell[0] as f32, cell[1] as f32, cell[2] as f32)
                + Vec3::splat(0.5))
                * CELL_M;
            cells.push(RefCell {
                part_id: geometry.part.id,
                cell: *cell,
                center: root + translation + rotation * local_center,
                rotation,
            });
        }
    }
    Ok(cells)
}

#[derive(Debug, Clone, Copy)]
struct RefHit {
    part_id: u32,
    cell: [i32; 3],
    toi: f32,
}

fn reference_hit_cells(
    cells: &[RefCell],
    origin: Vec3,
    direction: Vec3,
    max_toi: f32,
) -> Option<RefHit> {
    let mut best: Option<RefHit> = None;
    for cell in cells {
        let inv = cell.rotation.inverse();
        let local_origin = inv * (origin - cell.center);
        let local_direction = inv * direction;
        let mut near = 0.0f32;
        let mut far = max_toi;
        for axis in 0..3 {
            let o = local_origin[axis];
            let d = local_direction[axis];
            if d.abs() < 1.0e-8 {
                if !(-CELL_M * 0.5..=CELL_M * 0.5).contains(&o) {
                    far = -1.0;
                    break;
                }
            } else {
                let mut a = (-CELL_M * 0.5 - o) / d;
                let mut b = (CELL_M * 0.5 - o) / d;
                if a > b {
                    std::mem::swap(&mut a, &mut b);
                }
                near = near.max(a);
                far = far.min(b);
                if near > far {
                    break;
                }
            }
        }
        if near > far || far < 0.0 || near > max_toi {
            continue;
        }
        let candidate = RefHit {
            part_id: cell.part_id,
            cell: cell.cell,
            toi: near.max(0.0),
        };
        let replace = best.is_none_or(|old| {
            candidate.toi < old.toi - 1e-5
                || ((candidate.toi - old.toi).abs() <= 1e-5
                    && (candidate.part_id, candidate.cell) < (old.part_id, old.cell))
        });
        if replace {
            best = Some(candidate);
        }
    }
    best
}

fn same_hit(
    actual: &Result<spall_physics::SourceCellHit, spall_physics::ArticulatedError>,
    expected: Option<RefHit>,
    topology_token: u64,
    pose_token: u64,
    source_revision: u64,
) -> bool {
    match (actual, expected) {
        (Ok(actual), Some(expected)) => {
            actual.part_id == expected.part_id
                && actual.cell == expected.cell
                && (actual.toi_m - expected.toi).abs() <= 2.0e-4
                && actual.topology_token == topology_token
                && actual.pose_token == pose_token
                && actual.source_revision == source_revision
        }
        (Err(_), None) => true,
        _ => false,
    }
}

fn grid_without_cell(
    grid: &OccupancyGrid,
    source_cell: [i32; 3],
) -> Result<OccupancyGrid, Box<dyn std::error::Error>> {
    let origin = grid.origin();
    let local = [
        i64::from(source_cell[0]) - origin.x,
        i64::from(source_cell[1]) - origin.y,
        i64::from(source_cell[2]) - origin.z,
    ];
    let dims = grid.dims();
    if local
        .iter()
        .enumerate()
        .any(|(axis, value)| *value < 0 || *value >= i64::from(dims[axis]))
    {
        return Err(format!("cell {source_cell:?} lies outside its source grid").into());
    }
    let target = [local[0] as u32, local[1] as u32, local[2] as u32];
    if !grid.is_solid(target[0], target[1], target[2]) || grid.solid_count() <= 1 {
        return Err(format!("cell {source_cell:?} cannot be removed safely").into());
    }
    let count = dims.iter().map(|d| *d as usize).product::<usize>();
    let mut solid = vec![false; count];
    let mut material = vec![spall_core::MaterialId::AIR; count];
    for z in 0..dims[2] {
        for y in 0..dims[1] {
            for x in 0..dims[0] {
                if [x, y, z] == target {
                    continue;
                }
                if let Some(id) = grid.material(x, y, z) {
                    let index = x as usize
                        + dims[0] as usize * (y as usize + dims[1] as usize * z as usize);
                    solid[index] = true;
                    material[index] = id;
                }
            }
        }
    }
    Ok(OccupancyGrid::from_solid_mask(
        origin, dims, solid, material,
    )?)
}

fn check_source_cell_removal(
    animal: &Animal,
    representation: Representation,
    root: Vec3,
    world: &mut PhysicsWorld,
    id: ArticulatedBodyId,
    tokens: AssemblyTokens,
    locals: &[(u32, PartTransform)],
) -> Result<(usize, usize, usize), Box<dyn std::error::Error>> {
    let mut topology = tokens.topology;
    let pose_token = tokens.pose;
    let mut revisions = vec![1u64; animal.parts.len()];
    let mut checks = 0;
    let mut misses = 0;
    let mut mismatches = 0;
    for (part_index, part) in animal.parts.iter().enumerate() {
        let removed_cell = *part.cells.first().ok_or("part has no cells")?;
        let removed_grid = grid_without_cell(&part.grid, removed_cell)?;
        let removed_revision = revisions[part_index] + 1;
        world.rebuild_articulated_part_grid(
            id,
            topology,
            topology + 1,
            part.part.id,
            revisions[part_index],
            removed_revision,
            removed_grid,
        )?;
        topology += 1;
        revisions[part_index] = removed_revision;

        let mut changed = animal.clone();
        let changed_part = changed
            .parts
            .iter_mut()
            .find(|entry| entry.part.id == part.part.id)
            .unwrap();
        changed_part.cells.retain(|cell| *cell != removed_cell);
        changed_part.grid = grid_without_cell(&part.grid, removed_cell)?;
        changed.cell_count -= 1;
        let changed_cells = reference_cells(&changed, locals, root)?;
        let original_cell = reference_cells(animal, locals, root)?
            .into_iter()
            .find(|cell| cell.part_id == part.part.id && cell.cell == removed_cell)
            .ok_or("target source cell missing from independent oracle")?;
        for ray in rays_for_cells(&[original_cell]) {
            let expected = reference_hit_cells(&changed_cells, ray.origin, ray.direction, 8.0);
            let actual =
                world.raycast_articulated(id, ray.origin.to_array(), ray.direction.to_array(), 8.0);
            checks += 1;
            let hit_revision = expected.map_or(1, |hit| {
                let hit_index = animal
                    .parts
                    .iter()
                    .position(|entry| entry.part.id == hit.part_id)
                    .expect("reference hit must belong to an authored part");
                if hit.part_id == part.part.id {
                    removed_revision
                } else {
                    revisions[hit_index]
                }
            });
            let removed_hit =
                expected.is_some_and(|hit| hit.part_id == part.part.id && hit.cell == removed_cell);
            let agrees = same_hit(&actual, expected, topology, pose_token, hit_revision);
            if removed_hit || !agrees {
                eprintln!(
                    "source-cell-removal mismatch animal={} representation={} part={} removed_cell={:?} ray={:?} expected={:?} actual={:?} topology={} pose={} expected_revision={}",
                    animal.asset.name,
                    representation.label(),
                    part.part.id,
                    removed_cell,
                    ray,
                    expected,
                    actual,
                    topology,
                    pose_token,
                    hit_revision
                );
                mismatches += 1;
            }
        }

        let restore_revision = revisions[part_index] + 1;
        world.rebuild_articulated_part_grid(
            id,
            topology,
            topology + 1,
            part.part.id,
            revisions[part_index],
            restore_revision,
            part.grid.clone(),
        )?;
        topology += 1;
        revisions[part_index] = restore_revision;
        let restored_cells = reference_cells(animal, locals, root)?;
        for ray in rays_for_cells(&[original_cell]) {
            let expected = reference_hit_cells(&restored_cells, ray.origin, ray.direction, 8.0);
            let actual =
                world.raycast_articulated(id, ray.origin.to_array(), ray.direction.to_array(), 8.0);
            checks += 1;
            let hit_revision = expected.map_or(1, |hit| {
                let hit_index = animal
                    .parts
                    .iter()
                    .position(|entry| entry.part.id == hit.part_id)
                    .expect("reference hit must belong to an authored part");
                if hit.part_id == part.part.id {
                    restore_revision
                } else {
                    revisions[hit_index]
                }
            });
            if !same_hit(&actual, expected, topology, pose_token, hit_revision) {
                eprintln!(
                    "source-cell-restore mismatch animal={} representation={} part={} restored_cell={:?} ray={:?} expected={:?} actual={:?} topology={} pose={} expected_revision={}",
                    animal.asset.name,
                    representation.label(),
                    part.part.id,
                    removed_cell,
                    ray,
                    expected,
                    actual,
                    topology,
                    pose_token,
                    hit_revision
                );
                mismatches += 1;
            }
        }
    }
    let miss = world.raycast_articulated(
        id,
        (root + Vec3::splat(24.0)).to_array(),
        Vec3::X.to_array(),
        1.0,
    );
    misses += 1;
    if miss.is_ok() {
        mismatches += 1;
    }
    Ok((checks, misses, mismatches))
}

fn summarize(values: &[Duration]) -> Summary {
    let mut sorted = values.iter().map(Duration::as_secs_f64).collect::<Vec<_>>();
    sorted.sort_by(f64::total_cmp);
    let percentile = |p: f64| sorted[((sorted.len() - 1) as f64 * p).ceil() as usize] * 1e6;
    Summary {
        p50_us: percentile(0.50),
        p95_us: percentile(0.95),
        p99_us: percentile(0.99),
    }
}

fn hardware() -> serde_json::Value {
    serde_json::json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "available_parallelism": std::thread::available_parallelism().map(usize::from).ok(),
        "cpu_model": std::env::var("PROCESSOR_IDENTIFIER").ok(),
        "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
    })
}

fn name_salt(name: &str) -> u8 {
    match name {
        "trout" => 1,
        "crow" => 2,
        "deer" => 3,
        _ => 9,
    }
}
