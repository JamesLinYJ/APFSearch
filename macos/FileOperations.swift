import AppKit
import Darwin
import Foundation

/// Device/inode plus both nanosecond timestamps. Never treat a failed stat as
/// a zero-valued identity, and never follow the final symlink for file actions.
struct OperationIdentity: Hashable {
  let device: UInt64
  let inode: UInt64
  let size: Int64
  let mode: UInt32
  let modified: Int64
  let modifiedNS: Int64
  let changed: Int64
  let changedNS: Int64

  init(_ url: URL) throws {
    var value = stat()
    guard url.path.withCString({ lstat($0, &value) }) == 0 else {
      throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno))
    }
    self.init(value)
  }
  init(_ value: stat) {
    device = UInt64(UInt32(bitPattern: value.st_dev)); inode = UInt64(value.st_ino)
    size = Int64(value.st_size); mode = UInt32(value.st_mode)
    modified = Int64(value.st_mtimespec.tv_sec); modifiedNS = Int64(value.st_mtimespec.tv_nsec)
    changed = Int64(value.st_ctimespec.tv_sec); changedNS = Int64(value.st_ctimespec.tv_nsec)
  }
  init?(wire: [String: Any]) {
    guard let device = wire["device_id"] as? NSNumber, let inode = wire["file_id"] as? NSNumber,
      let size = wire["size"] as? NSNumber, let mode = wire["mode"] as? NSNumber,
      let modified = wire["modified"] as? NSNumber, let modifiedNS = wire["modified_nsec"] as? NSNumber,
      let changed = wire["changed"] as? NSNumber, let changedNS = wire["changed_nsec"] as? NSNumber else { return nil }
    self.device = device.uint64Value; self.inode = inode.uint64Value
    self.size = size.int64Value; self.mode = mode.uint32Value
    self.modified = modified.int64Value; self.modifiedNS = modifiedNS.int64Value
    self.changed = changed.int64Value; self.changedNS = changedNS.int64Value
  }
  func hasSamePayload(as other: Self) -> Bool {
    device == other.device && inode == other.inode && size == other.size && mode == other.mode
      && modified == other.modified && modifiedNS == other.modifiedNS
  }
  var wire: [String: Any] {
    ["device_id": device, "file_id": inode, "size": size, "mode": mode,
      "modified": modified, "modified_nsec": modifiedNS, "changed": changed, "changed_nsec": changedNS]
  }
  func matches(_ value: [String: Any]) -> Bool {
    guard (value["file_id"] as? NSNumber)?.uint64Value == inode else { return false }
    if let v = value["device_id"] as? NSNumber, v.uint64Value != device { return false }
    if let v = value["mode"] as? NSNumber, v.uint32Value != mode { return false }
    if let ns = value["modified_ns"] as? NSNumber {
      let (modifiedBase, modifiedOverflow) = modified.multipliedReportingOverflow(by: 1_000_000_000)
      let (modifiedValue, modifiedAddOverflow) = modifiedBase.addingReportingOverflow(modifiedNS)
      let (changedBase, changedOverflow) = changed.multipliedReportingOverflow(by: 1_000_000_000)
      let (changedValue, changedAddOverflow) = changedBase.addingReportingOverflow(changedNS)
      guard !modifiedOverflow, !modifiedAddOverflow, !changedOverflow, !changedAddOverflow,
        ns.int64Value == modifiedValue,
        (value["changed_ns"] as? NSNumber)?.int64Value == changedValue
      else { return false }
    } else {
      guard (value["modified"] as? NSNumber)?.int64Value == modified,
        (value["modified_nsec"] as? NSNumber)?.int64Value == modifiedNS,
        (value["changed"] as? NSNumber)?.int64Value == changed,
        (value["changed_nsec"] as? NSNumber)?.int64Value == changedNS else { return false }
    }
    // Indexed directories deliberately do not expose a POSIX byte size.
    return (mode & UInt32(S_IFMT) == UInt32(S_IFDIR))
      || (value["size"] as? NSNumber)?.int64Value == size
  }
}

/// Pin one object at a time, including the final symlink. Metadata-only handles
/// survive renames and cross-volume unlinking without reading file contents.
private final class OperationHandle {
  let descriptor: Int32
  init(_ location: OperationLocation) throws {
    descriptor = location.name.withCString { openat(location.parent.descriptor, $0, O_EVTONLY | O_SYMLINK | O_CLOEXEC) }
    guard descriptor >= 0 else { throw operationPOSIX() }
  }
  func identity() throws -> OperationIdentity {
    var value = stat()
    guard fstat(descriptor, &value) == 0 else { throw operationPOSIX() }
    return OperationIdentity(value)
  }
  deinit { Darwin.close(descriptor) }
}

private func operationPOSIX(_ code: Int32 = errno) -> NSError {
  NSError(domain: NSPOSIXErrorDomain, code: Int(code))
}

/// A pathname is used only to acquire an authority. Every subsequent lookup and
/// mutation uses a single leaf relative to that open directory. Resolving a
/// user-selected parent alias happens before acquisition; O_NOFOLLOW_ANY closes
/// the gap between resolution and open, and the reviewed inode closes the gap
/// between preview and execution.
private final class OperationDirectory {
  let descriptor: Int32
  let url: URL
  init(_ url: URL, resolveAliases: Bool = true) throws {
    if resolveAliases {
      guard let canonical = url.path.withCString({ realpath($0, nil) }) else { throw operationPOSIX() }
      defer { free(canonical) }
      self.url = URL(fileURLWithPath: String(cString: canonical), isDirectory: true)
    } else { self.url = url }
    descriptor = self.url.path.withCString { Darwin.open($0, O_SEARCH | O_CLOEXEC | O_NOFOLLOW_ANY) }
    guard descriptor >= 0 else { throw operationPOSIX() }
  }
  init(parent: OperationDirectory, name: String) throws {
    url = parent.url.appendingPathComponent(name, isDirectory: true)
    descriptor = name.withCString { openat(parent.descriptor, $0, O_SEARCH | O_CLOEXEC | O_NOFOLLOW) }
    guard descriptor >= 0 else { throw operationPOSIX() }
  }
  init(handle: OperationHandle, url: URL) throws {
    self.url = url
    descriptor = openat(handle.descriptor, ".", O_SEARCH | O_CLOEXEC | O_NOFOLLOW)
    guard descriptor >= 0 else { throw operationPOSIX() }
  }
  func metadata() throws -> stat {
    var value = stat()
    guard fstat(descriptor, &value) == 0 else { throw operationPOSIX() }
    return value
  }
  func identity() throws -> OperationIdentity { OperationIdentity(try metadata()) }
  func currentURL() throws -> URL {
    var bytes = [CChar](repeating: 0, count: Int(PATH_MAX))
    guard bytes.withUnsafeMutableBufferPointer({ fcntl(descriptor, F_GETPATH, $0.baseAddress!) }) == 0 else { throw operationPOSIX() }
    return URL(fileURLWithPath: String(cString: bytes), isDirectory: true)
  }
  func identity(_ name: String) throws -> OperationIdentity {
    var value = stat()
    guard name.withCString({ fstatat(descriptor, $0, &value, AT_SYMLINK_NOFOLLOW) }) == 0 else { throw operationPOSIX() }
    return OperationIdentity(value)
  }
  func exists(_ name: String) throws -> Bool {
    do { _ = try identity(name); return true }
    catch let error as NSError where error.domain == NSPOSIXErrorDomain && error.code == Int(ENOENT) { return false }
  }
  func names() throws -> [String] {
    // Opening '.' relative to the pinned directory never reacquires its path.
    let fd = openat(descriptor, ".", O_RDONLY | O_DIRECTORY | O_CLOEXEC)
    guard fd >= 0 else { throw operationPOSIX() }
    guard let stream = fdopendir(fd) else { let error = operationPOSIX(); close(fd); throw error }
    defer { closedir(stream) }
    var names = [String]()
    while true {
      errno = 0
      guard let entry = readdir(stream) else {
        guard errno == 0 else { throw operationPOSIX() }
        return names
      }
      let name = withUnsafePointer(to: &entry.pointee.d_name) {
        $0.withMemoryRebound(to: CChar.self, capacity: Int(entry.pointee.d_namlen) + 1) { String(validatingCString: $0) }
      }
      guard let name else { throw operationPOSIX(EILSEQ) }
      if name != "." && name != ".." { names.append(name) }
    }
  }
  deinit { close(descriptor) }
}

private struct OperationLocation {
  let parent: OperationDirectory
  let name: String
  var url: URL { parent.url.appendingPathComponent(name) }
  init(_ url: URL) throws {
    parent = try OperationDirectory(url.deletingLastPathComponent())
    name = url.lastPathComponent
  }
  init(parent: OperationDirectory, name: String) { self.parent = parent; self.name = name }
  func identity() throws -> OperationIdentity { try parent.identity(name) }
  func exists() throws -> Bool { try parent.exists(name) }
  func rename(to target: OperationLocation) throws {
    let result = name.withCString { from in target.name.withCString { to in
      renameatx_np(parent.descriptor, from, target.parent.descriptor, to, UInt32(RENAME_EXCL))
    } }
    guard result == 0 else { throw operationPOSIX() }
  }
}

/// Only private, same-volume storage with an ancestry that another uid cannot
/// rename is handed to Foundation's URL-only Trash API. Checking ownership of
/// only the final mkdtemp directory would not establish that property.
private final class OperationWorkspace {
  let parent: OperationDirectory
  let directory: OperationDirectory
  let name: String
  init(preferred: URL, source: OperationLocation, excluding: [URL]) throws {
    let device = try source.parent.identity().device
    var candidates = [(try? OperationDirectory(preferred).url) ?? preferred]
    candidates += source.parent.url.pathComponents.indices.reversed().compactMap { index in
      index == 0 ? nil : URL(fileURLWithPath: NSString.path(withComponents: Array(source.parent.url.pathComponents.prefix(index + 1))), isDirectory: true)
    }
    var selected: (OperationDirectory, String, OperationDirectory)?
    for candidate in candidates {
      let path = candidate.path
      if excluding.contains(where: { path == $0.path || path.hasPrefix($0.path + "/") }) { continue }
      guard let candidate = try? Self.trustedDirectory(candidate),
        (try? candidate.identity().device) == device else { continue }
      let name = ".apfsearch-operations-" + UUID().uuidString
      guard mkdirat(candidate.descriptor, name, 0o700) == 0 else { continue }
      do {
        let directory = try OperationDirectory(parent: candidate, name: name)
        guard fchmod(directory.descriptor, 0o700) == 0 else { throw operationPOSIX() }
        // Remove inherited grants from this newly created, still-empty object.
        if let acl = acl_init(0) {
          defer { acl_free(UnsafeMutableRawPointer(acl)) }
          if acl_set_fd(directory.descriptor, acl) != 0 && errno != ENOTSUP { throw operationPOSIX() }
        } else { throw operationPOSIX() }
        selected = (candidate, name, directory); break
      } catch { _ = unlinkat(candidate.descriptor, name, AT_REMOVEDIR) }
    }
    guard let selected else { throw operationPOSIX(EPERM) }
    (parent, name, directory) = selected
  }
  private static func trustedDirectory(_ url: URL) throws -> OperationDirectory {
    let path = url
    // No canonicalization here: a substituted symlink must not turn an unsafe
    // native-API path into an apparently safe destination elsewhere.
    var directory = try OperationDirectory(URL(fileURLWithPath: "/"), resolveAliases: false)
    try trusted(directory)
    for component in path.pathComponents.dropFirst() {
      directory = try OperationDirectory(parent: directory, name: component)
      try trusted(directory)
    }
    return directory
  }
  private static func trusted(_ directory: OperationDirectory) throws {
    let value = try directory.metadata()
    guard value.st_uid == 0 || value.st_uid == geteuid() else { throw operationPOSIX(EPERM) }
    let shared = value.st_mode & 0o022 != 0
    // In a root-owned sticky directory another uid cannot rename our child.
    guard !shared || (value.st_uid == 0 && value.st_mode & S_ISVTX != 0) else { throw operationPOSIX(EPERM) }
    guard let acl = acl_get_fd(directory.descriptor) else {
      if errno == ENOTSUP || errno == ENOENT { return }
      throw operationPOSIX()
    }
    defer { acl_free(UnsafeMutableRawPointer(acl)) }
    var entry: acl_entry_t?, position = ACL_FIRST_ENTRY.rawValue
    while acl_get_entry(acl, position, &entry) == 0, let current = entry {
      position = ACL_NEXT_ENTRY.rawValue
      var tag = ACL_UNDEFINED_TAG
      guard acl_get_tag_type(current, &tag) == 0 else { throw operationPOSIX() }
      if tag != ACL_EXTENDED_ALLOW { continue }
      var mask: acl_permset_mask_t = 0
      guard acl_get_permset_mask_np(current, &mask) == 0 else { throw operationPOSIX() }
      // Conservatively reject explicit writable grants; ordinary home-directory
      // deny-delete ACLs and read-only inherited grants remain compatible.
      let writes = [ACL_WRITE_DATA, ACL_APPEND_DATA, ACL_DELETE, ACL_DELETE_CHILD, ACL_WRITE_SECURITY, ACL_CHANGE_OWNER]
      guard !writes.contains(where: { mask & UInt64($0.rawValue) != 0 }) else { throw operationPOSIX(EPERM) }
    }
  }
  func stage(_ id: String, leaf: String, preservingName: Bool = false) throws -> OperationStage {
    try OperationStage(workspace: self, id: id, leaf: leaf, preservingName: preservingName)
  }
  func path(_ id: String, leaf: String, preservingName: Bool = false) -> String {
    let path = directory.url.appendingPathComponent(id)
    return (preservingName ? path.appendingPathComponent(leaf) : path).path
  }
  // Never recursively clean up a workspace: unresolved capture/partial-copy
  // paths remain available in the durable intent and the failed result.
  deinit { _ = unlinkat(parent.descriptor, name, AT_REMOVEDIR) }
}

private final class OperationStage {
  let workspace: OperationWorkspace
  let container: String?
  let location: OperationLocation
  init(workspace: OperationWorkspace, id: String, leaf: String, preservingName: Bool) throws {
    self.workspace = workspace; container = preservingName ? id : nil
    if preservingName {
      guard mkdirat(workspace.directory.descriptor, id, 0o700) == 0 else { throw operationPOSIX() }
      do { location = OperationLocation(parent: try OperationDirectory(parent: workspace.directory, name: id), name: leaf) }
      catch { _ = unlinkat(workspace.directory.descriptor, id, AT_REMOVEDIR); throw error }
    } else { location = OperationLocation(parent: workspace.directory, name: id) }
  }
  deinit { if let container { _ = unlinkat(workspace.directory.descriptor, container, AT_REMOVEDIR) } }
}

private struct OperationFailure: LocalizedError {
  let underlying: Error
  let recoveryPaths: [String]
  var completedDestination: String? = nil
  var completedIdentity: OperationIdentity? = nil
  var errorDescription: String? { ([underlying.localizedDescription] + recoveryPaths).joined(separator: "\n") }
}
private final class OperationSecurityScope {
  let url: URL
  private let active: Bool
  init(_ url: URL) { self.url = url; active = url.startAccessingSecurityScopedResource() }
  convenience init(bookmark: String) throws {
    guard let data = Data(base64Encoded: bookmark) else { throw LT("error.operation_record_incomplete") }
    var stale = false
    let url = try URL(resolvingBookmarkData: data, options: [.withSecurityScope, .withoutUI, .withoutMounting],
      relativeTo: nil, bookmarkDataIsStale: &stale)
    self.init(url)
  }
  deinit { if active { url.stopAccessingSecurityScopedResource() } }
}
private struct OperationOutcome {
  let location: OperationLocation
  let identity: OperationIdentity
  var securityScope: OperationSecurityScope? = nil
  var destinationBookmark: String? = nil
}

#if TEST_BUILD
/// Deterministic scheduling at authority transitions; absent from production.
struct OperationTestHooks {
  var afterParentsOpened: (() throws -> Void)?
  var beforeCapture: (() throws -> Void)?
  var afterCapture: (() throws -> Void)?
  var beforePublish: (() throws -> Void)?
  var beforeNativeTrash: (() throws -> Void)?
  var forceCopyFallback = false
  var clonedFiles = 0
  var copiedFiles = 0
}
#endif

private final class OperationTransfer {
  let cancelled: () -> Bool
  #if TEST_BUILD
  var hooks: OperationTestHooks
  init(cancelled: @escaping () -> Bool, hooks: OperationTestHooks) { self.cancelled = cancelled; self.hooks = hooks }
  #else
  init(cancelled: @escaping () -> Bool) { self.cancelled = cancelled }
  #endif
  func checkCancellation() throws { if cancelled() { throw LT("status.cancelled") } }
  func perform(action: String, source: OperationLocation, sourceHandle: OperationHandle,
    expected: OperationIdentity, target: OperationLocation?, sourceWorkspace: OperationWorkspace?,
    targetWorkspace: OperationWorkspace?, id: String) throws -> OperationOutcome {
    var capture: OperationStage?, output: OperationStage?, captured = false
    var nativeDestination: String?, nativeIdentity: OperationIdentity?
    do {
      try checkCancellation()
      if action != "copy" {
        guard let sourceWorkspace else { throw operationPOSIX(EINVAL) }
        capture = try sourceWorkspace.stage(id, leaf: source.name, preservingName: action == "trash")
        #if TEST_BUILD
        try hooks.beforeCapture?()
        #endif
        try source.rename(to: capture!.location); captured = true
        // Rename has no expected-inode variant. Capture into private storage
        // first, then authorize the captured inode before publishing or trashing.
        guard try capture!.location.identity().hasSamePayload(as: expected) else { throw LT("files.source_changed") }
        #if TEST_BUILD
        try hooks.afterCapture?()
        #endif
        try checkCancellation()
      }
      let input = capture?.location ?? source
      if action == "trash" {
        #if TEST_BUILD
        try hooks.beforeNativeTrash?()
        #endif
        var resulting: NSURL?
        try FileManager.default.trashItem(at: input.url, resultingItemURL: &resulting)
        captured = false
        nativeDestination = (resulting as URL?)?.path
        nativeIdentity = try sourceHandle.identity()
        guard let resulting else { throw LT("error.trash_location_missing") }
        // Native Trash grants a capability for this item, not directory-read
        // access to ~/.Trash. Retain that capability and pin its parent using
        // O_SEARCH; never broaden the request to directory enumeration/FDA.
        let scope = OperationSecurityScope(resulting as URL)
        let actual = try OperationLocation(scope.url), after = nativeIdentity!
        guard try actual.identity() == after else { throw LT("files.source_changed") }
        let bookmark = try scope.url.bookmarkData(options: .withSecurityScope, includingResourceValuesForKeys: nil, relativeTo: nil)
        return OperationOutcome(location: actual, identity: after, securityScope: scope, destinationBookmark: bookmark.base64EncodedString())
      }
      guard let target else { throw operationPOSIX(EINVAL) }
      if action != "copy", try input.parent.identity().device == target.parent.identity().device {
        #if TEST_BUILD
        try hooks.beforePublish?()
        #endif
        try input.rename(to: target); captured = false
        return OperationOutcome(location: target, identity: try sourceHandle.identity())
      }
      guard let targetWorkspace else { throw operationPOSIX(EINVAL) }
      output = try targetWorkspace.stage(id + "-copy", leaf: target.name)
      var links = [String: [String]]()
      try copy(input, to: output!.location, expected: action == "copy" ? expected : sourceHandle.identity(),
        root: output!.location, relative: [], links: &links, pinned: sourceHandle)
      let outputHandle = try OperationHandle(output!.location)
      try checkCancellation()
      #if TEST_BUILD
      try hooks.beforePublish?()
      #endif
      try output!.location.rename(to: target)
      if captured {
        try remove(input)
        captured = false
      }
      return OperationOutcome(location: target, identity: try outputHandle.identity())
    } catch {
      var recovery = [String]()
      if captured, let capture {
        // Restore exclusively through the original pinned parent. If occupied,
        // leave the captured object and durable recovery path intact.
        do { try capture.location.rename(to: source) }
        catch { recovery.append(capture.location.url.path) }
      }
      if let output, (try? output.location.exists()) == true { recovery.append(output.location.url.path) }
      if let nativeDestination { recovery.append(nativeDestination) }
      throw OperationFailure(underlying: error, recoveryPaths: recovery,
        completedDestination: nativeDestination, completedIdentity: nativeIdentity)
    }
  }
  private func copy(_ source: OperationLocation, to target: OperationLocation, expected: OperationIdentity,
    root: OperationLocation, relative: [String], links: inout [String: [String]], pinned: OperationHandle? = nil) throws {
    try checkCancellation()
    let handle = try pinned ?? OperationHandle(source)
    guard try handle.identity() == expected else { throw LT("files.source_changed") }
    let type = expected.mode & UInt32(S_IFMT)
    if type == UInt32(S_IFDIR) {
      let from = try OperationDirectory(handle: handle, url: source.url)
      guard try from.identity() == expected else { throw LT("files.source_changed") }
      guard mkdirat(target.parent.descriptor, target.name, 0o700) == 0 else { throw operationPOSIX() }
      let to = try OperationDirectory(parent: target.parent, name: target.name)
      for name in try from.names() {
        let child = OperationLocation(parent: from, name: name)
        try copy(child, to: OperationLocation(parent: to, name: name), expected: child.identity(), root: root,
          relative: relative + [name], links: &links)
      }
      guard try from.identity() == expected else { throw LT("files.source_changed") }
      guard fcopyfile(from.descriptor, to.descriptor, nil, copyfile_flags_t(COPYFILE_METADATA)) == 0 else { throw operationPOSIX() }
      return
    }
    guard type == UInt32(S_IFREG) || type == UInt32(S_IFLNK) else { throw LT("files.unsupported_type") }
    var metadata = stat()
    guard fstat(handle.descriptor, &metadata) == 0 else { throw operationPOSIX() }
    let linkKey = "\(expected.device):\(expected.inode)"
    if type == UInt32(S_IFREG), metadata.st_nlink > 1, let earlier = links[linkKey] {
      var parent = try OperationDirectory(parent: root.parent, name: root.name)
      for component in earlier.dropLast() { parent = try OperationDirectory(parent: parent, name: component) }
      guard let name = earlier.last,
        linkat(parent.descriptor, name, target.parent.descriptor, target.name, 0) == 0 else { throw operationPOSIX() }
      return
    }
    var cloneResult: Int32
    #if TEST_BUILD
    if hooks.forceCopyFallback { cloneResult = -1; errno = ENOTSUP }
    else { cloneResult = fclonefileat(handle.descriptor, target.parent.descriptor, target.name, UInt32(CLONE_ACL)) }
    #else
    cloneResult = fclonefileat(handle.descriptor, target.parent.descriptor, target.name, UInt32(CLONE_ACL))
    #endif
    if cloneResult == 0 {
      #if TEST_BUILD
      hooks.clonedFiles += 1
      #endif
    } else {
      let cloneError = errno
      guard [EXDEV, ENOTSUP, EOPNOTSUPP, ENOSYS, EINVAL].contains(cloneError) else { throw operationPOSIX(cloneError) }
      if type == UInt32(S_IFLNK) {
        var bytes = [CChar](repeating: 0, count: Int(PATH_MAX) + 1)
        let count = bytes.withUnsafeMutableBufferPointer { readlinkat(source.parent.descriptor, source.name, $0.baseAddress!, $0.count - 1) }
        guard count >= 0, count < bytes.count - 1 else { throw operationPOSIX(count < 0 ? errno : ENAMETOOLONG) }
        guard try source.identity() == expected else { throw LT("files.source_changed") }
        guard bytes.withUnsafeBufferPointer({ symlinkat($0.baseAddress!, target.parent.descriptor, target.name) }) == 0 else { throw operationPOSIX() }
        let destination = try OperationHandle(target)
        guard fcopyfile(handle.descriptor, destination.descriptor, nil, copyfile_flags_t(COPYFILE_METADATA)) == 0 else { throw operationPOSIX() }
      } else {
        let from = openat(source.parent.descriptor, source.name, O_RDONLY | O_CLOEXEC | O_NOFOLLOW)
        guard from >= 0 else { throw operationPOSIX() }; defer { close(from) }
        var value = stat()
        guard fstat(from, &value) == 0, OperationIdentity(value) == expected else { throw LT("files.source_changed") }
        let to = openat(target.parent.descriptor, target.name, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW, 0o600)
        guard to >= 0 else { throw operationPOSIX() }; defer { close(to) }
        guard fcopyfile(from, to, nil, copyfile_flags_t(COPYFILE_ALL | COPYFILE_DATA_SPARSE)) == 0 else { throw operationPOSIX() }
      }
      #if TEST_BUILD
      hooks.copiedFiles += 1
      #endif
    }
    guard try handle.identity() == expected else { throw LT("files.source_changed") }
    if type == UInt32(S_IFREG), metadata.st_nlink > 1, !relative.isEmpty { links[linkKey] = relative }
  }
  private func remove(_ location: OperationLocation) throws {
    let identity = try location.identity()
    if identity.mode & UInt32(S_IFMT) == UInt32(S_IFDIR) {
      let directory = try OperationDirectory(parent: location.parent, name: location.name)
      guard try directory.identity() == identity else { throw LT("files.source_changed") }
      for name in try directory.names() { try remove(OperationLocation(parent: directory, name: name)) }
      guard unlinkat(location.parent.descriptor, location.name, AT_REMOVEDIR) == 0 else { throw operationPOSIX() }
    } else {
      guard unlinkat(location.parent.descriptor, location.name, 0) == 0 else { throw operationPOSIX() }
    }
  }
}

/// Only changes observed across our own successful namespace operations can
/// advance an expected identity. External ctime changes still fail exact checks.
/// Path compression makes replay linear even for many operations on one inode.
private struct OperationIdentityChanges {
  private var changes = [OperationIdentity: OperationIdentity]()
  mutating func record(from before: OperationIdentity, to after: OperationIdentity) {
    guard before != after, before.hasSamePayload(as: after) else { return }
    changes[before] = after
  }
  mutating func current(_ identity: OperationIdentity) -> OperationIdentity {
    var result = identity, visited = Set<OperationIdentity>()
    while let next = changes[result], visited.insert(result).inserted { result = next }
    for earlier in visited { changes[earlier] = result == earlier ? nil : result }
    return result
  }
}

private enum OperationHistoryVersion {
  static let current = 1
  static func validate(_ record: [String: Any]) throws {
    guard let number = record["schema_version"] as? NSNumber,
      CFGetTypeID(number) != CFBooleanGetTypeID(), number.doubleValue == Double(current) else {
      throw LT("files.undo_version_unsupported")
    }
  }
}

/// A completed operation must carry every authority needed by undo. Missing
/// fields or another format never authorize a filesystem mutation.
private struct OperationUndoAuthority {
  enum Action: String { case rename, move, copy, trash }
  let id: String
  let action: Action
  let source: URL
  let destination: URL
  let expected: OperationIdentity
  let sourceParent: OperationIdentity
  let destinationParent: OperationIdentity
  let sourceParentURL: URL
  let destinationParentURL: URL
  let destinationBookmark: String?

  init(_ record: [String: Any]) throws {
    try OperationHistoryVersion.validate(record)
    func path(_ value: Any?, directory: Bool = false) throws -> URL {
      guard let text = value as? String, text.hasPrefix("/"), !text.contains("\0") else { throw LT("files.undo_authority_unavailable") }
      let url = URL(fileURLWithPath: text, isDirectory: directory)
      guard directory || (url.path != "/" && !["", ".", ".."].contains(url.lastPathComponent)) else { throw LT("files.undo_authority_unavailable") }
      return url
    }
    func identity(_ value: Any?, directory: Bool) throws -> OperationIdentity {
      guard let wire = value as? [String: Any], let stamp = OperationIdentity(wire: wire), stamp.inode != 0 else {
        throw LT("files.undo_authority_unavailable")
      }
      let type = stamp.mode & UInt32(S_IFMT)
      guard directory ? type == UInt32(S_IFDIR) : [UInt32(S_IFREG), UInt32(S_IFDIR), UInt32(S_IFLNK)].contains(type) else {
        throw LT("files.undo_authority_unavailable")
      }
      return stamp
    }
    guard let id = record["id"] as? String, !id.isEmpty,
      let name = record["action"] as? String, let action = Action(rawValue: name) else { throw LT("files.undo_authority_unavailable") }
    self.id = id; self.action = action
    source = try path(record["source"]); destination = try path(record["destination"])
    expected = try identity(record["after"], directory: false)
    sourceParent = try identity(record["source_parent"], directory: true)
    destinationParent = try identity(record["destination_parent"], directory: true)
    sourceParentURL = try path(record["source_parent_path"], directory: true)
    destinationParentURL = try path(record["destination_parent_path"], directory: true)
    if let raw = record["destination_bookmark"] {
      guard let bookmark = raw as? String, let data = Data(base64Encoded: bookmark), !data.isEmpty else { throw LT("files.undo_authority_unavailable") }
      destinationBookmark = bookmark
    } else { destinationBookmark = nil }
  }
}

/// Append-only write-ahead operation history. Intents are durable before the
/// batch starts; results are synchronized once at completion/cancellation. A
/// crash leaves explicit unresolved intents, never a fabricated completed undo.
final class OperationJournal {
  let url: URL
  init(directory: URL) {
    url = directory.appendingPathComponent("file-operations.jsonl")
  }
  func open() throws -> FileHandle {
    let fd = url.path.withCString { Darwin.open($0, O_RDWR | O_CREAT | O_APPEND | O_CLOEXEC | O_NOFOLLOW, 0o600) }
    guard fd >= 0 else { throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno)) }
    let handle = FileHandle(fileDescriptor: fd, closeOnDealloc: true)
    do {
      var metadata = stat()
      guard fstat(fd, &metadata) == 0, UInt32(metadata.st_mode) & UInt32(S_IFMT) == UInt32(S_IFREG) else {
        throw LT("files.invalid_journal")
      }
      // A killed writer can leave an incomplete final line. Delimit it before
      // appending; otherwise it would swallow the next valid result too.
      let end = try handle.seekToEnd()
      if end > 0 {
        try handle.seek(toOffset: end - 1)
        if try handle.read(upToCount: 1) != Data([0x0A]) { try handle.write(contentsOf: Data([0x0A])) }
      }
      return handle
    } catch { try? handle.close(); throw error }
  }
  func append(_ record: [String: Any], to handle: FileHandle) throws {
    var versioned = record
    versioned["schema_version"] = OperationHistoryVersion.current
    var data = try JSONSerialization.data(withJSONObject: versioned, options: [.sortedKeys])
    data.append(0x0A)
    try handle.write(contentsOf: data)
  }
  func history() throws -> [[String: Any]] {
    var ordered = [String](), records = [String: [String: Any]]()
    var identities = OperationIdentityChanges()
    guard FileManager.default.fileExists(atPath: url.path) else { return ordered.compactMap { records[$0] } }
    let handle = try FileHandle(forReadingFrom: url)
    defer { try? handle.close() }
    var pending = Data()
    func consume(_ line: Data) throws {
      guard let row = (try? JSONSerialization.jsonObject(with: line)) as? [String: Any] else { return }
      let kind = row["kind"] as? String ?? "operation"
      try OperationHistoryVersion.validate(row)
      if kind == "identity_change", let before = row["before"] as? [String: Any],
        let after = row["after"] as? [String: Any],
        let original = OperationIdentity(wire: before), let updated = OperationIdentity(wire: after) {
        identities.record(from: original, to: updated)
      } else if let target = row["target_id"] as? String, kind == "undo" || kind == "undo_intent" {
        records[target]?["undone"] = kind == "undo"
        records[target]?["undo_pending"] = kind == "undo_intent"
      } else if let id = row["id"] as? String {
        if records[id] == nil { ordered.append(id) }
        records[id] = row
      }
    }
    while let chunk = try handle.read(upToCount: 65_536), !chunk.isEmpty {
      pending.append(chunk)
      while let newline = pending.firstIndex(of: 0x0A) {
        try consume(Data(pending[..<newline])); pending.removeSubrange(...newline)
      }
      guard pending.count <= 4 * 1024 * 1024 else { throw LT("files.invalid_journal") }
    }
    // An unterminated tail was never acknowledged as durable; ignore it.
    return ordered.compactMap { id in
      guard var row = records[id] else { return nil }
      if let after = row["after"] as? [String: Any], let identity = OperationIdentity(wire: after) {
        row["after"] = identities.current(identity).wire
      }
      return row
    }
  }
}

final class FileOperations {
  let fm = FileManager.default
  let journal: URL
  private let history: OperationJournal
  private let operationLock = NSRecursiveLock()
  private let cancelLock = NSLock()
  private var activeRequests = Set<String>()
  private var cancelled = Set<String>()
  #if TEST_BUILD
  var testHooks = OperationTestHooks()
  #endif

  init(directory: URL) { history = OperationJournal(directory: directory); journal = history.url }
  /// Register before enqueueing work, so cancellation also reaches queued jobs.
  func register(_ id: String) -> Bool {
    cancelLock.lock(); defer { cancelLock.unlock() }
    guard !id.isEmpty, id.utf8.count <= 128, activeRequests.count < 32,
      !activeRequests.contains(id) else { return false }
    activeRequests.insert(id); return true
  }
  func cancel(_ id: String) {
    cancelLock.lock(); defer { cancelLock.unlock() }
    if activeRequests.contains(id) { cancelled.insert(id) }
  }
  private func isCancelled(_ id: String?) -> Bool {
    cancelLock.lock(); defer { cancelLock.unlock() }
    return id.map { cancelled.contains($0) } ?? false
  }
  private func finished(_ id: String?) {
    guard let id else { return }
    cancelLock.lock(); defer { cancelLock.unlock() }
    activeRequests.remove(id); cancelled.remove(id)
  }
  func log() -> [[String: Any]] { (try? history.history()) ?? [] }
  func identity(_ url: URL) -> [String: Any] { (try? OperationIdentity(url).wire) ?? [:] }
  func isLink(_ url: URL) -> Bool { (try? fm.destinationOfSymbolicLink(atPath: url.path)) != nil }
  private func exists(_ url: URL) -> Bool { fm.fileExists(atPath: url.path) || isLink(url) }
  private func validName(_ name: String) -> Bool {
    !name.isEmpty && name != "." && name != ".." && !name.contains("/") && !name.contains("\0")
  }
  private func renameName(_ url: URL, index: Int, count: Int, request: [String: Any]) -> String? {
    if let names = request["new_names"] as? [String] { return names.count == count ? names[index] : nil }
    if count == 1, let name = request["new_name"] as? String { return name }
    guard let rule = request["rename_rule"] as? [String: Any] else { return nil }
    let ext = url.pathExtension
    var stem = ext.isEmpty ? url.lastPathComponent : url.deletingPathExtension().lastPathComponent
    if let search = rule["search"] as? String, !search.isEmpty {
      stem = stem.replacingOccurrences(of: search, with: rule["replace"] as? String ?? "")
    }
    stem = (rule["prefix"] as? String ?? "") + stem + (rule["suffix"] as? String ?? "")
    if let start = (rule["number_start"] as? NSNumber)?.int64Value {
      let (value, overflow) = start.addingReportingOverflow(Int64(index))
      guard !overflow else { return nil }
      let width = max(1, min(12, (rule["number_padding"] as? NSNumber)?.intValue ?? 1))
      let number = String(value)
      stem += (rule["number_separator"] as? String ?? " ") + String(repeating: "0", count: max(0, width - number.count)) + number
    }
    return ext.isEmpty ? stem : stem + "." + ext
  }
  func perform(_ request: [String: Any]) -> [String: Any] {
    operationLock.lock(); defer { operationLock.unlock() }
    let id = request["request_id"] as? String
    defer { finished(id) }
    do { return try performChecked(request) } catch { return localizedErrorResponse(error) }
  }
  private func performChecked(_ request: [String: Any]) throws -> [String: Any] {
    let action = request["action"] as? String ?? ""
    if action == "history" {
      let rows = try history.history().reversed()
      let offset = max(0, (request["offset"] as? NSNumber)?.intValue ?? 0)
      let limit = max(1, min(1000, (request["limit"] as? NSNumber)?.intValue ?? 200))
      return ["success": true, "operations": Array(rows.dropFirst(offset).prefix(limit)), "total": rows.count, "offset": offset]
    }
    if action == "undo" { return try undoChecked() }
    let paths = request["paths"] as? [String] ?? ((request["path"] as? String).map { [$0] } ?? [])
    guard !paths.isEmpty else { throw LT("error.no_files_selected") }
    guard paths.count <= 100_000 else { throw LT("files.batch_limit") }
    guard paths.allSatisfy({ $0.hasPrefix("/") && !$0.contains("\0") }) else { throw LT("error.absolute_path_required") }
    let urls = paths.map { URL(fileURLWithPath: $0).standardizedFileURL }
    guard Set(urls.map(\.path)).count == urls.count else { throw LT("files.repeated_source") }
    if ["open", "reveal", "copy_paths"].contains(action) {
      let work = {
        if action == "reveal" { NSWorkspace.shared.activateFileViewerSelecting(urls) }
        else if action == "copy_paths" {
          NSPasteboard.general.clearContents(); NSPasteboard.general.setString(paths.joined(separator: "\n"), forType: .string)
        } else { for url in urls { NSWorkspace.shared.open(url) } }
      }
      if Thread.isMainThread { work() } else { DispatchQueue.main.sync(execute: work) }
      return ["success": true]
    }
    guard ["rename", "move", "copy", "trash"].contains(action) else { throw LT("error.unknown_file_operation") }
    guard !urls.contains(where: { $0.path == "/" }) else { throw LT("files.protected_root") }
    let policy = request["conflict_policy"] as? String ?? "stop"
    guard ["stop", "skip"].contains(policy) else { throw LT("files.invalid_conflict_policy") }
    let requestID = request["request_id"] as? String
    let skippedPaths = Set(request["skip_paths"] as? [String] ?? [])
    var expected = [String: [String: Any]]()
    if let rows = request["expected"] as? [[String: Any]] {
      for row in rows { if let path = row["path"] as? String { expected[path] = row } }
      guard urls.allSatisfy({ expected[$0.path] != nil }) else { throw LT("files.source_changed") }
    }
    var reviewedParents = [String: [String: Any]]()
    if let rows = request["expected_parents"] as? [[String: Any]] {
      for row in rows { if let path = row["path"] as? String { reviewedParents[path] = row } }
      guard urls.allSatisfy({ reviewedParents[$0.path] != nil }) else { throw LT("files.source_changed") }
    }
    func sameDirectory(_ current: OperationIdentity, _ wire: [String: Any]?) -> Bool {
      guard let wire, let expected = OperationIdentity(wire: wire) else { return false }
      return current.device == expected.device && current.inode == expected.inode
        && current.mode & UInt32(S_IFMT) == UInt32(S_IFDIR)
    }
    var destination: URL?
    if action == "move" || action == "copy" {
      guard let path = request["destination"] as? String, path.hasPrefix("/"), !path.contains("\0") else { throw LT("error.destination_required") }
      guard let directory = try? OperationDirectory(URL(fileURLWithPath: path)) else { throw LT("error.destination_folder_missing") }
      destination = directory.url
    }
    let overrideNames = request["target_names"] as? [String: String] ?? [:]
    let sources = Set(urls.map(\.path).filter { !skippedPaths.contains($0) })
    var overlapping = Set<String>()
    for url in urls where sources.contains(url.path) {
      var ancestor = url.deletingLastPathComponent()
      while ancestor.path != "/" {
        if sources.contains(ancestor.path) { overlapping.insert(url.path); overlapping.insert(ancestor.path) }
        ancestor.deleteLastPathComponent()
      }
    }
    // Bound descriptor use while reusing common parents for large batches.
    // A cache entry is still an authority, not a path-validation result.
    var previewDirectories = [String: OperationDirectory]()
    func previewDirectory(_ url: URL) throws -> OperationDirectory {
      if let existing = previewDirectories[url.path] { return existing }
      let directory = try OperationDirectory(url)
      if previewDirectories.count >= 128 { previewDirectories.removeAll(keepingCapacity: true) }
      previewDirectories[url.path] = directory; return directory
    }
    var preview = [[String: Any]](), stamps = [String: OperationIdentity]()
    var parents = [String: OperationIdentity](), destinationParents = [String: OperationIdentity]()
    var sourceParentPaths = [String: String](), destinationParentPaths = [String: String]()
    var owners = [String: [Int]](), caseSensitivity = [String: Bool]()
    for (index, source) in urls.enumerated() {
      if isCancelled(requestID) { throw LT("status.cancelled") }
      var messages = [LocalizedText]()
      var row: [String: Any] = ["source": source.path, "id": UUID().uuidString]
      do {
        let location = OperationLocation(parent: try previewDirectory(source.deletingLastPathComponent()), name: source.lastPathComponent)
        let stamp = try location.identity(); stamps[source.path] = stamp
        let parent = try location.parent.identity()
        parents[source.path] = parent; sourceParentPaths[source.path] = location.parent.url.path
        row["identity"] = stamp.wire; row["source_parent"] = parent.wire
        if let reviewed = reviewedParents[source.path], !sameDirectory(parent, reviewed["source_parent"] as? [String: Any]) {
          messages.append(LT("files.source_changed"))
        }
        if let wanted = expected[source.path], !stamp.matches(wanted) { messages.append(LT("files.source_changed")) }
        if stamp.mode & UInt32(S_IFMT) != UInt32(S_IFREG)
          && stamp.mode & UInt32(S_IFMT) != UInt32(S_IFDIR)
          && stamp.mode & UInt32(S_IFMT) != UInt32(S_IFLNK) { messages.append(LT("files.unsupported_type")) }
        if overlapping.contains(source.path) { messages.append(LT("files.overlapping_sources")) }
      } catch { messages.append(LT("error.file_not_found", .text(source.path))) }
      var target: URL?
      if action == "rename" || destination != nil {
        let name = overrideNames[source.path] ?? (action == "rename"
          ? renameName(source, index: index, count: urls.count, request: request) : source.lastPathComponent)
        guard let name, validName(name) else { throw LT("error.rename_selection_or_name") }
        target = (destination ?? source.deletingLastPathComponent()).appendingPathComponent(name).standardizedFileURL
      }
      if let target {
        row["destination"] = target.path
        if let parent = try? previewDirectory(target.deletingLastPathComponent()), let stamp = try? parent.identity() {
          destinationParents[source.path] = stamp; destinationParentPaths[source.path] = parent.url.path
          row["destination_parent"] = stamp.wire
          if let reviewed = reviewedParents[source.path],
            (!sameDirectory(stamp, reviewed["destination_parent"] as? [String: Any]) || reviewed["destination"] as? String != target.path) {
            messages.append(LT("files.source_changed"))
          }
        } else { messages.append(LT("error.destination_folder_missing")) }
        if target.path == source.path && action == "rename" { row["noop"] = true }
        else {
          if exists(target) { messages.append(LT("error.destination_exists", .text(target.path))) }
          let physicalSource = source.resolvingSymlinksInPath().path
          let physicalTarget = target.deletingLastPathComponent().resolvingSymlinksInPath().appendingPathComponent(target.lastPathComponent).path
          if physicalTarget == physicalSource || physicalTarget.hasPrefix(physicalSource + "/") {
            messages.append(LT("error.folder_inside_itself", .text(source.path)))
          }
          let parent = target.deletingLastPathComponent()
          if caseSensitivity[parent.path] == nil {
            caseSensitivity[parent.path] = (try? parent.resourceValues(forKeys: [.volumeSupportsCaseSensitiveNamesKey]).volumeSupportsCaseSensitiveNames) ?? false
          }
          let normalized = target.path.decomposedStringWithCanonicalMapping
          let key = caseSensitivity[parent.path] == true ? normalized : normalized.lowercased()
          if !skippedPaths.contains(source.path) { owners[key, default: []].append(index) }
        }
      } else { row = LT("files.trash_destination").adding(to: row, field: "destination") }
      row["conflict_messages"] = messages.map(\.wire)
      row["conflicts"] = messages.map { $0.render() }
      row["skipped"] = skippedPaths.contains(source.path)
      preview.append(row)
    }
    for indexes in owners.values where indexes.count > 1 {
      for index in indexes {
        let message = LT("error.duplicate_destination_name", .text(URL(fileURLWithPath: preview[index]["destination"] as? String ?? "").lastPathComponent))
        preview[index]["conflicts"] = (preview[index]["conflicts"] as? [String] ?? []) + [message.render()]
        preview[index]["conflict_messages"] = (preview[index]["conflict_messages"] as? [[String: Any]] ?? []) + [message.wire]
      }
    }
    let conflicts = preview.filter { $0["skipped"] as? Bool != true }.flatMap { $0["conflicts"] as? [String] ?? [] }
    let conflictMessages = preview.filter { $0["skipped"] as? Bool != true }
      .flatMap { $0["conflict_messages"] as? [[String: Any]] ?? [] }
    let normalizedExpected: [[String: Any]] = urls.compactMap { source in
      stamps[source.path].map { $0.wire.merging(["path": source.path]) { _, new in new } }
    }
    let normalizedParents: [[String: Any]] = preview.compactMap { row in
      guard let source = row["source"] as? String, let parent = row["source_parent"] else { return nil }
      var value: [String: Any] = ["path": source, "source_parent": parent]
      if let target = row["destination"] as? String, let parent = row["destination_parent"] {
        value["destination"] = target; value["destination_parent"] = parent
      }
      return value
    }
    if request["dry_run"] as? Bool == true {
      return ["success": true, "conflicts": conflicts, "conflict_messages": conflictMessages,
        "preview": preview, "expected": normalizedExpected, "expected_parents": normalizedParents, "warnings": []]
    }
    guard policy != "stop" || conflicts.isEmpty else {
      return ["success": false, "error": conflicts.joined(separator: "\n"), "error_messages": conflictMessages,
        "conflicts": conflicts, "conflict_messages": conflictMessages, "preview": preview]
    }
    let eligible = preview.filter { $0["skipped"] as? Bool != true && $0["noop"] as? Bool != true && ($0["conflicts"] as? [String] ?? []).isEmpty }
    guard !eligible.isEmpty else {
      return LT("files.completed_count", .integer(0)).adding(to: ["success": true, "completed": 0, "skipped": preview, "results": preview])
    }
    let keeperRows = request["keepers"] as? [[String: Any]] ?? []
    let keeperGroups = Dictionary(grouping: keeperRows, by: { $0["group"] as? String ?? "" })
    let sourceGroups = request["groups"] as? [String: String] ?? [:]
    let operatedSources = Set(eligible.compactMap { $0["source"] as? String })
    // Duplicate cleanup must leave at least one independently verified survivor.
    for keeper in keeperRows {
      guard let path = keeper["path"] as? String, !operatedSources.contains(path),
        try OperationLocation(URL(fileURLWithPath: path)).identity().matches(keeper) else { throw LT("files.survivor_changed") }
    }
    // Allocate one protected workspace per volume, not one descriptor per
    // selected entry. Every capture/copy pathname is durable before mutation.
    var workspaces = [UInt64: OperationWorkspace]()
    let protectedSources = urls.map { source in
      sourceParentPaths[source.path].map { URL(fileURLWithPath: $0).appendingPathComponent(source.lastPathComponent) } ?? source
    }
    previewDirectories.removeAll()
    func workspace(_ location: OperationLocation) throws -> OperationWorkspace {
      let device = try location.parent.identity().device
      if let existing = workspaces[device] { return existing }
      let created = try OperationWorkspace(preferred: journal.deletingLastPathComponent(), source: location, excluding: protectedSources)
      workspaces[device] = created; return created
    }
    for row in eligible {
      let source = row["source"] as! String
      if action != "copy", let parent = parents[source], workspaces[parent.device] == nil,
        let path = sourceParentPaths[source] {
        let directory = try OperationDirectory(URL(fileURLWithPath: path), resolveAliases: false)
        _ = try workspace(OperationLocation(parent: directory, name: URL(fileURLWithPath: source).lastPathComponent))
      }
      if action != "trash", let parent = destinationParents[source], workspaces[parent.device] == nil,
        let path = destinationParentPaths[source], let destination = row["destination"] as? String {
        let directory = try OperationDirectory(URL(fileURLWithPath: path), resolveAliases: false)
        _ = try workspace(OperationLocation(parent: directory, name: URL(fileURLWithPath: destination).lastPathComponent))
      }
    }
    let handle = try history.open()
    defer { try? handle.close() }
    for row in eligible {
      var intent = row; intent["kind"] = "intent"; intent["action"] = action
      let source = row["source"] as! String, operationID = row["id"] as! String
      intent["before"] = row["identity"]; intent["source_parent"] = parents[source]?.wire
      intent["source_parent_path"] = sourceParentPaths[source]
      if let device = parents[source]?.device, action != "copy" {
        intent["staging"] = workspaces[device]?.path(operationID, leaf: URL(fileURLWithPath: source).lastPathComponent, preservingName: action == "trash")
      }
      if let device = destinationParents[source]?.device, let destination = row["destination"] as? String {
        intent["copy_staging"] = workspaces[device]?.path(operationID + "-copy", leaf: URL(fileURLWithPath: destination).lastPathComponent)
      }
      intent["time"] = Date().timeIntervalSince1970
      try history.append(intent, to: handle)
    }
    try handle.synchronize()
    var results = [[String: Any]](), completed = 0, failures = 0, stopped = false
    var identities = OperationIdentityChanges()
    for row in preview {
      var result = row
      let source = row["source"] as? String ?? ""
      if row["skipped"] as? Bool == true || row["noop"] as? Bool == true || !(row["conflicts"] as? [String] ?? []).isEmpty {
        result["status"] = row["noop"] as? Bool == true ? "unchanged" : "skipped"; results.append(result); continue
      }
      if stopped || isCancelled(requestID) {
        result["status"] = "not_started"; result["kind"] = "not_started"
        do { try history.append(result, to: handle) }
        catch { failures += 1; result["journal_error"] = error.localizedDescription }
        results.append(result); continue
      }
      let from = URL(fileURLWithPath: source)
      do {
        guard let originalStamp = stamps[source] else { throw LT("files.source_changed") }
        let expectedStamp = identities.current(originalStamp)
        guard let sourceParentPath = sourceParentPaths[source], let previousParent = parents[source] else { throw LT("files.source_changed") }
        let sourceParent = try OperationDirectory(URL(fileURLWithPath: sourceParentPath), resolveAliases: false)
        guard sameDirectory(try sourceParent.identity(), previousParent.wire) else { throw LT("files.source_changed") }
        let sourceLocation = OperationLocation(parent: sourceParent, name: from.lastPathComponent)
        let sourceHandle = try OperationHandle(sourceLocation)
        guard try sourceHandle.identity() == expectedStamp else { throw LT("files.source_changed") }
        var target: OperationLocation?
        if action != "trash" {
          guard let path = destinationParentPaths[source], let expectedParent = destinationParents[source],
            let destination = row["destination"] as? String else { throw LT("files.source_changed") }
          let parent = try OperationDirectory(URL(fileURLWithPath: path), resolveAliases: false)
          guard sameDirectory(try parent.identity(), expectedParent.wire) else { throw LT("files.source_changed") }
          target = OperationLocation(parent: parent, name: URL(fileURLWithPath: destination).lastPathComponent)
        }
        #if TEST_BUILD
        try testHooks.afterParentsOpened?()
        let transfer = OperationTransfer(cancelled: { self.isCancelled(requestID) }, hooks: testHooks)
        defer { testHooks = transfer.hooks }
        #else
        let transfer = OperationTransfer(cancelled: { self.isCancelled(requestID) })
        #endif
        for keeper in keeperGroups[sourceGroups[source] ?? ""] ?? [] {
          guard let path = keeper["path"] as? String, try OperationLocation(URL(fileURLWithPath: path)).identity().matches(keeper) else { throw LT("files.survivor_changed") }
        }
        // Both successful and rolled-back captures change ctime. Persist only
        // changes to the pinned authorized inode, including failed operations.
        let outcome: OperationOutcome
        do {
          outcome = try transfer.perform(action: action, source: sourceLocation, sourceHandle: sourceHandle,
            expected: expectedStamp, target: target, sourceWorkspace: workspaces[previousParent.device],
            targetWorkspace: destinationParents[source].flatMap { workspaces[$0.device] }, id: row["id"] as! String)
        } catch {
          let after = try sourceHandle.identity()
          if expectedStamp != after, expectedStamp.hasSamePayload(as: after) {
            try history.append(["kind": "identity_change", "before": expectedStamp.wire, "after": after.wire], to: handle)
            identities.record(from: expectedStamp, to: after)
          }
          throw error
        }
        completed += 1
        let after = try sourceHandle.identity()
        if action != "copy", expectedStamp != after, expectedStamp.hasSamePayload(as: after) {
          try history.append(["kind": "identity_change", "before": expectedStamp.wire, "after": after.wire], to: handle)
          identities.record(from: expectedStamp, to: after)
        }
        let actualParentURL = try outcome.location.parent.currentURL()
        result["destination"] = actualParentURL.appendingPathComponent(outcome.location.name).path; result["after"] = outcome.identity.wire
        result["destination_parent_path"] = actualParentURL.path
        result["destination_bookmark"] = outcome.destinationBookmark
        result["before"] = expectedStamp.wire; result["kind"] = "operation"
        result["source_parent"] = previousParent.wire; result["source_parent_path"] = sourceParentPath
        result["destination_parent"] = try outcome.location.parent.identity().wire
        result["action"] = action; result["time"] = Date().timeIntervalSince1970
        result["undone"] = false; result["status"] = "completed"
      } catch {
        failures += 1; result["kind"] = "failed"; result["status"] = "needs_review"
        result["error"] = error.localizedDescription; result["action"] = action
        if let failure = error as? OperationFailure {
          result["recovery_paths"] = failure.recoveryPaths
          if let destination = failure.completedDestination {
            completed += 1; result["mutation_completed"] = true; result["destination"] = destination
            result["after"] = failure.completedIdentity?.wire
          }
        }
        // FileManager can partially copy a directory before reporting an error.
        // Leave the intent and paths visible; do not delete or retry blindly.
        stopped = policy == "stop"
      }
      do { try history.append(result, to: handle) }
      catch { failures += 1; stopped = true; result["journal_error"] = error.localizedDescription }
      results.append(result)
    }
    do { try handle.synchronize() } catch { failures += 1; results.append(["source": journal.path, "error": error.localizedDescription, "status": "journal_error"]) }
    let wasCancelled = isCancelled(requestID)
    let payload: [String: Any] = ["success": failures == 0 && !wasCancelled, "completed": completed,
      "cancelled": wasCancelled, "results": results, "skipped": results.filter { $0["status"] as? String == "skipped" },
      "failures": results.filter { $0["error"] != nil || $0["journal_error"] != nil }]
    return (failures == 0 && !wasCancelled ? LT("files.completed_count", .integer(completed)) : LT("files.partial_completion_notice")).adding(to: payload)
  }
  func undo() -> [String: Any] { perform(["action": "undo"]) }
  private func undoChecked() throws -> [String: Any] {
    let rows = try history.history()
    guard let record = rows.last(where: { $0["undone"] as? Bool != true && ($0["kind"] == nil || $0["kind"] as? String == "operation") }) else { throw LT("error.nothing_to_undo") }
    guard record["undo_pending"] as? Bool != true else { throw LT("files.undo_needs_review") }
    let authority = try OperationUndoAuthority(record)
    let id = authority.id
    let targetScope = try authority.destinationBookmark.map { try OperationSecurityScope(bookmark: $0) }
    defer { withExtendedLifetime(targetScope) {} }
    let targetDirectory = try OperationDirectory(authority.destinationParentURL, resolveAliases: false)
    let originalDirectory = try OperationDirectory(authority.sourceParentURL, resolveAliases: false)
    let currentTarget = try targetDirectory.identity(), currentOriginal = try originalDirectory.identity()
    guard currentTarget.device == authority.destinationParent.device, currentTarget.inode == authority.destinationParent.inode,
      currentOriginal.device == authority.sourceParent.device, currentOriginal.inode == authority.sourceParent.inode else { throw LT("error.undo_destination_changed") }
    let target = OperationLocation(parent: targetDirectory, name: authority.destination.lastPathComponent)
    let original = OperationLocation(parent: originalDirectory, name: authority.source.lastPathComponent)
    let targetHandle = try OperationHandle(target)
    let before = try targetHandle.identity()
    guard before == authority.expected else { throw LT("error.undo_destination_changed") }
    let removingCopy = authority.action == .copy
    if !removingCopy, try original.exists() { throw LT("error.undo_original_occupied") }
    let sourceWorkspace = try OperationWorkspace(preferred: journal.deletingLastPathComponent(), source: target, excluding: [target.url, original.url])
    let targetWorkspace = !removingCopy && currentTarget.device != currentOriginal.device
      ? try OperationWorkspace(preferred: journal.deletingLastPathComponent(), source: original, excluding: [target.url, original.url]) : nil
    let operationID = "undo-" + UUID().uuidString
    let handle = try history.open(); defer { try? handle.close() }
    var intent: [String: Any] = ["kind": "undo_intent", "target_id": id, "time": Date().timeIntervalSince1970,
      "staging": sourceWorkspace.path(operationID, leaf: target.name, preservingName: removingCopy)]
    if let targetWorkspace { intent["copy_staging"] = targetWorkspace.path(operationID + "-copy", leaf: original.name) }
    try history.append(intent, to: handle)
    try handle.synchronize()
    #if TEST_BUILD
    try testHooks.afterParentsOpened?()
    let transfer = OperationTransfer(cancelled: { false }, hooks: testHooks)
    defer { testHooks = transfer.hooks }
    #else
    let transfer = OperationTransfer(cancelled: { false })
    #endif
    let outcome: OperationOutcome
    do {
      outcome = try transfer.perform(action: removingCopy ? "trash" : "move", source: target, sourceHandle: targetHandle,
        expected: before, target: removingCopy ? nil : original, sourceWorkspace: sourceWorkspace,
        targetWorkspace: targetWorkspace, id: operationID)
    } catch {
      if let after = try? targetHandle.identity(), before != after, before.hasSamePayload(as: after) {
        try history.append(["kind": "identity_change", "before": before.wire, "after": after.wire], to: handle)
      }
      try handle.synchronize()
      throw error
    }
    let after = try targetHandle.identity()
    if before != after, before.hasSamePayload(as: after) {
      try history.append(["kind": "identity_change", "before": before.wire, "after": after.wire], to: handle)
    }
    var undone: [String: Any] = ["kind": "undo", "target_id": id, "time": Date().timeIntervalSince1970,
      "destination": try outcome.location.parent.currentURL().appendingPathComponent(outcome.location.name).path]
    undone["destination_bookmark"] = outcome.destinationBookmark
    try history.append(undone, to: handle)
    try handle.synchronize()
    return LT("files.last_file_operation_undone").adding(to: ["success": true].merging(undone) { _, next in next })
  }
}
