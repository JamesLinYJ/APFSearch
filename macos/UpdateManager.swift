import CryptoKit
import Foundation

struct UpdateManifest: Decodable {
  let version: String
  let url: String
  let sha256: String
  let signature: String
}

enum UpdateError: Error, LocalizedError {
  case disabled, invalidManifest, invalidSignature, invalidURL, transport, digestMismatch
  var errorDescription: String? {
    switch self {
    case .disabled: return "Automatic updates are disabled because no valid update public key is installed."
    case .invalidManifest: return "The update manifest is invalid."
    case .invalidSignature: return "The update manifest signature is invalid."
    case .invalidURL: return "The update URL is invalid."
    case .transport: return "The update could not be downloaded."
    case .digestMismatch: return "The downloaded update failed its SHA-256 verification."
    }
  }
}

/// Updates fail closed. A release manifest is trusted only when its canonical
/// payload is signed by the embedded Ed25519 key; the downloaded installer must
/// then match the signed SHA-256 digest. Notarization remains independently
/// enforced by the release pipeline and macOS when the package is opened.
final class UpdateManager {
  static let shared = UpdateManager()
  private let session: URLSession
  private init(session: URLSession = .shared) { self.session = session }

  private func publicKey() throws -> Curve25519.Signing.PublicKey {
    guard let url = Bundle.main.url(forResource: "UpdatePublicKey", withExtension: "txt"),
      let encoded = try? String(contentsOf: url, encoding: .utf8)
        .trimmingCharacters(in: .whitespacesAndNewlines),
      let bytes = Data(base64Encoded: encoded), bytes.count == 32,
      let key = try? Curve25519.Signing.PublicKey(rawRepresentation: bytes)
    else { throw UpdateError.disabled }
    return key
  }

  private func canonical(_ manifest: UpdateManifest) -> Data? {
    guard !manifest.version.contains("\n"), !manifest.url.contains("\n"),
      manifest.sha256.range(of: "^[0-9a-fA-F]{64}$", options: .regularExpression) != nil
    else { return nil }
    return Data("\(manifest.version)\n\(manifest.url)\n\(manifest.sha256.lowercased())\n".utf8)
  }

  func verify(_ data: Data) throws -> UpdateManifest {
    guard let manifest = try? JSONDecoder().decode(UpdateManifest.self, from: data),
      let payload = canonical(manifest),
      let signature = Data(base64Encoded: manifest.signature)
    else { throw UpdateError.invalidManifest }
    let key = try publicKey()
    guard key.isValidSignature(signature, for: payload) else {
      throw UpdateError.invalidSignature
    }
    return manifest
  }

  func check(manifestURL: URL, completion: @escaping (Result<UpdateManifest?, Error>) -> Void) {
    session.dataTask(with: manifestURL) { data, response, error in
      guard error == nil, let http = response as? HTTPURLResponse,
        (200..<300).contains(http.statusCode), let data
      else { DispatchQueue.main.async { completion(.failure(UpdateError.transport)) }; return }
      do {
        let manifest = try self.verify(data)
        let current = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "0"
        let newer = current.compare(manifest.version, options: .numeric) == .orderedAscending
        DispatchQueue.main.async { completion(.success(newer ? manifest : nil)) }
      } catch {
        DispatchQueue.main.async { completion(.failure(error)) }
      }
    }.resume()
  }

  func download(_ manifest: UpdateManifest, completion: @escaping (Result<URL, Error>) -> Void) {
    guard let remote = URL(string: manifest.url), remote.scheme == "https" else {
      completion(.failure(UpdateError.invalidURL)); return
    }
    session.downloadTask(with: remote) { temporary, response, error in
      guard error == nil, let http = response as? HTTPURLResponse,
        (200..<300).contains(http.statusCode), let temporary,
        let bytes = try? Data(contentsOf: temporary, options: .mappedIfSafe)
      else { DispatchQueue.main.async { completion(.failure(UpdateError.transport)) }; return }
      let digest = SHA256.hash(data: bytes).map { String(format: "%02x", $0) }.joined()
      guard digest.caseInsensitiveCompare(manifest.sha256) == .orderedSame else {
        DispatchQueue.main.async { completion(.failure(UpdateError.digestMismatch)) }; return
      }
      let destination = FileManager.default.temporaryDirectory
        .appendingPathComponent("APFSearch-\(manifest.version)-\(UUID().uuidString).pkg")
      do {
        try FileManager.default.moveItem(at: temporary, to: destination)
        DispatchQueue.main.async { completion(.success(destination)) }
      } catch {
        DispatchQueue.main.async { completion(.failure(error)) }
      }
    }.resume()
  }
}
