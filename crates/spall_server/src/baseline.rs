//! Late-join baseline capture and bulk chunking (T17).
//!
//! `docs/protocol.md` "Late join": the server captures "an immutable,
//! dependency-complete regional snapshot at tick T and journal cursor J",
//! continues simulating, and streams "compressed, hashed baseline parts on bulk
//! streams" while retaining "subsequent relevant transactions in a bounded
//! catch-up queue". This module owns the first half: turning the live
//! [`SimWorld`] into a [`spall_protocol::BaselineWorld`] blob and the
//! `BaselineBegin` / `BaselinePart` / `BaselineEnd` records that carry it.
//!
//! It carries **authoritative geometry only** — every resident brick with its
//! real revision, so the replica reaches the exact canonical topology hash with
//! no follow-up repair. Body motion is delivered afterwards as a
//! [`spall_protocol::MotionSnapshot`] keyframe per body (`docs/protocol.md`
//! step 4).

use spall_core::{BrickCoord, CELLS_PER_BRICK, JournalSeq, LocalCell, Revision, Tick};
use spall_protocol::{
    BaselineBegin, BaselineBrick, BaselineCells, BaselineEnd, BaselineOwner, BaselinePart,
    BaselineRegion, BaselineVolume, BaselineWorld, Hash32, InterestEpoch, TransferId, limits,
};
use spall_sim::{Body, Simulation};
use spall_voxel::BrickSnapshot;

/// World/content schema versions stamped into a `BaselineBegin`. These mirror
/// the T10 bridge session's fixed values; real negotiation is a later task.
pub const BASELINE_WORLD_VERSION: u32 = 1;
pub const BASELINE_CONTENT_VERSION: u32 = 1;

/// A complete baseline ready to push to one client: the control-stream
/// `BaselineBegin` / `BaselineEnd` markers and the ordered bulk parts.
#[derive(Debug, Clone)]
pub struct BaselineTransfer {
    pub begin: BaselineBegin,
    pub parts: Vec<BaselinePart>,
    pub end: BaselineEnd,
    /// The decoded payload, retained for server-side assertions / metrics.
    pub world: BaselineWorld,
}

impl BaselineTransfer {
    /// Total bulk payload bytes (excludes the small control markers).
    pub fn payload_bytes(&self) -> usize {
        self.parts.iter().map(|p| p.payload.len()).sum()
    }
}

/// Why a baseline could not be captured for transfer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BaselineError {
    #[error(
        "baseline payload is {bytes} bytes; the assembled-transfer cap is {cap} (a bigger world \
         needs region splitting, T18)"
    )]
    TooLarge { bytes: usize, cap: usize },
    #[error("baseline needs {parts} parts; the ceiling is {cap}")]
    TooManyParts { parts: usize, cap: usize },
}

/// Snapshots `sim`'s live world into an immutable [`BaselineWorld`] coherent
/// with the current tick.
pub fn world_baseline(sim: &Simulation) -> BaselineWorld {
    let world = sim.world();
    let mut volumes = vec![baseline_volume(
        world.terrain(),
        BaselineOwner::Terrain,
    )];
    for body in world.bodies() {
        let entity = body.entity.expect("a detached body carries an entity id");
        volumes.push(baseline_volume(body, BaselineOwner::Body(entity)));
    }
    volumes.sort_by_key(|v| v.volume_id.get());
    BaselineWorld {
        schema: spall_protocol::BASELINE_WORLD_SCHEMA,
        checkpoint_tick: sim.current_tick().get(),
        volumes,
    }
}

/// Captures a baseline and packages it for one client: `transfer_id` and
/// `interest_epoch` identify the transfer, `journal_cursor` is the highest
/// committed journal sequence the snapshot already includes (the replica's
/// catch-up barrier starts strictly after it).
pub fn capture_transfer(
    sim: &Simulation,
    transfer_id: TransferId,
    interest_epoch: InterestEpoch,
    journal_cursor: JournalSeq,
) -> Result<BaselineTransfer, BaselineError> {
    let world = world_baseline(sim);
    let payload = world.encode();

    if payload.len() > limits::MAX_ASSEMBLED_TRANSFER {
        return Err(BaselineError::TooLarge {
            bytes: payload.len(),
            cap: limits::MAX_ASSEMBLED_TRANSFER,
        });
    }
    let parts = chunk_payload(&payload, transfer_id);
    if parts.len() > limits::MAX_BASELINE_PARTS {
        return Err(BaselineError::TooManyParts {
            parts: parts.len(),
            cap: limits::MAX_BASELINE_PARTS,
        });
    }
    // At least one part always: an empty world is not a valid late-join target.
    debug_assert!(!parts.is_empty());

    let assembled_hash = Hash32::of(&payload);
    let regions: Vec<BaselineRegion> = world
        .volumes
        .iter()
        .filter_map(region_of)
        .collect();

    let begin = BaselineBegin {
        transfer_id,
        interest_epoch,
        checkpoint_tick: Tick(world.checkpoint_tick),
        journal_cursor,
        world_version: BASELINE_WORLD_VERSION,
        content_version: BASELINE_CONTENT_VERSION,
        total_bytes: payload.len() as u64,
        part_count: parts.len() as u32,
        regions,
    };
    let end = BaselineEnd {
        transfer_id,
        assembled_hash,
        journal_cursor,
    };
    Ok(BaselineTransfer {
        begin,
        parts,
        end,
        world,
    })
}

/// Splits `payload` into ordered, hashed [`BaselinePart`]s no larger than
/// [`limits::MAX_BULK_PART`]. Reassembly is plain concatenation in `part_index`
/// order, which [`spall_net`]'s bulk reader already enforces.
pub fn chunk_payload(payload: &[u8], transfer_id: TransferId) -> Vec<BaselinePart> {
    let mut parts = Vec::new();
    for (i, chunk) in payload.chunks(limits::MAX_BULK_PART).enumerate() {
        parts.push(BaselinePart {
            transfer_id,
            part_index: i as u32,
            part_hash: Hash32::of(chunk),
            payload: chunk.to_vec(),
        });
    }
    parts
}

/// Reassembles and decodes a received part list (client side helper; also used
/// by the server-side round-trip test).
pub fn assemble(parts: &[BaselinePart]) -> Result<BaselineWorld, spall_protocol::BaselineDecodeError> {
    let mut bytes = Vec::new();
    for part in parts {
        bytes.extend_from_slice(&part.payload);
    }
    BaselineWorld::decode(&bytes)
}

fn baseline_volume(body: &Body, owner: BaselineOwner) -> BaselineVolume {
    let v = &body.volume;
    let mut bricks: Vec<BaselineBrick> = v
        .resident_brick_coords()
        .into_iter()
        .filter_map(|coord| {
            let snap = v.snapshot_brick(coord).ok().flatten()?;
            Some(BaselineBrick {
                coord: [coord.x, coord.y, coord.z],
                revision: snap.revision().get(),
                edited: snap.is_edited(),
                cells: cells_of(&snap),
            })
        })
        .collect();
    bricks.sort_by_key(|b| (b.coord[2], b.coord[1], b.coord[0]));
    BaselineVolume {
        volume_id: v.id(),
        cell_size_code: v.cell_size().to_u8(),
        owner,
        bounds: v
            .bounds()
            .map(|b| [[b.min.x, b.min.y, b.min.z], [b.max.x, b.max.y, b.max.z]]),
        bricks,
    }
}

fn cells_of(snap: &BrickSnapshot) -> BaselineCells {
    let first = snap.get(LocalCell::from_linear_index(0).expect("0 < 32768")).raw();
    let mut uniform = true;
    let mut dense = vec![0u16; CELLS_PER_BRICK];
    for (i, slot) in dense.iter_mut().enumerate() {
        let local = LocalCell::from_linear_index(i as u16).expect("i < CELLS_PER_BRICK");
        let raw = snap.get(local).raw();
        *slot = raw;
        if raw != first {
            uniform = false;
        }
    }
    if uniform {
        BaselineCells::Uniform(first)
    } else {
        BaselineCells::Dense(dense)
    }
}

fn region_of(v: &BaselineVolume) -> Option<BaselineRegion> {
    let first = v.bricks.first()?;
    let mut min = first.coord;
    let mut max = first.coord;
    let mut revision = first.revision;
    for b in &v.bricks {
        for axis in 0..3 {
            min[axis] = min[axis].min(b.coord[axis]);
            max[axis] = max[axis].max(b.coord[axis]);
        }
        revision = revision.max(b.revision);
    }
    Some(BaselineRegion {
        volume: v.volume_id,
        min_brick: BrickCoord::new(min[0], min[1], min[2]),
        max_brick: BrickCoord::new(max[0], max[1], max[2]),
        revision: Revision(revision),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::units::{BRUSH_UNIT, BrushPoint};
    use spall_core::{EntityId, SphereBrush};
    use spall_sim::{EditIntent, EditTarget, SimulationConfig, fixtures};

    fn bridge_after_cut() -> Simulation {
        let mut sim = Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup()))
            .expect("bridge scene is valid");
        let h = BRUSH_UNIT / 2;
        let brush = SphereBrush::new(
            BrushPoint::from_units(10 * BRUSH_UNIT + h, 4 * BRUSH_UNIT + h, BRUSH_UNIT + h),
            2 * BRUSH_UNIT,
        )
        .expect("brush is valid");
        sim.submit(EditIntent::cut(
            spall_protocol::RequestId(1),
            EntityId::new(1).unwrap(),
            EditTarget::Terrain,
            brush,
        ))
        .expect("submit");
        sim.run_until_idle(24).expect("run to idle");
        sim
    }

    #[test]
    fn a_capture_round_trips_through_parts_and_matches_the_live_world() {
        let sim = bridge_after_cut();
        assert!(sim.world().body_count() >= 1, "the cut detached the beam");

        let transfer =
            capture_transfer(&sim, TransferId(1), InterestEpoch(1), JournalSeq(3)).unwrap();

        // Markers are consistent.
        assert_eq!(transfer.begin.part_count as usize, transfer.parts.len());
        assert_eq!(
            transfer.begin.total_bytes as usize,
            transfer.payload_bytes()
        );
        assert_eq!(transfer.begin.journal_cursor, JournalSeq(3));
        assert_eq!(
            transfer.begin.checkpoint_tick.get(),
            sim.current_tick().get()
        );
        // One region per volume (terrain + every body).
        assert_eq!(
            transfer.begin.regions.len(),
            sim.world().body_count() + 1
        );

        // Reassembly reproduces the captured world exactly.
        let rebuilt = assemble(&transfer.parts).unwrap();
        assert_eq!(rebuilt, transfer.world);
        assert_eq!(rebuilt.volumes.len(), sim.world().body_count() + 1);
        assert_eq!(
            Hash32::of(&rebuilt.encode()),
            transfer.end.assembled_hash
        );
    }

    #[test]
    fn every_baseline_brick_encoding_is_well_formed_and_decodes_equal() {
        let sim = bridge_after_cut();
        let world = world_baseline(&sim);
        for v in &world.volumes {
            assert!(!v.bricks.is_empty());
            for b in &v.bricks {
                match &b.cells {
                    BaselineCells::Uniform(_) => {}
                    BaselineCells::Dense(cells) => assert_eq!(cells.len(), CELLS_PER_BRICK),
                }
            }
        }
        let decoded = BaselineWorld::decode(&world.encode()).unwrap();
        assert_eq!(decoded, world);
    }
}
