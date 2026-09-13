import AppKit
import CryptoKit

/// Optional, test-only main-thread timestamps. The builder inserts calls into
/// a temporary controller copy; production source and transport stay unchanged.
enum WindowBenchmarkPhases {
#if BENCHMARK_PHASE_TIMING
    static let enabled = true
#else
    static let enabled = false
#endif
    private static var origin: TimeInterval = 0
    private static var marks: [String: Double] = [:]
    static func begin(at started: TimeInterval) {
        guard enabled else { return }
        origin = started; marks.removeAll(keepingCapacity: true)
    }
    static func mark(_ name: String) {
        guard enabled else { return }
        marks[name] = (ProcessInfo.processInfo.systemUptime - origin) * 1000
    }
    static func snapshot() -> [String: Double] { marks }
}

/// Production AppKit controller and SearchClient; only application startup is
/// isolated by the builder. No transport replies or result rows are substituted.
@main
@MainActor
enum RuntimeWindowBenchmark {
    static let client = SearchClient.shared
    static func pause(_ seconds: Double = 0.001) async {
        try? await Task.sleep(nanoseconds: UInt64(seconds * 1_000_000_000))
    }
    static func request(_ value: [String: Any], timeout: Double) async -> [String: Any] {
        var result: [String: Any]?
        let started = ProcessInfo.processInfo.systemUptime
        client.call(value) { result = $0 }
        while result == nil && ProcessInfo.processInfo.systemUptime - started < timeout { await pause() }
        return result ?? ["success": false, "timed_out": true, "elapsed_lower_bound_ms": (ProcessInfo.processInfo.systemUptime - started) * 1000]
    }
    static func digest(_ value: Any) -> String {
        let data = try! JSONSerialization.data(withJSONObject: value, options: [.sortedKeys, .withoutEscapingSlashes])
        return SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
    }
    static func binary(_ url: URL) -> [String: Any] {
        guard let data = try? Data(contentsOf: url) else { return ["path": url.path, "unavailable": true] }
        var result: [String: Any] = ["path": url.path, "sha256": SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()]
        let process = Process(), pipe = Pipe()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/codesign")
        process.arguments = ["-d", "--verbose=4", url.path]
        process.standardError = pipe; process.standardOutput = pipe
        do {
            try process.run()
            result["signature"] = String(decoding: pipe.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
            process.waitUntilExit(); result["signature_inspection_exit_code"] = process.terminationStatus
        } catch { result["signature_error"] = error.localizedDescription }
        return result
    }
    static func percentile(_ values: [Double], _ quantile: Double) -> Double? {
        guard !values.isEmpty else { return nil }
        let sorted = values.sorted()
        return sorted[max(0, Int(ceil(Double(sorted.count) * quantile)) - 1)]
    }
    static func main() {
        let app = NSApplication.shared
        app.setActivationPolicy(.accessory)
        Timer.scheduledTimer(withTimeInterval: 0.001, repeats: false) { _ in
            Task { @MainActor in await run() }
        }
        app.run()
    }
    static func run() async {
        if CommandLine.arguments.contains("--self-test") {
            var checks = [[String: Any]]()
            func check(_ name: String, _ passed: Bool) { checks.append(["test": name, "passed": passed]) }
            check("nearest_rank_p95_includes_29th_of_30", percentile(Array(1...30).map(Double.init), 0.95) == 29)
            check("empty_samples_have_no_percentile", percentile([], 0.95) == nil)
            check("actual_display_available", !NSScreen.screens.isEmpty)
            let controller = SearchWindowController(offlineListID: UUID().uuidString)
            check("startup_isolated_without_requests", controller.requestIDs.isEmpty && controller.queryTimer == nil && controller.historyTimer == nil && !controller.statusRequestPending && controller.statusRetry == nil)
            controller.window?.makeKeyAndOrderFront(nil)
            controller.window?.contentView?.layoutSubtreeIfNeeded(); controller.table.displayIfNeeded()
            check("actual_appkit_window_is_visible", controller.window?.isVisible == true)
            controller.search.stringValue = "fixture"
            WindowBenchmarkPhases.begin(at: ProcessInfo.processInfo.systemUptime)
            controller.controlTextDidChange(Notification(name: NSControl.textDidChangeNotification, object: controller.search))
            if WindowBenchmarkPhases.enabled {
                let phase = WindowBenchmarkPhases.snapshot()["input_handler_enter"]
                check("optional_phase_hook_runs_on_production_input", phase != nil && phase! >= 0)
            }
            check("production_input_starts_debounce", controller.pendingInputStartedAt != nil && controller.queryTimer?.isValid == true && controller.requestIDs.isEmpty)
            // Stay on the main actor and cancel before the run loop can fire it:
            // this preflight never sends a transport request to the stopped agent.
            controller.queryTimer?.invalidate(); controller.historyTimer?.invalidate(); controller.window?.orderOut(nil)
            let passed = checks.allSatisfy { $0["passed"] as? Bool == true }
            print(String(decoding: jsonData(["success": passed, "screen_count": NSScreen.screens.count, "tests": checks, "boundary": "No XPC calls: AppKit/configuration preflight only"]), as: UTF8.self))
            exit(passed ? 0 : 1)
        }
        guard CommandLine.arguments.count >= 3,
              let data = try? Data(contentsOf: URL(fileURLWithPath: CommandLine.arguments[1])) else {
            fputs("Usage: RuntimeWindowBenchmark config.json output.json\n", stderr); exit(2)
        }
        let config = jsonObject(data)
        let output = URL(fileURLWithPath: CommandLine.arguments[2])
        let runs = config["runs"] as? Int ?? 30, warmups = config["warmups"] as? Int ?? 2
        let timeout = (config["timeout"] as? NSNumber)?.doubleValue ?? 10
        let setupTimeout = (config["setup_timeout"] as? NSNumber)?.doubleValue ?? 180
        let specifications = config["queries"] as? [[String: Any]] ?? []
        guard let listID = config["list_id"] as? String, UUID(uuidString: listID) != nil,
              (1...1000).contains(runs), (0...100).contains(warmups), timeout > 0, timeout <= 120,
              setupTimeout >= timeout, setupTimeout <= 600, !specifications.isEmpty else {
            fputs("Invalid configuration: a fixed offline list and bounded runs/timeouts are required.\n", stderr); exit(2)
        }
        let appURL = URL(fileURLWithPath: config["application_bundle"] as? String ?? "/Applications/APFSearch.app")
        var report: [String: Any] = [
            "schema_version": 1, "configuration": config, "started_at_utc": ISO8601DateFormatter().string(from: Date()),
            "boundary": "Programmatic controlTextDidChange on production SearchWindowController -> production 16ms debounce -> unmodified SearchClient and authenticated real Mach XPC -> AppKit layout/table.displayIfNeeded submission. Includes controller/table work; excludes physical keyboard, IME composition and compositor presentation.",
            "screen_count": NSScreen.screens.count,
            "screens": NSScreen.screens.map { ["name": $0.localizedName, "scale": $0.backingScaleFactor, "width": $0.frame.width, "height": $0.frame.height] as [String: Any] },
            "defaults_domain": Bundle.main.bundleIdentifier ?? "", "reduce_motion_enabled": NSWorkspace.shared.accessibilityDisplayShouldReduceMotion,
            "binary_context": ["benchmark": binary(URL(fileURLWithPath: CommandLine.arguments[0])), "application": binary(appURL.appendingPathComponent("Contents/MacOS/APFSearch")), "service": binary(appURL.appendingPathComponent("Contents/MacOS/APFSearchService"))],
            "queries": [], "success": false, "complete": false,
            "timeout_policy": "Keep every timeout as a lower bound, cancel only this controller's unique request IDs, and abort further measurements to prevent overlapping heavy work.",
            "history_policy": "Invalidate the production history timer immediately after every reply; confirm the actual service history is identical before and after. No preferences set requests are issued by the driver."
        ]
        if let receiptURL = Bundle.main.url(forResource: "build-receipt", withExtension: "json"), let receipt = try? Data(contentsOf: receiptURL) { report["build_receipt"] = jsonObject(receipt) }
        func save() { try? jsonData(report).write(to: output, options: .atomic) }
        guard !NSScreen.screens.isEmpty else { report["failure"] = "No display is available; AppKit geometry/render submission cannot be accepted."; save(); exit(2) }
        let beforePreferences = await request(["op": "preferences", "action": "get"], timeout: setupTimeout)
        guard beforePreferences["success"] as? Bool == true else { report["failure"] = ["stage": "history_before", "response": beforePreferences]; save(); exit(1) }
        let beforeValues = beforePreferences["values"] as? [String: Any] ?? beforePreferences
        let beforeHistory = beforeValues["history"] ?? []
        report["history_before_sha256"] = digest(beforeHistory)
        report["live_status_before"] = await request(["op": "status"], timeout: setupTimeout)
        let datasetStatus = await request(["op": "status", "list_id": listID], timeout: setupTimeout)
        report["dataset_status_before"] = datasetStatus
        guard datasetStatus["success"] as? Bool == true, let expectedGeneration = datasetStatus["generation"] as? NSNumber else {
            report["failure"] = ["stage": "dataset_before", "response": datasetStatus]; save(); exit(1)
        }
        let constructionStarted = ProcessInfo.processInfo.systemUptime
        let controller = SearchWindowController(offlineListID: listID, count: (datasetStatus["count"] as? NSNumber)?.intValue ?? 0)
        if WindowBenchmarkPhases.enabled {
            report["controller_construction_ms"] = (ProcessInfo.processInfo.systemUptime - constructionStarted) * 1000
            report["phase_timing_boundary"] = "Optional diagnostic hooks in a temporary controller copy record input/timer, request dispatch/reply delivery, row application, layout and display submission. SearchClient is unchanged. Dispatch-to-reply includes transport, service work, main-queue scheduling and JSON decoding. Hook overhead remains in measured samples; keep diagnostic runs separate from uninstrumented acceptance runs."
        }
        controller.window?.title = "APFSearch performance validation"
        controller.window?.setFrame(NSRect(x: 100, y: 100, width: 1150, height: 740), display: false)
        controller.table.autosaveTableColumns = false
        var results = [[String: Any]](), aborted = false, stable = true, targetsPassed = true
        for specification in specifications {
            let name = specification["name"] as? String ?? "query"
            if aborted { results.append(["name": name, "not_run": true, "reason": "Previous query timed out or failed"]); continue }
            let text = specification["text"] as? String ?? ""
            let sort = specification["sort"] as? [[String: Any]] ?? config["sort"] as? [[String: Any]] ?? [["field": "name", "ascending": true]]
            controller.historyTimer?.invalidate()
            // Configure sorting while hidden, using the production method's
            // existing visibility guard to avoid an extra untimed query.
            let preparationStarted = ProcessInfo.processInfo.systemUptime
            controller.window?.orderOut(nil)
            controller.table.sortDescriptors = sort.map { NSSortDescriptor(key: $0["field"] as? String ?? "name", ascending: $0["ascending"] as? Bool ?? true) }
            controller.window?.makeKeyAndOrderFront(nil)
            controller.window?.contentView?.layoutSubtreeIfNeeded(); controller.table.displayIfNeeded()
            let preparationMs = (ProcessInfo.processInfo.systemUptime - preparationStarted) * 1000
            let settlingStarted = ProcessInfo.processInfo.systemUptime
            await pause(0.05)
            let settlingMs = (ProcessInfo.processInfo.systemUptime - settlingStarted) * 1000
            var samples = [[String: Any]](), referenceDigest: String?, referenceTotal: Int?
            for run in 0..<(warmups + runs) {
                controller.historyTimer?.invalidate()
                controller.search.stringValue = text
                let started = ProcessInfo.processInfo.systemUptime
                let sequence = controller.querySequence
                WindowBenchmarkPhases.begin(at: started)
                controller.controlTextDidChange(Notification(name: NSControl.textDidChangeNotification, object: controller.search))
                while ProcessInfo.processInfo.systemUptime - started < timeout {
                    if controller.querySequence >= sequence + 2, !controller.queryPending, controller.pendingInputStartedAt == nil { break }
                    await pause()
                }
                // The callback schedules this timer only after display submission.
                controller.historyTimer?.invalidate()
                let timedOut = controller.queryPending || controller.pendingInputStartedAt != nil || controller.querySequence < sequence + 2
                var sample: [String: Any] = ["run": run, "warmup": run < warmups, "timed_out": timedOut, "input_event": controller.queryStartedFromInput]
                if timedOut {
                    sample["end_to_end_lower_bound_ms"] = (ProcessInfo.processInfo.systemUptime - started) * 1000
                    sample["cancelled_request_ids"] = Array(controller.requestIDs)
                    controller.queryTimer?.invalidate(); controller.cancelQueries(); controller.querySequence += 1
                    sample["success"] = false; samples.append(sample); aborted = true; stable = false; targetsPassed = false; break
                }
                let phaseMarks = WindowBenchmarkPhases.snapshot()
                let rows = (0..<min(controller.pageSize, controller.total)).compactMap { controller.cachedRows[$0] }
                let pageDigest = digest(rows)
                let generationMatches = (controller.generation as? NSNumber) == expectedGeneration
                let totalMatches = specification["expected_total"].map { ($0 as? NSNumber)?.intValue == controller.total } ?? true
                let successful = controller.resultsAreCurrent && controller.queryWarnings.isEmpty && generationMatches && totalMatches && controller.queryStartedFromInput
                sample.merge(["success": successful, "end_to_end_ms": controller.elapsed, "observer_ms": (ProcessInfo.processInfo.systemUptime - started) * 1000,
                              "core_ms": controller.coreElapsed, "total": controller.total, "generation": controller.generation ?? NSNull(), "generation_matches_fixed_dataset": generationMatches,
                              "expected_total_matches": totalMatches, "row_count": rows.count, "page_sha256": pageDigest, "window_visible": controller.window?.isVisible == true,
                              "window_occlusion_visible": controller.window?.occlusionState.contains(.visible) == true, "warnings": controller.queryWarnings]) { _, new in new }
                if WindowBenchmarkPhases.enabled {
                    sample["phase_marks_ms"] = phaseMarks
                    sample["window_preparation_ms"] = preparationMs
                    sample["window_settling_ms"] = settlingMs
                }
                if let referenceDigest, let referenceTotal {
                    if referenceDigest != pageDigest || referenceTotal != controller.total { stable = false }
                } else { referenceDigest = pageDigest; referenceTotal = controller.total }
                samples.append(sample)
                if !successful { stable = false; aborted = true; break }
                await pause(0.005)
            }
            let measured = samples.filter { $0["warmup"] as? Bool == false }
            let latencies = measured.compactMap { ($0["end_to_end_ms"] as? NSNumber)?.doubleValue }
            let p95 = percentile(latencies, 0.95)
            let complete = measured.count == runs && measured.allSatisfy { $0["success"] as? Bool == true }
            let targetPassed = complete && (p95 ?? .infinity) <= 100
            targetsPassed = targetsPassed && targetPassed
            results.append(["name": name, "text": text, "sort": sort, "samples": samples, "complete": complete,
                            "statistics": ["measured_samples": measured.count, "p50_ms": percentile(latencies, 0.5).map { $0 as Any } ?? NSNull(), "p95_ms": complete ? (p95.map { $0 as Any } ?? NSNull()) : NSNull(), "completed_samples_p95_ms": p95.map { $0 as Any } ?? NSNull(), "max_ms": latencies.max().map { $0 as Any } ?? NSNull(), "p95_at_most_100ms": targetPassed]])
            report["queries"] = results; save()
        }
        controller.historyTimer?.invalidate(); controller.queryTimer?.invalidate(); controller.stopStatusObservation(); controller.cancelQueries(); controller.window?.orderOut(nil)
        let afterPreferences = await request(["op": "preferences", "action": "get"], timeout: setupTimeout)
        let afterValues = afterPreferences["values"] as? [String: Any] ?? afterPreferences
        let afterHistory = afterValues["history"] ?? []
        let historyUnchanged = afterPreferences["success"] as? Bool == true && digest(beforeHistory) == digest(afterHistory)
        report["history_after_sha256"] = digest(afterHistory); report["history_unchanged"] = historyUnchanged
        report["dataset_status_after"] = await request(["op": "status", "list_id": listID], timeout: setupTimeout)
        report["live_status_after"] = await request(["op": "status"], timeout: setupTimeout)
        report["queries"] = results; report["complete"] = !aborted; report["page_results_stable"] = stable
        report["latency_target_passed"] = targetsPassed; report["success"] = !aborted && stable && historyUnchanged
        report["finished_at_utc"] = ISO8601DateFormatter().string(from: Date()); save()
        print(String(decoding: jsonData(report), as: UTF8.self)); exit(!aborted && stable && historyUnchanged ? 0 : 1)
    }
}
