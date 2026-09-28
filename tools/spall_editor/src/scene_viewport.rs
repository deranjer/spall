//! The scene viewport: geometry is drawn by `spall_render`'s shadow/HDR/tone-map
//! passes into a texture, and yakui only composites that texture plus a few
//! vector overlays. Meshing and uploads happen when the scene changes, not per
//! frame, and picking casts a ray through the voxel grid.

use std::sync::{Arc, Mutex};

use glam::Vec3;
use spall_editor::scene_mesh::{SceneMesh, build_scene_mesh};
use spall_render::{Camera, MeshChunk, ViewportFrame};
use yakui::{Vec2, Vec4};

use crate::{
    EditorApp, PreviewCamera, SceneMeshKey, SceneMeshState, VoxelCoord, Workspace, draw_wire_box,
};

/// Pitch limit for the perspective orbit camera, just short of the poles.
pub(crate) const ORBIT_PITCH_LIMIT: f32 = 1.5;

impl PreviewCamera {
    /// Pitch as the orbit camera sees it: wrapped into `(-pi, pi]`, then held
    /// away from the poles. The asset preview still wraps freely.
    fn orbit_pitch(&self) -> f32 {
        let wrapped = (self.pitch + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU)
            - std::f32::consts::PI;
        wrapped.clamp(-ORBIT_PITCH_LIMIT, ORBIT_PITCH_LIMIT)
    }
}

/// Orbit camera around the scene bounds. `view.pan` is in screen pixels and
/// shifts the target so the scene follows the pointer; `view.zoom` scales the
/// distance. `size` is the viewport in layout units.
pub(crate) fn scene_camera(view: &PreviewCamera, bounds: (Vec3, Vec3), size: Vec2) -> Camera {
    let center = (bounds.0 + bounds.1) * 0.5;
    let radius = ((bounds.1 - bounds.0) * 0.5).length().max(0.25);
    let mut camera = Camera {
        aspect: size.x.max(1.0) / size.y.max(1.0),
        ..Camera::default()
    };
    // Dragging right must swing the scene right, so the camera swings left.
    camera.yaw = -view.yaw;
    camera.pitch = -view.orbit_pitch();
    let half_fov = camera.fov_y * 0.5;
    let fit = radius / half_fov.sin() / camera.aspect.min(1.0) * 1.1;
    let distance = fit / view.zoom.max(0.01);
    let metres_per_pixel = 2.0 * distance * half_fov.tan() / size.y.max(1.0);
    let target = center - camera.right() * view.pan.x * metres_per_pixel
        + camera.up() * view.pan.y * metres_per_pixel;
    camera.position = target - camera.forward() * distance;
    camera.z_near = (distance * 0.05).max(0.005);
    camera.z_far = distance + radius * 3.0;
    camera
}

/// Everything the viewport canvas needs, owned so the paint closure can be
/// `'static`.
pub(crate) struct SceneCanvas {
    pub mesh: Arc<SceneMesh>,
    pub texture: yakui::TextureId,
    pub camera: PreviewCamera,
    pub cursor: Option<Vec2>,
    pub hover_cell: Arc<Mutex<Option<VoxelCoord>>>,
    pub rect_out: Arc<Mutex<Option<(Vec2, Vec2)>>>,
    pub selected_voxel: Option<VoxelCoord>,
    pub background: [u8; 3],
}

/// Fill the viewport with the engine-rendered texture, then overlay the
/// selection and hover boxes and update the hovered cell by ray picking.
pub(crate) fn draw_scene_canvas(scene: SceneCanvas) {
    yakui::expanded(|| {
        let [r, g, b] = scene.background;
        yakui::colored_box_container(yakui::Color::rgb(r, g, b), || {
            // Canvas meshes do not clip themselves; a non-scrolling Scrollable
            // clips the overlays to the viewport.
            yakui::expanded(|| {
                yakui::widgets::Scrollable::none().show(|| {
                    yakui::canvas(move |ctx| paint_scene(&scene, ctx));
                });
            });
        });
    });
}

fn paint_scene(scene: &SceneCanvas, ctx: &mut yakui::widget::PaintContext<'_>) {
    let widget = ctx.dom.current();
    let Some(rect) = ctx.layout.get(widget).map(|layout| layout.rect) else {
        return;
    };
    if let Ok(mut out) = scene.rect_out.lock() {
        *out = Some((rect.pos(), rect.size()));
    }
    let Some(bounds) = scene.mesh.bounds else {
        return;
    };

    let (min, max) = (rect.pos(), rect.max());
    let corner = |x: f32, y: f32, u: f32, v: f32| {
        yakui::paint::Vertex::new(Vec2::new(x, y), Vec2::new(u, v), Vec4::ONE)
    };
    let mut quad = yakui::paint::PaintMesh::new(
        [
            corner(min.x, min.y, 0.0, 0.0),
            corner(max.x, min.y, 1.0, 0.0),
            corner(max.x, max.y, 1.0, 1.0),
            corner(min.x, max.y, 0.0, 1.0),
        ],
        [0_u16, 1, 2, 0, 2, 3],
    );
    quad.texture = Some((scene.texture, yakui::geometry::Rect::ONE));
    ctx.paint.add_mesh(quad);

    let camera = scene_camera(&scene.camera, bounds, rect.size());
    let metres = scene.mesh.cell_metres;
    // Cell-space (grid units) to layout-space point, for the wire boxes.
    let screen = |x: f32, y: f32, z: f32| {
        camera
            .project(Vec3::new(x, y, z) * metres)
            .map_or(Vec2::splat(-1.0e6), |ndc| {
                Vec2::new(
                    min.x + (ndc[0] * 0.5 + 0.5) * rect.size().x,
                    min.y + (0.5 - ndc[1] * 0.5) * rect.size().y,
                )
            })
    };

    let inside = |p: Vec2| p.x >= min.x && p.x <= max.x && p.y >= min.y && p.y <= max.y;
    let hovered = scene.cursor.filter(|p| inside(*p)).and_then(|p| {
        let ndc = [
            (p.x - min.x) / rect.size().x * 2.0 - 1.0,
            1.0 - (p.y - min.y) / rect.size().y * 2.0,
        ];
        let (origin, dir) = camera.ray(ndc);
        scene.mesh.pick(origin, dir)
    });
    if let Ok(mut current) = scene.hover_cell.lock() {
        *current = hovered;
    }
    if let Some(cell) = hovered {
        draw_wire_box(
            cell,
            cell,
            &screen,
            Vec4::new(1.0, 1.0, 1.0, 0.85),
            1.5,
            ctx,
        );
    }
    if let Some(cell) = scene.selected_voxel {
        draw_wire_box(
            cell,
            cell,
            &screen,
            Vec4::new(1.0, 0.82, 0.16, 1.0),
            2.5,
            ctx,
        );
    }
}

impl EditorApp {
    /// The scene mesh for the current scene, selection and boundary setting,
    /// rebuilt only when one of those changed.
    pub(crate) fn current_scene_mesh(&mut self) -> Option<Arc<SceneMesh>> {
        let (composite, owners) = self.scene_view.as_ref()?;
        let key = SceneMeshKey {
            revision: self.scene_revision,
            selected: self.selected_entity,
            grid_lines: self.show_voxel_boundaries,
        };
        if self
            .scene_mesh
            .as_ref()
            .is_none_or(|state| state.key != key)
        {
            let mesh = build_scene_mesh(composite, owners, key.selected, key.grid_lines);
            self.scene_mesh = Some(SceneMeshState {
                key,
                mesh: Arc::new(mesh),
                uploaded: false,
            });
        }
        self.scene_mesh.as_ref().map(|state| state.mesh.clone())
    }

    /// Render the scene into the viewport texture. Runs after layout and
    /// before yakui paints, so the texture the canvas samples is current.
    pub(crate) fn sync_scene_viewport(&mut self) {
        if self.workspace != Workspace::Scene || self.model.is_none() {
            return;
        }
        let Some(state) = self.scene_mesh.as_mut() else {
            return;
        };
        let Some(bounds) = state.mesh.bounds else {
            return;
        };
        let Some((_, size)) = self.scene_viewport_rect.lock().ok().and_then(|rect| *rect) else {
            return;
        };
        let Some(gpu) = self.gpu.as_mut() else {
            return;
        };

        if !state.uploaded {
            gpu.viewport
                .set_materials(&gpu.context, &state.mesh.materials);
            let chunks: Vec<_> = state
                .mesh
                .chunks
                .iter()
                .map(|chunk| MeshChunk {
                    vertices: &chunk.vertices,
                    indices: &chunk.indices,
                })
                .collect();
            match gpu.viewport.set_meshes(&gpu.context, &chunks) {
                Ok(()) => {
                    state.uploaded = true;
                    let occupancy = state.mesh.sky_occupancy();
                    gpu.viewport
                        .set_sky_occupancy(&gpu.context, occupancy.as_ref());
                }
                Err(error) => {
                    self.status = format!("Scene viewport upload failed: {error}");
                    return;
                }
            }
        }

        let pixels = size * gpu.ui_scale;
        if gpu.viewport.resize(
            &gpu.context,
            pixels.x.round().max(1.0) as u32,
            pixels.y.round().max(1.0) as u32,
        ) {
            gpu.yakui
                .update_texture(gpu.viewport_texture, gpu.viewport.color_view().clone());
        }

        gpu.viewport.render(
            &gpu.context,
            &ViewportFrame {
                camera: scene_camera(&self.preview_camera, bounds, size),
                environment: self.preview_environment.environment(),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> PreviewCamera {
        PreviewCamera::default()
    }

    const BOUNDS: (Vec3, Vec3) = (Vec3::new(-2.0, 0.0, -2.0), Vec3::new(2.0, 3.0, 2.0));

    #[test]
    fn the_default_view_looks_at_the_scene_centre_from_above() {
        let camera = scene_camera(&view(), BOUNDS, Vec2::new(800.0, 600.0));
        let centre = (BOUNDS.0 + BOUNDS.1) * 0.5;
        let ndc = camera.project(centre).expect("centre in front");
        assert!(ndc[0].abs() < 1e-4 && ndc[1].abs() < 1e-4, "{ndc:?}");
        assert!(camera.position.y > centre.y, "orbit starts above the scene");
    }

    #[test]
    fn zoom_moves_the_camera_closer_without_changing_the_target() {
        let near = scene_camera(
            &PreviewCamera {
                zoom: 4.0,
                ..view()
            },
            BOUNDS,
            Vec2::new(800.0, 600.0),
        );
        let far = scene_camera(&view(), BOUNDS, Vec2::new(800.0, 600.0));
        let centre = (BOUNDS.0 + BOUNDS.1) * 0.5;
        assert!((near.position - centre).length() < (far.position - centre).length());
        assert!(near.project(centre).unwrap()[0].abs() < 1e-4);
    }

    #[test]
    fn panning_moves_the_scene_with_the_pointer() {
        let camera = scene_camera(
            &PreviewCamera {
                pan: Vec2::new(100.0, 50.0),
                ..view()
            },
            BOUNDS,
            Vec2::new(800.0, 600.0),
        );
        let centre = (BOUNDS.0 + BOUNDS.1) * 0.5;
        let ndc = camera.project(centre).unwrap();
        // 100 px right of 400 is +0.25 NDC; 50 px down of 300 is -0.1667.
        assert!((ndc[0] - 0.25).abs() < 0.01, "{ndc:?}");
        assert!((ndc[1] + 1.0 / 6.0).abs() < 0.01, "{ndc:?}");
    }

    #[test]
    fn orbit_pitch_stays_off_the_poles_even_after_the_asset_preview_wraps_it() {
        for pitch in [-7.0, -3.0, 0.0, 1.9, 3.5, 6.0] {
            let p = PreviewCamera { pitch, ..view() }.orbit_pitch();
            assert!(p.abs() <= ORBIT_PITCH_LIMIT, "{pitch} -> {p}");
        }
    }
}
