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

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use spall_core::{BRICK_EDGE, CELLS_PER_BRICK, LocalCell, Revision, VolumeId};
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

/// How one brick's labels are stored. Most bricks are uniform rock or a single
/// connected surface layer, so a per-cell `u16` array (64 KiB) is the exception.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Repr {
    /// No solid cell.
    Empty,
    /// Every cell solid, one component (label 1).
    Full,
    /// Exactly one component (label 1): the solid cells are the set bits.
    Single(Box<[u64]>),
    /// General case: one label per cell, indexed by `LocalCell::linear_index`.
    Many(Box<[u16]>),
}

/// What the graph needs to know about one component without scanning its cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ComponentInfo {
    cell_count: u32,
    /// Whether any cell touches the `-x, +x, -y, +y, -z, +z` face of the brick.
    faces: [bool; 6],
    /// Bit `y` is set when the component has a cell at local height `y`.
    layers: u32,
}

/// The connected-component labelling of one brick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrickLabels {
    repr: Repr,
    /// Number of distinct solid components (labels `1..=count`).
    count: u16,
    /// Per component, in label order (`info[label - 1]`).
    info: Vec<ComponentInfo>,
    /// Which boundary cells are solid, per face (`-x, +x, -y, +y, -z, +z`): row
    /// `v`, bit `u` is the cell at face coordinates `(u, v)` — `(y, z)` on the
    /// x faces, `(x, z)` on the y faces, `(x, y)` on the z faces. Opposite faces
    /// of neighbouring bricks use the same `(u, v)`, so linking two bricks
    /// whose bricks are single components is a few dozen `AND`s.
    face_masks: [[u32; EDGE]; 6],
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

    /// Raw label at a linear index (`0` == air). Panics if `index >= 32768`.
    #[inline]
    pub fn label_at_index(&self, index: usize) -> u16 {
        assert!(index < CELLS_PER_BRICK, "cell index out of range");
        match &self.repr {
            Repr::Empty => 0,
            Repr::Full => 1,
            Repr::Single(bits) => u16::from(bits[index / 64] >> (index % 64) & 1 == 1),
            Repr::Many(labels) => labels[index],
        }
    }

    /// The component a cell belongs to, or `None` for air.
    #[inline]
    pub fn component_at(&self, cell: LocalCell) -> Option<LocalComponent> {
        match self.label_at_index(cell.linear_index() as usize) {
            0 => None,
            n => Some(LocalComponent(n)),
        }
    }

    fn info_of(&self, component: LocalComponent) -> Option<&ComponentInfo> {
        usize::from(component.0)
            .checked_sub(1)
            .and_then(|i| self.info.get(i))
    }

    /// Number of cells in one component (`0` for an id that does not exist).
    pub fn cell_count(&self, component: LocalComponent) -> u32 {
        self.info_of(component).map_or(0, |i| i.cell_count)
    }

    /// Whether `component` has a cell on each brick face, in the order
    /// `-x, +x, -y, +y, -z, +z`.
    pub fn faces(&self, component: LocalComponent) -> [bool; 6] {
        self.info_of(component).map_or([false; 6], |i| i.faces)
    }

    /// Whether `component` has a cell at brick-local height `y` (`0..32`).
    pub fn has_layer(&self, component: LocalComponent, y: u8) -> bool {
        y < BRICK_EDGE as u8
            && self
                .info_of(component)
                .is_some_and(|i| i.layers >> y & 1 == 1)
    }

    /// The solid boundary cells of one face (see the field doc), `face` in
    /// `0..6` as `-x, +x, -y, +y, -z, +z`.
    pub fn face_mask(&self, face: usize) -> &[u32; EDGE] {
        &self.face_masks[face]
    }

    /// `true` when every cell is solid and belongs to one component, regardless
    /// of material diversity: its boundary matches any solid boundary cell.
    pub fn is_full(&self) -> bool {
        matches!(self.repr, Repr::Full)
    }

    /// Every cell belonging to `component`, in canonical linear-index order.
    pub fn cells(&self, component: LocalComponent) -> Vec<LocalCell> {
        let mut out = Vec::new();
        if self.info_of(component).is_none() {
            return out;
        }
        for index in 0..CELLS_PER_BRICK {
            if self.label_at_index(index) == component.0 {
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

    /// Heap bytes held (diagnostics).
    pub fn heap_bytes(&self) -> usize {
        let labels = match &self.repr {
            Repr::Empty | Repr::Full => 0,
            Repr::Single(bits) => bits.len() * 8,
            Repr::Many(labels) => labels.len() * 2,
        };
        labels + self.info.len() * std::mem::size_of::<ComponentInfo>()
    }
}

#[inline]
fn idx(x: usize, y: usize, z: usize) -> usize {
    x + EDGE * (y + EDGE * z)
}

/// Six-face-connected component labelling of `brick`.
pub fn label_brick(brick: &BrickSnapshot) -> BrickLabels {
    // Connectivity depends on occupancy, not material uniformity. The exact
    // memoized count also proves mixed-material bricks wholly full or empty.
    let solid_cells = brick.solid_cells();
    if solid_cells == 0 || solid_cells as usize == CELLS_PER_BRICK {
        if solid_cells == 0 {
            return BrickLabels {
                repr: Repr::Empty,
                count: 0,
                info: Vec::new(),
                face_masks: [[0; EDGE]; 6],
            };
        }
        return BrickLabels {
            repr: Repr::Full,
            count: 1,
            info: vec![ComponentInfo {
                cell_count: CELLS_PER_BRICK as u32,
                faces: [true; 6],
                layers: u32::MAX,
            }],
            face_masks: [[u32::MAX; EDGE]; 6],
        };
    }

    label_partial_brick(brick)
}

/// General six-face flood fill, also retained as the exact test reference for
/// the count-proven full/empty shortcut.
fn label_partial_brick(brick: &BrickSnapshot) -> BrickLabels {
    // Solid mask first, so the flood fill never re-reads the snapshot.
    let solid: Vec<_> = brick
        .material_cells()
        .into_iter()
        .map(|m| !m.is_air())
        .collect();

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

    // Per-component facts, so graph assembly never rescans cells.
    let mut info = vec![
        ComponentInfo {
            cell_count: 0,
            faces: [false; 6],
            layers: 0,
        };
        usize::from(count)
    ];
    for (index, &label) in labels.iter().enumerate() {
        if label == 0 {
            continue;
        }
        let (x, y, z) = (index % EDGE, (index / EDGE) % EDGE, index / (EDGE * EDGE));
        let c = &mut info[usize::from(label) - 1];
        c.cell_count += 1;
        c.layers |= 1 << y;
        c.faces[0] |= x == 0;
        c.faces[1] |= x == EDGE - 1;
        c.faces[2] |= y == 0;
        c.faces[3] |= y == EDGE - 1;
        c.faces[4] |= z == 0;
        c.faces[5] |= z == EDGE - 1;
    }

    // Solid boundary cells per face, from the label array.
    let mut face_masks = [[0u32; EDGE]; 6];
    for (index, &label) in labels.iter().enumerate() {
        if label == 0 {
            continue;
        }
        let (x, y, z) = (index % EDGE, (index / EDGE) % EDGE, index / (EDGE * EDGE));
        if x == 0 {
            face_masks[0][z] |= 1 << y;
        }
        if x == EDGE - 1 {
            face_masks[1][z] |= 1 << y;
        }
        if y == 0 {
            face_masks[2][z] |= 1 << x;
        }
        if y == EDGE - 1 {
            face_masks[3][z] |= 1 << x;
        }
        if z == 0 {
            face_masks[4][y] |= 1 << x;
        }
        if z == EDGE - 1 {
            face_masks[5][y] |= 1 << x;
        }
    }
    let repr = match count {
        0 => Repr::Empty,
        1 => {
            // One component: a bit mask is 16x smaller than the label array.
            let mut bits = vec![0u64; CELLS_PER_BRICK / 64];
            for (index, &label) in labels.iter().enumerate() {
                if label != 0 {
                    bits[index / 64] |= 1 << (index % 64);
                }
            }
            if info[0].cell_count as usize == CELLS_PER_BRICK {
                Repr::Full
            } else {
                Repr::Single(bits.into_boxed_slice())
            }
        }
        _ => Repr::Many(labels.into_boxed_slice()),
    };
    BrickLabels {
        repr,
        count,
        info,
        face_masks,
    }
}

/// `(volume id, brick x, y, z)` to the brick's revision and its labels.
type CacheMap = BTreeMap<(u64, i64, i64, i64), (Revision, Arc<BrickLabels>)>;

/// Memoized brick labellings of one or more volumes, shared between staging
/// passes. Labels are a pure function of a brick's cells, so an entry is reused
/// while the brick's revision is unchanged: building the structural index of a
/// large world then relabels only the bricks an edit touched, not all of them.
///
/// Entries are written only from builds over the *live* volume, never from a
/// dry run: a revision number identifies a brick's contents along one history,
/// and a discarded dry run is a history that never happened.
#[derive(Debug, Clone, Default)]
pub struct LabelCache(Arc<Mutex<CacheState>>);

#[derive(Debug, Default)]
struct CacheState {
    entries: CacheMap,
    empty: Option<Arc<BrickLabels>>,
    full: Option<Arc<BrickLabels>>,
}

impl LabelCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CacheState> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn get(
        &self,
        volume: VolumeId,
        key: (i64, i64, i64),
        revision: Revision,
    ) -> Option<Arc<BrickLabels>> {
        let entry = self
            .lock()
            .entries
            .get(&(volume.get(), key.0, key.1, key.2))
            .cloned()?;
        (entry.0 == revision).then_some(entry.1)
    }

    pub(crate) fn put(
        &self,
        volume: VolumeId,
        key: (i64, i64, i64),
        revision: Revision,
        labels: Arc<BrickLabels>,
    ) -> Arc<BrickLabels> {
        let mut state = self.lock();
        // Empty and completely solid bricks have identical connectivity,
        // component metadata and face masks regardless of their materials.
        // Share these two immutable shapes within this cache, not globally.
        let labels = if matches!(&labels.repr, Repr::Empty) {
            state.empty.get_or_insert(labels).clone()
        } else if matches!(&labels.repr, Repr::Full) {
            state.full.get_or_insert(labels).clone()
        } else {
            labels
        };
        state.entries.insert(
            (volume.get(), key.0, key.1, key.2),
            (revision, labels.clone()),
        );
        labels
    }

    /// Drops every entry of `volume` whose brick is not in `keep`.
    pub(crate) fn retain_volume(&self, volume: VolumeId, keep: impl Fn(&(i64, i64, i64)) -> bool) {
        let id = volume.get();
        self.lock()
            .entries
            .retain(|(v, x, y, z), _| *v != id || keep(&(*x, *y, *z)));
    }

    /// Cached bricks (diagnostics and tests).
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn count_proven_labels_match_flood_fill_across_storage_and_cow_edits() {
        for unique in [3, 300, u16::MAX as usize] {
            let cells: Vec<_> = (0..CELLS_PER_BRICK)
                .map(|i| {
                    if unique == u16::MAX as usize {
                        MaterialId(u16::MAX - i as u16)
                    } else {
                        MaterialId(1 + (i % unique) as u16)
                    }
                })
                .collect();
            let mut brick = Brick::restored(&cells, Revision(1), false);
            let full = brick.snapshot();
            assert_eq!(label_brick(&full), label_partial_brick(&full));
            assert!(label_brick(&full).is_full());
            // Removing an entire interior plane creates two components. The
            // old snapshot remains fully solid; the new count must not reuse
            // that fast path or bond across the gap.
            for z in 0..BRICK_EDGE as u8 {
                for y in 0..BRICK_EDGE as u8 {
                    brick.set_cell(LocalCell::new(16, y, z).unwrap(), MaterialId::AIR);
                }
            }
            brick.collapse();
            let split = brick.snapshot();
            assert_eq!(label_brick(&split), label_partial_brick(&split));
            assert_eq!(label_brick(&split).count(), 2);
            assert!(label_brick(&full).is_full());
        }
        let empty = Brick::empty().snapshot();
        assert_eq!(label_brick(&empty), label_partial_brick(&empty));
    }
    #[test]
    fn cached_full_and_empty_shapes_share_without_sharing_partial_connectivity() {
        let cache = LabelCache::new();
        let volume = VolumeId::new(1).unwrap();
        let full = Arc::new(label_brick(
            &Brick::uniform(MaterialId(1), Revision(1)).snapshot(),
        ));
        let mixed_cells: Vec<_> = (0..CELLS_PER_BRICK)
            .map(|i| MaterialId(1 + (i % 3) as u16))
            .collect();
        let mixed = Arc::new(label_brick(
            &Brick::restored(&mixed_cells, Revision(2), false).snapshot(),
        ));
        assert_eq!(full, mixed);
        let first = cache.put(volume, (0, 0, 0), Revision(1), full);
        let second = cache.put(volume, (1, 0, 0), Revision(2), mixed);
        assert!(Arc::ptr_eq(&first, &second));
        let empty = Arc::new(label_brick(&Brick::empty().snapshot()));
        let air_first = cache.put(volume, (2, 0, 0), Revision(1), empty.clone());
        let air_second = cache.put(volume, (3, 0, 0), Revision(9), Arc::new((*empty).clone()));
        assert!(Arc::ptr_eq(&air_first, &air_second));
        let partial = Arc::new(label_brick(&brick_from(&[(0, 0, 0), (31, 31, 31)])));
        let partial = cache.put(volume, (4, 0, 0), Revision(1), partial);
        assert_eq!(partial.count(), 2);
        assert!(!Arc::ptr_eq(&first, &partial));
        assert!(cache.get(volume, (1, 0, 0), Revision(1)).is_none());
        assert!(Arc::ptr_eq(
            &second,
            &cache.get(volume, (1, 0, 0), Revision(2)).unwrap()
        ));
    }
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

    /// The compact representations must label exactly like a plain per-cell
    /// flood fill, including every per-component fact the graph relies on.
    #[test]
    fn compact_labels_match_a_reference_flood_fill_on_random_bricks() {
        fn reference(solid: &[bool]) -> (Vec<u16>, u16) {
            let mut labels = vec![0u16; CELLS_PER_BRICK];
            let mut count = 0u16;
            for start in 0..CELLS_PER_BRICK {
                if !solid[start] || labels[start] != 0 {
                    continue;
                }
                count += 1;
                labels[start] = count;
                let mut stack = vec![start];
                while let Some(here) = stack.pop() {
                    let (x, y, z) = (here % EDGE, (here / EDGE) % EDGE, here / (EDGE * EDGE));
                    let mut push = |n: usize| {
                        if solid[n] && labels[n] == 0 {
                            labels[n] = count;
                            stack.push(n);
                        }
                    };
                    if x > 0 {
                        push(idx(x - 1, y, z));
                    }
                    if x + 1 < EDGE {
                        push(idx(x + 1, y, z));
                    }
                    if y > 0 {
                        push(idx(x, y - 1, z));
                    }
                    if y + 1 < EDGE {
                        push(idx(x, y + 1, z));
                    }
                    if z > 0 {
                        push(idx(x, y, z - 1));
                    }
                    if z + 1 < EDGE {
                        push(idx(x, y, z + 1));
                    }
                }
            }
            (labels, count)
        }

        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        // Fill fractions from sparse specks to nearly solid, so single-component,
        // multi-component, full and empty representations all occur.
        for (round, fill) in [0u64, 1, 5, 20, 50, 80, 95, 99, 100]
            .into_iter()
            .cycle()
            .take(27)
            .enumerate()
        {
            let mut b = Brick::uniform(MaterialId::AIR, Revision(1));
            let mut solid = vec![false; CELLS_PER_BRICK];
            for (i, cell_solid) in solid.iter_mut().enumerate() {
                let on = if fill == 100 {
                    true
                } else {
                    next() % 100 < fill
                };
                if on {
                    let cell = LocalCell::from_linear_index(i as u16).unwrap();
                    b.set_cell(cell, STONE);
                    *cell_solid = true;
                }
            }
            b.collapse();
            let labels = label_brick(&b.snapshot());
            let (expected, count) = reference(&solid);
            assert_eq!(labels.count(), count, "round {round} fill {fill}");
            for (i, &want) in expected.iter().enumerate() {
                assert_eq!(labels.label_at_index(i), want, "round {round} cell {i}");
            }
            // Face masks: bit `u` of row `v` is a solid boundary cell at `(u, v)`.
            for (face, mask) in (0..6).map(|f| (f, labels.face_mask(f))) {
                for (v, row) in mask.iter().enumerate() {
                    for u in 0..EDGE {
                        let (x, y, z) = match face {
                            0 => (0, u, v),
                            1 => (EDGE - 1, u, v),
                            2 => (u, 0, v),
                            3 => (u, EDGE - 1, v),
                            4 => (u, v, 0),
                            _ => (u, v, EDGE - 1),
                        };
                        assert_eq!(
                            row >> u & 1 == 1,
                            solid[idx(x, y, z)],
                            "round {round} face {face} ({u},{v})"
                        );
                    }
                }
            }
            for c in 1..=count {
                let comp = LocalComponent(c);
                let cells: Vec<usize> =
                    (0..CELLS_PER_BRICK).filter(|&i| expected[i] == c).collect();
                assert_eq!(labels.cell_count(comp) as usize, cells.len());
                assert_eq!(
                    labels
                        .cells(comp)
                        .iter()
                        .map(|l| usize::from(l.linear_index()))
                        .collect::<Vec<_>>(),
                    cells
                );
                let on = |f: &dyn Fn(usize, usize, usize) -> bool| {
                    cells
                        .iter()
                        .any(|&i| f(i % EDGE, (i / EDGE) % EDGE, i / (EDGE * EDGE)))
                };
                assert_eq!(
                    labels.faces(comp),
                    [
                        on(&|x, _, _| x == 0),
                        on(&|x, _, _| x == EDGE - 1),
                        on(&|_, y, _| y == 0),
                        on(&|_, y, _| y == EDGE - 1),
                        on(&|_, _, z| z == 0),
                        on(&|_, _, z| z == EDGE - 1),
                    ]
                );
                for y in 0..EDGE as u8 {
                    assert_eq!(
                        labels.has_layer(comp, y),
                        cells.iter().any(|&i| (i / EDGE) % EDGE == usize::from(y))
                    );
                }
            }
        }
    }

    #[test]
    fn uniform_and_single_component_bricks_use_compact_storage() {
        let full = label_brick(&Brick::uniform(STONE, Revision(1)).snapshot());
        assert!(full.is_full());
        assert_eq!(full.heap_bytes(), std::mem::size_of::<ComponentInfo>());
        let one = brick_from(&[(0, 0, 0), (1, 0, 0), (2, 0, 0)]);
        let labels = label_brick(&one);
        assert_eq!(labels.count(), 1);
        assert!(labels.heap_bytes() < 8 * 1024, "{}", labels.heap_bytes());
    }
}
