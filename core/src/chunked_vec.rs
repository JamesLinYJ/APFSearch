//! Stable-slot storage with block-level copy-on-write. Cloning a snapshot copies
//! its small block directory, not one reference count per indexed file.
use serde::{Serialize, Serializer, ser::SerializeSeq};
use std::{
    ops::{Index, IndexMut},
    sync::Arc,
};

const CHUNK_LENGTH: usize = 4096;

#[derive(Clone, Debug)]
pub struct ChunkedVec<T> {
    chunks: Vec<Arc<Vec<T>>>,
    length: usize,
}
impl<T> Default for ChunkedVec<T> {
    fn default() -> Self {
        Self {
            chunks: Vec::new(),
            length: 0,
        }
    }
}
impl<T> ChunkedVec<T> {
    pub fn len(&self) -> usize {
        self.length
    }
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }
    pub fn get(&self, slot: usize) -> Option<&T> {
        self.chunks
            .get(slot / CHUNK_LENGTH)?
            .get(slot % CHUNK_LENGTH)
    }
    pub fn last(&self) -> Option<&T> {
        self.length.checked_sub(1).and_then(|slot| self.get(slot))
    }
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &T> {
        self.chunks.iter().flat_map(|chunk| chunk.iter())
    }
}
impl<T: Clone> ChunkedVec<T> {
    pub fn push(&mut self, value: T) {
        if self.length.is_multiple_of(CHUNK_LENGTH) {
            self.chunks.push(Arc::new(Vec::with_capacity(CHUNK_LENGTH)));
        }
        Arc::make_mut(self.chunks.last_mut().unwrap()).push(value);
        self.length += 1;
    }
}
impl<T> FromIterator<T> for ChunkedVec<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut iter = iter.into_iter();
        let mut chunks = Vec::new();
        let mut length = 0;
        loop {
            let chunk: Vec<_> = iter.by_ref().take(CHUNK_LENGTH).collect();
            if chunk.is_empty() {
                break;
            }
            length += chunk.len();
            chunks.push(Arc::new(chunk));
        }
        Self { chunks, length }
    }
}
impl<T> From<Vec<T>> for ChunkedVec<T> {
    fn from(values: Vec<T>) -> Self {
        values.into_iter().collect()
    }
}
impl<T> Index<usize> for ChunkedVec<T> {
    type Output = T;
    fn index(&self, slot: usize) -> &T {
        self.get(slot).expect("Entry slot out of bounds")
    }
}
impl<T: Clone> IndexMut<usize> for ChunkedVec<T> {
    fn index_mut(&mut self, slot: usize) -> &mut T {
        assert!(slot < self.length, "Entry slot out of bounds");
        &mut Arc::make_mut(&mut self.chunks[slot / CHUNK_LENGTH])[slot % CHUNK_LENGTH]
    }
}
impl<T: Serialize> Serialize for ChunkedVec<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.length))?;
        for entry in self.iter() {
            sequence.serialize_element(entry)?;
        }
        sequence.end()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshots_share_untouched_blocks_and_preserve_slot_boundaries() {
        let original: ChunkedVec<_> = (0..CHUNK_LENGTH * 3 + 2).collect();
        let mut updated = original.clone();
        updated[CHUNK_LENGTH] = 99;
        updated.push(100);
        assert!(Arc::ptr_eq(&original.chunks[0], &updated.chunks[0]));
        assert!(!Arc::ptr_eq(&original.chunks[1], &updated.chunks[1]));
        assert!(Arc::ptr_eq(&original.chunks[2], &updated.chunks[2]));
        assert!(!Arc::ptr_eq(&original.chunks[3], &updated.chunks[3]));
        assert_eq!(original[CHUNK_LENGTH], CHUNK_LENGTH);
        assert_eq!(updated[CHUNK_LENGTH - 1], CHUNK_LENGTH - 1);
        assert_eq!(updated[CHUNK_LENGTH + 1], CHUNK_LENGTH + 1);
        assert_eq!(updated.last(), Some(&100));
        assert_eq!(original.get(original.len()), None);
        assert_eq!(
            original.iter().copied().collect::<Vec<_>>(),
            (0..original.len()).collect::<Vec<_>>()
        );
    }
    #[test]
    fn appending_to_full_blocks_does_not_copy_existing_entries() {
        let original: ChunkedVec<_> = (0..CHUNK_LENGTH).collect();
        let mut updated = original.clone();
        updated.push(CHUNK_LENGTH);
        assert!(Arc::ptr_eq(&original.chunks[0], &updated.chunks[0]));
        assert_eq!(updated.len(), CHUNK_LENGTH + 1);
        assert_eq!(
            serde_json::to_value(&updated).unwrap(),
            serde_json::json!((0..=CHUNK_LENGTH).collect::<Vec<_>>())
        );
    }
}
