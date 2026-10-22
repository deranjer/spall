//! Full-screen grade applied in the tone-map pass after tone mapping: how the
//! game shows the player's body state (cold, wet, hurt) on screen without a
//! HUD. The renderer knows only these generic knobs; what drives them is the
//! client's business.

/// One frame's screen grade. [`ScreenGrade::IDENTITY`] leaves the tone-mapped
/// image unchanged.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenGrade {
    /// 1 keeps colour, 0 is greyscale.
    pub saturation: f32,
    /// Multiplies the graded colour (1, 1, 1 = no tint).
    pub tint: [f32; 3],
    /// How strongly the screen edge fades toward `vignette_color`, 0..=1.
    pub vignette: f32,
    pub vignette_color: [f32; 3],
    /// Blur toward the screen edge, 0..=1.
    pub edge_blur: f32,
    /// Ice crystals creeping in from the screen edge, 0..=1.
    pub frost: f32,
}

impl ScreenGrade {
    pub const IDENTITY: Self = Self {
        saturation: 1.0,
        tint: [1.0; 3],
        vignette: 0.0,
        vignette_color: [0.0; 3],
        edge_blur: 0.0,
        frost: 0.0,
    };

    /// The grade with every knob in its valid range (non-finite values fall
    /// back to identity), so a bad input can never blank the screen.
    pub fn clamped(&self) -> Self {
        let unit = |value: f32, fallback: f32| {
            if value.is_finite() {
                value.clamp(0.0, 1.0)
            } else {
                fallback
            }
        };
        Self {
            saturation: unit(self.saturation, 1.0),
            tint: self.tint.map(|c| unit(c, 1.0)),
            vignette: unit(self.vignette, 0.0),
            vignette_color: self.vignette_color.map(|c| unit(c, 0.0)),
            edge_blur: unit(self.edge_blur, 0.0),
            frost: unit(self.frost, 0.0),
        }
    }
}

impl Default for ScreenGrade {
    fn default() -> Self {
        Self::IDENTITY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The grade lives in the tone-map shader; a WGSL uniformity mistake there
    /// would only otherwise show up at pipeline creation on a GPU.
    #[test]
    fn tone_map_wgsl_validates() {
        let module = naga::front::wgsl::parse_str(include_str!("shaders/tonemap.wgsl"))
            .expect("tone-map WGSL parses");
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .expect("tone-map shader validates");
    }

    #[test]
    fn clamping_keeps_identity_and_rejects_non_finite_values() {
        assert_eq!(ScreenGrade::IDENTITY.clamped(), ScreenGrade::IDENTITY);
        let wild = ScreenGrade {
            saturation: f32::NAN,
            tint: [2.0, -1.0, f32::INFINITY],
            vignette: 5.0,
            vignette_color: [f32::NAN, 0.5, 3.0],
            edge_blur: -2.0,
            frost: 1.5,
        }
        .clamped();
        assert_eq!(wild.saturation, 1.0);
        assert_eq!(wild.tint, [1.0, 0.0, 1.0]);
        assert_eq!(wild.vignette, 1.0);
        assert_eq!(wild.vignette_color, [0.0, 0.5, 1.0]);
        assert_eq!((wild.edge_blur, wild.frost), (0.0, 1.0));
    }
}
