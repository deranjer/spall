//! One brick: a fixed `32 x 32 x 32` block of authoritative per-cell data.
//!
//! Each layer is stored as `Uniform(value)` (metadata only), a full-width dense
//! array, or an exact material palette with byte indices. Nonuniform payloads
//! are reference-counted so an immutable
//! [`BrickSnapshot`] shares it for free and a later edit copies it on write —
//! the snapshot never changes. The content hash is representation-independent:
//! a dense layer whose cells are all equal hashes exactly like the uniform
//! layer with that value.

use std::sync::{Arc, OnceLock};

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

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
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
    Paletted(Arc<PalettedArray>),
}

#[derive(Debug)]
struct PalettedArray {
    // One allocation for all immutable data. Separate tiny palette/Arc
    // allocations interleaved with indices on worldgen workers caused severe
    // subsequent occupancy-buffer allocation cost on the Windows heap.
    palette: [MaterialId; 256],
    palette_len: usize,
    indices: [u8; CELLS_PER_BRICK],
    contiguous_base: Option<u16>,
}

/// Exact internal storage only: canonical/wire cells remain full-width IDs.
fn palette_cells(cells: &[MaterialId]) -> Option<Arc<PalettedArray>> {
    let (min, max) = cells.iter().fold((u16::MAX, 0), |(min, max), cell| {
        (min.min(cell.raw()), max.max(cell.raw()))
    });
    if max - min < 256 {
        let mut palette = [MaterialId::AIR; 256];
        for (i, value) in (min..=max).enumerate() {
            palette[i] = MaterialId(value);
        }
        return Some(Arc::new(PalettedArray {
            palette,
            palette_len: usize::from(max - min) + 1,
            indices: std::array::from_fn(|i| (cells[i].raw() - min) as u8),
            contiguous_base: Some(min),
        }));
    }
    let mut lookup = vec![u16::MAX; usize::from(u16::MAX) + 1];
    let mut palette = [MaterialId::AIR; 256];
    let mut palette_len = 0usize;
    let mut indices = [0u8; CELLS_PER_BRICK];
    for (i, &material) in cells.iter().enumerate() {
        let slot = &mut lookup[usize::from(material.raw())];
        if *slot == u16::MAX {
            if palette_len == 256 {
                return None;
            }
            *slot = palette_len as u16;
            palette[palette_len] = material;
            palette_len += 1;
        }
        indices[i] = *slot as u8;
    }
    Some(Arc::new(PalettedArray {
        contiguous_base: None,
        palette,
        palette_len,
        indices,
    }))
}

impl Layer {
    #[inline]
    fn get(&self, cell: LocalCell) -> MaterialId {
        match self {
            Self::Uniform(value) => *value,
            Self::Dense(cells) => cells[cell.linear_index() as usize],
            Self::Paletted(cells) => {
                let index = cells.indices[cell.linear_index() as usize];
                match cells.contiguous_base {
                    Some(base) => MaterialId(base + u16::from(index)),
                    None => cells.palette[usize::from(index)],
                }
            }
        }
    }

    /// Ensures dense storage and returns the mutable cell array, copying the
    /// payload first if it is shared with a snapshot.
    fn make_dense_mut(&mut self) -> &mut DenseArray {
        if let Self::Uniform(value) = *self {
            *self = Self::Dense(dense_filled(value));
        }
        if let Self::Paletted(cells) = self {
            let expanded: Box<DenseArray> = cells
                .indices
                .iter()
                .map(|&i| cells.palette[usize::from(i)])
                .collect::<Vec<_>>()
                .into_boxed_slice()
                .try_into()
                .expect("fixed brick length");
            *self = Self::Dense(Arc::from(expanded));
        }
        match self {
            Self::Dense(cells) => Arc::make_mut(cells),
            Self::Uniform(_) => unreachable!("just materialized to dense"),
            Self::Paletted(_) => unreachable!("just expanded palette"),
        }
    }

    /// Collapse equal cells to uniform, otherwise pack a bounded palette.
    fn collapse(&mut self) {
        if let Self::Dense(cells) = self {
            let first = cells[0];
            if cells.iter().all(|c| *c == first) {
                *self = Self::Uniform(first);
            } else if let Some(packed) = palette_cells(cells.as_slice()) {
                *self = Self::Paletted(packed);
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
            Self::Paletted(cells) => (cells.palette_len == 1).then_some(cells.palette[0]),
        }
    }

    fn is_dense(&self) -> bool {
        !matches!(self, Self::Uniform(_))
    }

    /// `Some` only when dense storage is actually shared (an outstanding
    /// snapshot).
    fn shared_dense(&self) -> bool {
        match self {
            Self::Dense(cells) => Arc::strong_count(cells) > 1,
            Self::Paletted(cells) => Arc::strong_count(cells) > 1,
            Self::Uniform(_) => false,
        }
    }

    fn storage_bytes(&self) -> usize {
        match self {
            Self::Uniform(_) => 0,
            Self::Dense(_) => DENSE_LAYER_BYTES,
            Self::Paletted(_) => CELLS_PER_BRICK + 256 * size_of::<MaterialId>(),
        }
    }
}

/// A resident brick.
#[derive(Debug, Clone)]
pub struct Brick {
    material: Layer,
    revision: Revision,
    edited: bool,
    /// Memoized [`Brick::content_hash`] and [`Brick::solid_cells`]. Shared by
    /// every clone and snapshot of the same contents, so a value computed on a
    /// snapshot is remembered by the live brick; a mutation swaps in fresh,
    /// empty cells, so no clone ever sees a value for different contents.
    derived: Arc<Derived>,
}

#[derive(Debug, Default)]
struct Derived {
    hash: OnceLock<BrickHash>,
    solid_cells: OnceLock<u32>,
}

impl Brick {
    /// A brick with every cell set to `material` and no edit history.
    pub fn uniform(material: MaterialId, revision: Revision) -> Self {
        Self {
            material: Layer::Uniform(material),
            revision,
            edited: false,
            derived: Arc::default(),
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
        Self::restored_with_packing(cells, revision, edited, true)
    }

    /// Full-width worker construction. The owning thread may compact with
    /// `collapse` after joining bulk generation workers. This avoids parallel
    /// mixed-size palette allocations on the Windows heap.
    pub fn restored_unpacked(cells: &[MaterialId], revision: Revision, edited: bool) -> Self {
        Self::restored_with_packing(cells, revision, edited, false)
    }

    fn restored_with_packing(
        cells: &[MaterialId],
        revision: Revision,
        edited: bool,
        allow_palette: bool,
    ) -> Self {
        assert_eq!(
            cells.len(),
            CELLS_PER_BRICK,
            "a restored brick layer is exactly {CELLS_PER_BRICK} cells"
        );
        let first = cells[0];
        let material = if cells.iter().all(|&cell| cell == first) {
            Layer::Uniform(first)
        } else if let Some(packed) = allow_palette.then(|| palette_cells(cells)).flatten() {
            Layer::Paletted(packed)
        } else {
            let boxed: Box<DenseArray> = cells
                .to_vec()
                .into_boxed_slice()
                .try_into()
                .expect("validated fixed brick length");
            Layer::Dense(Arc::from(boxed))
        };
        // Restore a fresh immutable payload directly. Per-cell mutation would
        // allocate/invalidate the derived cache for each changed cell despite
        // there being no published contents or cache to preserve yet.
        Self {
            material,
            revision,
            edited,
            derived: Arc::default(),
        }
    }

    /// Restores uniform contents without materializing a dense temporary.
    pub fn restored_uniform(material: MaterialId, revision: Revision, edited: bool) -> Self {
        Self {
            edited,
            ..Self::uniform(material, revision)
        }
    }

    /// Restores authoritative metadata while reusing an immutable payload only
    /// when every material equals the received cells. No revision or digest is
    /// trusted as a substitute for that exact comparison.
    pub fn restored_reusing(
        cells: &[MaterialId],
        revision: Revision,
        edited: bool,
        candidate: &BrickSnapshot,
    ) -> Self {
        assert_eq!(cells.len(), CELLS_PER_BRICK);
        let equal = match &candidate.0.material {
            Layer::Uniform(m) => cells.iter().all(|cell| cell == m),
            Layer::Dense(raw) => raw.as_slice() == cells,
            Layer::Paletted(raw) => match raw.contiguous_base {
                Some(base) => raw
                    .indices
                    .iter()
                    .zip(cells)
                    .all(|(&i, &m)| MaterialId(base + u16::from(i)) == m),
                None => raw
                    .indices
                    .iter()
                    .zip(cells)
                    .all(|(&i, &m)| raw.palette[usize::from(i)] == m),
            },
        };
        if equal {
            Self {
                material: candidate.0.material.clone(),
                revision,
                edited,
                derived: if candidate.0.edited == edited {
                    candidate.0.derived.clone()
                } else {
                    Arc::default()
                },
            }
        } else {
            Self::restored(cells, revision, edited)
        }
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
        self.material.shared_dense()
    }

    /// Actual cell/palette payload bytes, excluding fixed brick/Arc metadata.
    pub fn material_storage_bytes(&self) -> usize {
        self.material.storage_bytes()
    }

    /// Sets one cell. Returns whether the stored value changed. Materializes to
    /// dense storage if needed; does not collapse (callers batch and collapse
    /// once).
    pub fn set_cell(&mut self, cell: LocalCell, material: MaterialId) -> bool {
        if self.material.get(cell) == material {
            return false;
        }
        self.material.make_dense_mut()[cell.linear_index() as usize] = material;
        self.derived = Arc::default();
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
        if !self.edited {
            // The modified flag is part of the hash.
            self.derived = Arc::default();
        }
        self.edited = true;
    }

    /// A cheap immutable copy that shares dense storage with this brick.
    pub fn snapshot(&self) -> BrickSnapshot {
        BrickSnapshot(self.clone())
    }

    /// Representation-independent BLAKE3 hash over the authoritative layers and
    /// the modified flag. Dense-but-uniform hashes like uniform.
    pub fn content_hash(&self) -> BrickHash {
        *self
            .derived
            .hash
            .get_or_init(|| self.compute_content_hash())
    }

    /// Number of non-air cells. Uniform bricks answer without reading a cell;
    /// dense bricks count once and remember the answer.
    pub fn solid_cells(&self) -> u32 {
        match &self.material {
            Layer::Uniform(value) => {
                if value.is_air() {
                    0
                } else {
                    CELLS_PER_BRICK as u32
                }
            }
            Layer::Dense(cells) => *self
                .derived
                .solid_cells
                .get_or_init(|| cells.iter().filter(|c| !c.is_air()).count() as u32),
            Layer::Paletted(cells) => {
                *self
                    .derived
                    .solid_cells
                    .get_or_init(|| match cells.contiguous_base {
                        Some(0) => cells.indices.iter().filter(|&&i| i != 0).count() as u32,
                        Some(_) => CELLS_PER_BRICK as u32,
                        None => cells
                            .indices
                            .iter()
                            .filter(|&&i| !cells.palette[usize::from(i)].is_air())
                            .count() as u32,
                    })
            }
        }
    }

    fn compute_content_hash(&self) -> BrickHash {
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

/// Feeds the dense cells to `hasher` as one contiguous little-endian `u16` stream.
///
/// The byte stream is exactly what hashing each cell's two little-endian bytes one `update` at a
/// time produces (BLAKE3 is a streaming hash), so every brick hash is unchanged; a single
/// 64 KiB update instead of 32,768 two-byte updates is what makes the whole-volume topology hash
/// that a terrain commit recomputes cheap (it measured ~35 ms for 84 bricks).
fn hash_dense_cells(hasher: &mut blake3::Hasher, cells: &DenseArray) {
    let mut bytes = vec![0u8; DENSE_LAYER_BYTES];
    for (chunk, cell) in bytes.chunks_exact_mut(2).zip(cells.iter()) {
        chunk.copy_from_slice(&cell.raw().to_le_bytes());
    }
    hasher.update(&bytes);
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
            match layer {
                Layer::Dense(cells) => hash_dense_cells(hasher, cells),
                Layer::Paletted(cells) => {
                    let mut bytes = vec![0u8; DENSE_LAYER_BYTES];
                    for (chunk, &index) in bytes.chunks_exact_mut(2).zip(cells.indices.iter()) {
                        chunk.copy_from_slice(
                            &cells.palette[usize::from(index)].raw().to_le_bytes(),
                        );
                    }
                    hasher.update(&bytes);
                }
                Layer::Uniform(_) => unreachable!("handled uniform contents"),
            }
        }
    }
}

/// An immutable view of a brick at one instant. Cloning is O(1); the dense
/// payload is shared with the live brick until that brick is next edited.
#[derive(Debug, Clone)]
pub struct BrickSnapshot(Brick);

impl BrickSnapshot {
    /// Full-width cells in canonical order, decoded with one storage dispatch.
    /// This bounded temporary avoids a representation branch per cell in
    /// whole-brick consumers such as collision and baseline capture.
    pub fn material_cells(&self) -> Vec<MaterialId> {
        match &self.0.material {
            Layer::Uniform(value) => vec![*value; CELLS_PER_BRICK],
            Layer::Dense(cells) => cells.as_slice().to_vec(),
            Layer::Paletted(cells) => match cells.contiguous_base {
                Some(base) => cells
                    .indices
                    .iter()
                    .map(|&i| MaterialId(base + u16::from(i)))
                    .collect(),
                None => cells
                    .indices
                    .iter()
                    .map(|&i| cells.palette[usize::from(i)])
                    .collect(),
            },
        }
    }
    pub fn material_storage_bytes(&self) -> usize {
        self.0.material_storage_bytes()
    }
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

    #[inline]
    pub fn is_dense(&self) -> bool {
        self.0.is_dense()
    }

    pub fn content_hash(&self) -> BrickHash {
        self.0.content_hash()
    }

    /// Number of non-air cells (see [`Brick::solid_cells`]).
    pub fn solid_cells(&self) -> u32 {
        self.0.solid_cells()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "manual full-residency allocator pressure diagnostic; about 2-4 GiB"]
    fn allocator_pressure_probe() {
        let storage =
            std::env::var("SPALL_ALLOCATOR_PROBE_STORAGE").unwrap_or_else(|_| "palette".into());
        let dense = storage == "dense" || storage == "owner" || storage == "owner-batches";
        let cells: Vec<_> = (0..CELLS_PER_BRICK)
            .map(|i| MaterialId((i % 17) as u16))
            .collect();
        let started = std::time::Instant::now();
        let mut bricks: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..16)
                .map(|_| {
                    let cells = &cells;
                    scope.spawn(move || {
                        let mut bricks = Vec::with_capacity(12_288);
                        for i in 0..12_288 {
                            let mut brick = Brick::empty();
                            let scratch = cells.clone();
                            brick.material = if i >= 3750 {
                                Layer::Uniform(MaterialId(1))
                            } else if dense {
                                let raw: Box<DenseArray> =
                                    scratch.clone().into_boxed_slice().try_into().unwrap();
                                Layer::Dense(Arc::from(raw))
                            } else {
                                Layer::Paletted(palette_cells(&scratch).unwrap())
                            };
                            bricks.push(brick);
                        }
                        bricks
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|handle| handle.join().unwrap())
                .collect()
        });
        let build_ms = started.elapsed().as_millis();
        let source_guard = (storage == "owner").then(|| bricks.clone());
        let packing_start = std::time::Instant::now();
        if storage == "owner" {
            for brick in &mut bricks {
                brick.collapse();
            }
        } else if storage == "owner-batches" {
            for batch in bricks.chunks_mut(4096) {
                let guard = batch.to_vec();
                for brick in batch {
                    brick.collapse();
                }
                drop(guard);
            }
        }
        let packing_ms = packing_start.elapsed().as_millis();
        drop(source_guard);
        let allocated: usize = bricks.iter().map(Brick::material_storage_bytes).sum();
        let started = std::time::Instant::now();
        for _ in 0..1000 {
            std::hint::black_box((
                vec![true; CELLS_PER_BRICK],
                vec![MaterialId(1); CELLS_PER_BRICK],
            ));
        }
        eprintln!(
            "allocator_probe storage={storage} bricks={} payload_bytes={allocated} build_ms={build_ms} packing_ms={packing_ms} grid_allocations=1000 allocation_ms={}",
            bricks.len(),
            started.elapsed().as_millis()
        );
        std::hint::black_box(&bricks);
    }

    fn cell(x: u8, y: u8, z: u8) -> LocalCell {
        LocalCell::new(x, y, z).unwrap()
    }

    #[test]
    fn palette_boundaries_full_width_ids_and_edits_match_unpacked_cells() {
        for base in [0, 65530] {
            let cells: Vec<_> = (0..CELLS_PER_BRICK)
                .map(|i| MaterialId(base + (i % 6) as u16))
                .collect();
            let brick = Brick::restored(&cells, Revision(2), false);
            assert_eq!(brick.snapshot().material_cells(), cells);
            assert_eq!(brick.material_storage_bytes(), CELLS_PER_BRICK + 512);
            for (i, &material) in cells.iter().enumerate() {
                assert_eq!(
                    brick.get(LocalCell::from_linear_index(i as u16).unwrap()),
                    material
                );
            }
            let snapshot = brick.snapshot();
            let mut edited = brick;
            edited.set_cell(cell(0, 0, 0), MaterialId(1000));
            edited.collapse();
            assert_eq!(snapshot.get(cell(0, 0, 0)), MaterialId(base));
            assert_eq!(edited.get(cell(0, 0, 0)), MaterialId(1000));
        }
        for unique in [2, 255, 256, 257, 1024] {
            let cells: Vec<_> = (0..CELLS_PER_BRICK)
                .map(|i| {
                    let value = i % unique;
                    MaterialId(if value == 0 { u16::MAX } else { value as u16 })
                })
                .collect();
            let mut packed = Brick::restored(&cells, Revision(15), true);
            assert_eq!(packed.snapshot().material_cells(), cells);
            let mut reference = Brick::uniform(cells[0], Revision(15));
            for (i, &m) in cells.iter().enumerate() {
                let local = LocalCell::from_linear_index(i as u16).unwrap();
                reference.set_cell(local, m);
                assert_eq!(packed.get(local), m);
            }
            reference.edited = true;
            assert_eq!(packed.content_hash(), reference.content_hash());
            assert_eq!(packed.solid_cells(), reference.solid_cells());
            assert_eq!(
                packed.material_storage_bytes(),
                if unique <= 256 {
                    CELLS_PER_BRICK + 512
                } else {
                    DENSE_LAYER_BYTES
                }
            );
            let snapshot = packed.snapshot();
            packed.set_cell(cell(0, 0, 0), MaterialId::AIR);
            packed.collapse();
            assert_eq!(snapshot.get(cell(0, 0, 0)), MaterialId(u16::MAX));
            assert_eq!(packed.get(cell(0, 0, 0)), MaterialId::AIR);
            reference.set_cell(cell(0, 0, 0), MaterialId::AIR);
            assert_eq!(packed.content_hash(), reference.content_hash());
        }
    }

    #[test]
    fn direct_restore_matches_mutation_hash_and_preserves_snapshot_isolation() {
        let cells: Vec<_> = (0..CELLS_PER_BRICK)
            .map(|i| MaterialId((i % 17) as u16))
            .collect();
        let mut reference = Brick::uniform(cells[0], Revision(9));
        for (i, &m) in cells.iter().enumerate() {
            reference.set_cell(LocalCell::from_linear_index(i as u16).unwrap(), m);
        }
        reference.edited = true;
        let mut restored = Brick::restored(&cells, Revision(9), true);
        assert_eq!(restored.content_hash(), reference.content_hash());
        assert_eq!(restored.solid_cells(), reference.solid_cells());
        let snapshot = restored.snapshot();
        restored.set_cell(cell(0, 0, 0), MaterialId(99));
        assert_eq!(snapshot.content_hash(), reference.content_hash());
        for m in [MaterialId::AIR, MaterialId(3)] {
            let fast = Brick::restored_uniform(m, Revision(27), true);
            let dense_input = Brick::restored(&vec![m; CELLS_PER_BRICK], Revision(27), true);
            assert_eq!(fast.content_hash(), dense_input.content_hash());
            assert_eq!(fast.is_modified_air(), m.is_air());
            assert!(!fast.is_dense());
        }
    }

    #[test]
    fn reuse_compares_exact_cells_and_restamps_metadata_without_stale_hashes() {
        let mut cells = vec![MaterialId(2); CELLS_PER_BRICK];
        cells[23] = MaterialId::AIR;
        let original = Brick::restored(&cells, Revision(9), true);
        let snapshot = original.snapshot();
        let _ = original.content_hash();
        let mut reused = Brick::restored_reusing(&cells, Revision(0), false, &snapshot);
        assert!(reused.dense_payload_shared());
        assert_eq!(reused.revision(), Revision(0));
        assert!(!reused.is_edited());
        assert_eq!(
            reused.content_hash(),
            Brick::restored(&cells, Revision(0), false).content_hash()
        );
        reused.set_cell(cell(1, 0, 0), MaterialId(99));
        assert_eq!(snapshot.get(cell(1, 0, 0)), MaterialId(2));
        cells[29] = MaterialId(7);
        let different = Brick::restored_reusing(&cells, Revision(8), true, &snapshot);
        assert!(!different.dense_payload_shared());
        assert_eq!(
            different.content_hash(),
            Brick::restored(&cells, Revision(8), true).content_hash()
        );
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
    #[test]
    fn the_bulk_dense_hash_is_byte_identical_to_the_per_cell_stream() {
        // Reference: the original definition, one two-byte update per cell.
        fn reference(brick: &Brick) -> BrickHash {
            let mut hasher = blake3::Hasher::new();
            hasher.update(&(BRICK_HASH_DOMAIN.len() as u32).to_le_bytes());
            hasher.update(BRICK_HASH_DOMAIN);
            hasher.update(&[u8::from(brick.edited)]);
            hasher.update(&1u32.to_le_bytes());
            hasher.update(&LayerKind::Material.code().to_le_bytes());
            match brick.material.uniform_value() {
                Some(v) => {
                    hasher.update(&[0u8]);
                    hasher.update(&v.raw().to_le_bytes());
                }
                None => {
                    hasher.update(&[1u8]);
                    let Layer::Dense(cells) = &brick.material else {
                        unreachable!()
                    };
                    for cell in cells.iter() {
                        hasher.update(&cell.raw().to_le_bytes());
                    }
                }
            }
            BrickHash(*hasher.finalize().as_bytes())
        }
        let mut brick = Brick::uniform(MaterialId(1), Revision(1));
        assert_eq!(brick.content_hash(), reference(&brick), "uniform");
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        for i in 0..CELLS_PER_BRICK as u16 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let local = LocalCell::from_linear_index(i).unwrap();
            brick.set_cell(local, MaterialId((x % 7) as u16));
        }
        assert!(brick.is_dense());
        assert_eq!(brick.content_hash(), reference(&brick), "random dense");
        // High byte set too, so a byte-order mistake could not hide.
        brick.set_cell(cell(1, 2, 3), MaterialId(0xABCD));
        assert_eq!(brick.content_hash(), reference(&brick), "wide ids");
    }

    #[test]
    fn cached_hash_and_solid_count_follow_every_change_and_survive_clones() {
        let stone = MaterialId(1);
        let mut brick = Brick::uniform(MaterialId::AIR, Revision(1));
        assert_eq!(brick.solid_cells(), 0);
        brick.set_cell(cell(1, 2, 3), stone);
        brick.set_cell(cell(4, 5, 6), stone);
        let first = (brick.content_hash(), brick.solid_cells());
        assert_eq!(first.1, 2);
        // A fresh brick with the same contents hashes the same without the cache.
        let mut twin = Brick::uniform(MaterialId::AIR, Revision(9));
        twin.set_cell(cell(1, 2, 3), stone);
        twin.set_cell(cell(4, 5, 6), stone);
        assert_eq!(twin.content_hash(), first.0);

        // A snapshot shares the memoized values, and computing on it is
        // remembered by the live brick.
        let mut live = twin.clone();
        let snap = live.snapshot();
        assert_eq!(snap.content_hash(), first.0);

        // Editing the live brick changes both values; the snapshot keeps its own.
        live.set_cell(cell(7, 7, 7), stone);
        assert_eq!(live.solid_cells(), 3);
        assert_ne!(live.content_hash(), first.0);
        assert_eq!(snap.solid_cells(), 2);
        assert_eq!(snap.content_hash(), first.0);

        // Removing the cell again restores the original hash.
        live.set_cell(cell(7, 7, 7), MaterialId::AIR);
        assert_eq!(live.content_hash(), first.0);
        assert_eq!(live.solid_cells(), 2);

        // The modified flag is part of the hash.
        let before = live.content_hash();
        live.mark_edited();
        assert_ne!(live.content_hash(), before);

        // Uniform bricks count without cells.
        assert_eq!(
            Brick::uniform(stone, Revision(1)).solid_cells(),
            CELLS_PER_BRICK as u32
        );
    }
}
