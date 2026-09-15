//! Small, real publication-path tests for snapshot ownership. Weak references
//! observe destruction without keeping a snapshot alive themselves.
use crate::entry_table::FileEntry;
use crate::{SearchEngine, index_store::SearchSnapshot, scanner::ScannedFile};
use roaring::RoaringTreemap;
use serde_json::{Value, json};
use std::sync::{Arc, Weak};

fn fixture() -> (tempfile::TempDir, Arc<SearchEngine>, Vec<ScannedFile>) {
    let temporary = tempfile::tempdir().unwrap();
    let engine = SearchEngine::open(&temporary.path().join("index.sqlite")).unwrap();
    let files: Vec<_> = (0..3)
        .map(|index| ScannedFile {
            path: format!("/snapshot-history/item{index}.txt"),
            name: format!("item{index}.txt"),
            extension: "txt".into(),
            size: index + 1,
            file_id: index + 1,
            parent_id: 1,
            volume_id: "snapshot-history".into(),
            ..ScannedFile::default()
        })
        .collect();
    engine.index_store.lock().unwrap().batch(&files, 1).unwrap();
    engine.refresh(false).unwrap();
    (temporary, engine, files)
}
fn success(engine: &Arc<SearchEngine>, request: Value) -> Value {
    let response = engine.call(request.clone());
    assert_eq!(response["success"], true, "{request} => {response}");
    response
}
fn observe(engine: &SearchEngine) -> (u64, Weak<SearchSnapshot>) {
    let current = engine.snapshot.load_full();
    (current.generation, Arc::downgrade(&current))
}
fn assert_expired(engine: &Arc<SearchEngine>, generation: u64) {
    assert!(engine.pin(Some(generation)).is_err());
    let reply = engine.call(json!({"op":"query","text":"","generation":generation}));
    assert_eq!(reply["success"], false, "{reply}");
    assert!(
        reply["error"]
            .as_str()
            .unwrap()
            .contains("generation expired"),
        "{reply}"
    );
}
fn change_first(engine: &SearchEngine, files: &mut [ScannedFile]) {
    files[0].size += 10;
    engine
        .index_store
        .lock()
        .unwrap()
        .batch(&files[..1], 1)
        .unwrap();
}
fn publish_incremental(engine: &SearchEngine, files: &mut [ScannedFile]) {
    change_first(engine, files);
    engine.refresh(false).unwrap();
    assert_eq!(
        engine.state.lock().unwrap()["last_refresh"]["mode"],
        "incremental"
    );
}
fn force_full(engine: &SearchEngine) {
    // Lose only the disposable delta baseline, as happens after a journal gap.
    // Actual rows and user data remain in the authoritative small SQLite store.
    engine
        .index_store
        .lock()
        .unwrap()
        .set("changes_base_revision", &json!(u64::MAX))
        .unwrap();
    engine.refresh(false).unwrap();
    assert_eq!(engine.state.lock().unwrap()["last_refresh"]["mode"], "full");
}
fn pad_inactive_slots(engine: &Arc<SearchEngine>) {
    // Simulate an aged snapshot without creating 10,000 files or SQLite rows.
    // The authoritative database still contains only the original three rows.
    let current = engine.snapshot.load_full();
    let mut entries: Vec<_> = current
        .visible_entries()
        .map(|entry| entry.to_owned_file())
        .collect();
    let first_unused_id = entries.iter().map(|file| file.id).max().unwrap() + 1;
    let template = entries[0].clone();
    for offset in 0..10_000 {
        let mut inactive = template.clone();
        inactive.id = first_unused_id + offset;
        inactive.file_id = inactive.id as u64;
        inactive.name = format!("inactive{offset}.dead");
        inactive.path = format!("/snapshot-history/inactive/{}", inactive.name);
        inactive.extension = "dead".into();
        inactive.size = 1_000_001 + offset as u64;
        entries.push(inactive);
    }
    let mut padded = SearchSnapshot::new(entries, current.generation);
    padded.content_revision = current.content_revision;
    // Retire the synthetic rows through the actual update algorithm so orders,
    // postings and ID lookup retain the same invariants as an aged real index.
    // Each small delta is below the production incremental budget. With three
    // rows left, 10,003 slots exactly meet (not exceed) the compaction threshold.
    for batch in 0..10 {
        let first_id = first_unused_id + batch * 1_000;
        let changes = (first_id..first_id + 1_000).map(|id| (id, None)).collect();
        padded = SearchSnapshot::from_changes(changes, current.generation, &padded).unwrap();
        assert_eq!(padded.entries.len(), 10_003, "premature compaction");
    }
    assert_eq!(padded.entries.len(), 10_003);
    assert_eq!(padded.len(), 3);
    assert_eq!(SearchSnapshot::remap_reason(&[], &padded), None);
    engine.snapshot.store(Arc::new(padded));
    assert_eq!(
        engine.index_store.lock().unwrap().entries().unwrap().len(),
        3
    );
    assert_only_live_rows(engine, 3);
}
fn delete_middle_incrementally(engine: &Arc<SearchEngine>, files: &[ScannedFile]) {
    engine
        .index_store
        .lock()
        .unwrap()
        .finish_observed(
            &[],
            std::slice::from_ref(&files[1].path),
            &RoaringTreemap::new(),
            0,
        )
        .unwrap();
    engine.refresh(false).unwrap();
    assert_eq!(
        engine.state.lock().unwrap()["last_refresh"]["mode"],
        "incremental"
    );
    let snapshot = engine.snapshot.load_full();
    assert_eq!(snapshot.entries.len(), 10_003);
    assert_eq!(snapshot.len(), 2);
    assert_eq!(
        SearchSnapshot::remap_reason(&[], &snapshot),
        Some("inactive_slots_require_compaction")
    );
    assert!(
        snapshot
            .visible_entries()
            .all(|entry| entry.path() != files[1].path)
    );
    assert_only_live_rows(engine, 2);
}
fn assert_only_live_rows(engine: &Arc<SearchEngine>, count: usize) {
    let current = engine.snapshot.load_full();
    let visible: Vec<_> = current.visible_entries().collect();
    assert_eq!(visible.len(), count);
    // Query paths must exclude every inactive slot after the same incremental
    // deletions production uses, including metadata, numeric and sorted paths.
    for text in ["inactive", "ext:dead", "size:>1000000"] {
        let result = success(engine, json!({"op":"query","text":text}));
        assert_eq!(result["total"], 0, "dead slots escaped through {text}");
        assert!(result["rows"].as_array().unwrap().is_empty());
    }
    for text in ["", "item", "ext:txt", "size:>0"] {
        for (field, ascending) in [("name", true), ("path", true), ("size", false)] {
            let mut expected = visible.clone();
            expected.sort_by(|left, right| match field {
                "name" => left.name().cmp(right.name()),
                "path" => left.path().cmp(&right.path()),
                "size" => right.size().cmp(&left.size()),
                _ => unreachable!(),
            });
            let paths: Vec<_> = expected.iter().map(|file| file.path()).collect();
            let result = success(
                engine,
                json!({"op":"query","text":text,
                "sort":[{"field":field,"ascending":ascending}],"limit":100}),
            );
            assert_eq!(result["total"], count, "{text}, {field}");
            let actual: Vec<_> = result["rows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["path"].as_str().unwrap())
                .collect();
            assert_eq!(
                actual, paths,
                "live rows or sorting changed: {text}, {field}"
            );
        }
    }
}
#[derive(Clone, Copy, Debug)]
enum Boundary {
    Full,
    Compacted,
}
fn publish_boundary(engine: &Arc<SearchEngine>, files: &mut [ScannedFile], boundary: Boundary) {
    match boundary {
        Boundary::Full => {
            change_first(engine, files);
            force_full(engine);
        }
        Boundary::Compacted => {
            change_first(engine, files);
            engine.refresh(false).unwrap();
            let diagnostic = engine.state.lock().unwrap()["last_refresh"].clone();
            assert_eq!(diagnostic["mode"], "remapped");
            assert_eq!(diagnostic["reason"], "inactive_slots_require_compaction");
            assert_eq!(diagnostic["previous_live_entries"], 2);
            assert_eq!(diagnostic["previous_slots"], 10_003);
            let snapshot = engine.snapshot.load_full();
            assert_eq!(snapshot.len(), 2);
            assert_eq!(snapshot.entries.len(), 2);
            assert!(
                snapshot
                    .entries
                    .iter()
                    .zip(snapshot.entries.iter().skip(1))
                    .all(|(first, second)| first.id() < second.id())
            );
            assert_only_live_rows(engine, 2);
        }
    }
}
fn snapshot_rows(snapshot: &SearchSnapshot) -> Vec<(String, u64)> {
    snapshot
        .visible_entries()
        .map(|file| (file.path().to_string(), file.size()))
        .collect()
}

#[test]
fn incremental_publications_retain_two_unleased_generations_then_expire_them() {
    let (_temporary, engine, mut files) = fixture();
    let (first, first_weak) = observe(&engine);
    let original = success(&engine, json!({"op":"query","text":"","generation":first}));
    publish_incremental(&engine, &mut files);
    let (second, second_weak) = observe(&engine);
    publish_incremental(&engine, &mut files);
    let third = observe(&engine).0;
    assert!(first_weak.upgrade().is_some());
    assert!(second_weak.upgrade().is_some());
    assert_eq!(
        success(&engine, json!({"op":"query","text":"","generation":first}))["rows"],
        original["rows"]
    );
    success(&engine, json!({"op":"query","text":"","generation":second}));
    publish_incremental(&engine, &mut files);
    assert!(
        first_weak.upgrade().is_none(),
        "expired history retained the oldest snapshot"
    );
    assert_expired(&engine, first);
    success(&engine, json!({"op":"query","text":"","generation":second}));
    success(&engine, json!({"op":"query","text":"","generation":third}));
}

#[test]
fn full_boundary_releases_unleased_current_and_earlier_history_snapshots() {
    let (_temporary, engine, mut files) = fixture();
    let (first, first_weak) = observe(&engine);
    publish_incremental(&engine, &mut files);
    let (second, second_weak) = observe(&engine);
    publish_boundary(&engine, &mut files, Boundary::Full);
    assert!(
        first_weak.upgrade().is_none(),
        "full rebuild retained older history"
    );
    assert!(
        second_weak.upgrade().is_none(),
        "full rebuild retained the previous current snapshot"
    );
    assert_expired(&engine, first);
    assert_expired(&engine, second);
    let current = success(&engine, json!({"op":"query","text":""}));
    assert_eq!(current["rows"][0]["size"], files[0].size);
    assert_eq!(current["total"], 3);
}

#[test]
fn compaction_boundary_releases_unleased_current_and_earlier_history_snapshots() {
    let (_temporary, engine, mut files) = fixture();
    pad_inactive_slots(&engine);
    let (first, first_weak) = observe(&engine);
    delete_middle_incrementally(&engine, &files);
    let (second, second_weak) = observe(&engine);
    publish_boundary(&engine, &mut files, Boundary::Compacted);
    assert!(
        first_weak.upgrade().is_none(),
        "compaction retained older history"
    );
    assert!(
        second_weak.upgrade().is_none(),
        "compaction retained the previous current snapshot"
    );
    assert_expired(&engine, first);
    assert_expired(&engine, second);
    assert_eq!(
        success(&engine, json!({"op":"query","text":""}))["total"],
        2
    );
}

#[test]
fn explicit_lease_and_inflight_pin_survive_boundaries_until_their_owners_release() {
    for boundary in [Boundary::Full, Boundary::Compacted] {
        let (_temporary, engine, mut files) = fixture();
        if matches!(boundary, Boundary::Compacted) {
            pad_inactive_slots(&engine);
            delete_middle_incrementally(&engine, &files);
        }
        let pinned = engine.pin(None).unwrap();
        let generation = pinned.generation;
        let weak = Arc::downgrade(&pinned);
        let old_rows = snapshot_rows(&pinned);
        let original = success(&engine, json!({"op":"query","text":"","limit":100}));
        let retained = success(
            &engine,
            json!({"op":"retain_snapshot","generation":generation}),
        );
        let lease = retained["snapshot_lease"].as_str().unwrap();
        publish_boundary(&engine, &mut files, boundary);
        // Evict even the weak history handle: only the explicit readers retain
        // this generation, independently of the short bare-generation window.
        publish_incremental(&engine, &mut files);
        publish_incremental(&engine, &mut files);
        assert!(weak.upgrade().is_some(), "{boundary:?}");
        assert_eq!(
            snapshot_rows(&pinned),
            old_rows,
            "inflight pin changed across {boundary:?}"
        );
        let mut rows = Vec::new();
        for offset in 0..old_rows.len() {
            let page = success(
                &engine,
                json!({"op":"query","text":"","generation":generation,
                "snapshot_lease":lease,"offset":offset,"limit":1}),
            );
            assert_eq!(page["generation"], generation);
            assert_eq!(page["total"], old_rows.len());
            rows.extend(page["rows"].as_array().unwrap().iter().cloned());
        }
        assert_eq!(
            rows,
            *original["rows"].as_array().unwrap(),
            "leased metadata changed across {boundary:?}"
        );
        assert_expired(&engine, generation);
        assert_eq!(
            success(
                &engine,
                json!({"op":"release_snapshot","snapshot_lease":lease})
            )["released"],
            true
        );
        assert!(
            weak.upgrade().is_some(),
            "release invalidated an active pin"
        );
        assert_eq!(snapshot_rows(&pinned), old_rows);
        drop(pinned);
        assert!(
            weak.upgrade().is_none(),
            "{boundary:?}: no reader remains but the old snapshot is retained"
        );
        assert_eq!(
            engine.call(json!({"op":"query","text":"","snapshot_lease":lease}))["success"],
            false
        );
    }
}
