//! Bounded, current-state checks for animal spawn/relocation candidates.
//!
//! This validates an axis-aligned envelope against one immutable terrain view
//! and (for aquatic animals) a live water query. Call it again after terrain
//! edits or water steps; biome and generation data are deliberately absent.

use glam::DVec3;
use spall_core::{CellSizeCode, GlobalCell};
use spall_fluid::columns::{ColumnWorld, SpanView};
use spall_voxel::{Sample, Volume};

const TERRAIN_CELL_M: f64 = 0.25;
const MAX_ALLOWED_WORK: usize = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimalHabitat {
    Aquatic,
    Land,
    Flying,
}

/// Centre and full extents in world metres. The envelope is axis aligned.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CandidateEnvelope {
    pub center_m: DVec3,
    pub size_m: DVec3,
}

/// A current water span in global terrain-cell units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WaterSpan {
    pub floor_y: i64,
    pub depth_cells: f64,
}

/// Implemented by the authoritative live water state; implementations must
/// answer from the current step, not generated-world or biome metadata.
pub trait CurrentWaterQuery {
    /// Visits at most `max_spans`, returning false when the column exceeds the
    /// bound. Implementations must check the bound before allocating or visiting.
    fn visit_spans_in_column(
        &self,
        x: i64,
        z: i64,
        max_spans: usize,
        visit: &mut dyn FnMut(WaterSpan),
    ) -> bool;
}

impl CurrentWaterQuery for ColumnWorld {
    fn visit_spans_in_column(
        &self,
        x: i64,
        z: i64,
        max_spans: usize,
        visit: &mut dyn FnMut(WaterSpan),
    ) -> bool {
        ColumnWorld::visit_spans_in_column(self, x, z, max_spans, |span: SpanView| {
            visit(WaterSpan {
                floor_y: span.floor_y,
                depth_cells: span.depth,
            })
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ValidationLimits {
    /// Maximum terrain cells sampled plus water spans inspected.
    pub max_work: usize,
    /// Placement tolerance below the envelope for finding support. The default
    /// is zero; nonzero values are only for fitting an axis-aligned envelope to
    /// uneven ground and must not be used to authorize live floating spawns.
    pub max_support_drop_cells: u32,
    /// Maximum height variation between support points under the footprint.
    pub max_support_height_delta_m: f64,
    /// Required water depth below the candidate envelope, in metres.
    pub aquatic_min_depth_m: f64,
}

impl Default for ValidationLimits {
    fn default() -> Self {
        Self {
            max_work: 16_384,
            max_support_drop_cells: 0,
            max_support_height_delta_m: 0.5,
            aquatic_min_depth_m: 0.5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    InvalidEnvelope,
    UnsupportedCellSize,
    OutsideCellRange,
    WorkBudgetExceeded,
    TerrainUnknown,
    TerrainOccupied,
    MissingSupport,
    ExcessiveSlopeOrStep,
    WaterUnavailable,
    WaterTooShallow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidatedCandidate {
    pub terrain_state_stamp: u64,
}

/// Checks current terrain occupancy and category-specific support or water.
/// The returned stamp is informational only; callers must re-run this query
/// against their newly committed terrain/water snapshots before using a
/// candidate after either state changes.
pub fn validate_candidate(
    terrain: &Volume,
    water: Option<&dyn CurrentWaterQuery>,
    habitat: AnimalHabitat,
    candidate: CandidateEnvelope,
    limits: ValidationLimits,
) -> Result<ValidatedCandidate, RejectReason> {
    if terrain.cell_size() != CellSizeCode::Quarter {
        return Err(RejectReason::UnsupportedCellSize);
    }
    if !candidate.center_m.is_finite()
        || !candidate.size_m.is_finite()
        || candidate.size_m.min_element() <= 0.0
        || !limits.max_support_height_delta_m.is_finite()
        || limits.max_support_height_delta_m < 0.0
        || !limits.aquatic_min_depth_m.is_finite()
        || limits.aquatic_min_depth_m < 0.0
        || limits.max_work == 0
    {
        return Err(RejectReason::InvalidEnvelope);
    }
    let max_work = limits.max_work.min(MAX_ALLOWED_WORK);
    let half = candidate.size_m * 0.5;
    let min = candidate.center_m - half;
    let max = candidate.center_m + half;
    if !min.is_finite() || !max.is_finite() {
        return Err(RejectReason::InvalidEnvelope);
    }
    let bounds = [
        cell_range(min.x, max.x)?,
        cell_range(min.y, max.y)?,
        cell_range(min.z, max.z)?,
    ];
    let counts = bounds.map(|(lo, hi)| {
        hi.checked_sub(lo)
            .and_then(|n| n.checked_add(1))
            .and_then(|n| usize::try_from(n).ok())
            .ok_or(RejectReason::WorkBudgetExceeded)
    });
    let counts = [counts[0]?, counts[1]?, counts[2]?];
    let volume_cells = counts[0]
        .checked_mul(counts[1])
        .and_then(|v| v.checked_mul(counts[2]))
        .ok_or(RejectReason::WorkBudgetExceeded)?;
    let footprint = counts[0]
        .checked_mul(counts[2])
        .ok_or(RejectReason::WorkBudgetExceeded)?;
    if volume_cells > max_work || footprint > max_work {
        return Err(RejectReason::WorkBudgetExceeded);
    }

    let mut work = 0usize;
    for z in bounds[2].0..=bounds[2].1 {
        for y in bounds[1].0..=bounds[1].1 {
            for x in bounds[0].0..=bounds[0].1 {
                charge(&mut work, max_work)?;
                match terrain.sample(GlobalCell::new(x, y, z)) {
                    Ok(Sample::Empty { .. }) => {}
                    Ok(Sample::Filled(_)) => return Err(RejectReason::TerrainOccupied),
                    Ok(Sample::Unknown(_)) | Err(_) => return Err(RejectReason::TerrainUnknown),
                }
            }
        }
    }

    match habitat {
        AnimalHabitat::Flying => {}
        AnimalHabitat::Land => {
            let bottom_cell = floor_i64((min.y / TERRAIN_CELL_M).ceil() - 1.0)?;
            let drop = i64::from(limits.max_support_drop_cells);
            let mut support_min = f64::INFINITY;
            let mut support_max = f64::NEG_INFINITY;
            for z in bounds[2].0..=bounds[2].1 {
                for x in bounds[0].0..=bounds[0].1 {
                    let mut found = None;
                    for offset in 0..=drop {
                        let y = bottom_cell
                            .checked_sub(offset)
                            .ok_or(RejectReason::OutsideCellRange)?;
                        charge(&mut work, max_work)?;
                        match terrain.sample(GlobalCell::new(x, y, z)) {
                            Ok(Sample::Filled(_)) => {
                                found = Some((y + 1) as f64 * TERRAIN_CELL_M);
                                break;
                            }
                            Ok(Sample::Empty { .. }) => {}
                            Ok(Sample::Unknown(_)) | Err(_) => {
                                return Err(RejectReason::TerrainUnknown);
                            }
                        }
                    }
                    let height = found.ok_or(RejectReason::MissingSupport)?;
                    support_min = support_min.min(height);
                    support_max = support_max.max(height);
                }
            }
            if support_max - support_min > limits.max_support_height_delta_m {
                return Err(RejectReason::ExcessiveSlopeOrStep);
            }
        }
        AnimalHabitat::Aquatic => {
            let water = water.ok_or(RejectReason::WaterUnavailable)?;
            let min_y = min.y / TERRAIN_CELL_M;
            let max_y = max.y / TERRAIN_CELL_M;
            let required_below = limits.aquatic_min_depth_m / TERRAIN_CELL_M;
            for z in bounds[2].0..=bounds[2].1 {
                for x in bounds[0].0..=bounds[0].1 {
                    // Charge even for dry/unallocated columns. The visitor checks
                    // its span limit before any per-column allocation or scan.
                    charge(&mut work, max_work)?;
                    let remaining = max_work - work;
                    let mut contains = false;
                    let mut over_budget = false;
                    let within_span_budget =
                        water.visit_spans_in_column(x, z, remaining, &mut |span| {
                            if work < max_work {
                                work += 1;
                            } else {
                                over_budget = true;
                                return;
                            }
                            let surface_y = span.floor_y as f64 + span.depth_cells;
                            contains |= span.depth_cells.is_finite()
                                && span.depth_cells > 0.02
                                && surface_y.is_finite()
                                && min_y >= span.floor_y as f64
                                && min_y - span.floor_y as f64 >= required_below
                                && max_y <= surface_y;
                        });
                    if !within_span_budget || over_budget {
                        return Err(RejectReason::WorkBudgetExceeded);
                    }
                    if !contains {
                        return Err(RejectReason::WaterTooShallow);
                    }
                }
            }
        }
    }

    Ok(ValidatedCandidate {
        terrain_state_stamp: terrain.state_stamp(),
    })
}

fn charge(work: &mut usize, max_work: usize) -> Result<(), RejectReason> {
    *work = work
        .checked_add(1)
        .ok_or(RejectReason::WorkBudgetExceeded)?;
    if *work > max_work {
        Err(RejectReason::WorkBudgetExceeded)
    } else {
        Ok(())
    }
}

fn cell_range(min_m: f64, max_m: f64) -> Result<(i64, i64), RejectReason> {
    let lo = floor_i64((min_m / TERRAIN_CELL_M).floor())?;
    let hi = floor_i64((max_m / TERRAIN_CELL_M).ceil() - 1.0)?;
    if hi < lo {
        return Err(RejectReason::InvalidEnvelope);
    }
    Ok((lo, hi))
}

fn floor_i64(value: f64) -> Result<i64, RejectReason> {
    const LIMIT: f64 = 4.611_686_018_427_388e18;
    if !value.is_finite() || value.abs() > LIMIT {
        Err(RejectReason::OutsideCellRange)
    } else {
        Ok(value as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{BrickCoord, MaterialId, Revision, VolumeId};
    use spall_voxel::{Brick, EditPlan};

    struct Water {
        spans: Vec<WaterSpan>,
        dry_column: Option<(i64, i64)>,
    }

    impl CurrentWaterQuery for Water {
        fn visit_spans_in_column(
            &self,
            x: i64,
            z: i64,
            max_spans: usize,
            visit: &mut dyn FnMut(WaterSpan),
        ) -> bool {
            if self.dry_column == Some((x, z)) {
                return true;
            }
            if self.spans.len() > max_spans {
                return false;
            }
            for &span in &self.spans {
                visit(span);
            }
            true
        }
    }

    fn water(depth_cells: f64) -> Water {
        Water {
            spans: vec![WaterSpan {
                floor_y: 0,
                depth_cells,
            }],
            dry_column: None,
        }
    }

    fn empty_terrain() -> Volume {
        let mut volume = Volume::new(VolumeId::new(41).unwrap(), CellSizeCode::Quarter);
        volume
            .insert_brick(
                BrickCoord::new(0, 0, 0),
                Brick::uniform(MaterialId::AIR, Revision(1)),
            )
            .unwrap();
        volume
    }

    fn envelope(center: DVec3, size: DVec3) -> CandidateEnvelope {
        CandidateEnvelope {
            center_m: center,
            size_m: size,
        }
    }

    #[test]
    fn checks_full_volume_edges_and_rejects_unknown_terrain() {
        let mut terrain = empty_terrain();
        let mut edit = EditPlan::new(terrain.id());
        edit.set(GlobalCell::new(5, 8, 4), MaterialId(1));
        terrain.apply_edit(&edit).unwrap();
        let near_edge = envelope(DVec3::new(1.25, 2.0, 1.0), DVec3::splat(0.5));
        assert_eq!(
            validate_candidate(
                &terrain,
                None,
                AnimalHabitat::Flying,
                near_edge,
                ValidationLimits::default()
            ),
            Err(RejectReason::TerrainOccupied)
        );
        let outside_resident = envelope(DVec3::new(8.1, 2.0, 1.0), DVec3::splat(0.2));
        assert_eq!(
            validate_candidate(
                &terrain,
                None,
                AnimalHabitat::Flying,
                outside_resident,
                ValidationLimits::default()
            ),
            Err(RejectReason::TerrainUnknown)
        );
    }

    #[test]
    fn aquatic_requires_current_containing_water_and_depth() {
        let terrain = empty_terrain();
        let candidate = envelope(DVec3::new(1.0, 2.0, 1.0), DVec3::splat(0.5));
        let deep = water(16.0);
        assert!(
            validate_candidate(
                &terrain,
                Some(&deep),
                AnimalHabitat::Aquatic,
                candidate,
                ValidationLimits::default()
            )
            .is_ok()
        );
        let shallow = water(8.5);
        assert_eq!(
            validate_candidate(
                &terrain,
                Some(&shallow),
                AnimalHabitat::Aquatic,
                candidate,
                ValidationLimits::default()
            ),
            Err(RejectReason::WaterTooShallow)
        );
        let drained = water(0.0);
        assert_eq!(
            validate_candidate(
                &terrain,
                Some(&drained),
                AnimalHabitat::Aquatic,
                candidate,
                ValidationLimits::default()
            ),
            Err(RejectReason::WaterTooShallow)
        );
        let near_floor = envelope(DVec3::new(1.0, 0.625, 1.0), DVec3::splat(0.5));
        assert_eq!(
            validate_candidate(
                &terrain,
                Some(&deep),
                AnimalHabitat::Aquatic,
                near_floor,
                ValidationLimits::default()
            ),
            Err(RejectReason::WaterTooShallow)
        );
        let near_surface = envelope(DVec3::new(1.0, 3.5, 1.0), DVec3::splat(0.5));
        assert!(
            validate_candidate(
                &terrain,
                Some(&deep),
                AnimalHabitat::Aquatic,
                near_surface,
                ValidationLimits::default()
            )
            .is_ok()
        );
    }

    #[test]
    fn aquatic_requires_water_under_every_horizontal_footprint_column() {
        let terrain = empty_terrain();
        let candidate = envelope(DVec3::new(1.0, 2.0, 1.0), DVec3::splat(0.5));
        let mut edge_dry = water(16.0);
        edge_dry.dry_column = Some((4, 3));
        assert_eq!(
            validate_candidate(
                &terrain,
                Some(&edge_dry),
                AnimalHabitat::Aquatic,
                candidate,
                ValidationLimits::default()
            ),
            Err(RejectReason::WaterTooShallow)
        );
    }

    #[test]
    fn aquatic_water_span_scan_obeys_remaining_work_budget() {
        let terrain = empty_terrain();
        let candidate = envelope(DVec3::new(1.0, 2.0, 1.0), DVec3::splat(0.5));
        let many_spans = Water {
            spans: vec![
                WaterSpan {
                    floor_y: 0,
                    depth_cells: 16.0
                };
                20
            ],
            dry_column: None,
        };
        let limits = ValidationLimits {
            max_work: 12,
            ..ValidationLimits::default()
        };
        assert_eq!(
            validate_candidate(
                &terrain,
                Some(&many_spans),
                AnimalHabitat::Aquatic,
                candidate,
                limits
            ),
            Err(RejectReason::WorkBudgetExceeded)
        );
    }

    #[test]
    fn land_requires_support_and_rejects_large_height_steps() {
        let mut flat = Volume::new(VolumeId::new(42).unwrap(), CellSizeCode::Quarter);
        let mut edit = EditPlan::filled_box(
            flat.id(),
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(31, 31, 31),
            MaterialId(1),
        );
        edit.set(GlobalCell::new(0, 32, 0), MaterialId::AIR);
        flat.apply_edit(&edit).unwrap();
        flat.insert_brick(
            BrickCoord::new(0, 1, 0),
            Brick::uniform(MaterialId::AIR, Revision(1)),
        )
        .unwrap();
        let candidate = envelope(DVec3::new(1.0, 8.25, 1.0), DVec3::new(0.5, 0.5, 0.5));
        assert!(
            validate_candidate(
                &flat,
                None,
                AnimalHabitat::Land,
                candidate,
                ValidationLimits::default()
            )
            .is_ok()
        );
        let floating = envelope(DVec3::new(1.0, 8.35, 1.0), DVec3::new(0.5, 0.5, 0.5));
        assert_eq!(
            validate_candidate(
                &flat,
                None,
                AnimalHabitat::Land,
                floating,
                ValidationLimits::default()
            ),
            Err(RejectReason::MissingSupport)
        );
        let removed = EditPlan::filled_box(
            flat.id(),
            GlobalCell::new(0, 27, 0),
            GlobalCell::new(31, 31, 31),
            MaterialId::AIR,
        );
        flat.apply_edit(&removed).unwrap();
        assert_eq!(
            validate_candidate(
                &flat,
                None,
                AnimalHabitat::Land,
                candidate,
                ValidationLimits::default()
            ),
            Err(RejectReason::MissingSupport)
        );
    }

    #[test]
    fn steep_support_change_is_rejected() {
        let mut terrain = Volume::new(VolumeId::new(43).unwrap(), CellSizeCode::Quarter);
        terrain
            .insert_brick(
                BrickCoord::new(0, 0, 0),
                Brick::uniform(MaterialId(1), Revision(1)),
            )
            .unwrap();
        terrain
            .insert_brick(
                BrickCoord::new(0, 1, 0),
                Brick::uniform(MaterialId::AIR, Revision(1)),
            )
            .unwrap();
        let mut trench = EditPlan::new(terrain.id());
        for y in 28..=31 {
            for z in 4..=5 {
                trench.set(GlobalCell::new(4, y, z), MaterialId::AIR);
            }
        }
        terrain.apply_edit(&trench).unwrap();
        let candidate = envelope(DVec3::new(1.25, 8.25, 1.25), DVec3::new(0.5, 0.5, 0.5));
        let strict = ValidationLimits {
            max_support_drop_cells: 4,
            max_support_height_delta_m: 0.25,
            ..ValidationLimits::default()
        };
        assert_eq!(
            validate_candidate(&terrain, None, AnimalHabitat::Land, candidate, strict),
            Err(RejectReason::ExcessiveSlopeOrStep)
        );
    }

    #[test]
    fn candidate_work_is_bounded_and_invalid_dimensions_are_rejected() {
        let terrain = empty_terrain();
        let large = envelope(DVec3::new(1.0, 1.0, 1.0), DVec3::splat(20.0));
        assert_eq!(
            validate_candidate(
                &terrain,
                None,
                AnimalHabitat::Flying,
                large,
                ValidationLimits::default()
            ),
            Err(RejectReason::WorkBudgetExceeded)
        );
        let small = envelope(DVec3::new(1.0, 1.0, 1.0), DVec3::splat(0.25));
        let tiny_budget = ValidationLimits {
            max_work: 1,
            ..ValidationLimits::default()
        };
        assert_eq!(
            validate_candidate(&terrain, None, AnimalHabitat::Flying, small, tiny_budget),
            Err(RejectReason::WorkBudgetExceeded)
        );
        let invalid = envelope(DVec3::ZERO, DVec3::new(f64::NAN, 1.0, 1.0));
        assert_eq!(
            validate_candidate(
                &terrain,
                None,
                AnimalHabitat::Flying,
                invalid,
                ValidationLimits::default()
            ),
            Err(RejectReason::InvalidEnvelope)
        );
        let overflow = envelope(DVec3::splat(1.7e308), DVec3::splat(1.7e308));
        assert_eq!(
            validate_candidate(
                &terrain,
                None,
                AnimalHabitat::Flying,
                overflow,
                ValidationLimits::default()
            ),
            Err(RejectReason::InvalidEnvelope)
        );
        let finite_out_of_range = envelope(DVec3::new(1.0e100, 1.0, 1.0), DVec3::splat(0.25));
        assert_eq!(
            validate_candidate(
                &terrain,
                None,
                AnimalHabitat::Flying,
                finite_out_of_range,
                ValidationLimits::default()
            ),
            Err(RejectReason::OutsideCellRange)
        );
    }
}
