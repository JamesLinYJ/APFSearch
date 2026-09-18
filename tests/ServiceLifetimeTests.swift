import Foundation

private final class LeaseResult: @unchecked Sendable {
  private let lock = NSLock()
  private var result: Result<OfflineEngineLease, Error>?
  func set(_ value: Result<OfflineEngineLease, Error>) {
    lock.lock(); result = value; lock.unlock()
  }
  func get() -> Result<OfflineEngineLease, Error>? {
    lock.lock(); defer { lock.unlock() }; return result
  }
}

@main struct ServiceLifetimeTests {
  static func main() throws {
    let root = URL(fileURLWithPath: CommandLine.arguments[1], isDirectory: true)
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    var checks = [String]()
    func check(_ name: String, _ condition: Bool) throws {
      guard condition else {
        throw NSError(domain: "ServiceLifetimeTests", code: 1,
          userInfo: [NSLocalizedDescriptionKey: name])
      }
      checks.append(name)
    }
    func waitUntil(_ condition: () -> Bool) -> Bool {
      let deadline = ProcessInfo.processInfo.systemUptime + 5
      while !condition() && ProcessInfo.processInfo.systemUptime < deadline {
        _ = RunLoop.current.run(mode: .default, before: Date().addingTimeInterval(0.005))
      }
      return condition()
    }

    // A parent path that is a file used to bypass the recoverable error code.
    let blockedDirectory = root.appendingPathComponent("blocked")
    let original = Data("preserve existing file".utf8)
    try original.write(to: blockedDirectory)
    let failed = SearchEngine(directory: blockedDirectory)
    try check("directory_failure_is_recoverable", failed.call(["op": "status"])["error_code"] as? String == "index_open_failed")
    try check("directory_failure_preserves_existing_file", Data(contentsOf: blockedDirectory) == original)
    try FileManager.default.removeItem(at: blockedDirectory)
    try check("explicit_retry_creates_missing_directory", failed.call(["op": "status", "retry_index": true])["success"] as? Bool == true)

    setenv("APFSEARCH_DATA_DIR", root.appendingPathComponent("primary").path, 1)
    let service = SearchService()
    let lists = service.engine.directory.appendingPathComponent("Imported Lists", isDirectory: true)
    let firstID = UUID().uuidString, secondID = UUID().uuidString
    let firstDirectory = lists.appendingPathComponent(firstID)
    let secondDirectory = lists.appendingPathComponent(secondID)
    // Empty offline databases are sufficient for ownership tests, without scans.
    for directory in [firstDirectory, secondDirectory] {
      let engine = SearchEngine(directory: directory)
      try check("fixture_open_\(directory == firstDirectory ? "first" : "second")", engine.call(["op": "status"])["success"] as? Bool == true)
    }

    let registry = OfflineEngineRegistry()
    let loaderEntered = DispatchSemaphore(value: 0)
    let finishLoading = DispatchSemaphore(value: 0)
    registry.beforeLoad = { id in
      if id == firstID {
        loaderEntered.signal()
        _ = finishLoading.wait(timeout: .now() + 5)
      }
    }
    let firstResult = LeaseResult(), waiterResult = LeaseResult(), independentResult = LeaseResult()
    DispatchQueue.global().async {
      firstResult.set(Result { try registry.acquire(id: firstID, directory: firstDirectory) })
    }
    try check("loader_started", loaderEntered.wait(timeout: .now() + 5) == .success)
    DispatchQueue.global().async {
      waiterResult.set(Result { try registry.acquire(id: firstID, directory: firstDirectory) })
    }
    try check("concurrent_request_waits_for_existing_load", waitUntil { registry.waitingCount(for: firstID) == 1 })
    DispatchQueue.global().async {
      independentResult.set(Result { try registry.acquire(id: secondID, directory: secondDirectory) })
    }
    try check("unrelated_list_loads_outside_registry_lock", waitUntil { independentResult.get() != nil })
    finishLoading.signal()
    try check("both_loading_requests_finish", waitUntil { firstResult.get() != nil && waiterResult.get() != nil })
    let firstLease = try firstResult.get()!.get(), waiterLease = try waiterResult.get()!.get()
    try check("concurrent_load_shares_engine", firstLease.engine === waiterLease.engine)
    _ = try independentResult.get()!.get()

    // Expiry must run without another request, and actually release the Rust
    // token while keeping the still-borrowed engine alive for this assertion.
    func request(_ payload: [String: Any]) throws -> [String: Any] {
      let completed = DispatchSemaphore(value: 0)
      var response = [String: Any]()
      var input = payload; input["protocol_version"] = protocolVersion
      service.request(jsonData(input)) { response = jsonObject($0); completed.signal() }
      guard completed.wait(timeout: .now() + 5) == .success else {
        throw NSError(domain: "ServiceLifetimeTests", code: 2)
      }
      return response
    }
    let firstPage = try request(["op": "query", "list_id": firstID, "text": "", "retain_snapshot": true])
    let token = firstPage["snapshot_lease"] as? String ?? ""
    let heldEngine = try service.offlineEngine(firstID)
    try check("first_page_registers_bridge_lease", !token.isEmpty && service.hasOfflineSnapshotLeaseForTesting(token, id: firstID))
    service.expireOfflineSnapshotLeaseForTesting(token, id: firstID)
    try check("abandoned_lease_expires_without_new_requests", waitUntil {
      !service.hasOfflineSnapshotLeaseForTesting(token, id: firstID)
    })
    try check("expiry_releases_rust_lease", waitUntil {
      heldEngine.engine.call(["op": "renew_snapshot", "snapshot_lease": token])["success"] as? Bool == false
    })

    let corruptID = UUID().uuidString
    let corruptDirectory = lists.appendingPathComponent(corruptID)
    try FileManager.default.createDirectory(at: corruptDirectory, withIntermediateDirectories: true)
    try Data("not SQLite".utf8).write(to: corruptDirectory.appendingPathComponent("index.sqlite"))
    let corruptResult = try request(["op": "status", "list_id": corruptID])
    try check("corrupt_offline_index_reports_error", corruptResult["error_code"] as? String == "index_open_failed")
    try check("corrupt_offline_index_does_not_break_primary", request(["op": "status"])["success"] as? Bool == true)
    print(String(data: jsonData(["success": true, "checks": checks]), encoding: .utf8)!)
  }
}
