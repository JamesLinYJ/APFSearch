//! File metadata, mounted-volume discovery and bounded directory enumeration.
//!
//! Directory records are decoded as checked byte slices, never cast to packed
//! structs. Directory descriptors are owned, and callers use closures and `io::Result`.
use std::{
    borrow::Cow,
    ffi::{c_char, CStr, CString},
    fs::OpenOptions,
    io,
    mem::MaybeUninit,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Path, PathBuf},
    ptr,
};

pub(crate) const ATTR_CMN_ERROR: u32 = 0x2000_0000; // sys/attr.h; absent from libc's public constants.
const SF_FIRMLINK: u32 = 0x0080_0000;
const BUFFER_SIZE: usize = 256 * 1024;
const NANOSECONDS: i64 = 1_000_000_000;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileKind {
    File,
    Directory,
    Symlink,
    Other(u32),
}
impl From<u32> for FileKind {
    fn from(value: u32) -> Self {
        match value {
            1 => Self::File,
            2 => Self::Directory,
            5 => Self::Symlink,
            n => Self::Other(n),
        }
    }
}
#[derive(Clone, Debug)]
pub(crate) struct FileMetadata {
    pub file_id: u64,
    pub parent_id: u64,
    pub size: u64,
    /// Observed hard-link count; directory counts and unsupported attributes are unknown.
    pub link_count: Option<u64>,
    pub modified: i64,
    pub changed: i64,
    pub created: i64,
    pub modified_ns: i64,
    pub changed_ns: i64,
    pub flags: u32,
    pub kind: FileKind,
    pub error: Option<i32>,
    pub volume_id: String,
}
#[derive(Clone, Debug)]
pub(crate) struct Volume {
    pub path: String,
    pub id: String,
    pub filesystem: String,
}
/// A mount-table snapshot defines which namespaces this metadata pass may touch.
/// Loading uses getfsstat(MNT_NOWAIT), so rejecting SMB/autofs never opens them.
#[derive(Clone, Debug)]
pub(crate) struct MountScope {
    mounts: Vec<(PathBuf, bool)>,
    aliases: Vec<(PathBuf, PathBuf)>,
}
impl MountScope {
    pub(crate) fn load() -> io::Result<Self> {
        let mut mounts = Vec::new();
        for filesystem in mounted_filesystems()? {
            let path = PathBuf::from(
                c_array_string(&filesystem.f_mntonname)?
                    .to_str()
                    .map_err(|_| invalid("Non-UTF8 mount path"))?,
            );
            let allowed = c_array_string(&filesystem.f_fstypename)?.to_bytes() == b"apfs"
                && filesystem.f_flags & libc::MNT_LOCAL as u32 != 0
                && (path == Path::new("/") || filesystem.f_flags & libc::MNT_SNAPSHOT as u32 == 0);
            mounts.push((path, allowed));
        }
        Ok(Self::from_mounts(mounts))
    }
    pub(crate) fn from_mounts(mut mounts: Vec<(PathBuf, bool)>) -> Self {
        mounts.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
        Self {
            mounts,
            aliases: Vec::new(),
        }
    }
    pub(crate) fn with_aliases(
        mut self,
        aliases: impl Iterator<Item = (PathBuf, PathBuf)>,
    ) -> Self {
        let aliases: Vec<_> = aliases.collect();
        let mut mapped = Vec::new();
        for (physical, logical) in &aliases {
            for (mount, allowed) in &self.mounts {
                if let Ok(suffix) = mount.strip_prefix(physical) {
                    mapped.push((logical.join(suffix), *allowed));
                }
                if let Ok(suffix) = mount.strip_prefix(logical) {
                    mapped.push((physical.join(suffix), *allowed));
                }
            }
        }
        self.mounts.extend(mapped);
        self.mounts
            .sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
        self.aliases.extend(aliases);
        self
    }
    pub(crate) fn allows(&self, path: &Path) -> bool {
        self.mounts
            .iter()
            .find(|(mount, _)| path.starts_with(mount))
            .is_some_and(|(_, allowed)| *allowed)
    }
    fn check(&self, path: &Path) -> io::Result<()> {
        if self.allows(path) {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(libc::ENOTSUP))
        }
    }
    fn check_current_mount(&self, path: &Path) -> io::Result<()> {
        self.check(path)?;
        // A mount can change while this pass is walking other directories.
        // Refresh only at actual path IO boundaries, not for every packed record.
        Self::load()?
            .with_aliases(self.aliases.iter().cloned())
            .check(path)
    }
    pub(crate) fn excluded_within(&self, roots: &[String]) -> Vec<String> {
        let mut excluded: Vec<_> = self
            .mounts
            .iter()
            .filter(|(_, allowed)| !allowed)
            .filter(|(mount, _)| roots.iter().any(|root| mount.starts_with(root)))
            .map(|(path, _)| path.to_string_lossy().into_owned())
            .collect();
        excluded.extend(
            roots
                .iter()
                .filter(|root| !self.allows(Path::new(root)))
                .cloned(),
        );
        excluded.sort();
        excluded.dedup();
        excluded
    }
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
/// Only these verified, root-owned OS aliases may appear inside an input path.
/// Cache their identity once: changing the root namespace requires administrator
/// authority, unlike arbitrary symlinks inside an indexed user directory.
pub(crate) fn system_aliases() -> &'static [(PathBuf, PathBuf)] {
    static ALIASES: std::sync::OnceLock<Vec<(PathBuf, PathBuf)>> = std::sync::OnceLock::new();
    ALIASES.get_or_init(|| {
        ["var", "tmp", "etc"]
            .into_iter()
            .filter_map(|name| {
                let logical = PathBuf::from(format!("/{name}"));
                let physical = PathBuf::from(format!("/private/{name}"));
                let metadata = std::fs::symlink_metadata(&logical).ok()?;
                if !metadata.file_type().is_symlink() || metadata.uid() != 0 {
                    return None;
                }
                let target = std::fs::read_link(&logical).ok()?;
                (target == physical || target == physical.strip_prefix("/").unwrap())
                    .then_some((physical, logical))
            })
            .collect()
    })
}
pub(crate) fn system_path(path: &Path) -> Cow<'_, Path> {
    for (physical, logical) in system_aliases() {
        if let Ok(suffix) = path.strip_prefix(logical) {
            // A selected alias itself remains a symlink directory entry.
            if !suffix.as_os_str().is_empty() {
                return Cow::Owned(physical.join(suffix));
            }
        }
    }
    Cow::Borrowed(path)
}
pub(crate) fn path_string(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}
fn nanoseconds(seconds: i64, part: i64) -> io::Result<i64> {
    if !(0..NANOSECONDS).contains(&part) {
        return Err(invalid("Invalid nanosecond field"));
    }
    seconds
        .checked_mul(NANOSECONDS)
        .and_then(|s| s.checked_add(part))
        .ok_or_else(|| io::Error::from_raw_os_error(libc::EOVERFLOW))
}
pub(crate) fn c_array_string<const N: usize>(bytes: &[c_char; N]) -> io::Result<&CStr> {
    // c_char has the same one-byte layout as u8. No CStr scan can run beyond
    // the fixed kernel-returned field: from_bytes_until_nul checks the bound.
    let bytes = unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<u8>(), N) };
    CStr::from_bytes_until_nul(bytes).map_err(|_| invalid("Unterminated filesystem field"))
}
fn filesystem_for_fd(fd: &impl AsRawFd) -> io::Result<libc::statfs> {
    let mut result = MaybeUninit::uninit();
    // OwnedFd guarantees the descriptor remains valid for the syscall.
    if unsafe { libc::fstatfs(fd.as_raw_fd(), result.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { result.assume_init() })
}
pub(crate) fn mounted_filesystems() -> io::Result<Vec<libc::statfs>> {
    // getmntinfo uses process-global mutable storage. getfsstat writes only our
    // private allocation, so volume discovery and watcher startup can overlap.
    for _ in 0..4 {
        let count = unsafe { libc::getfsstat(ptr::null_mut(), 0, libc::MNT_NOWAIT) };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        let capacity = (count as usize)
            .checked_add(16)
            .ok_or_else(|| invalid("Mount count overflow"))?;
        let bytes = capacity
            .checked_mul(size_of::<libc::statfs>())
            .and_then(|n| i32::try_from(n).ok())
            .ok_or_else(|| invalid("Mount table too large"))?;
        let mut buffer: Vec<MaybeUninit<libc::statfs>> = Vec::with_capacity(capacity);
        buffer.resize_with(capacity, MaybeUninit::uninit);
        let written =
            unsafe { libc::getfsstat(buffer.as_mut_ptr().cast(), bytes, libc::MNT_NOWAIT) };
        if written < 0 {
            return Err(io::Error::last_os_error());
        }
        if written as usize >= capacity {
            continue;
        }
        // Only the records reported as initialized by getfsstat are read.
        return Ok(buffer
            .into_iter()
            .take(written as usize)
            .map(|slot| unsafe { slot.assume_init() })
            .collect());
    }
    Err(io::Error::from_raw_os_error(libc::EAGAIN))
}
fn volume_id(fs: &libc::statfs) -> io::Result<String> {
    let mount = c_array_string(&fs.f_mntonname)?;
    let mut attrs = libc::attrlist {
        bitmapcount: 5,
        reserved: 0,
        commonattr: 0,
        volattr: libc::ATTR_VOL_INFO | libc::ATTR_VOL_UUID,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut data = [0u8; 20];
    // The returned volume record is uint32 length + 16 UUID bytes. Byte storage
    // avoids inventing padding or alignment for that packed layout.
    let code = unsafe {
        libc::getattrlist(
            mount.as_ptr(),
            ptr::from_mut(&mut attrs).cast(),
            data.as_mut_ptr().cast(),
            data.len(),
            0,
        )
    };
    if code == 0
        && u32::from_ne_bytes(data[..4].try_into().unwrap()) as usize >= data.len()
        && data[4..].iter().any(|v| *v != 0)
    {
        let uuid = &data[4..];
        return Ok(format!("{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",uuid[0],uuid[1],uuid[2],uuid[3],uuid[4],uuid[5],uuid[6],uuid[7],uuid[8],uuid[9],uuid[10],uuid[11],uuid[12],uuid[13],uuid[14],uuid[15]));
    }
    // fsid_t is an opaque two-int ABI object in libc. Copy its representation
    // rather than access private fields or label this mount-only fallback UUID.
    let mut bytes = [0u8; 8];
    unsafe {
        ptr::copy_nonoverlapping(
            ptr::addr_of!(fs.f_fsid).cast::<u8>(),
            bytes.as_mut_ptr(),
            bytes.len(),
        );
    }
    Ok(format!(
        "fsid:{:08x}:{:08x}",
        u32::from_ne_bytes(bytes[..4].try_into().unwrap()),
        u32::from_ne_bytes(bytes[4..].try_into().unwrap())
    ))
}
/// Pin the parent namespace and stat exactly one leaf without following it.
/// Unlike opening the leaf, fstatat can inspect a chmod(000) entry itself.
pub(crate) fn metadata(path: &Path, scope: &MountScope) -> io::Result<libc::stat> {
    let path = system_path(path);
    scope.check_current_mount(&path)?;
    let (parent_path, name) = entry_parts(&path)?;
    metadata_at(&open_metadata_parent(parent_path)?, name)
}
fn entry_parts(path: &Path) -> io::Result<(&Path, &std::ffi::OsStr)> {
    if path == Path::new("/") {
        Ok((path, std::ffi::OsStr::new(".")))
    } else {
        Ok((
            path.parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
            path.file_name()
                .ok_or_else(|| invalid("Missing directory-entry name"))?,
        ))
    }
}
fn open_metadata_parent(path: &Path) -> io::Result<std::fs::File> {
    // No content read or cloud hydration; every intermediate component and the
    // parent itself must be real directories rather than mutable user aliases.
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_EVTONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW_ANY | libc::O_CLOEXEC)
        .open(path)
}
fn metadata_at(parent: &std::fs::File, name: &std::ffi::OsStr) -> io::Result<libc::stat> {
    let name = path_string(Path::new(name))?;
    let mut metadata = MaybeUninit::uninit();
    // entry_parts supplies one component (or root's "."); the held parent
    // cannot be retargeted and AT_SYMLINK_NOFOLLOW preserves final symlinks.
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { metadata.assume_init() })
}
struct MetadataParent {
    path: PathBuf,
    file: std::fs::File,
    metadata: std::fs::Metadata,
    volume_id: String,
}
/// Sorted metadata batches normally contain adjacent siblings. Reuse only the
/// last pinned parent, keeping descriptor use and cached namespace authority
/// constant regardless of batch size, without enumerating unrelated siblings.
pub(crate) struct MetadataReader<'a> {
    scope: &'a MountScope,
    parent: Option<MetadataParent>,
}
impl<'a> MetadataReader<'a> {
    pub(crate) fn new(scope: &'a MountScope) -> Self {
        Self {
            scope,
            parent: None,
        }
    }
    pub(crate) fn stat(&mut self, path: &Path) -> io::Result<FileMetadata> {
        let path = system_path(path);
        self.scope.check_current_mount(&path)?;
        let (parent_path, name) = entry_parts(&path)?;
        if self
            .parent
            .as_ref()
            .is_none_or(|parent| parent.path != parent_path)
        {
            let file = open_metadata_parent(parent_path)?;
            let metadata = file.metadata()?;
            let volume_id = filesystem_for_fd(&file)
                .and_then(|fs| volume_id(&fs))
                .unwrap_or_default();
            self.parent = Some(MetadataParent {
                path: parent_path.to_owned(),
                file,
                metadata,
                volume_id,
            });
        }
        let parent = self.parent.as_ref().unwrap();
        let metadata = metadata_at(&parent.file, name)?;
        let kind = match metadata.st_mode & libc::S_IFMT {
            libc::S_IFDIR => FileKind::Directory,
            libc::S_IFLNK => FileKind::Symlink,
            _ => FileKind::File,
        };
        let volume_id = if metadata.st_dev as u64 == parent.metadata.dev() {
            parent.volume_id.clone()
        } else {
            // A mount/firmlink entry may belong to a different device. Resolve
            // its volume from a metadata-only descriptor under the pinned parent.
            let name = path_string(Path::new(name))?;
            let descriptor = unsafe {
                libc::openat(
                    parent.file.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_EVTONLY | libc::O_SYMLINK | libc::O_NOFOLLOW_ANY | libc::O_CLOEXEC,
                )
            };
            if descriptor < 0 {
                return Err(io::Error::last_os_error());
            }
            // Transfer the new descriptor to exactly one RAII owner.
            let file = unsafe { std::fs::File::from_raw_fd(descriptor) };
            let current = file.metadata()?;
            if current.dev() != metadata.st_dev as u64 || current.ino() != metadata.st_ino {
                return Err(io::Error::from_raw_os_error(libc::EAGAIN));
            }
            filesystem_for_fd(&file)
                .and_then(|fs| volume_id(&fs))
                .unwrap_or_default()
        };
        Ok(FileMetadata {
            file_id: metadata.st_ino,
            parent_id: parent.metadata.ino(),
            size: if kind == FileKind::Directory {
                0
            } else {
                metadata.st_size as u64
            },
            link_count: (kind != FileKind::Directory).then_some(u64::from(metadata.st_nlink)),
            modified: metadata.st_mtime,
            changed: metadata.st_ctime,
            created: metadata.st_birthtime,
            modified_ns: nanoseconds(metadata.st_mtime, metadata.st_mtime_nsec)?,
            changed_ns: nanoseconds(metadata.st_ctime, metadata.st_ctime_nsec)?,
            flags: metadata.st_flags,
            kind,
            error: None,
            volume_id,
        })
    }
}
pub(crate) fn stat_entry(path: &Path, scope: &MountScope) -> io::Result<FileMetadata> {
    MetadataReader::new(scope).stat(path)
}
pub(crate) fn volumes() -> io::Result<Vec<Volume>> {
    let mut result = Vec::new();
    for fs in mounted_filesystems()? {
        let filesystem = c_array_string(&fs.f_fstypename)?
            .to_str()
            .map_err(|_| invalid("Non-UTF8 filesystem type"))?;
        if filesystem != "apfs" {
            continue;
        }
        let path = c_array_string(&fs.f_mntonname)?
            .to_str()
            .map_err(|_| invalid("Non-UTF8 mount path"))?;
        if path != "/"
            && fs.f_flags & (libc::MNT_DONTBROWSE as u32 | libc::MNT_SNAPSHOT as u32) != 0
        {
            continue;
        }
        if path.starts_with("/System/Volumes/") {
            continue;
        }
        result.push(Volume {
            path: path.into(),
            id: volume_id(&fs)?,
            filesystem: filesystem.into(),
        });
    }
    Ok(result)
}
struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}
impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }
    fn take<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        let end = self
            .position
            .checked_add(N)
            .ok_or_else(|| invalid("Attribute offset overflow"))?;
        let bytes = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| invalid("Truncated attribute record"))?;
        self.position = end;
        Ok(bytes.try_into().unwrap())
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_ne_bytes(self.take()?))
    }
    fn i32(&mut self) -> io::Result<i32> {
        Ok(i32::from_ne_bytes(self.take()?))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_ne_bytes(self.take()?))
    }
    fn i64(&mut self) -> io::Result<i64> {
        Ok(i64::from_ne_bytes(self.take()?))
    }
    fn time(&mut self) -> io::Result<(i64, i64)> {
        Ok((self.i64()?, self.i64()?))
    }
}
struct DirectoryRecord<'a> {
    name: &'a [u8],
    entry: FileMetadata,
    resolve_visible: bool,
}
fn parse_record<'a>(bytes: &'a [u8], volume: &str) -> io::Result<DirectoryRecord<'a>> {
    let mut cursor = Cursor::new(bytes);
    let length = cursor.u32()? as usize;
    if length != bytes.len() || !length.is_multiple_of(4) {
        return Err(invalid("Invalid packed record length"));
    }
    let returned_common = cursor.u32()?;
    let _returned_volume = cursor.u32()?;
    let _returned_directory = cursor.u32()?;
    let returned_file = cursor.u32()?;
    let _returned_fork = cursor.u32()?;
    let error = cursor.u32()?;
    let reference_start = cursor.position;
    let offset = cursor.i32()?;
    let name_length = cursor.u32()? as usize;
    let _device = cursor.u32()?;
    let kind = FileKind::from(cursor.u32()?);
    let (created, _) = cursor.time()?;
    let (modified, modified_part) = cursor.time()?;
    let (changed, changed_part) = cursor.time()?;
    let flags = cursor.u32()?;
    let file_id = cursor.u64()?;
    let parent_id = cursor.u64()?;
    // PACK_INVAL_ATTRS still selects either directory OR file attribute groups.
    let (mount_status, size, link_count) = if kind == FileKind::Directory {
        (cursor.u32()?, 0, None)
    } else {
        // sys/attr.h orders ATTR_FILE_LINKCOUNT (u32) before DATALENGTH
        // (off_t). PACK_INVAL_ATTRS reserves this slot even when unsupported;
        // only the returned bitmap says whether its value may be trusted.
        let links = cursor.u32()?;
        let link_count =
            (returned_file & libc::ATTR_FILE_LINKCOUNT != 0).then_some(u64::from(links));
        (0, cursor.i64()?.max(0) as u64, link_count)
    };
    let name_start = reference_start
        .checked_add_signed(offset as isize)
        .ok_or_else(|| invalid("Invalid attribute name offset"))?;
    let name_end = name_start
        .checked_add(name_length)
        .ok_or_else(|| invalid("Attribute name length overflow"))?;
    if name_length == 0 || name_start < cursor.position {
        return Err(invalid("Attribute name overlaps fixed fields"));
    }
    let name = bytes
        .get(name_start..name_end)
        .ok_or_else(|| invalid("Attribute name outside record"))?;
    if name.last() != Some(&0) || name[..name.len() - 1].contains(&0) {
        return Err(invalid("Attribute name is not one terminated string"));
    }
    Ok(DirectoryRecord {
        name: &name[..name.len() - 1],
        entry: FileMetadata {
            file_id,
            parent_id,
            size,
            link_count,
            modified,
            changed,
            created,
            modified_ns: nanoseconds(modified, modified_part)?,
            changed_ns: nanoseconds(changed, changed_part)?,
            flags,
            kind,
            error: (error != 0).then_some(error as i32),
            volume_id: volume.into(),
        },
        resolve_visible: flags & SF_FIRMLINK != 0
            || mount_status != 0
            || returned_common & libc::ATTR_CMN_FILEID == 0,
    })
}
fn parse_batch(
    buffer: &[u8],
    count: usize,
    volume: &str,
    mut callback: impl FnMut(DirectoryRecord<'_>) -> io::Result<()>,
) -> io::Result<()> {
    let mut offset = 0usize;
    for _ in 0..count {
        let header = buffer
            .get(
                offset
                    ..offset
                        .checked_add(4)
                        .ok_or_else(|| invalid("Record offset overflow"))?,
            )
            .ok_or_else(|| invalid("Truncated record header"))?;
        let length = u32::from_ne_bytes(header.try_into().unwrap()) as usize;
        if length < 24 {
            return Err(invalid("Invalid record length"));
        }
        let end = offset
            .checked_add(length)
            .ok_or_else(|| invalid("Record length overflow"))?;
        let record = parse_record(
            buffer
                .get(offset..end)
                .ok_or_else(|| invalid("Record exceeds batch buffer"))?,
            volume,
        )?;
        callback(record)?;
        offset = end;
    }
    Ok(())
}
/// Probe the same directory descriptor used by enumeration, including search
/// permission. Reading a directory alone does not authorize accessing its children.
pub(crate) fn check_directory_access(path: &Path, scope: &MountScope) -> io::Result<()> {
    open_searchable_directory(path, scope).map(drop)
}
fn open_searchable_directory(path: &Path, scope: &MountScope) -> io::Result<OwnedFd> {
    let path = system_path(path);
    let path = path.as_ref();
    scope.check_current_mount(path)?;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW_ANY)
        .open(path)?;
    // SAFETY: the owned descriptor remains live; "." is a static terminated path.
    // AT_EACCESS checks this process's effective credentials, including ACLs.
    if unsafe {
        libc::faccessat(
            directory.as_raw_fd(),
            c".".as_ptr(),
            libc::R_OK | libc::X_OK,
            libc::AT_EACCESS,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(directory.into())
}
pub(crate) fn scan_directory(
    path: &Path,
    scope: &MountScope,
    mut callback: impl FnMut(&[u8], &FileMetadata) -> std::ops::ControlFlow<()>,
) -> io::Result<()> {
    let fd = open_searchable_directory(path, scope)?;
    let fs = filesystem_for_fd(&fd)?;
    if c_array_string(&fs.f_fstypename)?.to_bytes() != b"apfs" {
        return Err(io::Error::from_raw_os_error(libc::ENOTSUP));
    }
    let volume = volume_id(&fs)?;
    let mut attrs = libc::attrlist {
        bitmapcount: 5,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS
            | ATTR_CMN_ERROR
            | libc::ATTR_CMN_NAME
            | libc::ATTR_CMN_DEVID
            | libc::ATTR_CMN_OBJTYPE
            | libc::ATTR_CMN_CRTIME
            | libc::ATTR_CMN_MODTIME
            | libc::ATTR_CMN_CHGTIME
            | libc::ATTR_CMN_FLAGS
            | libc::ATTR_CMN_FILEID
            | libc::ATTR_CMN_PARENTID,
        volattr: 0,
        dirattr: libc::ATTR_DIR_MOUNTSTATUS,
        fileattr: libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_DATALENGTH,
        forkattr: 0,
    };
    let mut buffer = vec![0u8; BUFFER_SIZE];
    loop {
        // The kernel writes up to buffer.len bytes; parser bounds every record,
        // field, relative reference, and terminating NUL before it is exposed.
        let count = unsafe {
            libc::getattrlistbulk(
                fd.as_raw_fd(),
                ptr::from_mut(&mut attrs).cast(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                libc::FSOPT_PACK_INVAL_ATTRS as u64,
            )
        };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        if count == 0 {
            return Ok(());
        }
        parse_batch(&buffer, count as usize, &volume, |mut record| {
            if matches!(record.name, b"." | b"..") {
                return Ok(());
            }
            let full = path.join(std::ffi::OsStr::from_bytes(record.name));
            // Mount-point/firmlink fallback stat must obey the same boundary as
            // directory open. Otherwise merely listing /Volumes asks for SMB access.
            if !scope.allows(&full) {
                return Ok(());
            }
            if record.resolve_visible {
                match stat_entry(&full, scope) {
                    Ok(target) => record.entry = target,
                    Err(error) => {
                        record.entry.error = Some(error.raw_os_error().unwrap_or(libc::EIO))
                    }
                }
            }
            if callback(record.name, &record.entry).is_break() {
                Err(io::Error::from_raw_os_error(libc::ECANCELED))
            } else {
                Ok(())
            }
        })?;
    }
}

#[cfg(test)]
#[path = "filesystem_tests.rs"]
mod tests;

#[cfg(test)]
mod link_count_tests {
    use super::*;

    fn packed_record(kind: u32, returned_file: u32, links: u32) -> (Vec<u8>, usize) {
        let mut bytes = vec![0u8; 4];
        let common = libc::ATTR_CMN_NAME | libc::ATTR_CMN_OBJTYPE | libc::ATTR_CMN_FILEID;
        for attributes in [common, 0, libc::ATTR_DIR_MOUNTSTATUS, returned_file, 0] {
            bytes.extend(attributes.to_ne_bytes());
        }
        bytes.extend(0u32.to_ne_bytes()); // ATTR_CMN_ERROR
        let reference = bytes.len();
        bytes.extend([0u8; 8]);
        bytes.extend(3u32.to_ne_bytes()); // device
        bytes.extend(kind.to_ne_bytes());
        for seconds in [100i64, 200, 300] {
            bytes.extend(seconds.to_ne_bytes());
            bytes.extend(123i64.to_ne_bytes());
        }
        bytes.extend(0u32.to_ne_bytes()); // flags
        bytes.extend(42u64.to_ne_bytes());
        bytes.extend(7u64.to_ne_bytes());
        let link_offset = bytes.len();
        if kind == 2 {
            bytes.extend(0u32.to_ne_bytes()); // directory mount status; no file group
        } else {
            bytes.extend(links.to_ne_bytes());
            bytes.extend(0x0102_0304_0506_0708i64.to_ne_bytes());
        }
        let name_start = bytes.len();
        bytes.extend(b"file\0");
        while !bytes.len().is_multiple_of(4) {
            bytes.push(0);
        }
        let length = bytes.len() as u32;
        bytes[..4].copy_from_slice(&length.to_ne_bytes());
        bytes[reference..reference + 4]
            .copy_from_slice(&((name_start - reference) as i32).to_ne_bytes());
        bytes[reference + 4..reference + 8].copy_from_slice(&5u32.to_ne_bytes());
        (bytes, link_offset)
    }

    #[test]
    fn returned_link_count_precedes_data_length_without_alignment_padding() {
        for kind in [1, 5] {
            for links in [0, 1, 2, u32::MAX] {
                let (bytes, _) = packed_record(
                    kind,
                    libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_DATALENGTH,
                    links,
                );
                let parsed = parse_record(&bytes, "volume").unwrap();
                assert_eq!(parsed.entry.link_count, Some(u64::from(links)));
                assert_eq!(parsed.entry.size, 0x0102_0304_0506_0708);
                assert_eq!(parsed.entry.modified_ns, 200_000_000_123);
                assert_eq!(parsed.name, b"file");
            }
        }
    }

    #[test]
    fn absent_link_count_mask_ignores_value_but_consumes_its_packed_slot() {
        for mask in [0, libc::ATTR_FILE_DATALENGTH] {
            let (bytes, _) = packed_record(1, mask, 1);
            let parsed = parse_record(&bytes, "volume").unwrap();
            assert_eq!(parsed.entry.link_count, None);
            assert_eq!(parsed.entry.size, 0x0102_0304_0506_0708);
            assert_eq!(parsed.name, b"file");
        }
    }

    #[test]
    fn directory_group_has_no_file_link_count_slot() {
        // Even a spurious file mask cannot make a directory consume file data.
        let (bytes, _) = packed_record(2, libc::ATTR_FILE_LINKCOUNT, 2);
        let parsed = parse_record(&bytes, "volume").unwrap();
        assert_eq!(parsed.entry.kind, FileKind::Directory);
        assert_eq!(parsed.entry.link_count, None);
        assert_eq!(parsed.entry.size, 0);
        assert_eq!(parsed.name, b"file");
    }

    #[test]
    fn truncated_link_count_and_following_length_are_rejected() {
        let (bytes, link_offset) = packed_record(1, libc::ATTR_FILE_LINKCOUNT, 2);
        for end in link_offset..link_offset + 12 {
            let mut truncated = bytes[..end].to_vec();
            // Match the enclosing length, so rejection must also validate the
            // packed fields/reference rather than rely on the old total alone.
            truncated[..4].copy_from_slice(&(end as u32).to_ne_bytes());
            assert!(parse_record(&truncated, "volume").is_err(), "prefix {end}");
        }
    }

    #[test]
    fn actual_apfs_bulk_and_stat_report_one_then_two_hard_links() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let first = root.join("first.txt");
        let second = root.join("second.txt");
        let directory = root.join("directory");
        std::fs::write(&first, b"links").unwrap();
        std::fs::create_dir(&directory).unwrap();
        let scope = MountScope::load().unwrap();
        let collect = || {
            let mut found = std::collections::HashMap::new();
            scan_directory(&root, &scope, |name, metadata| {
                assert!(metadata.error.is_none(), "{name:?}: {:?}", metadata.error);
                found.insert(name.to_vec(), (metadata.link_count, metadata.size));
                std::ops::ControlFlow::Continue(())
            })
            .unwrap();
            found
        };
        assert_eq!(stat_entry(&first, &scope).unwrap().link_count, Some(1));
        assert_eq!(stat_entry(&directory, &scope).unwrap().link_count, None);
        let single = collect();
        assert_eq!(single[b"first.txt".as_slice()], (Some(1), 5));
        assert_eq!(single[b"directory".as_slice()], (None, 0));
        std::fs::hard_link(&first, &second).unwrap();
        assert_eq!(stat_entry(&first, &scope).unwrap().link_count, Some(2));
        assert_eq!(stat_entry(&second, &scope).unwrap().link_count, Some(2));
        let linked = collect();
        assert_eq!(linked[b"first.txt".as_slice()], (Some(2), 5));
        assert_eq!(linked[b"second.txt".as_slice()], (Some(2), 5));
        assert_eq!(linked[b"directory".as_slice()], (None, 0));
    }
}
