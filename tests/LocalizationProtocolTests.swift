import Foundation

@main struct LocalizationProtocolTests {
    static func main() throws {
        let resources = URL(fileURLWithPath: CommandLine.arguments[1])
        let english = Bundle(path: resources.appendingPathComponent("en.lproj").path)!
        let chinese = Bundle(path: resources.appendingPathComponent("zh-Hans.lproj").path)!
        let traditional = Bundle(path: resources.appendingPathComponent("zh-Hant.lproj").path)!
        let reordered = Bundle(path: CommandLine.arguments[2])!
        var passed = [String](), failures = [[String: String]]()
        func check(_ name: String, _ condition: Bool, _ detail: Any = "") {
            if condition { passed.append(name) } else { failures.append(["name": name, "detail": String(describing: detail)]) }
        }
        func decode(_ response: [String: Any], _ bundle: Bundle = english, _ locale: Locale = Locale(identifier: "en_US")) -> [String: Any] {
            SearchClient.decodeResponse(jsonData(response), bundle: bundle, locale: locale)
        }
        // Simulate a persistent Chinese service sending the exact JSON wire
        // structure, then decode it in an English GUI without language mutation.
        let export = LT("export.completed_count", .integer(12345))
        var chineseResponse = export.adding(to: ["success": true])
        chineseResponse["message"] = export.render(bundle: chinese, locale: Locale(identifier: "zh_CN"))
        let englishResponse = decode(chineseResponse)
        check("Chinese service fallback is actually Chinese", (chineseResponse["message"] as? String ?? "").contains("已导出"), chineseResponse)
        check("English client resolves key in its own Bundle", englishResponse["message"] as? String == export.render(bundle: english, locale: Locale(identifier: "en_US")) && englishResponse["message"] as? String != chineseResponse["message"] as? String, englishResponse)
        check("wire preserves legacy fallback and raw argument", (jsonObject(jsonData(chineseResponse))["message_args"] as? [[String: Any]])?.first?["value"] as? Int == 12345 && chineseResponse["message"] is String)
        let regional = decode(chineseResponse, english, Locale(identifier: "de_DE"))
        check("English UI uses receiving German number region", (regional["message"] as? String ?? "").contains("12.345"), regional)
        let traditionalResponse = decode(chineseResponse, traditional)
        check("Traditional Chinese client resolves its own catalog", traditionalResponse["message"] as? String == export.render(bundle: traditional, locale: Locale(identifier: "en_US")), traditionalResponse)
        let reorder = LT("test.reorder", .text("first"), .text("second"))
        let swapped = decode(reorder.adding(to: ["success": true]), reordered)
        check("catalog can reorder argument positions", swapped["message"] as? String == "second before first", swapped)
        let summary = LT("content.completed_summary", .integer(12), .integer(3), .localized(LT("status.cancelled_suffix")))
        var summaryResponse = summary.adding(to: ["success": true]); summaryResponse["message"] = summary.render(bundle: chinese)
        let englishSummary = decode(summaryResponse)
        check("nested cancellation suffix uses client Bundle", englishSummary["message"] as? String == summary.render(bundle: english, locale: Locale(identifier: "en_US")) && !(englishSummary["message"] as? String ?? "").contains("取消"), englishSummary)
        for (language, localeName, expectedExport) in [
            ("ja", "ja_JP", "書き出したレコード数:"),
            ("ko", "ko_KR", "내보낸 레코드:"),
            ("ru", "ru_RU", "Экспортировано записей:"),
            ("es", "es_ES", "Registros exportados:"),
            ("pt", "pt_PT", "Registos exportados:")
        ] {
            let bundle = Bundle(path: resources.appendingPathComponent(language + ".lproj").path)!
            let locale = Locale(identifier: localeName)
            let translated = decode(chineseResponse, bundle, locale)["message"] as? String ?? ""
            check("\(language) client renders a persistent Chinese service reply",
                  translated.hasPrefix(expectedExport) && translated.contains(12345.formatted(.number.locale(locale))), translated)
            let translatedSummary = decode(summaryResponse, bundle, locale)["message"] as? String ?? ""
            check("\(language) client translates nested message arguments",
                  translatedSummary == summary.render(bundle: bundle, locale: locale)
                    && translatedSummary.contains(catalogString("status.cancelled_suffix", bundle: bundle)), translatedSummary)
            let history = LT("files.history_page_notice", .integer(12), .integer(34))
            let translatedHistory = history.render(bundle: bundle, locale: locale)
            check("\(language) history placeholders support native reordering",
                  translatedHistory != history.key && translatedHistory.contains("12") && translatedHistory.contains("34"), translatedHistory)
        }
        let path = "/tmp/中文已取消%@\".txt"
        let conflict = LT("error.file_not_found", .text(path))
        let conflictResponse: [String: Any] = ["success": false, "error": "服务中文", "conflicts": ["服务中文"], "conflict_messages": [conflict.wire], "error_messages": [conflict.wire], "rows": [["path": path, "name": "已取消"]]]
        let decodedConflict = decode(conflictResponse)
        check("conflict display localizes prose and preserves literal path", (decodedConflict["error"] as? String ?? "").hasSuffix(path) && decodedConflict["error"] as? String == conflict.render(bundle: english, locale: Locale(identifier: "en_US")), decodedConflict)
        check("arbitrary result metadata is not traversed or translated", (decodedConflict["rows"] as? [[String: String]])?.first?["name"] == "已取消" && (decodedConflict["rows"] as? [[String: String]])?.first?["path"] == path)
        let reason = LT("error.file_inaccessible")
        var warning = reason.wire; warning["path"] = path
        let skipped = reason.adding(to: ["path": path], field: "reason")
        let warnings = decode(["success": true, "warning_messages": [["text": "PCRE2 technical error"], warning], "skipped": [skipped]])
        check("content warnings localize known reasons only", (warnings["warnings"] as? [String]) == ["PCRE2 technical error", path + ": " + reason.render(bundle: english)], warnings)
        check("structured skipped reason is localized", (warnings["skipped"] as? [[String: Any]])?.first?["reason"] as? String == reason.render(bundle: english), warnings)
        let systemError = NSError(domain: NSPOSIXErrorDomain, code: 13)
        let rawError = localizedErrorResponse(systemError)
        check("OS localizedDescription is preserved verbatim", decode(rawError)["error"] as? String == systemError.localizedDescription && rawError["error_key"] == nil)
        let raw: [String: Any] = ["success": false, "error": "已取消", "message": "legacy text", "warnings": ["technical: invalid regex"], "path": path]
        let legacy = decode(raw)
        check("legacy text without keys has no reverse translation", legacy["error"] as? String == "已取消" && legacy["message"] as? String == "legacy text" && legacy["warnings"] as? [String] == ["technical: invalid regex"])
        let unknown: [String: Any] = ["success": false, "error": "future-version explanation", "error_key": "unknown.future.key", "error_args": []]
        check("unknown future catalog key retains service fallback", decode(unknown)["error"] as? String == "future-version explanation")
        let malformed: [String: Any] = ["success": true, "message": "malformed fallback", "message_key": "test.reorder", "message_args": [["type": "integer", "value": "invalid"]]]
        check("malformed typed argument retains fallback", decode(malformed, reordered)["message"] as? String == "malformed fallback")
        let unsafeFormat = LT("test.invalid.format", .text(path)).adding(to: ["success": true])
        check("invalid catalog formatter does not enter variadic formatting", decode(unsafeFormat, reordered)["message"] as? String == unsafeFormat["message"] as? String)
        let missingArgument = LT("test.reorder", .text("first")).adding(to: ["success": true])
        check("out-of-range positional argument retains fallback", decode(missingArgument, reordered)["message"] as? String == missingArgument["message"] as? String)
        let trash = LT("files.trash_destination").adding(to: ["source": path], field: "destination")
        let preview = decode(["success": true, "preview": [trash, ["source": path, "destination": path + ".copy"]]])
        let previews = preview["preview"] as? [[String: Any]] ?? []
        check("trash label localizes while real destinations remain paths", previews.first?["destination"] as? String == LT("files.trash_destination").render(bundle: english) && previews.last?["destination"] as? String == path + ".copy")
        let report: [String: Any] = ["success": failures.isEmpty, "count": passed.count, "passed": passed, "failures": failures, "simulated_service_response": chineseResponse, "english_client_response": englishResponse, "independent_region_response": regional, "scope": "Actual SearchClient JSON decode path; compiled Foundation String Catalog bundles; no XPC service or language preference changes"]
        print(String(data: jsonData(report), encoding: .utf8)!)
        if !failures.isEmpty { exit(1) }
    }
}
