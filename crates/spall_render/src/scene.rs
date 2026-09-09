//! A camera, placed meshes, and linear-light material data.

use glam::{Mat4, Vec3};
use spall_mesh::Mesh;

use crate::camera::{Aabb, Camera};

/// One placed mesh.
pub struct SceneItem {
    pub mesh: Mesh,
    /// Volume-local to world transform (identity for terrain, a rigid pose for
    /// a body).
    pub model: Mat4,
    /// Volume-local bounds, for frustum culling before upload.
    pub local_bounds: Aabb,
    pub name: String,
}

impl SceneItem {
    /// Place `mesh` with `model`, computing local bounds from its vertices.
    pub fn new(name: impl Into<String>, mesh: Mesh, model: Mat4) -> Self {
        let mut min = Vec3::splat(f32::INFINITY);
        let mut max = Vec3::splat(f32::NEG_INFINITY);
        for v in &mesh.vertices {
            let p = Vec3::from_array(v.position);
            min = min.min(p);
            max = max.max(p);
        }
        if mesh.vertices.is_empty() {
            min = Vec3::ZERO;
            max = Vec3::ZERO;
        }
        Self {
            mesh,
            model,
            local_bounds: Aabb::new(min, max),
            name: name.into(),
        }
    }

    /// World-space bounds after `model`.
    pub fn world_bounds(&self) -> Aabb {
        self.local_bounds.transformed(self.model)
    }
}

/// A full scene to capture.
pub struct Scene {
    pub camera: Camera,
    pub items: Vec<SceneItem>,
    /// Linear-light PBR material per material id.
    pub materials: Vec<Material>,
    /// Linear clear colour.
    pub clear: [f64; 4],
}

impl Scene {
    pub fn new(camera: Camera) -> Self {
        Self {
            camera,
            items: Vec::new(),
            materials: default_materials(),
            clear: [0.017, 0.03, 0.06, 1.0],
        }
    }

    pub fn with_item(mut self, item: SceneItem) -> Self {
        self.items.push(item);
        self
    }

    /// Combined world bounds of every item, or `None` when the scene is empty.
    pub fn world_bounds(&self) -> Option<Aabb> {
        let mut it = self.items.iter().map(SceneItem::world_bounds);
        let first = it.next()?;
        Some(it.fold(first, |acc, b| {
            Aabb::new(acc.min.min(b.min), acc.max.max(b.max))
        }))
    }

    /// Aim the camera at the scene's bounding sphere from `dir` (a direction
    /// from the target toward the eye), filling the vertical FOV.
    pub fn frame_all(&mut self, dir: Vec3) {
        let Some(bounds) = self.world_bounds() else {
            return;
        };
        let centre = (bounds.min + bounds.max) * 0.5;
        let radius = ((bounds.max - bounds.min) * 0.5).length().max(0.5);
        let dist = radius / (self.camera.fov_y * 0.5).sin().max(1e-3) * 1.15;
        let eye = centre + dir.normalize_or(Vec3::new(0.6, 0.45, 1.0).normalize()) * dist;

        self.camera.position = eye;
        let to_centre = (centre - eye).normalize();
        self.camera.pitch = to_centre.y.clamp(-1.0, 1.0).asin();
        self.camera.yaw = (-to_centre.x).atan2(-to_centre.z);
        // Keep the near/far span tight around the object so the depth debug view
        // has usable contrast.
        self.camera.z_near = (dist - radius * 1.2).max(radius * 0.05).max(0.02);
        self.camera.z_far = dist + radius * 1.4;
    }
}

/// Material properties consumed by the direct-light pass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Material {
    /// Linear (not display/sRGB) reflectance.
    pub base_color: [f32; 3],
    /// Perceptual roughness in `[0, 1]`.
    pub roughness: f32,
    /// Metallic fraction in `[0, 1]`.
    pub metallic: f32,
}

impl Material {
    pub const fn new(base_color: [f32; 3], roughness: f32, metallic: f32) -> Self {
        Self {
            base_color,
            roughness,
            metallic,
        }
    }
}

/// Fixture materials keyed by material id (`0 = air`, `1 = stone`, ...).
/// Real worlds build this table from their material manifest.
pub fn default_materials() -> Vec<Material> {
    vec![
        Material::new([0.0, 0.0, 0.0], 1.0, 0.0),      // 0 air
        Material::new([0.42, 0.44, 0.47], 0.86, 0.0),  // 1 stone
        Material::new([0.36, 0.26, 0.16], 0.94, 0.0),  // 2 dirt
        Material::new([0.20, 0.45, 0.18], 0.90, 0.0),  // 3 grass
        Material::new([0.62, 0.55, 0.38], 0.82, 0.0),  // 4 sand
        Material::new([0.70, 0.20, 0.16], 0.78, 0.0),  // 5 brick
        Material::new([0.14, 0.30, 0.55], 0.28, 0.82), // 6 metal
        Material::new([0.85, 0.85, 0.90], 0.96, 0.0),  // 7 chalk
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_mesh::Vertex;

    fn tri(z: f32) -> Mesh {
        Mesh {
            vertices: vec![
                Vertex {
                    position: [-1.0, -1.0, z],
                    normal: [0.0, 0.0, 1.0],
                    material: 1,
                    ao: 1.0,
                    local_uv: [0.0, 0.0],
                },
                Vertex {
                    position: [1.0, -1.0, z],
                    normal: [0.0, 0.0, 1.0],
                    material: 1,
                    ao: 1.0,
                    local_uv: [1.0, 0.0],
                },
                Vertex {
                    position: [0.0, 1.0, z],
                    normal: [0.0, 0.0, 1.0],
                    material: 1,
                    ao: 1.0,
                    local_uv: [0.5, 1.0],
                },
            ],
            indices: vec![0, 1, 2],
        }
    }

    #[test]
    fn item_bounds_track_the_mesh_and_the_model() {
        let item = SceneItem::new(
            "t",
            tri(0.0),
            Mat4::from_translation(Vec3::new(10.0, 0.0, 0.0)),
        );
        let wb = item.world_bounds();
        assert!((wb.min.x - 9.0).abs() < 1e-5 && (wb.max.x - 11.0).abs() < 1e-5);
    }

    #[test]
    fn frame_all_points_the_camera_at_the_scene() {
        let mut scene = Scene::new(Camera::default())
            .with_item(SceneItem::new("a", tri(0.0), Mat4::IDENTITY))
            .with_item(SceneItem::new(
                "b",
                tri(0.0),
                Mat4::from_translation(Vec3::new(4.0, 0.0, 0.0)),
            ));
        scene.frame_all(Vec3::new(0.0, 0.0, 1.0));

        // Every item is inside the framed frustum.
        let frustum = scene.camera.frustum();
        for item in &scene.items {
            assert!(
                frustum.intersects_aabb(item.world_bounds()),
                "{}",
                item.name
            );
        }
    }
}
