//! An in-memory [`WorldView`] for tests.
//!
//! Available to this crate's own tests and, behind the `testkit` feature, to
//! downstream crates (T05 meshing, T07 structure) so they can exercise result
//! validation without standing up a real simulation.

use std::collections::HashMap;

use spall_core::{BrickCoord, Revision, VolumeId};

use crate::generation::{Generation, TopologyEpoch};
use crate::token::{BrickRef, BrickStatus, WorldView};

/// A mutable, hand-driven world view. Unknown bricks read as
/// [`BrickStatus::Absent`].
#[derive(Debug, Clone, Default)]
pub struct MapWorld {
    generation: Generation,
    epoch: TopologyEpoch,
    bricks: HashMap<BrickRef, BrickStatus>,
}

impl MapWorld {
    pub fn new(generation: Generation) -> Self {
        Self {
            generation,
            epoch: TopologyEpoch::START,
            bricks: HashMap::new(),
        }
    }

    #[must_use]
    pub fn with_epoch(mut self, epoch: TopologyEpoch) -> Self {
        self.epoch = epoch;
        self
    }

    pub fn set_generation(&mut self, generation: Generation) {
        self.generation = generation;
    }

    pub fn set_epoch(&mut self, epoch: TopologyEpoch) {
        self.epoch = epoch;
    }

    /// Mark a brick resident at `revision`.
    pub fn set_brick(&mut self, volume: VolumeId, brick: BrickCoord, revision: Revision) {
        self.bricks.insert(
            BrickRef::new(volume, brick),
            BrickStatus::Resident(revision),
        );
    }

    /// Mark a brick's load as failed.
    pub fn fail_brick(&mut self, volume: VolumeId, brick: BrickCoord) {
        self.bricks
            .insert(BrickRef::new(volume, brick), BrickStatus::Failed);
    }

    /// Drop a brick so it reads as absent again.
    pub fn unload_brick(&mut self, volume: VolumeId, brick: BrickCoord) {
        self.bricks.remove(&BrickRef::new(volume, brick));
    }
}

impl WorldView for MapWorld {
    fn generation(&self) -> Generation {
        self.generation
    }

    fn topology_epoch(&self) -> TopologyEpoch {
        self.epoch
    }

    fn brick_status(&self, brick: BrickRef) -> BrickStatus {
        self.bricks
            .get(&brick)
            .copied()
            .unwrap_or(BrickStatus::Absent)
    }
}
