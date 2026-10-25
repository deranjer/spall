use super::*;

pub(super) struct InteractiveApp {
    pub(super) session: Arc<InteractiveSession>,
    pub(super) window: Option<Arc<Window>>,
    pub(super) renderer: Option<WorldRenderer>,
    pub(super) environment: Environment,
    pub(super) day_night_cycle: bool,
    pub(super) render_materials: Vec<spall_render::Material>,
    /// Seasonal kind of each render material (same indexing), if greenery.
    pub(super) material_kinds: Vec<Option<crate::season_colour::Greenery>>,
    /// Material index of the debug overlay colour (one past the manifest).
    pub(super) debug_material: u32,
    pub(super) uncapped: bool,
    pub(super) ambience: Option<AmbienceAudio>,
    pub(super) biome_audio: BiomeAudioConfig,
    pub(super) creature_view: Option<std::sync::Arc<dyn CreatureView>>,
    pub(super) pack_view: Option<std::sync::Arc<dyn PackView>>,
    /// The pack (Tab): open, or sliding in or out.
    pub(super) pack: Pack,
    /// The blueprint book (B) and its crafting cloth.
    pub(super) book: Book,
    /// The inventory the pack and book show, and the book's craft.
    pub(super) inventory: InventoryFeed,
    pub(super) pack_frame_at: Option<Instant>,
    /// The cursor, physical pixels, for hovering items in the open pack.
    pub(super) cursor_px: Option<[f32; 2]>,
    pub(super) player_view: Option<std::sync::Arc<dyn PlayerView>>,
    /// `F8`: the camera sits behind the player, who is drawn in view.
    pub(super) third_person: bool,
    /// Render-table index of the player view's first tint material.
    pub(super) player_tint_base: u32,
    /// First render-table index of the creature view's tinted materials.
    pub(super) creature_tint_base: u32,
    /// Last drawn creature cubes and remote players. A frame that finds the
    /// replica lock busy reuses them instead of dropping them for a frame.
    pub(super) last_creature_cubes: Vec<spall_render::CubeInstance>,
    /// World item cubes from the last frame the replica was free (ENG-257).
    pub(super) last_item_cubes: Vec<spall_render::CubeInstance>,
    pub(super) last_remote_players: Vec<super::options::PlayerBody>,
    /// Third-person camera distance (metres): pulled in at once when terrain
    /// is in the way, eased back out, and held when the replica is busy.
    pub(super) third_person_distance_m: f32,
    pub(super) third_person_at: Option<Instant>,
    ambience_field: Option<spall_ecology::cover::CoverField>,
    ambience_grid: Option<spall_ecology::cover::CoverGrid>,
    ambience_check_at: Instant,
    /// Distance to the nearest bird creature, for birdsong volume.
    nearest_bird_m: Option<f64>,
    birdsong_gate: crate::audio::BirdsongGate,
    /// Ambience layers last sent to the audio thread, volumes in twentieths.
    ambience_layers: Vec<(PathBuf, u8)>,
    ambience_mix_at: Option<Instant>,
    weather_audio: Option<AmbienceAudio>,
    weather_mix: Vec<(PathBuf, f32)>,
    pub(super) footsteps: Option<crate::footsteps::FootstepAudio>,
    pub(super) footstep_volume: f32,
    /// One stride counter per walking player (`0` is the local player).
    pub(super) footstep_trackers: HashMap<u64, crate::footsteps::FootstepTracker>,
    pub(super) footstep_at: Option<Instant>,
    /// Each creature's last tag, to start its sounds when it changes.
    pub(super) creature_sounds: crate::creature_sounds::CreatureSoundTracker,
    pub(super) footstep_log: bool,
    pub(super) breathing: Option<crate::breathing::BreathingAudio>,
    pub(super) breathing_state: crate::breathing::BreathingState,
    /// `SPALL_BREATHING_LOG`: print each breathing and endurance state change.
    pub(super) breathing_log: bool,
    pub(super) breathing_tick: Instant,
    pub(super) endurance_view: Option<EnduranceView>,
    pub(super) nature_volume: f32,
    pub(super) weather_volume: f32,
    pub(super) nature_muted: bool,
    pub(super) nature_preview: Option<usize>,
    pub(super) weather_muted: bool,
    pub(super) latest_weather: Option<spall_protocol::WeatherState>,
    pub(super) thunder_event: Option<(u64, u64)>,
    pub(super) pending_thunder: Option<(Instant, PathBuf)>,
    pub(super) weather_effects_enabled: bool,
    pub(super) ground_snow_enabled: bool,
    pub(super) reduced_weather_flashes: bool,
    pub(super) weather_exposure: f32,
    /// Felt cold and wetness, shown as a screen grade (`body_state`).
    pub(super) body_state: BodyState,
    /// Admin-menu previews of looks for stats the game does not model yet.
    pub(super) body_preview: BodyPreview,
    /// This client's authoritative health, last read from the replica; shown
    /// as the injury look.
    pub(super) player_health: Option<spall_protocol::PlayerHealth>,
    pub(super) body_state_at: Option<Instant>,
    /// Clock for the screen grade's heartbeat pulse.
    pub(super) body_epoch: Instant,
    /// Eased 0..1 camera submersion and shared wind strength, from the renderer.
    pub(super) weather_submerge: f32,
    pub(super) weather_wind: f32,
    /// When the last frame was requested, for the uncapped frame limiter.
    pub(super) last_redraw: Instant,
    /// Scripted camera tour (`--shots`); empty for normal play.
    script: std::collections::VecDeque<ShotStep>,
    shots_dir: PathBuf,
    /// When the next script step runs; `None` until the world has arrived.
    script_at: Option<Instant>,
    shot_index: u32,
    /// The current script step waits for its screenshot.
    capture_due: bool,
    /// Screenshot to take on the next presented frame.
    pub(super) pending_capture: Option<PathBuf>,
    /// The newest sky occupancy the rebuild worker produced, kept so `F5` can
    /// switch visibility-aware skylight off and on without a rebuild.
    pub(super) last_sky: Option<spall_render::indirect::LightingVolume>,
    /// A fresh `last_sky` (or an `F5` toggle) the renderer has not seen yet.
    pub(super) sky_dirty: bool,
    /// `F5`: visibility-aware skylight (on) versus the legacy unconditional
    /// hemispheric ambient (off).
    pub(super) sky_visibility_on: bool,
    /// `F6`: the one-diffuse-bounce term (colour bleed, emissive light).
    pub(super) bounce_on: bool,
    /// How much lighting is computed (`F7`, the menu, or `SPALL_LIGHTING`).
    pub(super) lighting: spall_render::sky_visibility::LightingQuality,
    /// Whether `lighting` has been sent to the renderer yet.
    pub(super) lighting_applied: bool,
    /// The lighting quality as an index, shared with the rebuild worker.
    lighting_index: Arc<std::sync::atomic::AtomicU8>,
    pub(super) held: HeldKeys,
    pub(super) yaw: f32,
    pub(super) pitch: f32,
    pub(super) cursor_locked: bool,
    pub(super) rebuild: RebuildWorker,
    pub(super) body_worker: BodyWorker,
    /// Centre the *last completed* background rebuild was built around — not
    /// the centre of the most recent request, which may still be in flight.
    pub(super) last_built_pos: Option<[f64; 3]>,
    pub(super) last_dispatch_at: Option<Instant>,
    /// The replica terrain generation the last rebuild request was issued for.
    pub(super) last_dispatch_generation: Option<u64>,
    /// A hammer swing was just previewed: rebuild at once, not at the next timer.
    pub(super) force_rebuild: bool,
    /// One-sample-delayed camera interpolation and correction smoothing.
    /// Keeping the two newest mover publications prevents forward
    /// extrapolation from overshooting the actual stop point.
    pub(super) camera_follow: CameraFollow,
    pub(super) hud: Hud,
    /// The last completed background terrain rebuild's instances, kept so
    /// every frame can re-combine them with this frame's *fresh* body
    /// instances (see [`build_body_instances`]) — bodies move continuously
    /// and are cheap to rebuild, so they must not wait on the throttled,
    /// much more expensive terrain rebuild to appear or move.
    pub(super) pending_mesh_updates: Vec<(BrickCoord, Vec<spall_render::GpuVertex>, Vec<u32>)>,
    pub(super) pending_mesh_removed: Vec<BrickCoord>,
    pub(super) pending_mesh_detail_only: Vec<BrickCoord>,
    /// Handler time of the most recent frames (ms), for the debug report's
    /// percentiles; the HUD line is the average of a moment.
    pub(super) frame_history: std::collections::VecDeque<f32>,
    /// Swapchain acquire and queue submit time of the same frames (ms).
    pub(super) frame_waits: std::collections::VecDeque<(f32, f32)>,
    /// Handler time of the same frames minus the swapchain acquire and
    /// present waits, where vsync blocks (ms): the frame's own CPU work.
    pub(super) frame_work: std::collections::VecDeque<f32>,
    /// The last line the HUD printed, kept for the debug report.
    pub(super) last_hud_line: String,
    /// What the last "copy debug report" did, shown in the menu for a while.
    pub(super) report_status: Option<(String, Instant)>,
    /// `F9` was pressed; the next frame copies the report.
    pub(super) copy_report_key: bool,
    started_at: Instant,
    /// [`BodyWorker`]'s most recently completed result — see that struct's
    /// doc for why this moved off the render thread entirely (first a
    /// blocking lock, then even a `try_lock`-gated build, both measurably
    /// stalled frames under a real ~200-body debris load).
    pub(super) last_body_draws: Vec<BodyDraw>,
    pub(super) pose_stats: PoseStats,
    /// `F1`: hide terrain instances entirely, leaving only bodies — useful
    /// for finding debris hidden inside/behind geometry. `F2`: hide body
    /// instances (isolate terrain). Both default on.
    pub(super) show_terrain: bool,
    pub(super) show_bodies: bool,
    /// `F3`: draw the player's own predicted capsule bounds (see
    /// [`build_capsule_debug_instances`]) — the closest thing to a collision
    /// outline this renderer's cube-only instancing can produce. Off by
    /// default (adds a small number of bright markers around the player,
    /// which can otherwise obscure the view up close).
    pub(super) show_capsule: bool,
    pub(super) admin_menu_open: bool,
    pub(super) fly_eye: Option<Vec3>,
    pub(super) last_fly_update: Option<Instant>,
    /// Leaving flight: when the teleport went out and the feet it asked for.
    /// The camera holds where it is until the predicted player arrives there,
    /// so the view never shows the body at its old spot.
    pub(super) landing: Option<(Instant, [f64; 3])>,
    /// Hammer state: the button is down / a click has not been sent yet / when
    /// the last swing went out / the radius in cells (mouse wheel).
    pub(super) hammer_held: bool,
    pub(super) hammer_pending: bool,
    /// `T` was pressed and the torch has not been sent yet.
    pub(super) torch_pending: bool,
    pub(super) door_spec: Option<spall_voxel::doors::DoorSpec>,
    /// E was pressed: use the prompted item or door.
    pub(super) interact_pending: bool,
    pub(super) last_hammer_at: Option<Instant>,
    pub(super) hammer_radius: i64,
    pub(super) next_action_id: u64,
    /// Emissive terrain boxes of the last rebuild (torches, lamps), for the
    /// per-frame point-light selection.
    pub(super) emitter_instances: Vec<Instance>,
    /// The solid cell under the crosshair, refreshed every frame.
    pub(super) hammer_target: Option<(GlobalCell, MaterialId)>,
    /// Material names by id, for the crosshair label.
    pub(super) material_names: std::collections::BTreeMap<u16, String>,
    /// An admin command was sent and its `AdminStatus` has not arrived.
    pub(super) admin_request_pending: bool,
    /// `InteractiveSession::world_resets` last acted on.
    pub(super) seen_world_resets: u64,
    pub(super) result: Result<(), ClientError>,
}

impl InteractiveApp {
    /// The shortest time between frames. With vsync the display paces and this
    /// is zero. Uncapped play is limited to `SPALL_MAX_FPS` frames a second
    /// (default 300; 0 means no limit at all). The report line shows the frame
    /// rate the work alone could reach.
    pub(super) fn max_frame_interval(&self) -> Duration {
        if !self.uncapped {
            return Duration::ZERO;
        }
        let fps = std::env::var("SPALL_MAX_FPS")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(300.0);
        if fps <= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(1.0 / fps)
        }
    }

    pub(super) fn new(
        session: Arc<InteractiveSession>,
        environment: Environment,
        render_materials: Vec<spall_render::Material>,
        debug_material: u32,
        options: InteractiveOptions,
        absent_is_open: bool,
    ) -> Result<Self, ClientError> {
        let emissive: Vec<bool> = render_materials.iter().map(|m| m.emissive > 0.0).collect();
        let lighting = std::env::var("SPALL_LIGHTING")
            .ok()
            .and_then(|v| spall_render::sky_visibility::LightingQuality::parse(&v))
            .unwrap_or_default();
        if let Some(radius) = options.view_distance_m {
            let radius = radius.clamp(MIN_VIEW_DISTANCE_M, MAX_VIEW_DISTANCE_M);
            VIEW_RADIUS_BITS.store(radius.to_bits(), std::sync::atomic::Ordering::Relaxed);
        }
        session
            .column_surface
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_view_radius_m(view_radius_m());
        let lighting_index = Arc::new(std::sync::atomic::AtomicU8::new(
            spall_render::sky_visibility::LightingQuality::ALL
                .iter()
                .position(|q| *q == lighting)
                .unwrap_or(3) as u8,
        ));
        let rebuild = RebuildWorker::spawn(
            session.clone(),
            absent_is_open,
            emissive,
            lighting_index.clone(),
        )?;
        let body_worker = BodyWorker::spawn(session.clone())?;
        let ambience = AmbienceAudio::start(0.2);
        let footsteps = crate::footsteps::FootstepAudio::start(&options.biome_audio.footsteps, 0.5);
        let breathing =
            crate::breathing::BreathingAudio::start(&options.biome_audio.breathing, 0.6);
        Ok(Self {
            session,
            window: None,
            renderer: None,
            environment,
            day_night_cycle: options.day_night_cycle.unwrap_or(true),
            render_materials,
            material_kinds: Vec::new(),
            debug_material,
            uncapped: options.uncapped,
            ambience,
            biome_audio: options.biome_audio,
            creature_view: options.creature_view,
            pack_view: options.pack_view,
            pack: Pack::new(),
            book: Book::new(),
            inventory: InventoryFeed::new(),
            pack_frame_at: None,
            cursor_px: None,
            player_view: options.player_view,
            third_person: false,
            player_tint_base: 0,
            creature_tint_base: 0,
            last_creature_cubes: Vec::new(),
            last_item_cubes: Vec::new(),
            last_remote_players: Vec::new(),
            third_person_distance_m: super::camera::THIRD_PERSON_DISTANCE_M,
            third_person_at: None,
            ambience_field: None,
            ambience_grid: None,
            ambience_check_at: Instant::now() - Duration::from_secs(1),
            nearest_bird_m: None,
            birdsong_gate: crate::audio::BirdsongGate::new(
                Instant::now(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos() as u64),
            ),
            ambience_layers: Vec::new(),
            ambience_mix_at: None,
            weather_audio: AmbienceAudio::start(0.3),
            weather_mix: Vec::new(),
            footsteps,
            footstep_volume: 0.5,
            footstep_trackers: HashMap::new(),
            footstep_at: None,
            creature_sounds: crate::creature_sounds::CreatureSoundTracker::new(
                std::process::id().into(),
            ),
            footstep_log: std::env::var_os("SPALL_FOOTSTEP_LOG").is_some(),
            breathing,
            breathing_state: crate::breathing::BreathingState::Calm,
            breathing_tick: Instant::now(),
            breathing_log: std::env::var_os("SPALL_BREATHING_LOG").is_some(),
            endurance_view: None,
            nature_volume: 0.2,
            weather_volume: 0.3,
            nature_muted: false,
            nature_preview: None,
            weather_muted: false,
            latest_weather: None,
            thunder_event: None,
            pending_thunder: None,
            weather_effects_enabled: true,
            ground_snow_enabled: true,
            reduced_weather_flashes: false,
            weather_exposure: 1.0,
            body_state: BodyState::default(),
            body_preview: BodyPreview::default(),
            player_health: None,
            body_state_at: None,
            body_epoch: Instant::now(),
            weather_submerge: 0.0,
            weather_wind: 0.0,
            last_redraw: Instant::now(),
            script: options.shots.into(),
            shots_dir: options.shots_dir,
            script_at: None,
            shot_index: 0,
            pending_capture: None,
            capture_due: false,
            last_sky: None,
            sky_dirty: false,
            sky_visibility_on: std::env::var_os("SPALL_NO_SKYVIS").is_none(),
            bounce_on: std::env::var_os("SPALL_NO_BOUNCE").is_none(),
            lighting,
            lighting_index,
            lighting_applied: false,
            held: HeldKeys::default(),
            // Face -Z at spawn, matching `PlayerInput::NEUTRAL`.
            yaw: 0.0,
            pitch: 0.0,
            cursor_locked: false,
            rebuild,
            body_worker,
            last_built_pos: None,
            last_dispatch_at: None,
            last_dispatch_generation: None,
            force_rebuild: false,
            camera_follow: CameraFollow::default(),
            hud: Hud::default(),
            pending_mesh_updates: Vec::new(),
            pending_mesh_removed: Vec::new(),
            pending_mesh_detail_only: Vec::new(),
            frame_history: std::collections::VecDeque::with_capacity(FRAME_HISTORY),
            frame_waits: std::collections::VecDeque::with_capacity(FRAME_HISTORY),
            frame_work: std::collections::VecDeque::with_capacity(FRAME_HISTORY),
            last_hud_line: String::new(),
            report_status: None,
            copy_report_key: false,
            started_at: Instant::now(),
            last_body_draws: Vec::new(),
            pose_stats: PoseStats::default(),
            show_terrain: true,
            show_bodies: true,
            show_capsule: false,
            admin_menu_open: false,
            fly_eye: None,
            last_fly_update: None,
            landing: None,
            hammer_held: false,
            hammer_pending: false,
            torch_pending: false,
            interact_pending: false,
            door_spec: options.door,
            last_hammer_at: None,
            hammer_radius: HAMMER_RADIUS_DEFAULT,
            next_action_id: HAMMER_REQUEST_BASE,
            emitter_instances: Vec::new(),
            hammer_target: None,
            material_names: std::collections::BTreeMap::new(),
            admin_request_pending: false,
            seen_world_resets: 0,
            result: Ok(()),
        })
    }

    fn view_dir(&self) -> [f32; 3] {
        view_dir_from(self.yaw, self.pitch)
    }

    /// Refreshes the cached climate raster at 4 Hz, expanding it only when
    /// the compressed replicated field changes (the visual frame itself is
    /// replaced more often). Invalid, empty, non-finite, and out-of-world
    /// samples resolve to silence.
    pub(super) fn update_ambience(&mut self) {
        if self.ambience_check_at.elapsed() < Duration::from_millis(250) {
            return;
        }
        self.ambience_check_at = Instant::now();
        if let Some(replica) = self.session.replica.get().and_then(|r| r.try_lock().ok()) {
            self.latest_weather = replica.weather().cloned();
            // Distance to the nearest bird, for how loud birdsong plays.
            let listener = self
                .session
                .view
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .map(|view| view.predicted.position_m);
            self.nearest_bird_m = listener.and_then(|at| {
                replica
                    .creatures()
                    .records()
                    .filter(|record| self.biome_audio.bird_species.contains(&record.species))
                    .map(|record| {
                        let [x, y, z] = record.translation_m;
                        ((x - at[0]).powi(2) + (y - at[1]).powi(2) + (z - at[2]).powi(2)).sqrt()
                    })
                    .min_by(f64::total_cmp)
            });
        }
        // Weather ducks each ambience layer (water far less, see `ambience_mix`).
        let weather_duck = if self.nature_preview.is_some() {
            1.0
        } else {
            self.latest_weather.as_ref().map_or(1.0, |state| {
                1.0 - 0.65 * f32::from(state.precipitation_q16.max(state.gust_q16))
                    / f32::from(u16::MAX)
            })
        };
        if let Some(audio) = &self.ambience {
            audio.set_gain(if self.nature_muted {
                0.0
            } else {
                self.nature_volume
            });
        }
        // The wind bed swells in gusts and drops in lulls with the shared wind
        // schedule; full strength is the old constant level.
        let wind_gain = (0.3 + 0.7 * self.weather_wind).clamp(0.0, 1.0);
        let weather_mix = self.latest_weather.as_ref().map_or_else(Vec::new, |state| {
            let windy = matches!(
                state.kind,
                spall_protocol::WeatherKind::Blizzard | spall_protocol::WeatherKind::HighWind
            );
            self.biome_audio
                .weather_mix(
                    state.kind as u8,
                    f32::from(state.precipitation_q16) / f32::from(u16::MAX),
                )
                .into_iter()
                .map(|(path, gain)| {
                    let gain = if windy { gain * wind_gain } else { gain };
                    (path, (gain * 20.0).round() / 20.0)
                })
                .collect()
        });
        if let Some(audio) = &self.weather_audio {
            if self
                .pending_thunder
                .as_ref()
                .is_some_and(|(due, _)| Instant::now() >= *due)
                && let Some((_, path)) = self.pending_thunder.take()
                && !self.weather_muted
            {
                let _ = audio.play_once(path);
            }
            audio.set_gain(if self.weather_muted {
                0.0
            } else {
                // Under water the weather is a muffled thud at most.
                self.weather_volume
                    * (0.25 + 0.75 * self.weather_exposure)
                    * (1.0 - 0.93 * self.weather_submerge)
            });
            if weather_mix != self.weather_mix && audio.set_mix(weather_mix.clone()).is_ok() {
                self.weather_mix = weather_mix;
            }
            if let Some(state) = &self.latest_weather {
                let event = (state.world_epoch, state.lightning_id);
                let age = state.effective_tick.saturating_sub(state.lightning_tick);
                if state.lightning_id != 0 && self.thunder_event != Some(event) {
                    if age <= 120
                        && !self.weather_muted
                        && let Some(path) = &self.biome_audio.thunder
                    {
                        // WX0 has no strike position: use a short atmospheric delay.
                        self.pending_thunder =
                            Some((Instant::now() + Duration::from_millis(750), path.clone()));
                    }
                    self.thunder_event = Some(event);
                }
            }
        }
        let field = self
            .session
            .vegetation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .map(|frame| frame.field.clone());
        if field != self.ambience_field {
            self.ambience_grid = field.as_ref().and_then(|field| {
                if field.is_empty() {
                    None
                } else {
                    match field.expand() {
                        Ok(grid) if grid.width() > 0 => Some(grid),
                        Ok(_) => None,
                        Err(error) => {
                            tracing::warn!(%error, "invalid climate field for ambience lookup");
                            None
                        }
                    }
                }
            });
            self.ambience_field = field;
        }

        let position = self
            .fly_eye
            .map(|eye| [f64::from(eye.x), f64::from(eye.y), f64::from(eye.z)])
            .or_else(|| {
                self.session
                    .view
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .as_ref()
                    .map(|view| view.predicted.position_m)
            });
        // Mixed layers, not one track: a stream carries into the meadow beside it.
        // Recomputed a few times a second; each layer fades on the audio thread.
        let due = self
            .ambience_mix_at
            .is_none_or(|at| at.elapsed() >= Duration::from_millis(250));
        if !due && self.nature_preview.is_none() {
            return;
        }
        let bird_near = crate::audio::bird_near(self.nearest_bird_m);
        let singing = self.birdsong_gate.singing(Instant::now(), bird_near);
        let birdsong_level = crate::audio::birdsong_level(self.nearest_bird_m);
        let mix: Vec<(PathBuf, f32)> = if let Some(index) = self.nature_preview {
            self.biome_audio
                .tracks
                .get(index)
                .cloned()
                .flatten()
                .map(|path| vec![(path, 1.0)])
                .unwrap_or_default()
        } else {
            position
                .and_then(|position| {
                    let field = self.ambience_field.as_ref()?;
                    let grid = self.ambience_grid.as_ref()?;
                    let mix = crate::audio::ambience_mix(
                        weather_duck,
                        field,
                        grid,
                        position,
                        &self.biome_audio,
                    );
                    Some(crate::audio::with_birdsong(
                        mix,
                        &self.biome_audio,
                        &self.birdsong_gate,
                        singing,
                        birdsong_level,
                        if bird_near { weather_duck } else { 0.0 },
                    ))
                })
                .unwrap_or_default()
        };
        self.ambience_mix_at = Some(Instant::now());
        // Quantize so a drifting listener does not resend an unchanged mix.
        let quantized: Vec<(PathBuf, u8)> = mix
            .iter()
            .map(|(path, volume)| (path.clone(), (volume * 20.0).round() as u8))
            .collect();
        if quantized != self.ambience_layers {
            let sent = self
                .ambience
                .as_ref()
                .map(|audio| audio.set_mix(mix.clone()));
            match sent {
                Some(Err(std::sync::mpsc::TrySendError::Full(_))) => {
                    self.ambience_mix_at = None;
                    return;
                }
                Some(Err(std::sync::mpsc::TrySendError::Disconnected(_))) => {
                    tracing::warn!("ambience worker stopped; continuing without audio");
                    self.ambience = None;
                }
                _ => {}
            }
            tracing::info!(layers = ?quantized, "ambience mix changed");
            self.ambience_layers = quantized;
        }
    }

    /// Switches between walking the authoritative player and a free
    /// spectator camera that starts at the current eye. The player stops
    /// (neutral input) while flying and resumes from the held keys on landing.
    /// Runs the scripted tour (`--shots`), one step at a time: each step
    /// settles for a moment (terrain rebuilds around a moved camera), then the
    /// next presented frame is saved. Closes the window after the last shot.
    pub(super) fn advance_script(&mut self, now: Instant, event_loop: &ActiveEventLoop) {
        let Some(at) = self.script_at else {
            if self.script.is_empty()
                || self
                    .session
                    .view
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_none()
            {
                return;
            }
            // Give the first terrain build and water keyframe time to land.
            self.script_at = Some(now + Duration::from_secs(8));
            return;
        };
        if now < at {
            return;
        }
        if self.capture_due {
            self.capture_due = false;
            self.session.input.set_movement([0.; 3]);
            self.pending_capture = Some(
                self.shots_dir
                    .join(format!("shot-{:02}.png", self.shot_index)),
            );
            self.shot_index += 1;
            self.script_at = Some(now + Duration::from_millis(800));
            return;
        }
        let Some(step) = self.script.pop_front() else {
            // Let the last PNG finish writing, then close.
            if now >= at + Duration::from_millis(1500) {
                self.session.request_stop();
                event_loop.exit();
            }
            return;
        };
        println!(
            "spall-interactive: shot step {step:?} (next shot index {})",
            self.shot_index
        );
        let settle = match step {
            ShotStep::Camera {
                eye,
                yaw_deg,
                pitch_deg,
            } => {
                self.fly_eye = Some(Vec3::from_array(eye));
                self.last_fly_update = Some(now);
                self.session.input.set_movement([0.0; 3]);
                self.yaw = yaw_deg.to_radians();
                self.pitch = pitch_deg.to_radians().clamp(-MAX_PITCH, MAX_PITCH);
                self.publish_look();
                self.last_built_pos = None;
                6.0
            }
            ShotStep::Walk => {
                if self.fly_eye.is_some() {
                    self.toggle_flight();
                }
                2.0
            }
            ShotStep::Move {
                seconds,
                movement,
                sprint,
            } => {
                if self.fly_eye.is_some() {
                    self.toggle_flight();
                }
                self.session.input.set_movement(movement);
                self.session.input.set_button(BUTTON_SPRINT, sprint);
                seconds
            }
            ShotStep::ThirdPerson(on) => {
                self.third_person = on && self.player_view.is_some();
                self.script_at = Some(now + Duration::from_secs(1));
                return;
            }
            ShotStep::Menu => {
                self.admin_menu_open = !self.admin_menu_open;
                if self.admin_menu_open {
                    self.set_cursor_locked(false);
                }
                0.5
            }
            ShotStep::Report => {
                self.copy_report_key = true;
                0.5
            }
            ShotStep::Reset => {
                self.admin_request_pending = true;
                self.session
                    .push_admin(spall_protocol::AdminCommand::ResetWorld);
                6.0
            }
            ShotStep::Weather(kind, intensity) => {
                self.admin_request_pending = true;
                self.session
                    .push_admin(spall_protocol::AdminCommand::SetWeather { kind, intensity });
                3.0
            }
            ShotStep::Season(season) => {
                self.admin_request_pending = true;
                self.session
                    .push_admin(spall_protocol::AdminCommand::SetSeason { season });
                3.0
            }
            ShotStep::Play => {
                self.script.clear();
                self.script_at = None;
                self.session.input.set_movement([0.0; 3]);
                if self.admin_menu_open {
                    self.set_cursor_locked(false);
                }
                println!("spall-interactive: capture tour complete; ready for player input");
                return;
            }
            ShotStep::Wait(seconds) => {
                self.script_at = Some(now + Duration::from_secs_f32(seconds.max(0.0)));
                return;
            }
            ShotStep::View(view) => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.debug_view = view;
                }
                self.script_at = Some(now + Duration::from_millis(500));
                return;
            }
            ShotStep::Environment(preset) => {
                self.environment = preset.environment();
                if let Some(renderer) = &mut self.renderer {
                    renderer.environment = preset.environment();
                    renderer.day_cycle_enabled = false;
                }
                self.script_at = Some(now + Duration::from_secs(3));
                return;
            }
            ShotStep::Skylight(on) => {
                if self.sky_visibility_on != on {
                    self.toggle_sky_visibility();
                }
                self.script_at = Some(now + Duration::from_secs(2));
                return;
            }
            ShotStep::Bounce(on) => {
                if self.bounce_on != on {
                    self.toggle_bounce();
                }
                self.script_at = Some(now + Duration::from_secs(2));
                return;
            }
            ShotStep::Lighting(quality) => {
                self.set_lighting(quality);
                self.script_at = Some(now + Duration::from_secs(2));
                return;
            }
        };
        self.capture_due = true;
        self.script_at = Some(now + Duration::from_secs_f32(settle));
    }

    /// Sets the lighting quality: `Off` is flat ambient light, the others are
    /// how many rays the bounce casts and how thinly a sweep is sliced.
    pub(super) fn set_lighting(&mut self, quality: spall_render::sky_visibility::LightingQuality) {
        use spall_render::sky_visibility::LightingQuality;
        self.lighting = quality;
        self.lighting_index.store(
            LightingQuality::ALL
                .iter()
                .position(|q| *q == quality)
                .unwrap_or(3) as u8,
            std::sync::atomic::Ordering::Relaxed,
        );
        self.sky_visibility_on = quality != LightingQuality::Off;
        self.bounce_on = quality.bounce();
        self.sky_dirty = true;
        if let Some(renderer) = &self.renderer {
            renderer
                .scene
                .set_lighting_quality(&renderer.queue, quality);
            self.lighting_applied = true;
        }
        println!("spall-interactive: lighting quality {}", quality.name());
    }

    pub(super) fn toggle_sky_visibility(&mut self) {
        self.sky_visibility_on = !self.sky_visibility_on;
        self.sky_dirty = true;
        println!(
            "spall-interactive: visibility-aware skylight {}",
            if self.sky_visibility_on {
                "ON"
            } else {
                "OFF (legacy unconditional ambient)"
            }
        );
    }

    pub(super) fn toggle_bounce(&mut self) {
        self.bounce_on = !self.bounce_on;
        if let Some(renderer) = &self.renderer {
            renderer
                .scene
                .set_bounce_enabled(&renderer.queue, self.bounce_on);
        }
        println!(
            "spall-interactive: diffuse bounce {}",
            if self.bounce_on { "ON" } else { "OFF" }
        );
    }

    pub(super) fn toggle_flight(&mut self) {
        if let Some(eye) = self.fly_eye {
            // Land: the body is brought to the camera, not the camera to the
            // body. The camera stays put until the server's answer reaches
            // the predicted player.
            if self.landing.is_some() {
                return;
            }
            let eye_height_m = f64::from(CharacterParams::DEFAULT.total_height_m()) * 0.9;
            let feet = [
                f64::from(eye.x),
                f64::from(eye.y) - eye_height_m,
                f64::from(eye.z),
            ];
            self.session
                .push_admin(spall_protocol::AdminCommand::TeleportSelf {
                    feet_mm: feet.map(|m| (m * 1000.0).round() as i64),
                });
            self.landing = Some((Instant::now(), feet));
            self.publish_movement();
            return;
        }
        if self.fly_eye.take().is_none() {
            self.fly_eye = self
                .session
                .view
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .map(|v| self.camera_follow.eye(v, Instant::now(), true));
            self.session.input.set_movement([0.0; 3]);
            self.session.input.set_button(BUTTON_JUMP, false);
            self.session.input.set_button(BUTTON_SPRINT, false);
        } else {
            self.session.input.set_button(BUTTON_SPRINT, self.held.fast);
            self.publish_movement();
        }
        self.last_fly_update = Some(Instant::now());
        // Terrain is built around the camera while flying; rebuild now.
        self.last_built_pos = None;
    }

    /// Looks straight ahead again and, while flying, returns the camera to
    /// the player.
    /// Everything worth pasting when something looks wrong: where the player
    /// is, how the frames have run, what the renderer and the water hold, and
    /// which settings are on. Plain ASCII, one fact to a line.
    pub(super) fn debug_report(&self, view: Option<&InteractiveView>) -> String {
        use std::fmt::Write as _;
        let mut r = String::new();
        let _ = writeln!(r, "SPALL DEBUG REPORT");
        let _ = writeln!(
            r,
            "uptime {:.0} s | version {} | commit {} on {} (when built) | pid {}",
            self.started_at.elapsed().as_secs_f32(),
            env!("CARGO_PKG_VERSION"),
            env!("SPALL_GIT_COMMIT"),
            env!("SPALL_GIT_BRANCH"),
            std::process::id(),
        );
        let _ = writeln!(
            r,
            "exe {} | args {}",
            std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            std::env::args().skip(1).collect::<Vec<_>>().join(" ")
        );
        let _ = writeln!(r, "\n-- Player --");
        match view {
            Some(v) => {
                let p = v.predicted.position_m;
                let _ = writeln!(
                    r,
                    "feet ({:.2}, {:.2}, {:.2}) m | cell ({}, {}, {}) | region ({}, {})",
                    p[0],
                    p[1],
                    p[2],
                    (p[0] * 4.0).floor() as i64,
                    (p[1] * 4.0).floor() as i64,
                    (p[2] * 4.0).floor() as i64,
                    ((p[0] * 4.0).floor() as i64).div_euclid(32),
                    ((p[2] * 4.0).floor() as i64).div_euclid(32),
                );
                let _ = writeln!(r, "server tick {}", v.server_tick);
            }
            None => {
                let _ = writeln!(r, "no predicted player yet");
            }
        }
        let _ = writeln!(
            r,
            "mode {} | yaw {:.1} deg pitch {:.1} deg{}",
            if self.fly_eye.is_some() {
                "flying"
            } else {
                "walking"
            },
            self.yaw.to_degrees(),
            self.pitch.to_degrees(),
            self.fly_eye
                .map(|e| format!(" | fly eye ({:.2}, {:.2}, {:.2})", e.x, e.y, e.z))
                .unwrap_or_default()
        );
        if let Some(window) = &self.window {
            let size = window.inner_size();
            let _ = writeln!(r, "window {}x{}", size.width, size.height);
        }

        let _ = writeln!(r, "\n-- Frames --");
        for line in frame_summary(&self.frame_history) {
            let _ = writeln!(r, "{line}");
        }
        if !self.frame_waits.is_empty() {
            let n = self.frame_waits.len() as f32;
            let mean = |f: fn(&(f32, f32)) -> f32| self.frame_waits.iter().map(f).sum::<f32>() / n;
            let share = |f: fn(&(f32, f32)) -> bool| {
                100.0 * self.frame_waits.iter().filter(|w| f(w)).count() as f32 / n
            };
            let _ = writeln!(
                r,
                "waiting on the GPU or driver: acquire mean {:.2} ms (over 2 ms in {:.1}% of frames), submit mean {:.2} ms (over 4 ms in {:.1}%)",
                mean(|w| w.0),
                share(|w| w.0 > 2.0),
                mean(|w| w.1),
                share(|w| w.1 > 4.0)
            );
        }
        let _ = writeln!(r, "HUD (last report, one field a line):");
        for field in self.last_hud_line.split(" | ") {
            let _ = writeln!(r, "  {field}");
        }

        let _ = writeln!(r, "\n-- Lighting and sky --");
        let _ = writeln!(
            r,
            "lighting quality {} | skylight {} | bounce {} | {}",
            self.lighting.name(),
            if self.sky_visibility_on { "on" } else { "off" },
            if self.bounce_on { "on" } else { "off" },
            self.hud.sky_report()
        );
        if let Some(renderer) = &self.renderer {
            let _ = writeln!(
                r,
                "day cycle {} | {}",
                if renderer.day_cycle_enabled {
                    "on"
                } else {
                    "paused"
                },
                renderer.day_clock_label
            );
            let _ = writeln!(r, "render view {}", renderer.debug_view.stem());
            let _ = writeln!(r, "{}", renderer.scene_report());
            let _ = writeln!(
                r,
                "terrain chunks resident {} | drawn last frame {}",
                renderer.scene.terrain_mesh_coords().count(),
                renderer.scene.terrain_chunks_drawn()
            );
        }

        let _ = writeln!(r, "\n-- Water (client) --");
        {
            let surface = self
                .session
                .column_surface
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let field = surface.field();
            let _ = writeln!(
                r,
                "regions replicated {} | animating tiles {} | field {}x{} cells at origin ({:.1}, {:.1}) m",
                surface.region_count(),
                surface.animating_tiles(),
                field.dim,
                field.dim,
                field.origin_xz[0],
                field.origin_xz[1]
            );
        }
        match std::env::var_os("SPALL_SERVER_LOG").map(std::path::PathBuf::from) {
            Some(path) => {
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                let tail: Vec<&str> = text.lines().rev().take(12).collect();
                let _ = writeln!(
                    r,
                    "\n-- Server log (last {} lines of {}) --",
                    tail.len(),
                    path.display()
                );
                for line in tail.into_iter().rev() {
                    let _ = writeln!(r, "{line}");
                }
            }
            None => {
                let _ = writeln!(
                    r,
                    "server water and tick status are in the server's stderr log (not in this report)"
                );
            }
        }

        let _ = writeln!(r, "\n-- Settings --");
        let _ = writeln!(
            r,
            "view radius {:.0} m | fog {:.0}%-{:.0}% of it | terrain LOD from {:?} m, {} level changes a pass | foliage rebuild every {:.1} s",
            view_radius_m(),
            FOG_START_FRACTION * 100.0,
            FOG_END_FRACTION * 100.0,
            TERRAIN_LOD_FROM_M,
            TERRAIN_LOD_FLIPS_PER_PASS,
            FOLIAGE_REBUILD_INTERVAL.as_secs_f32()
        );
        let mut vars: Vec<(String, String)> = std::env::vars()
            .filter(|(k, _)| {
                k.starts_with("SPALL_") || k.starts_with("RAYON") || k.starts_with("WGPU")
            })
            .collect();
        vars.sort();
        if vars.is_empty() {
            let _ = writeln!(r, "no SPALL_* / RAYON* / WGPU* environment variables set");
        }
        for (k, v) in vars {
            let _ = writeln!(r, "{k}={v}");
        }
        r
    }

    pub(super) fn recentre_view(&mut self) {
        self.yaw = 0.0;
        self.pitch = 0.0;
        self.publish_look();
        if self.fly_eye.is_some()
            && let Some(view) = *self.session.view.lock().unwrap_or_else(|e| e.into_inner())
        {
            self.fly_eye = Some(self.camera_follow.eye(view, Instant::now(), true));
            self.last_built_pos = None;
        }
    }

    pub(super) fn publish_look(&self) {
        self.session.input.set_view_dir(self.view_dir());
    }

    pub(super) fn publish_movement(&self) {
        self.session.input.set_movement(self.held.movement());
    }

    /// Releases local intent after the operating system moves focus away from
    /// this window. `winit` does not guarantee matching release events for
    /// keys/buttons that were down at focus loss, so carrying `held` across
    /// that boundary could make the server receive a stale walk or jump.
    pub(super) fn clear_held_actions(&mut self) {
        self.hammer_held = false;
        self.hammer_pending = false;
        self.torch_pending = false;
        self.interact_pending = false;
        self.held.clear_on_focus_loss(&self.session.input);
    }

    /// The pack or the book is open (the cursor is the player's).
    pub(super) fn inventory_ui_open(&self) -> bool {
        self.pack.is_open() || self.book.is_open()
    }

    /// The pack or the book is on screen, open or sliding away.
    pub(super) fn inventory_ui_shown(&self) -> bool {
        self.pack.is_shown() || self.book.is_shown()
    }

    /// Frees the cursor and asks for a fresh inventory, for the pack or book.
    fn begin_inventory_ui(&mut self) {
        self.clear_held_actions();
        // An answer from an earlier opening is stale.
        *self
            .session
            .progression_reply
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        self.inventory.reset();
        self.set_cursor_locked(false);
    }

    /// Tab: lay the pack out on the tarp, only when the game supplied a
    /// [`PackView`].
    pub(super) fn open_pack(&mut self) {
        if self.pack_view.is_none() {
            return;
        }
        self.book.close();
        self.begin_inventory_ui();
        self.pack.open();
    }

    pub(super) fn close_pack(&mut self) {
        self.pack.close();
        self.set_cursor_locked(true);
    }

    /// B: open the blueprint book at its contents.
    pub(super) fn open_book(&mut self) {
        if self.pack_view.is_none() {
            return;
        }
        self.pack.close();
        self.begin_inventory_ui();
        self.book.open();
    }

    pub(super) fn close_book(&mut self) {
        self.book.close();
        self.set_cursor_locked(true);
    }

    /// The cursor in the UI's logical pixels, and the screen's size in them.
    pub(super) fn ui_pointer(&self) -> Option<([f32; 2], [f32; 2])> {
        let renderer = self.renderer.as_ref()?;
        let scale = renderer.ui_scale();
        let [x, y] = self.cursor_px?;
        Some((renderer.ui_size(), [x / scale, y / scale]))
    }

    /// The left button in the open book: picks up, drops or crafts.
    pub(super) fn book_button(&mut self, pressed: bool) {
        let Some(view) = self.pack_view.clone() else {
            return;
        };
        if let Some((screen, cursor)) = self.ui_pointer() {
            self.book.pointer(screen, cursor);
        }
        if pressed {
            self.book.press(view.as_ref(), &self.inventory);
        } else if let Some(BookAction::Craft(recipe_id)) =
            self.book.release(view.as_ref(), &self.inventory)
        {
            let request = self.inventory.craft(recipe_id, view.catalog_version());
            self.session.push_progression(request);
        }
    }

    pub(super) fn set_cursor_locked(&mut self, locked: bool) {
        let Some(window) = &self.window else { return };
        if locked {
            let grabbed = window
                .set_cursor_grab(CursorGrabMode::Locked)
                .or_else(|_| window.set_cursor_grab(CursorGrabMode::Confined))
                .is_ok();
            window.set_cursor_visible(!grabbed);
            self.cursor_locked = grabbed;
        } else {
            let _ = window.set_cursor_grab(CursorGrabMode::None);
            window.set_cursor_visible(true);
            self.cursor_locked = false;
        }
    }

    pub(super) fn fail(&mut self, event_loop: &ActiveEventLoop, error: ClientError) {
        self.result = Err(error);
        event_loop.exit();
    }
}
