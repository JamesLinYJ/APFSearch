import CryptoKit
import Foundation

func fail(_ message: String) -> Never {
  FileHandle.standardError.write(Data((message + "\n").utf8)); exit(1)
}
// Run by release.sh only after notarization and stapling, which change bytes.
guard CommandLine.arguments.count == 6 else {
  fail("usage: sign_update_manifest.swift VERSION HTTPS_URL SHA256 SIZE OUTPUT")
}
let args = CommandLine.arguments
let version = args[1], address = args[2], digest = args[3].lowercased()
guard version.range(of: "^[0-9]{1,6}(\\.[0-9]{1,6}){1,3}$", options: .regularExpression) != nil,
  address.utf8.count <= 4096, !address.contains(where: { $0.isNewline || $0.asciiValue == 0 }),
  let url = URL(string: address), url.scheme == "https", let host = url.host, !host.isEmpty,
  url.user == nil, url.password == nil, url.fragment == nil, url.port == nil || url.port == 443,
  digest.range(of: "^[0-9a-f]{64}$", options: .regularExpression) != nil,
  let size = Int64(args[4]), size > 0, size <= 512 * 1024 * 1024
else { fail("Invalid update manifest fields") }
let environment = ProcessInfo.processInfo.environment
guard let encoded = environment["FILESEARCH_UPDATE_PRIVATE_KEY"],
  let bytes = Data(base64Encoded: encoded), bytes.count == 32,
  let key = try? Curve25519.Signing.PrivateKey(rawRepresentation: bytes),
  let expected = environment["FILESEARCH_UPDATE_PUBLIC_KEY"].flatMap({ Data(base64Encoded: $0) }),
  key.publicKey.rawRepresentation == expected
else { fail("Update signing key must match the public key embedded in the release") }
let payload = Data("APFSearch update v1\n\(version)\n\(address)\n\(digest)\n\(size)\n".utf8)
let signature = try key.signature(for: payload)
let object: [String: Any] = ["version": version, "url": address, "sha256": digest,
  "size": size, "signature": signature.base64EncodedString()]
let data = try JSONSerialization.data(withJSONObject: object, options: [.sortedKeys, .prettyPrinted])
try data.write(to: URL(fileURLWithPath: args[5]), options: .atomic)
