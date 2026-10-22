//! Snow lighting helper shared with the weather overlay. The snow layer
//! geometry lives in `snow_mesh`.

/// A softly bounded ambient/sun estimate for snow, kept below full white so the
/// weather whiteout overlay does not glare.
pub(crate) fn environment_brightness(environment: &crate::Environment) -> f32 {
    let luminance = |rgb: [f32; 3]| rgb[0] * 0.2126 + rgb[1] * 0.7152 + rgb[2] * 0.0722;
    let sky = luminance(environment.sky);
    let ground = luminance(environment.ground);
    let sun = luminance(environment.sun_color)
        * environment.sun_intensity.max(0.0)
        * environment.sun_visibility.clamp(0.0, 1.0)
        * (-environment.sun_dir.y).clamp(0.0, 1.0);
    (0.56 + sky * 0.20 + ground * 0.08 + sun * 0.012).clamp(0.52, 0.84)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_shading_is_bounded_and_avoids_full_white() {
        let daylight = environment_brightness(&crate::EnvironmentPreset::Daylight.environment());
        let night = environment_brightness(&crate::EnvironmentPreset::Night.environment());
        assert!((0.52..=0.84).contains(&daylight));
        assert!((0.52..=0.84).contains(&night));
        assert!(daylight > night);
    }
}
