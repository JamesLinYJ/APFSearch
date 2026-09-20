import Foundation

enum DecoderProbe {
  private static let lock = NSLock()
  private static var threads = [Bool]()
  static func record() {
    lock.lock(); defer { lock.unlock() }
    threads.append(Thread.isMainThread)
  }
  static var allOffMain: Bool {
    lock.lock(); defer { lock.unlock() }
    return !threads.isEmpty && threads.allSatisfy { !$0 }
  }
}

// Replace only the OS boundary in a temporary compilation of SearchClient.
// Production recovery and request scheduling run unchanged; no real agent or
// index is touched by these tests.
final class SMAppService {
  enum Status: Int { case enabled, requiresApproval, notRegistered }
  static let instance = SMAppService()
  static func agent(plistName: String) -> SMAppService { instance }
  var status = Status.enabled
  var unregisterCount = 0
  var registerCount = 0
  var completion: ((Error?) -> Void)?
  func register() throws { registerCount += 1; status = .enabled }
  func unregister(completionHandler: @escaping (Error?) -> Void) {
    unregisterCount += 1; completion = completionHandler
  }
}
final class NSXPCInterface {
  init(with: Protocol) {}
}
final class NSXPCConnection {
  static var requests: [([String: Any], (Data) -> Void)] = []
  static var transportError = false
  static var connectionError = NSError(domain: "Fixture", code: 1)
  var remoteObjectInterface: NSXPCInterface?
  var invalidationHandler: (() -> Void)?
  init(machServiceName: String, options: [Int]) {}
  func resume() {}
  func invalidate() { invalidationHandler?() }
  func remoteObjectProxyWithErrorHandler(_ handler: @escaping (Error) -> Void) -> Any {
    if Self.transportError { handler(Self.connectionError) }
    return Proxy()
  }
  final class Proxy: NSObject, SearchServiceProtocol {
    func request(_ data: Data, withReply reply: @escaping (Data) -> Void) {
      if !NSXPCConnection.transportError { NSXPCConnection.requests.append((jsonObject(data), reply)) }
    }
  }
}
@main enum ServiceRecoveryTests {
  static func drain() {
    RunLoop.main.run(until: Date(timeIntervalSinceNow: 0.02))
  }
  static func require(_ value: Bool, _ description: String) {
    precondition(value, description)
    print("PASS: " + description)
  }
  static func main() {
    let client = SearchClient()
    client.registerService()
    var replies = 0
    client.call(["op": "status"]) { _ in
      require(Thread.isMainThread, "decoded replies reach the UI on the main thread")
      replies += 1
    }
    client.call(["op": "query"]) { _ in replies += 1 }
    let old = NSXPCConnection.requests
    NSXPCConnection.requests.removeAll()
    for (_, reply) in old { reply(jsonData(["success": false, "error_key": "error.unsupported_protocol"])) }
    drain()
    let agent = SMAppService.instance
    require(agent.unregisterCount == 1, "concurrent protocol rejections replace the service once")
    require(agent.registerCount == 0 && replies == 0, "wait for old process exit before registration or retry")
    client.call(["op": "volumes"]) { _ in replies += 1 }
    require(NSXPCConnection.requests.isEmpty, "new requests wait during replacement")
    agent.status = .notRegistered
    agent.completion?(nil)
    drain()
    require(agent.registerCount == 1 && NSXPCConnection.requests.count == 3, "register after exit and resume queued requests")
    let retried = NSXPCConnection.requests
    NSXPCConnection.requests.removeAll()
    for (_, reply) in retried { reply(jsonData(["success": false, "error_key": "error.unsupported_protocol"])) }
    drain()
    require(agent.unregisterCount == 1 && replies == 3, "persistent mismatch terminates recovery without restart loop")
    NSXPCConnection.transportError = true
    client.call(["op": "move"]) { _ in replies += 1 }
    drain()
    require(replies == 4 && agent.unregisterCount == 1, "transport failure does not replay file operations")
    NSXPCConnection.transportError = false
    let approvedClient = SearchClient()
    agent.status = .requiresApproval
    approvedClient.registerService()
    var approvalReplies = 0
    approvedClient.call(["op": "status"]) { response in
      require(response["success"] as? Bool == false, "approval requirement returns actionable error")
      approvalReplies += 1
    }
    require(NSXPCConnection.requests.isEmpty, "pending approval does not contact disabled service")
    agent.status = .enabled
    approvedClient.call(["op": "status"]) { _ in approvalReplies += 1 }
    NSXPCConnection.requests.removeFirst().1(jsonData(["success": true]))
    drain()
    require(approvalReplies == 2 && agent.unregisterCount == 1, "granting approval reconnects without replacing service")
    let cli = SearchClient()
    cli.call(["op": "status"]) { _ in replies += 1 }
    NSXPCConnection.requests.removeFirst().1(jsonData(["error_key": "error.unsupported_protocol"]))
    drain()
    require(agent.unregisterCount == 1, "CLI without registration ownership cannot replace GUI service")
    client.retryConnection()
    client.call(["op": "status"]) { response in
      require(response["success"] as? Bool == false, "unregistration failure reaches caller")
      replies += 1
    }
    NSXPCConnection.requests.removeFirst().1(jsonData(["error_key": "error.unsupported_protocol"]))
    drain()
    agent.completion?(NSError(domain: "Fixture", code: 2))
    drain()
    require(agent.unregisterCount == 2 && agent.registerCount == 1 && replies == 6, "manual retry is bounded and failed removal never registers over live service")
    require(DecoderProbe.allOffMain, "all production response decoding runs outside the main thread")
    testUnloadedService()
  }

  static func testUnloadedService() {
    let agent = SMAppService.instance
    agent.status = .enabled
    agent.registerCount = 0
    agent.unregisterCount = 0
    agent.completion = nil
    NSXPCConnection.requests.removeAll()
    NSXPCConnection.transportError = true
    NSXPCConnection.connectionError = NSError(domain: NSCocoaErrorDomain, code: NSXPCConnectionInvalid)
    let client = SearchClient()
    client.registerService()
    var failedReplies = 0
    for operation in ["status", "move"] {
      client.call(["op": operation]) { response in
        require(response["success"] as? Bool == false, "failed transport reports an error without replaying \(operation)")
        failedReplies += 1
      }
    }
    drain()
    require(agent.unregisterCount == 1 && agent.registerCount == 0,
      "enabled but unloaded agent is repaired once and waits for removal")
    client.call(["op": "query"]) { _ in }
    require(NSXPCConnection.requests.isEmpty, "new queries wait for unavailable service repair")
    agent.status = .notRegistered
    NSXPCConnection.transportError = false
    agent.completion?(nil)
    drain()
    require(agent.registerCount == 1 && failedReplies == 2, "repair registers agent and completes each failed request once")
    require(NSXPCConnection.requests.count == 1 && NSXPCConnection.requests[0].0["op"] as? String == "query",
      "only unsent requests run after repair; interrupted file operations are never replayed")
    NSXPCConnection.requests.removeFirst().1(jsonData(["success": true]))
    drain()
    NSXPCConnection.transportError = true
    client.call(["op": "status"]) { _ in failedReplies += 1 }
    drain()
    require(agent.unregisterCount == 1 && failedReplies == 3, "persistent invalid connection does not restart indefinitely")

    let cli = SearchClient()
    cli.call(["op": "status"]) { _ in }
    drain()
    require(agent.unregisterCount == 1, "CLI cannot repair another application's registration")

    let disabledClient = SearchClient()
    NSXPCConnection.transportError = false
    disabledClient.registerService()
    disabledClient.call(["op": "status"]) { _ in }
    // User approval may be revoked after a request was sent.
    agent.status = .requiresApproval
    NSXPCConnection.transportError = true
    disabledClient.call(["op": "status"]) { _ in }
    drain()
    require(agent.unregisterCount == 1, "connection recovery respects revoked background approval")
    NSXPCConnection.requests.removeAll()
    agent.status = .enabled
    client.retryConnection()
    client.call(["op": "status"]) { _ in }
    drain()
    require(agent.unregisterCount == 2, "explicit retry permits a fresh bounded registration repair")
    agent.completion?(NSError(domain: "Fixture", code: 2))
    drain()
    require(agent.registerCount == 1, "failed removal cannot register over a possibly running service")
  }
}
