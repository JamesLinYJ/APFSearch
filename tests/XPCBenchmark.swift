import Foundation

/// Runs against a separately imported synthetic list through the installed,
/// authenticated Mach service. This measures transport + JSON + engine latency;
/// it deliberately does not claim to measure AppKit rendering.
@main struct XPCBenchmark {
  static func main() throws {
    let listID = CommandLine.arguments[1]
    let baseline = jsonObject(try Data(contentsOf: URL(fileURLWithPath: CommandLine.arguments[2])))
    let destination = URL(fileURLWithPath: CommandLine.arguments[3])
    let client = SearchClient()
    func call(_ request: [String: Any]) throws -> [String: Any] {
      var result: [String: Any]?
      client.call(request) { result = $0 }
      let deadline = Date().addingTimeInterval(120)
      while result == nil && Date() < deadline {
        RunLoop.current.run(until: Date().addingTimeInterval(0.001))
      }
      guard let result, result["success"] as? Bool == true else {
        throw NSError(
          domain: "XPCBenchmark", code: 1,
          userInfo: [NSLocalizedDescriptionKey: String(describing: result)])
      }
      return result
    }
    let startStatus = try call(["op": "status"])
    var results = [[String: Any]]()
    for specification in baseline["queries"] as? [[String: Any]] ?? [] {
      var timings = [Double]()
      var core = [Double]()
      var valid = true
      let query = specification["query"] as? String ?? ""
      let request: [String: Any] = [
        "op": "query", "list_id": listID, "text": query, "limit": 200,
        "sort": specification["sort"] ?? [],
      ]
      for index in 0..<32 {
        let start = ProcessInfo.processInfo.systemUptime
        let reply = try call(request)
        let elapsed = (ProcessInfo.processInfo.systemUptime - start) * 1000
        valid = valid && (reply["total"] as? Int == specification["total_matches"] as? Int)
        if index >= 2 {
          timings.append(elapsed)
          core.append((reply["elapsed_ms"] as? NSNumber)?.doubleValue ?? 0)
        }
      }
      let sorted = timings.sorted()
      let p95 = sorted[Int(ceil(Double(sorted.count) * 0.95)) - 1]
      results.append([
        "query": query, "name": specification["name"] ?? "", "samples": timings,
        "core_samples": core, "p95_ms": p95, "p50_ms": sorted[sorted.count / 2],
        "result_count_verified": valid, "under_100ms": p95 <= 100,
      ])
    }
    let report: [String: Any] = [
      "success": results.allSatisfy { $0["result_count_verified"] as? Bool == true },
      "boundary":
        "Persistent NSXPCConnection request through decoded JSON response; no AppKit render",
      "dataset": "1,000,000 synthetic offline records", "runs_per_query": 30,
      "warmups_per_query": 2, "live_scan_concurrent_at_start": startStatus["scanning"] ?? false,
      "queries": results,
    ]
    try jsonData(report).write(to: destination, options: .atomic)
    print(String(data: jsonData(report), encoding: .utf8)!)
  }
}
