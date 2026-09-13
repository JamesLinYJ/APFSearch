import AppKit

/// A virtualized, paginated confirmation table. Editing never performs an
/// operation: changed names are sent back through the service's dry-run first.
final class FileOperationReview: NSObject, NSTableViewDataSource, NSTableViewDelegate {
  enum Decision { case cancel, recheck([String: String], [String]), execute([String]) }
  private let alert = NSAlert()
  private let table = NSTableView()
  private let status = NSTextField(labelWithString: "")
  private let previous = NSButton(title: "‹", target: nil, action: nil)
  private let next = NSButton(title: "›", target: nil, action: nil)
  private var rows: [[String: Any]]
  private var selected = Set<String>()
  private var edits = [String: String]()
  private var page = 0
  private let pageSize = 100
  private let editable: Bool
  private let readOnly: Bool
  init(title: String, rows: [[String: Any]], editable: Bool, readOnly: Bool = false) {
    self.rows = rows; self.editable = editable; self.readOnly = readOnly
    super.init()
    alert.messageText = title
    alert.informativeText = readOnly ? L("files.results_notice") : L("files.preview_notice")
    for row in rows where row["skipped"] as? Bool != true && row["noop"] as? Bool != true && (row["conflicts"] as? [String] ?? []).isEmpty {
      if let source = row["source"] as? String { selected.insert(source) }
    }
    let scroll = NSScrollView(); scroll.hasVerticalScroller = true; scroll.hasHorizontalScroller = true
    table.dataSource = self; table.delegate = self; table.usesAlternatingRowBackgroundColors = true
    table.rowHeight = 25
    for (key, title, width) in [("selected", L("files.include"), 55.0), ("source", L("files.source"), 250.0),
      ("destination", L("files.destination"), 260.0), ("status", L("files.item_status"), 260.0)] {
      let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier(key)); column.title = title; column.width = width
      if key == "selected" {
        let cell = NSButtonCell(); cell.setButtonType(.switch); cell.title = ""; column.dataCell = cell
        column.isEditable = !readOnly
      } else { column.isEditable = key == "destination" && editable && !readOnly }
      if key != "selected" || !readOnly { table.addTableColumn(column) }
    }
    scroll.documentView = table
    previous.target = self; previous.action = #selector(previousPage)
    next.target = self; next.action = #selector(nextPage)
    let footer = NSStackView(views: [status, NSView(), previous, next]); footer.spacing = 10
    let stack = NSStackView(views: [scroll, footer]); stack.orientation = .vertical; stack.spacing = 8
    stack.frame = NSRect(x: 0, y: 0, width: 820, height: 360)
    scroll.translatesAutoresizingMaskIntoConstraints = false
    NSLayoutConstraint.activate([scroll.widthAnchor.constraint(equalTo: stack.widthAnchor), scroll.heightAnchor.constraint(equalToConstant: 315), footer.widthAnchor.constraint(equalTo: stack.widthAnchor)])
    alert.accessoryView = stack
    alert.addButton(withTitle: readOnly ? L("action.ok") : L("action.continue"))
    if !readOnly { alert.addButton(withTitle: L("action.cancel")) }
    refresh()
  }
  private var range: Range<Int> { let first = min(rows.count, page * pageSize); return first..<min(rows.count, first + pageSize) }
  private func index(_ visible: Int) -> Int { range.lowerBound + visible }
  func numberOfRows(in tableView: NSTableView) -> Int { range.count }
  func tableView(_ tableView: NSTableView, objectValueFor tableColumn: NSTableColumn?, row visible: Int) -> Any? {
    let i = index(visible); guard rows.indices.contains(i), let key = tableColumn?.identifier.rawValue else { return nil }
    let row = rows[i], source = rows[i]["source"] as? String ?? ""
    switch key {
    case "selected": return selected.contains(source)
    case "destination":
      let destination = row["destination"] as? String ?? ""
      return edits[source] ?? (editable && !readOnly ? URL(fileURLWithPath: destination).lastPathComponent : destination)
    case "status":
      let conflicts = row["conflicts"] as? [String] ?? []
      if !conflicts.isEmpty { return conflicts.joined(separator: "; ") }
      return row["error"] as? String ?? row["journal_error"] as? String ?? (row["status"] as? String).map(Self.statusText) ?? L("files.ready")
    default: return row[key] as? String ?? ""
    }
  }
  func tableView(_ tableView: NSTableView, setObjectValue object: Any?, for tableColumn: NSTableColumn?, row visible: Int) {
    let i = index(visible); guard !readOnly, rows.indices.contains(i), let source = rows[i]["source"] as? String else { return }
    if tableColumn?.identifier.rawValue == "selected" {
      if (object as? NSNumber)?.boolValue == true { selected.insert(source) } else { selected.remove(source) }
    } else if editable, let name = object as? String {
      // Only a leaf name is accepted by the service; never accept an edited path.
      edits[source] = name; selected.insert(source)
    }
    updateStatus()
  }
  private func updateStatus() {
    status.stringValue = L("files.page_summary", localizedCount(rows.count), localizedCount(page + 1), localizedCount(max(1, (rows.count + pageSize - 1) / pageSize)), localizedCount(selected.count))
    alert.buttons.first?.isEnabled = readOnly || !selected.isEmpty
  }
  private func refresh() {
    table.reloadData(); previous.isEnabled = page > 0; next.isEnabled = range.upperBound < rows.count; updateStatus()
  }
  @objc private func previousPage() { table.window?.makeFirstResponder(nil); if page > 0 { page -= 1; refresh() } }
  @objc private func nextPage() { table.window?.makeFirstResponder(nil); if range.upperBound < rows.count { page += 1; refresh() } }
  func run() -> Decision {
    guard alert.runModal() == .alertFirstButtonReturn, !readOnly else { return .cancel }
    table.window?.makeFirstResponder(nil)
    let skips = rows.compactMap { $0["source"] as? String }.filter { !selected.contains($0) }
    return edits.isEmpty ? .execute(skips) : .recheck(edits, skips)
  }
  static func statusText(_ status: String) -> String {
    switch status {
    case "completed": return L("files.completed")
    case "skipped": return L("files.skipped")
    case "unchanged": return L("files.unchanged")
    case "not_started": return L("files.not_started")
    default: return L("files.needs_review")
    }
  }
}

/// Own each incoming lease before checking UI freshness. Dropping a stale or
/// deferred response therefore releases its lease even when never displayed.
final class SearchSnapshotLease {
  let token: String
  let listID: String?
  init(token: String, listID: String?) { self.token = token; self.listID = listID }
  deinit {
    var request: [String: Any] = ["op": "release_snapshot", "snapshot_lease": token]
    if let listID { request["list_id"] = listID }
    SearchClient.shared.call(request) { _ in }
  }
}
