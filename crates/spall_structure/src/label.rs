//! Per-brick local connected-component labelling.
//!
//! Within one `32 x 32 x 32` brick, solid cells (material != air) are grouped
//! into six-face-connected components. Corner and edge contact never bonds two
//! cells — only a shared face does. A brick can hold any number of disjoint
//! components; do not assume its solid cells form one blob.
//!
//! Labels are assigned by a deterministic flood fill that scans cells in
//! canonical linear-index order (`x` fastest, then `y`, then `z`), so the same
//! brick contents always produce the same labelling. Label `0` means
//! air / empty; solid cells receive labels `1..=count`.

use spall_core::{BRICK_EDGE, CELLS_PER_BRICK, LocalCell};
use spall_voxel::BrickSnapshot;

const EDGE: usize = BRICK_EDGE as usize;

/// A local component identifier inside one brick. `LocalComponent(0)` is never a
/// real component; it is the "air / no component" sentinel used inside
/// [`BrickLabels`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocalComponent(pub u16);

impl LocalComponent {
    /// The sentinel stored for a non-solid cell.
    pub const NONE: Self = Self(0);

    #[inline]
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// The connected-component labelling of one brick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrickLabels {
    /// One label per cell, indexed by [`LocalCell::linear_index`]. `0` == air.
    labels: Box<[u16]>,
    /// Number of distinct solid components (labels `1..=count`).
    count: u16,
}

impl BrickLabels {
    /// The number of distinct solid components in this brick.
    #[inline]
    pub fn count(&self) -> u16 {
        self.count
    }

    /// `true` when the brick has no solid cell.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The component a cell belongs to, or `None` for air.
    #[inline]
    pub fn component_at(&self, cell: LocalCell) -> Option<LocalComponent> {
        match self.labels[cell.linear_index() as usize] {
            0 => None,
            n => Some(LocalComponent(n)),
        }
    }

    /// Raw label at a linear index (`0` == air). Panics if `index >= 32768`.
    #[inline]
    pub fn label_at_index(&self, index: usize) -> u16 {
        self.labels[index]
    }

    /// Number of cells in one component.
    pub fn cell_count(&self, component: LocalComponent) -> u32 {
        self.labels.iter().filter(|&&l| l == component.0).count() as u32
    }

    /// Every cell belonging to `component`, in canonical linear-index order.
    pub fn cells(&self, component: LocalComponent) -> Vec<LocalCell> {
        let mut out = Vec::new();
        for (index, &label) in self.labels.iter().enumerate() {
            if label == component.0 {
                out.push(
                    LocalCell::from_linear_index(index as u16)
                        .expect("index derived from a 0..32768 enumeration"),
                );
            }
        }
        out
    }

    /// Iterator over `1..=count` component ids.
    pub fn components(&self) -> impl Iterator<Item = LocalComponent> + '_ {
        (1..=self.count).map(LocalComponent)
    }
}

#[inline]
fn idx(x: usize, y: usize, z: usize) -> usize {
    x + EDGE * (y + EDGE * z)
}

/// Six-face-connected component labelling of `brick`.
pub fn label_brick(brick: &BrickSnapshot) -> BrickLabels {
    // Solid mask first, so the flood fill never re-reads the snapshot.
    let mut solid = vec![false; CELLS_PER_BRICK];
    for z in 0..EDGE {
        for y in 0..EDGE {
            for x in 0..EDGE {
                let cell = LocalCell::new(x as u8, y as u8, z as u8).expect("x,y,z < 32");
                solid[idx(x, y, z)] = !brick.get(cell).is_air();
            }
        }
    }

    let mut labels = vec![0u16; CELLS_PER_BRICK];
    let mut count: u16 = 0;
    let mut stack: Vec<usize> = Vec::new();

    for start in 0..CELLS_PER_BRICK {
        if !solid[start] || labels[start] != 0 {
            continue;
        }
        count = count.checked_add(1).expect("<= 32768 components per brick");
        let label = count;
        labels[start] = label;
        stack.push(start);

        while let Some(here) = stack.pop() {
            let x = here % EDGE;
            let y = (here / EDGE) % EDGE;
            let z = here / (EDGE * EDGE);

            // Six face neighbours, visited in a fixed order.
            let mut visit = |nx: usize, ny: usize, nz: usize, stack: &mut Vec<usize>| {
                let n = idx(nx, ny, nz);
                if solid[n] && labels[n] == 0 {
                    labels[n] = label;
                    stack.push(n);
                }
            };
            if x > 0 {
                visit(x - 1, y, z, &mut stack);
            }
            if x + 1 < EDGE {
                visit(x + 1, y, z, &mut stack);
            }
            if y > 0 {
                visit(x, y - 1, z, &mut stack);
            }
            if y + 1 < EDGE {
                visit(x, y + 1, z, &mut stack);
            }
            if z > 0 {
                visit(x, y, z - 1, &mut stack);
            }
            if z + 1 < EDGE {
                visit(x, y, z + 1, &mut stack);
            }
        }
    }

    BrickLabels {
        labels: labels.into_boxed_slice(),
        count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{CellSizeCode, GlobalCell};
    use spall_core::{MaterialId, Revision, VolumeId};
    use spall_voxel::{Brick, EditPlan, Volume};

    const STONE: MaterialId = MaterialId(1);

    fn brick_from(writes: &[(u8, u8, u8)]) -> BrickSnapshot {
        let mut b = Brick::uniform(MaterialId::AIR, Revision(1));
        for &(x, y, z) in writes {
            b.set_cell(LocalCell::new(x, y, z).unwrap(), STONE);
        }
        b.collapse();
        b.snapshot()
    }

    #[test]
    fn empty_brick_has_no_components() {
        let b = Brick::uniform(MaterialId::AIR, Revision(1)).snapshot();
        let labels = label_brick(&b);
        assert!(labels.is_empty());
        assert_eq!(labels.count(), 0);
    }

    #[test]
    fn a_solid_brick_is_one_component() {
        let b = Brick::uniform(STONE, Revision(1)).snapshot();
        let labels = label_brick(&b);
        assert_eq!(labels.count(), 1);
        assert_eq!(labels.cell_count(LocalComponent(1)), CELLS_PER_BRICK as u32);
    }

    #[test]
    fn face_adjacent_cells_bond_but_diagonal_ones_do_not() {
        // Two cells sharing a face.
        let face = brick_from(&[(0, 0, 0), (1, 0, 0)]);
        assert_eq!(label_brick(&face).count(), 1);

        // Two cells sharing only an edge (x+1, y+1).
        let edge = brick_from(&[(0, 0, 0), (1, 1, 0)]);
        assert_eq!(label_brick(&edge).count(), 2);

        // Two cells sharing only a corner (x+1, y+1, z+1).
        let corner = brick_from(&[(0, 0, 0), (1, 1, 1)]);
        assert_eq!(label_brick(&corner).count(), 2);
    }

    #[test]
    fn two_separated_blobs_in_one_brick_stay_separate() {
        let two = brick_from(&[
            (0, 0, 0),
            (0, 1, 0),
            (0, 0, 1),
            // gap
            (10, 10, 10),
            (10, 11, 10),
        ]);
        let labels = label_brick(&two);
        assert_eq!(labels.count(), 2);
        assert_eq!(labels.cell_count(LocalComponent(1)), 3);
        assert_eq!(labels.cell_count(LocalComponent(2)), 2);
        // Membership is disjoint and covers exactly the written cells.
        let all: usize = labels
            .components()
            .map(|c| labels.cell_count(c) as usize)
            .sum();
        assert_eq!(all, 5);
    }

    #[test]
    fn labelling_is_deterministic_and_canonically_seeded() {
        // The first solid cell in linear order seeds component 1.
        let b = brick_from(&[(5, 0, 0), (4, 0, 0), (31, 31, 31)]);
        let labels = label_brick(&b);
        assert_eq!(labels.count(), 2);
        assert_eq!(
            labels.component_at(LocalCell::new(4, 0, 0).unwrap()),
            Some(LocalComponent(1))
        );
        assert_eq!(
            labels.component_at(LocalCell::new(31, 31, 31).unwrap()),
            Some(LocalComponent(2))
        );
    }

    #[test]
    fn matches_a_hollow_shell_from_the_voxel_fixtures() {
        // A hollow box inside a single brick: the shell is one component,
        // the interior void is not solid at all.
        let mut v = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
        let shell = EditPlan::filled_box(
            v.id(),
            GlobalCell::new(2, 2, 2),
            GlobalCell::new(20, 20, 20),
            STONE,
        );
        v.apply_edit(&shell).unwrap();
        let hollow = EditPlan::filled_box(
            v.id(),
            GlobalCell::new(5, 5, 5),
            GlobalCell::new(17, 17, 17),
            MaterialId::AIR,
        );
        v.apply_edit(&hollow).unwrap();

        let snap = v
            .snapshot_brick(spall_core::BrickCoord::new(0, 0, 0))
            .unwrap()
            .unwrap();
        let labels = label_brick(&snap);
        assert_eq!(
            labels.count(),
            1,
            "the shell is a single connected component"
        );
    }
}
