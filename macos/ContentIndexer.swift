import AVFoundation
import Foundation
import ImageIO
import PDFKit
import UniformTypeIdentifiers

struct ExtractionError: Error, LocalizedError {
  let reason: String
  var errorDescription: String? { reason }
}
final class XMLText: NSObject, XMLParserDelegate {
  var text = ""
  let limit = 16 * 1024 * 1024
  func parser(_ parser: XMLParser, foundCharacters string: String) {
    if text.utf8.count + string.utf8.count > limit { parser.abortParsing() } else { text += string }
  }
  func parser(_ parser: XMLParser, foundCDATA data: Data) {
    self.parser(parser, foundCharacters: String(decoding: data, as: UTF8.self))
  }
  func parser(
    _ parser: XMLParser, didEndElement elementName: String, namespaceURI: String?,
    qualifiedName qName: String?
  ) {
    if ["w:p", "a:p", "row", "si", "t"].contains(qName ?? elementName) { text += "\n" }
  }
}
/// Office Open XML keeps XLSX cell text in a shared-string table. Reading all
/// XML character data would index table offsets as though they were cell text.
final class SpreadsheetStrings: NSObject, XMLParserDelegate {
  var strings = [String]()
  private var current = "", inString = false, inText = false, phoneticDepth = 0
  private var bytes = 0
  func parser(
    _ parser: XMLParser, didStartElement name: String, namespaceURI: String?,
    qualifiedName: String?, attributes: [String: String]
  ) {
    let local = name.split(separator: ":").last.map(String.init) ?? name
    if local == "si" {
      current = ""
      inString = true
    }
    if local == "rPh" { phoneticDepth += 1 }
    if local == "t" && inString && phoneticDepth == 0 { inText = true }
  }
  func parser(_ parser: XMLParser, foundCharacters text: String) {
    guard inText else { return }
    bytes += text.utf8.count
    if bytes > 16 * 1024 * 1024 { parser.abortParsing() } else { current += text }
  }
  func parser(_ parser: XMLParser, foundCDATA data: Data) {
    self.parser(parser, foundCharacters: String(decoding: data, as: UTF8.self))
  }
  func parser(
    _ parser: XMLParser, didEndElement name: String, namespaceURI: String?, qualifiedName: String?
  ) {
    let local = name.split(separator: ":").last.map(String.init) ?? name
    if local == "t" { inText = false }
    if local == "rPh" { phoneticDepth -= 1 }
    if local == "si" {
      strings.append(current)
      inString = false
    }
  }
}
final class SpreadsheetSheet: NSObject, XMLParserDelegate {
  let shared: [String]
  var text = ""
  var failure: LocalizedText?
  private var type = "", value = "", inline = "", formula = "", capture = "", inCell = false
  init(shared: [String]) { self.shared = shared }
  func parser(
    _ parser: XMLParser, didStartElement name: String, namespaceURI: String?,
    qualifiedName: String?, attributes: [String: String]
  ) {
    let local = name.split(separator: ":").last.map(String.init) ?? name
    if local == "c" {
      type = attributes["t"] ?? "n"
      value = ""
      inline = ""
      formula = ""
      inCell = true
    }
    if inCell && ["v", "t", "f"].contains(local) { capture = local }
  }
  func parser(_ parser: XMLParser, foundCharacters characters: String) {
    if capture == "v" {
      value += characters
    } else if capture == "t" {
      inline += characters
    } else if capture == "f" {
      formula += characters
    }
    if value.utf8.count + inline.utf8.count + formula.utf8.count + text.utf8.count > 16 * 1024
      * 1024
    {
      failure = LT("error.xlsx_text_limit")
      parser.abortParsing()
    }
  }
  func parser(_ parser: XMLParser, foundCDATA data: Data) {
    self.parser(parser, foundCharacters: String(decoding: data, as: UTF8.self))
  }
  func parser(
    _ parser: XMLParser, didEndElement name: String, namespaceURI: String?, qualifiedName: String?
  ) {
    let local = name.split(separator: ":").last.map(String.init) ?? name
    if ["v", "t", "f"].contains(local) { capture = "" }
    if local == "c" {
      var output = value
      if type == "s" {
        guard let index = Int(value.trimmingCharacters(in: .whitespacesAndNewlines)),
          shared.indices.contains(index)
        else {
          failure = LT("error.xlsx_invalid_shared_string")
          parser.abortParsing()
          return
        }
        output = shared[index]
      } else if type == "inlineStr" {
        output = inline
      } else if type == "b" {
        output = value == "1" ? "TRUE" : "FALSE"
      }
      text += output + "\t"
      inCell = false
    }
    if local == "row" { text += "\n" }
  }
}
/// Small OOXML metadata parts use the same entity-rejecting XML parser as text.
final class OfficeProperties: NSObject, XMLParserDelegate {
  let fields: [String: String]
  var values = [String: String]()
  private var capture: String?, value = "", depth = 0, captureDepth = 0
  init(fields: [String: String]) { self.fields = fields }
  func parser(_ parser: XMLParser, didStartElement name: String, namespaceURI: String?,
    qualifiedName: String?, attributes: [String: String]) {
    depth += 1
    if depth > 32 { parser.abortParsing(); return }
    let key = name.split(separator: ":").last.map(String.init) ?? name
    if capture == nil, let field = fields[key] { capture = field; captureDepth = depth; value = "" }
  }
  func parser(_ parser: XMLParser, foundCharacters string: String) {
    guard capture != nil else { return }
    if value.utf8.count + string.utf8.count > 65_536 { parser.abortParsing() } else { value += string }
  }
  func parser(_ parser: XMLParser, foundCDATA data: Data) { self.parser(parser, foundCharacters: String(decoding: data, as: UTF8.self)) }
  func parser(_ parser: XMLParser, didEndElement name: String, namespaceURI: String?, qualifiedName: String?) {
    if depth == captureDepth, let capture {
      values[capture] = value.trimmingCharacters(in: .whitespacesAndNewlines); self.capture = nil
    }
    depth -= 1
  }
}
struct MediaProperties: Sendable {
  var duration: Double
  var width: Double?
  var height: Double?
  var tags: [String: String] = [:]
}
final class AsyncExtractionResult<T>: @unchecked Sendable {
  private let lock = NSLock()
  private var stored: Result<T, Error>?
  func set(_ result: Result<T, Error>) {
    lock.lock()
    stored = result
    lock.unlock()
  }
  func get() -> Result<T, Error>? {
    lock.lock()
    defer { lock.unlock() }
    return stored
  }
}
final class ContentIndexer {
  let engine: SearchEngine
  private let lock = NSLock()
  private var cancelled = false
  init(engine: SearchEngine) { self.engine = engine }
  func cancel() {
    lock.lock()
    cancelled = true
    lock.unlock()
  }
  func isCancelled() -> Bool {
    lock.lock()
    defer { lock.unlock() }
    return cancelled
  }
  func index(_ req: [String: Any]) -> [String: Any] {
    if req["preserve_cancellation"] as? Bool != true {
      lock.lock()
      cancelled = false
      lock.unlock()
    }
    var indexed = 0
    var skipped = [[String: Any]]()
    var warningMessages = [[String: Any]]()
    var lease: String?
    var lastRenewed = ProcessInfo.processInfo.systemUptime
    defer {
      if let lease { _ = engine.call(["op": "release_snapshot", "snapshot_lease": lease]) }
      // Publish successful extraction even when a later page is cancelled or
      // fails. The next query can reuse completed work.
      _ = engine.call(["op": "publish_content"])
    }
    func consume(_ paths: [String]) -> [String: Any]? {
      for path in paths {
        if isCancelled() { break }
        if let lease, ProcessInfo.processInfo.systemUptime - lastRenewed >= 30 {
          let renewed = engine.call(["op": "renew_snapshot", "snapshot_lease": lease])
          guard renewed["success"] as? Bool == true else { return renewed }
          lastRenewed = ProcessInfo.processInfo.systemUptime
        }
        do {
          let result = try extract(URL(fileURLWithPath: path))
          let expected: [String: Any] = [
            "file_id": result.1["indexed_file_id"] ?? 0,
            "device_id": result.1["indexed_device_id"] ?? 0,
            "size": result.1["indexed_size"] ?? 0,
            "modified_ns": result.1["indexed_modified_ns"] ?? 0,
            "changed_ns": result.1["indexed_changed_ns"] ?? 0,
          ]
          let stored = engine.call([
            "op": "put_content", "path": path, "text": result.0, "properties": result.1,
            "defer_publish": true, "expected": expected,
          ])
          if stored["success"] as? Bool != true {
            if let reason = stored["error"] as? String { throw ExtractionError(reason: reason) }
            throw LT("error.content_index_save")
          }
          indexed += 1
        } catch {
          var row: [String: Any] = ["path": path, "reason": error.localizedDescription]
          var warning: [String: Any] = ["path": path, "text": error.localizedDescription]
          if let text = error as? LocalizedText {
            row = text.adding(to: row, field: "reason")
            warning = text.wire; warning["path"] = path
          }
          skipped.append(row); warningMessages.append(warning)
        }
      }
      return nil
    }
    let explicitPaths = req["paths"] as? [String] ?? []
    if explicitPaths.isEmpty {
      let retained = engine.call(["op": "retain_snapshot"])
      guard retained["success"] as? Bool == true, let retainedLease = retained["snapshot_lease"] as? String else { return retained }
      lease = retainedLease
      var request: [String: Any] = [
        "op": req["metadata_candidates"] as? Bool == true ? "content_candidates" : "query",
        "text": req["text"] as? String ?? "file:", "limit": 1000, "offset": 0,
        "snapshot_lease": retainedLease,
      ]
      request["request_id"] = req["request_id"]
      var offset = 0
      while true {
        if isCancelled() { break }
        let result = engine.call(request)
        guard result["success"] as? Bool == true else { return result }
        let rows = result["rows"] as? [[String: Any]] ?? []
        let paths = rows.filter { row in
          guard row["is_dir"] as? Bool != true, row["is_symlink"] as? Bool != true else {
            return false
          }
          if req["only_missing"] as? Bool == true,
            let properties = row["properties"] as? [String: Any],
            let indexed = properties["indexed_modified_ns"] as? NSNumber,
            let modified = row["modified_ns"] as? NSNumber, indexed.int64Value == modified.int64Value,
            let indexedSize = properties["indexed_size"] as? NSNumber,
            let size = row["size"] as? NSNumber, indexedSize.int64Value == size.int64Value
          {
            return false
          }
          return true
        }.compactMap { $0["path"] as? String }
        if let failure = consume(paths) { return failure }
        offset += rows.count
        if rows.isEmpty || offset >= (result["total"] as? Int ?? 0) { break }
        request["offset"] = offset
        request["generation"] = result["generation"]
      }
    } else if let failure = consume(explicitPaths) {
      return failure
    }
    return LT("content.completed_summary", .integer(indexed), .integer(skipped.count),
      isCancelled() ? .localized(LT("status.cancelled_suffix")) : .text("")).adding(to: [
      "success": true, "count": indexed, "cancelled": isCancelled(),
      "warnings": skipped.map { "\($0["path"] as? String ?? ""): \($0["reason"] as? String ?? "")" },
      "skipped": skipped, "warning_messages": warningMessages,
    ])
  }
  func extract(_ url: URL) throws -> (String, [String: Any]) {
    var st = stat()
    guard url.path.withCString({ lstat($0, &st) }) == 0 else {
      throw LT("error.file_inaccessible")
    }
    guard (st.st_mode & S_IFMT) == S_IFREG else { throw LT("error.not_regular_file") }
    guard st.st_flags & UInt32(SF_DATALESS) == 0 else {
      throw LT("content.cloud_placeholder_skipped")
    }
    let cloud = try? url.resourceValues(forKeys: [
      .isUbiquitousItemKey, .ubiquitousItemDownloadingStatusKey,
    ])
    if cloud?.isUbiquitousItem == true && cloud?.ubiquitousItemDownloadingStatus == .notDownloaded {
      throw LT("content.icloud_not_downloaded")
    }
    guard st.st_size <= 128 * 1024 * 1024 else {
      throw LT("error.extraction_file_size_limit")
    }
    if isCancelled() { throw LT("status.cancelled") }
    let ext = url.pathExtension.lowercased()
    var props: [String: Any] = ["extension": ext]
    var text = ""
    props["kind"] = UTType(filenameExtension: ext)?.localizedDescription
    let textExtensions: Set<String> = [
      "txt", "md", "markdown", "rs", "swift", "c", "h", "cpp", "hpp", "m", "mm", "py", "js", "jsx",
      "ts", "tsx", "json", "yaml", "yml", "toml", "ini", "xml", "html", "css", "csv", "log", "sh",
      "zsh", "sql", "rb", "go", "java", "kt", "tex", "rst", "conf", "gitignore", "",
    ]
    if ext == "pdf" {
      guard let pdf = PDFDocument(url: url) else {
        throw LT("error.pdf_unreadable")
      }
      guard !pdf.isLocked else { throw LT("error.pdf_encrypted") }
      props["pages"] = pdf.pageCount
      if let attributes = pdf.documentAttributes {
        props["title"] = attributes[PDFDocumentAttribute.titleAttribute] as? String
        props["author"] = attributes[PDFDocumentAttribute.authorAttribute] as? String
        props["subject"] = attributes[PDFDocumentAttribute.subjectAttribute] as? String
        props["creator"] = attributes[PDFDocumentAttribute.creatorAttribute] as? String
        props["producer"] = attributes[PDFDocumentAttribute.producerAttribute] as? String
        if let keywords = attributes[PDFDocumentAttribute.keywordsAttribute] as? [String] {
          props["keywords"] = keywords.joined(separator: "; ")
        } else { props["keywords"] = attributes[PDFDocumentAttribute.keywordsAttribute] as? String }
      }
      for n in 0..<pdf.pageCount {
        if isCancelled() { throw LT("status.cancelled") }
        text += (pdf.page(at: n)?.string ?? "") + "\n"
        if text.utf8.count > 32 * 1024 * 1024 { throw LT("error.pdf_text_limit") }
      }
    } else if ["docx", "xlsx", "pptx"].contains(ext) {
      text = try extractOffice(url, ext: ext, properties: &props)
    } else if textExtensions.contains(ext) {
      guard st.st_size <= 32 * 1024 * 1024 else {
        throw LT("error.text_file_size_limit")
      }
      let data = try Data(contentsOf: url, options: [.mappedIfSafe])
      if let utf8 = String(data: data, encoding: .utf8) {
        text = utf8
      } else if let utf16 = String(data: data, encoding: .utf16) {
        text = utf16
      } else {
        throw LT("error.unsupported_text_encoding")
      }
      guard !text.contains("\0") else { throw LT("error.binary_text_file") }
    } else if let source = CGImageSourceCreateWithURL(url as CFURL, nil),
      let metadata = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [String: Any]
    {
      props["width"] = metadata[kCGImagePropertyPixelWidth as String]
      props["height"] = metadata[kCGImagePropertyPixelHeight as String]
      props["orientation"] = metadata[kCGImagePropertyOrientation as String]
      if let tiff = metadata[kCGImagePropertyTIFFDictionary as String] as? [String: Any] {
        props["cameramake"] = tiff[kCGImagePropertyTIFFMake as String]
        props["cameramodel"] = tiff[kCGImagePropertyTIFFModel as String]
        props["copyright"] = tiff[kCGImagePropertyTIFFCopyright as String]
      }
      if let exif = metadata[kCGImagePropertyExifDictionary as String] as? [String: Any] {
        props["exif"] = exif.filter { JSONSerialization.isValidJSONObject([$0.value]) }
        props["iso"] = (exif[kCGImagePropertyExifISOSpeedRatings as String] as? [NSNumber])?.first
        props["focallength"] = exif[kCGImagePropertyExifFocalLength as String]
        props["aperture"] = exif[kCGImagePropertyExifFNumber as String]
        props["exposuretime"] = exif[kCGImagePropertyExifExposureTime as String]
      }
    } else if ["mp4", "mov", "m4v", "mkv", "mp3", "m4a", "wav", "flac", "aiff"].contains(ext) {
      let media = try mediaProperties(url)
      if media.duration.isFinite { props["duration"] = media.duration }
      props["width"] = media.width
      props["height"] = media.height
      for (key, value) in media.tags { props[key] = value }
    } else {
      throw LT("error.unsupported_content_format")
    }
    guard text.utf8.count <= 32 * 1024 * 1024 else {
      throw LT("error.extracted_text_limit")
    }
    var after = stat()
    guard url.path.withCString({ lstat($0, &after) }) == 0,
      after.st_dev == st.st_dev, after.st_ino == st.st_ino, after.st_size == st.st_size,
      after.st_mtimespec.tv_sec == st.st_mtimespec.tv_sec,
      after.st_mtimespec.tv_nsec == st.st_mtimespec.tv_nsec,
      after.st_ctimespec.tv_sec == st.st_ctimespec.tv_sec,
      after.st_ctimespec.tv_nsec == st.st_ctimespec.tv_nsec
    else { throw LT("error.file_changed_during_extraction") }
    props["indexed_modified"] = Double(st.st_mtimespec.tv_sec)
    props["indexed_modified_ns"] = st.st_mtimespec.tv_sec * 1_000_000_000 + st.st_mtimespec.tv_nsec
    props["indexed_changed_ns"] = st.st_ctimespec.tv_sec * 1_000_000_000 + st.st_ctimespec.tv_nsec
    props["indexed_size"] = st.st_size
    props["indexed_file_id"] = st.st_ino
    props["indexed_device_id"] = UInt32(bitPattern: st.st_dev)
    return (text, props)
  }
  private func parseXML(_ data: Data, delegate: XMLParserDelegate) throws {
    // OOXML parts do not require DTDs. Reject them rather than resolving any
    // external/internal entity graph from a local untrusted document.
    let encodings: [String.Encoding] = [.utf8, .utf16, .utf16LittleEndian, .utf16BigEndian]
    if encodings.contains(where: { encoding in
      guard let text = String(data: data, encoding: encoding) else { return false }
      return text.range(of: "<!DOCTYPE", options: .caseInsensitive) != nil
        || text.range(of: "<!ENTITY", options: .caseInsensitive) != nil
    }) {
      throw LT("error.xml_dtd_unsupported")
    }
    let parser = XMLParser(data: data)
    parser.shouldResolveExternalEntities = false
    parser.delegate = delegate
    guard parser.parse() else { throw LT("error.xml_parse_or_limit") }
  }
  func extractOffice(_ url: URL, ext: String) throws -> String {
    var ignored = [String: Any]()
    return try extractOffice(url, ext: ext, properties: &ignored)
  }
  private func extractOffice(_ url: URL, ext: String, properties: inout [String: Any]) throws -> String {
    let names = try runUnzip(["-Z1", url.path], limit: 2 * 1024 * 1024)
    let all = String(decoding: names, as: UTF8.self).split(separator: "\n").map(String.init)
    guard all.count <= 10000 else { throw LT("error.archive_entry_limit") }
    guard Set(all).count == all.count else { throw LT("error.archive_duplicate_entries") }
    for (part, fields) in [
      ("docProps/core.xml", ["creator": "author", "title": "title", "subject": "subject", "keywords": "keywords"]),
      ("docProps/app.xml", ["Application": "creator", "Pages": "pages"])
    ] where all.contains(part) {
      if isCancelled() { throw LT("status.cancelled") }
      let metadata = OfficeProperties(fields: fields)
      try parseXML(runUnzip(["-p", url.path, part], limit: 1024 * 1024), delegate: metadata)
      for (key, value) in metadata.values where !value.isEmpty {
        if key == "pages" { if let pages = Int(value), pages >= 0 { properties[key] = pages } }
        else { properties[key] = value }
      }
    }
    let pattern: String
    switch ext {
    case "docx": pattern = #"^word/(document|header[0-9]+|footer[0-9]+)\.xml$"#
    case "xlsx": pattern = #"^xl/(sharedStrings|workbook|worksheets/sheet[0-9]+)\.xml$"#
    default: pattern = #"^ppt/slides/slide[0-9]+\.xml$"#
    }
    let regex = try NSRegularExpression(pattern: pattern)
    let selected = all.filter {
      regex.firstMatch(in: $0, range: NSRange($0.startIndex..., in: $0)) != nil
    }.sorted { $0.localizedStandardCompare($1) == .orderedAscending }
    guard selected.count <= 512 else { throw LT("error.document_part_limit") }
    if ext == "docx" && !selected.contains("word/document.xml") {
      throw LT("error.docx_missing_document")
    }
    if ext == "xlsx"
      && (!selected.contains("xl/workbook.xml")
        || !selected.contains(where: { $0.hasPrefix("xl/worksheets/") }))
    {
      throw LT("error.xlsx_missing_workbook")
    }
    if ext == "pptx" && selected.isEmpty { throw LT("error.pptx_missing_slides") }
    var shared = [String]()
    var text = ""
    if selected.contains("xl/sharedStrings.xml") {
      let strings = SpreadsheetStrings()
      try parseXML(
        runUnzip(["-p", url.path, "xl/sharedStrings.xml"], limit: 16 * 1024 * 1024),
        delegate: strings)
      shared = strings.strings
    }
    for name in selected where name != "xl/sharedStrings.xml" && name != "xl/workbook.xml" {
      if isCancelled() { throw LT("status.cancelled") }
      let bytes = try runUnzip(["-p", url.path, name], limit: 16 * 1024 * 1024)
      if ext == "xlsx" {
        let sheet = SpreadsheetSheet(shared: shared)
        try parseXML(bytes, delegate: sheet)
        if let failure = sheet.failure { throw failure }
        text += sheet.text + "\n"
      } else {
        let collector = XMLText()
        try parseXML(bytes, delegate: collector)
        text += collector.text + "\n"
      }
      if text.utf8.count > 32 * 1024 * 1024 { throw LT("error.document_text_limit") }
    }
    return text
  }
  func mediaProperties(_ url: URL) throws -> MediaProperties {
    let result = AsyncExtractionResult<MediaProperties>()
    let semaphore = DispatchSemaphore(value: 0)
    let task = Task.detached(priority: .utility) {
      do {
        let asset = AVURLAsset(url: url)
        let duration = try await asset.load(.duration)
        try Task.checkCancellation()
        let tracks = try await asset.loadTracks(withMediaType: .video)
        var properties = MediaProperties(
          duration: CMTimeGetSeconds(duration), width: nil, height: nil)
        let metadata = try await asset.load(.commonMetadata)
        for (key, name) in [
          (AVMetadataKey.commonKeyTitle, "title"), (.commonKeyArtist, "artist"),
          (.commonKeyAlbumName, "album"), (.commonKeyAuthor, "author"),
          (.commonKeyCreator, "creator"), (.commonKeySubject, "subject"),
          (.commonKeyCopyrights, "copyright"),
        ] {
          if let item = AVMetadataItem.metadataItems(
            from: metadata, withKey: key, keySpace: .common
          ).first,
            let value = try await item.load(.stringValue)
          {
            properties.tags[name] = value
          }
          try Task.checkCancellation()
        }
        if let track = tracks.first {
          let size = try await track.load(.naturalSize)
          let transform = try await track.load(.preferredTransform)
          let transformed = size.applying(transform)
          properties.width = abs(transformed.width)
          properties.height = abs(transformed.height)
        }
        result.set(.success(properties))
      } catch { result.set(.failure(error)) }
      semaphore.signal()
    }
    let deadline = Date().addingTimeInterval(15)
    while semaphore.wait(timeout: .now() + 0.05) == .timedOut {
      if isCancelled() || Date() > deadline {
        task.cancel()
        throw isCancelled() ? LT("status.cancelled") : LT("error.media_metadata_timeout")
      }
    }
    guard let completed = result.get() else { throw LT("error.media_metadata_incomplete") }
    return try completed.get()
  }
  func runUnzip(_ args: [String], limit: Int) throws -> Data {
    let task = Process()
    task.executableURL = URL(fileURLWithPath: "/usr/bin/unzip")
    task.arguments = args
    let pipe = Pipe()
    task.standardOutput = pipe
    task.standardError = FileHandle.nullDevice
    try task.run()
    let deadline = DispatchWorkItem { if task.isRunning { task.terminate() } }
    DispatchQueue.global().asyncAfter(deadline: .now() + 20, execute: deadline)
    defer {
      deadline.cancel()
      try? pipe.fileHandleForReading.close()
    }
    var out = Data()
    while true {
      let data = pipe.fileHandleForReading.availableData
      if data.isEmpty { break }
      out.append(data)
      if out.count > limit || isCancelled() {
        if task.isRunning { task.terminate() }
        throw LT("error.extraction_limit_or_cancelled")
      }
    }
    task.waitUntilExit()
    guard task.terminationStatus == 0 else { throw LT("error.archive_read_or_timeout") }
    return out
  }
}
