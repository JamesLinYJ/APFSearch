//! Immutable cache sections and an atomically published manifest. Readers and
//! writers serialize only publication/collection, never query evaluation.
use super::*;
use std::collections::HashSet;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::sync::{Mutex, OnceLock, Weak};
#[path = "index_sections.rs"]
mod index_sections;

const MANIFEST_MAGIC: &[u8; 8] = b"APFMAP03";
const DESCRIPTOR_BYTES: usize = 40;

pub(crate) struct Section {
    directory: PathBuf,
    hash: [u8; 32],
    bytes: u64,
    dependencies: OnceLock<Vec<Arc<Section>>>,
    verified_identity: Mutex<Option<SectionIdentity>>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct SectionIdentity {
    device: u64,
    inode: u64,
    length: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}
impl SectionIdentity {
    fn read(metadata: &std::fs::Metadata) -> Option<Self> {
        metadata.is_file().then(|| Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }
}
impl Section {
    fn name(&self) -> String {
        format!("{}.segment", blake3::Hash::from_bytes(self.hash).to_hex())
    }
    fn path(&self) -> PathBuf {
        self.directory.join(self.name())
    }
}

#[derive(Default)]
struct Publications {
    // Weak pins prevent collection of sections still owned by older snapshots.
    // The current manifest is protected separately even after its reader exits.
    sections: HashMap<PathBuf, Weak<Section>>,
}
fn publications() -> &'static Mutex<Publications> {
    static PUBLICATIONS: OnceLock<Mutex<Publications>> = OnceLock::new();
    PUBLICATIONS.get_or_init(|| Mutex::new(Publications::default()))
}
impl Publications {
    fn pin(&mut self, section: Section) -> Arc<Section> {
        let path = section.path();
        if let Some(existing) = self.sections.get(&path).and_then(Weak::upgrade) {
            return existing;
        }
        let section = Arc::new(section);
        self.sections.insert(path, Arc::downgrade(&section));
        section
    }
    fn collect(&mut self, directory: &Path, current: &[Arc<Section>]) {
        let retained: HashSet<_> = current.iter().map(|section| section.path()).collect();
        let Ok(files) = std::fs::read_dir(directory) else {
            return;
        };
        for file in files.flatten() {
            let path = file.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(hash) = name.strip_suffix(".segment") else {
                continue;
            };
            if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                continue;
            }
            if retained.contains(&path)
                || self
                    .sections
                    .get(&path)
                    .is_some_and(|section| section.strong_count() != 0)
            {
                continue;
            }
            let _ = std::fs::remove_file(&path);
        }
        self.sections
            .retain(|_, section| section.strong_count() != 0);
    }
}

fn temporary(path: &Path) -> io::Result<(TemporaryCache, File)> {
    let guard = TemporaryCache(path.with_extension(format!(
        "{}-{}.tmp",
        std::process::id(),
        TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&guard.0)?;
    Ok((guard, file))
}
fn write_section(
    directory: &Path,
    publications: &mut Publications,
    encode: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> io::Result<Arc<Section>> {
    let (guard, file) = temporary(&directory.join("section"))?;
    let mut output = BufWriter::new(file);
    let mut hash = blake3::Hasher::new();
    let bytes = {
        let mut writer = PayloadWriter {
            output: &mut output,
            hash: &mut hash,
            bytes: 0,
        };
        encode(&mut writer)?;
        writer.bytes
    };
    output.flush()?;
    output.get_ref().sync_all()?;
    drop(output);
    let section = publications.pin(Section {
        directory: directory.into(),
        hash: *hash.finalize().as_bytes(),
        bytes,
        dependencies: OnceLock::new(),
        verified_identity: Mutex::new(None),
    });
    // Rename never modifies the old inode, including when identical contents
    // already exist. No active mapping can be truncated by publication.
    std::fs::rename(&guard.0, section.path())?;
    *section
        .verified_identity
        .lock()
        .map_err(|_| io::Error::other("Cache identity lock poisoned"))? =
        SectionIdentity::read(&std::fs::symlink_metadata(section.path())?);
    Ok(section)
}
fn reusable(section: &Arc<Section>, directory: &Path) -> bool {
    section.directory == directory
        && std::fs::symlink_metadata(section.path())
            .ok()
            .and_then(|metadata| SectionIdentity::read(&metadata))
            .is_some_and(|identity| {
                identity.length == section.bytes
                    && section
                        .verified_identity
                        .lock()
                        .is_ok_and(|verified| *verified == Some(identity))
            })
        && section
            .dependencies
            .get()
            .is_none_or(|parts| parts.iter().all(|part| reusable(part, directory)))
}

pub(super) fn write(path: &Path, snapshot: &SearchSnapshot, revision: u64) -> io::Result<()> {
    let mut publications = publications()
        .lock()
        .map_err(|_| io::Error::other("Cache publication lock poisoned"))?;
    let directory = path.with_extension("sections");
    match std::fs::symlink_metadata(&directory) {
        Ok(metadata) if !metadata.is_dir() => {
            return Err(io::Error::other("Cache sections must be a directory"));
        }
        Ok(_) => (),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new().mode(0o700).create(&directory)?
        }
        Err(error) => return Err(error),
    }
    let directory = directory.canonicalize()?;
    let mut sections = Vec::with_capacity(snapshot.entries.chunks.len() + 1);
    for chunk in &snapshot.entries.chunks {
        let section = match chunk
            .section
            .get()
            .filter(|section| reusable(section, &directory))
        {
            Some(section) => section.clone(),
            None => {
                let _cpu = crate::cpu_executor::enter_background();
                let bytes = encode_chunk(chunk)?;
                let section = write_section(&directory, &mut publications, |output| {
                    output.write_all(&bytes)
                })?;
                let _ = chunk.section.set(section.clone());
                section
            }
        };
        sections.push(section);
    }
    let index = match snapshot
        .index_section
        .get()
        .filter(|section| reusable(section, &directory))
    {
        Some(section) => section.clone(),
        None => {
            let section = index_sections::write(&directory, &mut publications, snapshot)?;
            let _ = snapshot.index_section.set(section.clone());
            section
        }
    };
    sections.push(index);
    let mut manifest = vec![0u8; HEADER];
    manifest[..8].copy_from_slice(MANIFEST_MAGIC);
    let mut position = 8;
    for value in [
        snapshot.generation,
        revision,
        snapshot.content_revision,
        snapshot.entries.len() as u64,
        snapshot.entries.chunks.len() as u64,
        0,
    ] {
        push64(&mut manifest, &mut position, value);
    }
    for section in &sections {
        manifest.extend_from_slice(&section.hash);
        manifest.extend_from_slice(&section.bytes.to_le_bytes());
    }
    let digest = checksum(&manifest[..HEADER], &payload_digest(&manifest[HEADER..]));
    manifest.extend_from_slice(digest.as_bytes());
    File::open(&directory)?.sync_all()?;
    let (guard, mut output) = temporary(path)?;
    output.write_all(&manifest)?;
    output.sync_all()?;
    drop(output);
    std::fs::rename(&guard.0, path)?;
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    publications.collect(&directory, &sections);
    Ok(())
}

fn map_section(section: &Section) -> Option<Arc<memmap2::Mmap>> {
    // Refuse symlinks rather than letting a manifest redirect cache reads.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(section.path())
        .ok()?;
    let identity = SectionIdentity::read(&file.metadata().ok()?)?;
    if identity.length != section.bytes {
        return None;
    }
    // SAFETY: sections are published by rename, never truncated or modified.
    // The owning Arc remains pinned for every reader of its validated columns.
    let mapping = Arc::new(unsafe { memmap2::Mmap::map(&file).ok()? });
    if payload_digest(&mapping).as_bytes() != &section.hash
        || SectionIdentity::read(&file.metadata().ok()?)? != identity
    {
        return None;
    }
    *section.verified_identity.lock().ok()? = Some(identity);
    Some(mapping)
}
pub(super) fn read(path: &Path, generation: u64, revision: u64) -> Option<SearchSnapshot> {
    let mut publications = publications().lock().ok()?;
    let file = File::open(path).ok()?;
    let maximum = (u32::MAX as usize)
        .div_ceil(crate::entry_table::CHUNK_LENGTH)
        .checked_add(1)?
        .checked_mul(DESCRIPTOR_BYTES)?
        .checked_add(HEADER + 32)?;
    if file.metadata().ok()?.len() > maximum as u64 {
        return None;
    }
    let mut manifest = Vec::new();
    file.take(maximum as u64 + 1)
        .read_to_end(&mut manifest)
        .ok()?;
    if manifest.get(..8)? != MANIFEST_MAGIC {
        return None;
    }
    let mut position = 8;
    if get64(&manifest, &mut position)? != generation
        || get64(&manifest, &mut position)? != revision
    {
        return None;
    }
    let content_revision = get64(&manifest, &mut position)?;
    let count = usize::try_from(get64(&manifest, &mut position)?).ok()?;
    let chunks = usize::try_from(get64(&manifest, &mut position)?).ok()?;
    if count > u32::MAX as usize
        || chunks != count.div_ceil(crate::entry_table::CHUNK_LENGTH)
        || get64(&manifest, &mut position)? != 0
    {
        return None;
    }
    let end = HEADER.checked_add(chunks.checked_add(1)?.checked_mul(DESCRIPTOR_BYTES)?)?;
    if manifest.len() != end.checked_add(32)?
        || checksum(
            &manifest[..HEADER],
            &payload_digest(manifest.get(HEADER..end)?),
        )
        .as_bytes()
            != manifest.get(end..)?
    {
        return None;
    }
    let directory = path.with_extension("sections");
    if !std::fs::symlink_metadata(&directory).ok()?.is_dir() {
        return None;
    }
    let directory = directory.canonicalize().ok()?;
    let mut sections = Vec::with_capacity(chunks);
    for _ in 0..chunks {
        sections.push(read_descriptor(
            &manifest,
            &mut position,
            &directory,
            &mut publications,
        )?);
    }
    // At most two filesystem workers. Each block's CPU admission is released
    // before the next block, so pending foreground work can take precedence.
    let blocks = std::thread::scope(|scope| {
        let width = sections.len().div_ceil(2).max(1);
        let tasks: Vec<_> = sections
            .chunks(width)
            .map(|part| {
                scope.spawn(move || {
                    part.iter()
                        .map(|section| {
                            let _cpu = crate::cpu_executor::enter_background();
                            let mapping = map_section(section)?;
                            let block = decode_chunk(&mapping, 0, mapping.len())?;
                            let _ = block.section.set(section.clone());
                            Some(block)
                        })
                        .collect::<Option<Vec<_>>>()
                })
            })
            .collect();
        let mut blocks = Vec::with_capacity(chunks);
        for task in tasks {
            blocks.extend(task.join().ok()??);
        }
        Some(blocks)
    })?;
    let entries = crate::entry_table::EntryTable::from_chunks(blocks)?;
    if entries.len() != count {
        return None;
    }
    let section = read_descriptor(&manifest, &mut position, &directory, &mut publications)?;
    let mapping = map_section(&section)?;
    let snapshot = index_sections::read(
        &section,
        &mapping,
        &mut publications,
        entries,
        generation,
        content_revision,
    )?;
    let _ = snapshot.index_section.set(section);
    Some(snapshot)
}
fn read_descriptor(
    bytes: &[u8],
    position: &mut usize,
    directory: &Path,
    publications: &mut Publications,
) -> Option<Arc<Section>> {
    let hash = bytes
        .get(*position..position.checked_add(32)?)?
        .try_into()
        .ok()?;
    *position += 32;
    let bytes = get64(bytes, position)?;
    let section = publications.pin(Section {
        directory: directory.into(),
        hash,
        bytes,
        dependencies: OnceLock::new(),
        verified_identity: Mutex::new(None),
    });
    (section.bytes == bytes).then_some(section)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry_table::FileEntry;
    use std::os::unix::fs::MetadataExt;

    fn fixture() -> SearchSnapshot {
        SearchSnapshot::from_rows((0..4097).map(|id|Ok(serde_json::from_value(json!({
            "id":id+1,"path":format!("/fixture/report-{id}.txt"),"name":format!("report-{id}.txt"),
            "extension":"txt","volume_id":"fixture","size":7,"modified":1,"created":1,
            "changed":1,"file_id":id+1,"parent_id":0,"flags":0,"properties":{},"is_dir":false,"is_symlink":false
        })).unwrap())),1).unwrap()
    }
    #[test]
    fn changed_columns_publish_one_block_and_pin_old_reader_sections() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cache.bin");
        let original = fixture();
        write(&path, &original, 1).unwrap();
        let reader = read(&path, 1, 1).unwrap();
        let old_section = reader.entries.chunks[0].section.get().unwrap().path();
        let unchanged = reader.entries.chunks[1].section.get().unwrap().path();
        let index = reader.index_section.get().unwrap().path();
        let inodes = [&unchanged, &index].map(|path| std::fs::metadata(path).unwrap().ino());
        let mut replacement = reader.entries.at(0).to_owned_file();
        replacement.size = 99;
        let updated =
            SearchSnapshot::from_changes(vec![(replacement.id, Some(replacement))], 2, &reader)
                .unwrap();
        write(&path, &updated, 2).unwrap();
        assert_eq!(updated.index_section.get().unwrap().path(), index);
        assert_eq!(
            [&unchanged, &index].map(|path| std::fs::metadata(path).unwrap().ino()),
            inodes
        );
        assert_ne!(
            updated.entries.chunks[0].section.get().unwrap().path(),
            old_section
        );
        assert!(old_section.exists());
        assert_eq!(reader.entries.at(0).size(), 7);
        assert_eq!(read(&path, 2, 2).unwrap().entries.at(0).size(), 99);
        drop(original);
        drop(reader);
        write(&path, &updated, 2).unwrap();
        assert!(!old_section.exists());
        assert!(unchanged.exists());
        assert!(index.exists());
    }
    #[test]
    fn interrupted_publication_and_bad_sections_leave_readers_safe() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cache.bin");
        let snapshot = fixture();
        write(&path, &snapshot, 1).unwrap();
        let reader = read(&path, 1, 1).unwrap();
        let section_directory = path.with_extension("sections").canonicalize().unwrap();
        let orphan = {
            let mut publications = publications().lock().unwrap();
            write_section(&section_directory, &mut publications, |out| {
                out.write_all(b"unpublished")
            })
            .unwrap()
        };
        let orphan_path = orphan.path();
        drop(orphan);
        assert!(read(&path, 1, 1).is_some());
        write(&path, &snapshot, 1).unwrap();
        assert!(!orphan_path.exists());
        let section = reader.entries.chunks[0].section.get().unwrap().path();
        let mut bytes = std::fs::read(&section).unwrap();
        bytes[0] ^= 1;
        let damaged = directory.path().join("damaged");
        std::fs::write(&damaged, bytes).unwrap();
        std::fs::rename(&damaged, &section).unwrap();
        assert!(read(&path, 1, 1).is_none());
        assert_eq!(reader.entries.at(0).name(), "report-0.txt");
        assert!(read(&path, 1, 2).is_none());
        write(&path, &snapshot, 1).unwrap();
        assert!(read(&path, 1, 1).is_some());
        // A valid root descriptor cannot hide a replaced secondary section.
        let part = reader
            .index_section
            .get()
            .unwrap()
            .dependencies
            .get()
            .unwrap()[0]
            .path();
        let mut bytes = std::fs::read(&part).unwrap();
        bytes[0] ^= 1;
        std::fs::write(&damaged, bytes).unwrap();
        std::fs::rename(&damaged, &part).unwrap();
        assert!(read(&path, 1, 1).is_none());
        write(&path, &snapshot, 1).unwrap();
        assert!(read(&path, 1, 1).is_some());
        assert_eq!(reader.entries.at(0).name(), "report-0.txt");
    }
    #[test]
    fn manifest_bounds_and_signed_descriptor_corruption_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cache.bin");
        let snapshot = fixture();
        write(&path, &snapshot, 1).unwrap();
        let original = std::fs::read(&path).unwrap();
        for length in 0..original.len() {
            std::fs::write(&path, &original[..length]).unwrap();
            assert!(read(&path, 1, 1).is_none());
        }
        for offset in [32, 40, 48, HEADER + 32] {
            let mut invalid = original.clone();
            invalid[offset..offset + 8].copy_from_slice(&u64::MAX.to_le_bytes());
            let end = invalid.len() - 32;
            let digest = checksum(&invalid[..HEADER], &payload_digest(&invalid[HEADER..end]));
            invalid[end..].copy_from_slice(digest.as_bytes());
            std::fs::write(&path, invalid).unwrap();
            assert!(read(&path, 1, 1).is_none());
        }
        std::fs::write(&path, original).unwrap();
        assert!(read(&path, 1, 1).is_some());
    }
    #[test]
    fn mapped_text_checks_utf8_and_reference_ranges_before_exposing_views() {
        let row = fixture().entries.at(0).to_owned_file();
        let snapshot = SearchSnapshot::new(vec![row], 1);
        let chunk = &snapshot.entries.chunks[0];
        let original = encode_chunk(chunk).unwrap();
        let decode = |bytes: &[u8]| {
            let mut mapping = memmap2::MmapMut::map_anon(bytes.len()).unwrap();
            mapping.copy_from_slice(bytes);
            let mapping = Arc::new(mapping.make_read_only().unwrap());
            decode_chunk(&mapping, 0, mapping.len())
        };
        assert!(decode(&original).is_some());
        let mut invalid = original.clone();
        let text_end = invalid.iter().rposition(|byte| *byte != 0).unwrap() + 1;
        invalid[text_end - chunk.text.text().len()] = 0xff;
        assert!(decode(&invalid).is_none());
        let mut invalid = original;
        // Skip the ten independently encoded integer columns, then corrupt
        // the first path fragment's text offset while retaining valid framing.
        let mut offset = 32usize;
        for _ in 0..10 {
            offset = offset.next_multiple_of(8);
            let width =
                u64::from_le_bytes(invalid[offset + 8..offset + 16].try_into().unwrap()) as usize;
            offset += 16 + width;
        }
        offset = offset.next_multiple_of(8) + 8; // Skip the reference-column alias tag.
        invalid[offset..offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode(&invalid).is_none());
    }
}

#[cfg(test)]
mod secondary_tests {
    use super::*;
    fn row(id: i64, name: &str) -> IndexedFile {
        serde_json::from_value(json!({"id":id,"path":format!("/fixture/{name}"),"name":name,"extension":"txt","volume_id":"fixture","size":10,"modified":1,"created":1,"changed":1,"file_id":id,"parent_id":0,"flags":0,"is_dir":false,"is_symlink":false,"content_indexed":false,"properties":{}})).unwrap()
    }
    #[test]
    fn changed_postings_publish_only_changed_shards_and_pin_old_readers() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("index.snapshot.bin");
        let original = SearchSnapshot::new(
            (1..100)
                .map(|id| row(id, &format!("item-{id}.txt")))
                .collect(),
            1,
        );
        write(&path, &original, 1).unwrap();
        let reader = read(&path, 1, 1).unwrap();
        let old_root = reader.index_section.get().unwrap();
        let old_parts = old_root.dependencies.get().unwrap();
        let updated =
            SearchSnapshot::from_changes(vec![(7, Some(row(7, "different.txt")))], 2, &original)
                .unwrap();
        write(&path, &updated, 2).unwrap();
        let new_parts = updated
            .index_section
            .get()
            .unwrap()
            .dependencies
            .get()
            .unwrap();
        let shared = old_parts
            .iter()
            .zip(new_parts)
            .filter(|(a, b)| Arc::ptr_eq(a, b))
            .count();
        assert!(shared > old_parts.len() / 2);
        assert!(old_parts.iter().all(|section| section.path().exists()));
        assert_eq!(reader.entries.at(6).name(), "item-7.txt");
        assert_eq!(
            read(&path, 2, 2).unwrap().entries.at(6).name(),
            "different.txt"
        );
        let obsolete: Vec<_> = old_parts
            .iter()
            .filter(|part| !new_parts.iter().any(|new| new.hash == part.hash))
            .map(|part| part.path())
            .collect();
        assert!(!obsolete.is_empty());
        drop(reader);
        drop(original);
        write(&path, &updated, 2).unwrap();
        assert!(obsolete.iter().all(|path| !path.exists()));
    }
}
