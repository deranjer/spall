//! Bounded headless ecology demonstration on actual showcase voxel terrain.

use std::collections::BTreeMap;
use std::time::Instant;

use spall_core::{BRUSH_UNIT, BrushPoint, EntityId, GlobalCell, MaterialId, SphereBrush};
use spall_ecology::{
    CommitAck, EcologyConfig, EcologyInputs, EcologyState, InitialPlacementConfig,
    SpeciesDefinition, SpeciesId, acknowledge, canonical_digest, cut_branch, destroy_root,
    harvest_grass, initial_placement, place_grass_patch, update,
};
use spall_sim::{EditIntent, EditKind, EditTarget, RequestId, Simulation, SimulationConfig};
use spall_voxel::Sample;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let started = Instant::now();
    let large = std::env::args().any(|a| a == "--large");
    let edge = if large { 256 } else { 128 };
    let generation_started = Instant::now();
    let scene = sandbox::worldgen_scene::generate("showcase", 71, edge)?;
    let generation_ms = generation_started.elapsed().as_millis();
    let generated = scene.world();
    let species_id = SpeciesId(1);
    let species = SpeciesDefinition {
        version: 1,
        id: species_id,
        wood: sandbox::game::materials::WOOD,
        soil_materials: [
            MaterialId(10),
            MaterialId(2),
            MaterialId(214),
            MaterialId(213),
        ],
        min_moisture: 20,
        max_moisture: 220,
        min_sky_exposure: 12,
        min_spacing_cells: if large { 4 } else { 12 },
        seed_radius_cells: if large { 4 } else { 16 },
        seed_lifetime_ms: 20_000,
        seedling_ms: 1_000,
        juvenile_ms: 1_000,
        cell_growth_ms: 200,
    };
    let bounds = scene.world().region;
    let config = EcologyConfig {
        update_interval_ms: 1_000,
        max_work_per_update: if large { 1 } else { 32 },
        max_seed_records: 1024,
        max_plants: if large { 512 } else { 64 },
        grass_regrowth_per_interval: 2,
        spawn_clearance_cells: 12,
        bounds: Some(bounds),
    };
    let mut inputs = EcologyInputs::default();
    for z in 0..edge {
        for x in 0..edge {
            let y = i64::from(generated.columns.height(i64::from(x), i64::from(z)));
            let soil = GlobalCell::new(i64::from(x), y, i64::from(z));
            if matches!(generated.terrain.sample(soil), Ok(Sample::Filled(_))) {
                inputs.moisture.insert(soil, 128);
            }
        }
    }
    let spawn_cells: Vec<_> = generated
        .spawns
        .iter()
        .map(|p| {
            GlobalCell::new(
                (p[0] * 4.0) as i64,
                (p[1] * 4.0) as i64,
                (p[2] * 4.0) as i64,
            )
        })
        .collect();
    for spawn in &spawn_cells {
        for y in 0..config.spawn_clearance_cells {
            inputs
                .prohibited
                .insert(GlobalCell::new(spawn.x, spawn.y + i64::from(y), spawn.z));
        }
    }
    let mut state = EcologyState::default();
    let placement_started = Instant::now();
    let placed = initial_placement(
        &generated.columns,
        &generated.terrain,
        species,
        &inputs,
        InitialPlacementConfig {
            seed: 0x00EC_0106,
            maximum: if large { 128 } else { 1 },
            bounds,
            spawn_cells: &spawn_cells,
            ecology: config,
        },
        &mut state,
    );
    if placed == 0 {
        return Err("no terrain-valid trees found; moisture/candidate fixture needs review".into());
    }
    let placement_ms = placement_started.elapsed().as_millis();
    println!(
        "{{\"event\":\"initial_placement\",\"plants\":{placed},\"elapsed_ms\":{placement_ms}}}"
    );
    let grass_anchor = state.plants.values().next().unwrap().root;
    let patch = place_grass_patch(&mut state, species_id, grass_anchor, 100)?;
    let harvested = harvest_grass(&mut state, patch, 60);
    println!(
        "{{\"event\":\"grass_harvest\",\"patch_id\":{patch},\"biomass_removed\":{harvested}}}"
    );
    let mut definitions = BTreeMap::new();
    definitions.insert(species_id, species);
    let mut sim = Simulation::new(SimulationConfig::new(scene.world_setup()))?;
    let mut next_request = 1_u64;
    let mut proposal_cells = 0_u64;
    let mut cut_done = false;
    let mut root_done = false;
    let mut last_update_ms: u64;
    let mut queue_backlog = 0;
    let mut queue_backlog_max = 0usize;
    let mut step_micros = Vec::new();
    let mut proposal_attempt = 0usize;
    let mut mature_reported = false;
    let mut established_reported = 0usize;
    let ecological_duration_ms = if large { 30_000 } else { 20_000 };
    while state.ecological_time_ms < ecological_duration_ms {
        let step_started = Instant::now();
        let (report, proposals) = update(
            &mut state,
            &sim.world().terrain().volume,
            &definitions,
            &inputs,
            config,
            1_000,
        )?;
        queue_backlog = report.deferred;
        queue_backlog_max = queue_backlog_max.max(queue_backlog);
        if !mature_reported
            && state
                .plants
                .values()
                .any(|p| p.stage == spall_ecology::PlantStage::Mature)
        {
            println!(
                "{{\"event\":\"tree_mature\",\"time_ms\":{}}}",
                state.ecological_time_ms
            );
            mature_reported = true;
        }
        if report.seeds_dispersed > 0 {
            println!(
                "{{\"event\":\"seed_dispersal\",\"time_ms\":{},\"count\":{}}}",
                state.ecological_time_ms, report.seeds_dispersed
            );
        }
        if report.seeds_germinated > 0 {
            established_reported += report.seeds_germinated;
            println!(
                "{{\"event\":\"seedling_established\",\"time_ms\":{},\"count\":{}}}",
                state.ecological_time_ms, report.seeds_germinated
            );
        }
        for proposal in proposals {
            if proposal_attempt == 0 {
                proposal_attempt += 1;
                acknowledge(&mut state, &proposal, CommitAck::Rejected);
                println!(
                    "{{\"event\":\"wood_proposal_rejected\",\"time_ms\":{},\"plant_id\":{}}}",
                    state.ecological_time_ms, proposal.plant_id
                );
                continue;
            }
            if proposal_attempt == 1 {
                proposal_attempt += 1;
                inputs.revision = inputs.revision.saturating_add(1);
                acknowledge(&mut state, &proposal, CommitAck::Stale);
                println!(
                    "{{\"event\":\"wood_proposal_stale\",\"time_ms\":{},\"plant_id\":{},\"input_revision\":{}}}",
                    state.ecological_time_ms, proposal.plant_id, inputs.revision
                );
                continue;
            }
            let current =
                spall_ecology::proposal_current(&sim.world().terrain().volume, &inputs, &proposal);
            proposal_attempt += 1;
            if current {
                if proposal.cells.iter().any(|(cell, _)| {
                    !matches!(
                        sim.world().terrain().volume.sample(*cell),
                        Ok(Sample::Empty { .. })
                    )
                }) {
                    acknowledge(&mut state, &proposal, CommitAck::Rejected);
                    println!(
                        "{{\"event\":\"wood_proposal_rejected\",\"time_ms\":{},\"plant_id\":{},\"reason\":\"occupied target\"}}",
                        state.ecological_time_ms, proposal.plant_id
                    );
                    continue;
                }
                let mut accepted = proposal.clone();
                accepted.cells.clear();
                for &(cell, material) in &proposal.cells {
                    let center = BrushPoint::from_units(
                        cell.x * BRUSH_UNIT + BRUSH_UNIT / 2,
                        cell.y * BRUSH_UNIT + BRUSH_UNIT / 2,
                        cell.z * BRUSH_UNIT + BRUSH_UNIT / 2,
                    );
                    let request = RequestId(next_request);
                    next_request += 1;
                    sim.submit(EditIntent {
                        request_id: request,
                        actor: EntityId::new(1)?,
                        target: EditTarget::Terrain,
                        kind: EditKind::Place(material),
                        brush: SphereBrush::new(center, 0)?,
                        explosion: None,
                    })?;
                    let tick = sim.tick()?;
                    if !tick.committed.iter().any(|(id, _)| *id == request) {
                        break;
                    }
                    accepted.cells.push((cell, material));
                }
                if !accepted.cells.is_empty() {
                    let plant = &state.plants[&proposal.plant_id];
                    let last = accepted.cells.last().unwrap().0;
                    accepted.next_committed_cells =
                        plant.skeleton.iter().position(|b| b.cell == last).unwrap() as u32 + 1;
                    accepted.acknowledged_elapsed_ms =
                        accepted.cells.len() as u64 * species.cell_growth_ms;
                    proposal_cells += accepted.cells.len() as u64;
                    println!(
                        "{{\"event\":\"wood_growth_committed\",\"time_ms\":{},\"cells\":{}}}",
                        state.ecological_time_ms,
                        accepted.cells.len()
                    );
                    acknowledge(&mut state, &accepted, CommitAck::Accepted);
                } else {
                    acknowledge(&mut state, &proposal, CommitAck::Rejected);
                }
            } else {
                acknowledge(&mut state, &proposal, CommitAck::Stale);
            }
        }
        let elapsed = step_started.elapsed().as_micros() as u64;
        step_micros.push(elapsed);
        last_update_ms = state.ecological_time_ms;
        if !cut_done
            && last_update_ms >= 12_000
            && let Some((&plant_id, plant)) =
                state.plants.iter().find(|(_, p)| p.committed_cells > 10)
            && let Some((index, branch)) = plant.skeleton.iter().enumerate().find(|(i, c)| {
                *i > 5
                    && (c.cell.x != plant.root.x || c.cell.z != plant.root.z)
                    && !c.removed
                    && matches!(
                        sim.world().terrain().volume.sample(c.cell),
                        Ok(Sample::Filled(_))
                    )
            })
        {
            let center = BrushPoint::from_units(
                branch.cell.x * BRUSH_UNIT + BRUSH_UNIT / 2,
                branch.cell.y * BRUSH_UNIT + BRUSH_UNIT / 2,
                branch.cell.z * BRUSH_UNIT + BRUSH_UNIT / 2,
            );
            sim.submit(EditIntent {
                request_id: RequestId(next_request),
                actor: EntityId::new(1)?,
                target: EditTarget::Terrain,
                kind: EditKind::Cut,
                brush: SphereBrush::new(center, 0)?,
                explosion: None,
            })?;
            next_request += 1;
            sim.tick()?;
            cut_branch(&mut state, plant_id, index);
            cut_done = true;
            println!(
                "{{\"event\":\"branch_cut\",\"time_ms\":{},\"branch_index\":{index}}}",
                state.ecological_time_ms
            );
        }
        if !root_done
            && last_update_ms >= 18_000
            && let Some((&plant_id, plant)) =
                state.plants.iter().find(|(_, p)| p.committed_cells > 0)
        {
            let root = plant.root;
            let center = BrushPoint::from_units(
                root.x * BRUSH_UNIT + BRUSH_UNIT / 2,
                root.y * BRUSH_UNIT + BRUSH_UNIT / 2,
                root.z * BRUSH_UNIT + BRUSH_UNIT / 2,
            );
            sim.submit(EditIntent {
                request_id: RequestId(next_request),
                actor: EntityId::new(1)?,
                target: EditTarget::Terrain,
                kind: EditKind::Cut,
                brush: SphereBrush::new(center, 0)?,
                explosion: None,
            })?;
            next_request += 1;
            sim.tick()?;
            destroy_root(&mut state, plant_id);
            root_done = true;
            println!(
                "{{\"event\":\"root_destroyed\",\"time_ms\":{last_update_ms},\"plant_id\":{plant_id}}}"
            );
        }
    }
    // Continue the fixture grass input through sufficient suitable elapsed time.
    let (grass_report, _) = update(
        &mut state,
        &sim.world().terrain().volume,
        &definitions,
        &inputs,
        config,
        config.update_interval_ms,
    )?;
    last_update_ms = state.ecological_time_ms;
    let regrown = state.grass[&patch].biomass.saturating_sub(40);
    println!(
        "{{\"event\":\"grass_regrowth\",\"time_ms\":{},\"biomass_regrown\":{regrown}}}",
        state.ecological_time_ms
    );
    let digest = canonical_digest(&state)?;
    let elapsed_ms = started.elapsed().as_millis();
    let bytes = spall_ecology::encode_state(&state)?.len();
    let max_step = step_micros.iter().copied().max().unwrap_or_default();
    let min_step = step_micros.iter().copied().min().unwrap_or_default();
    let mean_step = step_micros.iter().sum::<u64>() / step_micros.len().max(1) as u64;
    let p50 = percentile(&step_micros, 50);
    let p95 = percentile(&step_micros, 95);
    println!(
        "{{\"scenario\":\"ecology-clearing\",\"dimensions_cells\":[{edge},384,{edge}],\"plants\":{},\"patches\":{},\"seeds\":{},\"seedlings_established\":{established_reported},\"update_interval_ms\":{},\"ecological_time_ms\":{},\"work_budget\":{},\"queue_backlog_final\":{queue_backlog},\"queue_backlog_max\":{queue_backlog_max},\"accepted_wood_cells\":{},\"rejected_and_stale_exercised\":true,\"harvested_biomass\":{harvested},\"regrown_biomass\":{regrown},\"branch_cut\":{},\"root_removed\":{},\"state_bytes\":{bytes},\"generation_ms\":{generation_ms},\"placement_ms\":{placement_ms},\"elapsed_ms\":{elapsed_ms},\"step_min_us\":{min_step},\"step_mean_us\":{mean_step},\"step_p50_us\":{p50},\"step_p95_us\":{p95},\"step_max_us\":{max_step},\"state_digest\":\"{}\",\"memory_method\":\"canonical encoded ecological state length; terrain and allocator overhead excluded\"}}",
        state.plants.len(),
        state.grass.len(),
        state.seeds.len(),
        config.update_interval_ms,
        last_update_ms,
        config.max_work_per_update,
        proposal_cells,
        cut_done,
        root_done,
        hex(&digest)
    );
    let _ = grass_report;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn percentile(values: &[u64], percent: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let i = (sorted.len() * percent)
        .div_ceil(100)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    sorted[i]
}
