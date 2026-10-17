//! Coordinator-owned lifecycle rejection tests, independent of worker fixtures.
use super::*;
use spall_core::{CellSizeCode, GlobalCell, MaterialId, MaterialManifest, VolumeId};
use spall_physics::{PartTransform, Representation};
use spall_protocol::{
    CreatureClipMode, CreatureLifecycleState, CreatureStateV1, content_manifest_hash,
};
use spall_voxel::{EditPlan, Volume};

struct Fixture {
    sim: Simulation,
    materials: MaterialManifest,
    catalog: crate::creatures::tests::FixtureCatalog,
}

impl Fixture {
    fn new() -> Self {
        let setup = crate::fixtures::flat_terrain_setup();
        let materials = setup.materials.clone();
        let mut sim = Simulation::new(SimulationConfig::new(setup)).unwrap();
        sim.initialize_creature_host(WorldId::from_u128(10))
            .unwrap();
        Self {
            sim,
            materials,
            catalog: crate::creatures::tests::FixtureCatalog::new(),
        }
    }

    fn state(&self) -> CreatureStateV1 {
        let id = self.sim.world.registry().counters().1;
        let mut state = self.catalog.state(1, id);
        state.material_manifest_hash = content_manifest_hash(&self.materials);
        state
    }

    fn birth(
        &mut self,
        state: CreatureStateV1,
        material: MaterialId,
    ) -> Result<spall_physics::ArticulatedBodyId, crate::creatures::CreatureScheduleError> {
        let id = state.source_bindings[0].source_volume_id;
        let mut source = Volume::new(id, CellSizeCode::Quarter);
        source
            .apply_edit(&EditPlan::filled_box(
                id,
                GlobalCell::new(0, 0, 0),
                GlobalCell::new(0, 0, 0),
                material,
            ))
            .unwrap();
        let mut view = self.catalog.view();
        view.materials = &self.materials;
        self.sim.commit_creature_birth_for_test(
            state,
            &view,
            vec![CreatureBirthPartInput {
                part_id: 1,
                parent_id: None,
                local_transform: PartTransform::IDENTITY,
                volume: source,
            }],
            Representation::MergedCuboids,
        )
    }

    fn assert_empty_unchanged(
        &self,
        registry: (u64, u64, u64, u64),
        host: spall_core::CreatureAllocatorSnapshotV1,
        bodies: usize,
        source_hash: spall_protocol::Hash32,
    ) {
        assert_eq!(self.sim.world.registry().counters(), registry);
        assert_eq!(self.sim.creature_allocator_snapshot().unwrap(), host);
        assert_eq!(self.sim.world.physics().body_count(), bodies);
        assert_eq!(self.sim.world.creature_source_hash_for_test(), source_hash);
        assert!(
            self.sim
                .creatures
                .as_ref()
                .unwrap()
                .record(spall_core::CreatureId::new(1).unwrap())
                .is_none()
        );
    }
}

#[test]
fn corpse_cannot_be_born_as_a_living_physics_owner() {
    let mut f = Fixture::new();
    let mut state = f.state();
    state.lifecycle = CreatureLifecycleState::Corpse;
    state.clip_mode = CreatureClipMode::Frozen;
    let registry = f.sim.world.registry().counters();
    let host = f.sim.creature_allocator_snapshot().unwrap();
    let bodies = f.sim.world.physics().body_count();
    let hash = f.sim.world.creature_source_hash_for_test();
    assert!(f.birth(state, MaterialId(1)).is_err());
    f.assert_empty_unchanged(registry, host, bodies, hash);
    assert!(
        f.birth(f.state(), MaterialId(1)).is_ok(),
        "rejection must permit the exact same next IDs on retry"
    );
}

#[test]
fn source_cells_require_known_and_mapped_authoritative_materials() {
    for material in [MaterialId(65000), crate::fixtures::DIRT] {
        let mut f = Fixture::new();
        let registry = f.sim.world.registry().counters();
        let host = f.sim.creature_allocator_snapshot().unwrap();
        let bodies = f.sim.world.physics().body_count();
        let hash = f.sim.world.creature_source_hash_for_test();
        assert!(
            f.birth(f.state(), material).is_err(),
            "unknown or unmapped source material {material:?} was accepted"
        );
        f.assert_empty_unchanged(registry, host, bodies, hash);
    }
}

#[test]
fn otherwise_valid_foreign_manifest_cannot_bind_actor_to_world() {
    let mut f = Fixture::new();
    let state = f.catalog.state(1, f.sim.world.registry().counters().1);
    let mut source = Volume::new(
        state.source_bindings[0].source_volume_id,
        CellSizeCode::Quarter,
    );
    source
        .apply_edit(&EditPlan::filled_box(
            source.id(),
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(0, 0, 0),
            MaterialId(1),
        ))
        .unwrap();
    let registry = f.sim.world.registry().counters();
    let host = f.sim.creature_allocator_snapshot().unwrap();
    let bodies = f.sim.world.physics().body_count();
    let hash = f.sim.world.creature_source_hash_for_test();
    assert!(
        f.sim
            .commit_creature_birth_for_test(
                state,
                &f.catalog.view(),
                vec![CreatureBirthPartInput {
                    part_id: 1,
                    parent_id: None,
                    local_transform: PartTransform::IDENTITY,
                    volume: source
                }],
                Representation::MergedCuboids
            )
            .is_err()
    );
    f.assert_empty_unchanged(registry, host, bodies, hash);
}

#[test]
fn exhausted_source_allocator_cannot_publish_actor_or_physics() {
    let mut f = Fixture::new();
    let old = f.sim.world.registry().counters();
    *f.sim.world.registry_mut() =
        crate::registry::IdRegistry::resume(old.0, u64::MAX, old.2, old.3).unwrap();
    let mut state = f.state();
    state.source_bindings[0].source_volume_id = VolumeId::new(u64::MAX).unwrap();
    let registry = f.sim.world.registry().counters();
    let host = f.sim.creature_allocator_snapshot().unwrap();
    let bodies = f.sim.world.physics().body_count();
    let hash = f.sim.world.creature_source_hash_for_test();
    assert!(f.birth(state, MaterialId(1)).is_err());
    f.assert_empty_unchanged(registry, host, bodies, hash);
}

#[test]
fn world_reset_retires_source_and_physics_without_reusing_identity() {
    let mut f = Fixture::new();
    let state = f.state();
    let owner = f.birth(state.clone(), MaterialId(1)).unwrap();
    let allocated = f.sim.creature_allocator_snapshot().unwrap();
    let counters = f.sim.world.registry().counters();
    let fresh =
        Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
    f.sim.replace_world(fresh).unwrap();
    f.sim.wait_for_retired();
    let reset = f.sim.creature_allocator_snapshot().unwrap();
    assert_eq!(reset.next_creature_id, allocated.next_creature_id);
    assert_eq!(reset.world_epoch, allocated.world_epoch + 1);
    assert!(f.sim.world.registry().counters().1 >= counters.1);
    assert!(
        f.sim
            .world
            .creature_source_owner_for_test(state.source_bindings[0].source_volume_id)
            .is_none()
    );
    assert!(
        f.sim
            .world
            .creature_physics_owner_for_test(state.key)
            .is_none()
    );
    assert!(
        f.sim
            .world
            .physics()
            .articulated_owner_snapshot(owner)
            .is_none()
    );
    assert!(
        f.sim
            .creatures
            .as_ref()
            .unwrap()
            .record(state.key.creature_id)
            .is_none()
    );
    // The reset world is again complete under the legacy terrain/body hash.
    let _ = f.sim.world.world_hash();
}

#[test]
fn initial_twist_must_match_the_stationary_birth_adapter() {
    for angular in [false, true] {
        let mut f = Fixture::new();
        let mut state = f.state();
        if angular {
            state.angular_velocity_rad_s = [0.0, 1.0, 0.0];
        } else {
            state.linear_velocity_m_s = [1.0, 0.0, 0.0];
        }
        let registry = f.sim.world.registry().counters();
        let host = f.sim.creature_allocator_snapshot().unwrap();
        let bodies = f.sim.world.physics().body_count();
        let hash = f.sim.world.creature_source_hash_for_test();
        assert!(f.birth(state, MaterialId(1)).is_err());
        f.assert_empty_unchanged(registry, host, bodies, hash);
    }
}

#[test]
fn corpse_record_uses_transferred_part_momentum_instead_of_living_root_twist() {
    let mut f = Fixture::new();
    let mut state = f.state();
    state.root_translation_m = [0.0, 4.0, 0.0];
    let owner = f.birth(state.clone(), MaterialId(1)).unwrap();
    f.sim
        .world
        .physics_mut()
        .update_articulated_pose(
            owner,
            spall_physics::ArticulatedPose {
                topology_token: state.aggregate_topology_revision,
                base_pose_token: 0,
                pose_token: 1,
                parts: vec![(
                    1,
                    PartTransform {
                        translation_m: [0.25, 0.0, 0.0],
                        ..PartTransform::IDENTITY
                    },
                )],
            },
        )
        .unwrap();
    f.sim.world.step_physics();
    let before = f
        .sim
        .world
        .physics()
        .articulated_owner_snapshot(owner)
        .unwrap();
    assert_eq!(before.root.linvel_m_s, [0.0; 3]);
    let mut view = f.catalog.view();
    view.materials = &f.materials;
    let outcome = f
        .sim
        .commit_creature_death_for_test(
            state.key,
            state.state_revision,
            state.aggregate_topology_revision,
            1,
            &view,
        )
        .unwrap();
    let dynamic = f
        .sim
        .world
        .physics()
        .articulated_owner_snapshot(owner)
        .unwrap()
        .root;
    assert!(
        outcome.linear_momentum_kg_m_s[0].abs() > 1.0,
        "fixture must transfer moving source momentum"
    );
    let corpse = f
        .sim
        .creatures
        .as_ref()
        .unwrap()
        .record(state.key.creature_id)
        .unwrap();
    assert_eq!(
        corpse.linear_velocity_m_s, dynamic.linvel_m_s,
        "recorded corpse motion must equal published physics"
    );
    assert_eq!(corpse.angular_velocity_rad_s, dynamic.angvel_rad_s);
}

#[test]
fn owner_rejection_of_resultant_motion_preserves_living_rig() {
    let mut f = Fixture::new();
    let mut state = f.state();
    state.root_translation_m = [0.0, 4.0, 0.0];
    let owner = f.birth(state.clone(), MaterialId(1)).unwrap();
    f.sim
        .world
        .physics_mut()
        .update_articulated_pose(
            owner,
            spall_physics::ArticulatedPose {
                topology_token: state.aggregate_topology_revision,
                base_pose_token: 0,
                pose_token: 1,
                parts: vec![(
                    1,
                    PartTransform {
                        translation_m: [250.0, 0.0, 0.0],
                        ..PartTransform::IDENTITY
                    },
                )],
            },
        )
        .unwrap();
    f.sim.world.step_physics();
    let bodies = f.sim.world.physics().body_count();
    let counters = f.sim.world.registry().counters();
    let allocator = f.sim.creature_allocator_snapshot().unwrap();
    let sources = f.sim.world.creature_source_hash_for_test();
    let before = f
        .sim
        .world
        .physics()
        .articulated_owner_snapshot(owner)
        .unwrap();
    let mut saw_result = false;
    let result = f
        .sim
        .world
        .physics_mut()
        .transfer_articulated_to_dynamic_corpse_checked(
            owner,
            state.aggregate_topology_revision,
            1,
            |material| {
                f.materials
                    .get(material)
                    .map_or(0.0, |m| f64::from(m.sim.density_kg_m3))
            },
            |outcome| {
                assert!(outcome.linear_velocity_m_s[0] > 0.0);
                saw_result = true;
                false
            },
        );
    assert!(
        saw_result,
        "fixture must reach the final owner validation hook"
    );
    assert_eq!(
        result.unwrap_err(),
        spall_physics::ArticulatedError::TransferRejected
    );
    assert_eq!(f.sim.world.physics().body_count(), bodies);
    assert_eq!(f.sim.world.registry().counters(), counters);
    assert_eq!(f.sim.creature_allocator_snapshot().unwrap(), allocator);
    assert_eq!(f.sim.world.creature_source_hash_for_test(), sources);
    assert_eq!(
        f.sim
            .creatures
            .as_ref()
            .unwrap()
            .record(state.key.creature_id)
            .unwrap(),
        &state
    );
    let after = f
        .sim
        .world
        .physics()
        .articulated_owner_snapshot(owner)
        .unwrap();
    assert_eq!(after.root.linvel_m_s, before.root.linvel_m_s);
    assert_eq!(after.pose_token, before.pose_token);
    // Only a living rig can accept another animation sample.
    f.sim
        .world
        .physics_mut()
        .update_articulated_pose(
            owner,
            spall_physics::ArticulatedPose {
                topology_token: state.aggregate_topology_revision,
                base_pose_token: 1,
                pose_token: 2,
                parts: vec![(
                    1,
                    PartTransform {
                        translation_m: [250.0, 0.0, 0.0],
                        ..PartTransform::IDENTITY
                    },
                )],
            },
        )
        .unwrap();
}

#[test]
fn exhausted_creature_allocator_cannot_consume_source_ids_or_publish_physics() {
    let mut f = Fixture::new();
    let mut snapshot = f.sim.creature_allocator_snapshot().unwrap();
    snapshot.next_creature_id = u64::MAX;
    f.sim.creatures =
        Some(crate::creatures::CreatureHost::from_allocator_snapshot(snapshot).unwrap());
    let mut state = f.state();
    state.key.creature_id = spall_core::CreatureId::new(u64::MAX).unwrap();
    let registry = f.sim.world.registry().counters();
    let bodies = f.sim.world.physics().body_count();
    let hash = f.sim.world.creature_source_hash_for_test();
    assert!(f.birth(state, MaterialId(1)).is_err());
    f.assert_empty_unchanged(registry, snapshot, bodies, hash);
}
