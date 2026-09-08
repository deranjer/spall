//! The atomic tick-boundary commit.
//!
//! [`commit`] takes one [`StagedEdit`], re-validates its [`spall_jobs::JobToken`]
//! against the *live* world, and — only if nothing it read has moved — applies
//! the whole transaction in one shot:
//!
//! 1. the deterministic brush plan is applied to the live target volume;
//! 2. every component the edit disconnected is copied into a new body at its
//!    exact world location with inherited mass and velocity, then removed from
//!    the parent;
//! 3. every affected collider is rebuilt in this same call, so a physics query
//!    on the next step can never see the old geometry;
//! 4. a [`TopologyTransaction`] and a journal entry are emitted from the one
//!    committed state.
//!
//! A stale token returns [`CommitOutcome::Stale`] and changes nothing; the
//! caller recomputes the intent and retries it in request order.

use spall_core::{BrickCoord, MaterialId, Revision, Tick, VolumeId};
use spall_jobs::Staleness;
use spall_physics::{BodyKind as PhysBodyKind, BodySpec, OccupancyGrid, analytic_mass_properties};
use spall_protocol::{
    ActionOutcome, ActionStatus, BrickRevision, ControlSeq, InputSeq, MotionSnapshot, Record,
    RecordError, SnapshotSeq, TopologyOp, TopologyTransaction, TransferId, VolumeHash,
};
use spall_voxel::{EditError, EditPlan};

use crate::body::{Body, BodyKind, BodyPose};
use crate::collider::plan_collider;
use crate::journal::{JournalEntry, JournalSink};
use crate::stage::StagedEdit;
use crate::transfer::{self, ChildBody, ParentState};
use crate::world::SimWorld;

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
    let parent_region = parent.collider_region;

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
        let (_, local_com, _) = world.physics().derived_mass_properties(parent_phys);
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

    // 3. Allocate child identities up front (never reused).
    let mut child_ids: Vec<(spall_core::EntityId, VolumeId)> = Vec::new();
    for _ in &staged.memberships {
        let e = world.registry_mut().allocate_entity()?;
        let v = world.registry_mut().allocate_volume()?;
        child_ids.push((e, v));
    }

    // 4. Apply the brush to the live target volume.
    let cut_outcome = {
        let parent = world
            .volume_body_mut(vid)
            .ok_or(CommitError::UnknownVolume(vid))?;
        parent.volume.apply_edit(&staged.plan)?
    };

    // 5. Build every child from the post-cut parent (components are still solid),
    //    and capture the canonical cell-run ops a replica needs to reconstruct
    //    the child without any structural code (`docs/protocol.md`).
    let mut children: Vec<ChildBody> = Vec::new();
    let mut child_fill_ops: Vec<Vec<spall_protocol::TopologyOp>> = Vec::new();
    {
        let parent_vol = world
            .volume_ref(vid)
            .ok_or(CommitError::UnknownVolume(vid))?;
        for ((entity, child_vid), membership) in child_ids.iter().zip(&staged.memberships) {
            let child = transfer::plan_child(
                parent_vol,
                membership,
                parent_state,
                *entity,
                *child_vid,
                &|m: MaterialId| world.density(m),
                0,
            )?;
            child_fill_ops.push(crate::replication::child_fill_ops(
                *child_vid, membership, parent_vol,
            ));
            children.push(child);
        }
    }
    transfer::apply_explosion(&mut children, staged.explosion);

    // 6. Remove the detached cells from the parent.
    let remove_outcome = if staged.splits() {
        let mut remove = EditPlan::new(vid);
        for membership in &staged.memberships {
            for cell in membership.cells() {
                remove.set(cell, MaterialId::AIR);
            }
        }
        let parent = world
            .volume_body_mut(vid)
            .ok_or(CommitError::UnknownVolume(vid))?;
        Some(parent.volume.apply_edit(&remove)?)
    } else {
        None
    };

    // 7. Rebuild the parent collider from its final geometry, and reinstall its
    //    mass properties from the post-cut fine material grid (a dynamic parent
    //    lost mass to the cut / to its children; a terrain parent has no mass).
    let rebuild = {
        let parent = world.volume_body(vid).expect("parent still exists");
        match OccupancyGrid::from_volume(&parent.volume)? {
            Some(grid) => {
                let plan = plan_collider(&grid)?;
                let mass_properties = (!parent_is_terrain).then(|| {
                    analytic_mass_properties(&grid, cell_size.metres(), |m| world.density(m))
                        .to_body_properties()
                });
                Some((parent.phys, plan, mass_properties))
            }
            None => None,
        }
    };
    if let Some((phys, plan, mass_properties)) = rebuild {
        world
            .physics_mut()
            .rebuild_collider(phys, &plan.grid, plan.representation);
        if let Some(mass_properties) = mass_properties {
            world
                .physics_mut()
                .set_mass_properties(phys, mass_properties);
        }
        let parent = world.volume_body_mut(vid).expect("parent still exists");
        parent.collider_revision += 1;
        parent.coarsen_k = plan.coarsen_k;
    }
    let _ = parent_region;

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

    // 9. Advance the epoch iff cells moved between components.
    let bumped_epoch = staged.splits();
    if bumped_epoch {
        world.bump_topology_epoch();
    }

    // 10. Emit the transaction DTO and the journal entry.
    let transaction_id = world.registry_mut().allocate_transaction()?;
    let journal_seq = world.registry_mut().allocate_journal_seq()?;

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
            revision: world
                .volume_ref(vid)
                .and_then(|v| v.brick_revision(coord).ok().flatten())
                .unwrap_or(Revision::ZERO),
        })
        .collect();

    // Self-describing op list: the brush, then for each child a `SplitOff`
    // marker followed by the canonical cell runs that fill it, then the runs
    // that remove every detached cell from the source. A replica applies these
    // in order to reproduce the exact committed geometry.
    let mut ops = vec![TopologyOp::IntegerBrush {
        volume: vid,
        brush: staged.brush,
        material: staged.kind.write_material(),
    }];
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
    crate::replication::check_op_budget(ops.len())?;

    let mut result_hashes = vec![VolumeHash {
        volume: vid,
        hash: world
            .volume_hash(vid)
            .unwrap_or(spall_protocol::Hash32::ZERO),
    }];
    for (_, child_vid) in &child_ids {
        result_hashes.push(VolumeHash {
            volume: *child_vid,
            hash: world
                .volume_hash(*child_vid)
                .unwrap_or(spall_protocol::Hash32::ZERO),
        });
    }

    let topology = TopologyTransaction {
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
    topology.validate()?;

    let participants = participant_snapshots(world, vid, &child_entities, server_tick, journal_seq);
    journal.append(JournalEntry {
        seq: journal_seq,
        transaction: topology.clone(),
        participants,
    });

    Ok(CommitOutcome::Committed(Committed {
        transaction: transaction_id,
        journal_seq,
        topology,
        children: child_entities,
        bumped_epoch,
    }))
}

/// A `MotionSnapshot` for every dynamic body that took part in the transaction:
/// the parent (if it is a body) and each child.
fn participant_snapshots(
    world: &SimWorld,
    parent_vid: VolumeId,
    children: &[spall_core::EntityId],
    server_tick: Tick,
    journal_seq: spall_core::JournalSeq,
) -> Vec<MotionSnapshot> {
    let mut out = Vec::new();
    let mut push = |body: &Body| {
        let Some(entity) = body.entity else { return };
        out.push(MotionSnapshot {
            server_tick,
            snapshot_seq: SnapshotSeq(journal_seq.0),
            acked_input: InputSeq(0),
            body: entity,
            topology_revision: latest_revision(world, body.volume_id),
            pose: body.pose.to_protocol(),
            linear_velocity: body.linvel_m_s.map(|v| v as f32),
            angular_velocity: body.angvel_rad_s.map(|v| v as f32),
            sleeping: body.sleeping,
        });
    };
    if let Some(parent) = world.body_by_volume(parent_vid) {
        push(parent);
    }
    for entity in children {
        if let Some(body) = world.body(*entity) {
            push(body);
        }
    }
    out
}

fn latest_revision(world: &SimWorld, volume: VolumeId) -> Revision {
    world
        .volume_ref(volume)
        .map(|v| {
            let next = v.next_revision().get();
            Revision(next.saturating_sub(1))
        })
        .unwrap_or(Revision::ZERO)
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
