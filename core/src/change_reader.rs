//! Read only journaled primary keys, including tombstones, in one ordered join.
use super::{IndexedFile, SnapshotDelta};
use rusqlite::{Connection, Row, types::ValueRef};
use serde_json::json;

pub(super) const COLUMNS: &str = "f.path,f.name,f.extension,f.size,f.modified,f.created,f.changed,f.is_dir,f.is_symlink,f.file_id,f.parent_id,f.volume_id,f.flags,COALESCE(c.properties,'{}'),f.modified_ns,f.changed_ns,c.path IS NOT NULL";

pub(super) fn decode_file(row: &Row<'_>) -> rusqlite::Result<IndexedFile> {
    Ok(IndexedFile {
        id: row.get(0)?,
        path: row.get(1)?,
        name: row.get(2)?,
        extension: row.get_ref(3)?.as_str()?.into(),
        size: row.get::<_, i64>(4)? as u64,
        modified: row.get(5)?,
        created: row.get(6)?,
        changed: row.get(7)?,
        is_dir: row.get(8)?,
        is_symlink: row.get(9)?,
        file_id: row.get::<_, i64>(10)? as u64,
        parent_id: row.get::<_, i64>(11)? as u64,
        volume_id: row.get_ref(12)?.as_str()?.into(),
        flags: row.get(13)?,
        properties: serde_json::from_str(&row.get::<_, String>(14)?).unwrap_or(json!({})),
        modified_ns: row.get(15)?,
        changed_ns: row.get(16)?,
        content_indexed: row.get(17)?,
        folded_name: Default::default(),
        folded_extension: Default::default(),
        folded_path: Default::default(),
        search_name: Default::default(),
        search_path: Default::default(),
        parent: Default::default(),
    })
}

pub(super) enum Journal {
    Snapshot,
    Cache,
}

fn select(journal: Journal) -> String {
    // The table is selected by an internal enum, never by request text. The
    // outer joins keep the bounded journal as the driving table and retain IDs
    // whose current metadata was removed or made inaccessible.
    let table = match journal {
        Journal::Snapshot => "snapshot_changes",
        Journal::Cache => "cache_changes",
    };
    format!(
        "SELECT j.id,{COLUMNS} FROM {table} j
         LEFT JOIN files f ON f.id=j.id AND f.accessible=1
         LEFT JOIN content c ON c.path=f.path ORDER BY j.id LIMIT ?1"
    )
}

pub(super) fn read(
    connection: &Connection,
    journal: Journal,
    limit: usize,
) -> Result<Option<SnapshotDelta>, String> {
    let mut statement = connection
        .prepare(&select(journal))
        .map_err(|error| error.to_string())?;
    let bound = i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX);
    let rows = statement
        .query_map([bound], |row| {
            let id = row.get(0)?;
            let file = if matches!(row.get_ref(1)?, ValueRef::Null) {
                None
            } else {
                Some(decode_file(row)?)
            };
            Ok((id, file))
        })
        .map_err(|error| error.to_string())?;
    let changes = rows
        .collect::<Result<SnapshotDelta, _>>()
        .map_err(|error| error.to_string())?;
    if changes.is_empty() || changes.len() > limit {
        Ok(None)
    } else {
        Ok(Some(changes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_query_looks_up_file_primary_keys_instead_of_scanning_files() {
        let temporary = tempfile::tempdir().unwrap();
        let store =
            crate::index_store::IndexStore::open(&temporary.path().join("index.sqlite")).unwrap();
        for journal in [Journal::Snapshot, Journal::Cache] {
            let mut statement = store
                .connection
                .prepare(&format!("EXPLAIN QUERY PLAN {}", select(journal)))
                .unwrap();
            let plan = statement
                .query_map([10], |row| row.get::<_, String>(3))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert!(
                plan.iter()
                    .any(|step| step.contains("SEARCH f USING INTEGER PRIMARY KEY")),
                "{plan:?}"
            );
            assert!(!plan.iter().any(|step| step.contains("SCAN f")), "{plan:?}");
        }
    }
}
