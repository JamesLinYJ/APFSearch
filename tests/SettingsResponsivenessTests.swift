import AppKit

// Isolated in-memory settings replies. No service registration, user preferences,
// index writes, or filesystem fixtures are performed by this harness.
final class SearchClient {
    static let shared = SearchClient()
    func call(_ request: [String: Any], completion: @escaping ([String: Any]) -> Void) {
        let response: [String: Any] = request["op"] as? String == "status"
            ? ["roots": ["/Fixture"], "count": 1_000_000, "scanning": false,
               "uncovered": (0..<888).map { "/Fixture/Library/Application Support/未覆盖文件夹/\($0)/Data" }]
            : ["success": true, "values": ["history": [String]()]]
        DispatchQueue.main.async { completion(response) }
    }
}

@main @MainActor enum SettingsResponsivenessTests {
    static func descendants(_ view: NSView) -> [NSView] { [view] + view.subviews.flatMap(descendants) }
    static func settle() async { try? await Task.sleep(nanoseconds: 40_000_000) }
    static func main() {
        let app = NSApplication.shared
        app.setActivationPolicy(.accessory)
        Task { @MainActor in await run() }
        app.run()
    }
    static func run() async {
        var measurements = [[String: Any]]()
        for trial in 0..<5 {
            let controller = makeSearchSettingsWindow()
            let window = controller.window!
            window.makeKeyAndOrderFront(nil)
            await settle()
            guard let tabs = descendants(window.contentView!).compactMap({ $0 as? NSTabView }).first else {
                fatalError("The real settings tab view was not created")
            }
            let start = ProcessInfo.processInfo.systemUptime
            tabs.selectTabViewItem(at: 1)
            window.contentView?.layoutSubtreeIfNeeded()
            window.displayIfNeeded()
            let switchTime = (ProcessInfo.processInfo.systemUptime - start) * 1000
            await settle()
            let views = descendants(window.contentView!)
            let textViews = views.filter { $0 is NSTextView }.count
            measurements.append(["trial": trial, "switch_and_display_ms": switchTime,
                "native_view_count": views.count, "text_view_count": textViews])
            window.orderOut(nil)
        }
        let values = measurements.compactMap { $0["switch_and_display_ms"] as? Double }.sorted()
        print(String(decoding: jsonData(["samples": measurements, "median_ms": values[values.count / 2],
            "max_ms": values.last!, "fixture_rows": 888, "screen_count": NSScreen.screens.count,
            "boundary": "Actual SwiftUI settings hosted in AppKit. Tab action through layout and display submission; excludes compositor and human input latency. In-memory transport only."]), as: UTF8.self))
        exit(0)
    }
}
