//! Read-only startup phase profile. No SearchEngine, schema writes, or watcher.
use apfsearch_core::index_store::{IndexStore, SearchSnapshot};
use rusqlite::{Connection, OpenFlags};
use serde_json::json;
use std::{path::PathBuf, time::Instant};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or("Provide an existing SQLite index path")?,
    );
    let started = Instant::now();
    let connection = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.execute_batch("PRAGMA query_only=ON")?;
    let store = IndexStore {
        connection,
        cache_path: path.with_extension("snapshot.bin"),
    };
    let dirty = store.cache_is_dirty();
    let cache_exists = store.cache_path.exists();
    let generation = store.get("generation", json!(0)).as_u64().unwrap_or(0);
    let loaded = Instant::now();
    let entries = store.entries()?;
    let load_ms = loaded.elapsed().as_secs_f64() * 1000.;
    let count = entries.len();
    eprintln!("Loaded {count} rows in {load_ms:.2} ms");
    drop(store);
    let mut phases = serde_json::Map::new();
    let snapshot = SearchSnapshot::build_with_metrics(entries, generation, |name, duration| {
        let milliseconds = duration.as_secs_f64() * 1000.;
        phases.insert(name.into(), json!(milliseconds));
        eprintln!("{name}: {milliseconds:.2} ms");
    });
    println!(
        "{}",
        json!({"scope":"Read-only SQLite metadata load and in-process search snapshot construction; no schema mutation, watcher, GUI or XPC. Live SQLite read may contend for IO with the running service.","path":path,"rows":count,"generation":generation,"cache_dirty":dirty,"cache_exists":cache_exists,"sqlite_load_ms":load_ms,"snapshot_phases_ms":phases,"total_ms":started.elapsed().as_secs_f64()*1000.,"snapshot_entries":snapshot.len()})
    );
    Ok(())
}
