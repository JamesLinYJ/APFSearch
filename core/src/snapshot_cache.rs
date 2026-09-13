//! Disposable prepared search snapshots, published with atomic rename.
//! SQLite remains authoritative. The cache restores normalized columns, postings,
//! visibility and name order without repeating Unicode work or sorting on launch.
use crate::{
    index_store::{FileSlots, IndexedFile, SearchSnapshot},
    metadata_postings::MetadataPostings,
};
use roaring::RoaringBitmap;
use serde_json::json;
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{self, BufWriter, Cursor, Read, Seek, SeekFrom, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
// Slot order remains stable when an older SQLite ID becomes visible again.
const MAGIC: &[u8; 8] = b"APFIDX01";
const HEADER: usize = 56;
const RECORD: usize = 212;
static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) struct PreparedSnapshot {
    pub entries: Vec<Arc<IndexedFile>>,
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
#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct StringReference {
    offset: u64,
    length: u32,
}
fn intern<'a>(
    text: &'a str,
    strings: &mut HashMap<&'a str, StringReference>,
    pool: &mut Vec<u8>,
) -> io::Result<StringReference> {
    if let Some(reference) = strings.get(text) {
        return Ok(*reference);
    }
    let reference = append_string(text, pool)?;
    strings.insert(text, reference);
    Ok(reference)
}
fn append_string(text: &str, pool: &mut Vec<u8>) -> io::Result<StringReference> {
    let reference = StringReference {
        offset: pool.len() as u64,
        length: text
            .len()
            .try_into()
            .map_err(|_| invalid("Cache string exceeds 4 GiB"))?,
    };
    pool.extend_from_slice(text.as_bytes());
    Ok(reference)
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn push64(out: &mut [u8], pos: &mut usize, value: u64) {
    out[*pos..*pos + 8].copy_from_slice(&value.to_le_bytes());
    *pos += 8;
}
fn push32(out: &mut [u8], pos: &mut usize, value: u32) {
    out[*pos..*pos + 4].copy_from_slice(&value.to_le_bytes());
    *pos += 4;
}
fn push_reference(out: &mut [u8], pos: &mut usize, reference: StringReference) {
    push64(out, pos, reference.offset);
    push32(out, pos, reference.length);
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
    write_snapshot(path, snapshot, revision).map_err(|error| error.to_string())
}
fn write_snapshot(path: &Path, snapshot: &SearchSnapshot, revision: u64) -> io::Result<()> {
    let temporary = TemporaryCache(path.with_extension(format!(
        "{}-{}.tmp",
        std::process::id(),
        TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary.0)?;
    let mut output = BufWriter::new(file);
    let mut header = [0u8; HEADER];
    output.write_all(&header)?;
    let mut payload_hash = blake3::Hasher::new();
    let (pool_bytes, index_bytes) = {
        let mut writer = PayloadWriter {
            output: &mut output,
            hash: &mut payload_hash,
            bytes: 0,
        };
        let mut pool = Vec::new();
        let mut strings = HashMap::new();
        let mut properties = HashMap::<String, StringReference>::new();
        let empty_properties = intern("{}", &mut strings, &mut pool)?;
        for entry in &snapshot.entries {
            let properties_reference = if entry
                .properties
                .as_object()
                .is_some_and(|object| object.is_empty())
            {
                empty_properties
            } else {
                let text = entry.properties.to_string();
                if let Some(reference) = properties.get(&text) {
                    *reference
                } else {
                    let reference = append_string(&text, &mut pool)?;
                    properties.insert(text, reference);
                    reference
                }
            };
            let canonical = Path::new(entry.parent.as_ref()).join(&entry.name);
            let override_path = if canonical.as_os_str() == Path::new(&entry.path).as_os_str() {
                ""
            } else {
                &entry.path
            };
            let mut row = [0u8; RECORD];
            let mut position = 0;
            push64(&mut row, &mut position, entry.id as u64);
            for text in [
                entry.parent.as_ref(),
                entry.name.as_str(),
                entry.extension.as_str(),
                entry.volume_id.as_str(),
            ] {
                push_reference(
                    &mut row,
                    &mut position,
                    intern(text, &mut strings, &mut pool)?,
                );
            }
            push_reference(&mut row, &mut position, properties_reference);
            for text in [
                override_path,
                entry.folded_name.as_ref(),
                entry.folded_extension.as_str(),
                entry.folded_path.as_ref(),
                entry.search_name.as_ref(),
                entry.search_path.as_ref(),
            ] {
                push_reference(
                    &mut row,
                    &mut position,
                    intern(text, &mut strings, &mut pool)?,
                );
            }
            for value in [
                entry.size,
                entry.modified as u64,
                entry.created as u64,
                entry.changed as u64,
                entry.modified_ns as u64,
                entry.changed_ns as u64,
                entry.file_id,
                entry.parent_id,
            ] {
                push64(&mut row, &mut position, value);
            }
            push32(&mut row, &mut position, entry.flags);
            push32(
                &mut row,
                &mut position,
                u32::from(entry.is_dir)
                    | (u32::from(entry.is_symlink) << 1)
                    | (u32::from(entry.content_indexed) << 2),
            );
            debug_assert_eq!(position, RECORD);
            writer.write_all(&row)?;
        }
        writer.write_all(&pool)?;
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
        (pool.len() as u64, writer.bytes - index_start)
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
    output.flush()?;
    output.get_ref().sync_all()?;
    drop(output);
    std::fs::rename(&temporary.0, path)
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
fn get32(data: &[u8], position: &mut usize) -> Option<u32> {
    let value = u32::from_le_bytes(
        data.get(*position..position.checked_add(4)?)?
            .try_into()
            .ok()?,
    );
    *position += 4;
    Some(value)
}
struct StringPool<'a> {
    bytes: &'a [u8],
    shared: HashMap<StringReference, Arc<str>>,
}
impl StringPool<'_> {
    fn reference(row: &[u8], position: &mut usize) -> Option<StringReference> {
        Some(StringReference {
            offset: get64(row, position)?,
            length: get32(row, position)?,
        })
    }
    fn text(&self, reference: StringReference) -> Option<&str> {
        let start = usize::try_from(reference.offset).ok()?;
        std::str::from_utf8(
            self.bytes
                .get(start..start.checked_add(reference.length as usize)?)?,
        )
        .ok()
    }
    fn owned(&self, row: &[u8], position: &mut usize) -> Option<String> {
        Some(self.text(Self::reference(row, position)?)?.into())
    }
    fn shared(&mut self, row: &[u8], position: &mut usize) -> Option<Arc<str>> {
        let reference = Self::reference(row, position)?;
        if let Some(text) = self.shared.get(&reference) {
            return Some(text.clone());
        }
        let text: Arc<str> = self.text(reference)?.into();
        self.shared.insert(reference, text.clone());
        Some(text)
    }
}
fn read_bitmap(reader: &mut impl Read, count: usize) -> Option<RoaringBitmap> {
    let bitmap = RoaringBitmap::deserialize_from(reader).ok()?;
    if bitmap.max().is_some_and(|slot| slot as usize >= count) {
        return None;
    }
    Some(bitmap)
}
pub fn read(path: &Path, generation: u64, revision: u64) -> Option<(SearchSnapshot, u64)> {
    let file = File::open(path).ok()?;
    // This module only publishes immutable files by rename; mapped files are
    // never truncated by a cache writer, and all offsets are checked below.
    let mapped = unsafe { memmap2::Mmap::map(&file).ok()? };
    decode(&mapped, generation, revision).map(|snapshot| (snapshot, generation))
}
fn decode(data: &[u8], generation: u64, revision: u64) -> Option<SearchSnapshot> {
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
    let pool_start = HEADER.checked_add(count.checked_mul(RECORD)?)?;
    let index_start = pool_start.checked_add(pool_bytes)?;
    let payload_end = index_start.checked_add(index_bytes)?;
    if payload_end.checked_add(32)? != data.len() {
        return None;
    }
    let expected = checksum(
        data.get(..HEADER)?,
        &blake3::hash(data.get(HEADER..payload_end)?),
    );
    if expected.as_bytes() != data.get(payload_end..)? {
        return None;
    }
    let mut pool = StringPool {
        bytes: data.get(pool_start..index_start)?,
        shared: HashMap::new(),
    };
    let mut entries: Vec<Arc<IndexedFile>> = Vec::with_capacity(count);
    for row in data.get(HEADER..pool_start)?.as_chunks::<RECORD>().0 {
        let mut position = 0;
        let id = get64(row, &mut position)? as i64;
        let parent = pool.shared(row, &mut position)?;
        let name = pool.owned(row, &mut position)?;
        let extension = pool.owned(row, &mut position)?;
        let volume_id = pool.owned(row, &mut position)?;
        let properties_text = pool.owned(row, &mut position)?;
        let override_path = pool.owned(row, &mut position)?;
        let path = if override_path.is_empty() {
            Path::new(parent.as_ref())
                .join(&name)
                .to_string_lossy()
                .into_owned()
        } else {
            override_path
        };
        let folded_name = pool.shared(row, &mut position)?;
        let folded_extension = pool.owned(row, &mut position)?;
        let folded_path = pool.shared(row, &mut position)?;
        let search_name = pool.shared(row, &mut position)?;
        let search_path = pool.shared(row, &mut position)?;
        let size = get64(row, &mut position)?;
        let modified = get64(row, &mut position)? as i64;
        let created = get64(row, &mut position)? as i64;
        let changed = get64(row, &mut position)? as i64;
        let modified_ns = get64(row, &mut position)? as i64;
        let changed_ns = get64(row, &mut position)? as i64;
        let file_id = get64(row, &mut position)?;
        let parent_id = get64(row, &mut position)?;
        let flags = get32(row, &mut position)?;
        let bits = get32(row, &mut position)?;
        if bits & !7 != 0 {
            return None;
        }
        entries.push(Arc::new(IndexedFile {
            id,
            path,
            name,
            extension,
            volume_id,
            properties: if properties_text == "{}" {
                json!({})
            } else {
                serde_json::from_str(&properties_text).ok()?
            },
            parent,
            folded_name,
            folded_extension,
            folded_path,
            search_name,
            search_path,
            size,
            modified,
            created,
            changed,
            modified_ns,
            changed_ns,
            file_id,
            parent_id,
            flags,
            is_dir: bits & 1 != 0,
            is_symlink: bits & 2 != 0,
            content_indexed: bits & 4 != 0,
        }));
    }
    drop(pool);
    // The usual sorted prefix needs no permanent ID map. Sparse appended IDs
    // are checked against both that prefix and each other by FileSlots.
    let file_slots = FileSlots::from_entries(&entries).ok()?;
    let mut indexes = Cursor::new(data.get(index_start..payload_end)?);
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
    Some(SearchSnapshot::from_prepared_cache(PreparedSnapshot {
        entries,
        file_slots,
        live,
        trigrams: Arc::new(trigrams),
        name_order: Arc::new(name_order),
        path_order: Arc::new(path_order),
        path_ties: Arc::new(path_ties),
        metadata_postings,
        generation,
        content_revision,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

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
        for magic in [b"AFSIDX03", b"AFSIDX04", b"APFIDX02"] {
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
    fn roundtrip_keeps_sparse_old_ids_in_their_original_slots() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stable.snapshot.bin");
        let original = stable_slot_fixture();
        assert_eq!(
            original
                .entries
                .iter()
                .map(|file| file.id)
                .collect::<Vec<_>>(),
            [1, 5, 9, 2, 3]
        );
        write(&path, &original, 19).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..8], MAGIC);
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
            assert_eq!(restored.slot_for_id(file.id), Some(slot));
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
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("duplicate.snapshot.bin");
        write(&path, &stable_slot_fixture(), 19).unwrap();
        let original = std::fs::read(&path).unwrap();
        // Duplicate within the initial sorted prefix, against that prefix from
        // a sparse tail entry, and within the sparse tail itself.
        for (slot, duplicate_id) in [(1, 1_i64), (3, 5_i64), (4, 2_i64)] {
            let mut bytes = original.clone();
            let offset = HEADER + slot * RECORD;
            bytes[offset..offset + 8].copy_from_slice(&duplicate_id.to_le_bytes());
            set_version(&mut bytes, MAGIC);
            std::fs::write(&path, &bytes).unwrap();
            assert!(read(&path, 8, 19).is_none());
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
        for (before, after) in original.entries.iter().zip(&restored.entries) {
            assert_eq!(before.folded_name, after.folded_name);
            assert_eq!(before.search_name, after.search_name);
            assert_eq!(before.folded_path, after.folded_path);
            assert_eq!(before.search_path, after.search_path);
            assert_eq!(before.parent, after.parent);
        }
        assert!(Arc::ptr_eq(
            &restored.entries[0].parent,
            &restored.entries[2].parent
        ));
        assert!(Arc::ptr_eq(
            &restored.entries[0].folded_name,
            &restored.entries[0].search_name
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
        write(
            &path,
            &SearchSnapshot::new(vec![entry(1, "replacement")], 9),
            20,
        )
        .unwrap();
        assert!(decode(&mapped, 8, 19).is_some());
        assert!(read(&path, 9, 20).is_some());
    }
    #[test]
    fn truncated_corrupt_and_obsolete_snapshots_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("index.snapshot.bin");
        write(&path, &fixture(), 19).unwrap();
        let bytes = std::fs::read(&path).unwrap();
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
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("index.snapshot.bin");
        write(&path, &fixture(), 19).unwrap();
        let original = std::fs::read(&path).unwrap();
        let mut position = 32;
        let count = get64(&original, &mut position).unwrap() as usize;
        let pool_size = get64(&original, &mut position).unwrap() as usize;
        let index_start = HEADER + count * RECORD + pool_size;
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
