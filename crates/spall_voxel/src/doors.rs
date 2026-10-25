//! Grid-aligned hinged voxel doors. State is a hinge material in ordinary terrain,
//! so edits, replication and saves share one authoritative representation.
use crate::{EditPlan, Sample, Volume};
use spall_core::{GlobalCell, MaterialId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DoorSpec {
    pub closed_hinge: MaterialId,
    pub open_hinge: MaterialId,
    pub panel: MaterialId,
    pub width: i64,
    pub height: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Door {
    pub hinge: GlobalCell,
    pub open: bool,
    pub spec: DoorSpec,
}
impl Door {
    pub fn at(volume: &Volume, hinge: GlobalCell, spec: DoorSpec) -> Option<Self> {
        let open = match volume.sample(hinge).ok()? {
            Sample::Filled(m) if m == spec.closed_hinge => false,
            Sample::Filled(m) if m == spec.open_hinge => true,
            _ => return None,
        };
        Some(Self { hinge, open, spec })
    }
    /// Panel cell `x` along the leaf, `y` up and `d` off the panel plane
    /// (`-1..=1`, for handles). Turning mirrors the leaf: `(x, d)` -> `(d, x)`.
    fn cell(self, x: i64, y: i64, d: i64, open: bool) -> GlobalCell {
        GlobalCell::new(
            self.hinge.x + if open { d } else { x },
            self.hinge.y + y,
            self.hinge.z + if open { x } else { d },
        )
    }
    pub fn center_m(self) -> [f64; 3] {
        let w = self.spec.width as f64 * 0.125;
        [
            self.hinge.x as f64 * 0.25 + if self.open { 0.125 } else { w },
            self.hinge.y as f64 * 0.25 + self.spec.height as f64 * 0.125,
            self.hinge.z as f64 * 0.25 + if self.open { w } else { 0.125 },
        ]
    }
    pub fn swing_bounds_m(self) -> ([f64; 3], [f64; 3]) {
        (
            [
                (self.hinge.x - 1) as f64 * 0.25,
                self.hinge.y as f64 * 0.25,
                (self.hinge.z - 1) as f64 * 0.25,
            ],
            [
                (self.hinge.x + self.spec.width) as f64 * 0.25,
                (self.hinge.y + self.spec.height) as f64 * 0.25,
                (self.hinge.z + self.spec.width) as f64 * 0.25,
            ],
        )
    }
    /// Conservative sweep box: refuse clipping a nearby player during either turn.
    pub fn overlaps_player(self, feet: [f64; 3], radius: f64, height: f64) -> bool {
        let (lo, hi) = self.swing_bounds_m();
        feet[0] + radius > lo[0]
            && feet[0] - radius < hi[0]
            && feet[2] + radius > lo[2]
            && feet[2] - radius < hi[2]
            && feet[1] + height > lo[1]
            && feet[1] < hi[1]
    }
    /// Preserve holes; never refill damage or overwrite foreign material. The
    /// hinge stays at the same cell; the panel turns from +X to +Z and back.
    pub fn plan(self, volume: &Volume) -> Result<EditPlan, &'static str> {
        if Self::at(volume, self.hinge, self.spec) != Some(self) {
            return Err("door changed");
        }
        let mut plan = EditPlan::new(volume.id());
        let mut moved = Vec::new();
        let mut vacated = Vec::new();
        for d in -1..=1 {
            for y in 0..self.spec.height {
                for x in 0..self.spec.width {
                    if x == 0 && y == 0 && d == 0 {
                        continue;
                    }
                    let source = self.cell(x, y, d, self.open);
                    match volume.sample(source) {
                        Ok(Sample::Filled(m)) if m == self.spec.panel => {
                            plan.set(source, MaterialId::AIR);
                            vacated.push(source);
                            let dest = self.cell(x, y, d, !self.open);
                            moved.push(dest);
                        }
                        Ok(Sample::Empty { .. }) => {}
                        // Foreign terrain beside the leaf is not part of the door.
                        Ok(Sample::Filled(_)) if d != 0 => {}
                        _ => return Err("door panel obstructed or unavailable"),
                    }
                }
            }
        }
        // Conservative rectangular sweep, including both endpoint positions. The
        // source plane and the handle cells this turn vacates are the door itself.
        for z in 0..self.spec.width {
            for x in 0..self.spec.width {
                for y in 0..self.spec.height {
                    let cell =
                        GlobalCell::new(self.hinge.x + x, self.hinge.y + y, self.hinge.z + z);
                    let source = if self.open { x == 0 } else { z == 0 };
                    if source || vacated.contains(&cell) {
                        continue;
                    }
                    if !matches!(volume.sample(cell), Ok(Sample::Empty { .. })) {
                        return Err("door swing is blocked");
                    }
                }
            }
        }
        // Every destination is free or is a cell this turn vacates.
        for cell in &moved {
            if !vacated.contains(cell) && !matches!(volume.sample(*cell), Ok(Sample::Empty { .. }))
            {
                return Err("door swing is blocked");
            }
        }
        for cell in moved {
            plan.set(cell, self.spec.panel);
        }
        plan.set(
            self.hinge,
            if self.open {
                self.spec.closed_hinge
            } else {
                self.spec.open_hinge
            },
        );
        Ok(plan)
    }
}

/// Find a visible door within interaction reach, including while it is open.
/// Quarter-metre terrain is the supported door grid.
pub fn nearby_door(volume: &Volume, eye: [f64; 3], spec: DoorSpec, reach: f64) -> Option<Door> {
    if !eye.iter().all(|v| v.is_finite()) || !reach.is_finite() || reach <= 0.0 {
        return None;
    }
    let [ex, ey, ez] = eye.map(|v| (v * 4.0).floor() as i64);
    let r = (reach * 4.0).ceil() as i64 + spec.width;
    let mut best = None;
    let mut distance = reach;
    for z in ez - r..=ez + r {
        for x in ex - r..=ex + r {
            for y in ey - spec.height - r / 2..=ey + r / 2 {
                let Some(door) = Door::at(volume, GlobalCell::new(x, y, z), spec) else {
                    continue;
                };
                let center = glam::DVec3::from_array(door.center_m());
                let origin = glam::DVec3::from_array(eye);
                let d = center.distance(origin);
                if d >= distance {
                    continue;
                }
                let ray = crate::Ray::new(origin / 0.25, center - origin);
                let visible = match crate::query::cast_ray(
                    volume,
                    ray,
                    crate::query::RayConfig::new(d / 0.25),
                ) {
                    Ok(crate::RayOutcome::Hit(hit)) => {
                        let c = hit.cell;
                        c.y >= door.hinge.y
                            && c.y < door.hinge.y + spec.height
                            && if door.open {
                                (c.x - door.hinge.x).abs() <= 1
                                    && c.z >= door.hinge.z
                                    && c.z < door.hinge.z + spec.width
                            } else {
                                (c.z - door.hinge.z).abs() <= 1
                                    && c.x >= door.hinge.x
                                    && c.x < door.hinge.x + spec.width
                            }
                    }
                    Ok(crate::RayOutcome::Miss { .. }) => true,
                    _ => false,
                };
                if visible {
                    best = Some(door);
                    distance = d;
                }
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{CellSizeCode, VolumeId};

    const SPEC: DoorSpec = DoorSpec {
        closed_hinge: MaterialId(218),
        open_hinge: MaterialId(219),
        panel: MaterialId(220),
        width: 6,
        height: 10,
    };

    fn put(volume: &mut Volume, x: i64, y: i64, z: i64, m: MaterialId) {
        let mut plan = EditPlan::new(volume.id());
        plan.set(GlobalCell::new(x, y, z), m);
        volume.apply_edit(&plan).unwrap();
    }
    fn filled(volume: &Volume, x: i64, y: i64, z: i64) -> bool {
        matches!(volume.sample(GlobalCell::new(x, y, z)), Ok(Sample::Filled(m)) if m == SPEC.panel)
    }
    fn turn(volume: &mut Volume, hinge: GlobalCell) -> Result<(), &'static str> {
        let plan = Door::at(volume, hinge, SPEC).unwrap().plan(volume)?;
        volume.apply_edit(&plan).unwrap();
        Ok(())
    }

    #[test]
    fn handle_cells_off_the_panel_plane_round_trip() {
        let mut v = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
        let hinge = GlobalCell::new(10, 4, 20);
        put(&mut v, 10, 4, 20, SPEC.closed_hinge);
        for y in 0..10 {
            for x in 0..6 {
                if (x, y) != (0, 0) {
                    put(&mut v, 10 + x, 4 + y, 20, SPEC.panel);
                }
            }
        }
        // A handle on each face, four cells along and four up.
        put(&mut v, 14, 8, 19, SPEC.panel);
        put(&mut v, 14, 8, 21, SPEC.panel);
        turn(&mut v, hinge).unwrap();
        // Closed (x, d) -> open (d, x): the handles now stand either side of x = 10.
        assert!(filled(&v, 9, 8, 24) && filled(&v, 11, 8, 24));
        assert!(!filled(&v, 14, 8, 19) && !filled(&v, 14, 8, 21));
        turn(&mut v, hinge).unwrap();
        assert!(filled(&v, 14, 8, 19) && filled(&v, 14, 8, 21));
        assert!(!filled(&v, 9, 8, 24) && !filled(&v, 11, 8, 24));
    }

    #[test]
    fn solid_where_a_handle_would_swing_blocks_the_turn() {
        let mut v = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
        let hinge = GlobalCell::new(10, 4, 20);
        put(&mut v, 10, 4, 20, SPEC.closed_hinge);
        put(&mut v, 14, 8, 19, SPEC.panel);
        put(&mut v, 9, 8, 24, MaterialId(1));
        assert!(turn(&mut v, hinge).is_err());
    }
    #[test]
    fn terrain_against_the_swing_face_still_blocks_the_turn() {
        let mut v = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
        let hinge = GlobalCell::new(10, 4, 20);
        put(&mut v, 10, 4, 20, SPEC.closed_hinge);
        // Foreign solid right beside the closed leaf, inside the swing.
        put(&mut v, 13, 6, 21, MaterialId(1));
        assert!(turn(&mut v, hinge).is_err());
    }
}
