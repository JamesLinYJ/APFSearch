//! Real APFS fixture validation. Changes only isolated temporary files below work/.
use apfsearch_core::{SearchEngine, scanner};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::{MetadataExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
#[derive(Debug, PartialEq, Eq)]
struct Expected {
    size: u64,
    is_dir: bool,
    is_symlink: bool,
    ino: u64,
    modified_ns: i64,
}
fn independent(root: &Path) -> Result<BTreeMap<String, Expected>> {
    let mut result = BTreeMap::new();
    let mut queue = vec![root.to_path_buf()];
    while let Some(path) = queue.pop() {
        let m = fs::symlink_metadata(&path)?;
        result.insert(
            path.to_string_lossy().into_owned(),
            Expected {
                size: if m.is_dir() { 0 } else { m.len() },
                is_dir: m.is_dir(),
                is_symlink: m.file_type().is_symlink(),
                ino: m.ino(),
                modified_ns: m.mtime() * 1_000_000_000 + m.mtime_nsec(),
            },
        );
        if m.is_dir() && !m.file_type().is_symlink() {
            for child in fs::read_dir(path)? {
                queue.push(child?.path());
            }
        }
    }
    Ok(result)
}
fn call(engine: &Arc<SearchEngine>, request: Value) -> Result<Value> {
    let value = engine.call(request);
    if value["success"] != true {
        return Err(format!("SearchEngine call failed: {value}").into());
    }
    Ok(value)
}
fn discrepancy(
    engine: &Arc<SearchEngine>,
    expected: &BTreeMap<String, Expected>,
) -> Result<Option<String>> {
    let response = call(engine, json!({"op":"query","text":"","limit":10000}))?;
    let rows = response["rows"].as_array().ok_or("Missing rows")?;
    if response["total"].as_u64() != Some(expected.len() as u64) {
        return Ok(Some(format!(
            "total {} expected {}",
            response["total"],
            expected.len()
        )));
    }
    let mut observed = BTreeMap::new();
    for row in rows {
        let path = row["path"].as_str().ok_or("Missing row path")?;
        if observed
            .insert(
                path.to_owned(),
                Expected {
                    size: row["size"].as_u64().unwrap_or(u64::MAX),
                    is_dir: row["is_dir"].as_bool().unwrap_or(false),
                    is_symlink: row["is_symlink"].as_bool().unwrap_or(false),
                    ino: row["file_id"].as_u64().unwrap_or(0),
                    modified_ns: row["modified_ns"].as_i64().unwrap_or(0),
                },
            )
            .is_some()
        {
            return Err(format!("Duplicate result path: {path}").into());
        }
    }
    if observed.len() != expected.len() {
        return Ok(Some(format!(
            "page length {} expected {}",
            observed.len(),
            expected.len()
        )));
    }
    for (path, want) in expected {
        match observed.get(path) {
            None => return Ok(Some(format!("Missing {path}"))),
            Some(actual) if actual != want => {
                return Ok(Some(format!(
                    "Metadata differs for {path}: {actual:?} expected {want:?}"
                )));
            }
            _ => (),
        }
    }
    Ok(None)
}
fn converge(
    engine: &Arc<SearchEngine>,
    root: &Path,
    start: Instant,
    timeout: Duration,
) -> Result<f64> {
    let mut last = String::new();
    while start.elapsed() < timeout {
        // Metadata can settle after a rename (or be touched by OS services).
        // Compare the current namespace, not a stale pre-wait timestamp sample.
        let expected = independent(root)?;
        match discrepancy(engine, &expected)? {
            None => return Ok(start.elapsed().as_secs_f64() * 1000.),
            Some(detail) => last = detail,
        }
        thread::sleep(Duration::from_millis(10));
    }
    Err(format!(
        "Index did not converge: {last}; current_root={:?}; status={}",
        independent(root)?.get(&root.to_string_lossy().into_owned()),
        engine.call(json!({"op":"status"}))
    )
    .into())
}
static STABILITY_RETRIES: AtomicU64 = AtomicU64::new(0);
static STABILITY_WAIT_MICROS: AtomicU64 = AtomicU64::new(0);
fn stable(engine: &Arc<SearchEngine>, root: &Path) -> Result<()> {
    let start = Instant::now();
    loop {
        let before = independent(root)?;
        thread::sleep(Duration::from_millis(25));
        let after = independent(root)?;
        if before == after && discrepancy(engine, &after)?.is_none() {
            STABILITY_WAIT_MICROS.fetch_add(start.elapsed().as_micros() as u64, Ordering::Relaxed);
            return Ok(());
        }
        // APFS/OS services may settle directory timestamps asynchronously.
        // Keep full metadata comparison and wait for agreement again instead of
        // declaring an in-flight legitimate metadata update to be corruption.
        STABILITY_RETRIES.fetch_add(1, Ordering::Relaxed);
        converge(engine, root, start, Duration::from_secs(10))?;
        if start.elapsed() > Duration::from_secs(10) {
            return Err("Namespace did not reach a stable metadata snapshot".into());
        }
    }
}
fn stats(samples: &[f64]) -> Value {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let q = |p: f64| {
        sorted[((sorted.len() as f64 * p).ceil() as usize)
            .saturating_sub(1)
            .min(sorted.len() - 1)]
    };
    json!({"runs":samples.len(),"p50_ms":q(0.5),"p95_ms":q(0.95),"max_ms":q(1.),"samples_ms":samples,"p95_under_1000ms":q(0.95)<=1000.})
}
fn create_fixture(root: &Path) -> Result<()> {
    fs::create_dir(root)?;
    for i in 0..24 {
        let dir = root.join(format!("目录-{i}"));
        fs::create_dir(&dir)?;
        for j in 0..8 {
            fs::write(dir.join(format!("report {j} café.txt")), format!("{i} {j}"))?;
        }
    }
    fs::write(root.join("zero.txt"), [])?;
    fs::write(root.join(".hidden"), "hidden")?;
    fs::hard_link(
        root.join("目录-0/report 0 café.txt"),
        root.join("hardlink.txt"),
    )?;
    symlink(root.join("目录-0"), root.join("symlink-directory"))?;
    fs::create_dir_all(root.join("Example.app/Contents"))?;
    fs::write(root.join("Example.app/Contents/info"), "package contents")?;
    fs::create_dir_all(root.join("moving-start/nested"))?;
    for i in 0..8 {
        fs::write(
            root.join(format!("moving-start/nested/data{i}.bin")),
            vec![i as u8; i * 31],
        )?;
    }
    Ok(())
}
fn crash_worker(args: &[String]) -> Result<()> {
    let index = Path::new(&args[2]);
    let root = Path::new(&args[3]);
    let ready = Path::new(&args[4]);
    let engine = SearchEngine::open(index)?;
    call(&engine, json!({"op":"watch","roots":[root],"watch":true}))?;
    converge(&engine, root, Instant::now(), Duration::from_secs(15))?;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if engine.call(json!({"op":"status"}))["state"] == "watching" {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    fs::write(ready, "watching and converged")?;
    loop {
        thread::sleep(Duration::from_secs(1));
    }
}
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|s| s == "--crash-worker") {
        return crash_worker(&args);
    }
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let workspace = project.parent().unwrap().parent().unwrap();
    let base = workspace.join("work/apfsearch");
    fs::create_dir_all(&base)?;
    let temp = tempfile::Builder::new()
        .prefix("filesystem-validation-")
        .tempdir_in(&base)?;
    let root = temp.path().join("files");
    create_fixture(&root)?;
    let root = root.canonicalize()?;
    let index = temp.path().join("index/index.sqlite3");
    let engine = SearchEngine::open(&index)?;
    let initial_start = Instant::now();
    call(&engine, json!({"op":"watch","roots":[root],"watch":true}))?;
    // These mutations overlap the initial scan. The engine must settle to the
    // real namespace without losing a create/rename/delete boundary.
    let racing_root = root.clone();
    let race = thread::spawn(move || -> std::io::Result<()> {
        for i in 0..50 {
            let p = racing_root.join(format!("目录-0/startup-{i}.txt"));
            fs::write(&p, format!("new {i}"))?;
            if i % 3 == 0 {
                fs::remove_file(&p)?
            } else if i % 3 == 1 {
                fs::rename(
                    &p,
                    racing_root.join(format!("目录-1/moved-startup-{i}.txt")),
                )?
            }
            thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    });
    race.join().map_err(|_| "Race writer panicked")??;
    let initial_ms = converge(&engine, &root, initial_start, Duration::from_secs(15))?;
    stable(&engine, &root)?;
    let initial_count = independent(&root)?.len();
    let mut creates = Vec::new();
    let mut renames = Vec::new();
    let mut deletes = Vec::new();
    let mut moving = root.join("moving-start");
    for i in 0..30 {
        let created = root.join(format!("目录-{}/event-{i}.txt", i % 24));
        let start = Instant::now();
        fs::write(&created, format!("event payload {i}"))?;
        creates.push(converge(&engine, &root, start, Duration::from_secs(10))?);
        stable(&engine, &root)?;
        let next = root.join(format!("renamed-directory-{i}"));
        let start = Instant::now();
        fs::rename(&moving, &next)?;
        moving = next;
        renames.push(converge(&engine, &root, start, Duration::from_secs(10))?);
        stable(&engine, &root)?;
        let start = Instant::now();
        fs::remove_file(&created)?;
        deletes.push(converge(&engine, &root, start, Duration::from_secs(10))?);
        stable(&engine, &root)?;
        eprintln!("APFS mutation round {} / 30 consistent", i + 1);
    }
    let final_map = independent(&root)?;
    let final_count = final_map.len();
    let actual_state = call(&engine, json!({"op":"status"}))?;
    call(&engine, json!({"op":"stop"}))?;
    // A separate helper process is killed using std::process::Child::kill.
    // This touches only our fixture and independent crash-test SQLite database.
    let crash_index = temp.path().join("crash-index/index.sqlite3");
    let ready = temp.path().join("crash-ready");
    let mut child = Command::new(std::env::current_exe()?)
        .arg("--crash-worker")
        .arg(&crash_index)
        .arg(&root)
        .arg(&ready)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let wait = Instant::now();
    while !ready.exists() && wait.elapsed() < Duration::from_secs(20) {
        if let Some(status) = child.try_wait()? {
            return Err(format!("Crash-test helper exited early: {status}").into());
        }
        thread::sleep(Duration::from_millis(20));
    }
    if !ready.exists() {
        let _ = child.kill();
        return Err("Crash-test helper did not become ready".into());
    }
    child.kill()?;
    let killed = child.wait()?;
    fs::write(
        root.join("after-crash.txt"),
        "written while no crash-test watcher was running",
    )?;
    fs::remove_file(root.join("目录-2/report 0 café.txt"))?;
    let recovery_start = Instant::now();
    let recovered = SearchEngine::open(&crash_index)?;
    let recovery_ms = converge(&recovered, &root, recovery_start, Duration::from_secs(15))?;
    stable(&recovered, &root)?;
    let recovered_state = call(&recovered, json!({"op":"status"}))?;
    call(&recovered, json!({"op":"stop"}))?;
    let dropped =
        scanner::ChangeEvent::from_flags(&root.join("目录-1").to_string_lossy(), 123, 0x04);
    let recovery_roots =
        scanner::reconciliation_roots(&[root.to_string_lossy().into_owned()], &[dropped]);
    if recovery_roots != vec![root.to_string_lossy().into_owned()] {
        return Err(
            "Injected event-loss invalidation did not request full configured-root reconciliation"
                .into(),
        );
    }
    let report = json!({
        "schema_version":1,"timestamp_unix":SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        "scope":"Real temporary files on local APFS; SearchEngine watch=true; JSON-query convergence against independent std::fs recursive lstat traversal. No user documents scanned or modified.",
        "fixture":{"initial_entries_after_racing_scan":initial_count,"final_entries_before_crash_test":final_count,"seeded_directory_groups":24,"directory_entries":final_map.values().filter(|e|e.is_dir).count(),"hidden_files":true,"unicode":true,"hardlinks":true,"symlinks_not_followed":true,"packages":true,"zero_byte_files":true,"temporary_files_removed_after_run":true},
        "consistency":{"all_90_mutations_verified":true,"duplicate_paths":0,"missing_paths":0,"extra_paths":0,"compared_fields":["path","file type","logical file size","inode","modified nanoseconds"],"independent_implementation":"std::fs::read_dir + symlink_metadata; does not call scanner/getattrlistbulk","post_convergence_recheck_ms":25,"additional_stability_retries":STABILITY_RETRIES.load(Ordering::Relaxed),"total_stability_wait_ms":STABILITY_WAIT_MICROS.load(Ordering::Relaxed) as f64/1000.},
        "latency_method":"From immediately before a filesystem mutation until the first complete engine result equals an independent traversal; includes mutation duration, JSON querying, and 10ms polling; excludes GUI and XPC.",
        "startup_with_concurrent_50_mutations_ms":initial_ms,"create":stats(&creates),"directory_rename":stats(&renames),"delete":stats(&deletes),
        "watcher":{"requested_before_initial_scan":true,"implementation_order":"SearchEngine scan_worker creates Watcher before reconcile; scanner live-event unit test also constructs Watcher before scanning","state_after_mutations":actual_state},
        "crash_recovery":{"method":"SIGKILL of an isolated helper after its watched initial index converged; mutate fixture while helper is stopped; SearchEngine::open automatically resumes persisted watcher and reconciles","actual_process_kill":true,"process_exit":format!("{killed}"),"result_consistency_passed":true,"recovery_ms":recovery_ms,"recovered_state":recovered_state},
        "event_loss":{"injected_kernel_dropped_flag":4,"full_root_reconciliation_requested":true,"actual_kernel_queue_overflow_forced":false,"existing_regression":"scanner::tests::journal_drop_and_rename_invalidation_are_conservative"},
        "additional_existing_coverage":["cache_dirty_transaction_wins_after_unpublished_batch","stale_snapshot_cannot_clear_new_batch_dirty_flag","denied_regions_are_retained_but_not_searchable_then_recover","watcher_recovers_initially_missing_scoped_root"],
        "not_tested":["Physical volume unplug/replug","Full Disk Access grant/revocation UI","Whole-machine coverage","GUI end-to-end latency","Forced real FSEvents kernel event loss"]
    });
    thread::sleep(Duration::from_millis(250));
    drop(recovered);
    drop(engine);
    let output = project.join("validation/filesystem.json");
    fs::create_dir_all(output.parent().unwrap())?;
    fs::write(&output, serde_json::to_vec_pretty(&report)?)?;
    println!("{}", output.display());
    println!(
        "create={} rename={} delete={} recovery_ms={recovery_ms:.2}",
        stats(&creates),
        stats(&renames),
        stats(&deletes)
    );
    Ok(())
}
