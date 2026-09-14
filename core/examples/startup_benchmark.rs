//! Profile an explicitly supplied offline fixture; never enumerate the filesystem.
use apfsearch_core::{SearchEngine, index_store::IndexStore};
use rusqlite::{Connection, OpenFlags};
use serde_json::json;
use std::{path::PathBuf, time::Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let mode = arguments.next().ok_or("Expected cache or engine mode")?;
    let path = PathBuf::from(
        arguments
            .next()
            .ok_or("Expected offline fixture database")?,
    );
    if arguments.next().is_some() || (mode != "cache" && mode != "engine") {
        return Err("Usage: startup_benchmark <cache|engine> <offline fixture database>".into());
    }
    // Refuse watched or live indexes before opening the production engine.
    // Engine mode performs its normal journal housekeeping in this fixture.
    let validation = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    for (key, expected) in [("offline", "true"), ("watch_enabled", "false")] {
        let value: String =
            validation.query_row("SELECT value FROM settings WHERE key=?1", [key], |row| {
                row.get(0)
            })?;
        if value != expected {
            return Err(format!("Fixture must have {key}={expected}").into());
        }
    }
    drop(validation);
    let started = Instant::now();
    if mode == "cache" {
        let store = IndexStore::open(&path)?;
        let store_ms = started.elapsed().as_secs_f64() * 1000.;
        let (snapshot, _) = store.cache_read().ok_or("Fixture cache must be valid")?;
        println!(
            "{}",
            json!({"mode":"cache", "store_ms":store_ms,
            "total_ms":started.elapsed().as_secs_f64()*1000., "count":snapshot.len()})
        );
        std::hint::black_box(snapshot);
    } else {
        let engine = SearchEngine::open(&path)?;
        let open_ms = started.elapsed().as_secs_f64() * 1000.;
        let status = engine.call(json!({"op":"status"}));
        println!(
            "{}",
            json!({"mode":"engine", "open_ms":open_ms,
            "total_ms":started.elapsed().as_secs_f64()*1000., "status":status})
        );
        std::hint::black_box(engine);
    }
    Ok(())
}
