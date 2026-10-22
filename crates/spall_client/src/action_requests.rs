//! Builders for the reliable `ActionRequest`s a client sends: a plain cut, or
//! any game tool by ID.

use spall_core::SphereBrush;
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_protocol::{ActionKind, ActionRequest, ClaimedTarget, InputSeq, RequestId};

/// A `Cut` request with a sphere brush centred on `(cell_x, cell_y, cell_z)` in
/// the terrain volume's local cell space.
pub fn cut_request(
    request_id: u64,
    input_seq: u64,
    cell: [i64; 3],
    radius_cells: i64,
) -> ActionRequest {
    tool_request(
        0,
        ActionKind::Cut,
        request_id,
        input_seq,
        cell,
        radius_cells,
    )
}

/// A request for a game-selected, server-approved tool. The server resolves
/// the tool ID to its own operation and validates the aim and brush claims.
pub fn tool_request(
    tool: u16,
    action: ActionKind,
    request_id: u64,
    input_seq: u64,
    cell: [i64; 3],
    radius_cells: i64,
) -> ActionRequest {
    let h = BRUSH_UNIT / 2;
    let brush = SphereBrush::new(
        BrushPoint::from_units(
            cell[0] * BRUSH_UNIT + h,
            cell[1] * BRUSH_UNIT + h,
            cell[2] * BRUSH_UNIT + h,
        ),
        radius_cells * BRUSH_UNIT,
    )
    .expect("radius within brush limits");
    ActionRequest {
        request_id: RequestId(request_id),
        input_seq: InputSeq(input_seq),
        action,
        tool,
        aim_origin_m: [0.0, 1.0, 0.0],
        aim_dir: [0.0, 0.0, 1.0],
        claimed_target: ClaimedTarget::Terrain,
        claimed_brush: brush,
    }
}
