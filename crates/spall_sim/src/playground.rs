//! Debris population for the interactive playground scene (`cargo xtask play
//! --scene playground`): distinct pre-staged bodies for two visible emitters
//! near the player spawn. One drops varied push-test blocks every five seconds;
//! the other feeds small bouncy blocks into the Plinko board every second.
//!
//! **Populated once, at scene construction** (`Scene::simulation`), not
//! per-tick. An earlier version of this spawned one new body every ~0.75 s
//! during the live tick loop, the same shape as a per-tick gameplay pass
//! (`spall_sim::contact_damage`, `spall_sim::dormancy`) -- but a body
//! `SimWorld::spawn_body` creates bypasses the ordinary commit/transaction
//! pipeline entirely (the same fact `spall_sim::fixtures::spawn_g4_workload_bodies`'s
//! own doc comment and `fixtures/scenarios/t23-g4-workload.json`'s
//! description already record for G4's debris), so **no already-connected
//! client -- not even a late-joiner -- ever learns a new body exists** once
//! its own baseline pull has already happened. A per-tick spawner therefore
//! produces bodies a real player can never see: correctly simulated,
//! correctly reconciled by the client's own bookkeeping, never once drawn,
//! because the client was never told they exist. Every body must exist
//! before the world is handed to `Simulation::new` so it is part of what a
//! late-join baseline actually captures -- the exact same constraint G4's
//! debris population, and G1's "moving hollow test volume", already live
//! with.
//!
//! To still get an ongoing "something drops in while you play" effect
//! within that constraint, [`populate`] spawns every body **dormant** (T21,
//! frozen in place, no physics-step cost) and [`DropSchedule`] wakes one at
//! a time on a real interval during the live tick loop. Reactivating an
//! *existing*, already-known body is an ordinary state change -- unlike
//! creating a new one, it replicates to already-connected clients through
//! the completely normal motion-snapshot path (see [`DropSchedule`]'s own
//! doc), so this sidesteps the constraint above instead of fighting it.
//!
//! Deliberately **not** a gate pass and deliberately **not** deterministic
//! (seeded from wall-clock time): this exists purely so a person can get a
//! hands-on feel for destruction and physics, not as reproducible evidence.
//! Every other fixture/pass in `spall_sim` stays RNG-free on purpose; this is
//! the one exception, confined to its own module and only ever wired in for
//! `Scene::Playground`.

use glam::DQuat;
use spall_core::{CellSizeCode, GlobalCell, MaterialId, VolumeId};
use spall_voxel::{EditPlan, Volume};

use crate::body::BodyPose;
use crate::fixtures::{
    DEBRIS_BLUE, DEBRIS_GREEN, DEBRIS_ORANGE, DEBRIS_PURPLE, DEBRIS_RED, DEBRIS_YELLOW,
};
use crate::world::SimWorld;

/// Bright, distinct debris colours (`spall_sim::fixtures::playground_manifest`)
/// -- deliberately not the muted structural palette the rest of the
/// playground terrain uses, so falling debris reads clearly against it.
const DEBRIS_MATERIALS: [MaterialId; 6] = [
    DEBRIS_RED,
    DEBRIS_ORANGE,
    DEBRIS_YELLOW,
    DEBRIS_GREEN,
    DEBRIS_BLUE,
    DEBRIS_PURPLE,
];

/// Pending bodies live well below the playable envelope, so their baseline
/// geometry is known to clients without a sky full of frozen future drops.
const STAGING_Y_M: f64 = -80.0;

/// A body-local solid box, `dims` cells on a side (not necessarily a cube --
/// unlike [`crate::fixtures::solid_block`]), painted `material`. The
/// spawner's own reason to want this: "random size/shaped" debris in more
/// than one colour.
fn solid_box(dims: [i64; 3], material: MaterialId) -> impl FnOnce(VolumeId) -> Volume {
    move |id| {
        let mut v = Volume::new(id, CellSizeCode::Quarter);
        v.apply_edit(&EditPlan::filled_box(
            id,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(dims[0] - 1, dims[1] - 1, dims[2] - 1),
            material,
        ))
        .expect("playground debris body is a well-formed box");
        v
    }
}

/// Spawns one dynamic 0.75 m box resting on the walk arena's floor, 3 m ahead
/// of the first player spawn along +x, for the deterministic one-box push repro
/// (`Scene::PushTest`). Returns its entity.
pub fn spawn_push_test_box(world: &mut SimWorld) -> Option<spall_core::EntityId> {
    world
        .spawn_body(
            solid_box([3, 3, 3], DEBRIS_MATERIALS[0]),
            BodyPose::new(DQuat::IDENTITY, [4.0, 1.0, 1.125]),
            [0.0; 3],
            [0.0; 3],
            2000.0,
            0,
        )
        .ok()
}

/// A tiny splitmix64 generator. Not for anything needing real randomness
/// quality or reproducibility -- just cheap variety for a play session.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // splitmix64 rejects an all-zero state (would emit a constant
        // stream); a fixed non-zero fallback keeps `new` infallible.
        Self(if seed == 0 {
            0x2026_0918_0000_0001
        } else {
            seed
        })
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform `[lo, hi)`.
    fn range_f64(&mut self, lo: f64, hi: f64) -> f64 {
        let u = (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        lo + u * (hi - lo)
    }

    /// Uniform `[lo, hi]`, inclusive both ends.
    fn range_i64(&mut self, lo: i64, hi_inclusive: i64) -> i64 {
        let span = (hi_inclusive - lo + 1).max(1) as u64;
        lo + (self.next_u64() % span) as i64
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[(self.next_u64() as usize) % items.len()]
    }
}

/// One rectangular area debris can drop into: a random `(x, z)` inside the
/// ranges, always released at `drop_y_m`.
#[derive(Debug, Clone, Copy)]
pub struct DropZone {
    pub x_range_m: (f64, f64),
    pub z_range_m: (f64, f64),
    pub drop_y_m: f64,
}

/// The two emitters beside [`crate::fixtures::PLAYGROUND_SPAWNS`]. The first is
/// the five-second mass/pushing demonstration; the second is centred over the
/// Plinko board built into `spall_voxel::fixtures::playground_scene`.
pub fn playground_drop_zones() -> Vec<DropZone> {
    vec![
        DropZone {
            x_range_m: (10.5, 12.5),
            z_range_m: (14.0, 18.0),
            drop_y_m: 7.0,
        },
        DropZone {
            x_range_m: (23.25, 25.75),
            z_range_m: (20.75, 21.25),
            drop_y_m: 10.5,
        },
    ]
}

/// Three minutes of five-second push-test drops.
pub const PLAYGROUND_SHOWCASE_COUNT: usize = 36;
/// Three minutes of one-per-second Plinko drops.
pub const PLAYGROUND_PLINKO_COUNT: usize = 180;
pub const PLAYGROUND_DEBRIS_COUNT: usize = PLAYGROUND_SHOWCASE_COUNT + PLAYGROUND_PLINKO_COUNT;

/// The two independently scheduled pools created before clients join.
pub struct PlaygroundDropPools {
    pub showcase: Vec<spall_core::EntityId>,
    pub plinko: Vec<spall_core::EntityId>,
}

/// Spawns [`PLAYGROUND_DEBRIS_COUNT`] randomly sized/shaped/coloured debris
/// bodies, split across `zones` (typically [`playground_drop_zones`]) at a
/// random `(x, z)` within its emitter lane and at [`STAGING_Y_M`] -- then
/// **immediately deactivates every one** (T21 dormancy,
/// `SimWorld::deactivate_body`): frozen in place, no physics-step cost,
/// same as G4's own sleeping population. Must be called before the world is
/// handed to `Simulation::new`/the tick loop starts, same as every other
/// out-of-band population in this codebase (see this module's doc comment).
///
/// Returns separate pools for [`DropSchedule`] to move to each zone's
/// `drop_y_m` and wake one at a time. Staging below the playable scene keeps
/// future blocks out of view while still including their topology in every
/// connected client's initial baseline.
pub fn populate(world: &mut SimWorld, zones: &[DropZone]) -> PlaygroundDropPools {
    if zones.len() < 2 {
        return PlaygroundDropPools {
            showcase: Vec::new(),
            plinko: Vec::new(),
        };
    }
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut rng = Rng::new(seed);
    let mut showcase = Vec::with_capacity(PLAYGROUND_SHOWCASE_COUNT);
    let mut plinko = Vec::with_capacity(PLAYGROUND_PLINKO_COUNT);
    for index in 0..PLAYGROUND_DEBRIS_COUNT {
        let is_plinko = index >= PLAYGROUND_SHOWCASE_COUNT;
        let zone = zones[usize::from(is_plinko)];
        let x = rng.range_f64(zone.x_range_m.0, zone.x_range_m.1);
        let z = rng.range_f64(zone.z_range_m.0, zone.z_range_m.1);
        let dims = if is_plinko {
            [2, 2, 2]
        } else {
            [
                rng.range_i64(1, 5),
                rng.range_i64(1, 5),
                rng.range_i64(1, 5),
            ]
        };
        let material = *rng.pick(&DEBRIS_MATERIALS);
        let yaw = rng.range_f64(0.0, std::f64::consts::TAU);
        let pose = BodyPose::new(DQuat::from_rotation_y(yaw), [x, STAGING_Y_M, z]);
        // A malformed spawn (should not happen -- dims are always >= 1, the
        // position always finite) is not worth failing the whole population
        // over; just skip it and keep going.
        let Ok(entity) = world.spawn_body(
            solid_box(dims, material),
            pose,
            [0.0; 3],
            [0.0; 3],
            2000.0,
            0,
        ) else {
            continue;
        };
        let deactivated = world.deactivate_body(entity);
        debug_assert!(deactivated, "a freshly spawned body always deactivates");
        if is_plinko {
            plinko.push(entity);
        } else {
            showcase.push(entity);
        }
    }
    for entities in [&mut showcase, &mut plinko] {
        for i in (1..entities.len()).rev() {
            let j = rng.range_i64(0, i as i64) as usize;
            entities.swap(i, j);
        }
    }
    PlaygroundDropPools { showcase, plinko }
}

/// Reconstructs the two pending pools after scene construction (or persistence
/// setup) from their non-overlapping emitter lanes. This keeps schedule state
/// out of durable world data while every staged body still has a stable id.
pub fn pending_drop_pools(world: &SimWorld) -> PlaygroundDropPools {
    let mut showcase = Vec::new();
    let mut plinko = Vec::new();
    for body in world.bodies().filter(|body| {
        body.entity
            .is_some_and(|entity| world.body_is_dormant(entity))
    }) {
        let entity = body.entity.expect("filtered to detached bodies");
        if body.pose.translation_m[2] >= 20.0 {
            plinko.push(entity);
        } else {
            showcase.push(entity);
        }
    }
    PlaygroundDropPools { showcase, plinko }
}

/// Contact restitution of a released Plinko ball. The client-authoritative
/// local playground (`spall_client::predict`) uses the same value; keep them
/// equal so both modes bounce identically.
pub const PLINKO_RESTITUTION: f32 = 0.45;

/// Contact restitution of a released showcase block.
pub const SHOWCASE_RESTITUTION: f32 = 0.15;

/// Wakes one pending debris body ([`SimWorld::reactivate_body`]) every
/// [`Self::interval_ticks`] ticks, so the playground's debris population
/// enters play as a real, ongoing "something drops in" effect instead of
/// arriving all at once. Reactivating an *existing* body (unlike creating a
/// new one -- see this module's doc comment) is an ordinary state change on
/// an entity every connected client already knows from its own baseline
/// pull, so it replicates through the normal 20 Hz motion-snapshot stream
/// with no special handling: `spall_sim::replication::MotionPublisher`
/// already reports every body, dormant or not, on every publish tick.
pub struct DropSchedule {
    pending: std::collections::VecDeque<spall_core::EntityId>,
    interval_ticks: u64,
    restitution: f32,
    release_height_m: Option<f64>,
}

impl DropSchedule {
    /// `entities` is typically [`populate`]'s return value.
    pub fn new(entities: Vec<spall_core::EntityId>, interval_ticks: u64) -> Self {
        Self {
            pending: entities.into(),
            interval_ticks: interval_ticks.max(1),
            restitution: SHOWCASE_RESTITUTION,
            release_height_m: None,
        }
    }

    /// Overrides the contact restitution installed as each body is released.
    pub fn with_restitution(mut self, restitution: f32) -> Self {
        self.restitution = restitution.clamp(0.0, 1.0);
        self
    }

    /// Moves each staged body from below the scene to this emitter height just
    /// before activation, making it appear as a new drop to connected clients.
    pub fn at_height(mut self, release_height_m: f64) -> Self {
        self.release_height_m = Some(release_height_m);
        self
    }

    /// Call once per server tick. A no-op once every body has been released.
    pub fn tick(&mut self, world: &mut SimWorld, tick: u64) {
        if self.pending.is_empty() || !tick.is_multiple_of(self.interval_ticks) {
            return;
        }
        if let Some(entity) = self.pending.pop_front() {
            let reactivated = if let Some(y) = self.release_height_m {
                world.reactivate_body_at_height(entity, y)
            } else {
                world.reactivate_body(entity)
            };
            if reactivated {
                world.set_body_restitution(entity, self.restitution);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::playground_setup;
    use crate::{Simulation, SimulationConfig};

    #[test]
    fn populate_creates_every_body_dormant_before_the_first_tick() {
        let mut sim = Simulation::new(SimulationConfig::new(playground_setup())).unwrap();
        let pools = populate(sim.world_mut(), &playground_drop_zones());
        assert_eq!(pools.showcase.len(), PLAYGROUND_SHOWCASE_COUNT);
        assert_eq!(pools.plinko.len(), PLAYGROUND_PLINKO_COUNT);
        assert_eq!(
            sim.world().body_count(),
            PLAYGROUND_DEBRIS_COUNT,
            "every body exists before any tick runs, so a late-join baseline pulled on tick 0 \
             already captures the full population"
        );
        assert_eq!(
            sim.world().dormant_body_count(),
            PLAYGROUND_DEBRIS_COUNT,
            "every body starts dormant -- DropSchedule wakes them one at a time"
        );
    }

    #[test]
    fn drop_schedule_wakes_exactly_one_body_per_interval() {
        let mut sim = Simulation::new(SimulationConfig::new(playground_setup())).unwrap();
        let pools = populate(sim.world_mut(), &playground_drop_zones());
        let total = pools.showcase.len() + pools.plinko.len();
        let mut schedule = DropSchedule::new(pools.showcase, 10);

        for tick in 0..10u64 {
            schedule.tick(sim.world_mut(), tick);
        }
        assert_eq!(
            sim.world().dormant_body_count(),
            total - 1,
            "exactly one body woken by the first interval boundary (tick 0)"
        );

        for tick in 10..20u64 {
            schedule.tick(sim.world_mut(), tick);
        }
        assert_eq!(
            sim.world().dormant_body_count(),
            total - 2,
            "a second body woken by the second interval boundary (tick 10)"
        );
    }

    #[test]
    fn the_two_emitters_keep_independent_five_second_and_one_second_cadences() {
        let mut sim = Simulation::new(SimulationConfig::new(playground_setup())).unwrap();
        let pools = populate(sim.world_mut(), &playground_drop_zones());
        let total = sim.world().dormant_body_count();
        let mut showcase = DropSchedule::new(pools.showcase, 300).at_height(7.0);
        let mut plinko = DropSchedule::new(pools.plinko, 60)
            .at_height(10.5)
            .with_restitution(PLINKO_RESTITUTION);
        for tick in 0..=300 {
            showcase.tick(sim.world_mut(), tick);
            plinko.tick(sim.world_mut(), tick);
        }
        assert_eq!(
            sim.world().dormant_body_count(),
            total - 8,
            "showcase releases at ticks 0/300; Plinko at 0/60/120/180/240/300"
        );
    }

    #[test]
    fn every_spawned_block_mass_is_the_sum_of_its_voxel_masses() {
        let mut sim = Simulation::new(SimulationConfig::new(playground_setup())).unwrap();
        let pools = populate(sim.world_mut(), &playground_drop_zones());
        for entity in pools
            .showcase
            .iter()
            .take(8)
            .chain(pools.plinko.iter().take(8))
        {
            let body = sim.world().body(*entity).unwrap();
            let grid = spall_physics::OccupancyGrid::from_volume(&body.volume)
                .unwrap()
                .unwrap();
            let expected = grid.solid_count() as f32 * 0.25_f32.powi(3) * 2_000.0;
            let (mass, _, _) = sim.world().physics().derived_mass_properties(body.phys);
            assert!(
                (mass - expected).abs() < 1.0e-3,
                "mass={mass}, expected={expected}"
            );
        }
    }

    #[test]
    fn drop_zones_define_showcase_and_plinko_emitters() {
        let zones = playground_drop_zones();
        assert_eq!(zones.len(), 2);
        assert!(zones[0].drop_y_m < zones[1].drop_y_m);
    }
}
