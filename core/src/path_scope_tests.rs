//! Independent semantic and operation-count checks for component-aware scopes.
//! All scale checks use strings, not filesystem enumeration.
use crate::scanner::{self, PathScopes};
use std::{cell::Cell, path::Path};

fn naive_compact(mut roots: Vec<String>) -> Vec<String> {
    roots.sort();
    roots.dedup();
    let mut result: Vec<String> = Vec::new();
    for root in roots {
        if !result.iter().any(|parent| {
            Path::new(&root).starts_with(parent)
                && scanner::subtree_in_namespace(&root, std::slice::from_ref(parent))
        }) {
            result.push(root);
        }
    }
    result
}
fn strings(paths: &[&str]) -> Vec<String> {
    paths.iter().map(|path| (*path).into()).collect()
}

#[test]
fn compact_scopes_match_the_previous_algorithm_across_namespace_combinations() {
    let candidates = strings(&[
        "/",
        "/System",
        "/System/Volumes",
        "/System/Volumes/Data",
        "/System/Volumes/Data/usr",
        "/System/Volumes/Preboot/chosen",
        "/System/Volumes/Preboot/chosen/sub",
        "/System/Volumes/Preboot/other",
        "/Users",
        "/Users/example",
    ]);
    for mask in 0..(1usize << candidates.len()) {
        let paths: Vec<_> = candidates
            .iter()
            .enumerate()
            .filter(|(index, _)| mask & (1 << index) != 0)
            .map(|(_, path)| path.clone())
            .collect();
        assert_eq!(
            scanner::compact_roots(paths.clone(), || false).unwrap(),
            naive_compact(paths),
            "namespace subset {mask}"
        );
    }
}

#[test]
fn prefix_neighbors_never_hide_a_selected_parent_or_expand_its_scope() {
    let roots = strings(&["/a", "/a-0", "/a/child", "/aa/child", "/a-0/child"]);
    assert_eq!(
        scanner::compact_roots(roots, || false).unwrap(),
        strings(&["/a", "/a-0", "/aa/child"])
    );
    let scopes = PathScopes::from_paths(&strings(&["/a"]));
    assert!(scopes.covers(Path::new("/a/child")));
    assert!(!scopes.covers(Path::new("/a-0/child")));
    assert!(!scopes.covers(Path::new("/aa/child")));
}

#[test]
fn path_component_equivalence_matches_starts_with_without_resolving_parent_segments() {
    let groups = [
        strings(&["/工作/./数据//", "/a/../b", "/other//child"]),
        strings(&["/", "/System/Volumes/Preboot/chosen"]),
        strings(&["relative/./root", "../parent", "."]),
    ];
    let queries = [
        "/工作/数据/file",
        "/工作/数据-old/file",
        "/工作//数据/../兄弟",
        "/a/../b/file",
        "/b/file",
        "/other/child/file",
        "/other/children",
        "/System/Volumes/Preboot/chosen/file",
        "relative/root/file",
        "../parent/file",
        "relative/rootish",
        "./relative/root",
        "",
    ];
    for roots in groups {
        let scopes = PathScopes::from_paths(&roots);
        for query in queries {
            let expected = roots.iter().any(|root| Path::new(query).starts_with(root));
            assert_eq!(
                scopes.covers(Path::new(query)),
                expected,
                "roots={roots:?}, query={query:?}"
            );
            assert_eq!(
                scopes.contains(Path::new(query)),
                roots.iter().any(|root| Path::new(query) == Path::new(root))
            );
        }
        assert_eq!(
            scanner::compact_roots(roots.clone(), || false).unwrap(),
            naive_compact(roots)
        );
    }
}

#[test]
fn ancestor_predicates_continue_past_a_rejected_match() {
    let scopes = PathScopes::from_paths(&strings(&["/", "/System", "/System/Volumes/Data"]));
    let path = Path::new("/System/Volumes/Data/usr");
    assert!(scopes.covers_where(path, |parent| parent == Path::new("/")));
    assert!(!scopes.covers_where(path, |parent| parent == Path::new("/Users")));
    assert!(
        scopes.covers_where(path, |parent| scanner::subtree_in_namespace(
            path.to_str().unwrap(),
            &[parent.to_str().unwrap().into()]
        ))
    );
}

#[test]
fn one_thousand_sibling_scopes_require_only_path_depth_probes() {
    let count = 1000usize;
    let roots: Vec<_> = (0..count)
        .map(|index| format!("/scope/sibling-{index:04}/item"))
        .collect();
    let scopes = PathScopes::from_paths(&roots);
    for root in &roots {
        assert!(scopes.covers(Path::new(&format!("{root}/child"))));
        assert!(!scopes.covers(Path::new(&format!("{root}-neighbor/child"))));
    }
    let probes = scopes.ancestor_probes();
    assert!(
        probes <= count * 8,
        "{probes} probes must depend on depth, not sibling count"
    );
    let mut naive_checks = 0usize;
    let mut selected: Vec<&String> = Vec::new();
    for root in &roots {
        assert!(!selected.iter().any(|parent| {
            naive_checks += 1;
            Path::new(root).starts_with(parent)
        }));
        selected.push(root);
    }
    assert_eq!(naive_checks, count * (count - 1) / 2);
    assert!(
        probes * 50 < naive_checks,
        "counted {probes} versus {naive_checks}"
    );
}

#[test]
fn normalization_cancellation_never_returns_a_partial_scope_set() {
    let roots: Vec<_> = (0..16)
        .map(|index| format!("/filesearch-cancel-fixture/{index}"))
        .collect();
    assert!(scanner::compact_roots(roots.clone(), || true).is_none());
    assert!(scanner::normalize_roots_until(&roots, || true).is_none());
    let calls = Cell::new(0usize);
    assert!(scanner::compact_roots(roots.clone(), || {
        calls.set(calls.get() + 1);
        calls.get() >= 7
    })
    .is_none());
    assert_eq!(calls.get(), 7);
    let calls = Cell::new(0usize);
    assert!(scanner::normalize_roots_until(&roots, || {
        calls.set(calls.get() + 1);
        calls.get() >= 6
    })
    .is_none());
    assert_eq!(calls.get(), 6);
}
