import CryptoKit
import Foundation

func fail(_ message: String) -> Never {
  FileHandle.standardError.write(Data((message + "\n").utf8))
  exit(1)
}

guard CommandLine.arguments.count == 5 else {
  fail("usage: sign_update_manifest.swift VERSION HTTPS_URL SHA256 OUTPUT")
}
let version = CommandLine.arguments[1]
let url = CommandLine.arguments[2]
let digest = CommandLine.arguments[3].lowercased()
let output = URL(fileURLWithPath: CommandLine.arguments[4])

guard !version.contains("\n"), !url.contains("\n"), URL(string: url)?.scheme == "https",
  digest.range(of: "^[0-9a-f]{64}$", options: .regularExpression) != nil
else { fail("invalid update manifest fields") }

guard let encoded = ProcessInfo.processInfo.environment["FILESEARCH_UPDATE_PRIVATE_KEY"],
  let raw = Data(base64Encoded: encoded), raw.count == 32,
  let privateKey = try? Curve25519.Signing.PrivateKey(rawRepresentation: raw)
else { fail("FILESEARCH_UPDATE_PRIVATE_KEY must be a base64-encoded 32-byte Ed25519 private key") }

let payload = Data("\(version)\n\(url)\n\(digest)\n".utf8)
guard let signature = try? privateKey.signature(for: payload) else { fail("could not sign update manifest") }
let object: [String: String] = [
  "version": version,
  "url": url,
  "sha256": digest,
  "signature": signature.base64EncodedString(),
]
let data = try JSONSerialization.data(withJSONObject: object, options: [.sortedKeys, .prettyPrinted])
try data.write(to: output, options: .atomic)
