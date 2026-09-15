use crate::label_pool::LabelPool;
use crate::shared_text::SharedText;
#[path = "cache_journal.rs"]
mod cache_journal;
#[path = "change_reader.rs"]
mod change_reader;
#[path = "ordered_merge.rs"]
mod ordered_merge;
#[cfg(test)]
#[path = "streaming_snapshot_tests.rs"]
mod streaming_snapshot_tests;

pub use crate::chunked_vec::ChunkedVec;
pub use crate::entry_table::{EntryTable, EntryView, FileEntry};

use crate::{
    metadata_postings::MetadataPostings,
    numeric_columns::NumericColumns,
    query::{Field, fold},
    result_order::{ResultOrder, sort_slots},
    scanner::ScannedFile,
};
use roaring::{RoaringBitmap, RoaringTreemap};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexedFile {
    pub id: i64,
    pub path: String,
    pub name: String,
    pub extension: Arc<str>,
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
    pub volume_id: Arc<str>,
    pub flags: u32,
    #[serde(default)]
    pub properties: Value,
    #[serde(default)]
    pub content_indexed: bool,
    #[serde(skip)]
    pub folded_name: SharedText,
    #[serde(skip)]
    pub folded_extension: Arc<str>,
    #[serde(skip)]
    pub folded_path: SharedText,
    #[serde(skip)]
    pub search_name: SharedText,
    #[serde(skip)]
    pub search_path: SharedText,
    #[serde(skip)]
    pub parent: SharedText,
}
impl IndexedFile {
    fn share_labels(&mut self, pool: &mut Arc<LabelPool>) {
        LabelPool::share(pool, &mut self.extension);
        LabelPool::share(pool, &mut self.folded_extension);
        LabelPool::share(pool, &mut self.volume_id);
    }
    pub fn prepare(&mut self) {
        self.folded_name = fold(&self.name).into();
        self.folded_extension = fold(&self.extension).into();
        self.folded_path = fold(&self.path).into();
        self.search_name = shared_search_fold(&self.name, &self.folded_name);
        self.search_path = shared_search_fold(&self.path, &self.folded_path);
        self.parent = Path::new(&self.path)
            .parent()
            .map(|parent| SharedText::from(parent.to_string_lossy().as_ref()))
            .unwrap_or_default()
    }
    fn prepare_replacement(&mut self, previous: Option<EntryView<'_>>) {
        let Some(previous) = previous else {
            return self.prepare();
        };
        if self.name == previous.name() {
            self.folded_name = previous.folded_name().into();
            self.search_name = previous.search_name().into();
        } else {
            self.folded_name = fold(&self.name).into();
            self.search_name = shared_search_fold(&self.name, &self.folded_name);
        }
        self.folded_extension = if self.extension.as_ref() == previous.extension() {
            previous.folded_extension().into()
        } else {
            fold(&self.extension).into()
        };
        if self.path == previous.path() {
            self.folded_path = previous.folded_path().into();
            self.search_path = previous.search_path().into();
            self.parent = previous.parent().into();
        } else {
            self.folded_path = fold(&self.path).into();
            self.search_path = shared_search_fold(&self.path, &self.folded_path);
            let parent = Path::new(&self.path)
                .parent()
                .map(|parent| parent.to_string_lossy());
            self.parent = if parent.as_deref() == Some(previous.parent()) {
                previous.parent().into()
            } else {
                parent
                    .map(|parent| SharedText::from(parent.as_ref()))
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
fn shared_search_fold(text: &str, folded: &SharedText) -> SharedText {
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
    pub(crate) fn from_entries(entries: &EntryTable) -> Result<Self, &'static str> {
        let mut slots = Self::default();
        for (slot, file) in entries.iter().enumerate() {
            slots.append_at(file.id(), entries, slot)?;
        }
        Ok(slots)
    }
    #[cfg(test)]
    pub(crate) fn layout(&self) -> (usize, usize) {
        (self.sorted_prefix, self.exceptions.len())
    }
    fn get(&self, id: i64, entries: &EntryTable) -> Option<usize> {
        let mut lower = 0;
        let mut upper = self.sorted_prefix;
        while lower < upper {
            let middle = lower + (upper - lower) / 2;
            match entries.at(middle).id().cmp(&id) {
                std::cmp::Ordering::Less => lower = middle + 1,
                std::cmp::Ordering::Greater => upper = middle,
                std::cmp::Ordering::Equal => return Some(middle),
            }
        }
        self.exceptions.get(&id).map(|slot| *slot as usize)
    }

    fn append(&mut self, id: i64, entries: &EntryTable) -> Result<usize, &'static str> {
        self.append_at(id, entries, entries.len())
    }
    fn append_at(
        &mut self,
        id: i64,
        entries: &EntryTable,
        length: usize,
    ) -> Result<usize, &'static str> {
        let slot = u32::try_from(length).map_err(|_| "snapshot_slot_overflow")?;
        if self.sorted_prefix == length
            && length
                .checked_sub(1)
                .is_none_or(|last| entries.at(last).id() < id)
        {
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
    pub(crate) index_section: Arc<std::sync::OnceLock<Arc<crate::snapshot_cache::Section>>>,
    // The cache owns the reusable tree; active queries hold their own Arcs.
    // A weak build result coalesces overlapping readers even when a tree is
    // too large for admission, without retaining it in every old snapshot.
    pub(crate) hierarchy: Mutex<std::sync::Weak<crate::relations::Hierarchy>>,
    // Stable slots keep postings valid when a directory entry is deleted. The
    // bitmap determines visibility; a later full rebuild compacts old slots.
    pub entries: EntryTable,
    labels: Arc<LabelPool>,
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
    name_column: crate::name_column::NameColumn,
    pub generation: u64,
    pub content_revision: u64,
    cache: crate::derived_cache::DerivedCache,
}
impl SearchSnapshot {
    pub(crate) fn directory_hierarchy(
        &self,
        cancelled: &AtomicBool,
    ) -> Result<Arc<crate::relations::Hierarchy>, String> {
        if let Some(tree) = self.cache.hierarchy() {
            return Ok(tree);
        }
        let mut building = self
            .hierarchy
            .lock()
            .map_err(|_| "Hierarchy lock poisoned")?;
        if let Some(tree) = building.upgrade() {
            return Ok(tree);
        }
        let tree = Arc::new(crate::relations::Hierarchy::build(self, cancelled)?);
        *building = Arc::downgrade(&tree);
        self.cache.insert_hierarchy(tree.clone());
        Ok(tree)
    }

    pub(crate) fn from_prepared_cache(parts: crate::snapshot_cache::PreparedSnapshot) -> Self {
        let path_rank = Arc::new(order_ranks(parts.entries.len(), &parts.path_order));
        // These contiguous numeric blocks are cheap derived columns. Restoring
        // them requires no folding, sorting or cache format rewrite.
        let numeric_columns = NumericColumns::build(&parts.entries);
        let name_column = crate::name_column::NameColumn::build(&parts.entries);
        let file_slots = parts.file_slots;
        Self {
            hierarchy: Mutex::new(std::sync::Weak::new()),
            index_section: Arc::default(),
            file_slots,
            entries: parts.entries,
            labels: parts.labels,
            live: parts.live,
            trigrams: parts.trigrams,
            name_order: parts.name_order,
            path_order: parts.path_order,
            path_ties: parts.path_ties,
            path_rank,
            metadata_postings: parts.metadata_postings,
            numeric_columns,
            name_column,
            generation: parts.generation,
            content_revision: parts.content_revision,
            cache: crate::derived_cache::DerivedCache::default(),
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
    pub fn visible_entries(&self) -> impl Iterator<Item = EntryView<'_>> {
        self.live
            .iter()
            .map(|index| self.entries.at(index as usize))
    }
    pub fn new(entries: Vec<IndexedFile>, generation: u64) -> Self {
        Self::build_with_metrics(entries, generation, |_, _| {})
    }
    /// Observe construction phases without opening or changing persistent storage.
    pub fn build_with_metrics(
        entries: Vec<IndexedFile>,
        generation: u64,
        record: impl FnMut(&str, std::time::Duration),
    ) -> Self {
        Self::build_rows_with_metrics(entries.into_iter().map(Ok), generation, record)
            .expect("In-memory snapshot rows cannot fail to decode")
    }
    /// Consume fallible rows without retaining a second full metadata array.
    /// No partially built snapshot is returned when a row fails to decode.
    pub fn from_rows(
        rows: impl IntoIterator<Item = Result<IndexedFile, String>>,
        generation: u64,
    ) -> Result<Self, String> {
        Self::build_rows_with_metrics(rows, generation, |_, _| {})
    }
    fn build_rows_with_metrics(
        rows: impl IntoIterator<Item = Result<IndexedFile, String>>,
        generation: u64,
        mut record: impl FnMut(&str, std::time::Duration),
    ) -> Result<Self, String> {
        let started = std::time::Instant::now();
        let entries = EntryTable::from_rows(rows.into_iter().map(|row| {
            let mut row = row?;
            row.prepare();
            Ok(row)
        }))?;
        record("prepare_compact_rows", started.elapsed());
        let started = std::time::Instant::now();
        let mut trigrams: HashMap<[u8; 3], Arc<RoaringBitmap>> = HashMap::new();
        let mut cpu = crate::cpu_executor::enter_background();
        for (slot, row) in entries.iter().enumerate() {
            if slot.is_multiple_of(crate::entry_table::CHUNK_LENGTH) {
                cpu.checkpoint();
            }
            for tri in row.search_name().as_bytes().windows(3) {
                Arc::make_mut(trigrams.entry([tri[0], tri[1], tri[2]]).or_default())
                    .insert(slot as u32);
            }
        }
        record("name_postings", started.elapsed());
        drop(cpu);
        Ok(Self::build_prepared_indexes(
            entries,
            Arc::new(LabelPool::default()),
            trigrams,
            generation,
            record,
        ))
    }
    /// Build slot-based derived structures from immutable, already folded rows.
    /// Reusing the row Arcs avoids cloning paths or recomputing Unicode columns
    /// when an old SQLite ID returns after visibility-based compaction.
    fn build_prepared_indexes(
        entries: EntryTable,
        labels: Arc<LabelPool>,
        trigrams: HashMap<[u8; 3], Arc<RoaringBitmap>>,
        generation: u64,
        mut record: impl FnMut(&str, std::time::Duration),
    ) -> Self {
        let mut cpu = crate::cpu_executor::enter_background();
        let file_slots = FileSlots::from_entries(&entries).expect("file IDs must be unique");
        let live = (0..entries.len() as u32).collect();
        let started = std::time::Instant::now();
        let metadata_postings = MetadataPostings::build(
            entries
                .iter()
                .enumerate()
                .map(|(slot, file)| (slot as u32, file)),
        );
        record("exact_metadata_postings", started.elapsed());
        cpu.checkpoint();
        let started = std::time::Instant::now();
        let numeric_columns = NumericColumns::build(&entries);
        record("numeric_columns", started.elapsed());
        cpu.checkpoint();
        let started = std::time::Instant::now();
        let name_column = crate::name_column::NameColumn::build(&entries);
        record("name_column", started.elapsed());
        cpu.checkpoint();
        let started = std::time::Instant::now();
        let mut name_order: Vec<u32> = (0..entries.len() as u32).collect();
        let mut comparisons = 0usize;
        name_order.sort_unstable_by(|a, b| {
            comparisons += 1;
            if comparisons.is_multiple_of(crate::entry_table::CHUNK_LENGTH) {
                cpu.checkpoint();
            }
            compare_names(&entries.at(*a as usize), &entries.at(*b as usize))
        });
        record("name_sort", started.elapsed());
        cpu.checkpoint();
        let started = std::time::Instant::now();
        let mut path_order: Vec<u32> = (0..entries.len() as u32).collect();
        let path_spec = ResultOrder::path();
        path_order.sort_unstable_by(|a, b| {
            comparisons += 1;
            if comparisons.is_multiple_of(crate::entry_table::CHUNK_LENGTH) {
                cpu.checkpoint();
            }
            path_spec.compare(&entries.at(*a as usize), &entries.at(*b as usize))
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
            hierarchy: Mutex::new(std::sync::Weak::new()),
            index_section: Arc::default(),
            entries,
            labels,
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
            name_column,
            cache: crate::derived_cache::DerivedCache::default(),
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
        let mut old_rows: Vec<_> = previous.live.iter().collect();
        if previous.file_slots.sorted_prefix != previous.entries.len() {
            old_rows.sort_unstable_by_key(|slot| previous.entries.at(*slot as usize).id());
        }
        let mut old = old_rows.into_iter().peekable();
        let mut replacements = unique.into_iter().peekable();
        // Materialize at most one block of I/O rows during compaction. The old
        // immutable snapshot remains available to its readers throughout.
        let rows = std::iter::from_fn(|| {
            loop {
                let next_id = replacements.peek().map(|(id, _)| *id);
                if old.peek().is_some_and(|slot| {
                    next_id.is_none_or(|id| previous.entries.at(*slot as usize).id() < id)
                }) {
                    return Some(Ok(previous
                        .entries
                        .at(old.next().unwrap() as usize)
                        .to_owned_file()));
                }
                let (id, replacement) = replacements.next()?;
                if old
                    .peek()
                    .is_some_and(|slot| previous.entries.at(*slot as usize).id() == id)
                {
                    old.next();
                }
                if let Some(mut row) = replacement {
                    row.prepare_replacement(
                        previous
                            .slot_for_id(id)
                            .map(|slot| previous.entries.at(slot)),
                    );
                    return Some(Ok(row));
                }
            }
        });
        let entries = EntryTable::from_rows(rows).expect("Prepared in-memory rows are valid");
        let labels = Arc::new(LabelPool::default());
        let mut trigrams: HashMap<[u8; 3], Arc<RoaringBitmap>> = HashMap::new();
        let mut cpu = crate::cpu_executor::enter_background();
        for (slot, file) in entries.iter().enumerate() {
            if slot.is_multiple_of(crate::entry_table::CHUNK_LENGTH) {
                cpu.checkpoint();
            }
            for trigram in file.search_name().as_bytes().windows(3) {
                Arc::make_mut(
                    trigrams
                        .entry([trigram[0], trigram[1], trigram[2]])
                        .or_default(),
                )
                .insert(slot as u32);
            }
        }
        drop(cpu);
        let mut snapshot =
            Self::build_prepared_indexes(entries, labels, trigrams, generation, |_, _| {});
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
        let mut cpu = crate::cpu_executor::enter_background();
        // Stable ordering preserves last-observation-wins for repeated IDs.
        // It is independent of the physical slot ordering of existing rows.
        changes.sort_by_key(|(id, _)| *id);
        let mut entries = previous.entries.clone();
        let mut labels = previous.labels.clone();
        let mut file_slots = previous.file_slots.clone();
        let mut live = previous.live.clone();
        let mut trigrams = previous.trigrams.clone();
        let mut metadata_postings = previous.metadata_postings.clone();
        let mut numeric_columns = previous.numeric_columns.clone();
        let mut order_changes = RoaringBitmap::new();
        let mut path_changes = RoaringBitmap::new();
        let mut changed_slots = RoaringBitmap::new();
        for (change_index, (id, replacement)) in changes.into_iter().enumerate() {
            if change_index.is_multiple_of(crate::entry_table::CHUNK_LENGTH) {
                cpu.checkpoint();
            }
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
                    .is_none_or(|(old, new)| old.path() != new.path)
            {
                path_changes.insert(slot as u32);
            }
            let index_changed = match (old, replacement.as_ref()) {
                (Some(old), Some(new)) if was_live => {
                    old.name() != new.name || old.path() != new.path
                }
                _ => true,
            };
            let name_changed = match (old, replacement.as_ref()) {
                (Some(old), Some(new)) if was_live => old.name() != new.name,
                _ => true,
            };
            let metadata_changed = match (old, replacement.as_ref()) {
                (Some(old), Some(new)) if was_live => {
                    old.name() != new.name
                        || old.extension() != new.extension.as_ref()
                        || old.is_dir() != new.is_dir
                        || old.is_symlink() != new.is_symlink
                }
                _ => true,
            };
            if was_live && metadata_changed {
                metadata_postings.remove(slot as u32, &old.unwrap());
            }
            if name_changed && was_live {
                let postings = Arc::make_mut(&mut trigrams);
                for tri in old.unwrap().search_name().as_bytes().windows(3) {
                    if let Some(bitmap) = postings.get_mut(&[tri[0], tri[1], tri[2]]) {
                        Arc::make_mut(bitmap).remove(slot as u32);
                    }
                }
            }
            if let Some(mut entry) = replacement {
                entry.prepare_replacement(old);
                entry.share_labels(&mut labels);
                if old.is_none_or(|old| {
                    old.size() != entry.size
                        || old.modified() != entry.modified
                        || old.created() != entry.created
                        || old.is_dir() != entry.is_dir
                }) {
                    numeric_columns.set(slot as u32, &entry);
                }
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
                    entries.push(entry);
                } else {
                    entries.set(slot, entry);
                }
                live.insert(slot as u32);
            } else {
                live.remove(slot as u32);
            }
            if index_changed {
                order_changes.insert(slot as u32);
            }
        }
        drop(cpu);
        entries.finish_update();
        let mut cpu = crate::cpu_executor::enter_background();
        let name_order = if order_changes.is_empty() {
            previous.name_order.clone()
        } else {
            Arc::new(updated_order(
                &previous.name_order,
                &previous.entries,
                &order_changes,
                &live,
                &entries,
                &ResultOrder::name(),
            ))
        };
        cpu.checkpoint();
        numeric_columns.finish_update(&entries);
        let name_column = crate::name_column::NameColumn::build(&entries);
        let old_orders = previous.cache.orders();
        let (path_order, path_rank, path_ties) = if path_changes.is_empty() {
            (
                previous.path_order.clone(),
                previous.path_rank.clone(),
                previous.path_ties.clone(),
            )
        } else {
            let order = updated_order(
                &previous.path_order,
                &previous.entries,
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
                            order.affected_by_change(&old, &entries.at(*slot as usize))
                        })
                    })
                    .collect();
                if relevant.is_empty() {
                    (order, slots)
                } else {
                    let updated = updated_order(
                        &slots,
                        &previous.entries,
                        &relevant,
                        &live,
                        &entries,
                        &order,
                    );
                    (order, Arc::new(updated))
                }
            })
            .collect();
        let index_section = if live == previous.live
            && Arc::ptr_eq(&trigrams, &previous.trigrams)
            && Arc::ptr_eq(&name_order, &previous.name_order)
            && Arc::ptr_eq(&path_order, &previous.path_order)
            && Arc::ptr_eq(&path_ties, &previous.path_ties)
            && metadata_postings.shares_storage(&previous.metadata_postings)
        {
            previous.index_section.clone()
        } else {
            Arc::default()
        };
        Ok(Self {
            hierarchy: Mutex::new(std::sync::Weak::new()),
            index_section,
            entries,
            labels,
            file_slots,
            live,
            trigrams,
            name_order,
            path_order,
            path_rank,
            path_ties,
            metadata_postings,
            numeric_columns,
            name_column,
            generation,
            content_revision: previous.content_revision,
            cache: crate::derived_cache::DerivedCache::from_orders(sort_orders),
        })
    }
    pub(crate) fn cached_page(&self, key: &str) -> Option<Arc<ResultPage>> {
        self.cache.page(key)
    }
    pub(crate) fn cache_page(&self, key: String, page: Arc<ResultPage>) {
        self.cache.insert_page(key, page);
    }
    pub(crate) fn cached_matches(&self, key: &str) -> Option<Arc<RoaringBitmap>> {
        self.cache.matches(key)
    }
    pub(crate) fn cache_matches(&self, key: String, matches: Arc<RoaringBitmap>) {
        self.cache.insert_matches(key, matches);
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
        self.cache.order(order)
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
                    |a, b| {
                        order.compare(&self.entries.at(a as usize), &self.entries.at(b as usize))
                    },
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
                |a, b| order.compare(&self.entries.at(a as usize), &self.entries.at(b as usize)),
                cancelled,
            )?;
        }
        let slots = Arc::new(slots);
        if let Some(existing) = self.cache.order(order) {
            return Ok(existing);
        }
        self.cache.insert_order(order.clone(), slots.clone());
        Ok(slots)
    }
    fn supports_exact(query: &crate::query::Query) -> bool {
        use crate::query::Query;
        match query {
            Query::And(queries) | Query::Or(queries) => queries.iter().all(Self::supports_exact),
            Query::Not(query) => Self::supports_exact(query),
            Query::Term(crate::query::Term::Resolved(result)) => result.unknown.is_empty(),
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
            Query::Term(crate::query::Term::Resolved(result)) => Ok(result
                .yes
                .iter()
                .filter_map(|id| self.slot_for_id(id as i64).map(|slot| slot as u32))
                .filter(|slot| self.live.contains(*slot))
                .collect()),
            Query::Term(term) if NumericColumns::supports(term) => self
                .numeric_columns
                .exact(term, &self.entries, &self.live, cancelled)?
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
    pub(crate) fn match_name_columns(
        &self,
        query: &crate::query::Query,
        exclusions: &[crate::query::Query],
        cancelled: &AtomicBool,
    ) -> Result<Option<RoaringBitmap>, String> {
        use crate::name_column::NameExpression;
        if exclusions.is_empty()
            && let Some((finder, mode)) = query.path_substring()
        {
            return crate::name_column::match_paths(
                &self.entries,
                finder,
                mode,
                &self.live,
                cancelled,
            )
            .map(Some);
        }
        let Some(expression) = NameExpression::compile(query) else {
            return Ok(None);
        };
        let Some(exclusions) = exclusions
            .iter()
            .map(NameExpression::compile)
            .collect::<Option<Vec<_>>>()
        else {
            return Ok(None);
        };
        let candidates = self.candidates_with_cancellation(query, cancelled)?;
        let visible = candidates.map(|set| set & &self.live);
        self.name_column
            .evaluate(
                &expression,
                &exclusions,
                visible.as_ref().unwrap_or(&self.live),
                cancelled,
            )
            .map(Some)
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
        let mut postings = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for text in literals {
            for tri in text.as_bytes().windows(3) {
                if cancelled.load(Ordering::Relaxed) {
                    return Err("Query cancelled".into());
                }
                let key = [tri[0], tri[1], tri[2]];
                if !seen.insert(key) {
                    continue;
                }
                let Some(matches) = self.trigrams.get(&key) else {
                    return Ok(Some(RoaringBitmap::new()));
                };
                postings.push(matches.as_ref());
            }
        }
        // Select a small starting set before allocating a result. Intersecting
        // every other posting against that set avoids copying a common prefix's
        // potentially huge bitmap. Selection is linear, not a full sort.
        let Some((smallest, _)) = postings.iter().enumerate().min_by_key(|(_, set)| set.len())
        else {
            return Ok(None);
        };
        let mut result = postings.swap_remove(smallest).clone();
        for matches in postings {
            if cancelled.load(Ordering::Relaxed) {
                return Err("Query cancelled".into());
            }
            if result.is_empty() {
                break;
            }
            result &= matches;
        }
        let result = Some(result);
        Ok(result)
    }
}
pub(crate) fn updated_order(
    previous: &[u32],
    previous_entries: &EntryTable,
    changed: &RoaringBitmap,
    live: &RoaringBitmap,
    entries: &EntryTable,
    order: &ResultOrder,
) -> Vec<u32> {
    let mut replacements: Vec<_> = changed.iter().filter(|slot| live.contains(*slot)).collect();
    replacements
        .sort_unstable_by(|a, b| order.compare(&entries.at(*a as usize), &entries.at(*b as usize)));
    let mut result = Vec::with_capacity(live.len() as usize);
    // For sparse deltas, locate old keys using the old immutable rows. New
    // values may already have moved elsewhere in sort order. Copy unchanged
    // runs directly instead of checking every old slot against the delta.
    // Dense deltas retain the linear membership pass to bound lookup work.
    // A comparison follows record/string references and performs natural-key
    // comparison; it is much more expensive than a bitmap membership check.
    // Calibrated conservatively against the sparse/dense crossover profile.
    const KEY_PROBE_RELATIVE_COST: u64 = 32;
    let lookup_work = changed
        .len()
        .saturating_mul(u64::from(previous.len().max(1).ilog2()) + 1)
        .saturating_mul(KEY_PROBE_RELATIVE_COST);
    if lookup_work < previous.len() as u64 {
        let mut removed = Vec::with_capacity(changed.len() as usize);
        for slot in changed {
            if let Some(old) = previous_entries.get(slot as usize)
                && let Ok(position) = previous.binary_search_by(|other| {
                    order.compare(&previous_entries.at(*other as usize), &old)
                })
            {
                removed.push(position);
            }
        }
        removed.sort_unstable();
        let mut start = 0;
        for position in removed {
            result.extend_from_slice(&previous[start..position]);
            start = position + 1;
        }
        result.extend_from_slice(&previous[start..]);
    } else {
        result.extend(
            previous
                .iter()
                .copied()
                .filter(|slot| !changed.contains(*slot)),
        );
    }
    ordered_merge::insert_sorted(&mut result, &replacements, |a, b| {
        order.compare(&entries.at(a as usize), &entries.at(b as usize))
    });
    result
}
#[cfg(test)]
pub(crate) fn linear_updated_order_reference(
    previous: &[u32],
    changed: &RoaringBitmap,
    entries: &EntryTable,
    order: &ResultOrder,
) -> Vec<u32> {
    // Previous algorithm for update-only crossover profiling, not a runtime path.
    let mut replacements: Vec<_> = changed.iter().collect();
    replacements
        .sort_unstable_by(|a, b| order.compare(&entries.at(*a as usize), &entries.at(*b as usize)));
    let mut result = Vec::with_capacity(previous.len());
    result.extend(
        previous
            .iter()
            .copied()
            .filter(|slot| !changed.contains(*slot)),
    );
    ordered_merge::insert_sorted(&mut result, &replacements, |a, b| {
        order.compare(&entries.at(a as usize), &entries.at(b as usize))
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
fn same_natural_path(entries: &EntryTable, first: u32, second: u32) -> bool {
    entries
        .at(first as usize)
        .folded_path()
        .natural_cmp(entries.at(second as usize).folded_path())
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
    entries: &EntryTable,
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
fn compare_names(a: &impl FileEntry, b: &impl FileEntry) -> std::cmp::Ordering {
    crate::query::natural_cmp_folded(a.folded_name(), b.folded_name())
        .then_with(|| a.path().cmp(&b.path()))
        .then_with(|| a.id().cmp(&b.id()))
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
pub(crate) type LinkVerifier<'a> = dyn FnMut(Vec<String>) -> Result<LinkVerification, String> + 'a;
// About one MiB of inode-set storage for a few volumes. Clearing the optional
// cache only restores repeated verification; it never changes indexed results.
const MAX_VERIFIED_LINK_OBJECTS: usize = 65_536;
#[derive(Clone, Default)]
pub(crate) struct VerifiedFileObjects {
    by_volume: HashMap<String, std::collections::HashSet<u64>>,
    count: usize,
    reused: u64,
    insertion_order: VecDeque<(String, u64)>,
    mount_identity: Option<u64>,
}
impl VerifiedFileObjects {
    pub(crate) fn bind_mount(&mut self, identity: u64) {
        if self.mount_identity != Some(identity) {
            let reused = self.reused;
            *self = Self::default();
            self.reused = reused;
            self.mount_identity = Some(identity);
        }
    }
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
            while let Some((old_volume, old_id)) = self.insertion_order.pop_front() {
                if self.contains(&old_volume, old_id) {
                    self.forget(&old_volume, old_id);
                    break;
                }
            }
        }
        self.by_volume
            .entry(volume.into())
            .or_default()
            .insert(file_id);
        self.count += 1;
        self.insertion_order.push_back((volume.into(), file_id));
        if self.insertion_order.len() > MAX_VERIFIED_LINK_OBJECTS * 2 {
            let live = &self.by_volume;
            let mut seen = std::collections::HashSet::new();
            self.insertion_order.retain(|(volume, id)| {
                live.get(volume).is_some_and(|ids| ids.contains(id))
                    && seen.insert((volume.clone(), *id))
            });
        }
    }
    pub(crate) fn reused_objects(&self) -> u64 {
        self.reused
    }
}
// Retain a small reusable WAL allocation after large transactions/read views.
// This is a post-reset retention limit, never a cap on live recovery records.
const WAL_RETAINED_BYTES: i64 = 16 * 1024 * 1024;

pub struct IndexStore {
    pub connection: Connection,
    pub cache_path: std::path::PathBuf,
}
impl IndexStore {
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?
        }
        let mut connection = Connection::open(path).map_err(|error| error.to_string())?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| error.to_string())?;
        let version = Self::schema_version(&connection)?;
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
        // SQLite defaults to retaining the largest WAL indefinitely. Bound only
        // the spare allocation on its normal reset path; preserve the default
        // automatic checkpoint cadence and never force a blocking checkpoint.
        connection
            .pragma_update(None, "journal_size_limit", WAL_RETAINED_BYTES)
            .map_err(|error| error.to_string())?;
        if version == 0 {
            let transaction = connection
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(|error| error.to_string())?;
            // A concurrent opener may have initialized the empty database.
            if Self::schema_version(&transaction)? == 0 {
                transaction.execute_batch("
                  CREATE TABLE IF NOT EXISTS files(id INTEGER PRIMARY KEY,path TEXT UNIQUE NOT NULL,name TEXT NOT NULL,extension TEXT NOT NULL,size INTEGER NOT NULL,modified INTEGER NOT NULL,created INTEGER NOT NULL,changed INTEGER NOT NULL,is_dir INTEGER NOT NULL,is_symlink INTEGER NOT NULL,file_id INTEGER NOT NULL,parent_id INTEGER NOT NULL,volume_id TEXT NOT NULL,flags INTEGER NOT NULL,seen INTEGER NOT NULL,modified_ns INTEGER NOT NULL DEFAULT 0,changed_ns INTEGER NOT NULL DEFAULT 0,accessible INTEGER NOT NULL DEFAULT 1);
                  CREATE TABLE IF NOT EXISTS content(path TEXT PRIMARY KEY REFERENCES files(path) ON DELETE CASCADE,body TEXT NOT NULL,properties TEXT NOT NULL DEFAULT '{}');
                  CREATE VIRTUAL TABLE IF NOT EXISTS content_fts USING fts5(path UNINDEXED,body,tokenize='unicode61');
                  CREATE TRIGGER IF NOT EXISTS content_delete AFTER DELETE ON content BEGIN DELETE FROM content_fts WHERE path=old.path; END;
                  CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
                  CREATE INDEX IF NOT EXISTS file_identity ON files(volume_id,file_id);
                  CREATE INDEX IF NOT EXISTS file_size ON files(size);
        ").map_err(|error| error.to_string())?;
                transaction.execute_batch(r#"
                  CREATE TABLE IF NOT EXISTS snapshot_changes(id INTEGER PRIMARY KEY);
                  CREATE TABLE IF NOT EXISTS cache_changes(id INTEGER PRIMARY KEY);
                  CREATE TRIGGER IF NOT EXISTS snapshot_file_insert AFTER INSERT ON files BEGIN INSERT INTO snapshot_changes VALUES(new.id) ON CONFLICT(id) DO NOTHING; INSERT INTO cache_changes SELECT new.id WHERE EXISTS(SELECT 1 FROM settings WHERE key='cache_base_revision') AND NOT EXISTS(SELECT 1 FROM settings WHERE key='cache_journal_overflow' AND value='true') ON CONFLICT(id) DO NOTHING; END;
                  CREATE TRIGGER IF NOT EXISTS snapshot_file_delete AFTER DELETE ON files BEGIN INSERT INTO snapshot_changes VALUES(old.id) ON CONFLICT(id) DO NOTHING; INSERT INTO cache_changes SELECT old.id WHERE EXISTS(SELECT 1 FROM settings WHERE key='cache_base_revision') AND NOT EXISTS(SELECT 1 FROM settings WHERE key='cache_journal_overflow' AND value='true') ON CONFLICT(id) DO NOTHING; END;
                  CREATE TRIGGER IF NOT EXISTS snapshot_file_update AFTER UPDATE ON files WHEN old.name!=new.name OR old.path!=new.path OR old.extension!=new.extension OR old.size!=new.size OR old.modified!=new.modified OR old.created!=new.created OR old.changed!=new.changed OR old.is_dir!=new.is_dir OR old.is_symlink!=new.is_symlink OR old.file_id!=new.file_id OR old.parent_id!=new.parent_id OR old.volume_id!=new.volume_id OR old.flags!=new.flags OR old.modified_ns!=new.modified_ns OR old.changed_ns!=new.changed_ns OR old.accessible!=new.accessible BEGIN INSERT INTO snapshot_changes VALUES(new.id) ON CONFLICT(id) DO NOTHING; INSERT INTO cache_changes SELECT new.id WHERE EXISTS(SELECT 1 FROM settings WHERE key='cache_base_revision') AND NOT EXISTS(SELECT 1 FROM settings WHERE key='cache_journal_overflow' AND value='true') ON CONFLICT(id) DO NOTHING; END;
                  CREATE TRIGGER IF NOT EXISTS snapshot_content_insert AFTER INSERT ON content BEGIN INSERT INTO snapshot_changes SELECT id FROM files WHERE path=new.path ON CONFLICT(id) DO NOTHING; INSERT INTO cache_changes SELECT id FROM files WHERE path=new.path AND EXISTS(SELECT 1 FROM settings WHERE key='cache_base_revision') AND NOT EXISTS(SELECT 1 FROM settings WHERE key='cache_journal_overflow' AND value='true') ON CONFLICT(id) DO NOTHING; INSERT INTO settings VALUES('content_revision','1') ON CONFLICT(key) DO UPDATE SET value=CAST(value AS INTEGER)+1; END;
                  CREATE TRIGGER IF NOT EXISTS snapshot_content_update AFTER UPDATE ON content BEGIN INSERT INTO snapshot_changes SELECT id FROM files WHERE path=new.path ON CONFLICT(id) DO NOTHING; INSERT INTO cache_changes SELECT id FROM files WHERE path=new.path AND EXISTS(SELECT 1 FROM settings WHERE key='cache_base_revision') AND NOT EXISTS(SELECT 1 FROM settings WHERE key='cache_journal_overflow' AND value='true') ON CONFLICT(id) DO NOTHING; INSERT INTO settings VALUES('content_revision','1') ON CONFLICT(key) DO UPDATE SET value=CAST(value AS INTEGER)+1; END;
                  CREATE TRIGGER IF NOT EXISTS snapshot_content_delete AFTER DELETE ON content BEGIN INSERT INTO snapshot_changes SELECT id FROM files WHERE path=old.path ON CONFLICT(id) DO NOTHING; INSERT INTO cache_changes SELECT id FROM files WHERE path=old.path AND EXISTS(SELECT 1 FROM settings WHERE key='cache_base_revision') AND NOT EXISTS(SELECT 1 FROM settings WHERE key='cache_journal_overflow' AND value='true') ON CONFLICT(id) DO NOTHING; INSERT INTO settings VALUES('content_revision','1') ON CONFLICT(key) DO UPDATE SET value=CAST(value AS INTEGER)+1; END;
                  PRAGMA application_id=1095779923;
                  PRAGMA user_version=1;"#).map_err(|error| error.to_string())?;
                cache_journal::install(&transaction)?;
            }
            transaction.commit().map_err(|error| error.to_string())?;
        }
        Ok(Self {
            connection,
            cache_path: path.with_extension("snapshot.bin"),
        })
    }
    /// Only a blank database or this application's complete format is accepted.
    /// Prior development schemas are never altered or adopted in place.
    fn schema_version(connection: &Connection) -> Result<i64, String> {
        let version = connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .map_err(|error| error.to_string())?;
        let application = connection
            .query_row("PRAGMA application_id", [], |row| row.get::<_, i64>(0))
            .map_err(|error| error.to_string())?;
        if version == 1 && application == 0x4150_4653 {
            return Ok(1);
        }
        if version == 0 && application == 0 {
            let populated = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%')",
                    [],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(|error| error.to_string())?;
            if !populated {
                return Ok(0);
            }
        }
        Err(format!(
            "Unsupported index format (application {application}, schema {version})"
        ))
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
    pub(crate) fn set_preferences(
        &self,
        values: &serde_json::Map<String, Value>,
    ) -> Result<(), String> {
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(|error| error.to_string())?;
        for (key, value) in values {
            self.set(key, value)?;
        }
        transaction.commit().map_err(|error| error.to_string())
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
    /// Discover the transitive alias closure without opening a write transaction.
    /// The caller performs filesystem reads after releasing the store mutex, then
    /// validates the revision before replaying the prepared observations at commit.
    pub(crate) fn verification_paths(
        &self,
        entries: &[ScannedFile],
        tracker: &VerifiedFileObjects,
    ) -> Result<Vec<String>, String> {
        let mut current = self.connection.prepare_cached("SELECT id,name,extension,size,modified,created,changed,is_dir,is_symlink,file_id,parent_id,volume_id,flags,modified_ns,changed_ns,accessible FROM files WHERE path=?1").map_err(|error| error.to_string())?;
        let mut peers = self
            .connection
            .prepare_cached(
                "SELECT path FROM files WHERE volume_id=?1 AND file_id=?2 AND accessible=1",
            )
            .map_err(|error| error.to_string())?;
        let mut objects = HashMap::<(String, u64), bool>::new();
        for file in entries {
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
                    let old_object = (metadata_changed && !old_is_dir && old_file_id != 0)
                        .then(|| (old_volume.to_owned(), old_file_id as u64));
                    Ok((id, metadata_changed, content_changed, old_object))
                })
                .optional()
                .map_err(|error| error.to_string())?;

            let changed = existing.as_ref().is_none_or(|row| row.1);
            if let Some(old) = existing.and_then(|row| row.3) {
                objects.insert(old, true);
            }
            if !file.is_dir && file.file_id != 0 && (changed || file.link_count != Some(1)) {
                objects
                    .entry((file.volume_id.clone(), file.file_id))
                    .and_modify(|force| *force |= changed)
                    .or_insert(changed);
            }
        }
        let mut paths = std::collections::BTreeSet::new();
        for ((volume, id), force) in objects {
            if !force && tracker.contains(&volume, id) {
                continue;
            }
            for row in peers
                .query_map(params![volume, id as i64], |row| row.get::<_, String>(0))
                .map_err(|error| error.to_string())?
            {
                paths.insert(row.map_err(|error| error.to_string())?);
            }
            for file in entries {
                if file.volume_id == volume && file.file_id == id {
                    paths.insert(file.path.clone());
                }
            }
        }
        Ok(paths.into_iter().collect())
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
                        if file.link_count.is_some_and(|count| count > 1) {
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
                    if !force
                        && let Some(verified_objects) = verified_objects.as_deref_mut()
                        && verified_objects.contains(&volume_id, file_id)
                    {
                        verified_objects.reused += 1;
                        continue;
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
        if verification_complete && let Some(verified_objects) = verified_objects {
            for (volume, file_id) in visited_objects.intersection(&cacheable_objects) {
                verified_objects.confirm(volume, *file_id);
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
    // Import rows stay in a disk-backed temporary table until the complete,
    // bounded input has been accepted. Queries and cache rebuilds only see files.
    pub(crate) fn begin_file_list_import(&mut self) -> Result<(), String> {
        self.connection.execute_batch("PRAGMA temp_store=FILE;
            DROP TABLE IF EXISTS temp.file_list_import;
            CREATE TEMP TABLE file_list_import(path TEXT PRIMARY KEY,name TEXT NOT NULL,extension TEXT NOT NULL,size INTEGER NOT NULL,modified INTEGER NOT NULL,created INTEGER NOT NULL,is_dir INTEGER NOT NULL,file_id INTEGER NOT NULL);")
            .map_err(|error| error.to_string())
    }
    pub(crate) fn append_file_list_import(
        &mut self,
        entries: &[ScannedFile],
    ) -> Result<(), String> {
        let transaction = self
            .connection
            .transaction()
            .map_err(|error| error.to_string())?;
        {
            let mut insert = transaction.prepare_cached("INSERT INTO temp.file_list_import VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(path) DO UPDATE SET name=excluded.name,extension=excluded.extension,size=excluded.size,modified=excluded.modified,created=excluded.created,is_dir=excluded.is_dir,file_id=excluded.file_id")
                .map_err(|error| error.to_string())?;
            for file in entries {
                insert
                    .execute(params![
                        file.path,
                        file.name,
                        file.extension,
                        file.size as i64,
                        file.modified,
                        file.created,
                        file.is_dir,
                        file.file_id as i64
                    ])
                    .map_err(|error| error.to_string())?;
            }
        }
        transaction.commit().map_err(|error| error.to_string())
    }
    pub(crate) fn finish_file_list_import(&mut self) -> Result<(), String> {
        let transaction = self
            .connection
            .transaction()
            .map_err(|error| error.to_string())?;
        transaction
            .execute("DELETE FROM files", [])
            .map_err(|error| error.to_string())?;
        transaction.execute("INSERT INTO files(path,name,extension,size,modified,created,changed,is_dir,is_symlink,file_id,parent_id,volume_id,flags,seen) SELECT path,name,extension,size,modified,created,0,is_dir,0,file_id,0,'offline:filelist',0,1 FROM temp.file_list_import", []).map_err(|error| error.to_string())?;
        transaction.execute_batch("INSERT INTO settings VALUES('offline','true') ON CONFLICT(key) DO UPDATE SET value='true'; INSERT INTO settings VALUES('watch_enabled','false') ON CONFLICT(key) DO UPDATE SET value='false'; DROP TABLE temp.file_list_import;").map_err(|error| error.to_string())?;
        mark_search_changed(&transaction)?;
        transaction.commit().map_err(|error| error.to_string())
    }
    pub(crate) fn abort_file_list_import(&mut self) -> Result<(), String> {
        self.connection
            .execute_batch("DROP TABLE IF EXISTS temp.file_list_import;")
            .map_err(|error| error.to_string())
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
    pub(crate) fn snapshot(&self, generation: u64) -> Result<SearchSnapshot, String> {
        let sql = format!(
            "SELECT f.id,{} FROM files f LEFT JOIN content c ON f.path=c.path WHERE f.accessible=1 ORDER BY f.id",
            change_reader::COLUMNS
        );
        let mut statement = self
            .connection
            .prepare(&sql)
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], change_reader::decode_file)
            .map_err(|error| error.to_string())?;
        SearchSnapshot::from_rows(
            rows.map(|row| row.map_err(|error| error.to_string())),
            generation,
        )
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
