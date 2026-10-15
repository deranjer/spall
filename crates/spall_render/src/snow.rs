//! Bounded, terrain-aligned winter surface coating and footprints.
//!
//! The client supplies only confirmed exposed +Y faces from committed terrain.
//! This module expands compact rectangles into a small dynamic vertex buffer;
//! it does not infer snow exposure from camera or geometry state.

pub const MAX_SNOW_PATCHES: usize = 8_192;
pub const MAX_SNOW_FOOTPRINTS: usize = 1_024;

/// A softly bounded ambient/sun estimate for snow, kept deliberately below a
/// full-white output so a coating does not glare against the lit terrain.
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

/// One merged rectangle of exposed quarter-metre +Y faces at a shared height
/// and snow depth. `centre.y` is the uncovered support plane in world metres.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SnowSurfacePatch {
    pub centre: [f32; 3],
    pub half_extent_xz: [f32; 2],
    pub depth_m: f32,
    /// Stable low-cost colour variation derived from the authoritative cohort.
    pub variation: u32,
}

/// A boot depression constrained to the confirmed contact cell. The renderer
/// clips the footprint polygon to that 0.25 m cell, so the mark cannot extend
/// over an unexposed edge, roof edge, or unknown terrain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SnowFootprintMark {
    pub centre: [f32; 3],
    pub cell_centre_xz: [f32; 2],
    pub heading_rad: f32,
    pub foot: u8,
    pub strength: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SnowRenderError {
    #[error("snow patch count exceeds the renderer bound")]
    TooManyPatches,
    #[error("snow footprint count exceeds the renderer bound")]
    TooManyFootprints,
    #[error("snow geometry contains a non-finite or invalid value")]
    InvalidGeometry,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct SnowVertex {
    pub position: [f32; 3],
    pub colour: [f32; 4],
}

impl SnowVertex {
    pub(crate) const LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x4],
    };
}

pub(crate) fn build_snow_vertices(
    patches: &[SnowSurfacePatch],
    footprints: &[SnowFootprintMark],
    output: &mut Vec<SnowVertex>,
) -> Result<(), SnowRenderError> {
    if patches.len() > MAX_SNOW_PATCHES {
        return Err(SnowRenderError::TooManyPatches);
    }
    if footprints.len() > MAX_SNOW_FOOTPRINTS {
        return Err(SnowRenderError::TooManyFootprints);
    }
    if patches.iter().any(|patch| {
        !patch.centre.iter().all(|value| value.is_finite())
            || !patch.half_extent_xz.iter().all(|value| value.is_finite())
            || !patch.depth_m.is_finite()
            || patch
                .half_extent_xz
                .iter()
                .any(|value| *value <= 0.0 || *value > 2.0)
            || !(0.0..=0.125).contains(&patch.depth_m)
    }) || footprints.iter().any(|mark| {
        !mark.centre.iter().all(|value| value.is_finite())
            || !mark.cell_centre_xz.iter().all(|value| value.is_finite())
            || !mark.heading_rad.is_finite()
            || !mark.strength.is_finite()
            || mark.foot > 1
            || !(0.0..=1.0).contains(&mark.strength)
            || (mark.centre[0] - mark.cell_centre_xz[0]).abs() > 0.125
            || (mark.centre[2] - mark.cell_centre_xz[1]).abs() > 0.125
    }) {
        return Err(SnowRenderError::InvalidGeometry);
    }

    output.clear();
    output.reserve(patches.len().saturating_mul(6) + footprints.len().saturating_mul(48));
    for patch in patches {
        append_patch(patch, output);
    }
    for mark in footprints {
        append_footprint(mark, output);
    }
    Ok(())
}

fn append_patch(patch: &SnowSurfacePatch, output: &mut Vec<SnowVertex>) {
    let [cx, base_y, cz] = patch.centre;
    let [hx, hz] = patch.half_extent_xz;
    let y = base_y + patch.depth_m.max(0.0015) + 0.0015;
    let shade = ((patch.variation.wrapping_mul(0x9E37_79B9) >> 27) as f32 / 31.0 - 0.5) * 0.045;
    let depth = (patch.depth_m / 0.125).clamp(0.0, 1.0);
    let colour = [
        (0.70 + depth * 0.07 + shade).clamp(0.0, 1.0),
        (0.77 + depth * 0.07 + shade).clamp(0.0, 1.0),
        (0.83 + depth * 0.06 + shade).clamp(0.0, 1.0),
        1.0,
    ];
    let a = [cx - hx, y, cz - hz];
    let b = [cx + hx, y, cz - hz];
    let c = [cx + hx, y, cz + hz];
    let d = [cx - hx, y, cz + hz];
    push_triangle(output, a, c, b, colour);
    push_triangle(output, a, d, c, colour);
}

fn append_footprint(mark: &SnowFootprintMark, output: &mut Vec<SnowVertex>) {
    // Boot shape in local (across, forward) coordinates, forward along the
    // recorded heading. Clipping to the authoritative contact cell prevents
    // footprints from drawing over a roof edge or a neighbouring unknown cell.
    let (sin, cos) = mark.heading_rad.sin_cos();
    // Player yaw zero faces -Z, with positive yaw turning toward +X.
    let (side_x, side_z) = (cos, sin);
    let (forward_x, forward_z) = (sin, -cos);
    let length = 0.18;
    let width = if mark.foot == 0 { 0.082 } else { 0.074 };
    let local = [
        (-width * 0.34, -length * 0.50),
        (width * 0.34, -length * 0.50),
        (width * 0.48, -length * 0.32),
        (width * 0.50, length * 0.20),
        (width * 0.40, length * 0.42),
        (width * 0.22, length * 0.50),
        (-width * 0.22, length * 0.50),
        (-width * 0.40, length * 0.42),
        (-width * 0.50, length * 0.20),
        (-width * 0.48, -length * 0.32),
    ];
    let polygon: Vec<[f32; 2]> = local
        .iter()
        .map(|(side, forward)| {
            [
                mark.centre[0] + side_x * side + forward_x * forward,
                mark.centre[2] + side_z * side + forward_z * forward,
            ]
        })
        .collect();
    let half = 0.125;
    let bounds = [
        mark.cell_centre_xz[0] - half,
        mark.cell_centre_xz[0] + half,
        mark.cell_centre_xz[1] - half,
        mark.cell_centre_xz[1] + half,
    ];
    let clipped = clip_to_cell(polygon, bounds);
    if clipped.len() < 3 {
        return;
    }
    let alpha = mark.strength.clamp(0.0, 1.0);
    // A soft, cool-grey depression stays legible while retaining the snow's
    // lighting range; alpha is baked into the blend-free color for subtlety.
    let colour = [
        0.66 - 0.15 * alpha,
        0.73 - 0.14 * alpha,
        0.80 - 0.13 * alpha,
        1.0,
    ];
    let centre = clipped.iter().fold([0.0, 0.0], |sum, point| {
        [sum[0] + point[0], sum[1] + point[1]]
    });
    let centre = [
        centre[0] / clipped.len() as f32,
        centre[1] / clipped.len() as f32,
    ];
    for index in 0..clipped.len() {
        let a = clipped[index];
        let b = clipped[(index + 1) % clipped.len()];
        push_triangle(
            output,
            [centre[0], mark.centre[1] + 0.003, centre[1]],
            [a[0], mark.centre[1] + 0.003, a[1]],
            [b[0], mark.centre[1] + 0.003, b[1]],
            colour,
        );
    }
}

fn clip_to_cell(mut polygon: Vec<[f32; 2]>, bounds: [f32; 4]) -> Vec<[f32; 2]> {
    for (axis, edge, keep_greater) in [
        (0, bounds[0], true),
        (0, bounds[1], false),
        (1, bounds[2], true),
        (1, bounds[3], false),
    ] {
        if polygon.is_empty() {
            break;
        }
        let input = std::mem::take(&mut polygon);
        let mut previous = *input.last().expect("non-empty polygon");
        let mut previous_inside = if keep_greater {
            previous[axis] >= edge
        } else {
            previous[axis] <= edge
        };
        for current in input {
            let current_inside = if keep_greater {
                current[axis] >= edge
            } else {
                current[axis] <= edge
            };
            if current_inside != previous_inside {
                let amount = (edge - previous[axis]) / (current[axis] - previous[axis]);
                let mut intersection = [
                    previous[0] + (current[0] - previous[0]) * amount,
                    previous[1] + (current[1] - previous[1]) * amount,
                ];
                intersection[axis] = edge;
                polygon.push(intersection);
            }
            if current_inside {
                polygon.push(current);
            }
            previous = current;
            previous_inside = current_inside;
        }
    }
    polygon
}

fn push_triangle(
    output: &mut Vec<SnowVertex>,
    a: [f32; 3],
    b: [f32; 3],
    c: [f32; 3],
    colour: [f32; 4],
) {
    output.extend([a, b, c].map(|position| SnowVertex { position, colour }));
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

    #[test]
    fn snow_geometry_is_bounded_and_depth_encoded_without_flat_zero_depth() {
        let mut output = Vec::new();
        build_snow_vertices(
            &[SnowSurfacePatch {
                centre: [2.0, 3.0, 4.0],
                half_extent_xz: [2.0, 2.0],
                depth_m: 0.0,
                variation: 9,
            }],
            &[],
            &mut output,
        )
        .unwrap();
        assert_eq!(output.len(), 6);
        assert!(output.iter().all(|vertex| vertex.position[1] > 3.0));
        assert!(matches!(
            build_snow_vertices(
                &[SnowSurfacePatch {
                    centre: [0.0, 0.0, 0.0],
                    half_extent_xz: [1.0, 1.0],
                    depth_m: 0.126,
                    variation: 0,
                }],
                &[],
                &mut output,
            ),
            Err(SnowRenderError::InvalidGeometry)
        ));
    }

    #[test]
    fn footprint_polygon_clips_to_contact_cell() {
        let mut output = Vec::new();
        build_snow_vertices(
            &[],
            &[SnowFootprintMark {
                centre: [0.124, 1.01, 0.0],
                cell_centre_xz: [0.0, 0.0],
                heading_rad: 0.3,
                foot: 0,
                strength: 1.0,
            }],
            &mut output,
        )
        .unwrap();
        assert!(!output.is_empty());
        assert!(output.iter().all(|vertex| {
            vertex.position[0] >= -0.125 - f32::EPSILON
                && vertex.position[0] <= 0.125 + f32::EPSILON
                && vertex.position[2] >= -0.125 - f32::EPSILON
                && vertex.position[2] <= 0.125 + f32::EPSILON
        }));
    }

    #[test]
    fn boot_toes_follow_the_authoritative_walking_heading() {
        for (heading, direction) in [
            (0.0, [0.0, -1.0]),
            (std::f32::consts::FRAC_PI_2, [1.0, 0.0]),
            (std::f32::consts::PI, [0.0, 1.0]),
        ] {
            let mut vertices = Vec::new();
            build_snow_vertices(
                &[],
                &[SnowFootprintMark {
                    centre: [0.0; 3],
                    cell_centre_xz: [0.0; 2],
                    heading_rad: heading,
                    foot: 0,
                    strength: 1.0,
                }],
                &mut vertices,
            )
            .unwrap();
            // The asymmetric boot has more area toward its toes than its heel.
            let toe_bias = vertices
                .iter()
                .map(|vertex| vertex.position[0] * direction[0] + vertex.position[2] * direction[1])
                .sum::<f32>()
                / vertices.len() as f32;
            assert!(
                toe_bias > 0.005,
                "boot points away from walking heading {heading}: {toe_bias}"
            );
        }
    }
}

#[cfg(test)]
mod snow_shader_tests {
    #[test]
    fn snow_material_wgsl_validates() {
        let source = include_str!("shaders/snow.wgsl");
        let module = naga::front::wgsl::parse_str(source).expect("snow shader parses");
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .expect("snow shader entry points validate");
    }
}
