//! Same-corpus acceptance harness; metadata never leaves the process.
//! The only SQLite writes are initialization of a small disposable empty index.
use super::*;
use std::io::Read;

#[test]
#[ignore = "Prepare a disposable read-only service cache fixture; never user data"]
fn prepare_cached_service_fixture() {
    let source = PathBuf::from(std::env::var_os("APFSEARCH_ACCEPTANCE_CACHE").unwrap());
    let output = PathBuf::from(std::env::var_os("APFSEARCH_SERVICE_FIXTURE_OUTPUT").unwrap());
    std::fs::create_dir(&output).expect("Fixture output must be a new directory");
    let bytes = std::fs::read(&source).unwrap();
    assert_eq!(&bytes[..8], b"APFMAP03");
    let number = |offset| u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
    let store = IndexStore::open(&output.join("index.sqlite")).unwrap();
    for (key, value) in [
        ("generation", json!(number(8))),
        ("revision", json!(number(16))),
        ("content_revision", json!(number(24))),
        ("cache_base_generation", json!(number(8))),
        ("cache_base_revision", json!(number(16))),
        ("offline", json!(true)),
        ("cache_dirty", json!(false)),
        ("roots", json!([])),
        ("watch_enabled", json!(false)),
    ] {
        store.set(key, &value).unwrap();
    }
    drop(store);
    let destination = output.join("index.snapshot.bin");
    std::fs::copy(&source, &destination).unwrap();
    let sections = destination.with_extension("sections");
    std::fs::create_dir(&sections).unwrap();
    for part in std::fs::read_dir(source.with_extension("sections")).unwrap() {
        let part = part.unwrap();
        assert!(part.file_type().unwrap().is_file());
        // Sections are immutable and publication uses rename, so the fixture
        // can share inodes without duplicating gigabytes or mutating the input.
        std::fs::hard_link(part.path(), sections.join(part.file_name())).unwrap();
    }
    println!(
        "Prepared offline metadata-cache fixture. SQLite rows are intentionally empty: use only for query/UI timing, never recovery, content or file-operation acceptance."
    );
}

// Public Darwin task port; use the C ABI directly for this test-only probe.
unsafe extern "C" {
    #[link_name = "mach_task_self_"]
    static SELF_TASK: libc::mach_port_t;
}
fn virtual_memory() -> Value {
    // TASK_VM_INFO's public rev0 ABI: 18 aligned 64-bit slots, with two
    // integer_t fields sharing slot 1. Newer kernel revisions retain this prefix.
    let mut values = [0u64; 18];
    let mut count = 36;
    // SAFETY: task_info writes at most count natural_t words to aligned storage.
    let status = unsafe { libc::task_info(SELF_TASK, 22, values.as_mut_ptr().cast(), &mut count) };
    if status != 0 || count < 36 {
        return json!(null);
    }
    json!({"resident_peak_bytes":values[3],"internal_bytes":values[6],"external_bytes":values[8],
        "compressed_bytes":values[15],"compressed_peak_bytes":values[16],"compressed_lifetime_bytes":values[17]})
}
fn usage() -> Value {
    let mut process = std::mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
    let mut resource = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: both public APIs write the indicated structure into aligned storage.
    unsafe {
        assert_eq!(
            libc::proc_pid_rusage(
                std::process::id() as i32,
                libc::RUSAGE_INFO_V4,
                process.as_mut_ptr().cast()
            ),
            0
        );
        assert_eq!(libc::getrusage(libc::RUSAGE_SELF, resource.as_mut_ptr()), 0);
        let process = process.assume_init();
        let resource = resource.assume_init();
        let cpu = |time: libc::timeval| time.tv_sec as f64 + time.tv_usec as f64 / 1e6;
        json!({"virtual_memory":virtual_memory(),"physical_footprint_bytes":process.ri_phys_footprint,
            "peak_physical_footprint_bytes":process.ri_lifetime_max_phys_footprint,
            "resident_bytes":process.ri_resident_size, "pageins":process.ri_pageins,
            "minor_faults":resource.ru_minflt, "major_faults":resource.ru_majflt,
            "cpu_seconds":cpu(resource.ru_utime)+cpu(resource.ru_stime),
            "disk_bytes_read":process.ri_diskio_bytesread,
            "disk_bytes_written":process.ri_diskio_byteswritten,
            "logical_writes":process.ri_logical_writes})
    }
}

#[test]
#[ignore = "Explicit immutable acceptance cache required; no filesystem scan"]
fn same_corpus_resource_profile() {
    let path = PathBuf::from(
        std::env::var_os("APFSEARCH_ACCEPTANCE_CACHE")
            .expect("Supply an immutable benchmark cache"),
    );
    let mut header = [0u8; 24];
    std::fs::File::open(&path)
        .unwrap()
        .read_exact(&mut header)
        .unwrap();
    let generation = u64::from_le_bytes(header[8..16].try_into().unwrap());
    let revision = u64::from_le_bytes(header[16..24].try_into().unwrap());
    let directory = tempfile::tempdir().unwrap();
    let engine = SearchEngine::open(&directory.path().join("empty.sqlite")).unwrap();
    engine.refresh(true).unwrap();
    engine.offline.store(true, Ordering::Relaxed);
    engine.preferences.write().unwrap()["_snapshot_query_time_millis"] =
        json!(1_750_000_000_000i64);
    let before = usage();
    let started = Instant::now();
    let (snapshot, _) =
        snapshot_cache::read(&path, generation, revision).expect("Prepared corpus must validate");
    let restore_ms = started.elapsed().as_secs_f64() * 1000.;
    let entries = snapshot.len();
    engine.snapshot.store(Arc::new(snapshot));
    let restored = usage();
    let cases = [
        ("", 0, json!(null)),
        ("a", 0, json!(null)),
        ("crossover", 0, json!(null)),
        ("report", 0, json!(null)),
        ("报告", 0, json!(null)),
        ("café", 0, json!(null)),
        ("case:CrossOver", 0, json!(null)),
        ("path:/Applications", 0, json!(null)),
        ("regex:^report.*", 0, json!(null)),
        ("size:>10mb", 0, json!(null)),
        ("dm:2020..2025", 0, json!(null)),
        ("ext:pdf", 0, json!(null)),
        ("a !ext:pdf", 0, json!(null)),
        ("report | image", 0, json!(null)),
        ("a", 10_000, json!(null)),
        (
            "ext:pdf",
            0,
            json!([{"field":"size","ascending":false},{"field":"modified","ascending":false}]),
        ),
        ("path:.app/Contents", 0, json!(null)),
        ("path:目录", 0, json!(null)),
        ("case:path:/Applications", 0, json!(null)),
        (
            r"path:regex:^/Applications/[^/]+\.app/Contents/",
            0,
            json!(null),
        ),
        (
            r#"path:regex:"/(?<part>[^/]+)/\k<part>(?=/|$)""#,
            10_000,
            json!(null),
        ),
    ];
    let mut results = Vec::new();
    for (case, (text, offset, sort)) in cases.into_iter().enumerate() {
        let request = json!({"op":"query","text":text,"offset":offset,"sort":sort,"limit":200});
        let mut samples = [Vec::new(), Vec::new()];
        let mut expected = None;
        let repetitions = std::env::var("APFSEARCH_ACCEPTANCE_REPETITIONS")
            .ok()
            .map(|value| value.parse::<usize>().unwrap())
            .unwrap_or(25);
        assert!(repetitions > 0);
        for _ in 0..repetitions {
            derived_cache::clear();
            for times in &mut samples {
                let start = Instant::now();
                let response = engine.call(request.clone());
                times.push(start.elapsed().as_secs_f64() * 1000.);
                assert_eq!(response["success"], true, "Acceptance case {case} failed");
                let canonical = json!({"total":response["total"],"rows":response["rows"]});
                let digest = blake3::hash(&serde_json::to_vec(&canonical).unwrap())
                    .to_hex()
                    .to_string();
                if let Some(expected) = &expected {
                    assert_eq!(expected, &digest);
                } else {
                    expected = Some(digest);
                }
            }
        }
        let stats = |times: &mut Vec<f64>| {
            times.sort_by(f64::total_cmp);
            json!({"median_ms":times[times.len()/2],"p95_ms":times[(times.len()*95).div_ceil(100)-1],"samples_ms":times})
        };
        results.push(json!({"case":case,"digest":expected,"uncached":stats(&mut samples[0]),"warm":stats(&mut samples[1])}));
    }
    let after_queries = usage();
    // Canonical key ordering keeps this fingerprint independent of struct field
    // order. No rows, paths or personal query strings are logged.
    let mut digest = blake3::Hasher::new();
    for entry in engine.snapshot.load().visible_entries() {
        let row = serde_json::to_value(entry).unwrap();
        digest.update(&serde_json::to_vec(&row).unwrap());
    }
    let after_digest = usage();
    println!(
        "{}",
        json!({"scope":"Same immutable metadata corpus; production restore and query entry points; excludes XPC, AppKit and event reconciliation. Clean mapped pages must be assessed using resident memory and faults as well as physical footprint.",
        "entries":entries,"restore_ms":restore_ms,"metadata_digest":digest.finalize().to_hex().to_string(),
        "before":before,"restored":restored,"after_queries":after_queries,"after_digest":after_digest,"queries":results})
    );
}
