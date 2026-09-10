//! The atomic tick-boundary commit.
//!
//! [`commit`] takes one [`StagedEdit`], re-validates its [`spall_jobs::JobToken`]
//! against the *live* world, and — only if nothing it read has moved — applies
//! the whole transaction in one shot.
//!
//! # Isolation (`ENG-54`)
//!
//! The commit is built as a self-contained *candidate* that touches no live
//! state:
//!
//! 1. a clone of the id registry reserves every child entity/volume id, the
//!    transaction id and the journal sequence;
//! 2. the deterministic brush plan and the cell-removal plan are applied to a
//!    *clone* of the target volume;
//! 3. every detached component is planned into a child body from that clone,
//!    with inherited mass and velocity;
//! 4. the parent collider rebuild is planned, the [`TopologyTransaction`] DTO is
//!    assembled and validated, and the journal participant snapshots are built.
//!
//! Only once every fallible step above has succeeded does the *publish* phase
//! run — swapping in the reserved registry, the edited volume, the rebuilt
//! parent collider, and the installed child bodies, then appending the journal
//! entry. Publish contains no fallible operation, so a late failure (id
//! exhaustion, DTO validation, op-budget) leaves the world hash, cell counts,
//! colliders, bodies, registry counters and journal exactly as they were and
//! the request is rejected deterministically by the pipeline.
//!
//! A stale token returns [`CommitOutcome::Stale`] and changes nothing; the
//! caller recomputes the intent and retries it in request order.

use spall_core::{BrickCoord, MaterialId, Revision, Tick, VolumeId};
use spall_jobs::Staleness;
use spall_physics::{BodyKind as PhysBodyKind, BodySpec, OccupancyGrid, analytic_mass_properties};
use spall_protocol::{
    ActionOutcome, ActionStatus, BrickRevision, CanonicalOwner, ControlSeq, InputSeq,
    MotionSnapshot, Record, RecordError, SnapshotSeq, TopologyOp, TopologyTransaction, TransferId,
    VolumeHash,
};
use spall_voxel::{EditError, EditPlan, Volume};

use crate::body::{Body, BodyKind, BodyPose};
use crate::collider::plan_collider;
use crate::journal::{JournalEntry, JournalSink};
use crate::stage::StagedEdit;
use crate::transfer::{self, ChildBody, ParentState};
use crate::world::{SimWorld, volume_topology_hash_for};

/// Structural algorithm version stamped into every transaction.
pub const ALGORITHM_VERSION: u32 = 1;

/// A committed transaction and everything it produced.
#[derive(Debug, Clone)]
pub struct Committed {
    pub transaction: spall_core::TransactionId,
    pub journal_seq: spall_core::JournalSeq,
    pub topology: TopologyTransaction,
    /// Entities of every body this commit created.
    pub children: Vec<spall_core::EntityId>,
    /// Whether the commit advanced the topology epoch (it did iff it split).
    pub bumped_epoch: bool,
    /// T17 increment 2: a giant split whose geometry did not fit even a
    /// compressed inline op blob. `topology.ops` are `SplitOffBulkBaseline` /
    /// `SourcePatchBulkBaseline` markers only; this is the out-of-band
    /// `BaselineWorld` (every child volume + the source's post-cut affected
    /// bricks) the host must deliver on a bulk stream and journal alongside the
    /// transaction. `None` for every ordinary commit.
    pub bulk_baseline: Option<spall_protocol::baseline::BaselineWorld>,
}

impl Committed {
    /// The [`ActionStatus`] to return for the request that produced this commit,
    /// and for any later duplicate of the same request id.
    pub fn action_status(&self, request_id: spall_protocol::RequestId) -> ActionStatus {
        ActionStatus {
            request_id,
            outcome: ActionOutcome::Committed {
                transaction: self.transaction,
            },
        }
    }
}

/// The result of a commit attempt.
#[derive(Debug, Clone)]
pub enum CommitOutcome {
    /// Applied. The world moved.
    Committed(Committed),
    /// The staged token no longer matches the live world; nothing changed.
    Stale(Staleness),
}

/// Why a commit could not proceed even though its token was fresh.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommitError {
    #[error("commit targets volume {0} which does not exist")]
    UnknownVolume(VolumeId),
    #[error("applying the plan to live state failed: {0}")]
    Edit(#[from] EditError),
    #[error("occupancy extraction failed: {0}")]
    Occupancy(#[from] spall_physics::ExtractError),
    #[error("no exact active collider for the body: {0}")]
    Collider(#[from] crate::collider::ColliderInfeasible),
    #[error("id space exhausted: {0}")]
    Ids(#[from] spall_core::IdError),
    #[error("emitted transaction DTO is invalid: {0}")]
    Record(#[from] RecordError),
    #[error("commit cannot be encoded for replication: {0}")]
    Replication(#[from] crate::replication::ReplicationError),
}

impl From<crate::transfer::PlanChildError> for CommitError {
    fn from(e: crate::transfer::PlanChildError) -> Self {
        match e {
            crate::transfer::PlanChildError::Occupancy(x) => CommitError::Occupancy(x),
            crate::transfer::PlanChildError::Collider(x) => CommitError::Collider(x),
        }
    }
}

/// Commits `staged` into `world`, appending a journal entry on success.
pub fn commit(
    world: &mut SimWorld,
    journal: &mut JournalSink,
    staged: &StagedEdit,
    server_tick: Tick,
    control_seq: ControlSeq,
) -> Result<CommitOutcome, CommitError> {
    // 1. Re-validate against the live world.
    match staged.token.check(world) {
        Staleness::Fresh => {}
        other => return Ok(CommitOutcome::Stale(other)),
    }

    let vid = staged.volume_id;
    let parent = world
        .volume_body(vid)
        .ok_or(CommitError::UnknownVolume(vid))?;
    let cell_size = parent.volume.cell_size();
    let parent_phys = parent.phys;
    let parent_is_terrain = parent.kind == BodyKind::Terrain;
    let parent_entity = parent.entity;

    // 2. Parent kinematic state at the split instant (world space).
    let parent_state = if parent_is_terrain {
        ParentState {
            pose: BodyPose::identity(),
            com_world_m: [0.0; 3],
            linvel_m_s: [0.0; 3],
            angvel_rad_s: [0.0; 3],
            cell_size,
        }
    } else {
        let st = world.physics().body_state(parent_phys);
        let (_, grid_local_com, _) = world.physics().derived_mass_properties(parent_phys);
        // `derived_mass_properties` is in the collider shape's grid-local frame;
        // shift it by the body-local grid-origin offset to get the body-frame
        // centre of mass the world-space COM is built from (`ENG-55`).
        let com_offset = world.physics().collider_offset_m(parent_phys);
        let local_com = [
            grid_local_com[0] + com_offset[0],
            grid_local_com[1] + com_offset[1],
            grid_local_com[2] + com_offset[2],
        ];
        let pose = BodyPose::new(
            glam::DQuat::from_xyzw(
                f64::from(st.rotation[0]),
                f64::from(st.rotation[1]),
                f64::from(st.rotation[2]),
                f64::from(st.rotation[3]),
            ),
            [
                f64::from(st.translation_m[0]),
                f64::from(st.translation_m[1]),
                f64::from(st.translation_m[2]),
            ],
        );
        ParentState {
            pose,
            com_world_m: transfer::com_world(pose, local_com),
            linvel_m_s: st.linvel_m_s.map(f64::from),
            angvel_rad_s: st.angvel_rad_s.map(f64::from),
            cell_size,
        }
    };

    // ---- Build an isolated commit candidate. Nothing from here to the publish
    // ---- marker mutates the live world, its physics, the id registry, or the
    // ---- journal; every fallible step runs against clones so a late failure
    // ---- leaves authoritative state exactly as it was (`ENG-54`).
    let mut reg = world.registry().clone();

    // 3. Reserve child identities up front from the candidate registry.
    let mut child_ids: Vec<(spall_core::EntityId, VolumeId)> = Vec::new();
    for _ in &staged.memberships {
        let e = reg.allocate_entity()?;
        let v = reg.allocate_volume()?;
        child_ids.push((e, v));
    }

    // 4. Apply the brush to a clone of the target volume.
    let mut parent_candidate: Volume = world
        .volume_ref(vid)
        .ok_or(CommitError::UnknownVolume(vid))?
        .clone();
    let cut_outcome = parent_candidate.apply_edit(&staged.plan)?;

    // 5. Build every child from the post-cut candidate (components are still
    //    solid), and capture the canonical cell-run ops a replica needs to
    //    reconstruct the child without any structural code (`docs/protocol.md`).
    let mut children: Vec<ChildBody> = Vec::new();
    let mut child_fill_ops: Vec<Vec<spall_protocol::TopologyOp>> = Vec::new();
    for ((entity, child_vid), membership) in child_ids.iter().zip(&staged.memberships) {
        let child = transfer::plan_child(
            &parent_candidate,
            membership,
            parent_state,
            *entity,
            *child_vid,
            &|m: MaterialId| world.density(m),
            0,
        )?;
        child_fill_ops.push(crate::replication::child_fill_ops(
            *child_vid,
            membership,
            &parent_candidate,
        ));
        children.push(child);
    }
    transfer::apply_explosion(&mut children, staged.explosion);

    // 6. Remove the detached cells from the candidate.
    let remove_outcome = if staged.splits() {
        let mut remove = EditPlan::new(vid);
        for membership in &staged.memberships {
            for cell in membership.cells() {
                remove.set(cell, MaterialId::AIR);
            }
        }
        Some(parent_candidate.apply_edit(&remove)?)
    } else {
        None
    };

    // 7. Plan the parent collider rebuild from the candidate's final geometry,
    //    plus the mass / COM / inertia to reinstall from its post-cut fine
    //    material grid (a dynamic parent lost mass to the cut / to its children;
    //    a terrain parent has no solver mass) (`ENG-41`). A fragmented parent
    //    with no exact active collider fails the commit here, before publish
    //    (`ENG-42`).
    let parent_rebuild = match OccupancyGrid::from_volume(&parent_candidate)? {
        Some(grid) => {
            let plan = plan_collider(&grid)?;
            let mass_properties = (!parent_is_terrain).then(|| {
                analytic_mass_properties(&grid, cell_size.metres(), |m| world.density(m))
                    .to_body_properties()
            });
            Some((plan, mass_properties))
        }
        None => None,
    };
    // The cut cleared the parent's last solid cell: its ownership is retired on
    // publish (`ENG-56`). A retired body emits no participant snapshot.
    let parent_emptied = !parent_is_terrain && parent_rebuild.is_none();

    // 8. Reserve the transaction id and journal sequence.
    let transaction_id = reg.allocate_transaction()?;
    let journal_seq = reg.allocate_journal_seq()?;

    // 9. Assemble and validate the transaction DTO from the candidate state.
    let mut affected: Vec<BrickCoord> = cut_outcome.bricks.iter().map(|b| b.coord).collect();
    if let Some(remove) = &remove_outcome {
        affected.extend(remove.bricks.iter().map(|b| b.coord));
    }
    affected.sort_by_key(|c| c.sort_key());
    affected.dedup();

    let before: Vec<BrickRevision> = cut_outcome
        .bricks
        .iter()
        .map(|b| BrickRevision {
            volume: vid,
            coord: b.coord,
            revision: b.before_revision,
        })
        .collect();
    let after: Vec<BrickRevision> = affected
        .iter()
        .map(|&coord| BrickRevision {
            volume: vid,
            coord,
            revision: parent_candidate
                .brick_revision(coord)
                .ok()
                .flatten()
                .unwrap_or(Revision::ZERO),
        })
        .collect();

    let parent_owner = match parent_entity {
        Some(entity) => CanonicalOwner::Body(entity),
        None => CanonicalOwner::Terrain,
    };
    let mut result_hashes = vec![VolumeHash {
        volume: vid,
        hash: volume_topology_hash_for(&parent_candidate, parent_owner),
    }];
    for child in &children {
        result_hashes.push(VolumeHash {
            volume: child.volume_id,
            hash: volume_topology_hash_for(&child.volume, CanonicalOwner::Body(child.entity)),
        });
    }

    // Self-describing op list: the brush, then for each child a `SplitOff`
    // marker followed by the canonical cell runs that fill it, then the runs
    // that remove every detached cell from the source. A replica applies these
    // in order to reproduce the exact committed geometry.
    let brush_op = TopologyOp::IntegerBrush {
        volume: vid,
        brush: staged.brush,
        material: staged.kind.write_material(),
    };
    let mut ops = vec![brush_op.clone()];
    for (((entity, child_vid), _), fill) in child_ids
        .iter()
        .zip(&staged.memberships)
        .zip(&child_fill_ops)
    {
        ops.push(TopologyOp::SplitOff {
            source: vid,
            child: *child_vid,
            child_entity: *entity,
        });
        ops.extend(fill.iter().cloned());
    }
    if staged.splits() {
        ops.extend(crate::replication::source_removal_ops(
            vid,
            &staged.memberships,
        ));
    }

    let mut topology = TopologyTransaction {
        transaction_id,
        server_tick,
        control_seq,
        algorithm_version: ALGORITHM_VERSION,
        dependencies: Vec::new(),
        before,
        after,
        ops,
        result_hashes,
    };

    // T17: an oversized split whose inline `CellRun` list overflows one reliable
    // control record is re-encoded so the child geometry and the source's
    // post-cut affected bricks travel as baseline blobs — compressed *inside*
    // `tx.ops` when each fits `MAX_SPLIT_BASELINE_BLOB` (increment 1), otherwise
    // as marker ops plus one out-of-band `BaselineWorld` on a bulk stream
    // (increment 2). `before` / `after` / `result_hashes` are unchanged and
    // remain the replica's acceptance check.
    let mut bulk_baseline = None;
    if !crate::replication::inline_wire_fits(&topology) {
        match build_split_baseline_ops(
            vid,
            brush_op,
            parent_owner,
            &parent_candidate,
            &affected,
            &child_ids,
            &children,
            staged.splits(),
            transaction_id,
            server_tick,
        )? {
            SplitEncoding::Inline(ops) => topology.ops = ops,
            SplitEncoding::Bulk { ops, baseline } => {
                topology.ops = ops;
                bulk_baseline = Some(baseline);
            }
        }
    }
    topology.validate()?;

    let bumped_epoch = staged.splits();
    let participants = candidate_participant_snapshots(
        world,
        parent_is_terrain,
        parent_emptied,
        parent_entity,
        &parent_candidate,
        &children,
        server_tick,
        journal_seq,
    );

    // ---- Publish. Every step below is infallible: the candidate is committed
    // ---- to the live world in one shot at the tick boundary.
    *world.registry_mut() = reg;

    if let Some(parent) = world.volume_body_mut(vid) {
        parent.volume = parent_candidate;
    }

    match parent_rebuild {
        Some((plan, mass_properties)) => {
            world
                .physics_mut()
                .rebuild_collider(parent_phys, &plan.grid, plan.representation);
            if let Some(mass_properties) = mass_properties {
                world
                    .physics_mut()
                    .set_mass_properties(parent_phys, mass_properties);
            }
            if let Some(parent) = world.volume_body_mut(vid) {
                parent.collider_revision += 1;
                parent.coarsen_k = plan.coarsen_k;
            }
        }
        None => {
            // The cut cleared the parent's last solid cell. Retire its
            // ownership atomically with the edit (`ENG-56`): a detached body
            // and its physics handle are removed; terrain loses its collider.
            // Nothing keeps colliding with the obsolete solid shape, and the
            // transaction's cell-removal ops already carry the emptying for
            // replicas and for journal replay.
            world.retire_empty_volume(vid);
        }
    }

    // 8. Install every child body and its collider. Mass / COM / inertia come
    //    from the child's fine material grid (`child.mass_properties`), installed
    //    into the body independently of the collision shape, so coarse collider
    //    inflation cannot change the physical mass.
    let mut child_entities = Vec::new();
    for child in children {
        let child_cell_m = child.volume.cell_size().metres() as f32;
        let rot = child.pose.rotation;
        let rot_xyzw = [rot.x as f32, rot.y as f32, rot.z as f32, rot.w as f32];
        let trans = child.pose.translation_m.map(|v| v as f32);
        let linvel = child.linvel_m_s.map(|v| v as f32);
        let angvel = child.angvel_rad_s.map(|v| v as f32);

        let phys = world.physics_mut().add_body(BodySpec {
            kind: PhysBodyKind::Dynamic { ccd: false },
            representation: child.collider_plan.representation,
            grid: child.collider_plan.grid.clone(),
            cell_m: child_cell_m,
            density_kg_m3: 1.0,
            mass_properties: Some(child.mass_properties),
            translation_m: trans,
            linvel_m_s: linvel,
        });
        world.physics_mut().set_body_pose(phys, trans, rot_xyzw);
        world.physics_mut().set_body_velocity(phys, linvel, angvel);

        let body = Body {
            entity: Some(child.entity),
            volume_id: child.volume_id,
            volume: child.volume,
            kind: BodyKind::Dynamic,
            pose: child.pose,
            linvel_m_s: child.linvel_m_s,
            angvel_rad_s: child.angvel_rad_s,
            sleeping: false,
            collider_revision: 1,
            coarsen_k: child.collider_plan.coarsen_k,
            phys,
            collider_region: child.collider_region,
        };
        world.insert_body(body);
        child_entities.push(child.entity);
    }

    if bumped_epoch {
        world.bump_topology_epoch();
    }

    journal.append(JournalEntry {
        seq: journal_seq,
        transaction: topology.clone(),
        participants,
        bulk_baseline: bulk_baseline.clone(),
    });

    Ok(CommitOutcome::Committed(Committed {
        transaction: transaction_id,
        journal_seq,
        topology,
        children: child_entities,
        bumped_epoch,
        bulk_baseline,
    }))
}

/// How an oversized split's geometry is carried once the inline `CellRun` list
/// no longer fits one reliable control record.
enum SplitEncoding {
    /// `[IntegerBrush, SplitOffBaseline*, SourcePatchBaseline]` — each geometry
    /// blob compressed inside `tx.ops` (T17 increment 1).
    Inline(Vec<TopologyOp>),
    /// `[IntegerBrush, SplitOffBulkBaseline*, SourcePatchBulkBaseline]` markers
    /// plus one out-of-band [`BaselineWorld`] (T17 increment 2), for a split
    /// whose compressed geometry exceeds
    /// [`spall_protocol::limits::MAX_SPLIT_BASELINE_BLOB`].
    Bulk {
        ops: Vec<TopologyOp>,
        baseline: spall_protocol::baseline::BaselineWorld,
    },
}

/// T17: re-encode an oversized split. Builds a
/// [`spall_protocol::baseline::BaselineVolume`] for every detached child and for
/// the source's post-cut affected bricks; if every compressed blob fits
/// `MAX_SPLIT_BASELINE_BLOB` it emits the inline
/// `SplitOffBaseline` / `SourcePatchBaseline` form, otherwise the bulk-marker
/// form plus the assembled [`BaselineWorld`]. Fails only if the assembled world
/// exceeds the bulk-transfer ceilings.
#[allow(clippy::too_many_arguments)]
fn build_split_baseline_ops(
    source: VolumeId,
    brush_op: TopologyOp,
    parent_owner: CanonicalOwner,
    parent_candidate: &Volume,
    affected: &[BrickCoord],
    child_ids: &[(spall_core::EntityId, VolumeId)],
    children: &[ChildBody],
    splits: bool,
    transaction_id: spall_core::TransactionId,
    server_tick: Tick,
) -> Result<SplitEncoding, CommitError> {
    use spall_protocol::baseline::{BaselineOwner, BaselineWorld};
    use spall_protocol::limits::{MAX_ASSEMBLED_TRANSFER, MAX_BULK_PART, MAX_SPLIT_BASELINE_BLOB};

    // Every volume the split produces, plus its compressed blob.
    let mut volumes: Vec<(spall_protocol::baseline::BaselineVolume, Vec<u8>)> = Vec::new();
    for ((entity, _child_vid), child) in child_ids.iter().zip(children) {
        let bv = crate::replication::baseline_volume_of(
            &child.volume,
            BaselineOwner::Body(*entity),
            None,
        );
        let blob = bv.encode_compressed();
        volumes.push((bv, blob));
    }
    if splits {
        let bv = crate::replication::baseline_volume_of(
            parent_candidate,
            crate::replication::baseline_owner(parent_owner),
            Some(affected),
        );
        let blob = bv.encode_compressed();
        volumes.push((bv, blob));
    }

    // Inline path: every compressed blob fits one op.
    if volumes
        .iter()
        .all(|(_, blob)| blob.len() <= MAX_SPLIT_BASELINE_BLOB)
    {
        let mut ops = vec![brush_op];
        let mut it = volumes.into_iter();
        for ((entity, child_vid), _child) in child_ids.iter().zip(children) {
            let (_, blob) = it.next().expect("one blob per child");
            ops.push(TopologyOp::SplitOffBaseline {
                source,
                child: *child_vid,
                child_entity: *entity,
                blob,
            });
        }
        if splits {
            let (_, blob) = it.next().expect("source-patch blob");
            ops.push(TopologyOp::SourcePatchBaseline { source, blob });
        }
        return Ok(SplitEncoding::Inline(ops));
    }

    // Bulk path: marker ops + one out-of-band `BaselineWorld` keyed by
    // `transfer_id` (the split's `TransactionId` with the reserved high bit set).
    let transfer_id = transaction_id.get() | spall_protocol::SPLIT_BULK_TRANSFER_ID_BIT;
    let mut ops = vec![brush_op];
    for ((entity, child_vid), _child) in child_ids.iter().zip(children) {
        ops.push(TopologyOp::SplitOffBulkBaseline {
            source,
            child: *child_vid,
            child_entity: *entity,
            transfer_id,
        });
    }
    if splits {
        ops.push(TopologyOp::SourcePatchBulkBaseline {
            source,
            transfer_id,
        });
    }

    let mut world_volumes: Vec<_> = volumes.into_iter().map(|(bv, _)| bv).collect();
    world_volumes.sort_by_key(|v| v.volume_id.get());
    let baseline = BaselineWorld {
        schema: spall_protocol::BASELINE_WORLD_SCHEMA,
        checkpoint_tick: server_tick.get(),
        volumes: world_volumes,
    };
    let encoded_len = baseline.encode().len();
    if encoded_len > MAX_ASSEMBLED_TRANSFER
        || encoded_len.div_ceil(MAX_BULK_PART) > spall_protocol::limits::MAX_BASELINE_PARTS
    {
        return Err(CommitError::Replication(
            crate::replication::ReplicationError::SplitTooLarge {
                volume: source.get(),
                blob_bytes: encoded_len,
                cap: MAX_ASSEMBLED_TRANSFER,
            },
        ));
    }
    Ok(SplitEncoding::Bulk { ops, baseline })
}

/// A `MotionSnapshot` for every dynamic body that took part in the transaction:
/// the parent (if it is a body, read from the live world — a commit never moves
/// the parent) and each planned child (read from the candidate, since the child
/// bodies are not installed until publish).
#[allow(clippy::too_many_arguments)]
fn candidate_participant_snapshots(
    world: &SimWorld,
    parent_is_terrain: bool,
    parent_emptied: bool,
    parent_entity: Option<spall_core::EntityId>,
    parent_candidate: &Volume,
    children: &[ChildBody],
    server_tick: Tick,
    journal_seq: spall_core::JournalSeq,
) -> Vec<MotionSnapshot> {
    let mut out = Vec::new();
    // A parent whose last cell this transaction removed is retired on publish
    // (`ENG-56`): it no longer exists, so it carries no motion snapshot.
    if !parent_is_terrain
        && !parent_emptied
        && let Some(entity) = parent_entity
        && let Some(parent) = world.body(entity)
    {
        out.push(MotionSnapshot {
            server_tick,
            snapshot_seq: SnapshotSeq(journal_seq.0),
            acked_input: InputSeq(0),
            body: entity,
            topology_revision: latest_volume_revision(parent_candidate),
            pose: parent.pose.to_protocol(),
            linear_velocity: parent.linvel_m_s.map(|v| v as f32),
            angular_velocity: parent.angvel_rad_s.map(|v| v as f32),
            sleeping: parent.sleeping,
        });
    }
    for child in children {
        out.push(MotionSnapshot {
            server_tick,
            snapshot_seq: SnapshotSeq(journal_seq.0),
            acked_input: InputSeq(0),
            body: child.entity,
            topology_revision: latest_volume_revision(&child.volume),
            pose: child.pose.to_protocol(),
            linear_velocity: child.linvel_m_s.map(|v| v as f32),
            angular_velocity: child.angvel_rad_s.map(|v| v as f32),
            sleeping: false,
        });
    }
    out
}

fn latest_volume_revision(volume: &Volume) -> Revision {
    Revision(volume.next_revision().get().saturating_sub(1))
}

/// Re-export for callers assembling an `ActionStatus::Queued` before a commit.
pub fn queued_status(request_id: spall_protocol::RequestId) -> ActionStatus {
    ActionStatus {
        request_id,
        outcome: ActionOutcome::Queued,
    }
}

/// Unused transfer id sentinel kept for the journal/DTO surface; real transfers
/// are T17.
pub const NO_TRANSFER: TransferId = TransferId(0);
