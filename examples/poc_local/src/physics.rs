//! Very basic, fully hand-rolled physics: gravity, a walk speed applied
//! directly as velocity (no acceleration/friction curve to reason about),
//! and per-axis swept AABB collision against the static block grid. No
//! rapier, no `spall_physics` — deliberately the simplest thing that can
//! move a box around blocks without tunneling or getting stuck.

use crate::world::World;

pub const HALF_WIDTH: f32 = 0.3;
pub const HALF_DEPTH: f32 = 0.3;
pub const HEIGHT: f32 = 1.8;
pub const EYE_HEIGHT: f32 = 1.6;

const GRAVITY_M_S2: f32 = -25.0;
const TERMINAL_VELOCITY_M_S: f32 = -50.0;
const WALK_SPEED_M_S: f32 = 5.0;
const JUMP_SPEED_M_S: f32 = 8.0;
/// Below this the player is treated as having fallen out of the world and
/// is returned to spawn — there's no floor under the grid's edges.
const RESPAWN_Y: f32 = -20.0;

pub struct Player {
    /// Feet position (bottom-center of the collision box).
    pub position: [f32; 3],
    pub velocity: [f32; 3],
    pub grounded: bool,
}

impl Player {
    pub fn new(spawn: [f32; 3]) -> Self {
        Self {
            position: spawn,
            velocity: [0.0, 0.0, 0.0],
            grounded: false,
        }
    }

    /// `wish_local` is `[strafe_right, forward]` in `[-1, 1]`-ish units
    /// (unnormalized diagonal is fine — normalized below); `yaw` rotates it
    /// into world space. `jump_pressed` triggers a jump only while grounded.
    pub fn step(
        &mut self,
        world: &World,
        dt: f32,
        wish_local: [f32; 2],
        yaw: f32,
        jump_pressed: bool,
        spawn: [f32; 3],
    ) {
        let (sin_y, cos_y) = yaw.sin_cos();
        // forward = (sin(yaw), 0, -cos(yaw)); right = (cos(yaw), 0, sin(yaw))
        // — matches the view direction convention used for the camera.
        let mut wish = [
            cos_y * wish_local[0] + sin_y * wish_local[1],
            0.0,
            sin_y * wish_local[0] - cos_y * wish_local[1],
        ];
        let len = (wish[0] * wish[0] + wish[2] * wish[2]).sqrt();
        if len > 1e-5 {
            wish[0] = wish[0] / len * WALK_SPEED_M_S;
            wish[2] = wish[2] / len * WALK_SPEED_M_S;
        }
        self.velocity[0] = wish[0];
        self.velocity[2] = wish[2];

        self.velocity[1] = (self.velocity[1] + GRAVITY_M_S2 * dt).max(TERMINAL_VELOCITY_M_S);
        if self.grounded && jump_pressed {
            self.velocity[1] = JUMP_SPEED_M_S;
            self.grounded = false;
        }

        let mut pos = self.position;
        pos[0] += self.velocity[0] * dt;
        let (pos_x, hit_x) = resolve_axis(world, self.position, pos, 0);
        pos = pos_x;
        if hit_x {
            self.velocity[0] = 0.0;
        }

        pos[2] = self.position[2] + self.velocity[2] * dt;
        let (pos_z, hit_z) = resolve_axis(world, pos_x, pos, 2);
        pos = pos_z;
        if hit_z {
            self.velocity[2] = 0.0;
        }

        pos[1] = pos_z[1] + self.velocity[1] * dt;
        let (pos_y, hit_y) = resolve_axis(world, pos_z, pos, 1);
        pos = pos_y;
        self.grounded = false;
        if hit_y {
            if self.velocity[1] < 0.0 {
                self.grounded = true;
            }
            self.velocity[1] = 0.0;
        }

        self.position = pos;
        if self.position[1] < RESPAWN_Y {
            self.position = spawn;
            self.velocity = [0.0, 0.0, 0.0];
        }
    }
}

/// The player's AABB (min, max) with feet at `position`.
fn aabb_at(position: [f32; 3]) -> ([f32; 3], [f32; 3]) {
    (
        [
            position[0] - HALF_WIDTH,
            position[1],
            position[2] - HALF_DEPTH,
        ],
        [
            position[0] + HALF_WIDTH,
            position[1] + HEIGHT,
            position[2] + HALF_DEPTH,
        ],
    )
}

/// True if the (slightly inset, to avoid false positives from exact edge
/// contact) AABB at `position` overlaps any solid block.
fn collides(world: &World, position: [f32; 3]) -> bool {
    const EPS: f32 = 1e-4;
    let (min, max) = aabb_at(position);
    let (min, max) = (
        [min[0] + EPS, min[1] + EPS, min[2] + EPS],
        [max[0] - EPS, max[1] - EPS, max[2] - EPS],
    );
    let x0 = min[0].floor() as i32;
    let x1 = max[0].floor() as i32;
    let y0 = min[1].floor() as i32;
    let y1 = max[1].floor() as i32;
    let z0 = min[2].floor() as i32;
    let z1 = max[2].floor() as i32;
    for z in z0..=z1 {
        for y in y0..=y1 {
            for x in x0..=x1 {
                if world.is_solid(x, y, z) {
                    return true;
                }
            }
        }
    }
    false
}

/// Moves `old` towards `new` along `axis` only, binary-searching for the
/// last non-colliding point if the full move collides. Returns the
/// resolved position and whether a collision was found along the way.
fn resolve_axis(world: &World, old: [f32; 3], new: [f32; 3], axis: usize) -> ([f32; 3], bool) {
    if !collides(world, new) {
        return (new, false);
    }
    let mut lo = 0.0f32;
    let mut hi = 1.0f32;
    for _ in 0..10 {
        let mid = (lo + hi) * 0.5;
        let mut candidate = old;
        candidate[axis] = old[axis] + (new[axis] - old[axis]) * mid;
        if collides(world, candidate) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    let mut resolved = old;
    resolved[axis] = old[axis] + (new[axis] - old[axis]) * lo;
    (resolved, true)
}
