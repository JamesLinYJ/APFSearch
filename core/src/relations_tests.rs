use super::*;
use crate::{index_store::IndexStore, scanner::ScannedFile};
use std::sync::Arc;

fn fixture() -> SearchSnapshot {
    let temp = tempfile::tempdir().unwrap();
    let mut store = IndexStore::open(&temp.path().join("index.sqlite")).unwrap();
    let definitions = [
        ("/fixture", true, 0),
        ("/fixture/alpha", true, 0),
        ("/fixture/alpha/nested", true, 0),
        ("/fixture/empty", true, 0),
        ("/fixture/denied", true, 0),
        ("/fixture/alphabet", true, 0),
        ("/fixture/alpha/report.pdf", false, 10),
        ("/fixture/alpha/nested/中文.txt", false, 20),
        ("/fixture/alpha/alias.txt", false, 10),
        ("/fixture/alphabet/neighbor.txt", false, 7),
    ];
    let files: Vec<_> = definitions
        .into_iter()
        .enumerate()
        .map(|(id, (path, is_dir, size))| {
            let path = Path::new(path);
            ScannedFile {
                path: path.to_string_lossy().into(),
                name: path.file_name().unwrap().to_string_lossy().into(),
                extension: path
                    .extension()
                    .map(|value| value.to_string_lossy().into())
                    .unwrap_or_default(),
                size,
                is_dir,
                file_id: if id == 8 { 7 } else { id as u64 + 1 },
                volume_id: "fixture".into(),
                ..ScannedFile::default()
            }
        })
        .collect();
    store.batch(&files, 1).unwrap();
    store
        .put_content(
            "/fixture/alpha/report.pdf",
            "indexed text",
            &json!({"author":"Document Author", "pages":3}),
        )
        .unwrap();
    SearchSnapshot::new(store.entries().unwrap(), 1)
}
fn coverage() -> Value {
    json!({"complete":true,"roots":["/fixture"],"uncovered":[]})
}
fn evaluate(snapshot: &SearchSnapshot, text: &str, coverage: &Value) -> Vec<String> {
    let cancelled = AtomicBool::new(false);
    let mut query = crate::query::parse(text, &HashMap::new()).unwrap();
    let tree = snapshot.directory_hierarchy(&cancelled).unwrap();
    resolve(
        &mut query,
        snapshot,
        tree,
        coverage,
        &cancelled,
        &mut |_| Ok(None),
    )
    .unwrap();
    let evaluator = query.evaluator();
    let mut result: Vec<_> = snapshot
        .visible_entries()
        .filter(|file| {
            let matched = evaluator.matches_available(file, None).unwrap();
            assert_eq!(matched, query.matches_available(file, None).unwrap());
            matched
        })
        .map(|file| file.path.clone())
        .collect();
    result.sort();
    result
}
#[test]
fn immediate_recursive_nested_and_unicode_relationships_are_distinct() {
    let snapshot = fixture();
    assert_eq!(
        evaluate(&snapshot, "child:report", &coverage()),
        ["/fixture/alpha"]
    );
    assert_eq!(
        evaluate(&snapshot, "descendant:<ext:pdf>", &coverage()),
        ["/fixture", "/fixture/alpha"]
    );
    assert_eq!(
        evaluate(&snapshot, "childfolder:<child:<ext:txt>>", &coverage()),
        ["/fixture", "/fixture/alpha"]
    );
    assert_eq!(
        evaluate(&snapshot, "child-file:中文", &coverage()),
        ["/fixture/alpha/nested"]
    );
    assert_eq!(
        evaluate(&snapshot, "case:child:Report", &coverage()),
        Vec::<String>::new()
    );
    assert_eq!(
        evaluate(&snapshot, "child:<author:document pages:3>", &coverage()),
        ["/fixture/alpha"]
    );
}
#[test]
fn directory_aggregates_count_entries_not_reclaimable_extents() {
    let snapshot = fixture();
    assert_eq!(
        evaluate(&snapshot, "foldersize:=40", &coverage()),
        ["/fixture/alpha"]
    );
    assert_eq!(
        evaluate(&snapshot, "foldersize:=47", &coverage()),
        ["/fixture"]
    );
    assert_eq!(
        evaluate(&snapshot, "childcount:=3", &coverage()),
        ["/fixture/alpha"]
    );
    assert_eq!(
        evaluate(&snapshot, "descendantfilecount:=3", &coverage()),
        ["/fixture/alpha"]
    );
    assert_eq!(
        evaluate(&snapshot, "empty:", &coverage()),
        ["/fixture/denied", "/fixture/empty"]
    );
    let info = snapshot
        .directory_hierarchy(&AtomicBool::new(false))
        .unwrap()
        .info(
            &snapshot,
            &coverage(),
            &["/fixture/alpha".into()],
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(info["rows"][0]["recursive_size"], 40);
    assert_eq!(info["rows"][0]["complete"], true);
}
#[test]
fn denied_scope_is_unknown_not_empty_and_prefix_neighbors_stay_independent() {
    let snapshot = fixture();
    let coverage = json!({"complete":true,"roots":["/fixture"],"uncovered":["/fixture/alpha/nested", "/fixture/denied"]});
    assert_eq!(evaluate(&snapshot, "empty:", &coverage), ["/fixture/empty"]);
    assert_eq!(
        evaluate(&snapshot, "folder: !descendant:missing", &coverage),
        ["/fixture/alphabet", "/fixture/empty"]
    );
    assert_eq!(
        evaluate(&snapshot, "foldersize:unknown", &coverage),
        [
            "/fixture",
            "/fixture/alpha",
            "/fixture/alpha/nested",
            "/fixture/denied"
        ]
    );
    assert_eq!(
        evaluate(&snapshot, "descendant:report", &coverage),
        ["/fixture", "/fixture/alpha"]
    );
    let info = snapshot
        .directory_hierarchy(&AtomicBool::new(false))
        .unwrap()
        .info(
            &snapshot,
            &coverage,
            &["/fixture/alpha".into()],
            &AtomicBool::new(false),
        )
        .unwrap();
    assert!(info["rows"][0]["recursive_size"].is_null());
    assert_eq!(info["rows"][0]["indexed_logical_size"], 40);
}
#[test]
fn cached_topology_is_shared_only_within_its_immutable_generation() {
    let snapshot = fixture();
    let cancelled = AtomicBool::new(false);
    let first = snapshot.directory_hierarchy(&cancelled).unwrap();
    assert!(std::ptr::eq(
        first,
        snapshot.directory_hierarchy(&cancelled).unwrap()
    ));
    let mut changed = snapshot
        .visible_entries()
        .find(|file| file.name == "中文.txt")
        .unwrap()
        .clone();
    changed.size = 30;
    let updated =
        SearchSnapshot::from_changes(vec![(changed.id, Some(changed))], 2, &snapshot).unwrap();
    assert_eq!(
        evaluate(&updated, "foldersize:=50", &coverage()),
        ["/fixture/alpha"]
    );
    assert_eq!(
        evaluate(&snapshot, "foldersize:=40", &coverage()),
        ["/fixture/alpha"]
    );
    assert!(Arc::ptr_eq(&snapshot.entries[0], &updated.entries[0]));
}
#[test]
fn cancellation_and_unknown_content_never_become_successful_empty_results() {
    let snapshot = fixture();
    assert!(
        snapshot
            .directory_hierarchy(&AtomicBool::new(true))
            .is_err()
    );
    assert!(snapshot.hierarchy.get().is_none());
    assert_eq!(
        evaluate(
            &snapshot,
            "folder: !descendant:<content:secret>",
            &coverage()
        ),
        ["/fixture/denied", "/fixture/empty"]
    );
    let mut query = crate::query::parse("child:report", &HashMap::new()).unwrap();
    let tree = snapshot
        .directory_hierarchy(&AtomicBool::new(false))
        .unwrap();
    assert!(
        resolve(
            &mut query,
            &snapshot,
            tree,
            &coverage(),
            &AtomicBool::new(true),
            &mut |_| panic!("cancelled query read content")
        )
        .is_err()
    );
}
#[test]
fn overflowing_logical_size_remains_unknown_without_wrapping() {
    let snapshot = fixture();
    let mut files: Vec<_> = snapshot.visible_entries().cloned().collect();
    for file in &mut files {
        if !file.is_dir {
            file.size = u64::MAX;
        }
    }
    let overflowed = SearchSnapshot::new(files, 2);
    assert!(
        evaluate(&overflowed, "foldersize:unknown", &coverage())
            .contains(&"/fixture/alpha".to_string())
    );
    assert!(
        !evaluate(&overflowed, "foldersize:=0", &coverage())
            .contains(&"/fixture/alpha".to_string())
    );
}
