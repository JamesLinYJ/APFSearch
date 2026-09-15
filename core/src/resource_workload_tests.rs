//! Opt-in, identical-workload comparison against the saved baseline sources.
use super::*;
use std::os::unix::fs::MetadataExt;

fn cpu_seconds() -> f64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // getrusage initializes this correctly aligned structure on success.
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
        0
    );
    let usage = unsafe { usage.assume_init() };
    (usage.ru_utime.tv_sec + usage.ru_stime.tv_sec) as f64
        + (usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) as f64 / 1_000_000.0
}

fn usage() -> libc::rusage_info_v4 {
    let mut value = std::mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
    // Darwin initializes the complete, correctly aligned public v4 structure.
    assert_eq!(
        unsafe {
            libc::proc_pid_rusage(
                std::process::id() as i32,
                libc::RUSAGE_INFO_V4,
                value.as_mut_ptr().cast(),
            )
        },
        0
    );
    unsafe { value.assume_init() }
}

#[test]
#[ignore = "Creates 512 tiny hard-link directory entries only in a temporary fixture"]
fn reconciliation_resource_workload() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let files = root.join("files");
    std::fs::create_dir(&files).unwrap();
    let mut originals = Vec::new();
    let mut paths = Vec::new();
    let small_directories =
        std::env::var("APFSEARCH_WORKLOAD_LAYOUT").as_deref() == Ok("directories");
    for object in 0..128 {
        let directory = if small_directories {
            let directory = files.join(format!("directory-{object:04}"));
            std::fs::create_dir(&directory).unwrap();
            directory
        } else {
            files.clone()
        };
        let first = directory.join(format!("object-{object:04}-0.txt"));
        std::fs::write(&first, b"initial").unwrap();
        originals.push(first.clone());
        paths.push(first.clone());
        for alias in 1..4 {
            let path = directory.join(format!("object-{object:04}-{alias}.txt"));
            std::fs::hard_link(&first, &path).unwrap();
            paths.push(path);
        }
    }
    let engine = SearchEngine::open(&root.join("index/index.sqlite")).unwrap();
    let roots = vec![files.to_str().unwrap().to_owned()];
    engine
        .start_scan(&json!({"roots":roots,"watch":false,"wait":true}))
        .unwrap();
    let mut measurements = Vec::new();
    for round in 0..3 {
        for path in &originals {
            std::fs::write(path, format!("updated round {round}")).unwrap();
        }
        let before = usage();
        let cpu_before = cpu_seconds();
        let start = Instant::now();
        assert!(engine.reconcile(&roots, &roots, 0, true, false).unwrap());
        let elapsed = start.elapsed();
        let cpu_elapsed = cpu_seconds() - cpu_before;
        let after = usage();
        let store = engine.index_store.lock().unwrap();
        let rows = store.entries().unwrap();
        assert_eq!(
            rows.len(),
            paths.len() + 1 + if small_directories { 128 } else { 0 }
        );
        for entry in rows {
            let metadata = std::fs::symlink_metadata(&entry.path).unwrap();
            assert_eq!(entry.file_id, metadata.ino());
            if !metadata.is_dir() {
                assert_eq!(entry.size, metadata.len());
            }
        }
        measurements.push(json!({"round":round,"wall_ms":elapsed.as_secs_f64()*1000.,
            "cpu_ms":cpu_elapsed * 1000.0,
            "disk_written":after.ri_diskio_byteswritten-before.ri_diskio_byteswritten,
            "logical_writes":after.ri_logical_writes-before.ri_logical_writes,
            "footprint":after.ri_phys_footprint}));
    }
    engine.stop.store(true, Ordering::Relaxed);
    println!("RESOURCE_RESULT {}", json!(measurements));
}
