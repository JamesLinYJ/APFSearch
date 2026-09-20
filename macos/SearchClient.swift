import Foundation
import ServiceManagement

final class SearchClient {
  static let shared = SearchClient()
  private var connection: NSXPCConnection?
  private let lock = NSLock()
  private var registrationError: String?
  private var awaitingApproval = false
  private var managesService = false
  private var replacementAttempted = false
  private var replacingService = false
  private var waitingRequests: [() -> Void] = []
  private let responseQueue = DispatchQueue(label: "APFSearch.ServiceResponses", qos: .userInitiated)
  init() {}

  var requiresApproval: Bool {
    SMAppService.agent(plistName: serviceName + ".plist").status == .requiresApproval
  }

  func retryConnection() {
    guard !replacingService else { return }
    replacementAttempted = false
    registerService()
    invalidateConnection()
  }

  private func invalidateConnection() {
    lock.lock()
    let previous = connection
    connection = nil
    lock.unlock()
    previous?.invalidate()
  }

  // Repair registration through ServiceManagement, waiting for removal before
  // registering again. Callers decide whether their request may be replayed.
  private func replaceService(completion: @escaping () -> Void) {
    waitingRequests.append(completion)
    guard !replacingService else { return }
    replacingService = true
    replacementAttempted = true
    invalidateConnection()
    let agent = SMAppService.agent(plistName: serviceName + ".plist")
    agent.unregister { [self] error in
      DispatchQueue.main.async { [self] in
        if let error {
          registrationError = L("error.service_registration", error.localizedDescription)
        } else {
          // The asynchronous completion guarantees the old process has exited.
          registerService()
        }
        replacingService = false
        let pending = waitingRequests
        waitingRequests.removeAll()
        pending.forEach { $0() }
      }
    }
  }
  static func decodeResponse(_ data: Data, bundle: Bundle = .main, locale: Locale = .current) -> [String: Any] {
    localizedServiceResponse(jsonObject(data), bundle: bundle, locale: locale)
  }
  func registerService() {
    managesService = true
    let agent = SMAppService.agent(plistName: serviceName + ".plist")
    do {
      // A bundled agent without a Background Task Management record can report
      // notFound before its first registration, not just notRegistered.
      if agent.status != .enabled && agent.status != .requiresApproval { try agent.register() }
      awaitingApproval = agent.status == .requiresApproval
      if awaitingApproval {
        registrationError = L("settings.enable_background_service_hint")
      } else if agent.status != .enabled {
        registrationError = L(
          "error.service_not_ready", localizedCount(agent.status.rawValue))
      } else {
        registrationError = nil
      }
    } catch { registrationError = L("error.service_registration", error.localizedDescription) }
  }
  private func connectionFailed(_ error: Error, completion: @escaping ([String: Any]) -> Void) {
    let failure = { [self] in
      completion([
        "success": false,
        "error": registrationError ?? L("error.service_connection", error.localizedDescription),
      ])
    }
    let transportError = error as NSError
    // Enabled describes the user's authorization, not whether the launchd job
    // is still loaded. For example, bootout removes that job without clearing
    // ServiceManagement's registration. Reconnecting alone cannot restore it.
    guard managesService, !replacementAttempted,
          transportError.domain == NSCocoaErrorDomain,
          transportError.code == NSXPCConnectionInvalid,
          SMAppService.agent(plistName: serviceName + ".plist").status == .enabled else {
      failure()
      return
    }
    // One repair per client (or explicit retry), never a restart loop. Unlike
    // protocol rejection, a transport failure does not prove non-execution:
    // report the original failure, even after repair, without replaying it.
    replaceService(completion: failure)
  }
  private func connect() -> NSXPCConnection {
    lock.lock()
    defer { lock.unlock() }
    if let c = connection { return c }
    let c = NSXPCConnection(machServiceName: serviceName, options: [])
    c.remoteObjectInterface = NSXPCInterface(with: SearchServiceProtocol.self)
    c.invalidationHandler = { [weak self, weak c] in
      guard let self = self else { return }
      self.lock.lock()
      if self.connection === c { self.connection = nil }
      self.lock.unlock()
    }
    c.resume()
    connection = c
    return c
  }
  func call(_ request: [String: Any], completion: @escaping ([String: Any]) -> Void) {
    if replacingService {
      waitingRequests.append { [self] in call(request, completion: completion) }
      return
    }
    if awaitingApproval && !requiresApproval { registerService() }
    if let registrationError {
      completion(["success": false, "error": registrationError])
      return
    }
    var body = request
    body["protocol_version"] = protocolVersion
    let c = connect()
    let proxy =
      c.remoteObjectProxyWithErrorHandler { [weak self] e in
        DispatchQueue.main.async {
          guard let self else {
            completion(["success": false, "error": L("error.service_connection", e.localizedDescription)])
            return
          }
          self.connectionFailed(e, completion: completion)
        }
      } as? SearchServiceProtocol
    proxy?.request(jsonData(body)) { data in
      // Decode and localize before entering the UI queue. Large operation or
      // coverage replies must not block keyboard, scrolling, or animation events.
      self.responseQueue.async { [self] in
        let response = autoreleasepool { Self.decodeResponse(data) }
        DispatchQueue.main.async { [self] in
          if managesService, response["error_key"] as? String == "error.unsupported_protocol",
             replacingService || !replacementAttempted {
            // Protocol rejection happens before dispatch, so replay is safe.
            replaceService { [self] in call(request, completion: completion) }
          } else {
            completion(response)
          }
        }
      }
    }
  }
}
