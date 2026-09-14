//! Independent semantic checks for the dense numeric index. Expected matches
//! come from the full per-file evaluator, never from column/block internals.
use crate::{
    index_store::{IndexedFile, SearchSnapshot},
    query::{self, Query, Term},
};
use chrono::{DateTime, Duration, Local, TimeZone};
use roaring::RoaringBitmap;
use serde_json::json;
use std::{
    collections::HashMap,
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};

fn now() -> DateTime<Local> {
    Local
        .with_ymd_and_hms(2024, 3, 1, 12, 0, 0)
        .earliest()
        .unwrap()
}
fn file(id: i64, size: u64, modified: i64, created: i64, is_dir: bool) -> IndexedFile {
    serde_json::from_value(json!({
        "id": id, "path": format!("/numeric-fixture/a{id}.txt"),
        "name": format!("a{id}.txt"), "extension": "txt", "size": size,
        "modified": modified, "created": created, "changed": modified,
        "is_dir": is_dir, "is_symlink": false, "file_id": id as u64,
        "parent_id": 1, "volume_id": "numeric-fixture", "flags": 0,
        "properties": {"width": if id % 2 == 0 { 200 } else { 20 }}
    }))
    .unwrap()
}
fn parse(text: &str) -> Query {
    query::parse_at(text, &HashMap::new(), now()).unwrap_or_else(|error| panic!("{text}: {error}"))
}
fn reference(snapshot: &SearchSnapshot, query: &Query) -> RoaringBitmap {
    snapshot
        .live
        .iter()
        .filter(|slot| {
            query
                .matches_available(&snapshot.entries[*slot as usize], None)
                .unwrap()
        })
        .collect()
}
fn check(snapshot: &SearchSnapshot, query: &Query, label: &str) -> RoaringBitmap {
    let expected = reference(snapshot, query);
    let actual = snapshot
        .exact_matches(query, &AtomicBool::new(false))
        .unwrap_or_else(|error| panic!("{label}: {error}"))
        .unwrap_or_else(|| panic!("{label}: supported query was not evaluated exactly"));
    assert_eq!(actual, expected, "{label}");
    assert!(
        actual.is_subset(&snapshot.live),
        "{label}: returned a deleted slot"
    );
    actual
}
fn ids(snapshot: &SearchSnapshot, matches: &RoaringBitmap) -> Vec<i64> {
    matches
        .iter()
        .map(|slot| snapshot.entries[slot as usize].id)
        .collect()
}
fn number(
    field: &str,
    low: f64,
    high: f64,
    include_low: bool,
    include_high: bool,
    negate: bool,
) -> Query {
    Query::Term(Term::Number {
        field: field.into(),
        low,
        high,
        include_low,
        include_high,
        negate,
    })
}

#[test]
fn numeric_size_precision_and_large_integer_casts_match_full_evaluator() {
    let sizes = [
        0,
        1,
        1023,
        1024,
        1025,
        1535,
        1536,
        1537,
        1638,
        1639,
        2047,
        2048,
        (1u64 << 53) - 1,
        1u64 << 53,
        (1u64 << 53) + 1,
        (1u64 << 53) + 2,
        u64::MAX,
    ];
    let snapshot = SearchSnapshot::new(
        sizes
            .iter()
            .enumerate()
            .map(|(index, size)| file(index as i64 + 1, *size, 0, 0, false))
            .collect(),
        1,
    );
    for text in [
        "size:>=1kb",
        "size:>1kb",
        "size:<=1kb",
        "size:<1kb",
        "size:=1kb",
        "size:!=1kb",
        "size:1kb",
        "size:!1kb",
        "size:1.5kb",
        "size:1.5kb..2kb",
        "size:=9007199254740992",
        "size:!=9007199254740993",
        "size:>=9007199254740993",
        "size:>9007199254740992",
        "size:=18446744073709551615",
    ] {
        check(&snapshot, &parse(text), text);
    }
    assert_eq!(
        ids(
            &snapshot,
            &check(&snapshot, &parse("size:1.5kb"), "precision bucket")
        ),
        vec![7, 8, 9]
    );
    // The established matcher casts u64 to f64. This test preserves that public
    // behavior at 2^53 instead of silently giving the new index different rules.
    assert_eq!(
        ids(
            &snapshot,
            &check(
                &snapshot,
                &parse("size:=9007199254740992"),
                "f64 integer rounding"
            )
        ),
        vec![14, 15]
    );
    let pivot = 1024f64;
    let edges = [
        f64::NEG_INFINITY,
        f64::from_bits(pivot.to_bits() - 1),
        pivot,
        f64::from_bits(pivot.to_bits() + 1),
        f64::INFINITY,
    ];
    for (low_index, &low) in edges.iter().enumerate() {
        for &high in &edges[low_index..] {
            for include_low in [false, true] {
                for include_high in [false, true] {
                    for negate in [false, true] {
                        check(
                            &snapshot,
                            &number("size", low, high, include_low, include_high, negate),
                            &format!(
                                "bounds {low:?}..{high:?}, {include_low}/{include_high}, negate={negate}"
                            ),
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn numeric_unknown_size_differs_from_outer_boolean_not() {
    let snapshot = SearchSnapshot::new(
        vec![
            file(1, 1024, 0, 0, false),
            file(2, 1025, 0, 0, false),
            file(3, 1024, 0, 0, true),
            file(4, u64::MAX, 0, 0, true),
        ],
        1,
    );
    for (text, expected) in [
        ("size:!=1024", vec![2]),
        ("!size:=1024", vec![2, 3, 4]),
        ("size:unknown", vec![3, 4]),
        ("size:!unknown", vec![1, 2]),
        ("size:!=unknown", vec![1, 2]),
        ("!size:unknown", vec![1, 2]),
        ("dm:unknown", vec![]),
        ("dc:!unknown", vec![1, 2, 3, 4]),
        ("folder: size:>=0", vec![]),
        ("folder: !size:<0", vec![3, 4]),
    ] {
        assert_eq!(
            ids(&snapshot, &check(&snapshot, &parse(text), text)),
            expected,
            "{text}"
        );
    }
    let empty = SearchSnapshot::new(vec![], 1);
    for text in ["size:>=0", "!size:=0", "size:unknown", "dm:today"] {
        assert!(check(&empty, &parse(text), text).is_empty());
    }
}

#[test]
fn numeric_calendar_ranges_and_relative_clock_keep_exact_boundaries() {
    let clock = now();
    let midnight = Local
        .with_ymd_and_hms(2024, 3, 1, 0, 0, 0)
        .earliest()
        .unwrap()
        .timestamp();
    let tomorrow = Local
        .with_ymd_and_hms(2024, 3, 2, 0, 0, 0)
        .earliest()
        .unwrap()
        .timestamp();
    let timestamps = [
        midnight - 1,
        midnight,
        clock.timestamp() - 3601,
        clock.timestamp() - 3600,
        clock.timestamp(),
        clock.timestamp() + 1,
        tomorrow - 1,
        tomorrow,
    ];
    let snapshot = SearchSnapshot::new(
        timestamps
            .iter()
            .enumerate()
            .map(|(index, timestamp)| {
                file(index as i64 + 1, 0, *timestamp, *timestamp, index % 2 == 0)
            })
            .collect(),
        1,
    );
    for text in [
        "dm:today",
        "dm:yesterday",
        "dm:2024-02-29",
        "dc:2024-02",
        "dm:>=2024-03-01",
        "dm:>2024-03-01",
        "dc:<2024-03-01",
        "dc:<=2024-03-01",
        "dm:!=today",
        "!dc:today",
        "dm:2024-02-29..2024-03-01",
        "dm:1hour",
        "dc:!1hour",
        "dm:1month",
        "dm:1year",
        "dm:lastmonth",
        "dc:thisyear",
    ] {
        check(&snapshot, &parse(text), text);
    }
    assert_eq!(
        ids(
            &snapshot,
            &check(&snapshot, &parse("dm:today"), "local day")
        ),
        vec![2, 3, 4, 5, 6, 7]
    );
    let frozen = query::parse_at("dm:1hour", &HashMap::new(), clock).unwrap();
    let later = query::parse_at("dm:1hour", &HashMap::new(), clock + Duration::hours(1)).unwrap();
    assert_eq!(
        ids(
            &snapshot,
            &check(&snapshot, &frozen, "frozen relative hour")
        ),
        vec![4, 5]
    );
    assert_eq!(
        ids(&snapshot, &check(&snapshot, &later, "later relative hour")),
        vec![5, 6]
    );
    check(
        &snapshot,
        &frozen,
        "original query remains bound to its clock",
    );
}

#[test]
fn numeric_signed_date_columns_preserve_f64_comparison_semantics() {
    let timestamps = [
        i64::MIN,
        -(1i64 << 53) - 1,
        -(1i64 << 53),
        -1,
        0,
        1,
        (1i64 << 53) - 1,
        1i64 << 53,
        (1i64 << 53) + 1,
        i64::MAX,
    ];
    let snapshot = SearchSnapshot::new(
        timestamps
            .iter()
            .enumerate()
            .map(|(index, timestamp)| {
                file(
                    index as i64 + 1,
                    0,
                    *timestamp,
                    timestamp.saturating_neg(),
                    index % 2 == 0,
                )
            })
            .collect(),
        1,
    );
    for field in ["modified", "created"] {
        for pivot in [
            i64::MIN as f64,
            -(1i64 << 53) as f64,
            -1.,
            0.,
            1.,
            (1i64 << 53) as f64,
            i64::MAX as f64,
        ] {
            for negate in [false, true] {
                for inclusive in [false, true] {
                    check(
                        &snapshot,
                        &number(field, pivot, f64::INFINITY, inclusive, true, negate),
                        field,
                    );
                    check(
                        &snapshot,
                        &number(field, pivot, pivot, true, true, negate),
                        field,
                    );
                }
            }
        }
    }
}

fn block_fixture() -> SearchSnapshot {
    // Three full 4096-value blocks plus a partial block: all-unknown, mixed,
    // all-in-range, and a final partial byte. No database or filesystem writes.
    let clock = now().timestamp();
    SearchSnapshot::new(
        (0..12_305)
            .map(|slot| {
                let size = if slot < 4096 {
                    u64::MAX
                } else if slot < 8192 {
                    (slot % 2049) as u64
                } else {
                    4096
                };
                file(
                    slot + 1,
                    size,
                    clock + slot - 8192,
                    clock - slot,
                    slot < 4096 || slot % 97 == 0,
                )
            })
            .collect(),
        1,
    )
}

#[test]
fn numeric_block_shortcuts_and_mixed_boolean_expressions_match_full_scan() {
    let snapshot = block_fixture();
    for text in [
        "size:>=1024",
        "size:>4096",
        "size:<=4096",
        "size:=4096",
        "size:!=4096",
        "size:unknown",
        "!size:>=1024",
        "size:>=1024 ext:txt",
        "<size:<1024 | folder:> a",
        "!<size:<1024 | dm:today>",
        "<size:>=1024 dm:today> | <size:unknown dc:yesterday>",
        "<size:unknown | size:!=1024> !folder:",
        "!<size:unknown | !size:>=1024>",
        "<size:>=1024 | ext:pdf> <dm:today | !folder:>",
    ] {
        check(&snapshot, &parse(text), text);
    }
}

#[test]
fn numeric_incremental_updates_preserve_old_snapshot_and_deleted_slot_visibility() {
    let old = block_fixture();
    let texts = [
        "size:>=1024",
        "size:!=4096",
        "size:unknown",
        "!size:>=1024",
        "dm:today",
        "dc:today",
        "<size:>=1024 | folder:> !dm:yesterday",
        "!<size:unknown | size:=1>",
    ];
    let old_results: Vec<_> = texts
        .iter()
        .map(|text| check(&old, &parse(text), text))
        .collect();
    // Replace extrema and transition known<->unknown at either side of a block
    // boundary; resurrect a deleted slot in a later publication as well.
    let mut changed = old.entries[4095].as_ref().clone();
    changed.is_dir = false;
    changed.size = 1024;
    changed.modified = now().timestamp();
    let mut unknown = old.entries[4096].as_ref().clone();
    unknown.is_dir = true;
    unknown.size = 1;
    unknown.created = now().timestamp();
    let mut maximum = old.entries[8192].as_ref().clone();
    maximum.size = u64::MAX;
    maximum.modified = 0;
    let mut minimum = old.entries[8193].as_ref().clone();
    minimum.size = 1;
    minimum.created = now().timestamp();
    let current = SearchSnapshot::from_changes(
        vec![
            (4096, Some(changed)),
            (4097, Some(unknown)),
            (8193, Some(maximum)),
            (8194, Some(minimum)),
            (12_305, None),
            (
                12_306,
                Some(file(
                    12_306,
                    2048,
                    now().timestamp(),
                    now().timestamp(),
                    false,
                )),
            ),
        ],
        2,
        &old,
    )
    .unwrap();
    assert!(!current.live.contains(12_304));
    for (index, text) in texts.iter().enumerate() {
        check(&current, &parse(text), text);
        assert_eq!(
            check(&old, &parse(text), text),
            old_results[index],
            "old snapshot changed: {text}"
        );
    }
    let restored = SearchSnapshot::from_changes(
        vec![(12_305, Some(file(12_305, 0, now().timestamp(), 0, true)))],
        3,
        &current,
    )
    .unwrap();
    for text in texts {
        check(&restored, &parse(text), text);
    }
    assert!(restored.live.contains(12_304));
    assert!(!current.live.contains(12_304));
    assert_eq!(old.generation, 1);
    assert_eq!(current.generation, 2);
    assert_eq!(restored.generation, 3);
}

#[test]
fn numeric_mixed_unsupported_predicates_keep_conservative_candidates() {
    let snapshot = SearchSnapshot::new(
        (0..16)
            .map(|slot| file(slot + 1, slot as u64 * 512, 0, 0, false))
            .collect(),
        1,
    );
    for text in [
        "size:>=1024 width:>100",
        "size:>=1024 | width:>100",
        "!<size:>=1024 width:>100>",
    ] {
        let query = parse(text);
        assert!(
            snapshot
                .exact_matches(&query, &AtomicBool::new(false))
                .unwrap()
                .is_none(),
            "{text}"
        );
        let expected = reference(&snapshot, &query);
        assert!(
            !expected.is_empty(),
            "fixture must contain real matches: {text}"
        );
        if let Some(candidates) = snapshot
            .candidates_with_cancellation(&query, &AtomicBool::new(false))
            .unwrap()
        {
            assert!(
                expected.is_subset(&candidates),
                "candidate filtering lost matches: {text}"
            );
        }
    }
}

#[test]
fn numeric_cancelled_requests_never_return_partial_or_empty_success() {
    let snapshot = SearchSnapshot::new(vec![file(1, 1024, 0, 0, false)], 1);
    let cancelled = AtomicBool::new(true);
    for text in [
        "size:>=0",
        "size:unknown",
        "!size:=1024",
        "size:>=0 | ext:txt",
        "size:>=0 width:>100",
    ] {
        assert_eq!(
            snapshot
                .exact_matches(&parse(text), &cancelled)
                .unwrap_err(),
            "Query cancelled"
        );
        assert_eq!(
            snapshot
                .candidates_with_cancellation(&parse(text), &cancelled)
                .unwrap_err(),
            "Query cancelled"
        );
    }
}

#[test]
fn numeric_concurrent_cancellation_aborts_multiblock_evaluation() {
    let snapshot = Arc::new(block_fixture());
    let cancelled = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(2));
    let (reply, result) = mpsc::channel();
    let worker = {
        let snapshot = snapshot.clone();
        let cancelled = cancelled.clone();
        let barrier = barrier.clone();
        thread::spawn(move || {
            // A long Boolean expression keeps the in-memory evaluation active
            // without a huge fixture or any artificial sleeps in production.
            let query = Query::Or(
                (0..8192)
                    .map(|_| number("size", 512., 1536., true, false, false))
                    .collect(),
            );
            barrier.wait();
            reply
                .send(snapshot.exact_matches(&query, &cancelled))
                .unwrap();
        })
    };
    barrier.wait();
    thread::sleep(std::time::Duration::from_millis(1));
    assert!(
        matches!(result.try_recv(), Err(mpsc::TryRecvError::Empty)),
        "fixture completed before cancellation was issued"
    );
    cancelled.store(true, Ordering::Release);
    let response = result
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("numeric query ignored cancellation");
    worker.join().unwrap();
    assert_eq!(response.unwrap_err(), "Query cancelled");
    // An aborted request does not poison the snapshot or a new request token.
    check(&snapshot, &parse("size:>=1024"), "query after cancellation");
}
