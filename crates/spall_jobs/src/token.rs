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
    /// A load had failed. Keeping this distinct from [`Absent`](Self::Absent)
    /// makes a result valid while the same failure is still in force, while any
    /// retry, successful load, or eviction invalidates it.
    Failed,
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
    /// The whole-volume state the reads were taken from, when the producer knows it. While the
    /// world still reports this exact stamp for that volume, nothing in it changed, so every
    /// read is valid without comparing them one by one.
    state_stamp: Option<(VolumeId, u64)>,
}

impl JobToken {
    /// A token with no brick dependencies — valid only while the generation and
    /// epoch still match.
    pub fn new(generation: Generation, topology_epoch: TopologyEpoch) -> Self {
        Self {
            generation,
            topology_epoch,
            reads: Vec::new(),
            state_stamp: None,
        }
    }

    /// Records that every read was taken from `volume` in exactly the state `stamp` (see
    /// [`WorldView::state_stamp`]). Only valid if all of this token's reads are of `volume`.
    #[must_use]
    pub fn with_state_stamp(mut self, volume: VolumeId, stamp: u64) -> Self {
        debug_assert!(
            self.reads.iter().all(|dep| dep.brick.volume == volume),
            "a state stamp covers one volume's reads only"
        );
        self.state_stamp = Some((volume, stamp));
        self
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

    /// Records that the job observed a failed load for `volume`/`brick`.
    #[must_use]
    pub fn reading_failed(mut self, volume: VolumeId, brick: BrickCoord) -> Self {
        self.put(ReadDep {
            brick: BrickRef::new(volume, brick),
            state: DepState::Failed,
        });
        self
    }

    /// Adds a dependency batch in one canonical sort. As with the individual
    /// builders, the last observation of a brick replaces earlier ones.
    /// Avoids shifting the whole read set for every missing-neighbour sentinel.
    #[must_use]
    pub fn reading_many(mut self, reads: impl IntoIterator<Item = ReadDep>) -> Self {
        self.reads.extend(reads);
        self.reads.sort_by_key(|dep| dep.brick.order_key());
        self.reads.dedup_by(|later, earlier| {
            if later.brick == earlier.brick {
                *earlier = *later;
                true
            } else {
                false
            }
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

        if let Some((volume, stamp)) = self.state_stamp
            && world.state_stamp(volume) == Some(stamp)
        {
            // The volume is bit-for-bit the state the reads came from. Debug builds still
            // run the full comparison, so every test checks the shortcut against it.
            debug_assert_eq!(
                self.check_reads(world),
                Staleness::Fresh,
                "an unchanged volume stamp must imply fresh reads"
            );
            return Staleness::Fresh;
        }
        self.check_reads(world)
    }

    fn check_reads<W: WorldView + ?Sized>(&self, world: &W) -> Staleness {
        for dep in &self.reads {
            let current = world.brick_status(dep.brick);
            let ok = match (dep.state, current) {
                (DepState::Revision(expected), BrickStatus::Resident(actual)) => expected == actual,
                (DepState::Absent, BrickStatus::Absent) => true,
                (DepState::Failed, BrickStatus::Failed) => true,
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
    /// An opaque stamp that changes whenever anything in `volume` changes, if the world keeps
    /// one. Equal stamps (of the same volume) mean identical contents. The default reports
    /// none, which only makes validation compare every read.
    fn state_stamp(&self, _volume: VolumeId) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bulk_dependencies_match_incremental_order_and_last_observation() {
        let mut reads = Vec::new();
        for i in (0..128).rev() {
            for volume in [vol(2), vol(1)] {
                let brick = BrickRef::new(volume, BrickCoord::new(i % 7, i % 3, i));
                reads.push(ReadDep {
                    brick,
                    state: DepState::Revision(Revision(i as u64)),
                });
                if i % 5 == 0 {
                    reads.push(ReadDep {
                        brick,
                        state: DepState::Absent,
                    });
                }
                if i % 11 == 0 {
                    reads.push(ReadDep {
                        brick,
                        state: DepState::Failed,
                    });
                }
            }
        }
        let prefix = JobToken::new(Generation(3), TopologyEpoch(4)).reading(
            vol(1),
            BrickCoord::new(0, 0, 0),
            Revision(999),
        );
        let mut reference = prefix.clone();
        for dep in &reads {
            reference = match dep.state {
                DepState::Revision(revision) => {
                    reference.reading(dep.brick.volume, dep.brick.brick, revision)
                }
                DepState::Absent => reference.reading_absent(dep.brick.volume, dep.brick.brick),
                DepState::Failed => reference.reading_failed(dep.brick.volume, dep.brick.brick),
            };
        }
        assert_eq!(prefix.reading_many(reads), reference);
    }

    struct FixedWorld {
        generation: Generation,
        epoch: TopologyEpoch,
        bricks: Vec<(BrickRef, BrickStatus)>,
        stamp: Option<u64>,
    }

    impl WorldView for FixedWorld {
        fn state_stamp(&self, _volume: VolumeId) -> Option<u64> {
            self.stamp
        }
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

    fn stamped_world(stamp: Option<u64>, revision: u64) -> FixedWorld {
        FixedWorld {
            generation: Generation(1),
            epoch: TopologyEpoch(0),
            bricks: vec![(
                BrickRef::new(vol(1), BrickCoord::new(0, 0, 0)),
                BrickStatus::Resident(Revision(revision)),
            )],
            stamp,
        }
    }

    fn stamped_token(stamp: u64) -> JobToken {
        JobToken::new(Generation(1), TopologyEpoch(0))
            .reading(vol(1), BrickCoord::new(0, 0, 0), Revision(3))
            .with_state_stamp(vol(1), stamp)
    }

    #[test]
    fn a_matching_state_stamp_is_fresh() {
        assert_eq!(
            stamped_token(7).check(&stamped_world(Some(7), 3)),
            Staleness::Fresh
        );
    }

    #[test]
    fn a_changed_state_stamp_falls_back_to_comparing_every_read() {
        // The stamp moved and the brick the job read changed with it: stale.
        assert!(matches!(
            stamped_token(7).check(&stamped_world(Some(8), 4)),
            Staleness::BrickRevision { .. }
        ));
        // The stamp moved (something unrelated changed) but the job's brick did not: still fresh.
        assert_eq!(
            stamped_token(7).check(&stamped_world(Some(8), 3)),
            Staleness::Fresh
        );
        // A world that keeps no stamps always compares.
        assert!(matches!(
            stamped_token(7).check(&stamped_world(None, 4)),
            Staleness::BrickRevision { .. }
        ));
    }

    #[test]
    fn a_matching_stamp_does_not_excuse_a_stale_epoch_or_generation() {
        let mut world = stamped_world(Some(7), 3);
        world.epoch = TopologyEpoch(1);
        assert!(matches!(
            stamped_token(7).check(&world),
            Staleness::TopologyEpoch { .. }
        ));
        let mut world = stamped_world(Some(7), 3);
        world.generation = Generation(2);
        assert!(matches!(
            stamped_token(7).check(&world),
            Staleness::Generation { .. }
        ));
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
            stamp: None,
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
            stamp: None,
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
            stamp: None,
        };
        assert!(token.is_fresh(&still_absent));

        let now_resident = FixedWorld {
            generation: Generation(1),
            epoch: TopologyEpoch(0),
            bricks: vec![(b, BrickStatus::Resident(Revision(1)))],
            stamp: None,
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
                stamp: None,
            };
            assert!(!token.is_fresh(&world), "{status:?} should be stale");
        }
    }
}
