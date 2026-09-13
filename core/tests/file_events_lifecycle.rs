#![cfg(target_os = "macos")]

use filesearch_core::scanner::Watcher;
use std::{
    fs,
    sync::{mpsc, Arc, Barrier},
    thread,
    time::{Duration, Instant},
};

fn descriptor_count() -> usize {
    fs::read_dir("/dev/fd").unwrap().count()
}

#[test]
fn active_watchers_can_move_threads_and_release_their_descriptors() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(fixture.path())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let mut baseline = 0;
    for round in 0..35 {
        let watcher = Watcher::start(std::slice::from_ref(&root), 0).unwrap();
        let path = format!("{root}/round-{round}.txt");
        fs::write(&path, b"live event before owner moves").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if watcher.drain().iter().any(|event| event.path == path) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "FSEvents did not deliver round {round}"
            );
            thread::sleep(Duration::from_millis(5));
        }
        // Exercise ownership transfer while another event may still be queued.
        fs::write(&path, b"pending event while owner shuts down").unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            drop(watcher);
            tx.send(()).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("Watcher drop deadlocked");
        fs::remove_file(path).unwrap();
        // CF/dispatch may open shared process resources on first use. Warm up
        // three complete start/callback/drop cycles before checking growth.
        if round == 2 {
            baseline = descriptor_count();
            // Exercise private mount-table buffers and independent CF owners
            // under actual parallel startup and teardown on four workers.
            let barrier = Arc::new(Barrier::new(4));
            let workers: Vec<_> = (0..4)
                .map(|_| {
                    let root = root.clone();
                    let barrier = Arc::clone(&barrier);
                    thread::spawn(move || {
                        barrier.wait();
                        for _ in 0..12 {
                            let watcher = Watcher::start(std::slice::from_ref(&root), 0).unwrap();
                            assert!(!filesearch_core::scanner::volumes().unwrap().is_empty());
                            drop(watcher);
                        }
                    })
                })
                .collect();
            for worker in workers {
                worker.join().unwrap();
            }
        }
    }
    let final_count = descriptor_count();
    assert!(
        final_count <= baseline + 3,
        "Descriptors leaked across 80 watcher lifetimes: baseline={baseline}, final={final_count}"
    );
}
