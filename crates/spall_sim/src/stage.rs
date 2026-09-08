//! Off-tick preparation of an accepted intent.
//!
//! [`stage_edit`] runs against an *immutable snapshot* of the target volume: it
//! never touches live state. It produces everything the atomic commit needs —
//! the deterministic [`EditPlan`], the predicted [`EditOutcome`], the list of
//! components the edit disconnects, a balanced conservation ledger, and a
//! [`JobToken`] over every brick it read — so the tick-boundary commit is just a
//! revalidate-and-apply.

use spall_core::{BrickCoord, VolumeId};
use spall_jobs::{Generation, JobToken, TopologyEpoch};
use spall_structure::{
    AnchorPlane, CancelToken, ComponentMembership, ConservationLedger, Interrupted, ResidencyMode,
    SearchBudget, StructureIndex, SupportReport,
};
use spall_voxel::{BrickState, EditError, EditOutcome, EditPlan, Volume};

use crate::intent::{EditIntent, EditKind, EditTarget, ExplosionImpulse};
use crate::world::solid_cells;

/// The immutable inputs to one staging pass — a cheap clone bundle captured when
/// the job is submitted.
#[derive(Debug, Clone)]
pub struct StageInput {
    pub request_id: spall_protocol::RequestId,
    pub actor: spall_core::EntityId,
    pub target: EditTarget,
    pub volume_id: VolumeId,
    /// Snapshot of the target volume.
    pub volume: Volume,
    pub anchor: AnchorPlane,
    pub generation: Generation,
    pub topology_epoch: TopologyEpoch,
    pub kind: EditKind,
    pub brush: spall_core::SphereBrush,
    pub explosion: Option<ExplosionImpulse>,
}

impl StageInput {
    /// Builds staging inputs for `intent` against `snapshot`.
    pub fn new(
        intent: &EditIntent,
        volume_id: VolumeId,
        snapshot: Volume,
        anchor: AnchorPlane,
        generation: Generation,
        topology_epoch: TopologyEpoch,
    ) -> Self {
        Self {
            request_id: intent.request_id,
            actor: intent.actor,
            target: intent.target,
            volume_id,
            volume: snapshot,
            anchor,
            generation,
            topology_epoch,
            kind: intent.kind,
            brush: intent.brush,
            explosion: intent.explosion,
        }
    }
}

/// The prepared, not-yet-applied transaction.
#[derive(Debug, Clone)]
pub struct StagedEdit {
    pub request_id: spall_protocol::RequestId,
    pub actor: spall_core::EntityId,
    pub target: EditTarget,
    pub volume_id: VolumeId,
    pub kind: EditKind,
    pub brush: spall_core::SphereBrush,
    pub explosion: Option<ExplosionImpulse>,
    /// The deterministic write list.
    pub plan: EditPlan,
    /// The outcome predicted by dry-running `plan` on the snapshot.
    pub predicted_outcome: EditOutcome,
    /// Read-dependency token: pre-edit revisions of every brick the plan touches
    /// plus every brick the structural analysis read.
    pub token: JobToken,
    /// Support classification of the target volume *after* the edit.
    pub report: SupportReport,
    /// Canonical membership of every component that detaches.
    pub memberships: Vec<ComponentMembership>,
    /// `source == retained + child + destroyed`, balanced by construction and
    /// re-checked at commit.
    pub ledger: ConservationLedger,
    /// Solid cells in the target volume before the edit.
    pub pre_solid: u64,
}

impl StagedEdit {
    /// `true` when the edit disconnects at least one component into a new body.
    pub fn splits(&self) -> bool {
        !self.memberships.is_empty()
    }

    /// Bricks the plan writes, in canonical `(z, y, x)` order.
    pub fn touched_bricks(&self) -> Vec<BrickCoord> {
        self.predicted_outcome
            .bricks
            .iter()
            .map(|b| b.coord)
            .collect()
    }
}

/// Why staging failed. On any error the snapshot is untouched (it is a clone).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StageError {
    #[error("brush covers no cell in the target volume")]
    EmptyBrush,
    #[error("edit touches brick {0:?} outside the volume bounds")]
    OutOfBounds(BrickCoord),
    #[error("structural analysis was interrupted: {0}")]
    Structure(#[from] Interrupted),
    #[error("dry-run edit failed: {0}")]
    Edit(#[from] EditError),
}

/// Prepares `input` into a [`StagedEdit`].
pub fn stage_edit(input: &StageInput) -> Result<StagedEdit, StageError> {
    let plan = EditPlan::sphere(input.volume_id, input.brush, input.kind.write_material());
    if plan.writes.is_empty() {
        return Err(StageError::EmptyBrush);
    }

    let cancel = CancelToken::new();

    // Pre-edit structural read set + generation / epoch.
    let mut index = StructureIndex::build(
        &input.volume,
        input.anchor,
        ResidencyMode::AllResident,
        input.generation,
        input.topology_epoch,
        &cancel,
    )?;
    let mut token = index.token();

    // Merge in the plan's touched bricks at their pre-edit state.
    for coord in touched_brick_coords(&plan) {
        match input.volume.brick_state(coord) {
            Ok(BrickState::Resident { revision, .. }) => {
                token = token.reading(input.volume_id, coord, revision);
            }
            Ok(BrickState::Absent) => {
                token = token.reading_absent(input.volume_id, coord);
            }
            Ok(BrickState::Failed) => {
                token = token.reading_failed(input.volume_id, coord);
            }
            Err(_) => return Err(StageError::OutOfBounds(coord)),
        }
    }

    let pre_solid = solid_cells(&input.volume);

    // Dry-run the edit and re-classify support on the result.
    let mut post = input.volume.clone();
    let outcome = post.apply_edit(&plan)?;
    let report = index.apply_edit(
        &post,
        &outcome,
        input.topology_epoch,
        &cancel,
        SearchBudget::UNLIMITED,
    )?;

    // Which components detach:
    // - Terrain: every component the support search calls unsupported.
    // - A dynamic body: a free body has no anchor, so keep the largest component
    //   as the original body (its identity, entity and volume are unchanged) and
    //   detach every other component into a new body. A second cut therefore adds
    //   bodies without leaving an empty husk.
    let memberships: Vec<ComponentMembership> = match input.target {
        EditTarget::Terrain => index.split_plans(),
        EditTarget::Body(_) => detached_from_body(&index),
    };

    let post_solid = report.total_solid_cells;
    let child_cells: u64 = memberships.iter().map(|m| m.cell_count).sum();
    let ledger = ConservationLedger {
        source_occupied: pre_solid,
        retained: post_solid.saturating_sub(child_cells),
        child_cells,
        destroyed: pre_solid.saturating_sub(post_solid),
    };

    Ok(StagedEdit {
        request_id: input.request_id,
        actor: input.actor,
        target: input.target,
        volume_id: input.volume_id,
        kind: input.kind,
        brush: input.brush,
        explosion: input.explosion,
        plan,
        predicted_outcome: outcome,
        token,
        report,
        memberships,
        ledger,
        pre_solid,
    })
}

/// Components of a dynamic body that detach when it is cut: all but the largest
/// (ties broken by the smallest canonical component id). Empty when the cut left
/// the body in one piece.
fn detached_from_body(index: &StructureIndex) -> Vec<ComponentMembership> {
    let graph = index.graph();
    let comps = graph.components();
    if comps.len() <= 1 {
        return Vec::new();
    }
    let keep = comps
        .iter()
        .enumerate()
        .max_by_key(|(_, c)| (c.cell_count, std::cmp::Reverse(c.id.0)))
        .map(|(i, _)| i)
        .expect("components is non-empty");
    comps
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != keep)
        .map(|(_, c)| ComponentMembership::from_component(graph, c))
        .collect()
}

fn touched_brick_coords(plan: &EditPlan) -> Vec<BrickCoord> {
    let mut coords: Vec<BrickCoord> = plan.writes.iter().map(|w| w.cell.split().0).collect();
    coords.sort_by_key(|c| c.sort_key());
    coords.dedup();
    coords
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::units::{BRUSH_UNIT, BrushPoint};
    use spall_core::{CellSizeCode, EntityId, GlobalCell, SphereBrush};
    use spall_structure::AnchorPlane;
    use spall_voxel::Volume;

    use crate::intent::{EditIntent, EditTarget};

    fn column_beam_terrain() -> (VolumeId, Volume) {
        let id = VolumeId::new(1).unwrap();
        let mut v = Volume::new(id, CellSizeCode::Quarter);
        for (a, b) in [
            (GlobalCell::new(0, 0, 0), GlobalCell::new(23, 1, 3)),
            (GlobalCell::new(10, 2, 1), GlobalCell::new(11, 7, 2)),
            (GlobalCell::new(4, 8, 1), GlobalCell::new(20, 9, 2)),
        ] {
            v.apply_edit(&EditPlan::filled_box(id, a, b, spall_core::MaterialId(1)))
                .unwrap();
        }
        (id, v)
    }

    fn brush(x: i64, y: i64, z: i64, r: i64) -> SphereBrush {
        let h = BRUSH_UNIT / 2;
        SphereBrush::new(
            BrushPoint::from_units(x * BRUSH_UNIT + h, y * BRUSH_UNIT + h, z * BRUSH_UNIT + h),
            r * BRUSH_UNIT,
        )
        .unwrap()
    }

    #[test]
    fn a_staged_edit_records_its_touched_bricks_and_balances_conservation() {
        let (vid, terrain) = column_beam_terrain();
        let intent = EditIntent::cut(
            spall_protocol::RequestId(1),
            EntityId::new(1).unwrap(),
            EditTarget::Terrain,
            brush(10, 4, 1, 2),
        );
        let input = StageInput::new(
            &intent,
            vid,
            terrain,
            AnchorPlane::at(0),
            Generation::START,
            TopologyEpoch::START,
        );
        let staged = stage_edit(&input).unwrap();

        // The token names every brick the plan writes.
        let touched = staged.touched_bricks();
        let dep_bricks: Vec<_> = staged.token.reads().iter().map(|d| d.brick.brick).collect();
        assert!(touched.iter().all(|b| dep_bricks.contains(b)));

        // source == retained + child + destroyed, exactly.
        let l = staged.ledger;
        assert_eq!(l.source_occupied, l.retained + l.child_cells + l.destroyed);
        assert!(l.check().is_ok());
        // Cutting the column disconnects the beam: there is a child.
        assert!(staged.splits(), "the beam detaches");
        assert!(l.child_cells > 0 && l.destroyed > 0);
    }

    #[test]
    fn an_empty_brush_is_rejected_deterministically() {
        let (vid, terrain) = column_beam_terrain();
        let intent = EditIntent::cut(
            spall_protocol::RequestId(1),
            EntityId::new(1).unwrap(),
            EditTarget::Terrain,
            brush(-500, -500, -500, 1), // nowhere near any solid or air cell it writes
        );
        // A sphere far from everything still writes air cells over empty space,
        // so use a zero-radius brush at an integer corner: no cell centre inside.
        let _ = intent;
        let zero = SphereBrush::new(BrushPoint::from_cells(0, 0, 0).unwrap(), 0).unwrap();
        let intent = EditIntent::cut(
            spall_protocol::RequestId(2),
            EntityId::new(1).unwrap(),
            EditTarget::Terrain,
            zero,
        );
        let input = StageInput::new(
            &intent,
            vid,
            terrain,
            AnchorPlane::at(0),
            Generation::START,
            TopologyEpoch::START,
        );
        assert!(matches!(stage_edit(&input), Err(StageError::EmptyBrush)));
    }
}
