use super::*;

/// Which built-in scene to serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scene {
    /// Anchored floor + single column + raised beam; cutting the column
    /// detaches the beam. See [`spall_voxel::fixtures::bridge_scene`].
    BridgeCut,
    /// Like [`Scene::BridgeCut`] but the seam-straddling column and the beam it
    /// holds both cross the `x = 32` brick boundary, so cutting the column
    /// detaches a body whose cells were owned across two bricks. See
    /// [`spall_voxel::fixtures::cross_brick_bridge_scene`].
    CrossBridgeCut,
    /// T19: a flat anchored walking arena with a step ledge. Each connecting
    /// client is given an authoritative player capsule; clients script movement
    /// and predict it locally. See [`spall_sim::fixtures::walk_arena_setup`].
    Walk,
    /// T17 / ENG-64: an anchored floor holds a `24³` checkerboard block through
    /// one column; cutting the column detaches a component whose ~13.8k
    /// single-cell runs overflow the inline `CellRun` budget, so the commit
    /// falls back to the compressed-baseline-blob op path. See
    /// [`spall_sim::fixtures::checkerboard_split_setup`].
    CheckerboardSplit,
    /// T17 increment 2 / ENG-64: like [`Scene::CheckerboardSplit`] but the
    /// detached block's compressed geometry exceeds even the inline op-blob cap,
    /// so the commit ships a bulk `BaselineWorld` on a stream. See
    /// [`spall_sim::fixtures::bulk_split_setup`].
    BulkSplit,
    /// T23 / G3 integrated acceptance: two independent collapsible bridge
    /// structures in one bounded `256 x 128 x 256 m` world. Connecting clients
    /// get a player capsule; alternating slots spawn in the two regions, so the
    /// harness drives geographically separated players and a multi-region
    /// collapse. See [`spall_sim::fixtures::separated_regions_setup`].
    SeparatedRegions,
    /// T23 / G4 eight-client workload (row 12): the same bounded world as
    /// [`Scene::SeparatedRegions`], eight player spawns in two 4-player
    /// clusters, plus 256 active + 4096 sleeping debris bodies. See
    /// [`spall_sim::fixtures::g4_workload_setup`] /
    /// [`spall_sim::fixtures::spawn_g4_workload_bodies`].
    G4Workload,
    /// Full-envelope separated regions joined by a causeway (T23 / G3 row 2).
    SeparatedRegionsFar,
    /// T11a / ENG-62: the full G1 gate envelope — `64 x 32 x 64 m` of real,
    /// resident, walkable terrain (not just isolated structures), a hollow
    /// tower/bridge spanning brick boundaries, an excavatable ramp, and a
    /// moving hollow test-volume body. See
    /// [`spall_sim::fixtures::g1_full_envelope_setup`] /
    /// [`spall_sim::fixtures::spawn_g1_hollow_test_volume`].
    G1FullEnvelope,
    /// T21 / ENG-28 increment 4 (3c): [`Scene::Walk`]'s arena with a small
    /// column-and-beam in the player lane. See
    /// [`spall_sim::fixtures::sleep_wake_setup`].
    SleepWake,
    /// Not a gate scene: a hands-on playground (plaza, stairs, maze, jump
    /// course) with a live debris spawner, for `cargo xtask play --scene
    /// playground`. See [`spall_sim::fixtures::playground_setup`] /
    /// [`spall_sim::playground`].
    Playground,
    /// Not a gate scene: [`Scene::Walk`]'s arena with one dynamic 0.75 m box
    /// resting in the player lane, for the headless prediction/push repro
    /// (`crates/spall_server/tests/prediction_timeline.rs`). See
    /// [`spall_sim::spawn_push_test_box`].
    PushTest,
    /// A game-supplied world (see [`CustomWorld`] and
    /// [`HostConfig::custom_world`]), e.g. a scene authored in the editor.
    /// Never selected by name; the host must supply the world.
    Custom,
}

/// A game-supplied world for [`Scene::Custom`]: a terrain/material/physics
/// setup builder plus the feet positions (metres) for connecting players,
/// indexed by connection slot. The builder runs once on the authoritative
/// simulation thread when a new world is created.
#[derive(Clone)]
pub struct CustomWorld {
    pub(super) player_spawns: Vec<[f64; 3]>,
    pub(super) build: Arc<dyn Fn() -> spall_sim::WorldSetup + Send + Sync>,
    pub(super) column_water: Option<spall_sim::column_water::ColumnWaterSetup>,
    pub(super) initial_bodies: Vec<InitialBody>,
    pub(super) world_items: Vec<InitialWorldItem>,
    pub(super) dam_gate: Option<DamGate>,
    pub(super) vegetation: Option<spall_ecology::living::LivingState>,
}

/// A dynamic body placed in a custom world when it is created: one solid box
/// of `dims` cells (x, y, z), painted `material`, at bulk `density_kg_m3`, whose
/// minimum corner is at `position_m`. A body lighter than water floats.
#[derive(Clone, Debug)]
pub struct InitialBody {
    pub dims: [i64; 3],
    pub material: spall_core::MaterialId,
    pub density_kg_m3: f32,
    pub position_m: [f64; 3],
}

/// A world item (ENG-257) placed when a custom world is created: a game item
/// kind lying at `translation_m` (metres), turned `yaw_rad` about +Y. The
/// server gives it its stable id.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InitialWorldItem {
    pub kind: u16,
    pub translation_m: [f64; 3],
    pub yaw_rad: f32,
}

/// A scene-authored dam gate an admin can open or close at runtime: the cells
/// it occupies while closed, and the material to restore them to on close. See
/// [`spall_protocol::AdminCommand::SetDamGate`].
#[derive(Clone, Debug)]
pub struct DamGate {
    pub cells: Vec<GlobalCell>,
    pub closed_material: spall_core::MaterialId,
}

impl CustomWorld {
    /// `player_spawns` must be non-empty: every connection gets a capsule.
    pub fn new(
        player_spawns: Vec<[f64; 3]>,
        build: impl Fn() -> spall_sim::WorldSetup + Send + Sync + 'static,
    ) -> Self {
        Self {
            player_spawns,
            build: Arc::new(build),
            column_water: None,
            initial_bodies: Vec::new(),
            world_items: Vec::new(),
            dam_gate: None,
            vegetation: None,
        }
    }

    /// Declares a runtime-togglable dam gate for this scene (see
    /// [`AdminCommand::SetDamGate`](spall_protocol::AdminCommand::SetDamGate)).
    #[must_use]
    pub fn with_dam_gate(mut self, gate: DamGate) -> Self {
        self.dam_gate = Some(gate);
        self
    }

    /// Installs stacked-column water (ENG-167), the production water model.
    #[must_use]
    pub fn with_column_water(mut self, setup: spall_sim::column_water::ColumnWaterSetup) -> Self {
        self.column_water = Some(setup);
        self
    }

    /// Bodies to place when the world is created (rafts, floating objects).
    #[must_use]
    pub fn with_initial_bodies(mut self, bodies: Vec<InitialBody>) -> Self {
        self.initial_bodies = bodies;
        self
    }

    /// Items to place when the world is created; a restored save keeps its own.
    #[must_use]
    pub fn with_world_items(mut self, items: Vec<InitialWorldItem>) -> Self {
        self.world_items = items;
        self
    }

    pub fn column_water_setup(&self) -> Option<&spall_sim::column_water::ColumnWaterSetup> {
        self.column_water.as_ref()
    }

    pub fn with_vegetation(mut self, vegetation: spall_ecology::living::LivingState) -> Self {
        self.vegetation = Some(vegetation);
        self
    }

    pub fn player_spawns(&self) -> &[[f64; 3]] {
        &self.player_spawns
    }

    pub fn dam_gate(&self) -> Option<&DamGate> {
        self.dam_gate.as_ref()
    }
}

impl std::fmt::Debug for CustomWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CustomWorld")
            .field("player_spawns", &self.player_spawns)
            .field("dam_gate", &self.dam_gate)
            .finish_non_exhaustive()
    }
}

impl Scene {
    /// Parses the `--scene` / scenario `scene` value. Unknown names are an error
    /// the caller surfaces rather than silently falling back.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "bridge-cut" | "bridgecut" | "bridge" => Some(Scene::BridgeCut),
            "cross-bridge-cut" | "cross-brick-bridge" | "crossbridgecut" => {
                Some(Scene::CrossBridgeCut)
            }
            "walk" | "walk-arena" | "player-movement" => Some(Scene::Walk),
            "checkerboard-split" | "oversized-split" => Some(Scene::CheckerboardSplit),
            "bulk-split" | "giant-split" => Some(Scene::BulkSplit),
            "separated-regions" | "t23-g3" | "g3" => Some(Scene::SeparatedRegions),
            "g4-workload" | "t23-g4" | "g4" => Some(Scene::G4Workload),
            "separated-regions-far" | "t23-g3-full-envelope" | "g3-far" => {
                Some(Scene::SeparatedRegionsFar)
            }
            "g1-full-envelope" | "g1-full-workload" | "g1" => Some(Scene::G1FullEnvelope),
            "sleep-wake" | "sleepwake" | "t21-sleep-wake" => Some(Scene::SleepWake),
            "playground" | "play" | "sandbox-playground" => Some(Scene::Playground),
            "push-test" | "pushtest" => Some(Scene::PushTest),
            // `Scene::Custom` is host-supplied and never parsed from a name.
            _ => None,
        }
    }

    /// A short stable name for the run summary.
    pub fn name(self) -> &'static str {
        match self {
            Scene::BridgeCut => "bridge-cut",
            Scene::CrossBridgeCut => "cross-bridge-cut",
            Scene::Walk => "walk",
            Scene::CheckerboardSplit => "checkerboard-split",
            Scene::BulkSplit => "bulk-split",
            Scene::SeparatedRegions => "separated-regions",
            Scene::G4Workload => "g4-workload",
            Scene::SeparatedRegionsFar => "separated-regions-far",
            Scene::G1FullEnvelope => "g1-full-envelope",
            Scene::SleepWake => "sleep-wake",
            Scene::Playground => "playground",
            Scene::PushTest => "push-test",
            Scene::Custom => "custom",
        }
    }

    /// `true` if this scene gives every connecting client a player capsule.
    pub fn has_players(self) -> bool {
        matches!(
            self,
            Scene::Walk
                | Scene::SeparatedRegions
                | Scene::G4Workload
                | Scene::SeparatedRegionsFar
                | Scene::G1FullEnvelope
                | Scene::SleepWake
                | Scene::Playground
                | Scene::PushTest
                | Scene::Custom
        )
    }

    /// Feet spawn positions (metres) for a player scene, indexed by connection
    /// slot. Empty for a scene without players.
    pub fn player_spawns(self) -> &'static [[f64; 3]] {
        match self {
            Scene::Walk => &WALK_ARENA_SPAWNS,
            Scene::SeparatedRegions => &SEPARATED_REGION_SPAWNS,
            Scene::G4Workload => &G4_WORKLOAD_SPAWNS,
            Scene::SeparatedRegionsFar => &SEPARATED_REGION_FAR_SPAWNS,
            Scene::G1FullEnvelope => &spall_sim::fixtures::G1_WORKLOAD_SPAWNS,
            Scene::SleepWake => &WALK_ARENA_SPAWNS,
            Scene::Playground => &spall_sim::fixtures::PLAYGROUND_SPAWNS,
            Scene::PushTest => &WALK_ARENA_SPAWNS,
            _ => &[],
        }
    }

    #[cfg(test)]
    pub(super) fn simulation(self) -> Simulation {
        self.simulation_with_terrain_collider_mode(TerrainColliderMode::PerBrick)
    }

    #[cfg(test)]
    pub(super) fn simulation_with_terrain_collider_mode(
        self,
        mode: TerrainColliderMode,
    ) -> Simulation {
        self.simulation_with_materials(mode, spall_sim::fixtures::stone_manifest(), None)
    }

    pub(super) fn simulation_with_materials(
        self,
        mode: TerrainColliderMode,
        materials: spall_core::MaterialManifest,
        custom: Option<&CustomWorld>,
    ) -> Simulation {
        let mut setup = match self {
            Scene::BridgeCut => spall_sim::fixtures::bridged_terrain_setup(),
            Scene::CrossBridgeCut => spall_sim::fixtures::cross_brick_bridged_setup(),
            Scene::Walk => spall_sim::fixtures::walk_arena_setup(),
            Scene::CheckerboardSplit => spall_sim::fixtures::checkerboard_split_setup(),
            Scene::BulkSplit => spall_sim::fixtures::bulk_split_setup(),
            Scene::SeparatedRegions => spall_sim::fixtures::separated_regions_setup(),
            Scene::G4Workload => spall_sim::fixtures::g4_workload_setup(),
            Scene::SeparatedRegionsFar => {
                spall_sim::fixtures::separated_regions_full_envelope_setup()
            }
            Scene::G1FullEnvelope => spall_sim::fixtures::g1_full_envelope_setup(),
            Scene::SleepWake => spall_sim::fixtures::sleep_wake_setup(),
            Scene::Playground => spall_sim::fixtures::playground_setup(),
            Scene::PushTest => spall_sim::fixtures::walk_arena_setup(),
            // `serve` rejects a `Custom` scene without a world before this runs.
            Scene::Custom => (custom.expect("Scene::Custom requires a CustomWorld").build)(),
        };
        setup.materials = materials;
        // No detached body in these scenes enables per-body CCD, and the serve
        // loop rebuilds the terrain collider on every committed cut. Rapier's
        // CCD broad-phase BVH can keep a stale proxy for the just-removed
        // collider and panic with "No element at index" mid-sweep when a body
        // is still settling (repro: `cargo xtask scenario --name
        // g1-networked-destruction`). Skip the CCD pass — a no-op here.
        setup.physics.disable_ccd = true;
        let mut sim_config = SimulationConfig::new(setup);
        sim_config.terrain_collider_mode = mode;
        sim_config.column_water = custom.and_then(|c| c.column_water.clone());
        // A server's tick must not wait for a lake: the fluid step has its own thread.
        sim_config.column_water_worker = true;
        sim_config.vegetation = custom.and_then(|c| c.vegetation.clone());
        let mut sim = Simulation::new(sim_config).expect("built-in scene is valid");
        if let Some(custom) = custom {
            for body in &custom.initial_bodies {
                spall_sim::playground::spawn_box_body(
                    sim.world_mut(),
                    body.dims,
                    body.material,
                    body.density_kg_m3,
                    spall_sim::BodyPose::new(glam::DQuat::IDENTITY, body.position_m),
                );
            }
            for item in &custom.world_items {
                sim.spawn_world_item(item.kind, item.translation_m, item.yaw_rad)
                    .expect("custom world item is valid");
            }
        }
        if matches!(self, Scene::G4Workload) {
            // Row 12: 256 active (64 near the west cluster's first spawn) +
            // 4096 sleeping debris bodies, built once at scene-construction
            // time (docs/reports/G3.md increment for this row).
            spall_sim::fixtures::spawn_g4_workload_bodies(sim.world_mut(), G4_WORKLOAD_SPAWNS[0]);
        }
        if matches!(self, Scene::G1FullEnvelope) {
            // The gate's "moving hollow test volume" — built once at
            // scene-construction time, same as G4Workload's debris.
            spall_sim::fixtures::spawn_g1_hollow_test_volume(sim.world_mut())
                .expect("hollow test volume spawns");
        }
        if matches!(self, Scene::PushTest) {
            spall_sim::spawn_push_test_box(sim.world_mut());
        }
        if matches!(self, Scene::Playground) {
            // Not a gate scene: the interactive playground's debris pile,
            // built once at scene-construction time for the same reason
            // G4Workload's and G1FullEnvelope's populations are — a body
            // `spawn_body` creates bypasses the commit pipeline, so it must
            // exist before any client's baseline pull to ever be seen (see
            // `spall_sim::playground`'s module doc).
            spall_sim::populate_playground_debris(
                sim.world_mut(),
                &spall_sim::playground_drop_zones(),
            );
        }
        sim
    }
}
