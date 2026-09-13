use super::*;
use std::os::unix::fs::MetadataExt;

#[test]
#[ignore = "Read-only existing million-row fixture; requires APFSEARCH_NUMERIC_PROFILE_INDEX"]
fn numeric_first_queries_on_existing_prepared_fixture() {
    let path = PathBuf::from(std::env::var("APFSEARCH_NUMERIC_PROFILE_INDEX").unwrap());
    let connection =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    connection.execute_batch("PRAGMA query_only=ON").unwrap();
    let store = IndexStore {
        connection,
        cache_path: path.with_extension("snapshot.bin"),
    };
    assert_eq!(store.get("benchmark_seed", json!(null)), json!(1_000_003));
    let before = std::fs::metadata(&store.cache_path).unwrap();
    let start = Instant::now();
    let (snapshot, _) = store
        .cache_read()
        .expect("Existing prepared fixture cache is required; no rewrite or full rebuild");
    let restore_ms = start.elapsed().as_secs_f64() * 1000.;
    assert_eq!(snapshot.len(), 1_000_000);
    let directory = tempfile::tempdir().unwrap();
    let engine = SearchEngine::open(&directory.path().join("empty.sqlite")).unwrap();
    engine.refresh(true).unwrap();
    engine.snapshot.store(Arc::new(snapshot));
    let clock = chrono::DateTime::from_timestamp_millis(chrono::Local::now().timestamp_millis())
        .unwrap()
        .with_timezone(&chrono::Local);
    engine.preferences.write().unwrap()["_snapshot_query_time_millis"] =
        json!(clock.timestamp_millis());
    let mut results = Vec::new();
    for text in [
        "size:>10mb",
        "size:>=11mb",
        "size:<=1kb",
        "size:=0",
        "size:!=10mb",
        "!size:>10mb",
        "dm:2020..2025",
        "dc:>=2024",
        "dm:7days",
        "a size:>20mb | ext:pdf",
        "size:unknown | dc:<2020",
    ] {
        let response = engine.call(json!({"op":"query","text":text,"limit":200}));
        assert_eq!(response["success"], true, "{text}: {response}");
        let snapshot = engine.snapshot.load();
        let query = query::parse_at(text, &HashMap::new(), clock).unwrap();
        let expected: roaring::RoaringBitmap = snapshot
            .live
            .iter()
            .filter(|slot| {
                query
                    .matches_available(&snapshot.entries[*slot as usize], None)
                    .unwrap()
            })
            .collect();
        assert_eq!(response["total"].as_u64(), Some(expected.len()), "{text}");
        assert_eq!(
            snapshot
                .exact_matches(&query, &AtomicBool::new(false))
                .unwrap()
                .unwrap(),
            expected,
            "{text}"
        );
        let expected_rows: Vec<i64> = snapshot
            .name_order
            .iter()
            .filter(|slot| expected.contains(**slot))
            .take(200)
            .map(|slot| snapshot.entries[*slot as usize].id)
            .collect();
        let actual_rows: Vec<i64> = response["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_i64().unwrap())
            .collect();
        assert_eq!(actual_rows, expected_rows, "{text}");
        eprintln!("numeric first {text}: {} ms", response["elapsed_ms"]);
        results.push(json!({"text":text,"core_ms":response["elapsed_ms"],"total":response["total"],"raw_response":response,"all_matching_ids_and_first_page_verified":true}));
    }
    let previous = engine.snapshot.load_full();
    let mut file = previous.entries[5678].as_ref().clone();
    file.size = 17 * 1024 * 1024;
    file.modified += 30;
    file.created += 30;
    let changed_id = file.id;
    let start = Instant::now();
    let updated = SearchSnapshot::from_changes(
        vec![(file.id, Some(file))],
        previous.generation + 1,
        &previous,
    )
    .unwrap();
    let update_ms = start.elapsed().as_secs_f64() * 1000.;
    engine.snapshot.store(Arc::new(updated));
    let mut after_update = Vec::new();
    for text in ["size:>10mb", "dm:7days", "dc:>=2024"] {
        let response = engine.call(json!({"op":"query","text":text,"limit":200}));
        assert_eq!(response["success"], true);
        let snapshot = engine.snapshot.load();
        let query = query::parse_at(text, &HashMap::new(), clock).unwrap();
        let expected: roaring::RoaringBitmap = snapshot
            .live
            .iter()
            .filter(|slot| {
                query
                    .matches_available(&snapshot.entries[*slot as usize], None)
                    .unwrap()
            })
            .collect();
        assert_eq!(response["total"].as_u64(), Some(expected.len()));
        let exact = snapshot
            .exact_matches(&query, &AtomicBool::new(false))
            .unwrap()
            .unwrap();
        assert_eq!(exact, expected);
        after_update.push(json!({"text":text,"core_ms":response["elapsed_ms"],"total":response["total"],"raw_response":response,"all_matching_ids_verified":true}));
        eprintln!("numeric after update {text}: {} ms", response["elapsed_ms"]);
    }
    assert_eq!(previous.entries[5678].id, changed_id);
    let after = std::fs::metadata(&store.cache_path).unwrap();
    assert_eq!(
        (
            before.ino(),
            before.len(),
            before.mtime(),
            before.mtime_nsec()
        ),
        (after.ino(), after.len(), after.mtime(), after.mtime_nsec())
    );
    let report = json!({"database":path,"entries":previous.len(),"scope":"Existing isolated million-row prepared cache and SQLite opened read-only. No database copy, filesystem scan or prepared-cache rewrite. Queries and updates execute in memory; each initial text is distinct and post-update results use a new generation. Full ordinary matcher verifies result bitmaps and name-ordered first pages. No XPC or GUI.","prepared_cache_restore_ms":restore_ms,"nominal_numeric_value_bytes":previous.entries.len()*3*8,"cache_inode_size_mtime_unchanged":true,"initial_queries":results,"single_numeric_update_ms":update_ms,"first_queries_after_update":after_update});
    std::fs::write(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("validation/numeric-first-query-million.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
}
