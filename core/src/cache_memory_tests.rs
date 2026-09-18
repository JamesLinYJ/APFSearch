use super::*;
use crate::entry_table::FileEntry;
use std::time::Instant;

fn load_profile_snapshot() -> (SearchSnapshot, u64) {
    let path = std::env::var_os("APFSEARCH_PROFILE_CACHE").expect("Set APFSEARCH_PROFILE_CACHE");
    let mut header = [0u8; 24];
    File::open(&path).unwrap().read_exact(&mut header).unwrap();
    let mut position = 8;
    let generation = get64(&header, &mut position).unwrap();
    let revision = get64(&header, &mut position).unwrap();
    let snapshot = read(Path::new(&path), generation, revision)
        .expect("A valid production cache is required")
        .0;
    (snapshot, revision)
}

fn dense_page_reference(
    order: &[u32],
    count: usize,
    matched: &RoaringBitmap,
    offset: usize,
    adaptive: bool,
) -> Vec<u32> {
    let words = count.div_ceil(64);
    let prefix = if adaptive {
        (words + matched.len() as usize).min(order.len())
    } else {
        0
    };
    let mut page = Vec::new();
    let mut rank = 0;
    for &slot in &order[..prefix] {
        if matched.contains(slot) {
            if rank >= offset {
                page.push(slot);
            }
            rank += 1;
            if page.len() == 200 {
                return page;
            }
        }
    }
    if prefix == order.len() {
        return page;
    }
    let mut bits = vec![0u64; words];
    for slot in matched.iter() {
        bits[slot as usize / 64] |= 1u64 << (slot % 64);
    }
    for &slot in &order[prefix..] {
        if bits[slot as usize / 64] & (1u64 << (slot % 64)) != 0 {
            if rank >= offset {
                page.push(slot);
            }
            rank += 1;
            if page.len() == 200 {
                break;
            }
        }
    }
    page
}

#[test]
#[ignore = "Same-process first-page selection profile on an existing disposable cache"]
fn prepared_cache_page_selection_profile() {
    let (snapshot, _) = load_profile_snapshot();
    // The slice-only reference algorithms must not time conversion of the
    // production leaf representation as part of page selection.
    let reference_order = snapshot.name_order.to_vec();
    let mut ranks = vec![0u32; snapshot.entries.len()];
    for (rank, &slot) in snapshot.name_order.iter().enumerate() {
        ranks[slot as usize] = rank as u32;
    }
    let cancelled = std::sync::atomic::AtomicBool::new(false);
    let order = crate::result_order::ResultOrder::name();
    let mut results = Vec::new();
    for text in ["report", "报告", "0000007", "txt"] {
        let query = crate::query::parse(text, &Default::default()).unwrap();
        let matched = snapshot
            .match_name_columns(&query, &[], &cancelled)
            .unwrap()
            .unwrap();
        for offset in [0usize, 200, 10_000] {
            let expected: Vec<_> = snapshot
                .name_order
                .iter()
                .copied()
                .filter(|slot| matched.contains(*slot))
                .skip(offset)
                .take(200)
                .collect();
            let mut timings = [Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new()];
            for run in 0..30 {
                for rotation in 0..5 {
                    let method = (run + rotation) % 5;
                    let started = Instant::now();
                    let page = if method == 0 {
                        snapshot
                            .name_order
                            .iter()
                            .copied()
                            .filter(|slot| matched.contains(*slot))
                            .skip(offset)
                            .take(200)
                            .collect::<Vec<_>>()
                    } else if method == 4 {
                        crate::ordered_page::select(
                            &reference_order,
                            snapshot.entries.len(),
                            &matched,
                            offset,
                            200,
                            &cancelled,
                        )
                        .unwrap()
                    } else if method == 3 {
                        dense_page_reference(
                            &reference_order,
                            snapshot.entries.len(),
                            &matched,
                            offset,
                            false,
                        )
                    } else {
                        let mut slots: Vec<_> = matched.iter().collect();
                        crate::result_order::select_prefix(
                            &mut slots,
                            offset + 200,
                            |a, b| {
                                if method == 1 {
                                    order.compare(
                                        &snapshot.entries.at(a as usize),
                                        &snapshot.entries.at(b as usize),
                                    )
                                } else {
                                    ranks[a as usize].cmp(&ranks[b as usize])
                                }
                            },
                            &cancelled,
                        )
                        .unwrap();
                        slots.into_iter().skip(offset).take(200).collect()
                    };
                    timings[method].push(started.elapsed().as_secs_f64() * 1000.);
                    assert_eq!(page, expected, "{text}, offset {offset}, method {method}");
                }
            }
            for (method, samples) in timings.iter_mut().enumerate() {
                samples.sort_by(f64::total_cmp);
                results.push(json!({"query":text,"offset":offset,"matches":matched.len(),"method":method,"median_ms":samples[15],"p95_ms":samples[28]}));
            }
        }
    }
    println!(
        "{}",
        json!({"scope":"Page selection only; serial; rank construction excluded; no production algorithm changed","extra_rank_bytes":ranks.capacity()*4,"results":results})
    );
}

#[test]
#[ignore = "Explicitly export one derived layout fixture from an existing disposable cache"]
fn prepared_cache_export_layout_fixture() {
    let output = PathBuf::from(
        std::env::var_os("APFSEARCH_PROFILE_OUTPUT").expect("Set APFSEARCH_PROFILE_OUTPUT"),
    );
    let (snapshot, revision) = load_profile_snapshot();
    // Reserve a new output name atomically before publishing its sections.
    drop(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&output)
            .unwrap(),
    );
    write(&output, &snapshot, revision).unwrap();
    let restored = read(&output, snapshot.generation, revision).unwrap().0;
    assert_eq!(restored.len(), snapshot.len());
}

#[test]
#[ignore = "Same-process cache layout comparison; reads a disposable cache and encodes only in memory"]
fn prepared_cache_name_layout_profile() {
    let (original, revision) = load_profile_snapshot();
    let generation = original.generation;
    let before = process_usage();
    let mut encoded = Cursor::new(Vec::new());
    let started = Instant::now();
    encode_snapshot(&mut encoded, &original, revision).unwrap();
    let encode_ms = started.elapsed().as_secs_f64() * 1000.;
    let packed = decode(encoded.get_ref(), generation, revision).unwrap();
    assert_eq!(original.len(), packed.len());
    let mut results = Vec::new();
    for text in [
        "report",
        "document",
        "image",
        "report report",
        "report | image",
        "0000007",
    ] {
        let query = crate::query::parse(text, &Default::default()).unwrap();
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let expected = original
            .match_name_columns(&query, &[], &cancelled)
            .unwrap()
            .unwrap();
        let mut samples = [Vec::new(), Vec::new()];
        for run in 0..30 {
            for variant in [run % 2, (run + 1) % 2] {
                let snapshot = [&original, &packed][variant];
                let started = Instant::now();
                let matched = snapshot
                    .match_name_columns(&query, &[], &cancelled)
                    .unwrap()
                    .unwrap();
                samples[variant].push(started.elapsed().as_secs_f64() * 1000.);
                assert_eq!(matched, expected, "{text}");
            }
        }
        for (variant, samples) in samples.iter_mut().enumerate() {
            samples.sort_by(f64::total_cmp);
            results.push(json!({"query":text,"variant":variant,"matches":expected.len(),"median_ms":samples[15],"p95_ms":samples[28]}));
        }
    }
    let after = process_usage();
    assert_eq!(before["disk_bytes_written"], after["disk_bytes_written"]);
    assert_eq!(before["logical_writes"], after["logical_writes"]);
    println!(
        "{}",
        json!({"scope":"Production file mappings versus an anonymous test archive; same-process name matching, both snapshots retained; excludes result pages, XPC and UI", "archive_bytes":encoded.get_ref().len(),"encode_ms":encode_ms,"queries":results})
    );
}

fn process_usage() -> serde_json::Value {
    let mut usage = std::mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
    // SAFETY: the public Darwin API writes exactly a v4 record into this
    // correctly sized and aligned buffer. Only inspect it after success.
    let status = unsafe {
        libc::proc_pid_rusage(
            std::process::id() as i32,
            libc::RUSAGE_INFO_V4,
            usage.as_mut_ptr().cast(),
        )
    };
    assert_eq!(status, 0, "{}", std::io::Error::last_os_error());
    // SAFETY: proc_pid_rusage successfully initialized the v4 record.
    let usage = unsafe { usage.assume_init() };
    json!({"physical_footprint_bytes":usage.ri_phys_footprint,
        "peak_physical_footprint_bytes":usage.ri_lifetime_max_phys_footprint,
        "resident_bytes":usage.ri_resident_size,
        "disk_bytes_read":usage.ri_diskio_bytesread,
        "disk_bytes_written":usage.ri_diskio_byteswritten,
        "logical_writes":usage.ri_logical_writes})
}

#[test]
#[ignore = "Read-only profiling of an explicitly supplied disposable cache file"]
fn prepared_cache_memory_profile() {
    let path = std::env::var_os("APFSEARCH_PROFILE_CACHE")
        .expect("Set APFSEARCH_PROFILE_CACHE to a disposable test cache");
    let before = process_usage();
    let mut file = File::open(&path).unwrap();
    let mut header = [0u8; 24];
    file.read_exact(&mut header).unwrap();
    let bytes = file.metadata().unwrap().len();
    let mut offset = 8;
    let generation = get64(&header, &mut offset).unwrap();
    let revision = get64(&header, &mut offset).unwrap();
    let started = Instant::now();
    let snapshot = read(Path::new(&path), generation, revision)
        .expect("Cache must pass the complete production decoder")
        .0;
    let elapsed = started.elapsed().as_secs_f64() * 1000.;
    let after = process_usage();
    assert_eq!(before["logical_writes"], after["logical_writes"]);
    assert_eq!(before["disk_bytes_written"], after["disk_bytes_written"]);
    println!(
        "{}",
        json!({"scope":"Read-only production cache restoration; includes checksum, mapping validation and derived indexes; excludes SQLite and XPC; OS cache warmth uncontrolled","entries":snapshot.len(),"manifest_bytes":bytes,"elapsed_ms":elapsed,"before":before,"after_restore":after,"storage":snapshot.entries.storage_metrics()})
    );
    std::hint::black_box(snapshot);
}

#[test]
#[ignore = "Read-only allocation inventory of an explicitly supplied disposable cache"]
fn prepared_cache_string_layout_profile() {
    let (snapshot, _) = load_profile_snapshot();
    let mut inventory = crate::memory_inventory::Inventory::default();
    inventory.owner(true);
    snapshot.inventory(&mut inventory);
    let mut prefixes = std::collections::HashSet::new();
    let mut block_prefix_bytes = 0;
    let mut block_prefix_count = 0;
    for chunk in &snapshot.entries.chunks {
        let references: std::collections::HashSet<_> =
            [&chunk.path, &chunk.folded_path, &chunk.search_path]
                .into_iter()
                .flat_map(|column| column.values().iter().map(|path| path.prefix))
                .collect();
        block_prefix_count += references.len();
        for reference in references {
            let prefix = chunk.text_at(reference);
            block_prefix_bytes += prefix.len();
            prefixes.insert(prefix);
        }
    }
    println!(
        "{}",
        json!({"storage":snapshot.entries.storage_metrics(),"inventory":inventory.report(),
        "block_prefix_bytes":block_prefix_bytes,"block_prefix_count":block_prefix_count,
        "unique_prefix_bytes":prefixes.iter().map(|prefix| prefix.len()).sum::<usize>(),"unique_prefix_count":prefixes.len()})
    );
}

#[test]
#[ignore = "Read-only cache restoration followed by in-memory incremental updates"]
fn prepared_cache_incremental_update_profile() {
    let (original, _) = load_profile_snapshot();
    assert!(original.len() > 4096);
    let mut current =
        SearchSnapshot::from_changes(Vec::new(), original.generation + 1, &original).unwrap();
    let before = process_usage();
    let mut samples = Vec::new();
    for iteration in 0..64 {
        let slot = (iteration * 4096) % original.entries.len();
        let previous = &current.entries.at(slot);
        let old_name = previous.search_name();
        let mut replacement = previous.to_owned_file();
        replacement.size = replacement.size.checked_add(1).unwrap();
        let rename = iteration % 8 == 0;
        if rename {
            replacement.name = format!("apfsearch-memory-change-{iteration}.txt");
            replacement.path = format!("{}/{}", replacement.parent, replacement.name);
        }
        let expected_name = replacement.name.clone();
        let started = Instant::now();
        let updated = SearchSnapshot::from_changes(
            vec![(replacement.id, Some(replacement))],
            current.generation + 1,
            &current,
        )
        .unwrap();
        let update_ms = started.elapsed().as_secs_f64() * 1000.;
        assert_eq!(updated.len(), current.len());
        assert!(crate::entry_table::same_record(
            &current.entries.at((slot + 1) % current.entries.len()),
            &updated.entries.at((slot + 1) % current.entries.len())
        ));
        if !rename {
            assert!(crate::entry_table::same_text_storage(
                old_name,
                updated.entries.at(slot).search_name()
            ));
        } else {
            let query = crate::query::parse(&expected_name, &HashMap::new()).unwrap();
            let cancelled = std::sync::atomic::AtomicBool::new(false);
            let old_matches = current
                .match_name_columns(&query, &[], &cancelled)
                .unwrap()
                .unwrap();
            let new_matches = updated
                .match_name_columns(&query, &[], &cancelled)
                .unwrap()
                .unwrap();
            assert!(old_matches.is_empty());
            assert_eq!(new_matches, RoaringBitmap::from_iter([slot as u32]));
        }
        samples.push(json!({"rename":rename,"update_ms":update_ms}));
        current = updated;
    }
    let after = process_usage();
    assert_eq!(before["logical_writes"], after["logical_writes"]);
    assert_eq!(before["disk_bytes_written"], after["disk_bytes_written"]);
    println!(
        "{}",
        json!({"scope":"64 successive in-memory updates after prepared-cache restoration; original snapshot retained and checked; excludes FSEvents, SQL commits and UI",
        "entries":original.len(),"before":before,"after":after,"samples":samples})
    );
}

#[test]
#[ignore = "Read-only posting-directory profiling of a disposable cache"]
fn prepared_cache_posting_update_profile() {
    let (original, _) = load_profile_snapshot();
    let mut keys: Vec<_> = original
        .trigrams
        .iter()
        .filter(|(_, bits)| !bits.is_empty())
        .map(|(key, _)| *key)
        .collect();
    keys.sort_unstable();
    let before = process_usage();
    let mut samples = Vec::new();
    for key in keys.iter().step_by((keys.len() / 100).max(1)).take(100) {
        let slot = original.trigrams.get(key).unwrap().min().unwrap();
        let mut changed = original.trigrams.clone();
        let start = Instant::now();
        let directory = Arc::make_mut(&mut changed);
        let cloned_ms = start.elapsed().as_secs_f64() * 1000.;
        Arc::make_mut(directory.get_mut(key).unwrap()).remove(slot);
        let update_ms = start.elapsed().as_secs_f64() * 1000.;
        assert!(original.trigrams.get(key).unwrap().contains(slot));
        assert!(!changed.get(key).unwrap().contains(slot));
        samples.push(json!({"directory_clone_ms":cloned_ms,"update_ms":update_ms}));
    }
    let after = process_usage();
    assert_eq!(before["logical_writes"], after["logical_writes"]);
    println!(
        "{}",
        json!({"scope":"Posting-directory COW plus one bitmap edit; excludes sorting and other snapshot columns", "entries":original.len(),"trigram_count":keys.len(),"samples":samples,"before":before,"after":after})
    );
}

#[test]
#[ignore = "Read-only sparse/dense sort-update crossover benchmark"]
fn prepared_cache_order_update_profile() {
    let (original, _) = load_profile_snapshot();
    let order = crate::result_order::ResultOrder::name();
    let original_order = original.name_order.to_vec();
    let mut reports = Vec::new();
    for count in [1usize, 16, 256, 1024, 2048, 4096, 16_384] {
        if count > original.len() {
            continue;
        }
        let changed: RoaringBitmap = (0..count)
            .map(|i| (i * original.len() / count) as u32)
            .collect();
        let mut entries = original.entries.clone();
        for slot in &changed {
            let mut entry = entries.at(slot as usize).to_owned_file();
            entry.name = format!("replacement-{slot}.txt");
            entry.path = format!("{}/{}", entry.parent, entry.name);
            entry.prepare();
            entries.set(slot as usize, entry);
        }
        let mut samples = Vec::new();
        for run in 0..6 {
            let mut outputs = Vec::new();
            for candidate in [run % 2 == 0, run % 2 != 0] {
                let start = Instant::now();
                let elapsed_ms = if candidate {
                    let result = crate::index_store::updated_order(
                        &original.name_order,
                        &original.entries,
                        &changed,
                        &original.live,
                        &entries,
                        &order,
                    );
                    let elapsed = start.elapsed().as_secs_f64() * 1000.;
                    outputs.push(result.to_vec());
                    elapsed
                } else {
                    let result = crate::index_store::linear_updated_order_reference(
                        &original_order,
                        &changed,
                        &entries,
                        &order,
                    );
                    let elapsed = start.elapsed().as_secs_f64() * 1000.;
                    outputs.push(result);
                    elapsed
                };
                samples.push(json!({"candidate":candidate,"elapsed_ms":elapsed_ms}));
            }
            assert_eq!(outputs[0], outputs[1]);
        }
        reports.push(json!({"changes":count,"samples":samples}));
    }
    println!(
        "{}",
        json!({"scope":"In-memory name-order update crossover, excludes other derived columns and SQL", "entries":original.len(), "reports":reports})
    );
}
