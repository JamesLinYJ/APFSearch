import AppKit
import QuickLookUI
import Carbon
import ServiceManagement

@main
final class AppDelegate: NSObject, NSApplicationDelegate {
    var windows: [SearchWindowController] = []
    var settingsWindow: NSWindowController?
    var hotKey: EventHotKeyRef?
    var hotKeyHandler: EventHandlerRef?

    static func main() {
        let app = NSApplication.shared
        let delegate = AppDelegate()
        app.delegate = delegate
        app.setActivationPolicy(.regular)
        withExtendedLifetime(delegate) { app.run() }
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        SearchClient.shared.registerService()
        installMenus()
        if let index = CommandLine.arguments.firstIndex(of: "--offline-list-id"), CommandLine.arguments.indices.contains(index + 1) {
            let identifier = CommandLine.arguments[index + 1]
            if UUID(uuidString: identifier) != nil { openDeveloperOfflineList(identifier) }
            else { NSLog("APFSearch: invalid --offline-list-id UUID"); newWindow(nil) }
        } else { newWindow(nil) }
        registerShortcut()
        NotificationCenter.default.addObserver(self, selector: #selector(registerShortcut), name: .searchShortcutChanged, object: nil)
        NSApp.activate(ignoringOtherApps: true)
    }
    func applicationDidBecomeActive(_ notification: Notification) { UpdateController.shared.applicationBecameActive() }
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }
    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        if !flag { newWindow(nil) }; return true
    }
    private func openDeveloperOfflineList(_ identifier: String) {
        SearchClient.shared.call(["op": "status", "list_id": identifier]) { [weak self] reply in
            guard let self else { return }
            if reply["success"] as? Bool == false {
                let alert = NSAlert(); alert.messageText = L("error.operation_incomplete"); alert.informativeText = reply["error"] as? String ?? ""; alert.runModal(); return
            }
            let controller = SearchWindowController(offlineListID: identifier, count: (reply["count"] as? NSNumber)?.intValue ?? 0)
            self.windows.append(controller); controller.showWindow(nil); controller.window?.makeKeyAndOrderFront(nil)
        }
    }
    @objc func newWindow(_ sender: Any?) {
        let controller = SearchWindowController()
        windows.append(controller)
        controller.showWindow(nil)
        controller.window?.makeKeyAndOrderFront(nil)
    }
    @objc func newTab(_ sender: Any?) {
        let previous = NSApp.keyWindow
        newWindow(sender)
        if let current = windows.last?.window, let previous, previous !== current {
            previous.addTabbedWindow(current, ordered: .above)
        }
    }
    @objc func showSettings(_ sender: Any?) {
        if settingsWindow == nil { settingsWindow = makeSearchSettingsWindow() }
        settingsWindow?.showWindow(nil)
        settingsWindow?.window?.makeKeyAndOrderFront(nil)
    }
    @objc func showHelp(_ sender: Any?) {
        let alert = NSAlert()
        alert.messageText = L("search.search_syntax")
        alert.informativeText = L("help.query_syntax_details")
        alert.addButton(withTitle: L("action.ok"))
        alert.runModal()
    }
    @objc func showAbout(_ sender: Any?) {
        NSApp.orderFrontStandardAboutPanel(options: [.applicationName: L("app.name"), .applicationVersion: Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "", .credits: NSAttributedString(string: L("app.about_description"))])
    }
    private func item(_ title: String, _ selector: Selector?, _ key: String = "", modifiers: NSEvent.ModifierFlags = .command, target: AnyObject? = nil) -> NSMenuItem {
        let item = NSMenuItem(title: title, action: selector, keyEquivalent: key)
        item.keyEquivalentModifierMask = modifiers; item.target = target
        return item
    }
    private func installMenus() {
        let bar = NSMenu(); NSApp.mainMenu = bar
        func submenu(_ title: String) -> NSMenu {
            let root = NSMenuItem(); root.title = title; let menu = NSMenu(title: title); root.submenu = menu; bar.addItem(root); return menu
        }
        let app = submenu(L("app.name"))
        app.addItem(item(L("app.about"), #selector(showAbout), target: self))
        UpdateController.shared.install(in: app)
        app.addItem(.separator())
        app.addItem(item(L("action.open_settings"), #selector(showSettings), ",", target: self))
        app.addItem(.separator())
        app.addItem(item(L("app.hide"), #selector(NSApplication.hide(_:)), "h"))
        app.addItem(item(L("app.hide_others"), #selector(NSApplication.hideOtherApplications(_:)), "h", modifiers: [.command, .option]))
        app.addItem(item(L("app.show_all"), #selector(NSApplication.unhideAllApplications(_:))))
        app.addItem(.separator())
        app.addItem(item(L("app.quit"), #selector(NSApplication.terminate(_:)), "q"))
        let file = submenu(L("menu.file"))
        file.addItem(item(L("action.new_window"), #selector(newWindow), "n", target: self))
        file.addItem(item(L("action.new_tab"), #selector(newTab), "t", target: self))
        file.addItem(item(L("action.close"), #selector(NSWindow.performClose(_:)), "w"))
        file.addItem(.separator())
        file.addItem(item(L("action.open"), #selector(SearchWindowController.openSelection)))
        file.addItem(item(L("action.reveal_in_finder"), #selector(SearchWindowController.revealSelection), "r", modifiers: [.command, .shift]))
        file.addItem(item(L("action.quick_look"), #selector(SearchWindowController.previewSelection)))
        file.addItem(item(L("action.rename_dialog"), #selector(SearchWindowController.renameSelection)))
        file.addItem(item(L("action.copy_dialog"), #selector(SearchWindowController.copySelection)))
        file.addItem(item(L("action.move_dialog"), #selector(SearchWindowController.moveSelection)))
        file.addItem(item(L("action.confirm_trash"), #selector(SearchWindowController.trashSelection), "\u{8}", modifiers: .command))
        file.addItem(.separator())
        file.addItem(item(L("action.export_file_list"), #selector(SearchWindowController.exportList)))
        file.addItem(item(L("action.import_file_list"), #selector(SearchWindowController.importList)))
        let edit = submenu(L("menu.edit"))
        edit.addItem(item(L("action.undo"), #selector(SearchResultsTable.undo(_:)), "z"))
        edit.addItem(item(L("action.undo_last_file_operation"), #selector(SearchWindowController.undoFileOperation)))
        edit.addItem(item(L("files.file_operation_history_dialog"), #selector(SearchWindowController.showOperationHistory)))
        edit.addItem(.separator())
        edit.addItem(item(L("action.cut"), #selector(NSText.cut(_:)), "x"))
        edit.addItem(item(L("action.copy"), #selector(NSText.copy(_:)), "c"))
        edit.addItem(item(L("action.paste"), #selector(NSText.paste(_:)), "v"))
        edit.addItem(item(L("action.select_all"), #selector(NSText.selectAll(_:)), "a"))
        edit.addItem(item(L("action.copy_path"), #selector(SearchWindowController.copyPaths), "c", modifiers: [.command, .shift]))
        let search = submenu(L("search.title"))
        search.addItem(item(L("search.field_label"), #selector(SearchWindowController.focusSearch), "f"))
        search.addItem(item(L("action.bookmark_current_search"), #selector(SearchWindowController.addBookmark), "d"))
        search.addItem(item(L("duplicates.window_title"), #selector(SearchWindowController.findDuplicates)))
        search.addItem(item(L("directory.show_sizes"), #selector(SearchWindowController.showDirectorySizes)))
        search.addItem(item(L("index.index_contents_of_selected_files"), #selector(SearchWindowController.indexContent)))
        search.addItem(item(L("action.stop_current_content_task"), #selector(SearchWindowController.cancelBackground)))
        let index = submenu(L("settings.indexing"))
        index.addItem(item(L("index.full_disk_access"), #selector(SearchWindowController.showPermissionGuide)))
        index.addItem(item(L("action.show_coverage"), #selector(SearchWindowController.showCoverage)))
        index.addItem(item(L("index.choose_index_folders"), #selector(SearchWindowController.chooseFolders)))
        index.addItem(item(L("action.choose_local_apfs_volumes"), #selector(SearchWindowController.chooseVolumes)))
        index.addItem(item(L("index.rescan"), #selector(SearchWindowController.rescan), "r"))
        let window = submenu(L("menu.window")); NSApp.windowsMenu = window
        window.addItem(item(L("action.minimize"), #selector(NSWindow.performMiniaturize(_:)), "m"))
        window.addItem(item(L("action.zoom"), #selector(NSWindow.performZoom(_:))))
        window.addItem(item(L("action.show_all_tabs"), #selector(NSWindow.toggleTabOverview(_:))))
        window.addItem(item(L("action.bring_all_to_front"), #selector(NSApplication.arrangeInFront(_:))))
        let help = submenu(L("menu.help")); help.addItem(item(L("settings.search_syntax_and_shortcuts"), #selector(showHelp), "?", target: self)); NSApp.helpMenu = help
    }
    @objc func registerShortcut() {
        if let hotKey { UnregisterEventHotKey(hotKey); self.hotKey = nil }
        let defaults = UserDefaults.standard
        if defaults.object(forKey: ApplicationIdentity.preferencePrefix + "shortcutEnabled") != nil && !defaults.bool(forKey: ApplicationIdentity.preferencePrefix + "shortcutEnabled") { return }
        if hotKeyHandler == nil {
            var type = EventTypeSpec(eventClass: OSType(kEventClassKeyboard), eventKind: UInt32(kEventHotKeyPressed))
            let pointer = Unmanaged.passUnretained(self).toOpaque()
            InstallEventHandler(GetApplicationEventTarget(), { _, _, context -> OSStatus in
                guard let context else { return OSStatus(eventNotHandledErr) }
                let delegate = Unmanaged<AppDelegate>.fromOpaque(context).takeUnretainedValue()
                DispatchQueue.main.async {
                    NSApp.activate(ignoringOtherApps: true)
                    if let controller = delegate.windows.last {
                        controller.showWindow(nil); controller.window?.makeKeyAndOrderFront(nil); controller.focusSearch(nil)
                    } else { delegate.newWindow(nil) }
                }
                return noErr
            }, 1, &type, pointer, &hotKeyHandler)
        }
        let choice = defaults.integer(forKey: ApplicationIdentity.preferencePrefix + "shortcutChoice")
        let key = choice == 1 ? UInt32(kVK_ANSI_F) : UInt32(kVK_Space)
        let modifiers = choice == 1 ? UInt32(cmdKey | shiftKey) : UInt32(controlKey | optionKey)
        let status = RegisterEventHotKey(key, modifiers, EventHotKeyID(signature: 0x41504653, id: 1), GetApplicationEventTarget(), 0, &hotKey)
        if status != noErr { NSLog("APFSearch global shortcut unavailable (%d)", status) }
    }
}

final class SearchResultsTable: NSTableView {
    weak var owner: SearchWindowController?
    override func keyDown(with event: NSEvent) {
        if event.keyCode == 49 && event.modifierFlags.intersection(.deviceIndependentFlagsMask).isEmpty { owner?.previewSelection(nil); return }
        if event.keyCode == 36 { owner?.openSelection(nil); return }
        super.keyDown(with: event)
    }
    override func menu(for event: NSEvent) -> NSMenu? {
        let row = row(at: convert(event.locationInWindow, from: nil))
        if row >= 0 && !selectedRowIndexes.contains(row) { selectRowIndexes(IndexSet(integer: row), byExtendingSelection: false) }
        return super.menu(for: event)
    }
    @objc func copy(_ sender: Any?) { owner?.copyPaths(sender) }
    @objc func undo(_ sender: Any?) { owner?.undoFileOperation(sender) }
}

final class SearchWindowController: NSWindowController, NSSearchFieldDelegate, NSTableViewDataSource, NSTableViewDelegate, QLPreviewPanelDataSource, QLPreviewPanelDelegate, NSMenuItemValidation, NSWindowDelegate, NSToolbarDelegate, NSToolbarItemValidation {
    let search = NSSearchField()
    let table = SearchResultsTable()
    let statusLabel = NSTextField(labelWithString: L("status.connecting_service"))
    let detailLabel = NSTextField(labelWithString: "")
    let progress = NSProgressIndicator()
    let shortcuts = NSPopUpButton()
    let resultsHeading = NSTextField(labelWithString: L("search.all_files"))
    let pathControl = NSPathControl()
    var searchToolbarItem: NSSearchToolbarItem?
    var emptyStateVisible = false
    let coverageButton = NSButton(title: "", target: nil, action: nil)
    let emptyTitle = NSTextField(labelWithString: "")
    let emptyDetail = NSTextField(wrappingLabelWithString: "")
    let emptyButton = NSButton(title: "", target: nil, action: nil)
    let emptyState = NSStackView()
    let splitController = NSSplitViewController()
    var sidebarController: SearchSidebarViewController!
    var activeFilter = ""
    var activeFilterTitle = L("search.all_files")
    var emptyAction = 0
    let cancelButton = NSButton(title: L("action.stop"), target: nil, action: nil)
    var backgroundRequestID: String?
    var fileOperationRequestID: String?
    var operationReviewPending = false
    var backgroundTaskName: String?
    var offlineListID: String?
    var offlineTitle: String?
    var offlineSourcePath: String?
    var offlineCount = 0
    var cachedRows: [Int: [String: Any]] = [:]
    var total = 0
    var generation: Any?
    var queryScopeToken: String?
    var currentLease: SearchSnapshotLease?
    var snapshotLease: String? { currentLease?.token }
    var selectionResolver: SelectionResolver?
    var selectionResolution = 0
    var statusObservationActive = true
    var statusEpoch = 0
    var statusRetry: DispatchWorkItem?
    var statusRetryDelay: TimeInterval = 1
    var duplicateWindows = [DuplicateResultsWindowController]()
    var querySequence = 0
    var requestIDs = Set<String>()
    var pendingPages = Set<Int>()
    var liveRefreshPending = false
    var resultAnimationInFlight = false
    var deferredQueryReplies = [() -> Void]()
    var statusRequestPending = false
    var progressDelay: Timer?
    var progressRequested = false
    var applyingResultChanges = false
    var progressIsAnimating = false
    var displayedSelectionPaths = [String]()
    var displayedSelectionCount = 0
    var liveScrollInProgress = false
    var liveScrollObservers = [NSObjectProtocol]()
    var queryText = ""
    var currentStatus: [String: Any] = [:]
    var queryWarnings: [String] = []
    var roots: [String] = []
    var pendingQuery: DispatchWorkItem?
    var historyTimer: Timer?
    let statusRequestID = UUID().uuidString
    var elapsed: Double = 0
    var coreElapsed: Double = 0
    var pendingInputStartedAt: TimeInterval?
    var queryStartedAt: TimeInterval = ProcessInfo.processInfo.systemUptime
    var queryStartedFromInput = false
    var initialStatus = true
    var queryPending = false
    var resultsAreCurrent = false
    var pendingSelectedPaths = Set<String>()
    var bookmarkQueries: [String] = []
    var previewURLs: [NSURL] = []
    var iconCache = NSCache<NSString, NSImage>()
    let pageSize = 200
    let dates: DateFormatter = { let date = DateFormatter(); date.locale = .autoupdatingCurrent; date.dateStyle = .medium; date.timeStyle = .short; return date }()
    let sizes = ByteCountFormatter()

    init(offlineListID: String? = nil, sourcePath: String? = nil, count: Int = 0) {
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 1150, height: 740), styleMask: [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView], backing: .buffered, defer: false)
        window.title = L("app.name")
        window.subtitle = ""
        window.minSize = NSSize(width: 850, height: 480)
        window.tabbingIdentifier = "APFSearch.Search"
        window.tabbingMode = .preferred
        super.init(window: window)
        self.offlineListID = offlineListID
        self.offlineSourcePath = sourcePath
        self.offlineTitle = sourcePath.map { URL(fileURLWithPath: $0).lastPathComponent }
        self.offlineCount = count
        if offlineListID != nil { initialStatus = false }
        sidebarController = SearchSidebarViewController(owner: self)
        let resultsController = NSViewController(); resultsController.view = NSView()
        splitController.splitView.dividerStyle = .thin
        let sidebar = NSSplitViewItem(sidebarWithViewController: sidebarController)
        sidebar.minimumThickness = 180; sidebar.maximumThickness = 250; sidebar.preferredThicknessFraction = 0.18
        // Let Auto Layout trade space between the panes. The convenience
        // fixed-split behavior can grow a window at its minimum width while
        // trying to preserve the collapsed pane's fitting width.
        sidebar.collapseBehavior = .useConstraints
        sidebar.allowsFullHeightLayout = true; sidebar.titlebarSeparatorStyle = .none
        sidebar.isCollapsed = !UserDefaults.standard.bool(forKey: ApplicationIdentity.preferencePrefix + "SidebarVisible")
        splitController.addSplitViewItem(sidebar)
        // Even the widest sidebar (250) plus the divider and this minimum fits
        // within the 850-point window minimum. Excess table columns can scroll.
        let results = NSSplitViewItem(viewController: resultsController); results.minimumThickness = 520
        splitController.addSplitViewItem(results)
        window.contentViewController = splitController
        let toolbar = NSToolbar(identifier: "APFSearch.Toolbar")
        toolbar.delegate = self; toolbar.displayMode = .iconOnly; toolbar.allowsUserCustomization = false
        window.titleVisibility = .hidden
        window.titlebarSeparatorStyle = .automatic
        window.toolbarStyle = .unifiedCompact
        configureSearchField()
        window.toolbar = toolbar
        // NSWindow.minSize is not authoritative under Auto Layout. Establish a
        // constant minimum independent of labels, the query, or sidebar state.
        let minimumContentHeight = window.contentRect(forFrameRect: NSRect(x: 0, y: 0, width: 850, height: 480)).height
        NSLayoutConstraint.activate([
            splitController.view.widthAnchor.constraint(greaterThanOrEqualToConstant: 850),
            splitController.view.heightAnchor.constraint(greaterThanOrEqualToConstant: minimumContentHeight)
        ])
        window.nextResponder = self
        window.delegate = self
        iconCache.countLimit = 512
        buildInterface()
        // Restore once after installing the complete content hierarchy. Later
        // status/search updates must never choose or restore a window frame.
        if !window.setFrameUsingName(ApplicationIdentity.preferencePrefix + "Main") { window.center() }
        window.setFrameAutosaveName(ApplicationIdentity.preferencePrefix + "Main")
        restoreTableConfiguration()
        observeLiveScrolling()
        NotificationCenter.default.addObserver(self, selector: #selector(preferencesChanged), name: .searchPreferencesChanged, object: nil)
        pollStatus()
        refreshShortcuts()
        runQuery()
    }
    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }
    deinit {
        progressDelay?.invalidate(); pendingQuery?.cancel(); pendingQuery = nil; historyTimer?.invalidate()
        if offlineListID == nil { SearchClient.shared.call(["op": "cancel", "request_id": statusRequestID]) { _ in } }
        liveScrollObservers.forEach { NotificationCenter.default.removeObserver($0) }
    }

    func buildInterface() {
        guard let content = splitController.splitViewItems.last?.viewController.view else { return }
        shortcuts.addItem(withTitle: L("search.bookmarks_and_history"))
        shortcuts.target = self; shortcuts.action = #selector(selectShortcut); shortcuts.controlSize = .small
        shortcuts.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        shortcuts.cell?.lineBreakMode = .byTruncatingTail
        let scroll = NSScrollView(); scroll.hasVerticalScroller = true; scroll.hasHorizontalScroller = true
        scroll.autohidesScrollers = true; scroll.borderType = .noBorder
        scroll.translatesAutoresizingMaskIntoConstraints = false
        table.owner = self; table.delegate = self; table.dataSource = self
        table.usesAlternatingRowBackgroundColors = true; table.allowsMultipleSelection = true
        table.allowsColumnReordering = true; table.allowsColumnResizing = true
        table.rowSizeStyle = .small; table.rowHeight = 23
        table.style = .inset
        // Initial constraints and the saved window frame must settle before
        // column restoration. Otherwise construction can overwrite autosave.
        table.columnAutoresizingStyle = .noColumnAutoresizing
        table.target = self; table.doubleAction = #selector(openSelection)
        table.setAccessibilityLabel(L("search.results"))
        for (key, title, width) in [("name", L("column.name"), 320.0), ("path", L("column.location"), 300.0), ("size", L("column.size"), 85.0), ("modified", L("column.modified"), 155.0), ("created", L("column.created"), 140.0)] {
            let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier(key))
            column.title = title; column.width = width
            column.minWidth = ["name": 140.0, "path": 160.0, "size": 70.0, "modified": 120.0, "created": 100.0][key] ?? 80
            column.resizingMask = [.autoresizingMask, .userResizingMask]
            column.sortDescriptorPrototype = NSSortDescriptor(key: key, ascending: true)
            table.addTableColumn(column)
        }
        let hiddenColumns = UserDefaults.standard.stringArray(forKey: ApplicationIdentity.preferencePrefix + "HiddenColumns") ?? ["created"]
        let columnMenu = NSMenu()
        for column in table.tableColumns {
            column.isHidden = hiddenColumns.contains(column.identifier.rawValue) && column.identifier.rawValue != "name"
            let item = NSMenuItem(title: column.title, action: #selector(toggleColumn), keyEquivalent: "")
            item.target = self; item.representedObject = column.identifier.rawValue; item.state = column.isHidden ? .off : .on
            columnMenu.addItem(item)
        }
        table.headerView?.menu = columnMenu
        table.registerForDraggedTypes([.fileURL]); table.setDraggingSourceOperationMask([.copy, .move], forLocal: false)
        let menu = NSMenu()
        for (name, selector) in [(L("action.open"), #selector(openSelection)), (L("action.reveal_in_finder"), #selector(revealSelection)), (L("action.quick_look"), #selector(previewSelection)), (L("action.copy_path"), #selector(copyPaths)), (L("action.rename_dialog"), #selector(renameSelection)), (L("action.copy_dialog"), #selector(copySelection)), (L("action.move_dialog"), #selector(moveSelection)), (L("action.confirm_trash"), #selector(trashSelection)), (L("index.index_file_contents"), #selector(indexContent))] {
            let item = NSMenuItem(title: name, action: selector, keyEquivalent: ""); item.target = self; menu.addItem(item)
        }
        table.menu = menu
        scroll.documentView = table; content.addSubview(scroll)
        progress.style = .spinning; progress.controlSize = .small; progress.isDisplayedWhenStopped = false
        statusLabel.font = .monospacedDigitSystemFont(ofSize: 11, weight: .regular)
        statusLabel.textColor = .secondaryLabelColor; statusLabel.lineBreakMode = .byTruncatingTail
        statusLabel.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        detailLabel.font = .systemFont(ofSize: 11); detailLabel.textColor = .secondaryLabelColor
        detailLabel.lineBreakMode = .byTruncatingMiddle
        detailLabel.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        cancelButton.target = self; cancelButton.action = #selector(cancelBackground); cancelButton.bezelStyle = .rounded; cancelButton.controlSize = .small; cancelButton.isHidden = true
        coverageButton.target = self; coverageButton.action = #selector(showCoverage)
        coverageButton.controlSize = .small; coverageButton.bezelStyle = .inline; coverageButton.isHidden = true
        coverageButton.image = NSImage(systemSymbolName: "exclamationmark.triangle", accessibilityDescription: L("index.uncovered_locations")); coverageButton.imagePosition = .imageLeading
        let footer = NSStackView(views: [progress, statusLabel, cancelButton, coverageButton, NSView(), detailLabel]); footer.spacing = 7
        footer.setClippingResistancePriority(.defaultLow, for: .horizontal)
        pathControl.pathStyle = .standard; pathControl.isEditable = false; pathControl.controlSize = .small
        pathControl.target = self; pathControl.doubleAction = #selector(revealPathComponent)
        pathControl.isHidden = true
        pathControl.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        // Selection changes only the contents of this fixed-height slot. Hiding
        // a path must not resize the table's scrolling viewport.
        let pathSlot = NSView(); pathSlot.translatesAutoresizingMaskIntoConstraints = false
        pathControl.translatesAutoresizingMaskIntoConstraints = false
        pathSlot.addSubview(pathControl)
        NSLayoutConstraint.activate([
            pathSlot.heightAnchor.constraint(equalToConstant: 20),
            pathControl.leadingAnchor.constraint(equalTo: pathSlot.leadingAnchor),
            pathControl.trailingAnchor.constraint(equalTo: pathSlot.trailingAnchor),
            pathControl.centerYAnchor.constraint(equalTo: pathSlot.centerYAnchor),
            pathControl.heightAnchor.constraint(equalToConstant: 20)
        ])
        let footerStack = NSStackView(views: [pathSlot, footer]); footerStack.orientation = .vertical; footerStack.alignment = .leading; footerStack.spacing = 4
        footerStack.translatesAutoresizingMaskIntoConstraints = false; content.addSubview(footerStack)
        footer.widthAnchor.constraint(equalTo: footerStack.widthAnchor).isActive = true
        pathSlot.widthAnchor.constraint(equalTo: footerStack.widthAnchor).isActive = true
        NSLayoutConstraint.activate([
            scroll.topAnchor.constraint(equalTo: content.safeAreaLayoutGuide.topAnchor), scroll.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 6), scroll.trailingAnchor.constraint(equalTo: content.trailingAnchor, constant: -6), scroll.bottomAnchor.constraint(equalTo: footerStack.topAnchor, constant: -7),
            footerStack.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 14), footerStack.trailingAnchor.constraint(equalTo: content.trailingAnchor, constant: -14), footerStack.bottomAnchor.constraint(equalTo: content.bottomAnchor, constant: -8), footer.heightAnchor.constraint(equalToConstant: 18),
            progress.widthAnchor.constraint(equalToConstant: 14), progress.heightAnchor.constraint(equalToConstant: 14)
        ])
        buildEmptyState(in: content, scroll: scroll)
        searchToolbarItem?.beginSearchInteraction()
        window?.makeFirstResponder(search)
    }

    func restoreTableConfiguration() {
        // AppKit restores by column identifier, so every column must exist.
        // Complete the initial layout at the restored window size first; only
        // subsequent resizes should redistribute the saved column widths.
        window?.contentView?.layoutSubtreeIfNeeded()
        table.columnAutoresizingStyle = .uniformColumnAutoresizingStyle
        table.autosaveName = ApplicationIdentity.preferencePrefix + "Columns"
        table.autosaveTableColumns = true
        if table.sortDescriptors.isEmpty {
            table.sortDescriptors = [NSSortDescriptor(key: "name", ascending: true)]
        }
        for item in table.headerView?.menu?.items ?? [] {
            guard let identifier = item.representedObject as? String,
                  let column = table.tableColumns.first(where: { $0.identifier.rawValue == identifier }) else { continue }
            item.state = column.isHidden ? .off : .on
        }
    }

    func configureSearchField() {
        search.placeholderString = L("search.field_label")
        search.font = .systemFont(ofSize: 14); search.controlSize = .regular
        search.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        search.delegate = self; search.sendsSearchStringImmediately = true; search.sendsWholeSearchString = false
        search.recentsAutosaveName = "APFSearch.Queries"; search.maximumRecents = 30
        search.setAccessibilityLabel(L("search.field_label"))
    }
    func toolbarAllowedItemIdentifiers(_ toolbar: NSToolbar) -> [NSToolbarItem.Identifier] { toolbarDefaultItemIdentifiers(toolbar) }
    func toolbarDefaultItemIdentifiers(_ toolbar: NSToolbar) -> [NSToolbarItem.Identifier] {
        [NSToolbarItem.Identifier("APFSearch.Sidebar"), NSToolbarItem.Identifier("APFSearch.SearchField"), .flexibleSpace, NSToolbarItem.Identifier("APFSearch.Preview"), NSToolbarItem.Identifier("APFSearch.Scope"), NSToolbarItem.Identifier("APFSearch.Actions")]
    }
    func toolbar(_ toolbar: NSToolbar, itemForItemIdentifier identifier: NSToolbarItem.Identifier, willBeInsertedIntoToolbar flag: Bool) -> NSToolbarItem? {
        if identifier.rawValue == "APFSearch.SearchField" {
            let item = NSSearchToolbarItem(itemIdentifier: identifier)
            item.label = L("search.title"); item.searchField = search
            // A safe preference, not a required width tied back to window.frame.
            // AppKit can fit the search item around the other native controls.
            item.preferredWidthForSearchField = 520
            item.resignsFirstResponderWithCancel = false
            searchToolbarItem = item
            return item
        }
        if identifier.rawValue == "APFSearch.Actions" {
            let item = NSMenuToolbarItem(itemIdentifier: identifier)
            item.label = L("menu.file"); item.image = NSImage(systemSymbolName: "ellipsis.circle", accessibilityDescription: item.label)
            let menu = NSMenu()
            for (title, action) in [(L("action.open"), #selector(openSelection)), (L("action.reveal_in_finder"), #selector(revealSelection)), (L("action.copy_path"), #selector(copyPaths)), (L("action.rename_dialog"), #selector(renameSelection)), (L("action.copy_dialog"), #selector(copySelection)), (L("action.move_dialog"), #selector(moveSelection)), (L("action.confirm_trash"), #selector(trashSelection))] {
                let command = NSMenuItem(title: title, action: action, keyEquivalent: ""); command.target = self; menu.addItem(command)
            }
            menu.addItem(.separator())
            for (title, action) in [(L("action.bookmark_current_search"), #selector(addBookmark)), (L("action.export_file_list"), #selector(exportList)), (L("action.import_file_list"), #selector(importList)), (L("action.open_settings"), #selector(showSettingsToolbar))] {
                let command = NSMenuItem(title: title, action: action, keyEquivalent: ""); command.target = self; menu.addItem(command)
            }
            item.menu = menu
            return item
        }
        let item = NSToolbarItem(itemIdentifier: identifier); item.target = self
        switch identifier.rawValue {
        case "APFSearch.Sidebar": item.label = L("action.toggle_sidebar"); item.image = NSImage(systemSymbolName: "sidebar.left", accessibilityDescription: item.label); item.action = #selector(toggleSidebar); item.isNavigational = true
        case "APFSearch.Scope": item.label = offlineListID == nil ? L("index.scope") : L("offline.list_source"); item.image = NSImage(systemSymbolName: offlineListID == nil ? "externaldrive" : "doc.text", accessibilityDescription: item.label); item.action = #selector(showCoverage)
        case "APFSearch.Preview": item.label = L("action.quick_look"); item.image = NSImage(systemSymbolName: "eye", accessibilityDescription: item.label); item.action = #selector(previewSelection)
        default: return nil
        }
        item.toolTip = item.label
        return item
    }
    func validateToolbarItem(_ item: NSToolbarItem) -> Bool {
        if item.itemIdentifier.rawValue == "APFSearch.Preview" { return canUseSelection && offlineListID == nil }
        return true
    }
    @objc func showSettingsToolbar(_ sender: Any?) { (NSApp.delegate as? AppDelegate)?.showSettings(sender) }
    @objc func toggleSidebar(_ sender: Any?) {
        guard let sidebar = splitController.splitViewItems.first else { return }
        // The split controller owns the native animation and its interruption
        // or reversal. Do not gate future user input on an enclosing animation
        // context's completion, which is not this control's lifecycle.
        if NSWorkspace.shared.accessibilityDisplayShouldReduceMotion {
            sidebar.isCollapsed.toggle()
        } else {
            splitController.toggleSidebar(sender)
        }
        UserDefaults.standard.set(!sidebar.isCollapsed, forKey: ApplicationIdentity.preferencePrefix + "SidebarVisible")
    }
    @objc func toggleColumn(_ sender: NSMenuItem) {
        guard let id = sender.representedObject as? String, id != "name", let column = table.tableColumns.first(where: { $0.identifier.rawValue == id }) else { return }
        column.isHidden.toggle(); sender.state = column.isHidden ? .off : .on
        UserDefaults.standard.set(table.tableColumns.filter(\.isHidden).map { $0.identifier.rawValue }, forKey: ApplicationIdentity.preferencePrefix + "HiddenColumns")
    }
    @objc func revealPathComponent(_ sender: Any?) {
        guard offlineListID == nil, let path = pathControl.clickedPathItem?.url?.path else { return }
        SearchClient.shared.call(["op": "files", "action": "reveal", "paths": [path]]) { [weak self] reply in self?.checkResult(reply) }
    }
    func selectFilter(query: String, title: String) {
        activeFilter = query; activeFilterTitle = title; resultsHeading.stringValue = title; runQuery()
    }
    func buildEmptyState(in content: NSView, scroll: NSScrollView) {
        let icon = NSImageView(image: NSImage(systemSymbolName: "magnifyingglass", accessibilityDescription: nil) ?? NSImage())
        icon.contentTintColor = .tertiaryLabelColor
        icon.translatesAutoresizingMaskIntoConstraints = false
        NSLayoutConstraint.activate([icon.widthAnchor.constraint(equalToConstant: 38), icon.heightAnchor.constraint(equalToConstant: 38)])
        emptyTitle.font = .systemFont(ofSize: 17, weight: .semibold); emptyTitle.alignment = .center
        emptyDetail.font = .systemFont(ofSize: 13); emptyDetail.textColor = .secondaryLabelColor; emptyDetail.alignment = .center
        emptyDetail.maximumNumberOfLines = 4
        emptyButton.bezelStyle = .rounded; emptyButton.target = self; emptyButton.action = #selector(emptyButtonPressed)
        emptyState.orientation = .vertical; emptyState.alignment = .centerX; emptyState.spacing = 12
        emptyState.wantsLayer = true; emptyState.isHidden = true
        [icon, emptyTitle, emptyDetail, emptyButton].forEach { emptyState.addArrangedSubview($0) }
        emptyState.translatesAutoresizingMaskIntoConstraints = false; content.addSubview(emptyState)
        NSLayoutConstraint.activate([emptyState.centerXAnchor.constraint(equalTo: scroll.centerXAnchor), emptyState.centerYAnchor.constraint(equalTo: scroll.centerYAnchor, constant: -20), emptyState.widthAnchor.constraint(equalToConstant: 410), emptyDetail.widthAnchor.constraint(lessThanOrEqualToConstant: 400)])
    }
    func updateEmptyState() {
        if table.usesAlternatingRowBackgroundColors != (total > 0) {
            table.usesAlternatingRowBackgroundColors = total > 0
        }
        // Keep a stable empty state while a new search is in flight. The footer
        // owns progress; replacing the whole centre on each keystroke flashes.
        guard !queryPending else { return }
        let shouldShow = total == 0
        guard shouldShow else { setEmptyStateVisible(false); return }
        let title: String
        let detail: String
        var buttonTitle = ""
        var buttonHidden = false
        var action = 1
        if currentStatus["success"] as? Bool == false && offlineListID == nil {
            title = L("index.index_service_not_connected"); detail = errorText(currentStatus)
            if SearchClient.shared.requiresApproval {
                buttonTitle = L("settings.open_background_service_settings"); action = 2
            } else {
                buttonTitle = L("service.retry_connection"); action = 3
            }
        } else if boolValue(currentStatus["scanning"]) && offlineListID == nil {
            title = L("status.building_index"); detail = L("index.progressive_results_hint"); buttonHidden = true
        } else if !queryWarnings.isEmpty {
            title = L("search.invalid_query_title"); detail = queryWarnings.joined(separator: "\n")
            buttonTitle = L("search.clear_search_and_filters")
        } else if offlineListID == nil && roots.isEmpty {
            title = L("search.find_files_on_this_mac"); detail = L("index.choose_scope_hint")
            buttonTitle = L("index.choose_index_scope_dialog"); action = 0
        } else {
            title = offlineListID == nil ? L("search.no_matching_files") : L("search.no_matching_records_in_this_list")
            detail = L("search.no_results_hint")
            buttonTitle = L("search.clear_search_and_filters")
        }
        // In particular, do not unhide and immediately re-hide the arranged
        // button on each scanning status poll: that invalidates stack layout.
        if emptyTitle.stringValue != title { emptyTitle.stringValue = title }
        if emptyDetail.stringValue != detail { emptyDetail.stringValue = detail }
        if emptyButton.title != buttonTitle { emptyButton.title = buttonTitle }
        if emptyButton.isHidden != buttonHidden { emptyButton.isHidden = buttonHidden }
        emptyAction = action
        setEmptyStateVisible(true)
    }
    func setEmptyStateVisible(_ visible: Bool) {
        guard emptyStateVisible != visible else { return }
        emptyStateVisible = visible
        // This view owns this layer. Removing its old opacity animation before
        // a new transition prevents a previous fade from reappearing later.
        emptyState.layer?.removeAllAnimations()
        let animate = visible && !NSWorkspace.shared.accessibilityDisplayShouldReduceMotion
        NSAnimationContext.runAnimationGroup { context in
            context.duration = 0
            context.allowsImplicitAnimation = false
            emptyState.alphaValue = animate ? 0 : 1
            emptyState.isHidden = !visible
        }
        if animate {
            NSAnimationContext.runAnimationGroup { context in
                context.duration = 0.16
                emptyState.animator().alphaValue = 1
            }
        }
    }
    @objc func emptyButtonPressed(_ sender: Any?) {
        if emptyAction == 2 { SMAppService.openSystemSettingsLoginItems() }
        else if emptyAction == 3 {
            SearchClient.shared.retryConnection()
            pollStatus()
        }
        else if emptyAction == 0 { chooseVolumes(nil) }
        else { search.stringValue = ""; sidebarController.selectAll(); focusSearch(nil) }
    }

    func controlTextDidChange(_ obj: Notification) {
        pendingInputStartedAt = ProcessInfo.processInfo.systemUptime
        resultsAreCurrent = false
        cancelQueries(); querySequence += 1
        pendingQuery?.cancel(); pendingQuery = nil
        let sequence = querySequence
        let work = DispatchWorkItem { [weak self] in
            guard let self, self.querySequence == sequence, self.pendingQuery != nil else { return }
            self.pendingQuery = nil
            if let editor = self.search.currentEditor() as? NSTextView, editor.hasMarkedText() { return }
            self.runQuery()
        }
        pendingQuery = work
        // Coalesce notifications from the current event without imposing a
        // fixed delay. Input invalidates older replies before this work runs.
        DispatchQueue.main.async(execute: work)
    }
    @objc func focusSearch(_ sender: Any?) { searchToolbarItem?.beginSearchInteraction(); window?.makeFirstResponder(search); search.selectText(nil) }
    @objc func preferencesChanged(_ notification: Notification) { refreshShortcuts(); runQuery() }
    func cancelQueries() {
        currentLease = nil
        selectionResolver?.cancel(); selectionResolver = nil; selectionResolution += 1
        for id in requestIDs {
            var request: [String: Any] = ["op": "cancel", "request_id": id]
            if let offlineListID { request["list_id"] = offlineListID }
            SearchClient.shared.call(request) { _ in }
        }
        requestIDs.removeAll(); pendingPages.removeAll(); liveRefreshPending = false
        deferredQueryReplies.removeAll()
    }
    func runQuery() {
        if let editor = search.currentEditor() as? NSTextView, editor.hasMarkedText() { return }
        pendingQuery?.cancel(); pendingQuery = nil
        queryStartedFromInput = pendingInputStartedAt != nil
        queryStartedAt = pendingInputStartedAt ?? ProcessInfo.processInfo.systemUptime
        pendingInputStartedAt = nil
        elapsed = 0; coreElapsed = 0
        cancelQueries(); querySequence += 1
        let typed = search.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        queryText = activeFilter.isEmpty ? typed : (typed.isEmpty ? activeFilter : "<" + typed + "> <" + activeFilter + ">")
        pendingSelectedPaths = Set(selectedPaths)
        queryWarnings = []; generation = nil; queryPending = true; resultsAreCurrent = false
        // Preserve selection visuals while typing; action validation prevents stale operations.
        table.isEnabled = true
        window?.toolbar?.validateVisibleItems()
        // Keep the old page visible while the next snapshot is prepared. Actions
        // remain disabled until the new first page has replaced these records.
        updateStatus(); requestPage(0)
    }
    func requestPage(_ page: Int) {
        guard !pendingPages.contains(page), page == 0 || resultsAreCurrent else { return }
        let sequence = querySequence, startedAt = queryStartedAt, startedFromInput = queryStartedFromInput
        let isInitialQuery = queryPending && generation == nil
        let id = UUID().uuidString; requestIDs.insert(id); pendingPages.insert(page)
        var request: [String: Any] = ["op": "query", "text": queryText, "offset": page * pageSize, "limit": pageSize, "request_id": id, "sort": table.sortDescriptors.map { ["field": $0.key ?? "name", "ascending": $0.ascending] as [String: Any] }]
        if isInitialQuery { request["retain_snapshot"] = true; request["snapshot_owner"] = "window" }
        else if let snapshotLease { request["snapshot_lease"] = snapshotLease }
        if let generation { request["generation"] = generation }
        if let offlineListID { request["list_id"] = offlineListID }
        let leaseListID = offlineListID
        SearchClient.shared.call(request) { [weak self] reply in
            let incomingLease = (reply["snapshot_lease"] as? String).map { SearchSnapshotLease(token: $0, listID: leaseListID) }
            guard let self else { return }
            self.afterResultAnimation { [weak self] in
            guard let self, sequence == self.querySequence else { return }
            self.requestIDs.remove(id); self.pendingPages.remove(page)
            if isInitialQuery { self.queryPending = false }
            if reply["success"] as? Bool == false {
                let error = self.errorText(reply)
                if (error.contains("generation") || error.contains("snapshot") || error.contains("stale")) && !isInitialQuery { self.refreshVisibleResults(); return }
                self.queryWarnings = [error]; self.updateStatus(); return
            }
            if isInitialQuery { self.currentLease = incomingLease; self.queryScopeToken = reply["scope_token"] as? String }
            self.generation = reply["generation"] ?? self.generation
            let rows = reply["rows"] as? [[String: Any]] ?? []
            let count = (reply["total"] as? NSNumber)?.intValue ?? 0
            self.applyingResultChanges = true
            self.applyResultRows(rows, offset: page * self.pageSize, count: count, replaceCache: isInitialQuery, animate: false)
            if isInitialQuery {
                self.resultsAreCurrent = true
                self.coreElapsed = (reply["elapsed_ms"] as? NSNumber)?.doubleValue ?? 0
                let restore = IndexSet(rows.enumerated().compactMap { offset, row in (row["path"] as? String).map { self.pendingSelectedPaths.contains($0) ? offset : nil } ?? nil })
                self.table.selectRowIndexes(restore, byExtendingSelection: false)
                if self.total > 0 { self.table.scrollRowToVisible(restore.first ?? 0) }
                self.window?.toolbar?.validateVisibleItems()
                let title = self.offlineListID == nil ? L("app.name") : (self.offlineTitle ?? L("export.file_list")) + L("offline.suffix")
                let label = self.search.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
                self.window?.title = label.isEmpty ? title : label + " — " + title
            }
            self.applyingResultChanges = false
            self.synchronizeSelection()
            self.queryWarnings = self.stringList(reply["warnings"]); self.updateStatus()
            if isInitialQuery {
                self.window?.contentView?.layoutSubtreeIfNeeded(); self.table.displayIfNeeded()
                self.elapsed = max(0, (ProcessInfo.processInfo.systemUptime - startedAt) * 1000)
                self.updateStatus()
                SearchMetrics.record(["query": self.queryText, "sort": request["sort"] as? [[String: Any]] ?? [], "total": self.total, "end_to_end_ms": self.elapsed, "core_ms": self.coreElapsed, "input_event": startedFromInput, "visible": self.window?.isVisible == true, "list_id": self.offlineListID ?? "", "boundary": "AppKit displayIfNeeded submission"])
                self.recordHistory()
            }
            }
        }
    }

    /// NSTableView keeps its row views and scroll geometry when metadata changes.
    /// New query pages and background index updates never reload the whole table.
    func afterResultAnimation(_ action: @escaping () -> Void) {
        if resultAnimationInFlight { deferredQueryReplies.append(action) }
        else { action() }
    }
    func finishResultAnimation() {
        resultAnimationInFlight = false
        let replies = deferredQueryReplies
        deferredQueryReplies.removeAll()
        replies.forEach { $0() }
    }
    func applyResultRows(_ rows: [[String: Any]], offset: Int, count: Int, replaceCache: Bool, animate: Bool) {
        let previous = cachedRows, oldCount = total
        if replaceCache { cachedRows.removeAll(keepingCapacity: true) }
        for (n, row) in rows.enumerated() { cachedRows[offset + n] = row }
        total = count
        var changes = IndexSet(rows.indices.compactMap { n -> Int? in
            let index = offset + n
            guard let old = previous[index] else { return index }
            return NSDictionary(dictionary: old).isEqual(to: rows[n]) ? nil : index
        })
        let wasApplying = applyingResultChanges
        applyingResultChanges = true
        defer { applyingResultChanges = wasApplying }
        var animatedStructure = false
        // AppKit retains and moves surviving row views. Compare those records by
        // file identity: shifted row numbers do not mean their cells changed.
        if animate && !resultAnimationInFlight && oldCount <= pageSize && count <= pageSize && offset == 0,
           previous.count == oldCount && rows.count == count {
            let oldIDs = (0..<oldCount).compactMap { previous[$0]?["path"] as? String }
            let newIDs = rows.compactMap { $0["path"] as? String }
            let difference = newIDs.difference(from: oldIDs)
            if !difference.isEmpty && difference.count <= 20 {
                var removed = IndexSet(), inserted = IndexSet()
                for change in difference {
                    switch change { case .remove(let index, _, _): removed.insert(index); case .insert(let index, _, _): inserted.insert(index) }
                }
                let previousByPath = Dictionary(uniqueKeysWithValues: previous.values.compactMap { row in (row["path"] as? String).map { ($0, row) } })
                changes = IndexSet(rows.indices.filter { n in
                    guard !inserted.contains(n), let path = rows[n]["path"] as? String, let old = previousByPath[path] else { return false }
                    return !NSDictionary(dictionary: old).isEqual(to: rows[n])
                })
                let reduceMotion = NSWorkspace.shared.accessibilityDisplayShouldReduceMotion
                resultAnimationInFlight = true
                NSAnimationContext.runAnimationGroup({ context in
                    context.duration = reduceMotion ? 0 : 0.16
                    table.beginUpdates()
                    table.removeRows(at: removed, withAnimation: reduceMotion ? [] : [.effectFade])
                    table.insertRows(at: inserted, withAnimation: reduceMotion ? [] : [.effectFade])
                    table.endUpdates()
                }, completionHandler: { [weak self] in self?.finishResultAnimation() })
                animatedStructure = true
            }
        }
        if oldCount != count && !animatedStructure {
            table.noteNumberOfRowsChanged()
            // AppKit binds newly added rows from the updated data source. Only
            // surviving rows can still contain cells from the previous result.
            // Reloading new rows discards cells that AppKit just created.
            if count > oldCount { changes.remove(integersIn: oldCount..<count) }
        }
        let visible = table.rows(in: table.visibleRect)
        if visible.location != NSNotFound && visible.length > 0 {
            let upper = min(count, visible.location + visible.length)
            if upper > visible.location {
                let visibleChanges = changes.intersection(IndexSet(integersIn: visible.location..<upper))
                if !visibleChanges.isEmpty { table.reloadData(forRowIndexes: visibleChanges, columnIndexes: IndexSet(integersIn: 0..<table.numberOfColumns)) }
            }
        }
        SearchMetrics.record(["event": "table_update", "background": animate, "changed_rows": changes.count, "row_count_changed": oldCount != count, "full_reload": false])
    }
    func observeLiveScrolling() {
        guard let scroll = table.enclosingScrollView else { return }
        liveScrollObservers.append(NotificationCenter.default.addObserver(forName: NSScrollView.willStartLiveScrollNotification, object: scroll, queue: .main) { [weak self] _ in self?.liveScrollInProgress = true })
        liveScrollObservers.append(NotificationCenter.default.addObserver(forName: NSScrollView.didEndLiveScrollNotification, object: scroll, queue: .main) { [weak self] _ in self?.liveScrollInProgress = false; self?.refreshVisibleResults() })
    }
    func refreshVisibleResults() {
        guard offlineListID == nil, resultsAreCurrent, !queryPending, !liveRefreshPending, !resultAnimationInFlight,
              pendingInputStartedAt == nil, !liveScrollInProgress, NSEvent.pressedMouseButtons == 0,
              window?.isVisible == true else { return }
        if let editor = search.currentEditor() as? NSTextView, editor.hasMarkedText() { return }
        if let known = generation as? NSNumber, let latest = currentStatus["generation"] as? NSNumber,
           known == latest, queryScopeToken == nil || queryScopeToken == currentStatus["scope_token"] as? String { return }
        let visible = table.rows(in: table.visibleRect)
        let first = visible.location == NSNotFound ? 0 : max(0, visible.location)
        let offset = first / pageSize * pageSize, limit = pageSize * 2
        // Keep a multi-selection pinned while the user works outside its viewport.
        if let selected = table.selectedRowIndexes.first, selected < offset { return }
        if let selected = table.selectedRowIndexes.last, selected >= offset + limit { return }
        let anchorRow = cachedRows[first]
        let anchor = anchorRow?["path"] as? String
        let oldOrigin = table.enclosingScrollView?.contentView.bounds.origin ?? .zero
        let pixelOffset = total > first ? oldOrigin.y - table.rect(ofRow: first).minY : 0
        let sequence = querySequence, id = UUID().uuidString
        liveRefreshPending = true; requestIDs.insert(id)
        var request: [String: Any] = ["op": "query", "text": queryText, "offset": offset, "limit": limit, "request_id": id, "sort": table.sortDescriptors.map { ["field": $0.key ?? "name", "ascending": $0.ascending] as [String: Any] }]
        request["retain_snapshot"] = true
        request["snapshot_owner"] = "window"
        if let anchor {
            request["anchor_path"] = anchor; request["anchor_delta"] = first - offset
            // Preserve the SQLite row ID as an integer; the core also checks
            // the path because a renamed or removed entry may have a new ID.
            if let identifier = anchorRow?["id"] as? NSNumber { request["anchor_id"] = identifier }
        }
        SearchClient.shared.call(request) { [weak self] reply in
            let incomingLease = (reply["snapshot_lease"] as? String).map { SearchSnapshotLease(token: $0, listID: nil) }
            guard let self, sequence == self.querySequence else { return }
            self.liveRefreshPending = false; self.requestIDs.remove(id)
            guard reply["success"] as? Bool == true, !self.queryPending, self.pendingInputStartedAt == nil,
                  !self.liveScrollInProgress, !self.resultAnimationInFlight, NSEvent.pressedMouseButtons == 0 else { return }
            let currentVisible = self.table.rows(in: self.table.visibleRect)
            guard currentVisible.location == visible.location else { return }
            if let clip = self.table.enclosingScrollView?.contentView,
               abs(clip.bounds.origin.y - oldOrigin.y) > 0.5 || abs(clip.bounds.origin.x - oldOrigin.x) > 0.5 { return }
            if let known = self.generation as? NSNumber, let incoming = reply["generation"] as? NSNumber,
               known == incoming, reply["scope_token"] as? String == nil || reply["scope_token"] as? String == self.queryScopeToken { return }
            // Selection can change while XPC is in flight (including Select All).
            // Do not replace a cache that cannot represent every selected file.
            if let selected = self.table.selectedRowIndexes.first, selected < offset { return }
            if let selected = self.table.selectedRowIndexes.last, selected >= offset + limit { return }
            guard self.table.selectedRowIndexes.count == self.selectedPaths.count else { return }
            let selection = Set(self.selectedPaths)
            self.cancelQueries(); self.querySequence += 1
            self.currentLease = incomingLease
            self.queryScopeToken = reply["scope_token"] as? String
            self.generation = reply["generation"]
            let rows = reply["rows"] as? [[String: Any]] ?? []
            let start = (reply["offset"] as? NSNumber)?.intValue ?? offset
            let count = (reply["total"] as? NSNumber)?.intValue ?? 0
            self.applyingResultChanges = true
            self.applyResultRows(rows, offset: start, count: count, replaceCache: true, animate: true)
            let selected = IndexSet(rows.enumerated().compactMap { n, row in (row["path"] as? String).map { selection.contains($0) ? start + n : nil } ?? nil })
            if selected != self.table.selectedRowIndexes { self.table.selectRowIndexes(selected, byExtendingSelection: false) }
            if let clip = self.table.enclosingScrollView?.contentView, count > 0 {
                let anchorIndex = min(count - 1, (reply["anchor_index"] as? NSNumber)?.intValue ?? first)
                // Headers and automatic insets can place the native top boundary
                // below zero. Constrain in clip-view coordinates, not row indices.
                var proposedBounds = clip.bounds
                proposedBounds.origin = NSPoint(x: oldOrigin.x, y: self.table.rect(ofRow: anchorIndex).minY + pixelOffset)
                let point = clip.constrainBoundsRect(proposedBounds).origin
                if abs(clip.bounds.origin.y - point.y) > 0.5 { clip.scroll(to: point); self.table.enclosingScrollView?.reflectScrolledClipView(clip) }
            }
            self.applyingResultChanges = false
            self.synchronizeSelection()
            self.updateStatus()
        }
    }
    func recordHistory() {
        historyTimer?.invalidate()
        let text = queryText
        guard !text.isEmpty else { return }
        historyTimer = Timer.scheduledTimer(withTimeInterval: 0.8, repeats: false) { [weak self] _ in
            guard let self, self.queryText == text else { return }
            SearchClient.shared.call(["op": "preferences", "action": "get"]) { reply in
                guard reply["success"] as? Bool != false else { return }
                let values = reply["values"] as? [String: Any] ?? reply
                var history = values["history"] as? [String] ?? []
                history.removeAll { $0 == text }; history.insert(text, at: 0)
                SearchClient.shared.call(["op": "preferences", "action": "set", "values": ["history": Array(history.prefix(100))]]) { _ in self.refreshShortcuts() }
            }
        }
    }
    func numberOfRows(in tableView: NSTableView) -> Int { total }
    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        guard let column = tableColumn else { return nil }
        let identifier = column.identifier
        let cell: NSTableCellView
        if let existing = tableView.makeView(withIdentifier: identifier, owner: self) as? NSTableCellView { cell = existing }
        else {
            cell = NSTableCellView(); cell.identifier = identifier
            let text = NSTextField(labelWithString: ""); text.font = .systemFont(ofSize: 12); text.lineBreakMode = .byTruncatingMiddle; text.translatesAutoresizingMaskIntoConstraints = false
            cell.textField = text; cell.addSubview(text)
            if identifier.rawValue == "name" {
                let icon = NSImageView(); icon.translatesAutoresizingMaskIntoConstraints = false; cell.imageView = icon; cell.addSubview(icon)
                NSLayoutConstraint.activate([icon.leadingAnchor.constraint(equalTo: cell.leadingAnchor, constant: 6), icon.centerYAnchor.constraint(equalTo: cell.centerYAnchor), icon.widthAnchor.constraint(equalToConstant: 16), icon.heightAnchor.constraint(equalToConstant: 16), text.leadingAnchor.constraint(equalTo: icon.trailingAnchor, constant: 7)])
            } else { text.leadingAnchor.constraint(equalTo: cell.leadingAnchor, constant: 7).isActive = true }
            NSLayoutConstraint.activate([text.trailingAnchor.constraint(equalTo: cell.trailingAnchor, constant: -7), text.centerYAnchor.constraint(equalTo: cell.centerYAnchor)])
        }
        guard let record = cachedRows[row] else {
            cell.textField?.stringValue = identifier.rawValue == "name" ? L("status.loading") : ""; cell.imageView?.image = nil
            let sequence = querySequence
            DispatchQueue.main.async { [weak self] in
                guard let self, self.querySequence == sequence, row < self.total,
                      NSLocationInRange(row, self.table.rows(in: self.table.visibleRect)) else { return }
                self.requestPage(row / self.pageSize)
            }
            return cell
        }
        let path = record["path"] as? String ?? ""
        cell.toolTip = path; cell.textField?.textColor = .labelColor
        switch identifier.rawValue {
        case "name":
            cell.textField?.stringValue = displayName(record, path: path)
            let cacheKey = path as NSString
            if offlineListID != nil {
                cell.imageView?.image = NSImage(systemSymbolName: boolValue(record["is_dir"]) ? "folder" : "doc", accessibilityDescription: L("offline.file_record"))
            } else if let image = iconCache.object(forKey: cacheKey) { cell.imageView?.image = image }
            else {
                cell.imageView?.image = NSImage(systemSymbolName: boolValue(record["is_dir"]) ? "folder" : "doc", accessibilityDescription: nil)
                DispatchQueue.global(qos: .utility).async { [weak self, weak cell] in
                    let image = NSWorkspace.shared.icon(forFile: path)
                    DispatchQueue.main.async {
                        self?.iconCache.setObject(image, forKey: cacheKey)
                        if cell?.toolTip == path { cell?.imageView?.image = image }
                    }
                }
            }
        case "path": cell.textField?.stringValue = displayParent(path); cell.textField?.textColor = .secondaryLabelColor
        case "size":
            cell.textField?.alignment = .right
            cell.textField?.stringValue = boolValue(record["is_dir"]) || record["size"] == nil ? "—" : sizes.string(fromByteCount: (record["size"] as? NSNumber)?.int64Value ?? 0)
        case "modified", "created":
            if let value = record[identifier.rawValue] as? NSNumber { cell.textField?.stringValue = dates.string(from: Date(timeIntervalSince1970: value.doubleValue > 1e12 ? value.doubleValue / 1000 : value.doubleValue)) } else { cell.textField?.stringValue = "—" }
        default: break
        }
        return cell
    }
    func isWindowsListPath(_ path: String) -> Bool {
        offlineListID != nil && (path.hasPrefix("\\\\") || path.range(of: #"^[A-Za-z]:\\"#, options: .regularExpression) != nil)
    }
    func displayName(_ record: [String: Any], path: String) -> String {
        if isWindowsListPath(path) { return path.split(separator: "\\", omittingEmptySubsequences: true).last.map(String.init) ?? path }
        return record["name"] as? String ?? URL(fileURLWithPath: path).lastPathComponent
    }
    func displayParent(_ path: String) -> String {
        if isWindowsListPath(path), let separator = path.lastIndex(of: "\\") { return String(path[..<separator]) }
        return URL(fileURLWithPath: path).deletingLastPathComponent().path
    }
    func tableView(_ tableView: NSTableView, sortDescriptorsDidChange oldDescriptors: [NSSortDescriptor]) {
        if window?.isVisible == true { runQuery() }
    }
    func tableViewSelectionDidChange(_ notification: Notification) {
        guard !applyingResultChanges else { return }
        synchronizeSelection()
    }
    func synchronizeSelection() {
        let paths = selectedPaths
        let selectionCount = table.selectedRowIndexes.count
        guard paths != displayedSelectionPaths || selectionCount != displayedSelectionCount else { return }
        displayedSelectionPaths = paths; displayedSelectionCount = selectionCount
        pathControl.isHidden = selectionCount != 1 || !selectionIsComplete || offlineListID != nil
        if !pathControl.isHidden, let path = selectedPaths.first { pathControl.url = URL(fileURLWithPath: path) }
        window?.toolbar?.validateVisibleItems()
        detailLabel.stringValue = selectionCount == 1 && paths.count == 1 ? (offlineListID == nil ? "" : paths[0]) : (selectionCount == 0 ? "" : L("status.selected_count", selectionCount.formatted()))
        if offlineListID == nil, QLPreviewPanel.sharedPreviewPanelExists(), QLPreviewPanel.shared()?.isVisible == true { updatePreview() }
    }
    var selectedPaths: [String] {
        let selected = table.selectedRowIndexes
        return cachedRows.keys.filter { selected.contains($0) }.sorted().compactMap { cachedRows[$0]?["path"] as? String }
    }
    var selectionIsComplete: Bool {
        let selected = table.selectedRowIndexes
        guard selected.count <= cachedRows.count else { return false }
        return selected.allSatisfy { cachedRows[$0]?["path"] is String }
    }
    var canUseSelection: Bool { resultsAreCurrent && !table.selectedRowIndexes.isEmpty && selectionIsComplete }
    var canResolveSelection: Bool {
        resultsAreCurrent && !table.selectedRowIndexes.isEmpty && backgroundRequestID == nil && !operationReviewPending
            && (selectionIsComplete || snapshotLease != nil)
    }
    func withResolvedRows(_ completion: @escaping ([[String: Any]]) -> Void) {
        guard canResolveSelection else { return }
        selectionResolver?.cancel(); selectionResolution += 1
        let resolution = selectionResolution, sequence = querySequence
        let selected = table.selectedRowIndexes
        var request: [String: Any] = ["text": queryText,
            "sort": table.sortDescriptors.map { ["field": $0.key ?? "name", "ascending": $0.ascending] as [String: Any] }]
        request["snapshot_lease"] = snapshotLease; request["generation"] = generation
        if let offlineListID { request["list_id"] = offlineListID }
        let resolver = SelectionResolver(selected: selected, cached: cachedRows, total: total, request: request, send: { [weak self] request, reply in
            guard let self, sequence == self.querySequence, resolution == self.selectionResolution else { return }
            var request = request; let id = UUID().uuidString; request["request_id"] = id
            self.requestIDs.insert(id)
            SearchClient.shared.call(request) { [weak self] value in
                self?.requestIDs.remove(id); reply(value)
            }
        }, completion: { [weak self] result in
            guard let self, sequence == self.querySequence, resolution == self.selectionResolution else { return }
            self.selectionResolver = nil
            switch result {
            case .success(let rows): completion(rows)
            case .failure(let error): self.checkResult(localizedErrorResponse(error))
            }
        })
        selectionResolver = resolver; resolver.start()
    }
    func withResolvedSelection(_ completion: @escaping ([String]) -> Void) {
        withResolvedRows { completion($0.compactMap { $0["path"] as? String }) }
    }
    func boolValue(_ value: Any?) -> Bool { (value as? Bool) ?? ((value as? NSNumber)?.intValue != 0 && value is NSNumber) }
    func stringList(_ value: Any?) -> [String] {
        if let items = value as? [String] { return items }
        if let items = value as? [[String: Any]] { return items.map { ($0["path"] as? String ?? "") + ": " + ($0["reason"] as? String ?? $0["message"] as? String ?? L("index.not_covered")) } }
        return []
    }
    func errorText(_ reply: [String: Any]) -> String {
        if let text = reply["error"] as? String { return text }
        if let object = reply["error"] as? [String: Any] { return object["message"] as? String ?? String(describing: object) }
        return reply["message"] as? String ?? L("error.operation_check_status")
    }
    func stopStatusObservation() {
        statusObservationActive = false; statusEpoch += 1
        statusRetry?.cancel(); statusRetry = nil
        if statusRequestPending { SearchClient.shared.call(["op": "cancel", "request_id": statusRequestID]) { _ in } }
        statusRequestPending = false
    }
    func pollStatus() {
        guard statusObservationActive, offlineListID == nil, !statusRequestPending else { return }
        statusRetry?.cancel(); statusRetry = nil
        statusRequestPending = true
        let epoch = statusEpoch
        let revision = (currentStatus["status_revision"] as? NSNumber)?.uint64Value
        var request: [String: Any] = revision.map {
            ["op": "wait_status", "after": $0, "timeout_ms": 30_000, "request_id": statusRequestID]
        } ?? ["op": "status"]
        if revision != nil, let snapshotLease { request["snapshot_lease"] = snapshotLease }
        SearchClient.shared.call(request) { [weak self] reply in
            guard let self, self.statusObservationActive, self.statusEpoch == epoch else { return }
            self.statusRequestPending = false
            guard reply["success"] as? Bool != false else {
                if (reply["error"] as? String ?? "").localizedCaseInsensitiveContains("lease") {
                    self.currentLease = nil; self.resultsAreCurrent = false; self.runQuery()
                }
                self.currentStatus = reply; self.updateStatus()
                let work = DispatchWorkItem { [weak self] in self?.pollStatus() }
                self.statusRetry = work
                DispatchQueue.main.asyncAfter(deadline: .now() + self.statusRetryDelay, execute: work)
                self.statusRetryDelay = min(60, self.statusRetryDelay * 2)
                return
            }
            self.statusRetryDelay = 1
            self.currentStatus = reply
            self.roots = self.stringList(reply["roots"])
            self.updateStatus(); self.refreshVisibleResults()
            if self.initialStatus {
                self.initialStatus = false
                if self.roots.isEmpty && (reply["count"] as? NSNumber)?.intValue ?? 0 == 0 && !UserDefaults.standard.bool(forKey: ApplicationIdentity.preferencePrefix + "ScopeChosen") {
                    DispatchQueue.main.async { [weak self] in self?.showInitialSetup() }
                }
            }
            // Old transports without revision support do not create a hot loop.
            if reply["status_revision"] is NSNumber {
                DispatchQueue.main.async { [weak self] in self?.pollStatus() }
            }
        }
    }
    func requestProgress(_ active: Bool, immediate: Bool = false) {
        guard active else {
            progressRequested = false; progressDelay?.invalidate(); progressDelay = nil
            setProgressActive(false); return
        }
        progressRequested = true
        if immediate { progressDelay?.invalidate(); progressDelay = nil; setProgressActive(true); return }
        guard !progressIsAnimating, progressDelay == nil else { return }
        // Normal hot queries finish before this timer. Do not flash a spinner
        // for every keystroke; slow searches still expose cancellable progress.
        progressDelay = Timer.scheduledTimer(withTimeInterval: 0.15, repeats: false) { [weak self] _ in
            guard let self else { return }; self.progressDelay = nil
            if self.progressRequested { self.setProgressActive(true) }
        }
    }
    func setProgressActive(_ active: Bool) {
        guard active != progressIsAnimating else { return }
        progressIsAnimating = active
        if active { progress.startAnimation(nil) } else { progress.stopAnimation(nil) }
    }
    func setStatusText(_ text: String) {
        if statusLabel.stringValue != text { statusLabel.stringValue = text }
    }
    func updateStatus() {
        updateEmptyState()
        if offlineListID != nil {
            requestProgress(queryPending)
            var offlineStatus = L("status.offline_summary", String(describing: total.formatted()), String(describing: offlineCount.formatted()), localizedDecimal(elapsed)) + (queryWarnings.first.map { " · " + $0 } ?? "")
            if queryPending { offlineStatus += " · " + L("status.searching") }
            setStatusText(offlineStatus)
            statusLabel.toolTip = L("offline.metadata_notice") + (queryWarnings.isEmpty ? "" : "\n" + queryWarnings.joined(separator: "\n"))
            if window?.subtitle != "" { window?.subtitle = "" }
            return
        }
        if currentStatus["success"] as? Bool == false { requestProgress(queryPending); setStatusText(L("index.service_label") + errorText(currentStatus)); return }
        let count = (currentStatus["count"] as? NSNumber)?.intValue ?? 0
        let scanning = boolValue(currentStatus["scanning"])
        requestProgress(scanning || queryPending || backgroundTaskName != nil, immediate: scanning || backgroundTaskName != nil)
        let uncovered = stringList(currentStatus["uncovered"])
        if coverageButton.isHidden != uncovered.isEmpty { coverageButton.isHidden = uncovered.isEmpty }
        let coverageTitle = L("status.uncovered_count", (uncovered.count).formatted())
        if coverageButton.title != coverageTitle { coverageButton.title = coverageTitle }
        var text = L("status.search_summary", String(describing: total.formatted()), String(describing: count.formatted()), localizedDecimal(elapsed))
        if queryPending { text += " · " + L("status.searching") }
        if scanning { text += L("status.scanning_suffix") }
        if currentStatus["state"] as? String == "error", let error = stringList(currentStatus["errors"]).first {
            text += " · " + L("index.service_label") + error
        }
        if let task = backgroundTaskName { text += " · " + task }

        if let first = queryWarnings.first { text += " · " + first }
        setStatusText(text)
        statusLabel.toolTip = (queryWarnings + uncovered).joined(separator: "\n")
        if window?.subtitle != "" { window?.subtitle = "" }
    }
    @objc func refreshShortcuts() {
        SearchClient.shared.call(["op": "preferences", "action": "get"]) { [weak self] reply in
            guard let self else { return }
            let values = reply["values"] as? [String: Any] ?? reply
            self.shortcuts.removeAllItems(); self.shortcuts.addItem(withTitle: L("search.bookmarks_and_history")); self.bookmarkQueries = []
            for value in values["bookmarks"] as? [[String: Any]] ?? [] {
                let query = value["query"] as? String ?? value["text"] as? String ?? ""
                self.shortcuts.addItem(withTitle: "★ " + (value["name"] as? String ?? value["title"] as? String ?? query)); self.bookmarkQueries.append(query)
            }
            let history = (values["history"] as? [String]) ?? (values["history"] as? [[String: Any]])?.compactMap { $0["query"] as? String ?? $0["text"] as? String } ?? []
            for query in history.prefix(20) { self.shortcuts.addItem(withTitle: query); self.bookmarkQueries.append(query) }
        }
    }
    @objc func selectShortcut(_ sender: Any?) {
        let index = shortcuts.indexOfSelectedItem - 1
        guard bookmarkQueries.indices.contains(index) else { return }
        let query = bookmarkQueries[index]
        search.stringValue = query; shortcuts.selectItem(at: 0); sidebarController.selectAll()
    }
    @objc func addBookmark(_ sender: Any?) {
        guard !queryText.isEmpty else { return }
        let alert = NSAlert(); alert.messageText = L("action.save_search_bookmark"); alert.informativeText = queryText
        let name = NSTextField(string: search.stringValue); name.frame = NSRect(x: 0, y: 0, width: 360, height: 24); alert.accessoryView = name
        alert.addButton(withTitle: L("action.save")); alert.addButton(withTitle: L("action.cancel"))
        guard alert.runModal() == .alertFirstButtonReturn else { return }
        let query = queryText
        SearchClient.shared.call(["op": "preferences", "action": "get"]) { [weak self] reply in
            var values = (reply["values"] as? [String: Any] ?? reply).filter { ["macros", "bookmarks", "exclusions", "history", "settings"].contains($0.key) }
            var bookmarks = values["bookmarks"] as? [[String: Any]] ?? []
            bookmarks.append(["name": name.stringValue, "query": query]); values["bookmarks"] = bookmarks
            SearchClient.shared.call(["op": "preferences", "action": "set", "values": values]) { result in self?.checkResult(result); self?.refreshShortcuts() }
        }
    }
    @objc func showCoverage(_ sender: Any?) {
        if offlineListID != nil {
            showText(L("offline.file_list"), text: L("offline.source_heading") + (offlineSourcePath ?? offlineTitle ?? "") + L("offline.details", String(describing: offlineCount.formatted())))
            return
        }
        let uncovered = stringList(currentStatus["uncovered"])
        let alert = NSAlert(); alert.messageText = L("index.coverage_title")
        alert.informativeText = L("index.current_scope_heading") + (roots.isEmpty ? L("index.no_selection") : roots.joined(separator: "\n")) + "\n\n" + (uncovered.isEmpty ? L("index.no_uncovered_folders_notice") : L("index.not_covered_heading") + uncovered.prefix(25).joined(separator: "\n")) + L("help.full_disk_access_scope")
        alert.addButton(withTitle: L("action.close")); alert.addButton(withTitle: L("action.choose_folder")); alert.addButton(withTitle: L("index.full_disk_access"))
        let result = alert.runModal()
        if result == .alertSecondButtonReturn { chooseFolders(nil) }
        else if result == .alertThirdButtonReturn { NSWorkspace.shared.open(URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles")!) }
    }
    @objc func chooseFolders(_ sender: Any?) {
        guard offlineListID == nil else { return }
        let panel = NSOpenPanel(); panel.title = L("index.choose_folders_to_index"); panel.prompt = L("index.start_indexing")
        panel.canChooseFiles = false; panel.canChooseDirectories = true; panel.allowsMultipleSelection = true
        panel.directoryURL = FileManager.default.homeDirectoryForCurrentUser
        guard panel.runModal() == .OK else { return }
        startScan(panel.urls.map(\.path))
    }
    private var setupController: InitialSetupController?
    @objc func showPermissionGuide(_ sender: Any?) { showInitialSetup(force: true) }
    func showInitialSetup(force: Bool = false) {
        let key = ApplicationIdentity.preferencePrefix + "SetupPresented"
        guard offlineListID == nil, let window, window.attachedSheet == nil,
              (force || !UserDefaults.standard.bool(forKey: key)) else { return }
        if let setupController { setupController.window?.makeKeyAndOrderFront(nil); return }
        UserDefaults.standard.set(true, forKey: key)
        let controller = InitialSetupController()
        setupController = controller
        controller.completion = { [weak self] response in
            self?.setupController = nil
            if response == .OK {
                DispatchQueue.main.async { [weak self] in self?.chooseVolumes(nil) }
            }
        }
        controller.window?.center()
        controller.showWindow(nil)
        controller.window?.makeKeyAndOrderFront(nil)
    }

    @objc func chooseVolumes(_ sender: Any?) {
        guard offlineListID == nil else { return }
        SearchClient.shared.call(["op": "volumes"]) { [weak self] reply in
            guard let self else { return }
            let list = reply["volumes"] as? [[String: Any]] ?? []
            var paths = list.filter { value in
                let fs = (value["filesystem"] as? String ?? value["fs_type"] as? String ?? value["format"] as? String ?? "apfs").lowercased()
                return fs.contains("apfs") && !(value["hidden"] as? Bool ?? false)
            }.compactMap { $0["path"] as? String ?? $0["mount_point"] as? String ?? $0["mount"] as? String }
            if paths.isEmpty { paths = self.stringList(reply["roots"]) }
            if paths.isEmpty { paths = ["/"] }
            paths = Array(Set(paths)).sorted()
            let alert = NSAlert(); alert.messageText = L("index.choose_local_apfs_index_scope")
            alert.informativeText = L("index.initial_scan_hint")
            let stack = NSStackView(); stack.orientation = .vertical; stack.alignment = .leading; stack.spacing = 8
            var boxes: [NSButton] = []
            for path in paths.prefix(12) {
                let box = NSButton(checkboxWithTitle: path == "/" ? L("index.system_volume") : path, target: nil, action: nil)
                box.state = .on; box.toolTip = path; stack.addArrangedSubview(box); boxes.append(box)
            }
            stack.frame = NSRect(x: 0, y: 0, width: 420, height: max(40, boxes.count * 27)); alert.accessoryView = stack
            alert.addButton(withTitle: L("index.start_indexing")); alert.addButton(withTitle: L("action.choose_folder")); alert.addButton(withTitle: L("action.later"))
            let response = alert.runModal()
            if response == .alertFirstButtonReturn {
                let selected = zip(paths, boxes).filter { $0.1.state == .on }.map { $0.0 }
                if !selected.isEmpty { self.startScan(selected) }
            } else if response == .alertSecondButtonReturn { self.chooseFolders(nil) }
        }
    }
    func startScan(_ paths: [String]) {
        UserDefaults.standard.set(true, forKey: ApplicationIdentity.preferencePrefix + "ScopeChosen")
        SearchClient.shared.call(["op": "scan", "roots": paths]) { [weak self] reply in self?.checkResult(reply); self?.pollStatus(); self?.runQuery() }
    }
    @objc func rescan(_ sender: Any?) { if roots.isEmpty { chooseVolumes(nil) } else { startScan(roots) } }

    @objc func openSelection(_ sender: Any?) { if resultsAreCurrent && offlineListID == nil { performSimple("open") } }
    @objc func revealSelection(_ sender: Any?) { performSimple("reveal") }
    @objc func copyPaths(_ sender: Any?) {
        withResolvedSelection { paths in
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(paths.joined(separator: "\n"), forType: .string)
        }
    }
    func performSimple(_ action: String) {
        guard offlineListID == nil else { return }
        withResolvedSelection { [weak self] paths in
            SearchClient.shared.call(["op": "files", "action": action, "paths": paths]) { [weak self] reply in self?.checkResult(reply) }
        }
    }
    @objc func renameSelection(_ sender: Any?) {
        withResolvedRows { rows in
            let paths = rows.compactMap { $0["path"] as? String }
            guard !paths.isEmpty else { return }
            let alert = NSAlert(); alert.messageText = L("files.rename_title")
            if paths.count == 1 {
                let field = NSTextField(string: URL(fileURLWithPath: paths[0]).lastPathComponent)
                field.frame = NSRect(x: 0, y: 0, width: 380, height: 25); alert.accessoryView = field
                alert.addButton(withTitle: L("action.rename")); alert.addButton(withTitle: L("action.cancel"))
                guard alert.runModal() == .alertFirstButtonReturn, !field.stringValue.isEmpty, !field.stringValue.contains("/") else { return }
                self.confirmFileOperation(["op": "files", "action": "rename", "paths": paths, "expected": rows, "new_name": field.stringValue], title: L("files.rename_confirmation", String(describing: field.stringValue)))
                return
            }
            alert.informativeText = L("files.rename_count", localizedCount(paths.count))
            let search = NSTextField(); search.placeholderString = L("files.rename_find")
            let replace = NSTextField(); replace.placeholderString = L("files.rename_replace")
            let prefix = NSTextField(); prefix.placeholderString = L("files.rename_prefix")
            let suffix = NSTextField(); suffix.placeholderString = L("files.rename_suffix")
            let numbering = NSButton(checkboxWithTitle: L("files.rename_numbering"), target: nil, action: nil)
            let stack = NSStackView(views: [search, replace, prefix, suffix, numbering])
            stack.orientation = .vertical; stack.spacing = 7; stack.frame = NSRect(x: 0, y: 0, width: 400, height: 130)
            alert.accessoryView = stack
            alert.addButton(withTitle: L("action.rename")); alert.addButton(withTitle: L("action.cancel"))
            guard alert.runModal() == .alertFirstButtonReturn else { return }
            var rule: [String: Any] = ["search": search.stringValue, "replace": replace.stringValue,
                "prefix": prefix.stringValue, "suffix": suffix.stringValue]
            if numbering.state == .on { rule["number_start"] = 1; rule["number_padding"] = String(paths.count).count }
            self.confirmFileOperation(["op": "files", "action": "rename", "paths": paths, "expected": rows,
                "rename_rule": rule, "conflict_policy": "skip"], title: L("files.rename_title"))
        }
    }
    @objc func copySelection(_ sender: Any?) { chooseDestination("copy") }
    @objc func moveSelection(_ sender: Any?) { chooseDestination("move") }
    func chooseDestination(_ action: String) {
        guard resultsAreCurrent, !table.selectedRowIndexes.isEmpty else { return }
        withResolvedRows { rows in
            let paths = rows.compactMap { $0["path"] as? String }
            let panel = NSOpenPanel(); panel.canChooseFiles = false; panel.canChooseDirectories = true; panel.canCreateDirectories = true
            panel.title = action == "copy" ? L("files.copy_folder_title") : L("files.move_folder_title"); panel.prompt = L("action.choose")
            guard panel.runModal() == .OK, let destination = panel.url?.path else { return }
            self.confirmFileOperation(["op": "files", "action": action, "paths": paths, "expected": rows, "destination": destination], title: L("files.destination_confirmation", String(describing: action == "copy" ? L("action.copy") : L("action.move")), (paths.count).formatted(), String(describing: destination)))
        }
    }
    @objc func trashSelection(_ sender: Any?) {
        withResolvedRows { rows in
            let paths = rows.compactMap { $0["path"] as? String }
            self.confirmFileOperation(["op": "files", "action": "trash", "paths": paths, "expected": rows], title: L("files.trash_confirmation", paths.count.formatted()))
        }
    }
    func confirmFileOperation(_ request: [String: Any], title: String) {
        guard offlineListID == nil, backgroundRequestID == nil, !operationReviewPending else { return }
        operationReviewPending = true
        let sequence = querySequence
        var preview = request; preview["dry_run"] = true
        SearchClient.shared.call(preview) { [weak self] reply in
            guard let self else { return }
            self.operationReviewPending = false
            guard self.backgroundRequestID == nil, sequence == self.querySequence,
              self.window?.isVisible == true else { return }
            guard reply["success"] as? Bool == true else { self.checkResult(reply); return }
            let rows = reply["preview"] as? [[String: Any]] ?? []
            let action = request["action"] as? String ?? ""
            let review = FileOperationReview(title: title, rows: rows, editable: action != "trash")
            var approved = request
            switch review.run() {
            case .cancel: return
            case .recheck(let names, let skips):
                approved["target_names"] = (request["target_names"] as? [String: String] ?? [:]).merging(names) { _, next in next }
                approved["skip_paths"] = skips
                self.confirmFileOperation(approved, title: title); return
            case .execute(let skips): approved["skip_paths"] = skips
            }
            if approved["expected"] == nil { approved["expected"] = reply["expected"] }
            approved["expected_parents"] = reply["expected_parents"]
            approved["conflict_policy"] = "stop"
            // Freeze generated target names; skipping a row must not renumber the rest.
            if action != "trash" {
                approved["target_names"] = Dictionary<String, String>(uniqueKeysWithValues: rows.compactMap { row in
                    guard let source = row["source"] as? String, let target = row["destination"] as? String else { return nil }
                    return (source, URL(fileURLWithPath: target).lastPathComponent)
                })
            }
            let id = self.beginBackground(L("files.performing_batch")); approved["request_id"] = id
            self.fileOperationRequestID = id
            SearchClient.shared.call(approved) { [weak self] result in
                guard let self else { return }
                self.fileOperationRequestID = nil
                if self.backgroundRequestID == id { self.endBackground() }
                guard self.window?.isVisible == true else { return }
                let results = result["results"] as? [[String: Any]] ?? []
                if !results.isEmpty {
                    _ = FileOperationReview(title: result["message"] as? String ?? title, rows: results, editable: false, readOnly: true).run()
                } else { self.checkResult(result) }
                self.runQuery()
            }
        }
    }
    @objc func undoFileOperation(_ sender: Any?) {
        guard offlineListID == nil else { return }
        SearchClient.shared.call(["op": "files", "action": "undo"]) { [weak self] reply in self?.checkResult(reply); self?.runQuery() }
    }
    @objc func indexContent(_ sender: Any?) {
        guard offlineListID == nil else { return }
        withResolvedSelection { [weak self] paths in
            guard let self else { return }
            let id = self.beginBackground(L("status.extracting_content"))
            SearchClient.shared.call(["op": "content_index", "paths": paths, "request_id": id]) { [weak self] reply in
                guard let self, self.backgroundRequestID == id else { return }
                self.endBackground(); self.checkResult(reply, success: L("index.content_indexing_processed")); self.runQuery()
            }
        }
    }
    @objc func showDirectorySizes(_ sender: Any?) {
        guard offlineListID == nil else { return }
        let lease = currentLease
        withResolvedRows { [weak self] rows in
            guard let self else { return }
            let paths = rows.filter { $0["is_dir"] as? Bool == true }.compactMap { $0["path"] as? String }
            guard !paths.isEmpty else { return }
            let id = self.beginBackground(L("directory.show_sizes"))
            var values = [[String: Any]]()
            func next(_ offset: Int) {
                guard self.backgroundRequestID == id else { return }
                guard offset < paths.count else {
                    self.endBackground()
                    let text = values.map { row -> String in
                        let size = (row["recursive_size"] as? NSNumber).map {
                            $0.uint64Value <= UInt64(Int64.max)
                              ? ByteCountFormatter.string(fromByteCount: $0.int64Value, countStyle: .file)
                              : $0.stringValue
                        } ?? L("directory.unknown")
                        return (row["path"] as? String ?? "") + "\n" + size
                    }.joined(separator: "\n\n")
                    self.showText(L("directory.show_sizes"), text: L("directory.logical_notice") + "\n\n" + text)
                    return
                }
                let end = min(paths.count, offset + 200)
                var request: [String: Any] = ["op": "directory_info", "paths": Array(paths[offset..<end]), "request_id": id]
                request["snapshot_lease"] = lease?.token
                SearchClient.shared.call(request) { reply in
                    withExtendedLifetime(lease) {}
                    guard self.backgroundRequestID == id else { return }
                    guard reply["success"] as? Bool == true else { self.endBackground(); self.checkResult(reply); return }
                    values += reply["rows"] as? [[String: Any]] ?? []
                    DispatchQueue.main.async { next(end) }
                }
            }
            next(0)
        }
    }
    func beginBackground(_ name: String) -> String {
        cancelBackground(nil)
        let id = UUID().uuidString; backgroundRequestID = id; backgroundTaskName = name; cancelButton.isHidden = false; updateStatus(); return id
    }
    func endBackground() { backgroundRequestID = nil; backgroundTaskName = nil; cancelButton.isHidden = true; updateStatus() }
    @objc func cancelBackground(_ sender: Any?) {
        guard let id = backgroundRequestID else { return }
        SearchClient.shared.call(["op": "cancel", "request_id": id]) { _ in }
        if fileOperationRequestID == id {
            // FileManager finishes the current item. Keep the batch busy until
            // its partial result arrives instead of cancelling a newer job.
            backgroundTaskName = L("files.cancelling_batch"); updateStatus()
        } else { endBackground() }
    }
    @objc func showOperationHistory(_ sender: Any?) { showOperationHistoryPage(0) }
    func showOperationHistoryPage(_ offset: Int) {
        SearchClient.shared.call(["op": "files", "action": "history", "offset": offset, "limit": 100]) { [weak self] reply in
            guard let self, self.window?.isVisible == true else { return }
            guard reply["success"] as? Bool == true else { self.checkResult(reply); return }
            let operations = reply["operations"] as? [[String: Any]] ?? []
            let total = (reply["total"] as? NSNumber)?.intValue ?? operations.count
            let lines = operations.map { operation -> String in
                let action = operation["action"] as? String ?? ""
                let kind = operation["kind"] as? String ?? "operation"
                let pending = kind == "intent" || kind == "failed" || operation["undo_pending"] as? Bool == true
                let state = pending ? L("files.needs_review") : (operation["undone"] as? Bool == true ? L("files.undone_suffix") : FileOperationReview.statusText(kind == "operation" ? "completed" : kind))
                return "\(action) · \(state)\n\(operation["source"] as? String ?? "")\n→ \(operation["destination"] as? String ?? "")"
            }
            let alert = NSAlert(); alert.messageText = L("files.file_operation_history")
            alert.informativeText = L("files.history_page_notice", localizedCount(operations.count), localizedCount(total))
            let scroll = NSScrollView(frame: NSRect(x: 0, y: 0, width: 650, height: 360)); scroll.hasVerticalScroller = true
            let text = NSTextView(frame: scroll.bounds); text.isEditable = false; text.isSelectable = true
            text.font = .systemFont(ofSize: 12); text.string = lines.isEmpty ? L("files.no_operation_history") : lines.joined(separator: "\n\n")
            text.textContainer?.widthTracksTextView = true; text.autoresizingMask = [.width]
            scroll.documentView = text; alert.accessoryView = scroll
            alert.addButton(withTitle: L("action.close"))
            var destinations = [Int]()
            if offset > 0 { alert.addButton(withTitle: L("files.previous_page")); destinations.append(max(0, offset - 100)) }
            if offset + operations.count < total { alert.addButton(withTitle: L("files.next_page")); destinations.append(offset + operations.count) }
            let decision = alert.runModal().rawValue - NSApplication.ModalResponse.alertSecondButtonReturn.rawValue
            if destinations.indices.contains(decision) {
                DispatchQueue.main.async { [weak self] in self?.showOperationHistoryPage(destinations[decision]) }
            }
        }
    }
    @objc func findDuplicates(_ sender: Any?) {
        guard offlineListID == nil else { return }
        let id = beginBackground(L("status.checking_duplicates"))
        SearchClient.shared.call(["op": "duplicates", "mode": "content", "request_id": id]) { [weak self] reply in
            guard let self, self.backgroundRequestID == id else { return }; self.endBackground()
            if reply["success"] as? Bool == false { self.checkResult(reply); return }
            let controller = DuplicateResultsWindowController(result: reply) { [weak self] request in
                self?.confirmFileOperation(request, title: L("duplicates.manage_selection"))
            }
            self.duplicateWindows.append(controller)
            controller.onClose = { [weak self, weak controller] in self?.duplicateWindows.removeAll { $0 === controller } }
            controller.showWindow(nil); controller.window?.makeKeyAndOrderFront(nil)
        }
    }
    func duplicateReport(_ reply: [String: Any]) -> String {
        func formattedGroups(_ value: Any?) -> String {
            let groups = value as? [[String: Any]] ?? []
            return groups.enumerated().map { index, group in
                let paths = group["paths"] as? [String] ?? (group["rows"] as? [[String: Any]] ?? []).compactMap { $0["path"] as? String }
                return L("duplicates.group_heading", localizedCount(index + 1)) + paths.joined(separator: "\n")
            }.joined(separator: "\n\n")
        }
        let duplicates = formattedGroups(reply["groups"])
        let hardlinks = formattedGroups(reply["hardlinks"])
        let errors = stringList(reply["errors"]) + (reply["errors"] as? [[String: Any]] ?? []).map { error in
            let path = error["path"] as? String ?? ""
            let message = error["message"] as? String ?? ""
            return path.isEmpty ? message : path + ": " + message
        }
        let partial = reply["partial"] as? Bool == true || !errors.isEmpty
        var sections = [String]()
        if partial { sections.append(L("duplicates.partial_notice")) }
        if !duplicates.isEmpty { sections.append(L("duplicates.independent_files") + "\n\n" + duplicates) }
        else if !partial { sections.append(L("duplicates.none_found")) }
        if !hardlinks.isEmpty { sections.append(L("duplicates.hardlinks") + "\n\n" + hardlinks) }
        if !errors.isEmpty { sections.append(L("duplicates.unfinished_files") + "\n\n" + errors.joined(separator: "\n")) }
        sections.append(L("duplicates.scope_notice"))
        if !duplicates.isEmpty || !hardlinks.isEmpty { sections.append(L("duplicates.storage_notice")) }
        return sections.joined(separator: "\n\n")
    }
    @objc func exportList(_ sender: Any?) {
        let panel = NSSavePanel(); panel.nameFieldStringValue = L("export.default_filename"); panel.title = L("export.dialog_title")
        guard panel.runModal() == .OK, let path = panel.url?.path else { return }
        let id = beginBackground(L("status.exporting"))
        var request: [String: Any] = ["op": "export_list", "path": path, "text": queryText, "request_id": id, "sort": table.sortDescriptors.map { ["field": $0.key ?? "name", "ascending": $0.ascending] as [String: Any] }]
        if let offlineListID { request["list_id"] = offlineListID }
        SearchClient.shared.call(request) { [weak self] reply in
            guard let self, self.backgroundRequestID == id else { return }
            self.endBackground(); self.checkResult(reply, success: L("export.completed"))
        }
    }
    @objc func importList(_ sender: Any?) {
        let panel = NSOpenPanel(); panel.canChooseDirectories = false; panel.title = L("import.dialog_title")
        guard panel.runModal() == .OK, let path = panel.url?.path else { return }
        SearchClient.shared.call(["op": "import_list", "path": path]) { [weak self] reply in
            guard let self else { return }
            if reply["success"] as? Bool == false { self.checkResult(reply); return }
            guard let listID = reply["list_id"] as? String, !listID.isEmpty else { self.checkResult(["success": false, "error": L("error.import_missing_list_id")]); return }
            let count = (reply["total"] as? NSNumber)?.intValue ?? (reply["count"] as? NSNumber)?.intValue ?? 0
            let controller = SearchWindowController(offlineListID: listID, sourcePath: path, count: count)
            (NSApp.delegate as? AppDelegate)?.windows.append(controller)
            controller.showWindow(nil)
            if let previous = self.window, let next = controller.window { previous.addTabbedWindow(next, ordered: .above); next.makeKeyAndOrderFront(nil) }
        }
    }
    func checkResult(_ reply: [String: Any], success: String? = nil) {
        let warnings = stringList(reply["warnings"])
        if reply["success"] as? Bool == false {
            let alert = NSAlert(); alert.alertStyle = .warning; alert.messageText = L("error.operation_incomplete"); alert.informativeText = errorText(reply); alert.runModal()
        } else if let success {
            let alert = NSAlert(); alert.messageText = success; alert.informativeText = (reply["message"] as? String ?? "") + (warnings.isEmpty ? "" : "\n" + warnings.joined(separator: "\n")); alert.runModal()
        } else if !warnings.isEmpty { showText(L("files.operation_notice"), text: warnings.joined(separator: "\n")) }
    }
    func showText(_ title: String, text: String) {
        let alert = NSAlert(); alert.messageText = title
        let scroll = NSScrollView(frame: NSRect(x: 0, y: 0, width: 650, height: 360)); scroll.hasVerticalScroller = true
        let view = NSTextView(frame: scroll.bounds); view.string = text; view.isEditable = false; view.isSelectable = true
        view.font = .systemFont(ofSize: 12); view.textContainerInset = NSSize(width: 8, height: 8); view.autoresizingMask = [.width]; view.isVerticallyResizable = true; view.textContainer?.widthTracksTextView = true
        scroll.documentView = view; alert.accessoryView = scroll; alert.runModal()
    }
    @objc func previewSelection(_ sender: Any?) {
        guard canUseSelection, offlineListID == nil else { return }
        if let panel = QLPreviewPanel.shared() { if panel.isVisible { panel.orderOut(nil) } else { updatePreview(); panel.makeKeyAndOrderFront(nil) } }
    }
    func updatePreview() { guard offlineListID == nil, selectionIsComplete else { QLPreviewPanel.shared()?.orderOut(nil); return }; previewURLs = selectedPaths.map { NSURL(fileURLWithPath: $0) }; QLPreviewPanel.shared()?.dataSource = self; QLPreviewPanel.shared()?.delegate = self; QLPreviewPanel.shared()?.reloadData() }
    override func acceptsPreviewPanelControl(_ panel: QLPreviewPanel!) -> Bool { offlineListID == nil }
    override func beginPreviewPanelControl(_ panel: QLPreviewPanel!) { panel.dataSource = self; panel.delegate = self; updatePreview() }
    override func endPreviewPanelControl(_ panel: QLPreviewPanel!) { panel.dataSource = nil; panel.delegate = nil }
    func numberOfPreviewItems(in panel: QLPreviewPanel!) -> Int { previewURLs.count }
    func previewPanel(_ panel: QLPreviewPanel!, previewItemAt index: Int) -> QLPreviewItem! { previewURLs.indices.contains(index) ? previewURLs[index] : nil }
    func tableView(_ tableView: NSTableView, pasteboardWriterForRow row: Int) -> NSPasteboardWriting? { guard resultsAreCurrent, selectionIsComplete, offlineListID == nil, let path = cachedRows[row]?["path"] as? String else { return nil }; return NSURL(fileURLWithPath: path) }
    func tableView(_ tableView: NSTableView, validateDrop info: NSDraggingInfo, proposedRow row: Int, proposedDropOperation operation: NSTableView.DropOperation) -> NSDragOperation {
        guard resultsAreCurrent, offlineListID == nil, row >= 0, let record = cachedRows[row], boolValue(record["is_dir"]) else { return [] }
        tableView.setDropRow(row, dropOperation: .on)
        return NSApp.currentEvent?.modifierFlags.contains(.option) == true ? .copy : .move
    }
    func tableView(_ tableView: NSTableView, acceptDrop info: NSDraggingInfo, row: Int, dropOperation: NSTableView.DropOperation) -> Bool {
        guard resultsAreCurrent, let destination = cachedRows[row]?["path"] as? String, let urls = info.draggingPasteboard.readObjects(forClasses: [NSURL.self], options: [.urlReadingFileURLsOnly: true]) as? [URL], !urls.isEmpty else { return false }
        let action = NSApp.currentEvent?.modifierFlags.contains(.option) == true ? "copy" : "move"
        confirmFileOperation(["op": "files", "action": action, "paths": urls.map(\.path), "destination": destination], title: L("files.destination_confirmation", String(describing: action == "copy" ? L("action.copy") : L("action.move")), (urls.count).formatted(), String(describing: destination)))
        return true
    }
    func windowWillClose(_ notification: Notification) {
        progressDelay?.invalidate(); pendingQuery?.cancel(); pendingQuery = nil; historyTimer?.invalidate()
        operationReviewPending = false
        stopStatusObservation(); cancelQueries(); cancelBackground(nil)
        let closing = duplicateWindows; duplicateWindows.removeAll()
        closing.forEach { $0.close() }
        if let delegate = NSApp.delegate as? AppDelegate { delegate.windows.removeAll { $0 === self } }
    }
    func validateMenuItem(_ menuItem: NSMenuItem) -> Bool {
        if menuItem.action == #selector(toggleColumn) { return menuItem.representedObject as? String != "name" }
        if menuItem.action == #selector(cancelBackground) { return backgroundRequestID != nil }
        if menuItem.action == #selector(undoFileOperation) || menuItem.action == #selector(findDuplicates) {
            return offlineListID == nil && backgroundRequestID == nil && !operationReviewPending
        }
        if menuItem.action == #selector(trashSelection), let editor = window?.firstResponder as? NSTextView, editor.isFieldEditor { return false }
        if offlineListID != nil {
            let liveActions: Set<Selector> = [#selector(openSelection), #selector(revealSelection), #selector(renameSelection), #selector(copySelection), #selector(moveSelection), #selector(trashSelection), #selector(indexContent), #selector(previewSelection), #selector(findDuplicates), #selector(rescan), #selector(chooseFolders), #selector(chooseVolumes), #selector(undoFileOperation)]
            if let action = menuItem.action, liveActions.contains(action) { return false }
        }
        let selectionActions: Set<Selector> = [#selector(openSelection), #selector(revealSelection), #selector(copyPaths), #selector(copySelection), #selector(moveSelection), #selector(trashSelection), #selector(indexContent), #selector(previewSelection)]
        if menuItem.action == #selector(previewSelection) { return canUseSelection }
        if let action = menuItem.action, selectionActions.contains(action) { return canResolveSelection }
        if menuItem.action == #selector(renameSelection) || menuItem.action == #selector(showDirectorySizes) { return offlineListID == nil && canResolveSelection }
        if menuItem.action == #selector(addBookmark) { return !queryText.isEmpty }
        return true
    }
}


final class SearchSidebarViewController: NSViewController, NSTableViewDataSource, NSTableViewDelegate {
    weak var owner: SearchWindowController?
    let table = NSTableView()
    let filters: [(title: String, symbol: String, query: String)] = [
        (L("search.all_files"), "internaldrive", ""),
        (L("filter.documents"), "doc.text", "ext:pdf;doc;docx;xls;xlsx;ppt;pptx;pages;numbers;key;txt;md;rtf"),
        (L("filter.images"), "photo", "ext:jpg;jpeg;png;heic;heif;gif;tif;tiff;webp;svg;psd"),
        (L("filter.audio"), "music.note", "ext:mp3;m4a;aac;flac;wav;aiff;ogg"),
        (L("filter.videos"), "film", "ext:mp4;mov;m4v;mkv;avi;webm"),
        (L("filter.archives"), "archivebox", "ext:zip;7z;rar;gz;tar;bz2;xz;dmg;iso"),
        (L("filter.folders"), "folder", "folder:"),
        (L("filter.recently_modified"), "clock", "dm:thisweek"),
        (L("filter.large_files"), "externaldrive.badge.plus", "file: size:>100mb")
    ]
    init(owner: SearchWindowController) { self.owner = owner; super.init(nibName: nil, bundle: nil) }
    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }
    override func loadView() {
        let backdrop = NSVisualEffectView(); backdrop.material = .sidebar; backdrop.blendingMode = .behindWindow
        view = backdrop
        let label = NSTextField(labelWithString: L("filter.section_title")); label.font = .systemFont(ofSize: 11, weight: .semibold); label.textColor = .secondaryLabelColor
        label.translatesAutoresizingMaskIntoConstraints = false; backdrop.addSubview(label)
        let scroll = NSScrollView(); scroll.drawsBackground = false; scroll.hasVerticalScroller = true; scroll.autohidesScrollers = true
        scroll.translatesAutoresizingMaskIntoConstraints = false; backdrop.addSubview(scroll)
        table.headerView = nil; table.style = .sourceList; table.backgroundColor = .clear; table.rowHeight = 31
        table.intercellSpacing = NSSize(width: 0, height: 1)
        table.dataSource = self; table.delegate = self; table.allowsEmptySelection = false
        let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("filter")); column.resizingMask = .autoresizingMask; table.addTableColumn(column)
        scroll.documentView = table
        if let shortcuts = owner?.shortcuts { shortcuts.translatesAutoresizingMaskIntoConstraints = false; backdrop.addSubview(shortcuts) }
        NSLayoutConstraint.activate([label.leadingAnchor.constraint(equalTo: backdrop.leadingAnchor, constant: 18), label.topAnchor.constraint(equalTo: backdrop.safeAreaLayoutGuide.topAnchor, constant: 12), scroll.topAnchor.constraint(equalTo: label.bottomAnchor, constant: 8), scroll.leadingAnchor.constraint(equalTo: backdrop.leadingAnchor, constant: 8), scroll.trailingAnchor.constraint(equalTo: backdrop.trailingAnchor, constant: -8), scroll.bottomAnchor.constraint(equalTo: backdrop.bottomAnchor, constant: -48)])
        if let shortcuts = owner?.shortcuts { NSLayoutConstraint.activate([shortcuts.leadingAnchor.constraint(equalTo: backdrop.leadingAnchor, constant: 12), shortcuts.trailingAnchor.constraint(equalTo: backdrop.trailingAnchor, constant: -12), shortcuts.bottomAnchor.constraint(equalTo: backdrop.bottomAnchor, constant: -14)]) }
        table.selectRowIndexes(IndexSet(integer: 0), byExtendingSelection: false)
    }
    func numberOfRows(in tableView: NSTableView) -> Int { filters.count }
    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        let cell = NSTableCellView()
        let image = NSImageView(image: NSImage(systemSymbolName: filters[row].symbol, accessibilityDescription: nil) ?? NSImage())
        image.contentTintColor = .secondaryLabelColor; image.translatesAutoresizingMaskIntoConstraints = false
        let text = NSTextField(labelWithString: filters[row].title); text.font = .systemFont(ofSize: 13); text.translatesAutoresizingMaskIntoConstraints = false
        cell.addSubview(image); cell.addSubview(text); cell.imageView = image; cell.textField = text
        NSLayoutConstraint.activate([image.leadingAnchor.constraint(equalTo: cell.leadingAnchor, constant: 8), image.widthAnchor.constraint(equalToConstant: 17), image.heightAnchor.constraint(equalToConstant: 17), image.centerYAnchor.constraint(equalTo: cell.centerYAnchor), text.leadingAnchor.constraint(equalTo: image.trailingAnchor, constant: 8), text.trailingAnchor.constraint(equalTo: cell.trailingAnchor, constant: -6), text.centerYAnchor.constraint(equalTo: cell.centerYAnchor)])
        return cell
    }
    func tableViewSelectionDidChange(_ notification: Notification) {
        guard filters.indices.contains(table.selectedRow), owner?.window?.isVisible == true else { return }
        let filter = filters[table.selectedRow]
        owner?.selectFilter(query: filter.query, title: filter.title)
    }
    func selectAll() {
        let alreadySelected = table.selectedRow == 0
        table.selectRowIndexes(IndexSet(integer: 0), byExtendingSelection: false)
        if alreadySelected, let first = filters.first { owner?.selectFilter(query: first.query, title: first.title) }
    }
}


/// Opt-in local metrics for development and acceptance runs. Writes happen after
/// the timed interval on a serial utility queue, never on the main/UI thread.
private enum SearchMetrics {
    private static let queue = DispatchQueue(label: ApplicationIdentity.bundleIdentifier + ".metrics", qos: .utility)
    static func record(_ event: [String: Any]) {
        guard let path = ProcessInfo.processInfo.environment[ApplicationIdentity.metricsEnvironment], !path.isEmpty else { return }
        queue.async {
            do {
                var line = try JSONSerialization.data(withJSONObject: event, options: [.sortedKeys])
                line.append(0x0a)
                let url = URL(fileURLWithPath: path)
                try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
                if !FileManager.default.fileExists(atPath: path) { FileManager.default.createFile(atPath: path, contents: nil) }
                let handle = try FileHandle(forWritingTo: url)
                defer { try? handle.close() }
                try handle.seekToEnd(); try handle.write(contentsOf: line)
            } catch { NSLog("APFSearch metrics write failed: %@", error.localizedDescription) }
        }
    }
}

/// Permission education only: macOS owns authorization, and no protected file
/// probes or private TCC APIs are used to infer a grant.
final class InitialSetupController: NSWindowController, NSPathControlDelegate {
    var completion: ((NSApplication.ModalResponse) -> Void)?
    private let demonstration = PermissionDragDemonstration()
    init() {
        let panel = NSPanel(contentRect: NSRect(x: 0, y: 0, width: 560, height: 530),
                            styleMask: [.titled, .fullSizeContentView], backing: .buffered, defer: false)
        panel.titleVisibility = .hidden
        panel.titlebarAppearsTransparent = true
        panel.isMovable = true
        panel.hidesOnDeactivate = false
        super.init(window: panel)
        let content = NSVisualEffectView()
        content.material = .windowBackground; content.blendingMode = .withinWindow
        panel.contentView = content
        let stack = NSStackView()
        stack.orientation = .vertical; stack.alignment = .leading; stack.spacing = 18
        stack.translatesAutoresizingMaskIntoConstraints = false
        content.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 32),
            stack.trailingAnchor.constraint(equalTo: content.trailingAnchor, constant: -32),
            stack.topAnchor.constraint(equalTo: content.topAnchor, constant: 32),
            stack.bottomAnchor.constraint(lessThanOrEqualTo: content.bottomAnchor, constant: -28),
        ])
        let icon = NSImageView()
        icon.image = NSImage(systemSymbolName: "magnifyingglass", accessibilityDescription: nil)
        icon.symbolConfiguration = .init(pointSize: 40, weight: .regular)
        icon.contentTintColor = .controlAccentColor
        icon.widthAnchor.constraint(equalToConstant: 48).isActive = true
        icon.heightAnchor.constraint(equalToConstant: 48).isActive = true
        icon.setAccessibilityElement(false)
        stack.addArrangedSubview(icon)
        stack.addArrangedSubview(label("setup.title", size: 24, weight: .semibold))
        stack.addArrangedSubview(label("setup.description", size: 13, secondary: true))

        let steps = NSStackView()
        steps.orientation = .vertical; steps.alignment = .leading; steps.spacing = 14
        steps.edgeInsets = NSEdgeInsets(top: 18, left: 18, bottom: 18, right: 18)
        steps.wantsLayer = true
        steps.layer?.cornerRadius = 12
        // A native visual-effect material follows the active system appearance.
        let material = NSVisualEffectView()
        material.material = .contentBackground; material.blendingMode = .withinWindow
        material.wantsLayer = true; material.layer?.cornerRadius = 12; material.layer?.masksToBounds = true
        material.translatesAutoresizingMaskIntoConstraints = false
        steps.addSubview(material, positioned: .below, relativeTo: nil)
        NSLayoutConstraint.activate([
            material.leadingAnchor.constraint(equalTo: steps.leadingAnchor),
            material.trailingAnchor.constraint(equalTo: steps.trailingAnchor),
            material.topAnchor.constraint(equalTo: steps.topAnchor),
            material.bottomAnchor.constraint(equalTo: steps.bottomAnchor),
        ])
        for (number, key) in ["setup.step_one", "setup.step_two", "setup.step_three"].enumerated() {
            let row = NSStackView(); row.orientation = .horizontal; row.alignment = .top; row.spacing = 12
            let badge = NSTextField(labelWithString: (number + 1).formatted())
            badge.font = .monospacedDigitSystemFont(ofSize: 13, weight: .semibold)
            badge.textColor = .secondaryLabelColor
            badge.widthAnchor.constraint(equalToConstant: 16).isActive = true
            row.addArrangedSubview(badge)
            row.addArrangedSubview(label(key, size: 13))
            steps.addArrangedSubview(row)
            row.widthAnchor.constraint(equalTo: steps.widthAnchor, constant: -36).isActive = true
        }
        let application = NSPathControl()
        application.pathStyle = .popUp
        application.isEditable = false
        application.delegate = self
        application.setDraggingSourceOperationMask(.copy, forLocal: false)
        application.setDraggingSourceOperationMask([], forLocal: true)
        application.url = Bundle.main.bundleURL
        application.controlSize = .large
        application.font = .systemFont(ofSize: 14, weight: .medium)
        application.heightAnchor.constraint(equalToConstant: 44).isActive = true
        application.widthAnchor.constraint(equalToConstant: 250).isActive = true
        application.toolTip = L("setup.step_two")
        application.setAccessibilityLabel("APFSearch.app")
        steps.addArrangedSubview(application)
        steps.addArrangedSubview(label("setup.drag_demonstration", size: 12, secondary: true))
        steps.addArrangedSubview(demonstration)
        demonstration.widthAnchor.constraint(equalTo: steps.widthAnchor, constant: -36).isActive = true
        demonstration.heightAnchor.constraint(equalToConstant: 70).isActive = true
        let settings = NSButton(title: L("settings.open_full_disk_access_settings"), target: self, action: #selector(openPermissions))
        settings.bezelStyle = .rounded
        steps.addArrangedSubview(settings)
        stack.addArrangedSubview(steps)
        steps.widthAnchor.constraint(equalTo: stack.widthAnchor).isActive = true
        stack.addArrangedSubview(label("setup.optional", size: 12, secondary: true))
        let buttons = NSStackView(); buttons.orientation = .horizontal; buttons.spacing = 12
        let later = NSButton(title: L("action.later"), target: self, action: #selector(skip))
        later.bezelStyle = .rounded; later.keyEquivalent = "\u{1b}"
        let spacer = NSView()
        let next = NSButton(title: L("setup.continue"), target: self, action: #selector(proceed))
        next.bezelStyle = .rounded; next.keyEquivalent = "\r"
        buttons.addArrangedSubview(later); buttons.addArrangedSubview(spacer); buttons.addArrangedSubview(next)
        stack.addArrangedSubview(buttons)
        buttons.widthAnchor.constraint(equalTo: stack.widthAnchor).isActive = true
        for view in stack.arrangedSubviews {
            if let text = view as? NSTextField {
                text.widthAnchor.constraint(equalTo: stack.widthAnchor).isActive = true
            }
        }
        content.layoutSubtreeIfNeeded()
        panel.setContentSize(NSSize(width: 560, height: stack.fittingSize.height + 60))
    }
    required init?(coder: NSCoder) { fatalError("init(coder:) is unavailable") }
    private func label(_ key: String, size: CGFloat, weight: NSFont.Weight = .regular, secondary: Bool = false) -> NSTextField {
        let field = NSTextField(wrappingLabelWithString: L(key))
        field.font = .systemFont(ofSize: size, weight: weight)
        field.textColor = secondary ? .secondaryLabelColor : .labelColor
        field.setContentCompressionResistancePriority(.required, for: .vertical)
        return field
    }
    func pathControl(_ pathControl: NSPathControl, shouldDrag pathItem: NSPathControlItem, with pasteboard: NSPasteboard) -> Bool {
        // NSPathControl supplies the native file URL and filename pasteboard
        // representations. Only the running application can be dragged here.
        pathItem.url?.standardizedFileURL.path == Bundle.main.bundleURL.standardizedFileURL.path
    }
    @objc private func openPermissions() {
        guard let window else { return }
        window.level = .floating
        if let screen = window.screen {
            let frame = screen.visibleFrame
            window.setFrameOrigin(NSPoint(x: frame.maxX - window.frame.width - 16,
                                          y: max(frame.minY, frame.midY - window.frame.height / 2)))
        }
        NSWorkspace.shared.open(URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles")!)
        demonstration.play()
    }
    @objc private func skip() { finish(.cancel) }
    @objc private func proceed() { finish(.OK) }
    private func finish(_ response: NSApplication.ModalResponse) {
        guard let window else { return }
        window.close()
        let finished = completion
        completion = nil
        finished?(response)
    }
}

/// A noninteractive illustration, never a representation of granted access.
/// Core Animation runs the bounded demonstration without polling or disk I/O.
final class PermissionDragDemonstration: NSView {
    private let applicationIcon = CALayer()
    private let destination = CALayer()
    private let toggle = CALayer()
    private let knob = CALayer()
    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        layer?.addSublayer(destination)
        layer?.addSublayer(toggle)
        toggle.addSublayer(knob)
        layer?.addSublayer(applicationIcon)
        let icon = NSWorkspace.shared.icon(forFile: Bundle.main.bundlePath)
        applicationIcon.contents = icon.cgImage(forProposedRect: nil, context: nil, hints: nil)
        applicationIcon.contentsGravity = .resizeAspect
        destination.cornerRadius = 8; destination.borderWidth = 1
        toggle.cornerRadius = 10; knob.cornerRadius = 8
        setAccessibilityElement(true)
        setAccessibilityLabel(L("setup.drag_demonstration"))
    }
    required init?(coder: NSCoder) { fatalError("init(coder:) is unavailable") }
    override func layout() {
        super.layout()
        CATransaction.begin(); CATransaction.setDisableActions(true)
        applicationIcon.frame = CGRect(x: 12, y: 16, width: 36, height: 36)
        destination.frame = CGRect(x: max(80, bounds.width - 160), y: 8, width: 156, height: 52)
        toggle.frame = CGRect(x: bounds.width - 48, y: 24, width: 32, height: 20)
        knob.frame = CGRect(x: 2, y: 2, width: 16, height: 16)
        destination.borderColor = NSColor.separatorColor.cgColor
        toggle.backgroundColor = NSColor.tertiaryLabelColor.cgColor
        knob.backgroundColor = NSColor.windowBackgroundColor.cgColor
        CATransaction.commit()
    }
    func play() {
        applicationIcon.removeAllAnimations(); toggle.removeAllAnimations(); knob.removeAllAnimations()
        guard !NSWorkspace.shared.accessibilityDisplayShouldReduceMotion else { return }
        layoutSubtreeIfNeeded()
        let drag = CAKeyframeAnimation(keyPath: "position")
        let start = applicationIcon.position
        let end = CGPoint(x: destination.frame.minX + 28, y: start.y)
        drag.values = [NSValue(point: start), NSValue(point: start), NSValue(point: end), NSValue(point: end)]
        drag.keyTimes = [0, 0.15, 0.6, 1]
        drag.timingFunctions = [.init(name: .easeInEaseOut), .init(name: .easeInEaseOut), .init(name: .linear)]
        drag.duration = 3; drag.repeatCount = 3
        applicationIcon.add(drag, forKey: "dragDemonstration")
        let enabled = CAKeyframeAnimation(keyPath: "backgroundColor")
        enabled.values = [NSColor.tertiaryLabelColor.cgColor, NSColor.tertiaryLabelColor.cgColor, NSColor.controlAccentColor.cgColor, NSColor.controlAccentColor.cgColor]
        enabled.keyTimes = [0, 0.7, 0.8, 1]; enabled.duration = 3; enabled.repeatCount = 3
        toggle.add(enabled, forKey: "switchDemonstration")
        let slide = CAKeyframeAnimation(keyPath: "position.x")
        slide.values = [10, 10, 22, 22]; slide.keyTimes = [0, 0.7, 0.8, 1]
        slide.duration = 3; slide.repeatCount = 3
        knob.add(slide, forKey: "switchDemonstration")
    }
}
