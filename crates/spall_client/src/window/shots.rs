use super::*;

/// One step of a scripted screenshot tour (see [`parse_shots`]).
#[derive(Debug, Clone, PartialEq)]
pub enum ShotStep {
    /// Fly the camera to `eye` (metres) looking along yaw/pitch (degrees;
    /// yaw 0 looks north along -Z, positive yaw turns toward +X).
    Camera {
        eye: [f32; 3],
        yaw_deg: f32,
        pitch_deg: f32,
    },
    /// Land and look from the player.
    Walk,
    /// Hold actual player input for a bounded walking/rendering measurement.
    Move { seconds: f32, movement: [f32; 3] },
    /// Toggle the admin menu.
    Menu,
    /// Copy the debug report, as `F9` does.
    Report,
    /// Ask the server for an admin world reset.
    Reset,
    /// Select authoritative weather through the same admin path as F10.
    Weather(Option<spall_protocol::WeatherKind>),
    /// End a capture tour and hand the running window to the player.
    Play,
    /// Pause without a screenshot.
    Wait(f32),
    /// Switch the render view (`shaded`, `indirect_only`, `sky_visibility`...)
    /// without a screenshot.
    View(DebugView),
    /// Visibility-aware skylight on or off, without a screenshot.
    Skylight(bool),
    /// Diffuse bounce on or off, without a screenshot.
    Bounce(bool),
    /// Lighting quality (`lighting:off|low|medium|high`), without a screenshot.
    Lighting(spall_render::sky_visibility::LightingQuality),
    /// Switch the environment preset (`daylight`, `night`...) without a
    /// screenshot.
    Environment(EnvironmentPreset),
}

/// Parses `x,y,z,yaw,pitch;menu;report;reset;walk;wait:2;view:indirect_only;sky:off;
/// bounce:on;weather:snow;play` into steps. `play` hands the window to the user.
pub fn parse_shots(spec: &str) -> Result<Vec<ShotStep>, String> {
    spec.split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|step| match step {
            "walk" => Ok(ShotStep::Walk),
            "menu" => Ok(ShotStep::Menu),
            "report" => Ok(ShotStep::Report),
            "reset" => Ok(ShotStep::Reset),
            "play" => Ok(ShotStep::Play),
            _ if step.starts_with("weather:") => {
                use spall_protocol::WeatherKind::*;
                let kind = match &step[8..] {
                    "auto" => None,
                    "clear" => Some(Clear),
                    "rain" => Some(Rain),
                    "storm" => Some(Storm),
                    "snow" => Some(Snow),
                    "wind" => Some(HighWind),
                    "blizzard" => Some(Blizzard),
                    _ => return Err(format!("unknown weather `{step}`")),
                };
                Ok(ShotStep::Weather(kind))
            }
            _ if step.starts_with("move:") => {
                let v = step[5..]
                    .split(',')
                    .map(str::parse::<f32>)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| format!("bad move `{step}`"))?;
                match v.as_slice() {
                    [seconds, x, z]
                        if seconds.is_finite()
                            && *seconds > 0.
                            && *seconds <= 120.
                            && x.is_finite()
                            && z.is_finite()
                            && x.abs() <= 1.
                            && z.abs() <= 1. =>
                    {
                        Ok(ShotStep::Move {
                            seconds: *seconds,
                            movement: [*x, 0., *z],
                        })
                    }
                    _ => Err(format!(
                        "move needs seconds (0..120), x and z (-1..1): `{step}`"
                    )),
                }
            }
            _ if step.starts_with("wait:") => step[5..]
                .parse()
                .map(ShotStep::Wait)
                .map_err(|_| format!("bad wait `{step}`")),
            _ if step.starts_with("view:") => LIGHTING_VIEWS
                .iter()
                .find(|v| v.stem() == &step[5..])
                .copied()
                .map(ShotStep::View)
                .ok_or_else(|| format!("unknown view `{step}`")),
            _ if step.starts_with("env:") => EnvironmentPreset::from_key(&step[4..])
                .map(ShotStep::Environment)
                .ok_or_else(|| format!("unknown environment `{step}`")),
            "sky:on" => Ok(ShotStep::Skylight(true)),
            "sky:off" => Ok(ShotStep::Skylight(false)),
            "bounce:on" => Ok(ShotStep::Bounce(true)),
            "bounce:off" => Ok(ShotStep::Bounce(false)),
            s if s.starts_with("lighting:") => {
                spall_render::sky_visibility::LightingQuality::parse(&s["lighting:".len()..])
                    .map(ShotStep::Lighting)
                    .ok_or_else(|| format!("unknown lighting quality `{step}`"))
            }
            _ => {
                let v: Vec<f32> = step
                    .split(',')
                    .map(|n| n.trim().parse::<f32>())
                    .collect::<Result<_, _>>()
                    .map_err(|_| format!("bad camera shot `{step}`"))?;
                match v[..] {
                    [x, y, z, yaw_deg, pitch_deg] => Ok(ShotStep::Camera {
                        eye: [x, y, z],
                        yaw_deg,
                        pitch_deg,
                    }),
                    _ => Err(format!("camera shot `{step}` needs x,y,z,yaw,pitch")),
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weather_capture_script_uses_typed_admin_choices() {
        assert_eq!(
            parse_shots("weather:auto;weather:rain;weather:snow;weather:blizzard;play").unwrap(),
            vec![
                ShotStep::Weather(None),
                ShotStep::Weather(Some(spall_protocol::WeatherKind::Rain)),
                ShotStep::Weather(Some(spall_protocol::WeatherKind::Snow)),
                ShotStep::Weather(Some(spall_protocol::WeatherKind::Blizzard)),
                ShotStep::Play,
            ]
        );
        assert!(parse_shots("weather:garbage").is_err());
    }
}
