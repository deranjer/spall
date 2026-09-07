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

/// Domain separation tag; bump the trailing version if the layout changes.
pub const TOPOLOGY_DOMAIN: &[u8] = b"spall.topology.v1";
/// Domain separation tag for the content manifest hash.
pub const MANIFEST_DOMAIN: &[u8] = b"spall.manifest.v1";

/// Hashes authoritative topology. The input may be in any order; volumes are
/// sorted by id, bricks by `(z, y, x)`, and layers by `kind` before encoding,
/// so equal state always produces an equal digest.
pub fn canonical_topology_hash(volumes: &[CanonicalVolume]) -> Hash32 {
    let mut order: Vec<&CanonicalVolume> = volumes.iter().collect();
    order.sort_by_key(|v| v.volume_id.get());

    let mut w = CanonicalWriter::with_domain(TOPOLOGY_DOMAIN);
    w.seq(order.len());
    for volume in order {
        w.u64(volume.volume_id.get());
        w.u8(volume.cell_size.to_u8());
        volume.owner.write(&mut w);

        let mut bricks: Vec<&CanonicalBrick> = volume.bricks.iter().collect();
        bricks.sort_by_key(|b| b.coord.sort_key());
        w.seq(bricks.len());
        for brick in bricks {
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
    }
    w.hash()
}

/// Hashes a validated material manifest. This is the "exact content manifest
/// hash" exchanged in the handshake.
pub fn content_manifest_hash(manifest: &MaterialManifest) -> Hash32 {
    let mut w = CanonicalWriter::with_domain(MANIFEST_DOMAIN);
    w.blob(&manifest.canonical_bytes());
    w.hash()
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
            "f608a1808d28deb0afc09c01da6c28048e3ad38de032da544606dc29a22e4e9b"
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
}
