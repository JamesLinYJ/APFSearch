import AppKit

// Only the transport is substituted. AppKit table, views, animations, and the
// controller's production methods run unchanged in a separate AppKit window.
final class SearchClient {
    static let shared = SearchClient()
    struct Pending {
        let request: [String: Any]
        let completion: ([String: Any]) -> Void
    }
    var pending = [Pending]()
    func registerService() {}
    func call(_ request: [String: Any], completion: @escaping ([String: Any]) -> Void) {
        pending.append(Pending(request: request, completion: completion))
    }
    func takeQuery() -> Pending? {
        take("query")
    }
    func take(_ operation: String) -> Pending? {
        guard let i = pending.firstIndex(where: { $0.request["op"] as? String == operation }) else { return nil }
        return pending.remove(at: i)
    }
    func clear() { pending.removeAll() }
}

@main
@MainActor
enum SearchWindowTests {
    static var assertions = [[String: Any]]()
    static var unavailable = [[String: Any]]()
    static var controllers = [SearchWindowController]()
    static func check(_ name: String, _ condition: Bool, _ evidence: [String: Any] = [:]) {
        assertions.append(["test": name, "passed": condition, "evidence": evidence])
    }
    static func checkDisplay(_ name: String, _ condition: Bool, _ evidence: [String: Any] = [:]) {
        guard !NSScreen.screens.isEmpty else {
            unavailable.append(["test": name, "reason": "No display is available to this process; headless NSWindow layout and model values are not evidence of on-screen geometry or animation."])
            return
        }
        check(name, condition, evidence)
    }
    static func pump(_ seconds: Double = 0.03) async {
        // Suspend the test task so NSApplication can finish its normal outer
        // event-loop transaction and deliver AppKit animation completions.
        try? await Task.sleep(nanoseconds: UInt64(seconds * 1_000_000_000))
    }
    static func row(_ number: Int, size: Int = 10) -> [String: Any] {
        ["id": Int64(9_007_199_254_740_993) + Int64(number), "path": String(format: "/UIRegression/%06d.txt", number), "name": String(format: "%06d.txt", number), "size": size, "modified": 1700000000, "is_dir": false]
    }
    static func reply(_ rows: [[String: Any]], count: Int? = nil, generation: Int = 1, offset: Int = 0, anchor: Int? = nil) -> [String: Any] {
        var result: [String: Any] = ["success": true, "rows": rows, "total": count ?? rows.count, "generation": generation, "offset": offset, "elapsed_ms": 1]
        if let anchor { result["anchor_index"] = anchor }
        return result
    }
    static func controller(_ rows: [[String: Any]], count: Int? = nil) async -> SearchWindowController {
        let c = SearchWindowController(offlineListID: UUID().uuidString, count: count ?? rows.count)
        c.window?.setFrame(NSRect(x: 100, y: 100, width: 1000, height: 600), display: false)
        c.window?.title = "APFSearch UI Regression"
        // A AppKit test window is needed for real row view/layout materialization.
        // It is not activated and never uses accessibility or the user's window.
        c.window?.orderBack(nil)
        c.applyResultRows(rows, offset: 0, count: count ?? rows.count, replaceCache: true, animate: false)
        c.resultsAreCurrent = true; c.generation = 1
        c.window?.contentView?.layoutSubtreeIfNeeded(); c.table.layoutSubtreeIfNeeded(); c.table.displayIfNeeded()
        await pump()
        c.table.testReloadedRows.removeAll(); c.table.testFullReloads = 0
        controllers.append(c)
        SearchClient.shared.clear()
        return c
    }
    static func viewIDs(_ c: SearchWindowController, indexes: Range<Int>) -> [Int: ObjectIdentifier] {
        Dictionary(uniqueKeysWithValues: indexes.compactMap { index in
            c.table.rowView(atRow: index, makeIfNecessary: true).map { (index, ObjectIdentifier($0)) }
        })
    }
    static func topViewportRegression() async {
        let rows = (0..<287).map { row($0) }
        let c = await controller(rows)
        c.offlineListID = nil; c.roots = ["/UIRegression"]
        c.search.stringValue = "crossover"
        let scroll = c.table.enclosingScrollView!, clip = scroll.contentView
        // Let AppKit choose its actual top boundary, including its header and
        // automatic content insets. Zero is not necessarily the top coordinate.
        var proposed = clip.bounds; proposed.origin.y = -100_000
        let top = clip.constrainBoundsRect(proposed).origin
        clip.scroll(to: top); scroll.reflectScrolledClipView(clip)
        await pump()
        let initial = clip.bounds.origin
        var samples = [Double]()
        for revision in 2...6 {
            c.currentStatus = ["success": true, "generation": revision]
            SearchClient.shared.clear(); c.refreshVisibleResults()
            guard let request = SearchClient.shared.takeQuery() else {
                check("top_viewport_refresh_dispatches", false); break
            }
            request.completion(reply(rows, generation: revision, anchor: 0))
            await pump(0.03)
            samples.append(clip.bounds.origin.y)
        }
        checkDisplay("unchanged_background_results_preserve_native_top_boundary",
            samples.count == 5 && samples.allSatisfy { abs($0 - initial.y) < 0.5 },
            ["initial_y": initial.y, "refreshed_y": samples, "top_inset": scroll.contentInsets.top,
             "header_height": c.table.headerView?.frame.height ?? 0])
        // Scrolling back to the top must remain there after live scrolling ends.
        NotificationCenter.default.post(name: NSScrollView.willStartLiveScrollNotification, object: scroll)
        clip.scroll(to: top); scroll.reflectScrolledClipView(clip)
        c.currentStatus = ["success": true, "generation": 7]
        SearchClient.shared.clear()
        NotificationCenter.default.post(name: NSScrollView.didEndLiveScrollNotification, object: scroll)
        let release = SearchClient.shared.takeQuery()
        release?.completion(reply(rows, generation: 7, anchor: 0))
        await pump()
        checkDisplay("releasing_live_scroll_does_not_hide_first_result", release != nil && abs(clip.bounds.origin.y - initial.y) < 0.5,
            ["initial_y": initial.y, "after_release_y": clip.bounds.origin.y])
        c.cancelQueries(); c.window?.orderOut(nil); SearchClient.shared.clear()
    }
    static func startupTablePreferencesRegression() async {
        // Exercise AppKit's actual autosave format in this test bundle's own
        // preference domain. No production app preferences are read or written.
        let defaults = UserDefaults.standard
        let domain = Bundle.main.bundleIdentifier!
        let savedDomain = defaults.persistentDomain(forName: domain)
        defer {
            if let savedDomain { defaults.setPersistentDomain(savedDomain, forName: domain) }
            else { defaults.removePersistentDomain(forName: domain) }
        }
        defaults.removePersistentDomain(forName: domain)
        defaults.set(["created"], forKey: "APFSearch.HiddenColumns")
        let saved = NSTableView(frame: NSRect(x: 0, y: 0, width: 1400, height: 400))
        saved.columnAutoresizingStyle = .noColumnAutoresizing
        for (key, width) in [("name", 251.0), ("path", 427.0), ("size", 137.0), ("modified", 191.0), ("created", 143.0)] {
            let column = NSTableColumn(identifier: .init(key))
            column.width = width
            column.sortDescriptorPrototype = NSSortDescriptor(key: key, ascending: true)
            saved.addTableColumn(column)
        }
        saved.autosaveName = "APFSearch.Columns"
        saved.autosaveTableColumns = true
        saved.moveColumn(1, toColumn: 0)
        saved.tableColumns.first { $0.identifier.rawValue == "created" }!.isHidden = true
        saved.tableColumns.first { $0.identifier.rawValue == "modified" }!.isHidden = true
        saved.sortDescriptors = [NSSortDescriptor(key: "path", ascending: false), NSSortDescriptor(key: "name", ascending: true)]
        await pump()
        let expectedOrder = saved.tableColumns.map { $0.identifier.rawValue }
        let expectedWidths = Dictionary(uniqueKeysWithValues: saved.tableColumns.map { ($0.identifier.rawValue, $0.width) })
        saved.autosaveTableColumns = false
        let restored = SearchWindowController(offlineListID: UUID().uuidString)
        controllers.append(restored)
        let actualSort = restored.table.sortDescriptors.map { ["field": $0.key ?? "", "ascending": $0.ascending] as [String: Any] }
        check("startup_restores_saved_multicolumn_sort", restored.table.sortDescriptors.map(\.key) == ["path", "name"] && restored.table.sortDescriptors.map(\.ascending) == [false, true], ["sort": actualSort])
        check("startup_restores_saved_column_order", restored.table.tableColumns.map { $0.identifier.rawValue } == expectedOrder, ["actual": restored.table.tableColumns.map { $0.identifier.rawValue }, "expected": expectedOrder])
        let widthChanges = restored.table.tableColumns.map { abs($0.width - expectedWidths[$0.identifier.rawValue]!) }
        check("startup_restores_saved_column_widths_after_initial_layout", widthChanges.allSatisfy { $0 <= 0.5 }, ["actual": Dictionary(uniqueKeysWithValues: restored.table.tableColumns.map { ($0.identifier.rawValue, $0.width) }), "expected": expectedWidths])
        let restoredHidden = restored.table.tableColumns.filter(\.isHidden).map { $0.identifier.rawValue }
        let hiddenMenuItems = restored.table.headerView?.menu?.items.filter { $0.state == .off }.compactMap { $0.representedObject as? String } ?? []
        check("startup_restores_column_visibility_and_matching_menu_state", Set(restoredHidden) == ["modified", "created"] && Set(hiddenMenuItems) == Set(restoredHidden), ["hidden_columns": restoredHidden, "hidden_menu_items": hiddenMenuItems])
        restored.window?.orderBack(nil)
        await pump(0.1)
        checkDisplay("first_display_preserves_restored_column_widths", restored.table.tableColumns.allSatisfy { abs($0.width - expectedWidths[$0.identifier.rawValue]!) <= 0.5 }, ["actual": Dictionary(uniqueKeysWithValues: restored.table.tableColumns.map { ($0.identifier.rawValue, $0.width) }), "expected": expectedWidths])
        restored.runQuery()
        let firstQuery = SearchClient.shared.takeQuery()?.request
        check("first_query_uses_restored_sort", ((firstQuery?["sort"] as? [[String: Any]])?.map { $0["field"] as? String }) == ["path", "name"], ["query": firstQuery ?? [:]])
        restored.table.autosaveTableColumns = false
        restored.window?.orderOut(nil)
        let reopened = SearchWindowController(offlineListID: UUID().uuidString)
        controllers.append(reopened)
        check("reopened_window_keeps_saved_sort_and_column_order", reopened.table.sortDescriptors.map(\.key) == ["path", "name"] && reopened.table.sortDescriptors.map(\.ascending) == [false, true] && reopened.table.tableColumns.map { $0.identifier.rawValue } == expectedOrder)
        reopened.table.autosaveTableColumns = false
        reopened.window?.orderOut(nil)
        defaults.removePersistentDomain(forName: domain)
        let fresh = SearchWindowController(offlineListID: UUID().uuidString)
        controllers.append(fresh)
        check("new_installation_defaults_to_name_ascending", fresh.table.sortDescriptors.map(\.key) == ["name"] && fresh.table.sortDescriptors.map(\.ascending) == [true])
        fresh.table.autosaveTableColumns = false
        fresh.window?.orderOut(nil)
        SearchClient.shared.clear()
    }

    static func windowFrameRegression(_ size: NSSize) async {
        guard !NSScreen.screens.isEmpty else {
            unavailable.append(["test": "window_outer_frame_stays_fixed_\(Int(size.width))x\(Int(size.height))", "reason": "This process has no available display (NSScreen.screens is empty); macOS constrains ordered windows to a synthetic minimum. A GUI-launched run is required."])
            return
        }
        let rows = (0..<20).map { row($0) }
        let c = await controller(rows)
        c.offlineListID = nil; c.roots = ["/UIRegression"]
        c.initialStatus = false
        c.currentStatus = ["success": true, "generation": 1, "count": rows.count, "scanning": false]
        NSApp.activate(ignoringOtherApps: true)
        c.window?.makeKeyAndOrderFront(nil)
        // Select a known, on-screen target before asking AppKit to resize.
        // Hosted macOS displays can be shorter than the nominal test height;
        // accepting the resulting baseline without an independent target would
        // conceal an application-driven resize, while an oversized target only
        // tests the window manager's screen constraint.
        let screen = c.window!.screen ?? NSScreen.screens[0]
        let available = screen.visibleFrame.insetBy(dx: 20, dy: 20)
        let targetSize = NSSize(width: min(size.width, available.width), height: min(size.height, available.height))
        guard targetSize.width >= c.window!.minSize.width, targetSize.height >= c.window!.minSize.height else {
            unavailable.append(["test": "window_outer_frame_stays_fixed", "reason": "The available display cannot fit the application's minimum window size."])
            c.window?.orderOut(nil)
            return
        }
        let targetFrame = NSRect(x: available.midX - targetSize.width / 2, y: available.midY - targetSize.height / 2,
            width: targetSize.width, height: targetSize.height)
        // This is the user's explicit resize being tested. No subsequent
        // action or observation resets, pins, or restores the window frame.
        c.window?.setFrame(targetFrame, display: true)
        await pump(0.2)
        let baseline = c.window!.frame
        var maxDelta = 0.0
        var firstChange = ""
        var samples = 0
        var sidebarSamples = [[String: Any]]()
        var paneOrigins = [CGFloat]()
        var animatedSidebarActions = 0
        var presentationSamples = 0
        func observe(_ action: String) {
            let frame = c.window!.frame
            let delta = max(abs(frame.minX - baseline.minX), abs(frame.minY - baseline.minY), abs(frame.width - baseline.width), abs(frame.height - baseline.height))
            maxDelta = max(maxDelta, delta); samples += 1
            let pane = c.splitController.splitViewItems.last!.viewController.view
            if let presentation = pane.layer?.presentation() {
                // AppKit can animate ancestor backing layers while NSView.frame
                // already holds the target geometry. Include that transform.
                let root = c.splitController.splitView.layer?.presentation()
                paneOrigins.append(presentation.convert(.zero, to: root).x); presentationSamples += 1
            } else { paneOrigins.append(pane.frame.minX) }
            if delta > 0.5 && firstChange.isEmpty { firstChange = action + ": " + NSStringFromRect(frame) }
        }
        func settle(_ action: String, duration: Double = 0.08) async {
            let end = ProcessInfo.processInfo.systemUptime + duration
            repeat { observe(action); await pump(0.01) } while ProcessInfo.processInfo.systemUptime < end
            observe(action)
        }
        for iteration in 0..<20 {
            let pane = c.splitController.splitViewItems.last!.viewController.view
            let beforeOrigin = pane.convert(.zero, to: c.splitController.splitView).x
            paneOrigins.removeAll()
            c.toggleSidebar(nil)
            await settle("sidebar_\(iteration)", duration: 0.3)
            let afterOrigin = pane.convert(.zero, to: c.splitController.splitView).x
            let low = min(beforeOrigin, afterOrigin), high = max(beforeOrigin, afterOrigin)
            let intermediateFrames = paneOrigins.filter { $0 > low + 0.5 && $0 < high - 0.5 }.count
            if intermediateFrames > 0 { animatedSidebarActions += 1 }
            sidebarSamples.append(["iteration": iteration, "collapsed": c.splitController.splitViewItems[0].isCollapsed,
                "application_active": NSApp.isActive, "window_key": c.window!.isKeyWindow,
                "window_frame": NSStringFromRect(c.window!.frame), "window_min_size": NSStringFromSize(c.window!.minSize),
                "content_min_size": NSStringFromSize(c.window!.contentMinSize), "split_fitting_size": NSStringFromSize(c.splitController.view.fittingSize),
                "pane_frames": c.splitController.splitViewItems.map { NSStringFromRect($0.viewController.view.frame) },
                "sampled_pane_origin_min": paneOrigins.min() ?? 0, "sampled_pane_origin_max": paneOrigins.max() ?? 0,
                "intermediate_geometry_samples": intermediateFrames])
        }
        SearchClient.shared.clear()
        c.search.stringValue = String(repeating: "很长的搜索条件 abcdefghijklmnopqrstuvwxyz ", count: 20)
        c.runQuery(); observe("long_search_pending")
        SearchClient.shared.takeQuery()?.completion(reply(rows))
        await settle("long_search_reply")
        c.selectFilter(query: "file:", title: String(repeating: "筛选", count: 100))
        SearchClient.shared.takeQuery()?.completion(reply(Array(rows.prefix(2))))
        await settle("filter_reply")
        c.search.stringValue = "missing"; c.runQuery()
        SearchClient.shared.takeQuery()?.completion(reply([]))
        await settle("empty_state", duration: 0.2)
        c.search.stringValue = ""; c.runQuery()
        SearchClient.shared.takeQuery()?.completion(reply(rows))
        await settle("results_return")
        c.cachedRows[0]?["path"] = "/UIRegression/" + String(repeating: "很长的父目录/", count: 60) + "file.txt"
        c.table.selectRowIndexes(IndexSet(integer: 0), byExtendingSelection: false)
        c.synchronizeSelection()
        await settle("long_selected_path")
        c.table.deselectAll(nil); c.synchronizeSelection()
        await settle("deselect_path")
        c.shortcuts.addItem(withTitle: String(repeating: "很长的书签名称 ", count: 100))
        c.shortcuts.selectItem(at: c.shortcuts.numberOfItems - 1)
        await settle("long_bookmark_label")
        for iteration in 0..<10 {
            c.pollStatus()
            SearchClient.shared.take("status")?.completion(["success": true, "generation": 1, "count": 1_333_654 + iteration, "scanning": iteration < 5, "roots": ["/UIRegression"], "uncovered": Array(repeating: "未覆盖的路径", count: iteration)])
            c.queryWarnings = [String(repeating: "状态消息与文件路径 ", count: 100)]
            c.updateStatus()
            await settle("status_\(iteration)", duration: 0.02)
        }
        c.queryTimer?.invalidate(); c.historyTimer?.invalidate(); c.stopStatusObservation()
        let requestedSizeWasAccepted = abs(baseline.width - targetSize.width) <= 0.5 && abs(baseline.height - targetSize.height) <= 0.5
        check("window_outer_frame_stays_fixed_\(Int(targetSize.width))x\(Int(targetSize.height))", requestedSizeWasAccepted && maxDelta <= 0.5, ["nominal_width": size.width, "nominal_height": size.height, "requested_width": targetSize.width, "requested_height": targetSize.height, "screen_visible_frame": NSStringFromRect(screen.visibleFrame), "baseline": NSStringFromRect(baseline), "samples": samples, "max_frame_delta_points": maxDelta, "first_changed_action": firstChange, "sidebar_actions": 20, "sidebar_samples": sidebarSamples])
        if !NSWorkspace.shared.accessibilityDisplayShouldReduceMotion {
            check("appkit_sidebar_has_intermediate_geometry_\(Int(size.width))", animatedSidebarActions == 20, ["actions": 20, "actions_with_intermediate_frames": animatedSidebarActions, "presentation_layer_samples": presentationSamples, "observation": "Core Animation presentation geometry sampled during AppKit constraint animation; this does not measure compositor frame timing."])
        }
        c.window?.orderOut(nil)
    }
    static func main() {
        let app = NSApplication.shared
        app.setActivationPolicy(.accessory)
        Timer.scheduledTimer(withTimeInterval: 0.001, repeats: false) { _ in Task { @MainActor in await runTests() } }
        app.run()
    }
    static func runTests() async {
        let base = (0..<20).map { row($0) }
        if CommandLine.arguments.contains("--sidebar-only") {
            let only = await controller(base)
            let sidebar = only.splitController.splitViewItems.first!
            print("before window=\(only.window!.frame) collapsed=\(sidebar.isCollapsed) canCollapse=\(sidebar.canCollapse) windowVisible=\(only.window!.isVisible) frames=\(only.splitController.splitView.subviews.map(\.frame))")
            only.window?.makeKeyAndOrderFront(nil)
            only.toggleSidebar(nil)
            await pump(0.55)
            print("single window=\(only.window!.frame) collapsed=\(sidebar.isCollapsed) frames=\(only.splitController.splitView.subviews.map(\.frame))")
            only.toggleSidebar(nil)
            await pump(0.55)
            print("reverse window=\(only.window!.frame) collapsed=\(sidebar.isCollapsed) frames=\(only.splitController.splitView.subviews.map(\.frame))")
            for _ in 0..<5 { only.toggleSidebar(nil) }
            await pump(0.55)
            print("rapid window=\(only.window!.frame) collapsed=\(sidebar.isCollapsed) frames=\(only.splitController.splitView.subviews.map(\.frame))")
            only.toggleSidebar(nil)
            await pump(0.55)
            print("after-rapid window=\(only.window!.frame) collapsed=\(sidebar.isCollapsed) frames=\(only.splitController.splitView.subviews.map(\.frame))")
            only.window?.orderOut(nil)
            exit(0)
        }
        await featureIntegrationRegression()
        await topViewportRegression()
        await startupTablePreferencesRegression()
        let c = await controller(base)
        let incompleteDuplicateReport = c.duplicateReport(["groups": [], "hardlinks": [], "errors": ["/UIRegression/unreadable.txt: denied"], "partial": true])
        check("incomplete_duplicate_report_does_not_claim_no_duplicates", incompleteDuplicateReport.contains(L("duplicates.partial_notice")) && !incompleteDuplicateReport.contains(L("duplicates.none_found")))
        let hardlinkReport = c.duplicateReport(["groups": [], "hardlinks": [["paths": ["/UIRegression/link-a.txt", "/UIRegression/link-b.txt"]]], "errors": []])
        check("hardlink_only_report_labels_shared_identity_without_independent_duplicate_group", hardlinkReport.contains(L("duplicates.hardlinks")) && !hardlinkReport.contains(L("duplicates.independent_files")) && hardlinkReport.contains("/UIRegression/link-a.txt") && hardlinkReport.contains("/UIRegression/link-b.txt"))
        let combinedDuplicateReport = c.duplicateReport(["groups": [["rows": [["path": "/UIRegression/copy-a.txt"], ["path": "/UIRegression/copy-b.txt"]]]], "hardlinks": [["paths": ["/UIRegression/link-a.txt", "/UIRegression/link-b.txt"]]], "errors": [["path": "/UIRegression/unreadable.txt", "stage": "open", "message": "denied"]]])
        check("duplicate_report_preserves_independent_hardlink_and_unreadable_paths", ["/UIRegression/copy-a.txt", "/UIRegression/copy-b.txt", "/UIRegression/link-a.txt", "/UIRegression/link-b.txt", "/UIRegression/unreadable.txt"].allSatisfy { combinedDuplicateReport.contains($0) } && combinedDuplicateReport.contains(L("duplicates.partial_notice")))
        let initialViews = viewIDs(c, indexes: 0..<10)
        c.applyResultRows(base, offset: 0, count: base.count, replaceCache: true, animate: true)
        await pump()
        check("unchanged_snapshot_does_not_reload_rows", c.table.testReloadedRows.isEmpty && c.table.testFullReloads == 0)
        check("unchanged_snapshot_keeps_appkit_row_views", initialViews == viewIDs(c, indexes: 0..<10))

        var modified = base; modified[3] = row(3, size: 11)
        c.applyResultRows(modified, offset: 0, count: modified.count, replaceCache: true, animate: true)
        await pump()
        let reloaded = c.table.testReloadedRows.reduce(into: IndexSet()) { $0.formUnion($1) }
        check("metadata_update_reloads_only_changed_row", reloaded == IndexSet(integer: 3), ["reloaded": Array(reloaded)])
        let afterMetadata = viewIDs(c, indexes: 0..<10)
        check("metadata_update_keeps_unrelated_row_views", (0..<10).filter { $0 != 3 }.allSatisfy { initialViews[$0] == afterMetadata[$0] })

        c.table.testReloadedRows.removeAll()
        c.table.selectRowIndexes(IndexSet(integer: 5), byExtendingSelection: false)
        let beforeInsert = viewIDs(c, indexes: 0..<10)
        let inserted = [row(-1)] + modified
        c.applyResultRows(inserted, offset: 0, count: inserted.count, replaceCache: true, animate: true)
        await pump(0.25)
        let insertReloads = c.table.testReloadedRows.reduce(into: IndexSet()) { $0.formUnion($1) }
        check("appkit_insertion_does_not_reload_shifted_rows", insertReloads.isEmpty, ["reloaded": Array(insertReloads)])
        let afterInsert = viewIDs(c, indexes: 1..<11)
        check("appkit_insertion_retains_surviving_row_views", (0..<10).allSatisfy { beforeInsert[$0] == afterInsert[$0 + 1] })
        check("appkit_insertion_finishes_with_correct_count_and_identity", c.table.numberOfRows == 21 && c.selectedPaths == [row(5)["path"] as! String] && !c.resultAnimationInFlight, ["selection": c.selectedPaths, "animation_in_flight": c.resultAnimationInFlight])

        c.table.testReloadedRows.removeAll()
        c.applyResultRows(modified, offset: 0, count: modified.count, replaceCache: true, animate: true)
        await pump(0.25)
        let deleteReloads = c.table.testReloadedRows.reduce(into: IndexSet()) { $0.formUnion($1) }
        check("appkit_deletion_does_not_reload_shifted_rows", deleteReloads.isEmpty, ["reloaded": Array(deleteReloads)])
        check("appkit_deletion_finishes_with_correct_count_and_identity", c.table.numberOfRows == 20 && c.selectedPaths == [row(5)["path"] as! String], ["selection": c.selectedPaths])

        // Hold a reply, then type without running the debounce timer. The old
        // reply must not become actionable or replace the displayed snapshot.
        let typing = await controller(base)
        typing.search.stringValue = "old"; typing.runQuery()
        let held = SearchClient.shared.takeQuery()
        typing.search.stringValue = "new"
        typing.controlTextDidChange(Notification(name: NSControl.textDidChangeNotification, object: typing.search))
        held?.completion(reply([row(99)]))
        check("input_invalidates_reply_before_debounce_fires", !typing.resultsAreCurrent && typing.cachedRows[0]?["path"] as? String == base[0]["path"] as? String, ["results_current": typing.resultsAreCurrent, "first_path": typing.cachedRows[0]?["path"] as? String ?? ""])
        typing.queryTimer?.invalidate(); typing.historyTimer?.invalidate(); SearchClient.shared.clear()

        // Deliver two different query replies while AppKit is moving rows.
        // Neither may tear down an in-flight insertion; only the newest query
        // may publish once the AppKit animation completion has arrived.
        let overlap = await controller(base)
        overlap.applyResultRows([row(-1)] + base, offset: 0, count: 21, replaceCache: true, animate: true)
        overlap.search.stringValue = "first"; overlap.runQuery()
        let superseded = SearchClient.shared.takeQuery()
        superseded?.completion(reply([row(97)]))
        overlap.search.stringValue = "latest"; overlap.runQuery()
        let latest = SearchClient.shared.takeQuery()
        latest?.completion(reply([row(98), row(99)]))
        check("query_replies_wait_for_appkit_animation_completion", overlap.total == 21 && overlap.resultAnimationInFlight)
        await pump(0.3)
        check("only_latest_deferred_query_becomes_visible", overlap.total == 2 && overlap.resultsAreCurrent && overlap.cachedRows[0]?["path"] as? String == row(98)["path"] as? String && !overlap.resultAnimationInFlight, ["count": overlap.total, "first_path": overlap.cachedRows[0]?["path"] as? String ?? "", "animation_in_flight": overlap.resultAnimationInFlight])
        overlap.historyTimer?.invalidate(); SearchClient.shared.clear()

        // The top visible path and selected path survive insertion before a
        // large, paged viewport; test the actual background-reply code path.
        let largeRows = (0..<1000).map { row($0) }
        let large = await controller(largeRows)
        large.offlineListID = nil
        large.roots = ["/UIRegression"]
        large.currentStatus = ["success": true, "generation": 2, "count": 1001, "scanning": false]
        large.table.selectRowIndexes(IndexSet(integer: 505), byExtendingSelection: false)
        let clip = large.table.enclosingScrollView!.contentView
        clip.scroll(to: NSPoint(x: 0, y: large.table.rect(ofRow: 500).minY + 4))
        large.table.enclosingScrollView!.reflectScrolledClipView(clip)
        large.window?.contentView?.layoutSubtreeIfNeeded(); await pump()
        SearchClient.shared.clear()
        let originalVisible = large.table.rows(in: large.table.visibleRect).location
        let originalAnchor = large.cachedRows[originalVisible]?["path"] as? String
        let originalAnchorID = large.cachedRows[originalVisible]?["id"] as? NSNumber
        let originalPixelOffset = clip.bounds.origin.y - large.table.rect(ofRow: originalVisible).minY
        large.refreshVisibleResults()
        if let refresh = SearchClient.shared.takeQuery() {
            let transmitted = jsonObject(jsonData(refresh.request))
            check("background_anchor_id_and_path_come_from_first_visible_cached_row", refresh.request["anchor_path"] as? String == originalAnchor && (refresh.request["anchor_id"] as? NSNumber) == originalAnchorID && refresh.request["anchor_delta"] as? Int == originalVisible % large.pageSize, ["first_visible_row": originalVisible, "selected_row": large.table.selectedRow, "anchor_id": refresh.request["anchor_id"] ?? "missing"])
            check("background_anchor_id_preserves_64_bit_integer_through_json", (transmitted["anchor_id"] as? NSNumber)?.int64Value == originalAnchorID?.int64Value && (originalAnchorID?.int64Value ?? 0) > 9_007_199_254_740_992)
            let newRows = [row(-1)] + largeRows
            let delta = refresh.request["anchor_delta"] as? Int ?? 0
            let anchor = originalVisible + 1, offset = max(0, anchor - delta)
            let limit = refresh.request["limit"] as? Int ?? 400
            refresh.completion(reply(Array(newRows[offset..<min(offset + limit, newRows.count)]), count: newRows.count, generation: 2, offset: offset, anchor: anchor))
            await pump(0.25)
            let now = large.table.rows(in: large.table.visibleRect).location
            let nowPath = large.cachedRows[now]?["path"] as? String
            let pixel = clip.bounds.origin.y - large.table.rect(ofRow: now).minY
            check("background_refresh_preserves_top_path_and_pixel_offset", nowPath == originalAnchor && abs(pixel - originalPixelOffset) < 0.5, ["before_index": originalVisible, "after_index": now, "before_pixel": originalPixelOffset, "after_pixel": pixel])
            check("background_refresh_preserves_selected_path", large.selectedPaths == [row(505)["path"] as! String], ["selection": large.selectedPaths])
            check("background_refresh_never_reloads_entire_table", large.table.testFullReloads == 0)
        } else {
            check("background_refresh_dispatches_for_changed_generation", false, ["visible": large.window?.isVisible == true, "origin": clip.bounds.origin.y])
        }

        // Page zero is also an ordinary cache miss. It must not reuse the
        // initial-query restoration/scrolling behavior after a live refresh.
        SearchClient.shared.clear()
        let oldSelection = large.selectedPaths, oldY = clip.bounds.origin.y
        large.pendingSelectedPaths = [row(2)["path"] as! String]
        large.requestPage(0)
        if let page = SearchClient.shared.takeQuery() {
            page.completion(reply([row(-1)] + Array(largeRows.prefix(199)), count: 1001, generation: 2))
            await pump()
            check("loading_page_zero_preserves_scroll_and_current_selection", large.selectedPaths == oldSelection && abs(clip.bounds.origin.y - oldY) < 0.5, ["selection": large.selectedPaths, "before_y": oldY, "after_y": clip.bounds.origin.y])
            check("loading_page_zero_preserves_other_cached_pages", large.cachedRows[506]?["path"] as? String == row(505)["path"] as? String)
        } else { check("loading_page_zero_dispatches", false) }

        SearchClient.shared.clear()
        large.initialStatus = false
        for _ in 0..<5 { large.pollStatus() }
        let statusRequests = SearchClient.shared.pending.filter { $0.request["op"] as? String == "status" }
        check("status_polling_does_not_overlap_requests", statusRequests.count == 1, ["request_count": statusRequests.count])

        // Selection may expand while a background request is in flight. The
        // reply must not silently narrow a user's Command-A to one page.
        SearchClient.shared.clear()
        large.currentStatus["generation"] = 3
        large.refreshVisibleResults()
        if let heldRefresh = SearchClient.shared.takeQuery() {
            large.table.selectAll(nil)
            let before = large.table.selectedRowIndexes
            let start = heldRefresh.request["offset"] as? Int ?? 400
            let newRows = [row(-1)] + largeRows
            heldRefresh.completion(reply(Array(newRows[start..<min(start + 400, newRows.count)]), count: newRows.count, generation: 3, offset: start, anchor: 501))
            await pump()
            check("background_reply_does_not_truncate_new_multiselection", before == large.table.selectedRowIndexes, ["before_selected": before.count, "after_selected": large.table.selectedRowIndexes.count])
        } else { check("multiselection_race_dispatches", false) }

        SearchClient.shared.clear()
        let selectedCount = large.table.selectedRowIndexes.count
        check("partially_cached_selection_is_not_actionable", !large.selectionIsComplete && !large.canUseSelection, ["selected_count": selectedCount, "loaded_selected_paths": large.selectedPaths.count])
        large.openSelection(nil); large.trashSelection(nil); large.indexContent(nil)
        check("expired_partial_selection_sends_no_file_or_content_mutation", !SearchClient.shared.pending.contains { ["files", "content_index"].contains($0.request["op"] as? String ?? "") }, ["requests": SearchClient.shared.pending.map { $0.request["op"] as? String ?? "" }])
        // An expired lease may request a fresh query, but cannot act on its
        // loaded subset. Reset only this controlled fixture for anchor tests.
        large.cancelQueries(); large.queryPending = false; large.resultsAreCurrent = true
        SearchClient.shared.clear()
        let loadedRow = large.cachedRows.keys.min()!
        check("partial_selection_cannot_start_file_drag", large.tableView(large.table, pasteboardWriterForRow: loadedRow) == nil)

        let anchorFallback = large
        anchorFallback.table.deselectAll(nil); anchorFallback.synchronizeSelection()
        anchorFallback.currentStatus = ["success": true, "generation": 4]
        let fallbackFirst = max(0, anchorFallback.table.rows(in: anchorFallback.table.visibleRect).location)
        let fallbackPath = anchorFallback.cachedRows[fallbackFirst]?["path"] as? String
        anchorFallback.cachedRows[fallbackFirst]?.removeValue(forKey: "id")
        anchorFallback.refreshVisibleResults()
        let pathOnly = SearchClient.shared.takeQuery()?.request
        check("cached_anchor_without_id_keeps_path_fallback", pathOnly?["anchor_path"] as? String == fallbackPath && pathOnly?["anchor_id"] == nil && pathOnly != nil, ["request": pathOnly ?? [:], "expected_path": fallbackPath ?? "missing", "visible": anchorFallback.window?.isVisible == true])
        anchorFallback.cancelQueries(); SearchClient.shared.clear()
        anchorFallback.cachedRows.removeValue(forKey: fallbackFirst)
        anchorFallback.refreshVisibleResults()
        let missingAnchor = SearchClient.shared.takeQuery()?.request
        check("uncached_first_visible_row_sends_no_anchor_pair", missingAnchor != nil && missingAnchor?["anchor_path"] == nil && missingAnchor?["anchor_id"] == nil && missingAnchor?["anchor_delta"] == nil)
        anchorFallback.cancelQueries(); SearchClient.shared.clear()

        let layout = await controller(base)
        layout.offlineListID = nil; layout.roots = ["/UIRegression"]
        layout.currentStatus = ["success": true, "count": base.count, "scanning": false]
        layout.table.deselectAll(nil); layout.synchronizeSelection()
        layout.window?.contentView?.layoutSubtreeIfNeeded(); await pump()
        let unselectedFrame = layout.table.enclosingScrollView!.frame
        layout.table.selectRowIndexes(IndexSet(integer: 2), byExtendingSelection: false)
        layout.synchronizeSelection(); layout.window?.contentView?.layoutSubtreeIfNeeded(); await pump()
        let selectedFrame = layout.table.enclosingScrollView!.frame
        layout.table.deselectAll(nil); layout.synchronizeSelection()
        layout.window?.contentView?.layoutSubtreeIfNeeded(); await pump()
        checkDisplay("selection_path_visibility_keeps_table_viewport_geometry", unselectedFrame == selectedFrame && layout.table.enclosingScrollView!.frame == unselectedFrame, ["unselected_height": unselectedFrame.height, "selected_height": selectedFrame.height])

        layout.total = 0; layout.cachedRows.removeAll(); layout.table.noteNumberOfRowsChanged()
        layout.currentStatus["scanning"] = true
        layout.updateEmptyState(); await pump(0.2)
        let emptyFrame = layout.emptyState.frame
        let buttonHidden = layout.emptyButton.isHidden
        for _ in 0..<10 { layout.updateEmptyState(); layout.window?.contentView?.layoutSubtreeIfNeeded() }
        checkDisplay("unchanged_scanning_empty_state_keeps_layout_stable", layout.emptyState.frame == emptyFrame && layout.emptyButton.isHidden == buttonHidden && buttonHidden)
        layout.setEmptyStateVisible(false); layout.setEmptyStateVisible(true)
        layout.setEmptyStateVisible(false); layout.setEmptyStateVisible(true)
        await pump(0.3)
        checkDisplay("rapid_empty_transitions_finish_visible_without_stale_fade", layout.emptyStateVisible && !layout.emptyState.isHidden && abs(layout.emptyState.alphaValue - 1) < 0.01)
        layout.queryPending = true; layout.updateEmptyState()
        check("pending_query_retains_existing_empty_state", layout.emptyStateVisible && !layout.emptyState.isHidden)
        layout.queryPending = false

        // AppKit owns reversal/interruption. The test-only build omits the
        // preference write, while invoking the production action unchanged.
        let sidebar = layout.splitController.splitViewItems.first!
        let originalCollapsed = sidebar.isCollapsed
        layout.window?.makeKeyAndOrderFront(nil)
        layout.toggleSidebar(nil)
        await pump(0.55)
        let firstToggleReversed = sidebar.isCollapsed != originalCollapsed
        layout.toggleSidebar(nil)
        await pump(0.55)
        checkDisplay("appkit_sidebar_opens_and_closes_on_separate_actions", firstToggleReversed && sidebar.isCollapsed == originalCollapsed)
        for _ in 0..<5 { layout.toggleSidebar(nil) }
        await pump(0.55)
        let sidebarFrames = layout.splitController.splitView.subviews.map(\.frame)
        let rapidCollapsed = sidebar.isCollapsed
        await pump(0.25)
        checkDisplay("rapid_appkit_sidebar_actions_settle_without_ongoing_geometry_changes", sidebarFrames == layout.splitController.splitView.subviews.map(\.frame), ["initial_collapsed": originalCollapsed, "after_five_rapid_actions_collapsed": rapidCollapsed, "observation": "AppKit may coalesce rapid actions; per-click parity is not an API guarantee", "stable_geometry_between_550_and_800_ms": sidebarFrames == layout.splitController.splitView.subviews.map(\.frame)])
        layout.toggleSidebar(nil)
        await pump(0.55)
        checkDisplay("appkit_sidebar_still_responds_after_rapid_actions", sidebar.isCollapsed != rapidCollapsed)

        layout.window?.setFrame(NSRect(x: 100, y: 100, width: 950, height: 600), display: true)
        for _ in 0..<10 { NotificationCenter.default.post(name: NSWindow.didResizeNotification, object: layout.window) }
        await pump()
        let preferredWidth = layout.searchToolbarItem!.preferredWidthForSearchField
        await pump()
        checkDisplay("repeated_resize_notifications_settle_at_one_search_width", preferredWidth == layout.searchToolbarItem!.preferredWidthForSearchField, ["preferred_width": preferredWidth])
        await windowFrameRegression(NSSize(width: 850, height: 600))
        await windowFrameRegression(NSSize(width: 1150, height: 740))
        for controller in controllers {
            controller.queryTimer?.invalidate(); controller.historyTimer?.invalidate(); controller.stopStatusObservation()
            controller.window?.orderOut(nil)
        }
        let passed = assertions.filter { $0["passed"] as? Bool == true }.count
        var report: [String: Any] = ["boundary": "Production AppKit table/controller objects, NSApplication.run with MainActor async tests, controlled transport replies. A nonzero screen_count is required for window geometry acceptance. No live XPC, IME synthesis, or compositor timing measurement.", "screen_count": NSScreen.screens.count, "reduce_motion_enabled": NSWorkspace.shared.accessibilityDisplayShouldReduceMotion, "passed": passed, "executed": assertions.count, "total": assertions.count + unavailable.count, "unavailable": unavailable, "complete": passed == assertions.count && unavailable.isEmpty, "tests": assertions]
        if let url = Bundle.main.url(forResource: "source-sha256", withExtension: "txt"), let hash = try? String(contentsOf: url, encoding: .utf8) { report["app_source_sha256"] = hash.trimmingCharacters(in: .whitespacesAndNewlines) }
        report["executed_at_utc"] = ISO8601DateFormatter().string(from: Date())
        let data = try! JSONSerialization.data(withJSONObject: report, options: [.prettyPrinted, .sortedKeys])
        if let url = Bundle.main.url(forResource: "report-path", withExtension: "txt"), let path = try? String(contentsOf: url, encoding: .utf8) {
            try? data.write(to: URL(fileURLWithPath: path.trimmingCharacters(in: .whitespacesAndNewlines)), options: .atomic)
        }
        print(String(data: data, encoding: .utf8)!)
        exit(passed != assertions.count ? 1 : (unavailable.isEmpty ? 0 : 2))
    }
}
