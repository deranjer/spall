use super::*;

pub(crate) struct WorldRenderer {
    pub(super) weather_state: Option<spall_protocol::WeatherState>,
    pub(super) weather_presentation: crate::weather::WeatherPresentation,
    weather_render_at: Instant,
    pub(super) weather_exposure: f32,
    pub(super) weather_effects_enabled: bool,
    pub(super) ground_snow_enabled: bool,
    pub(super) snow_frames: Vec<std::sync::Arc<spall_protocol::SnowSurfaceFrame>>,
    pub(super) snow_generation: Option<u64>,
    pub(super) snow_presentation: crate::snow::SnowPresentation,
    pub(super) snow_upload_key: Option<(u64, u64, [i64; 3], bool)>,
    /// When ground snow geometry was last rebuilt; rebuilds cost several ms, so
    /// arriving snow frames and camera movement are coalesced.
    snow_rebuilt_at: Option<Instant>,
    pub(super) reduced_weather_flashes: bool,
    pub(super) environment: Environment,
    pub(super) day_cycle_enabled: bool,
    pub(super) day_clock_label: String,
    pub(super) device: wgpu::Device,
    pub(super) queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    /// The shared `spall_render` frame renderer (shadows, HDR, tone map).
    /// Terrain, bodies and debug overlays live in its resident instance
    /// buffers.
    pub(super) scene: GameRenderer,
    pub(super) vegetation_frame: Option<Arc<spall_ecology::living::VisualFrame>>,
    /// A cheap clone of the replica's terrain, for the cover worker to read
    /// surfaces from, with the replica generation it was taken at.
    pub(super) vegetation_terrain: Option<(u64, Volume)>,
    vegetation_key: Option<VegetationKey>,
    /// When the foliage was last rebuilt, to space rebuilds out.
    vegetation_built: Instant,
    vegetation_worker: VegetationWorker,
    vegetation_job: Option<u64>,
    vegetation_seq: u64,
    /// Water as merged boxes, unclipped; the per-frame draw list in
    /// `debug_water_instances` is this clipped to the terrain window.
    /// The hammer crosshair and its label; `None` hides it (cursor not captured).
    crosshair: Option<CrosshairView>,
    debug_water_source: Vec<Instance>,
    water_window: Option<[f64; 3]>,
    /// Rectangles of the water field uploaded this frame (for the frame CSV).
    pub(super) water_rects_frame: usize,
    debug_water_instances: Vec<Instance>,
    /// Clock for the water's animated caustics.
    water_epoch: Instant,
    /// The grid water surface is installed and drawing.
    water_grid_on: bool,
    /// `F4` cycles this through the renderer's debug views.
    pub(super) debug_view: DebugView,
    /// The last camera basis the window built (`InteractiveApp` computes eye
    /// position / look direction; this struct only knows the surface aspect
    /// ratio needed to finish the projection).
    aspect: f32,
    yakui: yakui::Yakui,
    yakui_winit: YakuiWinit,
    yakui_wgpu: YakuiWgpu,
    yakui_buffers: yakui_wgpu::Buffers,
    hud_gpu_timer: Option<HudGpuTimer>,
    /// Set to capture the next finished frame to this PNG path.
    pub(crate) screenshot_request: Option<PathBuf>,
}

/// One render frame's timing breakdown — see [`WorldRenderer::begin_frame`]/
/// [`WorldRenderer::finish_frame`] and `Hud::record_frame`. Added ENG-69
/// round 12: the averaged `frame_ms_ema`
/// the HUD already tracked hides short, occasional stalls; a video review of
/// an actual hands-on run showed a "hold, then jump" pattern (~60% of
/// consecutive frames nearly identical, interspersed with larger jumps) that
/// an average can't diagnose. This breaks a frame into the stages that can
/// plausibly eat a vsync interval's worth of time on their own.
pub(crate) struct FrameTiming {
    /// Wall time for the whole `render` call.
    pub(super) total_ms: f32,
    /// `Some` only on the (occasional) frame a background `RebuildWorker`
    /// result landed and `begin_frame` uploaded a fresh instance buffer —
    /// see `create_buffer_init` there. This reallocates and copies the
    /// *entire* buffer every time rather than reusing one; round 12
    /// suspected this (a bigger buffer since `view_radius_m()` went 10m ->
    /// 48m) as the stall's cause, but round 13's data disproved it — this
    /// stayed under 0.2ms on every measured frame, stalled or not. Kept
    /// for visibility, not because it's still a live suspect.
    pub(super) buffer_upload_ms: Option<f32>,
    /// Instance count uploaded, when `buffer_upload_ms` is `Some`.
    pub(super) instance_count: Option<u32>,
    /// Time blocked in `surface.get_current_texture()` — round 13 found
    /// this is where virtually all frame-to-frame timing variance actually
    /// lives (a regular burst pattern from `desired_maximum_frame_latency`
    /// letting the CPU queue ahead of the display; see that field's doc).
    pub(super) acquire_ms: f32,
    /// Time in `queue.submit()` (usually just enqueues; doesn't normally
    /// wait on the GPU).
    pub(super) submit_ms: f32,
    /// Time in `surface_texture.present()` — on some backends this, not
    /// `acquire`, is where `PresentMode::Fifo` actually blocks for vsync.
    pub(super) present_ms: f32,
    pub(super) hud_cpu_ms: f32,
    pub(super) hud_gpu_ms: Option<f32>,
}

impl FrameTiming {
    /// A frame that bailed out of `render` early (an `Outdated`/`Lost`/
    /// `Timeout` swapchain acquire) — whatever ran before the early return
    /// still counts, the rest is `0.0`.
    fn early_return(
        total_ms: f32,
        buffer_upload_ms: Option<f32>,
        instance_count: Option<u32>,
        acquire_ms: f32,
    ) -> Self {
        Self {
            total_ms,
            buffer_upload_ms,
            instance_count,
            acquire_ms,
            submit_ms: 0.0,
            present_ms: 0.0,
            hud_cpu_ms: 0.0,
            hud_gpu_ms: None,
        }
    }
}

/// A frame that has acquired its swapchain image, awaiting only the
/// camera-dependent draw ([`WorldRenderer::finish_frame`]) — see
/// [`WorldRenderer::begin_frame`].
pub(crate) struct AcquiredFrame {
    frame_start: Instant,
    surface_texture: wgpu::SurfaceTexture,
    view: wgpu::TextureView,
    buffer_upload_ms: Option<f32>,
    instance_count: Option<u32>,
    acquire_ms: f32,
}

/// [`WorldRenderer::begin_frame`]'s result: either an acquired frame ready
/// for [`WorldRenderer::finish_frame`], or a frame that bailed out early
/// (an `Outdated`/`Lost`/`Timeout` swapchain acquire) and already has its
/// complete (if mostly-zero) timing.
pub(crate) enum AcquireOutcome {
    Ready(AcquiredFrame),
    Skipped(FrameTiming),
}

impl WorldRenderer {
    /// Installs (or, with `None`, clears) the smoothed water surface and the
    /// height field behind the underwater look.
    /// Brings the GPU water field up to date with the replicated surface:
    /// recentres it on the terrain window, eases the heights, and uploads the
    /// whole field only when it was replaced and otherwise just the rectangles
    /// that changed. With no terrain window yet there is nothing to anchor
    /// water to.
    pub(crate) fn sync_water_surface(
        &mut self,
        surface: &std::sync::Mutex<crate::column_surface::ColumnSurface>,
        window: Option<[f64; 3]>,
    ) {
        self.water_rects_frame = 0;
        // A field being replaced goes up a slice a frame (about a millisecond
        // each) instead of stalling the frame that recentred it.
        self.scene
            .advance_water_upload(&self.queue, WATER_UPLOAD_ROWS_PER_FRAME);
        let Some(center) = window else {
            return;
        };
        let trace = crate::column_surface::water_trace().then(Instant::now);
        let mut surface = surface.lock().unwrap_or_else(|e| e.into_inner());
        let lock_ms = trace.map(|t| t.elapsed().as_secs_f64() * 1e3);
        let recentred = surface.recentre(center);
        let recentre_ms = trace.map(|t| t.elapsed().as_secs_f64() * 1e3);
        let rects = surface.advance();
        let advance_ms = trace.map(|t| t.elapsed().as_secs_f64() * 1e3);
        let full = surface.take_full_upload();
        let finish = |full: bool, rect_count: usize| {
            if let (Some(t), Some(lock), Some(recentre), Some(advance)) =
                (trace, lock_ms, recentre_ms, advance_ms)
            {
                let total = t.elapsed().as_secs_f64() * 1e3;
                if total > 0.5 {
                    eprintln!(
                        "spall-water-trace: t={:.1} sync {total:.2} ms = lock {lock:.2} + recentre {:.2}{} + advance {:.2} + upload {:.2} ({}, {rect_count} rects)",
                        spall_render::trace_clock_ms(),
                        recentre - lock,
                        if recentred { " (moved)" } else { "" },
                        advance - recentre,
                        total - advance,
                        if full { "full field" } else { "rects" },
                    );
                }
            }
        };
        if full || !self.water_grid_on {
            // The grid surface replaces the old mesh sheet.
            self.scene
                .set_water_surface(&self.device, &self.queue, &[], &[]);
            if self.water_grid_on {
                self.scene
                    .replace_water_field(&self.device, &self.queue, surface.field().clone());
            } else {
                self.scene.set_water_field(
                    &self.device,
                    &self.queue,
                    Some(surface.field().clone()),
                );
            }
            self.scene.set_water_grid_surface(true);
            self.water_grid_on = true;
            finish(true, rects.len());
            return;
        }
        let rect_count = rects.len();
        self.water_rects_frame = rect_count;
        for rect in rects {
            let texels = surface.texels(rect);
            self.scene.update_water_field_rect(
                &self.queue,
                rect.x0,
                rect.z0,
                rect.w,
                rect.h,
                &texels,
            );
        }
        finish(false, rect_count);
    }

    pub(crate) fn set_crosshair(&mut self, view: Option<CrosshairView>) {
        self.crosshair = view;
    }

    /// Rebuilds the draw list: the water boxes clipped to the terrain window.
    fn clip_water(&mut self) {
        self.debug_water_instances.clear();
        if let Some(center) = self.water_window {
            clip_water_to_window(
                &self.debug_water_source,
                center,
                &mut self.debug_water_instances,
            );
        }
    }

    pub(crate) fn new(
        window: Arc<Window>,
        environment: Environment,
        materials: &[spall_render::Material],
        uncapped: bool,
        day_cycle_enabled: bool,
    ) -> Result<Self, ClientError> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(
            Box::new(window.clone()),
        ));
        let surface = instance
            .create_surface(window.clone())
            .map_err(|error| ClientError::Gpu(error.to_string()))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .map_err(|error| ClientError::Gpu(error.to_string()))?;
        let timestamp_features =
            wgpu::Features::TIMESTAMP_QUERY | wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS;
        let required_features = if adapter.features().contains(timestamp_features) {
            timestamp_features
        } else {
            wgpu::Features::empty()
        };
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("spall-interactive-device"),
            required_features,
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::Performance,
            ..Default::default()
        }))
        .map_err(|error| ClientError::Gpu(error.to_string()))?;
        let capabilities = surface.get_capabilities(&adapter);
        let present_mode = if uncapped {
            [wgpu::PresentMode::Immediate, wgpu::PresentMode::AutoNoVsync]
                .into_iter()
                .find(|mode| capabilities.present_modes.contains(mode))
                .unwrap_or(wgpu::PresentMode::Fifo)
        } else {
            wgpu::PresentMode::Fifo
        };
        eprintln!("spall-interactive: present mode {present_mode:?}");
        let format = capabilities
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
            .or_else(|| capabilities.formats.first().copied())
            .ok_or_else(|| ClientError::Gpu("surface exposes no texture format".into()))?;
        let size = window.inner_size();
        let surface_config = wgpu::SurfaceConfiguration {
            // COPY_SRC lets F12 / `--screenshot` read the finished frame back.
            usage: if capabilities.usages.contains(wgpu::TextureUsages::COPY_SRC) {
                wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC
            } else {
                wgpu::TextureUsages::RENDER_ATTACHMENT
            },
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            alpha_mode: capabilities.alpha_modes[0],
            color_space: wgpu::SurfaceColorSpace::Auto,
            view_formats: vec![],
            // ENG-69 round 12: `.local/runs/interactive-frames.jsonl` from a
            // live hands-on run showed a perfectly regular 4-frame cycle —
            // two near-free frames, one ~16ms frame, one ~50ms frame,
            // repeating — with every millisecond of it inside `acquire_ms`
            // (the wait in `get_current_texture`), never in `buffer_upload`/
            // `submit`/`present`. With `ControlFlow::Poll` never yielding
            // between iterations, a latency of `2` let the render loop burst
            // through two queued-ahead frames almost instantly and then stall
            // to let the display drain the backlog, instead of pacing evenly
            // at one vsync interval per frame — the felt "corners jitter"
            // when strafing past an object. `1` forces the CPU to wait for
            // the previous frame to actually present before acquiring the
            // next one, trading a little frame-queuing slack for even
            // pacing.
            desired_maximum_frame_latency: 1,
        };
        surface.configure(&device, &surface_config);
        let aspect = surface_config.width as f32 / surface_config.height.max(1) as f32;
        let yakui = yakui::Yakui::new();
        let yakui_winit = YakuiWinit::new(&window);
        let yakui_wgpu = YakuiWgpu::new(device.clone(), queue.clone());
        let yakui_buffers = yakui_wgpu.buffers();
        let hud_gpu_timer = required_features
            .contains(timestamp_features)
            .then(|| HudGpuTimer::new(&device, queue.get_timestamp_period()));

        let mut render_materials = materials.to_vec();
        // Water is presentation-only and drawn alpha-blended over the scene.
        render_materials.push(WATER_MATERIAL);
        let vegetation_material = render_materials.len() as u32;
        for colour in spall_ecology::living::PALETTE {
            render_materials.push(spall_render::Material::new(
                colour.map(|c| (f32::from(c) / 255.).powf(2.2)),
                0.9,
                0.0,
            ));
        }
        let scene = GameRenderer::new(
            &device,
            &queue,
            surface_config.format,
            &render_materials,
            (surface_config.width, surface_config.height),
            required_features
                .contains(timestamp_features)
                .then(|| queue.get_timestamp_period()),
        );

        Ok(Self {
            environment,
            day_cycle_enabled,
            weather_state: None,
            weather_presentation: crate::weather::WeatherPresentation::new(),
            weather_render_at: Instant::now(),
            weather_exposure: 1.0,
            weather_effects_enabled: true,
            ground_snow_enabled: true,
            reduced_weather_flashes: false,
            snow_frames: Vec::new(),
            snow_generation: None,
            snow_presentation: crate::snow::SnowPresentation::new(),
            snow_upload_key: None,
            snow_rebuilt_at: None,
            day_clock_label: "Waiting for server time".to_owned(),
            device,
            queue,
            surface,
            surface_config,
            scene,
            vegetation_frame: None,
            vegetation_terrain: None,
            vegetation_key: None,
            vegetation_built: Instant::now(),
            vegetation_worker: VegetationWorker::spawn(vegetation_material)?,
            vegetation_job: None,
            vegetation_seq: 0,
            crosshair: None,
            debug_water_source: Vec::new(),
            water_window: None,
            water_rects_frame: 0,
            debug_water_instances: Vec::new(),
            water_epoch: Instant::now(),
            water_grid_on: false,
            debug_view: DebugView::Shaded,
            aspect,
            yakui,
            yakui_winit,
            yakui_wgpu,
            yakui_buffers,
            hud_gpu_timer,
            screenshot_request: None,
        })
    }

    /// Queues a copy of the finished frame (HUD included) for a screenshot.
    /// `None` if this surface cannot be read back.
    fn queue_screenshot_copy(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        texture: &wgpu::Texture,
        path: PathBuf,
    ) -> Option<PendingScreenshot> {
        if !self
            .surface_config
            .usage
            .contains(wgpu::TextureUsages::COPY_SRC)
        {
            eprintln!("spall-interactive: this surface cannot be read back; screenshot skipped");
            return None;
        }
        let (width, height) = (self.surface_config.width, self.surface_config.height);
        let padded = (width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("spall-interactive-screenshot"),
            size: u64::from(padded) * u64::from(height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        Some(PendingScreenshot {
            buffer,
            path,
            width,
            height,
            padded,
            bgra: matches!(
                self.surface_config.format,
                wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
            ),
        })
    }

    /// Reads a submitted screenshot copy back and writes the PNG off-thread.
    fn finish_screenshot(&self, pending: PendingScreenshot) -> Result<(), ClientError> {
        let slice = pending.buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|error| ClientError::Render(error.to_string()))?;
        if !matches!(rx.recv(), Ok(Ok(()))) {
            eprintln!("spall-interactive: screenshot readback failed");
            return Ok(());
        }
        let mut rgba = Vec::with_capacity((pending.width * pending.height * 4) as usize);
        {
            let mapped = slice
                .get_mapped_range()
                .map_err(|error| ClientError::Render(error.to_string()))?;
            for row in mapped.chunks_exact(pending.padded as usize) {
                for px in row[..(pending.width * 4) as usize].chunks_exact(4) {
                    if pending.bgra {
                        rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
                    } else {
                        rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
                    }
                }
            }
        }
        pending.buffer.unmap();
        let PendingScreenshot {
            path,
            width,
            height,
            ..
        } = pending;
        std::thread::spawn(move || {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            match image::RgbaImage::from_raw(width, height, rgba).map(|img| img.save(&path)) {
                Some(Ok(())) => {
                    eprintln!("spall-interactive: screenshot saved to {}", path.display())
                }
                Some(Err(error)) => eprintln!(
                    "spall-interactive: screenshot {} failed: {error}",
                    path.display()
                ),
                None => eprintln!("spall-interactive: screenshot buffer had the wrong size"),
            }
        });
        Ok(())
    }

    /// One report-line fragment: the shared renderer's per-pass GPU time
    /// (previous completed frames) and resident instance memory.
    pub(super) fn scene_report(&self) -> String {
        let (_, _, overlay) = self.scene.instance_counts();
        let (terrain_chunks, terrain_triangles, body_meshes, body_triangles, mesh_bytes) =
            self.scene.mesh_stats();
        let memory = (self.scene.instance_bytes() + mesh_bytes) as f64 / (1024.0 * 1024.0);
        let sky_memory = self.scene.sky_bytes() as f64 / (1024.0 * 1024.0);
        let sweep = self.scene.lighting_sweep_frames().unwrap_or(0);
        let lit_after = self
            .scene
            .lighting_latency_ms()
            .map_or("n/a".to_owned(), |ms| format!("{ms:.0} ms"));
        match self.scene.pass_timings() {
            Some(t) => format!(
                "scene GPU {:.2} ms (shadow {:.2} / opaque {:.2} / tone {:.2}; sky visibility {} / bounce {} last recompute, {sky_memory:.0} MiB) | lighting sweep {sweep} frames (cache update to last slice recorded {lit_after}, not presented) | greedy {terrain_chunks} terrain chunks / {terrain_triangles} tris + {body_meshes} body meshes / {body_triangles} tris + {overlay} debug cubes ({memory:.1} MiB)",
                t.total_ms(),
                t.shadow_ms,
                t.opaque_ms,
                t.tone_map_ms,
                t.sky_visibility_ms
                    .map_or("n/a".to_owned(), |ms| format!("{ms:.2} ms")),
                t.bounce_ms
                    .map_or("n/a".to_owned(), |ms| format!("{ms:.2} ms"))
            ),
            None => format!(
                "scene GPU timing unavailable | greedy {terrain_chunks} terrain chunks / {terrain_triangles} tris + {body_meshes} body meshes / {body_triangles} tris + {overlay} debug cubes ({memory:.1} MiB)"
            ),
        }
    }

    pub(crate) fn handle_window_event(&mut self, event: &WindowEvent) -> bool {
        self.yakui_winit.handle_window_event(&mut self.yakui, event)
    }

    pub(crate) fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.surface_config.width = size.width;
        self.surface_config.height = size.height;
        self.surface.configure(&self.device, &self.surface_config);
        self.scene.resize(&self.device, size.width, size.height);
        self.aspect = size.width as f32 / size.height as f32;
    }

    /// `eye`/`look_dir` come from the window's own camera state; `scene` is
    /// `None` before the local player has an authoritative pose yet (still
    /// connecting), in which case this just clears the screen.
    /// Uploads any fresh terrain instances and acquires the next swapchain
    /// image — everything in a render frame whose cost doesn't depend on the
    /// camera. Split out from the old single `render` method (ENG-69 round
    /// 13): the caller now computes the camera *after* this returns, with a
    /// fresh timestamp, instead of before — see [`finish_frame`] and
    /// `InteractiveApp`'s `RedrawRequested` handler for why.
    pub(crate) fn begin_frame(
        &mut self,
        terrain: Option<&[Instance]>,
        overlay: &[Instance],
    ) -> Result<AcquireOutcome, ClientError> {
        let frame_start = Instant::now();
        let mut buffer_upload_ms = None;
        let mut instance_count = None;
        if let Some(instances) = terrain {
            let upload_start = Instant::now();
            self.scene.set_terrain(&self.device, &self.queue, instances);
            buffer_upload_ms = Some(upload_start.elapsed().as_secs_f32() * 1000.0);
            instance_count = Some(instances.len() as u32);
        }
        self.scene.set_overlay(&self.device, &self.queue, overlay);

        let acquire_start = Instant::now();
        let surface_texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => t,
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
                self.surface.configure(&self.device, &self.surface_config);
                drop(t);
                return Ok(AcquireOutcome::Skipped(FrameTiming::early_return(
                    frame_start.elapsed().as_secs_f32() * 1000.0,
                    buffer_upload_ms,
                    instance_count,
                    acquire_start.elapsed().as_secs_f32() * 1000.0,
                )));
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.surface_config);
                return Ok(AcquireOutcome::Skipped(FrameTiming::early_return(
                    frame_start.elapsed().as_secs_f32() * 1000.0,
                    buffer_upload_ms,
                    instance_count,
                    acquire_start.elapsed().as_secs_f32() * 1000.0,
                )));
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(AcquireOutcome::Skipped(FrameTiming::early_return(
                    frame_start.elapsed().as_secs_f32() * 1000.0,
                    buffer_upload_ms,
                    instance_count,
                    acquire_start.elapsed().as_secs_f32() * 1000.0,
                )));
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err(ClientError::Render(
                    "surface acquisition validation error".into(),
                ));
            }
        };
        let acquire_ms = acquire_start.elapsed().as_secs_f32() * 1000.0;
        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        Ok(AcquireOutcome::Ready(AcquiredFrame {
            frame_start,
            surface_texture,
            view,
            buffer_upload_ms,
            instance_count,
            acquire_ms,
        }))
    }

    /// Draws and presents an already-acquired frame. `cam` is `None` before
    /// the local player has an authoritative pose yet (still connecting), in
    /// which case this just clears the screen.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn finish_frame(
        &mut self,
        acquired: AcquiredFrame,
        cam: Option<&(Vec3, Vec3)>,
        body_meshes: &[(u64, Vec<spall_render::GpuVertex>, Vec<u32>)],
        live_body_meshes: &[u64],
        demo_hud: Option<(&str, &str, &str)>,
        admin_menu: Option<(&AdminMenuView, &mut AdminMenuActions)>,
        server_tick: Option<u64>,
    ) -> Result<FrameTiming, ClientError> {
        let AcquiredFrame {
            frame_start,
            surface_texture,
            view,
            buffer_upload_ms,
            instance_count,
            acquire_ms,
        } = acquired;

        if let Some(server_tick) = server_tick {
            let (environment, state) =
                environment_for_clock(server_tick, self.day_cycle_enabled, self.environment);
            self.environment = environment;
            if let Some(state) = state {
                self.day_clock_label = format!(
                    "Day {} · {:02}:{:02} · {} moon",
                    state.day_index + 1,
                    state.hour,
                    state.minute,
                    lunar_phase_name(state.lunar_phase),
                );
            }
        }

        self.scene.set_bodies(&self.device, &self.queue, &[]);
        self.scene
            .set_body_meshes(&self.device, &self.queue, body_meshes, live_body_meshes)
            .map_err(|error| ClientError::Render(error.to_string()))?;
        self.scene
            .set_water_time(self.water_epoch.elapsed().as_secs_f32());
        // Terrain, water and foliage stop at the view radius; fading them into
        // the horizon over its outer third leaves nothing to pop at the edge.
        // `SPALL_NO_FOG=1` turns it off for comparison.
        let fog_radius = view_radius_m();
        if std::env::var_os("SPALL_NO_FOG").is_some() {
            self.scene.set_fog_range(0.0, 0.0);
        } else {
            self.scene.set_fog_range(
                fog_radius * FOG_START_FRACTION,
                fog_radius * FOG_END_FRACTION,
            );
        }
        while let Ok((id, instances)) = self.vegetation_worker.rx.try_recv() {
            if self.vegetation_job == Some(id) {
                self.vegetation_job = None;
                let upload_started = Instant::now();
                self.scene
                    .set_vegetation(&self.device, &self.queue, &instances);
                if vegetation_trace() {
                    eprintln!(
                        "spall-vegetation-trace: upload of {} instances took {:.2} ms",
                        instances.len(),
                        upload_started.elapsed().as_secs_f32() * 1000.0
                    );
                }
            }
        }
        if let Some((eye, _)) = cam {
            if let Some(frame) = &self.vegetation_frame {
                let key = (
                    frame.time_ms,
                    (eye.x / 4.).floor() as i64,
                    (eye.z / 4.).floor() as i64,
                    self.scene.terrain_geometry_generation(),
                );
                if self.vegetation_job.is_none()
                    && self.vegetation_key != Some(key)
                    && self.vegetation_built.elapsed() >= FOLIAGE_REBUILD_INTERVAL
                    && std::env::var_os("SPALL_NO_FOLIAGE").is_none()
                {
                    self.vegetation_seq = self.vegetation_seq.wrapping_add(1);
                    let id = self.vegetation_seq;
                    if self
                        .vegetation_worker
                        .tx
                        .try_send((
                            id,
                            frame.clone(),
                            eye.to_array(),
                            self.scene.terrain_mesh_coords().collect(),
                            self.vegetation_terrain.as_ref().map(|(_, v)| v.clone()),
                        ))
                        .is_ok()
                    {
                        self.vegetation_job = Some(id);
                        self.vegetation_key = Some(key);
                        self.vegetation_built = Instant::now();
                    }
                }
            } else {
                self.vegetation_key = None;
                self.vegetation_job = None;
                self.scene.set_vegetation(&self.device, &self.queue, &[]);
            }
        }
        self.clip_water();
        if let Some((eye, _)) = cam {
            self.debug_water_instances.sort_by(|a, b| {
                let distance2 = |cube: &Instance| {
                    let p = Vec3::from_array(cube.offset);
                    (p - *eye).length_squared()
                };
                distance2(b).total_cmp(&distance2(a))
            });
        }
        self.scene
            .set_transparent_cubes(&self.device, &self.queue, &self.debug_water_instances);

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("spall-interactive-encoder"),
            });
        if let Some((eye, look_dir)) = cam {
            let mut camera = Camera::looking_along(
                *eye,
                *look_dir,
                CAMERA_FOV_Y_DEG.to_radians(),
                self.aspect.max(1e-4),
            );
            camera.z_near = CAMERA_Z_NEAR_M;
            camera.z_far = CAMERA_Z_FAR_M;
            self.weather_presentation
                .set_enabled(self.weather_effects_enabled);
            self.weather_presentation
                .set_reduced_flashes(self.reduced_weather_flashes);
            let weather_now = Instant::now();
            let weather_dt = weather_now
                .duration_since(self.weather_render_at)
                .as_secs_f32()
                .min(0.1);
            self.weather_render_at = weather_now;
            if let (Some(state), Some((_, volume))) =
                (&self.weather_state, &self.vegetation_terrain)
            {
                let frame = self
                    .weather_presentation
                    .update(state, &camera, weather_dt, volume);
                if self.screenshot_request.is_some() {
                    tracing::info!(kind = ?state.kind, particles = frame.particles.len(), exposure = frame.lighting.exposure, "weather capture presentation");
                }
                self.weather_exposure = frame.lighting.exposure;
                self.scene
                    .set_weather_particles(frame.particles, frame.lighting)
                    .map_err(|error| ClientError::Render(error.to_string()))?;
            } else {
                self.scene
                    .set_weather_particles(&[], spall_render::WeatherLighting::default())
                    .map_err(|error| ClientError::Render(error.to_string()))?;
            }
            let terrain_stamp = self
                .vegetation_terrain
                .as_ref()
                .map_or(0, |(_, v)| v.state_stamp());
            let snow_key = (
                self.snow_generation.unwrap_or(0),
                terrain_stamp,
                camera.position.to_array().map(|v| (v / 8.0).floor() as i64),
                self.ground_snow_enabled,
            );
            let snow_toggled = self.snow_upload_key.map(|k| k.3) != Some(snow_key.3);
            let snow_settled = self
                .snow_rebuilt_at
                .is_none_or(|at| at.elapsed() >= SNOW_REBUILD_INTERVAL);
            if self.snow_upload_key != Some(snow_key) && (snow_toggled || snow_settled) {
                self.snow_rebuilt_at = Some(Instant::now());
                if self.ground_snow_enabled
                    && let Some((_, terrain)) = &self.vegetation_terrain
                {
                    self.snow_presentation
                        .set_radius_m(view_radius_m().min(SNOW_MAX_RADIUS_M));
                    let snow_started = Instant::now();
                    let (patches, tracks) = self.snow_presentation.update(
                        &self.snow_frames,
                        terrain,
                        camera.position.to_array(),
                    );
                    let snow_ms = snow_started.elapsed().as_secs_f32() * 1000.0;
                    if snow_ms > 8.0 {
                        tracing::info!(
                            snow_ms,
                            patches = patches.len(),
                            "ground snow rebuild exceeded one 120 Hz frame"
                        );
                    }
                    self.scene
                        .set_snow_patches(&self.device, &self.queue, patches, tracks)
                        .map_err(|error| ClientError::Render(error.to_string()))?;
                } else {
                    self.scene
                        .set_snow_patches(&self.device, &self.queue, &[], &[])
                        .map_err(|error| ClientError::Render(error.to_string()))?;
                }
                self.snow_upload_key = Some(snow_key);
            }
            if self.screenshot_request.is_some() {
                let snow = self.snow_presentation.stats();
                let track_contacts: Vec<_> = self
                    .snow_frames
                    .iter()
                    .flat_map(|frame| &frame.footprints)
                    .take(16)
                    .map(|mark| (mark.contact_cell, mark.offset_mm, mark.strength_q8))
                    .collect();
                tracing::info!(
                    frames = snow.frames_considered,
                    faces = snow.rendered_faces,
                    patches = snow.patches,
                    tracks = snow.footprints,
                    max_depth_mm = snow.max_depth_mm,
                    capped = snow.cap_limited_surfaces,
                    stale = snow.revision_mismatches,
                    hidden = snow.hidden_or_unknown_faces,
                    enabled = self.ground_snow_enabled,
                    ?track_contacts,
                    "snow capture presentation"
                );
            }
            self.scene.render(
                &self.device,
                &self.queue,
                &mut encoder,
                &view,
                &camera,
                &self.environment,
                self.debug_view,
            );
        } else {
            // No pose yet (still connecting): show the environment's
            // background. Written straight to the sRGB surface, so the
            // displayed colour is the environment's exactly.
            let [r, g, b, a] = self.environment.background_linear();
            let _clear = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("spall-interactive-connecting-clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r, g, b, a }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
                multiview_mask: None,
            });
        }
        let hud_start = Instant::now();
        let (hud_gpu_slot, hud_gpu_ms) = self
            .hud_gpu_timer
            .as_mut()
            .map(HudGpuTimer::acquire)
            .unwrap_or((None, None));
        if hud_gpu_slot.is_some()
            && let Some(timer) = self.hud_gpu_timer.as_ref()
        {
            timer.write_start(&mut encoder);
        }
        self.yakui.start();
        let mut selected_environment = None;
        let mut admin_menu = admin_menu;
        let menu_height =
            (self.surface_config.height as f32 / self.yakui.scale_factor() - 24.0).max(100.0);
        {
            yakui::align(yakui::Alignment::TOP_LEFT, || {
                yakui::pad(yakui::widgets::Pad::all(12.0), || {
                    if let Some((heading, controls, status)) = demo_hud {
                        yakui::column(|| {
                            yakui::text(20.0, heading.to_owned());
                            yakui::text(14.0, controls.to_owned());
                            yakui::text(12.0, status.to_owned());
                        });
                    } else if let Some((view, actions)) = admin_menu.as_mut() {
                        yakui::constrained(yakui::Constraints::height(0.0, menu_height), || {
                            yakui::scroll_vertical(|| {
                                admin_menu_panel(
                                    view,
                                    actions,
                                    &mut selected_environment,
                                    &mut self.environment,
                                    &mut self.debug_view,
                                    &mut self.day_cycle_enabled,
                                );
                            });
                        });
                    } else {
                        yakui::colored_box_container(PANEL_BG.with_alpha(0.55), || {
                            yakui::pad(yakui::widgets::Pad::balanced(10.0, 6.0), || {
                                yakui::text(13.0, "F10  menu & controls");
                            });
                        });
                    }
                });
            });
        }
        if let Some(crosshair) = &self.crosshair {
            draw_crosshair(crosshair);
        }
        self.yakui.finish();
        if let Some(preset) = selected_environment {
            self.environment = preset.environment();
            self.day_cycle_enabled = false;
        }
        self.yakui_wgpu.paint_with_encoder(
            &mut self.yakui,
            &mut self.yakui_buffers,
            &mut encoder,
            YakuiSurfaceInfo {
                format: self.surface_config.format,
                sample_count: 1,
                color_attachment: &view,
                resolve_target: None,
            },
        );
        if let Some(slot) = hud_gpu_slot
            && let Some(timer) = self.hud_gpu_timer.as_ref()
        {
            timer.resolve(&mut encoder, slot);
        }
        let hud_cpu_ms = hud_start.elapsed().as_secs_f32() * 1000.0;
        let screenshot = self.screenshot_request.take().and_then(|path| {
            self.queue_screenshot_copy(&mut encoder, &surface_texture.texture, path)
        });
        let submit_start = Instant::now();
        self.queue.submit([encoder.finish()]);
        if let Some(pending) = screenshot {
            self.finish_screenshot(pending)?;
        }
        self.scene.finish_timing();
        if let Some(slot) = hud_gpu_slot
            && let Some(timer) = self.hud_gpu_timer.as_mut()
        {
            timer.map_after_submit(slot);
        }
        // A minimal isolated reproduction (ENG-69; the since-deleted `examples/poc_local`)
        // found that on this app's Vulkan backend, `desired_maximum_frame_latency: 1`
        // above did not actually stop the CPU from racing ~2 frames ahead of
        // the display — a measured, sustained 0.2ms/16ms/33ms three-frame
        // burst cycle even at complete idle, which reads as edge
        // ghosting/jitter under any camera motion regardless of what drives
        // it (confirmed independent of physics/input/networking). Blocking
        // here until the GPU has actually finished this frame's work caps
        // one submission in flight at a time and restored a rock-steady
        // ~16.6ms cadence in that reproduction.
        self.device
            .poll(wgpu::PollType::Poll)
            .map_err(|error| ClientError::Render(error.to_string()))?;
        let submit_ms = submit_start.elapsed().as_secs_f32() * 1000.0;
        let present_start = Instant::now();
        self.queue.present(surface_texture);
        let present_ms = present_start.elapsed().as_secs_f32() * 1000.0;
        Ok(FrameTiming {
            total_ms: frame_start.elapsed().as_secs_f32() * 1000.0,
            buffer_upload_ms,
            instance_count,
            acquire_ms,
            submit_ms,
            present_ms,
            hud_cpu_ms,
            hud_gpu_ms,
        })
    }
}

/// A frame copy waiting to be read back and written as a PNG.
pub(super) struct PendingScreenshot {
    buffer: wgpu::Buffer,
    path: PathBuf,
    width: u32,
    height: u32,
    padded: u32,
    bgra: bool,
}
