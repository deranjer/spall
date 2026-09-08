//! T16 save-integration acceptance: a real `spall_sim::Simulation` is edited,
//! settled, checkpointed, and recovered — geometry, ownership, materials, pose
//! of rotated fractured bodies, and sleeping state must survive the round trip.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use glam::{DQuat, DVec3};
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{CellSizeCode, GlobalCell, SphereBrush, VolumeId};
use spall_physics::PhysicsConfig;
use spall_protocol::RequestId;
use spall_server::persist::{self, PersistConfig};
use spall_sim::{BodyPose, EditIntent, EditTarget, Simulation, SimulationConfig, fixtures};
use spall_store::Writer;
use spall_structure::AnchorPlane;
use spall_voxel::{EditPlan, Volume};

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("spall_persist_{tag}_{}_{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn db(&self) -> PathBuf {
        self.0.join("world.db")
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn cfg() -> PersistConfig {
    PersistConfig {
        world_id: 0x5A11_0000_0000_7016,
        seed: 7,
        generator_version: 1,
    }
}

fn brush_cell(x: i64, y: i64, z: i64, radius_cells: i64) -> SphereBrush {
    let h = BRUSH_UNIT / 2;
    SphereBrush::new(
        BrushPoint::from_units(x * BRUSH_UNIT + h, y * BRUSH_UNIT + h, z * BRUSH_UNIT + h),
        radius_cells * BRUSH_UNIT,
    )
    .unwrap()
}

fn actor() -> spall_core::EntityId {
    spall_core::EntityId::new(1).unwrap()
}

fn publish(db: &std::path::Path, cp: &spall_store::Checkpoint) {
    let mut w = Writer::open(db).unwrap();
    w.publish_checkpoint(cp).unwrap();
}

fn recover_restore(db: &std::path::Path) -> (Simulation, u64) {
    let recovery = spall_store::recover(db).unwrap();
    persist::restore(
        &recovery,
        fixtures::stone_manifest(),
        AnchorPlane::at(0),
        PhysicsConfig::default(),
    )
    .unwrap()
}

fn sleep_map(sim: &Simulation) -> Vec<(u64, bool)> {
    let mut v: Vec<(u64, bool)> = sim
        .world()
        .bodies()
        .map(|b| (b.entity.unwrap().get(), b.sleeping))
        .collect();
    v.sort();
    v
}

fn dug_is_air(sim: &Simulation, cell: GlobalCell) -> bool {
    matches!(
        sim.world()
            .volume_ref(sim.world().terrain_volume_id())
            .unwrap()
            .sample(cell)
            .unwrap(),
        spall_voxel::Sample::Empty { .. }
    )
}

// -------------------------------------------------------------------------

#[test]
fn save_restart_preserves_excavation_and_sleeping_body() {
    let s = Scratch::new("dig_sleep");

    let mut sim = Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();
    // A compact stone cube resting on the slab settles and sleeps quickly.
    sim.world_mut()
        .spawn_body(
            fixtures::solid_block(4),
            BodyPose::new(DQuat::IDENTITY, [1.0, 0.5, 1.0]),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            1,
        )
        .unwrap();
    sim.submit(EditIntent::cut(
        RequestId(1),
        actor(),
        EditTarget::Terrain,
        brush_cell(14, 1, 14, 2),
    ))
    .unwrap();
    sim.run_until_idle(16).unwrap();
    for _ in 0..120 {
        sim.step_physics_only();
    }

    let want_hash = sim.world().world_hash();
    let want_solid = sim.world().total_solid_cells();
    let want_sleep = sleep_map(&sim);
    let want_tick = sim.current_tick();
    assert!(
        want_sleep.iter().any(|(_, slept)| *slept),
        "the resting cube is asleep"
    );

    let dug = GlobalCell::new(14, 1, 14);
    assert!(dug_is_air(&sim, dug));

    publish(&s.db(), &persist::capture(&sim, &cfg(), 0).unwrap());
    drop(sim);

    let (restored, durable_seq) = recover_restore(&s.db());
    assert_eq!(durable_seq, 0);
    assert_eq!(restored.current_tick(), want_tick);
    assert_eq!(restored.world().total_solid_cells(), want_solid);
    assert_eq!(
        restored.world().world_hash(),
        want_hash,
        "geometry + ownership + materials recovered exactly"
    );
    assert_eq!(sleep_map(&restored), want_sleep, "sleep state recovered");
    assert!(dug_is_air(&restored, dug), "excavation persisted");
}

#[test]
fn save_restart_preserves_a_rotated_fractured_body() {
    let s = Scratch::new("rotated_split");

    let mut sim = Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();
    // A dumbbell spawned rotated and airborne, cut through its bridge so it
    // fractures into two bodies that keep the parent's split-instant rotation.
    let spin = fixtures::oblique_spin();
    let body = sim
        .world_mut()
        .spawn_body(
            fixtures::dumbbell(4, 3),
            BodyPose::new(spin, [3.0, 6.0, 3.0]),
            [0.0, 0.0, 0.0],
            [0.4, 0.0, 0.0],
            2600.0,
            1,
        )
        .unwrap();
    // dumbbell(4, 3): the 1-cell bridge runs x = 4..=6 at y = z = 2 (local).
    sim.submit(EditIntent::cut(
        RequestId(1),
        actor(),
        EditTarget::Body(body),
        brush_cell(5, 2, 2, 1),
    ))
    .unwrap();
    sim.run_until_idle(24).unwrap();
    assert_eq!(sim.world().body_count(), 2, "dumbbell fractured into two");
    for _ in 0..10 {
        sim.step_physics_only();
    }

    let want_hash = sim.world().world_hash();
    let want_solid = sim.world().total_solid_cells();
    let want_pose: Vec<(u64, [f64; 3], [f64; 4])> = sim
        .world()
        .bodies()
        .map(|b| {
            let q = b.pose.rotation;
            (
                b.entity.unwrap().get(),
                b.pose.translation_m,
                [q.x, q.y, q.z, q.w],
            )
        })
        .collect();

    publish(&s.db(), &persist::capture(&sim, &cfg(), 0).unwrap());
    drop(sim);

    let (restored, _) = recover_restore(&s.db());
    assert_eq!(restored.world().body_count(), 2);
    assert_eq!(restored.world().total_solid_cells(), want_solid);
    assert_eq!(
        restored.world().world_hash(),
        want_hash,
        "fractured geometry + child ownership recovered"
    );

    for (entity, trans, quat) in want_pose {
        let body = restored
            .world()
            .bodies()
            .find(|b| b.entity.unwrap().get() == entity)
            .expect("child body present after restore");
        let got_t = DVec3::from_array(body.pose.translation_m);
        assert!(
            (got_t - DVec3::from_array(trans)).length() < 1e-9,
            "body {entity} translation preserved"
        );
        let got_q = body.pose.rotation;
        let want_q = DQuat::from_xyzw(quat[0], quat[1], quat[2], quat[3]);
        // q and -q are the same rotation.
        let dot =
            (got_q.x * want_q.x + got_q.y * want_q.y + got_q.z * want_q.z + got_q.w * want_q.w)
                .abs();
        assert!(
            dot > 1.0 - 1e-9,
            "body {entity} rotation preserved (dot={dot})"
        );
    }
}

#[test]
fn checkpoint_plus_journal_suffix_recovers_topology() {
    let s = Scratch::new("journal");

    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup())).unwrap();
    {
        let mut w = Writer::open(s.db()).unwrap();
        w.publish_checkpoint(&persist::capture(&sim, &cfg(), 0).unwrap())
            .unwrap();

        sim.submit(EditIntent::cut(
            RequestId(1),
            actor(),
            EditTarget::Terrain,
            brush_cell(10, 4, 1, 2),
        ))
        .unwrap();
        sim.run_until_idle(16).unwrap();
        assert_eq!(sim.world().body_count(), 1, "beam detached");

        let records = persist::journal_records(sim.journal().entries()).unwrap();
        assert_eq!(records.len(), 1);
        let durable = w.append_journal(&records).unwrap();
        assert_eq!(durable.journal_seq.0, records[0].seq);
    }

    let want_hash = sim.world().world_hash();
    let want_bodies = sim.world().body_count();

    let recovery = spall_store::recover(s.db()).unwrap();
    assert_eq!(recovery.journal.len(), 1);
    let (mut restored, durable_seq) = persist::restore(
        &recovery,
        fixtures::stone_manifest(),
        AnchorPlane::at(0),
        PhysicsConfig::default(),
    )
    .unwrap();

    assert_eq!(durable_seq, recovery.journal[0].seq);
    assert_eq!(restored.world().body_count(), want_bodies);
    assert_eq!(
        restored.world().world_hash(),
        want_hash,
        "replayed split reproduces server geometry + ownership"
    );

    // Ids resume past the replayed transaction: a fresh cut still commits.
    restored
        .submit(EditIntent::cut(
            RequestId(2),
            actor(),
            EditTarget::Terrain,
            brush_cell(6, 4, 1, 1),
        ))
        .unwrap();
    restored.run_until_idle(16).unwrap();
    assert!(restored.committed(RequestId(2)).is_some());
}

#[test]
fn a_manifest_hash_mismatch_is_rejected() {
    let s = Scratch::new("manifest");
    let sim = Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup())).unwrap();
    {
        let mut w = Writer::open(s.db()).unwrap();
        w.publish_checkpoint(&persist::capture(&sim, &cfg(), 0).unwrap())
            .unwrap();
    }
    let recovery = spall_store::recover(s.db()).unwrap();

    let air_only = spall_core::MaterialManifest::validated(vec![spall_core::MaterialDef {
        id: spall_core::MaterialId::AIR,
        name: "air".into(),
        render: spall_core::RenderProps {
            albedo: [0.0; 3],
            roughness: 1.0,
            metalness: 0.0,
            emissive: [0.0; 3],
        },
        sim: spall_core::SimProps {
            density_kg_m3: 0.0,
            friction: 0.0,
            restitution: 0.0,
            hardness: 0.0,
            bond_strength: 0.0,
            flags: spall_core::MaterialFlags::NONE,
        },
    }])
    .unwrap();

    match persist::restore(
        &recovery,
        air_only,
        AnchorPlane::at(0),
        PhysicsConfig::default(),
    ) {
        Err(persist::PersistError::ManifestMismatch { .. }) => {}
        Err(other) => panic!("expected ManifestMismatch, got {other:?}"),
        Ok(_) => panic!("expected ManifestMismatch, got a restored simulation"),
    }
}

// -------------------------------------------------------------------------
// ENG-38: a journalled split must reconstruct its children with the live
// physical mass / centre of mass / inertia, the *source* volume's cell size,
// and every participant's pose — geometry-hash parity is not enough.
// -------------------------------------------------------------------------

/// `entity id -> (mass_kg, local COM, principal inertia)` for every body.
fn mass_props_by_entity(
    sim: &Simulation,
) -> std::collections::BTreeMap<u64, (f32, [f32; 3], [f32; 3])> {
    sim.world()
        .bodies()
        .map(|b| {
            (
                b.entity.unwrap().get(),
                sim.world().physics().derived_mass_properties(b.phys),
            )
        })
        .collect()
}

/// A body-local dumbbell built at the fine `Sixteenth` cell size (the terrain
/// fixtures are all `Quarter`), so a replay that wrongly used the terrain cell
/// size would misplace ~64x the mass.
fn sixteenth_dumbbell(sz: i64, gap: i64) -> impl FnOnce(VolumeId) -> Volume {
    let stone = spall_core::MaterialId(1);
    move |id| {
        let mut v = Volume::new(id, CellSizeCode::Sixteenth);
        v.apply_edit(&EditPlan::filled_box(
            id,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(sz - 1, sz - 1, sz - 1),
            stone,
        ))
        .unwrap();
        let rx = sz + gap;
        v.apply_edit(&EditPlan::filled_box(
            id,
            GlobalCell::new(rx, 0, 0),
            GlobalCell::new(rx + sz - 1, sz - 1, sz - 1),
            stone,
        ))
        .unwrap();
        let mid = sz / 2;
        v.apply_edit(&EditPlan::filled_box(
            id,
            GlobalCell::new(sz, mid, mid),
            GlobalCell::new(rx - 1, mid, mid),
            stone,
        ))
        .unwrap();
        v
    }
}

/// Probe `review_replayed_child_must_preserve_mass`: checkpoint the bridge,
/// durably journal the column cut, restore checkpoint + journal, and compare
/// the recovered beam's actual Rapier mass properties against the live ones.
#[test]
fn review_replayed_child_must_preserve_mass() {
    let s = Scratch::new("eng38_terrain_mass");

    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup())).unwrap();
    {
        let mut w = Writer::open(s.db()).unwrap();
        w.publish_checkpoint(&persist::capture(&sim, &cfg(), 0).unwrap())
            .unwrap();

        sim.submit(EditIntent::cut(
            RequestId(1),
            actor(),
            EditTarget::Terrain,
            brush_cell(10, 4, 1, 2),
        ))
        .unwrap();
        sim.run_until_idle(16).unwrap();
        assert_eq!(sim.world().body_count(), 1, "the beam detached");

        let records = persist::journal_records(sim.journal().entries()).unwrap();
        assert_eq!(records.len(), 1, "one split transaction journalled");
        w.append_journal(&records).unwrap();
    }

    let (restored, _) = recover_restore(&s.db());

    let live = sim.world().bodies().next().unwrap();
    let got = restored.world().bodies().next().unwrap();
    let (want_mass, want_com, want_inertia) =
        sim.world().physics().derived_mass_properties(live.phys);
    let (got_mass, got_com, got_inertia) =
        restored.world().physics().derived_mass_properties(got.phys);

    assert!(
        want_mass > 1000.0,
        "sanity: a stone beam is heavy ({want_mass} kg) — not the ~1 kg the density=1.0 bug produced"
    );
    assert!(
        (want_mass - got_mass).abs() < 0.01,
        "replayed body mass: expected {want_mass} kg, got {got_mass} kg"
    );
    for i in 0..3 {
        assert!(
            (want_com[i] - got_com[i]).abs() < 1e-3,
            "COM axis {i}: expected {}, got {}",
            want_com[i],
            got_com[i]
        );
        let denom = want_inertia[i].abs().max(1e-6);
        assert!(
            (want_inertia[i] - got_inertia[i]).abs() / denom < 1e-3,
            "principal inertia axis {i}: expected {}, got {}",
            want_inertia[i],
            got_inertia[i]
        );
    }
}

/// A rotated, airborne dumbbell that keeps moving between the checkpoint and
/// the cut: the replay must fold every participant — including the surviving
/// parent — to the split frame, not leave the parent stranded at the
/// checkpoint frame while the child sits at the split frame.
#[test]
fn rotated_body_to_body_split_replay_preserves_mass_and_parent_frame() {
    let s = Scratch::new("eng38_body_split");

    let mut sim = Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();
    let parent = sim
        .world_mut()
        .spawn_body(
            fixtures::dumbbell(4, 3),
            BodyPose::new(fixtures::oblique_spin(), [3.0, 8.0, 3.0]),
            [0.2, 0.0, 0.0],
            [0.0, 0.3, 0.0],
            2600.0,
            1,
        )
        .unwrap();
    let checkpoint_pose = sim.world().body(parent).unwrap().pose;

    {
        let mut w = Writer::open(s.db()).unwrap();
        w.publish_checkpoint(&persist::capture(&sim, &cfg(), 0).unwrap())
            .unwrap();

        // Fall + spin so the body's frame diverges from the checkpoint frame.
        for _ in 0..12 {
            sim.step_physics_only();
        }
        let moved = sim.world().body(parent).unwrap().pose;
        assert!(
            (DVec3::from_array(moved.translation_m)
                - DVec3::from_array(checkpoint_pose.translation_m))
            .length()
                > 1e-3,
            "the body moved between checkpoint and cut"
        );

        sim.submit(EditIntent::cut(
            RequestId(1),
            actor(),
            EditTarget::Body(parent),
            brush_cell(5, 2, 2, 1),
        ))
        .unwrap();
        sim.run_until_idle(24).unwrap();
        assert_eq!(sim.world().body_count(), 2, "the dumbbell fractured");

        let records = persist::journal_records(sim.journal().entries()).unwrap();
        assert_eq!(records.len(), 1, "one split transaction journalled");
        w.append_journal(&records).unwrap();
    }

    let live = mass_props_by_entity(&sim);
    let (restored, _) = recover_restore(&s.db());
    let got = mass_props_by_entity(&restored);

    assert_eq!(
        got.keys().collect::<Vec<_>>(),
        live.keys().collect::<Vec<_>>(),
        "the same body ids recovered"
    );
    for (entity, (want_mass, _, _)) in &live {
        let (got_mass, _, _) = got[entity];
        assert!(
            *want_mass > 100.0,
            "body {entity} is stone, not density=1.0"
        );
        assert!(
            (want_mass - got_mass).abs() < 0.01,
            "body {entity}: mass expected {want_mass} kg, got {got_mass} kg"
        );
    }

    // The parent participant was carried to the split frame: off the
    // checkpoint pose, and sharing the split-instant rotation with the child
    // (a child reproduces the parent transform exactly at the cut).
    let rp = restored.world().body(parent).unwrap();
    assert!(
        (DVec3::from_array(rp.pose.translation_m)
            - DVec3::from_array(checkpoint_pose.translation_m))
        .length()
            > 1e-3,
        "restored parent advanced past the checkpoint frame, not stranded at it"
    );
    let child = restored
        .world()
        .bodies()
        .find(|b| b.entity.unwrap().get() != parent.get())
        .expect("a detached child exists");
    let (a, b) = (rp.pose.rotation, child.pose.rotation);
    let dot = (a.x * b.x + a.y * b.y + a.z * b.z + a.w * b.w).abs();
    assert!(
        dot > 1.0 - 1e-6,
        "restored parent + child share the split-instant rotation (dot={dot})"
    );
}

/// A body cut at a finer cell size than the terrain: the recovered children
/// must keep the source cell size and therefore the correct mass — a replay
/// that reached for `terrain_cell_size` would be ~64x off.
#[test]
fn detail_cell_body_split_replay_uses_the_source_cell_size() {
    let s = Scratch::new("eng38_detail");

    let mut sim = Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();
    let parent = sim
        .world_mut()
        .spawn_body(
            sixteenth_dumbbell(4, 3),
            BodyPose::new(DQuat::IDENTITY, [2.0, 6.0, 2.0]),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            1,
        )
        .unwrap();
    assert_ne!(
        sim.world().body(parent).unwrap().cell_size(),
        sim.world().terrain().cell_size(),
        "the body is finer than the terrain"
    );

    {
        let mut w = Writer::open(s.db()).unwrap();
        w.publish_checkpoint(&persist::capture(&sim, &cfg(), 0).unwrap())
            .unwrap();

        sim.submit(EditIntent::cut(
            RequestId(1),
            actor(),
            EditTarget::Body(parent),
            brush_cell(5, 2, 2, 1),
        ))
        .unwrap();
        sim.run_until_idle(24).unwrap();
        assert_eq!(
            sim.world().body_count(),
            2,
            "the detail-cell dumbbell fractured"
        );

        let records = persist::journal_records(sim.journal().entries()).unwrap();
        w.append_journal(&records).unwrap();
    }

    let live = mass_props_by_entity(&sim);
    let (restored, _) = recover_restore(&s.db());
    let got = mass_props_by_entity(&restored);

    for (entity, (want_mass, _, _)) in &live {
        let (got_mass, _, _) = got[entity];
        assert!(
            (want_mass - got_mass).abs() < 0.01,
            "detail-cell body {entity}: expected {want_mass} kg, got {got_mass} kg \
             (a terrain-cell-size child would be ~64x heavier)"
        );
    }
    for b in restored.world().bodies() {
        assert_eq!(
            b.cell_size(),
            CellSizeCode::Sixteenth,
            "recovered body {} kept the source cell size",
            b.entity.unwrap().get()
        );
    }
}
