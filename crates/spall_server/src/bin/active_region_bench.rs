use spall_core::{BrickCoord, GlobalCell, MaterialId};
use spall_server::{MemoryBrickBacking, ResidencyController};
use spall_sim::{Simulation, SimulationConfig, fixtures};
use spall_voxel::{
    ActiveRegions, BrickCacheKey, CacheBudget, EditPlan, RegionInterestRadii, RegionLayout, Sample,
};

fn main() {
    let setup = fixtures::separated_regions_setup();
    let mut sim = Simulation::new(SimulationConfig::new(setup)).expect("valid fixture world");
    let volume_id = sim.world().terrain_volume_id();
    let layout = RegionLayout::new([2, 2, 2]).expect("nonzero region span");
    let radii = RegionInterestRadii::new(0, 0).expect("valid interest radii");
    let mut active = ActiveRegions::new();
    active
        .update(layout, [BrickCoord::new(0, 0, 0)], radii, 8)
        .expect("bounded west selection");
    let active_bricks = layout
        .active_bricks(&active, 8)
        .expect("bounded west brick footprint");

    let mut residency = ResidencyController::new(
        CacheBudget::new(usize::MAX, usize::MAX),
        128,
        MemoryBrickBacking::default(),
    );
    residency.register_world(sim.world(), false);
    residency
        .persist_volume(&sim.world().terrain().volume)
        .expect("durable initial brick set");
    residency
        .cache
        .update_interest_set(volume_id, active_bricks);

    let target = sim
        .world()
        .terrain()
        .volume
        .resident_brick_coords()
        .into_iter()
        .max_by_key(|coord| (coord.x, coord.y, coord.z))
        .expect("fixture has resident terrain");
    assert!(
        !active
            .coords()
            .any(|region| region == layout.region_of(target)),
        "east target must start outside active set"
    );
    let edit_cell = first_filled_cell(&sim.world().terrain().volume, target);
    let key = BrickCacheKey::new(volume_id, target);
    let mut edit = EditPlan::new(volume_id);
    edit.set(edit_cell, MaterialId::AIR);
    sim.world_mut()
        .volume_body_mut(volume_id)
        .expect("terrain volume")
        .volume
        .apply_edit(&edit)
        .expect("make distant authoritative edit");
    let edited_revision = sim
        .world()
        .terrain()
        .volume
        .brick_revision(target)
        .expect("valid brick")
        .expect("edited brick remains resident");
    residency
        .mark_dirty(sim.world(), key)
        .expect("register dirty revision");

    let total_bricks = residency.cache.resident_bricks();
    let west_resident_bricks = residency
        .cache
        .keys()
        .filter(|key| {
            key.volume == volume_id
                && residency
                    .cache
                    .state(*key)
                    .is_some_and(|state| state.interested)
        })
        .count();
    residency
        .cache
        .set_budget(CacheBudget::new(west_resident_bricks, usize::MAX));
    let evicted = residency
        .enforce_budget(sim.world_mut())
        .expect("persist before eviction");
    assert!(evicted.contains(&key), "inactive east brick is evicted");
    let resident_before_reload = residency.cache.resident_bricks();
    assert_eq!(resident_before_reload, west_resident_bricks);

    active
        .update(layout, [target], radii, 8)
        .expect("bounded east selection");
    let east_bricks = layout
        .active_bricks(&active, 8)
        .expect("bounded east brick footprint");
    residency.cache.update_interest_set(volume_id, east_bricks);
    assert!(
        residency
            .load_brick(sim.world_mut(), key)
            .expect("reload from backing"),
        "durable target reloads"
    );
    let loaded = sim.world().terrain().volume.brick_revision(target).unwrap();
    let sample = sim.world().terrain().volume.sample(edit_cell).unwrap();
    assert_eq!(loaded, Some(edited_revision));
    assert_eq!(sample, Sample::Empty { modified: true });

    let east_resident_bricks = residency
        .cache
        .keys()
        .filter(|key| {
            key.volume == volume_id
                && residency
                    .cache
                    .state(*key)
                    .is_some_and(|state| state.interested)
        })
        .count();
    println!(
        "region_span_bricks=2 active_regions={} active_brick_slots={} east_resident_bricks={} initial_resident_bricks={} resident_before_reload={} evicted={} resident_after_reload={} resident_dense_bytes={} resident_dense_upper_bound={} edit_revision={} reloaded_sample=modified_air",
        active.len(),
        active.len() * 8,
        east_resident_bricks,
        total_bricks,
        resident_before_reload,
        evicted.len(),
        residency.cache.resident_bricks(),
        residency.cache.resident_dense_bytes(),
        residency
            .cache
            .keys()
            .filter(|key| key.volume == volume_id)
            .count()
            * spall_voxel::MemoryReport::DENSE_BRICK_BYTES,
        edited_revision.get(),
    );
    println!("reloaded_brick=({},{},{})", target.x, target.y, target.z);
}

fn first_filled_cell(volume: &spall_voxel::Volume, brick: BrickCoord) -> GlobalCell {
    let origin = [brick.x * 32, brick.y * 32, brick.z * 32];
    for z in 0..32 {
        for y in 0..32 {
            for x in 0..32 {
                let cell = GlobalCell::new(origin[0] + x, origin[1] + y, origin[2] + z);
                if matches!(volume.sample(cell), Ok(Sample::Filled(_))) {
                    return cell;
                }
            }
        }
    }
    panic!("target brick contains no solid cell: {brick:?}");
}
