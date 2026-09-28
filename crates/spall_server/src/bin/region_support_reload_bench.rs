use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, VolumeId};
use spall_jobs::{Generation, TopologyEpoch};
use spall_physics::PhysicsConfig;
use spall_server::{DiskBrickBacking, ResidencyController};
use spall_sim::{Simulation, SimulationConfig, WorldSetup, fixtures::stone_manifest};
use spall_structure::{AnchorPlane, CancelToken, ResidencyMode, StructureIndex};
use spall_voxel::{CacheBudget, EditPlan, RegionInterestRadii, RegionLayout, Sample, Volume};

fn main() -> std::process::ExitCode {
    let temp_root = std::env::temp_dir();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let run_dir = temp_root.join(format!(
        "spall-eng31-support-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir(&run_dir).expect("create unique run directory");
    let db_path = run_dir.join("residency.sqlite");
    let passed = run(&db_path);
    for suffix in ["", "-wal", "-shm"] {
        let mut path = db_path.as_os_str().to_os_string();
        path.push(suffix);
        let path = std::path::PathBuf::from(path);
        if path.exists() {
            std::fs::remove_file(path).expect("remove run database file");
        }
    }
    std::fs::remove_dir(&run_dir).expect("remove empty run directory");
    if passed {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

fn run(db_path: &Path) -> bool {
    let volume_id = VolumeId::new(1).expect("volume ID");
    let mut terrain = Volume::new(volume_id, CellSizeCode::Quarter);
    let mut bridge = EditPlan::new(volume_id);
    bridge.set(GlobalCell::new(0, 0, 1), MaterialId(1));
    for x in 0..96 {
        bridge.set(GlobalCell::new(x, 1, 1), MaterialId(1));
    }
    terrain
        .apply_edit(&bridge)
        .expect("build supported cross-region bridge");
    let setup = WorldSetup {
        terrain,
        terrain_collider_region: (GlobalCell::new(0, 0, 0), GlobalCell::new(95, 1, 2)),
        materials: stone_manifest(),
        anchor: AnchorPlane::at(0),
        physics: PhysicsConfig {
            disable_ccd: true,
            ..PhysicsConfig::default()
        },
    };
    let mut sim = Simulation::new(SimulationConfig::new(setup)).expect("valid bridge world");
    let initial = support(
        &sim.world().terrain().volume,
        AnchorPlane::at(0),
        ResidencyMode::AllResident,
    );
    let initially_supported = initial.supported_cells == 97 && initial.unsupported_cells == 0;

    let mut residency = ResidencyController::new(
        CacheBudget::new(usize::MAX, usize::MAX),
        128,
        DiskBrickBacking::open(db_path).expect("open disk backing"),
    );
    residency.register_world(sim.world(), false);
    residency
        .persist_volume(&sim.world().terrain().volume)
        .expect("persist initial bricks");
    let anchor = GlobalCell::new(0, 0, 1);
    let mut cut = EditPlan::new(volume_id);
    cut.set(anchor, MaterialId::AIR);
    sim.world_mut()
        .volume_body_mut(volume_id)
        .unwrap()
        .volume
        .apply_edit(&cut)
        .unwrap();
    let anchor_brick = BrickCoord::new(0, 0, 0);
    let key = spall_voxel::BrickCacheKey::new(volume_id, anchor_brick);
    residency
        .mark_dirty(sim.world(), key)
        .expect("mark distant support edit dirty");
    let cut_revision = sim
        .world()
        .terrain()
        .volume
        .brick_revision(anchor_brick)
        .unwrap()
        .unwrap();
    let anchor_removed = support(
        &sim.world().terrain().volume,
        AnchorPlane::at(0),
        ResidencyMode::AllResident,
    );
    let edit_breaks_support =
        anchor_removed.supported_cells == 0 && anchor_removed.unsupported_cells == 96;

    // Retain only the two eastern bricks, evicting the dirty edited anchor brick.
    let layout = RegionLayout::new([1, 1, 1]).unwrap();
    let radii = RegionInterestRadii::new(0, 0).unwrap();
    let mut active = spall_voxel::ActiveRegions::new();
    active
        .update(
            layout,
            [BrickCoord::new(1, 0, 0), BrickCoord::new(2, 0, 0)],
            radii,
            8,
        )
        .unwrap();
    residency
        .cache
        .update_interest_set(volume_id, layout.active_bricks(&active, 8).unwrap());
    residency.cache.set_budget(CacheBudget::new(2, usize::MAX));
    let evicted = residency
        .enforce_budget(sim.world_mut())
        .expect("persist then evict");
    let target_evicted = evicted.contains(&key);
    let pending = support(
        &sim.world().terrain().volume,
        AnchorPlane::at(0),
        ResidencyMode::Streamed,
    );
    let missing_dependency_stays_unknown = pending.unknown_cells > 0;

    // Reopen SQLite to prove the edited brick is available after a cache-controller restart.
    drop(residency);
    let mut reopened = ResidencyController::new(
        CacheBudget::new(usize::MAX, usize::MAX),
        128,
        DiskBrickBacking::open(db_path).expect("reopen disk backing"),
    );
    reopened.register_world(sim.world(), false);
    let reloaded = reopened
        .load_brick(sim.world_mut(), key)
        .expect("load persisted anchor brick");
    let loaded_revision = sim
        .world()
        .terrain()
        .volume
        .brick_revision(anchor_brick)
        .unwrap();
    let sample = sim.world().terrain().volume.sample(anchor).unwrap();
    let resolved = support(
        &sim.world().terrain().volume,
        AnchorPlane::at(0),
        ResidencyMode::AllResident,
    );
    let persisted_edit = reloaded
        && loaded_revision == Some(cut_revision)
        && sample == Sample::Empty { modified: true };
    let reloaded_edit_affects_support = resolved.unknown_cells == 0
        && resolved.supported_cells == 0
        && resolved.unsupported_cells == 96;
    let passed = initially_supported
        && edit_breaks_support
        && target_evicted
        && missing_dependency_stays_unknown
        && persisted_edit
        && reloaded_edit_affects_support;
    println!(
        "{{\"scenario\":\"distant_support_edit_disk_eviction_reload\",\"passed\":{passed},\
         \"initial_supported_cells\":{},\"post_edit_unsupported_cells\":{},\
         \"target_evicted\":{target_evicted},\"unknown_cells_while_evicted\":{},\
         \"reloaded_revision_matches\":{},\"reloaded_anchor_is_modified_air\":{},\
         \"resolved_unsupported_cells\":{},\"unknown_after_reload\":{}}}",
        initial.supported_cells,
        anchor_removed.unsupported_cells,
        pending.unknown_cells,
        loaded_revision == Some(cut_revision),
        sample == Sample::Empty { modified: true },
        resolved.unsupported_cells,
        resolved.unknown_cells,
    );
    passed
}

fn support(
    volume: &Volume,
    anchor: AnchorPlane,
    residency: ResidencyMode,
) -> spall_structure::SupportReport {
    StructureIndex::build(
        volume,
        anchor,
        residency,
        Generation(1),
        TopologyEpoch::START,
        &CancelToken::new(),
    )
    .expect("structural graph build")
    .report()
}
