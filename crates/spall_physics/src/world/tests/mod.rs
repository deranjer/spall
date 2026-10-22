mod rigid_contacts;

use super::*;
use crate::mass::analytic_mass_properties;
use spall_core::{CellSizeCode, GlobalCell, MaterialId, VolumeId};
use spall_voxel::{EditPlan, Volume, fixtures};

const CELL_M: f64 = 0.25;
const STONE_RHO: f64 = 1000.0;
const DIRT_RHO: f64 = 3000.0;

fn mix_density(m: MaterialId) -> f64 {
    if m == fixtures::STONE {
        STONE_RHO
    } else {
        DIRT_RHO
    }
}

/// Two adjacent 4³ blocks on x: left stone, right 3× denser dirt — the
/// mixed-material fixture from `mass.rs`. Analytic COM sits at x = 1.25 m,
/// the geometric centre is x = 1.0 m.
fn mixed_density_grid() -> OccupancyGrid {
    let vid = VolumeId::new(9).unwrap();
    let mut v = Volume::new(vid, CellSizeCode::Quarter);
    v.apply_edit(&EditPlan::filled_box(
        vid,
        GlobalCell::new(0, 0, 0),
        GlobalCell::new(3, 3, 3),
        fixtures::STONE,
    ))
    .unwrap();
    v.apply_edit(&EditPlan::filled_box(
        vid,
        GlobalCell::new(4, 0, 0),
        GlobalCell::new(7, 3, 3),
        fixtures::DIRT,
    ))
    .unwrap();
    OccupancyGrid::from_region(&v, GlobalCell::new(0, 0, 0), GlobalCell::new(7, 3, 3)).unwrap()
}

fn pushed_cube_speed(density_kg_m3: f64) -> (f32, f32) {
    let id = VolumeId::new(10).unwrap();
    let mut volume = Volume::new(id, CellSizeCode::Quarter);
    volume
        .apply_edit(&EditPlan::filled_box(
            id,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(3, 3, 3),
            fixtures::STONE,
        ))
        .unwrap();
    let grid = OccupancyGrid::from_volume(&volume).unwrap().unwrap();
    let props = analytic_mass_properties(&grid, CELL_M, |_| density_kg_m3).to_body_properties();
    let mut world = PhysicsWorld::new(PhysicsConfig {
        gravity_m_s2: [0.0; 3],
        ..PhysicsConfig::default()
    });
    let body = world.add_body(BodySpec {
        kind: BodyKind::Dynamic { ccd: false },
        representation: Representation::NativeVoxels,
        grid,
        cell_m: CELL_M as f32,
        density_kg_m3: density_kg_m3 as f32,
        mass_properties: Some(props),
        translation_m: [0.5, 0.0, 0.0],
        linvel_m_s: [0.0; 3],
    });
    world.step();
    world.sweep_character_pushing_excluding(
        crate::character::CharacterParams::DEFAULT,
        [0.0, 0.0, 0.5],
        [0.75, 0.0, 0.0],
        1.0 / 60.0,
        &[],
    );
    let state = world.body_state(body);
    (state.linvel_m_s[0], state.mass_kg)
}

/// Speed given to a zero-gravity 1 m cube at `cube_x` (its origin; the cube
/// spans `+1 m` in x) by one 0.075 m walking step along +x from x = 0.
/// Returns `(speed_along_x, speed_total)`.
fn carried_cube_speed(cube_x: f32, cube_z: f32) -> (f32, f32) {
    let id = VolumeId::new(12).unwrap();
    let mut volume = Volume::new(id, CellSizeCode::Quarter);
    volume
        .apply_edit(&EditPlan::filled_box(
            id,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(3, 3, 3),
            fixtures::STONE,
        ))
        .unwrap();
    let grid = OccupancyGrid::from_volume(&volume).unwrap().unwrap();
    let props = analytic_mass_properties(&grid, CELL_M, |_| 2_000.0).to_body_properties();
    let mut world = PhysicsWorld::new(PhysicsConfig {
        gravity_m_s2: [0.0; 3],
        ..PhysicsConfig::default()
    });
    let body = world.add_body(BodySpec {
        kind: BodyKind::Dynamic { ccd: false },
        representation: Representation::MergedCuboids,
        grid,
        cell_m: CELL_M as f32,
        density_kg_m3: 2_000.0,
        mass_properties: Some(props),
        translation_m: [cube_x, 0.0, cube_z],
        linvel_m_s: [0.0; 3],
    });
    world.step();
    world.sweep_character_pushing_excluding(
        crate::character::CharacterParams::DEFAULT,
        [0.0, 0.0, 0.5],
        [crate::character::tuning::WALK_SPEED_M_S / 60.0, 0.0, 0.0],
        1.0 / 60.0,
        &[],
    );
    let v = world.body_state(body).linvel_m_s;
    (v[0], (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt())
}

/// Audit of the "carry" helper (bodies near and ahead of a walking
/// character are lifted to its speed even without a registered contact):
/// it must never launch a body that is out of reach, behind, or faster
/// than the character itself walks.
#[test]
fn carry_helper_never_launches_out_of_reach_or_faster_than_the_walker() {
    const WALK: f32 = crate::character::tuning::WALK_SPEED_M_S;
    // Capsule radius 0.3 at x = 0.075: front at about 0.37. Cube x-extent 1 m.
    // Well ahead (gap > margin): untouched.
    let (far, _) = carried_cube_speed(0.6, 0.0);
    assert!(far.abs() < 1e-4, "out-of-reach cube launched: {far}");
    // Beside the walker (1 m off to the side, far beyond any margin).
    let (side, total) = carried_cube_speed(0.0, 1.5);
    assert!(total < 1e-4, "cube beside the walker launched: {side}");
    // Behind the walker.
    let (behind, total) = carried_cube_speed(-1.6, 0.0);
    assert!(total < 1e-4, "cube behind the walker launched: {behind}");
    // Touching / within the margin: carried, but never past walking speed.
    let (near, _) = carried_cube_speed(0.4, 0.0);
    assert!(near > 0.0, "the near cube should be pushed along");
    assert!(
        near <= WALK + 1e-3,
        "carried faster than the walker: {near}"
    );
}

fn gravity_free() -> PhysicsWorld {
    PhysicsWorld::new(PhysicsConfig {
        gravity_m_s2: [0.0; 3],
        ..PhysicsConfig::default()
    })
}

fn add_mixed_body(world: &mut PhysicsWorld, props: Option<BodyMassProperties>) -> BodyId {
    world.add_body(BodySpec {
        kind: BodyKind::Dynamic { ccd: false },
        representation: Representation::MergedCuboids,
        grid: mixed_density_grid(),
        cell_m: CELL_M as f32,
        density_kg_m3: 2000.0,
        mass_properties: props,
        translation_m: [0.0; 3],
        linvel_m_s: [0.0; 3],
    })
}

/// ENG-61: the authoritative serve loop rebuilds the *terrain* collider on
/// every committed cut. A body that is settling onto that terrain must still
/// come to rest on it — a rebuild that drops the broad-phase proxy and the
/// contact manifold each tick must not let a faller tunnel the floor or jitter
/// forever (CCD is disabled for the networked destruction scenes).
#[test]
fn a_body_settles_on_terrain_whose_collider_is_rebuilt_every_step() {
    use crate::fixtures as phys_fx;

    let mut world = PhysicsWorld::new(PhysicsConfig {
        disable_ccd: true,
        ..PhysicsConfig::default()
    });

    // Fixed floor: 4 cells (1.0 m) thick, top surface at y = 1.0 m.
    let floor = phys_fx::floor_slab(VolumeId::new(1).unwrap(), 2, 2, 4);
    let floor_grid = crate::occupancy::OccupancyGrid::from_volume(&floor)
        .unwrap()
        .unwrap();
    let floor_id = world.add_body(BodySpec {
        kind: BodyKind::Fixed,
        representation: Representation::MergedCuboids,
        grid: floor_grid.clone(),
        cell_m: phys_fx::CELL_M,
        density_kg_m3: phys_fx::STONE_DENSITY,
        mass_properties: None,
        translation_m: [0.0; 3],
        linvel_m_s: [0.0; 3],
    });

    // A small cube released ~4 m up, so it reaches a real fall speed before
    // it meets the floor whose collider is churning under it.
    let piece = phys_fx::debris_pieces(50, 1, 2).pop().unwrap().1;
    let grid = crate::occupancy::OccupancyGrid::from_volume(&piece)
        .unwrap()
        .unwrap();
    let body = world.add_body(BodySpec {
        kind: BodyKind::Dynamic { ccd: false },
        representation: Representation::MergedCuboids,
        grid,
        cell_m: phys_fx::CELL_M,
        density_kg_m3: phys_fx::STONE_DENSITY,
        mass_properties: None,
        translation_m: [4.0, 5.0, 4.0],
        linvel_m_s: [0.0; 3],
    });

    for _ in 0..600 {
        world.rebuild_collider(floor_id, &floor_grid, Representation::MergedCuboids);
        world.step();
        assert!(world.body_state(body).is_finite());
    }

    let st = world.body_state(body);
    assert!(
        st.translation_m[1] > 0.9,
        "body tunnelled the rebuilt-every-step floor (top at y = 1.0 m, body at y = {})",
        st.translation_m[1]
    );
    assert!(
        st.speed_m_s() < 0.05,
        "body never came to rest on the churning collider (speed = {} m/s)",
        st.speed_m_s()
    );
    assert!(
        world.max_penetration_m() < 0.1,
        "resting body did not sink into the floor ({} m)",
        world.max_penetration_m()
    );
}

/// PR #109 review finding: [`PhysicsWorld::sync_queries`] used to call
/// [`rapier3d::geometry::BroadPhaseBvh::update`] directly, which Rapier
/// permanently remembers as "this pair was announced" — so a single
/// `sync_queries` call before a pair's very first real [`PhysicsWorld::step`]
/// silently and permanently starved that pair of contacts, for the rest
/// of the simulation, however many times `step` ran afterwards. This is
/// exactly [`CharacterQueryCache`](crate::query_cache::CharacterQueryCache)'s
/// real call pattern: it shares this `PhysicsWorld` with a simulation it
/// does not own the tick loop for, and calls `sync_queries` (never
/// `step`) after every window rebuild.
#[test]
fn sync_queries_before_the_first_step_does_not_starve_a_dynamic_body_of_contacts() {
    fn unit_cube_body(world: &mut PhysicsWorld, kind: BodyKind, y: f32) -> BodyId {
        world.add_body(BodySpec {
            kind,
            representation: Representation::MergedCuboids,
            grid: OccupancyGrid::from_solid_mask(
                GlobalCell::new(0, 0, 0),
                [1, 1, 1],
                vec![true],
                vec![MaterialId(1)],
            )
            .unwrap(),
            cell_m: 1.0,
            density_kg_m3: 1.0,
            mass_properties: None,
            translation_m: [0.0, y, 0.0],
            linvel_m_s: [0.0; 3],
        })
    }

    let mut without_sync = PhysicsWorld::new(PhysicsConfig::default());
    unit_cube_body(&mut without_sync, BodyKind::Fixed, 0.0);
    let dynamic_no_sync = unit_cube_body(&mut without_sync, BodyKind::Dynamic { ccd: false }, 1.0);
    for _ in 0..60 {
        without_sync.step();
    }
    let baseline = without_sync.body_state(dynamic_no_sync);
    assert!(
        baseline.translation_m[1] > 0.9,
        "baseline (no sync_queries) should rest on the floor, got y = {}",
        baseline.translation_m[1]
    );

    let mut with_sync = PhysicsWorld::new(PhysicsConfig::default());
    unit_cube_body(&mut with_sync, BodyKind::Fixed, 0.0);
    let dynamic = unit_cube_body(&mut with_sync, BodyKind::Dynamic { ccd: false }, 1.0);
    with_sync.sync_queries();
    for _ in 0..60 {
        with_sync.step();
    }
    let st = with_sync.body_state(dynamic);
    assert!(
        st.translation_m[1] > 0.9,
        "a sync_queries call before the first step must not stop the pair from ever \
         contacting — expected the body to rest at y > 0.9 like the no-sync baseline \
         ({}), got y = {}",
        baseline.translation_m[1],
        st.translation_m[1]
    );
    assert!(
        with_sync.contact_pair_count() >= 1,
        "expected at least one live contact pair after settling, found none"
    );
}

/// Same regression, but exercising [`CharacterQueryCache`]'s actual usage
/// pattern: a collider *replacement* ([`PhysicsWorld::rebuild_collider`])
/// followed by `sync_queries`, on a `PhysicsWorld` a real dynamic body is
/// also resting in — not just a freshly-added collider.
#[test]
fn sync_queries_after_collider_replacement_does_not_starve_a_dynamic_body_of_contacts() {
    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    let grid = OccupancyGrid::from_solid_mask(
        GlobalCell::new(0, 0, 0),
        [1, 1, 1],
        vec![true],
        vec![MaterialId(1)],
    )
    .unwrap();
    let floor = world.add_body(BodySpec {
        kind: BodyKind::Fixed,
        representation: Representation::MergedCuboids,
        grid: grid.clone(),
        cell_m: 1.0,
        density_kg_m3: 1.0,
        mass_properties: None,
        translation_m: [0.0, 0.0, 0.0],
        linvel_m_s: [0.0; 3],
    });
    let dynamic = world.add_body(BodySpec {
        kind: BodyKind::Dynamic { ccd: false },
        representation: Representation::MergedCuboids,
        grid: grid.clone(),
        cell_m: 1.0,
        density_kg_m3: 1.0,
        mass_properties: None,
        translation_m: [0.0, 1.0, 0.0],
        linvel_m_s: [0.0; 3],
    });

    // Rebuild the floor's collider (a no-op geometry change) and sync
    // queries against it, exactly like `CharacterQueryCache::rebuild`
    // does for its window collider — before the dynamic body's own pair
    // has ever gone through a real `step`.
    world.rebuild_collider(floor, &grid, Representation::MergedCuboids);
    world.sync_queries();

    for _ in 0..60 {
        world.step();
    }
    let st = world.body_state(dynamic);
    assert!(
        st.translation_m[1] > 0.9,
        "a collider-replacement + sync_queries call must not stop the dynamic body's \
         pair from ever contacting — got y = {}",
        st.translation_m[1]
    );
    assert!(
        world.contact_pair_count() >= 1,
        "expected at least one live contact pair after settling, found none"
    );
}

/// T21 input surface: [`PhysicsWorld::contact_impulses`] must spike on a real
/// impact and then fall back to roughly the resting weight-support impulse
/// (`m·g·dt`) once the body settles — the separation the contact-damage rules
/// threshold on so a resting body never keeps fracturing the floor.
#[test]
fn contact_impulses_spike_on_impact_then_decay_to_the_resting_load() {
    use crate::fixtures as phys_fx;

    let mut world = PhysicsWorld::new(PhysicsConfig {
        disable_ccd: true,
        ..PhysicsConfig::default()
    });

    let floor = phys_fx::floor_slab(VolumeId::new(1).unwrap(), 2, 2, 4);
    let floor_grid = crate::occupancy::OccupancyGrid::from_volume(&floor)
        .unwrap()
        .unwrap();
    let floor_id = world.add_body(BodySpec {
        kind: BodyKind::Fixed,
        representation: Representation::MergedCuboids,
        grid: floor_grid.clone(),
        cell_m: phys_fx::CELL_M,
        density_kg_m3: phys_fx::STONE_DENSITY,
        mass_properties: None,
        translation_m: [0.0; 3],
        linvel_m_s: [0.0; 3],
    });

    let piece = phys_fx::debris_pieces(50, 1, 3).pop().unwrap().1;
    let grid = crate::occupancy::OccupancyGrid::from_volume(&piece)
        .unwrap()
        .unwrap();
    let body = world.add_body(BodySpec {
        kind: BodyKind::Dynamic { ccd: false },
        representation: Representation::MergedCuboids,
        grid,
        cell_m: phys_fx::CELL_M,
        density_kg_m3: phys_fx::STONE_DENSITY,
        mass_properties: None,
        translation_m: [4.0, 4.0, 4.0],
        linvel_m_s: [0.0; 3],
    });
    let mass = world.body_state(body).mass_kg;

    let mut peak_impact = 0.0_f32;
    let mut resting = 0.0_f32;
    for step in 0..500 {
        world.step();
        let contacts = world.contact_impulses();
        let pair_impulse: f32 = contacts
            .iter()
            .filter(|c| c.bodies.contains(&body))
            .map(|c| c.normal_impulse_n_s)
            .sum();
        if step < 200 {
            peak_impact = peak_impact.max(pair_impulse);
        } else {
            resting = pair_impulse;
        }
    }

    let st = world.body_state(body);
    assert!(
        st.speed_m_s() < 0.1,
        "body settled (speed {})",
        st.speed_m_s()
    );

    // Resting support impulse is on the order of m·g·dt; the impact was a
    // multiple of it (a real fall from ~2.5 m onto a rigid floor).
    let weight_impulse = mass * 9.81 * world.params.dt;
    assert!(
        resting > 0.0 && resting < 4.0 * weight_impulse,
        "settled contact impulse ({resting} N·s) is near the resting load \
         (m·g·dt = {weight_impulse} N·s)"
    );
    assert!(
        peak_impact > 8.0 * resting.max(weight_impulse),
        "impact impulse ({peak_impact} N·s) spikes well above the resting \
         load ({resting} N·s)"
    );

    // The settled pair is body-vs-terrain: exactly one side is dynamic.
    let contact = world
        .contact_impulses()
        .into_iter()
        .find(|c| c.bodies.contains(&body))
        .expect("a resting body keeps a tracked contact with the floor");
    assert_ne!(
        contact.dynamic[0], contact.dynamic[1],
        "a falling body resting on fixed terrain: one dynamic, one fixed"
    );
    assert!(
        contact.bodies.contains(&floor_id),
        "the other side of the contact is the floor"
    );
    assert!(
        (contact.point_m[1] - 1.0).abs() < 0.2,
        "contact point sits on the floor top (y = 1.0 m), got {}",
        contact.point_m[1]
    );
    assert!(
        (contact.point_m[0] - 4.375).abs() < 0.6 && (contact.point_m[2] - 4.375).abs() < 0.6,
        "contact point is under the body (~x/z 4.375 m), got {:?}",
        contact.point_m
    );
}

/// T21 dormancy: deactivating a settled body drops it out of the stepped
/// set, and reactivating it into the same slot restores its pose so it
/// resumes physics from where it was.
#[test]
fn a_dormant_body_leaves_the_step_set_and_reactivates_at_its_pose() {
    use crate::fixtures as phys_fx;

    let mut world = PhysicsWorld::new(PhysicsConfig {
        disable_ccd: true,
        ..PhysicsConfig::default()
    });
    let floor = phys_fx::floor_slab(VolumeId::new(1).unwrap(), 2, 2, 4);
    let floor_grid = crate::occupancy::OccupancyGrid::from_volume(&floor)
        .unwrap()
        .unwrap();
    world.add_body(BodySpec {
        kind: BodyKind::Fixed,
        representation: Representation::MergedCuboids,
        grid: floor_grid,
        cell_m: phys_fx::CELL_M,
        density_kg_m3: phys_fx::STONE_DENSITY,
        mass_properties: None,
        translation_m: [0.0; 3],
        linvel_m_s: [0.0; 3],
    });
    let piece = phys_fx::debris_pieces(50, 1, 3).pop().unwrap().1;
    let grid = crate::occupancy::OccupancyGrid::from_volume(&piece)
        .unwrap()
        .unwrap();
    let body = world.add_body(BodySpec {
        kind: BodyKind::Dynamic { ccd: false },
        representation: Representation::MergedCuboids,
        grid: grid.clone(),
        cell_m: phys_fx::CELL_M,
        density_kg_m3: phys_fx::STONE_DENSITY,
        mass_properties: None,
        translation_m: [4.0, 2.0, 4.0],
        linvel_m_s: [0.0; 3],
    });

    for _ in 0..400 {
        world.step();
    }
    let settled = world.body_state(body);
    assert!(settled.sleeping, "body settled to sleep");
    assert_eq!(world.active_body_count(), 2);

    world.deactivate_body(body);
    assert!(world.is_dormant(body));
    assert_eq!(world.active_body_count(), 1, "dormant body is not stepped");
    // Stepping the world for a while does nothing to the dormant body.
    for _ in 0..120 {
        world.step();
    }

    world.reactivate_body(
        body,
        &grid,
        settled.translation_m,
        settled.rotation,
        [0.0; 3],
        [0.0; 3],
    );
    assert!(!world.is_dormant(body));
    assert_eq!(world.active_body_count(), 2);
    let back = world.body_state(body);
    for axis in 0..3 {
        assert!(
            (back.translation_m[axis] - settled.translation_m[axis]).abs() < 1e-4,
            "reactivated at the frozen pose (axis {axis}: {} vs {})",
            back.translation_m[axis],
            settled.translation_m[axis]
        );
    }
    // It stays put on the floor after reactivation — no fall, no explosion.
    for _ in 0..200 {
        world.step();
    }
    let after = world.body_state(body);
    assert!(after.is_finite() && after.speed_m_s() < 0.1);
    assert!((after.translation_m[1] - settled.translation_m[1]).abs() < 0.05);
}
