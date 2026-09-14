use super::*;
use serde_json::json;
use std::time::Instant;

/// Explicit pool selection is test-only. Production uses its single bounded pool.
fn evaluate_partitioned(
    column: &NameColumn,
    query: &NameExpression<'_>,
    candidates: &RoaringBitmap,
    workers: usize,
    cancelled: &AtomicBool,
) -> Result<RoaringBitmap, String> {
    if workers == 1 {
        return column.evaluate_slots(query, &[], candidates.iter(), cancelled);
    }
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .build()
        .unwrap();
    pool.install(|| column.evaluate_partitioned(query, &[], candidates, workers, cancelled))
}

#[test]
fn partitioned_matching_preserves_sparse_boundaries_and_cancellation() {
    let column = NameColumn {
        names: (0..8195)
            .map(|slot| {
                crate::shared_text::SharedText::from(if slot % 3 == 0 {
                    "报告 report"
                } else {
                    "analysis"
                })
            })
            .collect(),
    };
    let query = crate::query::parse("report", &Default::default()).unwrap();
    let expression = NameExpression::compile(&query).unwrap();
    let cancelled = AtomicBool::new(false);
    for candidates in [
        RoaringBitmap::new(),
        RoaringBitmap::from_iter([0, 4095, 4096, 8194]),
        (0..8195).step_by(7).collect(),
    ] {
        let expected = column
            .evaluate_slots(&expression, &[], candidates.iter(), &cancelled)
            .unwrap();
        for workers in [1, 2, 4, 8] {
            assert_eq!(
                evaluate_partitioned(&column, &expression, &candidates, workers, &cancelled)
                    .unwrap(),
                expected
            );
        }
    }
    cancelled.store(true, Ordering::Relaxed);
    assert!(
        evaluate_partitioned(&column, &expression, &(0..8195).collect(), 4, &cancelled).is_err()
    );
}

#[test]
#[ignore = "Release-mode bounded parallelism experiment; one million in-memory names"]
fn parallel_name_matching_latency() {
    let count = 1_000_000;
    let column = NameColumn {
        names: (0..count)
            .map(|slot| {
                crate::shared_text::SharedText::from(format!(
                    "report-document-{slot:07}-{}.txt",
                    if slot % 97 == 0 { "报告" } else { "analysis" }
                ))
            })
            .collect(),
    };
    let cancelled = AtomicBool::new(false);
    let pools: Vec<_> = [1, 2, 4, 8]
        .into_iter()
        .map(|workers| {
            rayon::ThreadPoolBuilder::new()
                .num_threads(workers)
                .build()
                .unwrap()
        })
        .collect();
    let mut results = Vec::new();
    for text in [
        "report",
        "report analysis",
        "report report report report report",
    ] {
        let query = crate::query::parse(text, &Default::default()).unwrap();
        let expression = NameExpression::compile(&query).unwrap();
        for size in [1_000, 10_000, 100_000, 1_000_000] {
            for stride in [1, 97] {
                let candidates: RoaringBitmap = (0..size).step_by(stride).collect();
                let expected = column
                    .evaluate_slots(&expression, &[], candidates.iter(), &cancelled)
                    .unwrap();
                let mut samples = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
                for run in 0..20 {
                    for offset in 0..4 {
                        let method = (run + offset) % 4;
                        let started = Instant::now();
                        let workers = [1, 2, 4, 8][method];
                        let actual = if workers == 1 {
                            column.evaluate_slots(&expression, &[], candidates.iter(), &cancelled)
                        } else {
                            pools[method].install(|| {
                                column.evaluate_partitioned(
                                    &expression,
                                    &[],
                                    &candidates,
                                    workers,
                                    &cancelled,
                                )
                            })
                        }
                        .unwrap();
                        let elapsed = started.elapsed().as_secs_f64() * 1000.;
                        assert_eq!(actual, expected);
                        if run >= 2 {
                            samples[method].push(elapsed);
                        }
                    }
                }
                for (method, mut samples) in samples.into_iter().enumerate() {
                    samples.sort_by(f64::total_cmp);
                    results.push(json!({"query":text,"slot_range":size,"candidates":candidates.len(),"stride":stride,"workers":([1,2,4,8][method]),"median_ms":samples[8],"p95_ms":samples[17]}));
                }
            }
        }
    }
    println!(
        "{}",
        json!({"scope":"Synthetic in-memory name-column matching; includes reusable Rayon pool dispatch, partitioning and bitmap union; excludes one-time pool initialization; excludes query planning, sorting, database, XPC and UI; candidate bitmap and strings shared by reference", "samples_per_case":18,"results":results})
    );
}

#[test]
fn parallel_dispatch_preserves_exclusions_unicode_and_boolean_matching() {
    let column = NameColumn {
        names: (0..100_003)
            .map(|slot| {
                crate::shared_text::SharedText::from(if slot % 3 == 0 {
                    "报告 report"
                } else {
                    "analysis"
                })
            })
            .collect(),
    };
    let candidates: RoaringBitmap = (0..100_003).filter(|slot| slot % 17 != 0).collect();
    let cancelled = AtomicBool::new(false);
    let exclusion = crate::query::parse("analysis", &Default::default()).unwrap();
    let exclusions = [NameExpression::compile(&exclusion).unwrap()];
    for text in ["report", "报告", "report | analysis", "!analysis"] {
        let query = crate::query::parse(text, &Default::default()).unwrap();
        let expression = NameExpression::compile(&query).unwrap();
        let expected: RoaringBitmap = candidates.iter().filter(|slot| slot % 3 == 0).collect();
        assert_eq!(
            column
                .evaluate(&expression, &exclusions, &candidates, &cancelled)
                .unwrap(),
            expected
        );
        for workers in [2, 4] {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(workers)
                .build()
                .unwrap();
            assert_eq!(
                pool.install(|| column
                    .evaluate_partitioned(
                        &expression,
                        &exclusions,
                        &candidates,
                        workers,
                        &cancelled
                    )
                    .unwrap()),
                expected
            );
        }
    }
}
