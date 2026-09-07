//! T07 acceptance scenarios.
//!
//! Each test builds a small deterministic fixture, runs the incremental support
//! index, and cross-checks it against the dense flood-fill oracle. The oracle is
//! the "tiny reference algorithm" required by docs/validation.md; exact cell
//! sets, not floating point, are compared.

use std::collections::BTreeSet;

use spall_core::{CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};
use spall_jobs::testkit::MapWorld;
use spall_jobs::{Generation, Staleness, TopologyEpoch};
use spall_voxel::{EditPlan, Volume};

use crate::graph::{
    AnchorPlane, CancelToken, Interrupted, ResidencyMode, SearchBudget, SupportGraph,
};
use crate::oracle::dense_support;
use crate::support::{StructureIndex, Support};

const STONE: MaterialId = MaterialId(1);

fn vid() -> VolumeId {
    VolumeId::new(1).unwrap()
}

fn terrain() -> Volume {
    Volume::new(vid(), CellSizeCode::Quarter)
}

fn fill(v: &mut Volume, a: GlobalCell, b: GlobalCell, m: MaterialId) {
    let plan = EditPlan::filled_box(v.id(), a, b, m);
    v.apply_edit(&plan).expect("fill edit applies");
}

fn build(v: &Volume, plane_y: i64) -> StructureIndex {
    build_mode(v, plane_y, ResidencyMode::AllResident)
}

fn build_mode(v: &Volume, plane_y: i64, residency: ResidencyMode) -> StructureIndex {
    StructureIndex::build(
        v,
        AnchorPlane::at(plane_y),
        residency,
        Generation(1),
        TopologyEpoch::START,
        &CancelToken::new(),
    )
    .expect("uncancelled build succeeds")
}

/// The set of solid cells the index classifies as unsupported (across all
/// unsupported components).
fn index_unsupported_cells(index: &StructureIndex) -> BTreeSet<(i64, i64, i64)> {
    index
        .split_plans()
        .into_iter()
        .flat_map(|m| m.cells().map(|c| (c.x, c.y, c.z)).collect::<Vec<_>>())
        .collect()
}

/// The oracle's unsupported cells as one flat set.
fn oracle_unsupported_cells(v: &Volume, plane_y: i64) -> BTreeSet<(i64, i64, i64)> {
    dense_support(v, AnchorPlane::at(plane_y))
        .unsupported_components
        .into_iter()
        .flatten()
        .collect()
}

fn assert_matches_oracle(index: &StructureIndex, v: &Volume, plane_y: i64) {
    let oracle = dense_support(v, AnchorPlane::at(plane_y));
    let report = index.report();

    assert_eq!(
        report.total_solid_cells as usize, oracle.total_solid,
        "total solid cell count"
    );
    assert_eq!(
        report.supported_cells as usize,
        oracle.supported_count(),
        "supported cell count"
    );
    assert_eq!(
        report.unsupported_cells as usize,
        oracle.unsupported_count(),
        "unsupported cell count"
    );
    assert_eq!(
        index.split_plans().len(),
        oracle.unsupported_components.len(),
        "number of detached bodies"
    );
    assert_eq!(
        index_unsupported_cells(index),
        oracle_unsupported_cells(v, plane_y),
        "exact unsupported cell membership"
    );

    // Every split plan's own cell_count equals the length of its span list
    // expansion, and equals one oracle component.
    let oracle_sets: BTreeSet<BTreeSet<(i64, i64, i64)>> =
        oracle.unsupported_components.iter().cloned().collect();
    for plan in index.split_plans() {
        let cells: BTreeSet<(i64, i64, i64)> = plan.cells().map(|c| (c.x, c.y, c.z)).collect();
        assert_eq!(
            plan.cell_count as usize,
            cells.len(),
            "membership cell_count"
        );
        assert!(
            oracle_sets.contains(&cells),
            "split plan matches one oracle component exactly"
        );
    }
}

// --------------------------------------------------------------------------
// Scenario: sever a bridge across storage boundaries.
// --------------------------------------------------------------------------

#[test]
fn severing_a_cross_brick_bridge_detaches_exactly_the_unsupported_span() {
    // A 2x2 beam along X from x=-40..39 (bricks x = -2..1, four bricks), sitting
    // on a single support pillar at the far-left end down to the plane y=0.
    let mut v = terrain();
    fill(
        &mut v,
        GlobalCell::new(-40, 40, 0),
        GlobalCell::new(39, 41, 1),
        STONE,
    );
    fill(
        &mut v,
        GlobalCell::new(-40, 0, 0),
        GlobalCell::new(-39, 39, 1),
        STONE,
    );

    let mut index = build(&v, 0);
    let before = index.report();
    assert_eq!(before.components.len(), 1, "one connected structure");
    assert!(before.components[0].support.is_supported());
    assert_matches_oracle(&index, &v, 0);

    // Cut a full-section gap at x = -5..-4, inside brick x = -1. Everything to
    // the right (x = -3..39, spanning bricks -1, 0, 1) loses its only path to
    // the ground.
    let cut = EditPlan::filled_box(
        v.id(),
        GlobalCell::new(-5, 40, 0),
        GlobalCell::new(-4, 41, 1),
        MaterialId::AIR,
    );
    let outcome = v.apply_edit(&cut).expect("cut applies");

    let after = index
        .apply_edit(
            &v,
            &outcome,
            TopologyEpoch(1),
            &CancelToken::new(),
            SearchBudget::UNLIMITED,
        )
        .expect("re-analysis completes");

    assert_eq!(after.components.len(), 2, "beam split in two");
    assert_eq!(after.unsupported_ids().len(), 1, "one detached span");
    assert_eq!(after.supported_ids().len(), 1, "left stub stays anchored");
    assert!(after.is_fully_resolved(), "no unknown components");

    // The detached span crosses two storage boundaries (x = 0 and x = 32).
    let plan = &index.split_plans()[0];
    let bricks: BTreeSet<i64> = plan.cells().map(|c| c.x.div_euclid(32)).collect();
    assert!(
        bricks.len() >= 3,
        "detached span spans >= 3 bricks: {bricks:?}"
    );

    // Specific cells land on the right side.
    assert_eq!(
        index.support_of_cell(GlobalCell::new(20, 40, 0)).unwrap().1,
        Support::Unsupported
    );
    assert_eq!(
        index.support_of_cell(GlobalCell::new(-40, 5, 0)).unwrap().1,
        Support::Supported
    );
    assert!(
        index.support_of_cell(GlobalCell::new(-5, 40, 0)).is_none(),
        "the cut cell is now air"
    );

    assert_matches_oracle(&index, &v, 0);

    // Conservation balances: nothing destroyed here beyond the explicit cut,
    // which simply reduces total_solid_cells.
    let ledger = index.report().conservation().expect("fully resolved");
    ledger.check().expect("retained + child == source");
    assert_eq!(ledger.retained + ledger.child_cells, ledger.source_occupied);
}

// --------------------------------------------------------------------------
// Scenario: remove every bottom-plane support.
// --------------------------------------------------------------------------

#[test]
fn removing_the_last_anchor_cells_releases_the_remaining_solid() {
    let mut v = terrain();
    fill(
        &mut v,
        GlobalCell::new(0, 0, 0),
        GlobalCell::new(15, 20, 15),
        STONE,
    );

    let mut index = build(&v, 0);
    assert!(index.report().components[0].support.is_supported());
    assert_matches_oracle(&index, &v, 0);

    // Remove all but a single anchor cell: the block still hangs from that one
    // column, so it stays supported.
    let mut nearly = EditPlan::new(v.id());
    for z in 0..=15 {
        for x in 0..=15 {
            if (x, z) != (0, 0) {
                nearly.set(GlobalCell::new(x, 0, z), MaterialId::AIR);
            }
        }
    }
    let outcome = v.apply_edit(&nearly).expect("applies");
    let report = index
        .apply_edit(
            &v,
            &outcome,
            TopologyEpoch(1),
            &CancelToken::new(),
            SearchBudget::UNLIMITED,
        )
        .expect("completes");
    assert_eq!(
        report.unsupported_cells, 0,
        "one anchor cell still holds it"
    );
    assert!(report.components[0].support.is_supported());
    assert_matches_oracle(&index, &v, 0);

    // Remove that last anchor cell: everything above releases as one body.
    let mut last = EditPlan::new(v.id());
    last.set(GlobalCell::new(0, 0, 0), MaterialId::AIR);
    let outcome = v.apply_edit(&last).expect("applies");
    let report = index
        .apply_edit(
            &v,
            &outcome,
            TopologyEpoch(2),
            &CancelToken::new(),
            SearchBudget::UNLIMITED,
        )
        .expect("completes");

    assert_eq!(report.components.len(), 1);
    assert!(report.components[0].support.is_unsupported());
    assert_eq!(report.supported_cells, 0);
    assert_eq!(report.unsupported_cells, 16 * 20 * 16);
    assert_matches_oracle(&index, &v, 0);
}

// --------------------------------------------------------------------------
// Scenario: diagonal-only contact does not bond.
// --------------------------------------------------------------------------

#[test]
fn diagonal_and_corner_contact_never_transmits_support() {
    let mut v = terrain();
    // Block A rests on the plane y = 0.
    fill(
        &mut v,
        GlobalCell::new(0, 0, 0),
        GlobalCell::new(3, 3, 3),
        STONE,
    );
    // Block B touches A only at the corner (3,3,3)-(4,4,4): no shared face.
    fill(
        &mut v,
        GlobalCell::new(4, 4, 4),
        GlobalCell::new(7, 7, 7),
        STONE,
    );
    // Block C touches A only along an edge: adjacent in x and y, overlapping in
    // z, but kept clear of block B by a gap at z = 3.
    fill(
        &mut v,
        GlobalCell::new(4, 4, 0),
        GlobalCell::new(7, 7, 2),
        STONE,
    );

    let index = build(&v, 0);
    let report = index.report();
    assert_eq!(report.components.len(), 3, "three separate components");
    assert_eq!(report.supported_ids().len(), 1, "only block A is anchored");
    assert_eq!(report.unsupported_ids().len(), 2, "B and C float");
    assert_matches_oracle(&index, &v, 0);

    // Cross-brick diagonal: a cell at the +X face of brick 0 and a cell at the
    // -X face of brick 1 that are offset in Y do not bond.
    let mut w = terrain();
    fill(
        &mut w,
        GlobalCell::new(30, 10, 0),
        GlobalCell::new(31, 11, 0),
        STONE,
    );
    fill(
        &mut w,
        GlobalCell::new(32, 12, 0),
        GlobalCell::new(33, 13, 0),
        STONE,
    );
    let index = build(&w, -100);
    assert_eq!(
        index.report().components.len(),
        2,
        "offset boundary cells across a brick seam do not link"
    );
}

// --------------------------------------------------------------------------
// Scenario: disconnected solids in one brick stay separate.
// --------------------------------------------------------------------------

#[test]
fn two_disjoint_blobs_in_one_brick_are_two_bodies() {
    let mut v = terrain();
    fill(
        &mut v,
        GlobalCell::new(1, 1, 1),
        GlobalCell::new(4, 4, 4),
        STONE,
    );
    fill(
        &mut v,
        GlobalCell::new(20, 20, 20),
        GlobalCell::new(24, 24, 24),
        STONE,
    );

    let index = build(&v, -100); // plane far below: nothing anchored
    let report = index.report();
    assert_eq!(report.components.len(), 2);
    assert!(report.components.iter().all(|c| c.support.is_unsupported()));

    let mut sizes: Vec<u64> = report.components.iter().map(|c| c.cell_count).collect();
    sizes.sort_unstable();
    assert_eq!(sizes, vec![4 * 4 * 4, 5 * 5 * 5]);
    assert_matches_oracle(&index, &v, -100);
}

// --------------------------------------------------------------------------
// Scenario: a giant connected remainder is one body, not one per cell.
// --------------------------------------------------------------------------

#[test]
fn a_giant_connected_remainder_is_a_single_split_plan() {
    let mut v = terrain();
    // 80 x 11 x 40 solid block spanning bricks x = -2..1, z = -1..0, well above
    // the support plane.
    fill(
        &mut v,
        GlobalCell::new(-40, 50, -20),
        GlobalCell::new(39, 60, 19),
        STONE,
    );
    let cell_count: u64 = 80 * 11 * 40;

    let index = build(&v, 0);
    let plans = index.split_plans();
    assert_eq!(plans.len(), 1, "exactly one body");
    assert_eq!(plans[0].cell_count, cell_count);
    assert!(
        (plans[0].spans.len() as u64) < cell_count / 10,
        "membership is run-length encoded, not one entry per cell: {} spans for {} cells",
        plans[0].spans.len(),
        cell_count
    );
    assert_matches_oracle(&index, &v, 0);
}

// --------------------------------------------------------------------------
// Scenario: planned cell conservation vs. the dense oracle over random edits.
// --------------------------------------------------------------------------

#[test]
fn incremental_reanalysis_matches_a_full_rebuild_and_conserves_cells() {
    // A modest structure: a slab on the plane, a column, and a canopy that is
    // only held up by the column.
    let mut v = terrain();
    fill(
        &mut v,
        GlobalCell::new(-20, 0, -20),
        GlobalCell::new(19, 0, 19),
        STONE,
    ); // ground slab
    fill(
        &mut v,
        GlobalCell::new(0, 1, 0),
        GlobalCell::new(2, 30, 2),
        STONE,
    ); // column
    fill(
        &mut v,
        GlobalCell::new(-15, 31, -15),
        GlobalCell::new(16, 33, 16),
        STONE,
    ); // canopy

    let mut index = build(&v, 0);
    assert_matches_oracle(&index, &v, 0);
    assert_eq!(
        index.report().unsupported_cells,
        0,
        "canopy held by the column"
    );

    // A deterministic sequence of cuts; after each, the incremental result must
    // equal a fresh full build, and conservation must hold.
    let cuts = [
        (GlobalCell::new(0, 15, 0), GlobalCell::new(2, 16, 2)), // sever the column
        (GlobalCell::new(-20, 0, 5), GlobalCell::new(19, 0, 5)), // slice the slab
        (GlobalCell::new(-15, 32, -15), GlobalCell::new(16, 32, -8)), // nibble the canopy
    ];
    for (i, (a, b)) in cuts.into_iter().enumerate() {
        let plan = EditPlan::filled_box(v.id(), a, b, MaterialId::AIR);
        let outcome = v.apply_edit(&plan).expect("cut applies");
        let epoch = TopologyEpoch(i as u64 + 1);
        let incremental = index
            .apply_edit(
                &v,
                &outcome,
                epoch,
                &CancelToken::new(),
                SearchBudget::UNLIMITED,
            )
            .expect("completes");

        let fresh = build(&v, 0).report();
        assert_eq!(
            incremental.supported_cells, fresh.supported_cells,
            "cut {i}: supported cells match a full rebuild"
        );
        assert_eq!(
            incremental.unsupported_cells, fresh.unsupported_cells,
            "cut {i}: unsupported cells match a full rebuild"
        );
        assert_eq!(
            incremental.components.len(),
            fresh.components.len(),
            "cut {i}: component count matches a full rebuild"
        );
        assert_matches_oracle(&index, &v, 0);

        if let Some(ledger) = incremental.conservation() {
            ledger.check().unwrap_or_else(|e| panic!("cut {i}: {e}"));
        }
    }
}

// --------------------------------------------------------------------------
// Scenario: the shared spall_voxel `cross_brick_bridge` fixture.
// --------------------------------------------------------------------------

#[test]
fn the_shared_cross_brick_bridge_fixture_detaches_as_one_span_when_undermined() {
    // The fixture beam runs x = -40..39 (bricks x = -2..1) at y = 0..1, z = 0..1
    // with no support of its own. Give it one pillar at the left end, analyse,
    // then cut the pillar: the whole four-brick beam detaches as one body.
    let mut v = spall_voxel::fixtures::cross_brick_bridge(vid());
    // Pillar from the beam down to the plane at y = -20.
    fill(
        &mut v,
        GlobalCell::new(-40, -20, 0),
        GlobalCell::new(-39, -1, 1),
        STONE,
    );

    let mut index = build(&v, -20);
    assert_eq!(index.report().components.len(), 1);
    assert!(index.report().components[0].support.is_supported());
    assert_matches_oracle(&index, &v, -20);

    let cut = EditPlan::filled_box(
        v.id(),
        GlobalCell::new(-40, -10, 0),
        GlobalCell::new(-39, -9, 1),
        MaterialId::AIR,
    );
    let outcome = v.apply_edit(&cut).expect("cut applies");
    let report = index
        .apply_edit(
            &v,
            &outcome,
            TopologyEpoch(1),
            &CancelToken::new(),
            SearchBudget::UNLIMITED,
        )
        .expect("completes");

    let spans: Vec<i64> = index.split_plans()[0]
        .cells()
        .map(|c| c.x.div_euclid(32))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    assert_eq!(report.unsupported_ids().len(), 1, "one detached beam");
    assert_eq!(
        spans,
        vec![-2, -1, 0, 1],
        "detached beam spans all four bricks"
    );
    assert_matches_oracle(&index, &v, -20);
}

// --------------------------------------------------------------------------
// Property: incremental analysis equals a full rebuild equals the oracle,
// over a deterministic pseudo-random sequence of structures and cuts.
// --------------------------------------------------------------------------

#[test]
fn randomised_edits_stay_consistent_with_a_full_rebuild_and_the_oracle() {
    // A tiny SplitMix64, matching spall_voxel's no-rand-dependency convention.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn range(&mut self, lo: i64, hi: i64) -> i64 {
            lo + (self.next() % (hi - lo + 1) as u64) as i64
        }
    }

    for seed in 0..6u64 {
        let mut rng = Rng(seed.wrapping_mul(0xD1B5_4A32_D192_ED03).wrapping_add(1));
        let mut v = terrain();

        // Seed structure: 2-4 overlapping boxes near the origin, some touching
        // the plane y = 0.
        let boxes = rng.range(2, 4);
        for _ in 0..boxes {
            let x0 = rng.range(-20, 10);
            let y0 = rng.range(0, 10);
            let z0 = rng.range(-20, 10);
            let a = GlobalCell::new(x0, y0, z0);
            let b = GlobalCell::new(
                x0 + rng.range(2, 18),
                y0 + rng.range(2, 18),
                z0 + rng.range(2, 18),
            );
            fill(&mut v, a, b, STONE);
        }

        let mut index = build(&v, 0);
        assert_matches_oracle(&index, &v, 0);

        for _ in 0..4 {
            let x0 = rng.range(-24, 24);
            let y0 = rng.range(0, 24);
            let z0 = rng.range(-24, 24);
            let plan = EditPlan::filled_box(
                v.id(),
                GlobalCell::new(x0, y0, z0),
                GlobalCell::new(
                    x0 + rng.range(0, 6),
                    y0 + rng.range(0, 6),
                    z0 + rng.range(0, 6),
                ),
                MaterialId::AIR,
            );
            let outcome = v.apply_edit(&plan).expect("cut applies");
            if outcome.bricks.is_empty() {
                continue; // the cut hit only air
            }
            let incremental = index
                .apply_edit(
                    &v,
                    &outcome,
                    TopologyEpoch::START,
                    &CancelToken::new(),
                    SearchBudget::UNLIMITED,
                )
                .expect("completes");

            let fresh = build(&v, 0).report();
            assert_eq!(
                incremental, fresh,
                "seed {seed}: incremental report diverged from a full rebuild"
            );
            assert_matches_oracle(&index, &v, 0);
            if let Some(ledger) = incremental.conservation() {
                ledger.check().expect("conservation holds");
            }
        }
    }
}

// --------------------------------------------------------------------------
// Scenario: an unavailable dependency is Unknown, never air or a permanent
// anchor.
// --------------------------------------------------------------------------

#[test]
fn connectivity_into_an_unresident_brick_is_unknown() {
    // A column that reaches the -X face of brick x = 0 and would continue into
    // brick x = -1, which is never loaded. The column does not touch the plane.
    // Streamed residency: an absent neighbour is a pending dependency.
    let mut v = terrain();
    fill(
        &mut v,
        GlobalCell::new(0, 10, 0),
        GlobalCell::new(5, 14, 0),
        STONE,
    );

    let index = build_mode(&v, 0, ResidencyMode::Streamed);
    let report = index.report();
    assert_eq!(report.components.len(), 1);
    match &report.components[0].support {
        Support::Unknown { missing } => {
            assert!(
                missing.contains(&spall_core::BrickCoord::new(-1, 0, 0)),
                "the missing dependency names the unresident brick: {missing:?}"
            );
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
    assert!(!report.is_fully_resolved());
    assert!(
        report.conservation().is_none(),
        "no split ledger while a component is Unknown"
    );

    // Loading the brick as solid-and-grounded resolves it to Supported.
    let mut v2 = v.clone();
    fill(
        &mut v2,
        GlobalCell::new(-1, 0, 0),
        GlobalCell::new(-1, 14, 0),
        STONE,
    );
    let index2 = build_mode(&v2, 0, ResidencyMode::Streamed);
    assert!(index2.report().components[0].support.is_supported());
    assert_matches_oracle(&index2, &v2, 0);
}

// --------------------------------------------------------------------------
// Revision / cancellation validation.
// --------------------------------------------------------------------------

fn world_for(v: &Volume, generation: Generation, epoch: TopologyEpoch) -> MapWorld {
    let mut world = MapWorld::new(generation).with_epoch(epoch);
    for coord in v.resident_brick_coords() {
        if let Ok(Some(rev)) = v.brick_revision(coord) {
            world.set_brick(v.id(), coord, rev);
        }
    }
    world
}

#[test]
fn the_analysis_token_tracks_the_brick_revisions_it_read() {
    let mut v = terrain();
    fill(
        &mut v,
        GlobalCell::new(0, 0, 0),
        GlobalCell::new(10, 10, 10),
        STONE,
    );

    let index = build(&v, 0);
    let world = world_for(&v, Generation(1), TopologyEpoch::START);
    assert_eq!(index.validate(&world), Staleness::Fresh);

    // A newer generation invalidates the whole analysis.
    let mut reloaded = world.clone();
    reloaded.set_generation(Generation(2));
    assert!(matches!(
        index.validate(&reloaded),
        Staleness::Generation { .. }
    ));

    // A brick moving to a new revision invalidates it too.
    let mut edited = world.clone();
    edited.set_brick(v.id(), spall_core::BrickCoord::new(0, 0, 0), Revision(999));
    assert!(matches!(
        index.validate(&edited),
        Staleness::BrickRevision { .. }
    ));
}

#[test]
fn boundary_dependency_states_invalidate_when_their_meaning_changes() {
    let boundary = spall_core::BrickCoord::new(1, 0, 0);

    // An all-resident analysis may classify an absent neighbour as empty, but
    // it still read that absence. If a brick subsequently arrives there, the
    // unsupported split can no longer be installed.
    let mut absent = terrain();
    fill(
        &mut absent,
        GlobalCell::new(31, 10, 0),
        GlobalCell::new(31, 10, 0),
        STONE,
    );
    let absent_index = build(&absent, 0);
    let mut absent_world = world_for(&absent, Generation(1), TopologyEpoch::START);
    assert_eq!(absent_index.validate(&absent_world), Staleness::Fresh);
    absent_world.set_brick(absent.id(), boundary, Revision(7));
    assert!(matches!(
        absent_index.validate(&absent_world),
        Staleness::BrickRevision {
            expected: spall_jobs::DepState::Absent,
            current: spall_jobs::BrickStatus::Resident(Revision(7)),
            ..
        }
    ));

    // Failed is a distinct state: it leaves the component Unknown, remains
    // valid while the same failure persists, and becomes stale after a retry.
    let mut failed = terrain();
    fill(
        &mut failed,
        GlobalCell::new(31, 10, 0),
        GlobalCell::new(31, 10, 0),
        STONE,
    );
    failed.mark_failed(boundary).unwrap();
    let failed_index = build(&failed, 0);
    let mut failed_world = world_for(&failed, Generation(1), TopologyEpoch::START);
    failed_world.fail_brick(failed.id(), boundary);
    assert_eq!(failed_index.validate(&failed_world), Staleness::Fresh);
    failed_world.set_brick(failed.id(), boundary, Revision(8));
    assert!(matches!(
        failed_index.validate(&failed_world),
        Staleness::BrickRevision {
            expected: spall_jobs::DepState::Failed,
            current: spall_jobs::BrickStatus::Resident(Revision(8)),
            ..
        }
    ));
}

#[test]
fn apply_edit_refreshes_the_token_to_the_post_edit_revisions() {
    let mut v = terrain();
    fill(
        &mut v,
        GlobalCell::new(0, 0, 0),
        GlobalCell::new(10, 10, 10),
        STONE,
    );
    let mut index = build(&v, 0);

    let cut = EditPlan::filled_box(
        v.id(),
        GlobalCell::new(3, 3, 3),
        GlobalCell::new(4, 4, 4),
        MaterialId::AIR,
    );
    let outcome = v.apply_edit(&cut).expect("applies");
    index
        .apply_edit(
            &v,
            &outcome,
            TopologyEpoch(1),
            &CancelToken::new(),
            SearchBudget::UNLIMITED,
        )
        .expect("completes");

    // Stale against the pre-edit world…
    let stale = world_for_pre_edit(&v, &outcome);
    assert!(!matches!(index.validate(&stale), Staleness::Fresh));
    // …fresh against the post-edit world at the epoch the edit moved to.
    let fresh = world_for(&v, Generation(1), TopologyEpoch(1));
    assert_eq!(index.validate(&fresh), Staleness::Fresh);
}

/// A world view stamped with each brick's revision *before* `outcome` bumped it,
/// at the post-edit epoch (so only the brick revision is stale).
fn world_for_pre_edit(v: &Volume, outcome: &spall_voxel::EditOutcome) -> MapWorld {
    let mut world = world_for(v, Generation(1), TopologyEpoch(1));
    for rec in &outcome.bricks {
        world.set_brick(v.id(), rec.coord, rec.before_revision);
    }
    world
}

#[test]
fn a_cancelled_token_stops_the_build_and_the_reanalysis() {
    let mut v = terrain();
    fill(
        &mut v,
        GlobalCell::new(0, 0, 0),
        GlobalCell::new(40, 40, 40),
        STONE,
    );

    let cancel = CancelToken::new();
    cancel.cancel();
    assert_eq!(
        SupportGraph::build(&v, AnchorPlane::at(0), ResidencyMode::AllResident, &cancel)
            .unwrap_err(),
        Interrupted::Cancelled
    );

    let mut index = build(&v, 0);
    let cut = EditPlan::filled_box(
        v.id(),
        GlobalCell::new(1, 1, 1),
        GlobalCell::new(2, 2, 2),
        MaterialId::AIR,
    );
    let outcome = v.apply_edit(&cut).expect("applies");
    let cancel = CancelToken::new();
    cancel.cancel();
    assert_eq!(
        index
            .apply_edit(
                &v,
                &outcome,
                TopologyEpoch(1),
                &cancel,
                SearchBudget::UNLIMITED
            )
            .unwrap_err(),
        Interrupted::Cancelled
    );
}

// --------------------------------------------------------------------------
// Budget / resume: a tiny budget interrupts the component scan; resuming to
// completion yields the same report as an unbounded pass.
// --------------------------------------------------------------------------

#[test]
fn a_bounded_search_is_resumable_to_the_same_result() {
    let mut v = terrain();
    // Three disjoint blobs -> three graph nodes -> a budget of 1 node/call
    // forces at least two interruptions.
    fill(
        &mut v,
        GlobalCell::new(0, 0, 0),
        GlobalCell::new(2, 2, 2),
        STONE,
    );
    fill(
        &mut v,
        GlobalCell::new(10, 10, 10),
        GlobalCell::new(12, 12, 12),
        STONE,
    );
    fill(
        &mut v,
        GlobalCell::new(20, 20, 20),
        GlobalCell::new(22, 22, 22),
        STONE,
    );

    let mut index = build(&v, -100);
    let unbounded = index.report();

    // Re-run the analysis under a starvation budget via a no-op edit that
    // touches every brick.
    let touch = EditPlan::filled_box(
        v.id(),
        GlobalCell::new(0, 0, 0),
        GlobalCell::new(0, 0, 0),
        STONE,
    );
    let outcome = v.apply_edit(&touch).expect("applies");

    let tiny = SearchBudget::new(1);
    let cancel = CancelToken::new();

    let mut outcome_result: Result<(), Interrupted> = index
        .apply_edit(&v, &outcome, TopologyEpoch(1), &cancel, tiny)
        .map(|_| ());
    let mut rounds = 0;
    while let Err(interrupt) = outcome_result {
        assert_eq!(interrupt, Interrupted::Budget, "nothing was cancelled");
        assert!(index.is_pending());
        rounds += 1;
        assert!(rounds < 100, "resume made no progress");
        outcome_result = index.resume(&cancel, tiny);
    }
    assert!(rounds >= 2, "the tiny budget interrupted at least twice");
    assert!(!index.is_pending());

    let resumed = index.report();
    assert_eq!(resumed.components.len(), unbounded.components.len());
    assert_eq!(resumed.unsupported_cells, unbounded.unsupported_cells);
    assert_eq!(resumed.supported_cells, unbounded.supported_cells);
}
