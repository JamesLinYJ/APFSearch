//! Exact postings for common metadata predicates. Unknown predicates fall back
//! to the full query evaluator; a missing supported key means an empty result.
use crate::entry_table::FileEntry;
use crate::query::{self, Field, Query, Term};
use roaring::RoaringBitmap;
use std::{borrow::Cow, collections::HashMap, hash::Hash, sync::Arc};

#[cfg(test)]
use std::io::{self, Read, Write};

type Postings<Key> = Arc<HashMap<Key, Arc<RoaringBitmap>>>;

#[derive(Clone, Default)]
pub(crate) struct MetadataPostings {
    name_bytes: Postings<u8>,
    name_pairs: Postings<[u8; 2]>,
    extensions: Postings<String>,
    directories: Arc<RoaringBitmap>,
    symlinks: Arc<RoaringBitmap>,
}
impl MetadataPostings {
    pub(crate) fn visit_postings<'a>(
        &'a self,
        mut visit: impl FnMut(u8, &'a [u8], &'a RoaringBitmap),
    ) {
        for (key, bitmap) in self.name_bytes.iter() {
            visit(1, std::slice::from_ref(key), bitmap);
        }
        for (key, bitmap) in self.name_pairs.iter() {
            visit(2, key, bitmap);
        }
        for (key, bitmap) in self.extensions.iter() {
            visit(3, key.as_bytes(), bitmap);
        }
        visit(4, &[], &self.directories);
        visit(5, &[], &self.symlinks);
    }
    pub(crate) fn insert_restored(
        &mut self,
        kind: u8,
        key: &[u8],
        bitmap: RoaringBitmap,
    ) -> Option<()> {
        let bitmap = Arc::new(bitmap);
        let duplicate = match kind {
            1 if key.len() == 1 => Arc::make_mut(&mut self.name_bytes)
                .insert(key[0], bitmap)
                .is_some(),
            2 if key.len() == 2 => Arc::make_mut(&mut self.name_pairs)
                .insert(key.try_into().ok()?, bitmap)
                .is_some(),
            3 if key.len() <= MAX_EXTENSION_BYTES => Arc::make_mut(&mut self.extensions)
                .insert(std::str::from_utf8(key).ok()?.to_owned(), bitmap)
                .is_some(),
            4 if key.is_empty() => {
                self.directories = bitmap;
                false
            }
            5 if key.is_empty() => {
                self.symlinks = bitmap;
                false
            }
            _ => return None,
        };
        (!duplicate).then_some(())
    }
    pub(crate) fn shares_storage(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.name_bytes, &other.name_bytes)
            && Arc::ptr_eq(&self.name_pairs, &other.name_pairs)
            && Arc::ptr_eq(&self.extensions, &other.extensions)
            && Arc::ptr_eq(&self.directories, &other.directories)
            && Arc::ptr_eq(&self.symlinks, &other.symlinks)
    }
    pub(crate) fn build<F: FileEntry>(files: impl IntoIterator<Item = (u32, F)>) -> Self {
        let mut postings = Self::default();
        for (slot, file) in files {
            postings.insert(slot, &file);
        }
        postings
    }
    pub(crate) fn insert(&mut self, slot: u32, file: &impl FileEntry) {
        let bytes = file.search_name().as_bytes();
        let singles = Arc::make_mut(&mut self.name_bytes);
        // Deduplicating bytes is cheap and allocation-free. Repeated bigrams
        // remain harmless: inserting an existing slot in a bitmap is idempotent.
        let mut seen = [0u64; 4];
        for &byte in bytes {
            let (word, mask) = (byte as usize / 64, 1u64 << (byte % 64));
            if seen[word] & mask == 0 {
                seen[word] |= mask;
                insert(singles, byte, slot);
            }
        }
        let pairs = Arc::make_mut(&mut self.name_pairs);
        for pair in bytes.windows(2) {
            insert(pairs, [pair[0], pair[1]], slot);
        }
        // Default ext: uses the same diacritic-insensitive folding as filename
        // search. folded_extension retains accents for natural sorting instead.
        let extension = extension_key(file);
        let extensions = Arc::make_mut(&mut self.extensions);
        if let Some(matches) = extensions.get_mut(extension.as_ref()) {
            Arc::make_mut(matches).insert(slot);
        } else {
            insert(extensions, extension.into_owned(), slot);
        }
        if file.is_dir() {
            Arc::make_mut(&mut self.directories).insert(slot);
        }
        if file.is_symlink() {
            Arc::make_mut(&mut self.symlinks).insert(slot);
        }
    }
    pub(crate) fn remove(&mut self, slot: u32, file: &impl FileEntry) {
        let bytes = file.search_name().as_bytes();
        let singles = Arc::make_mut(&mut self.name_bytes);
        for &byte in bytes {
            remove(singles, &byte, slot);
        }
        let pairs = Arc::make_mut(&mut self.name_pairs);
        for pair in bytes.windows(2) {
            remove(pairs, &[pair[0], pair[1]], slot);
        }
        let extension = extension_key(file);
        let extensions = Arc::make_mut(&mut self.extensions);
        if let Some(matches) = extensions.get_mut(extension.as_ref()) {
            let matches = Arc::make_mut(matches);
            matches.remove(slot);
            if matches.is_empty() {
                extensions.remove(extension.as_ref());
            }
        }
        if file.is_dir() {
            Arc::make_mut(&mut self.directories).remove(slot);
        }
        if file.is_symlink() {
            Arc::make_mut(&mut self.symlinks).remove(slot);
        }
    }
    /// Cache format owned by this module; callers version and checksum the
    /// complete snapshot. Keys are sorted to make equivalent postings stable.
    #[cfg(test)]
    pub(crate) fn write_to(&self, writer: &mut impl Write) -> io::Result<()> {
        write_map(&self.name_bytes, writer, |key, writer| {
            writer.write_all(&[*key])
        })?;
        write_map(&self.name_pairs, writer, |key, writer| {
            writer.write_all(key)
        })?;
        write_map(&self.extensions, writer, |key, writer| {
            if key.len() > MAX_EXTENSION_BYTES {
                return Err(invalid_cache("extension exceeds cache limit"));
            }
            write_count(writer, key.len())?;
            writer.write_all(key.as_bytes())
        })?;
        write_bitmap(writer, &self.directories)?;
        write_bitmap(writer, &self.symlinks)
    }
    #[cfg(test)]
    pub(crate) fn read_from(reader: &mut impl Read, entry_count: usize) -> io::Result<Self> {
        let name_bytes = read_map(reader, 256, entry_count, |reader| {
            Ok(read_array::<1>(reader)?[0])
        })?;
        let name_pairs = read_map(reader, 65_536, entry_count, |reader| {
            read_array::<2>(reader)
        })?;
        let extensions = read_map(reader, entry_count, entry_count, |reader| {
            let length = read_count(reader, MAX_EXTENSION_BYTES)?;
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes)?;
            String::from_utf8(bytes).map_err(|_| invalid_cache("extension is not UTF-8"))
        })?;
        let directories = Arc::new(read_bitmap(reader, entry_count)?);
        let symlinks = Arc::new(read_bitmap(reader, entry_count)?);
        Ok(Self {
            name_bytes,
            name_pairs,
            extensions,
            directories,
            symlinks,
        })
    }
    /// Some(bitmap) is exact within this snapshot's live-slot universe. None
    /// means at least one operand needs the ordinary matcher (not "no hits").
    pub(crate) fn supports(query: &Query) -> bool {
        match query {
            Query::All => true,
            Query::Term(Term::Text {
                field: Field::Name,
                needle,
                sensitive: false,
                ..
            }) => needle.len() <= 2,
            Query::Term(Term::Extension(_) | Term::IsDir(_) | Term::IsSymlink) => true,
            Query::And(queries) | Query::Or(queries) => queries.iter().all(Self::supports),
            Query::Not(query) => Self::supports(query),
            _ => false,
        }
    }
    pub(crate) fn exact(&self, query: &Query, live: &RoaringBitmap) -> Option<RoaringBitmap> {
        let mut result = match query {
            Query::All => live.clone(),
            Query::Term(Term::Text {
                field: Field::Name,
                needle,
                sensitive: false,
                ..
            }) => match needle.as_bytes() {
                [] => live.clone(),
                [byte] => lookup(&self.name_bytes, byte),
                [left, right] => lookup(&self.name_pairs, &[*left, *right]),
                _ => return None,
            },
            Query::Term(Term::Extension(extensions)) => {
                let mut result = RoaringBitmap::new();
                for extension in extensions {
                    if let Some(matches) = self.extensions.get(extension) {
                        result |= matches.as_ref();
                    }
                }
                result
            }
            Query::Term(Term::IsDir(true)) => self.directories.as_ref().clone(),
            Query::Term(Term::IsDir(false)) => live - self.directories.as_ref(),
            Query::Term(Term::IsSymlink) => self.symlinks.as_ref().clone(),
            Query::And(queries) => {
                let mut result = live.clone();
                for query in queries {
                    result &= self.exact(query, live)?;
                }
                result
            }
            Query::Or(queries) => {
                let mut result = RoaringBitmap::new();
                for query in queries {
                    result |= self.exact(query, live)?;
                }
                result
            }
            Query::Not(query) => live - self.exact(query, live)?,
            _ => return None,
        };
        result &= live;
        Some(result)
    }
}
fn extension_key(file: &impl FileEntry) -> Cow<'_, str> {
    if file.folded_extension().is_ascii() {
        Cow::Borrowed(file.folded_extension())
    } else {
        Cow::Owned(query::fold_search(file.extension()))
    }
}
const MAX_EXTENSION_BYTES: usize = 1024;
// Both Roaring portable encodings can be emitted by serialize_into. Validate
// their header before handing it to the library to bound container allocations.
#[cfg(test)]
const ROARING_ARRAY_BITMAP_COOKIE: u32 = 12_346;
#[cfg(test)]
const ROARING_RUN_COOKIE: u32 = 12_347;
#[cfg(test)]
fn invalid_cache(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
#[cfg(test)]
fn read_array<const N: usize>(reader: &mut (impl Read + ?Sized)) -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}
#[cfg(test)]
fn write_count(writer: &mut (impl Write + ?Sized), count: usize) -> io::Result<()> {
    let count = u32::try_from(count).map_err(|_| invalid_cache("cache count exceeds u32"))?;
    writer.write_all(&count.to_le_bytes())
}
#[cfg(test)]
fn read_count(reader: &mut (impl Read + ?Sized), maximum: usize) -> io::Result<usize> {
    let count = u32::from_le_bytes(read_array(reader)?) as usize;
    if count > maximum {
        return Err(invalid_cache("cache count exceeds limit"));
    }
    Ok(count)
}
#[cfg(test)]
fn write_bitmap(writer: &mut impl Write, bitmap: &RoaringBitmap) -> io::Result<()> {
    write_count(writer, bitmap.serialized_size())?;
    bitmap.serialize_into(writer)
}
#[cfg(test)]
fn read_bitmap(reader: &mut impl Read, entry_count: usize) -> io::Result<RoaringBitmap> {
    // Slots below entry_count can occupy only these high-16-bit containers.
    let max_containers = entry_count.div_ceil(65_536).min(65_536);
    // A valid run container can have at most 32768 separated singletons.
    // Ordinary arrays/bitsets need at most 8192 bytes, below this bound.
    let max_bytes = 8 + max_containers.div_ceil(8) + max_containers * (8 + 2 + 4 * 32_768);
    let length = read_count(reader, max_bytes)?;
    if length < 4 {
        return Err(invalid_cache("bitmap header is truncated"));
    }
    let mut header = [0; 8];
    reader.read_exact(&mut header[..4])?;
    let cookie = u32::from_le_bytes(header[..4].try_into().unwrap());
    let (containers, header_length) = if cookie == ROARING_ARRAY_BITMAP_COOKIE && length >= 8 {
        reader.read_exact(&mut header[4..])?;
        (
            u32::from_le_bytes(header[4..].try_into().unwrap()) as usize,
            8,
        )
    } else if cookie & 0xffff == ROARING_RUN_COOKIE {
        ((cookie >> 16) as usize + 1, 4)
    } else {
        return Err(invalid_cache("invalid bitmap cookie"));
    };
    if containers > max_containers {
        return Err(invalid_cache("invalid bitmap container count"));
    }
    let mut payload = reader.take((length - header_length) as u64);
    let bitmap = RoaringBitmap::deserialize_from(header[..header_length].chain(&mut payload))?;
    if payload.limit() != 0
        || bitmap
            .max()
            .is_some_and(|slot| slot as usize >= entry_count)
    {
        return Err(invalid_cache("invalid bitmap length or slot"));
    }
    Ok(bitmap)
}
#[cfg(test)]
fn write_map<Key: Ord>(
    map: &HashMap<Key, Arc<RoaringBitmap>>,
    writer: &mut impl Write,
    mut write_key: impl FnMut(&Key, &mut dyn Write) -> io::Result<()>,
) -> io::Result<()> {
    write_count(writer, map.len())?;
    let mut ordered: Vec<_> = map.iter().collect();
    ordered.sort_unstable_by_key(|(key, _)| *key);
    for (key, bitmap) in ordered {
        write_key(key, writer)?;
        write_bitmap(writer, bitmap)?;
    }
    Ok(())
}
#[cfg(test)]
fn read_map<Key: Eq + Hash>(
    reader: &mut impl Read,
    maximum_keys: usize,
    entry_count: usize,
    mut read_key: impl FnMut(&mut dyn Read) -> io::Result<Key>,
) -> io::Result<Postings<Key>> {
    let count = read_count(reader, maximum_keys)?;
    let mut map = HashMap::new();
    for _ in 0..count {
        let key = read_key(reader)?;
        if map.contains_key(&key) {
            return Err(invalid_cache("duplicate posting key"));
        }
        let bitmap = read_bitmap(reader, entry_count)?;
        if bitmap.is_empty() {
            return Err(invalid_cache("empty posting bitmap"));
        }
        map.insert(key, Arc::new(bitmap));
    }
    Ok(Arc::new(map))
}
fn insert<Key: Eq + Hash>(map: &mut HashMap<Key, Arc<RoaringBitmap>>, key: Key, slot: u32) {
    Arc::make_mut(map.entry(key).or_default()).insert(slot);
}
fn remove<Key: Eq + Hash>(map: &mut HashMap<Key, Arc<RoaringBitmap>>, key: &Key, slot: u32) {
    if let Some(matches) = map.get_mut(key) {
        let matches = Arc::make_mut(matches);
        matches.remove(slot);
        if matches.is_empty() {
            map.remove(key);
        }
    }
}
fn lookup<Key: Eq + Hash>(map: &HashMap<Key, Arc<RoaringBitmap>>, key: &Key) -> RoaringBitmap {
    map.get(key)
        .map(|matches| matches.as_ref().clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index_store::IndexedFile;
    use serde_json::json;
    fn file(name: &str, extension: &str, is_dir: bool) -> IndexedFile {
        let mut file: IndexedFile = serde_json::from_value(json!({
            "id":1,"path":format!("/fixture/{name}"),"name":name,"extension":extension,
            "size":0,"modified":0,"created":0,"changed":0,"is_dir":is_dir,
            "is_symlink":name=="alias","file_id":1,"parent_id":0,"volume_id":"fixture","flags":0
        }))
        .unwrap();
        file.prepare();
        file
    }
    fn parsed(text: &str) -> Query {
        query::parse(text, &HashMap::new()).unwrap()
    }
    fn expected(files: &[IndexedFile], query: &Query, live: &RoaringBitmap) -> RoaringBitmap {
        live.iter()
            .filter(|slot| query.matches(&files[*slot as usize], None).unwrap())
            .collect()
    }
    #[test]
    fn byte_and_pair_postings_match_unicode_and_boolean_evaluation_exactly() {
        let files = [
            file("aaaa.txt", "txt", false),
            file("Straße.TXT", "TXT", false),
            file("中文μé", "é", false),
            file("目录", "", true),
            file("alias", "", false),
            file("other.e", "e", false),
        ];
        let postings = MetadataPostings::build(
            files
                .iter()
                .enumerate()
                .map(|(slot, file)| (slot as u32, file)),
        );
        let live: RoaringBitmap = (0..files.len() as u32).collect();
        for text in [
            "",
            "a",
            "aa",
            "ss",
            "μ",
            "é",
            "zz",
            "!a",
            "a | μ",
            "<a | μ> !ss",
            "!<a | μ>",
            "ext:TXT",
            "ext:é",
            "ext:txt;é",
            "folder:",
            "file:",
            "symlink:",
            "file:!a",
        ] {
            let query = parsed(text);
            assert_eq!(
                postings.exact(&query, &live),
                Some(expected(&files, &query, &live)),
                "{text}"
            );
        }
        for text in [
            "abc",
            "中",
            "case:a",
            "diacritics:e",
            "ww:a",
            "regex:a",
            "path:a",
            "content:a",
            "size:>1",
            "a | content:x",
            "!<a content:x>",
        ] {
            assert!(postings.exact(&parsed(text), &live).is_none(), "{text}");
        }
    }
    #[test]
    fn updates_removals_and_tombstones_do_not_modify_a_retained_snapshot() {
        let mut files = vec![
            file("aaa.txt", "txt", false),
            file("beta.csv", "csv", false),
            file("目录", "", true),
        ];
        let live: RoaringBitmap = (0..files.len() as u32).collect();
        let mut postings = MetadataPostings::build(
            files
                .iter()
                .enumerate()
                .map(|(slot, file)| (slot as u32, file)),
        );
        let retained = postings.clone();
        let old_a = retained.exact(&parsed("a"), &live).unwrap();
        postings.remove(0, &files[0]);
        files[0] = file("μé.CSV", "CSV", false);
        postings.insert(0, &files[0]);
        postings.remove(1, &files[1]);
        let mut current_live = live.clone();
        current_live.remove(1);
        for text in [
            "a", "aa", "μ", "ext:csv", "ext:txt", "!a", "folder:", "file:",
        ] {
            let query = parsed(text);
            assert_eq!(
                postings.exact(&query, &current_live),
                Some(expected(&files, &query, &current_live)),
                "{text}"
            );
        }
        assert_eq!(retained.exact(&parsed("a"), &live), Some(old_a));
        let mut tombstoned = live.clone();
        tombstoned.remove(0);
        assert!(
            !retained
                .exact(&parsed("a"), &tombstoned)
                .unwrap()
                .contains(0)
        );
        assert!(
            !retained
                .exact(&parsed("!zz"), &tombstoned)
                .unwrap()
                .contains(0)
        );
    }
    #[test]
    fn cached_postings_round_trip_and_reject_malformed_counts_keys_and_slots() {
        let files = [
            file("aaaa.txt", "txt", false),
            file("Straße.TXT", "TXT", false),
            file("中文μé", "é", false),
            file("目录", "", true),
            file("alias", "", false),
        ];
        let postings = MetadataPostings::build(
            files
                .iter()
                .enumerate()
                .map(|(slot, file)| (slot as u32, file)),
        );
        let mut bytes = Vec::new();
        postings.write_to(&mut bytes).unwrap();
        let mut input = io::Cursor::new(bytes.as_slice());
        let restored = MetadataPostings::read_from(&mut input, files.len()).unwrap();
        assert_eq!(input.position() as usize, bytes.len());
        let mut rewritten = Vec::new();
        restored.write_to(&mut rewritten).unwrap();
        assert_eq!(bytes, rewritten, "cache encoding must be deterministic");
        let live: RoaringBitmap = (0..files.len() as u32).collect();
        for text in [
            "a",
            "aa",
            "μ",
            "!a",
            "a | μ",
            "ext:txt;é",
            "folder:",
            "symlink:",
        ] {
            let query = parsed(text);
            assert_eq!(
                restored.exact(&query, &live),
                Some(expected(&files, &query, &live))
            );
        }
        for truncated_at in 0..bytes.len() {
            assert!(MetadataPostings::read_from(&mut &bytes[..truncated_at], files.len()).is_err());
        }
        let mut too_many_keys = bytes.clone();
        too_many_keys[..4].copy_from_slice(&257u32.to_le_bytes());
        assert!(MetadataPostings::read_from(&mut too_many_keys.as_slice(), files.len()).is_err());
        assert!(
            MetadataPostings::read_from(&mut bytes.as_slice(), 1).is_err(),
            "out of range slots"
        );

        let mut duplicate_keys = Vec::new();
        write_count(&mut duplicate_keys, 2).unwrap();
        for _ in 0..2 {
            duplicate_keys.push(b'a');
            write_bitmap(&mut duplicate_keys, &[0].into_iter().collect()).unwrap();
        }
        assert!(MetadataPostings::read_from(&mut duplicate_keys.as_slice(), 1).is_err());
        for invalid_length in [MAX_EXTENSION_BYTES as u32 + 1, u32::MAX] {
            let mut oversized = Vec::new();
            for count in [0, 0, 1, invalid_length] {
                oversized.extend(count.to_le_bytes());
            }
            assert!(MetadataPostings::read_from(&mut oversized.as_slice(), 1).is_err());
        }
        let mut invalid_utf8 = Vec::new();
        for count in [0u32, 0, 1, 1] {
            invalid_utf8.extend(count.to_le_bytes());
        }
        invalid_utf8.push(0xff);
        assert!(MetadataPostings::read_from(&mut invalid_utf8.as_slice(), 1).is_err());

        let mut run_bitmap: RoaringBitmap = (0..70_000).collect();
        assert!(
            run_bitmap.optimize(),
            "fixture exercises portable run containers"
        );
        let mut run_bytes = Vec::new();
        write_bitmap(&mut run_bytes, &run_bitmap).unwrap();
        assert_eq!(
            read_bitmap(&mut run_bytes.as_slice(), 70_000).unwrap(),
            run_bitmap
        );

        let mut invalid_container_count = Vec::new();
        for number in [8u32, ROARING_ARRAY_BITMAP_COOKIE, 65_536] {
            invalid_container_count.extend(number.to_le_bytes());
        }
        assert!(read_bitmap(&mut invalid_container_count.as_slice(), 1).is_err());
    }
}
