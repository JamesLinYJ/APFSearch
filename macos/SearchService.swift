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

/// Write an export in bounded pages and publish it atomically without replacing
/// a file that another process created while the export was running.
final class FileListOutput {
  let temporary: URL
  let destination: URL
  private var handle: FileHandle?
  init(destination: URL) throws {
    self.destination = destination
    temporary = destination.deletingLastPathComponent().appendingPathComponent(".filesearch-export-" + UUID().uuidString)
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

@_silgen_name("filesearch_engine_open") func engineOpen(_ path: UnsafePointer<CChar>)
  -> UnsafeMutableRawPointer?
@_silgen_name("filesearch_engine_call") func engineCall(
  _ engine: UnsafeMutableRawPointer, _ request: UnsafePointer<CChar>
) -> UnsafeMutablePointer<CChar>?
@_silgen_name("filesearch_engine_free_string") func engineFree(_ s: UnsafeMutablePointer<CChar>)
@_silgen_name("filesearch_engine_close") func engineClose(_ e: UnsafeMutableRawPointer)

final class SearchEngine {
  let pointer: UnsafeMutableRawPointer
  let directory: URL
  init(directory: URL? = nil) {
    do {
      if let directory { self.directory = directory }
      else if let path = ProcessInfo.processInfo.environment[ApplicationIdentity.dataDirectoryEnvironment] {
        self.directory = URL(fileURLWithPath: path)
      } else {
        self.directory = try LegacyDataMigration.dataDirectory(
          in: FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0])
      }
    } catch { fatalError("Could not migrate existing index: \(error.localizedDescription)") }
    try? FileManager.default.createDirectory(at: self.directory, withIntermediateDirectories: true)
    guard
      let ptr = self.directory.appendingPathComponent("index.sqlite").path.withCString({
        engineOpen($0)
      })
    else { fatalError("Could not open index") }
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
  lazy var files = FileOperations(directory: engine.directory)
  var daSession: DASession?
  private let work = DispatchQueue(label: ApplicationIdentity.serviceIdentifier + ".auxiliary", qos: .utility)
  private let listLock = NSLock()
  private var lists = [String: SearchEngine]()
  private let contentWork = DispatchQueue(label: ApplicationIdentity.serviceIdentifier + ".content", qos: .utility)
  private let jobLock = NSLock()
  private var contentJobs = [String: ContentIndexer]()
  private var exportJobs = [String: ExportJob]()
  override init() {
    super.init()
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
      jobLock.unlock()
      export?.cancel()
    }
    if let id = req["list_id"] as? String {
      guard ["query", "cancel", "status", "export_list", "retain_snapshot", "renew_snapshot", "release_snapshot"].contains(op) else {
        reply(jsonData(localizedErrorResponse(LT("error.offline_list_read_only"))))
        return
      }
      guard let list = offlineEngine(id) else {
        reply(jsonData(localizedErrorResponse(LT("error.offline_list_not_found"))))
        return
      }
      if op == "export_list" {
        scheduleExport(req, source: list, reply: reply)
        return
      }
      DispatchQueue.global(qos: .userInitiated).async {
        if (op == "query" || op == "retain_snapshot") && req["generation"] == nil && req["snapshot_lease"] == nil {
          self.syncOfflinePreferences(list)
        }
        reply(jsonData(list.call(req)))
      }
      return
    }
    if op == "files" {
      work.async { reply(jsonData(self.files.perform(req))) }
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
      work.async { reply(jsonData(self.fileList(req))) }
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
  func fileList(_ req: [String: Any], source: SearchEngine? = nil, export: ExportJob? = nil) -> [String: Any] {
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
      let text = try String(contentsOfFile: path, encoding: .utf8)
      let records = try CSVReader.parse(text)
      guard let header = records.first,
        let col = header.firstIndex(where: {
          $0.lowercased() == "filename" || $0.lowercased() == "path"
        })
      else { return localizedErrorResponse(LT("error.missing_filename_column")) }
      var rows = [[String: Any]]()
      let sizeColumn = header.firstIndex(where: { $0.lowercased() == "size" })
      let dateColumn = header.firstIndex(where: {
        $0.lowercased() == "date modified" || $0.lowercased() == "modified"
      })
      let attributesColumn = header.firstIndex(where: { $0.lowercased() == "attributes" })
      for rec in records.dropFirst() where rec.count > col {
        let p = rec[col]
        if p.isEmpty { continue }
        func value(_ column: Int?) -> String {
          guard let column = column, rec.indices.contains(column) else { return "" }
          return rec[column]
        }
        let rawDate = Int64(value(dateColumn)) ?? 0
        let modified = rawDate > 100_000_000_000 ? rawDate / 10_000_000 - 11_644_473_600 : rawDate
        let attributes = UInt64(value(attributesColumn)) ?? 0
        rows.append([
          "path": p,
          "name": p.replacingOccurrences(of: "\\", with: "/").split(separator: "/").last.map(
            String.init) ?? p, "size": UInt64(value(sizeColumn)) ?? 0, "modified": modified,
          "is_dir": attributes & 16 != 0, "offline": true,
        ])
      }
      let saved = engine.directory.appendingPathComponent("Imported Lists", isDirectory: true)
      try FileManager.default.createDirectory(at: saved, withIntermediateDirectories: true)
      let id = UUID().uuidString
      let list = SearchEngine(directory: saved.appendingPathComponent(id, isDirectory: true))
      let imported = list.call(["op": "import_file_list", "rows": rows])
      guard imported["success"] as? Bool == true else { return imported }
      listLock.lock()
      lists[id] = list
      listLock.unlock()
      let page = list.call(["op": "query", "text": "", "limit": 200])
      return LT("import.completed_count", .integer(rows.count)).adding(to: [
        "success": true, "list_id": id, "rows": page["rows"] ?? [], "count": rows.count,
        "total": rows.count, "offline": true,
      ])
    } catch { return localizedErrorResponse(error) }
  }
}

struct CSVReader {
  static func parse(_ text: String) throws -> [[String]] {
    var rows = [[String]]()
    var row = [String]()
    var field = ""
    var quoted = false
    var chars = text.unicodeScalars.map { Character(String($0)) }
    if chars.first == "\u{feff}" { chars.removeFirst() }
    var i = 0
    while i < chars.count {
      let c = chars[i]
      if c == "\"" {
        if quoted && i + 1 < chars.count && chars[i + 1] == "\"" {
          field.append("\"")
          i += 1
        } else {
          quoted.toggle()
        }
      } else if c == "," && !quoted {
        row.append(field)
        field = ""
      } else if (c == "\n" || c == "\r") && !quoted {
        row.append(field)
        rows.append(row)
        field = ""
        row = []
        if c == "\r" && i + 1 < chars.count && chars[i + 1] == "\n" { i += 1 }
      } else {
        field.append(c)
      }
      i += 1
    }
    if quoted {
      throw LT("error.unterminated_csv_quote")
    }
    if !field.isEmpty || !row.isEmpty {
      row.append(field)
      rows.append(row)
    }
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
