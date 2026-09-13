use super::*;
use crate::index_store::{IndexedFile, SearchSnapshot};
use std::os::unix::fs::MetadataExt;
use std::{path::Path, sync::atomic::AtomicBool};
fn indexed(path: &Path, id: i64) -> IndexedFile {
    let canonical = path.canonicalize().unwrap();
    let path = canonical.as_path();
    let meta = std::fs::symlink_metadata(path).unwrap();
    let mut entry:IndexedFile=serde_json::from_value(json!({"id":id,"path":path.to_string_lossy(),"name":path.file_name().unwrap().to_string_lossy(),"extension":path.extension().unwrap_or_default().to_string_lossy(),"size":meta.len(),"modified":meta.mtime(),"changed":meta.ctime(),"created":meta.mtime(),"is_dir":meta.is_dir(),"is_symlink":meta.file_type().is_symlink(),"file_id":meta.ino(),"parent_id":0,"volume_id":"test-volume","flags":0})).unwrap();
    entry.prepare();
    entry
}
fn row(path: &Path) -> Row {
    let path = path.canonicalize().unwrap().to_string_lossy().into_owned();
    Row {
        name: Path::new(&path)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        identity: fresh_identity(&path).unwrap(),
        path,
    }
}
fn results(snapshot: &SearchSnapshot) -> Value {
    find(snapshot, "content", &AtomicBool::new(false)).unwrap()
}
#[test]
fn hashes_distinct_objects_once_and_reports_aliases_separately() {
    let temp = tempfile::tempdir().unwrap();
    let a = temp.path().join("a.txt");
    let alias = temp.path().join("alias.txt");
    let b = temp.path().join("b.txt");
    std::fs::write(&a, b"identical").unwrap();
    std::fs::hard_link(&a, &alias).unwrap();
    std::fs::write(&b, b"identical").unwrap();
    let snapshot = SearchSnapshot::new(vec![indexed(&a, 1), indexed(&alias, 2), indexed(&b, 3)], 1);
    let result = results(&snapshot);
    assert_eq!(result["hash_read_count"], 2);
    assert_eq!(result["groups"].as_array().unwrap().len(), 1);
    assert_eq!(result["groups"][0]["distinct_files"], 2);
    assert_eq!(result["groups"][0]["rows"].as_array().unwrap().len(), 3);
    assert_eq!(result["hardlinks"].as_array().unwrap().len(), 1);
    assert_eq!(result["partial"], false);
}
#[test]
fn hardlinks_alone_are_not_independent_duplicate_files() {
    let temp = tempfile::tempdir().unwrap();
    let a = temp.path().join("a");
    let alias = temp.path().join("alias");
    std::fs::write(&a, b"data").unwrap();
    std::fs::hard_link(&a, &alias).unwrap();
    let snapshot = SearchSnapshot::new(vec![indexed(&a, 1), indexed(&alias, 2)], 1);
    let result = results(&snapshot);
    assert_eq!(result["hash_read_count"], 0);
    assert_eq!(result["groups"], json!([]));
    assert_eq!(result["hardlinks"].as_array().unwrap().len(), 1);
}
#[test]
fn fresh_size_buckets_find_duplicates_after_indexed_file_sizes_change() {
    let temp = tempfile::tempdir().unwrap();
    let a = temp.path().join("a");
    let b = temp.path().join("b");
    std::fs::write(&a, b"old").unwrap();
    std::fs::write(&b, b"data").unwrap();
    let snapshot = SearchSnapshot::new(vec![indexed(&a, 1), indexed(&b, 2)], 1);
    std::fs::write(&a, b"data").unwrap();
    let result = results(&snapshot);
    assert_eq!(result["hash_read_count"], 2);
    assert_eq!(result["groups"].as_array().unwrap().len(), 1);
    assert_eq!(result["groups"][0]["rows"][0]["size"], 4);
    assert_eq!(result["partial"], false);
}
#[test]
fn disappeared_and_replaced_nonregular_paths_make_results_explicitly_partial() {
    let temp = tempfile::tempdir().unwrap();
    let a = temp.path().join("a");
    let b = temp.path().join("b");
    std::fs::write(&a, b"data").unwrap();
    std::fs::write(&b, b"data").unwrap();
    let snapshot = SearchSnapshot::new(vec![indexed(&a, 1), indexed(&b, 2)], 1);
    std::fs::remove_file(&a).unwrap();
    std::fs::remove_file(&b).unwrap();
    std::os::unix::fs::symlink("missing", &b).unwrap();
    let result = results(&snapshot);
    assert_eq!(result["partial"], true);
    assert_eq!(result["errors"].as_array().unwrap().len(), 2);
    assert_eq!(result["hash_read_count"], 0);
    assert_eq!(result["groups"], json!([]));
}
#[test]
fn stable_old_descriptor_cannot_assign_its_hash_to_a_replaced_parent_path() {
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join("parent");
    std::fs::create_dir(&parent).unwrap();
    let path = parent.join("file");
    std::fs::write(&path, b"old data").unwrap();
    let expected = row(&path);
    let mut file = open_verified(&expected).unwrap();
    std::fs::rename(&parent, temp.path().join("moved")).unwrap();
    std::fs::create_dir(&parent).unwrap();
    std::fs::write(&path, b"new data").unwrap();
    // The old descriptor retains the exact same file version; only final-path
    // validation can establish that it no longer belongs to the displayed path.
    validate_descriptor(&file, &expected.identity).unwrap();
    assert!(matches!(
        hash_and_validate(
            &mut file,
            &expected,
            &AtomicBool::new(false),
            &mut [0; 4096]
        ),
        Err(Failure::File(_))
    ));
}
#[test]
fn same_length_rewrite_with_restored_mtime_is_rejected_using_ctime() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("file");
    std::fs::write(&path, b"old data").unwrap();
    let expected = row(&path);
    let mut file = open_verified(&expected).unwrap();
    let modified = file.metadata().unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    std::fs::write(&path, b"new data").unwrap();
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
    let current = fresh_identity(path.to_str().unwrap()).unwrap();
    assert_eq!(expected.identity.size, current.size);
    assert_eq!(
        (expected.identity.modified, expected.identity.modified_nsec),
        (current.modified, current.modified_nsec)
    );
    assert_ne!(
        (expected.identity.changed, expected.identity.changed_nsec),
        (current.changed, current.changed_nsec)
    );
    assert!(matches!(
        hash_and_validate(
            &mut file,
            &expected,
            &AtomicBool::new(false),
            &mut [0; 4096]
        ),
        Err(Failure::File(_))
    ));
}
#[test]
fn stale_open_identity_and_retargeted_aliases_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("file");
    let replacement = temp.path().join("replacement");
    std::fs::write(&path, b"old data").unwrap();
    let expected = row(&path);
    std::fs::write(&replacement, b"new data").unwrap();
    std::fs::rename(&replacement, &path).unwrap();
    assert!(matches!(open_verified(&expected), Err(Failure::File(_))));
    assert!(matches!(validate_path(&expected), Err(Failure::File(_))));
}
#[test]
fn cancellation_is_an_operation_error_and_never_a_partial_hash_result() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("file");
    std::fs::write(&path, b"data").unwrap();
    let expected = row(&path);
    let mut file = open_verified(&expected).unwrap();
    let cancelled = AtomicBool::new(true);
    assert!(matches!(
        hash_and_validate(&mut file, &expected, &cancelled, &mut [0; 4096]),
        Err(Failure::Cancelled)
    ));
    let snapshot = SearchSnapshot::new(vec![indexed(&path, 1)], 1);
    assert_eq!(
        find(&snapshot, "content", &cancelled).unwrap_err(),
        "Duplicate search cancelled"
    );
    let mut errors = vec![];
    assert!(record_error(&mut errors, &expected.path, "hash", Failure::Cancelled).is_err());
    assert!(errors.is_empty());
}
#[test]
fn size_and_name_modes_use_fresh_metadata_without_reading_content() {
    let temp = tempfile::tempdir().unwrap();
    let a = temp.path().join("file");
    let sub = temp.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let b = sub.join("file");
    std::fs::write(&a, b"one").unwrap();
    std::fs::write(&b, b"different").unwrap();
    let snapshot = SearchSnapshot::new(vec![indexed(&a, 1), indexed(&b, 2)], 1);
    let named = find(&snapshot, "name", &AtomicBool::new(false)).unwrap();
    assert_eq!(named["groups"].as_array().unwrap().len(), 1);
    assert_eq!(named["hash_read_count"], 0);
    std::fs::write(&a, b"new equal").unwrap();
    let sized = find(&snapshot, "size", &AtomicBool::new(false)).unwrap();
    assert_eq!(sized["groups"].as_array().unwrap().len(), 1);
    assert_eq!(sized["hash_read_count"], 0);
}

#[test]
#[cfg(target_os = "macos")]
fn an_intermediate_symlink_cannot_redirect_content_reads() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let directory = root.join("actual");
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("file");
    std::fs::write(&path, b"data").unwrap();
    let alias = root.join("alias");
    std::os::unix::fs::symlink(&directory, &alias).unwrap();
    let redirected = alias.join("file");
    let mut expected = row(&path);
    expected.path = redirected.to_string_lossy().into_owned();
    assert!(matches!(open_verified(&expected), Err(Failure::File(_))));
}
