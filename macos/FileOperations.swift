import AppKit
import Foundation

final class FileOperations {
  let fm = FileManager.default
  let journal: URL
  init(directory: URL) { journal = directory.appendingPathComponent("file-operations.json") }
  func log() -> [[String: Any]] {
    guard let d = try? Data(contentsOf: journal) else { return [] }
    return (try? JSONSerialization.jsonObject(with: d)) as? [[String: Any]] ?? []
  }
  func save(_ rows: [[String: Any]]) throws {
    try JSONSerialization.data(withJSONObject: rows, options: [.prettyPrinted, .sortedKeys]).write(
      to: journal, options: .atomic)
  }
  func identity(_ url: URL) -> [String: Any] {
    let a = try? fm.attributesOfItem(atPath: url.path)
    return [
      "inode": (a?[.systemFileNumber] as? NSNumber)?.stringValue ?? "",
      "modified": (a?[.modificationDate] as? Date)?.timeIntervalSince1970 ?? 0,
      "size": (a?[.size] as? NSNumber)?.uint64Value ?? 0,
    ]
  }
  func error(_ message: String) -> [String: Any] { ["success": false, "error": message] }
  func error(_ text: LocalizedText) -> [String: Any] { localizedErrorResponse(text) }
  func perform(_ req: [String: Any]) -> [String: Any] {
    let action = req["action"] as? String ?? ""
    if action == "history" { return ["success": true, "operations": log()] }
    if action == "undo" { return undo() }
    let paths = req["paths"] as? [String] ?? ((req["path"] as? String).map { [$0] } ?? [])
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
          for u in urls { NSWorkspace.shared.open(u) }
        }
      }
      return ["success": true]
    }
    guard ["rename", "move", "copy", "trash"].contains(action) else { return error(LT("error.unknown_file_operation")) }
    var preview = [[String: Any]]()
    var conflicts = [LocalizedText]()
    for url in urls {
      if !fm.fileExists(atPath: url.path) && !isLink(url) {
        conflicts.append(LT("error.file_not_found", .text(url.path)))
      }
      var target: URL?
      if action == "rename" {
        guard urls.count == 1, let name = req["new_name"] as? String, !name.isEmpty,
          !name.contains("/"), !name.contains("\0"), name != ".", name != ".."
        else { return error(LT("error.rename_selection_or_name")) }
        target = url.deletingLastPathComponent().appendingPathComponent(name)
      } else if action == "move" || action == "copy" {
        guard let dest = req["destination"] as? String, dest.hasPrefix("/") else {
          return error(LT("error.destination_required"))
        }
        var isDir: ObjCBool = false
        guard fm.fileExists(atPath: dest, isDirectory: &isDir), isDir.boolValue else {
          return error(LT("error.destination_folder_missing"))
        }
        target =
          URL(fileURLWithPath: dest).appendingPathComponent(url.lastPathComponent)
          .standardizedFileURL
      }
      if let to = target {
        if fm.fileExists(atPath: to.path) || isLink(to) { conflicts.append(LT("error.destination_exists", .text(to.path))) }
        if to.path == url.path || to.path.hasPrefix(url.path + "/") {
          conflicts.append(LT("error.folder_inside_itself", .text(url.path)))
        }
        if preview.contains(where: { $0["destination"] as? String == to.path }) {
          conflicts.append(LT("error.duplicate_destination_name", .text(to.lastPathComponent)))
        }
      }
      var row: [String: Any] = ["source": url.path, "destination": target?.path ?? ""]
      if target == nil { row = LT("files.trash_destination").adding(to: row, field: "destination") }
      preview.append(row)
    }
    if req["dry_run"] as? Bool == true {
      return ["success": true, "conflicts": conflicts.map { $0.render() }, "conflict_messages": conflicts.map(\.wire), "preview": preview, "warnings": []]
    }
    guard conflicts.isEmpty else {
      return ["success": false, "error": conflicts.map { $0.render() }.joined(separator: "\n"),
        "error_messages": conflicts.map(\.wire), "conflicts": conflicts.map { $0.render() },
        "conflict_messages": conflicts.map(\.wire)]
    }
    var journalRows = log()
    var completed = [[String: Any]]()
    do {
      for p in preview {
        let from = URL(fileURLWithPath: p["source"] as! String)
        let before = identity(from)
        var actual: URL
        if action == "trash" {
          var result: NSURL?
          try fm.trashItem(at: from, resultingItemURL: &result)
          guard let u = result as URL? else {
            throw LT("error.trash_location_missing")
          }
          actual = u
        } else {
          actual = URL(fileURLWithPath: p["destination"] as! String)
          if action == "copy" {
            try fm.copyItem(at: from, to: actual)
          } else {
            try fm.moveItem(at: from, to: actual)
          }
        }
        let record: [String: Any] = [
          "id": UUID().uuidString, "action": action, "source": from.path,
          "destination": actual.path, "before": before, "after": identity(actual),
          "time": Date().timeIntervalSince1970, "undone": false,
        ]
        journalRows.append(record)
        completed.append(record)
        try save(journalRows)
      }
      return LT("files.completed_count", .integer(completed.count)).adding(to: [
        "success": true, "completed": completed.count,
      ])
    } catch {
      var result = localizedErrorResponse(error)
      result["completed"] = completed.count
      return LT("files.partial_completion_notice").adding(to: result)
    }
  }
  func isLink(_ u: URL) -> Bool { (try? fm.destinationOfSymbolicLink(atPath: u.path)) != nil }
  func undo() -> [String: Any] {
    var rows = log()
    guard let index = rows.lastIndex(where: { $0["undone"] as? Bool != true }) else {
      return error(LT("error.nothing_to_undo"))
    }
    let record = rows[index]
    guard let dest = record["destination"] as? String, let source = record["source"] as? String,
      let expected = record["after"] as? [String: Any]
    else { return error(LT("error.operation_record_incomplete")) }
    let target = URL(fileURLWithPath: dest)
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
        guard !fm.fileExists(atPath: source), !isLink(original) else {
          return error(LT("error.undo_original_occupied"))
        }
        try fm.moveItem(at: target, to: original)
      }
      rows[index]["undone"] = true
      try save(rows)
      return LT("files.last_file_operation_undone").adding(to: ["success": true])
    } catch { return localizedErrorResponse(error) }
  }
}
