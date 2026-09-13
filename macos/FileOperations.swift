import AppKit
import Foundation

final class FileOperations {
  let fm = FileManager.default
  let journal: URL
  let legacyJournal: URL
  private let cancelLock = NSLock()
  private var cancelled = Set<String>()

  init(directory: URL) {
    journal = directory.appendingPathComponent("file-operations.jsonl")
    legacyJournal = directory.appendingPathComponent("file-operations.json")
  }

  func cancel(_ requestID: String) {
    cancelLock.lock(); cancelled.insert(requestID); cancelLock.unlock()
  }
  private func isCancelled(_ requestID: String?) -> Bool {
    guard let requestID else { return false }
    cancelLock.lock(); defer { cancelLock.unlock() }
    return cancelled.contains(requestID)
  }
  private func clearCancellation(_ requestID: String?) {
    guard let requestID else { return }
    cancelLock.lock(); cancelled.remove(requestID); cancelLock.unlock()
  }

  private func legacyLog() -> [[String: Any]] {
    guard let data = try? Data(contentsOf: legacyJournal) else { return [] }
    return (try? JSONSerialization.jsonObject(with: data)) as? [[String: Any]] ?? []
  }
  func log() -> [[String: Any]] {
    var rows = legacyLog()
    guard let data = try? Data(contentsOf: journal), !data.isEmpty else { return rows }
    var byID = Dictionary(uniqueKeysWithValues: rows.compactMap { row in
      (row["id"] as? String).map { ($0, row) }
    })
    var order = rows.compactMap { $0["id"] as? String }
    for line in data.split(separator: 0x0A) where !line.isEmpty {
      guard let object = try? JSONSerialization.jsonObject(with: Data(line)),
        let record = object as? [String: Any]
      else { continue }
      if record["kind"] as? String == "undo", let target = record["target_id"] as? String {
        byID[target]?["undone"] = true
        continue
      }
      guard let id = record["id"] as? String else { continue }
      if byID[id] == nil { order.append(id) }
      byID[id] = record
    }
    rows = order.compactMap { byID[$0] }
    return rows
  }

  private func openJournal() throws -> FileHandle {
    if !fm.fileExists(atPath: journal.path) {
      guard fm.createFile(atPath: journal.path, contents: nil) else {
        throw NSError(domain: NSCocoaErrorDomain, code: NSFileWriteUnknownError)
      }
    }
    let handle = try FileHandle(forWritingTo: journal)
    try handle.seekToEnd()
    return handle
  }
  private func writeRecord(_ record: [String: Any], to handle: FileHandle) throws {
    var data = try JSONSerialization.data(withJSONObject: record, options: [.sortedKeys])
    data.append(0x0A)
    try handle.write(contentsOf: data)
  }
  private func appendRecords(_ records: [[String: Any]]) throws {
    guard !records.isEmpty else { return }
    let handle = try openJournal()
    defer { try? handle.close() }
    for record in records { try writeRecord(record, to: handle) }
    try handle.synchronize()
  }

  func identity(_ url: URL) -> [String: Any] {
    let attributes = try? fm.attributesOfItem(atPath: url.path)
    return [
      "inode": (attributes?[.systemFileNumber] as? NSNumber)?.stringValue ?? "",
      "modified": (attributes?[.modificationDate] as? Date)?.timeIntervalSince1970 ?? 0,
      "size": (attributes?[.size] as? NSNumber)?.uint64Value ?? 0,
    ]
  }
  func error(_ message: String) -> [String: Any] { ["success": false, "error": message] }
  func error(_ text: LocalizedText) -> [String: Any] { localizedErrorResponse(text) }
  func isLink(_ url: URL) -> Bool { (try? fm.destinationOfSymbolicLink(atPath: url.path)) != nil }
  private func exists(_ url: URL) -> Bool { fm.fileExists(atPath: url.path) || isLink(url) }

  private func validName(_ name: String) -> Bool {
    !name.isEmpty && !name.contains("/") && !name.contains("\0") && name != "." && name != ".."
  }
  private func renameName(_ url: URL, index: Int, count: Int, request: [String: Any]) -> String? {
    if let names = request["new_names"] as? [String], names.count == count { return names[index] }
    if count == 1, let name = request["new_name"] as? String { return name }
    guard let rule = request["rename_rule"] as? [String: Any] else { return nil }
    let original = url.lastPathComponent
    let ext = url.pathExtension
    var stem = ext.isEmpty ? original : url.deletingPathExtension().lastPathComponent
    if let search = rule["search"] as? String, !search.isEmpty {
      stem = stem.replacingOccurrences(of: search, with: rule["replace"] as? String ?? "")
    }
    stem = (rule["prefix"] as? String ?? "") + stem + (rule["suffix"] as? String ?? "")
    if let start = (rule["number_start"] as? NSNumber)?.intValue {
      let width = max(1, min(12, (rule["number_padding"] as? NSNumber)?.intValue ?? 1))
      let number = String(format: "%0*d", width, start + index)
      stem += (rule["number_separator"] as? String ?? " ") + number
    }
    return ext.isEmpty ? stem : stem + "." + ext
  }

  func perform(_ request: [String: Any]) -> [String: Any] {
    let action = request["action"] as? String ?? ""
    if action == "history" { return ["success": true, "operations": log()] }
    if action == "undo" { return undo() }
    let paths = request["paths"] as? [String] ?? ((request["path"] as? String).map { [$0] } ?? [])
    guard !paths.isEmpty else { return error(LT("error.no_files_selected")) }
    guard paths.allSatisfy({ $0.hasPrefix("/") && !$0.contains("\0") }) else {
      return error(LT("error.absolute_path_required"))
    }
    let urls = paths.map { URL(fileURLWithPath: $0).standardizedFileURL }
    if action == "open" || action == "reveal" || action == "copy_paths" {
      DispatchQueue.main.sync {
        if action == "reveal" {
          NSWorkspace.shared.activateFileViewerSelecting(urls)
        } else if action == "copy_paths" {
          NSPasteboard.general.clearContents()
          NSPasteboard.general.setString(paths.joined(separator: "\n"), forType: .string)
        } else {
          for url in urls { NSWorkspace.shared.open(url) }
        }
      }
      return ["success": true]
    }
    guard ["rename", "move", "copy", "trash"].contains(action) else {
      return error(LT("error.unknown_file_operation"))
    }

    let requestID = request["request_id"] as? String
    defer { clearCancellation(requestID) }
    let policy = request["conflict_policy"] as? String ?? "stop"
    guard ["stop", "skip"].contains(policy) else { return error("Unsupported conflict policy") }

    var destinationDirectory: URL?
    if action == "move" || action == "copy" {
      guard let destination = request["destination"] as? String, destination.hasPrefix("/") else {
        return error(LT("error.destination_required"))
      }
      var isDirectory: ObjCBool = false
      guard fm.fileExists(atPath: destination, isDirectory: &isDirectory), isDirectory.boolValue else {
        return error(LT("error.destination_folder_missing"))
      }
      destinationDirectory = URL(fileURLWithPath: destination).standardizedFileURL
    }

    var preview = [[String: Any]]()
    var destinationOwners = [String: Int]()
    for (index, url) in urls.enumerated() {
      var row: [String: Any] = ["source": url.path]
      var rowConflicts = [LocalizedText]()
      if !exists(url) { rowConflicts.append(LT("error.file_not_found", .text(url.path))) }
      var target: URL?
      if action == "rename" {
        guard let name = renameName(url, index: index, count: urls.count, request: request), validName(name) else {
          return error(LT("error.rename_selection_or_name"))
        }
        target = url.deletingLastPathComponent().appendingPathComponent(name).standardizedFileURL
      } else if let destinationDirectory {
        target = destinationDirectory.appendingPathComponent(url.lastPathComponent).standardizedFileURL
      }
      if let target {
        row["destination"] = target.path
        if target.path == url.path || target.path.hasPrefix(url.path + "/") {
          rowConflicts.append(LT("error.folder_inside_itself", .text(url.path)))
        } else if exists(target) {
          rowConflicts.append(LT("error.destination_exists", .text(target.path)))
        }
        if let owner = destinationOwners[target.path], owner != index {
          rowConflicts.append(LT("error.duplicate_destination_name", .text(target.lastPathComponent)))
          if preview.indices.contains(owner) {
            var previous = preview[owner]["conflicts"] as? [String] ?? []
            let message = LT("error.duplicate_destination_name", .text(target.lastPathComponent)).render()
            if !previous.contains(message) { previous.append(message) }
            preview[owner]["conflicts"] = previous
          }
        } else {
          destinationOwners[target.path] = index
        }
      } else {
        row = LT("files.trash_destination").adding(to: row, field: "destination")
      }
      row["conflicts"] = rowConflicts.map { $0.render() }
      row["conflict_messages"] = rowConflicts.map(\.wire)
      preview.append(row)
    }
    let conflicts = preview.flatMap { $0["conflicts"] as? [String] ?? [] }
    if request["dry_run"] as? Bool == true {
      return ["success": true, "conflicts": conflicts, "preview": preview, "warnings": []]
    }
    if policy == "stop", !conflicts.isEmpty {
      return ["success": false, "error": conflicts.joined(separator: "\n"), "conflicts": conflicts,
        "preview": preview]
    }

    var completed = [[String: Any]]()
    var skipped = [[String: Any]]()
    var failures = [[String: Any]]()
    var journalHandle: FileHandle?
    do { journalHandle = try openJournal() }
    catch { return localizedErrorResponse(error) }
    defer { try? journalHandle?.close() }

    for row in preview {
      if isCancelled(requestID) { break }
      let rowConflicts = row["conflicts"] as? [String] ?? []
      if !rowConflicts.isEmpty {
        var skippedRow = row; skippedRow["status"] = "conflict"; skipped.append(skippedRow)
        continue
      }
      guard let source = row["source"] as? String else { continue }
      let from = URL(fileURLWithPath: source)
      let before = identity(from)
      do {
        let actual: URL
        if action == "trash" {
          var result: NSURL?
          try fm.trashItem(at: from, resultingItemURL: &result)
          guard let destination = result as URL? else { throw LT("error.trash_location_missing") }
          actual = destination
        } else {
          guard let destination = row["destination"] as? String else {
            throw LT("error.operation_record_incomplete")
          }
          actual = URL(fileURLWithPath: destination)
          if action == "copy" { try fm.copyItem(at: from, to: actual) }
          else { try fm.moveItem(at: from, to: actual) }
        }
        let record: [String: Any] = [
          "id": UUID().uuidString, "kind": "operation", "action": action,
          "source": from.path, "destination": actual.path, "before": before,
          "after": identity(actual), "time": Date().timeIntervalSince1970, "undone": false,
        ]
        if let journalHandle { try writeRecord(record, to: journalHandle) }
        completed.append(record)
      } catch {
        var failure: [String: Any] = ["source": source, "error": error.localizedDescription]
        if let text = error as? LocalizedText { failure = text.adding(to: failure, field: "error") }
        failures.append(failure)
        if policy == "stop" { break }
      }
    }
    do { try journalHandle?.synchronize() }
    catch {
      failures.append(["source": journal.path, "error": error.localizedDescription])
    }
    let wasCancelled = isCancelled(requestID)
    let payload: [String: Any] = [
      "success": failures.isEmpty && !wasCancelled,
      "completed": completed.count,
      "skipped": skipped,
      "failures": failures,
      "cancelled": wasCancelled,
    ]
    if failures.isEmpty && !wasCancelled {
      return LT("files.completed_count", .integer(completed.count)).adding(to: payload)
    }
    return LT("files.partial_completion_notice").adding(to: payload)
  }

  func undo() -> [String: Any] {
    let rows = log()
    guard let record = rows.last(where: { $0["undone"] as? Bool != true }) else {
      return error(LT("error.nothing_to_undo"))
    }
    guard let id = record["id"] as? String, let destination = record["destination"] as? String,
      let source = record["source"] as? String, let expected = record["after"] as? [String: Any]
    else { return error(LT("error.operation_record_incomplete")) }
    let target = URL(fileURLWithPath: destination)
    let original = URL(fileURLWithPath: source)
    let now = identity(target)
    guard now["inode"] as? String == expected["inode"] as? String,
      (now["modified"] as? Double) == (expected["modified"] as? Double),
      (now["size"] as? NSNumber) == (expected["size"] as? NSNumber)
    else { return error(LT("error.undo_destination_changed")) }
    do {
      if record["action"] as? String == "copy" {
        try fm.trashItem(at: target, resultingItemURL: nil)
      } else {
        guard !exists(original) else { return error(LT("error.undo_original_occupied")) }
        try fm.moveItem(at: target, to: original)
      }
      try appendRecords([[
        "kind": "undo", "target_id": id, "time": Date().timeIntervalSince1970,
      ]])
      return LT("files.last_file_operation_undone").adding(to: ["success": true])
    } catch { return localizedErrorResponse(error) }
  }
}
