use filesearch_core::{index_store::IndexedFile, query, SearchEngine};
use serde_json::{json, Value};
use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};
fn entry(name: &str) -> IndexedFile {
    let mut e:IndexedFile=serde_json::from_value(json!({"id":1,"path":format!("/Users/样本/{name}"),"name":name,"extension":Path::new(name).extension().unwrap_or_default().to_string_lossy(),"size":2048,"modified":1726185600_i64,"created":1726099200_i64,"changed":1726185600_i64,"is_dir":false,"is_symlink":false,"file_id":12,"parent_id":3,"volume_id":"APFS-test","flags":0,"properties":{"width":1920,"height":1080}})).unwrap();
    e.prepare();
    e
}
#[test]
fn query_grammar_unicode_and_pcre() {
    let e = entry("Straße café Report12.txt");
    for s in [
        "strasse",
        "cafe\u{301}",
        "report ext:txt",
        "<report|missing> !ext:pdf",
        "(missing OR report) AND file:",
        "size:>=2kb size:<3kb",
        "regex:\"Report(?=12)\\d+\\.txt$\"",
        "path:样本",
        "name:*Report??.txt",
        "width:>=1920 height:1080",
        "\"café Report\"",
    ] {
        assert!(
            query::parse(s, &HashMap::new())
                .unwrap_or_else(|err| panic!("{s}: {err}"))
                .matches(&e, None)
                .unwrap(),
            "{s}"
        );
    }
    for s in [
        "case:report",
        "ext:pdf",
        "NOT report",
        "<missing|other>",
        "size:>2kb",
        "regex:\"Report(?=99)\"",
    ] {
        assert!(
            !query::parse(s, &HashMap::new())
                .unwrap()
                .matches(&e, None)
                .unwrap(),
            "{s}"
        )
    }
    for s in [
        "unknown:foo",
        "size:garbage",
        "regex:\"[\"",
        "foo |",
        "(foo",
        "foo)",
        "\"unterminated",
    ] {
        assert!(query::parse(s, &HashMap::new()).is_err(), "{s}")
    }
    assert!(query::natural_cmp("file2", "file10").is_lt());
}
#[test]
fn content_negation_candidates_are_conservative() {
    let e = entry("report.txt");
    let macros = HashMap::from([("docs".to_string(), "ext:txt;md".to_string())]);
    assert!(query::parse("docs: !content:missing", &macros)
        .unwrap()
        .may_match_without_content(&e)
        .unwrap());
    assert!(!query::parse("ext:pdf !content:missing", &macros)
        .unwrap()
        .may_match_without_content(&e)
        .unwrap());
    assert!(query::parse("content:\"needle two\"", &macros)
        .unwrap()
        .matches(&e, Some("NEEDLE two adjacent"))
        .unwrap());
    assert!(!query::parse("content:\"needle two\"", &macros)
        .unwrap()
        .matches(&e, Some("needle unrelated two"))
        .unwrap());
    let cyclic = HashMap::from([("x".into(), "x:".into())]);
    assert!(query::parse("x:", &cyclic).is_err());
}
fn setup() -> (tempfile::TempDir, Arc<SearchEngine>, String) {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("files");
    std::fs::create_dir(&dir).unwrap();
    let engine = SearchEngine::open(&t.path().join("db/index.sqlite")).unwrap();
    (
        t,
        engine,
        std::fs::canonicalize(dir)
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    )
}
fn call(engine: &Arc<SearchEngine>, request: Value) -> Value {
    let response = engine.call(request);
    assert_eq!(response["success"], true, "{response}");
    response
}
fn scan(engine: &Arc<SearchEngine>, root: &str) {
    call(
        engine,
        json!({"op":"scan","roots":[root],"wait":true,"watch":false}),
    );
}
#[test]
fn disk_index_query_and_restart_consistency() {
    let (t, e, root) = setup();
    std::fs::write(format!("{root}/报告2.txt"), "needle two").unwrap();
    std::fs::write(format!("{root}/报告10.txt"), "other").unwrap();
    std::fs::hard_link(format!("{root}/报告2.txt"), format!("{root}/hardlink.txt")).unwrap();
    std::os::unix::fs::symlink("报告2.txt", format!("{root}/link.txt")).unwrap();
    scan(&e, &root);
    let result = call(&e, json!({"op":"query","text":"报告","limit":1}));
    assert_eq!(result["total"], 2);
    assert_eq!(result["rows"][0]["name"], "报告2.txt");
    let page = call(
        &e,
        json!({"op":"query","text":"报告","offset":1,"generation":result["generation"]}),
    );
    assert_eq!(page["rows"][0]["name"], "报告10.txt");
    let content = call(
        &e,
        json!({"op":"query","text":"file: !symlink: content:\"needle two\""}),
    );
    assert_eq!(content["total"], 2);
    let dups = call(&e, json!({"op":"duplicates","mode":"content"}));
    assert_eq!(dups["hardlinks"].as_array().unwrap().len(), 1);
    let reopened = SearchEngine::open(&t.path().join("db/index.sqlite")).unwrap();
    assert_eq!(
        call(&reopened, json!({"op":"query","text":"type:symlink"}))["total"],
        1
    );
    std::fs::remove_file(format!("{root}/报告10.txt")).unwrap();
    scan(&e, &root);
    assert_eq!(call(&e, json!({"op":"query","text":"报告"}))["total"], 1);
}
#[test]
fn content_invalidation_and_offline_file_list() {
    let (_t, e, root) = setup();
    let path = format!("{root}/document.pdf");
    std::fs::write(&path, b"AAAA").unwrap();
    scan(&e, &root);
    call(
        &e,
        json!({"op":"put_content","path":path,"text":"first sentence","properties":{"pages":3}}),
    );
    assert_eq!(
        call(&e, json!({"op":"query","text":"content:first pages:3"}))["total"],
        1
    );
    std::thread::sleep(Duration::from_millis(5));
    std::fs::write(&path, b"BBBB").unwrap();
    scan(&e, &root);
    assert_eq!(
        call(&e, json!({"op":"query","text":"content:first"}))["total"],
        0
    );
    call(
        &e,
        json!({"op":"import_file_list","rows":[{"path":"/offline/movie2.mov","size":50},{"path":"/offline/movie10.mov","size":50}]}),
    );
    let result = call(&e, json!({"op":"query","text":"ext:mov size:50"}));
    assert_eq!(result["total"], 2);
    assert_eq!(result["offline"], true);
    assert_eq!(
        e.call(json!({"op":"scan","roots":[root]}))["success"],
        false
    );
}
#[test]
fn scope_changes_do_not_leave_previous_roots_searchable() {
    let (t, e, root) = setup();
    let other = t.path().join("other");
    std::fs::create_dir(&other).unwrap();
    std::fs::write(other.join("out.txt"), "outside").unwrap();
    std::fs::write(format!("{root}/in.txt"), "inside").unwrap();
    call(
        &e,
        json!({"op":"scan","roots":[root,other],"wait":true,"watch":false}),
    );
    assert_eq!(call(&e, json!({"op":"query","text":"ext:txt"}))["total"], 2);
    scan(&e, &root);
    assert_eq!(call(&e, json!({"op":"query","text":"ext:txt"}))["total"], 1);
}
#[test]
fn watcher_updates_renames_and_can_restart() {
    let (_t, e, root) = setup();
    call(&e, json!({"op":"scan","roots":[root],"watch":true}));
    for _ in 0..50 {
        let status = call(&e, json!({"op":"status"}));
        assert_ne!(status["state"], "error", "watcher startup failed: {status}");
        if status["state"] == "watching" {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    std::fs::write(format!("{root}/before.txt"), "one").unwrap();
    let until = std::time::Instant::now() + Duration::from_secs(6);
    while std::time::Instant::now() < until {
        if call(&e, json!({"op":"query","text":"before"}))["total"] == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(100))
    }
    assert_eq!(call(&e, json!({"op":"query","text":"before"}))["total"], 1);
    std::fs::rename(format!("{root}/before.txt"), format!("{root}/after.txt")).unwrap();
    let until = std::time::Instant::now() + Duration::from_secs(6);
    while std::time::Instant::now() < until {
        let r = call(&e, json!({"op":"query","text":"after"}));
        if r["total"] == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(100))
    }
    assert_eq!(call(&e, json!({"op":"query","text":"before"}))["total"], 0);
    scan(&e, &root);
    assert_eq!(call(&e, json!({"op":"query","text":"after"}))["total"], 1);
    call(&e, json!({"op":"stop"}));
}
#[test]
fn cache_dirty_transaction_wins_after_unpublished_batch() {
    use filesearch_core::{index_store::IndexStore, scanner};
    let (t, e, root) = setup();
    let path = format!("{root}/one.txt");
    std::fs::write(&path, "one").unwrap();
    scan(&e, &root);
    let dbpath = t.path().join("db/index.sqlite");
    let mut db = IndexStore::open(&dbpath).unwrap();
    assert!(db.cache_read().is_some());
    let extra = format!("{root}/two.txt");
    std::fs::write(&extra, "two").unwrap();
    db.batch(&[scanner::stat_entry(&extra).unwrap()], 123456)
        .unwrap();
    assert!(db.cache_is_dirty());
    let (recovered, _) = db
        .cache_read()
        .expect("the changed-id journal restores a dirty prepared cache");
    assert!(recovered.visible_entries().any(|entry| entry.path == extra));
    drop(db);
    let reopened = SearchEngine::open(&dbpath).unwrap();
    assert_eq!(
        call(&reopened, json!({"op":"query","text":"two"}))["total"],
        1
    );
}
#[test]
fn denied_regions_are_retained_but_not_searchable_then_recover() {
    use filesearch_core::index_store::IndexStore;
    let (t, e, root) = setup();
    std::fs::write(format!("{root}/secret.txt"), "secret").unwrap();
    scan(&e, &root);
    let mut db = IndexStore::open(&t.path().join("db/index.sqlite")).unwrap();
    db.finish(
        std::slice::from_ref(&root),
        std::slice::from_ref(&root),
        54321,
        9,
    )
    .unwrap();
    assert!(db.entries().unwrap().is_empty());
    assert_eq!(
        db.connection
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    drop(db);
    scan(&e, &root);
    assert_eq!(call(&e, json!({"op":"query","text":"secret"}))["total"], 1);
}
#[test]
fn unavailable_content_does_not_mean_known_absence() {
    let e = entry("file.pdf");
    let q = query::parse("!content:needle", &HashMap::new()).unwrap();
    assert!(!q.matches_available(&e, None).unwrap());
    assert!(q.matches_available(&e, Some("other")).unwrap());
    let q = query::parse("name:file OR !content:needle", &HashMap::new()).unwrap();
    assert!(q.matches_available(&e, None).unwrap());
}
#[test]
fn concurrent_query_during_reindex_keeps_pinned_page_consistent() {
    let (_t, e, root) = setup();
    for i in 0..100 {
        std::fs::write(format!("{root}/row{i}.txt"), "x").unwrap();
    }
    scan(&e, &root);
    let first = call(&e, json!({"op":"query","text":"ext:txt","limit":10}));
    let background = e.clone();
    let thread = std::thread::spawn(move || {
        for _ in 0..10 {
            assert_eq!(
                call(&background, json!({"op":"query","text":"row1"}))["total"],
                11
            );
        }
    });
    std::fs::write(format!("{root}/extra.txt"), "new").unwrap();
    scan(&e, &root);
    thread.join().unwrap();
    let old = call(
        &e,
        json!({"op":"query","text":"ext:txt","offset":10,"generation":first["generation"]}),
    );
    assert_eq!(old["total"], 100);
    assert_eq!(
        call(&e, json!({"op":"query","text":"ext:txt"}))["total"],
        101
    );
}
#[test]
fn unknown_directory_size_is_not_a_zero_byte_file() {
    let mut e = entry("folder");
    e.is_dir = true;
    e.size = 0;
    assert!(!query::parse("size:0", &HashMap::new())
        .unwrap()
        .matches(&e, None)
        .unwrap());
    e.is_dir = false;
    assert!(query::parse("size:0", &HashMap::new())
        .unwrap()
        .matches(&e, None)
        .unwrap());
}
#[test]
fn content_case_and_file_prefixes_follow_requested_modifier() {
    let mut e = entry("Report.txt");
    assert!(query::parse("file:report", &HashMap::new())
        .unwrap()
        .matches(&e, None)
        .unwrap());
    assert!(!query::parse("folder:report", &HashMap::new())
        .unwrap()
        .matches(&e, None)
        .unwrap());
    assert!(!query::parse("case:content:HELLO", &HashMap::new())
        .unwrap()
        .matches_available(&e, Some("hello"))
        .unwrap());
    assert!(query::parse("content:HELLO", &HashMap::new())
        .unwrap()
        .matches_available(&e, Some("hello"))
        .unwrap());
    e.is_dir = true;
    assert!(query::parse("folder:report", &HashMap::new())
        .unwrap()
        .matches(&e, None)
        .unwrap());
}
#[test]
fn stale_snapshot_cannot_clear_new_batch_dirty_flag() {
    use filesearch_core::{
        index_store::{IndexStore, SearchSnapshot},
        scanner,
    };
    let (t, e, root) = setup();
    std::fs::write(format!("{root}/old.txt"), "old").unwrap();
    scan(&e, &root);
    let mut db = IndexStore::open(&t.path().join("db/index.sqlite")).unwrap();
    let revision = db.get("revision", json!(0)).as_u64().unwrap();
    let snapshot = SearchSnapshot::new(
        db.entries().unwrap(),
        db.get("generation", json!(0)).as_u64().unwrap(),
    );
    let added = format!("{root}/added.txt");
    std::fs::write(&added, "added").unwrap();
    db.batch(&[scanner::stat_entry(&added).unwrap()], 909090)
        .unwrap();
    db.cache_write(&snapshot, revision).unwrap();
    assert!(db.cache_is_dirty());
    let (recovered, _) = db.cache_read().unwrap();
    assert!(recovered.visible_entries().any(|entry| entry.path == added));
}
#[test]
fn date_comparisons_use_exclusive_next_calendar_boundary() {
    use chrono::{Local, TimeZone};
    let midnight = |y, m, d| {
        Local
            .with_ymd_and_hms(y, m, d, 0, 0, 0)
            .unwrap()
            .timestamp()
    };
    let mut e = entry("dated.txt");
    e.modified = midnight(2024, 2, 29);
    for text in [
        "dm:2024-02",
        "dm:2024",
        "dm:2024-02-01..2024-02-29",
        "dm:>=2024-02-29",
        "dm:<=2024-02-29",
    ] {
        assert!(
            query::parse(text, &HashMap::new())
                .unwrap()
                .matches(&e, None)
                .unwrap(),
            "{text}"
        )
    }
    e.modified = midnight(2024, 3, 1);
    assert!(!query::parse("dm:2024-02", &HashMap::new())
        .unwrap()
        .matches(&e, None)
        .unwrap());
    assert!(query::parse("dm:>2024-02", &HashMap::new())
        .unwrap()
        .matches(&e, None)
        .unwrap());
    for text in [
        "dm:today",
        "dm:yesterday",
        "dm:thisweek",
        "dm:lastweek",
        "dm:thismonth",
        "dm:lastmonth",
        "dm:thisyear",
        "dm:lastyear",
    ] {
        assert!(query::parse(text, &HashMap::new()).is_ok(), "{text}")
    }
}
#[test]
fn query_plan_expands_macros_exclusions_and_missing_properties() {
    let (_t, e, root) = setup();
    std::fs::write(format!("{root}/paper.pdf"), "PDF test").unwrap();
    scan(&e, &root);
    call(
        &e,
        json!({"op":"preferences","set":{"macros":{"papers":"ext:pdf pages:>=2"},"exclusions":["content:secret"]}}),
    );
    let plan = call(&e, json!({"op":"query_plan","text":"papers:"}));
    assert_eq!(plan["requires_content"], true);
    assert_eq!(plan["requires_properties"], true);
    let candidates = call(&e, json!({"op":"content_candidates","text":"papers:"}));
    assert_eq!(candidates["total"], 1);
    assert_eq!(
        call(
            &e,
            json!({"op":"content_candidates","text":"ext:pdf !pages:>=2"})
        )["total"],
        1
    );
}
#[test]
fn binary_cache_preserves_paths_properties_and_rejects_corruption() {
    let (t, e, _root) = setup();
    call(
        &e,
        json!({"op":"import_file_list","rows":[{"path":"/offline/original.txt","name":"display-name.txt","size":10},{"path":"/offline/folder/","is_dir":true}]}),
    );
    let expected = call(&e, json!({"op":"query","text":""}));
    let dbpath = t.path().join("db/index.sqlite");
    let reopened = SearchEngine::open(&dbpath).unwrap();
    let actual = call(&reopened, json!({"op":"query","text":""}));
    assert_eq!(actual["rows"], expected["rows"]);
    let db = filesearch_core::index_store::IndexStore::open(&dbpath).unwrap();
    let mut bytes = std::fs::read(&db.cache_path).unwrap();
    assert_eq!(&bytes[..8], b"AFSIDX04");
    bytes[55] ^= 1;
    std::fs::write(&db.cache_path, &bytes).unwrap();
    assert!(db.cache_read().is_none());
    let reopened = SearchEngine::open(&dbpath).unwrap();
    assert_eq!(
        call(&reopened, json!({"op":"query","text":""}))["rows"],
        expected["rows"]
    );
}
#[test]
fn background_anchor_tracks_sort_position_and_clamps_missing_anchor() {
    let (_t, e, root) = setup();
    for i in 0..100 {
        std::fs::write(format!("{root}/row{i}.txt"), vec![b'x'; i + 1]).unwrap();
    }
    scan(&e, &root);
    let anchor = format!("{root}/row40.txt");
    let first = call(
        &e,
        json!({"op":"query","text":"ext:txt","offset":35,"limit":20,"anchor_path":anchor,"anchor_delta":5}),
    );
    assert_eq!(first["anchor_index"], 40);
    assert_eq!(first["offset"], 35);
    assert_eq!(first["rows"][5]["path"], anchor);
    std::fs::write(format!("{root}/aaa.txt"), "inserted before anchor").unwrap();
    scan(&e, &root);
    let refreshed = call(
        &e,
        json!({"op":"query","text":"ext:txt","offset":35,"limit":20,"anchor_path":anchor,"anchor_delta":5}),
    );
    assert_eq!(refreshed["anchor_index"], 41);
    assert_eq!(refreshed["offset"], 36);
    assert_eq!(refreshed["rows"][5]["path"], anchor);
    let sized = call(
        &e,
        json!({"op":"query","text":"ext:txt","limit":20,"anchor_path":anchor,"anchor_delta":5,"sort":[{"field":"size","ascending":false}]}),
    );
    assert_eq!(sized["rows"][5]["path"], anchor);
    std::fs::remove_file(&anchor).unwrap();
    scan(&e, &root);
    let missing = call(
        &e,
        json!({"op":"query","text":"ext:txt","offset":999,"limit":20,"anchor_path":anchor,"anchor_delta":5}),
    );
    assert_eq!(missing["anchor_found"], false);
    assert_eq!(missing["offset"], 80);
    assert_eq!(missing["rows"].as_array().unwrap().len(), 20);
}

#[test]
fn unchanged_rescan_and_content_do_not_publish_new_search_generations() {
    let (temp, engine, root) = setup();
    let path = format!("{root}/stable.txt");
    std::fs::write(&path, "unchanged metadata").unwrap();
    scan(&engine, &root);
    let first = call(&engine, json!({"op":"status"}));
    assert_eq!(first["scanning"], false);
    assert_eq!(first["initial_scan_complete"], true);
    assert_eq!(first["scan_processed_entries"], 2);
    let db = filesearch_core::index_store::IndexStore::open(&temp.path().join("db/index.sqlite"))
        .unwrap();
    let revision = db.get("revision", json!(0));
    scan(&engine, &root);
    assert_eq!(
        call(&engine, json!({"op":"status"}))["generation"],
        first["generation"]
    );
    assert_eq!(db.get("revision", json!(0)), revision);
    call(
        &engine,
        json!({"op":"put_content","path":path,"text":"stable body","properties":{"title":"stable"}}),
    );
    let content = call(&engine, json!({"op":"status"}));
    assert!(content["generation"].as_u64().unwrap() > first["generation"].as_u64().unwrap());
    call(
        &engine,
        json!({"op":"put_content","path":path,"text":"stable body","properties":{"title":"stable"}}),
    );
    call(&engine, json!({"op":"publish_content"}));
    assert_eq!(
        call(&engine, json!({"op":"status"}))["generation"],
        content["generation"]
    );
}

#[test]
fn seen_epochs_and_event_cursor_keep_valid_cache_and_generation() {
    use filesearch_core::{index_store::IndexStore, scanner};
    let (temp, engine, root) = setup();
    let path = format!("{root}/visible.txt");
    std::fs::write(&path, "visible").unwrap();
    scan(&engine, &root);
    let mut db = IndexStore::open(&temp.path().join("db/index.sqlite")).unwrap();
    let revision = db.get("revision", json!(0));
    let generation = db.get("generation", json!(0));
    let entries = [
        scanner::stat_entry(&root).unwrap(),
        scanner::stat_entry(&path).unwrap(),
    ];
    assert_eq!(db.batch(&entries, 999999).unwrap(), 0);
    assert_eq!(db.finish(&[root], &[], 999999, 424242).unwrap(), 0);
    assert_eq!(db.get("revision", json!(0)), revision);
    assert_eq!(db.get("generation", json!(0)), generation);
    assert_eq!(db.get("event_id", json!(0)), 424242);
    assert!(db.cache_read().is_some());
}

#[test]
fn incremental_snapshot_matches_full_rebuild_for_metadata_and_renames() {
    use filesearch_core::index_store::SearchSnapshot;
    let mut entries = vec![
        entry("报告2.txt"),
        entry("café10.txt"),
        entry("Straße1.txt"),
    ];
    for (index, e) in entries.iter_mut().enumerate() {
        e.id = index as i64 + 1;
    }
    let previous = SearchSnapshot::new(entries.clone(), 1);
    entries[1].size += 100;
    let reused = SearchSnapshot::from_previous(entries.clone(), 2, &previous);
    let rebuilt = SearchSnapshot::new(entries.clone(), 2);
    assert_eq!(reused.trigrams, rebuilt.trigrams);
    assert_eq!(reused.name_order, rebuilt.name_order);
    for (a, b) in reused.entries.iter().zip(&rebuilt.entries) {
        assert_eq!(a.folded_path, b.folded_path);
        assert_eq!(a.folded_name, b.folded_name);
        assert_eq!(a.size, b.size);
    }
    entries.remove(0);
    let mut added = entry("新文件3.txt");
    added.id = 4;
    entries.push(added);
    let reused = SearchSnapshot::from_previous(entries.clone(), 3, &previous);
    let rebuilt = SearchSnapshot::new(entries, 3);
    assert_eq!(reused.trigrams, rebuilt.trigrams);
    assert_eq!(reused.name_order, rebuilt.name_order);
}

#[test]
fn replacing_offline_list_with_empty_list_publishes_empty_snapshot() {
    let (_temp, engine, _root) = setup();
    call(
        &engine,
        json!({"op":"import_file_list","rows":[{"path":"/offline/one.txt","size":1}]}),
    );
    let first = call(&engine, json!({"op":"query","text":""}));
    call(&engine, json!({"op":"import_file_list","rows":[]}));
    let cleared = call(&engine, json!({"op":"query","text":""}));
    assert_eq!(cleared["total"], 0);
    assert!(cleared["generation"].as_u64().unwrap() > first["generation"].as_u64().unwrap());
}
