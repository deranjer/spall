//! Complete checkpoint integration; no journal actor replay or spawn acceptance.
use spall_core::*;
use spall_physics::PhysicsConfig;
use spall_protocol::*;
use spall_server::persist::{self, PersistConfig, RecoveryChoice};
use spall_sim::{Simulation, SimulationConfig};
use spall_store::{CrashPoint, FaultPlan, Writer};
use spall_structure::AnchorPlane;

struct Scratch(std::path::PathBuf);
impl Scratch {
    fn new() -> Self {
        let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.local")
            .canonicalize()
            .unwrap();
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        for _ in 0..32 {
            let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = base.join(format!(
                "actor-checkpoint-{}-{}-{sequence}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
            ));
            match std::fs::create_dir(&dir) {
                Ok(()) => return Self(dir),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create owned checkpoint scratch directory: {error}"),
            }
        }
        panic!("could not reserve a unique checkpoint scratch directory");
    }

    fn db(&self) -> std::path::PathBuf {
        self.0.join("world.db")
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.local")
            .canonicalize()
            .unwrap();
        assert_eq!(self.0.parent(), Some(base.as_path()));
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn cfg() -> PersistConfig {
    PersistConfig {
        world_id: 10,
        seed: 1,
        generator_version: 1,
    }
}
struct Catalog {
    materials: MaterialManifest,
    assets: [CreatureAssetCatalogEntry<'static>; 1],
    species: [CreatureSpeciesCatalogEntry<'static>; 1],
    mapping: [CreatureMaterialMappingEntry<'static>; 1],
}
impl Catalog {
    fn new(materials: MaterialManifest) -> Self {
        let asset = CreatureAssetIdentity {
            uuid: [8; 16],
            logical_hash: Hash32([9; 32]),
        };
        Self {
            materials,
            assets: [CreatureAssetCatalogEntry {
                uuid: asset.uuid,
                logical_hash: asset.logical_hash,
                part_ids: &[1, 2, 3],
                clip_names: &["idle"],
                portable_material_keys: &["solid"],
            }],
            species: [CreatureSpeciesCatalogEntry {
                key: "fixture",
                version: 1,
                enabled: true,
                asset,
            }],
            mapping: [CreatureMaterialMappingEntry {
                portable_key: "solid",
                material_id: MaterialId(1),
            }],
        }
    }
    fn view(&self) -> CreatureValidationCatalog<'_> {
        CreatureValidationCatalog {
            assets: &self.assets,
            species: &self.species,
            materials: &self.materials,
            material_mapping_version: 1,
            material_mapping_hash: creature_material_mapping_hash(&self.mapping).unwrap(),
            material_mapping: &self.mapping,
        }
    }
}
fn fixture(empty: bool) -> (Simulation, Catalog) {
    let mut sim = Simulation::new(SimulationConfig::new(
        spall_sim::fixtures::flat_terrain_setup(),
    ))
    .unwrap();
    let cat = Catalog::new(sim.world().materials().clone());
    let mut next = sim.world().registry().counters().1;
    let mut records = Vec::new();
    if !empty {
        for id in 1..=2 {
            let corpse = id == 2;
            let owner_key = 100 + id as u128;
            let root = [10.0 * id as f64, 8.0, 10.0];
            let mut parts = Vec::new();
            let mut masks = Vec::new();
            let mut bindings = Vec::new();
            for part_id in 1..=3 {
                let volume = VolumeId::new(next).unwrap();
                next += 1;
                let mut cells = vec![0_u16; 32 * 32 * 32];
                cells[31] = 1;
                let mut part = CreatureSourcePartV2 {
                    part_id,
                    source_volume_id: volume,
                    source_generation: if corpse { 9 } else { 1 },
                    source_revision: 1,
                    next_brick_revision: 2,
                    cell_size_code: CellSizeCode::Quarter.to_u8(),
                    bounds: None,
                    source_content_hash: Hash32::ZERO,
                    parent_id: if part_id == 1 {
                        None
                    } else {
                        Some(part_id - 1)
                    },
                    initial_local_pose: CreatureLocalPoseV1 {
                        translation_m: if part_id == 1 {
                            [0.0; 3]
                        } else {
                            [0.25, 0.0, 0.0]
                        },
                        rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                    },
                    frozen_local_pose: CreatureLocalPoseV1 {
                        translation_m: if part_id == 1 {
                            [0.0; 3]
                        } else {
                            [0.25, 0.0, 0.0]
                        },
                        rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                    },
                    bricks: vec![BaselineBrick {
                        coord: [-1, 0, 0],
                        revision: 1,
                        edited: true,
                        cells: BaselineCells::Dense(cells),
                    }],
                };
                part.source_content_hash = part.canonical_source_content_hash();
                parts.push(part);
                bindings.push(CreaturePartBinding {
                    part_id,
                    source_volume_id: volume,
                    source_revision: 1,
                });
                masks.push(CreatureSourceMaskBrickV1 {
                    volume_id: volume,
                    part_id,
                    coord: [-1, 0, 0],
                    source_revision: 1,
                    brick_revision: 1,
                    cells: vec![CreatureMaskCellV1 {
                        index: 31,
                        material_id: 1,
                    }],
                });
            }
            let state = CreatureStateV1 {
                schema: WIRE_SCHEMA_VERSION,
                key: CreatureKey::new(
                    WorldId::from_u128(10),
                    WorldEpoch::INITIAL,
                    CreatureId::new(id).unwrap(),
                    CreatureGeneration::INITIAL,
                )
                .unwrap(),
                state_revision: 1,
                lifecycle: if corpse {
                    CreatureLifecycleState::Corpse
                } else {
                    CreatureLifecycleState::Alive
                },
                species_key: "fixture".into(),
                species_version: 1,
                asset: cat.species[0].asset,
                material_manifest_hash: content_manifest_hash(&cat.materials),
                material_mapping_version: 1,
                material_mapping_hash: creature_material_mapping_hash(&cat.mapping).unwrap(),
                clip_name: "idle".into(),
                clip_mode: if corpse {
                    CreatureClipMode::Frozen
                } else {
                    CreatureClipMode::Playing
                },
                clip_phase_tick: 4,
                clip_subtick_remainder: 0,
                clip_epoch: 1,
                source_bindings: bindings,
                aggregate_topology_revision: 2,
                pose_epoch: 3,
                root_translation_m: root,
                root_rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                linear_velocity_m_s: [0.5, 0.0, 0.0],
                angular_velocity_rad_s: [0.0, 0.0, 0.25],
                locomotion_tag: 0,
            };
            let component =
                |serial, translation, source_masks, velocity| CreatureOwnedComponentV1 {
                    owner_key,
                    serial,
                    translation_m: translation,
                    rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                    linear_velocity_m_s: velocity,
                    angular_velocity_rad_s: [0.0, 0.0, 0.25],
                    source_masks,
                };
            let components = if corpse {
                vec![
                    component(
                        Some(7),
                        [22.0, 8.0, 10.0],
                        vec![masks[2].clone()],
                        [-0.5, 0.0, 0.0],
                    ),
                    component(Some(41), root, masks[..2].to_vec(), [0.5, 0.0, 0.0]),
                ]
            } else {
                vec![component(None, root, masks, [0.5, 0.0, 0.0])]
            };
            records.push(CreatureOwnershipRecordV4 {
                schema: WIRE_SCHEMA_VERSION,
                state,
                owner_key,
                source_generation: if corpse { 9 } else { 1 },
                next_component_serial: if corpse { 100 } else { 1 },
                primary_component_serial: if corpse { Some(41) } else { None },
                part_velocities: if corpse {
                    vec![]
                } else {
                    (1..=3)
                        .map(|part_id| CreaturePartVelocityV2 {
                            part_id,
                            linear_velocity_m_s: if part_id == 1 {
                                [0.5, 0.0, 0.0]
                            } else {
                                [0.0, 0.25 * part_id as f32, 0.0]
                            },
                            angular_velocity_rad_s: [0.0, 0.0, 0.25],
                        })
                        .collect()
                },
                parts,
                components,
            });
        }
    }
    let c = sim.world().registry().counters();
    sim.world_mut()
        .resume_registry(c.0, next + 10, c.2, c.3)
        .unwrap();
    let snapshot = CreatureSnapshotV2 {
        schema: WIRE_SCHEMA_VERSION,
        allocator: CreatureAllocatorSnapshotV1 {
            schema: spall_core::STORE_SCHEMA_VERSION_U16,
            world_id: WorldId::from_u128(10),
            world_epoch: 1,
            next_creature_id: 100,
        },
        next_volume_id: next + 10,
        owners: CreatureOwnershipSetV4 {
            schema: WIRE_SCHEMA_VERSION,
            records,
        },
    };
    snapshot.validate(&cat.view()).unwrap();
    let prepared = sim
        .prepare_creature_restore(WorldId::from_u128(10), &snapshot, &cat.view())
        .unwrap();
    sim.commit_prepared_creature_restore(prepared, &cat.view())
        .unwrap();
    (sim, cat)
}
fn restore(
    recovery: &spall_store::Recovery,
    cat: &Catalog,
) -> Result<(Simulation, u64), persist::PersistError> {
    persist::restore_with_catalog(
        recovery,
        &cfg(),
        RecoveryChoice::RequireClean,
        cat.materials.clone(),
        AnchorPlane::at(0),
        PhysicsConfig::default(),
        &cat.view(),
    )
}
#[test]
fn complete_animals_and_empty_allocator_survive_atomic_sqlite_checkpoint() {
    for empty in [false, true] {
        let (sim, cat) = fixture(empty);
        let saved = sim.capture_creature_snapshot(&cat.view()).unwrap().unwrap();
        let cp = persist::capture_with_catalog(&sim, &cfg(), 0, &cat.view()).unwrap();
        assert!(persist::capture(&sim, &cfg(), 0).is_err());
        let scratch = Scratch::new();
        let mut writer = Writer::open(scratch.db()).unwrap();
        writer.publish_checkpoint(&cp).unwrap();
        drop(writer);
        let recovered = spall_store::recover(scratch.db()).unwrap();
        assert_eq!(recovered.checkpoint, cp);
        assert!(matches!(
            persist::restore(
                &recovered,
                &cfg(),
                RecoveryChoice::RequireClean,
                cat.materials.clone(),
                AnchorPlane::at(0),
                PhysicsConfig::default()
            ),
            Err(persist::PersistError::CreatureCatalogRequired)
        ));
        let (restored, cursor) = restore(&recovered, &cat).unwrap();
        assert_eq!(cursor, 0);
        assert_eq!(
            restored
                .capture_creature_snapshot(&cat.view())
                .unwrap()
                .unwrap(),
            saved
        );
        let again = persist::capture_with_catalog(&restored, &cfg(), 0, &cat.view()).unwrap();
        assert!(
            again.creatures == cp.creatures,
            "complete animal section changed"
        );
        assert_eq!(again.meta, cp.meta);
        assert_eq!(again.bodies, cp.bodies);
        assert!(again.bricks == cp.bricks, "ordinary geometry changed");
        assert_eq!(again.world_hash, cp.world_hash);
        assert_eq!(again.weather, cp.weather);
        // Snow loading intentionally marks it dirty for surface publication.
        // Compare complete reloaded state with that existing normalization.
        let normalized = |bytes: &Option<Vec<u8>>| {
            bytes.as_ref().map(|bytes| {
                spall_sim::snow::SnowState::from_canonical_bytes(bytes)
                    .unwrap()
                    .to_canonical_bytes()
                    .unwrap()
            })
        };
        assert_eq!(normalized(&again.snow), normalized(&cp.snow));
        assert_eq!(again.vegetation, cp.vegetation);
        assert_eq!(again.column_water, cp.column_water);
    }
}
#[test]
fn checkpoint_actor_hash_excludes_motion_but_full_section_and_counters_are_bound() {
    let (sim, cat) = fixture(false);
    let cp = persist::capture_with_catalog(&sim, &cfg(), 0, &cat.view()).unwrap();
    let snapshot = &cp.creatures.as_ref().unwrap().snapshot;
    let mut moved = snapshot.clone();
    let record = &mut moved.owners.records[0];
    record.state.root_translation_m[0] += 1.0;
    record.components[0].translation_m = record.state.root_translation_m;
    assert_ne!(moved.canonical_hash(), snapshot.canonical_hash());
    assert_eq!(moved.topology_hash(), snapshot.topology_hash());
    assert_eq!(
        spall_protocol::creature_snapshot::checkpoint_topology_hash(
            sim.world().geometry_topology_hash(),
            None
        ),
        sim.world().geometry_topology_hash()
    );
    let scratch = Scratch::new();
    let mut writer = Writer::open(scratch.db()).unwrap();
    writer.publish_checkpoint(&cp).unwrap();
    let mut bad = cp.clone();
    bad.creatures.as_mut().unwrap().snapshot = moved;
    assert!(writer.publish_checkpoint(&bad).is_err());
    assert_eq!(spall_store::recover(scratch.db()).unwrap().checkpoint, cp);
    for mode in 0..4 {
        let mut bad = cp.clone();
        let section = bad.creatures.as_mut().unwrap();
        match mode {
            0 => bad.meta.world_id += 1,
            1 => bad.meta.next_volume += 1,
            2 => section.snapshot_hash[0] ^= 1,
            _ => section.physics_origin_m[0] = f64::NAN,
        };
        assert!(writer.publish_checkpoint(&bad).is_err());
    }
}
#[test]
fn actor_checkpoint_failures_never_publish_partial_rows_and_after_commit_is_complete() {
    for point in [
        CrashPoint::MidCheckpointRows,
        CrashPoint::BeforeCheckpointCommit,
        CrashPoint::AfterCheckpointCommit,
    ] {
        let (sim, cat) = fixture(false);
        let old = persist::capture_with_catalog(&sim, &cfg(), 0, &cat.view()).unwrap();
        let mut new = old.clone();
        new.tick = 1;
        let scratch = Scratch::new();
        let mut writer = Writer::open(scratch.db()).unwrap();
        writer.publish_checkpoint(&old).unwrap();
        writer.set_faults(FaultPlan::crash(point));
        assert!(writer.publish_checkpoint(&new).is_err());
        drop(writer);
        let recovered = spall_store::recover(scratch.db()).unwrap();
        assert_eq!(
            recovered.checkpoint,
            if point == CrashPoint::AfterCheckpointCommit {
                new
            } else {
                old
            }
        );
        restore(&recovered, &cat).unwrap();
    }
}
#[test]
fn actor_rows_are_required_bounded_and_crc_checked_with_explicit_fallback() {
    for sql in [
        "DELETE FROM checkpoint_creatures WHERE tick=1",
        "UPDATE checkpoint_creatures SET crc=zeroblob(16) WHERE tick=1",
        "UPDATE checkpoint_creatures SET state=x'' WHERE tick=1",
        "UPDATE checkpoint_creatures SET has_host=0 WHERE tick=1",
    ] {
        let (sim, cat) = fixture(true);
        let old = persist::capture_with_catalog(&sim, &cfg(), 0, &cat.view()).unwrap();
        let mut new = old.clone();
        new.tick = 1;
        let scratch = Scratch::new();
        let mut writer = Writer::open(scratch.db()).unwrap();
        writer.publish_checkpoint(&old).unwrap();
        writer.publish_checkpoint(&new).unwrap();
        drop(writer);
        let conn = rusqlite::Connection::open(scratch.db()).unwrap();
        conn.execute_batch(sql).unwrap();
        drop(conn);
        let recovered = spall_store::recover(scratch.db()).unwrap();
        assert_eq!(recovered.checkpoint, old);
        assert!(!recovered.corruption.is_empty());
        assert!(restore(&recovered, &cat).is_err());
        persist::restore_with_catalog(
            &recovered,
            &cfg(),
            RecoveryChoice::AcceptDurablePrefix,
            cat.materials.clone(),
            AnchorPlane::at(0),
            PhysicsConfig::default(),
            &cat.view(),
        )
        .unwrap();
    }
}
#[test]
fn unsupported_actor_suffix_and_catalog_changes_fail_closed() {
    let (sim, cat) = fixture(false);
    let cp = persist::capture_with_catalog(&sim, &cfg(), 0, &cat.view()).unwrap();
    let scratch = Scratch::new();
    let mut writer = Writer::open(scratch.db()).unwrap();
    writer.publish_checkpoint(&cp).unwrap();
    let record = spall_store::JournalRecord {
        seq: 1,
        tick: 1,
        payload: spall_store::JournalPayload::VegetationClock {
            time_ms: 0,
            credit_ms: 0,
        },
    };
    assert!(
        writer
            .append_journal(std::slice::from_ref(&record))
            .is_err()
    );
    drop(writer);
    let mut recovered = spall_store::recover(scratch.db()).unwrap();
    assert_eq!(recovered.checkpoint, cp);
    assert!(recovered.journal.is_empty());
    recovered.journal.push(record);
    assert!(matches!(
        restore(&recovered, &cat),
        Err(persist::PersistError::CreatureJournalUnsupported)
    ));
    recovered.journal.clear();
    let mut wrong = cat.view();
    wrong.material_mapping_version += 1;
    assert!(
        persist::restore_with_catalog(
            &recovered,
            &cfg(),
            RecoveryChoice::RequireClean,
            cat.materials.clone(),
            AnchorPlane::at(0),
            PhysicsConfig::default(),
            &wrong
        )
        .is_err()
    );
}

#[test]
fn explicit_older_checkpoint_selection_reloads_its_own_journal_suffix() {
    let sim = Simulation::new(SimulationConfig::new(
        spall_sim::fixtures::flat_terrain_setup(),
    ))
    .unwrap();
    let old = persist::capture(&sim, &cfg(), 0).unwrap();
    let scratch = Scratch::new();
    let mut writer = Writer::open(scratch.db()).unwrap();
    writer.publish_checkpoint(&old).unwrap();
    let row = spall_store::JournalRecord {
        seq: 1,
        tick: 1,
        payload: spall_store::JournalPayload::VegetationClock {
            time_ms: 0,
            credit_ms: 0,
        },
    };
    writer.append_journal(std::slice::from_ref(&row)).unwrap();
    let mut latest = old.clone();
    latest.tick = 1;
    latest.journal_cursor = 1;
    latest.meta.next_journal_seq = 2;
    writer.publish_checkpoint(&latest).unwrap();
    drop(writer);
    assert!(
        spall_store::recover(scratch.db())
            .unwrap()
            .journal
            .is_empty()
    );
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    let selected =
        spall_store::recover_conn_from(&conn, spall_store::RecoverBase::AtOrBefore(0)).unwrap();
    assert_eq!(selected.checkpoint, old);
    assert_eq!(selected.journal, vec![row]);
}

#[test]
fn animal_publication_refuses_existing_suffix_and_cannot_drop_initialized_host() {
    let (sim, cat) = fixture(true);
    let actor = persist::capture_with_catalog(&sim, &cfg(), 0, &cat.view()).unwrap();
    let mut plain = actor.clone();
    plain.creatures = None;
    let scratch = Scratch::new();
    let mut writer = Writer::open(scratch.db()).unwrap();
    writer.publish_checkpoint(&plain).unwrap();
    let row = spall_store::JournalRecord {
        seq: 1,
        tick: 1,
        payload: spall_store::JournalPayload::VegetationClock {
            time_ms: 0,
            credit_ms: 0,
        },
    };
    writer.append_journal(std::slice::from_ref(&row)).unwrap();
    let mut later = actor.clone();
    later.tick = 1;
    assert!(writer.publish_checkpoint(&later).is_err());
    drop(writer);
    let recovered = spall_store::recover(scratch.db()).unwrap();
    assert_eq!(recovered.checkpoint, plain);
    assert_eq!(recovered.journal, vec![row]);
    let scratch = Scratch::new();
    let mut writer = Writer::open(scratch.db()).unwrap();
    writer.publish_checkpoint(&actor).unwrap();
    plain.tick = 1;
    assert!(writer.publish_checkpoint(&plain).is_err());
    drop(writer);
    assert_eq!(
        spall_store::recover(scratch.db()).unwrap().checkpoint,
        actor
    );
}

#[test]
fn saved_nonzero_physics_origin_restores_without_rounding_any_animal_pose() {
    let (live, cat) = fixture(false);
    let snapshot = live
        .capture_creature_snapshot(&cat.view())
        .unwrap()
        .unwrap();
    let world = spall_sim::SimWorld::new_with_terrain_collider_mode_and_origin(
        spall_sim::fixtures::flat_terrain_setup(),
        spall_sim::world::TerrainColliderMode::PerBrick,
        spall_physics::PhysicsOrigin::new([8.0, 0.0, 8.0]).unwrap(),
    )
    .unwrap();
    let mut shifted = Simulation::from_restored(world, Tick(0));
    let c = shifted.world().registry().counters();
    shifted
        .world_mut()
        .resume_registry(c.0, snapshot.next_volume_id, c.2, c.3)
        .unwrap();
    let prepared = shifted
        .prepare_creature_restore(snapshot.allocator.world_id, &snapshot, &cat.view())
        .unwrap();
    shifted
        .commit_prepared_creature_restore(prepared, &cat.view())
        .unwrap();
    let cp = persist::capture_with_catalog(&shifted, &cfg(), 0, &cat.view()).unwrap();
    assert_eq!(
        cp.creatures.as_ref().unwrap().physics_origin_m,
        [8.0, 0.0, 8.0]
    );
    let scratch = Scratch::new();
    let mut writer = Writer::open(scratch.db()).unwrap();
    writer.publish_checkpoint(&cp).unwrap();
    drop(writer);
    let recovered = spall_store::recover(scratch.db()).unwrap();
    let (rebuilt, _) = restore(&recovered, &cat).unwrap();
    assert_eq!(rebuilt.world().physics_origin().world_m(), [8.0, 0.0, 8.0]);
    assert_eq!(
        rebuilt
            .capture_creature_snapshot(&cat.view())
            .unwrap()
            .unwrap(),
        snapshot
    );
}

#[test]
fn rehashed_duplicate_creature_id_across_generations_refuses_before_host_map_install() {
    let (sim, cat) = fixture(false);
    let mut cp = persist::capture_with_catalog(&sim, &cfg(), 0, &cat.view()).unwrap();
    let section = cp.creatures.as_mut().unwrap();
    section.snapshot.owners.records[1].state.key.creature_id =
        section.snapshot.owners.records[0].state.key.creature_id;
    section.snapshot.owners.records[1].state.key.generation = CreatureGeneration::new(2).unwrap();
    section.snapshot_hash = section.snapshot.canonical_hash().0;
    assert!(section.snapshot.validate(&cat.view()).is_err());
    let scratch = Scratch::new();
    let mut writer = Writer::open(scratch.db()).unwrap();
    assert!(writer.publish_checkpoint(&cp).is_err());
    assert!(matches!(
        spall_store::recover(scratch.db()),
        Err(spall_store::StoreError::NoCheckpoint)
    ));
}

#[test]
fn journal_writer_refuses_a_missing_latest_creature_row_instead_of_skipping_it() {
    let (sim, cat) = fixture(true);
    let mut cp = persist::capture_with_catalog(&sim, &cfg(), 0, &cat.view()).unwrap();
    let scratch = Scratch::new();
    let mut writer = Writer::open(scratch.db()).unwrap();
    cp.creatures = None;
    writer.publish_checkpoint(&cp).unwrap();
    let mut actor = persist::capture_with_catalog(&sim, &cfg(), 0, &cat.view()).unwrap();
    actor.tick = 1;
    writer.publish_checkpoint(&actor).unwrap();
    drop(writer);
    let conn = rusqlite::Connection::open(scratch.db()).unwrap();
    conn.execute("DELETE FROM checkpoint_creatures WHERE tick=1", [])
        .unwrap();
    drop(conn);
    let mut writer = Writer::open(scratch.db()).unwrap();
    let row = spall_store::JournalRecord {
        seq: 1,
        tick: 1,
        payload: spall_store::JournalPayload::VegetationClock {
            time_ms: 0,
            credit_ms: 0,
        },
    };
    assert!(writer.append_journal(&[row]).is_err());
    drop(writer);
    let recovery = spall_store::recover(scratch.db()).unwrap();
    assert_eq!(recovery.checkpoint, cp);
    assert!(recovery.journal.is_empty());
    assert!(!recovery.corruption.is_empty());
}

#[test]
fn actor_checkpoint_cannot_rewind_a_pruned_durable_checkpoint_cursor() {
    let (sim, cat) = fixture(true);
    let actor = persist::capture_with_catalog(&sim, &cfg(), 0, &cat.view()).unwrap();
    let mut old = actor.clone();
    old.creatures = None;
    old.journal_cursor = 5;
    old.meta.next_journal_seq = 6;
    let scratch = Scratch::new();
    let mut writer = Writer::open(scratch.db()).unwrap();
    writer.publish_checkpoint(&old).unwrap();
    assert_eq!(writer.durable_journal_high_water().unwrap(), 5);
    let mut later = actor;
    later.tick = 1;
    assert!(writer.publish_checkpoint(&later).is_err());
    drop(writer);
    assert_eq!(spall_store::recover(scratch.db()).unwrap().checkpoint, old);
}

#[test]
fn corrupt_actor_free_section_cannot_authorize_an_append_or_replacement() {
    for replace in [false, true] {
        let sim = Simulation::new(SimulationConfig::new(
            spall_sim::fixtures::flat_terrain_setup(),
        ))
        .unwrap();
        let cp = persist::capture(&sim, &cfg(), 0).unwrap();
        let scratch = Scratch::new();
        let mut writer = Writer::open(scratch.db()).unwrap();
        writer.publish_checkpoint(&cp).unwrap();
        drop(writer);
        let conn = rusqlite::Connection::open(scratch.db()).unwrap();
        conn.execute("UPDATE checkpoint_creatures SET crc=zeroblob(16)", [])
            .unwrap();
        drop(conn);
        let mut writer = Writer::open(scratch.db()).unwrap();
        if replace {
            let mut new = cp.clone();
            new.tick = 1;
            assert!(writer.publish_checkpoint(&new).is_err());
        } else {
            let row = spall_store::JournalRecord {
                seq: 1,
                tick: 1,
                payload: spall_store::JournalPayload::VegetationClock {
                    time_ms: 0,
                    credit_ms: 0,
                },
            };
            assert!(writer.append_journal(&[row]).is_err());
        }
        drop(writer);
        let conn = rusqlite::Connection::open(scratch.db()).unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM journal", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM checkpoints", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}

#[test]
fn full_actor_capture_refuses_logical_geometry_with_evicted_bricks() {
    let (mut sim, cat) = fixture(false);
    let before = sim.capture_creature_snapshot(&cat.view()).unwrap().unwrap();
    let terrain = sim.world().terrain().volume.id();
    let coord = sim.world().terrain().volume.resident_brick_coords()[0];
    let geometry = sim.world().geometry_topology_hash();
    assert!(sim.world_mut().evict_brick(terrain, coord).unwrap());
    assert!(sim.world().has_evicted());
    assert_eq!(sim.world().geometry_topology_hash(), geometry);
    assert!(matches!(
        persist::capture_with_catalog(&sim, &cfg(), 0, &cat.view()),
        Err(persist::PersistError::BadField(
            "catalog checkpoint requires complete resident geometry"
        ))
    ));
    assert_eq!(
        sim.capture_creature_snapshot(&cat.view()).unwrap().unwrap(),
        before
    );
}

#[path = "support/animal_stream_checkpoint.rs"]
mod streamed;

#[path = "support/animal_journal_attachments.rs"]
mod journal_attachments;
