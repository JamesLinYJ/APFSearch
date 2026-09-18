//! Copy-on-write posting directories use the same stable 256-way partition as
//! persistence. A batch clones only touched directories and touched bitmaps.
use roaring::RoaringBitmap;
use std::{
    borrow::Borrow,
    collections::{HashMap, hash_map::Entry},
    hash::Hash,
    sync::Arc,
};
pub(crate) const SHARDS: usize = 256;
pub(crate) fn partition(kind: u8, key: &[u8]) -> usize {
    let hash = key
        .iter()
        .fold(2_166_136_261u32 ^ kind as u32, |hash, byte| {
            (hash ^ *byte as u32).wrapping_mul(16_777_619)
        });
    (hash >> 24) as usize
}
pub(crate) trait KeyBytes {
    fn key_bytes(&self) -> &[u8];
}
pub(crate) trait PostingKey: KeyBytes + Clone + Eq + Hash {
    const KIND: u8;
    fn heap_bytes(&self) -> usize {
        0
    }
}
impl KeyBytes for u8 {
    fn key_bytes(&self) -> &[u8] {
        std::slice::from_ref(self)
    }
}
impl PostingKey for u8 {
    const KIND: u8 = 1;
}
impl<const N: usize> KeyBytes for [u8; N] {
    fn key_bytes(&self) -> &[u8] {
        self
    }
}
impl PostingKey for [u8; 2] {
    const KIND: u8 = 2;
}
impl PostingKey for [u8; 3] {
    const KIND: u8 = 0;
}
impl KeyBytes for str {
    fn key_bytes(&self) -> &[u8] {
        self.as_bytes()
    }
}
impl KeyBytes for String {
    fn key_bytes(&self) -> &[u8] {
        self.as_bytes()
    }
}
impl PostingKey for String {
    const KIND: u8 = 3;
    fn heap_bytes(&self) -> usize {
        self.capacity()
    }
}
type Directory<K> = HashMap<K, Arc<RoaringBitmap>>;
pub(crate) fn reclaim<K>(map: Arc<PostingMap<K>>, cpu: &mut crate::cpu_executor::BackgroundPermit) {
    if let Ok(map) = Arc::try_unwrap(map) {
        for shard in map.shards {
            cpu.checkpoint();
            if let Ok(directory) = Arc::try_unwrap(shard) {
                for (_, bitmap) in directory {
                    cpu.checkpoint();
                    drop(bitmap);
                }
            }
        }
    }
}
#[derive(Clone, Debug)]
pub(crate) struct PostingMap<K> {
    shards: [Arc<Directory<K>>; SHARDS],
}
impl<K: PostingKey> PartialEq for PostingMap<K> {
    fn eq(&self, other: &Self) -> bool {
        self.shards == other.shards
    }
}
impl<K> Default for PostingMap<K> {
    fn default() -> Self {
        let empty = Arc::new(HashMap::new());
        Self {
            shards: std::array::from_fn(|_| empty.clone()),
        }
    }
}
impl<K: PostingKey> PostingMap<K> {
    pub(crate) fn shares_partition(&self, other: &Self, shard: usize) -> bool {
        Arc::ptr_eq(&self.shards[shard], &other.shards[shard])
    }
    pub(crate) fn partition_entries(
        &self,
        shard: usize,
    ) -> impl Iterator<Item = (&K, &Arc<RoaringBitmap>)> {
        self.shards[shard].iter()
    }
    pub(crate) fn get<Q: KeyBytes + Hash + Eq + ?Sized>(
        &self,
        key: &Q,
    ) -> Option<&Arc<RoaringBitmap>>
    where
        K: Borrow<Q>,
    {
        self.shards[partition(K::KIND, key.key_bytes())].get(key)
    }
    pub(crate) fn get_mut<Q: KeyBytes + Hash + Eq + ?Sized>(
        &mut self,
        key: &Q,
    ) -> Option<&mut Arc<RoaringBitmap>>
    where
        K: Borrow<Q>,
    {
        let shard = &mut self.shards[partition(K::KIND, key.key_bytes())];
        if !shard.contains_key(key) {
            return None;
        }
        Arc::make_mut(shard).get_mut(key)
    }
    pub(crate) fn entry(&mut self, key: K) -> Entry<'_, K, Arc<RoaringBitmap>> {
        Arc::make_mut(&mut self.shards[partition(K::KIND, key.key_bytes())]).entry(key)
    }
    pub(crate) fn insert(
        &mut self,
        key: K,
        value: Arc<RoaringBitmap>,
    ) -> Option<Arc<RoaringBitmap>> {
        Arc::make_mut(&mut self.shards[partition(K::KIND, key.key_bytes())]).insert(key, value)
    }
    pub(crate) fn remove<Q: KeyBytes + Hash + Eq + ?Sized>(
        &mut self,
        key: &Q,
    ) -> Option<Arc<RoaringBitmap>>
    where
        K: Borrow<Q>,
    {
        let shard = &mut self.shards[partition(K::KIND, key.key_bytes())];
        if !shard.contains_key(key) {
            return None;
        }
        Arc::make_mut(shard).remove(key)
    }
    #[cfg(test)]
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&K, &Arc<RoaringBitmap>)> {
        self.shards.iter().flat_map(|shard| shard.iter())
    }
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.shards.iter().map(|shard| shard.len()).sum()
    }
    pub(crate) fn inventory(&self, inventory: &mut crate::memory_inventory::Inventory) {
        for shard in &self.shards {
            inventory.record(
                "posting_directory_estimated_bytes",
                Arc::as_ptr(shard) as usize,
                shard.capacity() * std::mem::size_of::<(K, Arc<RoaringBitmap>)>()
                    + shard.keys().map(PostingKey::heap_bytes).sum::<usize>(),
            );
            for bitmap in shard.values() {
                inventory.bitmap(bitmap);
            }
        }
    }
}
impl<K: PostingKey> From<HashMap<K, Arc<RoaringBitmap>>> for PostingMap<K> {
    fn from(values: HashMap<K, Arc<RoaringBitmap>>) -> Self {
        let mut map = Self::default();
        for (key, value) in values {
            map.insert(key, value);
        }
        map
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn updates_clone_one_shard_once_and_leave_prior_bitmaps_unchanged() {
        let mut original = PostingMap::<[u8; 3]>::default();
        for key in [*b"one", *b"two", *b"tri"] {
            original.insert(key, Arc::new([1, 2].into()));
        }
        let mut updated = original.clone();
        Arc::make_mut(updated.get_mut(b"one").unwrap()).insert(3);
        let index = partition(0, b"one");
        let copied = Arc::as_ptr(&updated.shards[index]);
        Arc::make_mut(updated.get_mut(b"one").unwrap()).remove(1);
        assert_eq!(copied, Arc::as_ptr(&updated.shards[index]));
        for i in 0..SHARDS {
            assert_eq!(
                Arc::ptr_eq(&original.shards[i], &updated.shards[i]),
                i != index
            );
        }
        assert_eq!(
            original.get(b"one").unwrap().as_ref(),
            &RoaringBitmap::from([1, 2])
        );
        assert_eq!(
            updated.get(b"one").unwrap().as_ref(),
            &RoaringBitmap::from([2, 3])
        );
    }
    #[test]
    fn borrowed_extension_lookup_and_restored_map_preserve_values() {
        let original =
            HashMap::from([("中文".to_owned(), Arc::new(RoaringBitmap::from([u32::MAX])))]);
        let mut map = PostingMap::from(original);
        assert!(map.get("中文").unwrap().contains(u32::MAX));
        assert!(map.remove("missing").is_none());
        assert!(map.remove("中文").is_some());
        assert_eq!(map.len(), 0);
    }
}
