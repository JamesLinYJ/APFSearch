use super::discard_removed_path_errors;
use std::time::Instant;

fn reference(errors: &mut Vec<String>, removed: &[String]) {
    errors.retain(|error| {
        !removed
            .iter()
            .any(|path| error.starts_with(&format!("{path}: ")))
    });
}

#[test]
fn removal_errors_preserve_exact_path_boundaries_and_all_other_diagnostics() {
    let removed = vec!["/fixture/a".into(), "/fixture/目录: 名字.txt".into()];
    let retained = vec![
        "/fixture/ab: Permission denied".to_string(),
        "/fixture/a/child: Permission denied".into(),
        "/fixture/目录: 其他.txt: Permission denied".into(),
        "Could not inspect mounted filesystems: unavailable".into(),
        "permission error without a path separator".into(),
    ];
    let mut errors = retained.clone();
    errors.extend([
        "/fixture/a: No such file".into(),
        "/fixture/目录: 名字.txt: No such file: retry".into(),
    ]);
    discard_removed_path_errors(&mut errors, &removed);
    assert_eq!(errors, retained);
}

#[test]
fn indexed_removal_error_filter_matches_previous_semantics() {
    let paths: Vec<String> = ["a", "ab", "a/b", "目录", "e\u{301}", "a: b", "a: b: c", " "]
        .into_iter()
        .map(|name| format!("/fixture/{name}"))
        .collect();
    let errors: Vec<String> = paths
        .iter()
        .flat_map(|path| {
            ["No such file", "Permission denied: unavailable", ""]
                .map(|message| format!("{path}: {message}"))
        })
        .collect();
    for mask in 0..(1 << paths.len()) {
        let removed: Vec<String> = paths
            .iter()
            .enumerate()
            .filter(|(index, _)| mask & (1 << index) != 0)
            .map(|(_, path)| path.clone())
            .collect();
        let mut expected = errors.clone();
        reference(&mut expected, &removed);
        let mut actual = errors.clone();
        discard_removed_path_errors(&mut actual, &removed);
        assert_eq!(actual, expected, "subset {mask}");
    }
}

#[test]
fn large_removal_burst_filters_diagnostics_without_filesystem_access() {
    let removed: Vec<String> = (0..100_000)
        .map(|id| format!("/fixture/{id}.txt"))
        .collect();
    let mut errors: Vec<String> = removed
        .iter()
        .map(|path| format!("{path}: missing"))
        .collect();
    errors.push("/fixture/protected: Permission denied".into());
    discard_removed_path_errors(&mut errors, &removed);
    assert_eq!(errors, ["/fixture/protected: Permission denied"]);
}

#[test]
#[ignore = "explicit alternating CPU comparison; synthetic metadata only"]
fn removal_error_filter_profile() {
    let removed: Vec<String> = (0..4096)
        .map(|id| format!("/fixture/directory/{id}.txt"))
        .collect();
    let errors: Vec<String> = removed
        .iter()
        .rev()
        .map(|path| format!("{path}: missing"))
        .collect();
    for round in 0..5 {
        for candidate in if round % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            let mut output = errors.clone();
            let started = Instant::now();
            if candidate {
                discard_removed_path_errors(&mut output, &removed);
            } else {
                reference(&mut output, &removed);
            }
            let elapsed = started.elapsed();
            assert!(output.is_empty());
            eprintln!(
                "removal_filter round={round} candidate={candidate} elapsed_us={}",
                elapsed.as_micros()
            );
        }
    }
}
