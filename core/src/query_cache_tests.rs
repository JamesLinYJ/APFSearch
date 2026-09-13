use super::*;
use scanner::ScannedFile;
fn fixture(count: usize) -> (tempfile::TempDir, Arc<SearchEngine>, Vec<ScannedFile>) {
    let temporary = tempfile::tempdir().unwrap();
    let engine = SearchEngine::open(&temporary.path().join("index.sqlite")).unwrap();
    let files: Vec<_> = (0..count)
        .map(|index| {
            let name = format!(
                "item{:05}.{}",
                index % 37,
                if index % 2 == 0 { "txt" } else { "pdf" }
            );
            ScannedFile {
                path: format!("/query-cache-fixture/Group{:05}/{name}", index),
                name,
                extension: if index % 2 == 0 {
                    "txt".into()
                } else {
                    "pdf".into()
                },
                size: (index % 19) as u64,
                modified: index as i64,
                changed: index as i64,
                created: index as i64 / 3,
                modified_ns: index as i64 * 1_000_000_000,
                changed_ns: index as i64 * 1_000_000_000,
                is_dir: index % 7 == 0,
                is_symlink: false,
                link_count: None,
                file_id: index as u64 + 1,
                parent_id: index as u64 + 1000,
                volume_id: "synthetic-query-cache".into(),
                flags: 0,
            }
        })
        .collect();
    engine.index_store.lock().unwrap().batch(&files, 1).unwrap();
    engine.refresh(false).unwrap();
    (temporary, engine, files)
}
fn rows(response: &Value) -> Vec<String> {
    assert_eq!(response["success"], true, "{response}");
    response["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["path"].as_str().unwrap().into())
        .collect()
}
#[test]
fn broad_multicolumn_orders_are_reused_and_incrementally_updated() {
    let (_temporary, engine, mut files) = fixture(12_050);
    let sort = json!([{"field":"path","ascending":false},{"field":"name","ascending":true}]);
    let request = json!({"op":"query","text":"","offset":6010,"limit":200,"sort":sort});
    let spec = result_order::ResultOrder::parse(&sort).unwrap();
    engine
        .snapshot
        .load()
        .result_order(&spec, &AtomicBool::new(false))
        .unwrap();
    let first = engine.call(request.clone());
    let spec = result_order::ResultOrder::parse(&sort).unwrap();
    let old = engine.snapshot.load_full();
    let order = old.cached_order(&spec).unwrap();
    assert_eq!(rows(&first), rows(&engine.call(request.clone())));
    assert!(Arc::ptr_eq(&order, &old.cached_order(&spec).unwrap()));
    let anchor = first["rows"][7]["path"].as_str().unwrap();
    let anchored = engine.call(json!({"op":"query","text":"","offset":6010,"limit":200,"sort":sort,"anchor_path":anchor,"anchor_delta":7}));
    assert_eq!(anchored["anchor_index"], 6017);
    assert_eq!(rows(&anchored), rows(&first));
    files[0].path = "/query-cache-fixture/Group99999/new.txt".into();
    files[0].name = "new.txt".into();
    // Use metadata changes to existing rows as well as a new path; all cached
    // sort fields, not just the name ordering, must be updated.
    files[6015].size += 20000;
    engine
        .index_store
        .lock()
        .unwrap()
        .batch(&[files[0].clone(), files[6015].clone()], 2)
        .unwrap();
    engine.refresh(false).unwrap();
    let current = engine.snapshot.load_full();
    assert!(!Arc::ptr_eq(&order, &current.cached_order(&spec).unwrap()));
    let mut independently_sorted: Vec<_> = current.visible_entries().collect();
    independently_sorted.sort_by(|a, b| spec.compare(a, b));
    let expected: Vec<_> = independently_sorted[6010..6210]
        .iter()
        .map(|file| file.path.clone())
        .collect();
    assert_eq!(rows(&engine.call(request)), expected);
    assert!(Arc::ptr_eq(&order, &old.cached_order(&spec).unwrap()));
}
#[test]
fn match_cache_keeps_preferences_generations_and_pagination_distinct() {
    let (_temporary, engine, files) = fixture(12_050);
    let query = json!({"op":"query","text":"ext:txt","limit":30});
    let initial = engine.call(query.clone());
    assert_eq!(initial["total"], 6025);
    assert_eq!(rows(&initial), rows(&engine.call(query.clone())));
    let token = engine.call(json!({"op":"retain_snapshot"}))["snapshot_lease"].clone();
    engine.call(json!({"op":"preferences","set":{"exclusions":["ext:txt"]}}));
    assert_eq!(engine.call(query.clone())["total"], 0);
    assert_eq!(
        engine.call(json!({"op":"query","text":"ext:txt","snapshot_lease":token}))["total"],
        6025
    );
    engine
        .call(json!({"op":"preferences","set":{"exclusions":[],"macros":{"selected":"ext:pdf"}}}));
    assert_eq!(
        engine.call(json!({"op":"query","text":"selected:"}))["total"],
        6025
    );
    engine.call(json!({"op":"preferences","set":{"macros":{"selected":"ext:txt"}}}));
    let before = rows(&engine.call(json!({"op":"query","text":"selected:","limit":20})));
    assert!(before.iter().all(|path| path.ends_with(".txt")));
    let snapshot = engine.snapshot.load_full();
    engine
        .index_store
        .lock()
        .unwrap()
        .connection
        .execute("DELETE FROM files WHERE path=?1", [&files[0].path])
        .unwrap();
    engine
        .index_store
        .lock()
        .unwrap()
        .set("revision", &json!(999))
        .unwrap();
    engine.refresh(false).unwrap();
    assert_eq!(engine.call(query)["total"], 6024);
    assert_eq!(snapshot.len(), 12050);
}
#[test]
fn string_columns_share_equivalent_folds_and_parent_names() {
    let (_temporary, engine, _) = fixture(20);
    let snapshot = engine.snapshot.load_full();
    for file in snapshot.visible_entries() {
        assert!(Arc::ptr_eq(&file.folded_path, &file.search_path));
        assert!(Arc::ptr_eq(&file.folded_name, &file.search_name));
    }
    let mut entries: Vec<_> = snapshot.visible_entries().cloned().collect();
    entries[1].path = "/query-cache-fixture/Group00000/other.txt".into();
    let prepared = SearchSnapshot::new(entries, 99);
    assert!(Arc::ptr_eq(
        &prepared.entries[0].parent,
        &prepared.entries[1].parent
    ));
}
#[test]
fn chunked_sort_cancels_without_changing_comparator_semantics() {
    let cancelled = AtomicBool::new(false);
    let comparisons = std::cell::Cell::new(0);
    let mut values: Vec<u32> = (0..100_000).rev().collect();
    let result = result_order::sort_slots(
        &mut values,
        |first, second| {
            comparisons.set(comparisons.get() + 1);
            if comparisons.get() == 3000 {
                cancelled.store(true, Ordering::Relaxed);
            }
            first.cmp(&second)
        },
        &cancelled,
    );
    assert_eq!(result.unwrap_err(), "Query cancelled");
    assert!(comparisons.get() < 5000);
}

#[test]
fn selected_prefix_matches_full_sort_for_adversarial_input_and_boundaries() {
    for length in [0usize, 1, 2, 1023, 1024, 1025, 4096, 11_011] {
        let patterns = [
            (0..length as u32).collect::<Vec<_>>(),
            (0..length as u32).rev().collect(),
            (0..length as u32)
                .map(|value| (value * 104729) % 97)
                .collect(),
            (0..length as u32)
                .map(|value| value.min(length as u32 - value))
                .collect(),
        ];
        for original in patterns {
            let mut reference = original.clone();
            reference.sort_unstable();
            for end in [
                0,
                1,
                length / 2,
                length.saturating_sub(1),
                length,
                length + 100,
            ] {
                let mut actual = original.clone();
                result_order::select_prefix(
                    &mut actual,
                    end,
                    |a, b| a.cmp(&b),
                    &AtomicBool::new(false),
                )
                .unwrap();
                assert_eq!(
                    actual,
                    reference[..end.min(length)],
                    "length={length}, end={end}"
                );
            }
        }
    }
}
#[test]
fn first_path_page_reuses_path_index_and_anchor_id_is_only_a_hint() {
    let (_temporary, engine, _) = fixture(12_050);
    let sort = json!([{"field":"path","ascending":true},{"field":"name","ascending":false}]);
    let spec = result_order::ResultOrder::parse(&sort).unwrap();
    let request = json!({"op":"query","text":"","sort":sort,"offset":500,"limit":200});
    let first = engine.call(request.clone());
    assert!(Arc::ptr_eq(
        &engine.snapshot.load().cached_order(&spec).unwrap(),
        &engine.snapshot.load().path_order
    ));
    assert_eq!(rows(&first), rows(&engine.call(request)));
    let anchor = &first["rows"][30];
    for id in [
        anchor["id"].clone(),
        json!(-1),
        json!(9999999),
        first["rows"][0]["id"].clone(),
    ] {
        let anchored=engine.call(json!({"op":"query","text":"","sort":sort,"offset":100,"limit":200,"anchor_path":anchor["path"],"anchor_id":id,"anchor_delta":30}));
        assert_eq!(anchored["anchor_index"], 530);
        assert_eq!(rows(&anchored), rows(&first));
    }
}

fn assert_path_orders(snapshot: &SearchSnapshot) {
    for ascending in [true, false] {
        for secondary in [
            "name",
            "path",
            "extension",
            "size",
            "modified",
            "created",
            "is_dir",
        ] {
            for secondary_ascending in [true, false] {
                let spec = result_order::ResultOrder::parse(&json!([
                    {"field":"path","ascending":ascending},
                    {"field":secondary,"ascending":secondary_ascending}
                ]))
                .unwrap();
                let actual = snapshot
                    .result_order(&spec, &AtomicBool::new(false))
                    .unwrap();
                let mut reference: Vec<u32> = snapshot.live.iter().collect();
                reference.sort_by(|a, b| {
                    spec.compare(
                        &snapshot.entries[*a as usize],
                        &snapshot.entries[*b as usize],
                    )
                });
                assert_eq!(
                    actual.as_ref(),
                    &reference,
                    "{ascending} {secondary} {secondary_ascending}"
                );
            }
        }
        let spec =
            result_order::ResultOrder::parse(&json!([{"field":"path","ascending":ascending}]))
                .unwrap();
        let actual = snapshot
            .result_order(&spec, &AtomicBool::new(false))
            .unwrap();
        let mut reference: Vec<u32> = snapshot.live.iter().collect();
        reference.sort_by(|a, b| {
            spec.compare(
                &snapshot.entries[*a as usize],
                &snapshot.entries[*b as usize],
            )
        });
        assert_eq!(actual.as_ref(), &reference);
    }
    let mut expected_ties = roaring::RoaringBitmap::new();
    for pair in snapshot.path_order.windows(2) {
        if query::natural_cmp_folded(
            &snapshot.entries[pair[0] as usize].folded_path,
            &snapshot.entries[pair[1] as usize].folded_path,
        )
        .is_eq()
        {
            expected_ties.insert(pair[0]);
            expected_ties.insert(pair[1]);
        }
    }
    assert_eq!(snapshot.path_ties.as_ref(), &expected_ties);
}

#[test]
fn path_index_refines_equal_keys_and_preserves_order_across_updates() {
    let (_temporary, engine, _) = fixture(80);
    let mut entries: Vec<_> = engine.snapshot.load().visible_entries().cloned().collect();
    let paths = [
        "/folder/a2",
        "/FOLDER/A2",
        "/folder/a02",
        "/folder/a002",
        "/folder/b2",
        "/folder/B2",
        "/folder/a3",
        "/folder/A3",
    ];
    for (index, file) in entries.iter_mut().enumerate() {
        file.path = format!("{}/{}", paths[index % paths.len()], index / paths.len());
        // Imported rows need not have a basename-coherent name. A path tie
        // cannot erase explicitly requested secondary sort descriptors.
        file.name = format!("unrelated{:03}", 80 - index);
        file.size = ((index * 37) % 31) as u64;
    }
    let original = SearchSnapshot::new(entries, 10);
    assert!(!original.path_ties.is_empty());
    assert_path_orders(&original);
    let mut current = original;
    for (step, replacement_path) in [
        Some("/totally/new/path"),
        None,
        Some("/folder/b2/0"),
        Some("/folder/B2/0"),
        None,
    ]
    .into_iter()
    .enumerate()
    {
        let slot = step;
        let file = current.entries[slot].as_ref();
        let replacement = replacement_path.map(|path| {
            let mut replacement = file.clone();
            replacement.path = path.into();
            replacement.name = format!("new{step}");
            replacement.size += 200;
            replacement
        });
        let updated =
            SearchSnapshot::from_changes(vec![(file.id, replacement)], 11 + step as u64, &current)
                .unwrap();
        assert_path_orders(&updated);
        assert_path_orders(&current);
        current = updated;
    }
    let error = current.result_order(
        &result_order::ResultOrder::parse(&json!([{"field":"path","ascending":false}])).unwrap(),
        &AtomicBool::new(true),
    );
    assert_eq!(error.unwrap_err(), "Query cancelled");
}

#[test]
fn build_sized_delta_keeps_unchanged_entries_and_postings_shared() {
    let (_temporary, engine, _) = fixture(1);
    let seed = engine.snapshot.load().entries[0].as_ref().clone();
    let entries = (0..50_000)
        .map(|index| {
            let mut file = seed.clone();
            file.id = index + 1;
            file.path = format!("/large-delta/item{index}.txt");
            file.name = format!("item{index}.txt");
            file
        })
        .collect();
    let previous = SearchSnapshot::new(entries, 1);
    let changes = previous
        .visible_entries()
        .take(6000)
        .map(|file| {
            let mut file = file.clone();
            file.size += 1;
            (file.id, Some(file))
        })
        .collect();
    let metadata = SearchSnapshot::from_changes(changes, 2, &previous).unwrap();
    assert!(Arc::ptr_eq(&metadata.trigrams, &previous.trigrams));
    assert!(Arc::ptr_eq(&metadata.path_order, &previous.path_order));
    assert!(Arc::ptr_eq(&metadata.name_order, &previous.name_order));
    assert!(Arc::ptr_eq(
        &metadata.entries[30_000],
        &previous.entries[30_000]
    ));
    assert!(Arc::ptr_eq(
        &metadata.entries[0].folded_path,
        &previous.entries[0].folded_path
    ));
    let changes = metadata
        .visible_entries()
        .take(6000)
        .map(|file| {
            let mut file = file.clone();
            file.path = format!("/renamed-parent/{}", file.name);
            (file.id, Some(file))
        })
        .collect();
    let renamed = SearchSnapshot::from_changes(changes, 3, &metadata).unwrap();
    assert!(Arc::ptr_eq(&renamed.trigrams, &metadata.trigrams));
    assert!(Arc::ptr_eq(
        &renamed.entries[30_000],
        &previous.entries[30_000]
    ));
    for value in [
        json!([{"field":"name"}]),
        json!([{"field":"path"},{"field":"size","ascending":false}]),
    ] {
        let order = result_order::ResultOrder::parse(&value).unwrap();
        let actual = renamed
            .result_order(&order, &AtomicBool::new(false))
            .unwrap();
        let mut reference: Vec<u32> = renamed.live.iter().collect();
        reference.sort_by(|a, b| {
            order.compare(&renamed.entries[*a as usize], &renamed.entries[*b as usize])
        });
        assert_eq!(actual.as_ref(), &reference);
    }
    assert_eq!(previous.entries[0].size + 1, metadata.entries[0].size);
    assert!(previous.entries[0].path.starts_with("/large-delta/"));
}

#[test]
fn storage_delta_limit_uses_snapshot_budget_without_truncating_changes() {
    let (_temporary, engine, mut files) = fixture(2500);
    let revision = engine.published_revision.load(Ordering::Relaxed);
    for file in &mut files {
        file.size += 7;
    }
    let mut store = engine.index_store.lock().unwrap();
    store.batch(&files, 2).unwrap();
    assert!(store.changes_since(revision, 2000).unwrap().is_none());
    let changes = store
        .changes_since(revision, SearchSnapshot::incremental_limit(50_000))
        .unwrap()
        .unwrap();
    assert_eq!(changes.len(), files.len());
    for ((_, file), expected) in changes.iter().zip(&files) {
        assert_eq!(file.as_ref().unwrap().size, expected.size);
    }
}

#[test]
#[ignore = "Read-only large index profile; requires FILESEARCH_PROFILE_INDEX"]
fn profile_new_queries_and_in_memory_updates_on_existing_index() {
    let path = PathBuf::from(
        std::env::var("FILESEARCH_PROFILE_INDEX")
            .expect("Set FILESEARCH_PROFILE_INDEX to existing isolated index"),
    );
    let connection = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .unwrap();
    connection.execute_batch("PRAGMA query_only=ON").unwrap();
    let store = IndexStore {
        connection,
        cache_path: path.with_extension("snapshot.bin"),
    };
    let restore_started = Instant::now();
    let entries = store.entries().unwrap();
    drop(store);
    let count = entries.len();
    let snapshot = SearchSnapshot::new(entries, 1000);
    let restore_ms = restore_started.elapsed().as_secs_f64() * 1000.;
    let temporary = tempfile::tempdir().unwrap();
    let engine = SearchEngine::open(&temporary.path().join("empty/index.sqlite")).unwrap();
    // Complete the empty scaffold's disposable cache before injecting the
    // read-only large snapshot, so the startup cache worker cannot persist it.
    engine.refresh(true).unwrap();
    engine.snapshot.store(Arc::new(snapshot));
    let mut standard_queries = Vec::new();
    for (name, query) in [
        ("empty_name", json!({"text":""})),
        (
            "empty_path_name",
            json!({"text":"","sort":[{"field":"path","ascending":true},{"field":"name","ascending":true}]}),
        ),
        ("short_a", json!({"text":"a"})),
        ("substring_swift", json!({"text":"swift"})),
        ("extension_pdf", json!({"text":"ext:pdf"})),
    ] {
        let mut request = query.clone();
        request["op"] = json!("query");
        request["limit"] = json!(200);
        let start = Instant::now();
        let response = engine.call(request);
        let call_ms = start.elapsed().as_secs_f64() * 1000.;
        assert_eq!(response["success"], true);
        eprintln!("first standard {name}: {} ms", response["elapsed_ms"]);
        standard_queries.push(json!({"name":name,"request":query,"total":response["total"],"elapsed_ms":response["elapsed_ms"],"call_ms":call_ms,"rows":response["rows"]}));
    }
    let mut first_queries = Vec::new();
    for text in [
        "b",
        "e",
        "t",
        "z",
        "x",
        "ab",
        "10",
        "sw",
        "te",
        "ext:json",
        "ext:rs",
        "ext:png",
        "a ext:pdf",
    ] {
        let result = engine.call(json!({"op":"query","text":text,"limit":200}));
        assert_eq!(result["success"], true, "{result}");
        first_queries
            .push(json!({"text":text,"elapsed_ms":result["elapsed_ms"],"total":result["total"]}));
        eprintln!("new text {text}: {} ms", result["elapsed_ms"]);
    }
    let previous = engine.snapshot.load_full();
    let original = previous
        .visible_entries()
        .find(|file| !file.is_dir)
        .unwrap();
    let mut changed = original.clone();
    changed.name = "qqfixture-only-memory-update.txt".into();
    changed.path = format!("{}/{}", changed.parent, changed.name);
    changed.extension = "txt".into();
    changed.size += 1;
    let started = Instant::now();
    let updated =
        SearchSnapshot::from_changes(vec![(changed.id, Some(changed))], 1001, &previous).unwrap();
    let publish_ms = started.elapsed().as_secs_f64() * 1000.;
    engine.snapshot.store(Arc::new(updated));
    let mut after_update = Vec::new();
    for request in [
        json!({"text":"a"}),
        json!({"text":"ext:pdf"}),
        json!({"text":"qqfixture-only-memory-update"}),
        json!({"text":"","sort":[{"field":"path","ascending":true},{"field":"name","ascending":true}]}),
    ] {
        let mut request = request;
        request["op"] = json!("query");
        request["limit"] = json!(200);
        let result = engine.call(request.clone());
        assert_eq!(result["success"], true, "{result}");
        if request["text"] == "qqfixture-only-memory-update" {
            assert_eq!(result["total"], 1);
        }
        after_update.push(
            json!({"request":request,"elapsed_ms":result["elapsed_ms"],"total":result["total"]}),
        );
        eprintln!(
            "after update {}: {} ms",
            request["text"], result["elapsed_ms"]
        );
    }
    let mut bulk_updates = Vec::new();
    for rename in [false, true] {
        let before = engine.snapshot.load_full();
        let changes = before
            .visible_entries()
            .take(6000)
            .map(|file| {
                let mut file = file.clone();
                file.size += 1;
                if rename {
                    file.name = format!("bulk-memory-{}", file.id);
                    file.path = format!("{}/{}", file.parent, file.name);
                }
                (file.id, Some(file))
            })
            .collect();
        let start = Instant::now();
        let updated =
            SearchSnapshot::from_changes(changes, before.generation + 1, &before).unwrap();
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.;
        assert!(Arc::ptr_eq(&before.entries[6001], &updated.entries[6001]));
        engine.snapshot.store(Arc::new(updated));
        let response = engine.call(
            json!({"op":"query","text":"","limit":200,"sort":[{"field":"path"},{"field":"name"}]}),
        );
        assert_eq!(response["success"], true);
        bulk_updates.push(json!({"changed_entries":6000,"renamed":rename,"incremental_ms":elapsed_ms,"first_path_page_ms":response["elapsed_ms"],"unchanged_object_reused":true}));
        eprintln!(
            "bulk 6000 rename={rename}: {elapsed_ms} ms; first path page {} ms",
            response["elapsed_ms"]
        );
    }
    let snapshot = engine.snapshot.load();
    let order_bytes =
        (snapshot.path_order.len() + snapshot.entries.len()) * std::mem::size_of::<u32>();
    let report = json!({"source":path,"entries":count,"scope":"SQLite index read only; first unique query texts; after-update tests mutate only an in-memory SearchSnapshot. Existing copied SQLite and actual filesystem contents are unchanged. A separate empty temporary engine database is used only for API scaffolding. No FSEvents/XPC/GUI. restore_ms includes SQL rows and snapshot preparation, excluding service initialization.","restore_ms":restore_ms,"path_order_and_rank_bytes":order_bytes,"path_equal_key_slots":snapshot.path_ties.len(),"standard_queries":standard_queries,"in_memory_update_ms":publish_ms,"first_unique_queries":first_queries,"first_queries_after_update":after_update,"bulk_updates":bulk_updates});
    let output = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("validation/live-index-path-order-and-updates.json");
    std::fs::write(output, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
}

#[derive(Default)]
struct FileReadTrace {
    selects: std::sync::atomic::AtomicUsize,
    full_reads: std::sync::atomic::AtomicUsize,
}
unsafe extern "C" fn count_file_reads(
    event: u32,
    context: *mut c_void,
    _statement: *mut c_void,
    sql: *mut c_void,
) -> i32 {
    if event == rusqlite::ffi::SQLITE_TRACE_STMT && !sql.is_null() {
        // This box lives until the synchronous refresh ends and trace is removed.
        let trace = unsafe { &*context.cast::<FileReadTrace>() };
        let sql = unsafe { CStr::from_ptr(sql.cast()) }.to_bytes();
        if sql.starts_with(b"SELECT f.id,") {
            trace.selects.fetch_add(1, Ordering::Relaxed);
            if sql.windows(b"AND (1)".len()).any(|part| part == b"AND (1)") {
                trace.full_reads.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    0
}

#[test]
fn incremental_refresh_restores_a_reappeared_id_without_changing_publication_semantics() {
    let (_temporary, engine, files) = fixture(3);
    {
        let store = engine.index_store.lock().unwrap();
        store
            .connection
            .execute(
                "UPDATE files SET accessible=0 WHERE path=?1",
                [&files[1].path],
            )
            .unwrap();
    }
    engine.refresh(false).unwrap();
    // A prepared cache/full reload omits inaccessible rows, including this old
    // middle ID. Reappearance appends a stable slot and remains incremental.
    let generation = engine.generation.load(Ordering::Relaxed);
    let compacted = SearchSnapshot::new(
        engine.index_store.lock().unwrap().entries().unwrap(),
        generation,
    );
    engine.snapshot.store(Arc::new(compacted));
    engine
        .index_store
        .lock()
        .unwrap()
        .batch(&files[1..2], 1)
        .unwrap();
    let mut trace = Box::<FileReadTrace>::default();
    {
        let store = engine.index_store.lock().unwrap();
        unsafe {
            assert_eq!(
                rusqlite::ffi::sqlite3_trace_v2(
                    store.connection.handle(),
                    rusqlite::ffi::SQLITE_TRACE_STMT,
                    Some(count_file_reads),
                    (&mut *trace as *mut FileReadTrace).cast(),
                ),
                rusqlite::ffi::SQLITE_OK
            );
        }
    }
    let publication = engine.refresh(false);
    {
        let store = engine.index_store.lock().unwrap();
        unsafe {
            rusqlite::ffi::sqlite3_trace_v2(
                store.connection.handle(),
                0,
                None,
                std::ptr::null_mut(),
            );
        }
    }
    publication.unwrap();
    assert_eq!(
        trace.selects.load(Ordering::Relaxed),
        1,
        "only the changed-ID SQL selection"
    );
    assert_eq!(
        trace.full_reads.load(Ordering::Relaxed),
        0,
        "no all-files SQL reload"
    );
    let diagnostic = engine.state.lock().unwrap()["last_refresh"].clone();
    assert_eq!(diagnostic["mode"], "incremental");
    assert!(diagnostic["reason"].is_null());
    assert_eq!(diagnostic["changes"], 1);
    assert_eq!(diagnostic["previous_slots"], 2);
    assert_eq!(diagnostic["generation"], generation + 1);
    assert_eq!(engine.snapshot.load().len(), 3);
    assert_eq!(
        engine.index_store.lock().unwrap().get("revision", json!(0)),
        diagnostic["target_revision"]
    );
}
