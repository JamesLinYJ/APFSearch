import AppKit
import DiskArbitration
import Foundation
import Security
import Darwin

final class ExportJob {
  let engine: SearchEngine
  let requestID: String
  private let lock = NSLock()
  private var stopped = false
  init(engine: SearchEngine, requestID: String) { self.engine = engine; self.requestID = requestID }
  func cancel() {
    lock.lock(); stopped = true; lock.unlock()
    _ = engine.call(["op": "cancel", "request_id": requestID])
  }
  var isCancelled: Bool { lock.lock(); defer { lock.unlock() }; return stopped }
}

/// Registered before queuing so cancellation also applies to pending imports.
final class ImportJob {
  private let lock = NSLock()
  private var stopped = false
  private var completed = false
  func cancel() { lock.lock(); if !completed { stopped = true }; lock.unlock() }
  var isCancelled: Bool { lock.lock(); defer { lock.unlock() }; return stopped }
  func publish<T>(_ body: () throws -> T) throws -> T {
    lock.lock(); defer { lock.unlock() }
    if stopped { throw LT("error.query_cancelled") }
    let result = try body(); completed = true; return result
  }
}

/// Write an export in bounded pages and publish it atomically without replacing
/// a file that another process created while the export was running.
final class FileListOutput {
  let temporary: URL
  let destination: URL
  private var handle: FileHandle?
  init(destination: URL) throws {
    self.destination = destination
    temporary = destination.deletingLastPathComponent().appendingPathComponent(".apfsearch-export-" + UUID().uuidString)
    let descriptor = temporary.path.withCString { open($0, O_CREAT | O_EXCL | O_WRONLY, S_IRUSR | S_IWUSR) }
    guard descriptor >= 0 else { throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno)) }
    handle = FileHandle(fileDescriptor: descriptor, closeOnDealloc: true)
  }
  deinit { try? handle?.close(); try? FileManager.default.removeItem(at: temporary) }
  func write(_ text: String) throws { try handle?.write(contentsOf: Data(text.utf8)) }
  func finish() throws {
    try handle?.synchronize(); try handle?.close(); handle = nil
    let result = temporary.path.withCString { from in
      destination.path.withCString { to in renameatx_np(AT_FDCWD, from, AT_FDCWD, to, UInt32(RENAME_EXCL)) }
    }
    guard result == 0 else { throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno)) }
  }
}

@_silgen_name("apfsearch_engine_open") func engineOpen(_ path: UnsafePointer<CChar>)
  -> UnsafeMutableRawPointer?
@_silgen_name("apfsearch_engine_call") func engineCall(
  _ engine: UnsafeMutableRawPointer, _ request: UnsafePointer<CChar>
) -> UnsafeMutablePointer<CChar>?
@_silgen_name("apfsearch_engine_free_string") func engineFree(_ s: UnsafeMutablePointer<CChar>)
@_silgen_name("apfsearch_engine_close") func engineClose(_ e: UnsafeMutableRawPointer)

final class SearchEngine {
  let pointer: UnsafeMutableRawPointer
  let directory: URL
  init(directory: URL? = nil, applicationSupportDirectory: URL? = nil) {
    if let directory { self.directory = directory }
    else if let applicationSupportDirectory {
      self.directory = applicationSupportDirectory.appendingPathComponent(ApplicationIdentity.dataDirectoryName, isDirectory: true)
    } else if let path = ProcessInfo.processInfo.environment[ApplicationIdentity.dataDirectoryEnvironment] {
      self.directory = URL(fileURLWithPath: path)
    } else {
      self.directory = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        .appendingPathComponent(ApplicationIdentity.dataDirectoryName, isDirectory: true)
    }
    try? FileManager.default.createDirectory(at: self.directory, withIntermediateDirectories: true)
    guard
      let ptr = self.directory.appendingPathComponent("index.sqlite").path.withCString({
        engineOpen($0)
      })
    else { fatalError("Could not open index") }
    pointer = ptr
  }
  init(importDirectory: URL) throws {
    directory = importDirectory
    guard let ptr = directory.appendingPathComponent("index.sqlite").path.withCString({ engineOpen($0) }) else {
      throw LT("error.file_list_import_failed")
    }
    pointer = ptr
  }
  deinit { engineClose(pointer) }
  func call(_ request: [String: Any]) -> [String: Any] {
    let data = jsonData(request)
    guard let s = String(data: data, encoding: .utf8),
      let result = s.withCString({ engineCall(pointer, $0) })
    else { return localizedErrorResponse(LT("error.empty_engine_response")) }
    defer { engineFree(result) }
    return jsonObject(Data(String(cString: result).utf8))
  }
}

final class SearchService: NSObject, NSXPCListenerDelegate, SearchServiceProtocol {
  let engine = SearchEngine()
  private(set) var files: FileOperations!
  var daSession: DASession?
  private let work = DispatchQueue(label: ApplicationIdentity.serviceIdentifier + ".auxiliary", qos: .utility)
  private let listLock = NSLock()
  private var lists = [String: SearchEngine]()
  private let contentWork = DispatchQueue(label: ApplicationIdentity.serviceIdentifier + ".content", qos: .utility)
  private let jobLock = NSLock()
  private var contentJobs = [String: ContentIndexer]()
  private var exportJobs = [String: ExportJob]()
  private var importJobs = [String: ImportJob]()
  override init() {
    super.init()
    files = FileOperations(directory: engine.directory)
    watchVolumes()
  }
  func watchVolumes() {
    guard let session = DASessionCreate(kCFAllocatorDefault) else { return }
    daSession = session
    let opaque = Unmanaged.passUnretained(self).toOpaque()
    DARegisterDiskAppearedCallback(
      session, nil,
      { _, context in
        guard let context = context else { return }
        Unmanaged<SearchService>.fromOpaque(context).takeUnretainedValue().volumeChanged()
      }, opaque)
    DARegisterDiskDisappearedCallback(
      session, nil,
      { _, context in
        guard let context = context else { return }
        Unmanaged<SearchService>.fromOpaque(context).takeUnretainedValue().volumeChanged()
      }, opaque)
    DASessionSetDispatchQueue(session, DispatchQueue.global(qos: .utility))
  }
  func volumeChanged() {
    // Scanner restats registered roots; detached roots become unavailable, not empty.
    _ = engine.call(["op": "volumes_changed"])
  }
  func listener(_ listener: NSXPCListener, shouldAcceptNewConnection connection: NSXPCConnection)
    -> Bool
  {
    guard connection.effectiveUserIdentifier == getuid() else { return false }
    var guest: SecCode?
    let attrs = [kSecGuestAttributePid: connection.processIdentifier] as CFDictionary
    guard SecCodeCopyGuestWithAttributes(nil, attrs, [], &guest) == errSecSuccess, let code = guest
    else { return false }
    var requirement: SecRequirement?
    guard let rule = ApplicationIdentity.clientSecurityRequirement,
      SecRequirementCreateWithString(rule as CFString, [], &requirement) == errSecSuccess,
      let requirement,
      SecCodeCheckValidity(code, [], requirement) == errSecSuccess
    else { return false }
    connection.exportedInterface = NSXPCInterface(with: SearchServiceProtocol.self)
    connection.exportedObject = self
    connection.resume()
    return true
  }
  func request(_ data: Data, withReply reply: @escaping (Data) -> Void) {
    let req = jsonObject(data)
    guard req["protocol_version"] as? Int == protocolVersion else {
      reply(jsonData(localizedErrorResponse(LT("error.unsupported_protocol"))))
      return
    }
    let op = req["op"] as? String ?? ""
    if op == "cancel", let id = req["request_id"] as? String {
      jobLock.lock()
      contentJobs[id]?.cancel()
      let export = exportJobs[id]
      let importing = importJobs[id]
      jobLock.unlock()
      importing?.cancel()
      export?.cancel()
      files.cancel(id)
    }
    if let id = req["list_id"] as? String {
      guard ["query", "cancel", "status", "export_list", "retain_snapshot", "renew_snapshot", "release_snapshot"].contains(op) else {
        reply(jsonData(localizedErrorResponse(LT("error.offline_list_read_only"))))
        return
      }
      // Opening an imported index can restore millions of records. Keep that
      // work off the XPC request handler and in the same QoS as its query.
      DispatchQueue.global(qos: .userInitiated).async {
        guard let list = self.offlineEngine(id) else {
          reply(jsonData(localizedErrorResponse(LT("error.offline_list_not_found"))))
          return
        }
        if op == "export_list" {
          self.scheduleExport(req, source: list, reply: reply)
          return
        }
        if (op == "query" || op == "retain_snapshot") && req["generation"] == nil && req["snapshot_lease"] == nil {
          self.syncOfflinePreferences(list)
        }
        reply(jsonData(list.call(req)))
      }
      return
    }
    if op == "files" {
      var request = req
      let id = request["request_id"] as? String ?? UUID().uuidString
      guard files.register(id) else {
        reply(jsonData(localizedErrorResponse(LT("files.busy")))); return
      }
      request["request_id"] = id
      work.async { reply(jsonData(self.files.perform(request))) }
      return
    }
    if op == "content_index" {
      let id = req["request_id"] as? String ?? UUID().uuidString
      let job = ContentIndexer(engine: engine)
      jobLock.lock()
      let previous = contentJobs.updateValue(job, forKey: id)
      jobLock.unlock()
      previous?.cancel()
      var request = req
      request["request_id"] = id; request["preserve_cancellation"] = true
      contentWork.async {
        let result = job.index(request)
        self.jobLock.lock()
        if self.contentJobs[id] === job { self.contentJobs.removeValue(forKey: id) }
        self.jobLock.unlock()
        reply(jsonData(result))
      }
      return
    }
    if op == "content_cancel" {
      jobLock.lock(); let jobs = contentJobs; jobLock.unlock()
      for (id, job) in jobs {
        job.cancel()
        _ = engine.call(["op": "cancel", "request_id": id])
      }
      reply(jsonData(["success": true]))
      return
    }
    if op == "export_list" {
      scheduleExport(req, source: engine, reply: reply)
      return
    }
    if op == "import_list" {
      let id = req["request_id"] as? String ?? UUID().uuidString
      let job = ImportJob()
      jobLock.lock()
      guard !id.isEmpty, id.utf8.count <= 128, importJobs.isEmpty else {
        jobLock.unlock(); reply(jsonData(localizedErrorResponse(LT("error.import_busy")))); return
      }
      importJobs[id] = job
      jobLock.unlock()
      work.async {
        let result = self.fileList(req, importing: job)
        self.jobLock.lock()
        if self.importJobs[id] === job { self.importJobs.removeValue(forKey: id) }
        self.jobLock.unlock()
        reply(jsonData(result))
      }
      return
    }
    if op == "volumes" {
      reply(jsonData(["success": true, "volumes": self.volumes()]))
      return
    }
    if op == "query" && req["generation"] == nil && req["snapshot_lease"] == nil {
      let plan = engine.call(["op": "query_plan", "text": req["text"] ?? ""])
      if plan["success"] as? Bool != true {
        reply(jsonData(plan))
        return
      }
      if plan["requires_extraction"] as? Bool == true {
        let id = req["request_id"] as? String ?? UUID().uuidString
        let job = ContentIndexer(engine: engine)
        jobLock.lock()
        contentJobs[id]?.cancel()
        contentJobs[id] = job
        jobLock.unlock()
        contentWork.async {
          let extraction = job.index([
            "text": req["text"] ?? "", "metadata_candidates": true, "only_missing": true,
            "preserve_cancellation": true, "request_id": id,
          ])
          var result: [String: Any]
          if job.isCancelled() {
            result = localizedErrorResponse(LT("error.query_cancelled"))
          } else if extraction["success"] as? Bool != true {
            result = extraction
          } else {
            result = self.engine.call(req)
            let coreWarnings = result["warnings"] as? [String] ?? []
            result["warnings"] = coreWarnings + (extraction["warnings"] as? [String] ?? [])
            result["warning_messages"] = coreWarnings.map { ["text": $0] }
              + (extraction["warning_messages"] as? [[String: Any]] ?? [])
            result["extracted_count"] = extraction["count"] ?? 0
          }
          self.jobLock.lock()
          if self.contentJobs[id] === job { self.contentJobs.removeValue(forKey: id) }
          self.jobLock.unlock()
          reply(jsonData(result))
        }
        return
      }
    }
    DispatchQueue.global(qos: .userInitiated).async { reply(jsonData(self.engine.call(req))) }
  }
  func offlineEngine(_ id: String) -> SearchEngine? {
    guard UUID(uuidString: id) != nil else { return nil }
    listLock.lock()
    defer { listLock.unlock() }
    if let existing = lists[id] { return existing }
    let directory = engine.directory.appendingPathComponent("Imported Lists", isDirectory: true)
      .appendingPathComponent(id, isDirectory: true)
    guard
      FileManager.default.fileExists(atPath: directory.appendingPathComponent("index.sqlite").path)
    else { return nil }
    let value = SearchEngine(directory: directory)
    lists[id] = value
    return value
  }
  func volumes() -> [[String: Any]] {
    let keys: Set<URLResourceKey> = [
      .volumeUUIDStringKey, .volumeNameKey, .volumeIsLocalKey, .volumeIsReadOnlyKey,
      .volumeIsBrowsableKey,
    ]
    let urls =
      FileManager.default.mountedVolumeURLs(
        includingResourceValuesForKeys: Array(keys), options: []) ?? []
    var result = [[String: Any]]()
    for url in urls {
      var stat = statfs()
      guard url.path.withCString({ statfs($0, &stat) }) == 0 else { continue }
      let fs = withUnsafePointer(to: &stat.f_fstypename) {
        $0.withMemoryRebound(to: CChar.self, capacity: 16) { String(cString: $0) }
      }
      guard fs == "apfs" else { continue }
      let p = url.path
      if p.hasPrefix("/System/Volumes/") && p != "/System/Volumes/Data" { continue }
      if p == "/System/Volumes/Data" { continue }  // logical root includes its firmlinks
      let v = try? url.resourceValues(forKeys: keys)
      guard v?.volumeIsLocal != false else { continue }
      result.append([
        "path": p, "name": v?.volumeName ?? p, "uuid": v?.volumeUUIDString ?? "", "filesystem": fs,
        "read_only": v?.volumeIsReadOnly ?? false,
      ])
    }
    return result
  }
  func syncOfflinePreferences(_ list: SearchEngine) {
    let preferences = engine.call(["op": "preferences", "action": "get"])
    _ = list.call([
      "op": "preferences", "action": "set",
      "values": ["macros": preferences["macros"] ?? [:], "exclusions": preferences["exclusions"] ?? []],
    ])
  }
  func scheduleExport(_ request: [String: Any], source: SearchEngine, reply: @escaping (Data) -> Void) {
    let id = request["request_id"] as? String ?? UUID().uuidString
    let job = ExportJob(engine: source, requestID: id)
    jobLock.lock()
    let previous = exportJobs.updateValue(job, forKey: id)
    jobLock.unlock()
    previous?.cancel()
    var request = request; request["request_id"] = id
    work.async {
      if source !== self.engine { self.syncOfflinePreferences(source) }
      let result = self.fileList(request, source: source, export: job)
      self.jobLock.lock()
      if self.exportJobs[id] === job { self.exportJobs.removeValue(forKey: id) }
      self.jobLock.unlock()
      reply(jsonData(result))
    }
  }
  func fileList(_ req: [String: Any], source: SearchEngine? = nil, export: ExportJob? = nil, importing: ImportJob? = nil) -> [String: Any] {
    let source = source ?? engine
    guard let path = req["path"] as? String else {
      return localizedErrorResponse(LT("error.missing_file_list_path"))
    }
    do {
      if req["op"] as? String == "export_list" {
        if export?.isCancelled == true { return localizedErrorResponse(LT("error.query_cancelled")) }
        guard !FileManager.default.fileExists(atPath: path) else {
          return localizedErrorResponse(LT("error.file_already_exists"))
        }
        var retain: [String: Any] = ["op": "retain_snapshot"]
        retain["generation"] = req["generation"]
        let retained = source.call(retain)
        guard retained["success"] as? Bool == true, let lease = retained["snapshot_lease"] as? String else { return retained }
        defer { _ = source.call(["op": "release_snapshot", "snapshot_lease": lease]) }
        var q = req
        q["op"] = "query"
        q["offset"] = 0
        q["limit"] = 10000
        q["snapshot_lease"] = lease
        let output = try FileListOutput(destination: URL(fileURLWithPath: path))
        try output.write("Filename,Size,Date Modified,Attributes\r\n")
        var count = 0
        while true {
          if export?.isCancelled == true { return localizedErrorResponse(LT("error.query_cancelled")) }
          let page = source.call(q)
          guard page["success"] as? Bool == true else {
            if export?.isCancelled == true { return localizedErrorResponse(LT("error.query_cancelled")) }
            return page
          }
          let rows = page["rows"] as? [[String: Any]] ?? []
          var buffer = ""
          buffer.reserveCapacity(rows.count * 160)
          for r in rows {
            if export?.isCancelled == true { return localizedErrorResponse(LT("error.query_cancelled")) }
            let p = (r["path"] as? String ?? "").replacingOccurrences(of: "\"", with: "\"\"")
            let seconds = (r["modified"] as? NSNumber)?.int64Value ?? 0
            let (epoch, additionOverflow) = max(-11_644_473_600, seconds).addingReportingOverflow(11_644_473_600)
            let (filetime, multiplyOverflow) = epoch.multipliedReportingOverflow(by: 10_000_000)
            guard !additionOverflow, !multiplyOverflow else {
              throw LT("error.filetime_out_of_range")
            }
            buffer += "\"\(p)\",\(r["size"] ?? 0),\(filetime),\((r["is_dir"] as? Bool == true) ? 16:0)\r\n"
          }
          try output.write(buffer)
          count += rows.count
          if rows.isEmpty || count >= (page["total"] as? Int ?? 0) { break }
          q["offset"] = count
          q["generation"] = page["generation"]
        }
        if export?.isCancelled == true { return localizedErrorResponse(LT("error.query_cancelled")) }
        try output.finish()
        return LT("export.completed_count", .integer(count)).adding(to: ["success": true, "count": count])
      }
      return try importList(path: path, job: importing ?? ImportJob())
    } catch { return localizedErrorResponse(error) }
  }
  private func importList(path: String, job: ImportJob) throws -> [String: Any] {
    let saved = engine.directory.appendingPathComponent("Imported Lists", isDirectory: true)
    let id = UUID().uuidString
    let temporary = saved.appendingPathComponent(".import-" + id, isDirectory: true)
    let destination = saved.appendingPathComponent(id, isDirectory: true)
    var list: SearchEngine?
    // Drop the FFI handle before removing its SQLite files, on every exit path.
    defer { list = nil; try? FileManager.default.removeItem(at: temporary) }
    var column: Int?, sizeColumn: Int?, dateColumn: Int?, attributesColumn: Int?
    var count = 0, batchBytes = 0
    var batch = [[String: Any]]()
    batch.reserveCapacity(4096)
    func call(_ operation: String, rows: [[String: Any]]? = nil) throws {
      if job.isCancelled { throw LT("error.query_cancelled") }
      var request: [String: Any] = ["op": operation, "request_id": id]
      request["rows"] = rows
      guard let response = list?.call(request) else { throw LT("error.file_list_import_failed") }
      guard response["success"] as? Bool == true else {
        let reason = response["error"] as? String ?? ""
        if reason == "The file list exceeds the supported import limits." { throw LT("error.file_list_limit") }
        if reason == "Query cancelled" { throw LT("error.query_cancelled") }
        throw NSError(domain: "FileListImport", code: 1, userInfo: [NSLocalizedDescriptionKey: reason])
      }
    }
    func flush() throws {
      if batch.isEmpty { return }
      try call("append_file_list_import", rows: batch)
      batch.removeAll(keepingCapacity: true); batchBytes = 0
    }
    try CSVReader.read(path: path, cancelled: { job.isCancelled }) { record in
      if column == nil {
        guard let pathColumn = record.firstIndex(where: { $0.lowercased() == "filename" || $0.lowercased() == "path" }) else {
          throw LT("error.missing_filename_column")
        }
        column = pathColumn
        sizeColumn = record.firstIndex(where: { $0.lowercased() == "size" })
        dateColumn = record.firstIndex(where: { $0.lowercased() == "date modified" || $0.lowercased() == "modified" })
        attributesColumn = record.firstIndex(where: { $0.lowercased() == "attributes" })
        // No database or UUID list exists until a valid bounded header is read.
        try FileManager.default.createDirectory(at: saved, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: temporary, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
        list = try SearchEngine(importDirectory: temporary)
        try call("begin_file_list_import")
        return
      }
      guard let column, record.indices.contains(column), !record[column].isEmpty else { return }
      let path = record[column]
      func value(_ index: Int?) -> String {
        guard let index, record.indices.contains(index) else { return "" }
        return record[index]
      }
      let name = path.replacingOccurrences(of: "\\", with: "/").split(separator: "/").last.map(String.init) ?? path
      // Conservative UTF-8 case expansion allowance for Rust's extension field.
      let bytes = path.utf8.count + name.utf8.count * 4
      if batch.count == 4096 || batchBytes + bytes > 1024 * 1024 { try flush() }
      let rawDate = Int64(value(dateColumn)) ?? 0
      let modified = rawDate > 100_000_000_000 ? rawDate / 10_000_000 - 11_644_473_600 : rawDate
      batch.append(["path": path, "name": name, "size": UInt64(value(sizeColumn)) ?? 0,
        "modified": modified, "is_dir": (UInt64(value(attributesColumn)) ?? 0) & 16 != 0, "offline": true])
      batchBytes += bytes; count += 1
    }
    guard column != nil else { throw LT("error.missing_filename_column") }
    try flush(); try call("finish_file_list_import")
    let page = list!.call(["op": "query", "text": "", "limit": 200])
    list = nil
    return try job.publish {
      let result = temporary.path.withCString { from in
        destination.path.withCString { to in renameatx_np(AT_FDCWD, from, AT_FDCWD, to, UInt32(RENAME_EXCL)) }
      }
      guard result == 0 else { throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno)) }
      do {
        let published = try SearchEngine(importDirectory: destination)
        listLock.lock(); lists[id] = published; listLock.unlock()
      } catch { try? FileManager.default.removeItem(at: destination); throw error }
      return LT("import.completed_count", .integer(count)).adding(to: [
        "success": true, "list_id": id, "rows": page["rows"] ?? [], "count": count,
        "total": count, "offline": true,
      ])
    }
  }

}

/// CSV syntax is byte-oriented; UTF-8 is decoded strictly only after a complete
/// bounded field is available. Neither chunks nor rows split quoted newlines.
struct CSVLimits {
  var bytes = 1024 * 1024 * 1024
  var rows = 2_000_001 // header plus data records, including skipped empty rows
  var fieldBytes = 64 * 1024
  var recordBytes = 1024 * 1024
  var columns = 256
}
struct CSVReader {
  let limits: CSVLimits
  let cancelled: () -> Bool
  let onRow: ([String]) throws -> Void
  private var field = [UInt8]()
  private var row = [String]()
  private var prefix = [UInt8]()
  private var started = false
  private var quoted = false
  private var afterQuote = false
  private var skipLF = false
  private var pending = false
  private var total = 0
  private var recordBytes = 0
  private var rows = 0

  init(limits: CSVLimits = CSVLimits(), cancelled: @escaping () -> Bool = { false }, onRow: @escaping ([String]) throws -> Void) {
    self.limits = limits; self.cancelled = cancelled; self.onRow = onRow
  }
  mutating func feed(_ data: Data) throws {
    guard data.count <= limits.bytes - total else { throw LT("error.file_list_limit") }
    total += data.count
    // One cancellation check per bounded input chunk, plus every emitted row.
    if cancelled() { throw LT("error.query_cancelled") }
    for byte in data {
      if !started {
        prefix.append(byte)
        if prefix.count == 3 {
          started = true
          if prefix != [0xef, 0xbb, 0xbf] { for byte in prefix { try consume(byte) } }
          prefix.removeAll(keepingCapacity: false)
        }
      } else { try consume(byte) }
    }
  }
  private mutating func append(_ byte: UInt8) throws {
    guard field.count < limits.fieldBytes else { throw LT("error.file_list_limit") }
    field.append(byte)
  }
  private mutating func endField() throws {
    guard row.count < limits.columns else { throw LT("error.file_list_limit") }
    guard let value = String(bytes: field, encoding: .utf8) else { throw LT("error.invalid_csv_utf8") }
    row.append(value); field.removeAll(keepingCapacity: true); afterQuote = false
  }
  private mutating func endRow() throws {
    try endField()
    guard rows < limits.rows else { throw LT("error.file_list_limit") }
    if cancelled() { throw LT("error.query_cancelled") }
    rows += 1
    try onRow(row)
    row.removeAll(keepingCapacity: true); pending = false; recordBytes = 0
  }
  private mutating func consume(_ byte: UInt8) throws {
    if skipLF { skipLF = false; if byte == 10 { return } }
    guard recordBytes < limits.recordBytes else { throw LT("error.file_list_limit") }
    recordBytes += 1
    if quoted {
      if byte == 34 { quoted = false; afterQuote = true } else { try append(byte) }
      return
    }
    if afterQuote && byte == 34 { try append(byte); quoted = true; afterQuote = false; return }
    if byte == 44 { try endField(); pending = true; return }
    if byte == 10 || byte == 13 { try endRow(); skipLF = byte == 13; return }
    if afterQuote { throw LT("error.invalid_csv_quote") }
    if byte == 34 {
      guard field.isEmpty else { throw LT("error.invalid_csv_quote") }
      quoted = true; pending = true
    } else { try append(byte); pending = true }
  }
  mutating func finish() throws {
    if !started { for byte in prefix { try consume(byte) }; prefix.removeAll() }
    if cancelled() { throw LT("error.query_cancelled") }
    if quoted { throw LT("error.unterminated_csv_quote") }
    if pending || !row.isEmpty || !field.isEmpty { try endRow() }
  }
  static func read(path: String, limits: CSVLimits = CSVLimits(), cancelled: @escaping () -> Bool = { false }, onRow: @escaping ([String]) throws -> Void) throws {
    if cancelled() { throw LT("error.query_cancelled") }
    // NONBLOCK prevents a selected FIFO/device from hanging before fstat. Once
    // verified, all content reads use this descriptor, including symlink inputs.
    let descriptor = path.withCString { open($0, O_RDONLY | O_NONBLOCK | O_CLOEXEC) }
    guard descriptor >= 0 else { throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno)) }
    defer { close(descriptor) }
    var before = stat()
    guard fstat(descriptor, &before) == 0 else { throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno)) }
    guard before.st_mode & S_IFMT == S_IFREG else { throw LT("error.file_list_not_regular") }
    guard before.st_size >= 0, before.st_size <= limits.bytes else { throw LT("error.file_list_limit") }
    var reader = CSVReader(limits: limits, cancelled: cancelled, onRow: onRow)
    var buffer = [UInt8](repeating: 0, count: 64 * 1024)
    while true {
      if cancelled() { throw LT("error.query_cancelled") }
      let count = buffer.withUnsafeMutableBytes { Darwin.read(descriptor, $0.baseAddress!, $0.count) }
      if count < 0 {
        if errno == EINTR { continue }
        throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno))
      }
      if count == 0 { break }
      try reader.feed(Data(buffer.prefix(count)))
    }
    try reader.finish()
    var after = stat()
    guard fstat(descriptor, &after) == 0 else { throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno)) }
    guard before.st_dev == after.st_dev, before.st_ino == after.st_ino,
      before.st_size == after.st_size, before.st_mtimespec.tv_sec == after.st_mtimespec.tv_sec,
      before.st_mtimespec.tv_nsec == after.st_mtimespec.tv_nsec,
      before.st_ctimespec.tv_sec == after.st_ctimespec.tv_sec, before.st_ctimespec.tv_nsec == after.st_ctimespec.tv_nsec
    else { throw LT("error.file_list_changed") }
  }
  // Compatibility for bounded in-memory callers and export-format tests.
  static func parse(_ text: String) throws -> [[String]] {
    var rows = [[String]]()
    var reader = CSVReader { rows.append($0) }
    try reader.feed(Data(text.utf8)); try reader.finish()
    return rows
  }
}

#if !TEST_BUILD
  @main struct IndexerMain {
    static func main() {
      let service = SearchService()
      let listener = NSXPCListener(machServiceName: serviceName)
      listener.delegate = service
      listener.resume()
      RunLoop.current.run()
    }
  }

#endif
