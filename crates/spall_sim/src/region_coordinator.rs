//! Deterministic control-plane for rebased physics regions.
//!
//! This layer tracks stable authoritative entity ownership independently of
//! solver-local handles. It plans migration/merge operations; applying those
//! plans to physics worlds remains an explicit tick-boundary operation.

use std::collections::{BTreeMap, BTreeSet};

use spall_physics::{
    BodyId, BodySpec, PhysicsConfig, PhysicsOrigin, PhysicsRegionSet, PhysicsRegionSetError,
    RegionalBodyState,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PhysicsRegionId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RegionMergePlan {
    pub survivor: PhysicsRegionId,
    pub retired: PhysicsRegionId,
    pub survivor_origin: PhysicsOrigin,
    pub retired_origin: PhysicsOrigin,
    pub transferred_entities: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum RegionCoordinatorError {
    #[error("region {0:?} already exists")]
    DuplicateRegion(PhysicsRegionId),
    #[error("region id space exhausted")]
    IdExhausted,
    #[error("region IDs exceed the physics body namespace range")]
    PhysicsNamespaceExhausted,
    #[error("region {0:?} does not exist")]
    UnknownRegion(PhysicsRegionId),
    #[error("body entity {0} does not exist")]
    UnknownBody(u64),
    #[error("entity {entity} already belongs to region {owner:?}")]
    DuplicateOwner { entity: u64, owner: PhysicsRegionId },
    #[error("physics region coordinate must be finite")]
    NonFiniteOrigin,
    #[error("entity {0} has no world pose for region merge preflight")]
    MissingWorldPose(u64),
    #[error("entity {0} cannot be represented in the survivor physics frame")]
    DestinationOutOfRange(u64),
    #[error("region merge distance threshold or measured separation is invalid")]
    InvalidMergeDistance,
    #[error("regions are {separation_m} m apart, beyond merge threshold {threshold_m} m")]
    RegionsTooFar { separation_m: f64, threshold_m: f64 },
    #[error("region merge still has live physics bodies; transfer them first")]
    LivePhysicsBodiesRequireTransfer,
    #[error("physics region operation failed: {0}")]
    Physics(#[from] PhysicsRegionSetError),
}

#[derive(Debug)]
struct Region {
    origin: PhysicsOrigin,
    entities: BTreeSet<u64>,
}

/// Single-authority ownership index for region-local physics frames.
///
/// Entity IDs are the caller's monotonic world IDs (never solver body IDs).
/// Merging rewrites one owner entry per entity and emits the source/destination
/// origin pair needed by the tick-boundary physics handoff.
#[derive(Default)]
pub struct RegionCoordinator {
    next_id: u64,
    regions: BTreeMap<PhysicsRegionId, Region>,
    owner: BTreeMap<u64, PhysicsRegionId>,
    body_ids: BTreeMap<u64, BodyId>,
    physics: PhysicsRegionSet,
}

impl RegionCoordinator {
    pub fn create_region(
        &mut self,
        origin_m: [f64; 3],
    ) -> Result<PhysicsRegionId, RegionCoordinatorError> {
        self.create_region_with_config(origin_m, PhysicsConfig::default())
    }

    pub fn create_region_with_config(
        &mut self,
        origin_m: [f64; 3],
        config: PhysicsConfig,
    ) -> Result<PhysicsRegionId, RegionCoordinatorError> {
        let origin = PhysicsOrigin::new(origin_m).ok_or(RegionCoordinatorError::NonFiniteOrigin)?;
        let id = PhysicsRegionId(self.next_id);
        let namespace =
            u32::try_from(id.0).map_err(|_| RegionCoordinatorError::PhysicsNamespaceExhausted)?;
        self.physics.create_region(namespace, origin, config)?;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or(RegionCoordinatorError::IdExhausted)?;
        self.regions.insert(
            id,
            Region {
                origin,
                entities: BTreeSet::new(),
            },
        );
        Ok(id)
    }

    /// Install a body in its one authoritative region physics world. The
    /// entity key is the stable world ID; its solver handle stays internal.
    pub fn add_body(
        &mut self,
        entity: u64,
        region: PhysicsRegionId,
        spec: BodySpec,
        world_position_m: [f64; 3],
    ) -> Result<BodyId, RegionCoordinatorError> {
        if let Some(owner) = self.owner.get(&entity).copied() {
            return Err(RegionCoordinatorError::DuplicateOwner { entity, owner });
        }
        if !self.regions.contains_key(&region) {
            return Err(RegionCoordinatorError::UnknownRegion(region));
        }
        let namespace = u32::try_from(region.0)
            .map_err(|_| RegionCoordinatorError::PhysicsNamespaceExhausted)?;
        let body = self.physics.add_body(namespace, spec, world_position_m)?;
        self.assign(entity, region)?;
        self.body_ids.insert(entity, body);
        Ok(body)
    }

    pub fn body_state(&self, entity: u64) -> Option<RegionalBodyState> {
        let body = self.body_ids.get(&entity).copied()?;
        self.physics.state(body).ok()
    }

    pub fn step_physics(&mut self) -> Vec<(u32, spall_physics::StepTiming)> {
        self.physics.step_all()
    }

    pub fn active_physics_bodies(
        &self,
        region: PhysicsRegionId,
    ) -> Result<usize, RegionCoordinatorError> {
        Ok(self.physics.active_body_count(
            u32::try_from(region.0)
                .map_err(|_| RegionCoordinatorError::PhysicsNamespaceExhausted)?,
        )?)
    }

    /// Rebuild and stage an entity in the destination region before retiring
    /// its source handle, then atomically update the stable ownership index.
    pub fn transfer_body(
        &mut self,
        entity: u64,
        destination: PhysicsRegionId,
        destination_spec: BodySpec,
        position_tolerance_m: f64,
        velocity_tolerance_m_s: f64,
    ) -> Result<BodyId, RegionCoordinatorError> {
        let source = self
            .owner
            .get(&entity)
            .copied()
            .ok_or(RegionCoordinatorError::UnknownBody(entity))?;
        let source_body = self
            .body_ids
            .get(&entity)
            .copied()
            .ok_or(RegionCoordinatorError::UnknownBody(entity))?;
        if source == destination {
            return Ok(source_body);
        }
        if !self.regions.contains_key(&destination) {
            return Err(RegionCoordinatorError::UnknownRegion(destination));
        }
        let destination_namespace = u32::try_from(destination.0)
            .map_err(|_| RegionCoordinatorError::PhysicsNamespaceExhausted)?;
        let staged = self.physics.transfer_body(
            source_body,
            destination_namespace,
            destination_spec,
            position_tolerance_m,
            velocity_tolerance_m_s,
        )?;
        self.regions
            .get_mut(&source)
            .ok_or(RegionCoordinatorError::UnknownRegion(source))?
            .entities
            .remove(&entity);
        self.regions
            .get_mut(&destination)
            .ok_or(RegionCoordinatorError::UnknownRegion(destination))?
            .entities
            .insert(entity);
        self.owner.insert(entity, destination);
        self.body_ids.insert(entity, staged);
        Ok(staged)
    }

    pub fn region_count(&self) -> usize {
        self.regions.len()
    }
    pub fn entity_count(&self) -> usize {
        self.owner.len()
    }
    pub fn region_for(&self, entity: u64) -> Option<PhysicsRegionId> {
        self.owner.get(&entity).copied()
    }
    pub fn origin(&self, region: PhysicsRegionId) -> Option<PhysicsOrigin> {
        self.regions.get(&region).map(|r| r.origin)
    }

    pub fn assign(
        &mut self,
        entity: u64,
        region: PhysicsRegionId,
    ) -> Result<(), RegionCoordinatorError> {
        if let Some(owner) = self.owner.get(&entity).copied() {
            if owner == region {
                return Ok(());
            }
            return Err(RegionCoordinatorError::DuplicateOwner { entity, owner });
        }
        let target = self
            .regions
            .get_mut(&region)
            .ok_or(RegionCoordinatorError::UnknownRegion(region))?;
        target.entities.insert(entity);
        self.owner.insert(entity, region);
        Ok(())
    }

    /// Merge two nearby regions, choosing the lower stable region ID as the
    /// survivor. Callers must apply the returned transfer at a tick boundary.
    pub fn merge(
        &mut self,
        a: PhysicsRegionId,
        b: PhysicsRegionId,
        separation_m: f64,
        threshold_m: f64,
        world_positions_m: &BTreeMap<u64, [f64; 3]>,
    ) -> Result<RegionMergePlan, RegionCoordinatorError> {
        if !separation_m.is_finite() || !threshold_m.is_finite() || threshold_m < 0.0 {
            return Err(RegionCoordinatorError::InvalidMergeDistance);
        }
        if separation_m > threshold_m {
            return Err(RegionCoordinatorError::RegionsTooFar {
                separation_m,
                threshold_m,
            });
        }
        let (survivor, retired) = if a <= b { (a, b) } else { (b, a) };
        if survivor == retired {
            return Err(RegionCoordinatorError::UnknownRegion(retired));
        }
        if !self.regions.contains_key(&survivor) {
            return Err(RegionCoordinatorError::UnknownRegion(survivor));
        }
        let survivor_origin = self.regions[&survivor].origin;
        let retired_region = self
            .regions
            .get(&retired)
            .ok_or(RegionCoordinatorError::UnknownRegion(retired))?;
        if retired_region
            .entities
            .iter()
            .any(|entity| self.body_ids.contains_key(entity))
        {
            return Err(RegionCoordinatorError::LivePhysicsBodiesRequireTransfer);
        }
        // Validate the entire handoff before mutating ownership or removing the
        // source region, so an incomplete/out-of-range plan is atomic.
        for entity in &retired_region.entities {
            let position = world_positions_m
                .get(entity)
                .ok_or(RegionCoordinatorError::MissingWorldPose(*entity))?;
            if survivor_origin.to_local_f32(*position).is_none() {
                return Err(RegionCoordinatorError::DestinationOutOfRange(*entity));
            }
        }
        self.physics.remove_empty_region(
            u32::try_from(retired.0)
                .map_err(|_| RegionCoordinatorError::PhysicsNamespaceExhausted)?,
        )?;
        let retired_region = self
            .regions
            .remove(&retired)
            .ok_or(RegionCoordinatorError::UnknownRegion(retired))?;
        let survivor_region = self.regions.get_mut(&survivor).expect("checked above");
        let transferred_entities = retired_region.entities.len();
        for entity in retired_region.entities {
            survivor_region.entities.insert(entity);
            self.owner.insert(entity, survivor);
        }
        Ok(RegionMergePlan {
            survivor,
            retired,
            survivor_origin,
            retired_origin: retired_region.origin,
            transferred_entities,
        })
    }

    pub fn entities_in(&self, region: PhysicsRegionId) -> Option<impl Iterator<Item = u64> + '_> {
        self.regions
            .get(&region)
            .map(|r| r.entities.iter().copied())
    }
}
