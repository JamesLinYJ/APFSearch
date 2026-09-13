import Foundation

/// Requests never leave the process. Production URLSession delegate callbacks,
/// byte budgets, file writes, digest validation and cancellation still execute.
final class UpdateTransportFixture: URLProtocol {
  static let lock = NSLock()
  static var bodies: [String: Data] = [:]
  static var unfinished = Set<String>()
  override class func canInit(with request: URLRequest) -> Bool { request.url?.host == "updates.fixture" }
  override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
  override func startLoading() {
    Self.lock.lock()
    let path = request.url!.path
    let body = Self.bodies[path] ?? Data()
    let pending = Self.unfinished.contains(path)
    Self.lock.unlock()
    let response = HTTPURLResponse(url: request.url!, statusCode: 200, httpVersion: "HTTP/1.1",
      headerFields: ["Content-Length": pending ? "7" : String(body.count)])!
    client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
    if !body.isEmpty {
      for offset in stride(from: 0, to: body.count, by: 3) {
        client?.urlProtocol(self, didLoad: body.subdata(in: offset..<min(body.count, offset + 3)))
      }
    }
    if !pending { client?.urlProtocolDidFinishLoading(self) }
  }
  override func stopLoading() {}
}
