//! Metadata-only scope probe; excluded namespaces are never opened.
use apfsearch_core::scanner;
use std::{sync::atomic::AtomicBool, time::Instant};
fn main() {
    let roots: Vec<String> = std::env::args().skip(1).collect();
    assert!(!roots.is_empty(), "Provide mount paths to inspect");
    let started = Instant::now();
    let mut paths = Vec::new();
    let report = scanner::scan(&roots, &AtomicBool::new(false), |batch| {
        paths.extend(batch.into_iter().map(|file| file.path));
    });
    println!(
        "{}",
        serde_json::json!({"report":report,"paths":paths,
        "elapsed_ms":started.elapsed().as_secs_f64()*1000.})
    );
}
