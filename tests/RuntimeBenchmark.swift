import Foundation
import CryptoKit

/// Serial, authenticated XPC queries against a retained online or offline snapshot.
/// Deadlines cancel only this benchmark's request ID. A censored sample remains
/// in the report; an unresponsive cancelled query stops further heavy requests.
@main struct RuntimeBenchmark {
    final class Pending {
        let started = ProcessInfo.processInfo.systemUptime
        var response: [String: Any]?
        var completed: TimeInterval?
    }
    static let client = SearchClient()
    static func start(_ request: [String: Any]) -> Pending {
        let pending = Pending()
        client.call(request) { response in
            pending.completed = ProcessInfo.processInfo.systemUptime
            pending.response = response
        }
        return pending
    }
    @discardableResult static func wait(_ pending: Pending, seconds: Double) -> Bool {
        let deadline = ProcessInfo.processInfo.systemUptime + seconds
        while pending.response == nil && ProcessInfo.processInfo.systemUptime < deadline {
            RunLoop.current.run(until: Date().addingTimeInterval(0.001))
        }
        return pending.response != nil
    }
    static func elapsed(_ pending: Pending) -> Double {
        ((pending.completed ?? ProcessInfo.processInfo.systemUptime) - pending.started) * 1000
    }
    static func digest(_ value: Any) throws -> String {
        let data = try JSONSerialization.data(withJSONObject: value, options: [.sortedKeys, .withoutEscapingSlashes])
        return SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
    }
    static func executableContext(_ url: URL) -> [String: Any] {
        var result: [String: Any] = ["path": url.path]
        do {
            result["sha256"] = SHA256.hash(data: try Data(contentsOf: url)).map { String(format: "%02x", $0) }.joined()
            let process = Process(), pipe = Pipe()
            process.executableURL = URL(fileURLWithPath: "/usr/bin/codesign")
            process.arguments = ["-d", "--verbose=4", url.path]
            process.standardError = pipe; process.standardOutput = pipe
            try process.run()
            let signature = pipe.fileHandleForReading.readDataToEndOfFile()
            process.waitUntilExit()
            result["signature"] = String(decoding: signature, as: UTF8.self)
            result["signature_inspection_exit_code"] = process.terminationStatus
        } catch { result["context_error"] = error.localizedDescription }
        return result
    }
    static func scoped(_ request: [String: Any], listID: String?) -> [String: Any] {
        var value = request
        if let listID { value["list_id"] = listID }
        return value
    }
    static func status(_ listID: String?, timeout: Double) -> [String: Any] {
        let pending = start(scoped(["op": "status"], listID: listID))
        guard wait(pending, seconds: timeout) else { return ["timed_out": true, "elapsed_lower_bound_ms": elapsed(pending)] }
        let response = pending.response ?? [:]
        var selected = response.filter { ["success", "protocol_version", "generation", "count", "scanning", "state", "scan_phase", "scan_processed_entries", "error", "last_event_id", "roots"].contains($0.key) }
        selected["uncovered_count"] = (response["uncovered"] as? [Any])?.count ?? 0
        selected["xpc_ms"] = elapsed(pending)
        return selected
    }
    static func main() throws {
        guard CommandLine.arguments.count >= 3 else {
            FileHandle.standardError.write(Data("Usage: RuntimeBenchmark config.json output.json\n".utf8)); exit(2)
        }
        let configurationURL = URL(fileURLWithPath: CommandLine.arguments[1])
        let outputURL = URL(fileURLWithPath: CommandLine.arguments[2])
        let configuration = jsonObject(try Data(contentsOf: configurationURL))
        let runs = configuration["runs"] as? Int ?? 1
        let warmups = configuration["warmups"] as? Int ?? 0
        let timeout = (configuration["timeout"] as? NSNumber)?.doubleValue ?? 10
        let setupTimeout = (configuration["setup_timeout"] as? NSNumber)?.doubleValue ?? max(timeout, 120)
        let pageSize = configuration["page_size"] as? Int ?? 200
        let pageCount = configuration["pages"] as? Int ?? 1
        let listID = configuration["list_id"] as? String
        let specifications = configuration["queries"] as? [[String: Any]] ?? []
        guard (1...1000).contains(runs), (0...100).contains(warmups), timeout > 0, timeout <= 120, setupTimeout >= timeout, setupTimeout <= 600,
              (1...1000).contains(pageSize), (1...100).contains(pageCount), !specifications.isEmpty,
              listID == nil || UUID(uuidString: listID!) != nil else {
            throw NSError(domain: "RuntimeBenchmark", code: 1, userInfo: [NSLocalizedDescriptionKey: "Invalid benchmark configuration"])
        }
        let application = URL(fileURLWithPath: configuration["application_bundle"] as? String ?? "/Applications/APFSearch.app")
        let executableDirectory = application.appendingPathComponent("Contents/MacOS")
        var report: [String: Any] = [
            "schema_version": 1, "configuration": configuration, "started_at_utc": ISO8601DateFormatter().string(from: Date()),
            "boundary": "Signed SearchClient through actual Mach XPC until decoded reply. Includes engine and transport; excludes process launch, AppKit and compositor rendering.",
            "dataset": listID == nil ? "live local index; before/after generations may differ" : "fixed imported offline list",
            "list_id": listID ?? NSNull(), "timeout_policy": "Cancel unique request ID; retain timed-out samples as lower bounds. Never replay writes or silently discard slow samples.",
            "queries": [], "complete": false, "success": false,
            "binary_context": ["benchmark": executableContext(URL(fileURLWithPath: CommandLine.arguments[0])),
                               "application": executableContext(executableDirectory.appendingPathComponent("APFSearch")),
                               "service": executableContext(executableDirectory.appendingPathComponent("FileSearchService")),
                               "cli": executableContext(executableDirectory.appendingPathComponent("filesearch-cli"))],
            "os_version": ProcessInfo.processInfo.operatingSystemVersionString,
        ]
        func save() throws { try jsonData(report).write(to: outputURL, options: .atomic) }
        report["live_status_before"] = status(nil, timeout: setupTimeout)
        let datasetStatus = status(listID, timeout: setupTimeout)
        report["dataset_status_before"] = datasetStatus
        guard datasetStatus["success"] as? Bool == true else {
            report["failure"] = ["stage": "prepare_dataset", "detail": datasetStatus]
            report["lease_allocated"] = false
            try save(); exit(1)
        }
        let retained = start(scoped(["op": "retain_snapshot"], listID: listID))
        guard wait(retained, seconds: setupTimeout), let response = retained.response,
              response["success"] as? Bool == true, let lease = response["snapshot_lease"] as? String else {
            report["failure"] = retained.response ?? ["timed_out": true, "stage": "retain_snapshot", "elapsed_lower_bound_ms": elapsed(retained)]
            // If allocation finishes late, release it before leaving. No writes
            // are retried and no online lease is used for an offline query.
            if wait(retained, seconds: setupTimeout), let lease = retained.response?["snapshot_lease"] as? String {
                let release = start(scoped(["op": "release_snapshot", "snapshot_lease": lease], listID: listID))
                _ = wait(release, seconds: timeout); report["release"] = release.response ?? ["timed_out": true]
            }
            try save(); exit(1)
        }
        var releaseAttempted = false
        defer {
            if !releaseAttempted {
                let cleanup = start(scoped(["op": "release_snapshot", "snapshot_lease": lease], listID: listID))
                _ = wait(cleanup, seconds: timeout)
            }
        }
        report["snapshot"] = ["generation": response["generation"] ?? NSNull(), "retain_xpc_ms": elapsed(retained), "scope": "one lease shared by all measured pages and runs in this report"]
        var results = [[String: Any]](), abort = false, allStable = true, allTargets = true
        for specification in specifications {
            let name = specification["name"] as? String ?? specification["text"] as? String ?? "query"
            if abort { results.append(["name": name, "not_run": true, "reason": "Previous timed-out query did not drain after cancellation"]); continue }
            let text = specification["text"] as? String ?? specification["query"] as? String ?? ""
            let sort = specification["sort"] as? [[String: Any]] ?? configuration["sort"] as? [[String: Any]] ?? [["field": "name", "ascending": true]]
            let expectedTotal = specification["expected_total"] as? Int
            var samples = [[String: Any]](), referenceDigest: String?, referenceTotal: Int?
            for run in 0..<(warmups + runs) {
                var sample: [String: Any] = ["run": run, "warmup": run < warmups, "timed_out": false]
                var pages = [[String: Any]](), complete = true
                for page in 0..<pageCount {
                    let requestID = "runtime-benchmark-" + UUID().uuidString
                    let pending = start(scoped(["op": "query", "text": text, "sort": sort, "limit": pageSize,
                                                "offset": page * pageSize, "snapshot_lease": lease,
                                                "generation": response["generation"] ?? NSNull(), "request_id": requestID], listID: listID))
                    if !wait(pending, seconds: timeout) {
                        var timeoutPage: [String: Any] = ["offset": page * pageSize, "request_id": requestID, "timed_out": true, "xpc_elapsed_lower_bound_ms": elapsed(pending)]
                        let cancellation = start(scoped(["op": "cancel", "request_id": requestID], listID: listID))
                        _ = wait(cancellation, seconds: min(timeout, 2))
                        timeoutPage["cancel_response"] = cancellation.response ?? ["timed_out": true]
                        let drained = wait(pending, seconds: min(timeout, 2))
                        timeoutPage["drained_after_cancel"] = drained
                        if let completion = pending.completed { timeoutPage["terminal_reply_after_ms"] = (completion - pending.started) * 1000 }
                        if let terminal = pending.response { timeoutPage["terminal_reply"] = terminal.filter { ["success", "error", "error_key"].contains($0.key) } }
                        pages.append(timeoutPage); sample["timed_out"] = true
                        complete = false; if !drained { abort = true }; break
                    }
                    let reply = pending.response ?? [:]
                    let rows = reply["rows"] as? [[String: Any]] ?? []
                    var result: [String: Any] = ["offset": page * pageSize, "request_id": requestID, "success": reply["success"] ?? false,
                                               "xpc_ms": elapsed(pending), "core_ms": reply["elapsed_ms"] ?? NSNull(),
                                               "total": reply["total"] ?? NSNull(), "generation": reply["generation"] ?? NSNull(),
                                               "row_count": rows.count, "page_sha256": try digest(rows)]
                    if let error = reply["error"] { result["error"] = error }
                    let identityMatches = (reply["generation"] as? NSNumber) == (response["generation"] as? NSNumber)
                    result["generation_matches_lease"] = identityMatches
                    pages.append(result)
                    if reply["success"] as? Bool != true || !identityMatches { complete = false; break }
                    if rows.count < pageSize { break }
                }
                sample["pages"] = pages; sample["complete"] = complete
                if complete {
                    let total = pages.first?["total"] as? Int
                    let pageDigests = pages.compactMap { $0["page_sha256"] as? String }
                    let combined = try digest(pageDigests)
                    if referenceDigest == nil { referenceDigest = combined; referenceTotal = total }
                    let stable = combined == referenceDigest && total == referenceTotal
                    sample["stable_within_snapshot"] = stable
                    sample["expected_total_matches"] = expectedTotal.map { total == $0 } ?? NSNull()
                    allStable = allStable && stable && (expectedTotal == nil || total == expectedTotal)
                } else { allStable = false }
                samples.append(sample)
                report["queries"] = results + [["name": name, "text": text, "sort": sort, "samples": samples, "in_progress": true]]
                try save()
                if abort { break }
            }
            let measured = samples.filter { $0["warmup"] as? Bool == false }
            let values = measured.compactMap { sample -> Double? in
                guard let first = (sample["pages"] as? [[String: Any]])?.first else { return nil }
                return (first["xpc_ms"] as? NSNumber)?.doubleValue ?? (first["xpc_elapsed_lower_bound_ms"] as? NSNumber)?.doubleValue
            }.sorted()
            let censored = measured.contains { $0["timed_out"] as? Bool == true }
            let completed = measured.count == runs && measured.allSatisfy { $0["complete"] as? Bool == true }
            let p95 = values.isEmpty ? nil : values[max(0, Int(ceil(Double(values.count) * 0.95)) - 1)]
            let passed = completed && !censored && (p95 ?? .infinity) <= 100
            allTargets = allTargets && passed
            var result: [String: Any] = ["name": name, "text": text, "sort": sort, "samples": samples,
                                         "completed_all_measured_runs": completed, "timed_out_samples": measured.filter { $0["timed_out"] as? Bool == true }.count,
                                         "total": referenceTotal ?? NSNull(), "page_summary_sha256": referenceDigest ?? NSNull(),
                                         "p95_is_lower_bound": censored, "under_100ms": passed, "measured_samples": measured.count,
                                         "percentile_method": "Nearest rank; one sample is an individual latency, not a statistical P95 estimate."]
            if let p95 { result[censored ? "p95_lower_bound_ms" : "p95_ms"] = p95 }
            results.append(result); report["queries"] = results; try save()
            FileHandle.standardError.write(Data(("Completed case: " + name + "\n").utf8))
        }
        releaseAttempted = true
        let release = start(scoped(["op": "release_snapshot", "snapshot_lease": lease], listID: listID))
        _ = wait(release, seconds: timeout)
        report["release"] = release.response ?? ["timed_out": true]
        report["live_status_after"] = status(nil, timeout: timeout)
        report["dataset_status_after"] = status(listID, timeout: timeout)
        report["queries"] = results
        report["complete"] = !abort && results.count == specifications.count && results.allSatisfy { $0["completed_all_measured_runs"] as? Bool == true }
        report["success"] = allStable && !abort && release.response?["released"] as? Bool == true
        report["success_means"] = "Completed query results were stable within the retained snapshot and matched any supplied expected totals; lease release acknowledged. Latency target is separate."
        report["targets_all_passed"] = allTargets && !abort
        report["finished_at_utc"] = ISO8601DateFormatter().string(from: Date())
        try save()
        print(String(data: jsonData(["output": outputURL.path, "complete": report["complete"] ?? false, "success": report["success"] ?? false, "targets_all_passed": allTargets && !abort]), encoding: .utf8)!)
    }
}
