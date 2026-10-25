use super::*;

#[derive(Default)]
pub(super) struct HeldKeys {
    forward: bool,
    back: bool,
    left: bool,
    right: bool,
    jump: bool,
    descend: bool,
    pub(super) fast: bool,
}

impl HeldKeys {
    /// Local wish direction (see `spall_core::PlayerInput`): `x` strafes
    /// right, `z` drives forward. Diagonal presses are left un-normalized —
    /// `step_character` clamps the resulting world-space wish vector itself.
    pub(super) fn movement(&self) -> [f32; 3] {
        let mut x = 0.0f32;
        if self.right {
            x += 1.0;
        }
        if self.left {
            x -= 1.0;
        }
        let mut z = 0.0f32;
        if self.forward {
            z += 1.0;
        }
        if self.back {
            z -= 1.0;
        }
        [x, 0.0, z]
    }

    /// Clears the window-side key state and the matching cross-thread input
    /// snapshot as one focus-loss transition.
    pub(super) fn clear_on_focus_loss(&mut self, input: &LiveInput) {
        *self = Self::default();
        input.clear_held_actions();
    }
}

impl ApplicationHandler for InteractiveApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        event_loop.listen_device_events(DeviceEvents::Always);
        let window = match event_loop.create_window(
            WindowAttributes::default()
                .with_title("Spall sandbox — interactive")
                .with_inner_size(window_size_from_env().unwrap_or(PhysicalSize::new(1280, 720))),
        ) {
            Ok(window) => Arc::new(window),
            Err(error) => return self.fail(event_loop, ClientError::Gpu(error.to_string())),
        };
        match WorldRenderer::new(
            window.clone(),
            self.environment,
            &self.render_materials,
            &self.material_kinds,
            self.uncapped,
            self.day_night_cycle,
        ) {
            Ok(renderer) => {
                self.window = Some(window);
                self.renderer = Some(renderer);
                self.set_cursor_locked(true);
            }
            Err(error) => self.fail(event_loop, error),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        // Forward every native window event to Yakui first. Its return value
        // tells gameplay input whether the UI consumed this event.
        let ui_consumed = self
            .renderer
            .as_mut()
            .is_some_and(|renderer| renderer.handle_window_event(&event));
        match event {
            WindowEvent::CloseRequested => {
                self.session.request_stop();
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(size);
                }
            }
            WindowEvent::Focused(false) => {
                self.clear_held_actions();
                self.set_cursor_locked(false);
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } if self.reading_note.is_some() => {
                self.close_note();
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } if self.pack.is_open() && !ui_consumed => {
                if let Some(item) = self.pack.hovered_item() {
                    self.use_pack_item(item);
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } if !self.cursor_locked
                && !ui_consumed
                && !self.inventory_ui_open()
                && !self.esc.open =>
            {
                self.set_cursor_locked(true);
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } if self.book.is_open() && !ui_consumed => {
                self.book_button(state == ElementState::Pressed);
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } if self.pack.is_open() && !ui_consumed => {
                // Clicking a tool in the pack puts it in hand, or away again.
                let item = self.pack.hovered().filter(|&item| {
                    self.pack_view
                        .as_ref()
                        .is_some_and(|view| view.item_tool(item).is_some())
                });
                if let Some(item) = item {
                    self.held_item = (self.held_item != Some(item)).then_some(item);
                }
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } if self.cursor_locked && !ui_consumed => {
                self.hammer_held = state == ElementState::Pressed;
                self.hammer_pending |= self.hammer_held;
            }
            WindowEvent::MouseWheel { delta, .. }
                if self.cursor_locked && self.hammer_on && !ui_consumed =>
            {
                let steps = match delta {
                    winit::event::MouseScrollDelta::LineDelta(_, y) => y.round() as i64,
                    winit::event::MouseScrollDelta::PixelDelta(p) => (p.y / 40.0).round() as i64,
                };
                if steps != 0 {
                    self.hammer_radius = (self.hammer_radius + steps).clamp(0, HAMMER_RADIUS_MAX);
                    eprintln!(
                        "spall-interactive: hammer radius {} cells",
                        self.hammer_radius
                    );
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_px = Some([position.x as f32, position.y as f32]);
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let PhysicalKey::Code(code) = event.physical_key else {
                    return;
                };
                let held = event.state == ElementState::Pressed;
                let fresh = held && !event.repeat;
                // The Esc menu owns the keyboard: the next key rebinds the
                // action being edited, and Esc closes the menu.
                if self.esc.open {
                    if fresh {
                        if self.esc.capturing.is_some() {
                            if self.esc.capture(&mut self.keys, code) {
                                self.keys.save();
                            }
                        } else if code == KeyCode::Escape {
                            self.close_esc_menu();
                        }
                    }
                    return;
                }
                let action = self.keys.action_for(code);
                // Yakui sinks Tab for its focus navigation even with no
                // widget focused; Tab is the pack's key.
                if ui_consumed && action != Some(Action::Pack) {
                    return;
                }
                let walking = matches!(
                    action,
                    Some(
                        Action::Forward
                            | Action::Back
                            | Action::Left
                            | Action::Right
                            | Action::Jump
                    )
                );
                // Esc or any walking key puts a note away.
                if self.reading_note.is_some() {
                    if fresh
                        && (code == KeyCode::Escape || walking || action == Some(Action::Interact))
                    {
                        self.close_note();
                    }
                    return;
                }
                // The pack key or Esc packs up; walking or jumping gets going.
                if fresh
                    && self.pack.is_open()
                    && (code == KeyCode::Escape || action == Some(Action::Pack) || walking)
                {
                    self.close_pack();
                    if code == KeyCode::Escape || action == Some(Action::Pack) {
                        return;
                    }
                }
                // The book key or Esc closes the book; walking or jumping
                // gets going.
                if fresh
                    && self.book.is_open()
                    && (code == KeyCode::Escape || action == Some(Action::Book) || walking)
                {
                    self.close_book();
                    if code == KeyCode::Escape || action == Some(Action::Book) {
                        return;
                    }
                }
                // Esc: close the admin menu, else open the Esc menu.
                if code == KeyCode::Escape {
                    if fresh {
                        if self.admin_menu_open {
                            self.admin_menu_open = false;
                        } else {
                            self.open_esc_menu();
                        }
                    }
                    return;
                }
                let Some(action) = action else { return };
                match action {
                    Action::Forward => self.held.forward = held,
                    Action::Back => self.held.back = held,
                    Action::Left => self.held.left = held,
                    Action::Right => self.held.right = held,
                    Action::Jump => {
                        self.held.jump = held;
                        if self.fly_eye.is_none() {
                            self.session.input.set_button(BUTTON_JUMP, held);
                        }
                    }
                    Action::Descend => self.held.descend = held,
                    Action::Sprint => {
                        self.held.fast = held;
                        if self.fly_eye.is_none() {
                            self.session.input.set_button(BUTTON_SPRINT, held);
                        }
                    }
                    Action::ThirdPerson if fresh => {
                        if self.player_view.is_some() {
                            self.third_person = !self.third_person;
                            println!(
                                "spall-interactive: third-person view {}",
                                if self.third_person { "ON" } else { "OFF" }
                            );
                        }
                        return;
                    }
                    Action::Screenshot if fresh => {
                        let stamp = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0, |d| d.as_millis());
                        self.pending_capture = Some(
                            PathBuf::from(".local/screenshots").join(format!("spall-{stamp}.png")),
                        );
                        return;
                    }
                    Action::CopyReport if fresh => {
                        self.copy_report_key = true;
                        return;
                    }
                    Action::AdminMenu if fresh => {
                        self.admin_menu_open = !self.admin_menu_open;
                        if self.admin_menu_open {
                            self.set_cursor_locked(false);
                        }
                        return;
                    }
                    Action::Flight if fresh => {
                        self.toggle_flight();
                        return;
                    }
                    Action::Recentre if fresh => {
                        self.recentre_view();
                        return;
                    }
                    Action::Pack if fresh => {
                        self.open_pack();
                        return;
                    }
                    Action::Book if fresh => {
                        self.open_book();
                        return;
                    }
                    Action::Interact if fresh && self.cursor_locked && !self.admin_menu_open => {
                        self.interact_pending = true;
                        return;
                    }
                    Action::Torch if fresh => {
                        self.torch_pending = true;
                        return;
                    }
                    Action::ToggleTerrain if fresh => {
                        self.show_terrain = !self.show_terrain;
                        println!(
                            "spall-interactive: terrain greedy meshes {}",
                            if self.show_terrain { "ON" } else { "OFF" }
                        );
                        return;
                    }
                    Action::ToggleBodies if fresh => {
                        self.show_bodies = !self.show_bodies;
                        println!(
                            "spall-interactive: body instances {}",
                            if self.show_bodies { "ON" } else { "OFF" }
                        );
                        return;
                    }
                    Action::ToggleCapsule if fresh => {
                        self.show_capsule = !self.show_capsule;
                        println!(
                            "spall-interactive: capsule debug markers {}",
                            if self.show_capsule { "ON" } else { "OFF" }
                        );
                        return;
                    }
                    Action::Skylight if fresh => {
                        self.toggle_sky_visibility();
                        return;
                    }
                    Action::Bounce if fresh => {
                        self.toggle_bounce();
                        return;
                    }
                    Action::Lighting if fresh => {
                        self.set_lighting(self.lighting.next());
                        return;
                    }
                    Action::DebugView if fresh => {
                        if let Some(renderer) = &mut self.renderer {
                            let view = next_debug_view(renderer.debug_view);
                            renderer.debug_view = view;
                            println!("spall-interactive: render view {}", view.stem());
                        }
                        return;
                    }
                    _ => return,
                }
                if self.fly_eye.is_none() {
                    self.publish_movement();
                }
            }
            WindowEvent::RedrawRequested => {
                let now = Instant::now();
                self.update_ambience();
                self.update_footsteps(now);
                self.update_creature_sounds(now);
                self.update_endurance();
                let handler_started = now;
                self.advance_script(now, event_loop);
                let due_for_report = self.hud.tick(now);
                let pack_dt = self.pack_frame_at.replace(now).map_or(0.0, |at| {
                    now.saturating_duration_since(at).as_secs_f32().min(0.1)
                });
                self.pack.advance(pack_dt);
                self.book.advance(pack_dt);
                if let Some(pack_view) = &self.pack_view
                    && (self.pack.is_open() || self.book.is_open())
                {
                    let reply = self
                        .session
                        .progression_reply
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take();
                    if let Some(reply) = reply {
                        let (outcome, retry) = self.inventory.take_reply(reply);
                        if let Some(outcome) = outcome {
                            self.book.crafted(&outcome, pack_view.as_ref());
                        }
                        if let Some(request) = retry {
                            self.session.push_progression(request);
                        }
                    }
                    self.book.reconcile(pack_view.as_ref(), &self.inventory);
                    if let Some(request) =
                        self.inventory.due_request(pack_view.catalog_version(), now)
                    {
                        self.session.push_progression(request);
                    }
                }

                // Drain the background worker's result, if a fresh one has
                // landed since the last frame (never blocks — `try_recv`).
                // Only the newest matters if somehow more than one queued up.
                while let Ok(outcome) = self.rebuild.result_rx.try_recv() {
                    if outcome.terrain_included {
                        self.hud
                            .record_rebuild(outcome.mesh_elapsed, outcome.mesh_updates.len());
                        self.last_built_pos = Some(outcome.center_m);
                        self.pending_mesh_updates.extend(outcome.mesh_updates);
                        self.pending_mesh_removed.extend(outcome.mesh_removed);
                        self.pending_mesh_detail_only
                            .extend(outcome.mesh_detail_only);
                        if outcome.terrain_complete {
                            self.rebuild.in_flight = false;
                        }
                    } else {
                        self.hud.record_sky(outcome.sky_stats);
                    }
                    if let Some(emitters) = outcome.emitters {
                        self.emitter_instances = emitters;
                    }
                    if let Some(sky) = outcome.sky {
                        self.last_sky = Some(sky);
                        self.sky_dirty = true;
                    }
                }

                let view = *self.session.view.lock().unwrap_or_else(|e| e.into_inner());

                if let Some(v) = view {
                    // Terrain is built around whatever the camera follows:
                    // the player, or the spectator camera while flying.
                    let feet = self
                        .fly_eye
                        .map_or(v.predicted.position_m, |eye| eye.to_array().map(f64::from));
                    let moved_far_enough = self.last_built_pos.is_none_or(|p| {
                        let d = [feet[0] - p[0], feet[1] - p[1], feet[2] - p[2]];
                        (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt() >= REBUILD_DISTANCE_M
                    });
                    let due_for_recheck = self
                        .last_dispatch_at
                        .is_none_or(|t| now - t >= TERRAIN_RECHECK_INTERVAL);
                    // At most one request in flight — a faster player than the
                    // worker can keep up with just rides on a slightly stale
                    // draw rather than queuing requests it'll never need.
                    // An edit landing (the replica's terrain generation moved)
                    // redraws at once: the per-brick cache makes that cheap, and
                    // waiting for the recheck timer made a hammer swing take
                    // most of a second to show.
                    let generation = self
                        .session
                        .replica
                        .get()
                        .and_then(|r| r.try_lock().ok().map(|g| g.terrain_generation()));
                    let terrain_changed =
                        generation.is_some() && generation != self.last_dispatch_generation;
                    if !self.rebuild.in_flight
                        && (moved_far_enough
                            || due_for_recheck
                            || terrain_changed
                            || self.force_rebuild)
                        && self.rebuild.request_tx.send(feet).is_ok()
                    {
                        self.force_rebuild = false;
                        self.rebuild.in_flight = true;
                        self.last_dispatch_at = Some(now);
                        self.last_dispatch_generation = generation;
                    }
                }

                // Bodies rebuild fresh every frame (cheap — never more than
                // each live body's own resident bricks) and get combined
                // with the last completed (throttled, background) terrain
                // rebuild, so the combined buffer is re-uploaded every frame
                // regardless of whether terrain itself changed. Bodies move
                // continuously; waiting on the terrain rebuild cadence to
                // show that would make them look like they teleport between
                // rebuilds instead of falling.
                // Drain the body worker the same way as the terrain one
                // above: never blocks, only the newest result matters.
                while let Ok(draws) = self.body_worker.result_rx.try_recv() {
                    self.last_body_draws = draws;
                }
                let mut overlay = if self.show_capsule
                    && let Some(v) = view
                {
                    build_capsule_debug_instances(v.predicted.position_m, self.debug_material)
                } else {
                    Vec::new()
                };
                // The cells the hammer would break (last frame's target).
                if let Some((cell, _)) = self.hammer_target
                    && !self.admin_menu_open
                    && !self.inventory_ui_shown()
                {
                    overlay.extend(hammer_outline(
                        cell,
                        self.hammer_radius,
                        self.debug_material,
                    ));
                }
                // Terrain lives in a resident GPU buffer and is re-uploaded
                // only when a rebuild landed or its visibility toggled --
                // not every frame. Bodies move continuously, so they are
                // re-posed (into their own reused buffer) every frame.
                let terrain_updates = std::mem::take(&mut self.pending_mesh_updates);
                let terrain_removed = std::mem::take(&mut self.pending_mesh_removed);
                let terrain_detail_only = std::mem::take(&mut self.pending_mesh_detail_only);
                // ENG-168: the water is a persistent height field around the
                // terrain window; only changed rectangles are recomposed and
                // uploaded, never a mesh rebuilt per publication.
                let water_window = self.last_built_pos;
                let mut menu_actions = AdminMenuActions::default();
                let Some(renderer) = &mut self.renderer else {
                    return;
                };
                renderer.weather_state = self.latest_weather.clone();
                renderer.weather_effects_enabled = self.weather_effects_enabled;
                renderer.ground_snow_enabled = self.ground_snow_enabled;
                renderer.reduced_weather_flashes = self.reduced_weather_flashes;
                renderer.scene.set_terrain_meshes_visible(self.show_terrain);
                let terrain_upload_started = Instant::now();
                let terrain_upload_count = terrain_updates.len();
                if (!terrain_updates.is_empty() || !terrain_removed.is_empty())
                    && let Err(error) = renderer.scene.update_terrain_meshes_detail(
                        &renderer.device,
                        &terrain_updates,
                        &terrain_removed,
                        &terrain_detail_only,
                    )
                {
                    self.fail(event_loop, ClientError::Render(error.to_string()));
                    return;
                }
                if frame_trace() && terrain_upload_count > 0 {
                    let ms = terrain_upload_started.elapsed().as_secs_f32() * 1000.0;
                    if ms > 1.0 {
                        eprintln!(
                            "spall-frame-work: t={:.1} terrain upload {terrain_upload_count} chunks {ms:.1} ms",
                            spall_render::trace_clock_ms()
                        );
                    }
                }
                renderer.sync_water_surface(&self.session.column_surface, water_window);
                renderer.vegetation_frame = self
                    .session
                    .vegetation
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                if let Some(mut replica) =
                    self.session.replica.get().and_then(|r| r.try_lock().ok())
                {
                    let focus = self
                        .fly_eye
                        .map(|eye| eye.to_array().map(f64::from))
                        .or_else(|| {
                            (*self.session.view.lock().unwrap_or_else(|e| e.into_inner()))
                                .map(|v| v.predicted.position_m)
                        });
                    if let Some(position) = focus {
                        replica.set_snow_focus(
                            (position[0] / 4.0).floor() as i32,
                            (position[2] / 4.0).floor() as i32,
                        );
                    }
                    self.player_health = replica.player_health();
                    let snow_generation = replica.snow_generation();
                    if renderer.snow_generation != Some(snow_generation) {
                        renderer.snow_frames = replica.snow_frames_shared();
                        renderer.snow_generation = Some(snow_generation);
                    }
                    let generation = replica.terrain_generation();
                    if renderer.vegetation_terrain.as_ref().map(|(g, _)| *g) != Some(generation)
                        && let Some(volume) = replica.terrain_volume()
                    {
                        renderer.vegetation_terrain = Some((generation, volume.clone()));
                    }
                }
                if !self.lighting_applied {
                    // The quality asked for at start (`SPALL_LIGHTING`).
                    self.lighting_applied = true;
                    if self.lighting == spall_render::sky_visibility::LightingQuality::Off {
                        self.sky_visibility_on = false;
                    }
                    self.bounce_on = self.lighting.bounce() && self.bounce_on;
                    self.sky_dirty = true;
                    renderer
                        .scene
                        .set_lighting_quality(&renderer.queue, self.lighting);
                }
                if self.sky_dirty {
                    let occupancy = self.last_sky.as_ref().filter(|_| self.sky_visibility_on);
                    // Nothing to upload yet (no rebuild has landed) keeps the
                    // dirty flag so the first occupancy is not lost.
                    if occupancy.is_some() || !self.sky_visibility_on {
                        renderer.scene.set_sky_occupancy(
                            &renderer.device,
                            &renderer.queue,
                            occupancy,
                        );
                        self.sky_dirty = false;
                    }
                }
                // The replica lock is only tried, never waited on, so the frame
                // never stalls behind the network thread. A busy frame reuses
                // the last creatures and players instead of not drawing them.
                if let Some(view) = &self.creature_view {
                    if let Some(replica) =
                        self.session.replica.get().and_then(|r| r.try_lock().ok())
                    {
                        self.last_creature_cubes = if replica.creatures().is_empty() {
                            Vec::new()
                        } else {
                            let feet =
                                (*self.session.view.lock().unwrap_or_else(|e| e.into_inner()))
                                    .map(|v| v.predicted.position_m);
                            let aspect = self.window.as_ref().map_or(16.0 / 9.0, |w| {
                                let size = w.inner_size();
                                size.width as f32 / size.height.max(1) as f32
                            });
                            let cull = creature_cull(
                                self.fly_eye,
                                feet,
                                Vec3::from_array(view_dir_from(self.yaw, self.pitch)),
                                (self.third_person && self.player_view.is_some())
                                    .then_some(self.third_person_distance_m),
                                aspect,
                            );
                            view.instances(
                                replica.creatures(),
                                replica.now_tick_estimate(),
                                self.creature_tint_base,
                                Instant::now(),
                                cull.as_ref(),
                            )
                        };
                    }
                    overlay.extend_from_slice(&self.last_creature_cubes);
                }
                if let Some(view) = &self.pack_view {
                    if let Some(replica) =
                        self.session.replica.get().and_then(|r| r.try_lock().ok())
                    {
                        self.last_item_cubes = world_item_instances(
                            replica.world_items(),
                            view.as_ref(),
                            self.item_tint_base,
                        );
                    }
                    overlay.extend_from_slice(&self.last_item_cubes);
                }
                let water = self
                    .session
                    .column_surface
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .query();
                if let Some(player_view) = &self.player_view {
                    if let Some(mut replica) =
                        self.session.replica.get().and_then(|r| r.try_lock().ok())
                    {
                        let tick = replica.render_tick(Instant::now());
                        self.last_remote_players = replica
                            .remote_players(tick)
                            .into_iter()
                            .map(|p| PlayerBody {
                                entity: p.entity.get(),
                                feet_m: p.feet_m,
                                velocity_m_s: p.velocity_m_s,
                                in_water: player_swims(&water, p.feet_m),
                                look_yaw: None,
                            })
                            .collect();
                    }
                    if !self.last_remote_players.is_empty() {
                        overlay.extend(player_view.instances(
                            &self.last_remote_players,
                            self.player_tint_base,
                            Instant::now(),
                        ));
                    }
                }
                let outcome = match renderer.begin_frame(None, &overlay) {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        self.fail(event_loop, error);
                        return;
                    }
                };
                // `begin_frame` uploaded the terrain before it could fail or
                // skip, so the resident buffer is current either way.
                let timing = match outcome {
                    AcquireOutcome::Skipped(timing) => timing,
                    AcquireOutcome::Ready(acquired) => {
                        // Read the freshest pose and compute the camera with
                        // a timestamp taken right now — after the swapchain
                        // acquire's variable-length wait above, not before
                        // it. ENG-69 round 13: `interactive-frames.jsonl`
                        // showed that wait ranging ~1-33ms frame to frame
                        // (see `desired_maximum_frame_latency`'s doc); doing
                        // this before `begin_frame`, as this used to, meant
                        // some frames' camera was measurably staler than
                        // others by the time they were actually drawn —
                        // uneven sideways motion during a strafe, most
                        // visible along a nearby object's silhouette.
                        let fresh_view =
                            *self.session.view.lock().unwrap_or_else(|e| e.into_inner());
                        let look_dir = Vec3::from_array(view_dir_from(self.yaw, self.pitch));
                        let render_now = Instant::now();
                        // Client-authoritative bodies are posed here, at the
                        // exact instant the camera is sampled, rather than by
                        // the background worker at some earlier moment: the
                        // worker's result is reused for several frames, so a
                        // body would hold still and then jump while the
                        // camera glided.
                        let local_poses = self
                            .session
                            .local_body_poses
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .clone();
                        let (body_mesh_updates, live_body_meshes) = if self.show_bodies {
                            pose_body_meshes(
                                &self.last_body_draws,
                                local_poses.as_ref(),
                                render_now,
                                &mut self.pose_stats,
                            )
                        } else {
                            (Vec::new(), Vec::new())
                        };
                        if let (Some((at, feet)), Some(v)) = (self.landing, fresh_view) {
                            let p = v.predicted.position_m;
                            let d2: f64 = (0..3).map(|i| (p[i] - feet[i]).powi(2)).sum();
                            // Arrived, or the server refused: drop the camera
                            // back onto whatever the player is.
                            if d2 < 1.0
                                || render_now.saturating_duration_since(at) > Duration::from_secs(2)
                            {
                                self.fly_eye = None;
                                self.landing = None;
                                self.last_fly_update = None;
                                self.last_built_pos = None;
                            }
                        }
                        let mut followed_eye = None;
                        let cam = fresh_view.map(|v| {
                            let followed =
                                self.camera_follow.eye(v, render_now, local_poses.is_none());
                            followed_eye = Some(followed);
                            if let Some(eye) = &mut self.fly_eye {
                                let dt = self
                                    .last_fly_update
                                    .replace(render_now)
                                    .map(|at| {
                                        render_now.saturating_duration_since(at).as_secs_f32()
                                    })
                                    .unwrap_or(0.0)
                                    .min(0.05);
                                let forward = Vec3::from_array(view_dir_from(self.yaw, 0.0));
                                // Positive X is screen-right when looking
                                // down -Z at the default yaw. `forward × up`
                                // keeps that basis aligned as yaw changes.
                                let right = Vec3::new(-forward.z, 0.0, forward.x);
                                let mut wish = Vec3::ZERO;
                                if self.held.forward {
                                    wish += forward;
                                }
                                if self.held.back {
                                    wish -= forward;
                                }
                                if self.held.right {
                                    wish += right;
                                }
                                if self.held.left {
                                    wish -= right;
                                }
                                if self.held.jump {
                                    wish.y += 1.0;
                                }
                                if self.held.descend {
                                    wish.y -= 1.0;
                                }
                                if wish.length_squared() > 0.0 && self.landing.is_none() {
                                    let speed = if self.held.fast {
                                        FLY_FAST_SPEED_M_S
                                    } else {
                                        FLY_SPEED_M_S
                                    };
                                    *eye += wish.normalize() * (speed * dt);
                                }
                                (*eye, look_dir)
                            } else {
                                (followed, look_dir)
                            }
                        });
                        let screen = renderer.ui_size();
                        let scale = renderer.ui_scale();
                        let cursor = self.cursor_px.map(|[x, y]| [x / scale, y / scale]);
                        let stacks = self.inventory.stacks();
                        renderer.pack_frame = self.pack_view.as_ref().and_then(|pack_view| {
                            if let Some(cursor) = cursor {
                                self.pack.hover(screen, cursor, stacks);
                            }
                            let mut frame = self.pack.frame(pack_view.as_ref(), screen, stacks)?;
                            for slot in &mut frame.slots {
                                if Some(slot.item_id) == self.held_item {
                                    slot.name.push_str(" (in hand)");
                                }
                            }
                            Some(frame)
                        });
                        renderer.esc_menu = self.esc.open.then(|| self.esc.view(&self.keys));
                        renderer.compass_yaw =
                            (self.held_compass && cam.is_some() && !self.esc.open)
                                .then_some(self.yaw);
                        renderer.note_text = self.reading_note.clone();
                        renderer.book_frame = self.pack_view.as_ref().and_then(|pack_view| {
                            self.book.pointer(screen, cursor.unwrap_or([-1.0; 2]));
                            self.book.frame(pack_view.as_ref(), &self.inventory)
                        });
                        self.hammer_target = match cam {
                            Some((eye, dir)) if self.hammer_on => {
                                match self.session.replica.get().and_then(|r| r.try_lock().ok()) {
                                    Some(replica) => replica
                                        .terrain_volume()
                                        .and_then(|volume| cast_hammer_ray(volume, eye, dir)),
                                    // The network thread holds the replica: keep
                                    // last frame's target rather than flicker.
                                    None => self.hammer_target,
                                }
                            }
                            _ => None,
                        };
                        if let Some((eye, _)) = cam {
                            renderer.scene.set_point_lights(&emitter_point_lights(
                                &self.emitter_instances,
                                &self.render_materials,
                                eye,
                            ));
                        }
                        // Third person: the hammer and torch still aim from the eye
                        // (`cam`); only the drawn camera moves back, and the local
                        // body appears. First person never draws it.
                        let mut render_cam = cam;
                        if self.third_person
                            && self.fly_eye.is_none()
                            && let (Some(player_view), Some(v), Some(eye), Some((_, dir))) =
                                (&self.player_view, fresh_view, followed_eye, cam)
                        {
                            let target = self
                                .session
                                .replica
                                .get()
                                .and_then(|r| r.try_lock().ok())
                                .map(|replica| {
                                    replica
                                        .terrain_volume()
                                        .map_or(THIRD_PERSON_DISTANCE_M, |volume| {
                                            third_person_distance(volume, eye, dir)
                                        })
                                });
                            // Pull in at once so terrain never ends up inside the
                            // camera, ease back out, and hold the last distance on
                            // a frame that found the replica busy.
                            let dt = self.third_person_at.map_or(0.0, |at| {
                                render_now.saturating_duration_since(at).as_secs_f32()
                            });
                            self.third_person_at = Some(render_now);
                            if let Some(target) = target {
                                let current = self.third_person_distance_m;
                                self.third_person_distance_m = if target < current {
                                    target
                                } else {
                                    current + (target - current) * (dt * 4.0).min(1.0)
                                };
                            }
                            let reach = self.third_person_distance_m;
                            render_cam = Some((third_person_eye(eye, dir, reach), dir));
                            let eye_height =
                                f64::from(CharacterParams::DEFAULT.total_height_m()) * 0.9;
                            let feet = [
                                f64::from(eye.x),
                                f64::from(eye.y) - eye_height,
                                f64::from(eye.z),
                            ];
                            let local = PlayerBody {
                                entity: 0,
                                feet_m: feet,
                                velocity_m_s: v.predicted.velocity_m_s,
                                in_water: player_swims(&water, feet),
                                look_yaw: Some(self.yaw),
                            };
                            overlay.extend(player_view.instances(
                                &[local],
                                self.player_tint_base,
                                render_now,
                            ));
                            renderer
                                .scene
                                .set_overlay(&renderer.device, &renderer.queue, &overlay);
                        }
                        let prompt = if !self.admin_menu_open && self.fly_eye.is_none() {
                            cam.and_then(|(eye, _)| {
                                let replica = self.session.replica.get()?.try_lock().ok()?;
                                interaction_prompt(
                                    &replica,
                                    eye.to_array().map(f64::from),
                                    self.door_spec,
                                    self.pack_view.as_deref(),
                                )
                            })
                        } else {
                            None
                        };
                        let prompted = prompt.is_some();
                        renderer.set_prompt(prompt);
                        if self.interact_pending {
                            self.interact_pending = false;
                            if prompted && let Some((eye, dir)) = cam {
                                let id = self.next_action_id;
                                self.next_action_id += 1;
                                self.session.push_action(tool_request(
                                    id,
                                    5,
                                    spall_protocol::ActionKind::Interact,
                                    eye,
                                    dir,
                                    0,
                                ));
                            }
                        }
                        renderer.set_endurance(self.endurance_view);
                        // A held item that has left the pack is no longer in hand.
                        if let Some(item) = self.held_item
                            && self.inventory.stacks().is_some()
                            && self.inventory.count(item) == 0
                        {
                            self.held_item = None;
                        }
                        let held_tool = self.held_item.and_then(|item| {
                            Some((item, self.pack_view.as_ref()?.item_tool(item)?))
                        });
                        let held_name = held_tool.map_or_else(
                            || "empty hands".to_owned(),
                            |(item, _)| {
                                self.pack_view
                                    .as_ref()
                                    .and_then(|view| view.item_name(item))
                                    .unwrap_or("tool")
                                    .to_lowercase()
                            },
                        );
                        renderer.set_crosshair(
                            (!self.admin_menu_open
                                && !self.esc.open
                                && self.reading_note.is_none()
                                && !self.pack.is_shown()
                                && !self.book.is_shown()
                                && cam.is_some())
                            .then(|| CrosshairView {
                                in_reach: self.hammer_target.is_some(),
                                label: match self.hammer_target {
                                    // A click only captures the mouse until it is.
                                    _ if !self.cursor_locked => {
                                        "click to capture the mouse, then click to break".to_owned()
                                    }
                                    _ if !self.hammer_on => held_name.clone(),
                                    Some((_, material)) => format!(
                                        "hammer r{}  {}",
                                        self.hammer_radius,
                                        self.material_names
                                            .get(&material.0)
                                            .map_or("?", String::as_str)
                                    ),
                                    None => {
                                        format!("hammer r{}  out of reach", self.hammer_radius)
                                    }
                                },
                            }),
                        );
                        if let Some((eye, dir)) = cam
                            && self.cursor_locked
                            && self.hammer_on
                            && swing_due(
                                self.hammer_pending,
                                self.hammer_held,
                                self.last_hammer_at,
                                render_now,
                                HAMMER_REPEAT,
                            )
                        {
                            self.hammer_pending = false;
                            self.last_hammer_at = Some(render_now);
                            let id = self.next_action_id;
                            self.next_action_id += 1;
                            // Draw the cut now, a round trip before the server's.
                            if let Some((cell, _)) = self.hammer_target {
                                let mut pending = self
                                    .rebuild
                                    .pending
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner());
                                if pending.len() < MAX_PENDING_CUTS {
                                    pending.push(PendingCut {
                                        cell,
                                        radius_cells: self.hammer_radius,
                                        at: render_now,
                                    });
                                }
                                self.force_rebuild = true;
                            }
                            self.session.push_action(hammer_request(
                                id,
                                eye,
                                dir,
                                self.hammer_radius,
                            ));
                        }
                        // Off the hammer: left click swings the tool in hand;
                        // empty-handed, a click does nothing.
                        if !self.hammer_on {
                            match (cam, held_tool) {
                                (Some((eye, dir)), Some((_, tool)))
                                    if self.cursor_locked
                                        && swing_due(
                                            self.hammer_pending,
                                            self.hammer_held,
                                            self.last_hammer_at,
                                            render_now,
                                            TOOL_REPEAT,
                                        ) =>
                                {
                                    self.last_hammer_at = Some(render_now);
                                    let id = self.next_action_id;
                                    self.next_action_id += 1;
                                    // The server sets the stroke's radius.
                                    self.session.push_action(tool_request(
                                        id,
                                        tool,
                                        spall_protocol::ActionKind::Cut,
                                        eye,
                                        dir,
                                        0,
                                    ));
                                }
                                _ => {}
                            }
                            self.hammer_pending = false;
                        }
                        if self.torch_pending {
                            self.torch_pending = false;
                            if let Some((eye, dir)) = cam
                                && self.cursor_locked
                            {
                                let id = self.next_action_id;
                                self.next_action_id += 1;
                                self.session.push_action(torch_request(id, eye, dir));
                            }
                        }
                        if let Some(path) = self.pending_capture.take() {
                            renderer.screenshot_request = Some(path);
                        }
                        // Body state: eased from the weather, shelter and
                        // water the client already sees, then shown as a
                        // full-screen grade (presentation only).
                        let body_now = Instant::now();
                        let body_dt = self
                            .body_state_at
                            .map_or(0.0, |at| body_now.duration_since(at).as_secs_f32());
                        self.body_state_at = Some(body_now);
                        let submerged = fresh_view.is_some_and(|v| {
                            let [x, y, z] = v.predicted.position_m;
                            // Waist height: wading ankle-deep does not soak you.
                            let waist = y + 0.9;
                            self.session
                                .column_surface
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .surface_at_m(x, waist, z)
                                .is_some_and(|surface| surface > waist)
                        });
                        let conditions = BodyConditions {
                            air_temperature_c: self
                                .latest_weather
                                .as_ref()
                                .map_or(15.0, |w| f32::from(w.temperature_centi_c) / 100.0),
                            exposure: self.weather_exposure,
                            precipitation: self.latest_weather.as_ref().map_or(0.0, |w| {
                                let amount = f32::from(w.precipitation_q16) / f32::from(u16::MAX);
                                match w.kind {
                                    spall_protocol::WeatherKind::Rain
                                    | spall_protocol::WeatherKind::Storm => amount,
                                    // Snow mostly brushes off.
                                    spall_protocol::WeatherKind::Snow
                                    | spall_protocol::WeatherKind::Blizzard => 0.3 * amount,
                                    _ => 0.0,
                                }
                            }),
                            submerged,
                        };
                        self.body_state.advance(&conditions, body_dt);
                        // Real injury from the server's health, or the admin
                        // preview when that is stronger.
                        let body_looks =
                            crate::body_state::with_health(&self.body_preview, self.player_health);
                        renderer.body_reminders =
                            crate::body_state::reminders(&self.body_state, &body_looks);
                        renderer
                            .scene
                            .set_screen_grade(crate::body_state::screen_grade(
                                &self.body_state,
                                &body_looks,
                                self.body_epoch.elapsed().as_secs_f32(),
                            ));
                        let menu_view = self.admin_menu_open.then(|| {
                            let mut menu = admin_menu_view(
                                &self.session,
                                self.fly_eye.is_some(),
                                self.admin_request_pending,
                                self.sky_visibility_on,
                                self.bounce_on,
                                self.lighting.name(),
                                renderer.day_cycle_enabled,
                                renderer.day_clock_label.clone(),
                                self.report_status
                                    .as_ref()
                                    .filter(|(_, at)| at.elapsed() < Duration::from_secs(20))
                                    .map(|(text, _)| text.clone()),
                                frame_summary(&self.frame_history),
                            );
                            menu.hammer_on = self.hammer_on;
                            menu.weather = self
                                .latest_weather
                                .as_ref()
                                .map(|state| {
                                    format!(
                                        "Weather: {} ({}) | {} | {:.1} C",
                                        weather_state_label(state),
                                        if state.override_kind.is_some() {
                                            "forced"
                                        } else {
                                            "auto"
                                        },
                                        ["Spring", "Summer", "Autumn", "Winter"][(u64::from(
                                            state.start_season
                                        ) + state
                                            .clock_ms
                                            / state.season_ms.max(1))
                                            as usize
                                            % 4],
                                        f32::from(state.temperature_centi_c) / 100.0
                                    )
                                })
                                .unwrap_or_else(|| "Weather: waiting for server".into());
                            menu.nature_volume = self.nature_volume;
                            menu.weather_volume = self.weather_volume;
                            menu.footstep_volume = self.footstep_volume;
                            menu.nature_muted = self.nature_muted;
                            menu.nature_selection = self
                                .nature_preview
                                .and_then(|i| self.biome_audio.labels.get(i).cloned())
                                .unwrap_or_else(|| "Follow local biome".into());
                            menu.weather_muted = self.weather_muted;
                            menu.weather_effects_enabled = self.weather_effects_enabled;
                            menu.ground_snow_enabled = self.ground_snow_enabled;
                            let snow = renderer.snow_presentation.stats();
                            menu.snow = format!(
                                "Nearby snow: {} faces | up to {} mm | {} tracks",
                                snow.rendered_faces, snow.max_depth_mm, snow.footprints,
                            );
                            menu.reduced_weather_flashes = self.reduced_weather_flashes;
                            menu.body = self.body_state;
                            menu.body_preview = self.body_preview;
                            menu
                        });
                        renderer.eye_submerged = render_cam.as_ref().is_some_and(|(eye, _)| {
                            eye_below_surface(
                                self.session
                                    .column_surface
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .sample_at_m(
                                        f64::from(eye.x),
                                        f64::from(eye.y),
                                        f64::from(eye.z),
                                    ),
                                f64::from(eye.y),
                            )
                        });
                        match renderer.finish_frame(
                            acquired,
                            render_cam.as_ref(),
                            &body_mesh_updates,
                            &live_body_meshes,
                            None,
                            menu_view.as_ref().map(|view| (view, &mut menu_actions)),
                            view.map(|view| view.server_tick),
                            self.session.day_clock.get().copied().unwrap_or_default(),
                        ) {
                            Ok(timing) => timing,
                            Err(error) => {
                                self.fail(event_loop, error);
                                return;
                            }
                        }
                    }
                };
                self.hud.record_frame(&timing);
                if let Some(renderer) = &self.renderer {
                    self.weather_exposure = renderer.weather_exposure;
                    self.weather_submerge = renderer.weather_submerge;
                    self.weather_wind = renderer.weather_wind;
                }
                if self.frame_history.len() >= FRAME_HISTORY {
                    self.frame_history.pop_front();
                }
                self.frame_history
                    .push_back(handler_started.elapsed().as_secs_f32() * 1000.0);
                if self.frame_waits.len() >= FRAME_HISTORY {
                    self.frame_waits.pop_front();
                }
                self.frame_waits
                    .push_back((timing.acquire_ms, timing.submit_ms));
                if self.frame_work.len() >= FRAME_HISTORY {
                    self.frame_work.pop_front();
                }
                self.frame_work.push_back(
                    (handler_started.elapsed().as_secs_f32() * 1000.0
                        - timing.acquire_ms
                        - timing.present_ms)
                        .max(0.0),
                );
                if std::env::var_os("SPALL_FRAME_CSV").is_some() {
                    eprintln!(
                        "spall-frame-csv: {:.1},{:.2},{:.2},{:.2},{}",
                        spall_render::trace_clock_ms(),
                        handler_started.elapsed().as_secs_f32() * 1000.0,
                        timing.acquire_ms,
                        timing.submit_ms,
                        self.renderer.as_ref().map_or(0, |r| r.water_rects_frame)
                    );
                }
                if frame_trace() {
                    let total = handler_started.elapsed().as_secs_f32() * 1000.0;
                    if total > 20.0 {
                        eprintln!(
                            "spall-frame-trace: t={:.1} handler {total:.1} ms = render {:.1} (acquire {:.1}, submit {:.1}, present {:.1}) + other {:.1}",
                            spall_render::trace_clock_ms(),
                            timing.total_ms,
                            timing.acquire_ms,
                            timing.submit_ms,
                            timing.present_ms,
                            total - timing.total_ms
                        );
                    }
                }
                if menu_actions.toggle_flight {
                    self.toggle_flight();
                }
                if menu_actions.toggle_hammer {
                    self.hammer_on = !self.hammer_on;
                    self.hammer_pending = false;
                }
                if menu_actions.toggle_sky_visibility {
                    self.toggle_sky_visibility();
                }
                if menu_actions.toggle_bounce {
                    self.toggle_bounce();
                }
                if menu_actions.cycle_lighting {
                    self.set_lighting(self.lighting.next());
                }
                if menu_actions.recentre {
                    self.recentre_view();
                }
                if menu_actions.copy_debug_report || std::mem::take(&mut self.copy_report_key) {
                    let report = self.debug_report(view.as_ref());
                    let status = copy_report(&report);
                    println!("spall-interactive: {status}");
                    self.report_status = Some((status, Instant::now()));
                }
                if menu_actions.reset_world {
                    self.admin_request_pending = true;
                    self.session
                        .push_admin(spall_protocol::AdminCommand::ResetWorld);
                }
                if let Some(rate) = menu_actions.set_water_spring_rate {
                    self.admin_request_pending = true;
                    self.session
                        .push_admin(spall_protocol::AdminCommand::SetWaterSpring { rate });
                }
                if let Some(open) = menu_actions.set_dam_gate {
                    self.admin_request_pending = true;
                    self.session
                        .push_admin(spall_protocol::AdminCommand::SetDamGate { open });
                }
                if let Some((kind, intensity)) = menu_actions.set_weather {
                    self.admin_request_pending = true;
                    self.session
                        .push_admin(spall_protocol::AdminCommand::SetWeather { kind, intensity });
                }
                if let Some(season) = menu_actions.set_season {
                    self.admin_request_pending = true;
                    self.session
                        .push_admin(spall_protocol::AdminCommand::SetSeason { season });
                }
                if menu_actions.toggle_nature_audio {
                    self.nature_muted = !self.nature_muted;
                }
                if menu_actions.nature_follow {
                    self.nature_preview = None;
                }
                if menu_actions.nature_next && !self.biome_audio.tracks.is_empty() {
                    self.nature_preview = Some(
                        self.nature_preview
                            .map_or(0, |i| (i + 1) % self.biome_audio.tracks.len()),
                    );
                }
                if menu_actions.toggle_weather_audio {
                    self.weather_muted = !self.weather_muted;
                }
                self.nature_volume =
                    (self.nature_volume + menu_actions.nature_volume_delta).clamp(0.0, 1.0);
                self.weather_volume =
                    (self.weather_volume + menu_actions.weather_volume_delta).clamp(0.0, 1.0);
                self.footstep_volume =
                    (self.footstep_volume + menu_actions.footstep_volume_delta).clamp(0.0, 1.0);
                if menu_actions.toggle_weather_effects {
                    self.weather_effects_enabled = !self.weather_effects_enabled;
                }
                if menu_actions.toggle_ground_snow {
                    self.ground_snow_enabled = !self.ground_snow_enabled;
                }
                let preview = &mut self.body_preview;
                let delta = menu_actions.body_preview_delta;
                for (look, step) in [
                    (&mut preview.injury, delta.injury),
                    (&mut preview.thirst, delta.thirst),
                    (&mut preview.exhaustion, delta.exhaustion),
                    (&mut preview.hunger, delta.hunger),
                ] {
                    *look = (*look + step).clamp(0.0, 1.0);
                }
                if menu_actions.clear_body_preview {
                    *preview = BodyPreview::default();
                }
                if menu_actions.toggle_weather_flashes {
                    self.reduced_weather_flashes = !self.reduced_weather_flashes;
                }
                if menu_actions.close {
                    self.admin_menu_open = false;
                }
                let esc_actions = self
                    .renderer
                    .as_mut()
                    .map(|r| std::mem::take(&mut r.esc_actions))
                    .unwrap_or_default();
                if esc_actions.quit {
                    self.session.request_stop();
                    event_loop.exit();
                }
                if esc_actions.reset_defaults {
                    self.keys = KeyBindings::default();
                    self.keys.save();
                    self.esc.capturing = None;
                    self.esc.message = None;
                }
                if let Some(action) = esc_actions.start_rebind {
                    self.esc.capturing = Some(action);
                    self.esc.message = None;
                }
                if esc_actions.resume {
                    self.close_esc_menu();
                }
                // A reset baseline replaced the world: drop the old view.
                let resets = self
                    .session
                    .world_resets
                    .load(std::sync::atomic::Ordering::Relaxed);
                if resets != self.seen_world_resets {
                    self.seen_world_resets = resets;
                    self.last_built_pos = None;
                    self.last_body_draws.clear();
                    self.latest_weather = None;
                    self.thunder_event = None;
                    self.pending_thunder = None;
                    if let Some(renderer) = &mut self.renderer {
                        renderer.weather_presentation.reset();
                        renderer.snow_frames.clear();
                        renderer.snow_generation = None;
                        renderer.snow_upload_key = None;
                        let resident = renderer.snow_presentation.resident_keys();
                        renderer.snow_presentation = crate::snow::SnowPresentation::new();
                        if let Err(error) =
                            renderer
                                .scene
                                .update_snow_meshes(&renderer.device, &[], &resident)
                        {
                            tracing::warn!(%error, "clearing snow meshes failed");
                        }
                    }
                    if self.fly_eye.is_some() {
                        self.fly_eye = None;
                        self.landing = None;
                        self.last_fly_update = None;
                    }
                }
                if self.admin_request_pending
                    && self
                        .session
                        .admin_status
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .is_some()
                {
                    self.admin_request_pending = false;
                }
                if let Some(frames) = &self.session.frames {
                    frames.record(
                        timing.total_ms,
                        timing.buffer_upload_ms,
                        timing.instance_count,
                        timing.acquire_ms,
                        timing.submit_ms,
                        timing.present_ms,
                    );
                }
                if due_for_report {
                    let server_tick = view.map_or(0, |v| v.server_tick);
                    let corrections = view.map_or(0, |v| v.corrections);
                    let max_correction_m = view.map_or(0.0, |v| v.max_correction_m);
                    let idle_corrections = view.map_or(0, |v| v.idle_corrections);
                    let max_idle_correction_m = view.map_or(0.0, |v| v.max_idle_correction_m);
                    let max_vertical_correction_m =
                        view.map_or(0.0, |v| v.max_vertical_correction_m);
                    let max_horizontal_correction_m =
                        view.map_or(0.0, |v| v.max_horizontal_correction_m);
                    let unmatched_reconciles = view.map_or(0, |v| v.unmatched_reconciles);
                    let max_unmatched_displacement_m =
                        view.map_or(0.0, |v| v.max_unmatched_displacement_m);
                    let window_stats = view.map_or_else(Default::default, |v| v.window_stats);
                    let prediction_steps = view.map_or(0, |v| v.prediction_steps);
                    let prediction_elapsed_ms = view.map_or(0, |v| v.prediction_elapsed_ms);
                    let prediction_max_backlog_steps =
                        view.map_or(0, |v| v.prediction_max_backlog_steps);
                    let prediction_dropped_steps = view.map_or(0, |v| v.prediction_dropped_steps);
                    let line = self.hud.report(
                        now,
                        server_tick,
                        corrections,
                        max_correction_m,
                        idle_corrections,
                        max_idle_correction_m,
                        max_vertical_correction_m,
                        max_horizontal_correction_m,
                        unmatched_reconciles,
                        max_unmatched_displacement_m,
                        prediction_steps,
                        prediction_elapsed_ms,
                        prediction_max_backlog_steps,
                        prediction_dropped_steps,
                        window_stats,
                    );
                    let line = format!("{line} | {}", self.pose_stats.take_report());
                    // Headroom right after the fps, so it shows in the title.
                    let gpu_ms = self
                        .renderer
                        .as_ref()
                        .and_then(|r| r.scene.pass_timings())
                        .map(|t| t.total_ms());
                    let line = match (headroom(&self.frame_work, gpu_ms), line.split_once(" | ")) {
                        (Some(headroom), Some((fps, rest))) => {
                            format!("{fps} | {headroom} | {rest}")
                        }
                        (Some(headroom), None) => format!("{line} | {headroom}"),
                        (None, _) => line,
                    };
                    let line = match view {
                        Some(v) => format!(
                            "{line} | feet ({:.2}, {:.2}, {:.2}) m",
                            v.predicted.position_m[0],
                            v.predicted.position_m[1],
                            v.predicted.position_m[2]
                        ),
                        None => line,
                    };
                    let line = match &self.renderer {
                        Some(renderer) => format!(
                            "{line} | {} | {}",
                            renderer.scene_report(),
                            self.hud.sky_report()
                        ),
                        None => line,
                    };
                    if let Some(window) = &self.window {
                        window.set_title(&format!("Spall sandbox — interactive | {line}"));
                    }
                    eprintln!("spall-interactive: {line}");
                    self.last_hud_line = line;
                }
                // The next frame is requested by `about_to_wait`, which paces it.
            }
            _ => {}
        }
    }

    /// The network thread's wake-up (see `run_interactive_window`): the
    /// session ended, either the server closing it or a failed connect.
    /// `net_result` (checked after `run_app` returns) carries the actual
    /// error, if any — this just makes sure the window doesn't sit there
    /// showing an empty scene once there is no session left to drive it.
    fn user_event(&mut self, event_loop: &ActiveEventLoop, _event: ()) {
        event_loop.exit();
    }

    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _device_id: winit::event::DeviceId,
        event: DeviceEvent,
    ) {
        if let DeviceEvent::MouseMotion { delta } = event
            && self.cursor_locked
        {
            self.yaw += delta.0 as f32 * MOUSE_SENSITIVITY;
            self.pitch =
                (self.pitch - delta.1 as f32 * MOUSE_SENSITIVITY).clamp(-MAX_PITCH, MAX_PITCH);
            self.publish_look();
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Without vsync nothing paces the loop: the loading screen ran at
        // thousands of frames a second and starved the rest of the PC (the
        // desktop shares the GPU). Wait until the next frame is due instead.
        let interval = self.max_frame_interval();
        let next = self.last_redraw + interval;
        if interval.is_zero() || Instant::now() >= next {
            self.last_redraw = Instant::now();
            event_loop.set_control_flow(ControlFlow::Poll);
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        } else {
            event_loop.set_control_flow(ControlFlow::WaitUntil(next));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::BUTTON_JUMP;

    #[test]
    fn focus_loss_clears_window_keys_and_shared_actions_but_keeps_look() {
        let mut held = HeldKeys {
            forward: true,
            left: true,
            jump: true,
            ..HeldKeys::default()
        };
        let input = LiveInput::new();
        input.set_movement(held.movement());
        input.set_view_dir([0.5, 0.25, -0.75]);
        input.set_button(BUTTON_JUMP, true);

        held.clear_on_focus_loss(&input);

        assert_eq!(held.movement(), [0.0; 3]);
        let snapshot = input.snapshot();
        assert_eq!(snapshot.movement, [0.0; 3]);
        assert_eq!(snapshot.buttons, 0);
        assert_eq!(snapshot.view_dir, [0.5, 0.25, -0.75]);
    }
}
