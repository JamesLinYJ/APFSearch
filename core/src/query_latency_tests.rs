//! Explicit performance harness: synthetic metadata, no volume scan or file tree.
use super::*;
use crate::entry_table::FileEntry;

fn synthetic_snapshot(count: usize) -> SearchSnapshot {
    let template: IndexedFile = serde_json::from_value(json!({
        "id":1,"path":"/synthetic/report.txt","name":"report.txt","extension":"txt",
        "size":0,"modified":0,"created":0,"changed":0,"is_dir":false,"is_symlink":false,
        "file_id":1,"parent_id":0,"volume_id":"synthetic","flags":0,"properties":{}
    }))
    .unwrap();
    let entries = (0..count).map(|slot| {
        let mut file = template.clone();
        file.id = slot as i64 + 1;
        file.file_id = slot as u64 + 1;
        file.size = slot as u64;
        file.name = format!(
            "report-document-{slot:07}-{}.txt",
            if slot % 97 == 0 { "报告" } else { "analysis" }
        );
        file.path = format!("/synthetic/{}/{name}", slot / 1000, name = file.name);
        file
    });
    SearchSnapshot::from_rows(entries.map(Ok), 1).unwrap()
}

#[test]
fn name_column_preserves_matching_across_rename_append_delete_and_restore() {
    let packed = synthetic_snapshot(4100);
    let allocation = |slot: usize| packed.entries.text_block_identity(slot);
    assert_eq!(allocation(0), allocation(4095));
    assert_ne!(allocation(4095), allocation(4096));
    let path_allocation = |slot: usize| packed.entries.text_block_identity(slot);
    assert_eq!(path_allocation(0), path_allocation(4095));
    assert_ne!(path_allocation(4095), path_allocation(4096));
    // Names occupy the first region of the block; paths share its ownership,
    // while retaining distinct byte ranges for direct path matching.
    assert_eq!(allocation(0), path_allocation(0));
    assert_eq!(
        packed.entries.at(0).search_name().as_ptr(),
        packed.entries.at(0).search_path().suffix.as_ptr()
    );
    let mut entries: Vec<_> = packed
        .visible_entries()
        .map(|entry| entry.to_owned_file())
        .collect();
    for (slot, name) in [(0, "Straße café.txt"), (4096, "报告 café.txt")] {
        entries[slot].name = name.into();
        entries[slot].path = format!("/synthetic/{name}");
    }
    let original = SearchSnapshot::new(entries, 1);
    let mut renamed = original.entries.at(4096).to_owned_file();
    renamed.name = "changed-报告.txt".into();
    renamed.path = format!("/synthetic/{}", renamed.name);
    let mut appended = renamed.clone();
    appended.id = 9000;
    appended.name = "Straße cafe\u{301}.txt".into();
    appended.path = format!("/synthetic/{}", appended.name);
    let updated = SearchSnapshot::from_changes(
        vec![
            (1, None),
            (renamed.id, Some(renamed)),
            (appended.id, Some(appended)),
        ],
        2,
        &original,
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let cache = directory.path().join("snapshot.bin");
    snapshot_cache::write(&cache, &updated, 1).unwrap();
    let (restored, _) = snapshot_cache::read(&cache, 2, 1).unwrap();
    for snapshot in [&original, &updated, &restored] {
        for text in [
            "report",
            "cafe",
            "strasse",
            "报告",
            "report analysis",
            "report report report",
            "report | report | analysis",
            "!(report | report)",
            "(report report) | (cafe cafe)",
            "Straße STRASSE",
            "!report",
            "report | cafe",
            "(report | cafe) !missing",
            "changed",
            "a",
        ] {
            let query = query::parse(text, &HashMap::new()).unwrap();
            for excluded in ["missing", "analysis", "!报告"] {
                let exclusion = query::parse(excluded, &HashMap::new()).unwrap();
                let expected: roaring::RoaringBitmap = snapshot
                    .live
                    .iter()
                    .filter(|slot| {
                        let file = &snapshot.entries.at(*slot as usize);
                        query.matches_available(file, None).unwrap()
                            && !exclusion.matches_available(file, None).unwrap()
                    })
                    .collect();
                assert_eq!(
                    snapshot
                        .match_name_columns(&query, &[exclusion], &AtomicBool::new(false))
                        .unwrap()
                        .unwrap(),
                    expected,
                    "{text} excluding {excluded}, generation {}",
                    snapshot.generation
                );
            }
        }
        for text in [
            "path:report",
            "path:synthetic/",
            "path:0/report",
            "path:报告",
            "path:notpresent",
        ] {
            let query = query::parse(text, &HashMap::new()).unwrap();
            let expected: roaring::RoaringBitmap = snapshot
                .live
                .iter()
                .filter(|&slot| {
                    query
                        .matches_available(
                            &snapshot.entries.at(slot as usize).to_owned_file(),
                            None,
                        )
                        .unwrap()
                })
                .collect();
            assert_eq!(
                snapshot
                    .match_name_columns(&query, &[], &AtomicBool::new(false))
                    .unwrap()
                    .unwrap(),
                expected
            );
        }
        for text in [
            "case:report",
            "regex:report",
            "content:report",
            "report size:>1",
            "child:report",
        ] {
            let query = query::parse(text, &HashMap::new()).unwrap();
            assert!(
                snapshot
                    .match_name_columns(&query, &[], &AtomicBool::new(false))
                    .unwrap()
                    .is_none(),
                "{text}"
            );
        }
    }
    let query = query::parse("report", &HashMap::new()).unwrap();
    assert!(
        updated
            .match_name_columns(&query, &[], &AtomicBool::new(true))
            .is_err()
    );
}

#[test]
fn row_evaluation_masks_historical_postings_with_current_visibility() {
    let original = synthetic_snapshot(20);
    let changes: Vec<_> = original
        .entries
        .iter()
        .step_by(2)
        .map(|file| (file.id(), None))
        .collect();
    let deleted: Vec<_> = changes.iter().map(|(id, _)| *id).collect();
    let mut updated = SearchSnapshot::from_changes(changes, 2, &original).unwrap();
    // Deliberately retain a conservative historical candidate index. Ordinary
    // deletion also updates postings; exercise the visibility boundary itself.
    updated.trigrams = original.trigrams.clone();
    assert!(updated.trigrams.get(b"rep").unwrap().contains(0));
    assert!(!updated.live.contains(0));
    let directory = tempfile::tempdir().unwrap();
    let engine = SearchEngine::open(&directory.path().join("empty.sqlite")).unwrap();
    engine.refresh(true).unwrap();
    engine.snapshot.store(Arc::new(updated));
    for text in [
        "report",
        "regex:report",
        "!unavailable",
        "report | unavailable",
        "a",
    ] {
        let response = engine.call(json!({"op":"query","text":text,"limit":200}));
        assert_eq!(response["success"], true, "{response}");
        assert_eq!(response["total"], 10, "{text}");
        for row in response["rows"].as_array().unwrap() {
            assert!(!deleted.contains(&row["id"].as_i64().unwrap()), "{text}");
        }
    }
}

#[test]
#[ignore = "Release-mode million-record first-page benchmark; constructs metadata in memory"]
fn uncached_first_page_latency() {
    let count: usize = std::env::var("APFSEARCH_PROFILE_ROWS")
        .unwrap_or_else(|_| "1000000".into())
        .parse()
        .unwrap();
    assert!((1..=1_000_000).contains(&count));
    let original = Arc::new(synthetic_snapshot(count));
    let directory = tempfile::tempdir().unwrap();
    let engine = SearchEngine::open(&directory.path().join("empty.sqlite")).unwrap();
    engine.refresh(true).unwrap();
    let mut results = Vec::new();
    let mut generation = 1;
    for text in [
        "report",
        "analysis",
        "report analysis",
        "report report report report report",
        "报告",
        "a",
        "report-document-0000999",
        "unavailable",
    ] {
        let query = query::parse(text, &HashMap::new()).unwrap();
        let expected: roaring::RoaringBitmap = original
            .live
            .iter()
            .filter(|slot| {
                query
                    .matches_available(&original.entries.at(*slot as usize), None)
                    .unwrap()
            })
            .collect();
        let expected_rows: Vec<_> = original
            .name_order
            .iter()
            .filter(|slot| expected.contains(**slot))
            .take(200)
            .map(|slot| original.entries.at(*slot as usize).id())
            .collect();
        let mut samples = Vec::new();
        for _ in 0..30 {
            // Each sample gets a fresh generation and empty query caches through
            // the normal incremental publication path, not a unique cache key.
            let mut replacement = original.entries.at(count / 2).to_owned_file();
            replacement.size += 1;
            generation += 1;
            let updated = SearchSnapshot::from_changes(
                vec![(replacement.id, Some(replacement))],
                generation,
                &original,
            )
            .unwrap();
            engine.snapshot.store(Arc::new(updated));
            let start = Instant::now();
            let response = engine.call(json!({"op":"query","text":text,"limit":200}));
            let encoded = serde_json::to_vec(&response).unwrap();
            std::hint::black_box(encoded);
            samples.push(start.elapsed().as_secs_f64() * 1000.);
            assert_eq!(response["success"], true, "{response}");
            assert_eq!(response["generation"].as_u64(), Some(generation));
            assert_eq!(response["total"].as_u64(), Some(expected.len()), "{text}");
            let actual: Vec<_> = response["rows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["id"].as_i64().unwrap())
                .collect();
            assert_eq!(actual, expected_rows, "{text}");
        }
        samples.sort_by(f64::total_cmp);
        results.push(json!({"query":text,"total":expected.len(),"median_ms":samples[15],"p95_ms":samples[28]}));
    }
    println!(
        "{}",
        json!({"scope":"Uncached core query through SearchEngine.call plus JSON encoding; no XPC or UI; synthetic metadata only", "entries":count,"runs":30,"queries":results})
    );
}

#[test]
#[ignore = "Release-mode candidate and verification phase profile; synthetic metadata only"]
fn name_query_phase_latency() {
    let snapshot = synthetic_snapshot(1_000_000);
    let column = crate::name_column::NameColumn::build(&snapshot.entries);
    let cancelled = AtomicBool::new(false);
    let mut results = Vec::new();
    for text in [
        "report",
        "analysis",
        "report analysis",
        "报告",
        "report-document-0000999",
    ] {
        let query = query::parse(text, &HashMap::new()).unwrap();
        assert!(
            snapshot
                .exact_matches(&query, &cancelled)
                .unwrap()
                .is_none()
        );
        let expression = crate::name_column::NameExpression::compile(&query).unwrap();
        let expected: roaring::RoaringBitmap = snapshot
            .live
            .iter()
            .filter(|slot| {
                query
                    .matches_available(&snapshot.entries.at(*slot as usize), None)
                    .unwrap()
            })
            .collect();
        let mut samples = [Vec::new(), Vec::new()];
        let mut candidate_count = 0;
        for _ in 0..30 {
            let start = Instant::now();
            let candidates = snapshot
                .candidates_with_cancellation(&query, &cancelled)
                .unwrap();
            let visible = candidates.map(|set| set & &snapshot.live);
            let candidates = visible.as_ref().unwrap_or(&snapshot.live);
            samples[0].push(start.elapsed().as_secs_f64() * 1000.);
            candidate_count = candidates.len();
            let start = Instant::now();
            let matched = column
                .evaluate(&expression, &[], candidates, &cancelled)
                .unwrap();
            samples[1].push(start.elapsed().as_secs_f64() * 1000.);
            assert_eq!(matched, expected, "{text}");
        }
        for (phase, samples) in ["candidates_and_visibility", "name_verification"]
            .into_iter()
            .zip(&mut samples)
        {
            samples.sort_by(f64::total_cmp);
            results.push(json!({"query":text,"phase":phase,"candidates":candidate_count,"matches":expected.len(),"median_ms":samples[15],"p95_ms":samples[28]}));
        }
    }
    println!(
        "{}",
        json!({"scope":"In-memory sequential phase timings; extra column retained; excludes parsing, JSON, XPC and UI; not an end-to-end acceptance benchmark", "results":results})
    );
}

#[test]
#[ignore = "Release-mode text-layout experiment; one million in-memory records"]
fn name_layout_scan_latency() {
    let snapshot = synthetic_snapshot(1_000_000);
    let names: Vec<&str> = snapshot
        .entries
        .iter()
        .map(|file| file.search_name())
        .collect();
    let shared_names: crate::chunked_vec::ChunkedVec<_> = snapshot
        .entries
        .iter()
        .map(|file| file.search_name())
        .collect();
    let mut pool = String::new();
    let mut offsets = Vec::with_capacity(names.len() + 1);
    for name in &names {
        offsets.push(pool.len());
        pool.push_str(name);
    }
    offsets.push(pool.len());
    let mut results = Vec::new();
    for text in ["report", "analysis", "报告", "unavailable"] {
        let finder = memchr::memmem::Finder::new(text);
        let expected: Vec<_> = names
            .iter()
            .enumerate()
            .filter_map(|(slot, name)| finder.find(name.as_bytes()).map(|_| slot))
            .collect();
        let mut samples = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
        for run in 0..30 {
            for offset in 0..4 {
                let method = (run + offset) % 4;
                let start = Instant::now();
                let matches: Vec<_> = (0..names.len())
                    .filter(|&slot| {
                        let bytes = match method {
                            0 => snapshot.entries.at(slot).search_name().as_bytes(),
                            1 => names[slot].as_bytes(),
                            2 => &pool.as_bytes()[offsets[slot]..offsets[slot + 1]],
                            3 => shared_names[slot].as_bytes(),
                            _ => unreachable!(),
                        };
                        finder.find(std::hint::black_box(bytes)).is_some()
                    })
                    .collect();
                samples[method].push(start.elapsed().as_secs_f64() * 1000.);
                assert_eq!(matches, expected);
            }
        }
        for (method, samples) in samples.iter_mut().enumerate() {
            samples.sort_by(f64::total_cmp);
            let layout = [
                "record",
                "reference_column",
                "text_pool",
                "shared_text_column",
            ][method];
            results.push(json!({"query":text,"layout":layout,"matches":expected.len(),"median_ms":samples[15],"p95_ms":samples[28]}));
        }
    }
    println!(
        "{}",
        json!({"scope":"Layout experiment, identical literal matcher; includes collection of matching slots, excludes parsing, candidate indexes, sorting, transport and UI", "entries":names.len(),"extra_reference_column_bytes":names.capacity()*std::mem::size_of::<&str>(),"extra_pool_capacity_bytes":pool.capacity()+offsets.capacity()*std::mem::size_of::<usize>(),"queries":results})
    );
}

#[test]
#[ignore = "Million-record memory profile; synthetic metadata only"]
fn snapshot_memory_profile() {
    let started = Instant::now();
    let snapshot = synthetic_snapshot(1_000_000);
    let build_ms = started.elapsed().as_secs_f64() * 1000.;
    // Read the current process before allocating accounting sets. This is RSS,
    // not a sum of allocations or a claim about the installed application's RSS.
    let output = std::process::Command::new("/bin/ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    assert!(output.status.success());
    let resident_kib: u64 = String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    println!(
        "{}",
        json!({"scope":"Synthetic compact storage payload inventory, not allocator accounting", "build_ms":build_ms,"resident_kib":resident_kib,"storage":snapshot.entries.storage_metrics()})
    );
}
