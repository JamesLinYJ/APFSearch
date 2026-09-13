import Foundation
import AppKit
import SQLite3

final class ReplyBox: @unchecked Sendable {
    private let lock = NSLock()
    private var value: [String: Any]?
    func set(_ result: [String: Any]) { lock.lock(); value = result; lock.unlock() }
    func get() -> [String: Any]? { lock.lock(); defer { lock.unlock() }; return value }
}

@main struct ServiceTests {
    static func main() throws {
        let project = URL(fileURLWithPath: CommandLine.arguments[1], isDirectory: true)
        let source = URL(fileURLWithPath: CommandLine.arguments[2], isDirectory: true)
        let base = URL(fileURLWithPath: CommandLine.arguments[3], isDirectory: true)
        let run = base.appendingPathComponent(UUID().uuidString, isDirectory: true)
        let fixture = run.appendingPathComponent("fixture", isDirectory: true)
        let db = run.appendingPathComponent("index", isDirectory: true)
        try FileManager.default.createDirectory(at: fixture, withIntermediateDirectories: true)
        setenv("APFSEARCH_DATA_DIR", db.path, 1)
        let service = SearchService()
        var passed = [String](), failures = [[String: Any]](), responses = [String: Any]()
        func check(_ name: String, _ condition: Bool, _ detail: Any = "") {
            if condition { passed.append(name) } else { failures.append(["name": name, "detail": String(describing: detail)]) }
        }
        do {
            let support = run.appendingPathComponent("fresh-install-support")
            let unrelated = support.appendingPathComponent("APFSearch")
            try FileManager.default.createDirectory(at: unrelated, withIntermediateDirectories: true)
            let original = Data("existing private index; must not be opened or moved".utf8)
            try original.write(to: unrelated.appendingPathComponent("index.sqlite"))
            let engine = SearchEngine(applicationSupportDirectory: support)
            let status = engine.call(["op": "status"])
            check("fresh identity opens an empty independent index", engine.directory == support.appendingPathComponent("APFSearch/v1", isDirectory: true) && (status["count"] as? Int) == 0, status)
            check("fresh installation leaves neighboring data intact", try Data(contentsOf: unrelated.appendingPathComponent("index.sqlite")) == original)
        }
        func asyncRequest(_ payload: [String: Any], version: Int? = protocolVersion) -> ReplyBox {
            var request = payload
            if let version = version { request["protocol_version"] = version }
            let box = ReplyBox()
            service.request(jsonData(request), withReply: { box.set(jsonObject($0)) })
            return box
        }
        func wait(_ box: ReplyBox, timeout: TimeInterval = 60) throws -> [String: Any] {
            let until = Date().addingTimeInterval(timeout)
            while Date() < until {
                if let value = box.get() { return value }
                _ = RunLoop.current.run(mode: .default, before: Date().addingTimeInterval(0.005))
            }
            throw NSError(domain: "ServiceTests", code: 1, userInfo: [NSLocalizedDescriptionKey: "Timed out waiting for SearchService.request reply"])
        }
        func request(_ payload: [String: Any], version: Int? = protocolVersion) throws -> [String: Any] { try wait(asyncRequest(payload, version: version)) }
        func rows(_ response: [String: Any]) -> [[String: Any]] { response["rows"] as? [[String: Any]] ?? [] }
        func paths(_ response: [String: Any]) -> [String] { rows(response).compactMap { $0["path"] as? String } }
        func contains(_ response: [String: Any], _ filename: String) -> Bool { paths(response).contains { URL(fileURLWithPath: $0).lastPathComponent == filename } }

        check("wrong protocol version rejected", try request(["op": "status"], version: 999)["success"] as? Bool == false)
        check("protocol error carries an explicit catalog key", try request(["op": "status"], version: 999)["error_key"] as? String == "error.unsupported_protocol")
        check("missing protocol version rejected", try request(["op": "status"], version: nil)["success"] as? Bool == false)
        check("malformed JSON rejected", try { () throws -> Bool in
            let box=ReplyBox();service.request(Data("not json".utf8),withReply:{box.set(jsonObject($0))});return try wait(box)["success"] as? Bool == false
        }())
        for filename in ["sample.pdf", "文档.docx", "dimensions.png", "shared.xlsx", "slides.pptx"] {
            try FileManager.default.copyItem(at: source.appendingPathComponent(filename), to: fixture.appendingPathComponent(filename))
        }
        let named = "中文,双\"引号.txt"
        try "SearchService 自动 textneedle42".write(to: fixture.appendingPathComponent(named), atomically: true, encoding: .utf8)
        for filename in ["report2.txt", "report10.txt", "excluded.txt"] { try "plain text".write(to: fixture.appendingPathComponent(filename), atomically: true, encoding: .utf8) }
        let fixedModified: Int64 = 1_720_000_000
        try FileManager.default.setAttributes([.modificationDate: Date(timeIntervalSince1970: TimeInterval(fixedModified))], ofItemAtPath: fixture.appendingPathComponent(named).path)
        let scan = try request(["op": "scan", "roots": [fixture.path], "watch": false, "wait": true])
        check("scoped fixture scan through SearchService.request", scan["success"] as? Bool == true, scan)
        let baseline = try request(["op": "query", "text": "file:", "limit": 1000])
        check("only fixture files indexed", rows(baseline).count == 9 && paths(baseline).allSatisfy { $0.hasPrefix(fixture.path + "/") }, baseline)

        let pdf = try request(["op": "query", "text": "ext:pdf content:searchable", "request_id": "pdf-auto"])
        check("PDF query automatically invokes extraction", contains(pdf, "sample.pdf") && (pdf["extracted_count"] as? Int ?? 0) >= 1, pdf)
        let docx = try request(["op": "query", "text": "ext:docx content:\"Docx 文本 42\"", "request_id": "docx-auto"])
        check("DOCX query automatically invokes extraction", contains(docx, "文档.docx") && (docx["extracted_count"] as? Int ?? 0) >= 1, docx)
        let text = try request(["op": "query", "text": "ext:txt content:textneedle42", "request_id": "text-auto"])
        check("text query automatically invokes extraction", contains(text, named) && (text["extracted_count"] as? Int ?? 0) >= 1, text)
        let image = try request(["op": "query", "text": "ext:png width:32 height:17", "request_id": "image-auto"])
        check("property query automatically invokes ImageIO", contains(image, "dimensions.png") && (image["extracted_count"] as? Int ?? 0) >= 1, image)
        let properties = rows(image).first?["properties"] as? [String: Any] ?? [:]
        check("ImageIO dimensions reach service query result", (properties["width"] as? NSNumber)?.intValue == 32 && (properties["height"] as? NSNumber)?.intValue == 17, properties)
        let xlsx = try request(["op": "query", "text": "ext:xlsx content:\"Shared 中文\"", "request_id": "xlsx-auto"])
        check("XLSX query extracts shared-string text", contains(xlsx, "shared.xlsx"), xlsx)
        let pptx = try request(["op": "query", "text": "ext:pptx content:\"Slide two\"", "request_id": "pptx-auto"])
        check("PPTX query extracts slide text", contains(pptx, "slides.pptx"), pptx)
        let cached = try request(["op": "query", "text": "ext:pdf content:searchable", "request_id": "pdf-cached"])
        check("unchanged content query reuses indexed extraction", contains(cached, "sample.pdf") && (cached["extracted_count"] as? Int ?? -1) == 0, cached)

        let exported = run.appendingPathComponent("service-export.efu")
        let export = try request(["op": "export_list", "path": exported.path, "text": "file:"])
        check("EFU export through request", export["success"] as? Bool == true && (export["count"] as? Int ?? 0) == 9, export)
        check("export message carries a raw integer argument", export["message_key"] as? String == "export.completed_count" && (export["message_args"] as? [[String: Any]])?.first?["value"] as? Int == 9, export)
        let csv = try CSVReader.parse(String(contentsOf: exported, encoding: .utf8))
        let special = csv.dropFirst().first { $0.first == fixture.appendingPathComponent(named).path }
        check("EFU preserves Chinese comma and quote filename", special != nil, csv)
        let expectedFiletime = (fixedModified + 11_644_473_600) * 10_000_000
        check("EFU modified timestamp encoded as FILETIME", special.flatMap { $0.count > 2 ? Int64($0[2]) : nil } == expectedFiletime, special ?? [])
        let imported = try request(["op": "import_list", "path": exported.path])
        let listID = imported["list_id"] as? String ?? ""
        check("EFU import returns persistent UUID list_id", imported["success"] as? Bool == true && UUID(uuidString: listID) != nil && imported["offline"] as? Bool == true, imported)
        check("import message retains structured localization", imported["message_key"] as? String == "import.completed_count", imported)
        let roundtrip = try request(["op": "query", "list_id": listID, "text": "", "limit": 1000])
        let importedSpecial = rows(roundtrip).first { $0["path"] as? String == fixture.appendingPathComponent(named).path }
        check("EFU FILETIME decoded to original Unix second", (importedSpecial?["modified"] as? NSNumber)?.int64Value == fixedModified, importedSpecial ?? [:])
        check("imported list remains offline", roundtrip["offline"] as? Bool == true, roundtrip)
        let prefs = try request(["op": "preferences", "action": "set", "values": ["macros": ["docs": "ext:txt"], "exclusions": ["name:excluded.txt"]]])
        check("service accepts macros and result exclusions", prefs["success"] as? Bool == true, prefs)
        let live = try request(["op": "query", "text": "docs:", "limit": 1000, "sort": [["field": "name", "ascending": true]]])
        let listed = try request(["op": "query", "list_id": listID, "text": "docs:", "limit": 1000, "sort": [["field": "name", "ascending": true]]])
        check("offline list inherits core macros exclusions and natural sort", paths(live) == paths(listed) && rows(listed).count == 3 && !contains(listed, "excluded.txt"), listed)
        let natural = paths(listed).map { URL(fileURLWithPath: $0).lastPathComponent }
        check("offline natural sort orders report2 before report10", natural.firstIndex(of: "report2.txt")! < natural.firstIndex(of: "report10.txt")!, natural)
        let first = try request(["op": "query", "list_id": listID, "text": "docs:", "limit": 2])
        let second = try request(["op": "query", "list_id": listID, "text": "docs:", "limit": 2, "offset": 2, "generation": first["generation"] ?? 0])
        check("offline pagination pins generation and preserves result order", paths(first) + paths(second) == paths(listed) && first["generation"] as? Int == second["generation"] as? Int, [first, second])
        let descending = try request(["op": "query", "list_id": listID, "text": "docs:", "sort": [["field": "name", "ascending": false]]])
        check("offline descending sort matches core", paths(descending) == paths(listed).reversed(), descending)
        check("offline file operations refused", try request(["op": "files", "list_id": listID, "action": "trash", "paths": [fixture.appendingPathComponent(named).path]])["success"] as? Bool == false)
        check("refused offline file action leaves source untouched", FileManager.default.fileExists(atPath: fixture.appendingPathComponent(named).path))
        check("unknown offline list rejected", try request(["op": "status", "list_id": UUID().uuidString])["success"] as? Bool == false)
        check("invalid offline list identifier rejected", try request(["op": "status", "list_id": "../escape"])["success"] as? Bool == false)

        // Put an actual extraction in the serial queue so cancellation exercises
        // a queued query job deterministically instead of racing a tiny PDF.
        let large = fixture.appendingPathComponent("cancel-blocker.txt")
        try String(repeating: "buffer ", count: 1_000_000).write(to: large, atomically: true, encoding: .utf8)
        _ = try request(["op": "scan", "roots": [fixture.path], "watch": false, "wait": true])
        let blocker = asyncRequest(["op": "content_index", "paths": [large.path]])
        let cancelledQuery = asyncRequest(["op": "query", "text": "content:cancelneedle", "request_id": "service-cancel-test"])
        let cancelAck = try request(["op": "cancel", "request_id": "service-cancel-test"])
        let cancelledResult = try wait(cancelledQuery)
        let blockerResult = try wait(blocker)
        check("cancel request acknowledged", cancelAck["success"] as? Bool == true, cancelAck)
        check("queued automatic content query stops on cancel", cancelledResult["success"] as? Bool == false && ((cancelledResult["error"] as? String ?? "").localizedCaseInsensitiveContains("cancel") || (cancelledResult["error"] as? String ?? "").contains("取消")), cancelledResult)
        check("independent preceding extraction finishes", blockerResult["success"] as? Bool == true, blockerResult)
        check("content summary retains structured localization", blockerResult["message_key"] as? String == "content.completed_summary", blockerResult)
        check("cancel error retains structured localization", cancelledResult["error_key"] as? String == "error.query_cancelled", cancelledResult)
        let emptySelection = try request(["op": "files", "action": "rename", "paths": []])
        check("file validation errors retain structured localization", emptySelection["error_key"] as? String == "error.no_files_selected", emptySelection)
        let missingPath = fixture.appendingPathComponent("missing中文%@.txt").path
        let conflicts = try request(["op": "files", "action": "copy", "paths": [missingPath], "destination": fixture.path, "dry_run": true])
        let conflictMessage = (conflicts["conflict_messages"] as? [[String: Any]])?.first
        check("file conflicts preserve paths as typed text arguments", conflictMessage?["key"] as? String == "error.file_not_found" && (conflictMessage?["args"] as? [[String: Any]])?.first?["value"] as? String == missingPath, conflicts)
        let skipped = try request(["op": "content_index", "paths": [missingPath]])
        check("content skip reason carries explicit key and opaque path", (skipped["skipped"] as? [[String: Any]])?.first?["reason_key"] as? String == "error.file_inaccessible" && (skipped["warning_messages"] as? [[String: Any]])?.first?["path"] as? String == missingPath, skipped)

        let explicitBlocker = asyncRequest(["op": "content_index", "paths": [large.path], "request_id": "explicit-blocker"])
        let queuedExplicit = asyncRequest(["op": "content_index", "paths": [fixture.appendingPathComponent(named).path], "request_id": "explicit-queued-cancel"])
        _ = try request(["op": "cancel", "request_id": "explicit-queued-cancel"])
        let queuedExplicitResult = try wait(queuedExplicit)
        let explicitBlockerResult = try wait(explicitBlocker)
        check("queued explicit content cancellation survives job startup", queuedExplicitResult["cancelled"] as? Bool == true && queuedExplicitResult["count"] as? Int == 0, queuedExplicitResult)
        check("cancelling queued content leaves preceding independent job running", explicitBlockerResult["success"] as? Bool == true && explicitBlockerResult["cancelled"] as? Bool == false, explicitBlockerResult)

        // Two candidate pages, both for success and in-progress cancellation.
        // Read SQLite only to wait for a committed extraction, before its
        // deferred snapshot publication; never manufacture a progress signal.
        let contentPageCount = 1_002
        for index in 0..<contentPageCount {
            try "page_needle42".write(to: fixture.appendingPathComponent(String(format: "contentpage_%04d.txt", index)), atomically: true, encoding: .utf8)
            try "cancel_page_needle42".write(to: fixture.appendingPathComponent(String(format: "cancelpage_%04d.txt", index)), atomically: true, encoding: .utf8)
        }
        _ = try request(["op": "scan", "roots": [fixture.path], "watch": false, "wait": true])
        let contentPages = try request(["op": "query", "text": "name:contentpage_* content:page_needle42", "request_id": "content-pages"], version: protocolVersion)
        check("content extraction crosses the thousand-candidate page boundary", contentPages["success"] as? Bool == true && contentPages["total"] as? Int == contentPageCount && contentPages["extracted_count"] as? Int == contentPageCount, ["total": contentPages["total"] ?? -1, "extracted": contentPages["extracted_count"] ?? -1, "error": contentPages["error"] ?? ""])
        let cachedContentPages = try request(["op": "query", "text": "name:contentpage_* content:page_needle42", "request_id": "content-pages-cached"])
        check("all content candidate pages reuse completed extraction", cachedContentPages["total"] as? Int == contentPageCount && cachedContentPages["extracted_count"] as? Int == 0, ["total": cachedContentPages["total"] ?? -1, "extracted": cachedContentPages["extracted_count"] ?? -1])
        var contentDatabase: OpaquePointer?
        guard sqlite3_open_v2(db.appendingPathComponent("index.sqlite").path, &contentDatabase, SQLITE_OPEN_READONLY, nil) == SQLITE_OK else {
            throw NSError(domain: "ServiceTests", code: 2, userInfo: [NSLocalizedDescriptionKey: "Could not inspect isolated content progress"])
        }
        defer { sqlite3_close(contentDatabase) }
        sqlite3_busy_timeout(contentDatabase, 1000)
        func completedCancelContent() -> Int {
            var statement: OpaquePointer?
            guard sqlite3_prepare_v2(contentDatabase, "SELECT count(*) FROM content WHERE path LIKE '%/cancelpage_%'", -1, &statement, nil) == SQLITE_OK else { return -1 }
            defer { sqlite3_finalize(statement) }
            return sqlite3_step(statement) == SQLITE_ROW ? Int(sqlite3_column_int64(statement, 0)) : -1
        }
        let pagedCancel = asyncRequest(["op": "query", "text": "name:cancelpage_* content:cancel_page_needle42", "request_id": "content-pages-cancel"])
        let contentDeadline = Date().addingTimeInterval(15)
        var observedCompleted = 0
        while Date() < contentDeadline && pagedCancel.get() == nil {
            observedCompleted = completedCancelContent()
            if observedCompleted > 0 { break }
            _ = RunLoop.current.run(mode: .default, before: Date().addingTimeInterval(0.001))
        }
        check("content cancellation starts after real committed work and before completion", observedCompleted > 0 && observedCompleted < contentPageCount, observedCompleted)
        _ = try request(["op": "cancel", "request_id": "content-pages-cancel"])
        let pagedCancelResult = try wait(pagedCancel)
        let retainedContentCount = completedCancelContent()
        check("active paged content request returns localized cancellation", pagedCancelResult["success"] as? Bool == false && pagedCancelResult["error_key"] as? String == "error.query_cancelled", pagedCancelResult)
        let publishedPartial = try request(["op": "query", "text": "name:cancelpage_*", "limit": 2000])
        let publishedPartialCount = rows(publishedPartial).filter { ($0["properties"] as? [String: Any])?["indexed_modified_ns"] != nil }.count
        check("cancelled content task publishes its already committed cache entries", retainedContentCount > 0 && retainedContentCount < contentPageCount && publishedPartialCount == retainedContentCount, ["committed": retainedContentCount, "published": publishedPartialCount])
        let resumedContent = try request(["op": "query", "text": "name:cancelpage_* content:cancel_page_needle42", "request_id": "content-pages-resume"])
        check("resumed content query processes only unfinished files", resumedContent["success"] as? Bool == true && resumedContent["total"] as? Int == contentPageCount && resumedContent["extracted_count"] as? Int == contentPageCount - retainedContentCount, ["already_completed": retainedContentCount, "newly_extracted": resumedContent["extracted_count"] ?? -1, "total": resumedContent["total"] ?? -1])
        responses["content_paging"] = ["candidate_count": contentPageCount, "cancelled_completed": retainedContentCount, "resumed_extracted": resumedContent["extracted_count"] ?? -1]

        // More than two full export pages, kept offline so this fixture never
        // creates tens of thousands of real files or touches the user's index.
        let exportRows = 30_025
        let exportSource = run.appendingPathComponent("multipage-source.efu")
        let exportPaths = (0..<exportRows).map { String(format: "/offline/export/record%05d.txt", $0) }
        var exportCSV = "Filename,Size,Date Modified,Attributes\r\n"
        for (index, path) in exportPaths.enumerated() {
            exportCSV += "\"\(path)\",\(index),\(expectedFiletime),0\r\n"
        }
        exportCSV += "\"/offline/export/omit.txt\",1,\(expectedFiletime),0\r\n"
        try exportCSV.write(to: exportSource, atomically: true, encoding: .utf8)
        let exportImport = try request(["op": "import_list", "path": exportSource.path])
        let exportListID = exportImport["list_id"] as? String ?? ""
        check("multipage export fixture imports", exportImport["success"] as? Bool == true && exportImport["count"] as? Int == exportRows + 1, exportImport)
        let importDirectory = db.appendingPathComponent("Imported Lists", isDirectory: true)
        func importedNames() -> Set<String> {
            Set((try? FileManager.default.contentsOfDirectory(atPath: importDirectory.path)) ?? [])
        }
        let listsBeforeFailure = importedNames()
        let malformedImport = run.appendingPathComponent("malformed-after-batches.efu")
        try ("Filename\n" + (0..<9000).map { "/offline/partial/\($0).txt\n" }.joined() + "\"unterminated").write(to: malformedImport, atomically: true, encoding: .utf8)
        let failedImport = try request(["op": "import_list", "path": malformedImport.path])
        check("malformed import after several SQL batches is rejected", failedImport["success"] as? Bool == false && failedImport["error_key"] as? String == "error.unterminated_csv_quote", failedImport)
        check("failed import leaves neither UUID lists nor staging directories", importedNames() == listsBeforeFailure, importedNames())
        let cancelledImport = ImportJob(); cancelledImport.cancel()
        let cancelledImportResult = service.fileList(["op": "import_list", "path": exportSource.path], importing: cancelledImport)
        check("prequeued import cancellation is preserved", cancelledImportResult["success"] as? Bool == false && cancelledImportResult["error_key"] as? String == "error.query_cancelled", cancelledImportResult)
        check("cancelled import creates no persistent or temporary engine", importedNames() == listsBeforeFailure, importedNames())
        let invalidHeader = run.appendingPathComponent("invalid-header.efu")
        try "Other,Size\n\"unterminated".write(to: invalidHeader, atomically: true, encoding: .utf8)
        let headerResult = try request(["op": "import_list", "path": invalidHeader.path])
        check("invalid header is rejected before later malformed CSV", headerResult["error_key"] as? String == "error.missing_filename_column", headerResult)
        check("invalid header does not allocate an offline index", importedNames() == listsBeforeFailure)
        check("successful import publishes exactly one offline generation", try request(["op": "status", "list_id": exportListID])["generation"] as? Int == 1)
        let exportPreferences: [String: Any] = ["macros": ["docs": "ext:txt"], "exclusions": ["name:omit.txt"]]
        _ = try request(["op": "preferences", "action": "set", "values": exportPreferences])

        func availableLeaseCount() throws -> Int {
            var tokens = [String]()
            for _ in 0..<32 {
                let response = try request(["op": "retain_snapshot", "list_id": exportListID])
                guard response["success"] as? Bool == true, let token = response["snapshot_lease"] as? String else { break }
                tokens.append(token)
            }
            for token in tokens { _ = try request(["op": "release_snapshot", "list_id": exportListID, "snapshot_lease": token]) }
            return tokens.count
        }
        func exportTemporaries() -> [URL] {
            ((try? FileManager.default.contentsOfDirectory(at: run, includingPropertiesForKeys: nil)) ?? [])
                .filter { $0.lastPathComponent.hasPrefix(".apfsearch-export-") }
        }
        func waitForExportTemporary(_ box: ReplyBox) -> URL? {
            let deadline = Date().addingTimeInterval(15)
            while Date() < deadline {
                if let file = exportTemporaries().first { return file }
                if box.get() != nil { return nil }
                _ = RunLoop.current.run(mode: .default, before: Date().addingTimeInterval(0.001))
            }
            return nil
        }
        let leaseCapacityBefore = try availableLeaseCount()
        check("snapshot lease capacity is bounded and initially available", leaseCapacityBefore > 0 && leaseCapacityBefore < 32, leaseCapacityBefore)

        // Export immediately after import/preferences update, without a query
        // first: export itself must synchronize the offline query preferences.
        let multipagePath = run.appendingPathComponent("multipage-result.efu")
        let multipageBox = asyncRequest(["op": "export_list", "list_id": exportListID,
            "path": multipagePath.path, "text": "docs:", "request_id": "multipage-export"])
        let multipageTemp = waitForExportTemporary(multipageBox)
        check("export is staged before destination becomes visible", multipageTemp != nil && !FileManager.default.fileExists(atPath: multipagePath.path))
        // Retaining the snapshot precedes opening the temporary output. Change
        // the list's macros/exclusions now, during the multi-page operation.
        _ = try request(["op": "preferences", "action": "set", "values": ["macros": ["docs": "ext:pdf"], "exclusions": []]])
        let changedPreferences = try request(["op": "query", "list_id": exportListID, "text": "docs:"])
        check("concurrent offline query observes newly changed preferences", changedPreferences["success"] as? Bool == true && changedPreferences["total"] as? Int == 0, changedPreferences)
        let multipageResult = try wait(multipageBox)
        check("multipage export retains original macro and exclusions", multipageResult["success"] as? Bool == true && multipageResult["count"] as? Int == exportRows, multipageResult)
        let multipageRecords = try CSVReader.parse(String(contentsOf: multipagePath, encoding: .utf8))
        let exportedPaths = multipageRecords.dropFirst().compactMap(\.first)
        check("multipage export has exact sorted paths without duplicate or omitted page boundaries", exportedPaths == exportPaths, ["expected": exportPaths.count, "actual": exportedPaths.count, "unique": Set(exportedPaths).count])
        check("multipage export keeps numeric metadata on every page", multipageRecords.dropFirst().enumerated().allSatisfy { index, record in
            record.count == 4 && Int(record[1]) == index && Int64(record[2]) == expectedFiletime && record[3] == "0"
        })
        check("successful export removes its temporary output", exportTemporaries().isEmpty, exportTemporaries())

        _ = try request(["op": "preferences", "action": "set", "values": exportPreferences])
        let cancelledExportPath = run.appendingPathComponent("cancelled-export.efu")
        let cancelledExportBox = asyncRequest(["op": "export_list", "list_id": exportListID,
            "path": cancelledExportPath.path, "text": "docs:", "request_id": "cancel-export"])
        let cancelledTemp = waitForExportTemporary(cancelledExportBox)
        check("cancel test reaches an active export temporary", cancelledTemp != nil)
        let exportCancelAck = try request(["op": "cancel", "list_id": exportListID, "request_id": "cancel-export"])
        let cancelledExportResult = try wait(cancelledExportBox)
        check("in-progress export cancellation is acknowledged", exportCancelAck["success"] as? Bool == true, exportCancelAck)
        check("in-progress export returns cancellation instead of success", cancelledExportResult["success"] as? Bool == false && (cancelledExportResult["error_key"] as? String == "error.query_cancelled" || (cancelledExportResult["error"] as? String ?? "").localizedCaseInsensitiveContains("cancel")), cancelledExportResult)
        check("in-progress export cancellation carries the catalog localization key", cancelledExportResult["error_key"] as? String == "error.query_cancelled", cancelledExportResult)
        check("cancelled export publishes no partial destination and removes temporary", !FileManager.default.fileExists(atPath: cancelledExportPath.path) && exportTemporaries().isEmpty, exportTemporaries())

        let racedPath = run.appendingPathComponent("raced-export.efu")
        let racedBox = asyncRequest(["op": "export_list", "list_id": exportListID,
            "path": racedPath.path, "text": "docs:", "request_id": "race-export"])
        let racedTemp = waitForExportTemporary(racedBox)
        check("destination race starts after staging", racedTemp != nil)
        let competingData = Data("created by another writer while export was running\n".utf8)
        var competitorCreated = false
        if racedTemp != nil {
            do { try competingData.write(to: racedPath, options: .withoutOverwriting); competitorCreated = true }
            catch { check("competing destination can be created before export commits", false, error) }
        }
        let racedResult = try wait(racedBox)
        check("export refuses a destination created during the operation", competitorCreated && racedResult["success"] as? Bool == false, racedResult)
        check("competing destination remains byte-for-byte untouched", (try? Data(contentsOf: racedPath)) == competingData)
        check("failed race cleans temporary output", exportTemporaries().isEmpty, exportTemporaries())

        // Also verify the atomic publisher directly with deterministic ordering,
        // independent of scheduling speed in the SearchService.request race above.
        let directRacePath = run.appendingPathComponent("publisher-race.efu")
        var pendingOutput: FileListOutput? = try FileListOutput(destination: directRacePath)
        let directTemporary = pendingOutput!.temporary
        try pendingOutput!.write("partial export")
        try competingData.write(to: directRacePath, options: .withoutOverwriting)
        var directRaceRejected = false
        do { try pendingOutput!.finish() } catch { directRaceRejected = true }
        pendingOutput = nil
        check("atomic publisher refuses replacement at commit", directRaceRejected && (try? Data(contentsOf: directRacePath)) == competingData)
        check("atomic publisher cleans failed staging file on release", !FileManager.default.fileExists(atPath: directTemporary.path))
        check("success cancellation and failure release all snapshot leases", try availableLeaseCount() == leaseCapacityBefore)
        responses["export_regression"] = ["rows": exportRows, "page_size": 10_000, "lease_capacity_before": leaseCapacityBefore,
            "multipage": multipageResult, "cancelled": cancelledExportResult, "destination_race": racedResult]
        _ = try request(["op": "stop"])
        responses["automatic_extraction_counts"] = ["pdf": pdf["extracted_count"] ?? -1, "docx": docx["extracted_count"] ?? -1, "text": text["extracted_count"] ?? -1, "image": image["extracted_count"] ?? -1, "pdf_cached": cached["extracted_count"] ?? -1]
        responses["offline_list_id"] = listID
        responses["cancel_reply"] = cancelledResult
        let report: [String: Any] = ["schema_version": 1, "success": failures.isEmpty, "passed": passed, "failures": failures, "count": passed.count, "evidence": responses, "scope": "In-process asynchronous SearchService.request through real Rust staticlib and system extractors using an isolated APFSEARCH_DATA_DIR", "not_tested": ["Mach XPC connection authentication", "Full Disk Access UI", "Actual user database", "Notarization"], "fixture_directory": fixture.path, "database_directory": db.path, "macos_minimum_target": Bundle.main.object(forInfoDictionaryKey: "LSMinimumSystemVersion") as? String ?? "unknown"]
        let output = project.appendingPathComponent("validation/service.json")
        try FileManager.default.createDirectory(at: output.deletingLastPathComponent(), withIntermediateDirectories: true)
        try jsonData(report).write(to: output)
        print(String(data: jsonData(report), encoding: .utf8)!)
        if !failures.isEmpty { exit(1) }
    }
}
