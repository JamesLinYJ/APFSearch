import Foundation

@main struct ContentCancellationTests {
  static func main() throws {
    let root = URL(fileURLWithPath: CommandLine.arguments.dropFirst().first ?? NSTemporaryDirectory(), isDirectory: true)
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    setenv("APFSEARCH_DATA_DIR", root.appendingPathComponent("db", isDirectory: true).path, 1)
    let engine = SearchEngine()
    let content = ContentIndexer(engine: engine)

    let utf8URL = root.appendingPathComponent("chunked-utf8.txt")
    let utf8 = String(repeating: "分段取消 UTF-8 needle42\n", count: 20_000)
    try utf8.write(to: utf8URL, atomically: true, encoding: .utf8)
    var chunks = 0
    content.testTextChunkObserver = {
      chunks += 1
      if chunks == 1 { content.cancel() }
    }
    do {
      _ = try content.extract(utf8URL)
      throw ExtractionError(reason: "text cancellation was ignored")
    } catch let error as LocalizedText {
      guard error.key == "status.cancelled", chunks == 1 else {
        throw ExtractionError(reason: "unexpected cancellation result: \(error.key), chunks=\(chunks)")
      }
    }

    let utf8Decoded = try ContentIndexer(engine: engine).extract(utf8URL)
    guard utf8Decoded.0 == utf8 else {
      throw ExtractionError(reason: "UTF-8 segmented read changed decoded text")
    }

    let utf16URL = root.appendingPathComponent("chunked-utf16.txt")
    let utf16 = String(repeating: "UTF-16 中文 needle42\n", count: 20_000)
    try utf16.write(to: utf16URL, atomically: true, encoding: .utf16)
    let decoded = try ContentIndexer(engine: engine).extract(utf16URL)
    guard decoded.0 == utf16 else {
      throw ExtractionError(reason: "UTF-16 segmented read changed decoded text")
    }

    let textLimit = 32 * 1024 * 1024
    let boundaryURL = root.appendingPathComponent("text-limit.txt")
    try Data(repeating: 0x61, count: textLimit).write(to: boundaryURL)
    let boundary = try ContentIndexer(engine: engine).extract(boundaryURL)
    guard boundary.0.utf8.count == textLimit else {
      throw ExtractionError(reason: "exact 32 MiB text was rejected or truncated")
    }

    let overLimitURL = root.appendingPathComponent("text-over-limit.txt")
    try Data(repeating: 0x61, count: textLimit + 1).write(to: overLimitURL)
    do {
      _ = try ContentIndexer(engine: engine).extract(overLimitURL)
      throw ExtractionError(reason: "text over 32 MiB was accepted")
    } catch let error as LocalizedText {
      guard error.key == "error.text_file_size_limit" else {
        throw ExtractionError(reason: "unexpected over-limit result: \(error.key)")
      }
    }

    let identityURL = root.appendingPathComponent("identity.txt")
    let movedOriginalURL = root.appendingPathComponent("identity-original.txt")
    try Data(repeating: 0x61, count: 512 * 1024).write(to: identityURL)
    var replaced = false
    let identityContent = ContentIndexer(engine: engine)
    identityContent.testTextChunkObserver = {
      guard !replaced else { return }
      replaced = true
      try? FileManager.default.moveItem(at: identityURL, to: movedOriginalURL)
      try? Data("replacement".utf8).write(to: identityURL)
    }
    do {
      _ = try identityContent.extract(identityURL)
      throw ExtractionError(reason: "path replacement was not rejected")
    } catch let error as LocalizedText {
      guard error.key == "error.file_changed_during_extraction" else {
        throw ExtractionError(reason: "unexpected identity result: \(error.key)")
      }
    }

    print("{\"success\":true,\"checks\":[\"cancel_between_bounded_reads\",\"utf8_semantics\",\"utf16_semantics\",\"text_limit_boundary\",\"text_limit_overflow\",\"path_identity_replacement\"]}")
  }
}
