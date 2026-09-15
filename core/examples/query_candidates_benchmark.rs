//! In-memory, uncached candidate and matcher benchmark. No database or filesystem scan.
use apfsearch_core::entry_table::FileEntry;
use apfsearch_core::{
    index_store::{IndexedFile, SearchSnapshot},
    query,
};
use serde_json::json;
use std::{collections::HashMap, hint::black_box, time::Instant};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let count: usize = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "100000".into())
        .parse()?;
    if !(1..=1_000_000).contains(&count) {
        return Err("count must be 1..1000000".into());
    }
    let entries = (0..count)
        .map(|i| {
            let name = format!(
                "report-document-{:07}-{}.txt",
                i,
                if i % 97 == 0 { "报告" } else { "analysis" }
            );
            IndexedFile {
                id: i as i64 + 1,
                path: format!("/synthetic-candidates/{}/{name}", i / 1000),
                name,
                extension: "txt".into(),
                size: i as u64,
                modified: 0,
                created: 0,
                changed: 0,
                modified_ns: 0,
                changed_ns: 0,
                is_dir: false,
                is_symlink: false,
                file_id: i as u64 + 1,
                parent_id: i as u64 / 1000,
                volume_id: "synthetic".into(),
                flags: 0,
                properties: json!({}),
                content_indexed: false,
                folded_name: Default::default(),
                folded_extension: Default::default(),
                folded_path: Default::default(),
                search_name: Default::default(),
                search_path: Default::default(),
                parent: Default::default(),
            }
        })
        .collect();
    let start = Instant::now();
    let snapshot = SearchSnapshot::new(entries, 1);
    let build_ms = start.elapsed().as_secs_f64() * 1000.;
    let mut results = Vec::new();
    for text in [
        "report-document-0000999",
        "report-document-0000999.txt",
        "report-document-unavailable",
        "report report report report report",
        "report",
        "analysis",
        "报告",
        "a",
        "it",
        "absenttoken size:>10",
    ] {
        let query = query::parse(text, &HashMap::new())?;
        let mut evaluator = query.evaluator();
        let expected: Vec<_> = snapshot
            .live
            .iter()
            .filter(|slot| {
                query
                    .matches(&snapshot.entries.at(*slot as usize), None)
                    .unwrap()
            })
            .collect();
        let mut samples = Vec::new();
        let mut matching_samples = Vec::new();
        let mut row_api_samples = Vec::new();
        let mut candidate_count = 0;
        for run in 0..30 {
            let start = Instant::now();
            let candidates = black_box(snapshot.candidates(black_box(&query)));
            samples.push(start.elapsed().as_secs_f64() * 1000.);
            candidate_count = candidates
                .as_ref()
                .map_or(snapshot.len(), |x| x.len() as usize);
            // Alternate order to avoid systematically giving one evaluator a
            // warmer record working set. Both paths perform the same matching.
            for prepared in [run % 2 == 0, run % 2 != 0] {
                let start = Instant::now();
                let selected: Vec<_> = candidates
                    .as_ref()
                    .unwrap_or(&snapshot.live)
                    .iter()
                    .filter(|slot| {
                        snapshot.live.contains(*slot)
                            && if prepared {
                                evaluator
                                    .matches_available(&snapshot.entries.at(*slot as usize), None)
                            } else {
                                query.matches_available(&snapshot.entries.at(*slot as usize), None)
                            }
                            .unwrap()
                    })
                    .collect();
                let elapsed = start.elapsed().as_secs_f64() * 1000.;
                if prepared {
                    matching_samples.push(elapsed);
                } else {
                    row_api_samples.push(elapsed);
                }
                assert_eq!(selected, expected, "{text}");
            }
        }
        samples.sort_by(f64::total_cmp);
        matching_samples.sort_by(f64::total_cmp);
        row_api_samples.sort_by(f64::total_cmp);
        results.push(json!({"query":text,"candidates":candidate_count,"matches":expected.len(),"candidate_median_ms":samples[15],"candidate_p95_ms":samples[28],"prepared_verify_median_ms":matching_samples[15],"row_api_verify_median_ms":row_api_samples[15]}));
    }
    let mut update_samples = Vec::new();
    for _ in 0..30 {
        let mut replacement = snapshot.entries.at(count / 2).to_owned_file();
        replacement.size += 1;
        let start = Instant::now();
        let updated =
            SearchSnapshot::from_changes(vec![(replacement.id, Some(replacement))], 2, &snapshot)
                .ok_or("Incremental update rejected")?;
        update_samples.push(start.elapsed().as_secs_f64() * 1000.);
        assert_eq!(
            updated.entries.at(count / 2).size(),
            snapshot.entries.at(count / 2).size() + 1
        );
    }
    update_samples.sort_by(f64::total_cmp);
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"scope":"In-memory candidate filtering and full matcher; no result cache, database, XPC or UI", "entries":count,"snapshot_build_ms":build_ms,"single_update_median_ms":update_samples[15],"single_update_p95_ms":update_samples[28],"runs":30,"queries":results})
        )?
    );
    Ok(())
}
