//! Chunked, copy-on-write keyed storage with a change stamp per chunk.
//!
//! Entries are grouped into chunks of 8 x 8 x 8 bricks held behind `Arc`s. Cloning a store copies
//! one handle per chunk instead of every entry, and a write copies only the chunk it lands in, so
//! a clone that is edited pays for what it touches rather than for the whole world.
//!
//! Every chunk carries a **stamp**, a process-unique value that is replaced by every mutation of
//! that chunk. Two chunks with the same stamp hold identical entries (a clone shares its source's
//! chunk, and so its stamp, until either is written). Anything derived from a chunk's contents,
//! such as the topology hash's per-chunk digest, can therefore be reused exactly while the stamp
//! is unchanged, with no caller having to report what it modified.
//!
//! Iteration visits chunk by chunk, not in global key order; callers that need canonical order
//! sort.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Raw `(x, y, z)` brick key.
pub type BrickKey = (i64, i64, i64);

/// Key of a chunk: the brick key shifted right by [`CHUNK_SHIFT`] on each axis.
pub type ChunkKey = (i64, i64, i64);

/// Bricks per chunk edge, as a shift: chunks are 8 x 8 x 8 bricks. The topology hash groups
/// bricks into chunks with the same shift, so the two must agree
/// (`spall_protocol::HASH_CHUNK_SHIFT`; a test in `spall_sim` compares them).
pub const CHUNK_SHIFT: u32 = 3;

static NEXT_STAMP: AtomicU64 = AtomicU64::new(1);

/// A fresh process-unique stamp. Never zero, so zero can mean "absent".
pub(crate) fn fresh_stamp() -> u64 {
    NEXT_STAMP.fetch_add(1, Ordering::Relaxed)
}

/// The chunk that holds `key`.
pub fn chunk_of(key: BrickKey) -> ChunkKey {
    // An arithmetic shift is a floor division, so negative coordinates chunk correctly.
    (
        key.0 >> CHUNK_SHIFT,
        key.1 >> CHUNK_SHIFT,
        key.2 >> CHUNK_SHIFT,
    )
}

#[derive(Debug, Clone)]
pub(crate) struct Chunk<V> {
    slots: BTreeMap<BrickKey, V>,
    stamp: u64,
}

/// See the module documentation.
#[derive(Debug, Clone)]
pub(crate) struct ChunkStore<V> {
    chunks: BTreeMap<ChunkKey, Arc<Chunk<V>>>,
    len: usize,
}

impl<V> Default for ChunkStore<V> {
    fn default() -> Self {
        Self {
            chunks: BTreeMap::new(),
            len: 0,
        }
    }
}

impl<V: Clone> ChunkStore<V> {
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn get(&self, key: &BrickKey) -> Option<&V> {
        self.chunks.get(&chunk_of(*key))?.slots.get(key)
    }

    /// Inserts `value`, returning the entry it replaced.
    pub(crate) fn insert(&mut self, key: BrickKey, value: V) -> Option<V> {
        let chunk = self.chunks.entry(chunk_of(key)).or_insert_with(|| {
            Arc::new(Chunk {
                slots: BTreeMap::new(),
                stamp: 0,
            })
        });
        let chunk = Arc::make_mut(chunk);
        let replaced = chunk.slots.insert(key, value);
        chunk.stamp = fresh_stamp();
        if replaced.is_none() {
            self.len += 1;
        }
        replaced
    }

    pub(crate) fn remove(&mut self, key: &BrickKey) -> Option<V> {
        let chunk_key = chunk_of(*key);
        let chunk = self.chunks.get_mut(&chunk_key)?;
        // Do not copy a shared chunk just to find the key absent.
        if !chunk.slots.contains_key(key) {
            return None;
        }
        let chunk = Arc::make_mut(chunk);
        let removed = chunk.slots.remove(key);
        chunk.stamp = fresh_stamp();
        if chunk.slots.is_empty() {
            self.chunks.remove(&chunk_key);
        }
        self.len -= 1;
        removed
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&BrickKey, &V)> {
        self.chunks.values().flat_map(|chunk| chunk.slots.iter())
    }

    pub(crate) fn values(&self) -> impl Iterator<Item = &V> {
        self.chunks.values().flat_map(|chunk| chunk.slots.values())
    }

    /// Every non-empty chunk with its stamp.
    pub(crate) fn chunk_stamps(&self) -> impl Iterator<Item = (ChunkKey, u64)> + '_ {
        self.chunks.iter().map(|(&key, chunk)| (key, chunk.stamp))
    }

    /// The entries of one chunk, in no particular order.
    pub(crate) fn chunk_entries(&self, chunk: ChunkKey) -> impl Iterator<Item = (&BrickKey, &V)> {
        self.chunks
            .get(&chunk)
            .into_iter()
            .flat_map(|chunk| chunk.slots.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spread() -> Vec<BrickKey> {
        let mut keys = Vec::new();
        for x in [-9, -8, -1, 0, 7, 8, 17] {
            for y in [-8, -1, 0, 8] {
                for z in [-17, -1, 0, 7, 8] {
                    keys.push((x, y, z));
                }
            }
        }
        keys
    }

    #[test]
    fn chunks_floor_toward_negative_infinity() {
        assert_eq!(chunk_of((0, 7, 8)), (0, 0, 1));
        assert_eq!(chunk_of((-1, -8, -9)), (-1, -1, -2));
        assert_eq!(chunk_of((-8, 15, 16)), (-1, 1, 2));
    }

    #[test]
    fn len_get_insert_and_remove_agree_with_a_plain_map() {
        let mut store = ChunkStore::<u32>::default();
        let mut model = BTreeMap::new();
        for (i, key) in spread().into_iter().enumerate() {
            assert_eq!(store.insert(key, i as u32), model.insert(key, i as u32));
        }
        // Replace some, remove some, remove some that are not there.
        for (i, key) in spread().into_iter().enumerate() {
            match i % 3 {
                0 => assert_eq!(store.insert(key, 1000), model.insert(key, 1000)),
                1 => assert_eq!(store.remove(&key), model.remove(&key)),
                _ => {}
            }
        }
        assert_eq!(store.remove(&(9_999, 0, 0)), None);
        assert_eq!(store.len(), model.len());
        for (key, value) in &model {
            assert_eq!(store.get(key), Some(value));
        }
        let mut seen: Vec<_> = store.iter().map(|(&k, &v)| (k, v)).collect();
        seen.sort();
        let expected: Vec<_> = model.into_iter().collect();
        assert_eq!(seen, expected);
    }

    #[test]
    fn a_chunk_stamp_changes_exactly_when_that_chunk_changes() {
        let mut store = ChunkStore::<u32>::default();
        store.insert((0, 0, 0), 1);
        store.insert((20, 0, 0), 2);
        let stamps = |s: &ChunkStore<u32>| s.chunk_stamps().collect::<BTreeMap<_, _>>();
        let before = stamps(&store);
        assert_eq!(before.len(), 2);

        // A write in one chunk renews that chunk's stamp only.
        store.insert((1, 1, 1), 3);
        let after = stamps(&store);
        assert_ne!(after[&(0, 0, 0)], before[&(0, 0, 0)]);
        assert_eq!(after[&(2, 0, 0)], before[&(2, 0, 0)]);

        // Replacing a value renews it too, even though the length is unchanged.
        let renewed = after[&(0, 0, 0)];
        store.insert((1, 1, 1), 4);
        assert_ne!(stamps(&store)[&(0, 0, 0)], renewed);

        // Removing something that is not there changes nothing.
        let frozen = stamps(&store);
        store.remove(&(2, 2, 2));
        assert_eq!(stamps(&store), frozen);

        // Emptying a chunk removes it.
        store.remove(&(20, 0, 0));
        assert!(!stamps(&store).contains_key(&(2, 0, 0)));
    }

    #[test]
    fn a_clone_shares_stamps_until_either_side_is_written() {
        let mut original = ChunkStore::<u32>::default();
        for (i, key) in spread().into_iter().enumerate() {
            original.insert(key, i as u32);
        }
        let mut copy = original.clone();
        let stamps = |s: &ChunkStore<u32>| s.chunk_stamps().collect::<BTreeMap<_, _>>();
        assert_eq!(stamps(&copy), stamps(&original));

        copy.insert((0, 0, 0), 7777);
        assert_eq!(
            original.get(&(0, 0, 0)),
            Some(&(spread().iter().position(|k| *k == (0, 0, 0)).unwrap() as u32))
        );
        let (a, b) = (stamps(&original), stamps(&copy));
        let differing: Vec<_> = a.iter().filter(|(k, v)| b.get(k) != Some(v)).collect();
        assert_eq!(differing.len(), 1, "only the written chunk differs");

        // And a write to the original leaves the copy alone.
        original.remove(&(17, 8, 8));
        assert!(copy.get(&(17, 8, 8)).is_some());
    }

    #[test]
    fn chunk_entries_lists_only_that_chunk() {
        let mut store = ChunkStore::<u32>::default();
        for key in spread() {
            store.insert(key, 0);
        }
        for (chunk, _) in store.chunk_stamps().collect::<Vec<_>>() {
            let keys: Vec<_> = store.chunk_entries(chunk).map(|(k, _)| *k).collect();
            assert!(!keys.is_empty());
            assert!(keys.iter().all(|k| chunk_of(*k) == chunk));
        }
        assert_eq!(store.chunk_entries((999, 999, 999)).count(), 0);
    }
}
