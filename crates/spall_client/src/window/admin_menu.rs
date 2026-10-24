use super::*;

/// Background of HUD panels.
pub(super) const PANEL_BG: yakui::Color = yakui::Color::rgb(16, 20, 28);
pub(super) const PANEL_ACCENT: yakui::Color = yakui::Color::rgb(120, 190, 255);
pub(super) const PANEL_MUTED: yakui::Color = yakui::Color::rgb(160, 170, 185);

/// Every binding the interactive window handles, as shown in the admin menu.
pub(super) const KEYBINDS: &[(&str, &str)] = &[
    ("W A S D", "Walk / fly"),
    ("Mouse", "Look (left click captures the cursor)"),
    (
        "Left click",
        "Hammer: break voxels where you aim (hold to repeat)",
    ),
    ("T", "Torch: place a light where you aim"),
    (
        "Tab",
        "Pack: lay out what you carry (Tab, Esc or walk to pack up)",
    ),
    (
        "B",
        "Blueprint book: pick a blueprint, lay its pattern on the cloth, Craft",
    ),
    ("E", "Open / close a nearby door"),
    ("Wheel", "Hammer radius 0-8 cells (0 = one voxel)"),
    ("Space", "Jump (walking) / rise (flying)"),
    ("Ctrl", "Descend (flying)"),
    ("Shift", "Sprint (walking) / fly faster (flying)"),
    ("F", "Toggle flight"),
    ("R", "Recentre the view on your player"),
    ("F10", "Open / close this menu"),
    ("Esc", "Release the cursor"),
    ("F1", "Toggle terrain"),
    ("F2", "Toggle detached bodies"),
    ("F3", "Toggle capsule markers"),
    ("F4", "Cycle render debug views"),
    ("F5", "Toggle visibility-aware skylight"),
    ("F6", "Toggle diffuse bounce"),
    ("F7", "Cycle lighting quality"),
    ("F8", "Toggle third-person view"),
    ("F9", "Copy a debug report to the clipboard"),
];

/// What the admin menu shows this frame.
#[derive(Debug, Clone, Default)]
pub(crate) struct AdminMenuView {
    pub weather: String,
    pub nature_volume: f32,
    pub weather_volume: f32,
    pub footstep_volume: f32,
    pub nature_muted: bool,
    pub nature_selection: String,
    pub weather_muted: bool,
    pub weather_effects_enabled: bool,
    pub ground_snow_enabled: bool,
    pub snow: String,
    pub reduced_weather_flashes: bool,
    /// Felt cold and wetness, 0..=1, as the screen shows them.
    pub body: BodyState,
    pub body_preview: BodyPreview,
    pub flying: bool,
    /// Server's reply to the last admin command, or "waiting" while pending.
    pub admin_status: Option<String>,
    /// One-line summary of the replicated water, if any has arrived.
    pub water: Option<String>,
    /// Visibility-aware skylight (`F5`) is on.
    pub sky_visibility_on: bool,
    /// Diffuse bounce (`F6`) is on.
    pub bounce_on: bool,
    /// Lighting quality (`F7`), as a word.
    pub lighting: &'static str,
    pub world_time: String,
    pub day_cycle_enabled: bool,
    /// What the last debug-report copy did.
    pub report_status: Option<String>,
    /// A few lines of recent frame statistics, shown beside the copy button.
    pub frame_summary: Vec<String>,
}

/// Buttons clicked in the admin menu this frame.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct AdminMenuActions {
    pub set_weather: Option<(
        Option<spall_protocol::WeatherKind>,
        Option<spall_protocol::SnowTier>,
    )>,
    /// Season to switch to, 0 spring .. 3 winter.
    pub set_season: Option<u8>,
    pub toggle_nature_audio: bool,
    pub nature_follow: bool,
    pub nature_next: bool,
    pub toggle_weather_audio: bool,
    pub nature_volume_delta: f32,
    pub weather_volume_delta: f32,
    pub footstep_volume_delta: f32,
    pub toggle_weather_effects: bool,
    pub toggle_ground_snow: bool,
    pub toggle_weather_flashes: bool,
    pub toggle_flight: bool,
    pub reset_world: bool,
    pub recentre: bool,
    pub close: bool,
    /// `Some(rate)` when a spring rate button (off/normal/fast/max) was
    /// clicked this frame.
    pub set_water_spring_rate: Option<u8>,
    /// `Some(open)` when a dam gate open/close button was clicked this frame.
    pub set_dam_gate: Option<bool>,
    pub toggle_sky_visibility: bool,
    pub toggle_bounce: bool,
    pub cycle_lighting: bool,
    pub copy_debug_report: bool,
    /// Added to the matching [`BodyPreview`] look this frame.
    pub body_preview_delta: BodyPreview,
    pub clear_body_preview: bool,
}

pub(super) fn section(title: &'static str) {
    yakui::pad(yakui::widgets::Pad::balanced(0.0, 6.0), || {
        let mut text = yakui::widgets::Text::new(15.0, title);
        text.style.color = PANEL_ACCENT;
        text.show();
    });
}

pub(super) fn weather_label(kind: spall_protocol::WeatherKind) -> &'static str {
    use spall_protocol::WeatherKind::*;
    match kind {
        Clear => "Clear",
        Rain => "Rain",
        Storm => "Storm",
        Snow => "Snow",
        HighWind => "High wind",
        Blizzard => "Blizzard",
    }
}

/// The weather kind, with the tier for snow.
pub(super) fn weather_state_label(state: &spall_protocol::WeatherState) -> String {
    match state.kind {
        spall_protocol::WeatherKind::Snow => format!(
            "Snow ({})",
            spall_protocol::SnowTier::from_precipitation(state.precipitation_q16).label()
        ),
        kind => weather_label(kind).to_string(),
    }
}

pub(super) fn muted(size: f32, line: impl Into<std::borrow::Cow<'static, str>>) {
    let mut text = yakui::widgets::Text::new(size, line.into());
    text.style.color = PANEL_MUTED;
    text.show();
}

pub(super) fn admin_menu_panel(
    view: &AdminMenuView,
    actions: &mut AdminMenuActions,
    selected_environment: &mut Option<EnvironmentPreset>,
    environment: &mut Environment,
    debug_view: &mut DebugView,
    day_cycle_enabled: &mut bool,
) {
    let environment_before_controls = *environment;
    yakui::colored_box_container(PANEL_BG.with_alpha(0.88), || {
        yakui::pad(yakui::widgets::Pad::all(14.0), || {
            yakui::row(|| {
                yakui::column(|| {
                    yakui::row(|| {
                        yakui::text(22.0, "SPALL ADMIN");
                        yakui::pad(yakui::widgets::Pad::balanced(12.0, 6.0), || {
                            muted(13.0, "F10 to close | scroll for more");
                        });
                    });

                    section("Weather & nature sounds");
                    muted(13.0, view.weather.clone());
                    yakui::row(|| {
                        if yakui::button("Automatic").clicked {
                            actions.set_weather = Some((None, None));
                        }
                        for kind in [
                            spall_protocol::WeatherKind::Clear,
                            spall_protocol::WeatherKind::Rain,
                            spall_protocol::WeatherKind::Storm,
                        ] {
                            yakui::pad(yakui::widgets::Pad::horizontal(3.0), || {
                                if yakui::button(weather_label(kind)).clicked {
                                    actions.set_weather = Some((Some(kind), None));
                                }
                            });
                        }
                    });
                    yakui::row(|| {
                        for tier in spall_protocol::SnowTier::ALL {
                            yakui::pad(yakui::widgets::Pad::horizontal(3.0), || {
                                let label = format!("Snow ({})", tier.label());
                                if yakui::button(label).clicked {
                                    actions.set_weather =
                                        Some((Some(spall_protocol::WeatherKind::Snow), Some(tier)));
                                }
                            });
                        }
                    });
                    yakui::row(|| {
                        for kind in [
                            spall_protocol::WeatherKind::HighWind,
                            spall_protocol::WeatherKind::Blizzard,
                        ] {
                            yakui::pad(yakui::widgets::Pad::horizontal(3.0), || {
                                if yakui::button(weather_label(kind)).clicked {
                                    actions.set_weather = Some((Some(kind), None));
                                }
                            });
                        }
                    });
                    yakui::row(|| {
                        for (code, name) in ["Spring", "Summer", "Autumn", "Winter"]
                            .into_iter()
                            .enumerate()
                        {
                            yakui::pad(yakui::widgets::Pad::horizontal(3.0), || {
                                if yakui::button(name).clicked {
                                    actions.set_season = Some(code as u8);
                                }
                            });
                        }
                    });
                    yakui::row(|| {
                        if yakui::button(if view.nature_muted {
                            "Nature: muted"
                        } else {
                            "Nature: on"
                        })
                        .clicked
                        {
                            actions.toggle_nature_audio = true;
                        }
                        muted(13.0, format!(" {:.0}% ", view.nature_volume * 100.0));
                        if yakui::button("- Nature").clicked {
                            actions.nature_volume_delta = -0.1;
                        }
                        if yakui::button("+ Nature").clicked {
                            actions.nature_volume_delta = 0.1;
                        }
                    });
                    yakui::row(|| {
                        muted(12.0, view.nature_selection.clone());
                        if yakui::button("Next biome sound").clicked {
                            actions.nature_next = true;
                        }
                        if yakui::button("Follow biome").clicked {
                            actions.nature_follow = true;
                        }
                    });
                    yakui::row(|| {
                        if yakui::button(if view.weather_muted {
                            "Weather audio: muted"
                        } else {
                            "Weather audio: on"
                        })
                        .clicked
                        {
                            actions.toggle_weather_audio = true;
                        }
                        muted(13.0, format!(" {:.0}% ", view.weather_volume * 100.0));
                        if yakui::button("- Weather").clicked {
                            actions.weather_volume_delta = -0.1;
                        }
                        if yakui::button("+ Weather").clicked {
                            actions.weather_volume_delta = 0.1;
                        }
                    });
                    yakui::row(|| {
                        muted(
                            13.0,
                            format!("Footsteps {:.0}% ", view.footstep_volume * 100.0),
                        );
                        if yakui::button("- Steps").clicked {
                            actions.footstep_volume_delta = -0.1;
                        }
                        if yakui::button("+ Steps").clicked {
                            actions.footstep_volume_delta = 0.1;
                        }
                    });
                    yakui::row(|| {
                        if yakui::button(if view.weather_effects_enabled {
                            "Effects: on"
                        } else {
                            "Effects: off"
                        })
                        .clicked
                        {
                            actions.toggle_weather_effects = true;
                        }
                        if yakui::button(if view.reduced_weather_flashes {
                            "Lightning: reduced"
                        } else {
                            "Lightning: normal"
                        })
                        .clicked
                        {
                            actions.toggle_weather_flashes = true;
                        }
                    });
                    if yakui::button(if view.ground_snow_enabled {
                        "Ground snow and tracks: on"
                    } else {
                        "Ground snow and tracks: off"
                    })
                    .clicked
                    {
                        actions.toggle_ground_snow = true;
                    }
                    muted(12.0, view.snow.clone());
                    muted(
                        12.0,
                        "Snow builds below freezing. Walk to leave tracks; fly to inspect them.",
                    );
                    section("Body");
                    muted(
                        12.0,
                        format!(
                            "Felt cold {:.0}% | wet {:.0}% (from weather, shelter and water)",
                            view.body.cold * 100.0,
                            view.body.wet * 100.0
                        ),
                    );
                    muted(12.0, "Preview looks for stats that do not exist yet:");
                    let delta = &mut actions.body_preview_delta;
                    let preview = view.body_preview;
                    stepper_row(
                        &format!("Injury {:.0}%", preview.injury * 100.0),
                        &mut delta.injury,
                        -0.25,
                        0.25,
                    );
                    stepper_row(
                        &format!("Thirst {:.0}%", preview.thirst * 100.0),
                        &mut delta.thirst,
                        -0.25,
                        0.25,
                    );
                    stepper_row(
                        &format!("Exhaustion {:.0}%", preview.exhaustion * 100.0),
                        &mut delta.exhaustion,
                        -0.25,
                        0.25,
                    );
                    stepper_row(
                        &format!("Hunger {:.0}%", preview.hunger * 100.0),
                        &mut delta.hunger,
                        -0.25,
                        0.25,
                    );
                    actions.clear_body_preview |= yakui::button("Clear previews").clicked;
                    section("Movement");
                    muted(
                        13.0,
                        if view.flying {
                            "Flying: spectator camera, no collision. Landing brings your player to the camera."
                        } else {
                            "Walking: server-authoritative character physics."
                        },
                    );
                    yakui::row(|| {
                        let label = if view.flying { "Land (walk)" } else { "Fly" };
                        actions.toggle_flight |= yakui::button(label).clicked;
                        yakui::pad(yakui::widgets::Pad::horizontal(6.0), || {
                            actions.recentre |= yakui::button("Recentre view").clicked;
                        });
                    });

                    section("World");
                    yakui::row(|| {
                        actions.reset_world |= yakui::button("Reset world").clicked;
                        yakui::pad(yakui::widgets::Pad::horizontal(6.0), || {
                            actions.close |= yakui::button("Close menu").clicked;
                        });
                    });
                    muted(
                        12.0,
                        "Reset rebuilds terrain, water, and bodies from the scene for every player.",
                    );
                    if let Some(status) = &view.admin_status {
                        yakui::text(13.0, status.clone());
                    }
                    if let Some(water) = &view.water {
                        muted(12.0, water.clone());
                    }

                    section("Water");
                    muted(
                        12.0,
                        "A scene with no gated spring or dam gate refuses these.",
                    );
                    yakui::row(|| {
                        for (label, rate) in [
                            ("Spring off", 0u8),
                            ("Spring normal", 1),
                            ("Spring fast", 2),
                            ("Spring max", 3),
                        ] {
                            yakui::pad(yakui::widgets::Pad::horizontal(2.0), || {
                                if yakui::button(label).clicked {
                                    actions.set_water_spring_rate = Some(rate);
                                }
                            });
                        }
                    });
                    yakui::row(|| {
                        if yakui::button("Open dam gate").clicked {
                            actions.set_dam_gate = Some(true);
                        }
                        yakui::pad(yakui::widgets::Pad::horizontal(6.0), || {
                            if yakui::button("Close dam gate").clicked {
                                actions.set_dam_gate = Some(false);
                            }
                        });
                    });

                    section("Controls");
                    yakui::row(|| {
                        yakui::column(|| {
                            for (key, _) in KEYBINDS {
                                yakui::text(13.0, *key);
                            }
                        });
                        yakui::pad(yakui::widgets::Pad::horizontal(14.0), || {
                            yakui::column(|| {
                                for (_, action) in KEYBINDS {
                                    muted(13.0, *action);
                                }
                            });
                        });
                    });
                });
                yakui::pad(yakui::widgets::Pad::horizontal(18.0), || {
                    yakui::column(|| {
                        section("Lighting");
                        muted(13.0, view.world_time.clone());
                        yakui::row(|| {
                            if yakui::button(if view.day_cycle_enabled {
                                "Pause day/night cycle"
                            } else {
                                "Resume day/night cycle"
                            })
                            .clicked
                            {
                                *day_cycle_enabled = !*day_cycle_enabled;
                            }
                        });
                        muted(12.0, "Environment preset (resets sun and exposure)");
                        yakui::row(|| {
                            for preset in EnvironmentPreset::ALL {
                                yakui::pad(yakui::widgets::Pad::horizontal(2.0), || {
                                    if yakui::button(preset.label()).clicked {
                                        *selected_environment = Some(preset);
                                    }
                                });
                            }
                        });
                        yakui::row(|| {
                            let on_off = |on: bool| if on { "ON" } else { "OFF" };
                            if yakui::button(format!(
                                "F5 Skylight visibility: {}",
                                on_off(view.sky_visibility_on)
                            ))
                            .clicked
                            {
                                actions.toggle_sky_visibility = true;
                            }
                            yakui::pad(yakui::widgets::Pad::horizontal(6.0), || {
                                // Bounce lives in the skylight cache: with F5 off
                                // there is no cache, so F6 has nothing to show.
                                let note = if view.sky_visibility_on {
                                    ""
                                } else {
                                    " (needs F5)"
                                };
                                if yakui::button(format!(
                                    "F6 Bounce: {}{note}",
                                    on_off(view.bounce_on)
                                ))
                                .clicked
                                {
                                    actions.toggle_bounce = true;
                                }
                            });
                        });
                        yakui::pad(yakui::widgets::Pad::horizontal(6.0), || {
                            if yakui::button(format!("F7 Lighting quality: {}", view.lighting))
                                .clicked
                            {
                                actions.cycle_lighting = true;
                            }
                        });
                        muted(
                            12.0,
                            "Quality: Off = flat ambient, Low = skylight only, Medium / High = bounce light with fewer / more rays.",
                        );
                        muted(
                            12.0,
                            "F5 off = legacy flat ambient (no interior darkening, no bounce).",
                        );
                        muted(12.0, "Render view (F4 cycles)");
                        for row in LIGHTING_VIEWS.chunks(5) {
                            yakui::row(|| {
                                for &view_kind in row {
                                    let name = view_kind.stem();
                                    // Say when a view cannot show anything in the
                                    // current F5/F6 state instead of leaving it blank.
                                    let needs = match view_kind {
                                        DebugView::SkyVisibility if !view.sky_visibility_on => {
                                            " (needs F5)"
                                        }
                                        DebugView::IndirectOnly
                                            if !view.sky_visibility_on || !view.bounce_on =>
                                        {
                                            " (needs F5+F6)"
                                        }
                                        _ => "",
                                    };
                                    let label = if *debug_view == view_kind {
                                        format!("[{name}]{needs}")
                                    } else {
                                        format!("{name}{needs}")
                                    };
                                    yakui::pad(yakui::widgets::Pad::horizontal(2.0), || {
                                        if yakui::button(label).clicked {
                                            *debug_view = view_kind;
                                        }
                                    });
                                }
                            });
                        }
                        let elevation = (-environment.sun_dir.y)
                            .clamp(-1.0, 1.0)
                            .asin()
                            .to_degrees();
                        let azimuth = environment
                            .sun_dir
                            .z
                            .atan2(environment.sun_dir.x)
                            .to_degrees();
                        let (mut d_elevation, mut d_azimuth) = (0.0_f32, 0.0_f32);
                        let (mut d_exposure, mut d_sun, mut d_sky, mut d_size) =
                            (1.0_f32, 1.0, 1.0, 1.0);
                        yakui::row(|| {
                            stepper_row(
                                &format!("Sun elevation {elevation:.0} deg"),
                                &mut d_elevation,
                                -10.0,
                                10.0,
                            );
                            yakui::pad(yakui::widgets::Pad::horizontal(16.0), || {
                                stepper_row(
                                    &format!("Sun azimuth {azimuth:.0} deg"),
                                    &mut d_azimuth,
                                    -15.0,
                                    15.0,
                                );
                            });
                        });
                        yakui::row(|| {
                            stepper_scale_row(
                                &format!("Sun intensity {:.2}", environment.sun_intensity),
                                &mut d_sun,
                            );
                            yakui::pad(yakui::widgets::Pad::horizontal(16.0), || {
                                stepper_scale_row(
                                    &format!("Exposure {:.2}", environment.exposure),
                                    &mut d_exposure,
                                );
                            });
                        });
                        yakui::row(|| {
                            stepper_scale_row(
                                &format!(
                                    "Sky {:.2} / ground {:.2}",
                                    environment.sky[1], environment.ground[1]
                                ),
                                &mut d_sky,
                            );
                            yakui::pad(yakui::widgets::Pad::horizontal(16.0), || {
                                stepper_scale_row(
                                    &format!(
                                        "Sun size {:.1} deg",
                                        environment.sun_angular_diameter_deg
                                    ),
                                    &mut d_size,
                                );
                            });
                        });
                        if d_elevation != 0.0 || d_azimuth != 0.0 {
                            let el = (elevation + d_elevation).clamp(-89.0, 89.0).to_radians();
                            let az = (azimuth + d_azimuth).to_radians();
                            environment.sun_dir =
                                Vec3::new(el.cos() * az.cos(), -el.sin(), el.cos() * az.sin());
                        }
                        environment.sun_intensity =
                            (environment.sun_intensity * d_sun).clamp(0.0, 40.0);
                        environment.exposure =
                            (environment.exposure * d_exposure).clamp(0.05, 20.0);
                        environment.sun_angular_diameter_deg =
                            (environment.sun_angular_diameter_deg * d_size).clamp(0.05, 20.0);
                        for channel in environment
                            .sky
                            .iter_mut()
                            .chain(environment.ground.iter_mut())
                        {
                            *channel *= d_sky;
                        }
                        section("Debug");
                        yakui::row(|| {
                            actions.copy_debug_report |=
                                yakui::button("Copy debug report (F9)").clicked;
                        });
                        muted(
                            12.0,
                            "Copies position, frame statistics, lighting, water and settings as text.",
                        );
                        if let Some(status) = &view.report_status {
                            yakui::text(13.0, status.clone());
                        }
                        for line in &view.frame_summary {
                            muted(12.0, line.clone());
                        }
                    });
                });
            });
        });
    });
    if *environment != environment_before_controls {
        *day_cycle_enabled = false;
    }
}

/// Every render view the Lighting panel offers, in `F4` order.
pub(super) const LIGHTING_VIEWS: [DebugView; 9] = [
    DebugView::Shaded,
    DebugView::IndirectOnly,
    DebugView::SkyVisibility,
    DebugView::ShadowVisibility,
    DebugView::ShadowCascades,
    DebugView::Albedo,
    DebugView::Normals,
    DebugView::Depth,
    DebugView::Roughness,
];

/// A `-`/`+` button pair that adds `minus`/`plus` to `delta`, then a label.
pub(super) fn stepper_row(label: &str, delta: &mut f32, minus: f32, plus: f32) {
    yakui::row(|| {
        if yakui::button("-").clicked {
            *delta += minus;
        }
        yakui::pad(yakui::widgets::Pad::horizontal(6.0), || {
            if yakui::button("+").clicked {
                *delta += plus;
            }
        });
        yakui::text(13.0, label.to_owned());
    });
}

/// A `-`/`+` button pair that scales `scale` (starting at 1) by 1/1.25 or
/// 1.25, then a label.
pub(super) fn stepper_scale_row(label: &str, scale: &mut f32) {
    yakui::row(|| {
        if yakui::button("-").clicked {
            *scale /= 1.25;
        }
        yakui::pad(yakui::widgets::Pad::horizontal(6.0), || {
            if yakui::button("+").clicked {
                *scale *= 1.25;
            }
        });
        yakui::text(13.0, label.to_owned());
    });
}

/// The next debug view in the `F4` cycle, wrapping back to shaded.
pub(super) fn next_debug_view(view: DebugView) -> DebugView {
    let at = LIGHTING_VIEWS.iter().position(|v| *v == view).unwrap_or(0);
    LIGHTING_VIEWS[(at + 1) % LIGHTING_VIEWS.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_debug_view_cycle_visits_every_window_view_and_wraps_to_shaded() {
        let mut view = DebugView::Shaded;
        let mut seen = vec![view];
        for _ in 0..9 {
            view = next_debug_view(view);
            seen.push(view);
        }
        assert_eq!(seen.first(), seen.last(), "wraps after nine presses");
        seen.pop();
        seen.sort_by_key(|v| v.stem());
        seen.dedup();
        assert_eq!(seen.len(), 9, "no view repeats within a cycle");
    }
}
