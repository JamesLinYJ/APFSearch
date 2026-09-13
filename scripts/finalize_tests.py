from pathlib import Path

path = Path("tests/SearchWindowTests.swift")
text = path.read_text()

for stale in ["c.statusTimer?.invalidate()", "controller.statusTimer?.invalidate()"]:
    text = text.replace(stale, "")

old = '''        SearchClient.shared.clear()
        let selectedCount = large.table.selectedRowIndexes.count
        check("partially_cached_selection_is_not_actionable", !large.selectionIsComplete && !large.canUseSelection, ["selected_count": selectedCount, "loaded_selected_paths": large.selectedPaths.count])
        large.openSelection(nil); large.trashSelection(nil); large.indexContent(nil)
        check("partial_selection_file_actions_send_no_operation", SearchClient.shared.pending.isEmpty, ["operation_count": SearchClient.shared.pending.count])
        let loadedRow = large.cachedRows.keys.min()!
        check("partial_selection_cannot_start_file_drag", large.tableView(large.table, pasteboardWriterForRow: loadedRow) == nil)
'''
new = '''        SearchClient.shared.clear()
        let selectedCount = large.table.selectedRowIndexes.count
        large.snapshotLease = "cross-page-selection-lease"
        check("partially_cached_selection_uses_snapshot_resolution", !large.selectionIsComplete && !large.canUseSelection, ["selected_count": selectedCount, "loaded_selected_paths": large.selectedPaths.count])
        let actionItem = NSMenuItem(title: "Open", action: #selector(SearchWindowController.openSelection), keyEquivalent: "")
        check("cross_page_selection_action_stays_enabled", large.validateMenuItem(actionItem))

        // The visible cache is intentionally incomplete. Production must fetch
        // only the missing selected pages against the retained snapshot before
        // sending a file operation. Never silently operate on selectedPaths.
        let allRows = [row(-1)] + largeRows
        large.openSelection(nil)
        var resolutionRequests = 0
        var everyResolutionWasPinned = true
        while let resolution = SearchClient.shared.takeQuery() {
            resolutionRequests += 1
            everyResolutionWasPinned = everyResolutionWasPinned
                && resolution.request["snapshot_lease"] as? String == "cross-page-selection-lease"
            let offset = resolution.request["offset"] as? Int ?? 0
            let limit = resolution.request["limit"] as? Int ?? 1000
            let upper = min(allRows.count, offset + limit)
            resolution.completion(reply(Array(allRows[offset..<upper]), count: allRows.count, generation: 3, offset: offset))
        }
        let operation = SearchClient.shared.take("files")
        let operationPaths = operation?.request["paths"] as? [String] ?? []
        check("cross_page_file_action_resolves_every_selected_path", resolutionRequests > 0 && everyResolutionWasPinned && operationPaths.count == selectedCount, ["selected_count": selectedCount, "resolved_count": operationPaths.count, "resolution_requests": resolutionRequests, "snapshot_pinned": everyResolutionWasPinned])
        operation?.completion(["success": true])
        SearchClient.shared.clear()
        let loadedRow = large.cachedRows.keys.min()!
        check("partial_selection_cannot_start_file_drag", large.tableView(large.table, pasteboardWriterForRow: loadedRow) == nil)
'''
if text.count(old) != 1:
    raise SystemExit("expected exactly one stale partial-selection regression block")
text = text.replace(old, new, 1)

if "statusTimer" in text:
    raise SystemExit("stale statusTimer reference remains after migration")
text = "\n".join(line.rstrip() for line in text.splitlines()) + "\n"
path.write_text(text)
print("AppKit tests migrated to event-driven status and cross-page actions")
