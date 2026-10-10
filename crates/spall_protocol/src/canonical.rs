//! Canonical little-endian encoding and BLAKE3 hashing.
//!
//! A canonical hash must not depend on map iteration order, insertion order, or
//! Rust layout. This module defines:
//!
//! * [`CanonicalWriter`] — an explicit little-endian byte sink with
//!   length-prefixed blobs.
//! * [`canonical_topology_hash`] — hashes sorted volume / brick / layer state
//!   per `docs/protocol.md`: "sorted volume IDs, cell-size codes, brick
//!   coordinates, authoritative layer bytes, ownership, and revisions.
//!   Exclude motion, render caches, and library allocation order."
//! * [`content_manifest_hash`] — hashes [`MaterialManifest::canonical_bytes`].

use serde::{Deserialize, Serialize};
use spall_core::{BrickCoord, CellSizeCode, EntityId, MaterialManifest, Revision, VolumeId};

/// A 32-byte BLAKE3 digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Hash32(pub [u8; 32]);

impl Hash32 {
    pub const ZERO: Self = Self([0; 32]);

    pub fn of(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }
}

impl std::fmt::Display for Hash32 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Explicit little-endian byte writer. Integers are fixed-width LE; blobs and
/// sequences carry a `u32` LE length prefix so decoding is unambiguous.
#[derive(Debug, Default)]
pub struct CanonicalWriter {
    buf: Vec<u8>,
}

impl CanonicalWriter {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn with_domain(domain: &[u8]) -> Self {
        let mut w = Self::new();
        w.blob(domain);
        w
    }

    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }

    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn i64(&mut self, v: i64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn u128(&mut self, v: u128) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// Length-prefixed raw bytes.
    pub fn blob(&mut self, bytes: &[u8]) -> &mut Self {
        self.u32(bytes.len() as u32);
        self.buf.extend_from_slice(bytes);
        self
    }

    /// Length prefix for a sequence of `count` items; write the items after.
    pub fn seq(&mut self, count: usize) -> &mut Self {
        self.u32(count as u32)
    }

    pub fn finish(self) -> Vec<u8> {
        self.buf
    }

    pub fn hash(self) -> Hash32 {
        Hash32::of(&self.buf)
    }
}

/// Who owns a volume's cells: the world terrain grid, or one detached body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalOwner {
    Terrain,
    Body(EntityId),
}

impl CanonicalOwner {
    fn write(self, w: &mut CanonicalWriter) {
        match self {
            Self::Terrain => {
                w.u8(0).u64(0);
            }
            Self::Body(entity) => {
                w.u8(1).u64(entity.get());
            }
        }
    }
}

/// One authoritative layer of a brick (material ids, bond state, ...). `kind`
/// is a stable numeric layer code; `bytes` is that layer's authoritative
/// payload already in its own canonical form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalLayer {
    pub kind: u16,
    pub bytes: Vec<u8>,
}

/// One brick's contribution to the topology hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalBrick {
    pub coord: BrickCoord,
    pub revision: Revision,
    pub layers: Vec<CanonicalLayer>,
}

/// One volume's contribution to the topology hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalVolume {
    pub volume_id: VolumeId,
    pub cell_size: CellSizeCode,
    pub owner: CanonicalOwner,
    pub bricks: Vec<CanonicalBrick>,
}

/// Domain separation tag of the topology hash; the trailing version names the layout. `v2` hashes
/// bricks in chunks (see [`canonical_topology_hash`]); `v1` hashed every brick in one stream.
pub const TOPOLOGY_DOMAIN: &[u8] = b"spall.topology.v2";
/// Domain separation tag of one chunk's digest.
pub const TOPOLOGY_CHUNK_DOMAIN: &[u8] = b"spall.topology.chunk.v2";
/// Domain separation tag for the content manifest hash.
pub const MANIFEST_DOMAIN: &[u8] = b"spall.manifest.v1";

/// Bricks per chunk edge, as a shift: the topology hash groups bricks into chunks of
/// 8 x 8 x 8. Part of the hash definition; `spall_voxel::CHUNK_SHIFT` (the storage chunk, whose
/// stamps tell a cache which chunks changed) must equal it, which a test in `spall_sim` checks.
pub const HASH_CHUNK_SHIFT: u32 = 3;

/// Key of a hash chunk: `(x, y, z)` brick coordinates shifted right by [`HASH_CHUNK_SHIFT`].
pub type ChunkKey = (i64, i64, i64);

/// The chunk that holds `coord`. An arithmetic shift is a floor division, so negative
/// coordinates chunk correctly.
pub fn chunk_of(coord: BrickCoord) -> ChunkKey {
    (
        coord.x >> HASH_CHUNK_SHIFT,
        coord.y >> HASH_CHUNK_SHIFT,
        coord.z >> HASH_CHUNK_SHIFT,
    )
}

/// Canonical order of chunks, matching the `(z, y, x)` order of bricks.
fn chunk_order(chunk: ChunkKey) -> (i64, i64, i64) {
    (chunk.2, chunk.1, chunk.0)
}

/// The fixed part of a volume's contribution to the topology hash.
#[derive(Debug, Clone, Copy)]
pub struct HashedVolume {
    pub volume_id: VolumeId,
    pub cell_size: CellSizeCode,
    pub owner: CanonicalOwner,
}

fn write_brick(w: &mut CanonicalWriter, brick: &CanonicalBrick) {
    w.i64(brick.coord.x).i64(brick.coord.y).i64(brick.coord.z);
    w.u64(brick.revision.get());
    let mut layers: Vec<&CanonicalLayer> = brick.layers.iter().collect();
    layers.sort_by_key(|l| l.kind);
    w.seq(layers.len());
    for layer in layers {
        w.u16(layer.kind);
        w.blob(&layer.bytes);
    }
}

fn digest_chunk_refs(chunk: ChunkKey, mut bricks: Vec<&CanonicalBrick>) -> Hash32 {
    bricks.sort_by_key(|b| b.coord.sort_key());
    let mut w = CanonicalWriter::with_domain(TOPOLOGY_CHUNK_DOMAIN);
    w.i64(chunk.0).i64(chunk.1).i64(chunk.2);
    w.seq(bricks.len());
    for brick in bricks {
        debug_assert_eq!(chunk_of(brick.coord), chunk, "a brick in the wrong chunk");
        write_brick(&mut w, brick);
    }
    w.hash()
}

/// The digest of one chunk: its bricks sorted by `(z, y, x)` with their revisions and layers.
/// The input may be in any order. An empty chunk has no digest; it is simply absent from the
/// volume's list.
pub fn chunk_digest(chunk: ChunkKey, bricks: &[CanonicalBrick]) -> Hash32 {
    digest_chunk_refs(chunk, bricks.iter().collect())
}

/// One brick of a chunk whose bricks each carry exactly one layer, as
/// [`chunk_digest_single_layer`] consumes it.
#[derive(Debug, Clone, Copy)]
pub struct SingleLayerBrick<'a> {
    pub coord: BrickCoord,
    pub revision: Revision,
    pub layer_kind: u16,
    pub layer_bytes: &'a [u8],
}

/// [`chunk_digest`] for bricks that each carry exactly one layer, without building a
/// [`CanonicalBrick`] (two heap allocations) for each. It writes the same bytes in the same order,
/// so the digests are equal; [`chunk_digest`] stays the specification and a differential test keeps
/// the two in step.
///
/// `bricks` must already be in canonical `(z, y, x)` order, each coordinate once, every brick in
/// `chunk`, and its length exact (the encoding is length-prefixed); all are checked.
pub fn chunk_digest_single_layer<'a>(
    chunk: ChunkKey,
    bricks: impl ExactSizeIterator<Item = SingleLayerBrick<'a>>,
) -> Hash32 {
    let count = bricks.len();
    let mut w = CanonicalWriter::with_domain(TOPOLOGY_CHUNK_DOMAIN);
    w.i64(chunk.0).i64(chunk.1).i64(chunk.2);
    w.seq(count);
    let mut seen = 0usize;
    let mut previous = None;
    for brick in bricks {
        let key = brick.coord.sort_key();
        assert!(
            previous.is_none_or(|p| p < key),
            "bricks must be unique and in canonical order"
        );
        assert_eq!(chunk_of(brick.coord), chunk, "a brick in the wrong chunk");
        previous = Some(key);
        seen += 1;
        w.i64(brick.coord.x).i64(brick.coord.y).i64(brick.coord.z);
        w.u64(brick.revision.get());
        w.seq(1);
        w.u16(brick.layer_kind);
        w.blob(brick.layer_bytes);
    }
    assert_eq!(seen, count, "the brick iterator's length was not exact");
    w.hash()
}

/// The topology hash of volumes given by their chunk digests. `chunks` lists every non-empty chunk
/// of a volume with its [`chunk_digest`], in canonical order (by `(z, y, x)` of the chunk key),
/// each chunk once; the volumes may come in any order.
pub fn topology_hash_from_chunks(volumes: &[(HashedVolume, &[(ChunkKey, Hash32)])]) -> Hash32 {
    let mut order: Vec<&(HashedVolume, &[(ChunkKey, Hash32)])> = volumes.iter().collect();
    order.sort_by_key(|(v, _)| v.volume_id.get());

    let mut w = CanonicalWriter::with_domain(TOPOLOGY_DOMAIN);
    w.seq(order.len());
    for (volume, chunks) in order {
        w.u64(volume.volume_id.get());
        w.u8(volume.cell_size.to_u8());
        volume.owner.write(&mut w);
        w.seq(chunks.len());
        let mut previous = None;
        for (key, digest) in chunks.iter() {
            assert!(
                previous.is_none_or(|p| p < chunk_order(*key)),
                "chunks must be unique and in canonical order"
            );
            previous = Some(chunk_order(*key));
            w.i64(key.0).i64(key.1).i64(key.2);
            w.blob(&digest.0);
        }
    }
    w.hash()
}

/// Hashes authoritative topology. The input may be in any order; volumes are sorted by id and
/// bricks by `(z, y, x)`, so equal state always produces an equal digest.
///
/// **Layout (`v2`).** Bricks are grouped into chunks of 8 x 8 x 8 ([`HASH_CHUNK_SHIFT`]).
/// A chunk's digest ([`chunk_digest`]) covers its bricks in order, each as its coordinates,
/// revision and layers sorted by `kind`. The top-level hash covers, per volume, its id, cell-size
/// code and owner, then its non-empty chunks in order, each as its key and digest. Because a
/// change to a brick alters only its chunk's digest, a cache can recompute just the chunks that
/// changed ([`ChunkDigestCache`]).
pub fn canonical_topology_hash(volumes: &[CanonicalVolume]) -> Hash32 {
    let mut digests: Vec<(HashedVolume, Vec<(ChunkKey, Hash32)>)> = Vec::new();
    for volume in volumes {
        let mut by_chunk: std::collections::BTreeMap<
            (i64, i64, i64),
            (ChunkKey, Vec<&CanonicalBrick>),
        > = std::collections::BTreeMap::new();
        for brick in &volume.bricks {
            let key = chunk_of(brick.coord);
            by_chunk
                .entry(chunk_order(key))
                .or_insert_with(|| (key, Vec::new()))
                .1
                .push(brick);
        }
        digests.push((
            HashedVolume {
                volume_id: volume.volume_id,
                cell_size: volume.cell_size,
                owner: volume.owner,
            },
            by_chunk
                .into_values()
                .map(|(key, bricks)| (key, digest_chunk_refs(key, bricks)))
                .collect(),
        ));
    }
    let borrowed: Vec<(HashedVolume, &[(ChunkKey, Hash32)])> = digests
        .iter()
        .map(|(volume, chunks)| (*volume, chunks.as_slice()))
        .collect();
    topology_hash_from_chunks(&borrowed)
}

/// Remembers each chunk's digest of one volume, keyed by the stamps of the stores the chunk's
/// bricks come from, so a hash after a small change recomputes only the chunks that changed.
///
/// A stamp is a value that is replaced by every mutation of a chunk and shared by clones until
/// either is written (`spall_voxel::Volume::chunk_stamps`). Two equal stamp pairs mean identical
/// contents, however the volume was copied, so validity needs no report of what was modified
/// and is independent of which volume object is hashed.
#[derive(Debug, Default, Clone)]
pub struct ChunkDigestCache {
    entries: std::collections::BTreeMap<(i64, i64, i64), CachedChunk>,
}

#[derive(Debug, Clone)]
struct CachedChunk {
    key: ChunkKey,
    stamps: (u64, u64),
    /// `None` when the chunk holds no brick at all (it then has no digest).
    digest: Option<Hash32>,
}

impl ChunkDigestCache {
    /// Brings the cache up to date. `current` lists every chunk that exists in either store with
    /// its pair of stamps (`0` for a store that has none of it). `digest_of` computes the digest of
    /// one chunk (`None` if it holds nothing) and is called only for chunks whose stamps are new
    /// or changed. If it fails, the cache is left exactly as it was.
    pub fn refresh<E>(
        &mut self,
        current: impl IntoIterator<Item = (ChunkKey, (u64, u64))>,
        mut digest_of: impl FnMut(ChunkKey) -> Result<Option<Hash32>, E>,
    ) -> Result<(), E> {
        let mut next = std::collections::BTreeMap::new();
        for (key, stamps) in current {
            let order = chunk_order(key);
            let entry = match self.entries.get(&order) {
                Some(cached) if cached.stamps == stamps => cached.clone(),
                _ => CachedChunk {
                    key,
                    stamps,
                    digest: digest_of(key)?,
                },
            };
            next.insert(order, entry);
        }
        self.entries = next;
        Ok(())
    }

    /// The non-empty chunks and their digests, in canonical order, ready for
    /// [`topology_hash_from_chunks`].
    pub fn chunks(&self) -> Vec<(ChunkKey, Hash32)> {
        self.entries
            .values()
            .filter_map(|c| c.digest.map(|d| (c.key, d)))
            .collect()
    }

    /// Chunks currently remembered (including empty ones), for diagnostics and tests.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Hashes a validated material manifest. This is the "exact content manifest
/// hash" exchanged in the handshake.
pub fn content_manifest_hash(manifest: &MaterialManifest) -> Hash32 {
    let mut w = CanonicalWriter::with_domain(MANIFEST_DOMAIN);
    w.blob(&manifest.canonical_bytes());
    w.hash()
}

/// Combines the material registry with the game-owned asset manifest for
/// sandbox handshake compatibility. Engine-only callers retain the original
/// material-only hash through [`content_manifest_hash`].
pub fn content_manifest_hash_with_assets(
    materials: &MaterialManifest,
    asset_manifest_hash: Hash32,
) -> Hash32 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"spall.content-with-assets.v1");
    hasher.update(&content_manifest_hash(materials).0);
    hasher.update(&asset_manifest_hash.0);
    Hash32(*hasher.finalize().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{MaterialDef, MaterialFlags, MaterialId, RenderProps, SimProps, VolumeId};

    fn layer(kind: u16, bytes: &[u8]) -> CanonicalLayer {
        CanonicalLayer {
            kind,
            bytes: bytes.to_vec(),
        }
    }

    fn brick(x: i64, y: i64, z: i64, rev: u64, layers: Vec<CanonicalLayer>) -> CanonicalBrick {
        CanonicalBrick {
            coord: BrickCoord::new(x, y, z),
            revision: Revision(rev),
            layers,
        }
    }

    fn sample() -> Vec<CanonicalVolume> {
        vec![
            CanonicalVolume {
                volume_id: VolumeId::new(1).unwrap(),
                cell_size: CellSizeCode::Quarter,
                owner: CanonicalOwner::Terrain,
                bricks: vec![
                    brick(0, 0, 0, 4, vec![layer(0, b"mats0"), layer(3, b"bonds0")]),
                    brick(-1, 2, 0, 9, vec![layer(0, b"mats1")]),
                ],
            },
            CanonicalVolume {
                volume_id: VolumeId::new(7).unwrap(),
                cell_size: CellSizeCode::Sixteenth,
                owner: CanonicalOwner::Body(EntityId::new(42).unwrap()),
                bricks: vec![brick(5, 5, 5, 1, vec![layer(0, b"child")])],
            },
        ]
    }

    #[test]
    fn reordered_input_hashes_identically() {
        let ordered = sample();
        let hash_a = canonical_topology_hash(&ordered);

        let mut shuffled = sample();
        shuffled.reverse();
        shuffled[0].bricks.reverse();
        shuffled[1].bricks[0].layers.reverse();
        shuffled[1].bricks[1].layers.reverse();
        let hash_b = canonical_topology_hash(&shuffled);

        assert_eq!(hash_a, hash_b);
    }

    #[test]
    fn distinct_state_hashes_differently() {
        let base = sample();
        let mut changed = sample();
        changed[0].bricks[0].revision = Revision(5);
        assert_ne!(
            canonical_topology_hash(&base),
            canonical_topology_hash(&changed)
        );

        let mut owner_changed = sample();
        owner_changed[0].owner = CanonicalOwner::Body(EntityId::new(1).unwrap());
        assert_ne!(
            canonical_topology_hash(&base),
            canonical_topology_hash(&owner_changed)
        );
    }

    #[test]
    fn topology_hash_is_stable_across_runs() {
        // Pin the digest so an accidental layout change is caught.
        let hash = canonical_topology_hash(&sample());
        assert_eq!(
            hash.to_string(),
            "13da6535ee2862716acada1b99ee59783a7bea5ac259b9d112cfe4bbd92b5481"
        );
    }

    #[test]
    fn manifest_hash_matches_direct_blake3_of_canonical_bytes() {
        let manifest = MaterialManifest::validated(vec![
            MaterialDef {
                id: MaterialId::AIR,
                name: "air".into(),
                render: RenderProps {
                    albedo: [0.0; 3],
                    roughness: 1.0,
                    metalness: 0.0,
                    emissive: [0.0; 3],
                },
                sim: SimProps {
                    density_kg_m3: 0.0,
                    friction: 0.0,
                    restitution: 0.0,
                    hardness: 0.0,
                    bond_strength: 0.0,
                    flags: MaterialFlags::NONE,
                },
            },
            MaterialDef {
                id: MaterialId(1),
                name: "stone".into(),
                render: RenderProps {
                    albedo: [0.5, 0.5, 0.5],
                    roughness: 0.9,
                    metalness: 0.0,
                    emissive: [0.0; 3],
                },
                sim: SimProps {
                    density_kg_m3: 2600.0,
                    friction: 0.8,
                    restitution: 0.1,
                    hardness: 4.0,
                    bond_strength: 12.0,
                    flags: MaterialFlags(MaterialFlags::OPAQUE.0 | MaterialFlags::COLLIDES.0),
                },
            },
        ])
        .unwrap();

        let mut w = CanonicalWriter::with_domain(MANIFEST_DOMAIN);
        w.blob(&manifest.canonical_bytes());
        assert_eq!(content_manifest_hash(&manifest), w.hash());
    }

    // ---- v2: chunked layout ------------------------------------------------------------------

    /// Deterministic pseudo-random volumes over several chunks, negative coordinates included.
    fn random_volume(seed: u64, id: u64, bricks: usize) -> CanonicalVolume {
        let mut state = seed | 1;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut seen = std::collections::BTreeSet::new();
        let mut out = Vec::new();
        while out.len() < bricks {
            let (x, y, z) = (
                (next() % 40) as i64 - 20,
                (next() % 24) as i64 - 12,
                (next() % 40) as i64 - 20,
            );
            if !seen.insert((x, y, z)) {
                continue;
            }
            let mut content = [0u8; 32];
            content.iter_mut().for_each(|b| *b = next() as u8);
            out.push(brick(x, y, z, next() % 500, vec![layer(0, &content)]));
        }
        CanonicalVolume {
            volume_id: VolumeId::new(id).unwrap(),
            cell_size: CellSizeCode::Quarter,
            owner: CanonicalOwner::Terrain,
            bricks: out,
        }
    }

    /// The chunk digests of `volume`, computed by the single-layer path from sorted bricks.
    fn single_layer_chunks(volume: &CanonicalVolume) -> Vec<(ChunkKey, Hash32)> {
        let mut by_chunk: std::collections::BTreeMap<(i64, i64, i64), Vec<&CanonicalBrick>> =
            std::collections::BTreeMap::new();
        for b in &volume.bricks {
            let k = chunk_of(b.coord);
            by_chunk.entry((k.2, k.1, k.0)).or_default().push(b);
        }
        by_chunk
            .into_values()
            .map(|mut bricks| {
                bricks.sort_by_key(|b| b.coord.sort_key());
                let key = chunk_of(bricks[0].coord);
                let digest = chunk_digest_single_layer(
                    key,
                    bricks.iter().map(|b| SingleLayerBrick {
                        coord: b.coord,
                        revision: b.revision,
                        layer_kind: b.layers[0].kind,
                        layer_bytes: &b.layers[0].bytes,
                    }),
                );
                (key, digest)
            })
            .collect()
    }

    fn header_of(v: &CanonicalVolume) -> HashedVolume {
        HashedVolume {
            volume_id: v.volume_id,
            cell_size: v.cell_size,
            owner: v.owner,
        }
    }

    #[test]
    fn three_independent_computations_of_the_v2_hash_agree() {
        for (seed, bricks) in [(1, 0), (2, 1), (3, 40), (4, 900)] {
            let volumes = vec![
                random_volume(seed, 3, bricks),
                random_volume(seed + 100, 1, bricks / 2),
            ];
            // 1: the reference over whole volumes.
            let reference = canonical_topology_hash(&volumes);
            // 2: per-volume single-layer chunk digests, combined.
            let chunks: Vec<Vec<(ChunkKey, Hash32)>> =
                volumes.iter().map(single_layer_chunks).collect();
            let borrowed: Vec<_> = volumes
                .iter()
                .zip(&chunks)
                .map(|(v, c)| (header_of(v), c.as_slice()))
                .collect();
            assert_eq!(
                topology_hash_from_chunks(&borrowed),
                reference,
                "{bricks} bricks"
            );
            // 3: the general chunk digest agrees with the single-layer one, chunk by chunk.
            for (v, c) in volumes.iter().zip(&chunks) {
                for (key, digest) in c {
                    let members: Vec<CanonicalBrick> = v
                        .bricks
                        .iter()
                        .filter(|b| chunk_of(b.coord) == *key)
                        .cloned()
                        .collect();
                    assert_eq!(chunk_digest(*key, &members), *digest);
                }
            }
        }
    }

    #[test]
    fn chunks_floor_and_bricks_either_side_of_a_boundary_are_in_different_chunks() {
        assert_eq!(chunk_of(BrickCoord::new(0, 7, 8)), (0, 0, 1));
        assert_eq!(chunk_of(BrickCoord::new(-1, -8, -9)), (-1, -1, -2));
        assert_ne!(
            chunk_of(BrickCoord::new(7, 0, 0)),
            chunk_of(BrickCoord::new(8, 0, 0))
        );
        assert_ne!(
            chunk_of(BrickCoord::new(-1, 0, 0)),
            chunk_of(BrickCoord::new(0, 0, 0))
        );
    }

    #[test]
    fn an_empty_volume_hashes_with_an_empty_chunk_list() {
        let empty = CanonicalVolume {
            volume_id: VolumeId::new(1).unwrap(),
            cell_size: CellSizeCode::Quarter,
            owner: CanonicalOwner::Terrain,
            bricks: Vec::new(),
        };
        assert_eq!(
            canonical_topology_hash(std::slice::from_ref(&empty)),
            topology_hash_from_chunks(&[(header_of(&empty), &[])])
        );
    }

    #[test]
    fn a_change_to_one_brick_moves_the_hash_and_only_its_chunk_digest() {
        let before = random_volume(9, 1, 300);
        let mut after = before.clone();
        after.bricks[17].revision = Revision(after.bricks[17].revision.get() + 1);
        let changed = chunk_of(after.bricks[17].coord);
        let (a, b) = (single_layer_chunks(&before), single_layer_chunks(&after));
        assert_eq!(a.len(), b.len());
        for ((ka, da), (kb, db)) in a.iter().zip(&b) {
            assert_eq!(ka, kb);
            assert_eq!(da == db, *ka != changed, "only the changed chunk moves");
        }
        assert_ne!(
            canonical_topology_hash(&[before]),
            canonical_topology_hash(&[after])
        );
    }

    #[test]
    fn a_brick_on_the_other_side_of_a_chunk_boundary_hashes_differently() {
        let mut volume = random_volume(5, 1, 20);
        volume
            .bricks
            .push(brick(7, 100, 100, 1, vec![layer(0, b"x")]));
        let with_seven = canonical_topology_hash(std::slice::from_ref(&volume));
        volume.bricks.last_mut().unwrap().coord = BrickCoord::new(8, 100, 100);
        let with_eight = canonical_topology_hash(std::slice::from_ref(&volume));
        assert_ne!(with_seven, with_eight);
    }

    #[test]
    fn the_cache_recomputes_only_chunks_whose_stamps_changed() {
        let volume = random_volume(11, 1, 400);
        let single = single_layer_chunks(&volume);
        let stamps: Vec<(ChunkKey, (u64, u64))> = single
            .iter()
            .enumerate()
            .map(|(i, (k, _))| (*k, (i as u64 + 1, 0)))
            .collect();
        let digest_for = |key: ChunkKey| single.iter().find(|(k, _)| *k == key).map(|(_, d)| *d);

        let mut cache = ChunkDigestCache::default();
        let mut calls = 0;
        cache
            .refresh(stamps.clone(), |k| {
                calls += 1;
                Ok::<_, ()>(digest_for(k))
            })
            .unwrap();
        assert_eq!(calls, single.len(), "a cold cache computes every chunk");
        assert_eq!(cache.chunks(), single);

        // Unchanged stamps: nothing is recomputed.
        calls = 0;
        cache
            .refresh(stamps.clone(), |k| {
                calls += 1;
                Ok::<_, ()>(digest_for(k))
            })
            .unwrap();
        assert_eq!(calls, 0);

        // One chunk's stamp moves; another chunk disappears; a new one appears.
        let mut next = stamps.clone();
        next[3].1.0 += 1000;
        let gone = next.remove(7).0;
        next.push(((99, 99, 99), (5, 5)));
        calls = 0;
        cache
            .refresh(next.clone(), |k| {
                calls += 1;
                Ok::<_, ()>(if k == (99, 99, 99) {
                    None
                } else {
                    digest_for(k)
                })
            })
            .unwrap();
        assert_eq!(calls, 2, "the changed chunk and the new one");
        assert!(
            cache
                .chunks()
                .iter()
                .all(|(k, _)| *k != gone && *k != (99, 99, 99))
        );
        assert_eq!(
            cache.len(),
            next.len(),
            "empty chunks are remembered, not listed"
        );

        // A failure leaves the cache exactly as it was.
        let before = cache.chunks();
        let mut moved = next.clone();
        moved[0].1.0 += 1;
        assert!(
            cache
                .refresh(moved, |_| Err::<Option<Hash32>, _>("boom"))
                .is_err()
        );
        assert_eq!(cache.chunks(), before);
        assert_eq!(cache.len(), next.len());
    }

    #[test]
    #[should_panic(expected = "canonical order")]
    fn single_layer_digest_refuses_unsorted_bricks() {
        let b = |x| SingleLayerBrick {
            coord: BrickCoord::new(x, 0, 0),
            revision: Revision(1),
            layer_kind: 0,
            layer_bytes: &[0; 32],
        };
        chunk_digest_single_layer((0, 0, 0), vec![b(3), b(2)].into_iter());
    }

    #[test]
    #[should_panic(expected = "wrong chunk")]
    fn single_layer_digest_refuses_a_brick_from_another_chunk() {
        chunk_digest_single_layer(
            (0, 0, 0),
            vec![SingleLayerBrick {
                coord: BrickCoord::new(8, 0, 0),
                revision: Revision(1),
                layer_kind: 0,
                layer_bytes: &[0; 32],
            }]
            .into_iter(),
        );
    }

    #[test]
    #[should_panic(expected = "canonical order")]
    fn combining_refuses_unsorted_chunks() {
        let header = HashedVolume {
            volume_id: VolumeId::new(1).unwrap(),
            cell_size: CellSizeCode::Quarter,
            owner: CanonicalOwner::Terrain,
        };
        let chunks = [((1, 0, 0), Hash32::ZERO), ((0, 0, 0), Hash32::ZERO)];
        topology_hash_from_chunks(&[(header, &chunks)]);
    }
}
