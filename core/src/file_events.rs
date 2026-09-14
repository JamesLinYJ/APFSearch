//! Filesystem and volume events with owned callback state and serial-queue shutdown.
use crate::filesystem::{c_array_string, mounted_filesystems, path_string};
use std::{
    collections::{HashMap, HashSet},
    ffi::{CStr, c_char, c_void},
    io,
    path::Path,
    ptr::{self, NonNull},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

// Only the actual Apple system boundary uses C ABI types and raw pointers.
mod ffi {
    use super::*;
    pub type Ref = *const c_void;
    pub type MutRef = *mut c_void;
    #[repr(C)]
    pub struct ArrayCallbacks {
        pub version: isize,
        pub retain: *const c_void,
        pub release: *const c_void,
        pub describe: *const c_void,
        pub equal: *const c_void,
    }
    #[repr(C)]
    pub struct EventContext {
        pub version: isize,
        pub info: *mut c_void,
        pub retain: *const c_void,
        pub release: *const c_void,
        pub describe: *const c_void,
    }
    pub type EventCallback =
        extern "C" fn(Ref, *mut c_void, usize, *mut c_void, *const u32, *const u64);
    pub type DiskCallback = extern "C" fn(Ref, *mut c_void);
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        pub static kCFTypeArrayCallBacks: ArrayCallbacks;
        pub fn CFRelease(value: Ref);
        pub fn CFArrayCreateMutable(
            allocator: Ref,
            capacity: isize,
            callbacks: *const ArrayCallbacks,
        ) -> MutRef;
        pub fn CFArrayAppendValue(array: MutRef, value: Ref);
        pub fn CFStringCreateWithFileSystemRepresentation(
            allocator: Ref,
            path: *const c_char,
        ) -> Ref;
        pub fn CFStringGetMaximumSizeOfFileSystemRepresentation(string: Ref) -> isize;
        pub fn CFStringGetFileSystemRepresentation(
            string: Ref,
            buffer: *mut c_char,
            size: isize,
        ) -> u8;
        pub fn CFDictionaryGetValue(dictionary: Ref, key: Ref) -> Ref;
        pub fn CFURLCopyFileSystemPath(url: Ref, style: isize) -> Ref;
        pub fn CFUUIDCreateString(allocator: Ref, uuid: Ref) -> Ref;
    }
    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        pub fn FSEventStreamCreate(
            allocator: Ref,
            callback: EventCallback,
            context: *mut EventContext,
            roots: Ref,
            since: u64,
            latency: f64,
            flags: u32,
        ) -> MutRef;
        pub fn FSEventStreamSetDispatchQueue(stream: MutRef, queue: MutRef);
        pub fn FSEventStreamStart(stream: MutRef) -> u8;
        pub fn FSEventStreamStop(stream: MutRef);
        pub fn FSEventStreamInvalidate(stream: MutRef);
        pub fn FSEventStreamRelease(stream: MutRef);
        pub fn FSEventsGetCurrentEventId() -> u64;
        pub fn FSEventsCopyUUIDForDevice(device: libc::dev_t) -> Ref;
    }
    #[link(name = "DiskArbitration", kind = "framework")]
    unsafe extern "C" {
        pub static kDADiskDescriptionVolumePathKey: Ref;
        pub fn DASessionCreate(allocator: Ref) -> Ref;
        pub fn DASessionSetDispatchQueue(session: Ref, queue: MutRef);
        pub fn DARegisterDiskAppearedCallback(
            session: Ref,
            matching: Ref,
            callback: DiskCallback,
            context: *mut c_void,
        );
        pub fn DARegisterDiskDisappearedCallback(
            session: Ref,
            matching: Ref,
            callback: DiskCallback,
            context: *mut c_void,
        );
        pub fn DAUnregisterCallback(session: Ref, callback: *const c_void, context: *mut c_void);
        pub fn DADiskGetBSDName(disk: Ref) -> *const c_char;
        pub fn DADiskCopyDescription(disk: Ref) -> Ref;
    }
    unsafe extern "C" {
        pub fn dispatch_queue_create(label: *const c_char, attributes: Ref) -> MutRef;
        pub fn dispatch_sync_f(
            queue: MutRef,
            context: *mut c_void,
            work: extern "C" fn(*mut c_void),
        );
        pub fn dispatch_release(object: MutRef);
    }
}
struct CfOwned(NonNull<c_void>);
impl CfOwned {
    /// `pointer` must be a +1 result of a CF Create/Copy API, never a borrowed Get.
    unsafe fn from_owned(pointer: ffi::Ref) -> io::Result<Self> {
        NonNull::new(pointer.cast_mut())
            .map(Self)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOMEM))
    }
    fn as_ptr(&self) -> ffi::Ref {
        self.0.as_ptr()
    }
}
impl Drop for CfOwned {
    fn drop(&mut self) {
        unsafe {
            ffi::CFRelease(self.as_ptr());
        }
    }
}
struct DispatchQueue(NonNull<c_void>);
impl DispatchQueue {
    fn new() -> io::Result<Self> {
        // dispatch_queue_create returns a retained serial queue when attr is NULL.
        let pointer = unsafe {
            ffi::dispatch_queue_create(c"app.apfsearch.filesystem".as_ptr(), ptr::null())
        };
        NonNull::new(pointer)
            .map(Self)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOMEM))
    }
    fn as_ptr(&self) -> ffi::MutRef {
        self.0.as_ptr()
    }
    fn drain(&self) {
        extern "C" fn barrier(_: *mut c_void) {}
        unsafe {
            ffi::dispatch_sync_f(self.as_ptr(), ptr::null_mut(), barrier);
        }
    }
}
impl Drop for DispatchQueue {
    fn drop(&mut self) {
        unsafe {
            ffi::dispatch_release(self.as_ptr());
        }
    }
}
struct EventStream {
    pointer: NonNull<c_void>,
    started: bool,
    invalidated: bool,
}
impl EventStream {
    fn as_ptr(&self) -> ffi::MutRef {
        self.pointer.as_ptr()
    }
    fn stop_and_invalidate(&mut self) {
        unsafe {
            if self.started {
                ffi::FSEventStreamStop(self.as_ptr());
                self.started = false;
            }
            if !self.invalidated {
                ffi::FSEventStreamInvalidate(self.as_ptr());
                self.invalidated = true;
            }
        }
    }
}
impl Drop for EventStream {
    fn drop(&mut self) {
        self.stop_and_invalidate();
        unsafe {
            ffi::FSEventStreamRelease(self.as_ptr());
        }
    }
}
#[derive(Default)]
struct MountState {
    known: HashSet<String>,
    disk_paths: HashMap<Vec<u8>, String>,
}
type EventHandler = dyn Fn(&str, u64, u32) + Send + Sync + 'static;
struct CallbackState {
    handler: Box<EventHandler>,
    roots: Vec<String>,
    mounts: Mutex<MountState>,
    panicked: AtomicBool,
}
impl CallbackState {
    fn emit(&self, path: &str, id: u64, flags: u32) {
        (self.handler)(path, id, flags);
    }
    fn unreadable_path(&self, id: u64) {
        for root in &self.roots {
            self.emit(root, id, EVENT_MUST_SCAN | EVENT_USER_DROPPED);
        }
    }
}
const EVENT_MUST_SCAN: u32 = 0x01;
const EVENT_USER_DROPPED: u32 = 0x02;
const EVENT_ROOT_CHANGED: u32 = 0x20;
pub(crate) const EVENT_HISTORY_DONE: u32 = 0x10;
pub(crate) const EVENT_IDS_WRAPPED: u32 = 0x08;
pub(crate) const GLOBAL_HISTORY_INVALIDATION: u32 = 0x0e;
const CREATE_NO_DEFER: u32 = 0x02;
const CREATE_WATCH_ROOT: u32 = 0x04;
const CREATE_FILE_EVENTS: u32 = 0x10;
// Replays the containing history chunk across an unclean restart; duplicates
// are intentional and reconciled against current filesystem metadata.
const CREATE_FULL_HISTORY: u32 = 0x80;
const EVENT_ID_SINCE_NOW: u64 = u64::MAX;
fn callback_boundary(info: *mut c_void, body: impl FnOnce(&CallbackState)) {
    // info points to the stable boxed state retained through stop/unregister and
    // queue drain. Apple invokes callbacks only while that owner is alive.
    let Some(state) = (unsafe { info.cast::<CallbackState>().as_ref() }) else {
        return;
    };
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(state))).is_err() {
        state.panicked.store(true, Ordering::Release);
    }
}
extern "C" fn filesystem_event(
    _: ffi::Ref,
    info: *mut c_void,
    count: usize,
    paths: *mut c_void,
    flags: *const u32,
    ids: *const u64,
) {
    callback_boundary(info, |state| {
        if count == 0 {
            return;
        }
        if flags.is_null() || ids.is_null() {
            state.unreadable_path(0);
            return;
        }
        // Flags and IDs contain count elements. The sentinel has no meaningful
        // path, so do not dereference or decode its path pointer at all.
        let (flags, ids) = unsafe {
            (
                std::slice::from_raw_parts(flags, count),
                std::slice::from_raw_parts(ids, count),
            )
        };
        for (index, (&flag, &id)) in flags.iter().zip(ids).enumerate() {
            if flag & (EVENT_HISTORY_DONE | GLOBAL_HISTORY_INVALIDATION) != 0 {
                state.emit("", id, flag);
                continue;
            }
            // UseCFTypes is not set: ordinary events carry count C-string
            // pointers. Each pointer belongs to Apple for this callback only.
            let name = if paths.is_null() {
                ptr::null()
            } else {
                unsafe { *paths.cast::<*const c_char>().add(index) }
            };
            if name.is_null() {
                state.unreadable_path(id);
                continue;
            }
            match unsafe { CStr::from_ptr(name) }.to_str() {
                Ok(path) => state.emit(path, id, flag),
                Err(_) => state.unreadable_path(id),
            }
        }
    });
}
fn cf_path(string: ffi::Ref) -> Option<String> {
    if string.is_null() {
        return None;
    }
    let capacity = unsafe { ffi::CFStringGetMaximumSizeOfFileSystemRepresentation(string) };
    if capacity <= 0 || capacity > 1024 * 1024 {
        return None;
    }
    let mut buffer = vec![0u8; capacity as usize];
    if unsafe {
        ffi::CFStringGetFileSystemRepresentation(string, buffer.as_mut_ptr().cast(), capacity)
    } == 0
    {
        return None;
    }
    CStr::from_bytes_until_nul(&buffer)
        .ok()?
        .to_str()
        .ok()
        .map(str::to_owned)
}
fn disk_event(disk: ffi::Ref, state: &CallbackState, disappeared: bool) {
    let bsd = unsafe { ffi::DADiskGetBSDName(disk) };
    let bsd = if bsd.is_null() {
        None
    } else {
        Some(unsafe { CStr::from_ptr(bsd) }.to_bytes().to_vec())
    };
    let description = unsafe { CfOwned::from_owned(ffi::DADiskCopyDescription(disk)) }.ok();
    let name = description.as_ref().and_then(|description| {
        let url = unsafe {
            ffi::CFDictionaryGetValue(description.as_ptr(), ffi::kDADiskDescriptionVolumePathKey)
        };
        if url.is_null() {
            return None;
        }
        let name = unsafe { CfOwned::from_owned(ffi::CFURLCopyFileSystemPath(url, 0)) }.ok()?;
        cf_path(name.as_ptr())
    });
    let mut mounts = state.mounts.lock().unwrap_or_else(|e| e.into_inner());
    let name = name.or_else(|| {
        if disappeared {
            bsd.as_ref()
                .and_then(|bsd| mounts.disk_paths.get(bsd).cloned())
        } else {
            None
        }
    });
    if let Some(name) = name {
        let known = mounts.known.contains(&name);
        if disappeared {
            mounts.known.remove(&name);
            if let Some(bsd) = bsd {
                mounts.disk_paths.remove(&bsd);
            }
        } else {
            mounts.known.insert(name.clone());
            if let Some(bsd) = bsd {
                mounts.disk_paths.insert(bsd, name.clone());
            }
        }
        drop(mounts);
        if disappeared || !known {
            state.emit(
                &name,
                0, // Disk Arbitration cannot acknowledge the FSEvents stream.
                EVENT_ROOT_CHANGED | EVENT_MUST_SCAN,
            );
        }
    }
}
extern "C" fn disk_appeared(disk: ffi::Ref, info: *mut c_void) {
    callback_boundary(info, |state| disk_event(disk, state, false));
}
extern "C" fn disk_disappeared(disk: ffi::Ref, info: *mut c_void) {
    callback_boundary(info, |state| disk_event(disk, state, true));
}
/// Moving the owner between worker threads is safe: callback state has a stable
/// allocation and all mutable callback data is synchronized. It must never be
/// destroyed from its own serial callback queue (scanner callbacks cannot own it).
pub(crate) struct Watcher {
    stream: Option<EventStream>,
    session: Option<CfOwned>,
    queue: DispatchQueue,
    state: Box<CallbackState>,
}
unsafe impl Send for Watcher {}
impl Watcher {
    pub(crate) fn start(
        roots: &[String],
        since: u64,
        handler: impl Fn(&str, u64, u32) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        if roots.is_empty() {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let mut known = HashSet::new();
        for fs in mounted_filesystems()? {
            if let Ok(path) = c_array_string(&fs.f_mntonname)?.to_str() {
                known.insert(path.to_owned());
            }
        }
        let mut state = Box::new(CallbackState {
            handler: Box::new(handler),
            roots: roots.to_vec(),
            mounts: Mutex::new(MountState {
                known,
                disk_paths: HashMap::new(),
            }),
            panicked: AtomicBool::new(false),
        });
        let queue = DispatchQueue::new()?;
        let array = unsafe {
            CfOwned::from_owned(ffi::CFArrayCreateMutable(
                ptr::null(),
                0,
                ptr::addr_of!(ffi::kCFTypeArrayCallBacks),
            ))
        }?;
        for root in roots {
            let root = path_string(Path::new(root))?;
            let string = unsafe {
                CfOwned::from_owned(ffi::CFStringCreateWithFileSystemRepresentation(
                    ptr::null(),
                    root.as_ptr(),
                ))
            }?;
            unsafe {
                ffi::CFArrayAppendValue(array.as_ptr().cast_mut(), string.as_ptr());
            }
        }
        let info = ptr::from_mut(state.as_mut()).cast::<c_void>();
        let mut context = ffi::EventContext {
            version: 0,
            info,
            retain: ptr::null(),
            release: ptr::null(),
            describe: ptr::null(),
        };
        let pointer = unsafe {
            ffi::FSEventStreamCreate(
                ptr::null(),
                filesystem_event,
                &mut context,
                array.as_ptr(),
                if since == 0 {
                    EVENT_ID_SINCE_NOW
                } else {
                    since
                },
                0.1,
                CREATE_FILE_EVENTS | CREATE_WATCH_ROOT | CREATE_NO_DEFER | CREATE_FULL_HISTORY,
            )
        };
        let pointer = NonNull::new(pointer)
            .ok_or_else(|| io::Error::other("FSEvents stream creation failed"))?;
        let stream = EventStream {
            pointer,
            started: false,
            invalidated: false,
        };
        // Construct the full owner BEFORE scheduling anything. Every later error
        // follows the same stop/unregister/drain lifetime path as a normal drop.
        let mut watcher = Self {
            stream: Some(stream),
            session: None,
            queue,
            state,
        };
        unsafe {
            ffi::FSEventStreamSetDispatchQueue(pointer.as_ptr(), watcher.queue.as_ptr());
        }
        if unsafe { ffi::FSEventStreamStart(pointer.as_ptr()) } == 0 {
            return Err(io::Error::other("FSEvents stream could not start"));
        }
        watcher.stream.as_mut().unwrap().started = true;
        if let Ok(session) = unsafe { CfOwned::from_owned(ffi::DASessionCreate(ptr::null())) } {
            unsafe {
                ffi::DARegisterDiskAppearedCallback(
                    session.as_ptr(),
                    ptr::null(),
                    disk_appeared,
                    info,
                );
                ffi::DARegisterDiskDisappearedCallback(
                    session.as_ptr(),
                    ptr::null(),
                    disk_disappeared,
                    info,
                );
                ffi::DASessionSetDispatchQueue(session.as_ptr(), watcher.queue.as_ptr());
            }
            watcher.session = Some(session);
        }
        Ok(watcher)
    }
    pub(crate) fn take_callback_failure(&self) -> bool {
        self.state.panicked.swap(false, Ordering::AcqRel)
    }
}
impl Drop for Watcher {
    fn drop(&mut self) {
        if let Some(stream) = &mut self.stream {
            stream.stop_and_invalidate();
        }
        if let Some(session) = &self.session {
            let info = ptr::from_mut(self.state.as_mut()).cast::<c_void>();
            unsafe {
                ffi::DAUnregisterCallback(session.as_ptr(), disk_appeared as *const c_void, info);
                ffi::DAUnregisterCallback(
                    session.as_ptr(),
                    disk_disappeared as *const c_void,
                    info,
                );
                ffi::DASessionSetDispatchQueue(session.as_ptr(), ptr::null_mut());
            }
        }
        self.queue.drain();
        self.stream.take();
        self.session.take();
        // queue releases next; boxed callback state remains alive through the drain.
    }
}
pub(crate) fn current_event_id() -> u64 {
    unsafe { ffi::FSEventsGetCurrentEventId() }
}

/// Identifies the persisted event journal, not merely the APFS volume. A
/// missing UUID means the volume cannot safely restore historical events.
pub(crate) fn journal_uuid(path: &Path) -> io::Result<Option<String>> {
    use std::os::unix::fs::MetadataExt;
    let device = std::fs::symlink_metadata(path)?.dev() as libc::dev_t;
    let pointer = unsafe { ffi::FSEventsCopyUUIDForDevice(device) };
    if pointer.is_null() {
        return Ok(None);
    }
    let uuid = unsafe { CfOwned::from_owned(pointer) }?;
    let text = unsafe { CfOwned::from_owned(ffi::CFUUIDCreateString(ptr::null(), uuid.as_ptr())) }?;
    cf_path(text.as_ptr())
        .map(Some)
        .ok_or_else(|| io::Error::other("Invalid journal UUID"))
}
/// Conservative per-host replay proof: a changed boot session requires a new
/// baseline. This avoids treating a persisted host cursor as a per-device one.
pub(crate) fn boot_session() -> io::Result<String> {
    let mut bytes = [0u8; 128];
    let mut length = bytes.len();
    // The kernel writes at most the supplied length; the name is NUL terminated.
    let result = unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            bytes.as_mut_ptr().cast(),
            &mut length,
            ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    CStr::from_bytes_until_nul(&bytes[..length.min(bytes.len())])
        .ok()
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .ok_or_else(|| io::Error::other("Invalid boot session UUID"))
}

#[cfg(test)]
#[path = "file_events_tests.rs"]
mod tests;
