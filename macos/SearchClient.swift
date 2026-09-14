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

  // A protocol rejection happens before dispatch, so these requests have not
  // executed. Transport failures are deliberately never replayed: a file
  // operation may already have completed before its reply was interrupted.
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
          completion([
            "success": false,
            "error": self?.registrationError ?? L("error.service_connection", e.localizedDescription),
          ])
        }
      } as? SearchServiceProtocol
    proxy?.request(jsonData(body)) { data in
      DispatchQueue.main.async { [self] in
        let response = Self.decodeResponse(data)
        if managesService, response["error_key"] as? String == "error.unsupported_protocol",
           replacingService || !replacementAttempted {
          replaceService { [self] in call(request, completion: completion) }
        } else {
          completion(response)
        }
      }
    }
  }
}
