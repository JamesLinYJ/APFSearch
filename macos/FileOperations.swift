import AppKit
import Darwin
import Foundation

/// Device/inode plus both nanosecond timestamps. Never treat a failed stat as
/// a zero-valued identity, and never follow the final symlink for file actions.
struct OperationIdentity: Equatable {
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
    device = UInt64(UInt32(bitPattern: value.st_dev)); inode = UInt64(value.st_ino)
    size = Int64(value.st_size); mode = UInt32(value.st_mode)
    modified = Int64(value.st_mtimespec.tv_sec); modifiedNS = Int64(value.st_mtimespec.tv_nsec)
    changed = Int64(value.st_ctimespec.tv_sec); changedNS = Int64(value.st_ctimespec.tv_nsec)
  }
  var wire: [String: Any] {
    ["device_id": device, "file_id": inode, "size": size, "mode": mode,
      "modified": modified, "modified_nsec": modifiedNS, "changed": changed, "changed_nsec": changedNS]
  }
  func matches(_ value: [String: Any]) -> Bool {
    // Imported legacy operation records have a weaker identity. They remain
    // readable, but a missing inode never authorizes an undo.
    if let legacy = value["inode"] as? String {
      return !legacy.isEmpty && legacy == String(inode)
        && (value["size"] as? NSNumber)?.int64Value == size
        && abs(((value["modified"] as? NSNumber)?.doubleValue ?? -.infinity)
          - (Double(modified) + Double(modifiedNS) / 1_000_000_000)) < 0.000001
    }
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

/// Append-only write-ahead operation history. Intents are durable before the
/// batch starts; results are synchronized once at completion/cancellation. A
/// crash leaves explicit unresolved intents, never a fabricated completed undo.
final class OperationJournal {
  let url: URL
  let legacyURL: URL
  init(directory: URL) {
    url = directory.appendingPathComponent("file-operations.jsonl")
    legacyURL = directory.appendingPathComponent("file-operations.json")
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
    var data = try JSONSerialization.data(withJSONObject: record, options: [.sortedKeys])
    data.append(0x0A)
    try handle.write(contentsOf: data)
  }
  func history() throws -> [[String: Any]] {
    var ordered = [String](), records = [String: [String: Any]]()
    if FileManager.default.fileExists(atPath: legacyURL.path) {
      let data = try Data(contentsOf: legacyURL)
      guard let old = try JSONSerialization.jsonObject(with: data) as? [[String: Any]] else {
        throw LT("files.invalid_journal")
      }
      for row in old {
        guard let id = row["id"] as? String else { continue }
        if records[id] == nil { ordered.append(id) }
        records[id] = row
      }
    }
    guard FileManager.default.fileExists(atPath: url.path) else { return ordered.compactMap { records[$0] } }
    let handle = try FileHandle(forReadingFrom: url)
    defer { try? handle.close() }
    var pending = Data()
    func consume(_ line: Data) {
      guard let row = (try? JSONSerialization.jsonObject(with: line)) as? [String: Any] else { return }
      let kind = row["kind"] as? String ?? "operation"
      if let target = row["target_id"] as? String, kind == "undo" || kind == "undo_intent" {
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
        consume(Data(pending[..<newline])); pending.removeSubrange(...newline)
      }
      guard pending.count <= 4 * 1024 * 1024 else { throw LT("files.invalid_journal") }
    }
    // An unterminated tail was never acknowledged as durable; ignore it.
    return ordered.compactMap { records[$0] }
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
    var destination: URL?
    if action == "move" || action == "copy" {
      guard let path = request["destination"] as? String, path.hasPrefix("/"), !path.contains("\0") else { throw LT("error.destination_required") }
      var isDirectory: ObjCBool = false
      guard fm.fileExists(atPath: path, isDirectory: &isDirectory), isDirectory.boolValue else { throw LT("error.destination_folder_missing") }
      destination = URL(fileURLWithPath: path).resolvingSymlinksInPath().standardizedFileURL
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
    var preview = [[String: Any]](), stamps = [String: OperationIdentity]()
    var parents = [String: OperationIdentity](), destinationParents = [String: OperationIdentity]()
    var owners = [String: [Int]](), caseSensitivity = [String: Bool]()
    for (index, source) in urls.enumerated() {
      if isCancelled(requestID) { throw LT("status.cancelled") }
      var messages = [LocalizedText]()
      var row: [String: Any] = ["source": source.path, "id": UUID().uuidString]
      do {
        let stamp = try OperationIdentity(source); stamps[source.path] = stamp
        parents[source.path] = try OperationIdentity(source.deletingLastPathComponent().resolvingSymlinksInPath())
        row["identity"] = stamp.wire
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
        let targetParent = target.deletingLastPathComponent().resolvingSymlinksInPath()
        if let stamp = try? OperationIdentity(targetParent) { destinationParents[source.path] = stamp }
        else { messages.append(LT("error.destination_folder_missing")) }
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
    let normalizedExpected: [[String: Any]] = urls.compactMap { source in
      stamps[source.path].map { $0.wire.merging(["path": source.path]) { _, new in new } }
    }
    if request["dry_run"] as? Bool == true {
      return ["success": true, "conflicts": conflicts, "preview": preview, "expected": normalizedExpected, "warnings": []]
    }
    guard policy != "stop" || conflicts.isEmpty else {
      return ["success": false, "error": conflicts.joined(separator: "\n"), "conflicts": conflicts, "preview": preview]
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
        try OperationIdentity(URL(fileURLWithPath: path)).matches(keeper) else { throw LT("files.survivor_changed") }
    }
    let handle = try history.open()
    defer { try? handle.close() }
    for row in eligible {
      var intent = row; intent["kind"] = "intent"; intent["action"] = action
      intent["before"] = row["identity"]; intent["source_parent"] = parents[row["source"] as? String ?? ""]?.wire; intent["time"] = Date().timeIntervalSince1970
      try history.append(intent, to: handle)
    }
    try handle.synchronize()
    var results = [[String: Any]](), completed = 0, failures = 0, stopped = false
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
        guard let expectedStamp = stamps[source], try OperationIdentity(from) == expectedStamp else { throw LT("files.source_changed") }
        let parent = try OperationIdentity(from.deletingLastPathComponent().resolvingSymlinksInPath())
        guard let previousParent = parents[source], parent.device == previousParent.device,
          parent.inode == previousParent.inode else { throw LT("files.source_changed") }
        for keeper in keeperGroups[sourceGroups[source] ?? ""] ?? [] {
          guard let path = keeper["path"] as? String, try OperationIdentity(URL(fileURLWithPath: path)).matches(keeper) else { throw LT("files.survivor_changed") }
        }
        let actual: URL
        if action == "trash" {
          var output: NSURL?
          try fm.trashItem(at: from, resultingItemURL: &output)
          completed += 1
          guard let target = output as URL? else { throw LT("error.trash_location_missing") }
          actual = target
        } else {
          actual = URL(fileURLWithPath: row["destination"] as? String ?? "")
          guard !exists(actual) else { throw LT("error.destination_exists", .text(actual.path)) }
          let actualParent = try OperationIdentity(actual.deletingLastPathComponent().resolvingSymlinksInPath())
          guard let expectedParent = destinationParents[source], actualParent.device == expectedParent.device,
            actualParent.inode == expectedParent.inode else { throw LT("files.source_changed") }
          if action == "copy" { try fm.copyItem(at: from, to: actual) } else { try fm.moveItem(at: from, to: actual) }
          completed += 1
        }
        result["destination"] = actual.path; result["after"] = try OperationIdentity(actual).wire
        result["before"] = expectedStamp.wire; result["kind"] = "operation"
        result["source_parent"] = previousParent.wire
        result["action"] = action; result["time"] = Date().timeIntervalSince1970
        result["undone"] = false; result["status"] = "completed"
      } catch {
        failures += 1; result["kind"] = "failed"; result["status"] = "needs_review"
        result["error"] = error.localizedDescription; result["action"] = action
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
    guard let id = record["id"] as? String, let dest = record["destination"] as? String,
      let source = record["source"] as? String, let expected = record["after"] as? [String: Any] else { throw LT("error.operation_record_incomplete") }
    let target = URL(fileURLWithPath: dest), original = URL(fileURLWithPath: source)
    guard try OperationIdentity(target).matches(expected) else { throw LT("error.undo_destination_changed") }
    if let parent = record["source_parent"] as? [String: Any] {
      let current = try OperationIdentity(original.deletingLastPathComponent().resolvingSymlinksInPath())
      guard (parent["device_id"] as? NSNumber)?.uint64Value == current.device,
        (parent["file_id"] as? NSNumber)?.uint64Value == current.inode else { throw LT("error.undo_destination_changed") }
    }
    if record["action"] as? String != "copy", exists(original) { throw LT("error.undo_original_occupied") }
    let handle = try history.open(); defer { try? handle.close() }
    try history.append(["kind": "undo_intent", "target_id": id, "time": Date().timeIntervalSince1970], to: handle)
    try handle.synchronize()
    if record["action"] as? String == "copy" { try fm.trashItem(at: target, resultingItemURL: nil) }
    else { try fm.moveItem(at: target, to: original) }
    try history.append(["kind": "undo", "target_id": id, "time": Date().timeIntervalSince1970], to: handle)
    try handle.synchronize()
    return LT("files.last_file_operation_undone").adding(to: ["success": true])
  }
}
