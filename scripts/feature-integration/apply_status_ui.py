from pathlib import Path

path = Path("macos/Application.swift")
text = path.read_text()

def replace_once(old: str, new: str) -> None:
    global text
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"expected exactly one status UI anchor, found {count}: {old[:120]!r}")
    text = text.replace(old, new, 1)

replace_once(
    "    var historyTimer: Timer?\n    var statusTimer: Timer?\n    var elapsed: Double = 0\n",
    "    var historyTimer: Timer?\n    let statusRequestID = UUID().uuidString\n    var elapsed: Double = 0\n",
)

replace_once(
    '''        pollStatus()\n        refreshShortcuts()\n        runQuery()\n        if offlineListID == nil { statusTimer = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in self?.pollStatus() } }\n    }\n    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }\n    deinit { progressDelay?.invalidate(); queryTimer?.invalidate(); historyTimer?.invalidate(); statusTimer?.invalidate(); liveScrollObservers.forEach { NotificationCenter.default.removeObserver($0) } }\n''',
    '''        pollStatus()\n        refreshShortcuts()\n        runQuery()\n    }\n    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }\n    deinit {\n        progressDelay?.invalidate(); queryTimer?.invalidate(); historyTimer?.invalidate()\n        if offlineListID == nil { SearchClient.shared.call(["op": "cancel", "request_id": statusRequestID]) { _ in } }\n        liveScrollObservers.forEach { NotificationCenter.default.removeObserver($0) }\n    }\n''',
)

replace_once(
    '''        statusRequestPending = true\n        SearchClient.shared.call(["op": "status"]) { [weak self] reply in\n            guard let self else { return }\n            self.statusRequestPending = false\n            self.currentStatus = reply\n''',
    '''        statusRequestPending = true\n        let revision = (currentStatus["status_revision"] as? NSNumber)?.uint64Value\n        let request: [String: Any] = revision.map {\n            ["op": "wait_status", "after": $0, "timeout_ms": 30_000, "request_id": statusRequestID]\n        } ?? ["op": "status"]\n        SearchClient.shared.call(request) { [weak self] reply in\n            guard let self else { return }\n            self.statusRequestPending = false\n            guard reply["success"] as? Bool != false else {\n                DispatchQueue.main.asyncAfter(deadline: .now() + 1) { [weak self] in self?.pollStatus() }\n                return\n            }\n            self.currentStatus = reply\n''',
)

replace_once(
    '''                    DispatchQueue.main.asyncAfter(deadline: .now() + 0.4) { [weak self] in self?.chooseVolumes(nil) }\n                }\n            }\n        }\n    }\n    func requestProgress''',
    '''                    DispatchQueue.main.asyncAfter(deadline: .now() + 0.4) { [weak self] in self?.chooseVolumes(nil) }\n                }\n            }\n            self.pollStatus()\n        }\n    }\n    func requestProgress''',
)

path.write_text(text)
