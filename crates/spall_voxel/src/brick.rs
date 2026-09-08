//! One brick: a fixed `32 x 32 x 32` block of authoritative per-cell data.
//!
//! Each layer is stored as either `Uniform(value)` (metadata only) or `Dense`
//! (one value per cell). The dense payload is reference-counted so an immutable
//! [`BrickSnapshot`] shares it for free and a later edit copies it on write —
//! the snapshot never changes. The content hash is representation-independent:
//! a dense layer whose cells are all equal hashes exactly like the uniform
//! layer with that value.

use std::sync::Arc;

use spall_core::{CELLS_PER_BRICK, LocalCell, MaterialId, Revision};

/// Bytes a dense material layer occupies (`32768` cells x `u16`).
pub const DENSE_LAYER_BYTES: usize = CELLS_PER_BRICK * size_of::<u16>();

/// Domain tag for the brick content hash; bump the version on any layout change.
const BRICK_HASH_DOMAIN: &[u8] = b"spall.brick.v1";

type DenseArray = [MaterialId; CELLS_PER_BRICK];

fn dense_filled(value: MaterialId) -> Arc<DenseArray> {
    let boxed: Box<DenseArray> = vec![value; CELLS_PER_BRICK]
        .into_boxed_slice()
        .try_into()
        .expect("vec has exactly CELLS_PER_BRICK elements");
    Arc::from(boxed)
}

/// A 32-byte BLAKE3 digest over a brick's authoritative layers. Trivially
/// convertible to any other 32-byte hash newtype for cross-layer use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BrickHash([u8; 32]);

impl BrickHash {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn to_bytes(self) -> [u8; 32] {
        self.0
    }
}

impl std::fmt::Display for BrickHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Stable numeric code for an authoritative layer. Only `Material` exists in
/// T02; damage and other layers take later codes and are hashed in code order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum LayerKind {
    Material = 0,
}

impl LayerKind {
    const fn code(self) -> u16 {
        self as u16
    }
}

/// One layer's storage.
#[derive(Debug, Clone)]
enum Layer {
    Uniform(MaterialId),
    Dense(Arc<DenseArray>),
}

impl Layer {
    #[inline]
    fn get(&self, cell: LocalCell) -> MaterialId {
        match self {
            Self::Uniform(value) => *value,
            Self::Dense(cells) => cells[cell.linear_index() as usize],
        }
    }

    /// Ensures dense storage and returns the mutable cell array, copying the
    /// payload first if it is shared with a snapshot.
    fn make_dense_mut(&mut self) -> &mut DenseArray {
        if let Self::Uniform(value) = *self {
            *self = Self::Dense(dense_filled(value));
        }
        match self {
            Self::Dense(cells) => Arc::make_mut(cells),
            Self::Uniform(_) => unreachable!("just materialized to dense"),
        }
    }

    /// If dense with every cell equal, collapse back to uniform.
    fn collapse(&mut self) {
        if let Self::Dense(cells) = self {
            let first = cells[0];
            if cells.iter().all(|c| *c == first) {
                *self = Self::Uniform(first);
            }
        }
    }

    /// The single value if this layer is uniform in content (uniform storage,
    /// or dense storage with all cells equal).
    fn uniform_value(&self) -> Option<MaterialId> {
        match self {
            Self::Uniform(value) => Some(*value),
            Self::Dense(cells) => {
                let first = cells[0];
                cells.iter().all(|c| *c == first).then_some(first)
            }
        }
    }

    fn is_dense(&self) -> bool {
        matches!(self, Self::Dense(_))
    }

    /// `Some` only when dense storage is actually shared (an outstanding
    /// snapshot).
    fn shared_dense(&self) -> Option<&Arc<DenseArray>> {
        match self {
            Self::Dense(cells) if Arc::strong_count(cells) > 1 => Some(cells),
            _ => None,
        }
    }
}

/// A resident brick.
#[derive(Debug, Clone)]
pub struct Brick {
    material: Layer,
    revision: Revision,
    edited: bool,
}

impl Brick {
    /// A brick with every cell set to `material` and no edit history.
    pub fn uniform(material: MaterialId, revision: Revision) -> Self {
        Self {
            material: Layer::Uniform(material),
            revision,
            edited: false,
        }
    }

    /// A fresh, unedited air brick at revision zero — the implicit "before"
    /// state of a brick that an edit is about to create.
    pub fn empty() -> Self {
        Self::uniform(MaterialId::AIR, Revision::ZERO)
    }

    /// Rebuilds a brick from a persisted `32768`-cell material layer, its
    /// revision, and its modified flag. Used by save recovery (T16): the
    /// authoritative bytes come straight from the store, not from replaying an
    /// edit, so the `edited` tombstone bit and the exact revision are restored
    /// verbatim. Collapses to uniform storage when every cell is equal.
    ///
    /// Panics if `cells.len() != CELLS_PER_BRICK`.
    pub fn restored(cells: &[MaterialId], revision: Revision, edited: bool) -> Self {
        assert_eq!(
            cells.len(),
            CELLS_PER_BRICK,
            "a restored brick layer is exactly {CELLS_PER_BRICK} cells"
        );
        let first = cells[0];
        let mut brick = Self::uniform(first, revision);
        for (i, &material) in cells.iter().enumerate() {
            if material != first {
                let local = LocalCell::from_linear_index(i as u16)
                    .expect("i < CELLS_PER_BRICK fits a LocalCell");
                brick.set_cell(local, material);
            }
        }
        brick.collapse();
        brick.edited = edited;
        brick
    }

    #[inline]
    pub fn get(&self, cell: LocalCell) -> MaterialId {
        self.material.get(cell)
    }

    #[inline]
    pub fn revision(&self) -> Revision {
        self.revision
    }

    /// True once any authoritative edit has touched this brick. Such a brick
    /// is never regenerated from the world generator, even if it is now all
    /// air (a modified-air tombstone).
    #[inline]
    pub fn is_edited(&self) -> bool {
        self.edited
    }

    /// True when this brick has been edited and now contains only air.
    pub fn is_modified_air(&self) -> bool {
        self.edited && self.material.uniform_value() == Some(MaterialId::AIR)
    }

    #[inline]
    pub fn is_dense(&self) -> bool {
        self.material.is_dense()
    }

    /// True only when this brick is dense *and* its payload is shared with a
    /// live snapshot (`Arc` strong count > 1).
    #[inline]
    pub fn dense_payload_shared(&self) -> bool {
        self.material.shared_dense().is_some()
    }

    /// Sets one cell. Returns whether the stored value changed. Materializes to
    /// dense storage if needed; does not collapse (callers batch and collapse
    /// once).
    pub fn set_cell(&mut self, cell: LocalCell, material: MaterialId) -> bool {
        if self.material.get(cell) == material {
            return false;
        }
        self.material.make_dense_mut()[cell.linear_index() as usize] = material;
        true
    }

    /// Collapses dense storage back to uniform when possible.
    pub fn collapse(&mut self) {
        self.material.collapse();
    }

    pub(crate) fn set_revision(&mut self, revision: Revision) {
        self.revision = revision;
    }

    pub(crate) fn mark_edited(&mut self) {
        self.edited = true;
    }

    /// A cheap immutable copy that shares dense storage with this brick.
    pub fn snapshot(&self) -> BrickSnapshot {
        BrickSnapshot(self.clone())
    }

    /// Representation-independent BLAKE3 hash over the authoritative layers and
    /// the modified flag. Dense-but-uniform hashes like uniform.
    pub fn content_hash(&self) -> BrickHash {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&(BRICK_HASH_DOMAIN.len() as u32).to_le_bytes());
        hasher.update(BRICK_HASH_DOMAIN);
        hasher.update(&[u8::from(self.edited)]);

        // One layer today; encoded as a counted, kind-sorted sequence so more
        // layers slot in without changing existing bytes.
        hasher.update(&1u32.to_le_bytes());
        hash_material_layer(&mut hasher, LayerKind::Material, &self.material);

        BrickHash(*hasher.finalize().as_bytes())
    }
}

fn hash_material_layer(hasher: &mut blake3::Hasher, kind: LayerKind, layer: &Layer) {
    hasher.update(&kind.code().to_le_bytes());
    match layer.uniform_value() {
        Some(value) => {
            hasher.update(&[0u8]); // storage tag: uniform
            hasher.update(&value.raw().to_le_bytes());
        }
        None => {
            hasher.update(&[1u8]); // storage tag: dense
            let Layer::Dense(cells) = layer else {
                unreachable!("non-uniform layer is dense");
            };
            for cell in cells.iter() {
                hasher.update(&cell.raw().to_le_bytes());
            }
        }
    }
}

/// An immutable view of a brick at one instant. Cloning is O(1); the dense
/// payload is shared with the live brick until that brick is next edited.
#[derive(Debug, Clone)]
pub struct BrickSnapshot(Brick);

impl BrickSnapshot {
    #[inline]
    pub fn get(&self, cell: LocalCell) -> MaterialId {
        self.0.get(cell)
    }

    #[inline]
    pub fn revision(&self) -> Revision {
        self.0.revision()
    }

    #[inline]
    pub fn is_edited(&self) -> bool {
        self.0.is_edited()
    }

    #[inline]
    pub fn is_modified_air(&self) -> bool {
        self.0.is_modified_air()
    }

    pub fn content_hash(&self) -> BrickHash {
        self.0.content_hash()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(x: u8, y: u8, z: u8) -> LocalCell {
        LocalCell::new(x, y, z).unwrap()
    }

    #[test]
    fn uniform_brick_reads_one_value_everywhere_and_stays_uniform() {
        let brick = Brick::uniform(MaterialId(4), Revision(1));
        assert_eq!(brick.get(cell(0, 0, 0)), MaterialId(4));
        assert_eq!(brick.get(cell(31, 31, 31)), MaterialId(4));
        assert!(!brick.is_dense());
    }

    #[test]
    fn setting_a_cell_materializes_dense_then_collapses_when_uniform_again() {
        let mut brick = Brick::uniform(MaterialId(1), Revision(1));
        assert!(brick.set_cell(cell(5, 6, 7), MaterialId(0)));
        assert!(brick.is_dense());
        assert_eq!(brick.get(cell(5, 6, 7)), MaterialId(0));
        assert_eq!(brick.get(cell(0, 0, 0)), MaterialId(1));

        // Put it back; collapse returns to uniform storage.
        assert!(brick.set_cell(cell(5, 6, 7), MaterialId(1)));
        brick.collapse();
        assert!(!brick.is_dense());
    }

    #[test]
    fn redundant_write_reports_no_change() {
        let mut brick = Brick::uniform(MaterialId(2), Revision(1));
        assert!(!brick.set_cell(cell(1, 2, 3), MaterialId(2)));
        assert!(!brick.is_dense());
    }

    #[test]
    fn snapshot_is_unaffected_by_later_edits() {
        let mut brick = Brick::uniform(MaterialId(3), Revision(1));
        brick.set_cell(cell(0, 0, 0), MaterialId(9));
        let snap = brick.snapshot();
        let snap_hash = snap.content_hash();

        brick.set_cell(cell(0, 0, 0), MaterialId(1));
        brick.set_cell(cell(1, 1, 1), MaterialId(7));

        assert_eq!(snap.get(cell(0, 0, 0)), MaterialId(9));
        assert_eq!(snap.get(cell(1, 1, 1)), MaterialId(3));
        assert_eq!(snap.content_hash(), snap_hash);
        assert_ne!(brick.content_hash(), snap_hash);
    }

    #[test]
    fn content_hash_is_representation_independent() {
        let uniform = Brick::uniform(MaterialId(5), Revision(1));

        let mut dense = Brick::uniform(MaterialId(5), Revision(1));
        dense.set_cell(cell(0, 0, 0), MaterialId(6));
        dense.set_cell(cell(0, 0, 0), MaterialId(5)); // back to all-5, still dense
        assert!(dense.is_dense());

        assert_eq!(uniform.content_hash(), dense.content_hash());
    }

    #[test]
    fn restored_brick_round_trips_cells_revision_and_modified_flag() {
        let mut cells = vec![MaterialId(1); CELLS_PER_BRICK];
        cells[0] = MaterialId(4);
        cells[9] = MaterialId::AIR;
        let brick = Brick::restored(&cells, Revision(42), true);
        assert_eq!(brick.revision(), Revision(42));
        assert!(brick.is_edited());
        assert_eq!(brick.get(cell(0, 0, 0)), MaterialId(4));
        assert_eq!(brick.get(cell(9, 0, 0)), MaterialId::AIR);
        assert_eq!(brick.get(cell(1, 0, 0)), MaterialId(1));

        // An all-equal restore collapses to uniform storage.
        let uniform = Brick::restored(&vec![MaterialId(2); CELLS_PER_BRICK], Revision(1), false);
        assert!(!uniform.is_dense());
        assert!(!uniform.is_edited());

        // A fully mined restore is a modified-air tombstone.
        let tomb = Brick::restored(&vec![MaterialId::AIR; CELLS_PER_BRICK], Revision(7), true);
        assert!(tomb.is_modified_air());
    }

    #[test]
    fn modified_air_hashes_differently_from_natural_air() {
        let natural = Brick::uniform(MaterialId::AIR, Revision(1));

        let mut mined = Brick::uniform(MaterialId(1), Revision(1));
        mined.set_cell(cell(2, 2, 2), MaterialId::AIR);
        // pretend the edit emptied it entirely
        for z in 0..32 {
            for y in 0..32 {
                for x in 0..32 {
                    mined.set_cell(cell(x, y, z), MaterialId::AIR);
                }
            }
        }
        mined.collapse();
        mined.mark_edited();

        assert!(mined.is_modified_air());
        assert!(!natural.is_modified_air());
        assert_ne!(natural.content_hash(), mined.content_hash());
    }
}
