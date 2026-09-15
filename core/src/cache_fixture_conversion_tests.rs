//! Opt-in conversion of immutable v1 BENCHMARK fixtures for same-corpus A/B
//! comparisons. Not linked into production: runtime v1 caches recover from
//! SQLite instead, without a compatibility decoder.
use super::*;

struct FixturePool<'a>(&'a [u8]);
impl<'a> FixturePool<'a> {
    fn text(&self, row: &[u8], position: &mut usize) -> Option<&'a str> {
        let start = usize::try_from(get64(row, position)?).ok()?;
        let length = u32::from_le_bytes(
            row.get(*position..position.checked_add(4)?)?
                .try_into()
                .ok()?,
        ) as usize;
        *position += 4;
        std::str::from_utf8(self.0.get(start..start.checked_add(length)?)?).ok()
    }
    fn row(&self, row: &[u8]) -> Option<IndexedFile> {
        let mut p = 0;
        let id = get64(row, &mut p)? as i64;
        let parent = self.text(row, &mut p)?;
        let name = self.text(row, &mut p)?;
        let extension = self.text(row, &mut p)?;
        let volume_id = self.text(row, &mut p)?;
        let properties = self.text(row, &mut p)?;
        let override_path = self.text(row, &mut p)?;
        let folded_name = self.text(row, &mut p)?;
        let folded_extension = self.text(row, &mut p)?;
        let folded_path = self.text(row, &mut p)?;
        let search_name = self.text(row, &mut p)?;
        let search_path = self.text(row, &mut p)?;
        let size = get64(row, &mut p)?;
        let modified = get64(row, &mut p)? as i64;
        let created = get64(row, &mut p)? as i64;
        let changed = get64(row, &mut p)? as i64;
        let modified_ns = get64(row, &mut p)? as i64;
        let changed_ns = get64(row, &mut p)? as i64;
        let file_id = get64(row, &mut p)?;
        let parent_id = get64(row, &mut p)?;
        let flags = u32::from_le_bytes(row.get(p..p + 4)?.try_into().ok()?);
        let state = u32::from_le_bytes(row.get(p + 4..p + 8)?.try_into().ok()?);
        if state & !7 != 0 {
            return None;
        }
        let path = if override_path.is_empty() {
            Path::new(parent)
                .join(name)
                .into_os_string()
                .into_string()
                .ok()?
        } else {
            override_path.into()
        };
        Some(IndexedFile {
            id,
            path,
            name: name.into(),
            extension: extension.into(),
            volume_id: volume_id.into(),
            properties: serde_json::from_str(properties).ok()?,
            parent: parent.into(),
            folded_name: folded_name.into(),
            folded_extension: folded_extension.into(),
            folded_path: folded_path.into(),
            search_name: search_name.into(),
            search_path: search_path.into(),
            size,
            modified,
            created,
            changed,
            modified_ns,
            changed_ns,
            file_id,
            parent_id,
            flags,
            is_dir: state & 1 != 0,
            is_symlink: state & 2 != 0,
            content_indexed: state & 4 != 0,
        })
    }
}

#[test]
#[ignore = "Explicit immutable v1 benchmark input and new disposable output required"]
fn convert_immutable_benchmark_fixture() {
    let input = std::env::var_os("APFSEARCH_V1_FIXTURE")
        .expect("Provide an immutable disposable v1 fixture");
    let output = PathBuf::from(
        std::env::var_os("APFSEARCH_V2_FIXTURE").expect("Provide a new disposable output"),
    );
    assert!(
        !output.exists(),
        "Fixture conversion never overwrites an existing output"
    );
    let file = File::open(input).unwrap();
    // SAFETY: the explicitly supplied benchmark fixture is immutable.
    let mapping = unsafe { memmap2::Mmap::map(&file).unwrap() };
    assert_eq!(&mapping[..8], b"APFIDX01");
    let mut p = 8;
    let generation = get64(&mapping, &mut p).unwrap();
    let revision = get64(&mapping, &mut p).unwrap();
    let content_revision = get64(&mapping, &mut p).unwrap();
    let count = usize::try_from(get64(&mapping, &mut p).unwrap()).unwrap();
    let pool_bytes = usize::try_from(get64(&mapping, &mut p).unwrap()).unwrap();
    let index_bytes = usize::try_from(get64(&mapping, &mut p).unwrap()).unwrap();
    let pool_start = HEADER.checked_add(count.checked_mul(212).unwrap()).unwrap();
    let index_start = pool_start.checked_add(pool_bytes).unwrap();
    let end = index_start.checked_add(index_bytes).unwrap();
    assert_eq!(end.checked_add(32), Some(mapping.len()));
    assert_eq!(
        checksum(&mapping[..HEADER], &payload_digest(&mapping[HEADER..end])).as_bytes(),
        &mapping[end..]
    );
    let pool = FixturePool(&mapping[pool_start..index_start]);
    let (records, remainder) = mapping[HEADER..pool_start].as_chunks::<212>();
    assert!(remainder.is_empty());
    let rows = records.iter().map(|row| {
        pool.row(row)
            .ok_or_else(|| "Invalid benchmark record".into())
    });
    let entries = crate::entry_table::EntryTable::from_rows(rows).unwrap();
    let snapshot = restore_indexes(
        entries,
        &mapping[index_start..end],
        generation,
        content_revision,
        |_, _| {},
    )
    .unwrap();
    let storage = snapshot.entries.storage_metrics();
    write(&output, &snapshot, revision).unwrap();
    println!(
        "{}",
        json!({"scope":"One-time benchmark fixture conversion; no SQLite or filesystem traversal","entries":snapshot.len(),"storage":storage})
    );
}
