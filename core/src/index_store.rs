#[path = "cache_journal.rs"]
mod cache_journal;
#[path = "change_reader.rs"]
mod change_reader;
#[path = "ordered_merge.rs"]
mod ordered_merge;

use crate::{
    metadata_postings::MetadataPostings,
    numeric_columns::NumericColumns,
    query::{fold, Field},
    result_order::{sort_slots, ResultOrder},
    scanner::ScannedFile,
};
use roaring::{RoaringBitmap, RoaringTreemap};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexedFile {
    pub id: i64,
    pub path: String,
    pub name: String,
    pub extension: String,
    pub size: u64,
    pub modified: i64,
    pub created: i64,
    pub changed: i64,
    #[serde(default)]
    pub modified_ns: i64,
    #[serde(default)]
    pub changed_ns: i64,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub file_id: u64,
    pub parent_id: u64,
    pub volume_id: String,
    pub flags: u32,
    #[serde(default)]
    pub properties: Value,
    #[serde(default)]
    pub content_indexed: bool,
    #[serde(skip)]
    pub folded_name: Arc<str>,
    #[serde(skip)]
    pub folded_extension: String,
    #[serde(skip)]
    pub folded_path: Arc<str>,
    #[serde(skip)]
    pub search_name: Arc<str>,
    #[serde(skip)]
    pub search_path: Arc<str>,
    #[serde(skip)]
    pub parent: Arc<str>,
}
impl IndexedFile {
    pub fn prepare(&mut self) {
        self.folded_name = fold(&self.name).into();
        self.folded_extension = fold(&self.extension);
        self.folded_path = fold(&self.path).into();
        self.search_name = shared_search_fold(&self.name, &self.folded_name);
        self.search_path = shared_search_fold(&self.path, &self.folded_path);
        self.parent = Path::new(&self.path)
            .parent()
            .map(|parent| Arc::from(parent.to_string_lossy().as_ref()))
            .unwrap_or_default()
    }
    fn prepare_replacement(&mut self, previous: Option<&IndexedFile>) {
        let Some(previous) = previous else {
            return self.prepare();
        };
        if self.name == previous.name {
            self.folded_name = previous.folded_name.clone();
            self.search_name = previous.search_name.clone();
        } else {
            self.folded_name = fold(&self.name).into();
            self.search_name = shared_search_fold(&self.name, &self.folded_name);
        }
        self.folded_extension = if self.extension == previous.extension {
            previous.folded_extension.clone()
        } else {
            fold(&self.extension)
        };
        if self.path == previous.path {
            self.folded_path = previous.folded_path.clone();
            self.search_path = previous.search_path.clone();
            self.parent = previous.parent.clone();
        } else {
            self.folded_path = fold(&self.path).into();
            self.search_path = shared_search_fold(&self.path, &self.folded_path);
            let parent = Path::new(&self.path)
                .parent()
                .map(|parent| parent.to_string_lossy());
            self.parent = if parent.as_deref() == Some(previous.parent.as_ref()) {
                previous.parent.clone()
            } else {
                parent
                    .map(|parent| Arc::from(parent.as_ref()))
                    .unwrap_or_default()
            };
        }
    }
    pub fn text(&self, field: Field) -> &str {
        match field {
            Field::Name => &self.name,
            Field::Path => &self.path,
            Field::Parent => &self.parent,
        }
    }
    pub fn folded(&self, field: Field) -> std::borrow::Cow<'_, str> {
        match field {
            Field::Name => self.folded_name.as_ref().into(),
            Field::Path => self.folded_path.as_ref().into(),
            Field::Parent => fold(&self.parent).into(),
        }
    }
}
fn shared_search_fold(text: &str, folded: &Arc<str>) -> Arc<str> {
    if text.is_ascii() {
        return folded.clone();
    }
    let searchable = crate::query::fold_search(text);
    if searchable == folded.as_ref() {
        folded.clone()
    } else {
        searchable.into()
    }
}
pub(crate) struct ResultPage {
    pub slots: Vec<u32>,
    pub total: usize,
    pub offset: usize,
    pub anchor_index: Option<usize>,
}
/// Persistent IDs and posting slots are separate identities. Most snapshots
/// start in increasing-ID order, so that prefix needs no map at all. Restored
/// older IDs append stable slots and occupy only this sparse copy-on-write tail.
#[derive(Clone, Default)]
pub(crate) struct FileSlots {
    sorted_prefix: usize,
    exceptions: Arc<BTreeMap<i64, u32>>,
}
impl FileSlots {
    pub(crate) fn from_entries(entries: &[Arc<IndexedFile>]) -> Result<Self, &'static str> {
        let mut slots = Self::default();
        for (slot, file) in entries.iter().enumerate() {
            slots.append(file.id, &entries[..slot])?;
        }
        Ok(slots)
    }
    #[cfg(test)]
    pub(crate) fn layout(&self) -> (usize, usize) {
        (self.sorted_prefix, self.exceptions.len())
    }
    fn get(&self, id: i64, entries: &[Arc<IndexedFile>]) -> Option<usize> {
        entries[..self.sorted_prefix]
            .binary_search_by_key(&id, |file| file.id)
            .ok()
            .or_else(|| self.exceptions.get(&id).map(|slot| *slot as usize))
    }
    fn append(&mut self, id: i64, entries: &[Arc<IndexedFile>]) -> Result<usize, &'static str> {
        let slot = u32::try_from(entries.len()).map_err(|_| "snapshot_slot_overflow")?;
        if self.sorted_prefix == entries.len() && entries.last().is_none_or(|last| last.id < id) {
            self.sorted_prefix += 1;
        } else {
            if self.get(id, entries).is_some() {
                return Err("duplicate_persistent_file_id");
            }
            Arc::make_mut(&mut self.exceptions).insert(id, slot);
        }
        Ok(slot as usize)
    }
}
#[derive(Default)]
pub struct SearchSnapshot {
    // Stable slots keep postings valid when a directory entry is deleted. The
    // bitmap determines visibility; a later full rebuild compacts old slots.
    pub entries: Vec<Arc<IndexedFile>>,
    file_slots: FileSlots,
    pub live: RoaringBitmap,
    pub trigrams: Arc<HashMap<[u8; 3], Arc<RoaringBitmap>>>,
    pub name_order: Arc<Vec<u32>>,
    pub(crate) path_order: Arc<Vec<u32>>,
    // Slots in non-singleton natural-path equivalence classes. Other sort
    // descriptors can only affect ordering inside these classes.
    pub(crate) path_ties: Arc<RoaringBitmap>,
    path_rank: Arc<Vec<u32>>,
    pub(crate) metadata_postings: MetadataPostings,
    numeric_columns: NumericColumns,
    pub generation: u64,
    pub content_revision: u64,
    sort_orders: Mutex<VecDeque<(ResultOrder, Arc<Vec<u32>>)>>,
    matching_sets: Mutex<VecDeque<(String, Arc<RoaringBitmap>)>>,
    pages: Mutex<VecDeque<(String, Arc<ResultPage>)>>,
}
impl SearchSnapshot {
    pub(crate) fn from_prepared_cache(parts: crate::snapshot_cache::PreparedSnapshot) -> Self {
        let path_rank = Arc::new(order_ranks(parts.entries.len(), &parts.path_order));
        // These contiguous numeric blocks are cheap derived columns. Restoring
        // them requires no folding, sorting or cache format rewrite.
        let numeric_columns = NumericColumns::build(&parts.entries);
        let file_slots = parts.file_slots;
        Self {
            file_slots,
            entries: parts.entries,
            live: parts.live,
            trigrams: parts.trigrams,
            name_order: parts.name_order,
            path_order: parts.path_order,
            path_ties: parts.path_ties,
            path_rank,
            metadata_postings: parts.metadata_postings,
            numeric_columns,
            generation: parts.generation,
            content_revision: parts.content_revision,
            sort_orders: Mutex::new(VecDeque::new()),
            matching_sets: Mutex::new(VecDeque::new()),
            pages: Mutex::new(VecDeque::new()),
        }
    }
    pub(crate) fn slot_for_id(&self, id: i64) -> Option<usize> {
        self.file_slots.get(id, &self.entries)
    }
    pub fn len(&self) -> usize {
        self.live.len() as usize
    }
    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }
    pub fn visible_entries(&self) -> impl Iterator<Item = &IndexedFile> {
        self.live
            .iter()
            .map(|index| self.entries[index as usize].as_ref())
    }
    pub fn new(entries: Vec<IndexedFile>, generation: u64) -> Self {
        Self::build_with_metrics(entries, generation, |_, _| {})
    }
    /// Observe construction phases without opening or changing persistent storage.
    pub fn build_with_metrics(
        mut entries: Vec<IndexedFile>,
        generation: u64,
        mut record: impl FnMut(&str, std::time::Duration),
    ) -> Self {
        let started = std::time::Instant::now();
        let mut trigrams: HashMap<[u8; 3], Arc<RoaringBitmap>> = HashMap::new();
        let mut parents: HashMap<Arc<str>, ()> = HashMap::new();
        for (i, entry) in entries.iter_mut().enumerate() {
            entry.prepare();
            if let Some((parent, _)) = parents.get_key_value(entry.parent.as_ref()) {
                entry.parent = parent.clone();
            } else {
                parents.insert(entry.parent.clone(), ());
            }
            for tri in entry.search_name.as_bytes().windows(3) {
                Arc::make_mut(trigrams.entry([tri[0], tri[1], tri[2]]).or_default())
                    .insert(i as u32);
            }
        }
        drop(parents);
        record("prepare_and_postings", started.elapsed());
        let started = std::time::Instant::now();
        let entries: Vec<Arc<IndexedFile>> = entries.into_iter().map(Arc::new).collect();
        record("owned_entries_and_live_set", started.elapsed());
        Self::build_prepared_indexes(entries, trigrams, generation, record)
    }
    /// Build slot-based derived structures from immutable, already folded rows.
    /// Reusing the row Arcs avoids cloning paths or recomputing Unicode columns
    /// when an old SQLite ID returns after visibility-based compaction.
    fn build_prepared_indexes(
        entries: Vec<Arc<IndexedFile>>,
        trigrams: HashMap<[u8; 3], Arc<RoaringBitmap>>,
        generation: u64,
        mut record: impl FnMut(&str, std::time::Duration),
    ) -> Self {
        let file_slots = FileSlots::from_entries(&entries).expect("file IDs must be unique");
        let live = (0..entries.len() as u32).collect();
        let started = std::time::Instant::now();
        let metadata_postings = MetadataPostings::build(
            entries
                .iter()
                .enumerate()
                .map(|(slot, file)| (slot as u32, file.as_ref())),
        );
        record("exact_metadata_postings", started.elapsed());
        let started = std::time::Instant::now();
        let numeric_columns = NumericColumns::build(&entries);
        record("numeric_columns", started.elapsed());
        let started = std::time::Instant::now();
        let mut name_order: Vec<u32> = (0..entries.len() as u32).collect();
        name_order
            .sort_unstable_by(|a, b| compare_names(&entries[*a as usize], &entries[*b as usize]));
        record("name_sort", started.elapsed());
        let started = std::time::Instant::now();
        let mut path_order: Vec<u32> = (0..entries.len() as u32).collect();
        let path_spec = ResultOrder::path();
        path_order.sort_unstable_by(|a, b| {
            path_spec.compare(&entries[*a as usize], &entries[*b as usize])
        });
        let path_rank = order_ranks(entries.len(), &path_order);
        let mut path_ties = RoaringBitmap::new();
        for pair in path_order.windows(2) {
            if same_natural_path(&entries, pair[0], pair[1]) {
                path_ties.insert(pair[0]);
                path_ties.insert(pair[1]);
            }
        }
        record("path_sort_and_ranks", started.elapsed());
        Self {
            entries,
            file_slots,
            live,
            generation,
            content_revision: 0,
            trigrams: Arc::new(trigrams),
            name_order: Arc::new(name_order),
            path_order: Arc::new(path_order),
            path_ties: Arc::new(path_ties),
            path_rank: Arc::new(path_rank),
            metadata_postings,
            numeric_columns,
            sort_orders: Mutex::new(VecDeque::new()),
            matching_sets: Mutex::new(VecDeque::new()),
            pages: Mutex::new(VecDeque::new()),
        }
    }
    /// Full reloads remain available for recovery and compaction. Ordinary event
    /// batches use from_changes, which never rereads or copies a million rows.
    pub fn from_previous(
        entries: Vec<IndexedFile>,
        generation: u64,
        _previous: &SearchSnapshot,
    ) -> Self {
        Self::new(entries, generation)
    }
    pub(crate) fn incremental_limit(entry_count: usize) -> usize {
        // Copy-on-write posting updates and merging sorted slot arrays are
        // cheaper than rebuilding every prepared column for a small fraction
        // of a large index. A fixed row threshold made ordinary build output
        // batches reconstruct millions of unchanged rows.
        (entry_count / 8).max(2000)
    }
    /// Identify the small-delta cases that need different slots, not a SQL
    /// reload. A caller can release unleased generation history before building
    /// the replacement derived structures.
    pub(crate) fn remap_reason(
        changes: &[SnapshotChange],
        previous: &Self,
    ) -> Option<&'static str> {
        if changes.len() > Self::incremental_limit(previous.len()) {
            return None;
        }
        if previous.entries.len() > previous.len() + previous.len() / 4 + 10_000 {
            return Some("inactive_slots_require_compaction");
        }
        None
    }
    /// Merge ordered persistent IDs while retaining every unchanged live row.
    /// Only slots and their derived indexes change; older snapshots keep their
    /// own immutable orders, postings, and file metadata.
    pub(crate) fn remap_changes(
        mut changes: Vec<SnapshotChange>,
        generation: u64,
        previous: &Self,
    ) -> Self {
        changes.sort_by_key(|(id, _)| *id);
        // A normal SQL delta has unique IDs. Keep the final observation when a
        // direct internal caller supplies multiple changes for the same row.
        let mut unique: Vec<SnapshotChange> = Vec::with_capacity(changes.len());
        for change in changes {
            if unique.last().is_some_and(|last| last.0 == change.0) {
                *unique.last_mut().unwrap() = change;
            } else {
                unique.push(change);
            }
        }
        let mut old_rows: Vec<_> = previous
            .live
            .iter()
            .map(|slot| previous.entries[slot as usize].clone())
            .collect();
        if previous.file_slots.sorted_prefix != previous.entries.len() {
            old_rows.sort_unstable_by_key(|file| file.id);
        }
        let mut old = old_rows.into_iter().peekable();
        let mut entries = Vec::with_capacity(previous.len() + unique.len());
        for (id, replacement) in unique {
            while old.peek().is_some_and(|file| file.id < id) {
                entries.push(old.next().unwrap());
            }
            if old.peek().is_some_and(|file| file.id == id) {
                old.next();
            }
            if let Some(mut file) = replacement {
                let previous_file = previous
                    .slot_for_id(id)
                    .map(|slot| previous.entries[slot].as_ref());
                file.prepare_replacement(previous_file);
                entries.push(Arc::new(file));
            }
        }
        entries.extend(old);
        let mut trigrams: HashMap<[u8; 3], Arc<RoaringBitmap>> = HashMap::new();
        for (slot, file) in entries.iter().enumerate() {
            for trigram in file.search_name.as_bytes().windows(3) {
                Arc::make_mut(
                    trigrams
                        .entry([trigram[0], trigram[1], trigram[2]])
                        .or_default(),
                )
                .insert(slot as u32);
            }
        }
        let mut snapshot = Self::build_prepared_indexes(entries, trigrams, generation, |_, _| {});
        snapshot.content_revision = previous.content_revision;
        snapshot
    }
    pub fn from_changes(
        changes: Vec<(i64, Option<IndexedFile>)>,
        generation: u64,
        previous: &SearchSnapshot,
    ) -> Option<Self> {
        Self::from_changes_with_reason(changes, generation, previous).ok()
    }
    pub(crate) fn from_changes_with_reason(
        changes: Vec<(i64, Option<IndexedFile>)>,
        generation: u64,
        previous: &SearchSnapshot,
    ) -> Result<Self, &'static str> {
        if changes.len() > Self::incremental_limit(previous.len()) {
            return Err("delta_exceeds_incremental_limit");
        }
        Self::apply_changes(changes, generation, previous)
    }
    /// Callers enforce either the ordinary publication budget or the bounded
    /// durable-cache journal budget before entering this shared update path.
    fn apply_changes(
        mut changes: Vec<SnapshotChange>,
        generation: u64,
        previous: &SearchSnapshot,
    ) -> Result<Self, &'static str> {
        if previous.entries.len() > previous.len() + previous.len() / 4 + 10_000 {
            return Ok(Self::remap_changes(changes, generation, previous));
        }
        // Stable ordering preserves last-observation-wins for repeated IDs.
        // It is independent of the physical slot ordering of existing rows.
        changes.sort_by_key(|(id, _)| *id);
        let mut entries = previous.entries.clone();
        let mut file_slots = previous.file_slots.clone();
        let mut live = previous.live.clone();
        let mut trigrams = previous.trigrams.clone();
        let mut metadata_postings = previous.metadata_postings.clone();
        let mut numeric_columns = previous.numeric_columns.clone();
        let mut order_changes = RoaringBitmap::new();
        let mut path_changes = RoaringBitmap::new();
        let mut changed_slots = RoaringBitmap::new();
        for (id, replacement) in changes {
            let slot = match file_slots.get(id, &entries) {
                Some(slot) => slot,
                None if replacement.is_some() => file_slots.append(id, &entries)?,
                None => continue,
            };
            changed_slots.insert(slot as u32);
            let old = entries.get(slot);
            let was_live = live.contains(slot as u32);
            if !was_live
                || old
                    .zip(replacement.as_ref())
                    .is_none_or(|(old, new)| old.path != new.path)
            {
                path_changes.insert(slot as u32);
            }
            let index_changed = match (old, replacement.as_ref()) {
                (Some(old), Some(new)) if was_live => old.name != new.name || old.path != new.path,
                _ => true,
            };
            let name_changed = match (old, replacement.as_ref()) {
                (Some(old), Some(new)) if was_live => old.name != new.name,
                _ => true,
            };
            let metadata_changed = match (old, replacement.as_ref()) {
                (Some(old), Some(new)) if was_live => {
                    old.name != new.name
                        || old.extension != new.extension
                        || old.is_dir != new.is_dir
                        || old.is_symlink != new.is_symlink
                }
                _ => true,
            };
            if was_live && metadata_changed {
                metadata_postings.remove(slot as u32, old.unwrap());
            }
            if name_changed && was_live {
                let postings = Arc::make_mut(&mut trigrams);
                for tri in old.unwrap().search_name.as_bytes().windows(3) {
                    if let Some(bitmap) = postings.get_mut(&[tri[0], tri[1], tri[2]]) {
                        Arc::make_mut(bitmap).remove(slot as u32);
                    }
                }
            }
            if let Some(mut entry) = replacement {
                entry.prepare_replacement(old.map(Arc::as_ref));
                numeric_columns.set(slot as u32, &entry);
                if metadata_changed {
                    metadata_postings.insert(slot as u32, &entry);
                }
                if name_changed {
                    let postings = Arc::make_mut(&mut trigrams);
                    for tri in entry.search_name.as_bytes().windows(3) {
                        Arc::make_mut(postings.entry([tri[0], tri[1], tri[2]]).or_default())
                            .insert(slot as u32);
                    }
                }
                if slot == entries.len() {
                    entries.push(Arc::new(entry));
                } else {
                    entries[slot] = Arc::new(entry);
                }
                live.insert(slot as u32);
            } else {
                live.remove(slot as u32);
            }
            if index_changed {
                order_changes.insert(slot as u32);
            }
        }
        let name_order = if order_changes.is_empty() {
            previous.name_order.clone()
        } else {
            Arc::new(updated_order(
                &previous.name_order,
                &order_changes,
                &live,
                &entries,
                &ResultOrder::name(),
            ))
        };
        numeric_columns.finish_update();
        let old_orders = previous.sort_orders.lock().unwrap().clone();
        let (path_order, path_rank, path_ties) = if path_changes.is_empty() {
            (
                previous.path_order.clone(),
                previous.path_rank.clone(),
                previous.path_ties.clone(),
            )
        } else {
            let order = updated_order(
                &previous.path_order,
                &path_changes,
                &live,
                &entries,
                &ResultOrder::path(),
            );
            let ranks = order_ranks(entries.len(), &order);
            let ties = updated_path_ties(previous, &path_changes, &live, &entries, &order, &ranks);
            (Arc::new(order), Arc::new(ranks), Arc::new(ties))
        };
        let sort_orders = old_orders
            .into_iter()
            .map(|(order, slots)| {
                let relevant: RoaringBitmap = changed_slots
                    .iter()
                    .filter(|slot| {
                        if previous.live.contains(*slot) != live.contains(*slot) {
                            return true;
                        }
                        previous.entries.get(*slot as usize).is_none_or(|old| {
                            order.affected_by_change(old, &entries[*slot as usize])
                        })
                    })
                    .collect();
                if relevant.is_empty() {
                    (order, slots)
                } else {
                    let updated = updated_order(&slots, &relevant, &live, &entries, &order);
                    (order, Arc::new(updated))
                }
            })
            .collect();
        Ok(Self {
            entries,
            file_slots,
            live,
            trigrams,
            name_order,
            path_order,
            path_rank,
            path_ties,
            metadata_postings,
            numeric_columns,
            generation,
            content_revision: previous.content_revision,
            sort_orders: Mutex::new(sort_orders),
            matching_sets: Mutex::new(VecDeque::new()),
            pages: Mutex::new(VecDeque::new()),
        })
    }
    pub(crate) fn cached_page(&self, key: &str) -> Option<Arc<ResultPage>> {
        let mut pages = self.pages.lock().unwrap();
        let position = pages.iter().position(|(existing, _)| existing == key)?;
        let item = pages.remove(position).unwrap();
        let page = item.1.clone();
        pages.push_back(item);
        Some(page)
    }
    pub(crate) fn cache_page(&self, key: String, page: Arc<ResultPage>) {
        let mut pages = self.pages.lock().unwrap();
        pages.retain(|(existing, _)| existing != &key);
        pages.push_back((key, page));
        while pages.len() > 32 {
            pages.pop_front();
        }
    }
    pub(crate) fn cached_matches(&self, key: &str) -> Option<Arc<RoaringBitmap>> {
        let mut cache = self.matching_sets.lock().unwrap();
        let position = cache.iter().position(|(existing, _)| existing == key)?;
        let item = cache.remove(position).unwrap();
        let matches = item.1.clone();
        cache.push_back(item);
        Some(matches)
    }
    pub(crate) fn cache_matches(&self, key: String, matches: Arc<RoaringBitmap>) {
        const BYTE_LIMIT: usize = 64 * 1024 * 1024;
        if matches.serialized_size() > BYTE_LIMIT {
            return;
        }
        let mut cache = self.matching_sets.lock().unwrap();
        cache.retain(|(existing, _)| existing != &key);
        cache.push_back((key, matches));
        while cache.len() > 8
            || cache
                .iter()
                .map(|(_, set)| set.serialized_size())
                .sum::<usize>()
                > BYTE_LIMIT
        {
            cache.pop_front();
        }
    }
    pub(crate) fn cached_order(&self, order: &ResultOrder) -> Option<Arc<Vec<u32>>> {
        if order == &ResultOrder::name() {
            return Some(self.name_order.clone());
        }
        if order == &ResultOrder::path()
            || (order.primary_path_direction() == Some(true) && self.path_ties.is_empty())
        {
            return Some(self.path_order.clone());
        }
        let mut cache = self.sort_orders.lock().unwrap();
        let position = cache.iter().position(|(existing, _)| existing == order)?;
        let item = cache.remove(position).unwrap();
        let slots = item.1.clone();
        cache.push_back(item);
        Some(slots)
    }
    pub(crate) fn result_order(
        &self,
        order: &ResultOrder,
        cancelled: &AtomicBool,
    ) -> Result<Arc<Vec<u32>>, String> {
        if cancelled.load(Ordering::Relaxed) {
            return Err("Query cancelled".into());
        }
        if let Some(slots) = self.cached_order(order) {
            return Ok(slots);
        }
        let mut slots: Vec<u32>;
        if let Some(ascending) = order.primary_path_direction() {
            slots = self.path_order.as_ref().clone();
            if !ascending {
                slots.reverse();
            }
            // The primary path index already orders every unequal key. Refine
            // only equal-key groups; reversing must also restore their raw-path
            // and id fallback, which is ascending in either direction.
            let mut ranks = Vec::with_capacity(self.path_ties.len() as usize);
            for (position, slot) in self.path_ties.iter().enumerate() {
                if position % 1024 == 0 && cancelled.load(Ordering::Relaxed) {
                    return Err("Query cancelled".into());
                }
                ranks.push(self.path_rank[slot as usize] as usize);
            }
            ranks.sort_unstable();
            let mut first = 0;
            while first < ranks.len() {
                let start = ranks[first];
                let mut end = first + 1;
                while end < ranks.len()
                    && ranks[end] == ranks[end - 1] + 1
                    && same_natural_path(
                        &self.entries,
                        self.path_order[start],
                        self.path_order[ranks[end]],
                    )
                {
                    if end % 1024 == 0 && cancelled.load(Ordering::Relaxed) {
                        return Err("Query cancelled".into());
                    }
                    end += 1;
                }
                let (begin, finish) = if ascending {
                    (start, ranks[end - 1] + 1)
                } else {
                    (slots.len() - ranks[end - 1] - 1, slots.len() - start)
                };
                let mut group = slots[begin..finish].to_vec();
                sort_slots(
                    &mut group,
                    |a, b| order.compare(&self.entries[a as usize], &self.entries[b as usize]),
                    cancelled,
                )?;
                slots[begin..finish].copy_from_slice(&group);
                first = end;
            }
            if cancelled.load(Ordering::Relaxed) {
                return Err("Query cancelled".into());
            }
        } else {
            slots = self.live.iter().collect();
            sort_slots(
                &mut slots,
                |a, b| order.compare(&self.entries[a as usize], &self.entries[b as usize]),
                cancelled,
            )?;
        }
        let slots = Arc::new(slots);
        let mut cache = self.sort_orders.lock().unwrap();
        if let Some((_, existing)) = cache.iter().find(|(existing, _)| existing == order) {
            return Ok(existing.clone());
        }
        cache.push_back((order.clone(), slots.clone()));
        while cache.len() > 3 {
            cache.pop_front();
        }
        Ok(slots)
    }
    fn supports_exact(query: &crate::query::Query) -> bool {
        use crate::query::Query;
        match query {
            Query::And(queries) | Query::Or(queries) => queries.iter().all(Self::supports_exact),
            Query::Not(query) => Self::supports_exact(query),
            Query::Term(term) if NumericColumns::supports(term) => true,
            _ => MetadataPostings::supports(query),
        }
    }
    pub(crate) fn exact_matches(
        &self,
        query: &crate::query::Query,
        cancelled: &AtomicBool,
    ) -> Result<Option<RoaringBitmap>, String> {
        if cancelled.load(Ordering::Relaxed) {
            return Err("Query cancelled".into());
        }
        if !Self::supports_exact(query) {
            return Ok(None);
        }
        self.evaluate_exact(query, cancelled).map(Some)
    }
    fn evaluate_exact(
        &self,
        query: &crate::query::Query,
        cancelled: &AtomicBool,
    ) -> Result<RoaringBitmap, String> {
        use crate::query::Query;
        if cancelled.load(Ordering::Relaxed) {
            return Err("Query cancelled".into());
        }
        match query {
            Query::And(queries) => {
                let mut result = self.live.clone();
                for query in queries {
                    result &= self.evaluate_exact(query, cancelled)?;
                    if result.is_empty() {
                        break;
                    }
                }
                Ok(result)
            }
            Query::Or(queries) => {
                let mut result = RoaringBitmap::new();
                for query in queries {
                    result |= self.evaluate_exact(query, cancelled)?;
                }
                Ok(result)
            }
            Query::Not(query) => Ok(&self.live - &self.evaluate_exact(query, cancelled)?),
            Query::Term(term) if NumericColumns::supports(term) => self
                .numeric_columns
                .exact(term, &self.live, cancelled)?
                .ok_or_else(|| "Numeric index cannot evaluate its declared field".into()),
            _ => self
                .metadata_postings
                .exact(query, &self.live)
                .ok_or_else(|| "Metadata index cannot evaluate its declared field".into()),
        }
    }
    pub fn candidates(&self, query: &crate::query::Query) -> Option<RoaringBitmap> {
        self.candidates_with_cancellation(query, &AtomicBool::new(false))
            .ok()
            .flatten()
    }
    pub(crate) fn candidates_with_cancellation(
        &self,
        query: &crate::query::Query,
        cancelled: &AtomicBool,
    ) -> Result<Option<RoaringBitmap>, String> {
        if let Some(exact) = self.exact_matches(query, cancelled)? {
            return Ok(Some(exact));
        }
        match query {
            crate::query::Query::And(queries) => {
                let mut result: Option<RoaringBitmap> = None;
                for query in queries {
                    if let Some(candidates) = self.candidates_with_cancellation(query, cancelled)? {
                        if let Some(result) = &mut result {
                            *result &= candidates;
                        } else {
                            result = Some(candidates);
                        }
                    }
                }
                return Ok(result);
            }
            crate::query::Query::Or(queries) => {
                let mut result = RoaringBitmap::new();
                for query in queries {
                    let Some(candidates) = self.candidates_with_cancellation(query, cancelled)?
                    else {
                        return Ok(None);
                    };
                    result |= candidates;
                }
                return Ok(Some(result));
            }
            _ => (),
        }
        let mut literals = Vec::new();
        query.required_name_literals(&mut literals);
        let mut result: Option<RoaringBitmap> = None;
        for text in literals {
            if cancelled.load(Ordering::Relaxed) {
                return Err("Query cancelled".into());
            }
            for tri in text.as_bytes().windows(3) {
                let Some(matches) = self.trigrams.get(&[tri[0], tri[1], tri[2]]) else {
                    return Ok(Some(RoaringBitmap::new()));
                };
                match &mut result {
                    Some(result) => *result &= matches.as_ref(),
                    None => result = Some(matches.as_ref().clone()),
                }
            }
        }
        Ok(result)
    }
}
fn updated_order(
    previous: &[u32],
    changed: &RoaringBitmap,
    live: &RoaringBitmap,
    entries: &[Arc<IndexedFile>],
    order: &ResultOrder,
) -> Vec<u32> {
    let mut replacements: Vec<_> = changed.iter().filter(|slot| live.contains(*slot)).collect();
    replacements
        .sort_unstable_by(|a, b| order.compare(&entries[*a as usize], &entries[*b as usize]));
    let mut result = Vec::with_capacity(live.len() as usize);
    result.extend(
        previous
            .iter()
            .copied()
            .filter(|slot| !changed.contains(*slot)),
    );
    ordered_merge::insert_sorted(&mut result, &replacements, |a, b| {
        order.compare(&entries[a as usize], &entries[b as usize])
    });
    result
}
fn order_ranks(slot_count: usize, order: &[u32]) -> Vec<u32> {
    let mut ranks = vec![u32::MAX; slot_count];
    for (rank, slot) in order.iter().enumerate() {
        ranks[*slot as usize] = rank as u32;
    }
    ranks
}
fn same_natural_path(entries: &[Arc<IndexedFile>], first: u32, second: u32) -> bool {
    crate::query::natural_cmp_folded(
        &entries[first as usize].folded_path,
        &entries[second as usize].folded_path,
    )
    .is_eq()
}
fn include_order_neighbors(
    candidates: &mut RoaringBitmap,
    slot: u32,
    order: &[u32],
    ranks: &[u32],
) {
    let Some(&rank) = ranks.get(slot as usize).filter(|rank| **rank != u32::MAX) else {
        return;
    };
    let rank = rank as usize;
    candidates.extend(
        order[rank.saturating_sub(1)..(rank + 2).min(order.len())]
            .iter()
            .copied(),
    );
}
fn updated_path_ties(
    previous: &SearchSnapshot,
    changed: &RoaringBitmap,
    live: &RoaringBitmap,
    entries: &[Arc<IndexedFile>],
    order: &[u32],
    ranks: &[u32],
) -> RoaringBitmap {
    let mut candidates = changed.clone();
    for slot in changed {
        include_order_neighbors(
            &mut candidates,
            slot,
            &previous.path_order,
            &previous.path_rank,
        );
        include_order_neighbors(&mut candidates, slot, order, ranks);
    }
    let mut ties = previous.path_ties.as_ref().clone();
    for slot in candidates {
        ties.remove(slot);
        if !live.contains(slot) {
            continue;
        }
        let rank = ranks[slot as usize] as usize;
        if (rank > 0 && same_natural_path(entries, slot, order[rank - 1]))
            || (rank + 1 < order.len() && same_natural_path(entries, slot, order[rank + 1]))
        {
            ties.insert(slot);
        }
    }
    ties
}
fn compare_names(a: &IndexedFile, b: &IndexedFile) -> std::cmp::Ordering {
    crate::query::natural_cmp_folded(&a.folded_name, &b.folded_name)
        .then_with(|| a.path.cmp(&b.path))
        .then_with(|| a.id.cmp(&b.id))
}
/// A changed row's current accessible entry, or None after deletion/denial.
pub type SnapshotChange = (i64, Option<IndexedFile>);
pub type SnapshotDelta = Vec<SnapshotChange>;
/// Filesystem verification results for aliases of changed file objects.
#[derive(Default)]
pub(crate) struct LinkVerification {
    pub entries: Vec<ScannedFile>,
    pub unavailable: Vec<String>,
    pub removed: Vec<String>,
}
impl From<Vec<ScannedFile>> for LinkVerification {
    fn from(entries: Vec<ScannedFile>) -> Self {
        Self {
            entries,
            ..Self::default()
        }
    }
}
type LinkVerifier<'a> = dyn FnMut(Vec<String>) -> Result<LinkVerification, String> + 'a;
// About one MiB of inode-set storage for a few volumes. Clearing the optional
// cache only restores repeated verification; it never changes indexed results.
const MAX_VERIFIED_LINK_OBJECTS: usize = 65_536;
#[derive(Default)]
pub(crate) struct VerifiedFileObjects {
    by_volume: HashMap<String, std::collections::HashSet<u64>>,
    count: usize,
    reused: u64,
}
impl VerifiedFileObjects {
    fn contains(&self, volume: &str, file_id: u64) -> bool {
        self.by_volume
            .get(volume)
            .is_some_and(|files| files.contains(&file_id))
    }
    fn forget(&mut self, volume: &str, file_id: u64) {
        if let Some(files) = self.by_volume.get_mut(volume) {
            if files.remove(&file_id) {
                self.count -= 1;
            }
            if files.is_empty() {
                self.by_volume.remove(volume);
            }
        }
    }
    fn confirm(&mut self, volume: &str, file_id: u64) {
        if self.contains(volume, file_id) {
            return;
        }
        if self.count >= MAX_VERIFIED_LINK_OBJECTS {
            self.by_volume.clear();
            self.count = 0;
        }
        self.by_volume
            .entry(volume.into())
            .or_default()
            .insert(file_id);
        self.count += 1;
    }
    pub(crate) fn reused_objects(&self) -> u64 {
        self.reused
    }
}
pub struct IndexStore {
    pub connection: Connection,
    pub cache_path: std::path::PathBuf,
}
impl IndexStore {
    pub fn open(path: &Path) -> Result<Self, String> {
        const SCHEMA_VERSION: i64 = 4;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?
        }
        let mut connection = Connection::open(path).map_err(|error| error.to_string())?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| error.to_string())?;
        let version = connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .map_err(|error| error.to_string())?;
        if !(0..=SCHEMA_VERSION).contains(&version) {
            return Err(format!("Unsupported index schema version {version}"));
        }
        let journal_mode = connection
            .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            connection
                .execute_batch("PRAGMA journal_mode=WAL;")
                .map_err(|error| error.to_string())?;
        }
        connection
            .execute_batch("PRAGMA synchronous=NORMAL;PRAGMA foreign_keys=ON;")
            .map_err(|error| error.to_string())?;
        if version < SCHEMA_VERSION {
            let transaction = connection
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(|error| error.to_string())?;
            // A second opener may have completed the upgrade while we waited.
            let current_version = transaction
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .map_err(|error| error.to_string())?;
            if !(0..=SCHEMA_VERSION).contains(&current_version) {
                return Err(format!(
                    "Unsupported index schema version {current_version}"
                ));
            }
            if current_version < SCHEMA_VERSION {
                transaction.execute_batch("
                  CREATE TABLE IF NOT EXISTS files(id INTEGER PRIMARY KEY,path TEXT UNIQUE NOT NULL,name TEXT NOT NULL,extension TEXT NOT NULL,size INTEGER NOT NULL,modified INTEGER NOT NULL,created INTEGER NOT NULL,changed INTEGER NOT NULL,is_dir INTEGER NOT NULL,is_symlink INTEGER NOT NULL,file_id INTEGER NOT NULL,parent_id INTEGER NOT NULL,volume_id TEXT NOT NULL,flags INTEGER NOT NULL,seen INTEGER NOT NULL,modified_ns INTEGER NOT NULL DEFAULT 0,changed_ns INTEGER NOT NULL DEFAULT 0,accessible INTEGER NOT NULL DEFAULT 1);
                  CREATE TABLE IF NOT EXISTS content(path TEXT PRIMARY KEY REFERENCES files(path) ON DELETE CASCADE,body TEXT NOT NULL,properties TEXT NOT NULL DEFAULT '{}');
                  CREATE VIRTUAL TABLE IF NOT EXISTS content_fts USING fts5(path UNINDEXED,body,tokenize='unicode61');
                  CREATE TRIGGER IF NOT EXISTS content_delete AFTER DELETE ON content BEGIN DELETE FROM content_fts WHERE path=old.path; END;
                  CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
                  CREATE INDEX IF NOT EXISTS file_identity ON files(volume_id,file_id);
                  CREATE INDEX IF NOT EXISTS file_size ON files(size);
        ").map_err(|error| error.to_string())?;
                // Preserve metadata from indexes created before precise timestamps.
                let cols: Vec<String> = {
                    let mut s = transaction
                        .prepare("PRAGMA table_info(files)")
                        .map_err(|error| error.to_string())?;
                    let rows = s
                        .query_map([], |r| r.get::<_, String>(1))
                        .map_err(|error| error.to_string())?;
                    rows.collect::<Result<_, _>>()
                        .map_err(|error| error.to_string())?
                };
                for col in ["modified_ns", "changed_ns", "accessible"] {
                    if !cols.contains(&col.to_string()) {
                        transaction
                            .execute_batch(&format!(
                                "ALTER TABLE files ADD COLUMN {col} INTEGER NOT NULL DEFAULT {}",
                                if col == "accessible" { 1 } else { 0 }
                            ))
                            .map_err(|error| error.to_string())?;
                    }
                }
                transaction.execute_batch(r#"
                  CREATE TABLE IF NOT EXISTS snapshot_changes(id INTEGER PRIMARY KEY);
                  CREATE TABLE IF NOT EXISTS cache_changes(id INTEGER PRIMARY KEY);
                  DROP TRIGGER IF EXISTS snapshot_file_insert;
                  DROP TRIGGER IF EXISTS snapshot_file_delete;
                  DROP TRIGGER IF EXISTS snapshot_file_update;
                  DROP TRIGGER IF EXISTS snapshot_content_insert;
                  DROP TRIGGER IF EXISTS snapshot_content_update;
                  DROP TRIGGER IF EXISTS snapshot_content_delete;
                  CREATE TRIGGER IF NOT EXISTS snapshot_file_insert AFTER INSERT ON files BEGIN INSERT INTO snapshot_changes VALUES(new.id) ON CONFLICT(id) DO NOTHING; INSERT INTO cache_changes SELECT new.id WHERE EXISTS(SELECT 1 FROM settings WHERE key='cache_base_revision') AND NOT EXISTS(SELECT 1 FROM settings WHERE key='cache_journal_overflow' AND value='true') ON CONFLICT(id) DO NOTHING; END;
                  CREATE TRIGGER IF NOT EXISTS snapshot_file_delete AFTER DELETE ON files BEGIN INSERT INTO snapshot_changes VALUES(old.id) ON CONFLICT(id) DO NOTHING; INSERT INTO cache_changes SELECT old.id WHERE EXISTS(SELECT 1 FROM settings WHERE key='cache_base_revision') AND NOT EXISTS(SELECT 1 FROM settings WHERE key='cache_journal_overflow' AND value='true') ON CONFLICT(id) DO NOTHING; END;
                  CREATE TRIGGER IF NOT EXISTS snapshot_file_update AFTER UPDATE ON files WHEN old.name!=new.name OR old.path!=new.path OR old.extension!=new.extension OR old.size!=new.size OR old.modified!=new.modified OR old.created!=new.created OR old.changed!=new.changed OR old.is_dir!=new.is_dir OR old.is_symlink!=new.is_symlink OR old.file_id!=new.file_id OR old.parent_id!=new.parent_id OR old.volume_id!=new.volume_id OR old.flags!=new.flags OR old.modified_ns!=new.modified_ns OR old.changed_ns!=new.changed_ns OR old.accessible!=new.accessible BEGIN INSERT INTO snapshot_changes VALUES(new.id) ON CONFLICT(id) DO NOTHING; INSERT INTO cache_changes SELECT new.id WHERE EXISTS(SELECT 1 FROM settings WHERE key='cache_base_revision') AND NOT EXISTS(SELECT 1 FROM settings WHERE key='cache_journal_overflow' AND value='true') ON CONFLICT(id) DO NOTHING; END;
                  CREATE TRIGGER IF NOT EXISTS snapshot_content_insert AFTER INSERT ON content BEGIN INSERT INTO snapshot_changes SELECT id FROM files WHERE path=new.path ON CONFLICT(id) DO NOTHING; INSERT INTO cache_changes SELECT id FROM files WHERE path=new.path AND EXISTS(SELECT 1 FROM settings WHERE key='cache_base_revision') AND NOT EXISTS(SELECT 1 FROM settings WHERE key='cache_journal_overflow' AND value='true') ON CONFLICT(id) DO NOTHING; INSERT INTO settings VALUES('content_revision','1') ON CONFLICT(key) DO UPDATE SET value=CAST(value AS INTEGER)+1; END;
                  CREATE TRIGGER IF NOT EXISTS snapshot_content_update AFTER UPDATE ON content BEGIN INSERT INTO snapshot_changes SELECT id FROM files WHERE path=new.path ON CONFLICT(id) DO NOTHING; INSERT INTO cache_changes SELECT id FROM files WHERE path=new.path AND EXISTS(SELECT 1 FROM settings WHERE key='cache_base_revision') AND NOT EXISTS(SELECT 1 FROM settings WHERE key='cache_journal_overflow' AND value='true') ON CONFLICT(id) DO NOTHING; INSERT INTO settings VALUES('content_revision','1') ON CONFLICT(key) DO UPDATE SET value=CAST(value AS INTEGER)+1; END;
                  CREATE TRIGGER IF NOT EXISTS snapshot_content_delete AFTER DELETE ON content BEGIN INSERT INTO snapshot_changes SELECT id FROM files WHERE path=old.path ON CONFLICT(id) DO NOTHING; INSERT INTO cache_changes SELECT id FROM files WHERE path=old.path AND EXISTS(SELECT 1 FROM settings WHERE key='cache_base_revision') AND NOT EXISTS(SELECT 1 FROM settings WHERE key='cache_journal_overflow' AND value='true') ON CONFLICT(id) DO NOTHING; INSERT INTO settings VALUES('content_revision','1') ON CONFLICT(key) DO UPDATE SET value=CAST(value AS INTEGER)+1; END;
                  PRAGMA user_version=4;"#).map_err(|error| error.to_string())?;
                cache_journal::install(&transaction)?;
            }
            transaction.commit().map_err(|error| error.to_string())?;
        }
        Ok(Self {
            connection,
            cache_path: path.with_extension("snapshot.bin"),
        })
    }
    pub fn get(&self, key: &str, default: Value) -> Value {
        self.connection
            .query_row("SELECT value FROM settings WHERE key=?1", [key], |r| {
                r.get::<_, String>(0)
            })
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(default)
    }
    pub fn set(&self, key: &str, value: &Value) -> Result<(), String> {
        self.connection.execute("INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value WHERE settings.value!=excluded.value",params![key,value.to_string()]).map_err(|error| error.to_string())?;
        Ok(())
    }
    pub(crate) fn directory_is_covered(&self, path: &str) -> Result<bool, String> {
        self.connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM files WHERE path=?1 AND is_dir=1 AND accessible=1)",
                [path],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())
    }
    pub fn batch(&mut self, entries: &[ScannedFile], epoch: i64) -> Result<usize, String> {
        self.batch_inner(entries, Some(epoch), None, None, None)
    }
    /// Membership is transient. The primary path lookup compares metadata and
    /// supplies the row ID, so unchanged rows need neither UPDATE nor DELETE.
    /// Changed objects and their verified aliases commit in the same transaction:
    /// a crash cannot leave an updated path that suppresses alias repair on replay.
    pub(crate) fn observe_batch(
        &mut self,
        entries: &[ScannedFile],
        observed: Option<&mut RoaringTreemap>,
        check_links: Option<&mut LinkVerifier<'_>>,
        verified_objects: Option<&mut VerifiedFileObjects>,
    ) -> Result<usize, String> {
        self.batch_inner(entries, None, observed, check_links, verified_objects)
    }
    fn batch_inner(
        &mut self,
        entries: &[ScannedFile],
        epoch: Option<i64>,
        mut observed: Option<&mut RoaringTreemap>,
        mut check_links: Option<&mut LinkVerifier<'_>>,
        mut verified_objects: Option<&mut VerifiedFileObjects>,
    ) -> Result<usize, String> {
        let transaction = self
            .connection
            .transaction()
            .map_err(|error| error.to_string())?;
        let mut changed = 0usize;
        let mut visited_objects = std::collections::HashSet::new();
        let mut cacheable_objects = std::collections::HashSet::new();
        let mut verification_complete = true;
        {
            let mut current = transaction.prepare_cached("SELECT id,name,extension,size,modified,created,changed,is_dir,is_symlink,file_id,parent_id,volume_id,flags,modified_ns,changed_ns,accessible FROM files WHERE path=?1").map_err(|error| error.to_string())?;
            let mut invalidate = transaction
                .prepare_cached("DELETE FROM content WHERE path=?1")
                .map_err(|error| error.to_string())?;
            let mut upsert_statement=transaction.prepare_cached("INSERT INTO files(path,name,extension,size,modified,created,changed,is_dir,is_symlink,file_id,parent_id,volume_id,flags,seen,modified_ns,changed_ns) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16) ON CONFLICT(path) DO UPDATE SET name=excluded.name,extension=excluded.extension,size=excluded.size,modified=excluded.modified,created=excluded.created,changed=excluded.changed,is_dir=excluded.is_dir,is_symlink=excluded.is_symlink,file_id=excluded.file_id,parent_id=excluded.parent_id,volume_id=excluded.volume_id,flags=excluded.flags,seen=excluded.seen,modified_ns=excluded.modified_ns,changed_ns=excluded.changed_ns,accessible=1 WHERE files.name!=excluded.name OR files.extension!=excluded.extension OR files.size!=excluded.size OR files.modified!=excluded.modified OR files.created!=excluded.created OR files.changed!=excluded.changed OR files.is_dir!=excluded.is_dir OR files.is_symlink!=excluded.is_symlink OR files.file_id!=excluded.file_id OR files.parent_id!=excluded.parent_id OR files.volume_id!=excluded.volume_id OR files.flags!=excluded.flags OR files.modified_ns!=excluded.modified_ns OR files.changed_ns!=excluded.changed_ns OR files.accessible!=1").map_err(|error| error.to_string())?;
            let mut seen = transaction
                .prepare_cached("UPDATE files SET seen=?2 WHERE path=?1 AND seen!=?2")
                .map_err(|error| error.to_string())?;
            let mut peers = transaction
                .prepare_cached(
                    "SELECT path FROM files WHERE volume_id=?1 AND file_id=?2 AND accessible=1",
                )
                .map_err(|error| error.to_string())?;
            let mut visited_paths = std::collections::HashSet::new();
            let mut next_batch: Vec<ScannedFile>;
            let mut batch = entries;
            loop {
                // The flag forces verification even if this object succeeded in
                // an earlier batch of the same reconciliation.
                let mut objects: HashMap<(String, u64), bool> = HashMap::new();
                for file in batch {
                    // Compare borrowed SQLite values, avoiding per-row copies of
                    // names and volume IDs during unchanged reconciliation.
                    let existing = current
                        .query_row([&file.path], |row| {
                            let id: i64 = row.get(0)?;
                            let old_file_id: i64 = row.get(9)?;
                            let old_volume = row.get_ref(11)?.as_str()?;
                            let old_is_dir: bool = row.get(7)?;
                            let identity_changed =
                                old_file_id as u64 != file.file_id || old_volume != file.volume_id;
                            let content_changed = identity_changed
                                || row.get::<_, i64>(3)? != file.size as i64
                                || row.get::<_, i64>(4)? != file.modified
                                || row.get::<_, i64>(5)? != file.created
                                || row.get::<_, i64>(13)? != file.modified_ns
                                || row.get::<_, i64>(14)? != file.changed_ns;
                            let metadata_changed = content_changed
                                || row.get_ref(1)?.as_str()? != file.name
                                || row.get_ref(2)?.as_str()? != file.extension
                                || row.get::<_, i64>(6)? != file.changed
                                || old_is_dir != file.is_dir
                                || row.get::<_, bool>(8)? != file.is_symlink
                                || row.get::<_, i64>(10)? != file.parent_id as i64
                                || row.get::<_, i64>(12)? != file.flags as i64
                                || row.get::<_, i64>(15)? != 1;
                            let old_object = (metadata_changed
                                && check_links.is_some()
                                && !old_is_dir
                                && old_file_id != 0)
                                .then(|| (old_volume.to_owned(), old_file_id as u64));
                            Ok((id, metadata_changed, content_changed, old_object))
                        })
                        .optional()
                        .map_err(|error| error.to_string())?;
                    let file_changed = existing.as_ref().is_none_or(|row| row.1);
                    let mut id = existing.as_ref().map(|row| row.0);
                    if file_changed {
                        // Invalidate before any fallible SQL or callback. An
                        // unsuccessful attempt must never leave a reusable old
                        // success associated with changed object metadata.
                        if let Some(verified_objects) = verified_objects.as_deref_mut() {
                            verified_objects.forget(&file.volume_id, file.file_id);
                            if let Some((volume, file_id)) =
                                existing.as_ref().and_then(|row| row.3.as_ref())
                            {
                                verified_objects.forget(volume, *file_id);
                            }
                        }
                        if existing.as_ref().is_some_and(|row| row.2) {
                            invalidate
                                .execute([&file.path])
                                .map_err(|error| error.to_string())?;
                        }
                        if let Some(old_object) = existing.and_then(|row| row.3) {
                            objects.insert(old_object, true);
                        }
                        changed += upsert_statement
                            .execute(params![
                                file.path,
                                file.name,
                                file.extension,
                                file.size as i64,
                                file.modified,
                                file.created,
                                file.changed,
                                file.is_dir,
                                file.is_symlink,
                                file.file_id as i64,
                                file.parent_id as i64,
                                file.volume_id,
                                file.flags,
                                epoch.unwrap_or(0),
                                file.modified_ns,
                                file.changed_ns,
                            ])
                            .map_err(|error| error.to_string())?;
                        if id.is_none() {
                            id = Some(transaction.last_insert_rowid());
                        }
                    }
                    // A previous enumeration may have observed two aliases at
                    // different moments. A hard-linked (or unknown-link-count)
                    // object must verify peers even when this path is unchanged.
                    if check_links.is_some()
                        && !file.is_dir
                        && file.file_id != 0
                        && (file_changed || file.link_count != Some(1))
                    {
                        let object = (file.volume_id.clone(), file.file_id);
                        if file.link_count != Some(1) {
                            cacheable_objects.insert(object.clone());
                        }
                        objects
                            .entry(object)
                            .and_modify(|force| *force |= file_changed)
                            .or_insert(file_changed);
                    }
                    if let Some(epoch) = epoch {
                        seen.execute(params![file.path, epoch])
                            .map_err(|error| error.to_string())?;
                    }
                    if let Some(observed) = observed.as_deref_mut() {
                        observed.insert(id.expect("existing or inserted row") as u64);
                    }
                }
                let Some(check_links) = check_links.as_deref_mut() else {
                    break;
                };
                let mut paths = Vec::new();
                for ((volume_id, file_id), force) in objects {
                    if !force {
                        if let Some(verified_objects) = verified_objects.as_deref_mut() {
                            if verified_objects.contains(&volume_id, file_id) {
                                verified_objects.reused += 1;
                                continue;
                            }
                        }
                    }
                    if !visited_objects.insert((volume_id.clone(), file_id)) {
                        continue;
                    }
                    let rows = peers
                        .query_map(params![volume_id, file_id as i64], |row| {
                            row.get::<_, String>(0)
                        })
                        .map_err(|error| error.to_string())?;
                    for path in rows {
                        let path = path.map_err(|error| error.to_string())?;
                        if visited_paths.insert(path.clone()) {
                            paths.push(path);
                        }
                    }
                }
                if paths.is_empty() {
                    break;
                }
                let requested_paths = paths.len();
                let verified = check_links(paths)?;
                // Production verification accounts for every requested path.
                // Unknown/unavailable or incomplete results cannot establish a
                // reusable success, even if the remaining updates can commit.
                verification_complete &= verified.unavailable.is_empty()
                    && verified.entries.len() + verified.removed.len() == requested_paths;
                // Persist failed coverage together with the primary change.
                // Otherwise a crash could make its unchanged replay skip the
                // failed alias while the old alias row still looked accessible.
                if !verified.unavailable.is_empty() {
                    let encoded: Option<String> = transaction
                        .query_row(
                            "SELECT value FROM settings WHERE key='uncovered'",
                            [],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(|error| error.to_string())?;
                    let mut uncovered: Vec<String> = encoded
                        .as_deref()
                        .and_then(|value| serde_json::from_str(value).ok())
                        .unwrap_or_default();
                    for path in verified.unavailable {
                        changed += transaction
                            .execute(
                                "UPDATE files SET accessible=0 WHERE path=?1 AND accessible!=0",
                                [&path],
                            )
                            .map_err(|error| error.to_string())?;
                        uncovered.push(path);
                    }
                    uncovered.sort();
                    uncovered.dedup();
                    let value =
                        serde_json::to_string(&uncovered).map_err(|error| error.to_string())?;
                    transaction.execute("INSERT INTO settings(key,value) VALUES('uncovered',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value WHERE settings.value!=excluded.value", [value]).map_err(|error| error.to_string())?;
                }
                for path in verified.removed {
                    transaction
                        .execute("DELETE FROM content WHERE path=?1", [&path])
                        .map_err(|error| error.to_string())?;
                    changed += transaction
                        .execute("DELETE FROM files WHERE path=?1", [&path])
                        .map_err(|error| error.to_string())?;
                }
                next_batch = verified.entries;
                if next_batch.is_empty() {
                    break;
                }
                batch = &next_batch;
            }
        }
        if changed > 0 {
            mark_search_changed(&transaction)?;
        }
        transaction.commit().map_err(|error| error.to_string())?;
        // Only durable successful verification may influence a later batch.
        // This set belongs to one reconcile call, never another event drain.
        if verification_complete {
            if let Some(verified_objects) = verified_objects {
                for (volume, file_id) in visited_objects.intersection(&cacheable_objects) {
                    verified_objects.confirm(volume, *file_id);
                }
            }
        }
        Ok(changed)
    }
    pub(crate) fn finish_observed(
        &mut self,
        roots: &[String],
        uncovered: &[String],
        observed: &RoaringTreemap,
        event_id: u64,
    ) -> Result<u64, String> {
        let transaction = self
            .connection
            .transaction()
            .map_err(|error| error.to_string())?;
        let mut changed = 0usize;
        for root in uncovered {
            let (prefix, end) = subtree_range(root);
            changed += transaction.execute("UPDATE files SET accessible=0 WHERE accessible!=0 AND (path=?1 OR (path>=?2 AND path<?3))", params![root,prefix,end]).map_err(|error| error.to_string())?;
        }
        let mut removed = Vec::new();
        {
            let mut rows = transaction
                .prepare_cached("SELECT id,path FROM files WHERE path=?1 OR (path>=?2 AND path<?3)")
                .map_err(|error| error.to_string())?;
            for root in roots {
                let (prefix, end) = subtree_range(root);
                let entries = rows
                    .query_map(params![root, prefix, end], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                    })
                    .map_err(|error| error.to_string())?;
                for entry in entries {
                    let (id, path) = entry.map_err(|error| error.to_string())?;
                    if !observed.contains(id as u64)
                        && !uncovered
                            .iter()
                            .any(|root| Path::new(&path).starts_with(root))
                    {
                        removed.push(id);
                    }
                }
            }
        }
        {
            let mut delete = transaction
                .prepare_cached("DELETE FROM files WHERE id=?1")
                .map_err(|error| error.to_string())?;
            for id in removed {
                changed += delete.execute([id]).map_err(|error| error.to_string())?;
            }
        }
        if changed > 0 {
            mark_search_changed(&transaction)?;
        }
        transaction.execute("INSERT INTO settings(key,value) VALUES('event_id',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value WHERE settings.value!=excluded.value", [event_id.to_string()]).map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(changed as u64)
    }
    pub fn finish(
        &mut self,
        roots: &[String],
        uncovered: &[String],
        epoch: i64,
        event_id: u64,
    ) -> Result<u64, String> {
        let transaction = self
            .connection
            .transaction()
            .map_err(|error| error.to_string())?;
        let mut changed = 0usize;
        for root in uncovered {
            let (prefix, end) = subtree_range(root);
            changed += transaction.query_row("SELECT count(*) FROM files WHERE accessible=1 AND (path=?1 OR (path>=?2 AND path<?3))", params![root,prefix,end], |row| row.get::<_,usize>(0)).map_err(|error| error.to_string())?;
            transaction.execute("UPDATE files SET seen=?1,accessible=0 WHERE (accessible!=0 OR seen!=?1) AND (path=?2 OR (path>=?3 AND path<?4))",params![epoch,root,prefix,end]).map_err(|error| error.to_string())?;
        }
        for root in roots {
            let (prefix, end) = subtree_range(root);
            changed += transaction
                .execute(
                    "DELETE FROM files WHERE seen!=?1 AND (path=?2 OR (path>=?3 AND path<?4))",
                    params![epoch, root, prefix, end],
                )
                .map_err(|error| error.to_string())?;
        }
        if changed > 0 {
            mark_search_changed(&transaction)?;
        }
        transaction.execute("INSERT INTO settings(key,value) VALUES('event_id',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[event_id.to_string()]).map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(changed as u64)
    }
    pub(crate) fn invalidate_event_history(&mut self, reason: &str) -> Result<(), String> {
        let previous = json!({"reason":reason,"cursor":self.get("event_id", json!(0)),
            "proof":self.get("scan_resume_proof", Value::Null)});
        self.commit_history_settings(&[
            ("scan_resume_proof", Value::Null),
            ("event_id", json!(0)),
            ("event_history_invalid", json!(reason)),
            ("invalidated_event_history", previous),
        ])
    }
    pub(crate) fn commit_resume_proof(&mut self, proof: Value) -> Result<(), String> {
        self.commit_history_settings(&[
            ("scan_resume_proof", proof),
            ("event_history_invalid", Value::Null),
        ])
    }
    fn commit_history_settings(&mut self, settings: &[(&str, Value)]) -> Result<(), String> {
        let transaction = self
            .connection
            .transaction()
            .map_err(|error| error.to_string())?;
        for (key, value) in settings {
            transaction.execute("INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value WHERE settings.value!=excluded.value", params![key, value.to_string()]).map_err(|error| error.to_string())?;
        }
        transaction.commit().map_err(|error| error.to_string())
    }
    /// Persist a fully reconciled cursor without changing the search snapshot.
    pub fn advance_event_id(&self, event_id: u64) -> Result<(), String> {
        self.set("event_id", &json!(event_id))
    }
    pub fn clear(&mut self) -> Result<(), String> {
        let transaction = self
            .connection
            .transaction()
            .map_err(|error| error.to_string())?;
        if transaction
            .execute("DELETE FROM files", [])
            .map_err(|error| error.to_string())?
            > 0
        {
            mark_search_changed(&transaction)?;
        }
        transaction.commit().map_err(|error| error.to_string())
    }
    pub fn retain_roots(&mut self, roots: &[String]) -> Result<(), String> {
        let paths: Vec<(i64, String)> = {
            let mut s = self
                .connection
                .prepare("SELECT id,path FROM files")
                .map_err(|error| error.to_string())?;
            let rows = s
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(|error| error.to_string())?;
            rows.collect::<Result<_, _>>()
                .map_err(|error| error.to_string())?
        };
        let transaction = self
            .connection
            .transaction()
            .map_err(|error| error.to_string())?;
        let mut changed = 0usize;
        for (id, path) in paths {
            if !roots.iter().any(|r| Path::new(&path).starts_with(r)) {
                changed += transaction
                    .execute("DELETE FROM files WHERE id=?1", [id])
                    .map_err(|error| error.to_string())?;
            }
        }
        if changed > 0 {
            mark_search_changed(&transaction)?;
        }
        transaction.commit().map_err(|error| error.to_string())
    }
    /// Discover indexed direct children without traversing their indexed rows.
    /// A seek past each encountered subtree also finds roots that were unmounted.
    pub(crate) fn indexed_child_scopes(&self, parent: &str) -> Result<Vec<String>, String> {
        let (prefix, end) = subtree_range(parent);
        let mut inclusive = self
            .connection
            .prepare_cached(
                "SELECT path FROM files WHERE path>=?1 AND path<?2 ORDER BY path LIMIT 1",
            )
            .map_err(|error| error.to_string())?;
        let mut exclusive = self
            .connection
            .prepare_cached(
                "SELECT path FROM files WHERE path>?1 AND path<?2 ORDER BY path LIMIT 1",
            )
            .map_err(|error| error.to_string())?;
        let mut cursor = prefix.clone();
        let mut include_cursor = true;
        let mut children = std::collections::BTreeSet::new();
        loop {
            let statement = if include_cursor {
                &mut inclusive
            } else {
                &mut exclusive
            };
            let next: Option<String> = statement
                .query_row(params![cursor, end], |row| row.get(0))
                .optional()
                .map_err(|error| error.to_string())?;
            let Some(path) = next else { break };
            let suffix = &path[prefix.len()..];
            let child = format!("{}{}", prefix, suffix.split('/').next().unwrap());
            children.insert(child.clone());
            if path == child {
                // Do not skip siblings such as Update-old that sort between the
                // exact Update row and its Update/ descendants.
                cursor = path;
                include_cursor = false;
            } else {
                cursor = subtree_range(&child).1;
                include_cursor = true;
            }
        }
        Ok(children.into_iter().collect())
    }
    /// Remove derived entries outside the caller's configured namespace.
    /// Foreign-key and content triggers remove extracted content in this same
    /// transaction. Cancellation rolls back every scope, including its journals.
    pub(crate) fn prune_namespace_scopes(
        &mut self,
        scopes: &[String],
        cancelled: &AtomicBool,
    ) -> Result<Option<u64>, String> {
        if cancelled.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let transaction = self
            .connection
            .transaction()
            .map_err(|error| error.to_string())?;
        let mut removed = 0u64;
        {
            let mut delete = transaction
                .prepare_cached("DELETE FROM files WHERE path=?1 OR (path>=?2 AND path<?3)")
                .map_err(|error| error.to_string())?;
            for scope in scopes {
                if cancelled.load(Ordering::Relaxed) {
                    return Ok(None);
                }
                let root = if scope == "/" {
                    "/"
                } else {
                    scope.trim_end_matches('/')
                };
                let (prefix, end) = subtree_range(root);
                removed += delete
                    .execute(params![root, prefix, end])
                    .map_err(|error| error.to_string())? as u64;
            }
        }
        if cancelled.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let encoded: Option<String> = transaction
            .query_row(
                "SELECT value FROM settings WHERE key='uncovered'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        if let Some(encoded) = encoded {
            let mut uncovered: Vec<String> =
                serde_json::from_str(&encoded).map_err(|error| error.to_string())?;
            let old_count = uncovered.len();
            uncovered.retain(|path| {
                !scopes
                    .iter()
                    .any(|scope| Path::new(path).starts_with(scope))
            });
            if uncovered.len() != old_count {
                transaction
                    .execute(
                        "UPDATE settings SET value=?1 WHERE key='uncovered'",
                        [json!(uncovered).to_string()],
                    )
                    .map_err(|error| error.to_string())?;
            }
        }
        if removed != 0 {
            mark_search_changed(&transaction)?;
        }
        if cancelled.load(Ordering::Relaxed) {
            return Ok(None);
        }
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(Some(removed))
    }
    pub fn entries(&self) -> Result<Vec<IndexedFile>, String> {
        self.read_entries("1")
    }
    fn read_entries(&self, predicate: &str) -> Result<Vec<IndexedFile>, String> {
        let sql = format!(
            "SELECT f.id,{} FROM files f LEFT JOIN content c ON f.path=c.path WHERE f.accessible=1 AND ({predicate}) ORDER BY f.id",
            change_reader::COLUMNS
        );
        let mut statement = self
            .connection
            .prepare(&sql)
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], change_reader::decode_file)
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }
    /// Return a complete delta only when its baseline is still our published
    /// revision. Another process may have consumed the journal; then reload SQL.
    pub fn changes_since(
        &self,
        revision: u64,
        limit: usize,
    ) -> Result<Option<SnapshotDelta>, String> {
        if self.get("changes_base_revision", json!(null)).as_u64() != Some(revision) {
            return Ok(None);
        }
        change_reader::read(&self.connection, change_reader::Journal::Snapshot, limit)
    }
    pub fn clear_changes(&self, revision: u64) -> Result<(), String> {
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(|error| error.to_string())?;
        let current: Option<String> = transaction
            .query_row(
                "SELECT value FROM settings WHERE key='revision'",
                [],
                |row| row.get(0),
            )
            .ok();
        if current
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0)
            == revision
        {
            transaction
                .execute("DELETE FROM snapshot_changes", [])
                .map_err(|error| error.to_string())?;
            transaction.execute("INSERT INTO settings VALUES('changes_base_revision',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[revision.to_string()]).map_err(|error| error.to_string())?;
        }
        transaction.commit().map_err(|error| error.to_string())
    }
    pub fn put_content(
        &mut self,
        path: &str,
        text: &str,
        properties: &Value,
    ) -> Result<(), String> {
        let transaction = self
            .connection
            .transaction()
            .map_err(|error| error.to_string())?;
        let changed = transaction.execute("INSERT INTO content(path,body,properties) VALUES(?1,?2,?3) ON CONFLICT(path) DO UPDATE SET body=excluded.body,properties=excluded.properties WHERE content.body!=excluded.body OR content.properties!=excluded.properties",params![path,text,properties.to_string()]).map_err(|error| error.to_string())?;
        if changed == 0 {
            return transaction.commit().map_err(|error| error.to_string());
        }
        mark_search_changed(&transaction)?;
        transaction
            .execute("DELETE FROM content_fts WHERE path=?1", [path])
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "INSERT INTO content_fts(path,body) VALUES(?1,?2)",
                params![path, text],
            )
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())
    }
    pub fn content_for(&self, path: &str) -> Result<Option<String>, String> {
        use rusqlite::OptionalExtension;
        self.connection
            .query_row("SELECT body FROM content WHERE path=?1", [path], |r| {
                r.get(0)
            })
            .optional()
            .map_err(|error| error.to_string())
    }
    pub fn all_content(&self) -> Result<HashMap<String, String>, String> {
        let mut s = self
            .connection
            .prepare("SELECT path,body FROM content")
            .map_err(|error| error.to_string())?;
        let rows = s
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<HashMap<_, _>, _>>()
            .map_err(|error| error.to_string())
    }
    pub fn cache_write(&self, snapshot: &SearchSnapshot, revision: u64) -> Result<(), String> {
        if self.get("revision", json!(0)).as_u64().unwrap_or(0) != revision {
            return Ok(());
        }
        crate::snapshot_cache::write(&self.cache_path, snapshot, revision)?;
        // Publishing the file precedes one atomic journal checkpoint. A crash
        // between the two leaves mismatched cache metadata, which is rejected.
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(|error| error.to_string())?;
        if self.get("revision", json!(0)).as_u64().unwrap_or(0) == revision {
            self.set("cache_base_generation", &json!(snapshot.generation))?;
            self.set("cache_base_revision", &json!(revision))?;
            self.set("cache_journal_overflow", &json!(false))?;
            self.set("cache_dirty", &json!(false))?;
            transaction
                .execute("DELETE FROM cache_changes", [])
                .map_err(|error| error.to_string())?;
        }
        transaction.commit().map_err(|error| error.to_string())
    }
    pub fn cache_read(&self) -> Option<(SearchSnapshot, u64)> {
        // Pin metadata, journal IDs and their current rows to one SQLite read
        // transaction. Replay never checkpoints or rewrites the binary cache.
        let transaction = self.connection.unchecked_transaction().ok()?;
        let generation = self.get("generation", json!(0)).as_u64()?;
        let revision = self.get("revision", json!(0)).as_u64()?;
        let base_generation = self.get("cache_base_generation", json!(null)).as_u64()?;
        let base_revision = self.get("cache_base_revision", json!(null)).as_u64()?;
        if base_revision > revision
            || self.get("cache_journal_overflow", json!(false)) == json!(true)
        {
            return None;
        }
        let (base, _) =
            crate::snapshot_cache::read(&self.cache_path, base_generation, base_revision)?;
        let mut snapshot = if revision == base_revision {
            base
        } else {
            let changes = change_reader::read(
                &transaction,
                change_reader::Journal::Cache,
                cache_journal::MAX_IDS,
            )
            .ok()??;
            // Startup has its own hard journal bound. Applying the publication
            // fraction here would reintroduce a 2,000-ID cliff on small indexes.
            SearchSnapshot::apply_changes(changes, generation, &base).ok()?
        };
        snapshot.generation = generation;
        snapshot.content_revision = self.get("content_revision", json!(0)).as_u64()?;
        transaction.commit().ok()?;
        Some((snapshot, generation))
    }
    pub fn cache_is_dirty(&self) -> bool {
        self.get("cache_dirty", json!(true)) != json!(false)
    }
}

// Binary path bounds use the slash separator's immediate successor, avoiding
// wildcard escaping and full-table substr scans for each denied subtree.
fn subtree_range(root: &str) -> (String, String) {
    let stem = root.trim_end_matches('/');
    (format!("{stem}/"), format!("{stem}0"))
}
fn mark_search_changed(connection: &Connection) -> Result<(), String> {
    connection.execute("INSERT INTO settings(key,value) VALUES('cache_dirty','true') ON CONFLICT(key) DO UPDATE SET value='true'",[]).map_err(|error| error.to_string())?;
    connection.execute("INSERT INTO settings(key,value) VALUES('revision','1') ON CONFLICT(key) DO UPDATE SET value=CAST(value AS INTEGER)+1",[]).map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(test)]
#[path = "namespace_prune_tests.rs"]
mod namespace_prune_tests;

#[cfg(test)]
#[path = "bounded_cache_tests.rs"]
mod bounded_cache_tests;
