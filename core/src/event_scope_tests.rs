use super::*;
use std::os::unix::fs::PermissionsExt;

fn fixture() -> (tempfile::TempDir, Arc<SearchEngine>, String) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("files");
    std::fs::create_dir(&root).unwrap();
    let root = root.canonicalize().unwrap().to_string_lossy().into_owned();
    let engine = SearchEngine::open(&temp.path().join("index/index.sqlite")).unwrap();
    (temp, engine, root)
}
fn scan(engine: &Arc<SearchEngine>, root: &str) {
    assert_eq!(
        engine.call(json!({"op":"scan","roots":[root],"watch":false,"wait":true}))["success"],
        true
    );
}
fn inode_plan(engine: &SearchEngine, root: &str, path: &str) -> scanner::Reconciliation {
    let mut plan = scanner::reconciliation_plan(
        &[root.into()],
        &[scanner::ChangeEvent::from_flags(path, 42, 0x20400)],
    );
    assert!(
        plan.recursive.is_empty(),
        "inode metadata requires an access decision first"
    );
    engine.resolve_directory_checks(&mut plan).unwrap();
    plan
}
fn apply(engine: &SearchEngine, root: &str, plan: &scanner::Reconciliation) {
    if !plan.recursive.is_empty() {
        assert!(
            engine
                .reconcile(&plan.recursive, &[root.into()], 42, true, false)
                .unwrap()
        );
    }
    if !plan.metadata.is_empty() {
        assert!(
            engine
                .reconcile(&plan.metadata, &[root.into()], 42, false, false)
                .unwrap()
        );
    }
}
#[test]
fn ordinary_directory_inode_event_does_not_enumerate_its_existing_subtree() {
    let (_temp, engine, root) = fixture();
    for n in 0..128 {
        std::fs::write(format!("{root}/file-{n}.txt"), "tiny").unwrap();
    }
    scan(&engine, &root);
    let before = engine.call(json!({"op":"status"}))["reconciled_entries"]
        .as_u64()
        .unwrap();
    std::fs::write(format!("{root}/new.txt"), "new").unwrap();
    let plan = inode_plan(&engine, &root, &root);
    assert!(plan.recursive.is_empty());
    assert_eq!(plan.metadata.as_slice(), std::slice::from_ref(&root));
    apply(&engine, &root, &plan);
    let after = engine.call(json!({"op":"status"}))["reconciled_entries"]
        .as_u64()
        .unwrap();
    assert_eq!(
        after - before,
        1,
        "must stat exactly the directory, not its 129 children"
    );
    let plan = scanner::reconciliation_plan(
        std::slice::from_ref(&root),
        &[scanner::ChangeEvent::from_flags(
            &format!("{root}/new.txt"),
            43,
            0x10100,
        )],
    );
    apply(&engine, &root, &plan);
    assert_eq!(
        engine.call(json!({"op":"query","text":"new.txt"}))["total"],
        1
    );
}

#[test]
fn missing_child_below_a_replaced_symlink_stays_uncovered() {
    for recursive in [false, true] {
        let (temporary, engine, root) = fixture();
        let selected = Path::new(&root).join("selected");
        let outside = temporary.path().join("outside");
        std::fs::create_dir(&selected).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let child = selected.join("child.txt").to_string_lossy().into_owned();
        std::fs::write(&child, "selected content").unwrap();
        scan(&engine, &root);
        std::fs::rename(&selected, temporary.path().join("held")).unwrap();
        std::os::unix::fs::symlink(&outside, &selected).unwrap();
        assert!(
            engine
                .reconcile(
                    std::slice::from_ref(&child),
                    std::slice::from_ref(&root),
                    42,
                    recursive,
                    false
                )
                .unwrap()
        );
        let status = engine.call(json!({"op":"status"}));
        assert_eq!(status["uncovered"], json!([child]), "{status}");
        assert_eq!(
            engine.call(json!({"op":"query","text":"child.txt"}))["total"],
            0
        );
        assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
    }
}
#[test]
fn directory_read_without_search_permission_loses_coverage_and_recovers() {
    let (_temp, engine, root) = fixture();
    std::fs::write(format!("{root}/secret.txt"), "secret").unwrap();
    scan(&engine, &root);
    for mode in [0o000, 0o400] {
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(mode)).unwrap();
        let plan = inode_plan(&engine, &root, &root);
        assert_eq!(plan.recursive.as_slice(), std::slice::from_ref(&root));
        apply(&engine, &root, &plan);
        let hidden = engine.call(json!({"op":"query","text":"secret.txt"}));
        let status = engine.call(json!({"op":"status"}));
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(hidden["total"], 0, "mode={mode:o}");
        assert_eq!(status["uncovered"], json!([root]));
        let plan = inode_plan(&engine, &root, &root);
        assert_eq!(plan.recursive.as_slice(), std::slice::from_ref(&root));
        apply(&engine, &root, &plan);
        assert_eq!(
            engine.call(json!({"op":"query","text":"secret.txt"}))["total"],
            1
        );
    }
}
#[test]
fn parent_inode_event_retries_only_a_denied_scope_that_regained_access() {
    let (_temp, engine, root) = fixture();
    let denied = format!("{root}/denied");
    std::fs::create_dir(&denied).unwrap();
    std::fs::write(format!("{denied}/secret.txt"), "secret").unwrap();
    scan(&engine, &root);
    std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o000)).unwrap();
    apply(&engine, &root, &inode_plan(&engine, &root, &denied));
    let inaccessible = inode_plan(&engine, &root, &root);
    std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        inaccessible.recursive.is_empty(),
        "an unchanged denial must not rescan the parent"
    );
    let restored = inode_plan(&engine, &root, &root);
    assert_eq!(restored.recursive, [denied]);
    apply(&engine, &root, &restored);
    assert_eq!(
        engine.call(json!({"op":"query","text":"secret.txt"}))["total"],
        1
    );
}
#[test]
fn unchanged_batch_skips_alias_verification_and_alias_failure_rolls_back_main_rows() {
    let (_temp, engine, root) = fixture();
    let first = format!("{root}/first.txt");
    let second = format!("{root}/second.txt");
    std::fs::write(&first, "old").unwrap();
    std::fs::hard_link(&first, &second).unwrap();
    let single = format!("{root}/single.txt");
    std::fs::write(&single, "single").unwrap();
    scan(&engine, &root);
    let mut store = engine.index_store.lock().unwrap();
    let mut calls = 0;
    let unchanged = store
        .observe_batch(
            &[scanner::stat_entry(&single).unwrap()],
            None,
            Some(&mut |_| {
                calls += 1;
                Ok(Vec::new().into())
            }),
            None,
        )
        .unwrap();
    assert_eq!(unchanged, 0);
    assert_eq!(calls, 0);
    std::fs::write(&first, "new longer data").unwrap();
    let fresh = scanner::stat_entry(&first).unwrap();
    let failed = store.observe_batch(
        std::slice::from_ref(&fresh),
        None,
        Some(&mut |paths| {
            assert!(paths.contains(&second));
            Err("injected failure before alias metadata commit".into())
        }),
        None,
    );
    assert!(failed.is_err());
    let size: i64 = store
        .connection
        .query_row("SELECT size FROM files WHERE path=?1", [&first], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        size, 3,
        "primary update must roll back with alias verification"
    );
    let changed = store
        .observe_batch(
            &[fresh],
            None,
            Some(&mut |paths| {
                paths
                    .iter()
                    .map(|path| scanner::stat_entry(path))
                    .collect::<Result<Vec<_>, _>>()
                    .map(Into::into)
            }),
            None,
        )
        .unwrap();
    assert_eq!(changed, 2);
    let mismatches: i64 = store
        .connection
        .query_row(
            "SELECT count(*) FROM files WHERE path IN(?1,?2) AND size!=15",
            [&first, &second],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(mismatches, 0);
}
#[test]
fn replacing_one_path_verifies_both_old_and_new_inode_aliases() {
    let (_temp, engine, root) = fixture();
    let paths: Vec<_> = ["a", "b", "c", "d"]
        .map(|name| format!("{root}/{name}.txt"))
        .into();
    std::fs::write(&paths[0], "old").unwrap();
    std::fs::hard_link(&paths[0], &paths[1]).unwrap();
    std::fs::write(&paths[2], "replacement").unwrap();
    std::fs::hard_link(&paths[2], &paths[3]).unwrap();
    scan(&engine, &root);
    std::fs::remove_file(&paths[0]).unwrap();
    std::fs::hard_link(&paths[2], &paths[0]).unwrap();
    let mut verified = Vec::new();
    engine
        .index_store
        .lock()
        .unwrap()
        .observe_batch(
            &[scanner::stat_entry(&paths[0]).unwrap()],
            None,
            Some(&mut |peers| {
                verified.extend(peers.clone());
                peers
                    .iter()
                    .map(|path| scanner::stat_entry(path))
                    .collect::<Result<Vec<_>, _>>()
                    .map(Into::into)
            }),
            None,
        )
        .unwrap();
    verified.sort();
    assert_eq!(verified, paths);
    let store = engine.index_store.lock().unwrap();
    for path in &paths {
        let fresh = scanner::stat_entry(path).unwrap();
        let (inode, changed_ns): (i64, i64) = store
            .connection
            .query_row(
                "SELECT file_id,changed_ns FROM files WHERE path=?1",
                [path],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (inode as u64, changed_ns),
            (fresh.file_id, fresh.changed_ns)
        );
    }
}
#[test]
fn inaccessible_alias_is_durably_hidden_before_the_following_finish_scope_transaction() {
    let (_temp, engine, root) = fixture();
    let first = format!("{root}/first.txt");
    let second = format!("{root}/second.txt");
    std::fs::write(&first, "old").unwrap();
    std::fs::hard_link(&first, &second).unwrap();
    scan(&engine, &root);
    std::fs::write(&first, "changed").unwrap();
    let mut store = engine.index_store.lock().unwrap();
    store
        .observe_batch(
            &[scanner::stat_entry(&first).unwrap()],
            None,
            Some(&mut |paths| {
                assert!(paths.contains(&second));
                Ok(index_store::LinkVerification {
                    unavailable: vec![second.clone()],
                    ..Default::default()
                })
            }),
            None,
        )
        .unwrap();
    // Deliberately do not call reconcile/finish_observed. A new connection sees
    // the durable state that a crash immediately after this commit would leave.
    let durable = rusqlite::Connection::open(engine.index_directory.join("index.sqlite")).unwrap();
    let accessible: bool = durable
        .query_row(
            "SELECT accessible FROM files WHERE path=?1",
            [&second],
            |row| row.get(0),
        )
        .unwrap();
    let uncovered: String = durable
        .query_row(
            "SELECT value FROM settings WHERE key='uncovered'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!accessible);
    assert_eq!(
        serde_json::from_str::<Value>(&uncovered).unwrap(),
        json!([second])
    );
    let unchanged = store
        .observe_batch(
            &[scanner::stat_entry(&first).unwrap()],
            None,
            Some(&mut |paths| {
                paths
                    .iter()
                    .map(|path| scanner::stat_entry(path))
                    .collect::<Result<Vec<_>, _>>()
                    .map(Into::into)
            }),
            None,
        )
        .unwrap();
    assert_eq!(unchanged, 0);
    drop(store);
    let plan = inode_plan(&engine, &root, &root);
    assert_eq!(plan.recursive, [second]);
    apply(&engine, &root, &plan);
    assert_eq!(engine.call(json!({"op":"status"}))["uncovered"], json!([]));
}
#[test]
fn a_removed_alias_commits_as_deletion_without_poisoning_coverage_or_stopping_reconciliation() {
    let (_temp, engine, root) = fixture();
    std::fs::create_dir(format!("{root}/one")).unwrap();
    std::fs::create_dir(format!("{root}/two")).unwrap();
    let first = format!("{root}/one/first.txt");
    let second = format!("{root}/two/second.txt");
    std::fs::write(&first, "old").unwrap();
    std::fs::hard_link(&first, &second).unwrap();
    scan(&engine, &root);
    std::fs::remove_file(&second).unwrap();
    assert!(
        engine
            .reconcile(
                std::slice::from_ref(&first),
                std::slice::from_ref(&root),
                44,
                true,
                false
            )
            .unwrap()
    );
    assert_eq!(
        engine.call(json!({"op":"query","text":"second.txt"}))["total"],
        0
    );
    assert_eq!(engine.call(json!({"op":"status"}))["uncovered"], json!([]));
}
#[test]
fn mixed_old_and_new_alias_metadata_in_one_batch_is_reverified_before_commit() {
    let (_temp, engine, root) = fixture();
    let first = format!("{root}/first.txt");
    let second = format!("{root}/second.txt");
    std::fs::write(&first, "old").unwrap();
    std::fs::hard_link(&first, &second).unwrap();
    scan(&engine, &root);
    let old_first = scanner::stat_entry(&first).unwrap();
    std::fs::write(&second, "changed contents").unwrap();
    let new_second = scanner::stat_entry(&second).unwrap();
    let mut verified = Vec::new();
    let mut store = engine.index_store.lock().unwrap();
    store
        .observe_batch(
            &[old_first, new_second],
            None,
            Some(&mut |paths| {
                verified.extend(paths.clone());
                paths
                    .iter()
                    .map(|path| scanner::stat_entry(path))
                    .collect::<Result<Vec<_>, _>>()
                    .map(Into::into)
            }),
            None,
        )
        .unwrap();
    assert!(verified.contains(&first));
    for path in [&first, &second] {
        let size: u64 = store
            .connection
            .query_row("SELECT size FROM files WHERE path=?1", [path], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(size, 16);
    }
}
#[test]
fn a_denied_alias_file_recovers_from_only_its_parent_inode_event() {
    let (_temp, engine, root) = fixture();
    let one = format!("{root}/one");
    let two = format!("{root}/two");
    std::fs::create_dir(&one).unwrap();
    std::fs::create_dir(&two).unwrap();
    let first = format!("{one}/first.txt");
    let second = format!("{two}/second.txt");
    std::fs::write(&first, "old").unwrap();
    std::fs::hard_link(&first, &second).unwrap();
    scan(&engine, &root);
    std::fs::set_permissions(&two, std::fs::Permissions::from_mode(0o000)).unwrap();
    std::fs::write(&first, "changed contents").unwrap();
    let applied = engine.reconcile(
        std::slice::from_ref(&first),
        std::slice::from_ref(&root),
        44,
        true,
        false,
    );
    let uncovered = engine.call(json!({"op":"status"}))["uncovered"].clone();
    let hidden = engine.call(json!({"op":"query","text":"second.txt"}))["total"].clone();
    std::fs::set_permissions(&two, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(applied.unwrap());
    assert_eq!(uncovered, json!([second]));
    assert_eq!(hidden, 0);
    let plan = inode_plan(&engine, &root, &two);
    assert_eq!(plan.recursive, [second]);
    apply(&engine, &root, &plan);
    assert_eq!(
        engine.call(json!({"op":"query","text":"second.txt"}))["total"],
        1
    );
    assert_eq!(engine.call(json!({"op":"status"}))["uncovered"], json!([]));
}
#[test]
fn unchanged_event_for_one_hardlink_repairs_an_older_alias_from_a_previous_batch() {
    let (_temp, engine, root) = fixture();
    let first = format!("{root}/first.txt");
    let second = format!("{root}/second.txt");
    std::fs::write(&first, "old").unwrap();
    std::fs::hard_link(&first, &second).unwrap();
    scan(&engine, &root);
    std::fs::write(&second, "changed contents").unwrap();
    let current_second = scanner::stat_entry(&second).unwrap();
    // Simulate separate initial enumeration batches: A was read before the
    // write, B after it, and the next file event mentions only B.
    engine
        .index_store
        .lock()
        .unwrap()
        .observe_batch(std::slice::from_ref(&current_second), None, None, None)
        .unwrap();
    assert!(
        engine
            .reconcile(
                std::slice::from_ref(&second),
                std::slice::from_ref(&root),
                44,
                true,
                false
            )
            .unwrap()
    );
    let rows = engine.call(json!({"op":"query","text":"ext:txt"}));
    for row in rows["rows"].as_array().unwrap() {
        assert_eq!(row["size"], 16);
    }
}

#[test]
fn event_scopes_never_follow_a_replaced_intermediate_directory() {
    for flags in [0x20100, 0x20000, 0x20400] {
        let (temporary, engine, root) = fixture();
        let branch = Path::new(&root).join("branch");
        let requested = branch.join("Documents");
        std::fs::create_dir_all(&requested).unwrap();
        std::fs::write(requested.join("original.txt"), "original").unwrap();
        let outside = temporary.path().join("outside");
        std::fs::create_dir_all(outside.join("Documents")).unwrap();
        std::fs::write(outside.join("Documents/private.txt"), "private").unwrap();
        scan(&engine, &root);
        let event = scanner::ChangeEvent::from_flags(requested.to_str().unwrap(), 42, flags);
        std::fs::rename(&branch, temporary.path().join("original-branch")).unwrap();
        std::os::unix::fs::symlink(&outside, &branch).unwrap();

        let mut plan = scanner::reconciliation_plan(std::slice::from_ref(&root), &[event]);
        engine.resolve_directory_checks(&mut plan).unwrap();
        assert!(
            plan.recursive
                .iter()
                .all(|path| Path::new(path).starts_with(&root)),
            "an event became authority outside its configured root: {plan:?}"
        );
        apply(&engine, &root, &plan);
        let results = engine.call(json!({"op":"query","text":"private.txt"}));
        assert_eq!(results["total"], 0, "flags={flags:x}: {results}");
        assert!(
            engine
                .index_store
                .lock()
                .unwrap()
                .entries()
                .unwrap()
                .iter()
                .all(|entry| Path::new(&entry.path).starts_with(&root))
        );

        std::fs::remove_file(&branch).unwrap();
        std::fs::rename(temporary.path().join("original-branch"), &branch).unwrap();
        let plan = scanner::reconciliation_plan(
            std::slice::from_ref(&root),
            &[scanner::ChangeEvent::from_flags(
                branch.to_str().unwrap(),
                43,
                0x20100,
            )],
        );
        apply(&engine, &root, &plan);
        assert_eq!(
            engine.call(json!({"op":"query","text":"original.txt"}))["total"],
            1
        );
    }
}
