//! Small deterministic T13 colored-room fixtures.

use glam::{IVec3, Mat4, Vec3};
use spall_mesh::{Mesh, MeshStats, MeshStrategy, Vertex};

use crate::camera::Camera;
use crate::indirect::LightingVolume;
use crate::scene::{Scene, SceneItem, default_materials};

#[derive(Debug, Clone, Copy)]
pub struct LightingFixtureMetrics {
    pub open_probe_luminance: f32,
    pub closed_probe_luminance: f32,
    /// Residual emitter contribution behind one represented 0.5 m wall,
    /// divided by the unobstructed emitter contribution.
    pub thin_wall_leakage_ratio: f32,
}

pub struct LightingFixture {
    pub name: &'static str,
    pub scene: Scene,
    pub mesh_stats: MeshStats,
    pub metrics: LightingFixtureMetrics,
}

pub fn colored_rooms(aspect: f32) -> Vec<LightingFixture> {
    let materials = default_materials();
    let (open_volume, closed_volume) = room_volumes();
    let open_probe = open_volume.probe_radiance(Vec3::new(0.0, 2.0, 0.0), &materials);
    let closed_probe = closed_volume.probe_radiance(Vec3::new(0.0, 2.0, 0.0), &materials);
    let metrics = LightingFixtureMetrics {
        open_probe_luminance: luminance(open_probe),
        closed_probe_luminance: luminance(closed_probe),
        thin_wall_leakage_ratio: thin_wall_leakage(&materials),
    };

    vec![
        room_fixture("colored_room_open", false, aspect, open_volume, metrics),
        room_fixture("colored_room_closed", true, aspect, closed_volume, metrics),
    ]
}

/// Three scenes that share one non-emissive receiver wall, camera, and
/// `128^3` cache. They differ only in the emissive term of the represented
/// panel and in one represented `0.5 m` occluder, so a rendered
/// `DebugView::IndirectOnly` capture isolates transported emitter radiance on
/// a non-emissive receiver — and its leakage past the wall — entirely through
/// the GPU trace, GPU denoise, and surface-sample path rather than the CPU
/// probe copy.
pub struct EmitterOcclusionScenes {
    /// Emitter represented, no occluder: the receiver band sees the panel.
    pub lit: Scene,
    /// Emitter represented, one `0.5 m` wall cell column between panel and band.
    pub occluded: Scene,
    /// Panel cells set to plain stone: the sky/ambient floor with no transport.
    pub dark: Scene,
    /// Fractional image-x span `[x0, x1)` that views only receiver wall the
    /// occluder shadows (left of the panel and of the occluder column).
    pub receiver_band: [f32; 2],
}

pub fn emitter_occlusion_scenes(aspect: f32) -> EmitterOcclusionScenes {
    let origin = Vec3::splat(-32.0);
    let camera = Camera {
        position: Vec3::new(0.0, 2.5, 4.0),
        yaw: 0.0,
        pitch: 0.0,
        aspect,
        fov_y: 55_f32.to_radians(),
        z_near: 0.1,
        z_far: 40.0,
    };

    let mut receiver = Mesh::default();
    quad(
        &mut receiver,
        [
            Vec3::new(-6.0, 0.0, -5.75),
            Vec3::new(6.0, 0.0, -5.75),
            Vec3::new(6.0, 5.0, -5.75),
            Vec3::new(-6.0, 5.0, -5.75),
        ],
        Vec3::Z,
        1,
    );
    let mut panel = Mesh::default();
    quad(
        &mut panel,
        [
            Vec3::new(5.5, 1.0, -6.0),
            Vec3::new(5.5, 1.0, -4.0),
            Vec3::new(5.5, 4.0, -4.0),
            Vec3::new(5.5, 4.0, -6.0),
        ],
        -Vec3::X,
        10,
    );

    let volume = |panel_material: u32, occluder: bool| {
        let mut v = LightingVolume::empty(origin);
        // Receiver wall shell, one cache cell thick behind the raster quad.
        fill_world_box(
            &mut v,
            Vec3::new(-6.0, 0.0, -6.0),
            Vec3::new(6.0, 5.0, -5.5),
            1,
        );
        // Emitter panel on the far +X side, outside the measured band.
        fill_world_box(
            &mut v,
            Vec3::new(5.5, 1.0, -6.0),
            Vec3::new(6.0, 4.0, -4.0),
            panel_material,
        );
        if occluder {
            // One represented 0.5 m wall between the panel and the band.
            fill_world_box(
                &mut v,
                Vec3::new(2.0, 0.0, -6.0),
                Vec3::new(2.5, 5.0, -2.0),
                1,
            );
        }
        v
    };

    let build = |lighting: LightingVolume| {
        let mut scene = Scene::new(camera)
            .with_item(SceneItem::new("receiver", receiver.clone(), Mat4::IDENTITY))
            .with_item(SceneItem::new("panel", panel.clone(), Mat4::IDENTITY))
            .with_lighting(lighting);
        scene.materials = default_materials();
        scene.clear = [0.003, 0.004, 0.008, 1.0];
        scene
    };

    EmitterOcclusionScenes {
        lit: build(volume(10, false)),
        occluded: build(volume(10, true)),
        dark: build(volume(1, false)),
        receiver_band: [0.10, 0.42],
    }
}

fn room_fixture(
    name: &'static str,
    closed: bool,
    aspect: f32,
    lighting: LightingVolume,
    metrics: LightingFixtureMetrics,
) -> LightingFixture {
    let mesh = room_mesh(closed);
    let stats = MeshStats {
        strategy: MeshStrategy::Greedy,
        quad_count: mesh.indices.len() / 6,
        vertex_count: mesh.vertices.len(),
        triangle_count: mesh.indices.len() / 3,
        surface_area_m2: if closed { 176.0 } else { 112.0 },
        exposed_unit_faces: 0,
        unresolved_halo_faces: 0,
    };
    let camera = Camera {
        position: Vec3::new(0.0, 2.0, 3.2),
        yaw: 0.0,
        pitch: -0.03,
        aspect,
        fov_y: 65_f32.to_radians(),
        z_near: 0.1,
        z_far: 20.0,
    };
    let mut scene = Scene::new(camera)
        .with_item(SceneItem::new(name, mesh, Mat4::IDENTITY))
        .with_lighting(lighting);
    scene.materials = default_materials();
    scene.clear = [0.003, 0.004, 0.008, 1.0];
    LightingFixture {
        name,
        scene,
        mesh_stats: stats,
        metrics,
    }
}

fn room_volumes() -> (LightingVolume, LightingVolume) {
    let origin = Vec3::splat(-32.0);
    let mut open = LightingVolume::empty(origin);
    fill_room(&mut open, false);
    let mut closed = LightingVolume::empty(origin);
    fill_room(&mut closed, true);
    (open, closed)
}

fn fill_room(volume: &mut LightingVolume, closed: bool) {
    // 8 x 4 x 8 metre room; one clipmap cell (0.5 m) per wall.
    fill_world_box(
        volume,
        Vec3::new(-4.0, -0.5, -4.0),
        Vec3::new(4.0, 0.0, 4.0),
        1,
    );
    fill_world_box(
        volume,
        Vec3::new(-4.0, 0.0, -4.0),
        Vec3::new(-3.5, 4.0, 4.0),
        8,
    );
    fill_world_box(
        volume,
        Vec3::new(3.5, 0.0, -4.0),
        Vec3::new(4.0, 4.0, 4.0),
        9,
    );
    fill_world_box(
        volume,
        Vec3::new(-4.0, 0.0, -4.0),
        Vec3::new(4.0, 4.0, -3.5),
        1,
    );
    fill_world_box(
        volume,
        Vec3::new(-1.0, 1.0, -3.5),
        Vec3::new(1.0, 3.0, -3.0),
        10,
    );
    if closed {
        fill_world_box(
            volume,
            Vec3::new(-4.0, 4.0, -4.0),
            Vec3::new(4.0, 4.5, 4.0),
            1,
        );
        fill_world_box(
            volume,
            Vec3::new(-4.0, 0.0, 3.5),
            Vec3::new(4.0, 4.0, 4.0),
            1,
        );
    }
}

fn fill_world_box(volume: &mut LightingVolume, min: Vec3, max: Vec3, material: u32) {
    let lo = volume.world_to_cell(min);
    let hi = volume.world_to_cell(max - Vec3::splat(1.0e-4)) + IVec3::ONE;
    volume.fill_box(lo, hi, material);
}

fn room_mesh(closed: bool) -> Mesh {
    let mut mesh = Mesh::default();
    quad(
        &mut mesh,
        [
            Vec3::new(-4.0, 0.0, -4.0),
            Vec3::new(-4.0, 0.0, 4.0),
            Vec3::new(4.0, 0.0, 4.0),
            Vec3::new(4.0, 0.0, -4.0),
        ],
        Vec3::Y,
        1,
    );
    quad(
        &mut mesh,
        [
            Vec3::new(-4.0, 0.0, -4.0),
            Vec3::new(4.0, 0.0, -4.0),
            Vec3::new(4.0, 4.0, -4.0),
            Vec3::new(-4.0, 4.0, -4.0),
        ],
        Vec3::Z,
        1,
    );
    quad(
        &mut mesh,
        [
            Vec3::new(-4.0, 0.0, 4.0),
            Vec3::new(-4.0, 0.0, -4.0),
            Vec3::new(-4.0, 4.0, -4.0),
            Vec3::new(-4.0, 4.0, 4.0),
        ],
        Vec3::X,
        8,
    );
    quad(
        &mut mesh,
        [
            Vec3::new(4.0, 0.0, -4.0),
            Vec3::new(4.0, 0.0, 4.0),
            Vec3::new(4.0, 4.0, 4.0),
            Vec3::new(4.0, 4.0, -4.0),
        ],
        -Vec3::X,
        9,
    );
    quad(
        &mut mesh,
        [
            Vec3::new(-1.0, 1.0, -3.98),
            Vec3::new(1.0, 1.0, -3.98),
            Vec3::new(1.0, 3.0, -3.98),
            Vec3::new(-1.0, 3.0, -3.98),
        ],
        Vec3::Z,
        10,
    );
    if closed {
        quad(
            &mut mesh,
            [
                Vec3::new(-4.0, 4.0, 4.0),
                Vec3::new(-4.0, 4.0, -4.0),
                Vec3::new(4.0, 4.0, -4.0),
                Vec3::new(4.0, 4.0, 4.0),
            ],
            -Vec3::Y,
            1,
        );
    }
    mesh
}

fn quad(mesh: &mut Mesh, points: [Vec3; 4], normal: Vec3, material: u32) {
    let base = mesh.vertices.len() as u32;
    for (i, point) in points.into_iter().enumerate() {
        mesh.vertices.push(Vertex {
            position: point.to_array(),
            normal: normal.to_array(),
            material,
            ao: 1.0,
            local_uv: match i {
                0 => [0.0, 0.0],
                1 => [1.0, 0.0],
                2 => [1.0, 1.0],
                _ => [0.0, 1.0],
            },
        });
    }
    mesh.indices
        .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
}

fn luminance(rgb: Vec3) -> f32 {
    rgb.dot(Vec3::new(0.2126, 0.7152, 0.0722))
}

fn thin_wall_leakage(materials: &[crate::scene::Material]) -> f32 {
    let origin = Vec3::splat(-32.0);
    let mut unblocked = LightingVolume::empty(origin);
    fill_world_box(
        &mut unblocked,
        Vec3::new(3.5, 0.5, -1.0),
        Vec3::new(4.0, 3.5, 1.0),
        10,
    );
    let mut blocked = unblocked.clone();
    fill_world_box(
        &mut blocked,
        Vec3::new(1.5, 0.0, -2.0),
        Vec3::new(2.0, 4.0, 2.0),
        1,
    );
    let mut baseline = LightingVolume::empty(origin);
    fill_world_box(
        &mut baseline,
        Vec3::new(3.5, 0.5, -1.0),
        Vec3::new(4.0, 3.5, 1.0),
        1,
    );
    let point = Vec3::new(0.0, 2.0, 0.0);
    let direct = (luminance(unblocked.probe_radiance(point, materials))
        - luminance(baseline.probe_radiance(point, materials)))
    .max(1.0e-6);
    let leaked = (luminance(blocked.probe_radiance(point, materials))
        - luminance(baseline.probe_radiance(point, materials)))
    .max(0.0);
    (leaked / direct).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_measures_closed_room_and_thin_wall_behavior() {
        let fixtures = colored_rooms(16.0 / 9.0);
        let metrics = fixtures[0].metrics;
        assert!(metrics.closed_probe_luminance < metrics.open_probe_luminance);
        assert!(
            metrics.thin_wall_leakage_ratio <= 0.05,
            "leak={}",
            metrics.thin_wall_leakage_ratio
        );
        assert_eq!(
            fixtures[0].scene.lighting.as_ref().unwrap().cells().len(),
            128usize.pow(3)
        );
    }
}
