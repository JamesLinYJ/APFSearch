import AppKit
import CryptoKit
import Darwin
import Foundation
import PDFKit

@main struct FeatureTests {
  static func main() throws {
    let root = FileManager.default.temporaryDirectory.appendingPathComponent("FileSearch-features-" + UUID().uuidString)
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: false)
    defer { try? FileManager.default.removeItem(at: root) }
    setenv("FILESEARCH_DATA_DIR", root.appendingPathComponent("engine").path, 1)
    var passed = [String](), failures = [String]()
    func check(_ name: String, _ condition: @autoclosure () throws -> Bool) rethrows {
      if try condition() { passed.append(name) } else { failures.append(name) }
    }
    func fixture(_ name: String, _ content: String = "fixture") throws -> URL {
      let url = root.appendingPathComponent(name)
      try Data(content.utf8).write(to: url); return url
    }
    let files = FileOperations(directory: root)
    let a = try fixture("alpha.txt"), b = try fixture("beta.txt")
    let rename: [String: Any] = ["action": "rename", "paths": [a.path, b.path],
      "rename_rule": ["prefix": "new-", "number_start": 7, "number_padding": 3]]
    var previewRequest = rename; previewRequest["dry_run"] = true
    let preview = files.perform(previewRequest)
    let planned = preview["preview"] as? [[String: Any]] ?? []
    check("batch rename preview preserves extension and sequence", planned.count == 2 && (planned[0]["destination"] as? String)?.hasSuffix("new-alpha 007.txt") == true && (planned[1]["destination"] as? String)?.hasSuffix("new-beta 008.txt") == true)
    check("dry run has no journal or filesystem mutations", !FileManager.default.fileExists(atPath: files.journal.path) && FileManager.default.fileExists(atPath: a.path))
    var approved = rename; approved["expected"] = preview["expected"]
    approved["skip_paths"] = [a.path]
    let renamed = files.perform(approved)
    check("skipped rename does not renumber the other file", renamed["completed"] as? Int == 1 && FileManager.default.fileExists(atPath: root.appendingPathComponent("new-beta 008.txt").path) && FileManager.default.fileExists(atPath: a.path))
    let history = files.perform(["action": "history", "limit": 1])
    check("history is paginated and contains completed identities", (history["operations"] as? [[String: Any]])?.first?["kind"] as? String == "operation" && history["total"] as? Int == 1)
    check("batch rename can be undone without replacing an existing target", files.undo()["success"] as? Bool == true && FileManager.default.fileExists(atPath: b.path))

    // Namespace changes update ctime for every alias of an inode. They must not
    // make the rest of this batch, or its undo history, look externally modified.
    let linkDirectory = root.appendingPathComponent("hardlinks")
    try FileManager.default.createDirectory(at: linkDirectory, withIntermediateDirectories: false)
    let linkA = linkDirectory.appendingPathComponent("a.txt")
    let linkB = linkDirectory.appendingPathComponent("b.txt")
    try Data("hardlink fixture".utf8).write(to: linkA)
    try FileManager.default.linkItem(at: linkA, to: linkB)
    let linkFiles = FileOperations(directory: linkDirectory)
    let linkBatch = linkFiles.perform(["action": "rename", "paths": [linkA.path, linkB.path],
      "new_names": ["renamed-a.txt", "renamed-b.txt"]])
    check("batch rename processes both hard-link directory entries", linkBatch["success"] as? Bool == true && linkBatch["completed"] as? Int == 2)
    let renamedA = linkDirectory.appendingPathComponent("renamed-a.txt")
    let renamedB = linkDirectory.appendingPathComponent("renamed-b.txt")
    try check("renaming hard links preserves their shared object", OperationIdentity(renamedA).inode == OperationIdentity(renamedB).inode)
    // Reopen the journal: in-memory adjustments alone cannot preserve undo.
    let reopenedLinks = FileOperations(directory: linkDirectory)
    check("undo latest hard-link rename after journal reopen", reopenedLinks.undo()["success"] as? Bool == true)
    check("undo earlier alias after the latest undo updates ctime", reopenedLinks.undo()["success"] as? Bool == true)
    try check("both restored aliases retain the original content", Data(contentsOf: linkA) == Data(contentsOf: linkB))
    let nextRename = reopenedLinks.perform(["action": "rename", "paths": [linkA.path], "new_name": "renamed-a.txt"])
    check("independent hard-link batch succeeds", nextRename["success"] as? Bool == true)
    let linkStamp = try OperationIdentity(renamedA)
    usleep(20_000)
    try Data("modified fixture".utf8).write(to: linkB)
    var linkTimes = [timespec(tv_sec: Int(linkStamp.modified), tv_nsec: Int(linkStamp.modifiedNS)),
      timespec(tv_sec: Int(linkStamp.modified), tv_nsec: Int(linkStamp.modifiedNS))]
    let linkTimeResult = linkB.path.withCString { path in
      linkTimes.withUnsafeMutableBufferPointer { utimensat(AT_FDCWD, path, $0.baseAddress, 0) }
    }
    check("external alias edit cannot be hidden by restoring mtime", linkTimeResult == 0 && reopenedLinks.undo()["success"] as? Bool == false)

    let dangling = linkDirectory.appendingPathComponent("dangling-link")
    try FileManager.default.createSymbolicLink(atPath: dangling.path, withDestinationPath: "missing-target")
    let movedLink = reopenedLinks.perform(["action": "rename", "paths": [dangling.path], "new_name": "renamed-link"])
    try check("metadata handle renames the dangling symlink itself", movedLink["success"] as? Bool == true
      && FileManager.default.destinationOfSymbolicLink(atPath: linkDirectory.appendingPathComponent("renamed-link").path) == "missing-target")
    try check("undo restores the dangling symlink without following it", reopenedLinks.undo()["success"] as? Bool == true
      && FileManager.default.destinationOfSymbolicLink(atPath: dangling.path) == "missing-target")

    let missingConflictPath = root.appendingPathComponent("missing中文%@.txt").path
    let missingRequest: [String: Any] = ["action": "copy", "paths": [missingConflictPath], "destination": root.path]
    var missingPreview = missingRequest; missingPreview["dry_run"] = true
    for response in [files.perform(missingPreview), files.perform(missingRequest)] {
      let message = (response["conflict_messages"] as? [[String: Any]])?.first
      check("top-level conflict protocol preserves stable keys and opaque paths", message?["key"] as? String == "error.file_not_found"
        && (message?["args"] as? [[String: Any]])?.first?["value"] as? String == missingConflictPath)
      if response["success"] as? Bool == false {
        check("failed batch exposes structured errors for the client locale", (response["error_messages"] as? [[String: Any]])?.first?["key"] as? String == "error.file_not_found")
      }
    }
    let beforeNoop = try Data(contentsOf: files.journal)
    check("rename to same name is a no-op", files.perform(["action": "rename", "paths": [a.path], "new_name": a.lastPathComponent])["completed"] as? Int == 0)
    try check("no-op does not append journal rows", Data(contentsOf: files.journal) == beforeNoop)

    let existing = try fixture("occupied.txt", "do not replace")
    let conflict = files.perform(["action": "rename", "paths": [a.path], "new_name": existing.lastPathComponent])
    try check("existing destination is never overwritten", conflict["success"] as? Bool == false && String(contentsOf: existing, encoding: .utf8) == "do not replace")
    let collision = files.perform(["action": "rename", "paths": [a.path, b.path], "new_names": ["same.txt", "same.txt"], "dry_run": true])
    check("every conflicting target owner is marked", (collision["preview"] as? [[String: Any]] ?? []).allSatisfy { !($0["conflicts"] as? [String] ?? []).isEmpty })
    let duplicates = files.perform(["action": "rename", "paths": [a.path, a.path], "new_names": ["x.txt", "y.txt"]])
    check("duplicate source paths are rejected", duplicates["success"] as? Bool == false)
    let old = try OperationIdentity(a)
    usleep(20_000)
    try Data("changed".utf8).write(to: a)
    var times = [timespec(tv_sec: Int(old.modified), tv_nsec: Int(old.modifiedNS)), timespec(tv_sec: Int(old.modified), tv_nsec: Int(old.modifiedNS))]
    let restored = a.path.withCString { path in times.withUnsafeMutableBufferPointer { utimensat(AT_FDCWD, path, $0.baseAddress, 0) } }
    check("fixture restores the exact nanosecond mtime", restored == 0 && (try? OperationIdentity(a).modifiedNS) == old.modifiedNS)
    let stale = files.perform(["action": "rename", "paths": [a.path], "new_name": "stale.txt", "expected": [old.wire.merging(["path": a.path]) { _, new in new }]])
    check("restored mtime cannot authorize changed source bytes", stale["success"] as? Bool == false && FileManager.default.fileExists(atPath: a.path))
    let keeper = try fixture("keeper.txt"), remove = try fixture("extra.txt")
    let keeperStamp = try OperationIdentity(keeper)
    try Data("replaced".utf8).write(to: keeper, options: .atomic)
    let protected = files.perform(["action": "rename", "paths": [remove.path], "new_name": "should-not-run.txt",
      "keepers": [keeperStamp.wire.merging(["path": keeper.path, "group": "1"]) { _, new in new }], "groups": [remove.path: "1"]])
    check("duplicate cleanup requires unchanged survivor", protected["success"] as? Bool == false && FileManager.default.fileExists(atPath: remove.path))
    check("queued file job registers", files.register("queued"))
    files.cancel("queued")
    let cancelled = files.perform(["request_id": "queued", "action": "rename", "paths": [b.path], "new_name": "cancelled.txt"])
    check("pre-start cancellation performs no mutation", cancelled["success"] as? Bool == false && FileManager.default.fileExists(atPath: b.path))
    check("cancellation registration is released", files.register("queued"))
    _ = files.perform(["action": "history", "request_id": "queued"])
    check("number overflow fails without mutation", files.perform(["action": "rename", "paths": [a.path, b.path], "rename_rule": ["number_start": Int64.max]])["success"] as? Bool == false)

    let logRoot = root.appendingPathComponent("torn-log")
    try FileManager.default.createDirectory(at: logRoot, withIntermediateDirectories: false)
    let journal = OperationJournal(directory: logRoot)
    var handle = try journal.open()
    try journal.append(["id": "pending", "kind": "intent", "source": a.path, "destination": "unconfirmed"], to: handle)
    try handle.synchronize(); try handle.write(contentsOf: Data("{\"partial\":".utf8)); try handle.close()
    handle = try journal.open()
    try journal.append(["id": "later", "kind": "operation", "undone": false], to: handle)
    try handle.synchronize(); try handle.close()
    let recovered = try journal.history()
    check("torn tail never swallows the next record or fabricates completion", recovered.count == 2 && recovered[0]["kind"] as? String == "intent" && recovered[1]["id"] as? String == "later")
    let symlinkRoot = root.appendingPathComponent("journal-link")
    try FileManager.default.createDirectory(at: symlinkRoot, withIntermediateDirectories: false)
    try FileManager.default.createSymbolicLink(at: symlinkRoot.appendingPathComponent("file-operations.jsonl"), withDestinationURL: existing)
    let unsafeJournal = FileOperations(directory: symlinkRoot)
    check("symlink journal refuses mutation", unsafeJournal.perform(["action": "rename", "paths": [b.path], "new_name": "symlink-test.txt"])["success"] as? Bool == false && FileManager.default.fileExists(atPath: b.path))

    typealias Row = [String: Any]
    let selected = IndexSet([1, 1500, 2201])
    var calls = [Int](), resolved: Result<[Row], Error>?
    let request: Row = ["snapshot_lease": "fixture-lease", "generation": 9, "text": "fixed:", "sort": [["field": "name", "ascending": true]]]
    let resolver = SelectionResolver(selected: selected, cached: [1: ["path": "/fixture/1"]], total: 2400, request: request, send: { req, reply in
      let offset = req["offset"] as! Int; calls.append(offset)
      let end = min(2400, offset + 1000)
      reply(["success": true, "offset": offset, "total": 2400, "generation": 9,
        "rows": (offset..<end).map { ["path": "/fixture/\($0)"] }])
    }, completion: { resolved = $0 })
    resolver.start()
    let deadline = Date().addingTimeInterval(3)
    while resolved == nil && Date() < deadline { RunLoop.current.run(until: Date().addingTimeInterval(0.001)) }
    check("cross-page resolution only fetches missing selected pages", calls == [1000, 2000])
    check("cross-page resolution returns every selection in row order", (try? resolved?.get().compactMap { $0["path"] as? String }) == ["/fixture/1", "/fixture/1500", "/fixture/2201"])
    for corruption in ["generation", "offset", "total", "truncated"] {
      var result: Result<[Row], Error>?
      let broken = SelectionResolver(selected: IndexSet(integer: 0), cached: [:], total: 2, request: request, send: { _, reply in
        var payload: Row = ["success": true, "offset": 0, "generation": 9, "total": 2, "rows": [["path": "/a"], ["path": "/b"]]]
        if corruption == "truncated" { payload["rows"] = [["path": "/a"]] } else { payload[corruption] = -1 }
        reply(payload)
      }, completion: { result = $0 })
      broken.start()
      if case .failure? = result { passed.append("reject inconsistent selected page: " + corruption) }
      else { failures.append("reject inconsistent selected page: " + corruption) }
    }
    var sent = false, largeFailed = false
    let large = SelectionResolver(selected: IndexSet(integersIn: 0..<100_001), cached: [:], total: 100_001, request: request, send: { _, _ in sent = true }, completion: { if case .failure = $0 { largeFailed = true } })
    large.start(); check("large selections fail before page allocation or transport", largeFailed && !sent)
    var response: ((Row) -> Void)?, completedAfterCancel = false
    let stopped = SelectionResolver(selected: IndexSet(integer: 0), cached: [:], total: 1, request: request, send: { _, callback in response = callback }, completion: { _ in completedAfterCancel = true })
    stopped.start(); stopped.cancel()
    response?(["success": true, "generation": 9, "total": 1, "offset": 0, "rows": [["path": "/a"]]])
    check("late selection reply cannot execute after cancellation", !completedAfterCancel)

    let key = Curve25519.Signing.PrivateKey()
    let manager = UpdateManager(publicKey: key.publicKey.rawRepresentation)
    func manifest(_ version: String = "1.2.3", _ url: String = "https://example.invalid/update.pkg", _ size: Int64 = 7, _ signature: String = "") -> UpdateManifest {
      UpdateManifest(version: version, url: url, sha256: String(repeating: "a", count: 64), size: size, signature: signature)
    }
    let unsigned = manifest()
    let valid = manifest("1.2.3", unsigned.url, unsigned.size, try key.signature(for: unsigned.canonical!).base64EncodedString())
    let encoded = try JSONEncoder().encode(valid)
    try check("signed update manifest accepts the embedded key", manager.verify(encoded).version == "1.2.3")
    for invalid in [manifest("1.2.4", valid.url, valid.size, valid.signature), manifest("1.2.3", valid.url, 8, valid.signature), manifest("1.2.3", "http://example.invalid/a", 7, valid.signature), manifest("1.2.3", valid.url, UpdateManager.maximumPackageSize + 1, valid.signature)] {
      do { _ = try manager.verify(JSONEncoder().encode(invalid)); failures.append("reject tampered update") }
      catch { passed.append("reject tampered version, size, URL or budget") }
    }
    check("unsafe versions and URL credentials are rejected", manifest("../escape").canonical == nil && manifest("1.2.3", "https://user:password@example.invalid/a").canonical == nil)
    let other = UpdateManager(publicKey: Curve25519.Signing.PrivateKey().publicKey.rawRepresentation)
    do { _ = try other.verify(encoded); failures.append("reject another update key") } catch { passed.append("reject another update key") }
    do { _ = try manager.verify(Data(repeating: 0x20, count: 65_537)); failures.append("bounded manifest") } catch { passed.append("bounded manifest") }
    check("unconfigured updates do not opt into networking", !UpdateManager(bundle: .main).configured)

    let network = URLSessionConfiguration.ephemeral
    network.protocolClasses = [UpdateTransportFixture.self]
    let downloadManager = UpdateManager(publicKey: key.publicKey.rawRepresentation, configuration: network)
    let packageBytes = Data("fixture".utf8)
    let packageDigest = SHA256.hash(data: packageBytes).map { String(format: "%02x", $0) }.joined()
    func signedPackage(_ path: String, size: Int64 = 7, digest: String? = nil) throws -> UpdateManifest {
      let value = UpdateManifest(version: "1.2.3", url: "https://updates.fixture/" + path,
        sha256: digest ?? packageDigest, size: size, signature: "")
      return UpdateManifest(version: value.version, url: value.url, sha256: value.sha256,
        size: value.size, signature: try key.signature(for: value.canonical!).base64EncodedString())
    }
    UpdateTransportFixture.lock.lock()
    UpdateTransportFixture.bodies = ["/valid": packageBytes, "/oversized": Data(repeating: 1, count: 20), "/digest": packageBytes]
    UpdateTransportFixture.unfinished = ["/cancel"]
    UpdateTransportFixture.lock.unlock()
    for path in ["valid", "oversized", "digest", "cancel"] {
      var result: Result<URL, Error>?
      let value = try signedPackage(path, digest: path == "digest" ? String(repeating: "b", count: 64) : nil)
      let cancel = downloadManager.download(value) { result = $0 }
      if path == "cancel" { cancel() }
      let timeout = Date().addingTimeInterval(5)
      while result == nil && Date() < timeout { RunLoop.current.run(until: Date().addingTimeInterval(0.002)) }
      if path == "valid", case .success(let url)? = result {
        try check("streamed package is exact and readable only after verification", Data(contentsOf: url) == packageBytes)
        try FileManager.default.removeItem(at: url.deletingLastPathComponent())
      } else if path != "valid", case .failure? = result { passed.append("stream transport rejects " + path) }
      else { failures.append("stream transport case " + path) }
    }

    let service = SearchService()
    check("file RPC cancellation fixture registers before enqueue", service.files.register("wire-cancel"))
    service.request(jsonData(["protocol_version": protocolVersion, "op": "cancel", "request_id": "wire-cancel"])) { _ in }
    let wireCancelled = service.files.perform(["request_id": "wire-cancel", "action": "rename", "paths": [b.path], "new_name": "wire-must-not-rename.txt"])
    check("cancel RPC reaches the file-operation queue", wireCancelled["success"] as? Bool == false && FileManager.default.fileExists(atPath: b.path))

    let pdfURL = root.appendingPathComponent("author.pdf")
    let view = NSTextView(frame: NSRect(x: 0, y: 0, width: 200, height: 100))
    view.string = "Property fixture"
    let pdf = PDFDocument(data: view.dataWithPDF(inside: view.bounds))!
    pdf.documentAttributes = [PDFDocumentAttribute.authorAttribute: "Fixture Author", PDFDocumentAttribute.titleAttribute: "Fixture Title", PDFDocumentAttribute.subjectAttribute: "Fixture Subject"]
    check("create property fixture", pdf.write(to: pdfURL))
    let engine = SearchEngine()
    let properties = try ContentIndexer(engine: engine).extract(pdfURL).1
    check("PDF author and subject map to searchable fields, not artist", properties["author"] as? String == "Fixture Author" && properties["subject"] as? String == "Fixture Subject" && properties["artist"] == nil)
    let office = root.appendingPathComponent("office-source")
    try FileManager.default.createDirectory(at: office.appendingPathComponent("word"), withIntermediateDirectories: true)
    try FileManager.default.createDirectory(at: office.appendingPathComponent("docProps"), withIntermediateDirectories: true)
    try Data("<w:document xmlns:w=\"urn:w\"><w:p><w:t>Office fixture</w:t></w:p></w:document>".utf8).write(to: office.appendingPathComponent("word/document.xml"))
    try Data("<cp:coreProperties xmlns:cp=\"urn:cp\" xmlns:dc=\"urn:dc\"><dc:creator>Office Author</dc:creator><dc:subject>Office Subject</dc:subject></cp:coreProperties>".utf8).write(to: office.appendingPathComponent("docProps/core.xml"))
    try Data("<Properties><Application>Fixture Writer</Application><Pages>12</Pages></Properties>".utf8).write(to: office.appendingPathComponent("docProps/app.xml"))
    let archive = root.appendingPathComponent("properties.docx")
    let zip = Process(); zip.executableURL = URL(fileURLWithPath: "/usr/bin/zip")
    zip.currentDirectoryURL = office; zip.arguments = ["-q", "-r", archive.path, "."]
    try zip.run(); zip.waitUntilExit()
    check("create bounded Office property fixture", zip.terminationStatus == 0)
    let officeProperties = try ContentIndexer(engine: engine).extract(archive).1
    check("Office core and app metadata feed the shared property fields", officeProperties["author"] as? String == "Office Author" && officeProperties["subject"] as? String == "Office Subject" && officeProperties["creator"] as? String == "Fixture Writer" && officeProperties["pages"] as? Int == 12)

    let report: Row = ["success": failures.isEmpty, "passed": passed, "failures": failures,
      "scope": "isolated production-code fixtures; not foreground IME, full-volume or notarization acceptance"]
    print(String(data: try JSONSerialization.data(withJSONObject: report, options: [.sortedKeys]), encoding: .utf8)!)
    if !failures.isEmpty { exit(1) }
  }
}
