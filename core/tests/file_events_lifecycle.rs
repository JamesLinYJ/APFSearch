#![cfg(target_os = "macos")]

use apfsearch_core::scanner::Watcher;
use std::{
    fs,
    sync::{Arc, Barrier, mpsc},
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
        let owner = thread::spawn(move || {
            drop(watcher);
            tx.send(()).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("Watcher drop deadlocked");
        // The message confirms Drop completed, not that the owning thread has
        // exited. Join before measuring process-wide resources.
        owner.join().unwrap();
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
                            assert!(!apfsearch_core::scanner::volumes().unwrap().is_empty());
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
    // Stop/invalidate and callback draining protect our state synchronously;
    // Apple's dispatch-source cancellation may finish releasing kernel handles
    // asynchronously. Assert eventual reclamation with the SAME leak bound,
    // instead of treating a transient process-wide count as a permanent leak.
    // This deadline exists only in the test, never in Watcher::drop or queries.
    let immediate = descriptor_count();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut final_count = immediate;
    while final_count > baseline + 3 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
        final_count = descriptor_count();
    }
    eprintln!(
        "Watcher descriptor reclamation: baseline={baseline}, immediate={immediate}, final={final_count}"
    );
    assert!(
        final_count <= baseline + 3,
        "Descriptors leaked across 80 watcher lifetimes: baseline={baseline}, immediate={immediate}, final={final_count}"
    );
}
