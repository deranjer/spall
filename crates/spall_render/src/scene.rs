//! A camera, placed meshes, and linear-light material data.

use glam::{Mat4, Vec3};
use spall_mesh::Mesh;

use crate::camera::{Aabb, Camera};
use crate::indirect::LightingVolume;
use spall_core::RenderProps;

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
    /// Optional derived occupancy/material cache used only by T13 lighting.
    pub lighting: Option<LightingVolume>,
}

impl Scene {
    pub fn new(camera: Camera) -> Self {
        Self {
            camera,
            items: Vec::new(),
            materials: default_materials(),
            clear: [0.017, 0.03, 0.06, 1.0],
            lighting: None,
        }
    }

    pub fn with_lighting(mut self, lighting: LightingVolume) -> Self {
        self.lighting = Some(lighting);
        self
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
    /// Emissive radiance multiplier, using the material base colour.
    pub emissive: f32,
    /// Surface opacity used by alpha-blended debug geometry. Opaque scene
    /// materials should keep this at `1.0`.
    pub opacity: f32,
    /// Per-voxel colour variation, `0` for none: each 0.25 m cell of an
    /// axis-aligned cube instance gets a deterministic brightness shift of up to
    /// about `+-jitter` and a smaller per-channel drift, from a hash of the
    /// cell. Cosmetic and client-side only (not part of the material manifest).
    pub jitter: f32,
}

impl Material {
    pub const fn new(base_color: [f32; 3], roughness: f32, metallic: f32) -> Self {
        Self {
            base_color,
            roughness,
            metallic,
            emissive: 0.0,
            opacity: 1.0,
            jitter: 0.0,
        }
    }

    pub const fn jitter(mut self, amount: f32) -> Self {
        self.jitter = amount;
        self
    }

    pub const fn emissive(mut self, radiance: f32) -> Self {
        self.emissive = radiance;
        self
    }

    pub const fn opacity(mut self, opacity: f32) -> Self {
        self.opacity = opacity;
        self
    }
}

impl Default for Material {
    fn default() -> Self {
        Self::new([0.5, 0.5, 0.5], 0.8, 0.0)
    }
}

/// The runtime material table for `manifest`, indexed by `MaterialId` (the
/// index the shader uses). Ids the manifest does not define get the loud
/// fallback the shader also uses for out-of-range ids, so a missing entry is
/// visible rather than silently grey.
///
/// Albedo, roughness and metalness map one-to-one. The manifest stores emitted
/// radiance as linear RGB while [`Material`] stores a multiplier on the base
/// colour (the frozen T13 encoding, `emission = base_color * emissive`). The
/// multiplier is the least-squares fit of the radiance onto the albedo, exact
/// for the common case of an emitter tinted like its surface. An emitter with
/// black albedo cannot be expressed that way, so it takes its emission colour
/// as its base colour instead.
pub fn materials_from_manifest(manifest: &spall_core::MaterialManifest) -> Vec<Material> {
    let fallback = Material::new([0.8, 0.1, 0.8], 0.8, 0.0);
    let len = manifest
        .entries()
        .iter()
        .map(|def| usize::from(def.id.0) + 1)
        .max()
        .unwrap_or(1);
    let mut table = vec![fallback; len];
    for def in manifest.entries() {
        let RenderProps {
            albedo,
            roughness,
            metalness,
            emissive,
        } = def.render;
        let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
        let (base_color, multiplier) = if dot(emissive, emissive) <= 0.0 {
            (albedo, 0.0)
        } else if dot(albedo, albedo) <= 1e-12 {
            let peak = emissive.into_iter().fold(0.0_f32, f32::max);
            (emissive.map(|c| c / peak), peak)
        } else {
            (albedo, dot(emissive, albedo) / dot(albedo, albedo))
        };
        table[usize::from(def.id.0)] = Material {
            base_color,
            roughness,
            metallic: metalness,
            emissive: multiplier,
            opacity: 1.0,
            jitter: 0.0,
        };
    }
    table
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
        Material::new([0.72, 0.08, 0.06], 0.88, 0.0),  // 8 red wall
        Material::new([0.06, 0.12, 0.72], 0.88, 0.0),  // 9 blue wall
        Material::new([1.0, 0.42, 0.08], 0.65, 0.0).emissive(5.0), // 10 emitter
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

#[cfg(test)]
mod manifest_tests {
    use super::*;
    use spall_core::{MaterialDef, MaterialFlags, MaterialId, MaterialManifest, SimProps};

    fn def(id: u16, name: &str, render: RenderProps) -> MaterialDef {
        MaterialDef {
            id: MaterialId(id),
            name: name.into(),
            render,
            sim: SimProps {
                density_kg_m3: if id == 0 { 0.0 } else { 1000.0 },
                friction: 0.5,
                restitution: 0.0,
                hardness: 1.0,
                bond_strength: 1.0,
                flags: if id == 0 {
                    MaterialFlags::NONE
                } else {
                    MaterialFlags::OPAQUE
                },
            },
        }
    }

    fn render(albedo: [f32; 3], emissive: [f32; 3]) -> RenderProps {
        RenderProps {
            albedo,
            roughness: 0.4,
            metalness: 0.25,
            emissive,
        }
    }

    #[test]
    fn the_table_is_indexed_by_id_and_fills_gaps_loudly() {
        let manifest = MaterialManifest::validated(vec![
            def(0, "air", render([0.0; 3], [0.0; 3])),
            def(3, "grass", render([0.2, 0.5, 0.1], [0.0; 3])),
        ])
        .unwrap();
        let table = materials_from_manifest(&manifest);
        assert_eq!(table.len(), 4);
        assert_eq!(table[3].base_color, [0.2, 0.5, 0.1]);
        assert_eq!((table[3].roughness, table[3].metallic), (0.4, 0.25));
        assert_eq!(table[3].emissive, 0.0);
        assert_eq!(
            table[1].base_color,
            [0.8, 0.1, 0.8],
            "undefined id is magenta"
        );
    }

    #[test]
    fn emission_round_trips_as_base_colour_times_multiplier() {
        let manifest = MaterialManifest::validated(vec![
            def(0, "air", render([0.0; 3], [0.0; 3])),
            def(1, "lamp", render([1.0, 0.42, 0.08], [5.0, 2.1, 0.4])),
            def(2, "glow", render([0.0; 3], [0.0, 3.0, 1.5])),
        ])
        .unwrap();
        let table = materials_from_manifest(&manifest);
        let emitted = |m: &Material| m.base_color.map(|c| c * m.emissive);
        let lamp = emitted(&table[1]);
        for (got, want) in lamp.into_iter().zip([5.0, 2.1, 0.4]) {
            assert!((got - want).abs() < 1e-4, "{lamp:?}");
        }
        assert_eq!(emitted(&table[2]), [0.0, 3.0, 1.5]);
    }
}
