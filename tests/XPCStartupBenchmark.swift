import Foundation

/// Use a freshly restarted, separately signed fixture service. Both requests
/// share one real connection; no index mutation or history request is issued.
@main
struct XPCStartupBenchmark {
    static func main() throws {
        guard CommandLine.arguments.count == 4,
              UUID(uuidString: CommandLine.arguments[1]) != nil,
              let expectedCount = Int(CommandLine.arguments[2]), expectedCount > 0 else {
            fputs("Usage: XPCStartupBenchmark list-uuid expected-count output.json\n", stderr)
            exit(2)
        }
        let client = SearchClient()
        var initial: [String: Any]?
        client.call(["op": "status"]) { initial = $0 }
        let setupDeadline = ProcessInfo.processInfo.systemUptime + 30
        while initial == nil && ProcessInfo.processInfo.systemUptime < setupDeadline {
            RunLoop.current.run(until: Date().addingTimeInterval(0.001))
        }
        guard initial?["success"] as? Bool == true else {
            fputs("Could not connect to the fixture service\n", stderr); exit(1)
        }
        let started = ProcessInfo.processInfo.systemUptime
        var loaded: [String: Any]?, live: [String: Any]?
        var loadedMilliseconds: Double?, liveMilliseconds: Double?
        client.call(["op": "status", "list_id": CommandLine.arguments[1]]) {
            loadedMilliseconds = (ProcessInfo.processInfo.systemUptime - started) * 1000
            loaded = $0
        }
        client.call(["op": "status"]) {
            liveMilliseconds = (ProcessInfo.processInfo.systemUptime - started) * 1000
            live = $0
        }
        while (loaded == nil || live == nil) && ProcessInfo.processInfo.systemUptime - started < 60 {
            RunLoop.current.run(until: Date().addingTimeInterval(0.001))
        }
        let success = loaded?["success"] as? Bool == true
            && loaded?["count"] as? Int == expectedCount
            && live?["success"] as? Bool == true
            && live?["scanning"] as? Bool == false
        let report: [String: Any] = [
            "success": success,
            "scope": "Persistent authenticated XPC connection; first imported-index status overlaps an unrelated live-index status; no AppKit rendering; fixture service must be freshly restarted",
            "loaded_count": loaded?["count"] ?? NSNull(),
            "load_ms": loadedMilliseconds as Any? ?? NSNull(),
            "unrelated_status_ms": liveMilliseconds as Any? ?? NSNull(),
            "unrelated_status_preceded_load": liveMilliseconds != nil && loadedMilliseconds != nil && liveMilliseconds! < loadedMilliseconds!,
            "timed_out": loaded == nil || live == nil,
        ]
        try jsonData(report).write(to: URL(fileURLWithPath: CommandLine.arguments[3]), options: .atomic)
        print(String(decoding: jsonData(report), as: UTF8.self))
        exit(success ? 0 : 1)
    }
}
