//! Algorithm equivalence and allocation checks for query hot paths.
use filesearch_core::{index_store::IndexedFile, query};
use memchr::memmem::Finder;
use serde_json::json;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    cmp::Ordering,
    collections::HashMap,
    hint::black_box,
    time::Instant,
};

thread_local! {
    static COUNT_ALLOCATIONS: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}
struct CountingAllocator;
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;
fn count_allocation() {
    if COUNT_ALLOCATIONS.try_with(Cell::get).unwrap_or(false) {
        let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
    }
}
// This test allocator forwards all allocation contracts unchanged to System;
// the thread-local counter observes only the deliberately measured interval.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count_allocation();
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count_allocation();
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        count_allocation();
        unsafe { System.realloc(pointer, layout, size) }
    }
}
fn measured<T>(operation: impl FnOnce() -> T) -> (T, usize, f64) {
    ALLOCATIONS.set(0);
    COUNT_ALLOCATIONS.set(true);
    let start = Instant::now();
    let value = operation();
    let ms = start.elapsed().as_secs_f64() * 1000.;
    COUNT_ALLOCATIONS.set(false);
    (value, ALLOCATIONS.get(), ms)
}

fn timed<T>(mut operation: impl FnMut() -> T) -> (T, f64, Vec<f64>) {
    let mut samples = Vec::with_capacity(5);
    let mut value = None;
    for _ in 0..5 {
        let start = Instant::now();
        value = Some(operation());
        samples.push(start.elapsed().as_secs_f64() * 1000.);
    }
    let mut sorted = samples.clone();
    sorted.sort_by(f64::total_cmp);
    (value.unwrap(), sorted[2], samples)
}
// Frozen pre-optimization implementation, retained solely as a behavior oracle.
fn previous_natural_compare(a: &str, b: &str) -> Ordering {
    let mut left = a.chars().peekable();
    let mut right = b.chars().peekable();
    loop {
        match (left.peek().copied(), right.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(a), Some(b)) if a.is_ascii_digit() && b.is_ascii_digit() => {
                let mut left_digits = String::new();
                let mut right_digits = String::new();
                while left.peek().is_some_and(|c| c.is_ascii_digit()) {
                    left_digits.push(left.next().unwrap());
                }
                while right.peek().is_some_and(|c| c.is_ascii_digit()) {
                    right_digits.push(right.next().unwrap());
                }
                let a = left_digits.trim_start_matches('0');
                let b = right_digits.trim_start_matches('0');
                let order = a.len().cmp(&b.len()).then_with(|| a.cmp(b));
                if !order.is_eq() {
                    return order;
                }
            }
            (Some(a), Some(b)) => {
                let order = a.cmp(&b);
                if !order.is_eq() {
                    return order;
                }
                left.next();
                right.next();
            }
        }
    }
}
fn random(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}
fn corpus() -> Vec<String> {
    let mut names: Vec<String> = [
        "",
        "a",
        "a0",
        "a00",
        "a1",
        "a01",
        "a12x",
        "a123",
        "000",
        "0a",
        "00a",
        "a10/2",
        "a2/10",
        "中文９",
        "中文9",
        "العربية٢",
        "a\0b",
        "café2",
        "cafe\u{301}02",
        "😀0002",
        "😀10",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    names.push(format!("report{}", "9".repeat(256)));
    names.push(format!("report1{}", "0".repeat(256)));
    let pieces = [
        "0", "00", "1", "9", "10", "00123", "x", "ß", "İ", "中", "😀", "/", ".", "_", "é",
        "e\u{301}", "９", "٢",
    ];
    let mut seed = 12345678;
    for _ in 0..512 {
        let mut name = String::new();
        for _ in 0..(random(&mut seed) % 15) {
            name.push_str(pieces[random(&mut seed) as usize % pieces.len()]);
        }
        names.push(name);
    }
    names
}
fn file(path: &str) -> IndexedFile {
    let mut file: IndexedFile = serde_json::from_value(json!({
        "id":1, "path":path, "name":std::path::Path::new(path).file_name().unwrap_or_default().to_string_lossy(),
        "extension":"txt", "size":12, "modified":0, "created":0, "changed":0,
        "is_dir":false, "is_symlink":false,"file_id":1,"parent_id":0,"volume_id":"fixture","flags":0
    })).unwrap();
    file.prepare();
    file
}
#[test]
fn natural_comparison_matches_previous_unicode_and_numeric_order_without_allocating() {
    let names = corpus();
    for left in &names {
        for right in &names {
            assert_eq!(
                query::natural_cmp_folded(left, right),
                previous_natural_compare(left, right),
                "{left:?} / {right:?}"
            );
        }
    }
    let (_, previous_allocations, _) =
        measured(|| previous_natural_compare("x0012/a90", "x12/a900"));
    let (_, allocations, _) = measured(|| {
        for left in &names {
            for right in &names {
                black_box(query::natural_cmp_folded(black_box(left), black_box(right)));
            }
        }
    });
    assert!(previous_allocations > 0);
    assert_eq!(allocations, 0, "prepared-name comparison must not allocate");
}
#[test]
fn prepared_substring_matching_and_parent_scope_preserve_results_without_allocating() {
    let paths = [
        "/a/Café 中😀/Render02.txt",
        "/a/STRASSE/other.txt",
        "/a/İ/strasse.txt",
        "/",
        "/a/",
        "relative/file.txt",
        "a//Café///file.txt",
        "",
        ".",
    ];
    for path in paths {
        let file = file(path);
        for needle in [
            "a", "é", "CAFE", "中", "😀", "straße", "i", "xy", "02", "render",
        ] {
            let folded_needle = query::fold_search(needle);
            for (prefix, haystack) in [
                ("name:", query::fold_search(&file.name)),
                ("path:", query::fold_search(&file.path)),
                ("path-part:", query::fold_search(&file.parent)),
            ] {
                let parsed = query::parse(&format!("{prefix}{needle}"), &HashMap::new()).unwrap();
                let expected = haystack.contains(&folded_needle);
                assert_eq!(
                    parsed.matches(&file, None).unwrap(),
                    expected,
                    "{prefix}{needle} {path:?}"
                );
                let (actual, allocations, _) = measured(|| parsed.matches(&file, None).unwrap());
                assert_eq!(actual, expected);
                assert_eq!(allocations, 0, "{prefix}{needle} {path:?}");
            }
        }
    }
}
#[test]
fn compiled_word_matching_preserves_overlaps_and_unicode_boundaries() {
    for (text, name, expected) in [
        ("ww:ana", "banana ana.txt", true),
        ("ww:ana", "banana.txt", false),
        ("ww:ana", "anana.txt", false),
        ("prefix:ana", "banana_ana.txt", true),
        ("ww:中", "中文.txt", false),
        ("ww:中", "文-中.txt", true),
        ("suffix:cafe", "decafé.txt", true),
        ("ww:cafe", "café.txt", true),
        ("case:ww:Render", "x Render.txt", true),
        ("case:ww:Render", "x render.txt", false),
    ] {
        let parsed = query::parse(text, &HashMap::new()).unwrap();
        assert_eq!(
            parsed.matches(&file(name), None).unwrap(),
            expected,
            "{text} {name}"
        );
    }
}
#[test]
fn ascii_folding_matches_the_previous_icu_pipeline() {
    use icu_casemap::CaseMapper;
    use unicode_normalization::UnicodeNormalization;
    let mut samples = corpus();
    samples.extend((0..=127u8).map(|byte| format!("PREFIX{}Suffix", char::from(byte))));
    samples.push((0..=127u8).map(char::from).collect());
    for text in samples {
        let expected = CaseMapper::new()
            .fold_string(&text.nfc().collect::<String>())
            .into_owned();
        assert_eq!(query::fold(&text), expected, "{text:?}");
    }
}
#[test]
fn clock_hint_includes_relative_date_macros_without_classifying_plain_words_as_dates() {
    let macros = HashMap::from([("recent".into(), "dm:7days".into())]);
    for text in ["dm:today", "!<size:1 | recent:>", "dc:2024-01"] {
        assert!(
            query::parse(text, &macros).unwrap().may_depend_on_clock(),
            "{text}"
        );
    }
    for text in ["", "name:today", "size:>1mb", "content:2024-01"] {
        assert!(
            !query::parse(text, &macros).unwrap().may_depend_on_clock(),
            "{text}"
        );
    }
}

#[test]
#[ignore = "release microbenchmark; reports ratios without timing assertions"]
fn report_query_hot_path_speed() {
    let pairs: Vec<_> = (0..16_384).map(|index| {
        let path = format!("/Users/example/Documents/project-2026/09/13/build-12345/Contents/Resources/目录/render-{index:08}-version-00012.txt");
        let other = format!("/Users/example/Documents/project-2026/09/13/build-12345/Contents/Resources/目录/render-{:08}-version-00012.txt", (index * 193 + 17) % 16384);
        (path, other)
    }).collect();
    let compare = |comparison: fn(&str, &str) -> Ordering| {
        let mut checksum = 0i64;
        for _ in 0..32 {
            for (left, right) in &pairs {
                checksum += black_box(comparison(black_box(left), black_box(right))) as i64;
            }
        }
        checksum
    };
    let (_, old_allocations, _) = measured(|| compare(previous_natural_compare));
    let (_, new_allocations, _) = measured(|| compare(query::natural_cmp_folded));
    let (before, old_ms, old_samples) = timed(|| compare(previous_natural_compare));
    let (after, new_ms, new_samples) = timed(|| compare(query::natural_cmp_folded));
    assert_eq!(before, after);
    assert_eq!(new_allocations, 0);
    let mut substrings = vec![];
    for needle in [
        "r",
        "zz",
        "render",
        "version-00012",
        "not-present-substring",
        "目录",
        "目录/render-000001",
    ] {
        let finder = Finder::new(needle);
        let (old_count, old_find_ms, _) = timed(|| {
            let mut count = 0;
            for _ in 0..32 {
                for (path, _) in &pairs {
                    count += black_box(path).contains(black_box(needle)) as usize;
                }
            }
            count
        });
        let (new_count, new_find_ms, _) = timed(|| {
            let mut count = 0;
            for _ in 0..32 {
                for (path, _) in &pairs {
                    count += finder.find(black_box(path).as_bytes()).is_some() as usize;
                }
            }
            count
        });
        assert_eq!(old_count, new_count);
        substrings.push(json!({"needle":needle,"matches":new_count,"before_ms":old_find_ms,"after_ms":new_find_ms,"speedup":old_find_ms/new_find_ms}));
    }
    let report = json!({"comparisons":pairs.len()*32,"natural_order":{"before_ms":old_ms,"after_ms":new_ms,"speedup":old_ms/new_ms,"before_samples_ms":old_samples,"after_samples_ms":new_samples,"before_allocations":old_allocations,"after_allocations":new_allocations,"identical_checksum":before==after},"substring_primitives":substrings,"boundary":"Synthetic shared-prefix Unicode paths, isolated release process; excludes index traversal, XPC, paging, and GUI"});
    println!("{report}");
    if let Ok(path) = std::env::var("FILESEARCH_QUERY_HOT_PATH_REPORT") {
        std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
}
