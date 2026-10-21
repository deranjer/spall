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
use spall_voxel::{BrickState, EditError, EditOutcome, EditPlan, EvictedBricks, Sample, Volume};

use crate::intent::{EditIntent, EditKind, EditTarget, ExplosionImpulse};

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
    /// T23 / G3 row 7, slice B: the target volume's retained evicted-brick
    /// digests. Empty by default (residency is off until slice D), so every
    /// path below is byte-identical to today. When non-empty, staging refuses
    /// an edit whose structural analysis would read an evicted brick as empty —
    /// the caller must reload it and re-stage.
    pub evicted: EvictedBricks,
    pub anchor: AnchorPlane,
    pub generation: Generation,
    pub topology_epoch: TopologyEpoch,
    pub kind: EditKind,
    pub brush: spall_core::SphereBrush,
    pub explosion: Option<ExplosionImpulse>,
    /// Memoized brick labels of the **live** volume `volume` was cloned from,
    /// shared by every staging pass of one pipeline. `None` relabels every brick
    /// each time (small worlds, tests). Never hand it a dry-run volume.
    pub label_cache: Option<spall_structure::LabelCache>,
    /// The world's structure index for exactly this `volume` state, if it kept one (see
    /// [`crate::SimWorld::warm_structure_index`]). Cloned instead of rebuilding the index.
    pub warm_index: Option<std::sync::Arc<spall_structure::StructureIndex>>,
    /// Detached-cell removal still to be applied to `warm_index` (see
    /// [`crate::SimWorld::warm_structure_removal`]).
    pub warm_removal: Option<spall_voxel::EditOutcome>,
}

impl StageInput {
    /// Builds staging inputs for `intent` against `snapshot`. `evicted` is the
    /// target volume's retained evicted-brick digests (`EvictedBricks::new()`
    /// when residency is off).
    pub fn new(
        intent: &EditIntent,
        volume_id: VolumeId,
        snapshot: Volume,
        evicted: EvictedBricks,
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
            evicted,
            anchor,
            generation,
            topology_epoch,
            kind: intent.kind,
            brush: intent.brush,
            explosion: intent.explosion,
            label_cache: None,
            warm_index: None,
            warm_removal: None,
        }
    }

    /// Reuses `cache` for the structural analysis of this live snapshot.
    #[must_use]
    /// Hands staging a structure index that already describes `volume`'s exact state.
    pub fn with_warm_index(
        mut self,
        index: Option<std::sync::Arc<spall_structure::StructureIndex>>,
    ) -> Self {
        self.warm_index = index;
        self
    }

    /// Hands staging the removal that completes `warm_index` (see [`Self::warm_removal`]).
    pub fn with_warm_removal(mut self, removal: Option<spall_voxel::EditOutcome>) -> Self {
        self.warm_removal = removal;
        self
    }

    pub fn with_label_cache(mut self, cache: spall_structure::LabelCache) -> Self {
        self.label_cache = Some(cache);
        self
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
    /// Material counts removed by a validated cut. These are calculated from
    /// the immutable pre-edit snapshot and travel with the staged result; a
    /// server game hook may award drops only after this edit commits.
    pub removed_materials: std::collections::BTreeMap<spall_core::MaterialId, u64>,
    /// [`spall_voxel::Volume::state_stamp`] of the volume this was staged against.
    pub input_stamp: u64,
    /// The structure index right after the cut (any detached cells still in it), for a terrain
    /// edit: exactly the index of the world a commit produces when nothing detaches, and the
    /// starting point for it when something does.
    pub post_index: Option<std::sync::Arc<spall_structure::StructureIndex>>,
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
    #[error(
        "edit needs evicted geometry that is not loaded (bricks {0:?}); \
         the caller must reload it and re-stage"
    )]
    EvictedGeometryRequired(Vec<BrickCoord>),
}

static WARM_INDEX_REUSES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many staging passes, process-wide, cloned a warm structure index instead of building one.
pub fn warm_index_reuses() -> u64 {
    WARM_INDEX_REUSES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Prepares `input` into a [`StagedEdit`].
pub fn stage_edit(input: &StageInput) -> Result<StagedEdit, StageError> {
    // A shared index is cloned: other stagings may be using the same one.
    stage_with(input, input.warm_index.as_deref().cloned())
}

/// [`stage_edit`] for a caller that hands over the only reference to the warm index: it is
/// edited in place rather than cloned (a clone of a whole-world index costs more than the edit).
/// If anything else still holds the index, it is cloned after all.
pub(crate) fn stage_edit_owned(mut input: StageInput) -> Result<StagedEdit, StageError> {
    let warm = input
        .warm_index
        .take()
        .map(|index| std::sync::Arc::try_unwrap(index).unwrap_or_else(|shared| (*shared).clone()));
    stage_with(&input, warm)
}

fn stage_with(input: &StageInput, warm: Option<StructureIndex>) -> Result<StagedEdit, StageError> {
    let _total = crate::prof::Span::start("stage.total");
    let plan = EditPlan::sphere(input.volume_id, input.brush, input.kind.write_material());
    if plan.writes.is_empty() {
        return Err(StageError::EmptyBrush);
    }

    let mut removed_materials = std::collections::BTreeMap::new();
    if input.kind == EditKind::Cut {
        for write in &plan.writes {
            if let Sample::Filled(material) = input
                .volume
                .sample(write.cell)
                .map_err(|_| StageError::OutOfBounds(write.cell.split().0))?
            {
                *removed_materials.entry(material).or_default() += 1;
            }
        }
    }

    let cancel = CancelToken::new();

    // Pre-edit structural read set + generation / epoch.
    let sp_idx = crate::prof::Span::start("stage.structure_index_build");
    #[cfg(debug_assertions)]
    let had_warm_index = warm.is_some();
    let mut index = match warm {
        Some(mut warm) => {
            WARM_INDEX_REUSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if let Some(removal) = &input.warm_removal {
                // The index is for the volume right after the split's cut; take the detached
                // cells out, relabelling only the bricks they touched.
                warm.apply_edit(
                    &input.volume,
                    removal,
                    input.topology_epoch,
                    &cancel,
                    SearchBudget::UNLIMITED,
                )?;
            }
            warm
        }
        None => match &input.label_cache {
            Some(cache) => StructureIndex::build_cached(
                &input.volume,
                input.anchor,
                ResidencyMode::AllResident,
                input.generation,
                input.topology_epoch,
                &cancel,
                cache,
            )?,
            None => StructureIndex::build(
                &input.volume,
                input.anchor,
                ResidencyMode::AllResident,
                input.generation,
                input.topology_epoch,
                &cancel,
            )?,
        },
    };
    drop(sp_idx);
    let token_span = crate::prof::Span::start("stage.read_token");
    let mut token = index.token();
    #[cfg(debug_assertions)]
    let warm_token_before = had_warm_index.then(|| token.clone());

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

    // Every read above is of `input.volume`, which is exactly the state this edit was staged
    // against: while the live volume still carries this stamp, validation need not compare them.
    token = token.with_state_stamp(input.volume_id, input.volume.state_stamp());
    drop(token_span);
    // Conservation is over the *logical* solid-cell count (resident + retained
    // evicted digests), so eviction never shifts the ledger regardless of where
    // the evicted bricks are. Identical to a resident-only walk when nothing is
    // evicted.
    let pre_solid = spall_voxel::logical_solid_cells(&input.volume, &input.evicted)
        .map_err(|_| StageError::EvictedGeometryRequired(Vec::new()))?;

    // Dry-run the edit and re-classify support on the result.
    let sp_dry = crate::prof::Span::start("stage.dry_run_and_reclassify");
    let sp_clone = crate::prof::Span::start("stage.dry_run.volume_clone");
    let mut post = input.volume.clone();
    drop(sp_clone);
    let sp_edit = crate::prof::Span::start("stage.dry_run.apply_edit");
    let outcome = post.apply_edit(&plan)?;
    drop(sp_edit);
    let sp_reclassify = crate::prof::Span::start("stage.dry_run.reclassify");
    let report = index.apply_edit(
        &post,
        &outcome,
        input.topology_epoch,
        &cancel,
        SearchBudget::UNLIMITED,
    )?;
    drop(sp_reclassify);

    drop(sp_dry);
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

    // A reused index must be indistinguishable from a rebuilt one: every debug build (and so every
    // test that stages against a warm index) rebuilds it and compares.
    #[cfg(debug_assertions)]
    if let Some(token_before) = warm_token_before {
        let mut fresh = StructureIndex::build(
            &input.volume,
            input.anchor,
            ResidencyMode::AllResident,
            input.generation,
            input.topology_epoch,
            &cancel,
        )?;
        assert_eq!(
            fresh.token(),
            token_before,
            "warm structure index carries a different token (generation, epoch or reads) than a rebuilt one"
        );
        let fresh_report = fresh.apply_edit(
            &post,
            &outcome,
            input.topology_epoch,
            &cancel,
            SearchBudget::UNLIMITED,
        )?;
        assert_eq!(
            fresh_report, report,
            "warm structure index classified the edit differently from a rebuilt one"
        );
        let cell_counts = |index: &StructureIndex| {
            index
                .graph()
                .components()
                .iter()
                .map(|c| c.cell_count)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            cell_counts(&fresh),
            cell_counts(&index),
            "warm structure index holds different components than a rebuilt one"
        );
    }

    // T23 / G3 row 7, slice B: the structural analysis ran `AllResident`, which
    // reads an absent brick as known-empty. If an *evicted* brick (holds
    // durable geometry) is inside — or a face neighbour of — the region this
    // edit actually changes (its written bricks, and every component it
    // detaches), the classification for this transaction is unsound: refuse, so
    // the caller reloads that geometry and re-stages. An evicted brick far from
    // the edit, bordering unrelated geometry, does not affect this
    // transaction's `before` / `after` / `result_hashes` and is left alone
    // (conservation below uses the logical count regardless). Dependency-
    // complete reload-and-retry is the residency pass's job (slice C/D).
    if !input.evicted.is_empty() {
        let mut region: Vec<BrickCoord> = touched_brick_coords(&plan);
        for m in &memberships {
            for cell in m.cells() {
                region.push(cell.split().0);
            }
        }
        let mut needed: Vec<BrickCoord> = Vec::new();
        for brick in region {
            for c in std::iter::once(brick).chain(face_neighbours(brick)) {
                if input.evicted.contains(c) && !needed.contains(&c) {
                    needed.push(c);
                }
            }
        }
        if !needed.is_empty() {
            needed.sort_by_key(|c| c.sort_key());
            needed.dedup();
            return Err(StageError::EvictedGeometryRequired(needed));
        }
    }

    // Post-edit solid count, over the logical brick set: the evicted bricks are
    // untouched by the edit, so their digests still hold, and `destroyed` stays
    // exact no matter where they are.
    //
    // Only the bricks the edit wrote can differ, so the total follows from them instead of a
    // second pass over every brick of the world.
    let solid_in = |volume: &Volume, coord: BrickCoord| {
        volume
            .snapshot_brick(coord)
            .ok()
            .flatten()
            .map_or(0, |snap| i128::from(snap.solid_cells()))
    };
    let mut post_solid_signed = i128::from(pre_solid);
    for brick in &outcome.bricks {
        post_solid_signed += solid_in(&post, brick.coord) - solid_in(&input.volume, brick.coord);
    }
    let post_solid = u64::try_from(post_solid_signed)
        .map_err(|_| StageError::EvictedGeometryRequired(Vec::new()))?;
    // The shortcut is exact; every debug build (and so every test) checks it against the full count.
    debug_assert_eq!(
        Ok(post_solid),
        spall_voxel::logical_solid_cells(&post, &input.evicted),
        "incremental solid-cell count diverged from the full count"
    );
    // Kept for every terrain edit. It describes the volume right after the cut; a commit that
    // splits also removes the detached cells, which the commit applies to it before keeping it.
    let post_index = (input.target == EditTarget::Terrain).then(|| std::sync::Arc::new(index));
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
        removed_materials,
        input_stamp: input.volume.state_stamp(),
        post_index,
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

/// The six face-adjacent brick coordinates of `c` (T23 / G3 row 7 slice B
/// evicted-geometry check).
fn face_neighbours(c: BrickCoord) -> [BrickCoord; 6] {
    [
        BrickCoord::new(c.x - 1, c.y, c.z),
        BrickCoord::new(c.x + 1, c.y, c.z),
        BrickCoord::new(c.x, c.y - 1, c.z),
        BrickCoord::new(c.x, c.y + 1, c.z),
        BrickCoord::new(c.x, c.y, c.z - 1),
        BrickCoord::new(c.x, c.y, c.z + 1),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "manual full 4096-cell world staging profile; several GiB"]
    fn full_world_stage_profile() {
        full_world_edit_profile(false);
    }

    #[test]
    #[ignore = "manual: startup order probe (generate, world with colliders, warm labels)"]
    fn startup_order_probe() {
        use spall_worldgen::{Preset, WorldGenSpec, WorldgenPalette};
        let generated = spall_worldgen::generate(&WorldGenSpec::new(
            Preset::Showcase,
            1,
            4096,
            WorldgenPalette::sequential(1),
        ))
        .unwrap();
        let mut setup = crate::fixtures::flat_terrain_setup();
        setup.terrain = generated.terrain;
        let _ = crate::prof::drain();
        let started = std::time::Instant::now();
        let sim = crate::Simulation::new(crate::SimulationConfig::new(setup)).unwrap();
        eprintln!(
            "order_probe simulation_new_ms={}",
            started.elapsed().as_millis()
        );
        for (name, duration) in crate::prof::drain() {
            eprintln!(
                "order_probe {name} ms={:.0}",
                duration.as_secs_f64() * 1000.0
            );
        }
        drop(sim);
    }

    #[test]
    #[ignore = "manual full 4096-cell commit profile with exact terrain colliders; several GiB"]
    fn full_world_commit_profile() {
        full_world_edit_profile(true);
    }

    fn full_world_edit_profile(with_commit: bool) {
        use spall_worldgen::{Preset, WorldGenSpec, WorldgenPalette};
        let started = std::time::Instant::now();
        let generated = spall_worldgen::generate(&WorldGenSpec::new(
            Preset::Showcase,
            1,
            4096,
            WorldgenPalette::sequential(1),
        ))
        .unwrap();
        eprintln!(
            "stage_profile generation_ms={}",
            started.elapsed().as_millis()
        );
        let cache = spall_structure::LabelCache::new();
        let started = std::time::Instant::now();
        let graph = spall_structure::SupportGraph::build_cached(
            &generated.terrain,
            AnchorPlane::at(0),
            ResidencyMode::AllResident,
            &CancelToken::new(),
            &cache,
        )
        .unwrap();
        let clone_started = std::time::Instant::now();
        let cloned = graph.clone();
        eprintln!(
            "stage_profile support_graph_clone_ms={:.1}",
            clone_started.elapsed().as_secs_f64() * 1000.0
        );
        drop(cloned);
        drop(graph);
        eprintln!(
            "stage_profile warm_labels_ms={}",
            started.elapsed().as_millis()
        );
        let mut world = with_commit.then(|| {
            let mut setup = crate::fixtures::flat_terrain_setup();
            setup.terrain = generated.terrain.clone();
            let started = std::time::Instant::now();
            let world = crate::world::SimWorld::new(setup).unwrap();
            eprintln!(
                "commit_profile world_init_ms={}",
                started.elapsed().as_millis()
            );
            // The serving host hashes the initial baseline before accepting edits.
            let _ = world.world_hash();
            world
        });
        let intent = EditIntent::cut(
            spall_protocol::RequestId(1),
            spall_core::EntityId::new(1).unwrap(),
            EditTarget::Terrain,
            brush(1024, 16, 1024, 8),
        );
        let input = StageInput::new(
            &intent,
            generated.terrain.id(),
            generated.terrain,
            EvictedBricks::new(),
            AnchorPlane::at(0),
            Generation::START,
            TopologyEpoch::START,
        )
        .with_label_cache(cache);
        let _ = crate::prof::drain();
        let staged = stage_edit(&input).unwrap();
        for (name, duration) in crate::prof::drain() {
            eprintln!(
                "stage_profile {name} ms={:.3}",
                duration.as_secs_f64() * 1000.0
            );
        }
        assert!(staged.ledger.check().is_ok());
        eprintln!(
            "stage_profile read_dependencies={} splits={} destroyed={}",
            staged.token.reads().len(),
            staged.memberships.len(),
            staged.ledger.destroyed
        );
        if let Some(world) = world.as_mut() {
            let solids = world.total_solid_cells();
            let mut journal = crate::journal::JournalSink::new();
            let committed = crate::commit::commit(
                world,
                &mut journal,
                &staged,
                spall_core::Tick(1),
                spall_protocol::ControlSeq(1),
            )
            .unwrap();
            let crate::commit::CommitOutcome::Committed(committed) = committed else {
                panic!("profile token unexpectedly stale");
            };
            for (name, duration) in crate::prof::drain() {
                eprintln!(
                    "commit_profile {name} ms={:.3}",
                    duration.as_secs_f64() * 1000.0
                );
            }
            assert_eq!(world.total_solid_cells(), solids - staged.ledger.destroyed);
            for result in &committed.topology.result_hashes {
                assert_eq!(world.volume_hash(result.volume).unwrap(), result.hash);
            }
            assert_eq!(journal.len(), 1);

            // Ordinary edits against the now-warm world, staged exactly as the scheduler does:
            // snapshot, evicted digests, the warm index. What a normal cut costs.
            let terrain = world.terrain_volume_id();
            let label_cache = spall_structure::LabelCache::new();
            let _ = &label_cache;
            for k in 0..3_i64 {
                let intent = EditIntent::cut(
                    spall_protocol::RequestId(10 + k as u64),
                    spall_core::EntityId::new(1).unwrap(),
                    EditTarget::Terrain,
                    brush(1100 + k * 40, 16, 1100, 2),
                );
                let _ = crate::prof::drain();
                let wall = std::time::Instant::now();
                let snapshot = world.volume_ref(terrain).unwrap().clone();
                let stamp = snapshot.state_stamp();
                let (warm, removal) = match world.take_warm_structure(terrain, stamp) {
                    Some((index, removal)) => (Some(index), removal),
                    None => (None, None),
                };
                let had_warm = warm.is_some();
                let input = StageInput::new(
                    &intent,
                    terrain,
                    snapshot,
                    world.evicted(terrain).clone(),
                    world.anchor(),
                    world.generation(),
                    world.topology_epoch(),
                )
                .with_warm_index(warm)
                .with_warm_removal(removal);
                let snapshot_ms = wall.elapsed().as_secs_f64() * 1000.0;
                let staged = stage_edit_owned(input).unwrap();
                let stage_ms = wall.elapsed().as_secs_f64() * 1000.0 - snapshot_ms;
                let stage_spans = crate::prof::drain();
                let mut journal = crate::journal::JournalSink::new();
                let commit_started = std::time::Instant::now();
                let outcome = crate::commit::commit(
                    world,
                    &mut journal,
                    &staged,
                    spall_core::Tick(2 + k as u64),
                    spall_protocol::ControlSeq(2 + k as u64),
                )
                .unwrap();
                let commit_ms = commit_started.elapsed().as_secs_f64() * 1000.0;
                assert!(matches!(
                    outcome,
                    crate::commit::CommitOutcome::Committed(_)
                ));
                eprintln!(
                    "warm_edit[{k}] warm_index={had_warm} snapshot_ms={snapshot_ms:.1} stage_ms={stage_ms:.1} commit_ms={commit_ms:.1} splits={}",
                    staged.memberships.len()
                );
                for (name, duration) in stage_spans {
                    eprintln!(
                        "warm_edit[{k}]   {name} ms={:.2}",
                        duration.as_secs_f64() * 1000.0
                    );
                }
                for (name, duration) in crate::prof::drain() {
                    eprintln!(
                        "warm_edit[{k}]   {name} ms={:.2}",
                        duration.as_secs_f64() * 1000.0
                    );
                }
            }
        }
    }
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
            spall_voxel::EvictedBricks::new(),
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
            spall_voxel::EvictedBricks::new(),
            AnchorPlane::at(0),
            Generation::START,
            TopologyEpoch::START,
        );
        assert!(matches!(stage_edit(&input), Err(StageError::EmptyBrush)));
    }
}
