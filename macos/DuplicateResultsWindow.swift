import AppKit
import Foundation

final class DuplicateResultsWindowController: NSWindowController, NSTableViewDataSource, NSTableViewDelegate, NSWindowDelegate {
  typealias ManageHandler = ([String: Any]) -> Void
  var onClose: (() -> Void)?
  private let table = NSTableView()
  private let status = NSTextField(labelWithString: "")
  private let previous = NSButton(title: "‹", target: nil, action: nil)
  private let next = NSButton(title: "›", target: nil, action: nil)
  private let manage = NSButton(title: L("duplicates.manage_selection"), target: nil, action: nil)
  private let pageSize = 200
  private var page = 0
  private var rows = [[String: Any]]()
  private var groups = [String: [[String: Any]]]()
  private var selected = Set<String>()
  private var restoring = false
  private let handler: ManageHandler
  private let partial: Bool
  private let errors: [String]

  init(result: [String: Any], onManage: @escaping ManageHandler) {
    handler = onManage; partial = result["partial"] as? Bool == true
    errors = (result["errors"] as? [[String: Any]] ?? []).map {
      ($0["path"] as? String ?? "") + ": " + ($0["message"] as? String ?? "")
    }
    let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 1040, height: 620),
      styleMask: [.titled, .closable, .miniaturizable, .resizable], backing: .buffered, defer: false)
    window.title = L("duplicates.window_title"); window.minSize = NSSize(width: 750, height: 420)
    super.init(window: window); window.delegate = self
    let hardlinks = result["hardlinks"] as? [[String: Any]] ?? []
    let aliasPaths = Set(hardlinks.flatMap { ($0["rows"] as? [[String: Any]] ?? []).compactMap { $0["path"] as? String } })
    var displayed = Set<String>()
    let contentGroups = result["groups"] as? [[String: Any]] ?? []
    for (index, group) in contentGroups.enumerated() {
      let groupID = String(index + 1)
      let clones = Set((group["clone_groups"] as? [[String: Any]] ?? []).flatMap { $0["paths"] as? [String] ?? [] })
      for item in group["rows"] as? [[String: Any]] ?? [] {
        guard let path = item["path"] as? String, displayed.insert(path).inserted else { continue }
        var row = item; row["group"] = groupID
        let alias = aliasPaths.contains(path)
        row["eligible"] = !alias && group["kind"] as? String == "same_content"
        row["relationship"] = alias ? L("duplicates.hardlink_alias") : (clones.contains(path) ? L("duplicates.verified_clone") : L("duplicates.same_content"))
        rows.append(row); groups[groupID, default: []].append(row)
      }
    }
    for (index, group) in hardlinks.enumerated() {
      for item in group["rows"] as? [[String: Any]] ?? [] {
        guard let path = item["path"] as? String, displayed.insert(path).inserted else { continue }
        var row = item; row["group"] = String(contentGroups.count + index + 1)
        row["eligible"] = false; row["relationship"] = L("duplicates.hardlink_alias"); rows.append(row)
      }
    }
    build()
  }
  required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }
  func windowWillClose(_ notification: Notification) { onClose?() }

  private func build() {
    guard let content = window?.contentView else { return }
    let scroll = NSScrollView(); scroll.translatesAutoresizingMaskIntoConstraints = false
    scroll.hasVerticalScroller = true; scroll.hasHorizontalScroller = true
    table.delegate = self; table.dataSource = self; table.allowsMultipleSelection = true
    table.usesAlternatingRowBackgroundColors = true; table.rowHeight = 24
    for (key, title, width) in [("group", L("duplicates.group"), 65.0), ("name", L("column.name"), 200.0),
      ("size", L("column.size"), 100.0), ("path", L("column.location"), 350.0),
      ("relationship", L("duplicates.relationship"), 180.0)] {
      let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier(key)); column.title = title; column.width = width
      column.minWidth = 55; table.addTableColumn(column)
    }
    scroll.documentView = table
    previous.target = self; previous.action = #selector(previousPage)
    next.target = self; next.action = #selector(nextPage)
    manage.target = self; manage.action = #selector(manageSelected)
    let extras = NSButton(title: L("duplicates.select_extras"), target: self, action: #selector(selectExtras))
    let footer = NSStackView(views: [status, NSView(), previous, next, extras, manage]); footer.spacing = 8
    footer.translatesAutoresizingMaskIntoConstraints = false
    let notice = NSTextField(wrappingLabelWithString: L("duplicates.storage_notice"))
    notice.font = .systemFont(ofSize: 11); notice.textColor = .secondaryLabelColor
    notice.translatesAutoresizingMaskIntoConstraints = false
    content.addSubview(scroll); content.addSubview(footer); content.addSubview(notice)
    NSLayoutConstraint.activate([
      scroll.topAnchor.constraint(equalTo: content.topAnchor, constant: 10),
      scroll.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 10),
      scroll.trailingAnchor.constraint(equalTo: content.trailingAnchor, constant: -10),
      scroll.bottomAnchor.constraint(equalTo: footer.topAnchor, constant: -8),
      footer.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 12),
      footer.trailingAnchor.constraint(equalTo: content.trailingAnchor, constant: -12),
      footer.bottomAnchor.constraint(equalTo: notice.topAnchor, constant: -8), footer.heightAnchor.constraint(equalToConstant: 28),
      notice.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 12),
      notice.trailingAnchor.constraint(equalTo: content.trailingAnchor, constant: -12),
      notice.bottomAnchor.constraint(equalTo: content.bottomAnchor, constant: -10),
    ])
    updatePage()
  }
  private var pageRange: Range<Int> { let first = min(rows.count, page * pageSize); return first..<min(rows.count, first + pageSize) }
  private func item(_ visible: Int) -> [String: Any]? {
    let index = pageRange.lowerBound + visible; return rows.indices.contains(index) ? rows[index] : nil
  }
  func numberOfRows(in tableView: NSTableView) -> Int { pageRange.count }
  func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row visible: Int) -> NSView? {
    guard let column = tableColumn, let row = item(visible) else { return nil }
    let cell = (tableView.makeView(withIdentifier: column.identifier, owner: self) as? NSTableCellView) ?? {
      let cell = NSTableCellView(); cell.identifier = column.identifier
      let label = NSTextField(labelWithString: ""); label.translatesAutoresizingMaskIntoConstraints = false; label.lineBreakMode = .byTruncatingMiddle
      cell.textField = label; cell.addSubview(label)
      NSLayoutConstraint.activate([label.leadingAnchor.constraint(equalTo: cell.leadingAnchor, constant: 6), label.trailingAnchor.constraint(equalTo: cell.trailingAnchor, constant: -6), label.centerYAnchor.constraint(equalTo: cell.centerYAnchor)])
      return cell
    }()
    cell.textField?.stringValue = column.identifier.rawValue == "size"
      ? ByteCountFormatter.string(fromByteCount: (row["size"] as? NSNumber)?.int64Value ?? 0, countStyle: .file)
      : row[column.identifier.rawValue] as? String ?? ""
    cell.textField?.textColor = row["eligible"] as? Bool == true ? .labelColor : .secondaryLabelColor
    cell.toolTip = row["path"] as? String
    return cell
  }
  func tableView(_ tableView: NSTableView, shouldSelectRow row: Int) -> Bool { item(row)?["eligible"] as? Bool == true }
  func tableViewSelectionDidChange(_ notification: Notification) {
    guard !restoring else { return }
    for index in pageRange { if let path = rows[index]["path"] as? String { selected.remove(path) } }
    for visible in table.selectedRowIndexes {
      if let row = item(visible), row["eligible"] as? Bool == true, let path = row["path"] as? String { selected.insert(path) }
    }
    updateStatus()
  }
  private func updateStatus() {
    let pages = max(1, (rows.count + pageSize - 1) / pageSize)
    status.stringValue = L("files.page_summary", localizedCount(rows.count), localizedCount(page + 1), localizedCount(pages), localizedCount(selected.count)) + (partial ? " · " + L("duplicates.partial_label") : "")
    status.toolTip = errors.joined(separator: "\n"); manage.isEnabled = !selected.isEmpty
  }
  private func updatePage() {
    restoring = true; defer { restoring = false }
    table.reloadData()
    table.selectRowIndexes(IndexSet(pageRange.enumerated().compactMap { visible, index in
      (rows[index]["path"] as? String).flatMap { selected.contains($0) ? visible : nil }
    }), byExtendingSelection: false)
    previous.isEnabled = page > 0; next.isEnabled = pageRange.upperBound < rows.count
    updateStatus()
  }
  @objc private func previousPage() { if page > 0 { page -= 1; updatePage() } }
  @objc private func nextPage() { if pageRange.upperBound < rows.count { page += 1; updatePage() } }
  @objc private func selectExtras() {
    selected.removeAll()
    for members in groups.values {
      // Stable path order makes the suggested keeper deterministic; aliases
      // are never silently selected as independent, reclaimable copies.
      let sorted = members.sorted { ($0["path"] as? String ?? "") < ($1["path"] as? String ?? "") }
      let keeper = sorted.first
      for row in sorted where row["eligible"] as? Bool == true {
        if let path = row["path"] as? String, path != keeper?["path"] as? String { selected.insert(path) }
      }
    }
    updatePage()
  }
  @objc private func manageSelected() {
    guard !selected.isEmpty else { return }
    var keepers = [[String: Any]](), sources = [[String: Any]](), sourceGroups = [String: String]()
    for (groupID, members) in groups {
      let chosen = members.filter { ($0["path"] as? String).map { selected.contains($0) } ?? false }
      guard !chosen.isEmpty else { continue }
      guard let keeper = members.first(where: { ($0["path"] as? String).map { !selected.contains($0) } ?? false }) else {
        let alert = NSAlert(); alert.messageText = L("error.operation_incomplete"); alert.informativeText = L("duplicates.keep_one")
        alert.runModal(); return
      }
      keepers.append(keeper)
      for row in chosen { if let path = row["path"] as? String { sourceGroups[path] = groupID; sources.append(row) } }
    }
    sources.sort { ($0["path"] as? String ?? "") < ($1["path"] as? String ?? "") }
    handler(["op": "files", "action": "trash", "paths": sources.compactMap { $0["path"] as? String },
      "expected": sources, "keepers": keepers, "groups": sourceGroups])
  }
}
