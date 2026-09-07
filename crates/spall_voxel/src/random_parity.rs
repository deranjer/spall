//! Randomized differential test: a sequence of random edits applied to a
//! [`Volume`] must agree cell-for-cell with the [`DenseOracle`], and the
//! volume's own accounting must stay self-consistent.

use spall_core::{BrickCoord, CellSizeCode, GlobalCell, LocalCell, MaterialId, VolumeId};

use crate::edit::{CellEdit, EditPlan};
use crate::oracle::DenseOracle;
use crate::volume::{Sample, Volume};

/// SplitMix64 — deterministic, no dependency.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: i64) -> i64 {
        (self.next_u64() % n as u64) as i64
    }
}

fn run(seed: u64, span: i64, materials: u16, edits: usize, writes_per_edit: usize) {
    let mut rng = Rng(seed);
    let mut volume = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
    let mut oracle = DenseOracle::new();

    for _ in 0..edits {
        let mut plan = EditPlan::new(volume.id());
        for _ in 0..writes_per_edit {
            let cell = GlobalCell::new(
                rng.below(span) - span / 2,
                rng.below(span) - span / 2,
                rng.below(span) - span / 2,
            );
            // Bias toward air so bricks churn between dense and uniform.
            let material = match rng.next_u64() % 4 {
                0 => MaterialId::AIR,
                _ => MaterialId((rng.next_u64() % materials as u64) as u16 + 1),
            };
            plan.set(cell, material);
        }
        oracle.apply(&plan.writes);

        let outcome = volume.apply_edit(&plan).unwrap();

        // Every record's after-hash must match a re-read of the brick.
        for record in &outcome.bricks {
            let snap = volume
                .snapshot_brick(record.coord)
                .unwrap()
                .expect("edited brick is resident");
            assert_eq!(snap.content_hash(), record.after_hash);
            assert_eq!(snap.revision(), record.after_revision);
            if record.now_modified_air {
                assert!(brick_is_all_air(&volume, record.coord));
            }
        }
    }

    // Full cell-for-cell agreement over the whole touched region.
    for z in (-span / 2)..(span / 2) {
        for y in (-span / 2)..(span / 2) {
            for x in (-span / 2)..(span / 2) {
                let cell = GlobalCell::new(x, y, z);
                let expected = oracle.material_at(cell);
                match volume.sample(cell).unwrap() {
                    Sample::Filled(m) => assert_eq!(m, expected, "solid at {cell:?}"),
                    Sample::Empty { .. } => {
                        assert!(expected.is_air(), "expected air at {cell:?}")
                    }
                    Sample::Unknown(_) => {
                        // Only cells in never-touched bricks stay unknown.
                        assert!(
                            !oracle.brick_edited(cell.split().0),
                            "touched brick went missing at {cell:?}"
                        );
                        assert!(expected.is_air());
                    }
                }
            }
        }
    }

    // Accounting: dense payload bytes are a multiple of the layer size and the
    // owned/shared split covers every dense brick.
    let report = volume.memory_report();
    assert_eq!(
        report.dense_bricks,
        report.snapshot_shared_bricks + (report.owned_dense_bytes / 65_536)
    );
    assert_eq!(report.snapshot_shared_bytes % 65_536, 0);
}

fn brick_is_all_air(volume: &Volume, coord: BrickCoord) -> bool {
    (0..32u8).all(|z| {
        (0..32u8).all(|y| {
            (0..32u8).all(|x| {
                let local = LocalCell::new(x, y, z).unwrap();
                let global = GlobalCell::from_parts(coord, local).unwrap();
                matches!(volume.sample(global), Ok(Sample::Empty { .. }))
            })
        })
    })
}

#[test]
fn random_edits_track_a_dense_reference_model() {
    run(0x1234_5678, 40, 5, 50, 12);
    run(0xDEAD_BEEF, 20, 3, 90, 8);
    run(0x0000_0001, 64, 8, 25, 32);
}

#[test]
fn uniform_transitions_preserve_untouched_cells() {
    // Fill one brick uniform, punch a hole, refill it: the rest is intact and
    // storage returns to uniform.
    let mut volume = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
    let mut fill = EditPlan::new(volume.id());
    for z in 0..32 {
        for y in 0..32 {
            for x in 0..32 {
                fill.set(GlobalCell::new(x, y, z), MaterialId(2));
            }
        }
    }
    volume.apply_edit(&fill).unwrap();

    let mut hole = EditPlan::new(volume.id());
    hole.writes.push(CellEdit {
        cell: GlobalCell::new(5, 6, 7),
        material: MaterialId::AIR,
    });
    volume.apply_edit(&hole).unwrap();
    assert_eq!(
        volume.sample(GlobalCell::new(5, 6, 7)).unwrap(),
        Sample::Empty { modified: true }
    );
    assert_eq!(
        volume.sample(GlobalCell::new(0, 0, 0)).unwrap(),
        Sample::Filled(MaterialId(2))
    );

    let mut refill = EditPlan::new(volume.id());
    refill.writes.push(CellEdit {
        cell: GlobalCell::new(5, 6, 7),
        material: MaterialId(2),
    });
    volume.apply_edit(&refill).unwrap();
    assert_eq!(volume.memory_report().dense_bricks, 0);
    assert_eq!(volume.memory_report().uniform_bricks, 1);
}
