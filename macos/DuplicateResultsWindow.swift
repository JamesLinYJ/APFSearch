import AppKit
import Foundation

final class DuplicateResultsWindowController: NSWindowController, NSTableViewDataSource, NSTableViewDelegate {
  typealias ManageHandler = ([String]) -> Void

  private let table = NSTableView()
  private let status = NSTextField(labelWithString: "")
  private let previous = NSButton(title: "‹", target: nil, action: nil)
  private let next = NSButton(title: "›", target: nil, action: nil)
  private let manage = NSButton(title: L("action.continue"), target: nil, action: nil)
  private let pageSize = 200
  private var page = 0
  private var rows = [[String: Any]]()
  private var selected = Set<String>()
  private var distinctByGroup = [Int: Int]()
  private let handler: ManageHandler
  private let partial: Bool
  private let errors: [String]

  init(result: [String: Any], onManage: @escaping ManageHandler) {
    handler = onManage
    partial = result["partial"] as? Bool == true
    errors = (result["errors"] as? [[String: Any]] ?? []).map {
      let path = $0["path"] as? String ?? ""
      let message = $0["message"] as? String ?? ""
      return path.isEmpty ? message : path + ": " + message
    }
    let window = NSWindow(
      contentRect: NSRect(x: 0, y: 0, width: 920, height: 620),
      styleMask: [.titled, .closable, .miniaturizable, .resizable], backing: .buffered, defer: false)
    window.title = L("duplicates.window_title")
    window.minSize = NSSize(width: 700, height: 420)
    super.init(window: window)
    parse(result)
    build()
  }
  required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

  private func parse(_ result: [String: Any]) {
    let groups = result["groups"] as? [[String: Any]] ?? []
    for (groupIndex, group) in groups.enumerated() {
      distinctByGroup[groupIndex] = (group["distinct_files"] as? NSNumber)?.intValue ?? 0
      let clonePaths = Set((group["clone_groups"] as? [[String: Any]] ?? []).flatMap {
        $0["paths"] as? [String] ?? []
      })
      for item in group["rows"] as? [[String: Any]] ?? [] {
        guard let path = item["path"] as? String else { continue }
        var row = item
        row["group"] = groupIndex
        row["eligible"] = true
        row["relationship"] = clonePaths.contains(path) ? "APFS clone" : "Same content"
        row["reclaimable"] = "Unknown"
        rows.append(row)
      }
    }
    let hardlinks = result["hardlinks"] as? [[String: Any]] ?? []
    for (index, group) in hardlinks.enumerated() {
      for item in group["rows"] as? [[String: Any]] ?? [] {
        guard item["path"] is String else { continue }
        var row = item
        row["group"] = groups.count + index
        row["eligible"] = false
        row["relationship"] = "Hard-link alias"
        row["reclaimable"] = "Not independent"
        rows.append(row)
      }
    }
  }

  private func build() {
    guard let content = window?.contentView else { return }
    let scroll = NSScrollView()
    scroll.translatesAutoresizingMaskIntoConstraints = false
    scroll.hasVerticalScroller = true
    table.delegate = self
    table.dataSource = self
    table.allowsMultipleSelection = true
    table.usesAlternatingRowBackgroundColors = true
    table.rowHeight = 24
    for (identifier, title, width) in [
      ("group", "Group", 70.0), ("name", L("column.name"), 220.0),
      ("path", L("column.location"), 390.0), ("relationship", "Relationship", 130.0),
      ("reclaimable", "Reclaimable", 130.0),
    ] {
      let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier(identifier))
      column.title = title
      column.width = width
      column.minWidth = 60
      table.addTableColumn(column)
    }
    scroll.documentView = table
    previous.target = self; previous.action = #selector(previousPage)
    next.target = self; next.action = #selector(nextPage)
    manage.target = self; manage.action = #selector(manageSelected)
    let spacer = NSView()
    let footer = NSStackView(views: [status, spacer, previous, next, manage])
    footer.translatesAutoresizingMaskIntoConstraints = false
    footer.spacing = 8
    content.addSubview(scroll)
    content.addSubview(footer)
    NSLayoutConstraint.activate([
      scroll.topAnchor.constraint(equalTo: content.topAnchor, constant: 10),
      scroll.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 10),
      scroll.trailingAnchor.constraint(equalTo: content.trailingAnchor, constant: -10),
      scroll.bottomAnchor.constraint(equalTo: footer.topAnchor, constant: -8),
      footer.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 12),
      footer.trailingAnchor.constraint(equalTo: content.trailingAnchor, constant: -12),
      footer.bottomAnchor.constraint(equalTo: content.bottomAnchor, constant: -10),
      footer.heightAnchor.constraint(equalToConstant: 28),
    ])
    updatePage()
  }

  private var pageRange: Range<Int> {
    let lower = min(rows.count, page * pageSize)
    return lower..<min(rows.count, lower + pageSize)
  }
  private func item(at visibleRow: Int) -> [String: Any]? {
    let index = pageRange.lowerBound + visibleRow
    return rows.indices.contains(index) ? rows[index] : nil
  }
  func numberOfRows(in tableView: NSTableView) -> Int { pageRange.count }
  func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row visibleRow: Int) -> NSView? {
    guard let column = tableColumn, let row = item(at: visibleRow) else { return nil }
    let identifier = column.identifier
    let cell = (tableView.makeView(withIdentifier: identifier, owner: self) as? NSTableCellView) ?? {
      let value = NSTableCellView(); value.identifier = identifier
      let label = NSTextField(labelWithString: ""); label.translatesAutoresizingMaskIntoConstraints = false
      label.lineBreakMode = .byTruncatingMiddle
      value.textField = label; value.addSubview(label)
      NSLayoutConstraint.activate([
        label.leadingAnchor.constraint(equalTo: value.leadingAnchor, constant: 6),
        label.trailingAnchor.constraint(equalTo: value.trailingAnchor, constant: -6),
        label.centerYAnchor.constraint(equalTo: value.centerYAnchor),
      ])
      return value
    }()
    let text: String
    switch identifier.rawValue {
    case "group": text = "#\(((row["group"] as? Int) ?? 0) + 1)"
    case "name": text = row["name"] as? String ?? URL(fileURLWithPath: row["path"] as? String ?? "").lastPathComponent
    default: text = row[identifier.rawValue] as? String ?? ""
    }
    cell.textField?.stringValue = text
    cell.textField?.textColor = row["eligible"] as? Bool == true ? .labelColor : .secondaryLabelColor
    return cell
  }
  func tableView(_ tableView: NSTableView, shouldSelectRow visibleRow: Int) -> Bool {
    item(at: visibleRow)?["eligible"] as? Bool == true
  }
  func tableViewSelectionDidChange(_ notification: Notification) {
    for index in pageRange {
      if rows[index]["eligible"] as? Bool == true, let path = rows[index]["path"] as? String { selected.remove(path) }
    }
    for visible in table.selectedRowIndexes {
      let index = pageRange.lowerBound + visible
      if rows.indices.contains(index), let path = rows[index]["path"] as? String { selected.insert(path) }
    }
    manage.isEnabled = !selected.isEmpty
  }

  private func updatePage() {
    table.reloadData()
    var restored = IndexSet()
    for (visible, index) in pageRange.enumerated() {
      if let path = rows[index]["path"] as? String, selected.contains(path) { restored.insert(visible) }
    }
    table.selectRowIndexes(restored, byExtendingSelection: false)
    let pages = max(1, (rows.count + pageSize - 1) / pageSize)
    previous.isEnabled = page > 0
    next.isEnabled = page + 1 < pages
    status.stringValue = "\(rows.count) rows · \(page + 1)/\(pages)" + (partial ? " · partial" : "")
    status.toolTip = errors.isEmpty ? nil : errors.joined(separator: "\n")
    manage.isEnabled = !selected.isEmpty
  }
  @objc private func previousPage() { if page > 0 { page -= 1; updatePage() } }
  @objc private func nextPage() { if (page + 1) * pageSize < rows.count { page += 1; updatePage() } }

  @objc private func manageSelected() {
    guard !selected.isEmpty else { return }
    var selectedObjects = [Int: Set<String>]()
    for row in rows where row["eligible"] as? Bool == true {
      guard let path = row["path"] as? String, selected.contains(path), let group = row["group"] as? Int else { continue }
      let device = (row["device_id"] as? NSNumber)?.uint64Value ?? 0
      let inode = (row["file_id"] as? NSNumber)?.uint64Value ?? 0
      selectedObjects[group, default: []].insert("\(device):\(inode)")
    }
    for (group, objects) in selectedObjects where objects.count >= (distinctByGroup[group] ?? Int.max) {
      let alert = NSAlert()
      alert.messageText = L("error.operation_incomplete")
      alert.informativeText = "Keep at least one independent file in every duplicate group. Hard-link aliases are review-only."
      alert.runModal()
      return
    }
    handler(Array(selected).sorted())
  }
}
