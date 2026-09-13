import CryptoKit
import Foundation

struct UpdateManifest: Codable {
  let version: String
  let url: String
  let sha256: String
  let size: Int64
  let signature: String

  var canonical: Data? {
    guard Self.validVersion(version), Self.secureURL(url) != nil,
      sha256.range(of: "^[0-9a-fA-F]{64}$", options: .regularExpression) != nil,
      size > 0, size <= UpdateManager.maximumPackageSize else { return nil }
    return Data("APFSearch update v1\n\(version)\n\(url)\n\(sha256.lowercased())\n\(size)\n".utf8)
  }
  static func validVersion(_ value: String) -> Bool {
    value.range(of: "^[0-9]{1,6}(\\.[0-9]{1,6}){1,3}$", options: .regularExpression) != nil
  }
  static func secureURL(_ value: String) -> URL? {
    guard value.utf8.count <= 4096, !value.contains(where: { $0.isNewline || $0.asciiValue == 0 }),
      let url = URL(string: value), url.scheme?.lowercased() == "https",
      let host = url.host, !host.isEmpty, url.user == nil, url.password == nil,
      url.fragment == nil, url.port == nil || url.port == 443 else { return nil }
    return url
  }
}

enum UpdateError: Error, LocalizedError {
  case disabled, invalidManifest, invalidSignature, invalidURL, transport, digestMismatch, tooLarge
  var errorDescription: String? {
    switch self {
    case .disabled: return L("update.disabled")
    case .invalidManifest: return L("update.invalid_manifest")
    case .invalidSignature: return L("update.invalid_signature")
    case .invalidURL: return L("update.invalid_url")
    case .transport: return L("update.transport_error")
    case .digestMismatch: return L("update.digest_mismatch")
    case .tooLarge: return L("update.too_large")
    }
  }
}

/// A bounded streaming transfer. Delegate callbacks share one serial queue;
/// cancellation goes through URLSession. No whole-package Data allocation and
/// no unbounded download to disk before checking the signed byte count.
private final class UpdateTransfer: NSObject, URLSessionDataDelegate {
  private let maximum: Int64
  private let configuration: URLSessionConfiguration
  private let expected: UpdateManifest?
  private let completion: (Result<(Data, URL?), Error>) -> Void
  private var session: URLSession?
  private var task: URLSessionDataTask?
  private var directory: URL?
  private var file: FileHandle?
  private var data = Data()
  private var digest = SHA256()
  private var received: Int64 = 0
  private var failure: Error?

  init(maximum: Int64, expected: UpdateManifest?, configuration: URLSessionConfiguration,
    completion: @escaping (Result<(Data, URL?), Error>) -> Void) {
    self.maximum = maximum; self.expected = expected; self.completion = completion
    self.configuration = configuration.copy() as! URLSessionConfiguration
  }
  func start(_ url: URL) throws -> () -> Void {
    if expected != nil {
      let location = FileManager.default.temporaryDirectory.appendingPathComponent("APFSearch-update-" + UUID().uuidString, isDirectory: true)
      try FileManager.default.createDirectory(at: location, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
      directory = location
      let path = location.appendingPathComponent("installer.pkg")
      guard FileManager.default.createFile(atPath: path.path, contents: nil, attributes: [.posixPermissions: 0o600]) else { throw UpdateError.transport }
      file = try FileHandle(forWritingTo: path)
    }
    let configuration = self.configuration
    configuration.urlCache = nil; configuration.requestCachePolicy = .reloadIgnoringLocalCacheData
    configuration.timeoutIntervalForRequest = 30; configuration.timeoutIntervalForResource = 600
    let queue = OperationQueue(); queue.maxConcurrentOperationCount = 1; queue.qualityOfService = .utility
    let session = URLSession(configuration: configuration, delegate: self, delegateQueue: queue)
    self.session = session
    let task = session.dataTask(with: url); self.task = task
    task.resume()
    return { [weak task] in task?.cancel() }
  }
  func urlSession(_ session: URLSession, task: URLSessionTask, willPerformHTTPRedirection response: HTTPURLResponse,
    newRequest request: URLRequest, completionHandler: @escaping (URLRequest?) -> Void) {
    guard let url = request.url, UpdateManifest.secureURL(url.absoluteString) != nil else {
      failure = UpdateError.invalidURL; completionHandler(nil); task.cancel(); return
    }
    completionHandler(request)
  }
  func urlSession(_ session: URLSession, dataTask: URLSessionDataTask, didReceive response: URLResponse,
    completionHandler: @escaping (URLSession.ResponseDisposition) -> Void) {
    guard let http = response as? HTTPURLResponse, (200..<300).contains(http.statusCode),
      let url = response.url, UpdateManifest.secureURL(url.absoluteString) != nil else {
      failure = UpdateError.transport; completionHandler(.cancel); return
    }
    guard response.expectedContentLength <= maximum else {
      failure = UpdateError.tooLarge; completionHandler(.cancel); return
    }
    completionHandler(.allow)
  }
  func urlSession(_ session: URLSession, dataTask: URLSessionDataTask, didReceive chunk: Data) {
    guard failure == nil else { return }
    guard Int64(chunk.count) <= maximum - received else { failure = UpdateError.tooLarge; dataTask.cancel(); return }
    received += Int64(chunk.count)
    do {
      if let file { try file.write(contentsOf: chunk); digest.update(data: chunk) }
      else { data.append(chunk) }
    } catch { failure = error; dataTask.cancel(); return }
  }
  func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?) {
    var result: Result<(Data, URL?), Error>
    do {
      if let error = failure ?? error { throw error }
      if let expected {
        let hex = digest.finalize().map { String(format: "%02x", $0) }.joined()
        guard received == expected.size, hex == expected.sha256.lowercased() else { throw UpdateError.digestMismatch }
        try file?.synchronize(); try file?.close(); file = nil
        guard let directory else { throw UpdateError.transport }
        result = .success((Data(), directory.appendingPathComponent("installer.pkg")))
        self.directory = nil // Ownership transfers to the caller only after verification.
      } else { result = .success((data, nil)) }
    } catch { result = .failure(error) }
    try? file?.close(); file = nil
    if let directory { try? FileManager.default.removeItem(at: directory); self.directory = nil }
    session.finishTasksAndInvalidate(); self.session = nil; self.task = nil
    DispatchQueue.main.async { self.completion(result) }
  }
  deinit {
    try? file?.close()
    if let directory { try? FileManager.default.removeItem(at: directory) }
  }
}

/// The embedded public key is the trust anchor; a server-provided checksum is
/// not one. Verification is repeated by download(), even for a constructed
/// manifest. Missing release configuration disables networking by default.
final class UpdateManager {
  static let maximumPackageSize: Int64 = 512 * 1024 * 1024
  static let shared = UpdateManager()
  private let keyData: Data?
  private let configuration: URLSessionConfiguration
  let feedURL: URL?

  init(bundle: Bundle = .main) {
    configuration = .ephemeral
    func text(_ name: String) -> String? {
      guard let url = bundle.url(forResource: name, withExtension: "txt") else { return nil }
      return (try? String(contentsOf: url, encoding: .utf8))?.trimmingCharacters(in: .whitespacesAndNewlines)
    }
    keyData = text("UpdatePublicKey").flatMap { Data(base64Encoded: $0) }
    feedURL = text("UpdateFeedURL").flatMap(UpdateManifest.secureURL)
  }
  init(publicKey: Data, feedURL: URL? = nil, configuration: URLSessionConfiguration = .ephemeral) {
    keyData = publicKey; self.feedURL = feedURL; self.configuration = configuration
  }
  var configured: Bool { keyData?.count == 32 && feedURL != nil }

  func verify(_ data: Data) throws -> UpdateManifest {
    guard data.count <= 65_536, let manifest = try? JSONDecoder().decode(UpdateManifest.self, from: data) else { throw UpdateError.invalidManifest }
    try verify(manifest); return manifest
  }
  private func verify(_ manifest: UpdateManifest) throws {
    guard let keyData, keyData.count == 32 else { throw UpdateError.disabled }
    let key = try Curve25519.Signing.PublicKey(rawRepresentation: keyData)
    guard let payload = manifest.canonical, let signature = Data(base64Encoded: manifest.signature), signature.count == 64 else { throw UpdateError.invalidManifest }
    guard key.isValidSignature(signature, for: payload) else { throw UpdateError.invalidSignature }
  }
  @discardableResult
  func check(manifestURL: URL? = nil, completion: @escaping (Result<UpdateManifest?, Error>) -> Void) -> () -> Void {
    guard keyData?.count == 32, let remote = manifestURL ?? feedURL else {
      DispatchQueue.main.async { completion(.failure(UpdateError.disabled)) }; return {}
    }
    guard UpdateManifest.secureURL(remote.absoluteString) != nil else {
      DispatchQueue.main.async { completion(.failure(UpdateError.invalidURL)) }; return {}
    }
    do {
      let transfer = UpdateTransfer(maximum: 65_536, expected: nil, configuration: configuration) { result in
        do {
          let (data, _) = try result.get(); let manifest = try self.verify(data)
          let current = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "0.0.0"
          completion(.success(current.compare(manifest.version, options: .numeric) == .orderedAscending ? manifest : nil))
        } catch { completion(.failure(error)) }
      }
      return try transfer.start(remote)
    } catch { DispatchQueue.main.async { completion(.failure(error)) }; return {} }
  }
  @discardableResult
  func download(_ manifest: UpdateManifest, completion: @escaping (Result<URL, Error>) -> Void) -> () -> Void {
    do {
      try verify(manifest)
      guard let remote = UpdateManifest.secureURL(manifest.url) else { throw UpdateError.invalidURL }
      let transfer = UpdateTransfer(maximum: manifest.size, expected: manifest, configuration: configuration) { result in
        completion(result.flatMap { _, url in url.map { .success($0) } ?? .failure(UpdateError.transport) })
      }
      return try transfer.start(remote)
    } catch { DispatchQueue.main.async { completion(.failure(error)) }; return {} }
  }
}
