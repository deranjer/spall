//! Explicit f64 world to f32 local-physics coordinate conversion.

use crate::occupancy::OccupancyGrid;

/// World-space origin represented by a local physics frame.
///
/// Authoritative positions remain world-space `f64`; only the solver-facing
/// translation is localized and narrowed to `f32`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PhysicsOrigin {
    world_m: [f64; 3],
}

impl PhysicsOrigin {
    pub const ZERO: Self = Self { world_m: [0.0; 3] };

    /// Creates a frame at a finite world-space position.
    pub fn new(world_m: [f64; 3]) -> Option<Self> {
        world_m
            .iter()
            .all(|value| value.is_finite())
            .then_some(Self { world_m })
    }

    pub fn world_m(self) -> [f64; 3] {
        self.world_m
    }

    /// Converts an authoritative position to a finite local f32 translation.
    pub fn to_local_f32(self, world_position_m: [f64; 3]) -> Option<[f32; 3]> {
        let local = std::array::from_fn(|axis| world_position_m[axis] - self.world_m[axis]);
        local.iter().all(|value| value.is_finite()).then_some(())?;
        let narrowed = local.map(|value| value as f32);
        narrowed
            .iter()
            .all(|value| value.is_finite())
            .then_some(narrowed)
    }

    /// Converts an authoritative position to a local query position without
    /// narrowing it before the physics adapter does so.
    pub fn to_local_f64(self, world_position_m: [f64; 3]) -> Option<[f64; 3]> {
        let local = std::array::from_fn(|axis| world_position_m[axis] - self.world_m[axis]);
        local.iter().all(|value| value.is_finite()).then_some(local)
    }

    /// Converts a local solver position back to authoritative world space.
    pub fn to_world_f64(self, local_position_m: [f32; 3]) -> [f64; 3] {
        std::array::from_fn(|axis| self.world_m[axis] + f64::from(local_position_m[axis]))
    }

    /// Rebases a terrain grid's global cell origin near this physics frame.
    /// Returns the localized grid and its small residual body translation.
    /// Dynamic-body grids are body-local and must not use this method.
    pub fn localize_terrain_grid(
        self,
        grid: OccupancyGrid,
        cell_m: f64,
    ) -> Option<(OccupancyGrid, [f32; 3])> {
        let (whole_origin_cells, residual) = self.terrain_grid_frame(cell_m)?;
        let shift = grid.rebase_origin_by_cells(whole_origin_cells)?;
        Some((shift, residual))
    }

    /// Solver translation to use with any terrain grid localized at `cell_m`.
    pub fn terrain_translation(self, cell_m: f64) -> Option<[f32; 3]> {
        self.terrain_grid_frame(cell_m)
            .map(|(_, residual)| residual)
    }

    fn terrain_grid_frame(self, cell_m: f64) -> Option<([i64; 3], [f32; 3])> {
        if !cell_m.is_finite() || cell_m <= 0.0 {
            return None;
        }
        let whole = self.world_m.map(|axis| (axis / cell_m).floor());
        if whole
            .iter()
            .any(|axis| !axis.is_finite() || *axis < i64::MIN as f64 || *axis > i64::MAX as f64)
        {
            return None;
        }
        let whole = whole.map(|axis| axis as i64);
        let residual =
            std::array::from_fn(|axis| (whole[axis] as f64 * cell_m - self.world_m[axis]) as f32);
        residual
            .iter()
            .all(|value| value.is_finite())
            .then_some((whole, residual))
    }
}
