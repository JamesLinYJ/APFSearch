//! APFS namespace enumeration and journal-backed invalidation.
//!
//! File objects (volume UUID + inode) and directory entries (path) deliberately
//! have different identities. In particular, hard links are never deduplicated.
//! A watcher must be created *before* scan; events mark paths dirty, never invent
//! final metadata. Reconcile those paths and atomically commit the event cursor.
use crate::{
    file_events,
    filesystem::{self, FileKind, FileMetadata},
};
use serde::{Deserialize, Serialize};
use std::ops::ControlFlow;
use std::{
    collections::{HashSet, VecDeque},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{SyncSender, sync_channel},
    },
};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ScannedFile {
    pub path: String,
    pub name: String,
    pub extension: String,
    pub size: u64,
    pub modified: i64,
    pub changed: i64,
    pub created: i64,
    #[serde(default)]
    pub modified_ns: i64,
    #[serde(default)]
    pub changed_ns: i64,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub file_id: u64,
    pub parent_id: u64,
    pub volume_id: String,
    pub flags: u32,
    #[serde(default)]
    pub link_count: Option<u64>,
}
impl ScannedFile {
    pub fn is_dataless(&self) -> bool {
        self.flags & 0x4000_0000 != 0
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ScanReport {
    pub entries: u64,
    pub roots: Vec<String>,
    pub uncovered: Vec<String>,
    pub errors: Vec<String>,
    pub cancelled: bool,
    #[serde(default)]
    pub excluded_mounts: Vec<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Volume {
    pub path: String,
    pub id: String,
    pub filesystem: String,
}
fn from_metadata(path: String, metadata: &FileMetadata) -> ScannedFile {
    let file_path = Path::new(&path);
    ScannedFile {
        name: file_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.clone()),
        extension: if metadata.kind == FileKind::Directory {
            String::new()
        } else {
            file_path
                .extension()
                .map(|s| s.to_string_lossy().to_lowercase())
                .unwrap_or_default()
        },
        path,
        size: metadata.size,
        modified: metadata.modified,
        changed: metadata.changed,
        created: metadata.created,
        modified_ns: metadata.modified_ns,
        changed_ns: metadata.changed_ns,
        is_dir: metadata.kind == FileKind::Directory,
        is_symlink: metadata.kind == FileKind::Symlink,
        file_id: metadata.file_id,
        parent_id: metadata.parent_id,
        volume_id: metadata.volume_id.clone(),
        flags: metadata.flags,
        link_count: metadata.link_count,
    }
}
/// Read one directory entry without following a symlink.
pub fn stat_entry(path: &str) -> Result<ScannedFile, String> {
    let scope = mount_scope().map_err(|error| error.to_string())?;
    filesystem::stat_entry(Path::new(path), &scope)
        .map(|entry| from_metadata(path.into(), &entry))
        .map_err(|error| format!("{path}: {error}"))
}
/// Reuse a pinned parent for adjacent siblings and validate namespace authority
/// before returning any observations. No partial batch survives a mount change.
#[derive(Debug)]
pub(crate) enum MetadataBatchError {
    Cancelled,
    Filesystem(String),
}
impl From<String> for MetadataBatchError {
    fn from(message: String) -> Self {
        Self::Filesystem(message)
    }
}
pub(crate) struct MetadataBatch {
    pub entries: Vec<(String, Result<ScannedFile, String>)>,
}
pub(crate) fn stat_entries(
    mut paths: Vec<String>,
    cancelled: &AtomicBool,
) -> Result<MetadataBatch, MetadataBatchError> {
    paths.sort_unstable_by(|a, b| {
        Path::new(a)
            .parent()
            .cmp(&Path::new(b).parent())
            .then(a.cmp(b))
    });
    let scope = mount_scope().map_err(|error| error.to_string())?;
    let mut reader = filesystem::MetadataReader::new(&scope);
    let mut parent = None::<std::path::PathBuf>;
    let mut result = Vec::with_capacity(paths.len());
    for path in paths {
        if cancelled.load(Ordering::Relaxed) {
            return Err(MetadataBatchError::Cancelled);
        }
        let next = Path::new(&path).parent().map(Path::to_owned);
        if next != parent {
            reader
                .validate_parent()
                .map_err(|error| error.to_string())?;
            reader = filesystem::MetadataReader::new(&scope);
            parent = next;
        }
        let entry = reader
            .stat_in_batch(Path::new(&path))
            .map(|metadata| from_metadata(path.clone(), &metadata))
            .map_err(|error| format!("{path}: {error}"));
        result.push((path, entry));
    }
    reader
        .validate_parent()
        .map_err(|error| error.to_string())?;
    if !scope.still_current().map_err(|error| error.to_string())? {
        return Err(MetadataBatchError::Filesystem(
            "Mount identity changed during metadata verification".into(),
        ));
    }
    Ok(MetadataBatch { entries: result })
}

pub(crate) fn directory_accessible(path: &str) -> bool {
    mount_scope()
        .is_ok_and(|scope| filesystem::check_directory_access(Path::new(path), &scope).is_ok())
}
pub(crate) fn scope_accessible(path: &str) -> bool {
    match stat_entry(path) {
        Ok(file) if file.is_dir => directory_accessible(path),
        Ok(_) => Path::new(path)
            .parent()
            .and_then(Path::to_str)
            .is_some_and(directory_accessible),
        Err(_) => false,
    }
}
/// Return mounted, visible APFS volumes. Historical snapshots and hidden
/// /System/Volumes aliases are excluded; the live boot snapshot is included.
pub fn volumes() -> Result<Vec<Volume>, String> {
    filesystem::volumes()
        .map(|volumes| {
            volumes
                .into_iter()
                .map(|volume| Volume {
                    path: volume.path,
                    id: volume.id,
                    filesystem: volume.filesystem,
                })
                .collect()
        })
        .map_err(|error| error.to_string())
}

pub(crate) const SYSTEM_VOLUMES: &str = "/System/Volumes";
const DATA_ROOT: &str = "/System/Volumes/Data";

/// The default boot namespace includes the live Data overlay, but not hidden
/// auxiliary volumes. An explicit selection under System/Volumes keeps its
/// meaning; a temporary event/recovery scope must never become such a selection.
pub(crate) fn path_in_namespace(path: &str, configured: &[String]) -> bool {
    let path = Path::new(path);
    if path == Path::new(SYSTEM_VOLUMES) || !path.starts_with(SYSTEM_VOLUMES) {
        return true;
    }
    if path.starts_with(DATA_ROOT) && configured.iter().any(|root| root == "/") {
        return true;
    }
    configured.iter().any(|root| {
        Path::new(root).starts_with(SYSTEM_VOLUMES)
            && (path.starts_with(root) || Path::new(root).starts_with(path))
    })
}
/// Whether every descendant belongs to the configured namespace. An explicit
/// deeper selection can make an ancestor a structural node without authorizing
/// its sibling subtrees; cleanup must descend only along that selected branch.
pub(crate) fn subtree_in_namespace(path: &str, configured: &[String]) -> bool {
    let path = Path::new(path);
    if !path.starts_with(SYSTEM_VOLUMES) {
        return true;
    }
    if path.starts_with(DATA_ROOT) && configured.iter().any(|root| root == "/") {
        return true;
    }
    configured
        .iter()
        .any(|root| Path::new(root).starts_with(SYSTEM_VOLUMES) && path.starts_with(root))
}
fn descend_in_namespace(path: &str, configured: &[String]) -> bool {
    path_in_namespace(path, configured)
        && (path != SYSTEM_VOLUMES || configured.iter().any(|root| root == SYSTEM_VOLUMES))
}
#[derive(Clone, Debug)]
struct Firmlink {
    physical: String,
    logical: String,
}
fn firmlinks() -> &'static [Firmlink] {
    static LINKS: std::sync::OnceLock<Vec<Firmlink>> = std::sync::OnceLock::new();
    LINKS.get_or_init(|| {
        let mut links: Vec<_> = std::fs::read_to_string("/usr/share/firmlinks")
            .unwrap_or_default()
            .lines()
            .filter_map(|line| {
                let mut columns = line.split_whitespace();
                let logical = columns.next()?;
                let target = columns.next()?;
                if !logical.starts_with('/')
                    || target.starts_with('/')
                    || Path::new(target)
                        .components()
                        .any(|part| matches!(part, Component::ParentDir))
                {
                    return None;
                }
                Some(Firmlink {
                    physical: format!("{DATA_ROOT}/{target}"),
                    logical: logical.to_string(),
                })
            })
            .collect();
        links.sort_by_key(|link| std::cmp::Reverse(link.physical.len()));
        links
    })
}
pub(crate) fn mount_scope() -> std::io::Result<filesystem::MountScope> {
    static MAPPED: Mutex<Option<filesystem::MountScope>> = Mutex::new(None);
    let scope = filesystem::MountScope::load()?;
    let mut cached = MAPPED.lock().unwrap();
    if let Some(previous) = cached
        .as_ref()
        .filter(|previous| previous.identity() == scope.identity())
    {
        return Ok(previous.clone());
    }
    let scope = scope.with_aliases(
        firmlinks()
            .iter()
            .map(|link| (PathBuf::from(&link.physical), PathBuf::from(&link.logical)))
            .chain(filesystem::system_aliases().iter().cloned()),
    );
    *cached = Some(scope.clone());
    Ok(scope)
}

/// A missing entry may be retired only when a real, accessible ancestor inside
/// the selected scope proves its absence. Refusal to follow a symlink or an
/// access error is unknown coverage, never evidence of deletion.
pub(crate) fn entry_is_missing(
    path: &Path,
    configured: &[String],
    scope: &filesystem::MountScope,
) -> bool {
    let selected = |candidate: &Path| configured.iter().any(|root| candidate.starts_with(root));
    if configured.iter().any(|root| path == Path::new(root))
        || !selected(path)
        || !filesystem::metadata(path, scope)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    {
        return false;
    }
    for ancestor in path
        .ancestors()
        .skip(1)
        .take_while(|ancestor| selected(ancestor))
    {
        match filesystem::metadata(ancestor, scope) {
            Ok(metadata) => {
                return metadata.st_mode & libc::S_IFMT == libc::S_IFDIR
                    && filesystem::check_directory_access(ancestor, scope).is_ok();
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return false,
        }
    }
    false
}
fn same_firmlink_object(link: &Firmlink) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(scope) = filesystem::MountScope::load() else {
        return false;
    };
    if !scope.allows(Path::new(&link.physical)) || !scope.allows(Path::new(&link.logical)) {
        return false;
    }
    match (
        std::fs::symlink_metadata(&link.logical),
        std::fs::symlink_metadata(&link.physical),
    ) {
        (Ok(logical), Ok(physical)) => {
            logical.dev() == physical.dev() && logical.ino() == physical.ino()
        }
        _ => false,
    }
}
/// Only actual firmlink targets share a namespace. Data-only directories and
/// overlay ancestors (Data/usr, Data/System) retain their physical paths.
/// Longest-match mapping also verifies that both names identify the same object.
fn lexical_path(path: &str) -> PathBuf {
    let mut out = PathBuf::from("/");
    for component in Path::new(path).components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => out.push(name),
            _ => (),
        }
    }
    out
}
pub fn visible_path(path: &str) -> String {
    let out = lexical_path(path);
    for link in firmlinks() {
        if let Ok(suffix) = out.strip_prefix(&link.physical)
            && same_firmlink_object(link)
        {
            return if suffix.as_os_str().is_empty() {
                link.logical.clone()
            } else {
                Path::new(&link.logical)
                    .join(suffix)
                    .to_string_lossy()
                    .into_owned()
            };
        }
    }
    out.to_string_lossy().into_owned()
}
/// An ephemeral component-aware ancestor index. Every membership query probes
/// at most the path depth, independent of the number of sibling scopes.
#[derive(Default)]
pub(crate) struct PathScopes {
    roots: HashSet<PathBuf>,
    #[cfg(test)]
    ancestor_probes: std::cell::Cell<usize>,
}
impl PathScopes {
    pub(crate) fn from_paths(paths: &[String]) -> Self {
        Self {
            roots: paths.iter().map(PathBuf::from).collect(),
            #[cfg(test)]
            ancestor_probes: std::cell::Cell::new(0),
        }
    }
    pub(crate) fn insert(&mut self, path: &Path) {
        self.roots.insert(path.to_path_buf());
    }
    pub(crate) fn contains(&self, path: &Path) -> bool {
        self.roots.contains(path)
    }
    pub(crate) fn covers(&self, path: &Path) -> bool {
        self.covers_where(path, |_| true)
    }
    pub(crate) fn covers_where(&self, path: &Path, mut accepts: impl FnMut(&Path) -> bool) -> bool {
        path.ancestors().any(|ancestor| {
            #[cfg(test)]
            self.ancestor_probes.set(self.ancestor_probes.get() + 1);
            self.roots.get(ancestor).is_some_and(|root| accepts(root))
        })
    }
    #[cfg(test)]
    pub(crate) fn ancestor_probes(&self) -> usize {
        self.ancestor_probes.get()
    }
}
/// Collapse already mapped scopes without resolving or inventing aliases.
/// Namespace exceptions still decide whether a matching ancestor covers a root.
pub(crate) fn compact_roots(
    mut roots: Vec<String>,
    cancelled: impl Fn() -> bool,
) -> Option<Vec<String>> {
    if cancelled() {
        return None;
    }
    roots.sort();
    roots.dedup();
    let mut ancestors = PathScopes::default();
    let mut result = Vec::new();
    for root in roots {
        if cancelled() {
            return None;
        }
        if !ancestors.covers_where(Path::new(&root), |parent| {
            subtree_in_namespace(&root, &[parent.to_string_lossy().into_owned()])
        }) {
            ancestors.insert(Path::new(&root));
            result.push(root);
        }
    }
    Some(result)
}
/// Resolve a fresh user selection once. Stored roots and event/recovery scopes
/// must use normalize_scopes instead, so later symlink changes cannot retarget it.
pub fn normalize_roots(roots: &[String]) -> Vec<String> {
    normalize_roots_until(roots, || false).expect("normalization was not cancelled")
}
pub(crate) fn normalize_roots_until(
    roots: &[String],
    cancelled: impl Fn() -> bool,
) -> Option<Vec<String>> {
    normalize_paths(roots, true, cancelled)
}
/// Reuse persisted selections and event/recovery paths without following a
/// mutable user alias. Only verified OS aliases retain their physical spelling.
pub(crate) fn normalize_scopes(roots: &[String]) -> Vec<String> {
    normalize_scopes_until(roots, || false).expect("normalization was not cancelled")
}
pub(crate) fn normalize_scopes_until(
    roots: &[String],
    cancelled: impl Fn() -> bool,
) -> Option<Vec<String>> {
    normalize_paths(roots, false, cancelled)
}
fn normalize_paths(
    roots: &[String],
    new_selection: bool,
    cancelled: impl Fn() -> bool,
) -> Option<Vec<String>> {
    if cancelled() {
        return None;
    }
    let scope = new_selection.then(|| mount_scope().ok()).flatten();
    let mut out: Vec<String> = roots
        .iter()
        .map(|r| {
            if cancelled() {
                return None;
            }
            Some((|| {
                let absolute = if Path::new(r).is_absolute() {
                    r.clone()
                } else {
                    std::env::current_dir()
                        .unwrap_or_else(|_| PathBuf::from("/"))
                        .join(r)
                        .to_string_lossy()
                        .into_owned()
                };
                let absolute = lexical_path(&absolute).to_string_lossy().into_owned();
                // Never canonicalize or lstat a path inside an excluded mount.
                if new_selection
                    && !scope
                        .as_ref()
                        .is_some_and(|scope| scope.allows(Path::new(&absolute)))
                {
                    return absolute;
                }
                let visible = visible_path(&absolute);
                if !new_selection {
                    return filesystem::system_path(Path::new(&visible))
                        .to_string_lossy()
                        .into_owned();
                }
                // Resolve intermediate aliases (/var -> /private/var) so watcher paths
                // and scan paths agree. A selected symlink itself stays a symlink.
                if std::fs::symlink_metadata(&visible).is_ok_and(|m| m.file_type().is_symlink()) {
                    if let Some((parent, name)) = Path::new(&visible)
                        .parent()
                        .zip(Path::new(&visible).file_name())
                        && let Ok(real) = std::fs::canonicalize(parent)
                    {
                        return visible_path(&real.join(name).to_string_lossy());
                    }
                    return visible;
                }
                // Missing/offline roots still need their surviving /var or /tmp alias
                // resolved; a future mount/create event uses that physical spelling.
                for ancestor in Path::new(&visible).ancestors() {
                    if let Ok(real) = std::fs::canonicalize(ancestor) {
                        let suffix = Path::new(&visible).strip_prefix(ancestor).unwrap();
                        return visible_path(&real.join(suffix).to_string_lossy());
                    }
                }
                visible
            })())
        })
        .collect::<Option<_>>()?;
    let mapped_roots: Vec<_> = firmlinks()
        .iter()
        .filter(|link| {
            out.iter().any(|root| {
                root == SYSTEM_VOLUMES
                    || (Path::new(root).starts_with(DATA_ROOT)
                        && Path::new(&link.physical).starts_with(root))
            }) && same_firmlink_object(link)
        })
        .map(|link| link.logical.clone())
        .collect();
    out.extend(mapped_roots);
    compact_roots(out, cancelled)
}

fn excluded_namespace(path: &str) -> bool {
    path == "/dev" || path == "/Network" || path == "/net"
}

enum Message {
    Batch(Vec<ScannedFile>),
    Issue(String, i32),
    Done,
}
struct Work {
    dirs: VecDeque<String>,
    outstanding: usize,
}
struct CollectContext<'a> {
    directory: &'a str,
    cancelled: &'a AtomicBool,
    tx: &'a SyncSender<Message>,
    excluded: &'a [String],
    configured: &'a [String],
    entries: Vec<ScannedFile>,
    children: Vec<String>,
}
fn collect_entry(
    context: &mut CollectContext<'_>,
    name_bytes: &[u8],
    entry: &FileMetadata,
) -> ControlFlow<()> {
    if context.cancelled.load(Ordering::Relaxed) {
        return ControlFlow::Break(());
    }
    let Ok(name) = std::str::from_utf8(name_bytes) else {
        let _ = context.tx.send(Message::Issue(
            format!("{}/<non-UTF8 directory entry>", context.directory),
            84,
        ));
        return ControlFlow::Continue(());
    };
    let path = Path::new(context.directory)
        .join(name)
        .to_string_lossy()
        .into_owned();
    if excluded_namespace(&path)
        || !path_in_namespace(&path, context.configured)
        || context
            .excluded
            .iter()
            .any(|root| Path::new(&path).starts_with(root))
    {
        return ControlFlow::Continue(());
    }
    if let Some(error) = entry.error {
        let _ = context.tx.send(Message::Issue(path, error));
        return ControlFlow::Continue(());
    }
    let file = from_metadata(path.clone(), entry);
    if file.is_dir && !file.is_symlink && descend_in_namespace(&path, context.configured) {
        context.children.push(path);
    }
    context.entries.push(file);
    if context.entries.len() >= 512
        && context
            .tx
            .send(Message::Batch(std::mem::take(&mut context.entries)))
            .is_err()
    {
        return ControlFlow::Break(());
    }
    ControlFlow::Continue(())
}

/// Bounded four-worker scan. Batches are streamed through a bounded channel;
/// memory does not grow with the number of files already scanned. The supplied
/// roots themselves are included. Calling with [] performs no implicit scan.
pub fn scan(
    roots: &[String],
    cancelled: &AtomicBool,
    on_batch: impl FnMut(Vec<ScannedFile>),
) -> ScanReport {
    scan_excluding(roots, &[], cancelled, on_batch)
}
fn enumeration_scope(
    roots: &[String],
    configured: &[String],
    excluded: &[String],
) -> (Vec<String>, Vec<String>) {
    let mut traversal_roots = Vec::<String>::new();
    let mut traversal_scopes = PathScopes::default();
    for root in roots {
        if !traversal_scopes.covers_where(Path::new(root), |parent| {
            !(Path::new(root).starts_with(SYSTEM_VOLUMES)
                && Path::new(SYSTEM_VOLUMES).starts_with(parent)
                && !descend_in_namespace(SYSTEM_VOLUMES, configured))
        }) {
            traversal_scopes.insert(Path::new(root));
            traversal_roots.push(root.clone());
        }
    }
    if roots.iter().any(|root| root == "/")
        && !descend_in_namespace(SYSTEM_VOLUMES, configured)
        && std::fs::symlink_metadata(DATA_ROOT).is_ok()
    {
        traversal_roots.push(DATA_ROOT.into());
    }
    let mut excluded = normalize_scopes(excluded);
    excluded.extend(
        firmlinks()
            .iter()
            .filter(|link| {
                roots
                    .iter()
                    .any(|root| Path::new(&link.logical).starts_with(root))
                    && same_firmlink_object(link)
            })
            .map(|link| link.physical.clone()),
    );
    (traversal_roots, excluded)
}
/// Prune application-owned index/cache subtrees before enumeration. Filtering
/// emitted rows alone would still traverse them and react to our own writes.
pub fn scan_excluding(
    roots: &[String],
    excluded: &[String],
    cancelled: &AtomicBool,
    on_batch: impl FnMut(Vec<ScannedFile>),
) -> ScanReport {
    let Some(roots) = normalize_roots_until(roots, || cancelled.load(Ordering::Relaxed)) else {
        return ScanReport {
            roots: roots.to_vec(),
            cancelled: true,
            ..Default::default()
        };
    };
    scan_excluding_in_namespace(&roots, &roots, excluded, cancelled, on_batch)
}
/// Reconciliation scopes are derived from events; namespace authorization stays
/// bound to the user's configured roots rather than those temporary scopes.
pub(crate) fn scan_excluding_in_namespace(
    roots: &[String],
    configured: &[String],
    excluded: &[String],
    cancelled: &AtomicBool,
    on_batch: impl FnMut(Vec<ScannedFile>),
) -> ScanReport {
    let scope = match mount_scope() {
        Ok(scope) => scope,
        Err(error) => {
            return ScanReport {
                roots: roots.to_vec(),
                uncovered: roots.to_vec(),
                errors: vec![format!("Could not inspect mounted filesystems: {error}")],
                ..Default::default()
            };
        }
    };
    scan_excluding_with_scope(roots, configured, excluded, cancelled, &scope, on_batch)
}
fn scan_excluding_with_scope(
    roots: &[String],
    configured: &[String],
    excluded: &[String],
    cancelled: &AtomicBool,
    mount_scope: &filesystem::MountScope,
    mut on_batch: impl FnMut(Vec<ScannedFile>),
) -> ScanReport {
    let Some(roots) = normalize_scopes_until(roots, || cancelled.load(Ordering::Relaxed)) else {
        return ScanReport {
            roots: roots.to_vec(),
            cancelled: true,
            ..Default::default()
        };
    };
    let configured = normalize_scopes(configured);
    let configured = configured.as_slice();
    let selected = PathScopes::from_paths(configured);
    let roots: Vec<_> = roots
        .into_iter()
        .filter(|root| selected.covers(Path::new(root)) && path_in_namespace(root, configured))
        .collect();
    let (traversal_roots, excluded) = enumeration_scope(&roots, configured, excluded);
    let mut report = ScanReport {
        roots: roots.clone(),
        excluded_mounts: mount_scope.excluded_within(&traversal_roots),
        ..Default::default()
    };
    let mut dirs = VecDeque::new();
    let mut first = Vec::new();
    for root in traversal_roots {
        if !path_in_namespace(&root, configured) || !mount_scope.allows(Path::new(&root)) {
            continue;
        }
        if excluded
            .iter()
            .any(|path| Path::new(&root).starts_with(path))
        {
            continue;
        }
        match filesystem::stat_entry(Path::new(&root), mount_scope)
            .map(|entry| from_metadata(root.clone(), &entry))
        {
            Ok(e) => {
                if e.is_dir && !e.is_symlink && descend_in_namespace(&root, configured) {
                    dirs.push_back(root);
                }
                first.push(e);
            }
            Err(e) => {
                report.uncovered.push(root);
                report.errors.push(e.to_string());
            }
        }
    }
    if !first.is_empty() {
        report.entries += first.len() as u64;
        on_batch(first);
    }
    let work = Arc::new((
        Mutex::new(Work {
            outstanding: dirs.len(),
            dirs,
        }),
        Condvar::new(),
    ));
    let worker_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .clamp(1, 2);
    let (tx, rx) = sync_channel::<Message>(16);
    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            let work = Arc::clone(&work);
            let tx = tx.clone();
            let excluded = &excluded;
            scope.spawn(move || {
                loop {
                    let path = {
                        let (lock, cv) = &*work;
                        let mut queue = lock.lock().unwrap();
                        loop {
                            if cancelled.load(Ordering::Relaxed) || queue.outstanding == 0 {
                                break None;
                            }
                            if let Some(dir) = queue.dirs.pop_back() {
                                break Some(dir);
                            }
                            queue = cv.wait(queue).unwrap();
                        }
                    };
                    let Some(path) = path else {
                        break;
                    };
                    let mut ctx = CollectContext {
                        directory: &path,
                        cancelled,
                        tx: &tx,
                        excluded,
                        configured,
                        entries: Vec::with_capacity(512),
                        children: Vec::new(),
                    };
                    let code =
                        filesystem::scan_directory(Path::new(&path), mount_scope, |name, entry| {
                            collect_entry(&mut ctx, name, entry)
                        })
                        .err()
                        .map(|error| error.raw_os_error().unwrap_or(libc::EIO))
                        .unwrap_or(0);
                    if !ctx.entries.is_empty() {
                        let _ = tx.send(Message::Batch(ctx.entries));
                    }
                    if code != 0 && code != 89 {
                        let _ = tx.send(Message::Issue(path.clone(), code));
                    }
                    let (lock, cv) = &*work;
                    let mut queue = lock.lock().unwrap();
                    queue.outstanding -= 1;
                    queue.outstanding += ctx.children.len();
                    queue.dirs.extend(ctx.children);
                    cv.notify_all();
                }
                let _ = tx.send(Message::Done);
            });
        }
        drop(tx);
        let mut done = 0;
        // Directory boundaries are not transaction boundaries. Tiny directories
        // otherwise multiply mount preparation, alias planning and commits.
        // Drain only records already available; an empty queue flushes promptly
        // without waiting for a timer or an unrelated directory to finish.
        const COMMIT_ROWS: usize = 512;
        let mut pending = Vec::with_capacity(COMMIT_ROWS);
        loop {
            let next = if pending.is_empty() {
                rx.recv().ok()
            } else {
                match rx.try_recv() {
                    Ok(message) => Some(message),
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        on_batch(std::mem::replace(
                            &mut pending,
                            Vec::with_capacity(COMMIT_ROWS),
                        ));
                        continue;
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => None,
                }
            };
            let Some(message) = next else {
                break;
            };
            match message {
                Message::Batch(entries) => {
                    report.entries += entries.len() as u64;
                    for entry in entries {
                        pending.push(entry);
                        if pending.len() == COMMIT_ROWS {
                            on_batch(std::mem::replace(
                                &mut pending,
                                Vec::with_capacity(COMMIT_ROWS),
                            ));
                        }
                    }
                }
                Message::Issue(path, code) => {
                    report.uncovered.push(path.clone());
                    report.errors.push(format!(
                        "{}: {}",
                        path,
                        std::io::Error::from_raw_os_error(code)
                    ));
                }
                Message::Done => {
                    done += 1;
                    if done == worker_count {
                        break;
                    }
                }
            }
        }
        if !pending.is_empty() {
            on_batch(pending);
        }
    });
    report.cancelled = cancelled.load(Ordering::Relaxed);
    report.uncovered.sort();
    report.uncovered.dedup();
    report
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ChangeEvent {
    pub path: String,
    pub event_id: u64,
    pub flags: u32,
    pub must_rescan: bool,
    pub root_changed: bool,
}
impl ChangeEvent {
    pub fn from_flags(path: &str, event_id: u64, flags: u32) -> Self {
        // MustScanSubDirs, UserDropped, KernelDropped, EventIdsWrapped,
        // RootChanged, Mount and Unmount all invalidate ordinary delta replay.
        Self {
            path: visible_path(&filesystem::system_path(Path::new(path)).to_string_lossy()),
            event_id,
            flags,
            must_rescan: flags & (0x01 | 0x02 | 0x04 | 0x08 | 0x20 | 0x40 | 0x80) != 0,
            root_changed: flags & (0x20 | 0x40 | 0x80) != 0,
        }
    }
}
struct WatchContext {
    events: Mutex<VecDeque<ChangeEvent>>,
    roots: Vec<String>,
}
fn collect_event(ctx: &WatchContext, path: &str, id: u64, flags: u32) {
    // HistoryDone is a control event whose path is explicitly undefined by the
    // API. Preserve it before namespace normalization/filtering.
    if flags & (file_events::EVENT_HISTORY_DONE | file_events::GLOBAL_HISTORY_INVALIDATION) != 0 {
        ctx.events
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push_back(ChangeEvent::from_flags("/", id, flags));
        return;
    }
    let path = visible_path(&filesystem::system_path(Path::new(path)).to_string_lossy());
    // Disk Arbitration callbacks include all mounts: only invalidate roots
    // overlapping the configured namespace, never expand a scoped scan.
    if !ctx
        .roots
        .iter()
        .any(|r| Path::new(&path).starts_with(r) || Path::new(r).starts_with(&path))
    {
        return;
    }
    let mut queue = ctx.events.lock().unwrap_or_else(|error| error.into_inner());
    if queue.len() >= 65_536 {
        let controls = queue.iter().fold(
            flags & file_events::GLOBAL_HISTORY_INVALIDATION,
            |combined, event| combined | event.flags & file_events::GLOBAL_HISTORY_INVALIDATION,
        );
        queue.clear();
        for root in &ctx.roots {
            queue.push_back(ChangeEvent::from_flags(root, id, 0x03 | controls));
        }
    } else {
        queue.push_back(ChangeEvent::from_flags(&path, id, flags));
    }
}
/// Owns a serial macOS FSEvents dispatch queue and Disk Arbitration callbacks.
/// Can move between threads but must not be dropped from its callback thread.
pub struct Watcher {
    platform_watcher: file_events::Watcher,
    context: Arc<WatchContext>,
}
impl Watcher {
    pub fn start(roots: &[String], since_id: u64) -> Result<Self, String> {
        let roots = normalize_scopes(roots);
        let scope = mount_scope().map_err(|error| error.to_string())?;
        if roots.is_empty() {
            return Err("No roots selected for watching".into());
        }
        // A previously selected volume/folder can be offline at launch. Watch
        // its closest available ancestor for recreation, with callback filtering
        // still limited to the original roots. Disk Arbitration also wakes us on
        // a real mount event. Explicit symlinks are watched via their parent.
        let mut watched = Vec::new();
        let is_directory = |path: &Path| {
            filesystem::metadata(path, &scope)
                .is_ok_and(|metadata| metadata.st_mode & libc::S_IFMT == libc::S_IFDIR)
        };
        for root in &roots {
            let start = if is_directory(Path::new(root)) {
                Path::new(root)
            } else {
                Path::new(root).parent().unwrap_or(Path::new("/"))
            };
            let existing = start
                .ancestors()
                .find(|p| is_directory(p))
                .unwrap_or(Path::new("/"));
            watched.push(existing.to_string_lossy().into_owned());
        }
        let watched = normalize_scopes(&watched);
        let context = Arc::new(WatchContext {
            events: Mutex::new(VecDeque::new()),
            roots,
        });
        let callback_context = Arc::clone(&context);
        let platform_watcher =
            file_events::Watcher::start(&watched, since_id, move |path, id, flags| {
                collect_event(&callback_context, path, id, flags);
            })
            .map_err(|error| format!("FSEvents stream could not start: {error}"))?;
        Ok(Self {
            platform_watcher,
            context,
        })
    }
    pub fn drain(&self) -> Vec<ChangeEvent> {
        if self.platform_watcher.take_callback_failure() {
            for root in &self.context.roots {
                collect_event(&self.context, root, 0, 0x03);
            }
        }
        self.context
            .events
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .drain(..)
            .collect()
    }
    pub fn current_event_id() -> u64 {
        file_events::current_event_id()
    }
}

/// File-event paths identify the changed entry itself. Ascending to its parent
/// and recursively scanning it turns a /Users metadata event into a / rescan.
/// Pure directory metadata events only stat that directory; created/renamed
/// directories reconcile their subtree. Inode metadata first checks coverage and
/// current access, since ordinary child changes also modify directory timestamps.
#[derive(Debug, Default, PartialEq)]
pub struct Reconciliation {
    pub recursive: Vec<String>,
    pub metadata: Vec<String>,
    pub directory_checks: Vec<String>,
}
pub fn reconciliation_plan(roots: &[String], events: &[ChangeEvent]) -> Reconciliation {
    reconciliation_plan_with_history(roots, events, None)
}
/// The optional cursor is the fixed stream-start anchor of a completed baseline
/// whose mount namespace has been revalidated. FullHistory can replay mount
/// controls from long before that anchor (including the boot-time mount of /).
/// Only pure controls covered by that proof are redundant; ordinary older-ID
/// events, mixed flags, new mounts and Disk Arbitration's ID-zero controls stay.
/// Callers must stop supplying the anchor when historical replay finishes.
pub(crate) fn reconciliation_plan_with_history(
    roots: &[String],
    events: &[ChangeEvent],
    completed_mount_cursor: Option<u64>,
) -> Reconciliation {
    const HISTORY_DONE: u32 = 0x10;
    const ITEM_IS_DIR: u32 = 0x20000;
    const SUBTREE_CHANGE: u32 = 0x100 | 0x200 | 0x800 | 0x4000 | 0x8000;
    const INODE_METADATA: u32 = 0x400;
    let configured = normalize_scopes(roots);
    let mut recursive = HashSet::new();
    let mut metadata = HashSet::new();
    let mut directory_checks = HashSet::new();
    for event in events {
        if event.flags & HISTORY_DONE != 0 {
            continue;
        }
        if completed_mount_cursor.is_some_and(|cursor| {
            event.event_id != 0
                && event.event_id <= cursor
                && matches!(event.flags, 0x40 | 0x80 | 0xc0)
        }) {
            continue;
        }
        // Dropped/wrapped streams lose their global ordering guarantee. A
        // normal coalesced subtree event does not: only its covered subtree
        // needs enumeration, otherwise any historical change would rescan /.
        if event.flags & (0x02 | 0x04 | 0x08) != 0 {
            recursive.extend(configured.iter().cloned());
            continue;
        }
        if !path_in_namespace(&event.path, &configured) {
            continue;
        }
        // A broad change to the hidden-volume container cannot grant access to
        // its auxiliary children. The default / namespace still includes Data.
        if event.path == SYSTEM_VOLUMES
            && !descend_in_namespace(SYSTEM_VOLUMES, &configured)
            && (event.must_rescan || event.flags & SUBTREE_CHANGE != 0)
        {
            if configured
                .iter()
                .any(|root| Path::new(SYSTEM_VOLUMES).starts_with(root))
            {
                metadata.insert(event.path.clone());
                if path_in_namespace(DATA_ROOT, &configured) {
                    recursive.extend(normalize_scopes(&[DATA_ROOT.into()]));
                }
            }
            // Ancestor notifications also apply to explicitly selected auxiliary
            // subdirectories, without admitting the ancestor as an extra result.
            recursive.extend(
                configured
                    .iter()
                    .filter(|root| Path::new(root).starts_with(SYSTEM_VOLUMES))
                    .cloned(),
            );
            continue;
        }
        for root in &configured {
            let event_path = Path::new(&event.path);
            if event.must_rescan {
                if event_path.starts_with(root) {
                    recursive.insert(event.path.clone());
                } else if Path::new(root).starts_with(event_path) {
                    recursive.insert(root.clone());
                }
            } else if event_path.starts_with(root) {
                // Creating, removing or renaming a child changes its parent's
                // timestamps too. FileEvents does not guarantee a separate
                // directory event; stat the parent without scanning its tree.
                if let Some(parent) = event_path
                    .parent()
                    .filter(|parent| parent.starts_with(root))
                {
                    metadata.insert(parent.to_string_lossy().into_owned());
                }
                if event.flags & ITEM_IS_DIR != 0 && event.flags & SUBTREE_CHANGE == 0 {
                    if event.flags & INODE_METADATA != 0 {
                        directory_checks.insert(event.path.clone());
                    } else {
                        metadata.insert(event.path.clone());
                    }
                } else {
                    recursive.insert(event.path.clone());
                }
            } else if Path::new(root).starts_with(event_path) {
                // Missing/replaced roots are watched through an existing
                // ancestor. Renaming that ancestor can restore an entire tree
                // without producing a separate event for every descendant.
                if event.flags & SUBTREE_CHANGE != 0 {
                    recursive.insert(root.clone());
                } else if event.flags & INODE_METADATA != 0 {
                    directory_checks.insert(root.clone());
                }
            }
        }
    }
    let recursive = normalize_scopes(&recursive.into_iter().collect::<Vec<_>>());
    let recursive_scopes = PathScopes::from_paths(&recursive);
    // Keep every metadata entry, even when another metadata path is its parent.
    let mut metadata: Vec<_> = metadata
        .into_iter()
        .filter(|path| !recursive_scopes.covers(Path::new(path)))
        .collect();
    metadata.sort();
    let mut directory_checks: Vec<_> = directory_checks
        .into_iter()
        .filter(|path| !recursive_scopes.covers(Path::new(path)))
        .collect();
    directory_checks.sort();
    Reconciliation {
        recursive,
        metadata,
        directory_checks,
    }
}
/// Compatibility helper for callers interested only in recursive invalidations.
pub fn reconciliation_roots(roots: &[String], events: &[ChangeEvent]) -> Vec<String> {
    reconciliation_plan(roots, events).recursive
}

pub fn scan_metadata(
    paths: &[String],
    cancelled: &AtomicBool,
    on_batch: impl FnMut(Vec<ScannedFile>),
) -> ScanReport {
    scan_metadata_in_namespace(paths, paths, cancelled, on_batch)
}
pub(crate) fn scan_metadata_in_namespace(
    paths: &[String],
    configured: &[String],
    cancelled: &AtomicBool,
    mut on_batch: impl FnMut(Vec<ScannedFile>),
) -> ScanReport {
    // Metadata siblings and descendants each need a stat; recursive-scope
    // compaction would silently omit a changed child when its parent is present.
    let mut paths: Vec<_> = paths
        .iter()
        .map(|path| visible_path(&filesystem::system_path(Path::new(path)).to_string_lossy()))
        .collect();
    paths.sort();
    paths.dedup();
    let configured = normalize_scopes(configured);
    let selected = PathScopes::from_paths(&configured);
    let paths: Vec<_> = paths
        .into_iter()
        .filter(|path| selected.covers(Path::new(path)) && path_in_namespace(path, &configured))
        .collect();
    let mut report = ScanReport {
        roots: paths.to_vec(),
        ..Default::default()
    };
    let scope = match mount_scope() {
        Ok(scope) => scope,
        Err(error) => {
            report.uncovered = paths.to_vec();
            report.errors.push(error.to_string());
            return report;
        }
    };
    report.excluded_mounts = scope.excluded_within(&paths);
    let mut reader = filesystem::MetadataReader::new(&scope);
    let mut batch = Vec::new();
    for path in &paths {
        if !scope.allows(Path::new(path)) {
            continue;
        }
        if cancelled.load(Ordering::Relaxed) {
            report.cancelled = true;
            break;
        }
        match reader
            .stat(Path::new(path))
            .map(|metadata| from_metadata(path.clone(), &metadata))
        {
            Ok(entry) => {
                report.entries += 1;
                batch.push(entry);
            }
            Err(error) => {
                report.uncovered.push(path.clone());
                report.errors.push(format!("{path}: {error}"));
            }
        }
        if batch.len() >= 512 {
            on_batch(std::mem::take(&mut batch));
        }
    }
    if !batch.is_empty() {
        on_batch(batch);
    }
    report
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    #[test]
    fn mount_scope_skips_subtrees_and_preserves_old_database_rows_as_inaccessible() {
        let fixture = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(fixture.path()).unwrap();
        let remote = root.join("remote");
        std::fs::create_dir(&remote).unwrap();
        std::fs::write(remote.join("old-local.txt"), "retained").unwrap();
        std::fs::write(root.join("visible.txt"), "indexed").unwrap();
        let roots = vec![root.to_string_lossy().into_owned()];
        let unrestricted = filesystem::MountScope::from_mounts(vec![(PathBuf::from("/"), true)]);
        let mut store =
            crate::index_store::IndexStore::open(&fixture.path().join("db/index.sqlite")).unwrap();
        let mut original = Vec::new();
        scan_excluding_with_scope(
            &roots,
            &roots,
            &[root.join("db").to_string_lossy().into_owned()],
            &AtomicBool::new(false),
            &unrestricted,
            |batch| original.extend(batch),
        );
        store.batch(&original, 1).unwrap();
        let scope = filesystem::MountScope::from_mounts(vec![
            (PathBuf::from("/"), true),
            (remote.clone(), false),
        ]);
        let mut current = Vec::new();
        let report = scan_excluding_with_scope(
            &roots,
            &roots,
            &[root.join("db").to_string_lossy().into_owned()],
            &AtomicBool::new(false),
            &scope,
            |batch| current.extend(batch),
        );
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.uncovered.is_empty());
        assert_eq!(
            report.excluded_mounts,
            vec![remote.to_string_lossy().into_owned()]
        );
        assert!(
            current
                .iter()
                .all(|file| !Path::new(&file.path).starts_with(&remote))
        );
        store.batch(&current, 2).unwrap();
        store.finish(&roots, &report.excluded_mounts, 2, 0).unwrap();
        let old_path = remote.join("old-local.txt").to_string_lossy().into_owned();
        assert!(
            !store
                .connection
                .query_row(
                    "SELECT accessible FROM files WHERE path=?1",
                    [&old_path],
                    |row| row.get::<_, bool>(0)
                )
                .unwrap()
        );
        assert!(
            store
                .entries()
                .unwrap()
                .iter()
                .all(|file| file.path != old_path)
        );
        // The same indexed identity becomes available when the local mount
        // returns; it was never deleted by the scope exclusion.
        store.batch(&original, 3).unwrap();
        assert!(
            store
                .entries()
                .unwrap()
                .iter()
                .any(|file| file.path == old_path)
        );
    }
    use std::{
        collections::BTreeMap,
        fs,
        os::unix::fs::{MetadataExt, PermissionsExt, symlink},
        time::{Duration, Instant},
    };
    fn collect(root: &Path) -> (BTreeMap<String, ScannedFile>, ScanReport) {
        let mut entries = BTreeMap::new();
        let report = scan(
            &[root.to_string_lossy().into_owned()],
            &AtomicBool::new(false),
            |batch| {
                for e in batch {
                    assert!(
                        entries.insert(e.path.clone(), e).is_none(),
                        "duplicate path"
                    );
                }
            },
        );
        (entries, report)
    }
    #[test]
    fn bulk_scan_preserves_hardlinks_unicode_and_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::create_dir(root.join("目录")).unwrap();
        fs::write(root.join("目录/报告é😀.TXT"), b"hello bulk APFS").unwrap();
        fs::hard_link(root.join("目录/报告é😀.TXT"), root.join("hardlink.txt")).unwrap();
        symlink(root.join("目录"), root.join("alias")).unwrap();
        fs::write(root.join(".hidden"), []).unwrap();
        fs::create_dir(root.join("Example.app")).unwrap();
        fs::write(root.join("Example.app/internal"), "inside").unwrap();
        let (entries, report) = collect(&root);
        assert_eq!(report.entries, 8);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let first = entries
            .get(&root.join("目录/报告é😀.TXT").to_string_lossy().into_owned())
            .unwrap();
        let link = entries
            .get(&root.join("hardlink.txt").to_string_lossy().into_owned())
            .unwrap();
        assert_eq!(first.file_id, link.file_id);
        assert_eq!(first.volume_id, link.volume_id);
        assert_eq!(first.size, 15);
        assert_eq!(first.extension, "txt");
        assert_eq!(
            first.file_id,
            fs::symlink_metadata(&first.path).unwrap().ino()
        );
        assert!(first.created > 0);
        assert!(!first.volume_id.is_empty());
        assert_eq!(first.modified_ns / 1_000_000_000, first.modified);
        let actual = fs::symlink_metadata(&first.path).unwrap();
        assert_eq!(
            first.modified_ns,
            actual.mtime() * 1_000_000_000 + actual.mtime_nsec()
        );
        let alias = entries
            .get(&root.join("alias").to_string_lossy().into_owned())
            .unwrap();
        assert!(alias.is_symlink);
        assert!(!alias.is_dir);
        assert!(!entries.keys().any(|p| p.contains("/alias/")));
    }
    #[test]
    fn deep_scans_and_cancellation_are_bounded() {
        let temp = tempfile::tempdir().unwrap();
        for i in 0..32 {
            fs::create_dir(temp.path().join(format!("dir{i}"))).unwrap();
            for j in 0..70 {
                fs::write(temp.path().join(format!("dir{i}/file{j}.txt")), "data").unwrap();
            }
        }
        let cancel = AtomicBool::new(false);
        let mut seen = 0;
        let report = scan(
            &[temp.path().to_string_lossy().into_owned()],
            &cancel,
            |batch| {
                assert!(batch.len() <= 512);
                seen += batch.len();
                if seen >= 512 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
        );
        assert!(report.cancelled);
        assert!(seen < 2273);
        let (entries, report) = collect(temp.path());
        assert_eq!(entries.len(), 2273);
        assert!(report.errors.is_empty());
    }
    #[test]
    fn queued_directory_open_refuses_a_symlink_in_an_ancestor_component() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("outside/child")).unwrap();
        fs::write(root.join("outside/child/out-of-scope.txt"), "outside").unwrap();
        symlink(root.join("outside"), root.join("replaced-ancestor")).unwrap();
        let path = root.join("replaced-ancestor/child");
        let mut entries = 0usize;
        let error = filesystem::scan_directory(&path, &mount_scope().unwrap(), |_, _| {
            entries += 1;
            ControlFlow::Continue(())
        });
        assert!(error.is_err());
        assert_eq!(
            entries, 0,
            "A replaced ancestor must not redirect an already queued traversal"
        );
    }
    #[test]
    fn roots_deduplicate_and_data_alias_maps_once() {
        assert_eq!(
            normalize_roots(&[
                "/Users".into(),
                "/System/Volumes/Data/Users".into(),
                "/Users/example".into()
            ]),
            vec!["/Users"]
        );
        assert_eq!(
            visible_path("/System/Volumes/Data/Applications/Foo.app"),
            "/Applications/Foo.app"
        );
        assert_eq!(
            visible_path("/System/Volumes/Database/example"),
            "/System/Volumes/Database/example"
        );
    }
    #[test]
    fn data_only_paths_and_overlay_ancestors_are_not_erased_by_firmlinks() {
        for suffix in [
            "",
            "/usr",
            "/System",
            "/sw",
            "/.Spotlight-V100",
            "/MobileSoftwareUpdate",
            "/.PreviousSystemInformation",
        ] {
            let path = format!("{DATA_ROOT}{suffix}");
            assert_eq!(visible_path(&path), path);
        }
        let (traversed, excluded) = enumeration_scope(&["/".into()], &["/".into()], &[]);
        assert_eq!(traversed, vec!["/", DATA_ROOT]);
        assert!(!excluded.iter().any(|path| path == DATA_ROOT
            || path == &format!("{DATA_ROOT}/usr")
            || path == &format!("{DATA_ROOT}/System")));
        for link in firmlinks().iter().filter(|link| same_firmlink_object(link)) {
            assert!(excluded.contains(&link.physical));
            assert_eq!(visible_path(&link.physical), link.logical);
        }
        let physical_usr = format!("{DATA_ROOT}/usr");
        let selected = normalize_roots(std::slice::from_ref(&physical_usr));
        assert!(selected.contains(&physical_usr));
        for link in firmlinks().iter().filter(|link| {
            Path::new(&link.physical).starts_with(&physical_usr) && same_firmlink_object(link)
        }) {
            assert!(selected.contains(&link.logical));
        }
        assert!(
            !selected.contains(&"/usr".into()),
            "Data/usr is not the same object as /usr"
        );
    }
    #[test]
    fn data_only_live_directories_keep_physical_paths_and_match_independent_lstat() {
        use serde_json::json;
        let mut checked = Vec::new();
        for suffix in [
            "sw",
            "mnt",
            "MobileSoftwareUpdate",
            ".PreviousSystemInformation",
        ] {
            let path = format!("{DATA_ROOT}/{suffix}");
            if !Path::new(&path).is_dir() {
                continue;
            }
            let (entries, report) = collect(Path::new(&path));
            assert!(report.errors.is_empty(), "{report:?}");
            let mut independent = BTreeMap::new();
            let mut pending = vec![PathBuf::from(&path)];
            while let Some(next) = pending.pop() {
                let metadata = fs::symlink_metadata(&next).unwrap();
                independent.insert(
                    next.to_string_lossy().into_owned(),
                    (
                        metadata.ino(),
                        if metadata.is_dir() { 0 } else { metadata.len() },
                        metadata.mtime() * 1_000_000_000 + metadata.mtime_nsec(),
                    ),
                );
                if metadata.is_dir() && !metadata.file_type().is_symlink() {
                    for child in fs::read_dir(next).unwrap() {
                        pending.push(child.unwrap().path());
                    }
                }
            }
            let actual: BTreeMap<_, _> = entries
                .iter()
                .map(|(path, entry)| (path.clone(), (entry.file_id, entry.size, entry.modified_ns)))
                .collect();
            assert_eq!(actual, independent);
            assert!(
                actual
                    .keys()
                    .all(|candidate| candidate.starts_with(DATA_ROOT))
            );
            checked.push(json!({"root":path,"entries":actual.len(),"metadata_blake3":blake3::hash(&serde_json::to_vec(&actual).unwrap()).to_hex().to_string()}));
        }
        if let Ok(output) = std::env::var("APF_FIRMLINK_OUTPUT") {
            let report = json!({"scope":"Read-only metadata scan of bounded existing Data-only directories; independent recursive lstat comparison","data_only_roots":checked,"mapped_roots":firmlinks().iter().filter(|link|same_firmlink_object(link)).map(|link|json!({"physical":link.physical,"logical":link.logical})).collect::<Vec<_>>(),"default_root_traverses_data_overlay":true,"only_same_object_firmlink_targets_pruned":true,"full_root_completeness_proven":false});
            fs::write(output, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
        }
    }
    #[test]
    fn unavailable_root_is_reported_without_claiming_coverage() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("missing").to_string_lossy().into_owned();
        let report = scan(&[path], &AtomicBool::new(false), |_| {
            panic!("missing root produces no entries")
        });
        assert_eq!(report.entries, 0);
        assert_eq!(report.uncovered.len(), 1);
        assert_eq!(report.errors.len(), 1);
    }
    #[test]
    fn denied_directory_is_visible_and_not_removed_from_index() {
        let temp = tempfile::tempdir().unwrap();
        let blocked = temp.path().join("denied");
        fs::create_dir(&blocked).unwrap();
        fs::write(blocked.join("secret"), "metadata only").unwrap();
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
        let (_, report) = collect(temp.path());
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            report.uncovered.iter().any(|p| p.ends_with("/denied")),
            "{:?}",
            report
        );
    }
    #[test]
    fn directory_events_do_not_promote_a_small_change_to_the_root() {
        let roots = vec!["/".to_string()];
        let plan = reconciliation_plan(
            &roots,
            &[
                ChangeEvent::from_flags("/Users", 1, 0x20000 | 0x1000),
                ChangeEvent::from_flags("/Users/test/file.txt", 2, 0x10000 | 0x1000),
                ChangeEvent::from_flags("/Applications/New.app", 3, 0x20000 | 0x100),
                ChangeEvent::from_flags("/", 4, 0x10),
            ],
        );
        assert_eq!(
            plan.metadata,
            vec!["/", "/Applications", "/Users", "/Users/test"]
        );
        assert_eq!(
            plan.recursive,
            vec!["/Applications/New.app", "/Users/test/file.txt"]
        );
        let dropped =
            reconciliation_plan(&roots, &[ChangeEvent::from_flags("/Users/test", 5, 0x04)]);
        assert_eq!(dropped.recursive, roots);
    }
    #[test]
    fn journal_drop_and_rename_invalidation_are_conservative() {
        let roots = vec!["/Users/test/Documents".into()];
        let drop = ChangeEvent::from_flags("/Users/test/Documents/child", 12, 0x04);
        assert!(drop.must_rescan);
        assert_eq!(reconciliation_roots(&roots, &[drop]), roots);
        let rename = ChangeEvent::from_flags("/Users/test/Documents/old", 13, 0x800);
        assert_eq!(
            reconciliation_roots(&roots, &[rename]),
            vec!["/Users/test/Documents/old"]
        );
        let unrelated = ChangeEvent::from_flags("/Volumes/unrelated", 14, 0x20);
        assert!(reconciliation_roots(&roots, &[unrelated]).is_empty());
    }
    #[test]
    fn live_fsevents_capture_create_rename_delete_before_initial_scan() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let roots = vec![root.to_string_lossy().into_owned()];
        let watcher = Watcher::start(&roots, 0).unwrap();
        let first = root.join("原名.txt");
        let last = root.join("renamed.txt");
        fs::write(&first, "created after watch start").unwrap();
        let (_, initial) = collect(&root);
        assert!(initial.errors.is_empty());
        fs::rename(&first, &last).unwrap();
        fs::remove_file(&last).unwrap();
        let start = Instant::now();
        let mut events = Vec::new();
        while start.elapsed() < Duration::from_secs(5) {
            events.extend(watcher.drain());
            if events.iter().any(|e| e.path.ends_with("renamed.txt")) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            events.iter().any(|e| e.path.ends_with("renamed.txt")),
            "{:?}",
            events
        );
        assert!(!reconciliation_plan(&roots, &events).recursive.is_empty());
        let (final_entries, final_report) = collect(&root);
        assert_eq!(final_entries.len(), 1);
        assert!(final_report.errors.is_empty());
    }
    #[test]
    fn same_size_same_second_rewrite_changes_nanosecond_identity() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("text.txt");
        // Set explicit mtime instants to make the subsecond case deterministic.
        let epoch = std::time::UNIX_EPOCH + Duration::from_secs(1_780_000_000);
        fs::write(&file, "before").unwrap();
        let handle = fs::OpenOptions::new().write(true).open(&file).unwrap();
        handle
            .set_times(fs::FileTimes::new().set_modified(epoch + Duration::from_nanos(100)))
            .unwrap();
        let before = stat_entry(&file.to_string_lossy()).unwrap();
        fs::write(&file, "after!").unwrap();
        handle
            .set_times(fs::FileTimes::new().set_modified(epoch + Duration::from_nanos(200)))
            .unwrap();
        let after = stat_entry(&file.to_string_lossy()).unwrap();
        assert_eq!(before.size, after.size);
        assert_eq!(before.modified, after.modified);
        assert_ne!(before.modified_ns, after.modified_ns);
        let (entries, report) = collect(temp.path());
        assert!(report.errors.is_empty());
        let bulk = entries.values().find(|e| e.name == "text.txt").unwrap();
        assert_eq!(bulk.modified_ns, after.modified_ns);
    }

    #[test]
    fn watcher_recovers_initially_missing_scoped_root() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().canonicalize().unwrap();
        let root = parent.join("unavailable");
        let roots = vec![root.to_string_lossy().into_owned()];
        let watcher = Watcher::start(&roots, 0).unwrap();
        let missing = scan(&roots, &AtomicBool::new(false), |_| {});
        assert_eq!(missing.uncovered.len(), 1);
        fs::create_dir(&root).unwrap();
        fs::write(root.join("back.txt"), "mounted again").unwrap();
        fs::write(parent.join("unrelated.txt"), "must not expand scope").unwrap();
        let start = Instant::now();
        let mut events = Vec::new();
        while start.elapsed() < Duration::from_secs(5) {
            events.extend(watcher.drain());
            if events.iter().any(|e| e.path.ends_with("back.txt")) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            events.iter().any(|e| e.path.ends_with("back.txt")),
            "{:?}",
            events
        );
        assert!(!events.iter().any(|e| e.path.ends_with("unrelated.txt")));
        assert!(!reconciliation_plan(&roots, &events).recursive.is_empty());
        let (entries, report) = collect(&root);
        assert_eq!(entries.len(), 2);
        assert!(report.errors.is_empty());
    }

    #[test]
    fn bulk_zero_and_nonzero_sizes_match_lstat_for_every_entry() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        for (name, size) in [
            ("zero.txt", 0),
            ("one", 1),
            ("four.bin", 4),
            ("large.bin", 65_537),
            ("空文件报告.txt", 0),
        ] {
            fs::write(root.join(name), vec![b'x'; size]).unwrap();
        }
        fs::create_dir(root.join("empty-directory")).unwrap();
        symlink("zero.txt", root.join("symlink")).unwrap();
        let (entries, report) = collect(&root);
        assert!(report.errors.is_empty(), "{:?}", report);
        assert_eq!(entries.len(), 8);
        for entry in entries.values() {
            let real = fs::symlink_metadata(&entry.path).unwrap();
            assert_eq!(
                entry.size,
                if real.is_dir() { 0 } else { real.len() },
                "{}",
                entry.path
            );
            assert_eq!(entry.file_id, real.ino(), "{}", entry.path);
            assert_eq!(
                entry.modified_ns,
                real.mtime() * 1_000_000_000 + real.mtime_nsec()
            );
        }
    }
    #[test]
    fn history_done_is_retained_without_interpreting_its_undefined_path() {
        let context = WatchContext {
            roots: vec!["/some/scoped/root".into()],
            events: Mutex::new(VecDeque::new()),
        };
        collect_event(&context, "/outside/undefined/path", 123, 0x10);
        let events: Vec<_> = context.events.lock().unwrap().drain(..).collect();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].flags, 0x10);
        assert_eq!(
            reconciliation_plan(&context.roots, &events),
            Reconciliation::default()
        );
    }
    #[test]
    fn coalesced_history_and_mounts_rescan_their_subtree_but_drops_rescan_all_roots() {
        let roots = vec!["/fixture".into(), "/other".into()];
        for flags in [0x01, 0x21, 0x40, 0x80] {
            let plan = reconciliation_plan(
                &roots,
                &[ChangeEvent::from_flags("/fixture/changed", 42, flags)],
            );
            assert_eq!(plan.recursive, vec!["/fixture/changed"], "flags={flags}");
        }
        for flags in [0x02, 0x04, 0x08] {
            let plan = reconciliation_plan(
                &roots,
                &[ChangeEvent::from_flags("/fixture/changed", 42, flags)],
            );
            assert_eq!(plan.recursive, roots, "flags={flags}");
        }
    }
    #[test]
    fn boot_namespace_is_shared_by_enumeration_and_event_scopes() {
        let configured = vec!["/".into()];
        let auxiliary = "/System/Volumes/Preboot";
        assert!(path_in_namespace(DATA_ROOT, &configured));
        assert!(!path_in_namespace(auxiliary, &configured));
        assert!(!path_in_namespace(DATA_ROOT, &["/System".into()]));
        assert!(path_in_namespace(auxiliary, &[SYSTEM_VOLUMES.into()]));
        assert!(path_in_namespace(auxiliary, &[auxiliary.into()]));
        assert_eq!(
            normalize_roots(&["/".into(), auxiliary.into()]),
            ["/", auxiliary]
        );
        assert_eq!(
            normalize_roots(&["/".into(), SYSTEM_VOLUMES.into()]),
            ["/", SYSTEM_VOLUMES]
        );
        let explicit_container = normalize_roots(&["/".into(), SYSTEM_VOLUMES.into()]);
        assert_eq!(
            enumeration_scope(&explicit_container, &explicit_container, &[]).0,
            ["/"],
            "explicit overlap traverses each tree once"
        );
        let temporary = tempfile::tempdir().unwrap();
        let entry = filesystem::stat_entry(temporary.path(), &mount_scope().unwrap()).unwrap();
        let (tx, _rx) = sync_channel(1);
        let token = AtomicBool::new(false);
        let mut context = CollectContext {
            directory: "/System",
            cancelled: &token,
            tx: &tx,
            excluded: &[],
            configured: &configured,
            entries: Vec::new(),
            children: Vec::new(),
        };
        assert_eq!(
            collect_entry(&mut context, b"Volumes", &entry),
            ControlFlow::Continue(())
        );
        assert_eq!(
            context.entries.len(),
            1,
            "the container itself remains indexed"
        );
        assert!(
            context.children.is_empty(),
            "default baseline does not enumerate hidden volumes"
        );
        context.directory = SYSTEM_VOLUMES;
        context.entries.clear();
        let _ = collect_entry(&mut context, b"Preboot", &entry);
        assert!(context.entries.is_empty());
        let _ = collect_entry(&mut context, b"Data", &entry);
        assert_eq!(context.entries[0].path, DATA_ROOT);
        for flags in [0x40, 0x80, 0x20100, 0x20400, 0x28000] {
            let events = [ChangeEvent::from_flags(auxiliary, 42, flags)];
            assert_eq!(
                reconciliation_plan(&configured, &events),
                Reconciliation::default()
            );
            let explicit = reconciliation_plan(&[auxiliary.into()], &events);
            assert!(!explicit.recursive.is_empty() || !explicit.directory_checks.is_empty());
        }
        for flags in [0x02, 0x04, 0x08] {
            assert_eq!(
                reconciliation_plan(
                    &configured,
                    &[ChangeEvent::from_flags(auxiliary, 42, flags)]
                )
                .recursive,
                configured,
                "namespace policy never swallows global history loss"
            );
        }
        let broad = [ChangeEvent::from_flags(SYSTEM_VOLUMES, 42, 0x01)];
        assert_eq!(
            reconciliation_plan(&["/Users/example".into()], &broad),
            Reconciliation::default()
        );
        let chosen = "/System/Volumes/Preboot/chosen";
        let scoped = reconciliation_plan(&[chosen.into()], &broad);
        assert_eq!(scoped.recursive, [chosen]);
        assert!(scoped.metadata.is_empty());
        let combined = reconciliation_plan(&["/".into(), chosen.into()], &broad);
        assert!(combined.recursive.contains(&chosen.into()));
        assert!(combined.metadata.contains(&SYSTEM_VOLUMES.into()));
        assert!(path_in_namespace(auxiliary, &["/".into(), chosen.into()]));
        assert!(!subtree_in_namespace(
            auxiliary,
            &["/".into(), chosen.into()]
        ));
        assert!(subtree_in_namespace(chosen, &["/".into(), chosen.into()]));
        let plan = reconciliation_plan(
            &configured,
            &[ChangeEvent::from_flags(SYSTEM_VOLUMES, 42, 0x01)],
        );
        assert!(
            plan.recursive
                .iter()
                .all(|path| path_in_namespace(path, &configured))
        );
        assert!(!plan.recursive.contains(&"/".into()));
        assert!(!plan.recursive.contains(&SYSTEM_VOLUMES.into()));
    }
    #[test]
    fn explicit_volume_container_uses_the_same_firmlink_namespace_for_scan_and_events() {
        let roots = normalize_roots(&[SYSTEM_VOLUMES.into()]);
        assert!(roots.contains(&SYSTEM_VOLUMES.into()));
        let link = firmlinks()
            .iter()
            .find(|link| link.logical == "/Users" && same_firmlink_object(link))
            .expect("this APFS host has the verified Users firmlink");
        assert!(
            roots
                .iter()
                .any(|root| Path::new(&link.logical).starts_with(root))
        );
        let (_, excluded) = enumeration_scope(&roots, &roots, &[]);
        assert!(
            excluded.contains(&link.physical),
            "baseline must not emit the physical alias too"
        );
        let suffix = "/example/apfsearch-identity-probe.txt";
        let logical = format!("{}{suffix}", link.logical);
        let physical = format!("{}{suffix}", link.physical);
        assert_eq!(visible_path(&physical), logical);
        let context = WatchContext {
            roots: roots.clone(),
            events: Mutex::new(VecDeque::new()),
        };
        collect_event(&context, &physical, 42, 0x11000);
        let physical_events: Vec<_> = context.events.lock().unwrap().drain(..).collect();
        collect_event(&context, &logical, 42, 0x11000);
        let logical_events: Vec<_> = context.events.lock().unwrap().drain(..).collect();
        assert_eq!(
            physical_events.len(),
            1,
            "physical callback must remain inside the configured namespace"
        );
        assert_eq!(physical_events[0].path, logical);
        assert_eq!(
            reconciliation_plan(&roots, &physical_events),
            reconciliation_plan(&roots, &logical_events)
        );
        let plan = reconciliation_plan(&roots, &physical_events);
        assert_eq!(plan.recursive, [logical]);
        assert!(!plan.recursive.contains(&physical));
    }
    #[test]
    fn a_recovery_scope_cannot_turn_an_implicit_hidden_volume_into_an_explicit_root() {
        let cancelled = AtomicBool::new(false);
        let report = scan_excluding_in_namespace(
            &["/System/Volumes/Preboot".into()],
            &["/".into()],
            &[],
            &cancelled,
            |_| panic!("an excluded recovery scope must not enumerate entries"),
        );
        assert_eq!(report.entries, 0);
        assert!(report.uncovered.is_empty());
        cancelled.store(true, Ordering::Relaxed);
        let report = scan_excluding_in_namespace(
            &["/System/Volumes/Preboot".into()],
            &["/".into()],
            &[],
            &cancelled,
            |_| panic!("cancellation must not grant a recovery scope"),
        );
        assert!(report.cancelled);
        assert_eq!(report.entries, 0);
    }
    #[test]
    fn historical_mount_controls_require_a_completed_fixed_anchor_and_no_mixed_flags() {
        let roots = vec!["/fixture".into()];
        let old_mount = [ChangeEvent::from_flags("/", 40, 0x40)];
        assert_eq!(reconciliation_plan(&roots, &old_mount).recursive, roots);
        for flags in [0x40, 0x80, 0xc0] {
            let events = [ChangeEvent::from_flags("/", 40, flags)];
            assert_eq!(
                reconciliation_plan_with_history(&roots, &events, Some(50)),
                Reconciliation::default()
            );
            for (id, anchor) in [(0, Some(50)), (51, Some(50)), (40, None)] {
                let events = [ChangeEvent::from_flags("/", id, flags)];
                assert_eq!(
                    reconciliation_plan_with_history(&roots, &events, anchor).recursive,
                    roots,
                    "id={id}, flags={flags}, anchor={anchor:?}"
                );
            }
        }
        // The committed cursor may advance above 51 after another event; the
        // stream's fixed anchor stays 50, preserving a new out-of-order mount.
        for flags in [0x41, 0x42, 0x44, 0x48, 0x60, 0x140, 0x20440, 0x80c0] {
            let events = [ChangeEvent::from_flags("/", 40, flags)];
            assert_eq!(
                reconciliation_plan_with_history(&roots, &events, Some(50)).recursive,
                roots,
                "mixed flags={flags}"
            );
        }
        let ordinary = [ChangeEvent::from_flags("/fixture/old.txt", 39, 0x11000)];
        assert_eq!(
            reconciliation_plan_with_history(&roots, &ordinary, Some(50)),
            reconciliation_plan(&roots, &ordinary),
            "FullHistory's older ordinary records must remain reconcilable"
        );
        for flags in [0x02, 0x04, 0x08] {
            let events = [ChangeEvent::from_flags("/outside", 39, flags)];
            assert_eq!(
                reconciliation_plan_with_history(&roots, &events, Some(50)).recursive,
                roots
            );
        }
    }
    #[test]
    fn wrapped_control_survives_scoped_filtering_and_queue_compaction() {
        let context = WatchContext {
            roots: vec!["/fixture".into()],
            events: Mutex::new(VecDeque::new()),
        };
        collect_event(&context, "/outside", 7, file_events::EVENT_IDS_WRAPPED);
        assert_eq!(context.events.lock().unwrap().len(), 1);
        {
            let mut queue = context.events.lock().unwrap();
            for id in 0..65_536 {
                queue.push_back(ChangeEvent::from_flags("/fixture/file", id, 0));
            }
        }
        collect_event(&context, "/fixture/latest", 8, 0);
        let queue = context.events.lock().unwrap();
        assert_eq!(queue.len(), 1);
        assert_ne!(queue[0].flags & file_events::EVENT_IDS_WRAPPED, 0);
    }
}

#[cfg(all(test, target_os = "macos"))]
mod scope_boundary_tests {
    use super::*;
    use std::{fs, os::unix::fs::symlink};

    #[test]
    fn ancestor_replacement_rechecks_only_its_selected_descendants() {
        let roots = vec!["/fixture/parent/selected".into(), "/other".into()];
        let renamed = reconciliation_plan(
            &roots,
            &[ChangeEvent::from_flags("/fixture/parent", 42, 0x20800)],
        );
        assert_eq!(renamed.recursive, ["/fixture/parent/selected"]);
        let access = reconciliation_plan(
            &roots,
            &[ChangeEvent::from_flags("/fixture/parent", 43, 0x20400)],
        );
        assert!(access.recursive.is_empty());
        assert_eq!(access.directory_checks, ["/fixture/parent/selected"]);
    }

    #[test]
    fn explicit_alias_selection_is_resolved_once_and_recovery_never_retargets_it() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let actual = root.join("actual");
        let outside = root.join("outside");
        fs::create_dir_all(actual.join("selected")).unwrap();
        fs::create_dir_all(outside.join("selected")).unwrap();
        fs::write(actual.join("selected/original.txt"), "original").unwrap();
        fs::write(outside.join("selected/private.txt"), "private").unwrap();
        let alias = root.join("alias");
        symlink(&actual, &alias).unwrap();
        let chosen = normalize_roots(&[alias.join("selected").to_string_lossy().into_owned()]);
        assert_eq!(chosen, [actual.join("selected").to_string_lossy()]);
        let mut entries = Vec::new();
        scan_excluding_in_namespace(&chosen, &chosen, &[], &AtomicBool::new(false), |batch| {
            entries.extend(batch)
        });
        assert!(entries.iter().any(|entry| entry.name == "original.txt"));
        fs::rename(&actual, root.join("moved")).unwrap();
        symlink(&outside, &actual).unwrap();
        assert_eq!(normalize_scopes(&chosen), chosen);
        let report =
            scan_excluding_in_namespace(&chosen, &chosen, &[], &AtomicBool::new(false), |_| {
                panic!("saved selection followed a replaced intermediate directory")
            });
        assert_eq!(report.entries, 0);
        assert_eq!(report.uncovered, chosen);
    }

    #[test]
    fn derived_scopes_and_metadata_cannot_expand_a_selected_root() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let selected = root.join("selected");
        let outside = root.join("selected-neighbor");
        fs::create_dir(&selected).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("private.txt"), "private").unwrap();
        let configured = vec![selected.to_string_lossy().into_owned()];
        let scopes = vec![outside.to_string_lossy().into_owned()];
        let cancelled = AtomicBool::new(false);
        let report = scan_excluding_in_namespace(&scopes, &configured, &[], &cancelled, |_| {
            panic!("recursive scope escaped its selection")
        });
        assert_eq!(report.entries, 0);
        assert!(report.roots.is_empty());
        let report = scan_metadata_in_namespace(&scopes, &configured, &cancelled, |_| {
            panic!("metadata scope escaped its selection")
        });
        assert_eq!(report.entries, 0);
        assert!(report.roots.is_empty());
        fs::write(selected.join("child.txt"), "child").unwrap();
        let mut scopes = configured.clone();
        scopes.push(selected.join("child.txt").to_string_lossy().into_owned());
        let mut names = Vec::new();
        scan_metadata_in_namespace(&scopes, &configured, &cancelled, |batch| {
            names.extend(batch.into_iter().map(|entry| entry.name))
        });
        assert_eq!(names, ["selected", "child.txt"]);
    }

    #[test]
    fn final_symlinks_and_missing_system_aliases_keep_their_existing_meaning() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let alias = root.join("selected-link");
        symlink(root.join("missing-target"), &alias).unwrap();
        let selected = vec![alias.to_string_lossy().into_owned()];
        assert_eq!(normalize_roots(&selected), selected);
        assert_eq!(normalize_scopes(&selected), selected);
        let mut entries = Vec::new();
        scan_excluding_in_namespace(
            &selected,
            &selected,
            &[],
            &AtomicBool::new(false),
            |batch| entries.extend(batch),
        );
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_symlink);
        let intermediate = root.join("intermediate-alias");
        symlink(&root, &intermediate).unwrap();
        assert_eq!(
            normalize_roots(&[intermediate
                .join("selected-link")
                .to_string_lossy()
                .into_owned()]),
            selected,
            "resolve the selected link's parent once without following the leaf"
        );
        for (physical, logical) in filesystem::system_aliases() {
            let missing = logical.join("apfsearch-nonexistent-alias-fixture/child");
            assert_eq!(
                normalize_scopes(&[missing.to_string_lossy().into_owned()]),
                [physical
                    .join("apfsearch-nonexistent-alias-fixture/child")
                    .to_string_lossy()]
            );
            assert_eq!(
                normalize_scopes(&[logical.to_string_lossy().into_owned()]),
                [logical.to_string_lossy()]
            );
        }
    }
}
