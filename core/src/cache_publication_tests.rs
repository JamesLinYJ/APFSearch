use super::*;
use std::{sync::mpsc, time::Duration};

#[test]
fn metadata_commit_during_cache_publication_retains_the_new_delta() {
    // Exercise both sides of atomic file publication. The writer has already
    // checked its revision when it signals; no scheduler or filesystem timing
    // assumption determines whether the concurrent commit overlaps it.
    for pause_after_publish in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("index.sqlite");
        let mut store = IndexStore::open(&database).unwrap();
        let mut row = ScannedFile {
            path: "/cache-fixture/file.txt".into(),
            name: "file.txt".into(),
            extension: "txt".into(),
            volume_id: "fixture".into(),
            size: 1024,
            file_id: 1,
            ..Default::default()
        };
        store.batch(&[row.clone()], 1).unwrap();
        store.set("generation", &json!(1)).unwrap();
        let revision = store.get("revision", json!(0)).as_u64().unwrap();
        let snapshot = SearchSnapshot::new(store.entries().unwrap(), 1);
        store.cache_write(&snapshot, revision).unwrap();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(0);
        let (resume_sender, resume_receiver) = mpsc::sync_channel(0);
        let writer = std::thread::spawn(move || {
            let writer_store = IndexStore::open(&database).unwrap();
            writer_store
                .cache_write_with(&snapshot, revision, |path, snapshot, revision| {
                    if pause_after_publish {
                        crate::snapshot_cache::write(path, snapshot, revision)?;
                    }
                    ready_sender.send(()).unwrap();
                    resume_receiver
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap();
                    if !pause_after_publish {
                        crate::snapshot_cache::write(path, snapshot, revision)?;
                    }
                    Ok(())
                })
                .unwrap();
        });
        ready_receiver
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        row.size = 999_999;
        store.batch(&[row], 2).unwrap();
        store.set("generation", &json!(2)).unwrap();
        resume_sender.send(()).unwrap();
        writer.join().unwrap();
        assert!(
            store.cache_is_dirty(),
            "A stale publisher checkpointed a newer revision"
        );
        let (restored, generation) = store.cache_read().expect("The new delta must replay");
        assert_eq!(generation, 2);
        assert_eq!(restored.visible_entries().next().unwrap().size(), 999_999);
        assert_eq!(
            serde_json::to_value(restored.visible_entries().collect::<Vec<_>>()).unwrap(),
            serde_json::to_value(store.entries().unwrap()).unwrap()
        );
    }
}
