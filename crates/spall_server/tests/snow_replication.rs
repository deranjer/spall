//! ENG-208: two real QUIC clients receive the same authoritative winter snow,
//! including walker-authored footprints, and discard the old epoch on reset.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use spall_client::{
    BaselineScene, ClientNetConfig, MovementStep, ReplicaReadyHook, run_replication_client,
};
use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};
use spall_ecology::living::{LivingState, Season};
use spall_net::{Fingerprint, JoinToken, TransportConfig};
use spall_protocol::{AdminCommand, SnowSurfaceFrame, WeatherKind};
use spall_server::{CustomWorld, Scene, ServeConfig, serve};
use spall_structure::AnchorPlane;
use spall_voxel::{Brick, BrickBounds, EditPlan, Volume};
use spall_worldgen::{ColumnMap, Preset, WorldGenSpec, WorldgenPalette};

const SERVER_TICKS: u64 = 1_200; // 20 seconds at 60 Hz.
const RESET_TICK: u64 = 900; // 15 seconds after the server starts.
const FIRST_PATH: (u64, u64) = (400, 440);
const LATE_PATH: (u64, u64) = (600, 640);
// MovementStep ticks are local to each client. The late client joins at about
// server tick 480, so its local 120..160 window exercises server ticks 600..640.
const LATE_CLIENT_PATH: (u64, u64) = (120, 160);

fn unique_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("spall-eng208-snow-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create test directory");
    dir
}

fn wait_for_file(path: &PathBuf, timeout: Duration) -> String {
    let start = Instant::now();
    loop {
        if let Ok(value) = std::fs::read_to_string(path)
            && !value.trim().is_empty()
        {
            return value.trim().to_owned();
        }
        assert!(
            start.elapsed() < timeout,
            "timed out waiting for {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// A 64×64 quarter-metre slab with resident air overhead, so sky exposure and
/// grounded movement are sampled from committed geometry rather than Unknown.
fn flat_terrain() -> Volume {
    let id = VolumeId::new(1).unwrap();
    let bounds = BrickBounds::new(BrickCoord::new(0, 0, 0), BrickCoord::new(1, 1, 1))
        .expect("valid 64×64×64-cell world bounds");
    let mut volume = Volume::bounded(id, CellSizeCode::Quarter, bounds);
    for by in 0..2 {
        for bz in 0..2 {
            for bx in 0..2 {
                volume
                    .insert_brick(
                        BrickCoord::new(bx, by, bz),
                        Brick::uniform(MaterialId::AIR, Revision(1)),
                    )
                    .unwrap();
            }
        }
    }
    let mut floor = EditPlan::new(id);
    for z in 0..64 {
        for y in 0..2 {
            for x in 0..64 {
                floor.set(GlobalCell::new(x, y, z), MaterialId(1));
            }
        }
    }
    volume.apply_edit(&floor).unwrap();
    volume
}

fn winter_world() -> (CustomWorld, LivingState) {
    let columns = ColumnMap::compute(&WorldGenSpec::new(
        Preset::Natural,
        208,
        128,
        WorldgenPalette::sequential(1),
    ))
    .expect("natural climate columns");
    let mut ecology_terrain = flat_terrain();
    let living = LivingState::generate(
        208,
        &columns,
        &mut ecology_terrain,
        Vec::new(),
        Vec::new(),
        Season::Winter,
    )
    .expect("winter-empty living state");

    let world = CustomWorld::new(vec![[4.0, 0.7, 8.0], [11.0, 0.7, 8.0]], || {
        let mut setup = spall_sim::fixtures::flat_terrain_setup();
        setup.terrain = flat_terrain();
        setup.terrain_collider_region = (GlobalCell::new(0, 0, 0), GlobalCell::new(63, 63, 63));
        setup.anchor = AnchorPlane::at(0);
        setup
    })
    .with_vegetation(living.clone());
    (world, living)
}

#[derive(Clone, Default)]
struct ReplicaHandle(Arc<Mutex<Option<Arc<Mutex<spall_client::replica::ReplicaWorld>>>>>);

impl ReplicaHandle {
    fn hook(&self) -> ReplicaReadyHook {
        let slot = Arc::clone(&self.0);
        Arc::new(move |replica| *slot.lock().unwrap() = Some(replica))
    }

    fn get(&self) -> Option<Arc<Mutex<spall_client::replica::ReplicaWorld>>> {
        self.0.lock().unwrap().clone()
    }
}

#[derive(Clone, Default)]
struct Observation {
    samples: Arc<Mutex<Vec<Vec<SnowSurfaceFrame>>>>,
}

impl Observation {
    fn run(&self, handle: ReplicaHandle, stop: Arc<AtomicBool>) -> thread::JoinHandle<()> {
        let samples = Arc::clone(&self.samples);
        thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                if let Some(replica) = handle.get() {
                    let frames = replica.lock().unwrap().snow_frames().cloned().collect();
                    let mut history = samples.lock().unwrap();
                    history.push(frames);
                    if history.len() > 480 {
                        history.remove(0);
                    }
                }
                thread::sleep(Duration::from_millis(40));
            }
            if let Some(replica) = handle.get() {
                samples
                    .lock()
                    .unwrap()
                    .push(replica.lock().unwrap().snow_frames().cloned().collect());
            }
        })
    }

    fn snapshots(&self) -> Vec<Vec<SnowSurfaceFrame>> {
        self.samples.lock().unwrap().clone()
    }
}

fn has_snow(frames: &[SnowSurfaceFrame]) -> bool {
    frames
        .iter()
        .flat_map(|frame| &frame.surfaces)
        .any(|surface| surface.depth_q16 > 0)
}

fn positive_surfaces(frames: &[SnowSurfaceFrame]) -> BTreeSet<(u64, i32, i32, i32, [u8; 16])> {
    frames
        .iter()
        .flat_map(|frame| {
            frame
                .surfaces
                .iter()
                .filter(|surface| surface.depth_q16 > 0)
                .map(move |surface| {
                    (
                        frame.world_epoch,
                        frame.tile_x,
                        frame.tile_z,
                        surface.top_cell_y,
                        surface.id.0,
                    )
                })
        })
        .collect()
}

fn footprints(frames: &[SnowSurfaceFrame]) -> BTreeMap<u64, [i32; 3]> {
    frames
        .iter()
        .flat_map(|frame| {
            frame
                .footprints
                .iter()
                .map(|foot| (foot.id, foot.contact_cell))
        })
        .collect()
}

fn history_diagnostics(history: &[Vec<SnowSurfaceFrame>]) -> String {
    let frame_count: usize = history.iter().map(Vec::len).sum();
    let mut by_epoch = BTreeMap::<u64, (u64, BTreeSet<i32>)>::new();
    let mut by_tile = BTreeMap::<(u64, i32, i32), (usize, u16, BTreeSet<i32>)>::new();
    let mut marks = BTreeSet::new();
    let mut max_depth = 0;
    for frame in history.iter().flatten() {
        let entry = by_epoch.entry(frame.world_epoch).or_default();
        entry.0 = entry.0.max(frame.effective_tick);
        let tile = by_tile
            .entry((frame.world_epoch, frame.tile_x, frame.tile_z))
            .or_default();
        tile.0 += 1;
        for surface in &frame.surfaces {
            max_depth = max_depth.max(surface.depth_q16);
            tile.1 = tile.1.max(surface.depth_q16);
        }
        for mark in &frame.footprints {
            marks.insert(mark.id);
            entry.1.insert(mark.contact_cell[0]);
            tile.2.insert(mark.contact_cell[0]);
        }
    }
    format!(
        "samples={}, frames={frame_count}, epochs/latest_tick/contact_x={by_epoch:?}, tiles/count_maxdepth_contactx={by_tile:?}, max_depth_q16={max_depth}, imprint_ids={}",
        history.len(),
        marks.len(),
    )
}
fn common_populated_pair(
    first: &[Vec<SnowSurfaceFrame>],
    second: &[Vec<SnowSurfaceFrame>],
) -> Option<(Vec<SnowSurfaceFrame>, Vec<SnowSurfaceFrame>)> {
    let epoch_tick = |frames: &[SnowSurfaceFrame]| {
        let epoch = frames.first()?.world_epoch;
        if frames.iter().any(|frame| frame.world_epoch != epoch) {
            return None;
        }
        let latest_tick = frames.iter().map(|frame| frame.effective_tick).max()?;
        Some((epoch, latest_tick))
    };
    let mut best = None;
    let mut best_score = 0;
    for a in first {
        let Some((a_epoch, a_tick)) = epoch_tick(a) else {
            continue;
        };
        if a_tick < LATE_PATH.1 || a_tick >= RESET_TICK || !has_snow(a) {
            continue;
        }
        let a_marks = footprints(a);
        for b in second {
            let Some((b_epoch, b_tick)) = epoch_tick(b) else {
                continue;
            };
            if b_epoch != a_epoch || b_tick < LATE_PATH.1 || b_tick >= RESET_TICK || !has_snow(b) {
                continue;
            }
            let shared_surfaces = positive_surfaces(a)
                .intersection(&positive_surfaces(b))
                .count();
            let b_marks = footprints(b);
            let shared_marks: Vec<_> = a_marks
                .iter()
                .filter_map(|(id, cell)| (b_marks.get(id) == Some(cell)).then_some(*cell))
                .collect();
            let x_cells: BTreeSet<_> = shared_marks.iter().map(|cell| cell[0]).collect();
            let spans_both_paths = x_cells
                .iter()
                .any(|&x| x_cells.iter().any(|&other| x.abs_diff(other) >= 8));
            let score = shared_surfaces + shared_marks.len();
            if shared_surfaces > 0 && spans_both_paths && score > best_score {
                best_score = score;
                best = Some((a.clone(), b.clone()));
            }
        }
    }
    best
}
fn assert_reset_cleared(
    history: &[Vec<SnowSurfaceFrame>],
    old_epoch: u64,
    old_marks: &BTreeSet<u64>,
    client_name: &str,
) {
    let newest = history
        .last()
        .expect("observer recorded final replica state");
    let newest_epochs: BTreeSet<_> = newest.iter().map(|frame| frame.world_epoch).collect();
    assert!(
        !newest_epochs.contains(&old_epoch),
        "{client_name} retained pre-reset snow epoch {old_epoch}; current epochs {newest_epochs:?}"
    );
    let stale_marks: BTreeSet<_> = footprints(newest)
        .keys()
        .filter(|id| old_marks.contains(id))
        .copied()
        .collect();
    assert!(
        stale_marks.is_empty(),
        "{client_name} retained pre-reset footprint IDs {stale_marks:?}"
    );
}

fn client_config(
    dir: &std::path::Path,
    name: &str,
    server: (SocketAddr, Fingerprint, JoinToken),
    movement_script: Vec<MovementStep>,
    admin_script: Vec<(u64, AdminCommand)>,
    handle: ReplicaHandle,
) -> ClientNetConfig {
    let (connect_addr, fingerprint, token) = server;
    ClientNetConfig {
        connect_addr,
        server_fingerprint: fingerprint,
        join_token: token,
        script: Vec::new(),
        movement_script,
        creature_catalog: None,
        late_join: true,
        baseline_scene: BaselineScene::Walk,
        run_ticks: SERVER_TICKS,
        idle_grace: Duration::from_millis(900),
        overall_timeout: Duration::from_secs(45),
        log_json: dir.join(format!("{name}.jsonl")),
        summary_json: Some(dir.join(format!("{name}.summary.json"))),
        transport: TransportConfig::for_tests(),
        client_residency: None,
        baseline_budget_bytes: spall_client::segmented::DEFAULT_CLIENT_BASELINE_BUDGET_BYTES,
        on_replica_ready: Some(handle.hook()),
        interactive: None,
        client_authoritative: false,
        admin_script,
    }
}

#[test]
fn two_quic_clients_share_authoritative_snow_footprints_and_reset_epoch() {
    let dir = unique_dir();
    let token = JoinToken::generate().unwrap();
    let fingerprint_path = dir.join("server.fingerprint");
    let addr_path = dir.join("server.addr");
    let (world, living) = winter_world();
    assert_eq!(living.start_season, Season::Winter);
    assert!(living.species.is_empty() && living.spawns.is_empty() && living.organisms.is_empty());

    let mut server_cfg = ServeConfig::headless(
        "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        Scene::Custom,
        token,
    );
    server_cfg.harness.max_ticks = SERVER_TICKS;
    server_cfg.harness.quiescence_ticks = 0;
    server_cfg.server.paced = true;
    server_cfg.harness.startup_timeout = Duration::from_secs(20);
    server_cfg.server.log_json = dir.join("server.jsonl");
    server_cfg.harness.summary_json = Some(dir.join("server.summary.json"));
    server_cfg.harness.fingerprint_out = Some(fingerprint_path.clone());
    server_cfg.harness.addr_out = Some(addr_path.clone());
    server_cfg.harness.dev_unvalidated_actions = true;
    server_cfg.server.admin_commands = true;
    server_cfg.server.custom_world = Some(world);
    let server_thread = thread::spawn(move || serve(server_cfg));

    let fingerprint =
        Fingerprint::from_hex(&wait_for_file(&fingerprint_path, Duration::from_secs(20)))
            .expect("server fingerprint");
    let connect_addr = wait_for_file(&addr_path, Duration::from_secs(20))
        .parse::<SocketAddr>()
        .expect("server address");

    let first_handle = ReplicaHandle::default();
    let late_handle = ReplicaHandle::default();
    let first_observation = Observation::default();
    let late_observation = Observation::default();
    let stop_observers = Arc::new(AtomicBool::new(false));
    let first_observer = first_observation.run(first_handle.clone(), Arc::clone(&stop_observers));
    let late_observer = late_observation.run(late_handle.clone(), Arc::clone(&stop_observers));

    let first_cfg = client_config(
        &dir,
        "first-client",
        (connect_addr, fingerprint, token),
        vec![MovementStep {
            from_tick: FIRST_PATH.0,
            to_tick: FIRST_PATH.1,
            movement: [1.0, 0.0, 0.0],
            view_dir: [1.0, 0.0, 0.0],
            buttons: 0,
        }],
        vec![
            (
                30,
                AdminCommand::SetWeather {
                    kind: Some(WeatherKind::Snow),
                },
            ),
            (RESET_TICK, AdminCommand::ResetWorld),
        ],
        first_handle,
    );
    let late_cfg = client_config(
        &dir,
        "late-client",
        (connect_addr, fingerprint, token),
        vec![MovementStep {
            from_tick: LATE_CLIENT_PATH.0,
            to_tick: LATE_CLIENT_PATH.1,
            movement: [1.0, 0.0, 0.0],
            view_dir: [1.0, 0.0, 0.0],
            buttons: 0,
        }],
        Vec::new(),
        late_handle,
    );
    let late_thread = thread::spawn(move || {
        thread::sleep(Duration::from_secs(8));
        run_replication_client(late_cfg).expect("late QUIC client")
    });
    let first_client = run_replication_client(first_cfg).expect("first QUIC client");
    let late_client = late_thread.join().expect("late client thread");
    let server = server_thread
        .join()
        .expect("server thread")
        .expect("server run");

    stop_observers.store(true, Ordering::Release);
    first_observer.join().expect("first replica observer");
    late_observer.join().expect("late replica observer");

    assert!(first_client.connected && late_client.connected);
    assert_eq!(
        first_client.transactions_rejected, 0,
        "first client rejected topology"
    );
    assert_eq!(
        late_client.transactions_rejected, 0,
        "late client rejected topology"
    );
    assert_eq!(
        first_client.world_resets_installed, 1,
        "first client installs reset"
    );
    assert_eq!(
        late_client.world_resets_installed, 1,
        "late client installs reset"
    );
    assert_eq!(
        server.world_resets, 1,
        "server accepted exactly one world reset"
    );
    assert_eq!(
        first_client.admin_statuses.len(),
        2,
        "weather and reset statuses: {:?}",
        first_client.admin_statuses
    );
    assert!(
        first_client
            .admin_statuses
            .iter()
            .all(|status| status.accepted),
        "admin commands rejected: {:?}",
        first_client.admin_statuses
    );

    let first_movement = first_client
        .movement
        .as_ref()
        .expect("first walker movement summary");
    let late_movement = late_client
        .movement
        .as_ref()
        .expect("late walker movement summary");
    assert!(
        first_movement.max_distance_from_start_m > 0.5,
        "first scripted walker never left its start area: {first_movement:?}"
    );
    assert!(
        late_movement.max_distance_from_start_m > 0.5,
        "late scripted walker never left its start area: {late_movement:?}"
    );
    eprintln!("ENG208 movement: first={first_movement:?}; late={late_movement:?}");
    let first_history = first_observation.snapshots();
    let late_history = late_observation.snapshots();
    let (first_shared, late_shared) = common_populated_pair(&first_history, &late_history)
        .unwrap_or_else(|| panic!(
            "clients never shared positive-depth snow and authoritative footprints; first [{}], late [{}]",
            history_diagnostics(&first_history),
            history_diagnostics(&late_history),
        ));
    let first_epoch = first_shared
        .first()
        .expect("shared first epoch")
        .world_epoch;
    let old_marks: BTreeSet<_> = footprints(&first_shared).keys().copied().collect();
    let late_marks: BTreeSet<_> = footprints(&late_shared).keys().copied().collect();
    let shared_mark_ids: BTreeSet<_> = old_marks.intersection(&late_marks).copied().collect();
    let first_marks = footprints(&first_shared);
    let late_marks_by_id = footprints(&late_shared);
    for id in &shared_mark_ids {
        assert_eq!(
            first_marks.get(id),
            late_marks_by_id.get(id),
            "shared footprint {id} disagrees on its authoritative contact cell"
        );
    }
    assert!(
        !shared_mark_ids.is_empty(),
        "clients did not share imprint IDs"
    );

    let shared_marks: Vec<_> = first_shared
        .iter()
        .chain(&late_shared)
        .flat_map(|frame| frame.footprints.iter())
        .filter(|mark| shared_mark_ids.contains(&mark.id))
        .map(|mark| mark.contact_cell)
        .collect();
    let x_cells: BTreeSet<_> = shared_marks.iter().map(|cell| cell[0]).collect();
    let first_history_diag = history_diagnostics(&first_history);
    let late_history_diag = history_diagnostics(&late_history);
    let separated_bands = x_cells
        .iter()
        .any(|&a| x_cells.iter().any(|&b| a.abs_diff(b) >= 8));
    assert!(
        separated_bands,
        "shared authoritative footprints did not cover both scripted X paths; contact X cells={x_cells:?}; first movement={first_movement:?}; late movement={late_movement:?}; first history [{first_history_diag}]; late history [{late_history_diag}]"
    );

    assert_reset_cleared(&first_history, first_epoch, &old_marks, "first client");
    assert_reset_cleared(&late_history, first_epoch, &old_marks, "late client");
    assert_eq!(first_client.final_world_hash, server.final_world_hash);
    assert_eq!(late_client.final_world_hash, server.final_world_hash);

    eprintln!(
        "ENG-208 snow QUIC: shared epoch={first_epoch}, surfaces={}, shared imprint IDs={}, x-cell bands={x_cells:?}, reset cleared both replicas",
        positive_surfaces(&first_shared).len(),
        shared_mark_ids.len(),
    );
}
