//! Restore a prepared index using an existing isolated synthetic fixture.
//! Writes one derived cache; never rescans files or creates another database copy.
use apfsearch_core::{
    SearchEngine,
    index_store::{IndexStore, SearchSnapshot},
};
use serde_json::json;
use std::{path::PathBuf, time::Instant};
fn digest(snapshot: &SearchSnapshot) -> String {
    let mut digest = blake3::Hasher::new();
    for entry in snapshot.visible_entries() {
        digest.update(&serde_json::to_vec(entry).unwrap());
    }
    digest.finalize().to_hex().to_string()
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or("Provide the existing isolated synthetic index")?,
    );
    let report_path = PathBuf::from(std::env::args().nth(2).ok_or("Provide a report path")?);
    let store = IndexStore::open(&path)?;
    if store.get("offline", json!(false)) != json!(true)
        || store.get("benchmark_seed", json!(null)) != json!(1_000_003)
        || store.get("roots", json!([])) != json!(["/synthetic-apfs-benchmark-only"])
    {
        return Err("Refusing an index without the isolated synthetic fixture markers".into());
    }
    let generation = store.get("generation", json!(0)).as_u64().unwrap();
    let revision = store.get("revision", json!(0)).as_u64().unwrap();
    let started = Instant::now();
    let entries = store.entries()?;
    let sqlite_read_ms = started.elapsed().as_secs_f64() * 1000.;
    let started = Instant::now();
    let mut snapshot = SearchSnapshot::new(entries, generation);
    snapshot.content_revision = store.get("content_revision", json!(0)).as_u64().unwrap();
    let snapshot_build_ms = started.elapsed().as_secs_f64() * 1000.;
    let count = snapshot.len();
    let expected = digest(&snapshot);
    let started = Instant::now();
    store.cache_write(&snapshot, revision)?;
    let write_ms = started.elapsed().as_secs_f64() * 1000.;
    let cache_bytes = std::fs::metadata(&store.cache_path)?.len();
    drop(snapshot);
    let mut reads = Vec::new();
    for _ in 0..3 {
        let started = Instant::now();
        let (restored, _) = store.cache_read().ok_or("Prepared cache rejected")?;
        reads.push(started.elapsed().as_secs_f64() * 1000.);
        if restored.len() != count || digest(&restored) != expected {
            return Err("Prepared metadata mismatch".into());
        }
    }
    drop(store);
    let started = Instant::now();
    let engine = SearchEngine::open(&path)?;
    let engine_open_ms = started.elapsed().as_secs_f64() * 1000.;
    let mut queries = Vec::new();
    for (query, expected_total) in [
        ("", 1_000_000),
        ("a", 285_428),
        ("ext:pdf", 100_000),
        ("报告", 142_714),
    ] {
        let started = Instant::now();
        let response = engine.call(json!({"op":"query","text":query,"limit":200}));
        let wall_ms = started.elapsed().as_secs_f64() * 1000.;
        if response["success"] != json!(true) || response["total"] != json!(expected_total) {
            return Err(format!("Query failed: {response}").into());
        }
        queries.push(json!({"query":query,"total":response["total"],"wall_ms":wall_ms,"core_ms":response["elapsed_ms"]}));
    }
    let report = json!({"scope":"Existing isolated million-row metadata fixture; one prepared cache publication, three reads and one engine open. No filesystem scan, new database copy, or JSON comparison file.","database":path,"rows":count,"generation":generation,"sqlite_read_ms":sqlite_read_ms,"snapshot_build_ms":snapshot_build_ms,"prepared_cache_read_ms":reads,"prepared_cache_write_ms":write_ms,"cache_file_bytes_written_once":cache_bytes,"engine_open_ms":engine_open_ms,"metadata_digest":expected,"all_metadata_fields_equal":true,"first_queries":queries,"limitations":["OS file caches are warm; engine_open is not cold-disk timing.","Prepared cache restores owned strings and bitmap vectors; this is not zero-copy searching.","Logical cache bytes are not a measurement of physical NAND writes.","No XPC or GUI timing; three restore samples are not percentile evidence."]});
    std::fs::write(&report_path, serde_json::to_vec_pretty(&report)?)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
