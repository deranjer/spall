//! A free-fly camera and its frustum, in engine world space (right-handed,
//! `+Y` up, looking along local `-Z`).
//!
//! Projection matrices target wgpu clip space: `x, y in [-1, 1]`, `z in [0, 1]`.
//! Depth-buffer and winding conventions are centralised here so the rest of the
//! renderer never re-derives them.

use glam::camera::rh;
use glam::{Mat4, Vec3, Vec4, Vec4Swizzles};

/// A perspective camera positioned and oriented in world space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    /// Eye position in world metres.
    pub position: Vec3,
    /// Yaw around `+Y`, radians. Zero looks along `-Z`.
    pub yaw: f32,
    /// Pitch around the camera's right axis, radians. Clamped by [`Camera::look`].
    pub pitch: f32,
    /// Vertical field of view, radians.
    pub fov_y: f32,
    /// Viewport aspect ratio (width / height).
    pub aspect: f32,
    /// Near plane distance, metres. The far plane is at infinity (reverse-safe
    /// perspective is not used yet; see [`Camera::projection`]).
    pub z_near: f32,
    /// Far plane distance, metres.
    pub z_far: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            position: Vec3::ZERO,
            yaw: 0.0,
            pitch: 0.0,
            fov_y: 60_f32.to_radians(),
            aspect: 16.0 / 9.0,
            z_near: 0.05,
            z_far: 512.0,
        }
    }
}

const PITCH_LIMIT: f32 = 1.553_343; // ~89 degrees

impl Camera {
    /// Unit forward vector (the direction `-Z` maps to after yaw/pitch).
    pub fn forward(&self) -> Vec3 {
        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        // yaw about +Y, then pitch about the right axis. yaw = 0 => -Z.
        Vec3::new(-sy * cp, sp, -cy * cp).normalize()
    }

    /// Unit right vector.
    pub fn right(&self) -> Vec3 {
        self.forward().cross(Vec3::Y).normalize_or_zero()
    }

    /// Unit up vector.
    pub fn up(&self) -> Vec3 {
        self.right().cross(self.forward()).normalize()
    }

    /// Apply a yaw/pitch delta in radians, clamping pitch away from the poles.
    pub fn look(&mut self, delta_yaw: f32, delta_pitch: f32) {
        self.yaw = (self.yaw + delta_yaw).rem_euclid(std::f32::consts::TAU);
        self.pitch = (self.pitch + delta_pitch).clamp(-PITCH_LIMIT, PITCH_LIMIT);
    }

    /// Move along the camera basis: `x` right, `y` world-up, `z` forward.
    pub fn fly(&mut self, local: Vec3) {
        self.position += self.right() * local.x + Vec3::Y * local.y + self.forward() * local.z;
    }

    /// Right-handed look-at view matrix (world -> Y-up view, `-Z` forward).
    pub fn view(&self) -> Mat4 {
        rh::view::look_to_mat4(self.position, self.forward(), Vec3::Y)
    }

    /// Perspective projection (view -> clip). Targets the wgpu / DirectX NDC:
    /// `z in [0, 1]`, Y-up.
    pub fn projection(&self) -> Mat4 {
        rh::proj::directx::perspective(self.fov_y, self.aspect.max(1e-4), self.z_near, self.z_far)
    }

    /// Combined `projection * view` (world -> clip).
    pub fn view_projection(&self) -> Mat4 {
        self.projection() * self.view()
    }

    /// The view frustum in world space for culling.
    pub fn frustum(&self) -> Frustum {
        Frustum::from_view_projection(self.view_projection())
    }
}

/// An axis-aligned bounding box in world metres.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Aabb {
    pub min: Vec3,
    pub max: Vec3,
}

impl Aabb {
    pub fn new(min: Vec3, max: Vec3) -> Self {
        Self {
            min: min.min(max),
            max: min.max(max),
        }
    }

    /// The box transformed by `m` and re-fitted to an AABB.
    pub fn transformed(&self, m: Mat4) -> Self {
        let mut min = Vec3::splat(f32::INFINITY);
        let mut max = Vec3::splat(f32::NEG_INFINITY);
        for i in 0..8 {
            let corner = Vec3::new(
                if i & 1 == 0 { self.min.x } else { self.max.x },
                if i & 2 == 0 { self.min.y } else { self.max.y },
                if i & 4 == 0 { self.min.z } else { self.max.z },
            );
            let p = m.transform_point3(corner);
            min = min.min(p);
            max = max.max(p);
        }
        Self { min, max }
    }
}

/// Six world-space planes (`x*a + y*b + z*c + d >= 0` inside), extracted from a
/// view-projection matrix.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frustum {
    pub planes: [Vec4; 6],
}

impl Frustum {
    /// Gribb/Hartmann plane extraction from `view_projection`, normalised.
    pub fn from_view_projection(vp: Mat4) -> Self {
        let r = vp.transpose(); // rows as Vec4
        let raw = [
            r.w_axis + r.x_axis, // left
            r.w_axis - r.x_axis, // right
            r.w_axis + r.y_axis, // bottom
            r.w_axis - r.y_axis, // top
            r.w_axis + r.z_axis, // near (wgpu z in [0,1] uses w + z)
            r.w_axis - r.z_axis, // far
        ];
        let mut planes = [Vec4::ZERO; 6];
        for (out, p) in planes.iter_mut().zip(raw) {
            let n = p.xyz().length();
            *out = if n > 0.0 { p / n } else { p };
        }
        Self { planes }
    }

    /// True unless `aabb` is entirely outside at least one plane. Conservative:
    /// a box straddling the frustum edge counts as visible.
    pub fn intersects_aabb(&self, aabb: Aabb) -> bool {
        for plane in self.planes {
            let normal = plane.xyz();
            // The AABB corner furthest along the plane normal.
            let positive = Vec3::new(
                if normal.x >= 0.0 {
                    aabb.max.x
                } else {
                    aabb.min.x
                },
                if normal.y >= 0.0 {
                    aabb.max.y
                } else {
                    aabb.min.y
                },
                if normal.z >= 0.0 {
                    aabb.max.z
                } else {
                    aabb.min.z
                },
            );
            if normal.dot(positive) + plane.w < 0.0 {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cam() -> Camera {
        Camera {
            position: Vec3::new(0.0, 0.0, 5.0),
            aspect: 1.0,
            ..Camera::default()
        }
    }

    #[test]
    fn default_camera_looks_down_negative_z() {
        let c = cam();
        assert!((c.forward() - Vec3::new(0.0, 0.0, -1.0)).length() < 1e-6);
        assert!((c.right() - Vec3::X).length() < 1e-6);
        assert!((c.up() - Vec3::Y).length() < 1e-6);
    }

    #[test]
    fn a_point_ahead_projects_near_the_clip_centre_and_into_the_depth_range() {
        let c = cam();
        let clip = c.view_projection() * glam::Vec4::new(0.0, 0.0, 0.0, 1.0);
        let ndc = clip.xyz() / clip.w;
        assert!(ndc.x.abs() < 1e-5 && ndc.y.abs() < 1e-5);
        assert!(
            (0.0..=1.0).contains(&ndc.z),
            "wgpu depth range, got {}",
            ndc.z
        );
    }

    #[test]
    fn a_point_behind_the_camera_lands_outside_clip() {
        let c = cam();
        let clip = c.view_projection() * glam::Vec4::new(0.0, 0.0, 10.0, 1.0);
        // w <= 0 or |z| outside [0, w] => not visible.
        assert!(clip.w <= 0.0 || clip.z < 0.0 || clip.z > clip.w);
    }

    #[test]
    fn look_clamps_pitch_and_wraps_yaw() {
        let mut c = cam();
        c.look(0.0, 10.0);
        assert!(c.pitch <= PITCH_LIMIT && c.pitch > 1.5);
        c.look(std::f32::consts::TAU * 3.0 + 1.0, 0.0);
        assert!((0.0..std::f32::consts::TAU).contains(&c.yaw));
    }

    #[test]
    fn frustum_accepts_a_box_ahead_and_rejects_one_far_behind() {
        let f = cam().frustum();
        assert!(f.intersects_aabb(Aabb::new(
            Vec3::new(-1.0, -1.0, -1.0),
            Vec3::new(1.0, 1.0, 1.0),
        )));
        assert!(!f.intersects_aabb(Aabb::new(
            Vec3::new(-1.0, -1.0, 50.0),
            Vec3::new(1.0, 1.0, 52.0),
        )));
        // A box off to the far right is culled.
        assert!(!f.intersects_aabb(Aabb::new(
            Vec3::new(500.0, -1.0, -1.0),
            Vec3::new(502.0, 1.0, 1.0),
        )));
    }

    #[test]
    fn linear_eye_space_depth_is_flat_across_a_camera_facing_plane() {
        // The depth debug view (opaque.wgsl mode 2) derives linear depth as
        // `-(view * world_pos).z`. On a plane at constant view-space Z every
        // off-axis point must read the same depth; the old
        // `length(world_pos - camera_pos)` grew toward the edges.
        let c = Camera {
            position: Vec3::new(0.0, 0.0, 5.0),
            aspect: 1.0,
            ..Camera::default()
        };
        let view = c.view();

        let plane_z = -3.0_f32; // 8 m in front of the eye, perpendicular to -Z
        let mut depths = Vec::new();
        let mut radials = Vec::new();
        for x in [-4.0_f32, -2.0, 0.0, 2.0, 4.0] {
            for y in [-4.0_f32, -1.0, 0.0, 1.0, 4.0] {
                let p = Vec3::new(x, y, plane_z);
                depths.push(-view.transform_point3(p).z);
                radials.push((p - c.position).length());
            }
        }

        let d0 = depths[0];
        assert!(
            (d0 - 8.0).abs() < 1e-4,
            "linear depth is the view-axis distance"
        );
        for d in &depths {
            assert!((d - d0).abs() < 1e-4, "depth {d} varies across the plane");
        }
        // The radial distance the buggy shader used is clearly not flat.
        let r_min = radials.iter().cloned().fold(f32::INFINITY, f32::min);
        let r_max = radials.iter().cloned().fold(0.0_f32, f32::max);
        assert!(
            r_max - r_min > 1.5,
            "radial distance spans {r_min}..{r_max}"
        );
    }

    #[test]
    fn linear_eye_space_depth_projects_onto_the_view_axis_under_rotation() {
        // Even with the camera yawed/pitched, a point straight ahead at range R
        // reads linear depth R, and a point of the same range off to the side
        // reads a *smaller* depth (its view-axis projection), never a larger one.
        let mut c = Camera {
            position: Vec3::new(2.0, 1.0, -3.0),
            aspect: 16.0 / 9.0,
            ..Camera::default()
        };
        c.look(0.7, -0.3);
        let view = c.view();
        let fwd = c.forward();

        let ahead = c.position + fwd * 10.0;
        let off = c.position + (fwd + c.right() * 0.3).normalize() * 10.0;

        let depth_ahead = -view.transform_point3(ahead).z;
        let depth_off = -view.transform_point3(off).z;
        assert!((depth_ahead - 10.0).abs() < 1e-3);
        assert!(depth_off < depth_ahead && depth_off > 9.0);
    }

    #[test]
    fn aabb_transformed_by_rotation_refits() {
        let b = Aabb::new(Vec3::splat(-1.0), Vec3::splat(1.0));
        let r = Mat4::from_rotation_y(std::f32::consts::FRAC_PI_4);
        let t = b.transformed(r);
        // A 2x2x2 cube rotated 45 deg about Y widens to ~2*sqrt(2) on X and Z.
        assert!((t.max.x - std::f32::consts::SQRT_2).abs() < 1e-5);
        assert!((t.max.y - 1.0).abs() < 1e-5);
    }
}
