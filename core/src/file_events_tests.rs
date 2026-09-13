use super::*;
use std::{
    fs,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::Duration,
};

#[test]
fn watcher_drop_waits_for_an_in_flight_callback_before_releasing_its_state() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(fixture.path())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let path = format!("{root}/blocked-callback.txt");
    let expected = path.clone();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let fired = AtomicBool::new(false);
    let state_alive = Arc::new(());
    let weak_state = Arc::downgrade(&state_alive);
    let watcher = Watcher::start(std::slice::from_ref(&root), 0, move |path, _, _| {
        // Captured ownership lets the test independently observe that callback
        // state stays alive until the blocked callback and owner drop complete.
        let _keep_alive = &state_alive;
        if path == expected && !fired.swap(true, Ordering::AcqRel) {
            entered_tx.send(()).unwrap();
            release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
        }
    })
    .unwrap();
    fs::write(path, b"trigger a real Apple callback").unwrap();
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("No real callback arrived");
    let (dropped_tx, dropped_rx) = mpsc::channel();
    thread::spawn(move || {
        drop(watcher);
        dropped_tx.send(()).unwrap();
    });
    let premature = dropped_rx.recv_timeout(Duration::from_millis(100));
    let alive_while_blocked = weak_state.upgrade().is_some();
    // Always unblock before assertions so a failing check cannot strand FFI.
    release_tx.send(()).unwrap();
    dropped_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("Drop did not finish after callback returned");
    assert!(
        matches!(premature, Err(mpsc::RecvTimeoutError::Timeout)),
        "Owner released before callback completed"
    );
    assert!(alive_while_blocked, "Callback state was released too soon");
    assert!(
        weak_state.upgrade().is_none(),
        "Callback state leaked after Drop"
    );
}

#[test]
fn callback_panics_never_unwind_through_apple_abi() {
    let mut state = Box::new(CallbackState {
        handler: Box::new(|_, _, _| panic!("test callback")),
        roots: vec!["/fixture".into()],
        mounts: Mutex::new(MountState::default()),
        panicked: AtomicBool::new(false),
    });
    callback_boundary(ptr::from_mut(state.as_mut()).cast(), |s| {
        s.emit("/fixture", 1, 0)
    });
    assert!(state.panicked.load(Ordering::Acquire));
}

#[test]
fn history_done_does_not_read_an_undefined_path_or_request_a_rescan() {
    let received = Arc::new(Mutex::new(Vec::new()));
    let sink = received.clone();
    let mut state = Box::new(CallbackState {
        handler: Box::new(move |path, id, flags| {
            sink.lock().unwrap().push((path.to_owned(), id, flags))
        }),
        roots: vec!["/fixture".into()],
        mounts: Mutex::new(MountState::default()),
        panicked: AtomicBool::new(false),
    });
    filesystem_event(
        ptr::null(),
        ptr::from_mut(state.as_mut()).cast(),
        1,
        ptr::null_mut(),
        &EVENT_HISTORY_DONE,
        &123,
    );
    assert_eq!(
        *received.lock().unwrap(),
        vec![(String::new(), 123, EVENT_HISTORY_DONE)]
    );
    assert!(!state.panicked.load(Ordering::Acquire));
}
#[test]
fn global_history_controls_preserve_wrapped_flags_without_reading_paths() {
    let received = Arc::new(Mutex::new(Vec::new()));
    let sink = received.clone();
    let mut state = Box::new(CallbackState {
        handler: Box::new(move |path, id, flags| {
            sink.lock().unwrap().push((path.to_owned(), id, flags))
        }),
        roots: vec!["/fixture".into()],
        mounts: Mutex::new(MountState::default()),
        panicked: AtomicBool::new(false),
    });
    for flags in [0x02, 0x04, EVENT_IDS_WRAPPED, 0x0f] {
        filesystem_event(
            ptr::null(),
            ptr::from_mut(state.as_mut()).cast(),
            1,
            ptr::null_mut(),
            &flags,
            &7,
        );
        let invalid = [0xffu8, 0];
        let mut path = invalid.as_ptr().cast::<c_char>();
        filesystem_event(
            ptr::null(),
            ptr::from_mut(state.as_mut()).cast(),
            1,
            ptr::from_mut(&mut path).cast(),
            &flags,
            &8,
        );
        assert_eq!(
            received.lock().unwrap().pop(),
            Some((String::new(), 8, flags))
        );
        assert_eq!(
            received.lock().unwrap().pop(),
            Some((String::new(), 7, flags))
        );
    }
}
