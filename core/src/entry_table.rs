//! Immutable, stable-slot column blocks. Row views borrow storage; only the
//! I/O boundary materializes an owned IndexedFile.
use crate::integer_column::IntegerColumn;
use crate::text_view::TextView;
use crate::{index_store::IndexedFile, query::Field};
use serde::{Serialize, Serializer, ser::SerializeStruct};
use serde_json::Value;
use std::{
    collections::HashMap,
    ops::Range,
    sync::{Arc, OnceLock},
};
pub const CHUNK_LENGTH: usize = 4096;

pub trait FileEntry {
    fn id(&self) -> i64;
    fn size(&self) -> u64;
    fn modified(&self) -> i64;
    fn created(&self) -> i64;
    fn changed(&self) -> i64;
    fn modified_ns(&self) -> i64;
    fn changed_ns(&self) -> i64;
    fn file_id(&self) -> u64;
    fn parent_id(&self) -> u64;
    fn flags(&self) -> u32;
    fn path(&self) -> TextView<'_>;
    fn name(&self) -> &str;
    fn folded_name(&self) -> &str;
    fn folded_path(&self) -> TextView<'_>;
    fn search_name(&self) -> &str;
    fn search_path(&self) -> TextView<'_>;
    fn parent(&self) -> &str;
    fn extension(&self) -> &str;
    fn folded_extension(&self) -> &str;
    fn volume_id(&self) -> &str;
    fn is_dir(&self) -> bool;
    fn is_symlink(&self) -> bool;
    fn content_indexed(&self) -> bool;
    fn properties(&self) -> &Value;
    fn text(&self, field: Field) -> TextView<'_> {
        match field {
            Field::Name => self.name().into(),
            Field::Path => self.path(),
            Field::Parent => self.parent().into(),
        }
    }
    fn to_owned_file(&self) -> IndexedFile {
        IndexedFile {
            id: self.id(),
            size: self.size(),
            modified: self.modified(),
            created: self.created(),
            changed: self.changed(),
            modified_ns: self.modified_ns(),
            changed_ns: self.changed_ns(),
            file_id: self.file_id(),
            parent_id: self.parent_id(),
            flags: self.flags(),
            path: self.path().into(),
            name: self.name().into(),
            folded_name: self.folded_name().into(),
            folded_path: self.folded_path().into(),
            search_name: self.search_name().into(),
            search_path: self.search_path().into(),
            parent: self.parent().into(),
            extension: self.extension().into(),
            folded_extension: self.folded_extension().into(),
            volume_id: self.volume_id().into(),
            is_dir: self.is_dir(),
            is_symlink: self.is_symlink(),
            content_indexed: self.content_indexed(),
            properties: self.properties().clone(),
        }
    }
}
impl FileEntry for IndexedFile {
    #[inline]
    fn id(&self) -> i64 {
        self.id
    }
    #[inline]
    fn size(&self) -> u64 {
        self.size
    }
    #[inline]
    fn modified(&self) -> i64 {
        self.modified
    }
    #[inline]
    fn created(&self) -> i64 {
        self.created
    }
    #[inline]
    fn changed(&self) -> i64 {
        self.changed
    }
    #[inline]
    fn modified_ns(&self) -> i64 {
        self.modified_ns
    }
    #[inline]
    fn changed_ns(&self) -> i64 {
        self.changed_ns
    }
    #[inline]
    fn file_id(&self) -> u64 {
        self.file_id
    }
    #[inline]
    fn parent_id(&self) -> u64 {
        self.parent_id
    }
    #[inline]
    fn flags(&self) -> u32 {
        self.flags
    }
    #[inline]
    fn path(&self) -> TextView<'_> {
        self.path.as_str().into()
    }
    #[inline]
    fn name(&self) -> &str {
        &self.name
    }
    #[inline]
    fn folded_name(&self) -> &str {
        &self.folded_name
    }
    #[inline]
    fn folded_path(&self) -> TextView<'_> {
        self.folded_path.as_ref().into()
    }
    #[inline]
    fn search_name(&self) -> &str {
        &self.search_name
    }
    #[inline]
    fn search_path(&self) -> TextView<'_> {
        self.search_path.as_ref().into()
    }
    #[inline]
    fn parent(&self) -> &str {
        &self.parent
    }
    #[inline]
    fn extension(&self) -> &str {
        &self.extension
    }
    #[inline]
    fn folded_extension(&self) -> &str {
        &self.folded_extension
    }
    #[inline]
    fn volume_id(&self) -> &str {
        &self.volume_id
    }
    #[inline]
    fn is_dir(&self) -> bool {
        self.is_dir
    }
    #[inline]
    fn is_symlink(&self) -> bool {
        self.is_symlink
    }
    #[inline]
    fn content_indexed(&self) -> bool {
        self.content_indexed
    }
    fn properties(&self) -> &Value {
        &self.properties
    }
}
impl<T: FileEntry + ?Sized> FileEntry for Arc<T> {
    #[inline]
    fn id(&self) -> i64 {
        (**self).id()
    }
    #[inline]
    fn size(&self) -> u64 {
        (**self).size()
    }
    #[inline]
    fn modified(&self) -> i64 {
        (**self).modified()
    }
    #[inline]
    fn created(&self) -> i64 {
        (**self).created()
    }
    #[inline]
    fn changed(&self) -> i64 {
        (**self).changed()
    }
    #[inline]
    fn modified_ns(&self) -> i64 {
        (**self).modified_ns()
    }
    #[inline]
    fn changed_ns(&self) -> i64 {
        (**self).changed_ns()
    }
    #[inline]
    fn file_id(&self) -> u64 {
        (**self).file_id()
    }
    #[inline]
    fn parent_id(&self) -> u64 {
        (**self).parent_id()
    }
    #[inline]
    fn flags(&self) -> u32 {
        (**self).flags()
    }
    #[inline]
    fn path(&self) -> TextView<'_> {
        (**self).path()
    }
    #[inline]
    fn name(&self) -> &str {
        (**self).name()
    }
    #[inline]
    fn folded_name(&self) -> &str {
        (**self).folded_name()
    }
    #[inline]
    fn folded_path(&self) -> TextView<'_> {
        (**self).folded_path()
    }
    #[inline]
    fn search_name(&self) -> &str {
        (**self).search_name()
    }
    #[inline]
    fn search_path(&self) -> TextView<'_> {
        (**self).search_path()
    }
    #[inline]
    fn parent(&self) -> &str {
        (**self).parent()
    }
    #[inline]
    fn extension(&self) -> &str {
        (**self).extension()
    }
    #[inline]
    fn folded_extension(&self) -> &str {
        (**self).folded_extension()
    }
    #[inline]
    fn volume_id(&self) -> &str {
        (**self).volume_id()
    }
    #[inline]
    fn is_dir(&self) -> bool {
        (**self).is_dir()
    }
    #[inline]
    fn is_symlink(&self) -> bool {
        (**self).is_symlink()
    }
    #[inline]
    fn content_indexed(&self) -> bool {
        (**self).content_indexed()
    }
    fn properties(&self) -> &Value {
        (**self).properties()
    }
}
impl<T: FileEntry + ?Sized> FileEntry for &T {
    #[inline]
    fn id(&self) -> i64 {
        (**self).id()
    }
    #[inline]
    fn size(&self) -> u64 {
        (**self).size()
    }
    #[inline]
    fn modified(&self) -> i64 {
        (**self).modified()
    }
    #[inline]
    fn created(&self) -> i64 {
        (**self).created()
    }
    #[inline]
    fn changed(&self) -> i64 {
        (**self).changed()
    }
    #[inline]
    fn modified_ns(&self) -> i64 {
        (**self).modified_ns()
    }
    #[inline]
    fn changed_ns(&self) -> i64 {
        (**self).changed_ns()
    }
    #[inline]
    fn file_id(&self) -> u64 {
        (**self).file_id()
    }
    #[inline]
    fn parent_id(&self) -> u64 {
        (**self).parent_id()
    }
    #[inline]
    fn flags(&self) -> u32 {
        (**self).flags()
    }
    #[inline]
    fn path(&self) -> TextView<'_> {
        (**self).path()
    }
    #[inline]
    fn name(&self) -> &str {
        (**self).name()
    }
    #[inline]
    fn folded_name(&self) -> &str {
        (**self).folded_name()
    }
    #[inline]
    fn folded_path(&self) -> TextView<'_> {
        (**self).folded_path()
    }
    #[inline]
    fn search_name(&self) -> &str {
        (**self).search_name()
    }
    #[inline]
    fn search_path(&self) -> TextView<'_> {
        (**self).search_path()
    }
    #[inline]
    fn parent(&self) -> &str {
        (**self).parent()
    }
    #[inline]
    fn extension(&self) -> &str {
        (**self).extension()
    }
    #[inline]
    fn folded_extension(&self) -> &str {
        (**self).folded_extension()
    }
    #[inline]
    fn volume_id(&self) -> &str {
        (**self).volume_id()
    }
    #[inline]
    fn is_dir(&self) -> bool {
        (**self).is_dir()
    }
    #[inline]
    fn is_symlink(&self) -> bool {
        (**self).is_symlink()
    }
    #[inline]
    fn content_indexed(&self) -> bool {
        (**self).content_indexed()
    }
    fn properties(&self) -> &Value {
        (**self).properties()
    }
}

// Only primitive integers and TextRef are accepted by mapped columns. All bit
// patterns are valid, and alignment/length are checked before a slice is made.
mod sealed {
    pub trait ColumnValue {}
    impl ColumnValue for u8 {}
    impl ColumnValue for u16 {}
    impl ColumnValue for u32 {}
    impl ColumnValue for u64 {}
    impl ColumnValue for i64 {}
    impl ColumnValue for super::TextRef {}
    impl ColumnValue for super::PathRef {}
}
pub(crate) trait ColumnValue:
    sealed::ColumnValue + Copy + Clone + PartialEq + Send + Sync + 'static
{
}
impl ColumnValue for u8 {}
impl ColumnValue for u16 {}
impl ColumnValue for u32 {}
impl ColumnValue for u64 {}
impl ColumnValue for i64 {}
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, Hash)]
#[repr(C)]
pub(crate) struct TextRef {
    pub offset: u32,
    pub length: u32,
}
impl ColumnValue for TextRef {}
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
#[repr(C)]
pub(crate) struct PathRef {
    pub prefix: TextRef,
    pub suffix: TextRef,
}
impl ColumnValue for PathRef {}
#[derive(Clone)]
enum ColumnData<T> {
    Owned(Vec<T>),
    Mapped {
        mapping: Arc<memmap2::Mmap>,
        range: Range<usize>,
    },
}
#[derive(Clone)]
pub(crate) struct Column<T>(Arc<ColumnData<T>>);
impl<T: ColumnValue> Default for Column<T> {
    fn default() -> Self {
        Self(Arc::new(ColumnData::Owned(Vec::new())))
    }
}
impl<T: ColumnValue> Column<T> {
    pub(crate) fn owned(values: Vec<T>) -> Self {
        Self(Arc::new(ColumnData::Owned(values)))
    }
    pub(crate) fn is_mapped(&self) -> bool {
        matches!(self.0.as_ref(), ColumnData::Mapped { .. })
    }
    pub(crate) fn account(
        &self,
        owned: &mut usize,
        mappings: &mut HashMap<usize, usize>,
        allocations: &mut std::collections::HashSet<usize>,
    ) {
        if !allocations.insert(Arc::as_ptr(&self.0) as usize) {
            return;
        }
        match self.0.as_ref() {
            ColumnData::Owned(values) => *owned += values.capacity() * std::mem::size_of::<T>(),
            ColumnData::Mapped { mapping, .. } => {
                mappings.insert(Arc::as_ptr(mapping) as usize, mapping.len());
            }
        }
    }

    #[inline]
    pub fn values(&self) -> &[T] {
        match self.0.as_ref() {
            ColumnData::Owned(values) => values,
            ColumnData::Mapped { mapping, range } => {
                // SAFETY: mapped() validates alignment and exact element length;
                // the immutable mapping is owned by this column and T has no invalid bits.
                unsafe {
                    std::slice::from_raw_parts(
                        mapping.as_ptr().add(range.start).cast::<T>(),
                        range.len() / std::mem::size_of::<T>(),
                    )
                }
            }
        }
    }
    pub(crate) fn mapped(mapping: Arc<memmap2::Mmap>, range: Range<usize>) -> Option<Self> {
        if range.start > range.end
            || range.end > mapping.len()
            || !range.start.is_multiple_of(std::mem::align_of::<T>())
            || !range.len().is_multiple_of(std::mem::size_of::<T>())
        {
            return None;
        }
        Some(Self(Arc::new(ColumnData::Mapped { mapping, range })))
    }
    pub(crate) fn set(&mut self, slot: usize, value: T) {
        if self.values().get(slot) == Some(&value) {
            return;
        }
        let data = Arc::make_mut(&mut self.0);
        if let ColumnData::Mapped { mapping, range } = data {
            // SAFETY: the same validated immutable range as values().
            let values = unsafe {
                std::slice::from_raw_parts(
                    mapping.as_ptr().add(range.start).cast::<T>(),
                    range.len() / std::mem::size_of::<T>(),
                )
            }
            .to_vec();
            *data = ColumnData::Owned(values);
        }
        let ColumnData::Owned(values) = data else {
            unreachable!()
        };
        if slot == values.len() {
            values.push(value);
        } else {
            values[slot] = value;
        }
    }
    pub(crate) fn bytes(&self) -> &[u8] {
        let values = self.values();
        // SAFETY: ColumnValue types have initialized bytes and no padding.
        unsafe {
            std::slice::from_raw_parts(values.as_ptr().cast::<u8>(), std::mem::size_of_val(values))
        }
    }
}
#[derive(Clone)]
pub(crate) enum TextData {
    Owned(String),
    Mapped {
        mapping: Arc<memmap2::Mmap>,
        range: Range<usize>,
    },
}
impl TextData {
    #[inline]
    pub fn text(&self) -> &str {
        match self {
            Self::Owned(s) => s,
            Self::Mapped { mapping, range } => {
                // SAFETY: cache construction validates UTF-8 for the entire text region.
                unsafe { std::str::from_utf8_unchecked(&mapping[range.clone()]) }
            }
        }
    }
    fn append(&mut self, text: &str) -> TextRef {
        if matches!(self, Self::Mapped { .. }) {
            *self = Self::Owned(self.text().to_owned());
        }
        let Self::Owned(pool) = self else {
            unreachable!()
        };
        let reference = TextRef {
            offset: u32::try_from(pool.len()).expect("Chunk text exceeds 4 GiB"),
            length: u32::try_from(text.len()).expect("Text exceeds 4 GiB"),
        };
        assert!(
            pool.len()
                .checked_add(text.len())
                .is_some_and(|end| end <= u32::MAX as usize)
        );
        pool.push_str(text);
        reference
    }
}
#[derive(Clone)]
pub(crate) struct EntryChunk {
    pub(crate) section: OnceLock<Arc<crate::snapshot_cache::Section>>,
    pub(crate) id: IntegerColumn<i64>,
    pub(crate) size: IntegerColumn<u64>,
    pub(crate) modified: IntegerColumn<i64>,
    pub(crate) created: IntegerColumn<i64>,
    pub(crate) changed: IntegerColumn<i64>,
    pub(crate) modified_ns: IntegerColumn<i64>,
    pub(crate) changed_ns: IntegerColumn<i64>,
    pub(crate) file_id: IntegerColumn<u64>,
    pub(crate) parent_id: IntegerColumn<u64>,
    pub(crate) flags: IntegerColumn<u32>,
    pub(crate) path: Column<PathRef>,
    pub(crate) name: Column<TextRef>,
    pub(crate) folded_name: Column<TextRef>,
    pub(crate) folded_path: Column<PathRef>,
    pub(crate) search_name: Column<TextRef>,
    pub(crate) search_path: Column<PathRef>,
    pub(crate) parent: Column<TextRef>,
    pub(crate) extension: IntegerColumn<u32>,
    pub(crate) folded_extension: IntegerColumn<u32>,
    pub(crate) volume_id: IntegerColumn<u32>,
    pub(crate) states: Column<u8>,
    pub(crate) text: Arc<TextData>,
    pub(crate) labels: Arc<Vec<String>>,
    pub(crate) properties: Arc<HashMap<usize, Value>>,
    // Exists only while a changed block is being assembled; never retained in a published snapshot.
    pub(crate) pending_text: Option<HashMap<String, TextRef>>,
}
impl Default for EntryChunk {
    fn default() -> Self {
        Self {
            section: OnceLock::new(),
            id: IntegerColumn::default(),
            size: IntegerColumn::default(),
            modified: IntegerColumn::default(),
            created: IntegerColumn::default(),
            changed: IntegerColumn::default(),
            modified_ns: IntegerColumn::default(),
            changed_ns: IntegerColumn::default(),
            file_id: IntegerColumn::default(),
            parent_id: IntegerColumn::default(),
            flags: IntegerColumn::default(),
            path: Column::default(),
            name: Column::default(),
            folded_name: Column::default(),
            folded_path: Column::default(),
            search_name: Column::default(),
            search_path: Column::default(),
            parent: Column::default(),
            extension: IntegerColumn::default(),
            folded_extension: IntegerColumn::default(),
            volume_id: IntegerColumn::default(),
            states: Column::default(),
            text: Arc::new(TextData::Owned(String::new())),
            labels: Arc::new(Vec::new()),
            properties: Arc::new(HashMap::new()),
            pending_text: None,
        }
    }
}
impl EntryChunk {
    pub(crate) fn len(&self) -> usize {
        self.id.len()
    }
    #[inline]
    pub(crate) fn text_at(&self, reference: TextRef) -> &str {
        &self.text.text()
            [reference.offset as usize..reference.offset as usize + reference.length as usize]
    }
    pub(crate) fn path_at(&self, reference: PathRef) -> TextView<'_> {
        TextView::new(
            self.text_at(reference.prefix),
            self.text_at(reference.suffix),
        )
    }
    fn label(&mut self, text: &str) -> u32 {
        if let Some(index) = self.labels.iter().position(|value| value == text) {
            return index as u32;
        }
        let labels = Arc::make_mut(&mut self.labels);
        let index = labels.len();
        labels.push(text.into());
        index as u32
    }
    fn from_rows(rows: &[IndexedFile]) -> Self {
        let mut chunk = Self::default();
        let mut interned = HashMap::new();
        let mut pool = String::new();
        let intern = |text: &str, pool: &mut String, interned: &mut HashMap<String, TextRef>| {
            if let Some(reference) = interned.get(text) {
                return *reference;
            }
            let reference = TextRef {
                offset: u32::try_from(pool.len()).expect("Chunk text fits u32"),
                length: u32::try_from(text.len()).expect("Text fits u32"),
            };
            pool.push_str(text);
            interned.insert(text.into(), reference);
            reference
        };
        // The hottest names occupy the first contiguous region of each block.
        for row in rows {
            intern(row.search_name(), &mut pool, &mut interned);
        }
        for (slot, row) in rows.iter().enumerate() {
            chunk.id.set(slot, row.id());
            chunk.size.set(slot, row.size());
            chunk.modified.set(slot, row.modified());
            chunk.created.set(slot, row.created());
            chunk.changed.set(slot, row.changed());
            chunk.modified_ns.set(slot, row.modified_ns());
            chunk.changed_ns.set(slot, row.changed_ns());
            chunk.file_id.set(slot, row.file_id());
            chunk.parent_id.set(slot, row.parent_id());
            chunk.flags.set(slot, row.flags());
            let mut intern_path = |value: &str| {
                let split = value.rfind('/').map_or(0, |index| index + 1);
                PathRef {
                    prefix: intern(&value[..split], &mut pool, &mut interned),
                    suffix: intern(&value[split..], &mut pool, &mut interned),
                }
            };
            chunk.path.set(slot, intern_path(&row.path));
            chunk.folded_path.set(slot, intern_path(&row.folded_path));
            chunk.search_path.set(slot, intern_path(&row.search_path));
            chunk
                .name
                .set(slot, intern(row.name(), &mut pool, &mut interned));
            chunk
                .folded_name
                .set(slot, intern(row.folded_name(), &mut pool, &mut interned));
            chunk
                .search_name
                .set(slot, intern(row.search_name(), &mut pool, &mut interned));
            chunk
                .parent
                .set(slot, intern(row.parent(), &mut pool, &mut interned));
            let label = chunk.label(row.extension());
            chunk.extension.set(slot, label);
            let label = chunk.label(row.folded_extension());
            chunk.folded_extension.set(slot, label);
            let label = chunk.label(row.volume_id());
            chunk.volume_id.set(slot, label);
            chunk.set_properties(slot, row);
        }
        // Immutable blocks do not need growth headroom. Reclaim construction
        // capacity now instead of carrying it through every shared snapshot.
        pool.shrink_to_fit();
        chunk.text = Arc::new(TextData::Owned(pool));
        chunk.compact_integers();
        chunk
    }
    fn compact_integers(&mut self) {
        self.pending_text = None;
        if self.folded_name.values() == self.name.values() {
            self.folded_name = self.name.clone();
        }
        if self.search_name.values() == self.folded_name.values() {
            self.search_name = self.folded_name.clone();
        }
        if self.folded_path.values() == self.path.values() {
            self.folded_path = self.path.clone();
        }
        if self.search_path.values() == self.folded_path.values() {
            self.search_path = self.folded_path.clone();
        }
        self.id.compact();
        self.size.compact();
        self.modified.compact();
        self.created.compact();
        self.changed.compact();
        self.modified_ns.compact();
        self.changed_ns.compact();
        self.file_id.compact();
        self.parent_id.compact();
        self.flags.compact();
        self.extension.compact();
        self.folded_extension.compact();
        self.volume_id.compact();
    }
    fn set_properties(&mut self, slot: usize, row: &impl FileEntry) {
        let empty = row
            .properties()
            .as_object()
            .is_some_and(|map| map.is_empty());
        let null = row.properties().is_null();
        let bits = u8::from(row.is_dir())
            | (u8::from(row.is_symlink()) << 1)
            | (u8::from(row.content_indexed()) << 2)
            | (u8::from(null) << 3)
            | (u8::from(!empty && !null) << 4);
        self.states.set(slot, bits);
        if !empty && !null {
            if self.properties.get(&slot) != Some(row.properties()) {
                Arc::make_mut(&mut self.properties).insert(slot, row.properties().clone());
            }
        } else if self.properties.contains_key(&slot) {
            Arc::make_mut(&mut self.properties).remove(&slot);
        }
    }
    fn intern_update(&mut self, value: &str) -> TextRef {
        if self.pending_text.is_none() {
            let mut dictionary = HashMap::new();
            for reference in [
                &self.name,
                &self.folded_name,
                &self.search_name,
                &self.parent,
            ]
            .into_iter()
            .flat_map(|column| column.values().iter().copied())
            .chain(
                [&self.path, &self.folded_path, &self.search_path]
                    .into_iter()
                    .flat_map(|column| {
                        column
                            .values()
                            .iter()
                            .flat_map(|path| [path.prefix, path.suffix])
                    }),
            ) {
                dictionary
                    .entry(self.text_at(reference).to_owned())
                    .or_insert(reference);
            }
            self.pending_text = Some(dictionary);
        }
        let dictionary = self.pending_text.as_mut().unwrap();
        if let Some(reference) = dictionary.get(value) {
            return *reference;
        }
        let reference = Arc::make_mut(&mut self.text).append(value);
        dictionary.insert(value.to_owned(), reference);
        reference
    }
    fn set(&mut self, slot: usize, row: &impl FileEntry) {
        // Arc::make_mut cloned the old descriptor with the block. A mutation
        // must never publish that old section for the replacement values.
        self.section.take();
        self.id.set(slot, row.id());
        self.size.set(slot, row.size());
        self.modified.set(slot, row.modified());
        self.created.set(slot, row.created());
        self.changed.set(slot, row.changed());
        self.modified_ns.set(slot, row.modified_ns());
        self.changed_ns.set(slot, row.changed_ns());
        self.file_id.set(slot, row.file_id());
        self.parent_id.set(slot, row.parent_id());
        self.flags.set(slot, row.flags());
        if self
            .search_name
            .values()
            .get(slot)
            .is_none_or(|reference| self.text_at(*reference) != row.search_name())
        {
            let reference = self.intern_update(row.search_name());
            self.search_name.set(slot, reference);
        }
        if self
            .name
            .values()
            .get(slot)
            .is_none_or(|reference| self.text_at(*reference) != row.name())
        {
            let reference = self.intern_update(row.name());
            self.name.set(slot, reference);
        }
        if self
            .folded_name
            .values()
            .get(slot)
            .is_none_or(|reference| self.text_at(*reference) != row.folded_name())
        {
            let reference = self.intern_update(row.folded_name());
            self.folded_name.set(slot, reference);
        }
        if self
            .parent
            .values()
            .get(slot)
            .is_none_or(|reference| self.text_at(*reference) != row.parent())
        {
            let reference = self.intern_update(row.parent());
            self.parent.set(slot, reference);
        }
        if self
            .path
            .values()
            .get(slot)
            .is_none_or(|reference| self.path_at(*reference) != row.path())
        {
            let reference = row.path().with_str(|value| {
                let split = value.rfind('/').map_or(0, |index| index + 1);
                PathRef {
                    prefix: self.intern_update(&value[..split]),
                    suffix: self.intern_update(&value[split..]),
                }
            });
            self.path.set(slot, reference);
        }
        if self
            .folded_path
            .values()
            .get(slot)
            .is_none_or(|reference| self.path_at(*reference) != row.folded_path())
        {
            let reference = row.folded_path().with_str(|value| {
                let split = value.rfind('/').map_or(0, |index| index + 1);
                PathRef {
                    prefix: self.intern_update(&value[..split]),
                    suffix: self.intern_update(&value[split..]),
                }
            });
            self.folded_path.set(slot, reference);
        }
        if self
            .search_path
            .values()
            .get(slot)
            .is_none_or(|reference| self.path_at(*reference) != row.search_path())
        {
            let reference = row.search_path().with_str(|value| {
                let split = value.rfind('/').map_or(0, |index| index + 1);
                PathRef {
                    prefix: self.intern_update(&value[..split]),
                    suffix: self.intern_update(&value[split..]),
                }
            });
            self.search_path.set(slot, reference);
        }
        let label = self.label(row.extension());
        self.extension.set(slot, label);
        let label = self.label(row.folded_extension());
        self.folded_extension.set(slot, label);
        let label = self.label(row.volume_id());
        self.volume_id.set(slot, label);
        self.set_properties(slot, row);
    }
}
#[derive(Clone, Copy)]
pub struct EntryView<'a> {
    chunk: &'a EntryChunk,
    slot: usize,
}
impl<'a> EntryView<'a> {
    /// Construct the owned output boundary directly. Serializing Display-based
    /// fragments through a Value serializer would grow an intermediate String.
    pub(crate) fn to_json(self) -> Value {
        let mut fields = serde_json::Map::new();
        fields.insert("id".into(), Value::from(self.id()));
        fields.insert("size".into(), Value::from(self.size()));
        fields.insert("modified".into(), Value::from(self.modified()));
        fields.insert("created".into(), Value::from(self.created()));
        fields.insert("changed".into(), Value::from(self.changed()));
        fields.insert("modified_ns".into(), Value::from(self.modified_ns()));
        fields.insert("changed_ns".into(), Value::from(self.changed_ns()));
        fields.insert("file_id".into(), Value::from(self.file_id()));
        fields.insert("parent_id".into(), Value::from(self.parent_id()));
        fields.insert("flags".into(), Value::from(self.flags()));
        fields.insert("path".into(), Value::String(self.path().into()));
        fields.insert("name".into(), Value::String(self.name().into()));
        fields.insert("extension".into(), Value::String(self.extension().into()));
        fields.insert("volume_id".into(), Value::String(self.volume_id().into()));
        fields.insert("is_dir".into(), Value::Bool(self.is_dir()));
        fields.insert("is_symlink".into(), Value::Bool(self.is_symlink()));
        fields.insert(
            "content_indexed".into(),
            Value::Bool(self.content_indexed()),
        );
        fields.insert("properties".into(), self.properties().clone());
        Value::Object(fields)
    }

    #[inline]
    pub fn path(&self) -> TextView<'a> {
        self.chunk.path_at(self.chunk.path.values()[self.slot])
    }
    #[inline]
    pub fn name(&self) -> &'a str {
        self.chunk.text_at(self.chunk.name.values()[self.slot])
    }
    #[inline]
    pub fn folded_name(&self) -> &'a str {
        self.chunk
            .text_at(self.chunk.folded_name.values()[self.slot])
    }
    #[inline]
    pub fn folded_path(&self) -> TextView<'a> {
        self.chunk
            .path_at(self.chunk.folded_path.values()[self.slot])
    }
    #[inline]
    pub fn search_name(&self) -> &'a str {
        self.chunk
            .text_at(self.chunk.search_name.values()[self.slot])
    }
    #[inline]
    pub fn search_path(&self) -> TextView<'a> {
        self.chunk
            .path_at(self.chunk.search_path.values()[self.slot])
    }
    #[inline]
    pub fn parent(&self) -> &'a str {
        self.chunk.text_at(self.chunk.parent.values()[self.slot])
    }
    #[inline]
    pub fn extension(&self) -> &'a str {
        &self.chunk.labels[self.chunk.extension.get(self.slot) as usize]
    }
    #[inline]
    pub fn folded_extension(&self) -> &'a str {
        &self.chunk.labels[self.chunk.folded_extension.get(self.slot) as usize]
    }
    #[inline]
    pub fn volume_id(&self) -> &'a str {
        &self.chunk.labels[self.chunk.volume_id.get(self.slot) as usize]
    }
}
impl FileEntry for EntryView<'_> {
    #[inline]
    fn id(&self) -> i64 {
        self.chunk.id.get(self.slot)
    }
    #[inline]
    fn size(&self) -> u64 {
        self.chunk.size.get(self.slot)
    }
    #[inline]
    fn modified(&self) -> i64 {
        self.chunk.modified.get(self.slot)
    }
    #[inline]
    fn created(&self) -> i64 {
        self.chunk.created.get(self.slot)
    }
    #[inline]
    fn changed(&self) -> i64 {
        self.chunk.changed.get(self.slot)
    }
    #[inline]
    fn modified_ns(&self) -> i64 {
        self.chunk.modified_ns.get(self.slot)
    }
    #[inline]
    fn changed_ns(&self) -> i64 {
        self.chunk.changed_ns.get(self.slot)
    }
    #[inline]
    fn file_id(&self) -> u64 {
        self.chunk.file_id.get(self.slot)
    }
    #[inline]
    fn parent_id(&self) -> u64 {
        self.chunk.parent_id.get(self.slot)
    }
    #[inline]
    fn flags(&self) -> u32 {
        self.chunk.flags.get(self.slot)
    }
    #[inline]
    fn path(&self) -> TextView<'_> {
        self.chunk.path_at(self.chunk.path.values()[self.slot])
    }
    #[inline]
    fn name(&self) -> &str {
        self.chunk.text_at(self.chunk.name.values()[self.slot])
    }
    #[inline]
    fn folded_name(&self) -> &str {
        self.chunk
            .text_at(self.chunk.folded_name.values()[self.slot])
    }
    #[inline]
    fn folded_path(&self) -> TextView<'_> {
        self.chunk
            .path_at(self.chunk.folded_path.values()[self.slot])
    }
    #[inline]
    fn search_name(&self) -> &str {
        self.chunk
            .text_at(self.chunk.search_name.values()[self.slot])
    }
    #[inline]
    fn search_path(&self) -> TextView<'_> {
        self.chunk
            .path_at(self.chunk.search_path.values()[self.slot])
    }
    #[inline]
    fn parent(&self) -> &str {
        self.chunk.text_at(self.chunk.parent.values()[self.slot])
    }
    #[inline]
    fn extension(&self) -> &str {
        &self.chunk.labels[self.chunk.extension.get(self.slot) as usize]
    }
    #[inline]
    fn folded_extension(&self) -> &str {
        &self.chunk.labels[self.chunk.folded_extension.get(self.slot) as usize]
    }
    #[inline]
    fn volume_id(&self) -> &str {
        &self.chunk.labels[self.chunk.volume_id.get(self.slot) as usize]
    }
    #[inline]
    fn is_dir(&self) -> bool {
        self.chunk.states.values()[self.slot] & 1 != 0
    }
    #[inline]
    fn is_symlink(&self) -> bool {
        self.chunk.states.values()[self.slot] & 2 != 0
    }
    #[inline]
    fn content_indexed(&self) -> bool {
        self.chunk.states.values()[self.slot] & 4 != 0
    }
    fn properties(&self) -> &Value {
        let state = self.chunk.states.values()[self.slot];
        if state & 16 != 0 {
            &self.chunk.properties[&self.slot]
        } else if state & 8 != 0 {
            &Value::Null
        } else {
            static EMPTY: OnceLock<Value> = OnceLock::new();
            EMPTY.get_or_init(|| serde_json::json!({}))
        }
    }
}
impl Serialize for EntryView<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("IndexedFile", 18)?;
        state.serialize_field("id", &self.id())?;
        state.serialize_field("size", &self.size())?;
        state.serialize_field("modified", &self.modified())?;
        state.serialize_field("created", &self.created())?;
        state.serialize_field("changed", &self.changed())?;
        state.serialize_field("modified_ns", &self.modified_ns())?;
        state.serialize_field("changed_ns", &self.changed_ns())?;
        state.serialize_field("file_id", &self.file_id())?;
        state.serialize_field("parent_id", &self.parent_id())?;
        state.serialize_field("flags", &self.flags())?;
        state.serialize_field("path", &self.path())?;
        state.serialize_field("name", &self.name())?;
        state.serialize_field("extension", &self.extension())?;
        state.serialize_field("volume_id", &self.volume_id())?;
        state.serialize_field("is_dir", &self.is_dir())?;
        state.serialize_field("is_symlink", &self.is_symlink())?;
        state.serialize_field("content_indexed", &self.content_indexed())?;
        state.serialize_field("properties", &self.properties())?;
        state.end()
    }
}
#[derive(Clone, Default)]
pub struct EntryTable {
    pub(crate) chunks: Vec<Arc<EntryChunk>>,
    length: usize,
    text_changes: roaring::RoaringBitmap,
    changed_blocks: roaring::RoaringBitmap,
}
impl EntryTable {
    pub fn len(&self) -> usize {
        self.length
    }
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }
    #[inline]
    pub fn at(&self, slot: usize) -> EntryView<'_> {
        self.get(slot).expect("Entry slot out of bounds")
    }
    #[inline]
    pub fn get(&self, slot: usize) -> Option<EntryView<'_>> {
        (slot < self.length).then(|| EntryView {
            chunk: &self.chunks[slot / CHUNK_LENGTH],
            slot: slot % CHUNK_LENGTH,
        })
    }
    pub fn last(&self) -> Option<EntryView<'_>> {
        self.length.checked_sub(1).and_then(|slot| self.get(slot))
    }
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = EntryView<'_>> {
        self.chunks
            .iter()
            .flat_map(|chunk| (0..chunk.len()).map(move |slot| EntryView { chunk, slot }))
    }
    pub fn push(&mut self, row: IndexedFile) {
        if self.length.is_multiple_of(CHUNK_LENGTH) {
            self.chunks.push(Arc::new(EntryChunk::default()));
        }
        Arc::make_mut(self.chunks.last_mut().unwrap()).set(self.length % CHUNK_LENGTH, &row);
        self.text_changes
            .insert((self.length / CHUNK_LENGTH) as u32);
        self.changed_blocks
            .insert((self.length / CHUNK_LENGTH) as u32);
        self.length += 1;
    }
    pub fn set(&mut self, slot: usize, row: IndexedFile) {
        assert!(slot < self.length);
        let chunk = Arc::make_mut(&mut self.chunks[slot / CHUNK_LENGTH]);
        let previous_bytes = chunk.text.text().len();
        chunk.set(slot % CHUNK_LENGTH, &row);
        self.changed_blocks.insert((slot / CHUNK_LENGTH) as u32);
        if chunk.text.text().len() != previous_bytes {
            self.text_changes.insert((slot / CHUNK_LENGTH) as u32);
        }
    }
    pub(crate) fn finish_update(&mut self) {
        if self.changed_blocks.is_empty() {
            return;
        }
        let mut cpu = crate::cpu_executor::enter_background();
        for index in &self.text_changes {
            cpu.checkpoint();
            let chunk = &self.chunks[index as usize];
            let mut references: Vec<_> = [
                &chunk.name,
                &chunk.folded_name,
                &chunk.search_name,
                &chunk.parent,
            ]
            .into_iter()
            .flat_map(|column| column.values().iter().copied())
            .chain(
                [&chunk.path, &chunk.folded_path, &chunk.search_path]
                    .into_iter()
                    .flat_map(|column| {
                        column
                            .values()
                            .iter()
                            .flat_map(|path| [path.prefix, path.suffix])
                    }),
            )
            .map(|r| (r.offset as usize, r.offset as usize + r.length as usize))
            .collect();
            references.sort_unstable();
            let (mut live_bytes, mut covered_end) = (0usize, 0usize);
            for (start, end) in references {
                if end > covered_end {
                    live_bytes += end - start.max(covered_end);
                    covered_end = end;
                }
            }
            // Reclaim a text block when at least half of its bytes are dead.
            // This is an ownership bound, not a periodic idle rewrite.
            if chunk.text.text().len().saturating_sub(live_bytes) > live_bytes {
                let rows: Vec<_> = (0..chunk.len())
                    .map(|slot| EntryView { chunk, slot }.to_owned_file())
                    .collect();
                let packed = EntryChunk::from_rows(&rows);
                let chunk = Arc::make_mut(&mut self.chunks[index as usize]);
                chunk.text = packed.text;
                chunk.path = packed.path;
                chunk.name = packed.name;
                chunk.folded_name = packed.folded_name;
                chunk.folded_path = packed.folded_path;
                chunk.search_name = packed.search_name;
                chunk.search_path = packed.search_path;
                chunk.parent = packed.parent;
            }
        }
        self.text_changes.clear();
        for index in &self.changed_blocks {
            cpu.checkpoint();
            Arc::make_mut(&mut self.chunks[index as usize]).compact_integers();
        }
        self.changed_blocks.clear();
    }
    pub fn from_rows(
        rows: impl IntoIterator<Item = Result<IndexedFile, String>>,
    ) -> Result<Self, String> {
        let mut table = Self::default();
        let mut batch = Vec::with_capacity(CHUNK_LENGTH);
        let mut cpu = crate::cpu_executor::enter_background();
        for row in rows {
            if batch.is_empty() {
                cpu.checkpoint();
            }
            if table.length + batch.len() >= u32::MAX as usize {
                return Err("snapshot_slot_overflow".into());
            }
            batch.push(row?);
            if batch.len() == CHUNK_LENGTH {
                table.length += batch.len();
                table.chunks.push(Arc::new(EntryChunk::from_rows(&batch)));
                batch.clear();
            }
        }
        if !batch.is_empty() {
            table.length += batch.len();
            table.chunks.push(Arc::new(EntryChunk::from_rows(&batch)));
        }
        Ok(table)
    }
    pub(crate) fn from_chunks(chunks: Vec<Arc<EntryChunk>>) -> Option<Self> {
        let length = chunks.iter().map(|chunk| chunk.len()).sum::<usize>();
        if length > u32::MAX as usize
            || chunks.iter().enumerate().any(|(i, c)| {
                c.len() == 0
                    || c.len() > CHUNK_LENGTH
                    || (i + 1 < chunks.len() && c.len() != CHUNK_LENGTH)
            })
        {
            return None;
        }
        Some(Self {
            chunks,
            length,
            text_changes: roaring::RoaringBitmap::new(),
            changed_blocks: roaring::RoaringBitmap::new(),
        })
    }
    #[cfg(test)]
    pub fn text_block_identity(&self, slot: usize) -> usize {
        Arc::as_ptr(&self.chunks[slot / CHUNK_LENGTH].text) as usize
    }
    #[cfg(test)]
    pub fn shares_block(&self, other: &Self, slot: usize) -> bool {
        Arc::ptr_eq(
            &self.chunks[slot / CHUNK_LENGTH],
            &other.chunks[slot / CHUNK_LENGTH],
        )
    }
}
impl From<Vec<Arc<IndexedFile>>> for EntryTable {
    fn from(rows: Vec<Arc<IndexedFile>>) -> Self {
        Self::from_rows(rows.into_iter().map(|row| Ok(Arc::unwrap_or_clone(row)))).unwrap()
    }
}
impl FromIterator<IndexedFile> for EntryTable {
    fn from_iter<I: IntoIterator<Item = IndexedFile>>(rows: I) -> Self {
        Self::from_rows(rows.into_iter().map(Ok)).unwrap()
    }
}
impl Serialize for EntryTable {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.iter())
    }
}

#[cfg(test)]
#[path = "entry_table_tests.rs"]
mod tests;

impl EntryTable {
    /// Payload inventory, not physical footprint or allocator accounting.
    pub fn storage_metrics(&self) -> serde_json::Value {
        let mut owned_columns = 0usize;
        let mut owned_text = 0usize;
        let mut labels = 0usize;
        let mut property_rows = 0usize;
        let mut mappings = std::collections::HashMap::new();
        let mut allocations = std::collections::HashSet::new();
        for chunk in &self.chunks {
            chunk
                .id
                .account(&mut owned_columns, &mut mappings, &mut allocations);
            chunk
                .size
                .account(&mut owned_columns, &mut mappings, &mut allocations);
            chunk
                .modified
                .account(&mut owned_columns, &mut mappings, &mut allocations);
            chunk
                .created
                .account(&mut owned_columns, &mut mappings, &mut allocations);
            chunk
                .changed
                .account(&mut owned_columns, &mut mappings, &mut allocations);
            chunk
                .modified_ns
                .account(&mut owned_columns, &mut mappings, &mut allocations);
            chunk
                .changed_ns
                .account(&mut owned_columns, &mut mappings, &mut allocations);
            chunk
                .file_id
                .account(&mut owned_columns, &mut mappings, &mut allocations);
            chunk
                .parent_id
                .account(&mut owned_columns, &mut mappings, &mut allocations);
            chunk
                .flags
                .account(&mut owned_columns, &mut mappings, &mut allocations);
            chunk
                .path
                .account(&mut owned_columns, &mut mappings, &mut allocations);
            chunk
                .name
                .account(&mut owned_columns, &mut mappings, &mut allocations);
            chunk
                .folded_name
                .account(&mut owned_columns, &mut mappings, &mut allocations);
            chunk
                .folded_path
                .account(&mut owned_columns, &mut mappings, &mut allocations);
            chunk
                .search_name
                .account(&mut owned_columns, &mut mappings, &mut allocations);
            chunk
                .search_path
                .account(&mut owned_columns, &mut mappings, &mut allocations);
            chunk
                .parent
                .account(&mut owned_columns, &mut mappings, &mut allocations);
            chunk
                .extension
                .account(&mut owned_columns, &mut mappings, &mut allocations);
            chunk
                .folded_extension
                .account(&mut owned_columns, &mut mappings, &mut allocations);
            chunk
                .volume_id
                .account(&mut owned_columns, &mut mappings, &mut allocations);
            chunk
                .states
                .account(&mut owned_columns, &mut mappings, &mut allocations);
            match chunk.text.as_ref() {
                TextData::Owned(text) => owned_text += text.capacity(),
                TextData::Mapped { mapping, .. } => {
                    mappings.insert(Arc::as_ptr(mapping) as usize, mapping.len());
                }
            }
            labels += chunk.labels.iter().map(|s| s.capacity()).sum::<usize>();
            property_rows += chunk.properties.len();
        }
        serde_json::json!({"entries":self.len(),"blocks":self.chunks.len(),"owned_column_capacity_bytes":owned_columns,"owned_text_capacity_bytes":owned_text,"mapped_file_bytes":mappings.values().sum::<usize>(),"label_capacity_bytes":labels,"sparse_property_rows":property_rows,"row_view_bytes":std::mem::size_of::<EntryView<'_>>()})
    }
}

#[cfg(test)]
pub fn same_text_storage<'a, 'b>(
    first: impl Into<TextView<'a>>,
    second: impl Into<TextView<'b>>,
) -> bool {
    let first = first.into();
    let second = second.into();
    std::ptr::eq(first.prefix, second.prefix) && std::ptr::eq(first.suffix, second.suffix)
}
#[cfg(test)]
pub fn same_record(first: &impl FileEntry, second: &impl FileEntry) -> bool {
    serde_json::to_value(first.to_owned_file()).unwrap()
        == serde_json::to_value(second.to_owned_file()).unwrap()
}
