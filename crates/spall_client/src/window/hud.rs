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
