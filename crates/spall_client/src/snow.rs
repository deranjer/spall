//! Turns authoritative snow tile frames into per-tile fine-voxel meshes.
//!
//! Exposure is revalidated against the current immutable terrain replica: a
//! face stays only while its cell below is solid and the cell above is air.
//! Absent or failed bricks are never treated as air, but they also never make
//! existing snow vanish: a cell whose geometry is momentarily unknown keeps what
//! it showed. A terrain revision change alone does not hide snow either, since
//! the sampled geometry is what decides. Every update replaces a tile's mesh
//! whole, so snow never flickers out while a replacement is built.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use spall_core::GlobalCell;
use spall_protocol::WIRE_SCHEMA_VERSION;
use spall_protocol::snow::{
    SNOW_MAX_DEPTH_Q16, SNOW_MIN_FOOTPRINT_DEPTH_Q16, SNOW_TILE_CELLS, SnowSurface,
    SnowSurfaceFrame,
};
use spall_render::vertex::GpuVertex;
use spall_render::{
    SnowBeyond, SnowFootprintMark, SnowMesh, SnowNeighbours, SnowTile, SnowTileKey, mesh_tile,
};
use spall_voxel::{Sample, Volume};

const CELL_M: f32 = 0.25;
const TILE_M: f32 = 4.0;
/// Default draw radius; the window sets it to the terrain view distance.
const CAMERA_RADIUS_M: f32 = 72.0;
const MAX_INPUT_FRAMES: usize = 8_192;
/// Tiles nearer than this are meshed with 6.25 cm columns; they go back to
/// cell-sized columns only beyond the outer radius, so a camera hovering at the
/// boundary does not flip a tile every update.
const FINE_IN_M: f32 = 14.0;
const FINE_OUT_M: f32 = 18.0;
/// Tiles stay resident this much beyond the draw radius before they are dropped.
const KEEP_MARGIN_M: f32 = 8.0;

/// Compact counters for weather HUDs and bounded capture diagnostics.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct SnowPresentationStats {
    pub frames_considered: usize,
    pub tiles: usize,
    pub fine_tiles: usize,
    pub rendered_faces: usize,
    pub footprints: usize,
    /// Mesh quads of packed (carved) snow across the resident tiles.
    pub carved_quads: usize,
    pub max_depth_mm: u16,
    pub hidden_or_unknown_faces: usize,
    /// Faces the server marked but the terrain replica shows solid-free or covered.
    pub stale_faces: usize,
    /// Distance to the nearest frame tile, metres.
    pub nearest_frame_m: f32,
    /// Resident triangles over all tile meshes.
    pub triangles: usize,
    /// Tiles whose mesh is out of date and waits for the next update.
    pub pending_tiles: usize,
    /// Wall time of the tile meshing done by the latest update.
    pub last_build_ms: f32,
    pub last_rebuilt_tiles: usize,
}

/// Mesh changes to apply to the renderer. Each upsert replaces a tile whole;
/// `removed` lists tiles that no longer have snow.
#[derive(Debug, Default)]
pub struct SnowMeshUpdate {
    pub upserts: Vec<(SnowTileKey, Vec<GpuVertex>, Vec<u32>)>,
    pub removed: Vec<SnowTileKey>,
}

struct Validated {
    stale: usize,
    epoch: u64,
    seq: u64,
    terrain: u64,
    depth_q16: [u16; 256],
    marks: Vec<SnowFootprintMark>,
    hidden: usize,
}

struct TileEntry {
    valid: Validated,
    fine: bool,
    /// Hash of the inputs the resident mesh was built from; `None` before the first build.
    built: Option<u64>,
    has_mesh: bool,
    triangles: usize,
    carved_quads: usize,
    distance_m: f32,
    seen: u64,
}

/// Per-tile snow meshes and the cached validation behind them.
#[derive(Default)]
pub struct SnowPresentation {
    radius_m: f32,
    tiles: HashMap<SnowTileKey, TileEntry>,
    update_counter: u64,
    stats: SnowPresentationStats,
}

impl SnowPresentation {
    pub fn new() -> Self {
        Self {
            radius_m: CAMERA_RADIUS_M,
            ..Self::default()
        }
    }

    /// Sets how far from the camera snow is generated. Tiles beyond it are
    /// dropped by the next update.
    pub fn set_radius_m(&mut self, radius_m: f32) {
        self.radius_m = radius_m.clamp(16.0, 128.0);
    }

    pub fn stats(&self) -> SnowPresentationStats {
        self.stats
    }

    /// True while some tile's mesh is stale (a time budget cut the last update
    /// short); call `update` again soon.
    pub fn pending(&self) -> bool {
        self.stats.pending_tiles > 0
    }

    /// Tiles that currently have a mesh in the renderer.
    pub fn resident_keys(&self) -> Vec<SnowTileKey> {
        self.tiles
            .iter()
            .filter(|(_, tile)| tile.has_mesh)
            .map(|(key, _)| *key)
            .collect()
    }

    /// Validates changed frames and rebuilds stale tile meshes nearest-first
    /// until `budget` is spent. `camera` is in world metres; the volume uses
    /// quarter-metre cells.
    pub fn update<F: std::borrow::Borrow<SnowSurfaceFrame>>(
        &mut self,
        frames: &[F],
        volume: &Volume,
        camera: [f32; 3],
        budget: Duration,
    ) -> SnowMeshUpdate {
        let mut out = SnowMeshUpdate::default();
        if camera.iter().any(|value| !value.is_finite())
            || (volume.cell_size().metres() - f64::from(CELL_M)).abs() > f64::EPSILON
        {
            out.removed = self.resident_keys();
            self.tiles.clear();
            self.stats = SnowPresentationStats::default();
            return out;
        }
        self.update_counter += 1;
        let current_epoch = frames
            .iter()
            .map(|frame| frame.borrow().world_epoch)
            .max()
            .unwrap_or(0);
        let radius = self.radius_m;
        let mut stats = SnowPresentationStats::default();
        let mut considered = 0usize;
        for frame in frames {
            let frame = frame.borrow();
            let key = (frame.tile_x, frame.tile_z, frame.top_cell_y);
            if frame.schema != WIRE_SCHEMA_VERSION || frame.world_epoch != current_epoch {
                continue;
            }
            let distance = tile_distance(frame.tile_x, frame.tile_z, camera);
            let keep = if self.tiles.contains_key(&key) {
                radius + KEEP_MARGIN_M
            } else {
                radius
            };
            if distance > keep || considered >= MAX_INPUT_FRAMES {
                continue;
            }
            considered += 1;
            let terrain = terrain_fingerprint(volume, key);
            let previous = self.tiles.get(&key);
            let fresh = previous.is_some_and(|tile| {
                tile.valid.epoch == frame.world_epoch
                    && tile.valid.seq == frame.snapshot_seq
                    && tile.valid.terrain == terrain
            });
            if !fresh {
                let valid = validate_frame(frame, terrain, volume, previous.map(|t| &t.valid));
                match self.tiles.get_mut(&key) {
                    Some(tile) => tile.valid = valid,
                    None => {
                        self.tiles.insert(
                            key,
                            TileEntry {
                                valid,
                                fine: distance < FINE_IN_M,
                                built: None,
                                has_mesh: false,
                                triangles: 0,
                                carved_quads: 0,
                                distance_m: distance,
                                seen: 0,
                            },
                        );
                    }
                }
            }
            let tile = self.tiles.get_mut(&key).expect("inserted above");
            tile.seen = self.update_counter;
            tile.distance_m = distance;
            tile.fine = if distance < FINE_IN_M {
                true
            } else if distance > FINE_OUT_M {
                false
            } else {
                tile.fine
            };
        }
        stats.frames_considered = considered;
        stats.nearest_frame_m = self
            .tiles
            .values()
            .map(|tile| tile.distance_m)
            .fold(f32::INFINITY, f32::min);
        let counter = self.update_counter;
        self.tiles.retain(|key, tile| {
            let keep = tile.seen == counter;
            if !keep && tile.has_mesh {
                out.removed.push(*key);
            }
            keep
        });

        // Which tiles need a new mesh, nearest first.
        let mut stale: Vec<(SnowTileKey, u64)> = self
            .tiles
            .iter()
            .filter_map(|(key, tile)| {
                let stamp = self.mesh_stamp(*key, tile);
                (tile.built != Some(stamp)).then_some((*key, stamp))
            })
            .collect();
        stale.sort_by(|a, b| {
            self.tiles[&a.0]
                .distance_m
                .total_cmp(&self.tiles[&b.0].distance_m)
                .then_with(|| a.0.cmp(&b.0))
        });
        let started = Instant::now();
        let mut rebuilt = 0usize;
        let mut built: Vec<(SnowTileKey, u64, usize, bool, usize)> = Vec::new();
        for (key, stamp) in &stale {
            if rebuilt > 0 && started.elapsed() >= budget {
                break;
            }
            let tile = &self.tiles[key];
            let data = SnowTile {
                key: *key,
                depth_q16: tile.valid.depth_q16,
                fine: tile.fine,
                marks: tile.valid.marks.clone(),
            };
            let mut mesh = SnowMesh::default();
            mesh_tile(
                &data,
                &Neighbours {
                    tiles: &self.tiles,
                    volume,
                },
                &mut mesh,
            );
            let triangles = mesh.triangles();
            let has_mesh = triangles > 0;
            let carved = mesh
                .vertices
                .iter()
                .filter(|v| v.material == spall_render::snow_mesh::SNOW_MATERIAL_PACKED)
                .count()
                / 4;
            built.push((*key, *stamp, triangles, has_mesh, carved));
            if has_mesh {
                out.upserts.push((*key, mesh.vertices, mesh.indices));
            } else if tile.has_mesh {
                out.removed.push(*key);
            }
            rebuilt += 1;
        }
        for (key, stamp, triangles, has_mesh, carved) in built {
            if let Some(tile) = self.tiles.get_mut(&key) {
                tile.built = Some(stamp);
                tile.has_mesh = has_mesh;
                tile.triangles = triangles;
                tile.carved_quads = carved;
            }
        }
        stats.last_rebuilt_tiles = rebuilt;
        stats.pending_tiles = stale.len() - rebuilt;
        stats.last_build_ms = started.elapsed().as_secs_f32() * 1000.0;
        for tile in self.tiles.values() {
            stats.tiles += usize::from(tile.has_mesh);
            stats.fine_tiles += usize::from(tile.has_mesh && tile.fine);
            stats.triangles += tile.triangles;
            stats.carved_quads += tile.carved_quads;
            stats.hidden_or_unknown_faces += tile.valid.hidden;
            stats.stale_faces += tile.valid.stale;
            stats.footprints += if tile.fine { tile.valid.marks.len() } else { 0 };
            for depth in tile.valid.depth_q16.iter().filter(|d| **d > 0) {
                stats.rendered_faces += 1;
                stats.max_depth_mm = stats
                    .max_depth_mm
                    .max(((u32::from(*depth) * 1_000) / 65_536) as u16);
            }
        }
        self.stats = stats;
        out
    }

    /// Everything the tile's mesh depends on: its validated data and LOD, and
    /// the same for the four tiles that border it at the same layer.
    fn mesh_stamp(&self, key: SnowTileKey, tile: &TileEntry) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        let mut write = |value: u64| {
            for byte in value.to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x100_0000_01b3);
            }
        };
        let mut absorb = |entry: Option<&TileEntry>| match entry {
            Some(entry) => {
                write(entry.valid.seq);
                write(entry.valid.terrain);
                write(u64::from(entry.fine));
            }
            None => write(u64::MAX),
        };
        absorb(Some(tile));
        for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            absorb(self.tiles.get(&(key.0 + dx, key.1 + dz, key.2)));
        }
        hash
    }
}

struct Neighbours<'a> {
    tiles: &'a HashMap<SnowTileKey, TileEntry>,
    volume: &'a Volume,
}

impl SnowNeighbours for Neighbours<'_> {
    fn beyond(&self, cell_x: i64, cell_z: i64, layer_y: i32) -> SnowBeyond {
        let side = i64::from(SNOW_TILE_CELLS);
        let key = (
            cell_x.div_euclid(side) as i32,
            cell_z.div_euclid(side) as i32,
            layer_y,
        );
        if let Some(tile) = self.tiles.get(&key) {
            let local = (cell_z.rem_euclid(side) * side + cell_x.rem_euclid(side)) as usize;
            let depth = tile.valid.depth_q16[local];
            if depth > 0 {
                return SnowBeyond::Snow {
                    depth_q16: depth,
                    fine: tile.fine,
                };
            }
        }
        let above = GlobalCell::new(cell_x, i64::from(layer_y) + 1, cell_z);
        match self.volume.sample(above) {
            Ok(Sample::Empty { .. }) => SnowBeyond::Open,
            // Solid or unknown: hide the side rather than draw it inside terrain.
            _ => SnowBeyond::Wall,
        }
    }
}

fn terrain_fingerprint(volume: &Volume, key: SnowTileKey) -> u64 {
    let x = i64::from(key.0) * i64::from(SNOW_TILE_CELLS);
    let z = i64::from(key.1) * i64::from(SNOW_TILE_CELLS);
    let y = i64::from(key.2);
    let revision = |y: i64| {
        let (brick, _) = GlobalCell::new(x, y, z).split();
        match volume.brick_revision(brick) {
            Ok(Some(revision)) => revision.0.wrapping_add(1),
            Ok(None) => 0,
            Err(_) => u64::MAX,
        }
    };
    revision(y).rotate_left(17) ^ revision(y + 1)
}

fn tile_distance(tile_x: i32, tile_z: i32, camera: [f32; 3]) -> f32 {
    let axis = |tile: i32, value: f32| {
        let min = tile as f32 * TILE_M;
        if value < min {
            min - value
        } else if value > min + TILE_M {
            value - min - TILE_M
        } else {
            0.0
        }
    };
    axis(tile_x, camera[0]).hypot(axis(tile_z, camera[2]))
}

/// Samples each exposed face. Returns the bitmasks of faces that are confirmed
/// and of faces whose geometry is unknown (not yet loaded, or failed).
fn validate_surface_faces(
    surface: &SnowSurface,
    tile_min_x: i64,
    tile_min_z: i64,
    volume: &Volume,
    stale: &mut usize,
) -> ([u64; 4], [u64; 4]) {
    let mut valid = [0u64; 4];
    let mut unknown = [0u64; 4];
    let y = i64::from(surface.top_cell_y);
    for index in 0..256 {
        let bit = 1u64 << (index % 64);
        if surface.exposed_faces[index / 64] & bit == 0 {
            continue;
        }
        let x = tile_min_x + (index % 16) as i64;
        let z = tile_min_z + (index / 16) as i64;
        let solid = GlobalCell::new(x, y, z);
        let above = GlobalCell::new(x, y + 1, z);
        match (volume.sample(solid), volume.sample(above)) {
            (Ok(Sample::Filled(_)), Ok(Sample::Empty { .. })) => valid[index / 64] |= bit,
            (Ok(Sample::Filled(_)), Err(spall_voxel::AccessError::OutOfBounds { .. }))
                if volume.bounds().is_some_and(|bounds| {
                    i128::from(above.y) > i128::from(bounds.max.y) * 32 + 31
                }) =>
            {
                // The declared world ceiling is known exterior sky. An absent
                // in-bounds brick is still unknown.
                valid[index / 64] |= bit;
            }
            (Ok(Sample::Empty { .. }), _) | (Ok(Sample::Filled(_)), Ok(Sample::Filled(_))) => {
                // Known to be gone or covered: the authoritative face is stale.
                *stale += 1;
            }
            _ => unknown[index / 64] |= bit,
        }
    }
    (valid, unknown)
}

fn validate_frame(
    frame: &SnowSurfaceFrame,
    terrain: u64,
    volume: &Volume,
    previous: Option<&Validated>,
) -> Validated {
    let tile_min_x = i64::from(frame.tile_x) * i64::from(SNOW_TILE_CELLS);
    let tile_min_z = i64::from(frame.tile_z) * i64::from(SNOW_TILE_CELLS);
    let mut depth_q16 = [0u16; 256];
    let mut hidden = 0usize;
    let mut stale = 0usize;
    let mut face_masks: Vec<(&SnowSurface, [u64; 4])> = Vec::new();
    for surface in &frame.surfaces {
        if surface.top_cell_y != frame.top_cell_y
            || surface.depth_q16 > SNOW_MAX_DEPTH_Q16
            || surface.exposed_faces.iter().all(|mask| *mask == 0)
        {
            continue;
        }
        let (valid, unknown) =
            validate_surface_faces(surface, tile_min_x, tile_min_z, volume, &mut stale);
        for index in 0..256 {
            let bit = 1u64 << (index % 64);
            if valid[index / 64] & bit != 0 {
                depth_q16[index] = surface.depth_q16;
            } else if unknown[index / 64] & bit != 0 {
                hidden += 1;
                // Unknown geometry keeps what the cell last showed, at the new depth.
                if previous.is_some_and(|p| p.depth_q16[index] > 0) {
                    depth_q16[index] = surface.depth_q16;
                }
            }
        }
        face_masks.push((surface, valid));
    }
    let mut marks = Vec::new();
    for mark in &frame.footprints {
        let Some((surface, valid)) = face_masks
            .iter()
            .find(|(surface, _)| surface.id == mark.surface_id)
        else {
            continue;
        };
        if surface.depth_q16 < SNOW_MIN_FOOTPRINT_DEPTH_Q16
            || mark.contact_cell[1] != surface.top_cell_y
            || !face_valid(valid, mark.contact_cell, tile_min_x, tile_min_z)
        {
            continue;
        }
        let [cell_x, _, cell_z] = mark.contact_cell;
        marks.push(SnowFootprintMark {
            centre_xz: [
                cell_x as f32 * CELL_M + f32::from(mark.offset_mm[0]) * 0.001,
                cell_z as f32 * CELL_M + f32::from(mark.offset_mm[1]) * 0.001,
            ],
            heading_rad: f32::from(mark.heading_q15) / 32768.0 * std::f32::consts::PI,
            strength: f32::from(mark.strength_q8) / 255.0,
        });
    }
    Validated {
        stale,
        epoch: frame.world_epoch,
        seq: frame.snapshot_seq,
        terrain,
        depth_q16,
        marks,
        hidden,
    }
}

fn face_valid(mask: &[u64; 4], cell: [i32; 3], tile_min_x: i64, tile_min_z: i64) -> bool {
    let x = i64::from(cell[0]) - tile_min_x;
    let z = i64::from(cell[2]) - tile_min_z;
    if !(0..16).contains(&x) || !(0..16).contains(&z) {
        return false;
    }
    let index = z as usize * 16 + x as usize;
    mask[index / 64] & (1u64 << (index % 64)) != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{BrickCoord, CellSizeCode, LocalCell, MaterialId, Revision, VolumeId};
    use spall_protocol::SnowFootprint;
    use spall_voxel::Brick;

    const BUDGET: Duration = Duration::from_millis(500);

    fn surface(top_cell_y: i32, depth_q16: u16) -> SnowSurface {
        let mut exposed_faces = [0u64; 4];
        exposed_faces[0] = 1;
        SnowSurface {
            id: spall_protocol::snow::SnowSurfaceId([7; 16]),
            top_cell_y,
            geometry_revision: 1,
            exposure_generation: 1,
            exposed_faces,
            depth_q16,
        }
    }

    fn frame(top_cell_y: i32, depth_q16: u16) -> SnowSurfaceFrame {
        SnowSurfaceFrame {
            schema: WIRE_SCHEMA_VERSION,
            world_epoch: 1,
            world_revision: 1,
            effective_tick: 120,
            snapshot_seq: 1,
            tile_x: 0,
            tile_z: 0,
            top_cell_y,
            surfaces: vec![surface(top_cell_y, depth_q16)],
            footprints: Vec::new(),
        }
    }

    fn plane(top_y: u8, add_above: bool, revision: u64) -> Volume {
        let mut volume = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
        let mut brick = Brick::uniform(MaterialId::AIR, Revision(revision));
        assert!(brick.set_cell(LocalCell::new(0, top_y, 0).unwrap(), MaterialId(3),));
        if add_above && top_y < 31 {
            assert!(brick.set_cell(LocalCell::new(0, top_y + 1, 0).unwrap(), MaterialId(4),));
        }
        volume
            .insert_brick(BrickCoord::new(0, 0, 0), brick)
            .unwrap();
        volume
    }

    const CAMERA: [f32; 3] = [0.5, 1.0, 0.5];

    #[test]
    fn converter_keeps_only_exposed_solid_faces() {
        let mut presentation = SnowPresentation::new();
        let update = presentation.update(&[frame(0, 20_000)], &plane(0, false, 1), CAMERA, BUDGET);
        assert_eq!(update.upserts.len(), 1);
        assert!(update.removed.is_empty());
        assert_eq!(presentation.stats().rendered_faces, 1);

        // A newly filled above-cell removes the snow.
        let mut roofed = frame(0, 20_000);
        roofed.snapshot_seq = 2;
        let update = presentation.update(&[roofed], &plane(0, true, 2), CAMERA, BUDGET);
        assert_eq!(update.removed, vec![(0, 0, 0)]);
        assert_eq!(presentation.stats().rendered_faces, 0);
    }

    #[test]
    fn unknown_support_never_becomes_snow() {
        let mut presentation = SnowPresentation::new();
        let empty = Volume::new(VolumeId::new(2).unwrap(), CellSizeCode::Quarter);
        let update = presentation.update(&[frame(0, 20_000)], &empty, CAMERA, BUDGET);
        assert!(update.upserts.is_empty());
        let top_of_brick = plane(31, false, 1);
        let update =
            presentation.update(&[frame(31, 20_000)], &top_of_brick, [0.5, 8.0, 0.5], BUDGET);
        assert!(update.upserts.is_empty(), "unloaded brick above is unknown");
    }

    #[test]
    fn declared_world_ceiling_is_known_sky() {
        let bounds =
            spall_voxel::BrickBounds::new(BrickCoord::new(0, 0, 0), BrickCoord::new(0, 0, 0))
                .unwrap();
        let mut volume = Volume::bounded(VolumeId::new(1).unwrap(), CellSizeCode::Quarter, bounds);
        let mut brick = Brick::uniform(MaterialId::AIR, Revision(1));
        brick.set_cell(LocalCell::new(0, 31, 0).unwrap(), MaterialId(3));
        volume
            .insert_brick(BrickCoord::new(0, 0, 0), brick)
            .unwrap();
        let mut presentation = SnowPresentation::new();
        let update = presentation.update(&[frame(31, 20_000)], &volume, [0.5, 8.0, 0.5], BUDGET);
        assert_eq!(update.upserts.len(), 1);
    }

    #[test]
    fn depth_changes_replace_the_tile_in_place_without_an_empty_frame() {
        let mut presentation = SnowPresentation::new();
        let terrain = plane(0, false, 1);
        let first = presentation.update(&[frame(0, 12_000)], &terrain, CAMERA, BUDGET);
        assert_eq!(first.upserts.len(), 1);

        // Snow deepens, the frame's cohort is re-identified and the terrain
        // revision moved on: the tile is replaced, never removed.
        let mut deeper = frame(0, 24_000);
        deeper.snapshot_seq = 2;
        deeper.surfaces[0].geometry_revision = 2;
        let second = presentation.update(&[deeper], &plane(0, false, 2), CAMERA, BUDGET);
        assert!(second.removed.is_empty());
        assert_eq!(second.upserts.len(), 1);
        assert!(!second.upserts[0].1.is_empty());
        let top = |update: &SnowMeshUpdate| {
            update.upserts[0]
                .1
                .iter()
                .map(|v| v.position[1])
                .fold(f32::MIN, f32::max)
        };
        assert!(top(&second) > top(&first), "deeper snow stands taller");
    }

    #[test]
    fn unknown_geometry_keeps_the_snow_it_showed() {
        let mut presentation = SnowPresentation::new();
        presentation.update(&[frame(0, 20_000)], &plane(0, false, 1), CAMERA, BUDGET);
        let mut next = frame(0, 22_000);
        next.snapshot_seq = 2;
        let empty = Volume::new(VolumeId::new(2).unwrap(), CellSizeCode::Quarter);
        let update = presentation.update(&[next], &empty, CAMERA, BUDGET);
        assert!(update.removed.is_empty());
        assert_eq!(update.upserts.len(), 1);
        assert!(!update.upserts[0].1.is_empty());
    }

    #[test]
    fn footprint_needs_a_confirmed_deep_face_and_far_tiles_skip_it() {
        let mut presentation = SnowPresentation::new();
        let volume = plane(0, false, 1);
        let mut snapshot = frame(0, 20_000);
        snapshot.footprints.push(SnowFootprint {
            id: 1,
            surface_id: snapshot.surfaces[0].id,
            geometry_revision: 1,
            contact_cell: [0, 0, 0],
            offset_mm: [125, 125],
            heading_q15: 8192,
            foot: 1,
            created_tick: 119,
            strength_q8: 255,
        });
        let update = presentation.update(&[snapshot.clone()], &volume, CAMERA, BUDGET);
        assert_eq!(presentation.stats().footprints, 1);
        let packed = update.upserts[0]
            .1
            .iter()
            .filter(|v| v.material == spall_render::snow_mesh::SNOW_MATERIAL_PACKED)
            .count();
        assert!(packed > 0, "the print is carved into the mesh");

        snapshot.snapshot_seq += 1;
        snapshot.surfaces[0].depth_q16 = SNOW_MIN_FOOTPRINT_DEPTH_Q16 - 1;
        presentation.update(&[snapshot], &volume, CAMERA, BUDGET);
        assert_eq!(presentation.stats().footprints, 0);
    }
}
