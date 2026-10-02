//! Independently versioned, bounded vegetation presentation keyframes.
//! Canonical simulation state is stored separately and is never client authority.
use crate::{Record, RecordError, WireTag};
use serde::{Deserialize, Serialize};
pub const CHUNK: usize = 48 * 1024;
pub const MAX_BYTES: usize = 4 * 1024 * 1024;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VegetationSnapshot {
    pub version: u16,
    pub tick: u64,
    pub index: u16,
    pub count: u16,
    pub digest: [u8; 32],
    pub payload: Vec<u8>,
}
impl Record for VegetationSnapshot {
    const TAG: WireTag = WireTag::VegetationSnapshot;
    fn validate(&self) -> Result<(), RecordError> {
        if self.version != 1
            || self.count == 0
            || usize::from(self.count) > MAX_BYTES.div_ceil(CHUNK)
            || self.index >= self.count
            || self.payload.is_empty()
            || self.payload.len() > CHUNK
            || (self.index + 1 < self.count && self.payload.len() != CHUNK)
        {
            return Err(RecordError::Inconsistent("invalid vegetation chunk"));
        }
        Ok(())
    }
}
pub fn chunks(tick: u64, bytes: &[u8]) -> Result<Vec<VegetationSnapshot>, String> {
    if bytes.is_empty() || bytes.len() > MAX_BYTES {
        return Err("vegetation keyframe byte limit".into());
    }
    let count = bytes.len().div_ceil(CHUNK) as u16;
    let digest = *blake3::hash(bytes).as_bytes();
    Ok(bytes
        .chunks(CHUNK)
        .enumerate()
        .map(|(index, payload)| VegetationSnapshot {
            version: 1,
            tick,
            index: index as u16,
            count,
            digest,
            payload: payload.to_vec(),
        })
        .collect())
}
#[derive(Default)]
pub struct VegetationAssembler {
    tick: Option<u64>,
    digest: [u8; 32],
    count: u16,
    bytes: Vec<u8>,
    next: u16,
}
impl VegetationAssembler {
    pub fn push(&mut self, c: VegetationSnapshot) -> Result<Option<Vec<u8>>, String> {
        c.validate().map_err(|e| e.to_string())?;
        if c.index == 0 {
            self.tick = Some(c.tick);
            self.digest = c.digest;
            self.count = c.count;
            self.bytes.clear();
            self.next = 0;
        }
        if self.tick != Some(c.tick)
            || self.digest != c.digest
            || self.count != c.count
            || self.next != c.index
        {
            self.bytes.clear();
            return Err("inconsistent vegetation keyframe".into());
        }
        if self.bytes.len() + c.payload.len() > MAX_BYTES {
            return Err("vegetation keyframe byte limit".into());
        }
        self.bytes.extend(c.payload);
        self.next += 1;
        if self.next == self.count {
            let bytes = std::mem::take(&mut self.bytes);
            if blake3::hash(&bytes).as_bytes() != &self.digest {
                return Err("vegetation checksum".into());
            }
            Ok(Some(bytes))
        } else {
            Ok(None)
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chunks_validate_and_repair_after_interruption() {
        let bytes = vec![7; CHUNK + 3];
        let c = chunks(9, &bytes).unwrap();
        let mut a = VegetationAssembler::default();
        assert!(a.push(c[1].clone()).is_err());
        assert!(a.push(c[0].clone()).unwrap().is_none());
        assert_eq!(a.push(c[1].clone()).unwrap().unwrap(), bytes);
        let encoded = crate::encode_control(&c[0]).unwrap();
        assert_eq!(
            crate::decode_control::<VegetationSnapshot>(&encoded).unwrap(),
            c[0]
        );
    }
}
