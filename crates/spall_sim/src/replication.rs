//! Server-authoritative → replica wire conversion (T10).
//!
//! `spall_sim` owns the conversion between authoritative state and
//! [`spall_protocol`] records (`docs/architecture.md`). For replication a
//! committed [`spall_protocol::TopologyTransaction`] must be *self-describing*: a
//! replica that has no `spall_structure` reconstructs a split from the
//! transaction's ops alone (`docs/protocol.md`: "A split uses canonical
//! source-cell ranges or a baseline blob, not a client rerun of floating-point
//! physics or support heuristics").
//!
//! [`crate::commit::commit`] therefore appends, after the `IntegerBrush` op and
//! each `SplitOff`, the explicit [`spall_protocol::TopologyOp::CellRun`]s built
//! here:
//!
//! * [`child_fill_ops`] — every detached cell of one child, with its material,
//!   as canonical `+X` runs (the child volume starts empty on the replica).
//! * [`source_removal_ops`] — every detached cell set to air in the source.
//!
//! This module also builds the per-tick replication feed a host pushes to
//! clients: the committed transactions, the [`spall_protocol::ActionStatus`]
//! answers, the 20 Hz [`spall_protocol::MotionSnapshot`]s, and a `CellRun` reply
//! to a [`spall_protocol::RepairRequest`].

use glam::DQuat;
use spall_core::{
    BrickCoord, CELLS_PER_BRICK, CellSizeCode, GlobalCell, LocalCell, MaterialId, Revision, Tick,
    VolumeId,
};
use spall_protocol::baseline::{BaselineBrick, BaselineCells, BaselineOwner, BaselineVolume};
use spall_protocol::{
    ActionOutcome, ActionStatus, MotionSnapshot, RepairKey, RepairRequest, RequestId, SnapshotSeq,
    TopologyOp, TopologyTransaction,
};
use spall_structure::{CellSpanX, ComponentMembership};
use spall_voxel::{Brick, BrickBounds, Sample, Volume};

use crate::body::BodyPose;
use crate::schedule::TickReport;
use crate::world::SimWorld;

/// Largest cell count in one encoded [`TopologyOp::CellRun`].
const MAX_RUN_LEN: i64 = spall_protocol::limits::MAX_CELL_RUN_LEN as i64;

/// Why the authoritative → wire conversion could not produce a self-describing
/// transaction.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReplicationError {
    /// The inline `CellRun` encoding overflowed the wire, and the compressed
    /// baseline-blob alternative (T17 increment 1) is *also* over
    /// [`spall_protocol::limits::MAX_SPLIT_BASELINE_BLOB`] — a giant collapse
    /// that needs the bulk-stream baseline path (T17 increment 2).
    #[error(
        "split geometry is too large to replicate: {blob_bytes} compressed baseline bytes for \
         volume {volume}, cap is {cap} (giant split needs the bulk-stream baseline path)"
    )]
    SplitTooLarge {
        volume: u64,
        blob_bytes: usize,
        cap: usize,
    },
    /// A split baseline op blob could not be decoded on the apply/replay side.
    #[error("split baseline op blob for volume {volume} is invalid: {reason}")]
    BadBaselineBlob { volume: u64, reason: String },
}

/// Whether a fully-built [`TopologyTransaction`] fits one reliable control
/// record when encoded — the real replication budget (op *count* alone misses
/// that ~1300 `CellRun`s already blow the 64 KiB frame).
pub fn inline_wire_fits(topology: &TopologyTransaction) -> bool {
    spall_protocol::encode_control(topology).is_ok()
}

/// One brick's cells as [`BaselineCells`] — `Uniform` when every cell matches,
/// else a `Dense` 32768-entry layer. Mirrors `spall_server::baseline::cells_of`.
fn baseline_cells(snap: &spall_voxel::BrickSnapshot) -> BaselineCells {
    let first = snap
        .get(LocalCell::from_linear_index(0).expect("0 < 32768"))
        .raw();
    let mut dense = vec![0u16; CELLS_PER_BRICK];
    let mut uniform = true;
    for (i, slot) in dense.iter_mut().enumerate() {
        let raw = snap
            .get(LocalCell::from_linear_index(i as u16).expect("i < CELLS_PER_BRICK"))
            .raw();
        *slot = raw;
        uniform &= raw == first;
    }
    if uniform {
        BaselineCells::Uniform(first)
    } else {
        BaselineCells::Dense(dense)
    }
}

/// A [`BaselineVolume`] over `volume`'s resident bricks (all of them when
/// `only` is `None`, else just those coords), each at its authoritative
/// revision + edited flag + material layer. This is the same payload T17
/// late-join uses; a `SplitOffBaseline` op carries the whole child volume and a
/// `SourcePatchBaseline` op carries the source's post-cut affected bricks.
pub fn baseline_volume_of(
    volume: &Volume,
    owner: BaselineOwner,
    only: Option<&[BrickCoord]>,
) -> BaselineVolume {
    let coords: Vec<BrickCoord> = match only {
        Some(list) => list.to_vec(),
        None => volume.resident_brick_coords(),
    };
    let mut bricks: Vec<BaselineBrick> = coords
        .into_iter()
        .filter_map(|coord| {
            let snap = volume.snapshot_brick(coord).ok().flatten()?;
            Some(BaselineBrick {
                coord: [coord.x, coord.y, coord.z],
                revision: snap.revision().get(),
                edited: snap.is_edited(),
                cells: baseline_cells(&snap),
            })
        })
        .collect();
    bricks.sort_by_key(|b| (b.coord[2], b.coord[1], b.coord[0]));
    BaselineVolume {
        volume_id: volume.id(),
        cell_size_code: volume.cell_size().to_u8(),
        owner,
        bounds: volume
            .bounds()
            .map(|b| [[b.min.x, b.min.y, b.min.z], [b.max.x, b.max.y, b.max.z]]),
        bricks,
    }
}

/// Rebuild a `spall_voxel::Volume` from a decoded [`BaselineVolume`] — bounded to
/// the recorded brick bounds, every brick restored at its authoritative
/// revision. Mirrors `spall_client::replica::install_baseline_world`'s per-volume
/// loop so the canonical hash matches.
pub fn volume_from_baseline(bv: &BaselineVolume) -> Result<Volume, ReplicationError> {
    let bad = |reason: String| ReplicationError::BadBaselineBlob {
        volume: bv.volume_id.get(),
        reason,
    };
    let cs = CellSizeCode::from_u8(bv.cell_size_code)
        .ok_or_else(|| bad(format!("unknown cell-size code {}", bv.cell_size_code)))?;
    let mut volume = match bv.bounds {
        Some([mn, mx]) => {
            let bounds = BrickBounds::new(
                BrickCoord::new(mn[0], mn[1], mn[2]),
                BrickCoord::new(mx[0], mx[1], mx[2]),
            )
            .ok_or_else(|| bad("baseline bounds min > max".into()))?;
            Volume::bounded(bv.volume_id, cs, bounds)
        }
        None => Volume::new(bv.volume_id, cs),
    };
    for bb in &bv.bricks {
        let cells: Vec<MaterialId> = match &bb.cells {
            BaselineCells::Uniform(id) => vec![MaterialId(*id); CELLS_PER_BRICK],
            BaselineCells::Dense(raw) => {
                if raw.len() != CELLS_PER_BRICK {
                    return Err(bad(format!("dense brick has {} cells", raw.len())));
                }
                raw.iter().copied().map(MaterialId).collect()
            }
        };
        volume
            .insert_brick(
                BrickCoord::new(bb.coord[0], bb.coord[1], bb.coord[2]),
                Brick::restored(&cells, Revision(bb.revision), bb.edited),
            )
            .map_err(|e| bad(format!("insert brick {:?}: {e}", bb.coord)))?;
    }
    Ok(volume)
}

/// [`BaselineOwner`] twin of a [`spall_protocol::CanonicalOwner`].
pub fn baseline_owner(owner: spall_protocol::CanonicalOwner) -> BaselineOwner {
    match owner {
        spall_protocol::CanonicalOwner::Terrain => BaselineOwner::Terrain,
        spall_protocol::CanonicalOwner::Body(e) => BaselineOwner::Body(e),
    }
}

/// Canonical `+X` [`TopologyOp::CellRun`]s that fill `child` with exactly the
/// member cells of `membership`, taking each cell's material from
/// `source_after_cut` (which must still hold those cells — call before the
/// source-removal step, exactly like [`crate::transfer::build_child_volume`]).
///
/// Runs are split at every material change and capped at
/// [`spall_protocol::limits::MAX_CELL_RUN_LEN`], so the result is deterministic
/// and order-stable for a given cell set.
pub fn child_fill_ops(
    child: VolumeId,
    membership: &ComponentMembership,
    source_after_cut: &Volume,
) -> Vec<TopologyOp> {
    runs_from_spans(child, &membership.spans, |cell| {
        match source_after_cut.sample(cell) {
            Ok(Sample::Filled(m)) => m,
            other => panic!("split member cell {cell:?} is not solid in the source: {other:?}"),
        }
    })
}

/// Canonical `+X` [`TopologyOp::CellRun`]s that set every detached cell of every
/// `membership` back to air in `source`. The memberships are disjoint
/// components, so their spans never overlap.
pub fn source_removal_ops(
    source: VolumeId,
    memberships: &[ComponentMembership],
) -> Vec<TopologyOp> {
    let mut ops = Vec::new();
    for membership in memberships {
        ops.extend(runs_from_spans(source, &membership.spans, |_| {
            MaterialId::AIR
        }));
    }
    ops
}

/// Turns canonical X-spans into [`TopologyOp::CellRun`]s, coalescing adjacent
/// cells that share a material and splitting a run at
/// [`spall_protocol::limits::MAX_CELL_RUN_LEN`].
fn runs_from_spans(
    volume: VolumeId,
    spans: &[CellSpanX],
    material_at: impl Fn(GlobalCell) -> MaterialId,
) -> Vec<TopologyOp> {
    let mut ops = Vec::new();
    for span in spans {
        let mut x = span.x0;
        while x <= span.x1 {
            let material = material_at(GlobalCell::new(x, span.y, span.z));
            let mut end = x;
            while end < span.x1
                && (end - x + 1) < MAX_RUN_LEN
                && material_at(GlobalCell::new(end + 1, span.y, span.z)) == material
            {
                end += 1;
            }
            ops.push(TopologyOp::CellRun {
                volume,
                start: GlobalCell::new(x, span.y, span.z),
                len: (end - x + 1) as u32,
                material,
            });
            x = end + 1;
        }
    }
    ops
}

/// The committed transactions from one tick, in server-assigned commit order.
pub fn committed_transactions(
    report: &TickReport,
) -> impl Iterator<Item = (RequestId, &TopologyTransaction)> {
    report
        .committed
        .iter()
        .map(|(request, committed)| (*request, &committed.topology))
}

/// The [`ActionStatus`] answer for every request the tick resolved: `Committed`
/// for a commit, `Rejected` for a deterministic staging failure. `Queued` is the
/// caller's business (it is sent when the intent is accepted, before staging).
pub fn action_statuses(report: &TickReport) -> Vec<ActionStatus> {
    let mut out = Vec::with_capacity(report.committed.len() + report.rejected.len());
    for (request, committed) in &report.committed {
        out.push(ActionStatus {
            request_id: *request,
            outcome: ActionOutcome::Committed {
                transaction: committed.transaction,
            },
        });
    }
    for (request, reason) in &report.rejected {
        out.push(ActionStatus {
            request_id: *request,
            outcome: ActionOutcome::Rejected {
                reason: reason.clone(),
            },
        });
    }
    out
}

/// Emits [`MotionSnapshot`]s for every dynamic body at the fixed motion rate.
///
/// `docs/architecture.md`: "Server simulation is fixed at 60 Hz; publish motion
/// snapshots at 20 Hz initially." Terrain never moves and has no entity id, so
/// it is not published.
#[derive(Debug, Clone)]
pub struct MotionPublisher {
    /// Server ticks between published batches (`60 / 20 = 3`).
    interval_ticks: u64,
    next_seq: u64,
}

impl MotionPublisher {
    /// `server_tick_hz` and `snapshot_hz` come from the negotiated
    /// [`spall_protocol::Handshake`]; both must be non-zero.
    pub fn new(server_tick_hz: u16, snapshot_hz: u16) -> Self {
        let hz = u64::from(server_tick_hz.max(1));
        let snap = u64::from(snapshot_hz.max(1));
        Self {
            interval_ticks: hz.div_ceil(snap).max(1),
            next_seq: 0,
        }
    }

    /// Whether `tick` is a motion-publish tick.
    pub fn due(&self, tick: Tick) -> bool {
        tick.get().is_multiple_of(self.interval_ticks)
    }

    /// One [`MotionSnapshot`] per dynamic body **and per player capsule** (T19)
    /// at `tick`. Each carries a strictly increasing per-publisher
    /// `snapshot_seq` so a replica can order and dedup them. A player snapshot
    /// sets `body` to the player's reserved-band entity id and `acked_input` to
    /// the last input sequence the server integrated for that player — the
    /// client uses it to drop acknowledged inputs and replay the rest.
    pub fn snapshots(&mut self, world: &SimWorld, tick: Tick) -> Vec<MotionSnapshot> {
        let mut out = Vec::with_capacity(world.body_count() + world.player_count());
        for body in world.bodies() {
            let Some(entity) = body.entity else { continue };
            let seq = self.next_seq;
            self.next_seq += 1;
            out.push(MotionSnapshot {
                server_tick: tick,
                snapshot_seq: SnapshotSeq(seq),
                acked_input: spall_protocol::InputSeq(0),
                body: entity,
                topology_revision: latest_revision(world, body.volume_id),
                pose: body.pose.to_protocol(),
                linear_velocity: body.linvel_m_s.map(|v| v as f32),
                angular_velocity: body.angvel_rad_s.map(|v| v as f32),
                sleeping: body.sleeping,
            });
        }

        let terrain_rev = latest_revision(world, world.terrain_volume_id());
        for player in world.players() {
            let seq = self.next_seq;
            self.next_seq += 1;
            let pose = BodyPose::new(DQuat::IDENTITY, player.state.position_m);
            out.push(MotionSnapshot {
                server_tick: tick,
                snapshot_seq: SnapshotSeq(seq),
                acked_input: player.last_input_seq,
                body: player.entity,
                topology_revision: terrain_rev,
                pose: pose.to_protocol(),
                linear_velocity: player.state.velocity_m_s,
                angular_velocity: [0.0; 3],
                sleeping: false,
            });
        }
        out
    }
}

/// Authoritative reply to a [`RepairRequest`]: the current cells of the named
/// brick as canonical `+X` [`TopologyOp::CellRun`]s (air cells included, so the
/// replica can overwrite a diverged brick exactly). `None` for a body repair or
/// an unknown brick — whole-body / baseline repair is T17.
pub fn repair_ops(world: &SimWorld, request: &RepairRequest) -> Option<Vec<TopologyOp>> {
    let RepairKey::Brick { volume, coord } = request.key else {
        return None;
    };
    let vol = world.volume_ref(volume)?;
    // Bail unless the brick is addressable in this volume.
    vol.brick_revision(coord).ok()?;

    let mut spans: Vec<CellSpanX> = Vec::new();
    for index in 0..CELLS_PER_BRICK as u16 {
        let local = LocalCell::from_linear_index(index).expect("index < 32768");
        let Ok(cell) = GlobalCell::from_parts(coord, local) else {
            continue;
        };
        match spans.last_mut() {
            Some(span) if span.z == cell.z && span.y == cell.y && span.x1 + 1 == cell.x => {
                span.x1 = cell.x;
            }
            _ => spans.push(CellSpanX {
                z: cell.z,
                y: cell.y,
                x0: cell.x,
                x1: cell.x,
            }),
        }
    }
    spans.sort_unstable();
    Some(runs_from_spans(volume, &spans, |cell| {
        match vol.sample(cell) {
            Ok(Sample::Filled(m)) => m,
            _ => MaterialId::AIR,
        }
    }))
}

/// Latest allocated revision of `volume` (`Revision::ZERO` if it has none yet).
pub(crate) fn latest_revision(world: &SimWorld, volume: VolumeId) -> Revision {
    world
        .volume_ref(volume)
        .map(|v| Revision(v.next_revision().get().saturating_sub(1)))
        .unwrap_or(Revision::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::CellSizeCode;
    use spall_structure::CellSpanX;
    use spall_voxel::{EditPlan, Volume};

    fn one_material_source() -> (VolumeId, Volume) {
        let id = VolumeId::new(1).unwrap();
        let mut v = Volume::new(id, CellSizeCode::Quarter);
        v.apply_edit(&EditPlan::filled_box(
            id,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(9, 0, 0),
            MaterialId(1),
        ))
        .unwrap();
        (id, v)
    }

    #[test]
    fn a_contiguous_single_material_span_becomes_one_run() {
        let (id, source) = one_material_source();
        let membership = ComponentMembership {
            id: spall_structure::GlobalComponentId(0),
            cell_count: 10,
            spans: vec![CellSpanX {
                z: 0,
                y: 0,
                x0: 0,
                x1: 9,
            }],
        };
        let child = VolumeId::new(2).unwrap();
        let ops = child_fill_ops(child, &membership, &source);
        assert_eq!(ops.len(), 1);
        assert_eq!(
            ops[0],
            TopologyOp::CellRun {
                volume: child,
                start: GlobalCell::new(0, 0, 0),
                len: 10,
                material: MaterialId(1),
            }
        );

        let removal = source_removal_ops(id, std::slice::from_ref(&membership));
        assert_eq!(
            removal,
            vec![TopologyOp::CellRun {
                volume: id,
                start: GlobalCell::new(0, 0, 0),
                len: 10,
                material: MaterialId::AIR,
            }]
        );
    }

    #[test]
    fn a_run_splits_at_a_material_boundary() {
        let id = VolumeId::new(1).unwrap();
        let mut v = Volume::new(id, CellSizeCode::Quarter);
        v.apply_edit(&EditPlan::filled_box(
            id,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(2, 0, 0),
            MaterialId(1),
        ))
        .unwrap();
        v.apply_edit(&EditPlan::filled_box(
            id,
            GlobalCell::new(3, 0, 0),
            GlobalCell::new(4, 0, 0),
            MaterialId(2),
        ))
        .unwrap();

        let membership = ComponentMembership {
            id: spall_structure::GlobalComponentId(0),
            cell_count: 5,
            spans: vec![CellSpanX {
                z: 0,
                y: 0,
                x0: 0,
                x1: 4,
            }],
        };
        let child = VolumeId::new(2).unwrap();
        let ops = child_fill_ops(child, &membership, &v);
        assert_eq!(ops.len(), 2);
        assert_eq!(
            ops[0],
            TopologyOp::CellRun {
                volume: child,
                start: GlobalCell::new(0, 0, 0),
                len: 3,
                material: MaterialId(1),
            }
        );
        assert_eq!(
            ops[1],
            TopologyOp::CellRun {
                volume: child,
                start: GlobalCell::new(3, 0, 0),
                len: 2,
                material: MaterialId(2),
            }
        );
    }
}
