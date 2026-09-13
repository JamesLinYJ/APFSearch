import Foundation
import ServiceManagement

final class SearchClient {
  static let shared = SearchClient()
  private var connection: NSXPCConnection?
  private let lock = NSLock()
  private var registrationError: String?
  init() {}
  static func decodeResponse(_ data: Data, bundle: Bundle = .main, locale: Locale = .current) -> [String: Any] {
    localizedServiceResponse(jsonObject(data), bundle: bundle, locale: locale)
  }
  func registerService() {
    let agent = SMAppService.agent(plistName: serviceName + ".plist")
    do {
      // A bundled agent without a Background Task Management record can report
      // notFound before its first registration, not just notRegistered.
      if agent.status != .enabled && agent.status != .requiresApproval { try agent.register() }
      if agent.status == .requiresApproval {
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
      DispatchQueue.main.async { completion(Self.decodeResponse(data)) }
    }
  }
}
