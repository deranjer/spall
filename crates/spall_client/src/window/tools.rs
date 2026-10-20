use super::*;

/// The hammer: left click sends the game's `DIG` tool along the camera ray
/// (the server finds the hit cell itself and caps the radius). Holding the
/// button repeats at this interval.
pub(super) const HAMMER_TOOL: u16 = 0;
pub(super) const HAMMER_REPEAT: Duration = Duration::from_millis(140);
/// Radius 0 breaks exactly the struck voxel; 1, 2, 3 break 7, 33, 123 cells.
pub(super) const HAMMER_RADIUS_DEFAULT: i64 = 2;
/// The game's `DIG` tool reach (`sandbox::game::tool_catalog`): past this the
/// server refuses the swing, so the crosshair shows no target.
pub(super) const HAMMER_REACH_M: f64 = 12.0;
/// Stroke thickness of the target outline: a tenth of a voxel, visible at range.
pub(super) const HAMMER_STROKE_M: f32 = 0.025;
pub(super) const HAMMER_RADIUS_MAX: i64 = 8;
/// Request ids for window-fired tools start here, clear of the scripted (low)
/// and review-cut (`1_000_000 + n`) ranges.
pub(super) const HAMMER_REQUEST_BASE: u64 = 2_000_000;

/// The torch: the game's `PLACE_LAMP` tool, bound to `T`. It places a small
/// emissive lamp ball against the surface under the crosshair.
pub(super) const TORCH_TOOL: u16 = 4;
/// Radius in cells: 0 = exactly one voxel, so a torch fits in a crack and does
/// not fill the hole it lights.
pub const TORCH_RADIUS_CELLS: i64 = 0;

/// The `Cut` a hammer swing sends: aimed from the eye along the view. The
/// claimed brush centre is ignored by the server (it uses its own hit cell);
/// only the radius is kept, after the tool's cap.
pub fn hammer_request(
    id: u64,
    eye: Vec3,
    dir: Vec3,
    radius_cells: i64,
) -> spall_protocol::ActionRequest {
    tool_request(
        id,
        HAMMER_TOOL,
        spall_protocol::ActionKind::Cut,
        eye,
        dir,
        radius_cells,
    )
}

/// The `Place` a torch sends: aimed like a swing; the server places the lamp
/// in the empty cell against the struck face.
pub fn torch_request(id: u64, eye: Vec3, dir: Vec3) -> spall_protocol::ActionRequest {
    tool_request(
        id,
        TORCH_TOOL,
        spall_protocol::ActionKind::Place,
        eye,
        dir,
        TORCH_RADIUS_CELLS,
    )
}

/// An aimed tool use: the game tool `tool`, `action`, from `eye` along `dir`.
pub(super) fn tool_request(
    id: u64,
    tool: u16,
    action: spall_protocol::ActionKind,
    eye: Vec3,
    dir: Vec3,
    radius_cells: i64,
) -> spall_protocol::ActionRequest {
    use spall_core::units::{BRUSH_UNIT, BrushPoint};
    use spall_protocol::{ActionRequest, ClaimedTarget, InputSeq, RequestId};
    let brush = spall_core::SphereBrush::new(
        BrushPoint::from_units(0, 0, 0),
        radius_cells.clamp(0, HAMMER_RADIUS_MAX) * BRUSH_UNIT,
    )
    .expect("hammer radius is within brush limits");
    ActionRequest {
        request_id: RequestId(id),
        input_seq: InputSeq(id),
        action,
        tool,
        aim_origin_m: [f64::from(eye.x), f64::from(eye.y), f64::from(eye.z)],
        aim_dir: [dir.x, dir.y, dir.z],
        claimed_target: ClaimedTarget::Terrain,
        claimed_brush: brush,
    }
}

/// The solid cell the camera ray meets within the hammer's reach, and its
/// material, found in the client's own replica of the terrain. The server does
/// the authoritative version of this from the same eye and direction.
pub(super) fn cast_hammer_ray(
    volume: &Volume,
    eye: Vec3,
    dir: Vec3,
) -> Option<(GlobalCell, MaterialId)> {
    use spall_voxel::query::{Ray, RayConfig, RayOutcome, cast_ray};
    let cell_m = f64::from(CELL_M);
    let origin = glam::DVec3::new(f64::from(eye.x), f64::from(eye.y), f64::from(eye.z)) / cell_m;
    let heading = glam::DVec3::new(f64::from(dir.x), f64::from(dir.y), f64::from(dir.z));
    match cast_ray(
        volume,
        Ray::new(origin, heading),
        RayConfig::new(HAMMER_REACH_M / cell_m),
    ) {
        Ok(RayOutcome::Hit(hit)) => Some((hit.cell, hit.material)),
        _ => None,
    }
}

/// Wireframe of the cells a swing at `cell` with `radius_cells` can remove: the
/// sphere's bounding box (one voxel at radius 0), as thin strokes.
pub(super) fn hammer_outline(cell: GlobalCell, radius_cells: i64, material: u32) -> Vec<Instance> {
    let cell_m = CELL_M;
    let lo = Vec3::new(
        (cell.x - radius_cells) as f32,
        (cell.y - radius_cells) as f32,
        (cell.z - radius_cells) as f32,
    ) * cell_m;
    let hi = Vec3::new(
        (cell.x + radius_cells + 1) as f32,
        (cell.y + radius_cells + 1) as f32,
        (cell.z + radius_cells + 1) as f32,
    ) * cell_m;
    let corner = |x: bool, y: bool, z: bool| {
        Vec3::new(
            if x { hi.x } else { lo.x },
            if y { hi.y } else { lo.y },
            if z { hi.z } else { lo.z },
        )
    };
    let mut out = Vec::with_capacity(12);
    for y in [false, true] {
        for z in [false, true] {
            out.extend(debug_line_thick(
                corner(false, y, z),
                corner(true, y, z),
                material,
                HAMMER_STROKE_M,
            ));
        }
    }
    for x in [false, true] {
        for z in [false, true] {
            out.extend(debug_line_thick(
                corner(x, false, z),
                corner(x, true, z),
                material,
                HAMMER_STROKE_M,
            ));
        }
    }
    for x in [false, true] {
        for y in [false, true] {
            out.extend(debug_line_thick(
                corner(x, y, false),
                corner(x, y, true),
                material,
                HAMMER_STROKE_M,
            ));
        }
    }
    out
}

/// Whether a swing is due: a fresh click always is; a held button repeats
/// once [`HAMMER_REPEAT`] has passed since the last one.
pub(super) fn hammer_due(pending: bool, held: bool, last: Option<Instant>, now: Instant) -> bool {
    pending || (held && last.is_none_or(|at| now.saturating_duration_since(at) >= HAMMER_REPEAT))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hammer_request_aims_along_the_view_and_clamps_the_radius() {
        let r = hammer_request(
            2_000_005,
            Vec3::new(1.0, 2.0, 3.0),
            Vec3::new(0.0, -0.6, 0.8),
            99,
        );
        assert_eq!(r.request_id.0, 2_000_005);
        assert_eq!(r.tool, HAMMER_TOOL);
        assert_eq!(r.action, spall_protocol::ActionKind::Cut);
        assert_eq!(r.claimed_target, spall_protocol::ClaimedTarget::Terrain);
        assert_eq!(r.aim_origin_m, [1.0, 2.0, 3.0]);
        assert_eq!(r.aim_dir, [0.0, -0.6, 0.8]);
        assert_eq!(
            r.claimed_brush.radius_units(),
            HAMMER_RADIUS_MAX * spall_core::units::BRUSH_UNIT
        );
        let single = hammer_request(1, Vec3::ZERO, Vec3::Z, 0);
        assert_eq!(
            single.claimed_brush.radius_units(),
            0,
            "radius 0 breaks one voxel"
        );
        let below = hammer_request(1, Vec3::ZERO, Vec3::Z, -5);
        assert_eq!(below.claimed_brush.radius_units(), 0);
    }

    /// A 32x32x32-cell stone block whose top face is at cell y = 32 (8 m), under
    /// three bricks of air.
    fn stone_block() -> Volume {
        use spall_core::{BrickCoord, VolumeId};
        let mut volume = Volume::new(VolumeId::new(1).unwrap(), spall_core::CellSizeCode::Quarter);
        volume
            .insert_brick(
                BrickCoord::new(0, 0, 0),
                spall_voxel::brick::Brick::uniform(MaterialId(1), spall_core::Revision::ZERO),
            )
            .unwrap();
        // Resident air above, so a ray never crosses an unloaded brick.
        for by in 1..=3 {
            volume
                .insert_brick(
                    BrickCoord::new(0, by, 0),
                    spall_voxel::brick::Brick::uniform(MaterialId::AIR, spall_core::Revision::ZERO),
                )
                .unwrap();
        }
        volume
    }

    #[test]
    fn the_crosshair_ray_finds_the_struck_cell_within_reach_only() {
        let volume = stone_block();
        // 1 m above the block's top face (y = 8 m), looking straight down at x=z=4.1 m.
        let hit = cast_hammer_ray(&volume, Vec3::new(4.1, 9.0, 4.1), Vec3::NEG_Y);
        assert_eq!(hit, Some((GlobalCell::new(16, 31, 16), MaterialId(1))));
        // Looking up or sideways into open air: nothing.
        assert_eq!(
            cast_hammer_ray(&volume, Vec3::new(4.1, 9.0, 4.1), Vec3::Y),
            None
        );
        // Beyond the 12 m reach of the dig tool: no target, matching the server.
        assert_eq!(
            cast_hammer_ray(&volume, Vec3::new(4.1, 8.0 + 13.0, 4.1), Vec3::NEG_Y),
            None
        );
        // Just inside reach still hits.
        assert!(cast_hammer_ray(&volume, Vec3::new(4.1, 8.0 + 11.0, 4.1), Vec3::NEG_Y).is_some());
    }

    #[test]
    fn the_hammer_outline_is_a_twelve_edge_box_of_the_brush_extent() {
        // Strokes are `HAMMER_STROKE_M` thick and overhang the box by up to that.
        const STROKE: f32 = HAMMER_STROKE_M + 0.0001;
        let cell = GlobalCell::new(10, 20, 30);
        for radius in [0, 2, 8] {
            let edges = hammer_outline(cell, radius, 9);
            assert_eq!(edges.len(), 12, "radius {radius}");
            assert!(edges.iter().all(|e| e.material == 9));
            // Every stroke lies within the brush's bounding box (plus stroke width).
            let lo = Vec3::new(
                (10 - radius) as f32,
                (20 - radius) as f32,
                (30 - radius) as f32,
            ) * CELL_M;
            let hi = Vec3::new(
                (11 + radius) as f32,
                (21 + radius) as f32,
                (31 + radius) as f32,
            ) * CELL_M;
            for e in &edges {
                for a in 0..3 {
                    let half = e.size[a] * 0.5;
                    assert!(
                        e.offset[a] - half >= lo[a] - STROKE,
                        "radius {radius} axis {a}"
                    );
                    assert!(
                        e.offset[a] + half <= hi[a] + STROKE,
                        "radius {radius} axis {a}"
                    );
                }
            }
            // The strokes together span the whole box on every axis.
            for a in 0..3 {
                let min = edges
                    .iter()
                    .map(|e| e.offset[a] - e.size[a] * 0.5)
                    .fold(f32::MAX, f32::min);
                let max = edges
                    .iter()
                    .map(|e| e.offset[a] + e.size[a] * 0.5)
                    .fold(f32::MIN, f32::max);
                assert!((min - lo[a]).abs() <= STROKE && (max - hi[a]).abs() <= STROKE);
            }
        }
    }

    #[test]
    fn hammer_swings_on_click_then_repeats_only_while_held() {
        let t0 = Instant::now();
        assert!(!hammer_due(false, false, None, t0));
        assert!(
            hammer_due(true, false, None, t0),
            "a fresh click always swings"
        );
        assert!(
            hammer_due(false, true, None, t0),
            "held with no swing yet swings"
        );
        assert!(!hammer_due(false, true, Some(t0), t0 + HAMMER_REPEAT / 2));
        assert!(hammer_due(false, true, Some(t0), t0 + HAMMER_REPEAT));
        assert!(
            !hammer_due(false, false, Some(t0), t0 + HAMMER_REPEAT * 4),
            "released: no repeat"
        );
    }
}
