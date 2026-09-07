//! Immutable read-dependency tokens and the world view they are validated
//! against.
//!
//! A [`JobToken`] is captured when a job is submitted. It records the world
//! [`Generation`] and [`TopologyEpoch`] the job read, plus the exact set of
//! bricks it read and the revision (or explicit *absent* sentinel) it saw for
//! each. The token is never mutated after construction; validation is a pure
//! function of `(token, world)`.
//!
//! At install time the scheduler asks a [`WorldView`] for the *current* status
//! of every dependency. If anything moved — a newer generation, a newer
//! topology epoch, a brick at a different revision, a "missing neighbour" that
//! now has data, or data that has since failed or unloaded — the result is
//! [`Staleness`] and must not be installed.

use spall_core::{BrickCoord, Revision, VolumeId};

use crate::generation::{Generation, TopologyEpoch};

/// Identifies one brick within one volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BrickRef {
    pub volume: VolumeId,
    pub brick: BrickCoord,
}

impl BrickRef {
    pub fn new(volume: VolumeId, brick: BrickCoord) -> Self {
        Self { volume, brick }
    }

    /// Canonical ordering key: volume first, then `(z, y, x)` brick order to
    /// match the rest of the engine.
    fn order_key(&self) -> (u64, i64, i64, i64) {
        let (x, y, z) = (self.brick.x, self.brick.y, self.brick.z);
        (self.volume.get(), z, y, x)
    }
}

/// What a job saw for one brick when it captured its token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepState {
    /// The brick was resident at exactly this revision.
    Revision(Revision),
    /// The brick was treated as absent — a missing-neighbour sentinel. Any
    /// resident data arriving later invalidates the result.
    Absent,
}

/// One entry in a job's read-dependency set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadDep {
    pub brick: BrickRef,
    pub state: DepState,
}

/// The current authoritative status of a brick, as reported by a [`WorldView`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrickStatus {
    /// Resident at this revision.
    Resident(Revision),
    /// Not currently loaded (never existed, or unloaded).
    Absent,
    /// A load was attempted and failed.
    Failed,
}

/// The immutable snapshot of world state a job was computed against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobToken {
    generation: Generation,
    topology_epoch: TopologyEpoch,
    /// Read dependencies, kept sorted by [`BrickRef::order_key`] with one entry
    /// per brick.
    reads: Vec<ReadDep>,
}

impl JobToken {
    /// A token with no brick dependencies — valid only while the generation and
    /// epoch still match.
    pub fn new(generation: Generation, topology_epoch: TopologyEpoch) -> Self {
        Self {
            generation,
            topology_epoch,
            reads: Vec::new(),
        }
    }

    /// Records that the job read `volume`/`brick` at `revision`. A later call for
    /// the same brick replaces the earlier entry.
    #[must_use]
    pub fn reading(mut self, volume: VolumeId, brick: BrickCoord, revision: Revision) -> Self {
        self.put(ReadDep {
            brick: BrickRef::new(volume, brick),
            state: DepState::Revision(revision),
        });
        self
    }

    /// Records that the job treated `volume`/`brick` as absent (a
    /// missing-neighbour sentinel).
    #[must_use]
    pub fn reading_absent(mut self, volume: VolumeId, brick: BrickCoord) -> Self {
        self.put(ReadDep {
            brick: BrickRef::new(volume, brick),
            state: DepState::Absent,
        });
        self
    }

    fn put(&mut self, dep: ReadDep) {
        match self
            .reads
            .binary_search_by(|existing| existing.brick.order_key().cmp(&dep.brick.order_key()))
        {
            Ok(at) => self.reads[at] = dep,
            Err(at) => self.reads.insert(at, dep),
        }
    }

    pub fn generation(&self) -> Generation {
        self.generation
    }

    pub fn topology_epoch(&self) -> TopologyEpoch {
        self.topology_epoch
    }

    /// The read-dependency set, in canonical order.
    pub fn reads(&self) -> &[ReadDep] {
        &self.reads
    }

    /// Validates this token against the current `world`. Checks are ordered
    /// deterministically: generation, then topology epoch, then read
    /// dependencies in canonical order. The first mismatch is returned.
    pub fn check<W: WorldView + ?Sized>(&self, world: &W) -> Staleness {
        let current_generation = world.generation();
        if self.generation != current_generation {
            return Staleness::Generation {
                expected: self.generation,
                current: current_generation,
            };
        }

        let current_epoch = world.topology_epoch();
        if self.topology_epoch != current_epoch {
            return Staleness::TopologyEpoch {
                expected: self.topology_epoch,
                current: current_epoch,
            };
        }

        for dep in &self.reads {
            let current = world.brick_status(dep.brick);
            let ok = match (dep.state, current) {
                (DepState::Revision(expected), BrickStatus::Resident(actual)) => expected == actual,
                (DepState::Absent, BrickStatus::Absent) => true,
                _ => false,
            };
            if !ok {
                return Staleness::BrickRevision {
                    brick: dep.brick,
                    expected: dep.state,
                    current,
                };
            }
        }

        Staleness::Fresh
    }

    /// Convenience: `true` when [`check`](Self::check) is [`Staleness::Fresh`].
    pub fn is_fresh<W: WorldView + ?Sized>(&self, world: &W) -> bool {
        matches!(self.check(world), Staleness::Fresh)
    }
}

/// The outcome of validating a [`JobToken`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Staleness {
    /// Every dependency still matches; the result may be installed.
    Fresh,
    /// The world was reloaded since the job was submitted.
    Generation {
        expected: Generation,
        current: Generation,
    },
    /// Committed topology advanced past what the job read.
    TopologyEpoch {
        expected: TopologyEpoch,
        current: TopologyEpoch,
    },
    /// A brick the job read is no longer at the revision (or absence) it saw.
    BrickRevision {
        brick: BrickRef,
        expected: DepState,
        current: BrickStatus,
    },
}

impl Staleness {
    pub fn is_fresh(&self) -> bool {
        matches!(self, Staleness::Fresh)
    }
}

/// A read-only view of current authoritative world state, used to validate job
/// results. Implemented by the simulation for real runs and by
/// [`crate::testkit`] for tests.
pub trait WorldView {
    /// The current world / session generation.
    fn generation(&self) -> Generation;
    /// The current topology epoch.
    fn topology_epoch(&self) -> TopologyEpoch;
    /// The current status of one brick.
    fn brick_status(&self, brick: BrickRef) -> BrickStatus;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedWorld {
        generation: Generation,
        epoch: TopologyEpoch,
        bricks: Vec<(BrickRef, BrickStatus)>,
    }

    impl WorldView for FixedWorld {
        fn generation(&self) -> Generation {
            self.generation
        }
        fn topology_epoch(&self) -> TopologyEpoch {
            self.epoch
        }
        fn brick_status(&self, brick: BrickRef) -> BrickStatus {
            self.bricks
                .iter()
                .find(|(b, _)| *b == brick)
                .map(|(_, s)| *s)
                .unwrap_or(BrickStatus::Absent)
        }
    }

    fn vol(n: u64) -> VolumeId {
        VolumeId::new(n).unwrap()
    }

    #[test]
    fn reads_are_deduplicated_and_canonically_ordered() {
        let token = JobToken::new(Generation(1), TopologyEpoch(0))
            .reading(vol(1), BrickCoord::new(5, 0, 0), Revision(1))
            .reading(vol(1), BrickCoord::new(0, 0, 0), Revision(2))
            .reading(vol(1), BrickCoord::new(5, 0, 0), Revision(9)); // replaces

        let reads = token.reads();
        assert_eq!(reads.len(), 2);
        assert_eq!(reads[0].brick.brick, BrickCoord::new(0, 0, 0));
        assert_eq!(reads[1].brick.brick, BrickCoord::new(5, 0, 0));
        assert_eq!(reads[1].state, DepState::Revision(Revision(9)));
    }

    #[test]
    fn fresh_when_every_dependency_matches() {
        let b = BrickRef::new(vol(1), BrickCoord::new(0, 0, 0));
        let world = FixedWorld {
            generation: Generation(3),
            epoch: TopologyEpoch(4),
            bricks: vec![(b, BrickStatus::Resident(Revision(7)))],
        };
        let token = JobToken::new(Generation(3), TopologyEpoch(4)).reading(
            vol(1),
            BrickCoord::new(0, 0, 0),
            Revision(7),
        );
        assert_eq!(token.check(&world), Staleness::Fresh);
        assert!(token.is_fresh(&world));
    }

    #[test]
    fn generation_mismatch_wins_over_brick_mismatch() {
        let world = FixedWorld {
            generation: Generation(4),
            epoch: TopologyEpoch(0),
            bricks: vec![],
        };
        let token = JobToken::new(Generation(3), TopologyEpoch(1)).reading(
            vol(1),
            BrickCoord::new(0, 0, 0),
            Revision(1),
        );
        assert_eq!(
            token.check(&world),
            Staleness::Generation {
                expected: Generation(3),
                current: Generation(4)
            }
        );
    }

    #[test]
    fn absent_sentinel_is_invalidated_when_data_arrives() {
        let b = BrickRef::new(vol(2), BrickCoord::new(-1, 0, 0));
        let token = JobToken::new(Generation(1), TopologyEpoch(0))
            .reading_absent(vol(2), BrickCoord::new(-1, 0, 0));

        let still_absent = FixedWorld {
            generation: Generation(1),
            epoch: TopologyEpoch(0),
            bricks: vec![(b, BrickStatus::Absent)],
        };
        assert!(token.is_fresh(&still_absent));

        let now_resident = FixedWorld {
            generation: Generation(1),
            epoch: TopologyEpoch(0),
            bricks: vec![(b, BrickStatus::Resident(Revision(1)))],
        };
        assert_eq!(
            token.check(&now_resident),
            Staleness::BrickRevision {
                brick: b,
                expected: DepState::Absent,
                current: BrickStatus::Resident(Revision(1)),
            }
        );
    }

    #[test]
    fn revision_dependency_fails_on_unload_or_failure() {
        let b = BrickRef::new(vol(1), BrickCoord::new(0, 0, 0));
        let token = JobToken::new(Generation(1), TopologyEpoch(0)).reading(
            vol(1),
            BrickCoord::new(0, 0, 0),
            Revision(2),
        );
        for status in [
            BrickStatus::Absent,
            BrickStatus::Failed,
            BrickStatus::Resident(Revision(3)),
        ] {
            let world = FixedWorld {
                generation: Generation(1),
                epoch: TopologyEpoch(0),
                bricks: vec![(b, status)],
            };
            assert!(!token.is_fresh(&world), "{status:?} should be stale");
        }
    }
}
