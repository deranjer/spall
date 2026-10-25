//! T19 follow-up (interactive window input): a render window driven by real
//! keyboard/mouse input, connected to a live `sandbox-server` over real QUIC.
//!
//! [`run_interactive_window`] spawns [`crate::net::run_replication_client`] on
//! its own OS thread (an [`crate::interactive::InteractiveSession`] shared
//! between the two: the window writes live [`spall_core::PlayerInput`] into
//! it and reads the predicted player's pose + the replicated terrain back
//! out) and runs the winit event loop on the calling thread, presenting a
//! camera-following view every frame.
//!
//! This is deliberately a small, self-contained debug renderer: solid
//! terrain cells near the player draw as flat-shaded cubes under one
//! directional light — no shadows, no indirect light, no material catalog.
//! Wiring `spall_render`'s real G2 pipeline into a live presented window
//! (rather than its current offscreen-capture use) is out of scope here and
//! left to a follow-up increment.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use glam::Vec3;
use spall_core::{BUTTON_JUMP, BUTTON_SPRINT, BrickCoord, GlobalCell, MaterialId};
use spall_jobs::{BrickRef, BrickStatus, Generation, TopologyEpoch, WorldView};
use spall_physics::CharacterParams;
use spall_render::{
    Camera, CubeInstance, DebugView, Environment, EnvironmentPreset, GameRenderer,
    materials_from_manifest,
};
use spall_voxel::{Sample, Volume};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{DeviceEvent, ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, DeviceEvents, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowAttributes, WindowId};
use yakui_wgpu::{SurfaceInfo as YakuiSurfaceInfo, YakuiWgpu};
use yakui_winit::YakuiWinit;

use crate::ClientError;
use crate::audio::{AmbienceAudio, BiomeAudioConfig};
use crate::body_state::{BodyConditions, BodyCue, BodyPreview, BodyState};
use crate::day_night::environment_for_clock;
use crate::interactive::{InteractiveSession, InteractiveView, LiveInput};
use crate::net::{
    ClientNetConfig, run_replication_client_with_game_content,
    run_replication_client_with_manifest, run_replication_client_with_progression,
};
use crate::predict::CELL_M;
use crate::replica::ReplicaWorld;
use spall_core::MaterialManifest;

mod admin_menu;
mod app;
mod bodies;
mod book;
mod camera;
mod creature_sound_update;
mod endurance;
mod footstep_update;
mod gpu_timer;
mod handler;
mod hud;
mod instances;
mod inventory_feed;
mod options;
mod pack;
mod rebuild;
mod renderer;
mod shots;
mod terrain_mesh;
#[cfg(test)]
mod test_support;
mod tools;
mod vegetation;
mod world_items;

use self::admin_menu::*;
use self::app::*;
use self::bodies::*;
use self::book::*;
use self::camera::*;
use self::gpu_timer::*;
use self::handler::*;
use self::hud::*;
pub use self::instances::*;
use self::inventory_feed::*;
pub use self::options::*;
use self::pack::*;
pub use self::rebuild::*;
pub(crate) use self::renderer::*;
pub use self::shots::*;
pub use self::terrain_mesh::*;
pub use self::tools::*;
use self::vegetation::*;
use self::world_items::*;

/// Debug-view visible radius (metres) around the player's feet. This
/// renderer walks the raw volume every rebuild (no meshing/culling beyond a
/// buried-cell filter), so a wide radius costs real per-rebuild time — but
/// that cost lands on the background `RebuildWorker` thread (see its own
/// doc), not the render/input thread, so a bigger radius only makes terrain
/// pop in a little later after a big camera jump, never stalls a frame.
/// `10.0` (the original value) left almost the whole scene invisible until
/// the player was standing right in front of it — raised after user report.
/// The brick walk in [`build_instances`] skips empty and buried space, so the
/// window is tall enough for valley walls and a flying camera.
/// Rows of a replacement water field uploaded per frame: a 1024-row field takes
/// 16 frames.
const WATER_UPLOAD_ROWS_PER_FRAME: u32 = 64;
/// Frames of handler time the debug report summarises (about 10 s at 200 fps).
const FRAME_HISTORY: usize = 2400;
const DEFAULT_VIEW_RADIUS_M: f32 = crate::column_surface::DEFAULT_VIEW_RADIUS_M;
/// The horizontal radius (metres) the terrain, and the water field, is
/// drawn to. Set once at startup from `InteractiveOptions::view_distance_m`;
/// the server-streamed region around the player has to be at least this big.
static VIEW_RADIUS_BITS: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(DEFAULT_VIEW_RADIUS_M.to_bits());

fn view_radius_m() -> f32 {
    f32::from_bits(VIEW_RADIUS_BITS.load(std::sync::atomic::Ordering::Relaxed))
}

/// Shortest and longest view distances (metres). The longest is what the
/// regional streaming limit of 16 bricks can feed.
pub const MIN_VIEW_DISTANCE_M: f32 = 16.0;
pub const MAX_VIEW_DISTANCE_M: f32 = 120.0;

/// The default view distance in metres.
pub const DEFAULT_VIEW_DISTANCE_M: f32 = DEFAULT_VIEW_RADIUS_M;
const VIEW_HEIGHT_UP_M: f32 = 32.0;
/// Reaches the floor of a 96 m generated world from its highest peaks, so a
/// flying camera keeps the ground in view. Buried rock is skipped cheaply.
const VIEW_HEIGHT_DOWN_M: f32 = 96.0;
/// Rebuild the instanced terrain draw once the player has moved this far
/// (metres) from where it was last built, or the resident terrain changes.
pub const REBUILD_DISTANCE_M: f64 = 1.0;
/// How often, at most, a *stationary* player re-requests a rebuild just to
/// notice a terrain edit landing nearby (a moving player already re-requests
/// every `REBUILD_DISTANCE_M`). This governs background-worker traffic, not
/// frame time — see [`RebuildWorker`] — so it only needs to be "responsive
/// enough for a person to notice", not "cheap".
pub const TERRAIN_RECHECK_INTERVAL: Duration = Duration::from_millis(500);

const MOUSE_SENSITIVITY: f32 = 0.0025;
const MAX_PITCH: f32 = 1.5;

/// Runs an interactively-played `sandbox-client`: connects `net_config` on a
/// background thread and opens a render window on the calling thread. Blocks
/// until the window closes or the network session ends on its own (the
/// server closing, a fatal transport error). `net_config.interactive` is set
/// by this function; any value the caller passed there is overwritten.
pub fn run_interactive_window(net_config: ClientNetConfig) -> Result<(), ClientError> {
    run_interactive_window_with_manifest(net_config, spall_sim::fixtures::stone_manifest())
}

/// Runs the interactive client using the game's material manifest for handshake compatibility.
pub fn run_interactive_window_with_manifest(
    net_config: ClientNetConfig,
    materials: MaterialManifest,
) -> Result<(), ClientError> {
    run_interactive_window_with_game_content(net_config, materials, None)
}

/// Interactive client using material and optional game-asset manifests.
pub fn run_interactive_window_with_game_content(
    net_config: ClientNetConfig,
    materials: MaterialManifest,
    asset_manifest_hash: Option<[u8; 32]>,
) -> Result<(), ClientError> {
    run_interactive_window_with_progression(net_config, materials, asset_manifest_hash, Vec::new())
}

/// Interactive client with optional authenticated progression requests.
pub fn run_interactive_window_with_progression(
    net_config: ClientNetConfig,
    materials: MaterialManifest,
    asset_manifest_hash: Option<[u8; 32]>,
    progression_requests: Vec<spall_protocol::ProgressionRequest>,
) -> Result<(), ClientError> {
    run_interactive_window_with_environment(
        net_config,
        materials,
        asset_manifest_hash,
        progression_requests,
        EnvironmentPreset::Daylight.environment(),
    )
}

/// Interactive client lit by `environment` -- the same lighting model and
/// numbers `spall_render` gives the editor's scene viewport, so a scene reads
/// the same in both. The other `run_interactive_window_*` entry points use
/// [`EnvironmentPreset::Daylight`].
pub fn run_interactive_window_with_environment(
    net_config: ClientNetConfig,
    materials: MaterialManifest,
    asset_manifest_hash: Option<[u8; 32]>,
    progression_requests: Vec<spall_protocol::ProgressionRequest>,
    environment: Environment,
) -> Result<(), ClientError> {
    run_interactive_window_with_options(
        net_config,
        materials,
        asset_manifest_hash,
        progression_requests,
        environment,
        InteractiveOptions::default(),
    )
}

/// Interactive client with an optional no-vsync presentation path for
/// profiling. This changes presentation pacing only; simulation remains 60 Hz.
pub fn run_interactive_window_with_options(
    mut net_config: ClientNetConfig,
    materials: MaterialManifest,
    asset_manifest_hash: Option<[u8; 32]>,
    progression_requests: Vec<spall_protocol::ProgressionRequest>,
    environment: Environment,
    options: InteractiveOptions,
) -> Result<(), ClientError> {
    let session = InteractiveSession::new();
    let net_config_streams_residency = net_config.client_residency.is_some();
    net_config.interactive = Some(session.clone());
    // The render table comes from the same manifest the handshake validated,
    // so colour, roughness, metalness and emission match the editor and the
    // capture tools. One extra entry past the manifest ids draws debug
    // overlays.
    let material_names: std::collections::BTreeMap<u16, String> = materials
        .entries()
        .iter()
        .map(|def| (def.id.0, def.name.clone()))
        .collect();
    let mut render_materials = terrain_render_materials(&materials);
    let debug_material = render_materials.len() as u32;
    render_materials.push(spall_render::Material::new([0.02, 0.9, 0.9], 0.5, 0.0).emissive(2.0));
    // Tinted copies of manifest materials for the game's player bodies.
    let player_tint_base = render_materials.len() as u32;
    if let Some(view) = &options.player_view {
        for (material, tint) in view.tints() {
            let mut tinted = render_materials[usize::from(material.0)].jitter(0.0);
            tinted.base_color = tint.map(srgb_to_linear);
            render_materials.push(tinted);
        }
    }
    // And for the game's creatures.
    let creature_tint_base = render_materials.len() as u32;
    if let Some(view) = &options.creature_view {
        for (material, tint) in view.tints() {
            let mut tinted = render_materials[usize::from(material.0)].jitter(0.0);
            tinted.base_color = tint.map(srgb_to_linear);
            render_materials.push(tinted);
        }
    }

    // A user event (rather than a plain `()` event loop) so the network
    // thread ending — a failed connect, or the server closing the session —
    // wakes and closes the window right away. Without this the window has no
    // way to learn the session is over: it just keeps presenting an empty
    // scene forever until someone notices and closes it by hand.
    let event_loop = EventLoop::<()>::with_user_event().build()?;
    let net_done = event_loop.create_proxy();

    // Built before the network thread spawns: if this fails there is nothing
    // yet to clean up, whereas failing after would leak a running,
    // never-stopped network thread.
    let mut app = InteractiveApp::new(
        session.clone(),
        environment,
        render_materials,
        debug_material,
        options,
        // Only a session that streams bricks can have an absent brick that
        // is genuinely unknown; otherwise absent means empty.
        !net_config_streams_residency,
    )?;
    app.material_kinds = (0..app.render_materials.len())
        .map(|id| {
            let name = material_names.get(&u16::try_from(id).ok()?)?;
            crate::season_colour::Greenery::of_material(name)
        })
        .collect();
    app.material_names = material_names;
    app.player_tint_base = player_tint_base;
    app.creature_tint_base = creature_tint_base;

    let net_thread = std::thread::Builder::new()
        .name("spall-client-net".into())
        .spawn(move || {
            let result = if !progression_requests.is_empty() {
                run_replication_client_with_progression(
                    net_config,
                    materials,
                    asset_manifest_hash,
                    progression_requests,
                )
            } else if asset_manifest_hash.is_some() {
                run_replication_client_with_game_content(net_config, materials, asset_manifest_hash)
            } else {
                run_replication_client_with_manifest(net_config, materials)
            };
            let _ = net_done.send_event(());
            result
        })
        .map_err(|e| ClientError::Gpu(format!("spawning network thread: {e}")))?;

    event_loop.set_control_flow(ControlFlow::Poll);
    let run_result = event_loop.run_app(&mut app);

    // The window is gone either way; make sure the network thread notices
    // even if it was the window (not the server) that ended the session.
    session.request_stop();
    let net_result = net_thread
        .join()
        .map_err(|_| ClientError::NetThreadPanicked)?;

    run_result?;
    app.result?;
    net_result.map(|_summary| ()).map_err(ClientError::from)
}

/// Spectator flight speed, metres per second (`Shift` for the fast one).
const FLY_SPEED_M_S: f32 = 12.0;
const FLY_FAST_SPEED_M_S: f32 = 36.0;

/// How far (metres) the player may move from where the lighting cache was
/// centred before it is re-centred. The cache reaches 32 m each way and rays 24
/// m, so 8 m of drift keeps every nearby surface well inside it.
const SKY_ANCHOR_DRIFT_M: f64 = 8.0;

fn within_sky_anchor(anchor: [f64; 3], center: [f64; 3], drift_m: f64) -> bool {
    let horizontal = (anchor[0] - center[0]).hypot(anchor[2] - center[2]);
    horizontal <= drift_m && (anchor[1] - center[1]).abs() <= drift_m
}

/// One cube drawn by the shared `spall_render` instanced-cube path. The
/// material is the cell's `MaterialId`, looked up in the manifest-derived
/// table, so colour, roughness, metalness and emission all come from the same
/// definitions the editor and capture tools use.
pub(super) type Instance = CubeInstance;

const IDENTITY_ROTATION: [f32; 4] = CubeInstance::IDENTITY_ROTATION;
/// What a cover rebuild depends on; while it is unchanged the last result
/// stands, so the once-a-second vegetation frame does not redo the carpet.
/// Set `SPALL_FRAME_TRACE=1` to log frames whose handler took over 20 ms and where.
fn frame_trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SPALL_FRAME_TRACE").is_some())
}

/// `SPALL_WINDOW_SIZE=1928x931` opens the game window at that size, to measure
/// at the resolution a player actually uses.
fn window_size_from_env() -> Option<PhysicalSize<u32>> {
    let value = std::env::var("SPALL_WINDOW_SIZE").ok()?;
    let (w, h) = value.split_once(['x', 'X'])?;
    Some(PhysicalSize::new(
        w.trim().parse().ok()?,
        h.trim().parse().ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lighting_cache_only_recentres_after_real_movement() {
        let anchor = [10.0, 1.0, 10.0];
        assert!(within_sky_anchor(
            anchor,
            [10.5, 1.0, 12.0],
            SKY_ANCHOR_DRIFT_M
        ));
        assert!(
            within_sky_anchor(anchor, [15.0, 1.0, 16.0], SKY_ANCHOR_DRIFT_M),
            "6.4 m of drift is fine"
        );
        assert!(!within_sky_anchor(
            anchor,
            [10.0, 1.0, 19.0],
            SKY_ANCHOR_DRIFT_M
        ));
        assert!(!within_sky_anchor(
            anchor,
            [10.0, 10.0, 10.0],
            SKY_ANCHOR_DRIFT_M
        ));
    }
}
