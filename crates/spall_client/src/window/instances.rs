use super::*;

/// Cube instances (one per solid cell within the terrain-mesh cull window) for
/// the terrain around `center_m`. Public alongside `rebuild_pass` for the same
/// reason: instrumentation that needs the real geometry-building pipeline
/// without a background worker.
pub fn build_instances(volume: &Volume, center_m: [f64; 3]) -> Vec<Instance> {
    build_instances_cached(volume, center_m, &mut TerrainInstanceCache::new())
}

/// Per-brick results of [`build_instances_cached`], keyed by the brick's own
/// revision and its six neighbours' (the buried test reads their faces). An edit changes one brick's revision,
/// so the next rebuild recomputes that brick and its neighbours and reuses the
/// rest: a hammer swing costs milliseconds, not a full window walk.
pub struct TerrainInstanceCache {
    bricks: std::collections::HashMap<spall_core::BrickCoord, (u64, Vec<Instance>)>,
    grid: BrickGrid,
}

impl TerrainInstanceCache {
    pub fn new() -> Self {
        Self {
            bricks: std::collections::HashMap::new(),
            grid: BrickGrid::new(),
        }
    }

    /// Bricks currently cached (tests and diagnostics).
    pub fn len(&self) -> usize {
        self.bricks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bricks.is_empty()
    }
}

impl Default for TerrainInstanceCache {
    fn default() -> Self {
        Self::new()
    }
}

/// The inclusive cell window the terrain draw covers around `center_m`,
/// widened to whole bricks: a brick is drawn entirely or not at all, so its
/// cached boxes do not depend on where the window's edge falls and walking does
/// not invalidate every brick along the edge each metre.
pub(super) fn view_window(center_m: [f64; 3]) -> (GlobalCell, GlobalCell) {
    let cell_m = f64::from(CELL_M);
    let center_cell = GlobalCell::new(
        (center_m[0] / cell_m).floor() as i64,
        (center_m[1] / cell_m).floor() as i64,
        (center_m[2] / cell_m).floor() as i64,
    );
    let horiz = (view_radius_m() / CELL_M).ceil() as i64;
    let up = (VIEW_HEIGHT_UP_M / CELL_M).ceil() as i64;
    let down = (VIEW_HEIGHT_DOWN_M / CELL_M).ceil() as i64;
    let edge = i64::from(spall_core::BRICK_EDGE);
    let floor = |v: i64| v.div_euclid(edge) * edge;
    let ceil = |v: i64| v.div_euclid(edge) * edge + edge - 1;
    (
        GlobalCell::new(
            floor(center_cell.x - horiz),
            floor(center_cell.y - down),
            floor(center_cell.z - horiz),
        ),
        GlobalCell::new(
            ceil(center_cell.x + horiz),
            ceil(center_cell.y + up),
            ceil(center_cell.z + horiz),
        ),
    )
}

/// A hammer swing the client has sent but the server has not yet confirmed. It
/// is drawn at once on a private copy of the terrain so a carved trench appears
/// at the crosshair, not a round trip behind it, and dropped when the replica
/// shows the cut or after [`PENDING_CUT_LIFETIME`] (a refused swing disappears).
/// The replica itself is never touched, so integrity hashes are unaffected.
#[derive(Debug, Clone, Copy)]
pub struct PendingCut {
    pub cell: GlobalCell,
    pub radius_cells: i64,
    pub at: Instant,
}

/// How long an unconfirmed swing stays drawn.
pub(super) const PENDING_CUT_LIFETIME: Duration = Duration::from_millis(800);
/// Most previews kept at once.
pub(super) const MAX_PENDING_CUTS: usize = 64;

/// Drops previews the replica already shows (their centre cell is air in
/// `volume`) or that have expired, then carves the rest out of `volume`.
/// Returns the bricks whose boxes must not be cached: every brick a preview
/// changed and its face neighbours (their buried tests read those faces).
pub fn apply_pending_cuts(
    volume: &mut Volume,
    pending: &mut Vec<PendingCut>,
    now: Instant,
) -> std::collections::HashSet<spall_core::BrickCoord> {
    pending.retain(|p| {
        now.saturating_duration_since(p.at) < PENDING_CUT_LIFETIME
            && !matches!(volume.sample(p.cell), Ok(Sample::Empty { .. }))
    });
    let mut volatile = std::collections::HashSet::new();
    for p in pending.iter() {
        let r = p.radius_cells.clamp(0, HAMMER_RADIUS_MAX);
        let mut plan = spall_voxel::EditPlan::new(volume.id());
        for dz in -r..=r {
            for dy in -r..=r {
                for dx in -r..=r {
                    if dx * dx + dy * dy + dz * dz > r * r {
                        continue;
                    }
                    let cell = GlobalCell::new(p.cell.x + dx, p.cell.y + dy, p.cell.z + dz);
                    if matches!(volume.sample(cell), Ok(Sample::Filled(_))) {
                        plan.set(cell, MaterialId::AIR);
                    }
                }
            }
        }
        if let Ok(outcome) = volume.apply_edit(&plan) {
            for b in &outcome.bricks {
                // Greedy AO samples edge/corner halos too. A preview must
                // never cache any of those 26 neighbours under real revisions.
                for dz in -1..=1 {
                    for dy in -1..=1 {
                        for dx in -1..=1 {
                            volatile.insert(spall_core::BrickCoord::new(
                                b.coord.x + dx,
                                b.coord.y + dy,
                                b.coord.z + dz,
                            ));
                        }
                    }
                }
            }
        }
    }
    volatile
}

/// [`build_instances`], reusing the boxes of every brick whose cache key is
/// unchanged. Boxes are merged within a brick (not across bricks), so a flat
/// surface costs one box per brick it crosses.
pub fn build_instances_cached(
    volume: &Volume,
    center_m: [f64; 3],
    cache: &mut TerrainInstanceCache,
) -> Vec<Instance> {
    build_instances_volatile(volume, center_m, cache, &std::collections::HashSet::new())
}

/// [`build_instances_cached`] where bricks in `volatile` neither read nor write
/// the cache: they are rebuilt from `volume` every time. For a volume that
/// carries local, unconfirmed edits (see [`PendingCut`]): a revision number
/// identifies a brick's contents only along the server's history, so boxes
/// built from a preview must never be stored under it.
pub fn build_instances_volatile(
    volume: &Volume,
    center_m: [f64; 3],
    cache: &mut TerrainInstanceCache,
    volatile: &std::collections::HashSet<spall_core::BrickCoord>,
) -> Vec<Instance> {
    use std::hash::{Hash, Hasher};
    let cell_m = f64::from(CELL_M);
    let (min, max) = view_window(center_m);
    let (min_brick, _) = min.split();
    let (max_brick, _) = max.split();
    let revision = |c: spall_core::BrickCoord| {
        volume
            .brick_revision(c)
            .ok()
            .flatten()
            .map_or(0, |r| r.get() + 1)
    };
    let mut instances = Vec::new();
    let mut visited = std::collections::HashSet::new();
    for bz in min_brick.z..=max_brick.z {
        for by in min_brick.y..=max_brick.y {
            for bx in min_brick.x..=max_brick.x {
                let coord = spall_core::BrickCoord::new(bx, by, bz);
                let own = revision(coord);
                if own == 0 {
                    continue; // not resident: nothing to draw, nothing cached
                }
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                own.hash(&mut hasher);
                for [dx, dy, dz] in BRICK_NEIGHBOURS {
                    revision(spall_core::BrickCoord::new(bx + dx, by + dy, bz + dz))
                        .hash(&mut hasher);
                }
                let key = hasher.finish();
                visited.insert(coord);
                let is_volatile = volatile.contains(&coord);
                if !is_volatile
                    && let Some((cached_key, boxes)) = cache.bricks.get(&coord)
                    && *cached_key == key
                {
                    instances.extend_from_slice(boxes);
                    continue;
                }
                let mut cells = Vec::new();
                brick_visible_cells(volume, &mut cache.grid, coord, min, max, &mut cells);
                let boxes: Vec<Instance> = greedy_boxes(cells)
                    .into_iter()
                    .map(|(material, bmin, size)| Instance {
                        offset: [0, 1, 2]
                            .map(|a| ((bmin[a] as f64 + size[a] as f64 * 0.5) * cell_m) as f32),
                        material: u32::from(material.0),
                        size: [0, 1, 2].map(|a| (size[a] as f64 * cell_m) as f32),
                        _pad: 0.0,
                        rotation: IDENTITY_ROTATION,
                    })
                    .collect();
                instances.extend_from_slice(&boxes);
                if is_volatile {
                    // Forget any entry too: its key may now match a preview.
                    cache.bricks.remove(&coord);
                } else {
                    cache.bricks.insert(coord, (key, boxes));
                }
            }
        }
    }
    cache.bricks.retain(|coord, _| visited.contains(coord));
    instances
}

pub(super) const BRICK_NEIGHBOURS: [[i64; 3]; 6] = [
    [1, 0, 0],
    [-1, 0, 0],
    [0, 1, 0],
    [0, -1, 0],
    [0, 0, 1],
    [0, 0, -1],
];

/// Merges unit cells into as few axis-aligned boxes as a greedy pass finds:
/// runs along `x`, then runs of equal rows along `z`, then equal slabs along
/// `y`. Cells merge only with the same key (the material). Returns
/// `(key, min cell, size in cells)`; every input cell is covered exactly once.
/// The renderer draws a box as one instance, so a flat or terraced surface
/// costs a few boxes instead of one cube per cell.
pub(super) fn greedy_boxes<K: Ord + Copy>(
    mut cells: Vec<(K, [i64; 3])>,
) -> Vec<(K, [i64; 3], [i64; 3])> {
    cells.sort_unstable_by_key(|&(key, [x, y, z])| (key, y, z, x));
    // Runs along x: (key, y, z, x0, len).
    let mut runs: Vec<(K, i64, i64, i64, i64)> = Vec::new();
    for (key, [x, y, z]) in cells {
        match runs.last_mut() {
            Some(run) if run.0 == key && run.1 == y && run.2 == z && run.3 + run.4 == x => {
                run.4 += 1;
            }
            _ => runs.push((key, y, z, x, 1)),
        }
    }
    // Equal runs on consecutive rows: (key, y, x0, len, z0, dz).
    runs.sort_unstable_by_key(|&(key, y, z, x0, len)| (key, y, x0, len, z));
    let mut rects: Vec<(K, i64, i64, i64, i64, i64)> = Vec::new();
    for (key, y, z, x0, len) in runs {
        match rects.last_mut() {
            Some(r) if (r.0, r.1, r.2, r.3) == (key, y, x0, len) && r.4 + r.5 == z => r.5 += 1,
            _ => rects.push((key, y, x0, len, z, 1)),
        }
    }
    // Equal rectangles on consecutive layers: (key, min, size).
    rects.sort_unstable_by_key(|&(key, y, x0, len, z0, dz)| (key, x0, len, z0, dz, y));
    let mut boxes: Vec<(K, [i64; 3], [i64; 3])> = Vec::new();
    for (key, y, x0, len, z0, dz) in rects {
        match boxes.last_mut() {
            Some((k, min, size))
                if *k == key
                    && (min[0], size[0], min[2], size[2]) == (x0, len, z0, dz)
                    && min[1] + size[1] == y =>
            {
                size[1] += 1;
            }
            _ => boxes.push((key, [x0, y, z0], [len, 1, dz])),
        }
    }
    boxes
}

/// Whether the brick at `coord` is resident and one material throughout.
pub(super) fn uniform_solid(volume: &Volume, coord: spall_core::BrickCoord) -> bool {
    matches!(volume.snapshot_brick(coord), Ok(Some(brick))
        if !brick.is_dense()
            && brick.get(spall_core::LocalCell::new(0, 0, 0).expect("origin")) != MaterialId::AIR)
}

/// A uniform solid brick with a uniform solid brick on all six sides has no
/// exposed cell: its whole shell is buried.
pub(super) fn brick_is_interior(volume: &Volume, coord: spall_core::BrickCoord) -> bool {
    uniform_solid(volume, coord)
        && [
            [1, 0, 0],
            [-1, 0, 0],
            [0, 1, 0],
            [0, -1, 0],
            [0, 0, 1],
            [0, 0, -1],
        ]
        .iter()
        .all(|[dx, dy, dz]| {
            uniform_solid(
                volume,
                spall_core::BrickCoord::new(coord.x + dx, coord.y + dy, coord.z + dz),
            )
        })
}

/// Every exposed solid cell of `volume` inside the view window around
/// `center_m`, with its material.
#[cfg(test)]
pub(super) fn visible_cells(volume: &Volume, center_m: [f64; 3]) -> Vec<(MaterialId, [i64; 3])> {
    let (min, max) = view_window(center_m);
    let (min_brick, _) = min.split();
    let (max_brick, _) = max.split();
    let mut cells = Vec::new();
    let mut grid = BrickGrid::new();
    for bz in min_brick.z..=max_brick.z {
        for by in min_brick.y..=max_brick.y {
            for bx in min_brick.x..=max_brick.x {
                let coord = spall_core::BrickCoord::new(bx, by, bz);
                brick_visible_cells(volume, &mut grid, coord, min, max, &mut cells);
            }
        }
    }
    cells
}

/// Appends the exposed solid cells of one brick that lie inside the window
/// `min..=max`. Uniform air and fully buried uniform rock are skipped outright;
/// every other brick is scanned from a padded in-memory copy (see
/// [`BrickGrid`]) rather than six volume lookups per solid cell.
pub(super) fn brick_visible_cells(
    volume: &Volume,
    grid: &mut BrickGrid,
    coord: spall_core::BrickCoord,
    min: GlobalCell,
    max: GlobalCell,
    cells: &mut Vec<(MaterialId, [i64; 3])>,
) {
    let edge = i64::from(spall_core::BRICK_EDGE);
    let Ok(Some(brick)) = volume.snapshot_brick(coord) else {
        return;
    };
    if !brick.is_dense()
        && brick.get(spall_core::LocalCell::new(0, 0, 0).expect("origin")) == MaterialId::AIR
    {
        return;
    }
    if brick_is_interior(volume, coord) {
        return;
    }
    let base = GlobalCell::new(coord.x * edge, coord.y * edge, coord.z * edge);
    let lo = [
        (min.x - base.x).max(0),
        (min.y - base.y).max(0),
        (min.z - base.z).max(0),
    ];
    let hi = [
        (max.x - base.x).min(edge - 1),
        (max.y - base.y).min(edge - 1),
        (max.z - base.z).min(edge - 1),
    ];
    grid.load(volume, coord, &brick);
    for z in lo[2]..=hi[2] {
        for y in lo[1]..=hi[1] {
            for x in lo[0]..=hi[0] {
                let material = grid.material(x, y, z);
                if material != 0 && !grid.buried(x, y, z) {
                    cells.push((MaterialId(material), [base.x + x, base.y + y, base.z + z]));
                }
            }
        }
    }
}

/// One brick's materials plus a one-cell solid halo from its six face
/// neighbours, in plain arrays: a cell's buried test is then six array reads
/// instead of six `Volume` lookups (a `BTreeMap` walk each).
pub(super) struct BrickGrid {
    /// Material id per cell, `x + 32 * (y + 32 * z)`.
    materials: Vec<u16>,
    /// Solid flag over the `34^3` halo box, `(x + 1) + 34 * ((y + 1) + 34 * (z + 1))`.
    solid: Vec<bool>,
}

impl BrickGrid {
    const EDGE: i64 = 32;
    const PAD: i64 = 34;

    fn new() -> Self {
        Self {
            materials: vec![0; 32 * 32 * 32],
            solid: vec![false; 34 * 34 * 34],
        }
    }

    #[inline]
    fn cell_index(x: i64, y: i64, z: i64) -> usize {
        (x + Self::EDGE * (y + Self::EDGE * z)) as usize
    }

    #[inline]
    fn pad_index(x: i64, y: i64, z: i64) -> usize {
        ((x + 1) + Self::PAD * ((y + 1) + Self::PAD * (z + 1))) as usize
    }

    #[inline]
    fn material(&self, x: i64, y: i64, z: i64) -> u16 {
        self.materials[Self::cell_index(x, y, z)]
    }

    /// Whether all six face neighbours are solid. A neighbour in a missing
    /// brick counts as not solid, exactly like [`is_buried`].
    #[inline]
    fn buried(&self, x: i64, y: i64, z: i64) -> bool {
        let s = |x, y, z| self.solid[Self::pad_index(x, y, z)];
        s(x + 1, y, z)
            && s(x - 1, y, z)
            && s(x, y + 1, z)
            && s(x, y - 1, z)
            && s(x, y, z + 1)
            && s(x, y, z - 1)
    }

    fn load(
        &mut self,
        volume: &Volume,
        coord: spall_core::BrickCoord,
        brick: &spall_voxel::BrickSnapshot,
    ) {
        use spall_core::LocalCell;
        let edge = Self::EDGE;
        if brick.is_dense() {
            for z in 0..edge {
                for y in 0..edge {
                    for x in 0..edge {
                        let local = LocalCell::new(x as u8, y as u8, z as u8).expect("in-brick");
                        self.materials[Self::cell_index(x, y, z)] = brick.get(local).0;
                    }
                }
            }
        } else {
            let m = brick.get(LocalCell::new(0, 0, 0).expect("origin")).0;
            self.materials.fill(m);
        }
        self.solid.fill(false);
        for z in 0..edge {
            for y in 0..edge {
                for x in 0..edge {
                    self.solid[Self::pad_index(x, y, z)] =
                        self.materials[Self::cell_index(x, y, z)] != 0;
                }
            }
        }
        // Halo faces from the neighbouring bricks' adjacent layers.
        for [dx, dy, dz] in [
            [1, 0, 0],
            [-1, 0, 0],
            [0, 1, 0],
            [0, -1, 0],
            [0, 0, 1],
            [0, 0, -1],
        ]
        .into_iter()
        {
            let neighbour = spall_core::BrickCoord::new(coord.x + dx, coord.y + dy, coord.z + dz);
            let Ok(Some(n)) = volume.snapshot_brick(neighbour) else {
                continue;
            };
            let dense = n.is_dense();
            let uniform_solid =
                !dense && n.get(LocalCell::new(0, 0, 0).expect("origin")) != MaterialId::AIR;
            for a in 0..edge {
                for b in 0..edge {
                    // `(a, b)` runs over the two axes in the face; the face axis is fixed.
                    let (face_halo, face_src) = if dx != 0 {
                        (
                            if dx > 0 { edge } else { -1 },
                            if dx > 0 { 0 } else { edge - 1 },
                        )
                    } else if dy != 0 {
                        (
                            if dy > 0 { edge } else { -1 },
                            if dy > 0 { 0 } else { edge - 1 },
                        )
                    } else {
                        (
                            if dz > 0 { edge } else { -1 },
                            if dz > 0 { 0 } else { edge - 1 },
                        )
                    };
                    let (hx, hy, hz, sx, sy, sz) = if dx != 0 {
                        (face_halo, a, b, face_src, a, b)
                    } else if dy != 0 {
                        (a, face_halo, b, a, face_src, b)
                    } else {
                        (a, b, face_halo, a, b, face_src)
                    };
                    let solid = if dense {
                        n.get(LocalCell::new(sx as u8, sy as u8, sz as u8).expect("face cell"))
                            != MaterialId::AIR
                    } else {
                        uniform_solid
                    };
                    self.solid[Self::pad_index(hx, hy, hz)] = solid;
                }
            }
        }
    }
}

/// A cell whose six face neighbours are all solid contributes no visible
/// surface; skipping it keeps the instance count near the visible shell
/// instead of the whole solid volume.
#[cfg(test)]
pub(super) fn is_buried(volume: &Volume, cell: GlobalCell) -> bool {
    const NEIGHBORS: [[i64; 3]; 6] = [
        [1, 0, 0],
        [-1, 0, 0],
        [0, 1, 0],
        [0, -1, 0],
        [0, 0, 1],
        [0, 0, -1],
    ];
    NEIGHBORS.iter().all(|[dx, dy, dz]| {
        matches!(
            volume.sample(GlobalCell::new(cell.x + dx, cell.y + dy, cell.z + dz)),
            Ok(Sample::Filled(_))
        )
    })
}

/// Linear-space blue-green water, smooth and translucent.
pub(super) const WATER_MATERIAL: spall_render::Material =
    spall_render::Material::new([0.03, 0.16, 0.26], 0.08, 0.0).opacity(0.62);

/// The render table for a manifest, as the interactive window draws it:
/// [`materials_from_manifest`] plus per-voxel colour variation on every opaque,
/// non-emissive material, so terrain is not one flat colour per material. The
/// variation is computed in the shader, client-side and cosmetic only.
pub fn terrain_render_materials(manifest: &MaterialManifest) -> Vec<spall_render::Material> {
    let mut table = materials_from_manifest(manifest);
    for material in &mut table {
        if material.emissive == 0.0 && material.opacity >= 1.0 {
            *material = material.jitter(TERRAIN_COLOR_JITTER);
        }
    }
    table
}

/// Metres a torch's light reaches, and its intensity (colour x intensity).
pub(super) const TORCH_RADIUS_M: f32 = 14.0;
pub(super) const TORCH_INTENSITY: f32 = 26.0;
/// Emitters farther than this from the camera are not considered.
pub(super) const TORCH_CONSIDER_M: f32 = 60.0;

/// The point lights for the emissive voxels among `instances`: every instance
/// whose material emits, as a warm light at its centre, the
/// [`spall_render::MAX_POINT_LIGHTS`] nearest `eye` first. The lighting cache
/// cannot resolve an emitter smaller than about a metre, so a lone torch voxel
/// lights its surroundings through these instead.
pub fn emitter_point_lights(
    instances: &[Instance],
    materials: &[spall_render::Material],
    eye: Vec3,
) -> Vec<spall_render::PointLight> {
    let mut found: Vec<(f32, spall_render::PointLight)> = instances
        .iter()
        .filter_map(|i| {
            let m = materials.get(i.material as usize)?;
            (m.emissive > 0.0).then(|| {
                let p = Vec3::from_array(i.offset);
                let peak = m
                    .base_color
                    .iter()
                    .copied()
                    .fold(f32::MIN, f32::max)
                    .max(1e-3);
                (
                    (p - eye).length_squared(),
                    spall_render::PointLight {
                        position: i.offset,
                        color: m.base_color.map(|c| c / peak * TORCH_INTENSITY),
                        radius: TORCH_RADIUS_M,
                    },
                )
            })
        })
        .filter(|(d2, _)| *d2 <= TORCH_CONSIDER_M * TORCH_CONSIDER_M)
        .collect();
    found.sort_by(|a, b| a.0.total_cmp(&b.0));
    found
        .into_iter()
        .take(spall_render::MAX_POINT_LIGHTS)
        .map(|(_, light)| light)
        .collect()
}

/// Per-voxel colour variation of terrain materials: brightness up to about
/// +-18%, with a smaller per-channel drift (see `Material::jitter`).
pub(super) const TERRAIN_COLOR_JITTER: f32 = 0.18;

/// Merges the water columns of a keyframe (all one cell wide, same-height runs
/// side by side) into as few boxes as possible and assigns the water material:
/// a flat lake is a handful of boxes, not one per fluid column.
#[cfg(test)]
pub(super) fn merge_water_columns(columns: &[Instance], material: u32) -> Vec<Instance> {
    let mut cells = Vec::with_capacity(columns.len());
    for c in columns {
        let cell = c.size[0];
        if cell <= 0.0 {
            continue;
        }
        let ix = (c.offset[0] / cell - 0.5).round() as i64;
        let iz = (c.offset[2] / cell - 0.5).round() as i64;
        let bottom = c.offset[1] - c.size[1] * 0.5;
        // Columns merge only if cell size, bottom and height all agree (0.1 mm).
        let key = (
            cell.to_bits(),
            (f64::from(bottom) * 1e4).round() as i64,
            (f64::from(c.size[1]) * 1e4).round() as i64,
        );
        cells.push((key, [ix, 0, iz]));
    }
    greedy_boxes(cells)
        .into_iter()
        .map(|((cell_bits, bottom_q, height_q), min, size)| {
            let cell = f64::from(f32::from_bits(cell_bits));
            let height = height_q as f64 / 1e4;
            let bottom = bottom_q as f64 / 1e4;
            Instance {
                offset: [
                    ((min[0] as f64 + size[0] as f64 * 0.5) * cell) as f32,
                    (bottom + height * 0.5) as f32,
                    ((min[2] as f64 + size[2] as f64 * 0.5) * cell) as f32,
                ],
                material,
                size: [
                    (size[0] as f64 * cell) as f32,
                    height as f32,
                    (size[2] as f64 * cell) as f32,
                ],
                _pad: 0.0,
                rotation: IDENTITY_ROTATION,
            }
        })
        .collect()
}

/// Appends each box clipped to the terrain view window around `center_m`
/// (the same extent [`build_instances`] draws); boxes outside it are dropped.
pub(super) fn clip_water_to_window(
    boxes: &[Instance],
    center_m: [f64; 3],
    out: &mut Vec<Instance>,
) {
    let below = [
        f64::from(view_radius_m()),
        f64::from(VIEW_HEIGHT_DOWN_M),
        f64::from(view_radius_m()),
    ];
    let above = [
        f64::from(view_radius_m()),
        f64::from(VIEW_HEIGHT_UP_M),
        f64::from(view_radius_m()),
    ];
    for b in boxes {
        let mut offset = b.offset;
        let mut size = b.size;
        let mut visible = true;
        for a in 0..3 {
            let lo =
                (f64::from(b.offset[a]) - f64::from(b.size[a]) * 0.5).max(center_m[a] - below[a]);
            let hi =
                (f64::from(b.offset[a]) + f64::from(b.size[a]) * 0.5).min(center_m[a] + above[a]);
            if hi <= lo {
                visible = false;
                break;
            }
            offset[a] = ((lo + hi) * 0.5) as f32;
            size[a] = (hi - lo) as f32;
        }
        if visible {
            out.push(Instance { offset, size, ..*b });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replica::{ReplicaConfig, ReplicaWorld};
    use spall_core::VolumeId;

    /// The padded-array walk must agree with the per-cell `Volume` scan across
    /// brick boundaries and on missing neighbours: random holes in dense bricks,
    /// uniform rock and air bricks, and absent bricks at the edges.
    #[test]
    fn padded_walk_matches_the_per_cell_scan_on_a_random_volume() {
        use spall_core::{BrickCoord, LocalCell, VolumeId};
        use spall_voxel::brick::Brick;
        let mut volume = Volume::new(VolumeId::new(1).unwrap(), spall_core::CellSizeCode::Quarter);
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for bz in 0..3 {
            for by in 0..3 {
                for bx in 0..3 {
                    let coord = BrickCoord::new(bx, by, bz);
                    match next() % 6 {
                        0 => {} // absent
                        1 => volume
                            .insert_brick(
                                coord,
                                Brick::uniform(MaterialId::AIR, spall_core::Revision::ZERO),
                            )
                            .unwrap(),
                        2 | 3 => volume
                            .insert_brick(
                                coord,
                                Brick::uniform(MaterialId(2), spall_core::Revision::ZERO),
                            )
                            .unwrap(),
                        _ => {
                            let mut brick =
                                Brick::uniform(MaterialId(3), spall_core::Revision::ZERO);
                            for _ in 0..4000 {
                                let r = next();
                                let local = LocalCell::new(
                                    (r % 32) as u8,
                                    ((r >> 8) % 32) as u8,
                                    ((r >> 16) % 32) as u8,
                                )
                                .unwrap();
                                brick.set_cell(
                                    local,
                                    if r >> 30 & 1 == 0 {
                                        MaterialId::AIR
                                    } else {
                                        MaterialId(4)
                                    },
                                );
                            }
                            volume.insert_brick(coord, brick).unwrap();
                        }
                    }
                }
            }
        }
        let mut fast: Vec<_> = visible_cells(&volume, [12.0, 12.0, 12.0])
            .into_iter()
            .map(|(m, c)| (c, m.0))
            .collect();
        let mut slow = Vec::new();
        for z in 0..96 {
            for y in 0..96 {
                for x in 0..96 {
                    let cell = GlobalCell::new(x, y, z);
                    if let Ok(Sample::Filled(m)) = volume.sample(cell)
                        && !is_buried(&volume, cell)
                    {
                        slow.push(([x, y, z], m.0));
                    }
                }
            }
        }
        fast.sort_unstable();
        slow.sort_unstable();
        assert!(
            slow.len() > 5000,
            "a real test needs exposed cells: {}",
            slow.len()
        );
        assert_eq!(fast, slow);
    }

    fn water_column(ix: i64, iz: i64, bottom: f32, height: f32) -> Instance {
        let cell = 0.5_f32;
        Instance {
            offset: [
                (ix as f32 + 0.5) * cell,
                bottom + height * 0.5,
                (iz as f32 + 0.5) * cell,
            ],
            material: 0,
            size: [cell, height, cell],
            _pad: 0.0,
            rotation: IDENTITY_ROTATION,
        }
    }

    fn volume_of(boxes: &[Instance]) -> f64 {
        boxes
            .iter()
            .map(|b| f64::from(b.size[0]) * f64::from(b.size[1]) * f64::from(b.size[2]))
            .sum()
    }

    #[test]
    fn a_flat_lake_merges_to_a_few_boxes_and_keeps_its_volume() {
        let mut columns = Vec::new();
        for iz in 0..200 {
            for ix in 0..300 {
                if (ix, iz) != (5, 5) {
                    columns.push(water_column(ix, iz, 10.0, 0.5));
                }
            }
        }
        columns.push(water_column(5, 5, 10.0, 0.25)); // a shallower column
        let merged = merge_water_columns(&columns, 77);
        assert!(merged.len() <= 6, "{} boxes", merged.len());
        assert!(merged.iter().all(|b| b.material == 77));
        let diff = (volume_of(&columns) - volume_of(&merged)).abs();
        assert!(diff < 1e-2, "volume changed by {diff}");
    }

    #[test]
    fn water_is_clipped_to_the_terrain_window_and_dropped_beyond_it() {
        let mut columns = Vec::new();
        // Larger than any view window, so a clipped lake is a fraction of it.
        for iz in 0..800 {
            for ix in 0..800 {
                columns.push(water_column(ix, iz, 10.0, 0.5));
            }
        }
        let lake = merge_water_columns(&columns, 0); // 400 m x 400 m
        let mut near = Vec::new();
        clip_water_to_window(&lake, [100.0, 10.0, 100.0], &mut near);
        assert!(!near.is_empty());
        for b in &near {
            for (a, c) in [0, 1, 2].into_iter().zip([100.0, 10.0, 100.0]) {
                let half = f64::from(if a == 1 {
                    VIEW_HEIGHT_UP_M
                } else {
                    view_radius_m()
                });
                assert!(f64::from(b.offset[a]) - f64::from(b.size[a]) * 0.5 >= c - half - 1e-3);
                assert!(f64::from(b.offset[a]) + f64::from(b.size[a]) * 0.5 <= c + half + 1e-3);
            }
        }
        // A window centred far from the lake sees none of it.
        let mut far = Vec::new();
        clip_water_to_window(&lake, [900.0, 10.0, 900.0], &mut far);
        assert!(far.is_empty());
        // A clipped lake is smaller than the whole lake.
        assert!(volume_of(&near) < volume_of(&lake) * 0.5);
    }

    fn sorted(mut v: Vec<Instance>) -> Vec<([u32; 3], [u32; 3], u32)> {
        let mut out: Vec<_> = v
            .drain(..)
            .map(|i| {
                (
                    i.offset.map(f32::to_bits),
                    i.size.map(f32::to_bits),
                    i.material,
                )
            })
            .collect();
        out.sort_unstable();
        out
    }

    /// The per-brick cache must give exactly what a fresh build gives, before
    /// and after an edit, and an edit must invalidate only the brick it touched
    /// and its face neighbours.
    #[test]
    fn the_terrain_cache_matches_a_fresh_build_and_an_edit_invalidates_only_its_bricks() {
        use spall_core::{BrickCoord, CellSizeCode, VolumeId};
        use spall_voxel::EditPlan;
        let mut volume = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
        for bz in 0..4 {
            for bx in 0..4 {
                volume
                    .insert_brick(
                        BrickCoord::new(bx, 0, bz),
                        spall_voxel::brick::Brick::uniform(
                            MaterialId(1),
                            spall_core::Revision::ZERO,
                        ),
                    )
                    .unwrap();
                volume
                    .insert_brick(
                        BrickCoord::new(bx, 1, bz),
                        spall_voxel::brick::Brick::uniform(
                            MaterialId::AIR,
                            spall_core::Revision::ZERO,
                        ),
                    )
                    .unwrap();
            }
        }
        let center = [16.0, 8.0, 16.0];
        let mut cache = TerrainInstanceCache::new();
        let first = build_instances_cached(&volume, center, &mut cache);
        assert_eq!(sorted(first), sorted(build_instances(&volume, center)));
        let keys_before: std::collections::HashMap<_, _> =
            cache.bricks.iter().map(|(c, (k, _))| (*c, *k)).collect();
        assert!(!keys_before.is_empty());

        // Dig a pit in the middle of brick (2, 0, 2)'s top face.
        let mut plan = EditPlan::new(volume.id());
        for dz in 0..3 {
            for dx in 0..3 {
                plan.set(GlobalCell::new(70 + dx, 31, 70 + dz), MaterialId::AIR);
            }
        }
        volume.apply_edit(&plan).expect("dig");
        let second = build_instances_cached(&volume, center, &mut cache);
        assert_eq!(sorted(second), sorted(build_instances(&volume, center)));
        let changed: Vec<_> = cache
            .bricks
            .iter()
            .filter(|(c, (k, _))| keys_before.get(*c) != Some(k))
            .map(|(c, _)| *c)
            .collect();
        assert!(!changed.is_empty(), "the edit must be seen");
        // The edited brick and at most its six face neighbours.
        assert!(changed.len() <= 7, "{changed:?}");
        assert!(changed.contains(&BrickCoord::new(2, 0, 2)));
        assert_eq!(
            cache.len(),
            keys_before.len(),
            "no brick appeared or vanished"
        );
    }

    #[test]
    fn build_instances_timing_walk_arena() {
        let volume = spall_voxel::fixtures::walk_arena(VolumeId::new(1).unwrap());
        // On the floor (top surface y = 1.0 m) near the spawn end.
        let start = std::time::Instant::now();
        let instances = build_instances(&volume, [0.5, 1.0, 2.0]);
        eprintln!(
            "build_instances(walk_arena): {:?}, {} instances",
            start.elapsed(),
            instances.len()
        );
    }

    /// The brick walk's uniform-brick shortcuts must emit exactly the exposed
    /// cells a per-cell scan of the same window finds.
    #[test]
    fn brick_walk_matches_per_cell_scan() {
        let volume = spall_voxel::fixtures::g1_full_envelope_scene(VolumeId::new(1).unwrap());
        let center = [2.0, 13.5, 2.0];
        let cell_m = f64::from(CELL_M);
        let mut fast: Vec<_> = visible_cells(&volume, center)
            .into_iter()
            .map(|(m, c)| {
                (
                    c.map(|v| ((v as f64 + 0.5) * cell_m) as f32)
                        .map(f32::to_bits),
                    u32::from(m.0),
                )
            })
            .collect();
        // The draw window is whole bricks (see `view_window`).
        let (window_min, window_max) = view_window(center);
        let mut slow = Vec::new();
        for coord in volume.resident_brick_coords() {
            let edge = i64::from(spall_core::BRICK_EDGE);
            for z in 0..edge {
                for y in 0..edge {
                    for x in 0..edge {
                        let cell = GlobalCell::new(
                            coord.x * edge + x,
                            coord.y * edge + y,
                            coord.z * edge + z,
                        );
                        let inside = (window_min.x..=window_max.x).contains(&cell.x)
                            && (window_min.y..=window_max.y).contains(&cell.y)
                            && (window_min.z..=window_max.z).contains(&cell.z);
                        if !inside {
                            continue;
                        }
                        if let Ok(Sample::Filled(m)) = volume.sample(cell)
                            && !is_buried(&volume, cell)
                        {
                            let offset = [cell.x, cell.y, cell.z]
                                .map(|v| ((v as f64 + 0.5) * cell_m) as f32);
                            slow.push((offset.map(f32::to_bits), u32::from(m.0)));
                        }
                    }
                }
            }
        }
        fast.sort_unstable();
        slow.sort_unstable();
        assert!(!slow.is_empty());
        assert_eq!(fast, slow);
    }

    /// An unconfirmed swing carves a private copy at once, never enters the
    /// cache, and is dropped once the volume shows the cut or it expires.
    #[test]
    fn pending_cut_previews_without_poisoning_the_cache() {
        let volume = spall_voxel::fixtures::g1_full_envelope_scene(VolumeId::new(1).unwrap());
        let center = [2.0, 13.5, 2.0];
        let (cell, _) = visible_cells(&volume, center)
            .into_iter()
            .map(|(m, c)| (GlobalCell::new(c[0], c[1], c[2]), m))
            .next()
            .expect("the fixture has terrain");
        let mut cache = TerrainInstanceCache::new();
        let base = build_instances_cached(&volume, center, &mut cache);

        let now = Instant::now();
        let mut preview = volume.clone();
        let mut pending = vec![PendingCut {
            cell,
            radius_cells: 1,
            at: now,
        }];
        let volatile = apply_pending_cuts(&mut preview, &mut pending, now);
        assert!(!volatile.is_empty());
        assert!(matches!(preview.sample(cell), Ok(Sample::Empty { .. })));
        assert!(
            matches!(volume.sample(cell), Ok(Sample::Filled(_))),
            "the original is untouched"
        );
        let carved = build_instances_volatile(&preview, center, &mut cache, &volatile);
        assert_ne!(
            carved.len(),
            base.len(),
            "the preview changes what is drawn"
        );

        // Nothing from the preview leaked into the cache: the real volume still
        // draws exactly what it did before.
        let mut again = build_instances_cached(&volume, center, &mut cache);
        let mut base_sorted = base.clone();
        let key = |i: &Instance| (i.offset.map(f32::to_bits), i.material);
        again.sort_by_key(key);
        base_sorted.sort_by_key(key);
        assert_eq!(again, base_sorted);

        // Confirmed (the volume already shows it) or expired previews are dropped.
        let mut confirmed = vec![PendingCut {
            cell,
            radius_cells: 1,
            at: now,
        }];
        apply_pending_cuts(&mut preview.clone(), &mut confirmed, now);
        assert!(confirmed.is_empty());
        let mut stale = vec![PendingCut {
            cell,
            radius_cells: 1,
            at: now,
        }];
        apply_pending_cuts(&mut volume.clone(), &mut stale, now + PENDING_CUT_LIFETIME);
        assert!(stale.is_empty());
    }

    /// Greedy boxes cover every input cell exactly once, never merge across
    /// keys, and collapse flat slabs to one box.
    #[test]
    fn greedy_boxes_cover_each_cell_once_and_merge_flat_slabs() {
        // A 6x3x5 slab of key 1, with a key-2 cell and a hole cut in it.
        let mut cells = Vec::new();
        for z in 0..5 {
            for y in 0..3 {
                for x in 0..6 {
                    match (x, y, z) {
                        (2, 1, 2) => cells.push((2u8, [x, y, z])),
                        (4, 0, 3) => {}
                        _ => cells.push((1u8, [x, y, z])),
                    }
                }
            }
        }
        let mut shuffled = cells.clone();
        shuffled.reverse();
        let boxes = greedy_boxes(shuffled);
        let mut covered = std::collections::BTreeMap::new();
        for (key, min, size) in &boxes {
            for z in min[2]..min[2] + size[2] {
                for y in min[1]..min[1] + size[1] {
                    for x in min[0]..min[0] + size[0] {
                        assert!(
                            covered.insert([x, y, z], *key).is_none(),
                            "overlap at {x},{y},{z}"
                        );
                    }
                }
            }
        }
        let expected: std::collections::BTreeMap<_, _> =
            cells.iter().map(|(k, c)| (*c, *k)).collect();
        assert_eq!(covered, expected);
        assert!(
            boxes.len() < cells.len() / 4,
            "{} boxes for {} cells",
            boxes.len(),
            cells.len()
        );

        let flat: Vec<_> = (0..40)
            .flat_map(|z| (0..70).map(move |x| (7u8, [x, 3, z])))
            .collect();
        assert_eq!(greedy_boxes(flat), vec![(7u8, [0, 3, 0], [70, 1, 40])]);
        assert!(greedy_boxes(Vec::<(u8, [i64; 3])>::new()).is_empty());
    }

    /// Skipping buried interior bricks must not change what is drawn: the
    /// brick walk still equals a per-cell scan (see the test above), and a
    /// solid block with solid neighbours emits nothing for its interior.
    #[test]
    fn interior_bricks_are_skipped_but_the_block_surface_remains() {
        use spall_core::{BrickCoord, VolumeId};
        let mut volume = Volume::new(VolumeId::new(1).unwrap(), spall_core::CellSizeCode::Quarter);
        let stone = MaterialId(1);
        for z in 0..3 {
            for y in 0..3 {
                for x in 0..3 {
                    volume
                        .insert_brick(
                            BrickCoord::new(x, y, z),
                            spall_voxel::brick::Brick::uniform(stone, spall_core::Revision::ZERO),
                        )
                        .unwrap();
                }
            }
        }
        assert!(brick_is_interior(&volume, BrickCoord::new(1, 1, 1)));
        assert!(
            !brick_is_interior(&volume, BrickCoord::new(0, 1, 1)),
            "edge brick has a missing neighbour"
        );
        // A 96-cell cube's surface is 6 flat faces of 3 x 3 bricks: one box per
        // brick face (54), not ~55k cubes.
        let boxes = build_instances(&volume, [12.0, 12.0, 12.0]);
        assert_eq!(boxes.len(), 54, "{boxes:?}");
    }

    #[test]
    fn build_instances_timing_g1_full_envelope() {
        let volume = spall_voxel::fixtures::g1_full_envelope_scene(VolumeId::new(1).unwrap());
        // A real G1_WORKLOAD_SPAWNS point (spall_sim::fixtures), on the
        // ground rather than in mid-air.
        let start = std::time::Instant::now();
        let instances = build_instances(&volume, [2.0, 13.5, 2.0]);
        eprintln!(
            "build_instances(g1_full_envelope): {:?}, {} instances",
            start.elapsed(),
            instances.len()
        );
    }

    /// `build_scene` calls this *every `RedrawRequested`*, not just on a
    /// rebuild — unlike `build_instances` (throttled to once per metre moved
    /// / terrain change), this is the per-frame floor cost of the debug
    /// window regardless of camera movement.
    #[test]
    fn terrain_resident_hash_timing_g1_full_envelope() {
        let replica = ReplicaWorld::from_baseline(
            spall_voxel::fixtures::g1_full_envelope_scene(VolumeId::new(1).unwrap()),
            ReplicaConfig::default(),
        );
        let start = std::time::Instant::now();
        for _ in 0..10 {
            std::hint::black_box(replica.terrain_resident_hash());
        }
        eprintln!(
            "terrain_resident_hash(g1_full_envelope): {:?}/call over 10 calls",
            start.elapsed() / 10
        );
    }

    #[test]
    fn terrain_resident_hash_timing_walk_arena() {
        let replica = ReplicaWorld::from_baseline(
            spall_voxel::fixtures::walk_arena(VolumeId::new(1).unwrap()),
            ReplicaConfig::default(),
        );
        let start = std::time::Instant::now();
        for _ in 0..10 {
            std::hint::black_box(replica.terrain_resident_hash());
        }
        eprintln!(
            "terrain_resident_hash(walk_arena): {:?}/call over 10 calls",
            start.elapsed() / 10
        );
    }
}
