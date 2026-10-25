//! Where the Winter Crossing buildings stand (ENG-251): the cabin near the
//! south edge of the map (+Z), the lookout tower near the north edge (-Z,
//! where the compass points), and the woodshed about halfway between them.
//!
//! Each site is chosen by a deterministic search of the generated columns,
//! before plants grow: dry ground clear of water, cave mouths and the border
//! wall, the flattest such spot in the building's band, nearest the band's
//! target on a tie. The building then stands on a flat pad of earth levelled to
//! the highest ground under it, with the air above it cleared and a gentle
//! earth ramp down from it on every side.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use spall_core::{GlobalCell, MaterialId};
use spall_server::InitialWorldItem;
use spall_voxel::{EditPlan, Volume};
use spall_worldgen::{ColumnMap, GeneratedWorld};

use crate::editor_scene::{Prefab, load_prefab};
use crate::game::{items, materials};

const CELL_M: f64 = 0.25;
/// Cells of level pad around a building's own cells.
const PAD_MARGIN: i64 = 3;
/// Cells of air kept clear above a building's highest cell.
const CLEAR_ABOVE: i64 = 4;
/// The most a site's ground may rise and fall under its pad, cells (3 m).
/// The pad is levelled to the highest ground and ramped down to the rest.
const MAX_SITE_RELIEF: i32 = 12;
/// A site keeps at least this many cells from any river or lake (2 m).
const MIN_WATER_CELLS: u8 = 8;
/// A site keeps this many cells beyond its pad from any cave mouth.
const CAVE_MOUTH_CLEARANCE: i64 = 24;
/// Candidate sites are this many cells apart (2 m).
const SEARCH_STEP: i64 = 8;
/// Plants keep this far from the points ([`Layout::kept_clear`]) spread over
/// each pad, cells apart (4 m: the plants' own keep-clear radius).
const KEEP_CLEAR_STEP: i64 = 16;
/// Horizontal radius of the tower's win zone (fixtures/tower/README.md).
pub const WIN_ZONE_RADIUS_M: f64 = 1.0;

/// The three buildings, as authored in their editor projects.
#[derive(Debug, Clone)]
pub struct Buildings {
    pub cabin: Prefab,
    pub tower: Prefab,
    pub shed: Prefab,
}

/// The repository's `fixtures` directory, where the buildings are authored.
pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

impl Buildings {
    /// Loads the cabin, tower and shed projects from `fixtures`.
    pub fn load(fixtures: &Path) -> Result<Self, String> {
        let load = |dir: &str, origin: &str| {
            load_prefab(&fixtures.join(dir), origin).map_err(|e| format!("{dir}: {e}"))
        };
        let buildings = Self {
            cabin: load("cabin", "Timber cabin")?,
            tower: load("tower", "Lookout tower")?,
            shed: load("shed", "Woodshed")?,
        };
        if buildings.cabin.spawns.is_empty() {
            return Err("cabin: no player starts".into());
        }
        for (name, prefab, marker) in [
            ("tower", &buildings.tower, "win_zone"),
            ("shed", &buildings.shed, "axe_spawn"),
        ] {
            if !prefab.markers.contains_key(marker) {
                return Err(format!("{name}: no `{marker}` marker"));
            }
        }
        Ok(buildings)
    }
}

/// Where the tower's top is: a disc on the beacon platform.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WinZone {
    /// Centre of the disc on the platform surface, metres.
    pub centre_m: [f64; 3],
    pub radius_m: f64,
}

impl WinZone {
    /// Whether feet at `feet_m` stand in the zone: within the radius
    /// horizontally, and no more than 0.25 m below or 1 m above the platform.
    pub fn contains(&self, feet_m: [f64; 3]) -> bool {
        let [dx, dy, dz] = [0, 1, 2].map(|i| feet_m[i] - self.centre_m[i]);
        dx * dx + dz * dz <= self.radius_m * self.radius_m && (-0.25..=1.0).contains(&dy)
    }
}

/// Where everything stands in one generated world.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    /// Each building's origin cell (its prefab's cell `[0, 0, 0]`).
    pub cabin: [i64; 3],
    pub tower: [i64; 3],
    pub shed: [i64; 3],
    /// Player starts inside the cabin, feet metres.
    pub spawns: Vec<[f64; 3]>,
    /// The cabin's items where they lie, restored after a death.
    pub cabin_items: Vec<InitialWorldItem>,
    /// The axe on the shed's chopping block.
    pub axe: InitialWorldItem,
    pub win_zone: WinZone,
    /// Compass heading, degrees clockwise from north, from the cabin to the
    /// tower.
    pub tower_heading_deg: f32,
}

/// Compass heading, degrees clockwise from north (-Z; east is +X), of `to`
/// seen from `from`, both metres. `0..360`.
pub fn heading_deg(from: [f64; 3], to: [f64; 3]) -> f32 {
    let (east, north) = (to[0] - from[0], from[2] - to[2]);
    (east.atan2(north).to_degrees().rem_euclid(360.0) as f32) % 360.0
}

/// The columns a building's pad covers, for an origin at `(x, z)`:
/// inclusive `(x0, z0, x1, z1)`.
fn pad(prefab: &Prefab, x: i64, z: i64) -> (i64, i64, i64, i64) {
    (
        x + prefab.min[0] - PAD_MARGIN,
        z + prefab.min[2] - PAD_MARGIN,
        x + prefab.max[0] + PAD_MARGIN,
        z + prefab.max[2] + PAD_MARGIN,
    )
}

/// A search band: candidate origins `x0..x1`, `z0..z1` (cells), nearest
/// `target` first among equally flat sites.
#[derive(Debug, Clone, Copy)]
struct Band {
    x: (i64, i64),
    z: (i64, i64),
    target: (i64, i64),
}

/// The flattest valid origin `(x, z, ground)` in `band` (grown if it has none),
/// where `ground` is the highest ground cell under the pad.
fn find_site(
    columns: &ColumnMap,
    cave_mouths: &[(i64, i64)],
    prefab: &Prefab,
    band: Band,
) -> Option<(i64, i64, i32)> {
    // A band with no valid site grows by half its size each way, a few times.
    (0..4).find_map(|grow| {
        let (dx, dz) = (
            (band.x.1 - band.x.0) / 2 * grow,
            (band.z.1 - band.z.0) / 2 * grow,
        );
        let grown = Band {
            x: (band.x.0 - dx, band.x.1 + dx),
            z: (band.z.0 - dz, band.z.1 + dz),
            target: band.target,
        };
        search_band(columns, cave_mouths, prefab, grown)
    })
}

fn search_band(
    columns: &ColumnMap,
    cave_mouths: &[(i64, i64)],
    prefab: &Prefab,
    band: Band,
) -> Option<(i64, i64, i32)> {
    let mut best: Option<((i32, i64, i64, i64), i32)> = None;
    let mut z = band.z.0;
    while z <= band.z.1 {
        let mut x = band.x.0;
        while x <= band.x.1 {
            if let Some((relief, ground)) = site_relief(columns, cave_mouths, prefab, x, z) {
                let d2 = (x - band.target.0).pow(2) + (z - band.target.1).pow(2);
                let key = (relief, d2, x, z);
                if best.is_none_or(|(b, _)| key < b) {
                    best = Some((key, ground));
                }
            }
            x += SEARCH_STEP;
        }
        z += SEARCH_STEP;
    }
    best.map(|((_, _, x, z), ground)| (x, z, ground))
}

/// `(relief, highest ground)` of the pad at origin `(x, z)`, or `None` if any
/// of it is border, water or near water, too uneven, or near a cave mouth.
fn site_relief(
    columns: &ColumnMap,
    cave_mouths: &[(i64, i64)],
    prefab: &Prefab,
    x: i64,
    z: i64,
) -> Option<(i32, i32)> {
    let (x0, z0, x1, z1) = pad(prefab, x, z);
    let reach = (x1 - x0).max(z1 - z0) / 2 + CAVE_MOUTH_CLEARANCE;
    let (cx, cz) = ((x0 + x1) / 2, (z0 + z1) / 2);
    if cave_mouths
        .iter()
        .any(|&(mx, mz)| (mx - cx).abs().max((mz - cz).abs()) <= reach)
    {
        return None;
    }
    let (mut lo, mut hi) = (i32::MAX, i32::MIN);
    for zz in z0..=z1 {
        for xx in x0..=x1 {
            if columns.in_border(xx, zz)
                || columns.is_water(xx, zz)
                || columns.river_dist(xx, zz) < MIN_WATER_CELLS
            {
                return None;
            }
            let h = columns.height(xx, zz);
            lo = lo.min(h);
            hi = hi.max(h);
            if hi - lo > MAX_SITE_RELIEF {
                return None;
            }
        }
    }
    Some((hi - lo, hi))
}

/// Chooses the sites. Deterministic in the columns (so in preset, seed and
/// size). Fails if a building finds no valid ground in its band.
pub fn plan(
    columns: &ColumnMap,
    cave_mouths: &[(i64, i64)],
    buildings: &Buildings,
) -> Result<Layout, String> {
    let size = i64::from(columns.size());
    let at = |share: f64| (share * size as f64) as i64;
    let cabin = find_site(
        columns,
        cave_mouths,
        &buildings.cabin,
        Band {
            x: (at(0.25), at(0.75)),
            z: (at(0.80), at(0.92)),
            target: (at(0.5), at(0.86)),
        },
    )
    .ok_or("no dry, level ground for the cabin near the south edge")?;
    let tower = find_site(
        columns,
        cave_mouths,
        &buildings.tower,
        Band {
            x: (at(0.25), at(0.75)),
            z: (at(0.06), at(0.18)),
            // Due north of the cabin if the ground allows.
            target: (cabin.0, at(0.12)),
        },
    )
    .ok_or("no dry, level ground for the tower near the north edge")?;
    let middle = ((cabin.0 + tower.0) / 2, (cabin.1 + tower.1) / 2);
    let spread = at(0.08);
    let shed = find_site(
        columns,
        cave_mouths,
        &buildings.shed,
        Band {
            x: (middle.0 - spread, middle.0 + spread),
            z: (middle.1 - spread, middle.1 + spread),
            target: middle,
        },
    )
    .ok_or("no dry, level ground for the shed between the cabin and the tower")?;
    let origin = |(x, z, ground): (i64, i64, i32)| [x, i64::from(ground) + 1, z];
    let (cabin, tower, shed) = (origin(cabin), origin(tower), origin(shed));
    let metres =
        |cell: [i64; 3], rel: [f64; 3]| [0, 1, 2].map(|i| cell[i] as f64 * CELL_M + rel[i]);
    let centre = |cell: [i64; 3], p: &Prefab| {
        metres(
            cell,
            [0, 1, 2].map(|i| (p.min[i] + p.max[i] + 1) as f64 * 0.5 * CELL_M),
        )
    };
    let placed = |cell: [i64; 3], item: &InitialWorldItem| InitialWorldItem {
        translation_m: metres(cell, item.translation_m),
        ..*item
    };
    Ok(Layout {
        spawns: buildings
            .cabin
            .spawns
            .iter()
            .map(|s| metres(cabin, *s))
            .collect(),
        cabin_items: buildings
            .cabin
            .items
            .iter()
            .map(|item| placed(cabin, item))
            .collect(),
        axe: InitialWorldItem {
            kind: items::AXE.0,
            translation_m: metres(shed, buildings.shed.markers["axe_spawn"]),
            yaw_rad: 0.0,
        },
        win_zone: WinZone {
            centre_m: metres(tower, buildings.tower.markers["win_zone"]),
            radius_m: WIN_ZONE_RADIUS_M,
        },
        tower_heading_deg: heading_deg(
            centre(cabin, &buildings.cabin),
            centre(tower, &buildings.tower),
        ),
        cabin,
        tower,
        shed,
    })
}

impl Layout {
    /// Stamps the buildings into `terrain`: each on a level earth pad, with
    /// the air above it cleared. Returns the cells plants keep clear of.
    pub fn build(
        &self,
        terrain: &mut Volume,
        columns: &ColumnMap,
        region_top: i64,
        buildings: &Buildings,
    ) -> Result<Vec<[i64; 3]>, String> {
        let mut kept_clear = Vec::new();
        for (name, origin, prefab) in [
            ("cabin", self.cabin, &buildings.cabin),
            ("tower", self.tower, &buildings.tower),
            ("shed", self.shed, &buildings.shed),
        ] {
            let top = origin[1] + prefab.max[1] + CLEAR_ABOVE;
            if top > region_top {
                return Err(format!("the {name} would rise above the world"));
            }
            let mut cells: HashMap<(i64, i64, i64), MaterialId> = HashMap::new();
            let (x0, z0, x1, z1) = pad(prefab, origin[0], origin[2]);
            let skirt = 2 * i64::from(MAX_SITE_RELIEF);
            for z in z0 - skirt..=z1 + skirt {
                for x in x0 - skirt..=x1 + skirt {
                    let ground = i64::from(columns.height(x, z));
                    // Columns away from the pad get a 1-in-2 earth ramp up to
                    // it, so the pad has no step to climb.
                    let away = (x0 - x).max(x - x1).max(z0 - z).max(z - z1).max(0);
                    if away > 0 {
                        if columns.is_water(x, z) || columns.in_border(x, z) {
                            continue;
                        }
                        for y in ground + 1..origin[1] - (away + 1) / 2 {
                            cells.insert((x, y, z), materials::DIRT);
                        }
                        continue;
                    }
                    for y in (ground + 1).min(origin[1])..=top {
                        let fill = if y < origin[1] {
                            materials::DIRT
                        } else {
                            materials::AIR
                        };
                        cells.insert((x, y, z), fill);
                    }
                }
            }
            for &(c, material) in &prefab.cells {
                cells.insert(
                    (origin[0] + c[0], origin[1] + c[1], origin[2] + c[2]),
                    material,
                );
            }
            let id = terrain.id();
            let mut plan = EditPlan::new(id);
            for ((x, y, z), material) in cells {
                plan.set(GlobalCell::new(x, y, z), material);
            }
            terrain
                .apply_edit(&plan)
                .map_err(|e| format!("could not build the {name}: {e}"))?;
            let mut z = z0 - 8;
            while z <= z1 + 8 {
                let mut x = x0 - 8;
                while x <= x1 + 8 {
                    kept_clear.push([x, origin[1], z]);
                    x += KEEP_CLEAR_STEP;
                }
                z += KEEP_CLEAR_STEP;
            }
        }
        Ok(kept_clear)
    }
}

/// Lays the scenario out in a freshly generated world before plants grow (the
/// [`generate_with`](crate::worldgen_scene::generate_with) build step).
/// Returns the layout and the cells plants keep clear of.
pub fn lay_out(
    world: &mut GeneratedWorld,
    buildings: &Buildings,
) -> Result<(Layout, Vec<[i64; 3]>), String> {
    let layout = plan(&world.columns, &world.cave_mouths, buildings)?;
    let kept_clear = layout.build(
        &mut world.terrain,
        &world.columns,
        world.region.1.y,
        buildings,
    )?;
    Ok((layout, kept_clear))
}
