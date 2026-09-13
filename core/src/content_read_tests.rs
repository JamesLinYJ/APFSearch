use super::*;
use std::{fs, os::unix::fs::MetadataExt, path::Path};

fn scan_fixture() -> (tempfile::TempDir, Arc<SearchEngine>, String) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("files");
    fs::create_dir(&root).unwrap();
    let path = root.join("body.txt");
    fs::write(&path, "AAAA").unwrap();
    let path = path.canonicalize().unwrap().to_string_lossy().into_owned();
    let engine = SearchEngine::open(&temp.path().join("db/index.sqlite")).unwrap();
    assert_eq!(
        engine.call(json!({"op":"scan","roots":[root],"watch":false,"wait":true}))["success"],
        true
    );
    (temp, engine, path)
}
#[test]
fn uncached_old_lease_rejects_same_size_rewrite_with_restored_mtime() {
    let (_temp, engine, path) = scan_fixture();
    let lease = engine.call(json!({"op":"retain_snapshot"}));
    let before = engine.call(
        json!({"op":"query","text":"file: content:AAAA","snapshot_lease":lease["snapshot_lease"]}),
    );
    assert_eq!(before["total"], 1);
    let metadata = fs::metadata(&path).unwrap();
    fs::write(&path, "BBBB").unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(metadata.modified().unwrap()))
        .unwrap();
    let after = fs::metadata(&path).unwrap();
    assert_eq!(after.len(), metadata.len());
    assert_eq!(after.modified().unwrap(), metadata.modified().unwrap());
    assert_ne!(
        (after.ctime(), after.ctime_nsec()),
        (metadata.ctime(), metadata.ctime_nsec())
    );
    for query in [
        "file: content:BBBB",
        "file: !content:AAAA",
        "file: !content:missing",
    ] {
        let result = engine
            .call(json!({"op":"query","text":query,"snapshot_lease":lease["snapshot_lease"]}));
        assert_eq!(result["success"], true);
        assert_eq!(
            result["total"], 0,
            "An unreadable/stale body is unknown, not known absence: {result}"
        );
        assert!(!result["warnings"].as_array().unwrap().is_empty());
    }
}
#[test]
fn uncached_old_lease_does_not_read_a_replaced_path() {
    let (_temp, engine, path) = scan_fixture();
    let lease = engine.call(json!({"op":"retain_snapshot"}));
    fs::rename(&path, Path::new(&path).with_file_name("old-body.txt")).unwrap();
    fs::write(&path, "BBBB").unwrap();
    let result = engine.call(
        json!({"op":"query","text":"file: content:BBBB","snapshot_lease":lease["snapshot_lease"]}),
    );
    assert_eq!(result["total"], 0);
    assert!(!result["warnings"].as_array().unwrap().is_empty());
}
#[test]
fn final_path_check_rejects_replaced_parent_even_when_original_descriptor_is_stable() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    fs::create_dir(root.join("parent")).unwrap();
    let path = root.join("parent/body.txt");
    fs::write(&path, "AAAA").unwrap();
    let expected = file_identity::FileIdentity::from_metadata(&fs::metadata(&path).unwrap());
    let mut file = open_regular(path.to_str().unwrap()).unwrap();
    fs::rename(root.join("parent"), root.join("original-parent")).unwrap();
    fs::create_dir(root.join("parent")).unwrap();
    fs::write(&path, "BBBB").unwrap();
    assert_eq!(
        file_identity::FileIdentity::from_metadata(&file.metadata().unwrap()),
        expected
    );
    let error = read_verified_text(&mut file, path.to_str().unwrap(), &expected).unwrap_err();
    assert!(error.contains("path was replaced"), "{error}");
}
#[test]
fn opened_descriptor_must_still_match_after_an_in_place_rewrite() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().canonicalize().unwrap().join("body.txt");
    fs::write(&path, "AAAA").unwrap();
    let metadata = fs::metadata(&path).unwrap();
    let expected = file_identity::FileIdentity::from_metadata(&metadata);
    let mut file = open_regular(path.to_str().unwrap()).unwrap();
    fs::write(&path, "BBBB").unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(metadata.modified().unwrap()))
        .unwrap();
    let error = read_verified_text(&mut file, path.to_str().unwrap(), &expected).unwrap_err();
    assert!(error.contains("Opened file identity"), "{error}");
}
