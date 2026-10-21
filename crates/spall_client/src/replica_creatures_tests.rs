//! End-to-end checks: a real server `Simulation` publishes lifecycle
//! transactions and a `ReplicaWorld` converges on the same ownership records.
use super::*;
use crate::replica::{ApplyOutcome, ReplicaConfig, ReplicaWorld};
use spall_core::{
    CellSizeCode, CreatureGeneration, CreatureId, GlobalCell, MaterialId, Tick, WorldEpoch,
};
use spall_physics::{PartTransform, Representation};
use spall_protocol::{
    ControlSeq, CreatureAssetIdentity, CreatureClipMode, CreatureLifecycleState,
    CreaturePartBinding, CreatureStateV1, Hash32, OwnedCreatureAsset, OwnedCreatureSpecies,
    WIRE_SCHEMA_VERSION, content_manifest_hash,
};
use spall_sim::{
    AuthoritativeCreatureIntent, CommittedCreatureLifecycle, CreatureBirthPartInput, Simulation,
    SimulationConfig, fixtures,
};
use spall_voxel::{EditPlan, Volume};

const WORLD: u128 = 900;

fn catalog(materials: spall_core::MaterialManifest) -> Arc<OwnedCreatureCatalog> {
    let asset = CreatureAssetIdentity {
        uuid: [3; 16],
        logical_hash: Hash32([4; 32]),
    };
    Arc::new(OwnedCreatureCatalog {
        assets: vec![OwnedCreatureAsset {
            uuid: asset.uuid,
            logical_hash: asset.logical_hash,
            part_ids: vec![1],
            clip_names: vec!["idle".into()],
            portable_material_keys: vec!["stone".into()],
        }],
        species: vec![OwnedCreatureSpecies {
            key: "deer".into(),
            version: 1,
            enabled: true,
            asset,
        }],
        materials,
        material_mapping_version: 1,
        material_mapping: vec![("stone".into(), MaterialId(1))],
    })
}

fn server() -> (Simulation, Arc<OwnedCreatureCatalog>) {
    let mut sim = Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();
    sim.initialize_creature_host(WorldId::from_u128(WORLD))
        .unwrap();
    let catalog = catalog(sim.world().materials().clone());
    (sim, catalog)
}

fn birth_intent(
    sim: &Simulation,
    catalog: &OwnedCreatureCatalog,
    root: [f64; 3],
) -> AuthoritativeCreatureIntent {
    let creature_id = sim.creature_allocator_snapshot().unwrap().next_creature_id;
    let volume_id = spall_core::VolumeId::new(sim.world().registry().counters().1).unwrap();
    let mut volume = Volume::new(volume_id, CellSizeCode::Quarter);
    volume
        .apply_edit(&EditPlan::filled_box(
            volume_id,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(0, 0, 0),
            MaterialId(1),
        ))
        .unwrap();
    let mapping_hash = catalog
        .with_view(|view| view.material_mapping_hash)
        .unwrap();
    let state = CreatureStateV1 {
        schema: WIRE_SCHEMA_VERSION,
        key: CreatureKey::new(
            WorldId::from_u128(WORLD),
            WorldEpoch::INITIAL,
            CreatureId::new(creature_id).unwrap(),
            CreatureGeneration::INITIAL,
        )
        .unwrap(),
        state_revision: 1,
        lifecycle: CreatureLifecycleState::Alive,
        species_key: "deer".into(),
        species_version: 1,
        asset: catalog.species[0].asset,
        material_manifest_hash: content_manifest_hash(sim.world().materials()),
        material_mapping_version: 1,
        material_mapping_hash: mapping_hash,
        clip_name: "idle".into(),
        clip_mode: CreatureClipMode::Playing,
        clip_phase_tick: 0,
        clip_subtick_remainder: 0,
        clip_epoch: 1,
        source_bindings: vec![CreaturePartBinding {
            part_id: 1,
            source_volume_id: volume_id,
            source_revision: 1,
        }],
        aggregate_topology_revision: 1,
        pose_epoch: 1,
        root_translation_m: root,
        root_rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
        linear_velocity_m_s: [0.0; 3],
        angular_velocity_rad_s: [0.0; 3],
        locomotion_tag: 0,
    };
    AuthoritativeCreatureIntent::Birth {
        state: Box::new(state),
        parts: vec![CreatureBirthPartInput {
            part_id: 1,
            parent_id: None,
            local_transform: PartTransform::IDENTITY,
            volume,
        }],
        representation: Representation::MergedCuboids,
    }
}

fn commit(
    sim: &mut Simulation,
    catalog: &OwnedCreatureCatalog,
    tick: u64,
    seq: u64,
    intent: AuthoritativeCreatureIntent,
) -> CommittedCreatureLifecycle {
    catalog
        .with_view(|view| {
            sim.commit_creature_lifecycle_transaction(
                Tick(tick),
                ControlSeq(seq),
                vec![intent],
                None,
                view,
            )
        })
        .unwrap()
        .unwrap()
}

fn replica(catalog: &Arc<OwnedCreatureCatalog>) -> ReplicaWorld {
    let mut replica = ReplicaWorld::from_baseline(
        fixtures::flat_terrain_setup().terrain,
        ReplicaConfig::default(),
    );
    replica.install_creature_catalog(catalog.clone());
    replica
}

fn server_record(sim: &Simulation, catalog: &OwnedCreatureCatalog) -> CreatureOwnershipRecordV4 {
    catalog
        .with_view(|view| sim.capture_creature_snapshot(view))
        .unwrap()
        .unwrap()
        .unwrap()
        .owners
        .records[0]
        .clone()
}

#[test]
fn replica_converges_on_authoritative_birth_and_death() {
    let (mut sim, catalog) = server();
    let mut replica = replica(&catalog);
    let intent = birth_intent(&sim, &catalog, [12.0, 6.0, 12.0]);
    let born = commit(&mut sim, &catalog, 1, 1, intent);

    assert!(matches!(
        replica.apply_transaction(&born.topology),
        ApplyOutcome::Published { .. }
    ));
    assert!(matches!(
        replica.apply_transaction(&born.topology),
        ApplyOutcome::Duplicate
    ));
    let creatures = replica.creatures().unwrap();
    assert_eq!(creatures.len(), 1);
    let record = creatures.records().next().unwrap().clone();
    assert_eq!(record, server_record(&sim, &catalog));

    let died = commit(
        &mut sim,
        &catalog,
        2,
        2,
        AuthoritativeCreatureIntent::Death {
            key: record.state.key,
            expected_state_revision: record.state.state_revision,
            expected_topology_token: record.state.aggregate_topology_revision,
            expected_pose_token: 0,
        },
    );
    assert!(matches!(
        replica.apply_transaction(&died.topology),
        ApplyOutcome::Published { .. }
    ));
    let replicated = replica.creatures().unwrap().get(&record.state.key).unwrap();
    assert_eq!(replicated.state.lifecycle, CreatureLifecycleState::Corpse);
    assert_eq!(*replicated, server_record(&sim, &catalog));
}

#[test]
fn rejected_lifecycle_leaves_the_replica_unchanged() {
    let (mut sim, catalog) = server();
    let mut replica = replica(&catalog);
    let intent = birth_intent(&sim, &catalog, [12.0, 6.0, 12.0]);
    let born = commit(&mut sim, &catalog, 1, 1, intent);
    assert!(matches!(
        replica.apply_transaction(&born.topology),
        ApplyOutcome::Published { .. }
    ));
    let revision = replica.creatures().unwrap().revision();

    // A replica without the catalog refuses the transaction.
    let mut bare = ReplicaWorld::from_baseline(
        fixtures::flat_terrain_setup().terrain,
        ReplicaConfig::default(),
    );
    assert!(matches!(
        bare.apply_transaction(&born.topology),
        ApplyOutcome::Rejected { .. }
    ));

    // The same transaction id with a different payload is divergent.
    let (mut other_sim, _) = server();
    let other_intent = birth_intent(&other_sim, &catalog, [20.0, 6.0, 20.0]);
    let other = commit(&mut other_sim, &catalog, 1, 1, other_intent);
    assert_eq!(other.topology.transaction_id, born.topology.transaction_id);
    assert!(matches!(
        replica.apply_transaction(&other.topology),
        ApplyOutcome::Rejected { .. }
    ));
    assert_eq!(replica.creatures().unwrap().revision(), revision);
    assert_eq!(replica.creatures().unwrap().len(), 1);
}

fn motion_frame(
    world_epoch: u64,
    server_tick: u64,
    creature_id: u64,
    x: f64,
) -> spall_protocol::CreatureMotionFrame {
    spall_protocol::CreatureMotionFrame {
        schema: spall_protocol::WIRE_SCHEMA_VERSION,
        world_epoch,
        server_tick,
        entries: vec![spall_protocol::CreatureMotionEntry {
            creature_id,
            translation_m: [x, 2.0, 8.0],
            rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
            linear_velocity_m_s: [1.0, 0.0, 0.0],
            locomotion_tag: 1,
            clip_phase_tick: server_tick,
        }],
    }
}

#[test]
fn motion_frames_interpolate_hold_and_ignore_stale_or_foreign_entries() {
    let (mut sim, catalog) = server();
    let mut replica = replica(&catalog);
    let intent = birth_intent(&sim, &catalog, [12.0, 6.0, 12.0]);
    let born = commit(&mut sim, &catalog, 1, 1, intent);
    assert!(matches!(
        replica.apply_transaction(&born.topology),
        ApplyOutcome::Published { .. }
    ));
    let key = replica
        .creatures()
        .unwrap()
        .records()
        .next()
        .unwrap()
        .state
        .key;
    let id = key.creature_id.get();
    let epoch = key.world_epoch.get();
    let t0 = Instant::now();
    let creatures = replica.creatures_mut_for_tests();

    // An unknown creature and a foreign epoch change nothing.
    assert_eq!(
        creatures.apply_motion(&motion_frame(epoch, 10, id + 99, 0.0), t0),
        0
    );
    assert_eq!(
        creatures.apply_motion(&motion_frame(epoch + 1, 10, id, 0.0), t0),
        0
    );

    assert_eq!(
        creatures.apply_motion(&motion_frame(epoch, 10, id, 10.0), t0),
        1
    );
    let later = t0 + Duration::from_millis(100);
    assert_eq!(
        creatures.apply_motion(&motion_frame(epoch, 16, id, 11.0), later),
        1
    );
    // A stale tick is ignored.
    assert_eq!(
        creatures.apply_motion(&motion_frame(epoch, 13, id, 99.0), later),
        0
    );

    let record = creatures.records().next().unwrap().clone();
    // Render time = now - delay: halfway between the two arrivals.
    let halfway = t0 + CREATURE_RENDER_DELAY + Duration::from_millis(50);
    let mid = creatures.presentation(&record, halfway);
    assert!(
        (mid.translation_m[0] - 10.5).abs() < 1e-6,
        "{:?}",
        mid.translation_m
    );
    assert_eq!(mid.locomotion_tag, 1);
    // Past the newest sample it extrapolates along velocity, capped.
    let far = later + CREATURE_RENDER_DELAY + Duration::from_secs(5);
    let held = creatures.presentation(&record, far);
    assert!(
        (held.translation_m[0] - (11.0 + 0.15)).abs() < 1e-6,
        "{:?}",
        held.translation_m
    );
    // A world reset forgets motion.
    creatures.reset();
    assert!(creatures.is_empty());
}

#[test]
fn steady_motion_presents_without_hold_and_jump() {
    let (mut sim, catalog) = server();
    let mut replica = replica(&catalog);
    let intent = birth_intent(&sim, &catalog, [12.0, 6.0, 12.0]);
    let born = commit(&mut sim, &catalog, 1, 1, intent);
    assert!(matches!(
        replica.apply_transaction(&born.topology),
        ApplyOutcome::Published { .. }
    ));
    let key = replica
        .creatures()
        .unwrap()
        .records()
        .next()
        .unwrap()
        .state
        .key;
    let (id, epoch) = (key.creature_id.get(), key.world_epoch.get());
    let creatures = replica.creatures_mut_for_tests();
    let record = creatures.records().next().unwrap().clone();

    // The server moves a walker 0.2 m every 8 ticks (133 ms); frames are drawn
    // every 16 ms and each draw first takes whatever poses have arrived.
    let t0 = Instant::now();
    let move_every = Duration::from_millis(133);
    let mut sent = 0_u32;
    let mut presented = Vec::new();
    for frame in 0..150_u32 {
        let now = t0 + Duration::from_millis(16) * frame;
        while t0 + move_every * sent <= now {
            let tick = 10 + u64::from(sent) * 8;
            creatures.apply_motion(
                &motion_frame(epoch, tick, id, 0.2 * f64::from(sent)),
                t0 + move_every * sent,
            );
            sent += 1;
        }
        presented.push(creatures.presentation(&record, now).translation_m[0]);
    }
    // Once the render delay has passed, every frame advances by about the
    // steady per-frame distance: no frame holds still and none jumps.
    let per_frame = 0.2 * 16.0 / 133.0;
    for pair in presented[20..].windows(2) {
        let step = pair[1] - pair[0];
        assert!(
            step > per_frame * 0.5 && step < per_frame * 1.5,
            "uneven step {step} (expected about {per_frame}); presented {presented:?}"
        );
    }
}
