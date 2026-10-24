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
    fn cell(self, x: i64, y: i64, open: bool) -> GlobalCell {
        GlobalCell::new(
            self.hinge.x + if open { 0 } else { x },
            self.hinge.y + y,
            self.hinge.z + if open { x } else { 0 },
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
                self.hinge.x as f64 * 0.25,
                self.hinge.y as f64 * 0.25,
                self.hinge.z as f64 * 0.25,
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
        for y in 0..self.spec.height {
            for x in 0..self.spec.width {
                if x == 0 && y == 0 {
                    continue;
                }
                let source = self.cell(x, y, self.open);
                match volume.sample(source) {
                    Ok(Sample::Filled(m)) if m == self.spec.panel => {
                        plan.set(source, MaterialId::AIR);
                        moved.push(self.cell(x, y, !self.open));
                    }
                    Ok(Sample::Empty { .. }) => {}
                    _ => return Err("door panel obstructed or unavailable"),
                }
            }
        }
        // Conservative rectangular sweep, including both endpoint positions.
        for z in 0..self.spec.width {
            for x in 0..self.spec.width {
                for y in 0..self.spec.height {
                    let cell =
                        GlobalCell::new(self.hinge.x + x, self.hinge.y + y, self.hinge.z + z);
                    let source = if self.open { x == 0 } else { z == 0 };
                    if source {
                        continue;
                    }
                    if !matches!(volume.sample(cell), Ok(Sample::Empty { .. })) {
                        return Err("door swing is blocked");
                    }
                }
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
                                c.x == door.hinge.x
                                    && c.z >= door.hinge.z
                                    && c.z < door.hinge.z + spec.width
                            } else {
                                c.z == door.hinge.z
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
