//! Collection of independent, explicitly rebased physics regions.
//!
//! This adapter owns solver worlds only; authoritative geometry and entity
//! IDs remain with `spall_sim`. Callers provide a freshly built `BodySpec`
//! when transferring an entity, after validating the corresponding authority
//! snapshot.

use std::collections::BTreeMap;

use crate::character::{CharacterMove, CharacterParams};
use crate::coordinates::PhysicsOrigin;
use crate::occupancy::ExtractError;
use crate::query_cache::{CharacterQueryCache, RebuildCost};
use crate::world::{BodyId, BodySpec, BodyState, PhysicsConfig, PhysicsWorld, StepTiming};
use spall_voxel::Volume;

#[derive(Debug, Clone, Copy)]
pub struct RegionalBodyState {
    pub region: u32,
    pub local: BodyState,
    pub world_translation_m: [f64; 3],
}

#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum PhysicsRegionSetError {
    #[error("physics region {0} already exists")]
    DuplicateRegion(u32),
    #[error("physics region {0} does not exist")]
    UnknownRegion(u32),
    #[error("body namespace {0} does not exist")]
    UnknownBodyNamespace(u32),
    #[error("world pose cannot be represented in the target physics region")]
    PoseOutOfRange,
    #[error("transfer tolerances must be finite and nonnegative")]
    InvalidTolerance,
    #[error("staged body differs from the source beyond transfer tolerance")]
    TransferValidationFailed,
    #[error("physics region {0} still has active bodies")]
    RegionNotEmpty(u32),
    #[error("terrain query-window build failed: {0}")]
    TerrainExtraction(#[from] ExtractError),
}

struct RegionWorld {
    origin: PhysicsOrigin,
    physics: PhysicsWorld,
}

/// Multiple independent solver worlds with disjoint body-id namespaces.
#[derive(Default)]
pub struct PhysicsRegionSet {
    regions: BTreeMap<u32, RegionWorld>,
}

impl PhysicsRegionSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create_region(
        &mut self,
        region: u32,
        origin: PhysicsOrigin,
        config: PhysicsConfig,
    ) -> Result<(), PhysicsRegionSetError> {
        if self.regions.contains_key(&region) {
            return Err(PhysicsRegionSetError::DuplicateRegion(region));
        }
        self.regions.insert(
            region,
            RegionWorld {
                origin,
                physics: PhysicsWorld::new_in_namespace(config, region),
            },
        );
        Ok(())
    }

    pub fn origin(&self, region: u32) -> Option<PhysicsOrigin> {
        self.regions.get(&region).map(|world| world.origin)
    }

    pub fn add_body(
        &mut self,
        region: u32,
        mut spec: BodySpec,
        world_translation_m: [f64; 3],
    ) -> Result<BodyId, PhysicsRegionSetError> {
        let world = self
            .regions
            .get_mut(&region)
            .ok_or(PhysicsRegionSetError::UnknownRegion(region))?;
        spec.translation_m = world
            .origin
            .to_local_f32(world_translation_m)
            .ok_or(PhysicsRegionSetError::PoseOutOfRange)?;
        Ok(world.physics.add_body(spec))
    }

    pub fn state(&self, id: BodyId) -> Result<RegionalBodyState, PhysicsRegionSetError> {
        let region = id.namespace();
        let world = self
            .regions
            .get(&region)
            .ok_or(PhysicsRegionSetError::UnknownBodyNamespace(region))?;
        let local = world.physics.body_state(id);
        Ok(RegionalBodyState {
            region,
            local,
            world_translation_m: world.origin.to_world_f64(local.translation_m),
        })
    }

    /// Step all active regions in ascending namespace order.
    pub fn step_all(&mut self) -> Vec<(u32, StepTiming)> {
        self.regions
            .iter_mut()
            .map(|(id, world)| (*id, world.physics.step()))
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn sweep_character(
        &mut self,
        region: u32,
        cache: &mut CharacterQueryCache,
        volume: &Volume,
        cell_m: f32,
        params: CharacterParams,
        world_position_m: [f64; 3],
        desired_translation_m: [f32; 3],
        dt_s: f32,
        revision: u64,
        exclude: &[BodyId],
    ) -> Result<(CharacterMove, Option<RebuildCost>), PhysicsRegionSetError> {
        let world = self
            .regions
            .get_mut(&region)
            .ok_or(PhysicsRegionSetError::UnknownRegion(region))?;
        let local_position_m = world
            .origin
            .to_local_f64(world_position_m)
            .ok_or(PhysicsRegionSetError::PoseOutOfRange)?;
        let (_, rebuild_cost) = cache.ensure_covers_in_frame(
            &mut world.physics,
            volume,
            cell_m,
            world_position_m,
            revision,
            world.origin,
        )?;
        let movement = world.physics.sweep_character_excluding(
            params,
            local_position_m,
            desired_translation_m,
            dt_s,
            exclude,
        );
        Ok((movement, rebuild_cost))
    }

    pub fn active_body_count(&self, region: u32) -> Result<usize, PhysicsRegionSetError> {
        self.regions
            .get(&region)
            .map(|world| world.physics.active_body_count())
            .ok_or(PhysicsRegionSetError::UnknownRegion(region))
    }

    pub fn contact_pair_count(&self, region: u32) -> Result<usize, PhysicsRegionSetError> {
        self.regions
            .get(&region)
            .map(|world| world.physics.contact_pair_count())
            .ok_or(PhysicsRegionSetError::UnknownRegion(region))
    }

    pub fn set_body_velocity(
        &mut self,
        id: BodyId,
        linear_m_s: [f32; 3],
        angular_rad_s: [f32; 3],
    ) -> Result<(), PhysicsRegionSetError> {
        self.regions
            .get_mut(&id.namespace())
            .ok_or(PhysicsRegionSetError::UnknownBodyNamespace(id.namespace()))?
            .physics
            .set_body_velocity(id, linear_m_s, angular_rad_s);
        Ok(())
    }

    /// Stage a rebuilt authoritative body in the destination frame, validate
    /// pose and velocities, then retire the source. Until validation succeeds
    /// there is one live owner; validation failure retires the staged copy.
    pub fn transfer_body(
        &mut self,
        source_id: BodyId,
        destination_region: u32,
        mut destination_spec: BodySpec,
        position_tolerance_m: f64,
        velocity_tolerance_m_s: f64,
    ) -> Result<BodyId, PhysicsRegionSetError> {
        if !position_tolerance_m.is_finite()
            || position_tolerance_m < 0.0
            || !velocity_tolerance_m_s.is_finite()
            || velocity_tolerance_m_s < 0.0
        {
            return Err(PhysicsRegionSetError::InvalidTolerance);
        }
        let source_region = source_id.namespace();
        if source_region == destination_region {
            return Ok(source_id);
        }
        let source = self
            .regions
            .get(&source_region)
            .ok_or(PhysicsRegionSetError::UnknownBodyNamespace(source_region))?;
        let destination_origin = self
            .regions
            .get(&destination_region)
            .ok_or(PhysicsRegionSetError::UnknownRegion(destination_region))?
            .origin;
        let source_state = source.physics.body_state(source_id);
        let source_world_translation = source.origin.to_world_f64(source_state.translation_m);
        destination_spec.translation_m = destination_origin
            .to_local_f32(source_world_translation)
            .ok_or(PhysicsRegionSetError::PoseOutOfRange)?;
        destination_spec.linvel_m_s = source_state.linvel_m_s;

        let destination_id = self
            .regions
            .get_mut(&destination_region)
            .expect("destination existence checked above")
            .physics
            .add_body(destination_spec);
        {
            let destination = self
                .regions
                .get_mut(&destination_region)
                .expect("destination existence checked above");
            destination.physics.set_body_pose(
                destination_id,
                destination_origin
                    .to_local_f32(source_world_translation)
                    .expect("pose preflighted"),
                source_state.rotation,
            );
            destination.physics.set_body_velocity(
                destination_id,
                source_state.linvel_m_s,
                source_state.angvel_rad_s,
            );
        }
        let staged = self.state(destination_id)?;
        let position_error = distance(staged.world_translation_m, source_world_translation);
        let velocity_error = distance(
            staged.local.linvel_m_s.map(f64::from),
            source_state.linvel_m_s.map(f64::from),
        );
        let angular_error = distance(
            staged.local.angvel_rad_s.map(f64::from),
            source_state.angvel_rad_s.map(f64::from),
        );
        let rotation_error = distance4(
            staged.local.rotation.map(f64::from),
            source_state.rotation.map(f64::from),
        );
        if !staged.local.is_finite()
            || position_error > position_tolerance_m
            || velocity_error > velocity_tolerance_m_s
            || angular_error > velocity_tolerance_m_s
            || rotation_error > 0.0001
        {
            self.regions
                .get_mut(&destination_region)
                .expect("destination still exists")
                .physics
                .retire_body(destination_id);
            return Err(PhysicsRegionSetError::TransferValidationFailed);
        }
        self.regions
            .get_mut(&source_region)
            .expect("source still exists")
            .physics
            .retire_body(source_id);
        Ok(destination_id)
    }

    pub fn remove_empty_region(&mut self, region: u32) -> Result<(), PhysicsRegionSetError> {
        let world = self
            .regions
            .get(&region)
            .ok_or(PhysicsRegionSetError::UnknownRegion(region))?;
        if world.physics.active_body_count() != 0 {
            return Err(PhysicsRegionSetError::RegionNotEmpty(region));
        }
        self.regions.remove(&region);
        Ok(())
    }
}

fn distance(a: [f64; 3], b: [f64; 3]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

fn distance4(a: [f64; 4], b: [f64; 4]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2) + (a[3] - b[3]).powi(2))
        .sqrt()
}
