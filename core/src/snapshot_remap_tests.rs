//! Stable-slot restoration, deliberate compaction, and cache-restart regressions.
use super::*;

fn file(id: i64, name: &str) -> IndexedFile {
    serde_json::from_value(json!({
        "id":id,"path":format!("/remap-fixture/Group{}/{name}", id % 2),
        "name":name,"extension":Path::new(name).extension().unwrap_or_default().to_string_lossy(),
        "size":id * 10,"modified":id,"created":id,"changed":id,
        "modified_ns":id * 100,"changed_ns":id * 100,"is_dir":false,"is_symlink":false,
        "file_id":id + 100,"parent_id":20,"volume_id":"remap-fixture-volume","flags":0,
        "properties":{"verified":true},"content_indexed":false
    }))
    .unwrap()
}
fn ids(snapshot: &SearchSnapshot) -> Vec<i64> {
    let mut ids: Vec<_> = snapshot.visible_entries().map(|file| file.id).collect();
    ids.sort_unstable();
    ids
}
fn previous() -> SearchSnapshot {
    let mut snapshot = SearchSnapshot::new(
        vec![
            file(1, "report10.txt"),
            file(3, "Cafe\u{301}2.pdf"),
            file(5, "报告10.txt"),
        ],
        10,
    );
    snapshot.content_revision = 91;
    snapshot
}
fn delta() -> Vec<index_store::SnapshotChange> {
    vec![
        (2, Some(file(2, "report2.txt"))),
        (3, None),
        (4, Some(file(4, "报告2.pdf"))),
    ]
}
#[test]
fn a_restored_middle_id_appends_stable_slots_without_mutating_the_old_snapshot() {
    let previous = previous();
    assert_eq!(SearchSnapshot::remap_reason(&delta(), &previous), None);
    let remapped = SearchSnapshot::from_changes_with_reason(delta(), 11, &previous).unwrap();
    assert_eq!(ids(&remapped), [1, 2, 4, 5]);
    assert_eq!(ids(&previous), [1, 3, 5]);
    assert_eq!(remapped.content_revision, 91);
    assert!(Arc::ptr_eq(&previous.entries[0], &remapped.entries[0]));
    assert!(Arc::ptr_eq(&previous.entries[2], &remapped.entries[2]));
    assert_eq!(previous.entries[1].name, "Cafe\u{301}2.pdf");
    assert_eq!(remapped.entries.len(), 5);
    assert_eq!(remapped.slot_for_id(1), Some(0));
    assert_eq!(remapped.slot_for_id(3), Some(1));
    assert_eq!(remapped.slot_for_id(5), Some(2));
    assert_eq!(remapped.slot_for_id(2), Some(3));
    assert_eq!(remapped.slot_for_id(4), Some(4));
    assert!(!remapped.live.contains(1));
    let mut changed = remapped.entries[0].as_ref().clone();
    changed.size = 999;
    let updated = SearchSnapshot::from_changes(vec![(1, Some(changed))], 12, &remapped).unwrap();
    assert_eq!(updated.entries[0].size, 999);
    assert_eq!(previous.entries[0].size, 10);
    assert_eq!(remapped.entries[0].size, 10);
    assert!(Arc::ptr_eq(
        &remapped.entries[0].search_name,
        &updated.entries[0].search_name
    ));
}

#[test]
fn remapping_compacts_inactive_slots_and_preserves_order_after_delete_restore() {
    let previous = previous();
    let hidden = SearchSnapshot::from_changes(vec![(1, None)], 11, &previous).unwrap();
    assert_eq!(hidden.entries.len(), 3);
    assert_eq!(ids(&hidden), [3, 5]);
    let compacted = SearchSnapshot::remap_changes(vec![(3, None)], 12, &hidden);
    assert_eq!(ids(&compacted), [5]);
    assert_eq!(compacted.entries.len(), 1);
    assert!(Arc::ptr_eq(&previous.entries[2], &compacted.entries[0]));
    let restored =
        SearchSnapshot::from_changes(vec![(1, Some(file(1, "report10.txt")))], 13, &compacted)
            .unwrap();
    assert_eq!(ids(&restored), [1, 5]);
    assert!(Arc::ptr_eq(&compacted.entries[0], &restored.entries[0]));
    let restored_again =
        SearchSnapshot::from_changes(vec![(3, Some(file(3, "Cafe\u{301}2.pdf")))], 14, &restored)
            .unwrap();
    assert_eq!(ids(&restored_again), [1, 3, 5]);
    assert_eq!(
        restored_again
            .entries
            .iter()
            .map(|file| file.id)
            .collect::<Vec<_>>(),
        [5, 1, 3]
    );
    let compacted_again = SearchSnapshot::remap_changes(vec![], 15, &restored_again);
    assert_eq!(
        compacted_again
            .entries
            .iter()
            .map(|file| file.id)
            .collect::<Vec<_>>(),
        [1, 3, 5]
    );
}

#[test]
fn stable_slot_indexes_and_cache_preserve_queries_natural_sort_and_anchors() {
    let temporary = tempfile::tempdir().unwrap();
    let engine = SearchEngine::open(&temporary.path().join("index.sqlite")).unwrap();
    let remapped = SearchSnapshot::from_changes_with_reason(delta(), 11, &previous()).unwrap();
    let cache = temporary.path().join("prepared.cache");
    snapshot_cache::write(&cache, &remapped, 123).unwrap();
    assert_eq!(&std::fs::read(&cache).unwrap()[..8], b"APFIDX01");
    let (restored, generation) = snapshot_cache::read(&cache, 11, 123).unwrap();
    assert_eq!(generation, 11);
    let reference = SearchSnapshot::new(
        vec![
            file(1, "report10.txt"),
            file(2, "report2.txt"),
            file(4, "报告2.pdf"),
            file(5, "报告10.txt"),
        ],
        11,
    );
    let requests = [
        json!({"op":"query","text":"","limit":100}),
        json!({"op":"query","text":"r","limit":100}),
        json!({"op":"query","text":"report","limit":100}),
        json!({"op":"query","text":"报告","limit":100}),
        json!({"op":"query","text":"ext:pdf","limit":100}),
        json!({"op":"query","text":"size:>=20","limit":100}),
        json!({"op":"query","text":"","limit":2,"offset":0,"anchor_id":2,
            "anchor_path":file(2,"report2.txt").path,"anchor_delta":0,
            "sort":[{"field":"path","ascending":true},{"field":"name","ascending":true}]}),
        json!({"op":"query","text":"","limit":100,"sort":[{"field":"size","ascending":false}]}),
    ];
    let collect = |snapshot: SearchSnapshot| {
        engine.snapshot.store(Arc::new(snapshot));
        requests
            .iter()
            .map(|request| {
                let result = engine.call(request.clone());
                assert_eq!(result["success"], true, "{result}");
                json!([
                    result["rows"],
                    result["total"],
                    result["offset"],
                    result["anchor_index"]
                ])
            })
            .collect::<Vec<_>>()
    };
    let expected = collect(reference);
    assert_eq!(collect(remapped), expected);
    assert_eq!(collect(restored), expected);
}

#[test]
fn unordered_new_ids_and_repeated_restoration_keep_stable_id_lookup_valid() {
    let previous = previous();
    let appended = SearchSnapshot::from_changes(
        vec![(9, Some(file(9, "item9"))), (7, Some(file(7, "item7")))],
        11,
        &previous,
    )
    .unwrap();
    assert_eq!(ids(&appended), [1, 3, 5, 7, 9]);
    let changes = vec![
        (2, Some(file(2, "discarded"))),
        (4, None),
        (2, Some(file(2, "kept"))),
    ];
    let updated = SearchSnapshot::from_changes(changes, 12, &appended).unwrap();
    assert_eq!(ids(&updated), [1, 2, 3, 5, 7, 9]);
    assert_eq!(
        updated.entries[updated.slot_for_id(2).unwrap()].name,
        "kept"
    );
    let too_many = (0..2001).map(|_| (1, None)).collect::<Vec<_>>();
    assert_eq!(SearchSnapshot::remap_reason(&too_many, &previous), None);
    assert_eq!(
        SearchSnapshot::from_changes_with_reason(too_many, 12, &previous).err(),
        Some("delta_exceeds_incremental_limit")
    );
}

#[test]
fn an_engine_reopens_stable_slot_cache_and_incrementally_updates_a_restored_id() {
    let temporary = tempfile::tempdir().unwrap();
    let database = temporary.path().join("index.sqlite");
    let engine = SearchEngine::open(&database).unwrap();
    let files: Vec<scanner::ScannedFile> = [
        file(1, "item10.txt"),
        file(2, "item2.txt"),
        file(3, "中文.txt"),
    ]
    .iter()
    .map(|file| serde_json::from_value(serde_json::to_value(file).unwrap()).unwrap())
    .collect();
    engine.index_store.lock().unwrap().batch(&files, 1).unwrap();
    engine.refresh(false).unwrap();
    engine
        .index_store
        .lock()
        .unwrap()
        .connection
        .execute(
            "UPDATE files SET accessible=0 WHERE path=?1",
            [&files[1].path],
        )
        .unwrap();
    engine.refresh(false).unwrap();
    let generation = engine.snapshot.load().generation;
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
    engine.refresh(true).unwrap();
    assert_eq!(
        engine.state.lock().unwrap()["last_refresh"]["mode"],
        "incremental"
    );
    let request = json!({"op":"query", "text":"", "limit":100});
    let before = engine.call(request.clone());
    assert_eq!(before["success"], true);
    assert_eq!(before["total"], 3);
    assert_eq!(before["rows"][0]["name"], "item2.txt");
    let snapshot = engine.snapshot.load_full();
    assert!(
        engine.index_store.lock().unwrap().cache_read().is_some(),
        "the actual persisted cache is valid"
    );
    drop(snapshot);
    drop(engine);
    let reopened = SearchEngine::open(&database).unwrap();
    assert!(!reopened.needs_cache_rebuild.load(Ordering::Relaxed));
    let after = reopened.call(request);
    assert_eq!(after["success"], true);
    assert_eq!(after["generation"], before["generation"]);
    assert_eq!(after["rows"], before["rows"]);
    let restored_id = reopened
        .snapshot
        .load()
        .visible_entries()
        .find(|file| file.path == files[1].path)
        .unwrap()
        .id;
    let restored_slot = reopened.snapshot.load().slot_for_id(restored_id).unwrap();
    let mut updated_file = files[1].clone();
    updated_file.size = 12345;
    reopened
        .index_store
        .lock()
        .unwrap()
        .batch(&[updated_file.clone()], 1)
        .unwrap();
    reopened.refresh(false).unwrap();
    assert_eq!(
        reopened.snapshot.load().slot_for_id(restored_id),
        Some(restored_slot)
    );
    assert_eq!(reopened.snapshot.load().entries[restored_slot].size, 12345);
    reopened
        .index_store
        .lock()
        .unwrap()
        .finish_observed(
            std::slice::from_ref(&files[1].path),
            &[],
            &roaring::RoaringTreemap::new(),
            0,
        )
        .unwrap();
    reopened.refresh(false).unwrap();
    assert!(!reopened.snapshot.load().live.contains(restored_slot as u32));
    assert_eq!(
        reopened.state.lock().unwrap()["last_refresh"]["mode"],
        "incremental"
    );
}

#[test]
fn persistent_id_extremes_and_tail_changes_keep_every_existing_slot() {
    let make = |id, name| {
        let mut entry = file(1, name);
        entry.id = id;
        entry
    };
    let previous = SearchSnapshot::new(
        vec![
            make(-5, "negative"),
            make(8, "ordinary"),
            make(i64::MAX, "maximum"),
        ],
        1,
    );
    let restored = SearchSnapshot::from_changes(
        vec![
            (i64::MIN, Some(make(i64::MIN, "minimum"))),
            (0, Some(make(0, "zero"))),
            (9, Some(make(9, "tail"))),
        ],
        2,
        &previous,
    )
    .unwrap();
    for (id, expected) in [
        (-5, 0),
        (8, 1),
        (i64::MAX, 2),
        (i64::MIN, 3),
        (0, 4),
        (9, 5),
    ] {
        assert_eq!(restored.slot_for_id(id), Some(expected));
    }
    let removed =
        SearchSnapshot::from_changes(vec![(9, None), (i64::MAX, None), (0, None)], 3, &restored)
            .unwrap();
    let again = SearchSnapshot::from_changes(
        vec![
            (9, Some(make(9, "tail-restored"))),
            (10, Some(make(10, "later"))),
            (i64::MAX, Some(make(i64::MAX, "maximum-restored"))),
        ],
        4,
        &removed,
    )
    .unwrap();
    assert_eq!(again.slot_for_id(9), Some(5));
    assert_eq!(again.slot_for_id(i64::MAX), Some(2));
    assert_eq!(again.slot_for_id(10), Some(6));
    assert!(!again.live.contains(again.slot_for_id(0).unwrap() as u32));
    assert_eq!(restored.entries[5].name, "tail");
}

#[test]
fn sorted_prefix_index_only_allocates_entries_for_the_out_of_order_tail() {
    let previous = SearchSnapshot::new(
        (1..=1000)
            .map(|index| file(index * 2, &format!("item{index}")))
            .collect(),
        1,
    );
    assert_eq!(
        index_store::FileSlots::from_entries(&previous.entries)
            .unwrap()
            .layout(),
        (1000, 0)
    );
    let restored =
        SearchSnapshot::from_changes(vec![(1, Some(file(1, "restored")))], 2, &previous).unwrap();
    assert_eq!(
        index_store::FileSlots::from_entries(&restored.entries)
            .unwrap()
            .layout(),
        (1000, 1)
    );
    let later = SearchSnapshot::from_changes(vec![(3000, Some(file(3000, "later")))], 3, &restored)
        .unwrap();
    assert_eq!(
        index_store::FileSlots::from_entries(&later.entries)
            .unwrap()
            .layout(),
        (1000, 2)
    );
    for slot in 0..1000 {
        assert!(Arc::ptr_eq(&previous.entries[slot], &later.entries[slot]));
        assert_eq!(later.slot_for_id((slot as i64 + 1) * 2), Some(slot));
    }
    let repeated = vec![
        later.entries[0].clone(),
        later.entries[1].clone(),
        later.entries[0].clone(),
    ];
    assert_eq!(
        index_store::FileSlots::from_entries(&repeated).err(),
        Some("duplicate_persistent_file_id")
    );
}

#[test]
fn restoring_102_middle_ids_reuses_original_rows_and_unaffected_posting_bitmaps() {
    let previous = SearchSnapshot::new(
        (1..=1000)
            .map(|index| file(index * 2, &format!("stable-original-{index}")))
            .collect(),
        1,
    );
    let original_posting = previous.trigrams.get(b"sta").unwrap().clone();
    let changes: Vec<_> = (0..102)
        .map(|index| {
            let id = index * 2 + 1;
            (id, Some(file(id, &format!("restore-{id}"))))
        })
        .collect();
    assert_eq!(SearchSnapshot::remap_reason(&changes, &previous), None);
    let started = Instant::now();
    let updated = SearchSnapshot::from_changes_with_reason(changes, 2, &previous).unwrap();
    let elapsed = started.elapsed();
    assert_eq!(updated.entries.len(), 1102);
    assert_eq!(
        index_store::FileSlots::from_entries(&updated.entries)
            .unwrap()
            .layout(),
        (1000, 102)
    );
    for (slot, old) in previous.entries.iter().enumerate() {
        assert!(
            Arc::ptr_eq(old, &updated.entries[slot]),
            "old record {slot} must not be cloned or prepared"
        );
        assert_eq!(updated.slot_for_id(old.id), Some(slot));
    }
    assert!(
        Arc::ptr_eq(&original_posting, updated.trigrams.get(b"sta").unwrap()),
        "unaffected posting bitmap remains shared"
    );
    assert_eq!(original_posting.len(), 1000);
    for index in 0..102 {
        assert_eq!(
            updated.slot_for_id(index * 2 + 1),
            Some(1000 + index as usize)
        );
    }
    eprintln!("102 restored IDs / 1000 retained rows: {:.3}ms; 1000 Arc-identical rows, one unchanged posting Arc, 102 sparse ID entries", elapsed.as_secs_f64() * 1000.0);
}
