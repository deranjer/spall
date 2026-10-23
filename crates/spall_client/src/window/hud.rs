use super::*;

/// On-screen-debug support (per the ENG-69 lag investigation): there is no
/// text-rendering pipeline in this debug renderer, so "on screen" means the
/// window title bar — plus a mirrored line on stdout so it is visible in
/// whatever terminal launched the client (`cargo xtask play` included).
#[derive(Default)]
pub(super) struct Hud {
    last_frame_at: Option<Instant>,
    /// Exponential moving average, so a single slow frame doesn't make the
    /// readout unreadable jitter.
    frame_ms_ema: f32,
    frames_since_report: u32,
    last_report_at: Option<Instant>,
    last_rebuild_ms: f32,
    last_rebuild_instances: usize,
    last_sky_stats: crate::sky::SkyOccupancyStats,
    /// Worst single-frame total render time seen since the last report —
    /// see `record_frame`. Reset to `0.0` each time `report` runs.
    max_frame_ms: f32,
    /// Worst single-frame terrain-instance-buffer upload time seen since the
    /// last report (frames without a fresh upload don't count — see
    /// `FrameTiming::buffer_upload_ms`). Reset to `0.0` each time `report`
    /// runs.
    max_buffer_upload_ms: f32,
    max_hud_cpu_ms: f32,
    hud_cpu_sum_ms: f64,
    max_hud_gpu_ms: f32,
    hud_gpu_sum_ms: f64,
    hud_gpu_samples: u32,
    /// `InteractiveView::corrections` as of the last report — a *lifetime*
    /// counter, so the report shows how many are new since then rather than
    /// a running total that only ever grows and stops being useful for
    /// spotting "did one just happen".
    last_corrections_total: u64,
    /// Same idea for `InteractiveView::idle_corrections` — see
    /// `PredictedPlayer::idle_corrections`.
    last_idle_corrections_total: u64,
    /// Same idea for `InteractiveView::unmatched_reconciles` (ENG-69
    /// round 21) — reconciles with no comparison at all, tracked
    /// separately so a large silent resync can never hide behind a
    /// "+0 corrections" line.
    last_unmatched_total: u64,
    /// `InteractiveView::window_stats` as of the last report — same
    /// lifetime-counter-to-delta idea as `last_corrections_total`. ENG-69
    /// round 18: live proof the character-query-window cache is actually
    /// serving sweeps during a hands-on session, not just present in the
    /// code and never exercised.
    last_window_stats: crate::predict::WindowStats,
}

impl Hud {
    const REPORT_INTERVAL: Duration = Duration::from_millis(500);

    /// Call once per `RedrawRequested`, before doing any frame work — updates
    /// the frame-time average and returns `true` on the (throttled) tick
    /// where a report is due.
    pub(super) fn tick(&mut self, now: Instant) -> bool {
        if let Some(prev) = self.last_frame_at {
            let ms = (now - prev).as_secs_f32() * 1000.0;
            self.frame_ms_ema = if self.frame_ms_ema == 0.0 {
                ms
            } else {
                self.frame_ms_ema * 0.9 + ms * 0.1
            };
        }
        self.last_frame_at = Some(now);
        self.frames_since_report += 1;
        self.last_report_at
            .is_none_or(|t| now - t >= Self::REPORT_INTERVAL)
    }

    pub(super) fn record_sky(&mut self, stats: crate::sky::SkyOccupancyStats) {
        self.last_sky_stats = stats;
    }

    pub(super) fn sky_report(&self) -> String {
        let stats = self.last_sky_stats;
        format!(
            "sky occupancy {} resident / {} unknown bricks, {} solid cells",
            stats.resident_bricks, stats.unknown_bricks, stats.solid_cells
        )
    }

    pub(super) fn record_rebuild(&mut self, elapsed: Duration, instance_count: usize) {
        self.last_rebuild_ms = elapsed.as_secs_f32() * 1000.0;
        self.last_rebuild_instances = instance_count;
    }

    /// Folds one render frame's timing into the worst-case-since-last-report
    /// trackers (see `max_frame_ms`/`max_buffer_upload_ms`) — the averaged
    /// `frame_ms_ema` above hides exactly the short, occasional stall these
    /// exist to surface (ENG-69 round 12: a video review of a hands-on run
    /// showed a "hold, then jump" pattern the average alone didn't explain).
    pub(super) fn record_frame(&mut self, timing: &FrameTiming) {
        self.max_frame_ms = self.max_frame_ms.max(timing.total_ms);
        self.max_hud_cpu_ms = self.max_hud_cpu_ms.max(timing.hud_cpu_ms);
        self.hud_cpu_sum_ms += f64::from(timing.hud_cpu_ms);
        if let Some(ms) = timing.hud_gpu_ms {
            self.max_hud_gpu_ms = self.max_hud_gpu_ms.max(ms);
            self.hud_gpu_sum_ms += f64::from(ms);
            self.hud_gpu_samples += 1;
        }
        if let Some(ms) = timing.buffer_upload_ms {
            self.max_buffer_upload_ms = self.max_buffer_upload_ms.max(ms);
        }
    }

    /// The due report's text, and resets the report window. `None` fps until
    /// the first `REPORT_INTERVAL` has actually elapsed (avoids a bogus huge
    /// number from a near-zero-duration first window). The correction fields
    /// are `PredictedPlayer`'s own reconciliation-error counters (see
    /// `InteractiveView`) — a felt "snap back" that lines up with these
    /// climbing is a real prediction/authoritative disagreement, not a
    /// rendering artifact. The idle/vertical/horizontal breakdown (ENG-69
    /// round 7) separates a resting-contact disagreement (idle, vertical)
    /// from a collision-sweep one incurred while moving (horizontal) —
    /// `+N corrections (M idle)` with `M` tracking `N` closely, alongside a
    /// vertical max near the overall max, means it reproduces at a dead
    /// stop and is a ground-height/terrain-collider mismatch, not
    /// strafe-into-a-corner sweep divergence.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn report(
        &mut self,
        now: Instant,
        server_tick: u64,
        corrections_total: u64,
        max_correction_m: f64,
        idle_corrections_total: u64,
        max_idle_correction_m: f64,
        max_vertical_correction_m: f64,
        max_horizontal_correction_m: f64,
        unmatched_total: u64,
        max_unmatched_displacement_m: f64,
        prediction_steps: u64,
        prediction_elapsed_ms: u64,
        prediction_max_backlog_steps: u64,
        prediction_dropped_steps: u64,
        window_stats_total: crate::predict::WindowStats,
    ) -> String {
        let elapsed = self
            .last_report_at
            .map_or(Self::REPORT_INTERVAL, |t| now - t);
        let fps = self.frames_since_report as f32 / elapsed.as_secs_f32();
        let avg_hud_cpu_ms = self.hud_cpu_sum_ms / f64::from(self.frames_since_report.max(1));
        let hud_gpu_report = if self.hud_gpu_samples == 0 {
            "unavailable".to_owned()
        } else {
            format!(
                "{:.3} ms (avg) / {:.3} ms (max)",
                self.hud_gpu_sum_ms / f64::from(self.hud_gpu_samples),
                self.max_hud_gpu_ms
            )
        };
        self.last_report_at = Some(now);
        self.frames_since_report = 0;
        let new_corrections = corrections_total.saturating_sub(self.last_corrections_total);
        self.last_corrections_total = corrections_total;
        let new_idle = idle_corrections_total.saturating_sub(self.last_idle_corrections_total);
        self.last_idle_corrections_total = idle_corrections_total;
        let new_unmatched = unmatched_total.saturating_sub(self.last_unmatched_total);
        self.last_unmatched_total = unmatched_total;
        let max_frame_ms = self.max_frame_ms;
        let max_buffer_upload_ms = self.max_buffer_upload_ms;
        let max_hud_cpu_ms = self.max_hud_cpu_ms;
        self.max_frame_ms = 0.0;
        self.max_buffer_upload_ms = 0.0;
        self.max_hud_cpu_ms = 0.0;
        self.hud_cpu_sum_ms = 0.0;
        self.max_hud_gpu_ms = 0.0;
        self.hud_gpu_sum_ms = 0.0;
        self.hud_gpu_samples = 0;
        // ENG-69 round 18: `window_sweeps` vs `terrain_fallbacks` shows
        // whether the character-query-window cache is actually the thing
        // resolving movement this session, or silently falling back to the
        // whole-resident-set collider the whole time (e.g. because the
        // player has never strayed far enough from the world's streaming
        // edge for a window to build) — either is a real, different fact
        // about *this* session, not visible any other way.
        let new_window_sweeps = window_stats_total
            .window_sweeps
            .saturating_sub(self.last_window_stats.window_sweeps);
        let new_window_rebuilds = window_stats_total
            .window_rebuilds
            .saturating_sub(self.last_window_stats.window_rebuilds);
        let new_terrain_fallbacks = window_stats_total
            .terrain_fallbacks
            .saturating_sub(self.last_window_stats.terrain_fallbacks);
        self.last_window_stats = window_stats_total;
        let prediction_hz = if prediction_elapsed_ms == 0 {
            0.0
        } else {
            prediction_steps as f64 * 1_000.0 / prediction_elapsed_ms as f64
        };
        format!(
            "{fps:.0} fps | frame {:.1} ms (avg) / {max_frame_ms:.1} ms (max) | HUD CPU {avg_hud_cpu_ms:.3} ms (avg) / {max_hud_cpu_ms:.3} ms (max), GPU {hud_gpu_report} | buffer upload {max_buffer_upload_ms:.1} ms (max) | \
             rebuild {:.1} ms ({} changed greedy chunks) | server tick {server_tick} | \
             +{new_corrections} corrections ({new_idle} idle) (lifetime max {max_correction_m:.3} m idle {max_idle_correction_m:.3} m vert {max_vertical_correction_m:.3} m horiz {max_horizontal_correction_m:.3} m) | \
             +{new_unmatched} unmatched (lifetime max displacement {max_unmatched_displacement_m:.3} m) | \
             prediction {prediction_hz:.1} Hz, backlog max {prediction_max_backlog_steps}, dropped {prediction_dropped_steps} | \
             window cache: {new_window_sweeps} sweeps ({new_window_rebuilds} rebuilt), {new_terrain_fallbacks} terrain fallbacks",
            self.frame_ms_ema, self.last_rebuild_ms, self.last_rebuild_instances,
        )
    }
}

/// What the crosshair shows: whether the hammer has a target in reach, and a
/// one-line label (radius and what is being hit).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CrosshairView {
    pub in_reach: bool,
    pub label: String,
}

/// The endurance bar: how full it is, and whether the player is exhausted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct EnduranceView {
    pub fraction: f32,
    pub exhausted: bool,
}

/// The bar to draw for the local player: hidden at full endurance, so it only
/// shows while draining or recovering.
pub(super) fn endurance_view(endurance: f32, exhausted: bool) -> Option<EnduranceView> {
    (exhausted || endurance < 0.995).then(|| EnduranceView {
        fraction: endurance.clamp(0.0, 1.0),
        exhausted,
    })
}

/// A thin bar low on the screen, above the hammer label: pale while spending or
/// regaining endurance, amber-red while exhausted.
pub(super) fn draw_endurance(view: EnduranceView) {
    const WIDTH: f32 = 160.0;
    const HEIGHT: f32 = 5.0;
    let fill = if view.exhausted {
        yakui::Color::rgb(235, 110, 70)
    } else {
        yakui::Color::rgb(215, 225, 235)
    };
    yakui::align(yakui::Alignment::BOTTOM_CENTER, || {
        yakui::pad(yakui::widgets::Pad::all(78.0), || {
            // Two fixed-size boxes stacked at the same corner: track, then fill.
            yakui::stack(|| {
                yakui::colored_box(PANEL_BG.with_alpha(0.85), yakui::Vec2::new(WIDTH, HEIGHT));
                yakui::colored_box(
                    fill.with_alpha(0.85),
                    yakui::Vec2::new(WIDTH * view.fraction.max(0.01), HEIGHT),
                );
            });
        });
    });
}

/// Mean, percentiles and worst of the recent frames' handler time, and how many
/// ran long, as a few lines.
pub(super) fn frame_summary(history: &std::collections::VecDeque<f32>) -> Vec<String> {
    if history.is_empty() {
        return Vec::new();
    }
    let mut sorted: Vec<f32> = history.iter().copied().collect();
    sorted.sort_by(f32::total_cmp);
    let n = sorted.len();
    let at = |q: f32| sorted[((n as f32 - 1.0) * q).round() as usize];
    let mean = sorted.iter().sum::<f32>() / n as f32;
    let over = |ms: f32| sorted.iter().filter(|v| **v > ms).count();
    let pct = |count: usize| 100.0 * count as f32 / n as f32;
    vec![
        format!(
            "last {n} frames: mean {mean:.1} ms, p50 {:.1}, p90 {:.1}, p99 {:.1}, worst {:.1}",
            at(0.5),
            at(0.9),
            at(0.99),
            sorted[n - 1]
        ),
        format!(
            "over 8 ms {:.1}%, over 16 ms {:.2}%, over 33 ms {:.2}%",
            pct(over(8.0)),
            pct(over(16.0)),
            pct(over(33.0))
        ),
    ]
}

/// Puts `report` on the clipboard (Windows `clip`, macOS `pbcopy`, X11
/// `xclip`) and always saves it beside the system temp files too; says what
/// happened.
pub(super) fn copy_report(report: &str) -> String {
    use std::io::Write;
    // The clipboard tools read the console code page, not UTF-8: keep it ASCII.
    let ascii: String = report
        .chars()
        .map(|c| match c {
            _ if c.is_ascii() => c,
            '\u{b7}' => '|',
            '\u{b3}' => '3',
            _ => '?',
        })
        .collect();
    let report = ascii.as_str();
    let path = std::env::temp_dir().join("spall-debug-report.txt");
    let saved = std::fs::write(&path, report).is_ok();
    let tool: &[(&str, &[&str])] = &[
        ("clip", &[]),
        ("pbcopy", &[]),
        ("xclip", &["-selection", "clipboard"]),
    ];
    let mut copied = false;
    for (program, args) in tool {
        let child = std::process::Command::new(program)
            .args(*args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        let Ok(mut child) = child else { continue };
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(report.as_bytes());
        }
        drop(child.stdin.take());
        if child.wait().is_ok_and(|status| status.success()) {
            copied = true;
            break;
        }
    }
    match (copied, saved) {
        (true, true) => format!(
            "Copied {} lines to the clipboard (also saved to {})",
            report.lines().count(),
            path.display()
        ),
        (true, false) => format!("Copied {} lines to the clipboard", report.lines().count()),
        (false, true) => format!("No clipboard tool; saved the report to {}", path.display()),
        (false, false) => "Could not copy or save the report".to_owned(),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn admin_menu_view(
    session: &InteractiveSession,
    flying: bool,
    pending: bool,
    sky_visibility_on: bool,
    bounce_on: bool,
    lighting: &'static str,
    day_cycle_enabled: bool,
    world_time: String,
    report_status: Option<String>,
    frame_summary: Vec<String>,
) -> AdminMenuView {
    let admin_status = match (
        pending,
        session
            .admin_status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref(),
    ) {
        (_, Some(status)) if status.accepted => Some(format!("Server: {}", status.message)),
        (_, Some(status)) => Some(format!("Refused: {}", status.message)),
        (true, None) => Some("Waiting for the server...".to_owned()),
        (false, None) => None,
    };
    let water = {
        let regions = session
            .column_surface
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .region_count();
        (regions > 0).then(|| format!("Water: {regions} regions replicated"))
    };
    AdminMenuView {
        flying,
        admin_status,
        water,
        sky_visibility_on,
        bounce_on,
        lighting,
        day_cycle_enabled,
        world_time,
        report_status,
        frame_summary,
        weather: String::new(),
        nature_volume: 0.0,
        weather_volume: 0.0,
        footstep_volume: 0.0,
        nature_muted: false,
        nature_selection: String::new(),
        weather_muted: false,
        weather_effects_enabled: true,
        ground_snow_enabled: true,
        snow: String::new(),
        reduced_weather_flashes: false,
        body: BodyState::default(),
        body_preview: BodyPreview::default(),
    }
}

pub(super) fn lunar_phase_name(phase: f32) -> &'static str {
    [
        "new",
        "waxing crescent",
        "first quarter",
        "waxing gibbous",
        "full",
        "waning gibbous",
        "last quarter",
        "waning crescent",
    ][((phase.rem_euclid(1.0) * 8.0).round() as usize) % 8]
}

/// A plus at the centre of the screen, white with a target in reach and red
/// without, and the hammer label just under it.
pub(super) fn draw_crosshair(view: &CrosshairView) {
    let color = if view.in_reach {
        yakui::Color::rgb(255, 255, 255)
    } else {
        yakui::Color::rgb(255, 90, 80)
    };
    yakui::align(yakui::Alignment::CENTER, || {
        yakui::stack(|| {
            yakui::align(yakui::Alignment::CENTER, || {
                yakui::colored_box(color, yakui::Vec2::new(18.0, 2.0));
            });
            yakui::align(yakui::Alignment::CENTER, || {
                yakui::colored_box(color, yakui::Vec2::new(2.0, 18.0));
            });
        });
    });
    yakui::align(yakui::Alignment::BOTTOM_CENTER, || {
        yakui::pad(yakui::widgets::Pad::all(28.0), || {
            yakui::colored_box_container(PANEL_BG.with_alpha(0.55), || {
                yakui::pad(yakui::widgets::Pad::balanced(10.0, 4.0), || {
                    yakui::text(13.0, view.label.clone());
                });
            });
        });
    });
}

/// The bundled reminder icons (white strokes on a dark disc, tinted per cue
/// when drawn), uploaded once, in [`BodyCue::ALL`] order.
pub(super) fn load_body_icons(
    yakui: &mut yakui::Yakui,
) -> Result<[yakui::ManagedTextureId; 6], ClientError> {
    let mut ids = Vec::with_capacity(BodyCue::ALL.len());
    for cue in BodyCue::ALL {
        let png: &[u8] = match cue {
            BodyCue::Injury => include_bytes!("../../assets/hud/injury.png"),
            BodyCue::Thirst => include_bytes!("../../assets/hud/thirst.png"),
            BodyCue::Hunger => include_bytes!("../../assets/hud/hunger.png"),
            BodyCue::Exhaustion => include_bytes!("../../assets/hud/exhaustion.png"),
            BodyCue::Cold => include_bytes!("../../assets/hud/cold.png"),
            BodyCue::Wet => include_bytes!("../../assets/hud/wet.png"),
        };
        ids.push(png_texture(yakui, png, &format!("{cue:?} HUD icon"))?);
    }
    Ok(ids.try_into().expect("one icon per cue"))
}

/// Decodes `png` and uploads it as a linearly filtered UI texture.
pub(super) fn png_texture(
    yakui: &mut yakui::Yakui,
    png: &[u8],
    what: &str,
) -> Result<yakui::ManagedTextureId, ClientError> {
    let image = image::load_from_memory(png)
        .map_err(|error| ClientError::Render(format!("{what}: {error}")))?
        .into_rgba8();
    let size = yakui::UVec2::new(image.width(), image.height());
    let mut texture = yakui::paint::Texture::new(
        yakui::paint::TextureFormat::Rgba8Srgb,
        size,
        image.into_raw(),
    );
    texture.min_filter = yakui::paint::TextureFilter::Linear;
    Ok(yakui.add_texture(texture))
}

/// Ink colour of each reminder icon.
fn reminder_tint(cue: BodyCue) -> yakui::Color {
    match cue {
        BodyCue::Injury => yakui::Color::rgb(220, 70, 60),
        BodyCue::Thirst => yakui::Color::rgb(225, 180, 95),
        BodyCue::Hunger => yakui::Color::rgb(215, 150, 80),
        BodyCue::Exhaustion => yakui::Color::rgb(190, 190, 200),
        BodyCue::Cold => yakui::Color::rgb(170, 215, 255),
        BodyCue::Wet => yakui::Color::rgb(120, 170, 220),
    }
}

/// Reminder icons in the lower-right corner for any body state at 70% or more,
/// most urgent rightmost-first.
pub(super) fn draw_body_reminders(
    icons: &[yakui::ManagedTextureId; 6],
    reminders: &[(BodyCue, f32); 6],
) {
    if reminders.iter().all(|(_, opacity)| *opacity <= 0.0) {
        return;
    }
    yakui::align(yakui::Alignment::BOTTOM_RIGHT, || {
        yakui::pad(yakui::widgets::Pad::all(24.0), || {
            // A default row fills the screen width and packs its icons at the
            // left; shrink it to its icons so the corner alignment holds.
            let mut row = yakui::widgets::List::row();
            row.main_axis_size = yakui::MainAxisSize::Min;
            row.show(|| {
                for (icon, (cue, opacity)) in icons.iter().zip(reminders).rev() {
                    if *opacity <= 0.0 {
                        continue;
                    }
                    yakui::pad(yakui::widgets::Pad::horizontal(4.0), || {
                        let mut image = yakui::widgets::Image::new(*icon, yakui::Vec2::splat(52.0));
                        image.color = reminder_tint(*cue).with_alpha(*opacity);
                        image.show();
                    });
                }
            });
        });
    });
}

/// Ink on the tarp: names and counts, written like charcoal on canvas.
const PACK_INK: yakui::Color = yakui::Color::rgb(48, 34, 22);

/// The pack: the world dimmed a little, the tarp, and each stack's icon,
/// count and name. A hovered stack's icon grows and its name darkens.
pub(super) fn draw_pack(
    frame: &PackFrame,
    tarp: yakui::ManagedTextureId,
    icons: &HashMap<u16, Option<yakui::ManagedTextureId>>,
    screen: [f32; 2],
) {
    let fade = |color: yakui::Color, alpha: f32| color.with_alpha(alpha * frame.shown);
    let at = |x: f32, y: f32, children: &mut dyn FnMut()| {
        yakui::align(yakui::Alignment::TOP_LEFT, || {
            yakui::offset(yakui::Vec2::new(x, y), children);
        });
    };
    yakui::colored_box(
        fade(yakui::Color::BLACK, 0.35),
        yakui::Vec2::from_array(screen),
    );
    let t = frame.tarp;
    at(t.x, t.y, &mut || {
        let mut image = yakui::widgets::Image::new(tarp, yakui::Vec2::new(t.w, t.h));
        image.color = fade(yakui::Color::WHITE, 1.0);
        image.show();
    });
    // Text scales with the tarp so it stays readable at any window size.
    let name_size = (t.h * 0.034).clamp(16.0, 30.0);
    let count_size = name_size * 1.15;
    let text = |size: f32, color: yakui::Color, line: String| {
        let mut text = yakui::widgets::Text::new(size, line);
        text.style.color = color;
        text.show();
    };
    for slot in &frame.slots {
        let grow = if slot.hovered { 1.12 } else { 1.0 };
        let icon = slot.icon;
        let side = icon.w * grow;
        let (x, y) = (
            icon.x - (side - icon.w) * 0.5,
            icon.y - (side - icon.h) * 0.5,
        );
        at(
            x,
            y,
            &mut || match icons.get(&slot.item_id).copied().flatten() {
                Some(texture) => {
                    let mut image = yakui::widgets::Image::new(texture, yakui::Vec2::splat(side));
                    image.color = fade(yakui::Color::WHITE, 1.0);
                    image.show();
                }
                None => {
                    yakui::colored_box(fade(PACK_INK, 0.35), yakui::Vec2::splat(side * 0.6));
                }
            },
        );
        if slot.count > 1 {
            let label = format!("\u{d7}{}", slot.count);
            at(icon.x + icon.w * 0.72, icon.y + icon.h * 0.72, &mut || {
                text(count_size, fade(PACK_INK, 1.0), label.clone());
            });
        }
        // Centre the name under the icon; text width is estimated.
        let width = slot.name.chars().count() as f32 * name_size * 0.52;
        let cell = slot.cell;
        let ink = if slot.hovered {
            PACK_INK
        } else {
            PACK_INK.with_alpha(0.8)
        };
        at(
            cell.x + (cell.w - width) * 0.5,
            icon.y + icon.h + cell.h * 0.04,
            &mut || {
                text(
                    name_size,
                    fade(ink, ink.a as f32 / 255.0),
                    slot.name.clone(),
                )
            },
        );
    }
    if let Some(note) = &frame.note {
        let width = note.chars().count() as f32 * name_size * 0.52;
        at(t.x + (t.w - width) * 0.5, t.y + t.h * 0.47, &mut || {
            text(name_size * 1.2, fade(PACK_INK, 0.8), note.clone());
        });
    }
    let hint = "Tab to pack up";
    let width = hint.chars().count() as f32 * name_size * 0.4;
    at(t.x + (t.w - width) * 0.5, t.y + t.h * 0.9, &mut || {
        text(name_size * 0.75, fade(PACK_INK, 0.55), hint.to_owned());
    });
}

/// The blueprint book's art.
pub(super) struct BookTextures {
    pub(super) book: yakui::ManagedTextureId,
    pub(super) cloth: yakui::ManagedTextureId,
}

/// Places `children` with its top left at `(x, y)` logical pixels.
fn place(x: f32, y: f32, children: impl FnOnce()) {
    yakui::align(yakui::Alignment::TOP_LEFT, || {
        yakui::offset(yakui::Vec2::new(x, y), children);
    });
}

fn ink_text(size: f32, color: yakui::Color, line: impl Into<String>) {
    let mut text = yakui::widgets::Text::new(size, line.into());
    text.style.color = color;
    text.show();
}

/// A filled rectangle.
fn fill(rect: UiRect, color: yakui::Color) {
    place(rect.x, rect.y, || {
        yakui::colored_box(color, yakui::Vec2::new(rect.w, rect.h));
    });
}

/// A rectangle's outline, `width` logical pixels thick.
fn outline(rect: UiRect, color: yakui::Color, width: f32) {
    let UiRect { x, y, w, h } = rect;
    for side in [
        UiRect { x, y, w, h: width },
        UiRect {
            x,
            y: y + h - width,
            w,
            h: width,
        },
        UiRect { x, y, w: width, h },
        UiRect {
            x: x + w - width,
            y,
            w: width,
            h,
        },
    ] {
        fill(side, color);
    }
}

/// `rect` shrunk by `share` of its width on every side.
fn padded(rect: UiRect, share: f32) -> UiRect {
    let pad = rect.w * share;
    UiRect {
        x: rect.x + pad,
        y: rect.y + pad,
        w: rect.w - 2.0 * pad,
        h: rect.h - 2.0 * pad,
    }
}

/// An item's icon filling `rect`, or a plain marker when it has none.
fn item_icon(
    rect: UiRect,
    item: &ItemDraw,
    icons: &HashMap<u16, Option<yakui::ManagedTextureId>>,
    color: yakui::Color,
) {
    match icons.get(&item.item_id).copied().flatten() {
        Some(texture) => place(rect.x, rect.y, || {
            let mut image = yakui::widgets::Image::new(texture, yakui::Vec2::new(rect.w, rect.h));
            image.color = color;
            image.show();
        }),
        None => fill(
            padded(rect, 0.25),
            PACK_INK.with_alpha(0.35 * color.a as f32 / 255.0),
        ),
    }
}

/// A blueprint card: blue paper with white linework, its name, what it
/// makes, its pattern, and each material as have / need (red when short).
fn draw_blueprint_card(
    card: &BlueprintCard,
    icons: &HashMap<u16, Option<yakui::ManagedTextureId>>,
    alpha: f32,
) {
    let paper = if card.hovered {
        yakui::Color::rgb(58, 98, 148)
    } else {
        yakui::Color::rgb(44, 78, 122)
    };
    let line = yakui::Color::rgb(226, 236, 248);
    let r = card.rect;
    fill(r, paper.with_alpha(0.94 * alpha));
    outline(r, line.with_alpha(0.55 * alpha), 2.0);
    let title = (r.h * 0.12).clamp(15.0, 30.0);
    place(r.x + r.w * 0.06, r.y + r.h * 0.06, || {
        ink_text(title, line.with_alpha(alpha), card.name.clone());
    });
    // What it makes, top right, with how many.
    let side = r.h * 0.2;
    let out = UiRect {
        x: r.x + r.w * 0.94 - side,
        y: r.y + r.h * 0.04,
        w: side,
        h: side,
    };
    item_icon(
        out,
        &card.output,
        icons,
        yakui::Color::WHITE.with_alpha(alpha),
    );
    if card.output.count > 1 {
        place(out.x - title * 1.6, out.y + side * 0.2, || {
            ink_text(
                title * 0.9,
                line.with_alpha(alpha),
                format!("\u{d7}{}", card.output.count),
            );
        });
    }
    // The layout: a faint panel, the ground for a side view, then each
    // slot's item where it goes.
    let d = card.diagram;
    fill(d, line.with_alpha(0.07 * alpha));
    outline(d, line.with_alpha(0.45 * alpha), 1.5);
    if card.view == BlueprintView::Side {
        let ground = d.y + d.h * SIDE_GROUND;
        fill(
            UiRect {
                x: d.x,
                y: ground,
                w: d.w,
                h: 2.0,
            },
            line.with_alpha(0.7 * alpha),
        );
    }
    for (rect, item) in &card.pattern {
        item_icon(*rect, item, icons, yakui::Color::WHITE.with_alpha(alpha));
    }
    let size = (r.h * 0.09).clamp(13.0, 24.0);
    for (i, need) in card.needs.iter().enumerate() {
        let color = if need.have >= need.need {
            line
        } else {
            yakui::Color::rgb(255, 150, 135)
        };
        place(
            r.x + r.w * 0.54,
            r.y + r.h * 0.34 + size * 1.5 * i as f32,
            || {
                ink_text(
                    size,
                    color.with_alpha(alpha),
                    format!("{} {}/{}", need.name, need.have.min(9999), need.need),
                );
            },
        );
    }
}

/// The blueprint book: the contents (every blueprint as a card), or one
/// blueprint beside the crafting cloth, the materials to drag onto it and
/// the Craft button.
pub(super) fn draw_book(
    frame: &BookFrame,
    textures: &BookTextures,
    icons: &HashMap<u16, Option<yakui::ManagedTextureId>>,
    screen: [f32; 2],
) {
    let a = frame.shown;
    let white = yakui::Color::WHITE.with_alpha(a);
    yakui::colored_box(
        yakui::Color::BLACK.with_alpha(0.4 * a),
        yakui::Vec2::from_array(screen),
    );
    let b = frame.book;
    place(b.x, b.y, || {
        let mut image = yakui::widgets::Image::new(textures.book, yakui::Vec2::new(b.w, b.h));
        image.color = white;
        image.show();
    });
    let size = (b.h * 0.028).clamp(15.0, 28.0);
    match &frame.page {
        BookPage::Contents { cards, note } => {
            for card in cards {
                draw_blueprint_card(card, icons, a);
            }
            if let Some(note) = note {
                let width = note.chars().count() as f32 * size * 0.52;
                place(b.x + (b.w - width) * 0.5, b.y + b.h * 0.47, || {
                    ink_text(size * 1.2, PACK_INK.with_alpha(0.8 * a), note.clone());
                });
            }
            let hint = "Click a blueprint to craft it  \u{b7}  B to close";
            let width = hint.chars().count() as f32 * size * 0.4;
            place(b.x + (b.w - width) * 0.5, b.y + b.h * 0.945, || {
                ink_text(
                    size * 0.75,
                    yakui::Color::rgb(236, 224, 196).with_alpha(0.8 * a),
                    hint,
                );
            });
        }
        BookPage::Cloth(page) => {
            let ink = |alpha: f32| PACK_INK.with_alpha(alpha * a);
            place(page.back.x, page.back.y, || {
                let alpha = if page.back_hovered { 1.0 } else { 0.7 };
                ink_text(size, ink(alpha), "\u{2190} Blueprints");
            });
            draw_blueprint_card(&page.blueprint, icons, a);
            let c = page.cloth;
            place(c.x, c.y, || {
                let mut image =
                    yakui::widgets::Image::new(textures.cloth, yakui::Vec2::new(c.w, c.h));
                image.color = white;
                image.show();
            });
            // Side views get a ground line; both say which way they look.
            if page.view == BlueprintView::Side {
                let ground = c.y + c.h * SIDE_GROUND;
                fill(
                    UiRect {
                        x: c.x + c.w * 0.05,
                        y: ground,
                        w: c.w * 0.9,
                        h: 3.0,
                    },
                    ink(0.55),
                );
            }
            place(c.x + c.w * 0.07, c.y + c.h * 0.06, || {
                let label = match page.view {
                    BlueprintView::Overhead => "From above",
                    BlueprintView::Side => "From the side",
                };
                ink_text(size * 0.7, ink(0.5), label);
            });
            // Empty slots are faint outlines of their item; the slot a drag
            // would snap into is lit; filled slots show the item itself.
            for slot in page.slots.iter().filter(|slot| !slot.filled) {
                if slot.snap {
                    fill(padded(slot.rect, 0.04), ink(0.16));
                }
                item_icon(slot.rect, &slot.item, icons, white.with_alpha(0.22 * a));
            }
            for slot in page.slots.iter().filter(|slot| slot.filled) {
                if slot.hovered {
                    fill(padded(slot.rect, 0.04), ink(0.1));
                }
                item_icon(slot.rect, &slot.item, icons, white);
            }
            for slot in &page.materials {
                let r = slot.rect;
                let some = slot.item.count > 0;
                if slot.hovered && some {
                    fill(r, ink(0.1));
                }
                let pad = r.w * 0.12;
                let inner = UiRect {
                    x: r.x + pad,
                    y: r.y + pad * 0.5,
                    w: r.w - 2.0 * pad,
                    h: r.w - 2.0 * pad,
                };
                let shade = if some { a } else { 0.35 * a };
                item_icon(
                    inner,
                    &slot.item,
                    icons,
                    yakui::Color::WHITE.with_alpha(shade),
                );
                place(inner.x + inner.w * 0.6, inner.y + inner.h * 0.62, || {
                    let alpha = if some { 1.0 } else { 0.4 };
                    ink_text(
                        size * 0.95,
                        ink(alpha),
                        format!("\u{d7}{}", slot.item.count),
                    );
                });
                let width = slot.name.chars().count() as f32 * size * 0.38;
                place(r.x + (r.w - width) * 0.5, inner.y + inner.h + 2.0, || {
                    ink_text(size * 0.72, ink(0.8), slot.name.clone());
                });
            }
            let button = page.craft;
            let grey = yakui::Color::rgb(120, 110, 95);
            let (colour, label) = if page.crafting {
                (grey.with_alpha(0.7 * a), "Crafting\u{2026}")
            } else if page.ready {
                let green = if page.craft_hovered {
                    yakui::Color::rgb(78, 132, 70)
                } else {
                    yakui::Color::rgb(62, 110, 58)
                };
                (green.with_alpha(a), "Craft")
            } else {
                (grey.with_alpha(0.45 * a), "Craft")
            };
            fill(button, colour);
            outline(button, ink(0.6), 2.0);
            let width = label.chars().count() as f32 * size * 0.55;
            place(
                button.x + (button.w - width) * 0.5,
                button.y + (button.h - size * 1.2) * 0.5,
                || {
                    ink_text(
                        size * 1.05,
                        yakui::Color::rgb(245, 238, 220).with_alpha(a),
                        label,
                    )
                },
            );
            let hint = "Lay the materials out in the pattern";
            let note = page
                .note
                .clone()
                .or_else(|| (!page.ready).then(|| hint.to_owned()));
            if let Some(note) = note {
                let small = size * 0.8;
                let width = note.chars().count() as f32 * small * 0.5;
                place(c.x + (c.w - width) * 0.5, button.y - small * 1.8, || {
                    ink_text(small, ink(0.85), note);
                });
            }
            if let Some((rect, item)) = &page.dragging {
                item_icon(*rect, item, icons, white);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_endurance_bar_hides_when_full_and_flags_exhaustion() {
        assert_eq!(endurance_view(1.0, false), None);
        assert_eq!(endurance_view(0.7, false).map(|v| v.exhausted), Some(false));
        let drained = endurance_view(0.0, true).unwrap();
        assert!(drained.exhausted && drained.fraction == 0.0);
        // Recovered past the exhaustion line but not yet full: shown, normal colour.
        assert_eq!(endurance_view(0.4, false).map(|v| v.exhausted), Some(false));
    }

    #[test]
    fn the_frame_summary_reports_percentiles_and_long_frames() {
        assert!(frame_summary(&std::collections::VecDeque::new()).is_empty());
        let mut history = std::collections::VecDeque::new();
        for _ in 0..98 {
            history.push_back(4.0);
        }
        history.push_back(12.0);
        history.push_back(40.0);
        let lines = frame_summary(&history);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("last 100 frames"), "{}", lines[0]);
        assert!(lines[0].contains("p50 4.0"), "{}", lines[0]);
        assert!(lines[0].contains("worst 40.0"), "{}", lines[0]);
        assert!(lines[1].contains("over 8 ms 2.0%"), "{}", lines[1]);
        assert!(lines[1].contains("over 33 ms 1.00%"), "{}", lines[1]);
    }
}
