"""One-shot, guarded development patch; removed before opening the PR."""
from pathlib import Path
import subprocess

path = Path("core/src/index_store.rs")
expected = "fca05fcbd0d48bdebe0bc2c79e1783b6e677a19b"
actual = subprocess.check_output(["git", "hash-object", str(path)], text=True).strip()
assert actual == expected, f"Refusing to patch unexpected index_store.rs: {actual}"
text = path.read_text()

def replace_once(old: str, new: str) -> None:
    global text
    assert text.count(old) == 1, f"Expected one patch anchor: {old[:100]!r}"
    text = text.replace(old, new, 1)

def replace_section(start: str, end: str, replacement: str) -> None:
    global text
    assert text.count(start) == 1 and text.count(end) == 1
    first, last = text.index(start), text.index(end)
    assert first < last
    text = text[:first] + replacement + text[last:]

text = '''#[path = "cache_journal.rs"]
mod cache_journal;
#[path = "change_reader.rs"]
mod change_reader;
#[path = "ordered_merge.rs"]
mod ordered_merge;

''' + text
replace_once("const SCHEMA_VERSION: i64 = 3;", "const SCHEMA_VERSION: i64 = 4;")
replace_once('''                  CREATE TRIGGER IF NOT EXISTS cache_change_limit AFTER INSERT ON cache_changes
                    WHEN (SELECT count(*) FROM cache_changes)>2000 BEGIN
                    INSERT INTO settings VALUES('cache_journal_overflow','true') ON CONFLICT(key) DO UPDATE SET value='true';
                    DELETE FROM cache_changes;
                  END;
''', "")
replace_once('''                  PRAGMA user_version=3;"#).map_err(|error| error.to_string())?;''', '''                  PRAGMA user_version=4;"#).map_err(|error| error.to_string())?;
                cache_journal::install(&transaction)?;''')
replace_section("    fn read_entries(&self, predicate: &str)", "    /// Return a complete delta only", '''    fn read_entries(&self, predicate: &str) -> Result<Vec<IndexedFile>, String> {
        let sql = format!(
            "SELECT f.id,{} FROM files f LEFT JOIN content c ON f.path=c.path WHERE f.accessible=1 AND ({predicate}) ORDER BY f.id",
            change_reader::COLUMNS
        );
        let mut statement = self.connection.prepare(&sql).map_err(|error| error.to_string())?;
        let rows = statement.query_map([], change_reader::decode_file).map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|error| error.to_string())
    }
''')
replace_section("    pub fn changes_since(", "    pub fn clear_changes(", '''    pub fn changes_since(
        &self,
        revision: u64,
        limit: usize,
    ) -> Result<Option<SnapshotDelta>, String> {
        if self.get("changes_base_revision", json!(null)).as_u64() != Some(revision) {
            return Ok(None);
        }
        change_reader::read(&self.connection, change_reader::Journal::Snapshot, limit)
    }
''')
replace_section("    pub fn cache_read(&self)", "    pub fn cache_is_dirty(&self)", '''    pub fn cache_read(&self) -> Option<(SearchSnapshot, u64)> {
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
        let (base, _) = crate::snapshot_cache::read(&self.cache_path, base_generation, base_revision)?;
        let mut snapshot = if revision == base_revision {
            base
        } else {
            let changes = change_reader::read(
                &transaction,
                change_reader::Journal::Cache,
                cache_journal::MAX_IDS,
            ).ok()??;
            // Startup has its own hard journal bound. Applying the publication
            // fraction here would reintroduce a 2,000-ID cliff on small indexes.
            SearchSnapshot::apply_changes(changes, generation, &base).ok()?
        };
        snapshot.generation = generation;
        snapshot.content_revision = self.get("content_revision", json!(0)).as_u64()?;
        transaction.commit().ok()?;
        Some((snapshot, generation))
    }
''')
replace_section("    pub(crate) fn from_changes_with_reason(", "        // Stable ordering preserves last-observation-wins", '''    pub(crate) fn from_changes_with_reason(
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
''')
replace_section("fn updated_order(", "fn order_ranks(", '''fn updated_order(
    previous: &[u32],
    changed: &RoaringBitmap,
    live: &RoaringBitmap,
    entries: &[Arc<IndexedFile>],
    order: &ResultOrder,
) -> Vec<u32> {
    let mut replacements: Vec<_> = changed.iter().filter(|slot| live.contains(*slot)).collect();
    replacements.sort_unstable_by(|a, b| order.compare(&entries[*a as usize], &entries[*b as usize]));
    let mut result = Vec::with_capacity(live.len() as usize);
    result.extend(previous.iter().copied().filter(|slot| !changed.contains(*slot)));
    ordered_merge::insert_sorted(&mut result, &replacements, |a, b| {
        order.compare(&entries[a as usize], &entries[b as usize])
    });
    result
}
''')
text += '''
#[cfg(test)]
#[path = "bounded_cache_tests.rs"]
mod bounded_cache_tests;
'''
path.write_text(text)
readme = Path("README.md")
text = readme.read_text()
old = "See [the core interface](core/README.md) and [localization conventions](docs/LOCALIZATION.md)."
assert text.count(old) == 1
readme.write_text(text.replace(old, "See [the core interface](core/README.md), [incremental-index design](docs/INCREMENTAL_INDEX.md), and [localization conventions](docs/LOCALIZATION.md)."))
readme = Path("core/README.md")
readme.write_text(readme.read_text() + '''
## Bounded incremental recovery

SQLite schema 4 retains up to 65,536 distinct cache-change IDs with a transactionally maintained counter. Startup reads only these keys in one ordered outer join, preserving deletions and denied entries, and replays them without rewriting the binary cache. V3/V4 prepared-cache formats are unchanged. The ordinary publication budget remains separate from startup recovery. See [the design and validation boundaries](../docs/INCREMENTAL_INDEX.md).
''')
