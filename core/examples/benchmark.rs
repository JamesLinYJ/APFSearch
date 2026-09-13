//! Synthetic metadata benchmark; never reads a user's files or starts a watcher.
//! cargo run --release --example benchmark -- [--count 1000000] [--runs 30]
//! Re-run a created database: --reuse /absolute/path/to/benchmark.sqlite3
use apfsearch_core::{index_store::IndexStore, scanner::ScannedFile, SearchEngine};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::PathBuf,
    process::Command,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
const SYNTHETIC_ROOT: &str = "/synthetic-apfs-benchmark-only";
const SEED: u64 = 1_000_003;
fn argument(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.windows(2).find(|a| a[0] == name).map(|a| a[1].clone())
}
fn command(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|r| r.status.success())
        .map(|r| String::from_utf8_lossy(&r.stdout).trim().to_string())
        .unwrap_or_default()
}
fn rss_bytes() -> Option<u64> {
    command("ps", &["-o", "rss=", "-p", &std::process::id().to_string()])
        .trim()
        .parse::<u64>()
        .ok()
        .map(|kb| kb * 1024)
}
#[cfg(target_os = "macos")]
fn peak_rss_bytes() -> Option<u64> {
    // Darwin rusage is 2 timeval values followed by maxrss and 13 other longs.
    // Darwin reports ru_maxrss in bytes (Linux reports KiB).
    #[repr(C)]
    struct RUsage {
        user: [i64; 2],
        system: [i64; 2],
        maxrss: i64,
        rest: [i64; 13],
    }
    unsafe extern "C" {
        fn getrusage(who: i32, usage: *mut RUsage) -> i32;
    }
    let mut usage: RUsage = unsafe { std::mem::zeroed() };
    if unsafe { getrusage(0, &mut usage) } == 0 {
        Some(usage.maxrss as u64)
    } else {
        None
    }
}
#[cfg(not(target_os = "macos"))]
fn peak_rss_bytes() -> Option<u64> {
    None
}
fn synthetic(i: u64) -> ScannedFile {
    let ext = [
        "rs", "swift", "pdf", "docx", "xlsx", "png", "jpg", "txt", "json", "mp4",
    ][(i % 10) as usize];
    let prefix = [
        "report", "invoice", "photo", "source", "backup", "报告", "analysis",
    ][(i % 7) as usize];
    let is_dir = i.is_multiple_of(1000);
    let name = if is_dir {
        format!("folder {}", i)
    } else {
        format!("{prefix}-{}-{i:07}.{ext}", i % 10007)
    };
    let modified = 1_780_000_000 + (i % (86_400 * 120)) as i64;
    let created = modified - (i % 86400) as i64;
    ScannedFile {
        path: format!(
            "{SYNTHETIC_ROOT}/volume-{}/group-{}/folder-{}/{name}",
            i % 2,
            i / 10000,
            i / 250
        ),
        name,
        extension: if is_dir { String::new() } else { ext.into() },
        size: if is_dir {
            0
        } else {
            (i * 104729 + SEED) % (64 * 1024 * 1024) + i % 4096
        },
        modified,
        changed: modified,
        created,
        modified_ns: modified * 1_000_000_000 + (i % 1_000_000_000) as i64,
        changed_ns: modified * 1_000_000_000 + (i % 1_000_000_000) as i64,
        is_dir,
        is_symlink: !is_dir && i.is_multiple_of(997),
        link_count: None,
        file_id: i + 10_000,
        parent_id: i / 250 + 1,
        volume_id: format!("synthetic-volume-{}", i % 2),
        flags: if i.is_multiple_of(499) { 0x8000 } else { 0 },
    }
}
fn expected(entry: &ScannedFile, counts: &mut HashMap<&'static str, usize>) {
    *counts.entry("empty").or_default() += 1;
    if entry.name.contains("rep") {
        *counts.entry("substring_3").or_default() += 1;
    }
    if entry.name.contains('a') {
        *counts.entry("short_1").or_default() += 1;
    }
    if entry.extension == "pdf" {
        *counts.entry("extension").or_default() += 1;
    }
    if entry.size > 10 * 1024 * 1024 {
        *counts.entry("numeric_size").or_default() += 1;
    }
    if entry.name.contains("report") && entry.size > 10 * 1024 * 1024 {
        *counts.entry("name_and_size").or_default() += 1;
    }
    if entry.name.contains("报告") {
        *counts.entry("unicode").or_default() += 1;
    }
    if entry.name.contains("report") {
        *counts.entry("multi_column_sort").or_default() += 1;
    }
}
fn quantile(samples: &[f64], fraction: f64) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let index = ((sorted.len() as f64 * fraction).ceil() as usize)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    sorted[index]
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let count = argument("--count")
        .map(|s| s.parse::<u64>())
        .transpose()?
        .unwrap_or(1_000_000);
    let runs = argument("--runs")
        .map(|s| s.parse::<usize>())
        .transpose()?
        .unwrap_or(30);
    if count == 0 || count > 1_000_000 || runs == 0 || runs > 100 {
        return Err("count must be 1..1000000 and runs 1..100".into());
    }
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let workspace = project.parent().unwrap().parent().unwrap();
    let run_id = format!(
        "{}-{}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        std::process::id()
    );
    let work = argument("--directory")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("work/apfsearch/benchmark-index"));
    std::fs::create_dir_all(&work)?;
    let reuse = argument("--reuse");
    let database = reuse
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| work.join(format!("synthetic-{run_id}.sqlite3")));
    let output = argument("--output")
        .map(PathBuf::from)
        .unwrap_or_else(|| project.join("validation/benchmark.json"));
    let started = Instant::now();
    let rss_before = rss_bytes();
    let seed_start = Instant::now();
    let mut expected_counts = HashMap::new();
    if reuse.is_none() {
        let mut storage = IndexStore::open(&database)?;
        storage.set("offline", &json!(true))?;
        storage.set("watch_enabled", &json!(false))?;
        storage.set("roots", &json!([SYNTHETIC_ROOT]))?;
        storage.set("benchmark_seed", &json!(SEED))?;
        let mut batch = Vec::with_capacity(2000);
        for i in 0..count {
            let entry = synthetic(i);
            expected(&entry, &mut expected_counts);
            batch.push(entry);
            if batch.len() == 2000 || i + 1 == count {
                storage.batch(&batch, 1)?;
                batch.clear();
            }
            if (i + 1) % 100_000 == 0 {
                eprintln!("Synthetic rows committed: {} / {}", i + 1, count);
            }
        }
        storage.finish(&[SYNTHETIC_ROOT.into()], &[], 1, 0)?;
        storage
            .connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    } else {
        for i in 0..count {
            expected(&synthetic(i), &mut expected_counts);
        }
    }
    let seed_seconds = seed_start.elapsed().as_secs_f64();
    eprintln!("Opening synthetic index {}", database.display());
    let open_start = Instant::now();
    let engine = SearchEngine::open(&database)?;
    let startup_ms = open_start.elapsed().as_secs_f64() * 1000.;
    let rss_after_open = rss_bytes();
    let status = engine.call(json!({"op":"status"}));
    if status["count"].as_u64() != Some(count) {
        return Err(format!("Index count mismatch: {status}").into());
    }
    eprintln!(
        "SearchEngine::open: {:.1} ms; RSS: {:?} bytes",
        startup_ms, rss_after_open
    );
    let cases = [
        ("empty", "", json!([{"field":"name","ascending":true}])),
        (
            "substring_3",
            "rep",
            json!([{"field":"name","ascending":true}]),
        ),
        ("short_1", "a", json!([{"field":"name","ascending":true}])),
        (
            "extension",
            "ext:pdf",
            json!([{"field":"name","ascending":true}]),
        ),
        (
            "numeric_size",
            "size:>10mb",
            json!([{"field":"name","ascending":true}]),
        ),
        (
            "name_and_size",
            "report size:>10mb",
            json!([{"field":"name","ascending":true}]),
        ),
        (
            "unicode",
            "报告",
            json!([{"field":"name","ascending":true}]),
        ),
        (
            "multi_column_sort",
            "report",
            json!([{"field":"size","ascending":false},{"field":"name","ascending":true}]),
        ),
    ];
    let mut results = Vec::new();
    for (name, query, sorts) in cases {
        let request = serde_json::to_string(
            &json!({"op":"query","text":query,"limit":200,"offset":0,"sort":sorts,"request_id":"synthetic-benchmark"}),
        )?;
        let mut samples = Vec::with_capacity(runs);
        let mut internal = Vec::with_capacity(runs);
        let mut response_bytes = 0;
        let mut total = 0;
        let mut fingerprint = String::new();
        for run in 0..runs + 2 {
            let began = Instant::now();
            let response = engine.call(serde_json::from_str(&request)?);
            let encoded = serde_json::to_vec(&response)?;
            let ms = began.elapsed().as_secs_f64() * 1000.;
            if response["success"] != json!(true) {
                return Err(format!("Query {query:?} failed: {response}").into());
            }
            total = response["total"].as_u64().unwrap_or(0) as usize;
            if total != expected_counts[name] {
                return Err(
                    format!("{name}: expected {}, got {total}", expected_counts[name]).into(),
                );
            }
            let rows = response["rows"].as_array().unwrap();
            if rows.len() != total.min(200) {
                return Err(format!("{name}: incorrect page length").into());
            }
            let ids: Vec<Value> = rows.iter().map(|r| r["id"].clone()).collect();
            let current = blake3::hash(&serde_json::to_vec(&ids)?)
                .to_hex()
                .to_string();
            if fingerprint.is_empty() {
                fingerprint = current;
            } else if fingerprint != current {
                return Err(format!("{name}: non-deterministic first page").into());
            }
            response_bytes = encoded.len();
            std::hint::black_box(&encoded);
            if run >= 2 {
                samples.push(ms);
                internal.push(response["elapsed_ms"].as_f64().unwrap_or(0.));
            }
        }
        let p95 = quantile(&samples, 0.95);
        eprintln!(
            "{name}: {total} matches, P50 {:.2} ms / P95 {:.2} ms / max {:.2} ms",
            quantile(&samples, 0.5),
            p95,
            quantile(&samples, 1.)
        );
        results.push(json!({"name":name,"query":query,"sort":sorts,"runs":runs,"warmups":2,
            "total_matches":total,"page_rows":total.min(200),"response_bytes":response_bytes,
            "core_round_trip_ms":{"p50":quantile(&samples,0.5),"p95":p95,"max":quantile(&samples,1.),"samples":samples},
            "engine_reported_ms":{"p50":quantile(&internal,0.5),"p95":quantile(&internal,0.95),"max":quantile(&internal,1.)},
            "under_100ms_core_p95":p95<=100.,"result_count_and_page_stability_verified":true,"page_fingerprint":fingerprint}));
    }
    let report = json!({
        "schema_version":1,"run_id":run_id,"timestamp_unix":SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        "scope":"Synthetic metadata, Rust engine plus JSON request decode and response encode; no GUI, XPC, actual APFS enumeration or file content extraction",
        "limitations":["Not proof of GUI end-to-end P95 <=100ms","Startup is SQLite reconstruction with a warm or reused OS page cache, not a cold-storage launch","Synthetic filenames cannot represent every real-world path distribution","No whole-system memory pressure, battery or power measurement","No actual million-file filesystem created"],
        "machine":{"architecture":std::env::consts::ARCH,"os":command("sw_vers",&["-productVersion"]),"model":command("sysctl",&["-n","hw.model"]),"cpu":command("sysctl",&["-n","machdep.cpu.brand_string"]),"logical_cpus":command("sysctl",&["-n","hw.ncpu"]),"physical_memory_bytes":command("sysctl",&["-n","hw.memsize"]),"build":"cargo release, thin LTO"},
        "dataset":{"kind":"synthetic","entries":count,"seed":SEED,"directories":count.div_ceil(1000),"volumes":2,"filename_prefixes":7,"extensions":10,"includes_unicode":true,"database":database,"database_bytes":std::fs::metadata(&database)?.len(),"reused":reuse.is_some()},
        "population_seconds":if reuse.is_none(){Some(seed_seconds)}else{None},"expectation_generation_seconds":if reuse.is_some(){Some(seed_seconds)}else{None},"engine_open_ms":startup_ms,
        "memory_bytes":{"rss_before":rss_before,"rss_after_engine_open":rss_after_open,"rss_after_queries":rss_bytes(),"peak_process_rss":peak_rss_bytes()},
        "queries":results,"all_query_result_counts_verified":true,"overall_seconds":started.elapsed().as_secs_f64(),
    });
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp_output = output.with_extension("json.tmp");
    std::fs::write(&temp_output, serde_json::to_vec_pretty(&report)?)?;
    std::fs::rename(temp_output, &output)?;
    let archived = work.join(format!("report-{run_id}.json"));
    std::fs::write(archived, serde_json::to_vec_pretty(&report)?)?;
    println!("{}", output.display());
    // Stop is metadata only here; benchmark has never enabled the watcher.
    let _ = engine.call(json!({"op":"stop"}));
    Ok(())
}
