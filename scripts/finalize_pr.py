from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one anchor, found {count}: {old[:120]!r}")
    file.write_text(text.replace(old, new, 1))


def replace_section(path: str, start: str, end: str, replacement: str) -> None:
    file = Path(path)
    text = file.read_text()
    if text.count(start) != 1 or text.count(end) != 1:
        raise SystemExit(f"{path}: section anchors are not unique: {start!r}, {end!r}")
    left, right = text.index(start), text.index(end)
    if left >= right:
        raise SystemExit(f"{path}: section anchors are reversed")
    file.write_text(text[:left] + replacement + text[right:])


app = "macos/Application.swift"
replace_once(
    app,
    '        NSApp.activate(ignoringOtherApps: true)\n    }\n    func applicationShouldTerminateAfterLastWindowClosed',
    '''        NSApp.activate(ignoringOtherApps: true)\n        DispatchQueue.main.asyncAfter(deadline: .now() + 2) { [weak self] in self?.checkForUpdatesIfConfigured() }\n    }\n    private func checkForUpdatesIfConfigured() {\n        guard let feed = Bundle.main.url(forResource: "UpdateFeed", withExtension: "txt"),\n              let text = try? String(contentsOf: feed, encoding: .utf8).trimmingCharacters(in: .whitespacesAndNewlines),\n              let url = URL(string: text), url.scheme == "https" else { return }\n        UpdateManager.shared.check(manifestURL: url) { result in\n            guard case .success(.some(let manifest)) = result else { return }\n            let alert = NSAlert()\n            alert.messageText = L("app.name") + " " + manifest.version\n            alert.informativeText = manifest.url\n            alert.addButton(withTitle: L("action.continue"))\n            alert.addButton(withTitle: L("action.cancel"))\n            guard alert.runModal() == .alertFirstButtonReturn else { return }\n            UpdateManager.shared.download(manifest) { download in\n                switch download {\n                case .success(let package): NSWorkspace.shared.open(package)\n                case .failure(let error):\n                    let failure = NSAlert(); failure.alertStyle = .warning\n                    failure.messageText = L("error.operation_incomplete")\n                    failure.informativeText = error.localizedDescription\n                    failure.runModal()\n                }\n            }\n        }\n    }\n    func applicationShouldTerminateAfterLastWindowClosed''',
)
replace_once(
    app,
    '    var backgroundRequestID: String?\n    var backgroundTaskName: String?\n    var offlineListID: String?\n',
    '    var backgroundRequestID: String?\n    var backgroundTaskName: String?\n    var duplicateWindow: DuplicateResultsWindowController?\n    var offlineListID: String?\n',
)
replace_once(
    app,
    '''    func performSimple(_ action: String) {\n        guard canUseSelection, offlineListID == nil else { return }\n        SearchClient.shared.call(["op": "files", "action": action, "paths": selectedPaths]) { [weak self] reply in self?.checkResult(reply) }\n    }\n''',
    '''    func performSimple(_ action: String) {\n        guard offlineListID == nil else { return }\n        withResolvedSelection { paths in\n            SearchClient.shared.call(["op": "files", "action": action, "paths": paths]) { [weak self] reply in self?.checkResult(reply) }\n        }\n    }\n''',
)
replace_section(
    app,
    '    func confirmFileOperation(_ request: [String: Any], title: String) {',
    '    @objc func undoFileOperation',
    '''    func confirmFileOperation(_ request: [String: Any], title: String) {\n        guard offlineListID == nil else { return }\n        var previewRequest = request\n        previewRequest["dry_run"] = true\n        SearchClient.shared.call(previewRequest) { [weak self] reply in\n            guard let self else { return }\n            if reply["success"] as? Bool == false { self.checkResult(reply); return }\n            let preview = reply["preview"] as? [[String: Any]] ?? []\n            let conflicts = preview.filter { !self.stringList($0["conflicts"]).isEmpty }\n            let alert = NSAlert(); alert.messageText = title\n            let lines = preview.prefix(40).map { row -> String in\n                let source = URL(fileURLWithPath: row["source"] as? String ?? "").lastPathComponent\n                let destination = row["destination"] as? String ?? ""\n                let issues = self.stringList(row["conflicts"])\n                return issues.isEmpty ? "✓ \\(source) → \\(destination)" : "⚠︎ \\(source) → \\(destination)\\n  " + issues.joined(separator: " · ")\n            }\n            alert.informativeText = lines.joined(separator: "\\n")\n                + (preview.count > 40 ? "\\n" + L("files.additional_count", (preview.count - 40).formatted()) : "")\n            alert.addButton(withTitle: L("action.continue")); alert.addButton(withTitle: L("action.cancel"))\n            guard alert.runModal() == .alertFirstButtonReturn else { return }\n            var operation = request\n            if !conflicts.isEmpty { operation["conflict_policy"] = "skip" }\n            let id = self.beginBackground(title)\n            operation["request_id"] = id\n            SearchClient.shared.call(operation) { [weak self] result in\n                guard let self, self.backgroundRequestID == id else { return }\n                self.endBackground(); self.checkResult(result); self.pollStatus(); self.runQuery()\n            }\n        }\n    }\n''',
)
replace_section(
    app,
    '    @objc func indexContent(_ sender: Any?) {',
    '    func beginBackground',
    '''    @objc func indexContent(_ sender: Any?) {\n        guard offlineListID == nil else { return }\n        withResolvedSelection { paths in\n            let id = self.beginBackground(L("status.extracting_content"))\n            SearchClient.shared.call(["op": "content_index", "paths": paths, "request_id": id]) { [weak self] reply in\n                guard let self, self.backgroundRequestID == id else { return }\n                self.endBackground(); self.checkResult(reply, success: L("index.content_indexing_processed")); self.runQuery()\n            }\n        }\n    }\n''',
)
replace_section(
    app,
    '    @objc func findDuplicates(_ sender: Any?) {',
    '    func duplicateReport',
    '''    @objc func findDuplicates(_ sender: Any?) {\n        guard offlineListID == nil else { return }\n        let id = beginBackground(L("status.checking_duplicates"))\n        SearchClient.shared.call(["op": "duplicates", "mode": "content", "request_id": id]) { [weak self] reply in\n            guard let self, self.backgroundRequestID == id else { return }; self.endBackground()\n            if reply["success"] as? Bool == false { self.checkResult(reply); return }\n            let controller = DuplicateResultsWindowController(result: reply) { [weak self] paths in\n                guard let self, !paths.isEmpty else { return }\n                self.confirmFileOperation(\n                    ["op": "files", "action": "trash", "paths": paths, "conflict_policy": "skip"],\n                    title: L("files.trash_confirmation", paths.count.formatted()))\n            }\n            self.duplicateWindow = controller\n            controller.showWindow(nil); controller.window?.makeKeyAndOrderFront(nil)\n        }\n    }\n''',
)
replace_section(
    app,
    '    @objc func previewSelection(_ sender: Any?) {',
    '    override func acceptsPreviewPanelControl',
    '''    @objc func previewSelection(_ sender: Any?) {\n        guard offlineListID == nil else { return }\n        withResolvedSelection { paths in\n            self.previewURLs = paths.map { NSURL(fileURLWithPath: $0) }\n            if let panel = QLPreviewPanel.shared() {\n                panel.dataSource = self; panel.delegate = self; panel.reloadData()\n                if panel.isVisible { panel.orderOut(nil) } else { panel.makeKeyAndOrderFront(nil) }\n            }\n        }\n    }\n    func updatePreview() {\n        guard offlineListID == nil else { QLPreviewPanel.shared()?.orderOut(nil); return }\n        if selectionIsComplete { previewURLs = selectedPaths.map { NSURL(fileURLWithPath: $0) } }\n        QLPreviewPanel.shared()?.dataSource = self; QLPreviewPanel.shared()?.delegate = self; QLPreviewPanel.shared()?.reloadData()\n    }\n''',
)
replace_once(
    app,
    '''        let selectionActions: Set<Selector> = [#selector(openSelection), #selector(revealSelection), #selector(copyPaths), #selector(copySelection), #selector(moveSelection), #selector(trashSelection), #selector(indexContent), #selector(previewSelection)]\n        if let action = menuItem.action, selectionActions.contains(action) { return canUseSelection }\n        if menuItem.action == #selector(renameSelection) { return canUseSelection && selectedPaths.count == 1 }\n''',
    '''        let selectionActions: Set<Selector> = [#selector(openSelection), #selector(revealSelection), #selector(copyPaths), #selector(copySelection), #selector(moveSelection), #selector(trashSelection), #selector(indexContent), #selector(previewSelection), #selector(renameSelection)]\n        if let action = menuItem.action, selectionActions.contains(action) {\n            return resultsAreCurrent && !table.selectedRowIndexes.isEmpty\n        }\n''',
)

service = "macos/SearchService.swift"
replace_once(
    service,
    '''      let export = exportJobs[id]\n      jobLock.unlock()\n      export?.cancel()\n''',
    '''      let export = exportJobs[id]\n      jobLock.unlock()\n      export?.cancel()\n      files.cancel(id)\n''',
)

build = "build.sh"
replace_once(
    build,
    '''fi\nxattr -cr "$FILESEARCH_APP"\n''',
    '''fi\nif [[ -n "${FILESEARCH_UPDATE_MANIFEST_URL:-}" ]]; then\n  python3 - "$FILESEARCH_UPDATE_MANIFEST_URL" "$FILESEARCH_APP/Contents/Resources/UpdateFeed.txt" <<'PY'\nimport pathlib, sys, urllib.parse\nvalue, destination = sys.argv[1:]\nparsed = urllib.parse.urlparse(value)\nif parsed.scheme != "https" or not parsed.netloc:\n    raise SystemExit("FILESEARCH_UPDATE_MANIFEST_URL must be an absolute HTTPS URL")\npathlib.Path(destination).write_text(value + "\\n")\nPY\nfi\nxattr -cr "$FILESEARCH_APP"\n''',
)

release = ".github/workflows/release.yml"
replace_once(
    release,
    '''          FILESEARCH_UPDATE_PUBLIC_KEY: ${{ secrets.FILESEARCH_UPDATE_PUBLIC_KEY }}\n          FILESEARCH_UPDATE_PRIVATE_KEY: ${{ secrets.FILESEARCH_UPDATE_PRIVATE_KEY }}\n''',
    '''          FILESEARCH_UPDATE_PUBLIC_KEY: ${{ secrets.FILESEARCH_UPDATE_PUBLIC_KEY }}\n          FILESEARCH_UPDATE_PRIVATE_KEY: ${{ secrets.FILESEARCH_UPDATE_PRIVATE_KEY }}\n          FILESEARCH_UPDATE_MANIFEST_URL: https://github.com/${{ github.repository }}/releases/latest/download/update.json\n''',
)

runner = "tests/run_search_window_tests.py"
replace_once(
    runner,
    '''startup = """        pollStatus()\n        refreshShortcuts()\n        runQuery()\n        if offlineListID == nil { statusTimer = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in self?.pollStatus() } }\n"""\n''',
    '''startup = """        pollStatus()\n        refreshShortcuts()\n        runQuery()\n"""\n''',
)
replace_once(
    runner,
    '''["ApplicationIdentity.swift", "LegacyDataMigration.swift", "SearchProtocol.swift", "Localization.swift", "SettingsWindow.swift"]''',
    '''["ApplicationIdentity.swift", "LegacyDataMigration.swift", "SearchProtocol.swift", "Localization.swift", "DuplicateResultsWindow.swift", "UpdateManager.swift", "SettingsWindow.swift"]''',
)
replace_once(
    runner,
    '''for framework in ["AppKit", "SwiftUI", "ServiceManagement", "Quartz", "Carbon"]:''',
    '''for framework in ["AppKit", "SwiftUI", "ServiceManagement", "Quartz", "Carbon", "CryptoKit"]:''',
)

test_swift = Path("tests/SearchWindowTests.swift")
test_text = test_swift.read_text().replace("c.statusTimer?.invalidate(); ", "")
if "statusTimer" in test_text:
    raise SystemExit("tests/SearchWindowTests.swift: stale statusTimer reference remains")
test_swift.write_text(test_text)

# The final tree must contain the implementation, not build-time source patches.
print("Final source transformations applied")
