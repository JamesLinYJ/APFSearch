//! Disposable prepared search snapshots, published with atomic rename.
//! SQLite remains authoritative. The cache restores normalized columns, postings,
//! visibility and name order without repeating Unicode work or sorting on launch.
#[cfg(test)]
use crate::entry_table::FileEntry;
#[cfg(test)]
use crate::index_store::IndexedFile;
use crate::label_pool::LabelPool;
use crate::{
    index_store::{FileSlots, SearchSnapshot},
    metadata_postings::MetadataPostings,
};
use roaring::RoaringBitmap;
#[cfg(test)]
use serde_json::json;
#[cfg(test)]
use std::io::{Seek, SeekFrom};
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{self, BufWriter, Cursor, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
// Slot order remains stable when an older SQLite ID becomes visible again.
#[cfg(test)]
const MAGIC: &[u8; 8] = b"APFIDX03";
const HEADER: usize = 56;
static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) struct PreparedSnapshot {
    pub entries: crate::index_store::EntryTable,
    pub labels: Arc<LabelPool>,
    pub file_slots: FileSlots,
    pub live: RoaringBitmap,
    pub trigrams: Arc<HashMap<[u8; 3], Arc<RoaringBitmap>>>,
    pub name_order: Arc<Vec<u32>>,
    pub path_order: Arc<Vec<u32>>,
    pub path_ties: Arc<RoaringBitmap>,
    pub metadata_postings: MetadataPostings,
    pub generation: u64,
    pub content_revision: u64,
}
fn push64(out: &mut [u8], pos: &mut usize, value: u64) {
    out[*pos..*pos + 8].copy_from_slice(&value.to_le_bytes());
    *pos += 8;
}
struct TemporaryCache(PathBuf);
impl Drop for TemporaryCache {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
struct PayloadWriter<'a, W> {
    output: &'a mut W,
    hash: &'a mut blake3::Hasher,
    bytes: u64,
}
impl<W: Write> Write for PayloadWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.output.write(bytes)?;
        self.hash.update(&bytes[..written]);
        self.bytes += written as u64;
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}
fn checksum(header: &[u8], payload: &blake3::Hash) -> blake3::Hash {
    let mut hash = blake3::Hasher::new();
    hash.update(header);
    hash.update(payload.as_bytes());
    hash.finalize()
}
pub fn write(path: &Path, snapshot: &SearchSnapshot, revision: u64) -> Result<(), String> {
    segments::write(path, snapshot, revision).map_err(|error| error.to_string())
}
#[path = "snapshot_segments.rs"]
mod segments;
pub(crate) use segments::Section;
#[cfg(test)]
fn encode_snapshot(
    output: &mut (impl Write + Seek),
    snapshot: &SearchSnapshot,
    revision: u64,
) -> io::Result<()> {
    let mut header = [0u8; HEADER];
    output.write_all(&header)?;
    let mut payload_hash = blake3::Hasher::new();
    let (pool_bytes, index_bytes) = {
        let mut writer = PayloadWriter {
            output: &mut *output,
            hash: &mut payload_hash,
            bytes: 0,
        };
        writer.write_all(&(snapshot.entries.chunks.len() as u64).to_le_bytes())?;
        for chunk in &snapshot.entries.chunks {
            let bytes = encode_chunk(chunk)?;
            writer.write_all(&(bytes.len() as u64).to_le_bytes())?;
            writer.write_all(&bytes)?;
        }
        let chunk_bytes = writer.bytes;
        let index_start = writer.bytes;
        snapshot.live.serialize_into(&mut writer)?;
        writer.write_all(&(snapshot.name_order.len() as u64).to_le_bytes())?;
        for slot in snapshot.name_order.iter() {
            writer.write_all(&slot.to_le_bytes())?;
        }
        writer.write_all(&(snapshot.path_order.len() as u64).to_le_bytes())?;
        for slot in snapshot.path_order.iter() {
            writer.write_all(&slot.to_le_bytes())?;
        }
        snapshot.path_ties.serialize_into(&mut writer)?;
        writer.write_all(&(snapshot.trigrams.len() as u32).to_le_bytes())?;
        for (key, slots) in snapshot.trigrams.iter() {
            writer.write_all(key)?;
            slots.serialize_into(&mut writer)?;
        }
        snapshot.metadata_postings.write_to(&mut writer)?;
        (chunk_bytes, writer.bytes - index_start)
    };
    header[..8].copy_from_slice(MAGIC);
    let mut position = 8;
    for value in [
        snapshot.generation,
        revision,
        snapshot.content_revision,
        snapshot.entries.len() as u64,
        pool_bytes,
        index_bytes,
    ] {
        push64(&mut header, &mut position, value);
    }
    let digest = checksum(&header, &payload_hash.finalize());
    output.write_all(digest.as_bytes())?;
    output.seek(SeekFrom::Start(0))?;
    output.write_all(&header)?;
    Ok(())
}
fn get64(data: &[u8], position: &mut usize) -> Option<u64> {
    let value = u64::from_le_bytes(
        data.get(*position..position.checked_add(8)?)?
            .try_into()
            .ok()?,
    );
    *position += 8;
    Some(value)
}

fn align_buffer(bytes: &mut Vec<u8>) {
    bytes.resize(bytes.len().next_multiple_of(8), 0);
}
fn encode_chunk(chunk: &crate::entry_table::EntryChunk) -> io::Result<Vec<u8>> {
    let labels = serde_json::to_vec(chunk.labels.as_ref())?;
    let properties: std::collections::BTreeMap<_, _> = chunk
        .properties
        .iter()
        .map(|(slot, value)| (*slot, value))
        .collect();
    let properties = serde_json::to_vec(&properties)?;
    let mut bytes = Vec::new();
    let mut paths = Vec::new();
    let mut texts = Vec::new();
    for value in [
        chunk.len() as u64,
        chunk.text.text().len() as u64,
        labels.len() as u64,
        properties.len() as u64,
    ] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    chunk.id.encode(&mut bytes);
    chunk.size.encode(&mut bytes);
    chunk.modified.encode(&mut bytes);
    chunk.created.encode(&mut bytes);
    chunk.changed.encode(&mut bytes);
    chunk.modified_ns.encode(&mut bytes);
    chunk.changed_ns.encode(&mut bytes);
    chunk.file_id.encode(&mut bytes);
    chunk.parent_id.encode(&mut bytes);
    chunk.flags.encode(&mut bytes);
    encode_reference(&mut bytes, &chunk.path, &mut paths);
    encode_reference(&mut bytes, &chunk.name, &mut texts);
    encode_reference(&mut bytes, &chunk.folded_name, &mut texts);
    encode_reference(&mut bytes, &chunk.folded_path, &mut paths);
    encode_reference(&mut bytes, &chunk.search_name, &mut texts);
    encode_reference(&mut bytes, &chunk.search_path, &mut paths);
    encode_reference(&mut bytes, &chunk.parent, &mut texts);
    chunk.extension.encode(&mut bytes);
    chunk.folded_extension.encode(&mut bytes);
    chunk.volume_id.encode(&mut bytes);
    align_buffer(&mut bytes);
    bytes.extend_from_slice(chunk.states.bytes());
    bytes.extend_from_slice(&labels);
    bytes.extend_from_slice(&properties);
    bytes.extend_from_slice(chunk.text.text().as_bytes());
    align_buffer(&mut bytes);
    Ok(bytes)
}
fn encode_reference<'a, T: crate::entry_table::ColumnValue>(
    bytes: &mut Vec<u8>,
    column: &'a crate::entry_table::Column<T>,
    previous: &mut Vec<&'a crate::entry_table::Column<T>>,
) {
    align_buffer(bytes);
    let alias = previous
        .iter()
        .position(|other| other.values() == column.values());
    bytes.extend_from_slice(&(alias.map_or(0, |index| index + 1) as u64).to_le_bytes());
    if alias.is_none() {
        bytes.extend_from_slice(column.bytes());
    }
    previous.push(column);
}
fn decode_reference<T: crate::entry_table::ColumnValue>(
    mapping: &Arc<memmap2::Mmap>,
    position: &mut usize,
    count: usize,
    end: usize,
    previous: &mut Vec<crate::entry_table::Column<T>>,
) -> Option<crate::entry_table::Column<T>> {
    *position = position.checked_add(7)? & !7;
    let alias = usize::try_from(get64(mapping.get(..end)?, position)?).ok()?;
    let column = if alias == 0 {
        mapped_column::<T>(mapping, position, count, end)?
    } else {
        previous.get(alias - 1)?.clone()
    };
    previous.push(column.clone());
    Some(column)
}
fn mapped_column<T: crate::entry_table::ColumnValue>(
    mapping: &Arc<memmap2::Mmap>,
    offset: &mut usize,
    count: usize,
    end: usize,
) -> Option<crate::entry_table::Column<T>> {
    *offset = offset.checked_add(7)? & !7;
    let next = offset.checked_add(count.checked_mul(std::mem::size_of::<T>())?)?;
    if next > end {
        return None;
    }
    let column = crate::entry_table::Column::mapped(mapping.clone(), *offset..next)?;
    *offset = next;
    Some(column)
}
fn decode_chunk(
    mapping: &Arc<memmap2::Mmap>,
    start: usize,
    end: usize,
) -> Option<Arc<crate::entry_table::EntryChunk>> {
    use crate::entry_table::{EntryChunk, PathRef, TextData, TextRef};
    let data = mapping.get(start..end)?;
    let mut position = 0;
    let count = usize::try_from(get64(data, &mut position)?).ok()?;
    if count == 0 || count > crate::entry_table::CHUNK_LENGTH {
        return None;
    }
    let text_bytes = usize::try_from(get64(data, &mut position)?).ok()?;
    let label_bytes = usize::try_from(get64(data, &mut position)?).ok()?;
    let property_bytes = usize::try_from(get64(data, &mut position)?).ok()?;
    let mut position = start + position;
    let mut paths = Vec::new();
    let mut texts = Vec::new();
    let id =
        crate::integer_column::IntegerColumn::<i64>::decode(mapping, &mut position, count, end)?;
    let size =
        crate::integer_column::IntegerColumn::<u64>::decode(mapping, &mut position, count, end)?;
    let modified =
        crate::integer_column::IntegerColumn::<i64>::decode(mapping, &mut position, count, end)?;
    let created =
        crate::integer_column::IntegerColumn::<i64>::decode(mapping, &mut position, count, end)?;
    let changed =
        crate::integer_column::IntegerColumn::<i64>::decode(mapping, &mut position, count, end)?;
    let modified_ns =
        crate::integer_column::IntegerColumn::<i64>::decode(mapping, &mut position, count, end)?;
    let changed_ns =
        crate::integer_column::IntegerColumn::<i64>::decode(mapping, &mut position, count, end)?;
    let file_id =
        crate::integer_column::IntegerColumn::<u64>::decode(mapping, &mut position, count, end)?;
    let parent_id =
        crate::integer_column::IntegerColumn::<u64>::decode(mapping, &mut position, count, end)?;
    let flags =
        crate::integer_column::IntegerColumn::<u32>::decode(mapping, &mut position, count, end)?;
    let path = decode_reference::<PathRef>(mapping, &mut position, count, end, &mut paths)?;
    let name = decode_reference::<TextRef>(mapping, &mut position, count, end, &mut texts)?;
    let folded_name = decode_reference::<TextRef>(mapping, &mut position, count, end, &mut texts)?;
    let folded_path = decode_reference::<PathRef>(mapping, &mut position, count, end, &mut paths)?;
    let search_name = decode_reference::<TextRef>(mapping, &mut position, count, end, &mut texts)?;
    let search_path = decode_reference::<PathRef>(mapping, &mut position, count, end, &mut paths)?;
    let parent = decode_reference::<TextRef>(mapping, &mut position, count, end, &mut texts)?;
    let extension =
        crate::integer_column::IntegerColumn::<u32>::decode(mapping, &mut position, count, end)?;
    let folded_extension =
        crate::integer_column::IntegerColumn::<u32>::decode(mapping, &mut position, count, end)?;
    let volume_id =
        crate::integer_column::IntegerColumn::<u32>::decode(mapping, &mut position, count, end)?;
    let states = mapped_column::<u8>(mapping, &mut position, count, end)?;
    let labels_end = position.checked_add(label_bytes)?;
    if labels_end > end {
        return None;
    }
    let labels: Vec<String> = serde_json::from_slice(mapping.get(position..labels_end)?).ok()?;
    let properties_end = labels_end.checked_add(property_bytes)?;
    if properties_end > end {
        return None;
    }
    let properties: HashMap<usize, serde_json::Value> =
        serde_json::from_slice(mapping.get(labels_end..properties_end)?).ok()?;
    let text_end = properties_end.checked_add(text_bytes)?;
    if text_end > end || text_end.checked_add(7)? & !7 != end {
        return None;
    }
    let text = std::str::from_utf8(mapping.get(properties_end..text_end)?).ok()?;
    let references = [
        name.values(),
        folded_name.values(),
        search_name.values(),
        parent.values(),
    ]
    .into_iter()
    .flatten()
    .copied()
    .chain(
        [path.values(), folded_path.values(), search_path.values()]
            .into_iter()
            .flatten()
            .flat_map(|path| [path.prefix, path.suffix]),
    );
    {
        for reference in references {
            let start = reference.offset as usize;
            let end = start.checked_add(reference.length as usize)?;
            text.get(start..end)?;
        }
    }
    // Matchers rely on the writer's last-separator split. Check it before
    // publishing borrowed views, even for a checksummed but malformed section.
    for column in [&path, &folded_path, &search_path] {
        for path in column.values() {
            let fragment = |reference: TextRef| {
                &text[reference.offset as usize
                    ..(reference.offset as usize + reference.length as usize)]
            };
            let prefix = fragment(path.prefix);
            if (!prefix.is_empty() && !prefix.ends_with('/')) || fragment(path.suffix).contains('/')
            {
                return None;
            }
        }
    }
    for column in [&extension, &folded_extension, &volume_id] {
        if (0..count).any(|slot| column.get(slot) as usize >= labels.len()) {
            return None;
        }
    }
    if states.values().iter().enumerate().any(|(slot, state)| {
        state & !31 != 0 || (*state & 16 != 0) != properties.contains_key(&slot) || state & 24 == 24
    }) || properties.keys().any(|slot| *slot >= count)
    {
        return None;
    }
    Some(Arc::new(EntryChunk {
        pending_text: None,
        section: std::sync::OnceLock::new(),
        id,
        size,
        modified,
        created,
        changed,
        modified_ns,
        changed_ns,
        file_id,
        parent_id,
        flags,
        path,
        name,
        folded_name,
        folded_path,
        search_name,
        search_path,
        parent,
        extension,
        folded_extension,
        volume_id,
        states,
        text: Arc::new(TextData::Mapped {
            mapping: mapping.clone(),
            range: properties_end..text_end,
        }),
        labels: Arc::new(labels),
        properties: Arc::new(properties),
    }))
}

fn read_bitmap(reader: &mut impl Read, count: usize) -> Option<RoaringBitmap> {
    let bitmap = RoaringBitmap::deserialize_from(reader).ok()?;
    if bitmap.max().is_some_and(|slot| slot as usize >= count) {
        return None;
    }
    Some(bitmap)
}
pub fn read(path: &Path, generation: u64, revision: u64) -> Option<(SearchSnapshot, u64)> {
    segments::read(path, generation, revision).map(|snapshot| (snapshot, generation))
}
const PARALLEL_CHECKSUM_MIN_BYTES: usize = 8 * 1024 * 1024;

fn payload_digest(bytes: &[u8]) -> blake3::Hash {
    if bytes.len() >= PARALLEL_CHECKSUM_MIN_BYTES
        && let Some(permit) = crate::cpu_executor::acquire_all()
    {
        return permit.run(|| {
            let mut hasher = blake3::Hasher::new();
            hasher.update_rayon(bytes);
            hasher.finalize()
        });
    }
    blake3::hash(bytes)
}

#[cfg(test)]
fn decode(data: &[u8], generation: u64, revision: u64) -> Option<SearchSnapshot> {
    decode_with_metrics(data, generation, revision, |_, _| {})
}
#[cfg(test)]
fn decode_with_metrics(
    data: &[u8],
    generation: u64,
    revision: u64,
    phase: impl FnMut(&str, usize),
) -> Option<SearchSnapshot> {
    let mut mapping = memmap2::MmapMut::map_anon(data.len()).ok()?;
    mapping.copy_from_slice(data);
    decode_mapping(
        Arc::new(mapping.make_read_only().ok()?),
        generation,
        revision,
        phase,
    )
}
#[cfg(test)]
fn decode_mapping(
    mapping: Arc<memmap2::Mmap>,
    generation: u64,
    revision: u64,
    mut phase: impl FnMut(&str, usize),
) -> Option<SearchSnapshot> {
    let data = mapping.as_ref();
    if data.get(..8)? != MAGIC {
        return None;
    }
    let mut position = 8;
    if get64(data, &mut position)? != generation || get64(data, &mut position)? != revision {
        return None;
    }
    let content_revision = get64(data, &mut position)?;
    let count = usize::try_from(get64(data, &mut position)?).ok()?;
    if count > u32::MAX as usize {
        return None;
    }
    let pool_bytes = usize::try_from(get64(data, &mut position)?).ok()?;
    let index_bytes = usize::try_from(get64(data, &mut position)?).ok()?;
    let index_start = HEADER.checked_add(pool_bytes)?;
    let payload_end = index_start.checked_add(index_bytes)?;
    if payload_end.checked_add(32)? != data.len() {
        return None;
    }
    let expected = checksum(
        data.get(..HEADER)?,
        &payload_digest(data.get(HEADER..payload_end)?),
    );
    if expected.as_bytes() != data.get(payload_end..)? {
        return None;
    }
    phase("checksum_verified", 0);
    let mut position = HEADER;
    let chunk_count = usize::try_from(get64(data, &mut position)?).ok()?;
    if chunk_count != count.div_ceil(crate::entry_table::CHUNK_LENGTH) {
        return None;
    }
    let mut chunks = Vec::with_capacity(chunk_count);
    for _ in 0..chunk_count {
        let length = usize::try_from(get64(data, &mut position)?).ok()?;
        let end = position.checked_add(length)?;
        if end > index_start || !position.is_multiple_of(8) {
            return None;
        }
        chunks.push(decode_chunk(&mapping, position, end)?);
        position = end;
    }
    if position != index_start {
        return None;
    }
    let entries = crate::entry_table::EntryTable::from_chunks(chunks)?;
    if entries.len() != count {
        return None;
    }
    phase("columns_mapped", 0);
    restore_indexes(
        entries,
        data.get(index_start..payload_end)?,
        generation,
        content_revision,
        phase,
    )
}

#[cfg(test)]
fn restore_indexes(
    entries: crate::entry_table::EntryTable,
    index_data: &[u8],
    generation: u64,
    content_revision: u64,
    mut phase: impl FnMut(&str, usize),
) -> Option<SearchSnapshot> {
    let count = entries.len();
    let index_bytes = index_data.len();
    let labels = Arc::new(LabelPool::default());
    let file_slots = FileSlots::from_entries(&entries).ok()?;
    let mut indexes = Cursor::new(index_data);
    let live = read_bitmap(&mut indexes, count)?;
    let mut size_bytes = [0; 8];
    indexes.read_exact(&mut size_bytes).ok()?;
    let order_count = usize::try_from(u64::from_le_bytes(size_bytes)).ok()?;
    if order_count != live.len() as usize {
        return None;
    }
    let mut name_order = Vec::with_capacity(order_count);
    let mut ordered = RoaringBitmap::new();
    for _ in 0..order_count {
        let mut slot_bytes = [0; 4];
        indexes.read_exact(&mut slot_bytes).ok()?;
        let slot = u32::from_le_bytes(slot_bytes);
        if !live.contains(slot) || !ordered.insert(slot) {
            return None;
        }
        name_order.push(slot);
    }
    let mut size_bytes = [0; 8];
    indexes.read_exact(&mut size_bytes).ok()?;
    let path_count = usize::try_from(u64::from_le_bytes(size_bytes)).ok()?;
    if path_count != live.len() as usize {
        return None;
    }
    let mut path_order = Vec::with_capacity(path_count);
    let mut ordered = RoaringBitmap::new();
    for _ in 0..path_count {
        let mut slot_bytes = [0; 4];
        indexes.read_exact(&mut slot_bytes).ok()?;
        let slot = u32::from_le_bytes(slot_bytes);
        if !live.contains(slot) || !ordered.insert(slot) {
            return None;
        }
        path_order.push(slot);
    }
    let path_ties = read_bitmap(&mut indexes, count)?;
    if !path_ties.is_subset(&live) {
        return None;
    }
    let mut size_bytes = [0; 4];
    indexes.read_exact(&mut size_bytes).ok()?;
    let trigram_count = u32::from_le_bytes(size_bytes) as usize;
    if trigram_count > 1 << 24 || trigram_count > index_bytes / 3 {
        return None;
    }
    let mut trigrams = HashMap::new();
    for _ in 0..trigram_count {
        let mut key = [0; 3];
        indexes.read_exact(&mut key).ok()?;
        let bitmap = read_bitmap(&mut indexes, count)?;
        if trigrams.insert(key, Arc::new(bitmap)).is_some() {
            return None;
        }
    }
    let metadata_postings = MetadataPostings::read_from(&mut indexes, count).ok()?;
    if indexes.position() != index_bytes as u64 {
        return None;
    }
    phase("postings_restored", 0);
    let snapshot = SearchSnapshot::from_prepared_cache(PreparedSnapshot {
        entries,
        labels,
        file_slots,
        live,
        trigrams: Arc::new(trigrams),
        name_order: Arc::new(name_order),
        path_order: Arc::new(path_order),
        path_ties: Arc::new(path_ties),
        metadata_postings,
        generation,
        content_revision,
    });
    phase("snapshot_ready", 0);
    Some(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn parallel_payload_digest_preserves_full_blake3_verification() {
        for length in [
            0,
            1023,
            1024,
            PARALLEL_CHECKSUM_MIN_BYTES - 1,
            PARALLEL_CHECKSUM_MIN_BYTES,
            PARALLEL_CHECKSUM_MIN_BYTES + 1025,
        ] {
            let mut bytes: Vec<u8> = (0..length).map(|index| (index % 251) as u8).collect();
            let original = payload_digest(&bytes);
            assert_eq!(original, blake3::hash(&bytes));
            if let Some(last) = bytes.last_mut() {
                *last ^= 1;
                assert_ne!(original, payload_digest(&bytes));
                assert_eq!(payload_digest(&bytes), blake3::hash(&bytes));
            }
        }
    }

    fn update_checksum(bytes: &mut [u8]) {
        let end = bytes.len() - 32;
        let digest = checksum(&bytes[..HEADER], &blake3::hash(&bytes[HEADER..end]));
        bytes[end..].copy_from_slice(digest.as_bytes());
    }

    // Change a checksummed header independently of the production writer.
    fn set_version(bytes: &mut [u8], magic: &[u8; 8]) {
        bytes[..8].copy_from_slice(magic);
        update_checksum(bytes);
    }

    fn entry(id: i64, name: &str) -> IndexedFile {
        serde_json::from_value(json!({"id":id,"path":format!("/fixture/shared/{name}"),"name":name,"extension":Path::new(name).extension().unwrap_or_default().to_string_lossy(),"size":2048,"modified":1726185600_i64,"created":1726099200_i64,"changed":1726185600_i64,"modified_ns":123456789,"changed_ns":987654321,"is_dir":false,"is_symlink":false,"file_id":id+100,"parent_id":3,"volume_id":"fixture-volume","flags":0,"properties":{"width":1920},"content_indexed":true})).unwrap()
    }
    fn fixture() -> SearchSnapshot {
        let first = SearchSnapshot::new(
            vec![
                entry(1, "File2.txt"),
                entry(2, "removed.pdf"),
                entry(3, "Straße café 报告10.pdf"),
                entry(4, "FILE2.txt"),
            ],
            7,
        );
        let mut snapshot = SearchSnapshot::from_changes(vec![(2, None)], 8, &first).unwrap();
        snapshot.content_revision = 13;
        snapshot
    }

    fn stable_slot_fixture() -> SearchSnapshot {
        let original = SearchSnapshot::new(
            vec![
                entry(1, "File2.txt"),
                entry(5, "removed.pdf"),
                entry(9, "Straße café 报告10.pdf"),
            ],
            7,
        );
        SearchSnapshot::from_changes(
            vec![
                (5, None),
                (2, Some(entry(2, "restored2.pdf"))),
                (3, Some(entry(3, "restored10.txt"))),
            ],
            8,
            &original,
        )
        .unwrap()
    }

    #[test]
    fn other_cache_formats_are_rejected_without_rewriting() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("unsupported.snapshot.bin");
        write(&path, &fixture(), 19).unwrap();
        let original = std::fs::read(&path).unwrap();
        for magic in [b"AFSIDX03", b"AFSIDX04", b"APFIDX01"] {
            let mut bytes = original.clone();
            set_version(&mut bytes, magic);
            std::fs::write(&path, &bytes).unwrap();
            let before = std::fs::metadata(&path).unwrap();
            assert!(read(&path, 8, 19).is_none());
            let after = std::fs::metadata(&path).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert_eq!(before.ino(), after.ino());
            assert_eq!(before.mtime_nsec(), after.mtime_nsec());
        }
    }

    #[test]
    fn restored_paths_and_properties_outlive_the_cache_mapping() {
        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("owned.snapshot.bin");
        let entries = [
            ("/", "/"),
            ("/fixture/路径 café.txt", "路径 café.txt"),
            ("/fixture/original.txt", "display-alias.txt"),
            ("/fixture//nested/item", "item"),
            ("/fixture/trailing/", ""),
            ("relative/file.txt", "file.txt"),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (path, name))| {
            let mut file = entry(index as i64 + 1, name);
            file.path = path.into();
            file.properties = if index % 2 == 0 {
                json!({})
            } else {
                json!({"description":"属性文本 café", "nested":{"tags":["one", "二"]}})
            };
            file
        })
        .collect();
        let original = SearchSnapshot::new(entries, 1);
        write(&cache, &original, 1).unwrap();
        let (restored, _) = read(&cache, 1, 1).unwrap();
        std::fs::remove_file(cache).unwrap();
        assert_eq!(
            serde_json::to_value(&restored.entries).unwrap(),
            serde_json::to_value(&original.entries).unwrap()
        );
    }

    #[test]
    fn roundtrip_keeps_sparse_old_ids_in_their_original_slots() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stable.snapshot.bin");
        let original = stable_slot_fixture();
        assert_eq!(
            original
                .entries
                .iter()
                .map(|file| file.id())
                .collect::<Vec<_>>(),
            [1, 5, 9, 2, 3]
        );
        write(&path, &original, 19).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..8], b"APFMAP03");
        let (restored, _) = read(&path, 8, 19).unwrap();
        assert_eq!(restored.live, original.live);
        assert_eq!(restored.name_order, original.name_order);
        assert_eq!(restored.path_order, original.path_order);
        assert_eq!(restored.path_ties, original.path_ties);
        assert_eq!(restored.trigrams, original.trigrams);
        assert_eq!(
            serde_json::to_value(&restored.entries).unwrap(),
            serde_json::to_value(&original.entries).unwrap()
        );
        for (slot, file) in original.entries.iter().enumerate() {
            assert_eq!(restored.slot_for_id(file.id()), Some(slot));
        }
        assert_eq!(restored.slot_for_id(4), None);
        for text in [
            "a",
            "re",
            "restored",
            "ext:pdf",
            "!ext:pdf",
            "cafe",
            "报告",
            "size:>=2048",
            "size:>2048 | ext:txt",
        ] {
            let query = crate::query::parse(text, &HashMap::new()).unwrap();
            assert_eq!(original.candidates(&query), restored.candidates(&query));
            assert_eq!(
                original.metadata_postings.exact(&query, &original.live),
                restored.metadata_postings.exact(&query, &restored.live)
            );
        }
    }

    #[test]
    fn valid_checksum_cannot_hide_duplicate_ids() {
        let mut archive = Cursor::new(Vec::new());
        encode_snapshot(&mut archive, &stable_slot_fixture(), 19).unwrap();
        let original = archive.into_inner();
        assert!(decode(&original, 8, 19).is_some());
        // Duplicate within the initial sorted prefix, against that prefix from
        // a sparse tail entry, and within the sparse tail itself.
        for (slot, duplicate_id) in [(1, 1_i64), (3, 5_i64), (4, 2_i64)] {
            let mut bytes = original.clone();
            let column = HEADER + 8 + 8 + 32;
            let base = u64::from_le_bytes(bytes[column..column + 8].try_into().unwrap());
            let width =
                u64::from_le_bytes(bytes[column + 8..column + 16].try_into().unwrap()) as usize;
            assert!(width > 0);
            let stored = if width == 8 {
                duplicate_id as u64
            } else {
                ((duplicate_id as u64) ^ (1 << 63)) - base
            };
            let offset = column + 16 + slot * width;
            bytes[offset..offset + width].copy_from_slice(&stored.to_le_bytes()[..width]);
            set_version(&mut bytes, MAGIC);
            assert!(decode(&bytes, 8, 19).is_none());
        }
    }
    #[test]
    fn prepared_snapshot_roundtrip_preserves_slots_search_columns_and_shared_strings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("index.snapshot.bin");
        let original = fixture();
        write(&path, &original, 19).unwrap();
        let (restored, generation) = read(&path, 8, 19).unwrap();
        assert_eq!(generation, 8);
        assert_eq!(restored.content_revision, 13);
        assert_eq!(restored.entries.len(), 4);
        assert!(!restored.path_ties.is_empty());
        assert_eq!(restored.live, original.live);
        assert_eq!(restored.name_order, original.name_order);
        assert_eq!(restored.path_order, original.path_order);
        assert_eq!(restored.path_ties, original.path_ties);
        assert_eq!(restored.trigrams, original.trigrams);
        assert_eq!(
            serde_json::to_value(&restored.entries).unwrap(),
            serde_json::to_value(&original.entries).unwrap()
        );
        for (before, after) in original.entries.iter().zip(restored.entries.iter()) {
            assert_eq!(before.folded_name(), after.folded_name());
            assert_eq!(before.search_name(), after.search_name());
            assert_eq!(before.folded_path(), after.folded_path());
            assert_eq!(before.search_path(), after.search_path());
            assert_eq!(before.parent(), after.parent());
        }
        assert!(crate::entry_table::same_text_storage(
            restored.entries.at(0).parent(),
            restored.entries.at(2).parent()
        ));
        assert!(crate::entry_table::same_text_storage(
            restored.entries.at(0).folded_name(),
            restored.entries.at(0).search_name()
        ));
        for text in ["a", "fi", "ext:pdf", "!ext:pdf", "folder:", "cafe", "报告"] {
            let query = crate::query::parse(text, &HashMap::new()).unwrap();
            assert_eq!(original.candidates(&query), restored.candidates(&query));
            assert_eq!(
                original.metadata_postings.exact(&query, &original.live),
                restored.metadata_postings.exact(&query, &restored.live)
            );
        }
        assert!(read(&path, 9, 19).is_none());
        assert!(read(&path, 8, 20).is_none());
        // A second publication does not truncate an existing mapping.
        let file = File::open(&path).unwrap();
        let mapped = unsafe { memmap2::Mmap::map(&file).unwrap() };
        let original_manifest = mapped.to_vec();
        write(
            &path,
            &SearchSnapshot::new(vec![entry(1, "replacement")], 9),
            20,
        )
        .unwrap();
        assert_eq!(&mapped[..], original_manifest);
        assert_eq!(
            serde_json::to_value(&restored.entries).unwrap(),
            serde_json::to_value(&original.entries).unwrap()
        );
        assert!(read(&path, 9, 20).is_some());
    }
    #[test]
    fn truncated_corrupt_and_obsolete_snapshots_are_rejected() {
        let mut archive = Cursor::new(Vec::new());
        encode_snapshot(&mut archive, &fixture(), 19).unwrap();
        let bytes = archive.into_inner();
        assert!(decode(&bytes, 8, 19).is_some());
        for length in 0..bytes.len() {
            assert!(
                decode(&bytes[..length], 8, 19).is_none(),
                "accepted truncated cache at {length}"
            );
        }
        for position in [0, 24, HEADER, bytes.len() / 2, bytes.len() - 1] {
            let mut corrupt = bytes.clone();
            corrupt[position] ^= 1;
            assert!(decode(&corrupt, 8, 19).is_none());
        }
        let mut obsolete = bytes;
        obsolete[..8].copy_from_slice(b"APFCIDX2");
        assert!(decode(&obsolete, 8, 19).is_none());
    }
    #[test]
    fn valid_checksum_does_not_allow_invalid_slot_or_duplicate_order() {
        let mut archive = Cursor::new(Vec::new());
        encode_snapshot(&mut archive, &fixture(), 19).unwrap();
        let original = archive.into_inner();
        assert!(decode(&original, 8, 19).is_some());
        let mut position = 32;
        let count = get64(&original, &mut position).unwrap() as usize;
        let pool_size = get64(&original, &mut position).unwrap() as usize;
        let index_start = HEADER + pool_size;
        let mut cursor = Cursor::new(&original[index_start..]);
        let _ = read_bitmap(&mut cursor, count).unwrap();
        let order_start = index_start + cursor.position() as usize + 8;
        let first_slot =
            u32::from_le_bytes(original[order_start..order_start + 4].try_into().unwrap());
        for value in [u32::MAX, first_slot] {
            let mut bytes = original.clone();
            bytes[order_start + 4..order_start + 8].copy_from_slice(&value.to_le_bytes());
            let end = bytes.len() - 32;
            let digest = checksum(&bytes[..HEADER], &blake3::hash(&bytes[HEADER..end]));
            bytes[end..].copy_from_slice(digest.as_bytes());
            assert!(decode(&bytes, 8, 19).is_none());
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
#[path = "cache_memory_tests.rs"]
mod memory_tests;

#[cfg(test)]
#[path = "cache_fixture_conversion_tests.rs"]
mod fixture_conversion;
