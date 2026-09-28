//! CPU-side meshing and picking for the scene viewport.
//!
//! The composited scene is a sparse voxel grid. Every exposed face becomes one
//! quad in world metres, lit by the engine's `spall_render` opaque pass. Each
//! distinct display colour is one entry of a material table, so per-voxel
//! colour survives without adding a vertex attribute. Nothing here touches the
//! GPU; the app uploads the result and reuses it until the scene changes.

use std::collections::{BTreeMap, HashMap, HashSet};

use glam::Vec3;
use spall_render::indirect::LightingVolume;
use spall_render::{GpuVertex, Material, cache_origin_around, mark_world_box};

use crate::{EditorEntityId, VoxelAssetFile, VoxelCoord};

/// Quads per resident chunk (4 vertices, 40 bytes each). Keeps one buffer near
/// 32 MB however large the scene grows.
const QUADS_PER_CHUNK: usize = 200_000;
/// Perceptual occlusion per AO level, from fully pinched (0) to open (3).
const AO_LEVELS: [f32; 4] = [0.0, 0.4, 0.7, 1.0];
/// The selected entity's cells are blended toward this colour.
const SELECT_TINT: [u16; 3] = [0xff, 0x8a, 0x4a];

/// Metres per cell for a stable SPVX cell-size code.
pub fn cell_metres(cell_size_code: u8) -> f32 {
    match cell_size_code {
        0 => 0.25,
        _ => 0.0625,
    }
}

pub struct MeshChunkData {
    pub vertices: Vec<GpuVertex>,
    pub indices: Vec<u32>,
}

pub struct SceneMesh {
    pub chunks: Vec<MeshChunkData>,
    /// Indexed by `GpuVertex::material`; entry 0 is unused (air).
    pub materials: Vec<Material>,
    /// World-space (metres) bounds of the occupied cells.
    pub bounds: Option<(Vec3, Vec3)>,
    pub cell_metres: f32,
    occupied: HashSet<VoxelCoord>,
    cell_min: VoxelCoord,
    cell_max: VoxelCoord,
}

impl SceneMesh {
    pub fn quad_count(&self) -> usize {
        self.chunks.iter().map(|c| c.indices.len() / 6).sum()
    }

    /// Camera-local sky occupancy for this scene, so the editor viewport lights
    /// skylight the way the game does: open to the sky outdoors, dark under
    /// roofs. Centred on the scene; the scene is fully known, so nothing is
    /// marked unknown. `None` for an empty scene. Derived render data only.
    pub fn sky_occupancy(&self) -> Option<LightingVolume> {
        let (min, max) = self.bounds?;
        let mut volume = LightingVolume::empty(cache_origin_around((min + max) * 0.5));
        let m = self.cell_metres;
        for cell in &self.occupied {
            let lo = Vec3::new(cell.x as f32, cell.y as f32, cell.z as f32) * m;
            mark_world_box(&mut volume, lo, lo + Vec3::splat(m), 1);
        }
        Some(volume)
    }

    /// First occupied cell along a world-space ray (metres), if any.
    pub fn pick(&self, origin: Vec3, dir: Vec3) -> Option<VoxelCoord> {
        if self.occupied.is_empty() || dir.length_squared() < 1e-12 {
            return None;
        }
        let inv = 1.0 / self.cell_metres;
        let origin = origin * inv;
        let lo = Vec3::new(
            self.cell_min.x as f32,
            self.cell_min.y as f32,
            self.cell_min.z as f32,
        );
        let hi = Vec3::new(
            self.cell_max.x as f32 + 1.0,
            self.cell_max.y as f32 + 1.0,
            self.cell_max.z as f32 + 1.0,
        );
        // Slab test against the occupied extent; the walk starts where the ray
        // enters it, so a distant camera costs nothing extra.
        let (mut t_enter, mut t_exit) = (0.0_f32, f32::INFINITY);
        for axis in 0..3 {
            let (o, d) = (origin[axis], dir[axis]);
            if d.abs() < 1e-9 {
                if o < lo[axis] || o > hi[axis] {
                    return None;
                }
                continue;
            }
            let (a, b) = ((lo[axis] - o) / d, (hi[axis] - o) / d);
            t_enter = t_enter.max(a.min(b));
            t_exit = t_exit.min(a.max(b));
        }
        if t_enter > t_exit {
            return None;
        }
        // Nudge inside so the starting cell is well defined on a boundary.
        let start = origin + dir * (t_enter + 1e-4);
        let mut cell = [
            start.x.floor() as i32,
            start.y.floor() as i32,
            start.z.floor() as i32,
        ];
        let mut step = [0_i32; 3];
        let mut t_max = [f32::INFINITY; 3];
        let mut t_delta = [f32::INFINITY; 3];
        for axis in 0..3 {
            let d = dir[axis];
            if d > 1e-9 {
                step[axis] = 1;
                t_delta[axis] = 1.0 / d;
                t_max[axis] = ((cell[axis] + 1) as f32 - start[axis]) / d;
            } else if d < -1e-9 {
                step[axis] = -1;
                t_delta[axis] = -1.0 / d;
                t_max[axis] = (cell[axis] as f32 - start[axis]) / d;
            }
        }
        let span = (self.cell_max.x - self.cell_min.x + self.cell_max.y - self.cell_min.y
            + self.cell_max.z
            - self.cell_min.z) as usize
            + 6;
        for _ in 0..span {
            let coord = VoxelCoord {
                x: cell[0],
                y: cell[1],
                z: cell[2],
            };
            if self.occupied.contains(&coord) {
                return Some(coord);
            }
            let axis = if t_max[0] <= t_max[1] && t_max[0] <= t_max[2] {
                0
            } else if t_max[1] <= t_max[2] {
                1
            } else {
                2
            };
            if !t_max[axis].is_finite() || t_max[axis] > t_exit - t_enter + 1e-3 {
                return None;
            }
            cell[axis] += step[axis];
            t_max[axis] += t_delta[axis];
        }
        None
    }
}

fn srgb_to_linear(channel: u8) -> f32 {
    let c = f32::from(channel) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Mesh `composite` (the flattened scene grid). Cells owned by `selected` are
/// tinted. `grid_lines` gives faces cell-local UVs so the shader draws its
/// faint per-cell boundary; otherwise faces are flat.
pub fn build_scene_mesh(
    composite: &VoxelAssetFile,
    owners: &BTreeMap<VoxelCoord, EditorEntityId>,
    selected: Option<EditorEntityId>,
    grid_lines: bool,
) -> SceneMesh {
    let metres = cell_metres(composite.cell_size_code);
    let occupied: HashSet<VoxelCoord> = composite.voxels.keys().copied().collect();
    let mut materials = vec![Material::default()];
    let mut material_of: HashMap<([u8; 3], bool), u32> = HashMap::new();
    let mut chunks = vec![MeshChunkData {
        vertices: Vec::new(),
        indices: Vec::new(),
    }];
    let mut cell_min = VoxelCoord { x: 0, y: 0, z: 0 };
    let mut cell_max = cell_min;

    for (index, (&cell, &material)) in composite.voxels.iter().enumerate() {
        if index == 0 {
            (cell_min, cell_max) = (cell, cell);
        } else {
            cell_min = VoxelCoord {
                x: cell_min.x.min(cell.x),
                y: cell_min.y.min(cell.y),
                z: cell_min.z.min(cell.z),
            };
            cell_max = VoxelCoord {
                x: cell_max.x.max(cell.x),
                y: cell_max.y.max(cell.y),
                z: cell_max.z.max(cell.z),
            };
        }
        let mut color = composite
            .colors
            .get(&cell)
            .copied()
            .unwrap_or_else(|| crate::default_voxel_color(material));
        if selected.is_some() && owners.get(&cell).copied() == selected {
            for channel in 0..3 {
                color[channel] =
                    ((u16::from(color[channel]) * 3 + SELECT_TINT[channel] * 2) / 5) as u8;
            }
        }
        // Cells of the portable `emissive.lamp` material glow, as the game's
        // lamp does (emission = base colour x 5, the renderer's encoding).
        let emissive = composite
            .material_keys
            .get(&material)
            .is_some_and(|key| key == "emissive.lamp");
        let id = *material_of.entry((color, emissive)).or_insert_with(|| {
            let base = Material::new(color.map(srgb_to_linear), 0.9, 0.0);
            materials.push(if emissive { base.emissive(5.0) } else { base });
            (materials.len() - 1) as u32
        });

        for axis in 0..3 {
            for positive in [false, true] {
                let mut normal = [0_i32; 3];
                normal[axis] = if positive { 1 } else { -1 };
                let outside = offset(cell, normal);
                if occupied.contains(&outside) {
                    continue;
                }
                if chunks
                    .last()
                    .is_some_and(|c| c.indices.len() / 6 >= QUADS_PER_CHUNK)
                {
                    chunks.push(MeshChunkData {
                        vertices: Vec::new(),
                        indices: Vec::new(),
                    });
                }
                let chunk = chunks.last_mut().expect("a chunk always exists");
                emit_face(
                    chunk, &occupied, cell, axis, positive, id, metres, grid_lines,
                );
            }
        }
    }
    chunks.retain(|c| !c.indices.is_empty());

    let bounds = (!occupied.is_empty()).then(|| {
        (
            Vec3::new(cell_min.x as f32, cell_min.y as f32, cell_min.z as f32) * metres,
            Vec3::new(
                (cell_max.x + 1) as f32,
                (cell_max.y + 1) as f32,
                (cell_max.z + 1) as f32,
            ) * metres,
        )
    });
    SceneMesh {
        chunks,
        materials,
        bounds,
        cell_metres: metres,
        occupied,
        cell_min,
        cell_max,
    }
}

fn offset(cell: VoxelCoord, delta: [i32; 3]) -> VoxelCoord {
    VoxelCoord {
        x: cell.x + delta[0],
        y: cell.y + delta[1],
        z: cell.z + delta[2],
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_face(
    chunk: &mut MeshChunkData,
    occupied: &HashSet<VoxelCoord>,
    cell: VoxelCoord,
    axis: usize,
    positive: bool,
    material: u32,
    metres: f32,
    grid_lines: bool,
) {
    // Tangents (t1, t2) satisfy t1 x t2 = +axis, so (u, v) = (t1, t2) winds
    // counter-clockwise seen from +axis and the swap does so from -axis.
    let (t1, t2) = ((axis + 1) % 3, (axis + 2) % 3);
    let (u, v) = if positive { (t1, t2) } else { (t2, t1) };
    let mut normal_i = [0_i32; 3];
    normal_i[axis] = if positive { 1 } else { -1 };
    let base = [cell.x, cell.y, cell.z];
    let mut origin = base.map(|c| c as f32);
    if positive {
        origin[axis] += 1.0;
    }
    let normal = normal_i.map(|c| c as f32);

    let layer = offset(cell, normal_i);
    let is_occupied = |du: i32, dv: i32| {
        let mut c = [layer.x, layer.y, layer.z];
        c[u] += du;
        c[v] += dv;
        occupied.contains(&VoxelCoord {
            x: c[0],
            y: c[1],
            z: c[2],
        })
    };
    // Corner order (0,0) (1,0) (1,1) (0,1) along (u, v).
    let corners = [(0, 0), (1, 0), (1, 1), (0, 1)];
    let first = chunk.vertices.len() as u32;
    let mut ao = [1.0_f32; 4];
    for (slot, (cu, cv)) in corners.into_iter().enumerate() {
        let (su, sv) = (cu * 2 - 1, cv * 2 - 1);
        let side_u = is_occupied(su, 0);
        let side_v = is_occupied(0, sv);
        let level = if side_u && side_v {
            0
        } else {
            3 - usize::from(side_u) - usize::from(side_v) - usize::from(is_occupied(su, sv))
        };
        ao[slot] = AO_LEVELS[level];
        let mut position = origin;
        position[u] += cu as f32;
        position[v] += cv as f32;
        chunk.vertices.push(GpuVertex {
            position: position.map(|p| p * metres),
            normal,
            local_uv: if grid_lines {
                [cu as f32, cv as f32]
            } else {
                [0.5, 0.5]
            },
            ao: ao[slot],
            material,
        });
    }
    // Split along the diagonal that keeps the AO gradient smooth.
    let quad = if ao[0] + ao[2] < ao[1] + ao[3] {
        [1, 2, 3, 1, 3, 0]
    } else {
        [0, 1, 2, 0, 2, 3]
    };
    chunk.indices.extend(quad.map(|i| first + i));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AssetId, VoxelAssetFile};

    fn grid(cells: &[(i32, i32, i32)]) -> VoxelAssetFile {
        let mut asset = VoxelAssetFile::new(AssetId(0), "t");
        asset.cell_size_code = 0;
        for &(x, y, z) in cells {
            let cell = VoxelCoord { x, y, z };
            asset.voxels.insert(cell, 1);
            asset.colors.insert(cell, [200, 40, 40]);
        }
        asset
    }

    fn mesh(cells: &[(i32, i32, i32)]) -> SceneMesh {
        build_scene_mesh(&grid(cells), &BTreeMap::new(), None, false)
    }

    #[test]
    fn sky_occupancy_marks_exactly_the_cells_the_scene_occupies() {
        assert!(mesh(&[]).sky_occupancy().is_none());
        // Two 0.25 m voxels in one 0.5 m cache cell, and one in another.
        let occupancy = mesh(&[(0, 0, 0), (1, 1, 1), (4, 0, 0)])
            .sky_occupancy()
            .expect("a non-empty scene has occupancy");
        assert_eq!(occupancy.cells().iter().filter(|c| **c != 0).count(), 2);
        let cell = occupancy.world_to_cell(Vec3::new(0.1, 0.1, 0.1));
        let index = (cell.x + 128 * (cell.y + 128 * cell.z)) as usize;
        assert_eq!(occupancy.cells()[index], 1);
    }

    #[test]
    fn a_lone_voxel_is_six_quads_and_shared_faces_are_culled() {
        assert_eq!(mesh(&[(0, 0, 0)]).quad_count(), 6);
        // Two touching voxels hide one face each: 12 - 2.
        assert_eq!(mesh(&[(0, 0, 0), (1, 0, 0)]).quad_count(), 10);
    }

    #[test]
    fn every_triangle_winds_counter_clockwise_about_its_normal() {
        let m = mesh(&[(0, 0, 0), (2, 1, -1), (-3, 0, 4)]);
        for chunk in &m.chunks {
            for tri in chunk.indices.chunks_exact(3) {
                let p = |i: u32| Vec3::from_array(chunk.vertices[i as usize].position);
                let n = Vec3::from_array(chunk.vertices[tri[0] as usize].normal);
                let geometric = (p(tri[1]) - p(tri[0])).cross(p(tri[2]) - p(tri[0]));
                assert!(geometric.dot(n) > 0.0, "back-facing triangle {tri:?}");
            }
        }
    }

    #[test]
    fn positions_are_in_metres_and_bounds_cover_the_cells() {
        let m = mesh(&[(0, 0, 0), (3, 1, 2)]);
        let (lo, hi) = m.bounds.expect("bounds");
        assert_eq!(lo, Vec3::ZERO);
        assert_eq!(hi, Vec3::new(1.0, 0.5, 0.75));
    }

    #[test]
    fn identical_colours_share_one_material() {
        let m = mesh(&[(0, 0, 0), (5, 0, 0), (9, 9, 9)]);
        assert_eq!(m.materials.len(), 2, "air placeholder plus one colour");
    }

    #[test]
    fn a_selected_entity_gets_its_own_tinted_material() {
        let asset = grid(&[(0, 0, 0), (5, 0, 0)]);
        let id = EditorEntityId(7);
        let owners = BTreeMap::from([
            (VoxelCoord { x: 0, y: 0, z: 0 }, id),
            (VoxelCoord { x: 5, y: 0, z: 0 }, EditorEntityId(8)),
        ]);
        let m = build_scene_mesh(&asset, &owners, Some(id), false);
        assert_eq!(m.materials.len(), 3);
    }

    #[test]
    fn a_pick_ray_hits_the_nearest_occupied_cell() {
        let m = mesh(&[(0, 0, 0), (1, 0, 0), (2, 0, 0)]);
        let cm = m.cell_metres;
        let y = 0.5 * cm;
        // From +X looking -X the nearest cell is x = 2.
        let hit = m.pick(Vec3::new(10.0, y, y), Vec3::NEG_X);
        assert_eq!(hit, Some(VoxelCoord { x: 2, y: 0, z: 0 }));
        // From -X looking +X it is x = 0.
        let hit = m.pick(Vec3::new(-10.0, y, y), Vec3::X);
        assert_eq!(hit, Some(VoxelCoord { x: 0, y: 0, z: 0 }));
        // A ray that passes beside the row misses.
        assert_eq!(m.pick(Vec3::new(10.0, 5.0, y), Vec3::NEG_X), None);
        // A ray pointing away misses.
        assert_eq!(m.pick(Vec3::new(10.0, y, y), Vec3::X), None);
    }

    #[test]
    fn a_diagonal_pick_ray_walks_across_cells() {
        let m = mesh(&[(0, 0, 0), (1, 1, 1)]);
        let cm = m.cell_metres;
        let from = Vec3::splat(-3.0 * cm);
        let dir = (Vec3::splat(1.5 * cm) - from).normalize();
        assert_eq!(m.pick(from, dir), Some(VoxelCoord { x: 0, y: 0, z: 0 }));
    }

    #[test]
    fn large_scenes_split_into_bounded_chunks() {
        // A checkerboard exposes every face, so quads scale with voxel count.
        let cells: Vec<_> = (0..130)
            .flat_map(|x| (0..130).flat_map(move |z| (0..2).map(move |y| (x * 2, y * 2, z * 2))))
            .collect();
        let m = mesh(&cells);
        assert_eq!(m.quad_count(), cells.len() * 6);
        assert!(m.chunks.len() > 1);
        assert!(
            m.chunks
                .iter()
                .all(|c| c.indices.len() / 6 <= QUADS_PER_CHUNK)
        );
    }
}
