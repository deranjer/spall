//! Candidate ENG-122 canonical component-amount payload. Not a registered wire
//! record or production save format. The owning snapshot must supply an exact
//! occupancy/grid identity; anchors alone cannot identify component membership.

pub const VERSION: u16 = 1;
pub const MAX_COMPONENTS: usize = 200_000;
const HEADER_BYTES: usize = 90;
const ENTRY_BYTES: usize = 44;

/// Exact geometry identity for the candidate payload. `open` is one byte per
/// fine voxel in x-fastest order: 0 solid, 1 open. Same anchors and capacities
/// are insufficient; every fine occupancy decision participates in this hash.
pub fn geometry_hash(
    origin: [i64; 3],
    dimensions: [u32; 3],
    coarsen: u32,
    voxel_size_m_bits: u64,
    open: &[u8],
) -> Result<[u8; 32], AmountCodecError> {
    let header = ComponentAmounts {
        origin,
        dimensions,
        coarsen,
        voxel_size_m_bits,
        geometry_hash: [0; 32],
        components: Vec::new(),
    };
    header.validate()?;
    let count = dimensions
        .into_iter()
        .map(|n| n as usize)
        .product::<usize>();
    if open.len() != count || open.iter().any(|v| *v > 1) {
        return Err(AmountCodecError::Invalid);
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"Spall open component geometry v1\0");
    for v in origin {
        hasher.update(&v.to_le_bytes());
    }
    for v in dimensions {
        hasher.update(&v.to_le_bytes());
    }
    hasher.update(&coarsen.to_le_bytes());
    hasher.update(&voxel_size_m_bits.to_le_bytes());
    hasher.update(open);
    Ok(*hasher.finalize().as_bytes())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentAmount {
    /// Stable fine global coordinate, not a snapshot array index.
    pub anchor: [i64; 3],
    pub open_voxels: u32,
    pub water_m3_bits: u64,
    pub trapped_m3_bits: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentAmounts {
    pub origin: [i64; 3],
    pub dimensions: [u32; 3],
    pub coarsen: u32,
    pub voxel_size_m_bits: u64,
    /// Canonical hash of exact fine occupancy, origin, dimensions and factor.
    /// Computed/verified by the owner; never inferred from component anchors.
    pub geometry_hash: [u8; 32],
    /// Strict (z,y,x) order, independently retaining both sides of thin walls.
    pub components: Vec<ComponentAmount>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AmountCodecError {
    #[error("invalid or unsupported component-amount payload")]
    Invalid,
    #[error("component-amount payload exceeds bounds")]
    Limit,
    #[error("component-amount payload geometry differs from owning snapshot")]
    Geometry,
}

impl ComponentAmounts {
    pub fn validate(&self) -> Result<(), AmountCodecError> {
        let size = f64::from_bits(self.voxel_size_m_bits);
        let voxel_volume = size.powi(3);
        if !(1..=8).contains(&self.coarsen)
            || !size.is_finite()
            || size <= 0.0
            || !voxel_volume.is_finite()
            || voxel_volume <= 0.0
        {
            return Err(AmountCodecError::Invalid);
        }
        let mut fine_count = 1usize;
        for (axis, n) in self.dimensions.into_iter().enumerate() {
            if n == 0
                || n % self.coarsen != 0
                || self.origin[axis].checked_add(i64::from(n) - 1).is_none()
            {
                return Err(AmountCodecError::Invalid);
            }
            fine_count = fine_count
                .checked_mul(n as usize)
                .ok_or(AmountCodecError::Limit)?;
        }
        if fine_count > 32 * 1024 * 1024
            || self.components.len() > MAX_COMPONENTS
            || self.components.len() > fine_count
        {
            return Err(AmountCodecError::Limit);
        }
        let mut previous = None;
        for c in &self.components {
            let key = (c.anchor[2], c.anchor[1], c.anchor[0]);
            if previous.is_some_and(|p| p >= key)
                || c.open_voxels == 0
                || c.open_voxels > self.coarsen.pow(3)
            {
                return Err(AmountCodecError::Invalid);
            }
            previous = Some(key);
            for axis in 0..3 {
                let local = c.anchor[axis]
                    .checked_sub(self.origin[axis])
                    .ok_or(AmountCodecError::Invalid)?;
                if local < 0 || local >= i64::from(self.dimensions[axis]) {
                    return Err(AmountCodecError::Invalid);
                }
            }
            let water = f64::from_bits(c.water_m3_bits);
            let trapped = f64::from_bits(c.trapped_m3_bits);
            let capacity = f64::from(c.open_voxels) * voxel_volume;
            if !capacity.is_finite()
                || !water.is_finite()
                || water < 0.0
                || water > capacity
                || !trapped.is_finite()
                || trapped < 0.0
            {
                return Err(AmountCodecError::Invalid);
            }
        }
        Ok(())
    }

    /// Explicit little-endian integer/IEEE-bit encoding, without Rust layout.
    pub fn encode(&self) -> Result<Vec<u8>, AmountCodecError> {
        self.validate()?;
        let mut out = Vec::with_capacity(HEADER_BYTES + ENTRY_BYTES * self.components.len());
        out.extend_from_slice(b"SCWA");
        out.extend_from_slice(&VERSION.to_le_bytes());
        for v in self.origin {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for v in self.dimensions {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&self.coarsen.to_le_bytes());
        out.extend_from_slice(&self.voxel_size_m_bits.to_le_bytes());
        out.extend_from_slice(&self.geometry_hash);
        out.extend_from_slice(&(self.components.len() as u32).to_le_bytes());
        for c in &self.components {
            for v in c.anchor {
                out.extend_from_slice(&v.to_le_bytes());
            }
            out.extend_from_slice(&c.open_voxels.to_le_bytes());
            out.extend_from_slice(&c.water_m3_bits.to_le_bytes());
            out.extend_from_slice(&c.trapped_m3_bits.to_le_bytes());
        }
        Ok(out)
    }

    /// Inspect version, count, exact byte length and geometry before allocating.
    /// The owner must subsequently validate each anchor/capacity against its
    /// rebuilt geometry, before atomically installing any recovered amounts.
    pub fn decode(bytes: &[u8], expected_geometry: [u8; 32]) -> Result<Self, AmountCodecError> {
        if bytes.len() < HEADER_BYTES || &bytes[..4] != b"SCWA" {
            return Err(AmountCodecError::Invalid);
        }
        let mut reader = Reader { bytes, pos: 4 };
        if u16::from_le_bytes(reader.take()) != VERSION {
            return Err(AmountCodecError::Invalid);
        }
        let origin = std::array::from_fn(|_| i64::from_le_bytes(reader.take()));
        let dimensions = std::array::from_fn(|_| u32::from_le_bytes(reader.take()));
        let coarsen = u32::from_le_bytes(reader.take());
        let voxel_size_m_bits = u64::from_le_bytes(reader.take());
        let geometry_hash = reader.take();
        let count = u32::from_le_bytes(reader.take()) as usize;
        if count > MAX_COMPONENTS {
            return Err(AmountCodecError::Limit);
        }
        if bytes.len() != HEADER_BYTES + ENTRY_BYTES * count {
            return Err(AmountCodecError::Invalid);
        }
        if geometry_hash != expected_geometry {
            return Err(AmountCodecError::Geometry);
        }
        // Validate domain/header before allocating entries as well.
        let mut result = Self {
            origin,
            dimensions,
            coarsen,
            voxel_size_m_bits,
            geometry_hash,
            components: Vec::new(),
        };
        result.validate()?;
        let fine_count = dimensions
            .into_iter()
            .map(|n| n as usize)
            .product::<usize>();
        if count > fine_count {
            return Err(AmountCodecError::Limit);
        }
        result.components.reserve_exact(count);
        for _ in 0..count {
            result.components.push(ComponentAmount {
                anchor: std::array::from_fn(|_| i64::from_le_bytes(reader.take())),
                open_voxels: u32::from_le_bytes(reader.take()),
                water_m3_bits: u64::from_le_bytes(reader.take()),
                trapped_m3_bits: u64::from_le_bytes(reader.take()),
            });
        }
        result.validate()?;
        Ok(result)
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}
impl Reader<'_> {
    // Only used after header and exact record length checks.
    fn take<const N: usize>(&mut self) -> [u8; N] {
        let mut out = [0; N];
        out.copy_from_slice(&self.bytes[self.pos..self.pos + N]);
        self.pos += N;
        out
    }
}
