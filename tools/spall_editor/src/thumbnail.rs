//! Shaded isometric voxel thumbnails for asset cards.
//!
//! A thumbnail is projected once, when an asset is loaded or edited, into a
//! unit-square mesh. Drawing a card only scales that mesh into its layout
//! rectangle, so a library full of assets costs a handful of small meshes per
//! frame instead of re-sorting and re-shading every voxel.

use super::*;

/// Pre-projected, pre-shaded faces in unit-square coordinates.
#[derive(Debug, Default)]
pub(crate) struct ThumbMesh {
    batches: Vec<ThumbBatch>,
}

#[derive(Debug)]
struct ThumbBatch {
    vertices: Vec<(Vec2, Vec4)>,
    indices: Vec<u16>,
}

/// Faces per mesh batch; four vertices each must stay below the `u16` limit.
const FACES_PER_BATCH: usize = 8000;

impl ThumbMesh {
    pub(crate) fn build(asset: &spall_editor::VoxelAssetFile) -> Arc<Self> {
        let camera = PreviewCamera::default();
        let (sin_yaw, cos_yaw) = camera.yaw.sin_cos();
        let (sin_pitch, cos_pitch) = camera.pitch.sin_cos();
        let rotate = |x: f32, y: f32, z: f32| {
            let horizontal = x * cos_yaw - z * sin_yaw;
            let depth = x * sin_yaw + z * cos_yaw;
            (horizontal, depth, y)
        };
        let project = |x: f32, y: f32, z: f32| {
            let (horizontal, depth, vertical) = rotate(x, y, z);
            Vec2::new(horizontal, depth * sin_pitch - vertical * cos_pitch)
        };

        let mut cells: Vec<_> = asset.voxels.keys().copied().collect();
        cells.sort_by(|a, b| {
            let depth = |cell: &VoxelCoord| {
                let (_, d, y) = rotate(
                    cell.x as f32 + 0.5,
                    cell.y as f32 + 0.5,
                    cell.z as f32 + 0.5,
                );
                d * cos_pitch + y * sin_pitch
            };
            depth(a).total_cmp(&depth(b))
        });

        let (ambient, direct, tint) = crate::preview_lighting(PreviewEnvironment::Studio);
        let mut faces: Vec<([Vec2; 4], Vec4)> = Vec::new();
        for cell in cells {
            let Some(state) = asset.state_at(cell) else {
                continue;
            };
            for (neighbor, corners, brightness, normal) in
                voxel_faces(cell, sin_yaw, cos_yaw, sin_pitch, cos_pitch)
            {
                if normal <= 0.0 || asset.state_at(neighbor).is_some() {
                    continue;
                }
                let light = ambient + direct * brightness;
                let color = Vec4::new(
                    f32::from(state.color[0]) / 255.0 * light * tint[0],
                    f32::from(state.color[1]) / 255.0 * light * tint[1],
                    f32::from(state.color[2]) / 255.0 * light * tint[2],
                    1.0,
                );
                faces.push((corners.map(|(x, y, z)| project(x, y, z)), color));
            }
        }
        if faces.is_empty() {
            return Arc::new(Self::default());
        }

        let mut min = Vec2::splat(f32::INFINITY);
        let mut max = Vec2::splat(f32::NEG_INFINITY);
        for (points, _) in &faces {
            for point in points {
                min = min.min(*point);
                max = max.max(*point);
            }
        }
        let extent = (max - min).max_element().max(f32::EPSILON);
        let center = (min + max) * 0.5;
        let normalize = |point: Vec2| (point - center) / extent + Vec2::splat(0.5);

        let batches = faces
            .chunks(FACES_PER_BATCH)
            .map(|chunk| {
                let mut vertices = Vec::with_capacity(chunk.len() * 4);
                let mut indices = Vec::with_capacity(chunk.len() * 6);
                for (points, color) in chunk {
                    let base = vertices.len() as u16;
                    vertices.extend(points.iter().map(|&point| (normalize(point), *color)));
                    indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
                }
                ThumbBatch { vertices, indices }
            })
            .collect();
        Arc::new(Self { batches })
    }
}

/// A square thumbnail of `side` logical pixels. Draws nothing for an empty
/// asset or when no mesh is available yet.
pub(crate) fn thumbnail(mesh: Option<&Arc<ThumbMesh>>, side: f32) {
    let mesh = mesh.cloned();
    yakui::constrained(yakui::Constraints::tight(Vec2::splat(side)), || {
        yakui::canvas(move |ctx| {
            let Some(mesh) = &mesh else {
                return;
            };
            let widget = ctx.dom.current();
            let Some(rect) = ctx.layout.get(widget).map(|layout| layout.rect) else {
                return;
            };
            for batch in &mesh.batches {
                let vertices: Vec<yakui::paint::Vertex> = batch
                    .vertices
                    .iter()
                    .map(|&(point, color)| {
                        yakui::paint::Vertex::new(
                            rect.pos() + point * rect.size(),
                            Vec2::ZERO,
                            color,
                        )
                    })
                    .collect();
                ctx.paint.add_mesh(yakui::paint::PaintMesh::new(
                    vertices,
                    batch.indices.clone(),
                ));
            }
        });
    });
}
