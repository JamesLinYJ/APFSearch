//! Same-corpus acceptance harness; metadata never leaves the process.
//! Query fixtures use disposable empty databases; update fixtures mutate only a
//! clone of an explicitly supplied consistent backup, never the source database.
use super::*;
use crate::entry_table::FileEntry;
use std::io::Read;

type RestorePhases = Option<(Instant, Vec<(&'static str, f64)>)>;
#[test]
#[ignore = "Explicit immutable cache required; repacks one metadata block at a time in memory"]
fn prefix_pool_profile() {
    let path = PathBuf::from(std::env::var_os("APFSEARCH_ACCEPTANCE_CACHE").unwrap());
    let mut header = [0u8; 24];
    std::fs::File::open(&path)
        .unwrap()
        .read_exact(&mut header)
        .unwrap();
    let generation = u64::from_le_bytes(header[8..16].try_into().unwrap());
    let revision = u64::from_le_bytes(header[16..24].try_into().unwrap());
    let (snapshot, _) = snapshot_cache::read(&path, generation, revision).unwrap();
    let mut original_bytes = 0usize;
    let mut candidate_bytes = 0usize;
    let started = Instant::now();
    for (index, chunk) in snapshot.entries.chunks.iter().enumerate() {
        let start = index * crate::entry_table::CHUNK_LENGTH;
        let rebuilt = crate::entry_table::EntryTable::from_rows(
            (start..start + chunk.len()).map(|slot| Ok(snapshot.entries.at(slot).to_owned_file())),
        )
        .unwrap();
        for offset in 0..chunk.len() {
            assert!(
                crate::entry_table::same_record(
                    &snapshot.entries.at(start + offset),
                    &rebuilt.at(offset)
                ),
                "Repacking must preserve every metadata field"
            );
        }
        original_bytes += chunk.text.text().len();
        candidate_bytes += rebuilt.chunks[0].text.text().len();
    }
    println!(
        "{}",
        json!({"scope":"One-block-at-a-time text arena reconstruction; all metadata fields compared; no cache writes or filesystem scan; not a process RSS benchmark",
        "entries":snapshot.entries.len(), "original_text_bytes":original_bytes,
        "candidate_text_bytes":candidate_bytes, "elapsed_ms":started.elapsed().as_secs_f64()*1000.})
    );
}
#[test]
#[ignore = "Read-only name dictionary study on an explicit immutable metadata cache"]
fn name_dictionary_profile() {
    use std::collections::{HashMap, HashSet};
    let path = PathBuf::from(std::env::var_os("APFSEARCH_ACCEPTANCE_CACHE").unwrap());
    let mut header = [0u8; 24];
    std::fs::File::open(&path)
        .unwrap()
        .read_exact(&mut header)
        .unwrap();
    let generation = u64::from_le_bytes(header[8..16].try_into().unwrap());
    let revision = u64::from_le_bytes(header[16..24].try_into().unwrap());
    let (snapshot, _) = snapshot_cache::read(&path, generation, revision).unwrap();
    let mut names = HashMap::<&str, u64>::new();
    let mut block_names = HashSet::new();
    let mut block_unique_bytes = 0usize;
    let mut reference_bytes = 0usize;
    let mut text_bytes = 0usize;
    let mut parent_overlap_bytes = 0usize;
    for chunk in &snapshot.entries.chunks {
        block_names.clear();
        for reference in chunk.search_name.values() {
            let name = chunk.text_at(*reference);
            *names.entry(name).or_default() += 1;
            if block_names.insert(name) {
                block_unique_bytes += name.len();
            }
        }
        text_bytes += chunk.text.text().len();
        let mut columns = HashSet::new();
        for column in [
            &chunk.name,
            &chunk.folded_name,
            &chunk.search_name,
            &chunk.parent,
        ] {
            let values = column.values();
            if columns.insert(values.as_ptr() as usize) {
                reference_bytes += std::mem::size_of_val(values);
            }
        }
        for column in [&chunk.path, &chunk.folded_path, &chunk.search_path] {
            let values = column.values();
            if columns.insert(values.as_ptr() as usize) {
                reference_bytes += std::mem::size_of_val(values);
            }
        }
        let mut ranges = Vec::new();
        let mut add = |reference: crate::entry_table::TextRef| {
            if reference.length != 0 {
                ranges.push((
                    reference.offset as usize,
                    reference.offset as usize + reference.length as usize,
                ));
            }
        };
        for column in [&chunk.name, &chunk.folded_name, &chunk.search_name] {
            for &reference in column.values() {
                add(reference);
            }
        }
        for column in [&chunk.path, &chunk.folded_path, &chunk.search_path] {
            for reference in column.values() {
                add(reference.prefix);
                add(reference.suffix);
            }
        }
        for (&parent, path) in chunk.parent.values().iter().zip(chunk.path.values()) {
            if chunk
                .text_at(path.prefix)
                .starts_with(chunk.text_at(parent))
            {
                add(crate::entry_table::TextRef {
                    offset: path.prefix.offset,
                    length: parent.length,
                });
            } else {
                add(parent);
            }
        }
        ranges.sort_unstable();
        let mut live_bytes = 0usize;
        let mut last_end = 0usize;
        for (start, end) in ranges {
            live_bytes += end.saturating_sub(last_end.max(start));
            last_end = last_end.max(end);
        }
        parent_overlap_bytes += chunk.text.text().len() - live_bytes;
    }
    let mut gram_counts = [0u64; 3];
    let mut grams = HashSet::new();
    for (name, &count) in &names {
        grams.clear();
        grams.extend(name.as_bytes().windows(3));
        gram_counts[0] += grams.len() as u64 * count;
        gram_counts[1] += grams.len() as u64;
        gram_counts[2] += count;
    }
    // Evaluate the actual existing Roaring representation in a name-ID
    // universe, rather than extrapolating memory from occurrence counts.
    let started = Instant::now();
    let mut dictionary: Vec<_> = names.keys().copied().collect();
    dictionary.sort_unstable();
    let mut name_postings = HashMap::<[u8; 3], roaring::RoaringBitmap>::new();
    for (id, name) in dictionary.iter().enumerate() {
        for gram in name.as_bytes().windows(3) {
            name_postings
                .entry(gram.try_into().unwrap())
                .or_default()
                .insert(id as u32);
        }
    }
    let mut postings_bytes = [0u64; 2];
    for posting in name_postings.values_mut() {
        let payload = |posting: &roaring::RoaringBitmap| {
            let stats = posting.statistics();
            stats.n_values_array_containers as u64 * 2
                + stats.n_bitset_containers as u64 * 8192
                + stats.n_bytes_run_containers
        };
        postings_bytes[0] += payload(posting);
        posting.optimize();
        postings_bytes[1] += payload(posting);
    }
    let prototype_ms = started.elapsed().as_secs_f64() * 1000.;
    println!(
        "{}",
        json!({
            "scope":"Exact normalized-name sharing potential; counts only, no production or RSS claim",
            "entries":snapshot.len(), "distinct_search_names":names.len(),
            "search_name_bytes_per_block":block_unique_bytes,
            "search_name_bytes_global":names.keys().map(|name| name.len()).sum::<usize>(),
        "all_text_bytes":text_bytes, "text_reference_bytes":reference_bytes,
        "text_bytes_reclaimable_by_parent_overlap":parent_overlap_bytes,
        "trigram_file_memberships":gram_counts[0], "trigram_unique_name_memberships":gram_counts[1],
        "rows_including_tombstones":gram_counts[2],
        "prototype_trigram_payload_bytes":postings_bytes,
        "prototype_trigram_build_ms":prototype_ms,
        "single_occurrence_names":names.values().filter(|&&count| count == 1).count()
        })
    );
}
#[test]
#[ignore = "Read-only representation study on an explicit immutable metadata cache"]
fn posting_representation_profile() {
    use roaring::RoaringBitmap;
    let path = PathBuf::from(std::env::var_os("APFSEARCH_ACCEPTANCE_CACHE").unwrap());
    let mut header = [0u8; 24];
    std::fs::File::open(&path)
        .unwrap()
        .read_exact(&mut header)
        .unwrap();
    let generation = u64::from_le_bytes(header[8..16].try_into().unwrap());
    let revision = u64::from_le_bytes(header[16..24].try_into().unwrap());
    let (snapshot, _) = snapshot_cache::read(&path, generation, revision).unwrap();
    let payload = |bitmap: &RoaringBitmap| {
        let stats = bitmap.statistics();
        stats.n_values_array_containers as u64 * 2
            + stats.n_bitset_containers as u64 * 8192
            + stats.n_bytes_run_containers
    };
    let mut totals = [[0u64; 6]; 6];
    let mut examine = |kind: usize, bitmap: &RoaringBitmap| {
        let mut compact = bitmap.clone();
        let row = &mut totals[kind];
        row[0] += 1;
        row[1] += payload(bitmap);
        row[3] += bitmap.serialized_size() as u64;
        let started = Instant::now();
        compact.optimize();
        row[5] += started.elapsed().as_nanos() as u64;
        assert_eq!(&compact, bitmap);
        row[2] += payload(&compact);
        row[4] += compact.serialized_size() as u64;
    };
    for shard in 0..crate::posting_map::SHARDS {
        for (_, bitmap) in snapshot.trigrams.partition_entries(shard) {
            examine(0, bitmap);
        }
        snapshot
            .metadata_postings
            .visit_partition(shard, |kind, _, bitmap| examine(kind as usize, bitmap));
    }
    println!(
        "{}",
        json!({"entries":snapshot.len(), "scope":"Representation study only; clones one bitmap at a time, no query latency claim, no cache writes", "columns":["posting_count","payload_before","payload_after","serialized_before","serialized_after","optimization_nanoseconds"],"kinds":["trigrams","single_bytes","byte_pairs","extensions","directories","symlinks"],"totals":totals})
    );
}
thread_local! {
    static RESTORE_PHASES: std::cell::RefCell<RestorePhases> =
        const { std::cell::RefCell::new(None) };
}

/// Test-only phase attribution; no paths or query text enter the report.
pub(crate) fn restore_phase(name: &'static str) {
    RESTORE_PHASES.with_borrow_mut(|state| {
        if let Some((started, phases)) = state {
            phases.push((name, started.elapsed().as_secs_f64() * 1000.));
            *started = Instant::now();
        }
    });
}

#[test]
#[ignore = "Explicit immutable acceptance cache required; reads metadata cache only"]
fn cache_restore_resource_profile() {
    let path = PathBuf::from(std::env::var_os("APFSEARCH_ACCEPTANCE_CACHE").unwrap());
    let mut header = [0u8; 24];
    std::fs::File::open(&path)
        .unwrap()
        .read_exact(&mut header)
        .unwrap();
    let generation = u64::from_le_bytes(header[8..16].try_into().unwrap());
    let revision = u64::from_le_bytes(header[16..24].try_into().unwrap());
    let before = usage();
    RESTORE_PHASES.with_borrow_mut(|state| *state = Some((Instant::now(), Vec::new())));
    let started = Instant::now();
    let (snapshot, _) = snapshot_cache::read(&path, generation, revision).unwrap();
    let restore_ms = started.elapsed().as_secs_f64() * 1000.;
    restore_phase("ready");
    let phases = RESTORE_PHASES.with_borrow_mut(|state| state.take().unwrap().1);
    println!(
        "{}",
        json!({
            "scope": "Cache recovery only; all validation precedes publication; no filesystem scan or writes",
            "entries": snapshot.len(), "restore_ms": restore_ms, "phases": phases,
            "before": before, "restored": usage()
        })
    );
}

#[test]
#[ignore = "Explicit consistent SQLite copy and new cache output required; no file traversal"]
fn prepare_database_acceptance_cache() {
    let source = PathBuf::from(std::env::var_os("APFSEARCH_ACCEPTANCE_DATABASE").unwrap());
    let output = PathBuf::from(std::env::var_os("APFSEARCH_ACCEPTANCE_OUTPUT").unwrap());
    assert!(
        !output.exists(),
        "Never overwrite an existing acceptance corpus"
    );
    let store = IndexStore {
        connection: rusqlite::Connection::open_with_flags(
            source,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap(),
        cache_path: output.clone(),
    };
    // Pin all metadata and settings to one SQLite read transaction.
    store.connection.execute_batch("BEGIN").unwrap();
    let generation = store.get("generation", json!(0)).as_u64().unwrap();
    let revision = store.get("revision", json!(0)).as_u64().unwrap();
    let before = usage();
    let started = Instant::now();
    let mut snapshot = store.snapshot(generation).unwrap();
    snapshot.content_revision = store.get("content_revision", json!(0)).as_u64().unwrap();
    let built = usage();
    let build_ms = started.elapsed().as_secs_f64() * 1000.;
    snapshot_cache::write(&output, &snapshot, revision).unwrap();
    println!(
        "{}",
        json!({"scope":"One-time cache conversion from read-only consistent SQLite, no filesystem metadata reads", "entries":snapshot.len(),"build_ms":build_ms,"total_ms":started.elapsed().as_secs_f64()*1000.,"before":before,"built":built,"published":usage()})
    );
}

#[test]
#[ignore = "Requires a consistent SQLite backup plus its matching cache; mutates an APFS clone only"]
fn database_update_lifecycle_profile() {
    let database = PathBuf::from(std::env::var_os("APFSEARCH_ACCEPTANCE_DATABASE").unwrap());
    let cache = PathBuf::from(std::env::var_os("APFSEARCH_ACCEPTANCE_CACHE").unwrap());
    let directory = tempfile::tempdir().unwrap();
    let copy = directory.path().join("index.sqlite");
    // clonefile-backed cp deliberately fails rather than silently copying many
    // gigabytes on an unsupported filesystem. The source must be a prior backup.
    assert!(
        std::process::Command::new("/bin/cp")
            .arg("-c")
            .arg(&database)
            .arg(&copy)
            .status()
            .unwrap()
            .success()
    );
    let destination = copy.with_extension("snapshot.bin");
    std::fs::copy(&cache, &destination).unwrap();
    let sections = destination.with_extension("sections");
    std::fs::create_dir(&sections).unwrap();
    for entry in std::fs::read_dir(cache.with_extension("sections")).unwrap() {
        let entry = entry.unwrap();
        assert!(entry.file_type().unwrap().is_file());
        std::fs::hard_link(entry.path(), sections.join(entry.file_name())).unwrap();
    }
    let mut header = [0u8; 24];
    std::fs::File::open(&cache)
        .unwrap()
        .read_exact(&mut header)
        .unwrap();
    let generation = u64::from_le_bytes(header[8..16].try_into().unwrap());
    let revision = u64::from_le_bytes(header[16..24].try_into().unwrap());
    {
        let store = IndexStore::open(&copy).unwrap();
        assert_eq!(store.get("revision", json!(null)), json!(revision));
        store.set("watch_enabled", &json!(false)).unwrap();
        store.set("roots", &json!([])).unwrap();
        store.set("offline", &json!(true)).unwrap();
        store.set("generation", &json!(generation)).unwrap();
        // Fixture setup pins this prior consistent backup to its prepared cache.
        // Use the same setup on both revisions, outside the measured workload.
        store
            .set("cache_base_generation", &json!(generation))
            .unwrap();
        store.set("cache_base_revision", &json!(revision)).unwrap();
        store.set("cache_journal_overflow", &json!(false)).unwrap();
        store.set("cache_dirty", &json!(false)).unwrap();
        store
            .connection
            .execute_batch("DELETE FROM cache_changes; DELETE FROM snapshot_changes;")
            .unwrap();
    }
    let engine = SearchEngine::open(&copy).unwrap();
    assert!(!engine.needs_cache_rebuild.load(Ordering::Relaxed));
    let entries = engine.snapshot.load().len();
    assert!(entries > 4096);
    let originals: Vec<_> = engine
        .snapshot
        .load()
        .visible_entries()
        .filter(|entry| !entry.content_indexed())
        .step_by(entries / 8)
        .take(8)
        .map(|entry| entry.to_owned_file())
        .collect();
    let export = engine.call(json!({"op":"retain_snapshot"}));
    assert_eq!(export["success"], true);
    let export_page =
        engine.call(json!({"op":"query","text":"","snapshot_lease":export["snapshot_lease"]}));
    let before = usage();
    let mut rounds = Vec::new();
    let inventory_each_round = std::env::var_os("APFSEARCH_ACCEPTANCE_INVENTORY").is_some();
    for round in 0..10 {
        let started = Instant::now();
        let usage_before = usage();
        for changed in [true, false] {
            {
                let store = engine.index_store.lock().unwrap();
                let transaction = store.connection.unchecked_transaction().unwrap();
                for (index, row) in originals.iter().enumerate() {
                    let (name, path, size) = if changed {
                        (
                            format!("apfsearch-lifecycle-{index}.txt"),
                            format!("/apfsearch-fixture/lifecycle-{index}.txt"),
                            row.size.saturating_add(1),
                        )
                    } else {
                        (row.name.clone(), row.path.clone(), row.size)
                    };
                    transaction
                        .execute(
                            "UPDATE files SET name=?1,path=?2,size=?3 WHERE id=?4",
                            rusqlite::params![name, path, size, row.id],
                        )
                        .unwrap();
                }
                transaction
                    .execute(
                        "UPDATE settings SET value=CAST(value AS INTEGER)+1 WHERE key='revision'",
                        [],
                    )
                    .unwrap();
                transaction
                    .execute(
                        "UPDATE settings SET value='true' WHERE key='cache_dirty'",
                        [],
                    )
                    .unwrap();
                transaction.commit().unwrap();
            }
            engine.refresh(true).unwrap();
            assert_eq!(engine.snapshot.load().len(), entries);
            let window = engine.call(
                json!({"op":"query","text":"","retain_snapshot":true,"snapshot_owner":"window"}),
            );
            assert_eq!(window["success"], true);
            let next = engine.call(json!({"op":"query","text":"","offset":200,"snapshot_lease":window["snapshot_lease"]}));
            assert_eq!(next["generation"], window["generation"]);
            engine.call(json!({"op":"release_snapshot","snapshot_lease":window["snapshot_lease"]}));
            let held = engine
                .call(json!({"op":"query","text":"","snapshot_lease":export["snapshot_lease"]}));
            assert_eq!(held["rows"], export_page["rows"]);
        }
        let after = usage();
        rounds.push(
            json!({"round":round,"elapsed_ms":started.elapsed().as_secs_f64()*1000.,
            "before":usage_before,"after":after}),
        );
        if inventory_each_round {
            // Explicit attribution run only. Keep this allocation-heavy walk
            // outside the timed work and out of normal acceptance runs.
            let diagnostic = engine.call(json!({"op":"status","diagnostics":"memory"}));
            rounds.last_mut().unwrap()["memory_inventory"] = diagnostic["memory_inventory"].clone();
        }
    }
    engine.call(json!({"op":"release_snapshot","snapshot_lease":export["snapshot_lease"]}));
    let diagnostic = engine.call(json!({"op":"status","diagnostics":"memory"}));
    assert!(!engine.scanning.load(Ordering::Relaxed));
    assert!(engine.worker.lock().unwrap().is_none());
    let after = usage();
    drop(engine);
    let reopened = SearchEngine::open(&copy).unwrap();
    assert!(!reopened.needs_cache_rebuild.load(Ordering::Relaxed));
    for row in &originals {
        let snapshot = reopened.snapshot.load();
        let slot = snapshot.slot_for_id(row.id).unwrap();
        assert!(entry_table::same_record(&snapshot.entries.at(slot), row));
    }
    println!(
        "{}",
        json!({"scope":"Ten change/restore cycles on an isolated APFS clone of real SQLite metadata, production delta refresh and cache publication, window leases and a long-lived export lease. No filesystem traversal or XPC.",
        "entries":entries,"before":before,"after":after,"after_reopen":usage(),"rounds":rounds,
        "memory_inventory":diagnostic.get("memory_inventory")})
    );
}

#[test]
#[ignore = "Prepare a disposable read-only service cache fixture; never user data"]
fn prepare_cached_service_fixture() {
    let source = PathBuf::from(std::env::var_os("APFSEARCH_ACCEPTANCE_CACHE").unwrap());
    let output = PathBuf::from(std::env::var_os("APFSEARCH_SERVICE_FIXTURE_OUTPUT").unwrap());
    std::fs::create_dir(&output).expect("Fixture output must be a new directory");
    let bytes = std::fs::read(&source).unwrap();
    assert_eq!(&bytes[..8], b"APFMAP04");
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
    let selected_cases = std::env::var("APFSEARCH_ACCEPTANCE_CASES")
        .ok()
        .map(|value| {
            value
                .split(',')
                .map(|case| {
                    let case = case.parse::<usize>().expect("Numeric acceptance case");
                    assert!(case < cases.len());
                    case
                })
                .collect::<std::collections::HashSet<_>>()
        });
    let mut results = Vec::new();
    for (case, (text, offset, sort)) in cases.into_iter().enumerate() {
        if selected_cases
            .as_ref()
            .is_some_and(|selected| !selected.contains(&case))
        {
            continue;
        }
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
