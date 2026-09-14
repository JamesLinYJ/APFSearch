//! Independent real-file checks for the lifetime and validity of pass-local
//! hard-link verification. Fixtures contain only four tiny aliases.
use super::*;
use crate::index_store::{LinkVerification, VerifiedFileObjects};

struct Fixture {
    _temporary: tempfile::TempDir,
    store: IndexStore,
    paths: Vec<PathBuf>,
}
fn fixture() -> Fixture {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let paths: Vec<_> = (0..4)
        .map(|index| root.join(format!("alias-{index}.txt")))
        .collect();
    std::fs::write(&paths[0], b"old").unwrap();
    for path in &paths[1..] {
        std::fs::hard_link(&paths[0], path).unwrap();
    }
    let mut store = IndexStore::open(&root.join("index/index.sqlite")).unwrap();
    let entries: Vec<_> = paths.iter().map(|path| current(path)).collect();
    store.batch(&entries, 1).unwrap();
    Fixture {
        _temporary: temporary,
        store,
        paths,
    }
}
fn current(path: &Path) -> scanner::ScannedFile {
    scanner::stat_entry(path.to_str().unwrap()).unwrap()
}
fn verify(paths: Vec<String>) -> Result<LinkVerification, String> {
    paths
        .iter()
        .map(|path| scanner::stat_entry(path))
        .collect::<Result<Vec<_>, _>>()
        .map(Into::into)
}
fn sizes(store: &IndexStore) -> Vec<u64> {
    store
        .entries()
        .unwrap()
        .into_iter()
        .map(|entry| entry.size)
        .collect()
}
fn verify_one(
    fixture: &mut Fixture,
    index: usize,
    tracker: &mut VerifiedFileObjects,
    calls: &mut usize,
    paths_read: &mut usize,
) {
    let entry = current(&fixture.paths[index]);
    fixture
        .store
        .observe_batch(
            &[entry],
            None,
            Some(&mut |paths| {
                *calls += 1;
                *paths_read += paths.len();
                verify(paths)
            }),
            Some(tracker),
        )
        .unwrap();
}

#[test]
fn unchanged_aliases_across_batches_are_verified_once_per_pass() {
    let mut fixture = fixture();
    let mut tracker = VerifiedFileObjects::default();
    let mut calls = 0;
    let mut paths_read = 0;
    for index in 0..fixture.paths.len() {
        verify_one(
            &mut fixture,
            index,
            &mut tracker,
            &mut calls,
            &mut paths_read,
        );
    }
    assert_eq!(
        calls, 1,
        "one verifier invocation, not one for every alias batch"
    );
    assert_eq!(paths_read, fixture.paths.len(), "L paths, not L squared");
    assert!(tracker.reused_objects() >= 3);
    assert_eq!(sizes(&fixture.store), vec![3; 4]);
    let mut next_pass = VerifiedFileObjects::default();
    verify_one(&mut fixture, 0, &mut next_pass, &mut calls, &mut paths_read);
    assert_eq!(
        calls, 2,
        "completed proofs must not escape their reconciliation pass"
    );
}

#[test]
fn a_content_change_forces_all_aliases_even_after_a_successful_pass_cache_entry() {
    let mut fixture = fixture();
    let mut tracker = VerifiedFileObjects::default();
    let (mut calls, mut paths_read) = (0, 0);
    verify_one(&mut fixture, 0, &mut tracker, &mut calls, &mut paths_read);
    let replacement = b"changed content";
    std::fs::write(&fixture.paths[1], replacement).unwrap();
    verify_one(&mut fixture, 1, &mut tracker, &mut calls, &mut paths_read);
    assert_eq!(calls, 2);
    assert_eq!(paths_read, 8);
    assert_eq!(sizes(&fixture.store), vec![replacement.len() as u64; 4]);
    verify_one(&mut fixture, 2, &mut tracker, &mut calls, &mut paths_read);
    assert_eq!(
        calls, 2,
        "a fully committed fresh verification becomes reusable again"
    );
}

#[test]
fn replacement_invalidates_the_previous_object_and_checks_its_remaining_aliases() {
    let mut fixture = fixture();
    let mut tracker = VerifiedFileObjects::default();
    let (mut calls, mut paths_read) = (0, 0);
    verify_one(&mut fixture, 0, &mut tracker, &mut calls, &mut paths_read);
    let old_identity = current(&fixture.paths[0]).file_id;
    std::fs::remove_file(&fixture.paths[0]).unwrap();
    std::fs::write(&fixture.paths[0], b"independent replacement").unwrap();
    let fresh = current(&fixture.paths[0]);
    assert_ne!(fresh.file_id, old_identity);
    let mut checked = Vec::new();
    fixture
        .store
        .observe_batch(
            &[fresh],
            None,
            Some(&mut |paths| {
                checked.extend(paths.iter().cloned());
                verify(paths)
            }),
            Some(&mut tracker),
        )
        .unwrap();
    for path in &fixture.paths {
        assert!(
            checked.contains(&path.to_string_lossy().into_owned()),
            "old and new identities both require verification"
        );
    }
    for entry in fixture.store.entries().unwrap() {
        let fresh = current(Path::new(&entry.path));
        assert_eq!(entry.file_id, fresh.file_id);
        assert_eq!(entry.changed_ns, fresh.changed_ns);
        assert_eq!(entry.size, fresh.size);
    }
}

#[test]
fn verifier_failure_or_cancellation_revokes_prior_success_even_when_sql_rolls_back() {
    for reason in [
        "injected verifier failure",
        "injected verifier cancellation",
    ] {
        let mut fixture = fixture();
        let mut tracker = VerifiedFileObjects::default();
        let (mut calls, mut paths_read) = (0, 0);
        verify_one(&mut fixture, 0, &mut tracker, &mut calls, &mut paths_read);
        let queued_old = current(&fixture.paths[0]);
        let replacement = b"new content following a successful check";
        std::fs::write(&fixture.paths[0], replacement).unwrap();
        let fresh = current(&fixture.paths[0]);
        let result = fixture.store.observe_batch(
            &[fresh],
            None,
            Some(&mut |_| Err(reason.into())),
            Some(&mut tracker),
        );
        assert!(result.is_err());
        assert_eq!(
            sizes(&fixture.store),
            vec![3; 4],
            "primary writes roll back with the verifier"
        );
        let mut retried = 0;
        // A queued bulk record can still equal the rolled-back SQL row. It must
        // not reuse the successful check invalidated by the failed new version.
        fixture
            .store
            .observe_batch(
                &[queued_old],
                None,
                Some(&mut |paths| {
                    retried += 1;
                    verify(paths)
                }),
                Some(&mut tracker),
            )
            .unwrap();
        assert_eq!(
            retried, 1,
            "failure must revoke any earlier success for this object"
        );
        assert_eq!(sizes(&fixture.store), vec![replacement.len() as u64; 4]);
    }
}

#[test]
fn successful_verification_is_not_cached_before_sqlite_commit_succeeds() {
    let mut fixture = fixture();
    let mut tracker = VerifiedFileObjects::default();
    let queued_old = current(&fixture.paths[0]);
    let replacement = b"changed with a deferred foreign key failure";
    std::fs::write(&fixture.paths[0], replacement).unwrap();
    fixture.store.connection.execute_batch(
        "CREATE TABLE commit_parent(id INTEGER PRIMARY KEY);
         CREATE TABLE commit_child(parent_id INTEGER REFERENCES commit_parent(id) DEFERRABLE INITIALLY DEFERRED);
         CREATE TEMP TRIGGER fail_commit AFTER UPDATE ON files BEGIN INSERT INTO commit_child VALUES(1); END;"
    ).unwrap();
    let fresh = current(&fixture.paths[0]);
    let mut calls = 0;
    let result = fixture.store.observe_batch(
        &[fresh],
        None,
        Some(&mut |paths| {
            calls += 1;
            verify(paths)
        }),
        Some(&mut tracker),
    );
    assert!(
        result.is_err(),
        "the deferred constraint must fail at commit"
    );
    assert_eq!(calls, 1, "the verifier succeeded before the commit failure");
    assert_eq!(sizes(&fixture.store), vec![3; 4]);
    assert_eq!(
        fixture
            .store
            .connection
            .query_row("SELECT count(*) FROM commit_child", [], |row| row
                .get::<_, u64>(0))
            .unwrap(),
        0
    );
    fixture
        .store
        .connection
        .execute_batch("DROP TRIGGER fail_commit")
        .unwrap();
    fixture
        .store
        .observe_batch(
            &[queued_old],
            None,
            Some(&mut |paths| {
                calls += 1;
                verify(paths)
            }),
            Some(&mut tracker),
        )
        .unwrap();
    assert_eq!(
        calls, 2,
        "rolled-back success cannot skip the next alias check"
    );
    assert_eq!(sizes(&fixture.store), vec![replacement.len() as u64; 4]);
}

#[test]
fn unavailable_aliases_prevent_reuse_after_an_earlier_success() {
    let mut fixture = fixture();
    let mut tracker = VerifiedFileObjects::default();
    let (mut calls, mut paths_read) = (0, 0);
    verify_one(&mut fixture, 0, &mut tracker, &mut calls, &mut paths_read);
    std::fs::write(&fixture.paths[0], b"new metadata").unwrap();
    let unavailable = fixture.paths[3].to_string_lossy().into_owned();
    let fresh = current(&fixture.paths[0]);
    fixture
        .store
        .observe_batch(
            &[fresh],
            None,
            Some(&mut |paths| {
                let mut result = LinkVerification::default();
                for path in paths {
                    if path == unavailable {
                        result.unavailable.push(path);
                    } else {
                        result.entries.push(scanner::stat_entry(&path)?);
                    }
                }
                Ok(result)
            }),
            Some(&mut tracker),
        )
        .unwrap();
    assert_eq!(fixture.store.entries().unwrap().len(), 3);
    let mut retried = 0;
    let entry = current(&fixture.paths[0]);
    fixture
        .store
        .observe_batch(
            &[entry],
            None,
            Some(&mut |paths| {
                retried += 1;
                verify(paths)
            }),
            Some(&mut tracker),
        )
        .unwrap();
    assert_eq!(
        retried, 1,
        "an incomplete verification cannot reuse prior success"
    );
    assert!(
        fixture
            .store
            .get("uncovered", json!([]))
            .as_array()
            .unwrap()
            .contains(&json!(unavailable))
    );
}
