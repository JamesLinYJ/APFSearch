//! Isolated synthetic-cache comparison. Does not scan files or start a watcher.
use apfsearch_core::entry_table::FileEntry;
use apfsearch_core::{
    SearchEngine,
    index_store::{IndexStore, IndexedFile, SearchSnapshot},
};
use serde_json::json;
use std::{
    fs::File,
    io::{BufWriter, Write},
    path::PathBuf,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

fn signature(entries: &[IndexedFile]) -> String {
    let mut hasher = blake3::Hasher::new();
    for entry in entries {
        hasher.update(&serde_json::to_vec(entry).unwrap());
        hasher.update(b"\n");
    }
    hasher.finalize().to_hex().to_string()
}
fn json_read(path: &std::path::Path) -> Result<Vec<IndexedFile>, Box<dyn std::error::Error>> {
    let file = File::open(path)?;
    let mapped = unsafe { memmap2::Mmap::map(&file)? };
    Ok(serde_json::from_slice(&mapped)?)
}
fn stats(samples: &[f64]) -> serde_json::Value {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    json!({"runs":samples.len(), "median":sorted[sorted.len()/2], "min":sorted[0], "max":sorted[sorted.len()-1], "samples":samples})
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_owned();
    let workspace = project.parent().unwrap().parent().unwrap();
    let database = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            workspace.join("work/apfsearch/benchmark-index/synthetic-1789226413-83044.sqlite3")
        });
    let db = IndexStore::open(&database)?;
    if db.get("offline", json!(false)) != json!(true)
        || db.get("benchmark_seed", json!(null)) != json!(1_000_003)
        || db.get("roots", json!([])) != json!(["/synthetic-apfs-benchmark-only"])
    {
        return Err("Refusing a database without the isolated synthetic benchmark markers".into());
    }
    let generation = db.get("generation", json!(0)).as_u64().unwrap_or(0);
    let revision = db.get("revision", json!(0)).as_u64().unwrap_or(0);
    let began = Instant::now();
    let original = db.entries()?;
    let sqlite_read_ms = began.elapsed().as_secs_f64() * 1000.;
    let count = original.len();
    if count != 1_000_000 {
        return Err("Expected one million metadata rows".into());
    }
    let digest = signature(&original);
    let began = Instant::now();
    let snapshot = SearchSnapshot::new(original, generation);
    let sqlite_snapshot_build_ms = began.elapsed().as_secs_f64() * 1000.;
    let began = Instant::now();
    db.cache_write(&snapshot, revision)?;
    let binary_write_ms = began.elapsed().as_secs_f64() * 1000.;
    let binary_bytes = std::fs::metadata(&db.cache_path)?.len();
    let json_path = database.with_extension("comparison.json");
    let began = Instant::now();
    let mut output = BufWriter::new(File::create(&json_path)?);
    serde_json::to_writer(&mut output, &snapshot.entries)?;
    output.flush()?;
    drop(output);
    let json_write_ms = began.elapsed().as_secs_f64() * 1000.;
    let json_bytes = std::fs::metadata(&json_path)?.len();
    drop(snapshot);
    eprintln!("Cache bytes: binary {binary_bytes}; JSON {json_bytes}");
    let mut binary_reads = Vec::new();
    let mut json_reads = Vec::new();
    let binary_rebuild_ms: Option<f64> = None;
    let mut json_rebuild_ms = 0.;
    for i in 0..5 {
        let began = Instant::now();
        let (snapshot, got_generation) = db.cache_read().ok_or("Binary cache rejected")?;
        binary_reads.push(began.elapsed().as_secs_f64() * 1000.);
        if got_generation != generation
            || snapshot.len() != count
            || signature(
                &snapshot
                    .visible_entries()
                    .map(|entry| entry.to_owned_file())
                    .collect::<Vec<_>>(),
            ) != digest
        {
            return Err("Binary metadata mismatch".into());
        }
        if i == 0 {
            // Cache reads already return a prepared snapshot. No separate
            // reconstruction runs here, so its timing is reported as null.
            std::hint::black_box(&snapshot);
            drop(snapshot);
        } else {
            drop(snapshot);
        }
        let began = Instant::now();
        let entries = json_read(&json_path)?;
        json_reads.push(began.elapsed().as_secs_f64() * 1000.);
        if entries.len() != count || signature(&entries) != digest {
            return Err("JSON metadata mismatch".into());
        }
        if i == 0 {
            let began = Instant::now();
            let snapshot = SearchSnapshot::new(entries, generation);
            json_rebuild_ms = began.elapsed().as_secs_f64() * 1000.;
            std::hint::black_box(&snapshot);
            drop(snapshot);
        } else {
            drop(entries);
        }
        eprintln!(
            "Run {}: binary {:.1} ms; JSON {:.1} ms",
            i + 1,
            binary_reads[i],
            json_reads[i]
        );
    }
    drop(db);
    let began = Instant::now();
    let engine = SearchEngine::open(&database)?;
    let engine_open_from_binary_cache_ms = began.elapsed().as_secs_f64() * 1000.;
    if engine.call(json!({"op":"status"}))["count"] != json!(count) {
        return Err("SearchEngine cache startup count mismatch".into());
    }
    let result = engine.call(json!({"op":"query","text":"ext:pdf","limit":200}));
    if result["total"] != json!(100000) || result["rows"].as_array().map(Vec::len) != Some(200) {
        return Err("SearchEngine cache search mismatch".into());
    }
    let _ = engine.call(json!({"op":"stop"}));
    let report = json!({
        "schema_version":1,"timestamp_unix":SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        "scope":"One million synthetic metadata entries. Prepared binary search snapshot vs uncompressed serde JSON metadata array. Both use mmap for reading; binary restores postings and sorting while JSON rebuilds them.",
        "database":database,"entries":count,"generation":generation,"revision":revision,
        "binary_cache":{"path":database.with_extension("snapshot.bin"),"bytes":binary_bytes,"write_ms":binary_write_ms,"read_ms":stats(&binary_reads),"snapshot_rebuild_ms_single_run":binary_rebuild_ms},
        "json_baseline":{"path":json_path,"bytes":json_bytes,"write_ms":json_write_ms,"read_ms":stats(&json_reads),"snapshot_rebuild_ms_single_run":json_rebuild_ms},
        "binary_size_fraction_of_json":binary_bytes as f64/json_bytes as f64,
        "sqlite_entries_read_ms_single_run":sqlite_read_ms,"sqlite_snapshot_rebuild_ms_single_run":sqlite_snapshot_build_ms,
        "engine_open_from_binary_cache_ms_single_run":engine_open_from_binary_cache_ms,
        "metadata_all_fields_equal_every_run":true,"metadata_blake3":digest,"engine_count_and_pdf_first_page_verified":true,
        "limitations":["Warm OS page cache; not a cold disk launch","JSON baseline recreated from identical IndexedFile values, not an archived earlier application binary","Owned strings and bitmap vectors are reconstructed; cached Unicode columns, postings and name order avoid recomputation. This is not zero-copy live search.","No GUI or XPC timing","SearchSnapshot rebuild and engine startup are single observations, not percentile claims","Synthetic path/string repetition may compress better than a real user index"]
    });
    let report_path = project.join("validation/cache.json");
    std::fs::write(&report_path, serde_json::to_vec_pretty(&report)?)?;
    println!("{}", report_path.display());
    Ok(())
}
