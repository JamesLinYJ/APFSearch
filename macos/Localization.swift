import Foundation

/// Localization is resolved by Foundation from the app's compiled String Catalog.
/// Keys such as `search.all_files` remain stable when translated wording changes.
/// The user's system/App Language preference selects the bundle localization.
/// Dynamic arguments use positional printf placeholders in the catalog, so each
/// translation can reorder arguments without string concatenation or patching.
@inline(__always)
func L(_ key: String, _ arguments: CVarArg...) -> String {
    let translated = NSLocalizedString(key, tableName: "Localizable", bundle: .main, value: key, comment: "")
    guard !arguments.isEmpty else { return translated }
    return String(format: translated, locale: Locale.current, arguments: arguments)
}

@inline(__always)
func LF(_ key: String, _ arguments: CVarArg...) -> String {
    let translated = NSLocalizedString(key, tableName: "Localizable", bundle: .main, value: key, comment: "")
    return String(format: translated, locale: Locale.current, arguments: arguments)
}

/// Locale.current is intentionally independent of the selected app language.
/// For example an English UI can retain the user's Chinese date/number region.
func localizedCount(_ value: Int) -> String { value.formatted(.number.locale(.current)) }
func localizedDecimal(_ value: Double, fractionDigits: Int = 0) -> String {
    value.formatted(.number.locale(.current).precision(.fractionLength(fractionDigits)))
}
func localizedDate(_ value: Date) -> String {
    value.formatted(.dateTime.year().month().day().hour().minute().locale(.current))
}

/// Additive JSON protocol: catalog identifiers and typed arguments cross XPC,
/// while the existing text remains available to older clients and the CLI.
/// Text arguments are opaque user/OS data; only explicit `.localized` arguments
/// are looked up. Numbers are formatted in the receiving process's region.
indirect enum LocalizedArgument {
    case text(String)
    case integer(Int)
    case localized(LocalizedText)

    var wire: [String: Any] {
        switch self {
        case .text(let value): return ["type": "text", "value": value]
        case .integer(let value): return ["type": "integer", "value": value]
        case .localized(let value): return ["type": "localized", "value": value.wire]
        }
    }
    func render(bundle: Bundle, locale: Locale) -> String {
        switch self {
        case .text(let value): return value
        case .integer(let value): return value.formatted(.number.locale(locale))
        case .localized(let value): return value.render(bundle: bundle, locale: locale)
        }
    }
    static func decode(_ value: [String: Any], depth: Int) -> LocalizedArgument? {
        switch value["type"] as? String {
        case "text": return (value["value"] as? String).map(LocalizedArgument.text)
        case "integer": return (value["value"] as? Int).map(LocalizedArgument.integer)
        case "localized":
            guard let object = value["value"] as? [String: Any],
                  let text = LocalizedText.decode(object, depth: depth + 1) else { return nil }
            return .localized(text)
        default: return nil
        }
    }
}

struct LocalizedText: Error, LocalizedError {
    let key: String
    let arguments: [LocalizedArgument]
    var errorDescription: String? { render() }
    var wire: [String: Any] { ["key": key, "args": arguments.map(\.wire), "text": render()] }
    func render(bundle: Bundle = .main, locale: Locale = .current, fallback: String? = nil) -> String {
        let missing = "\u{1f}FileSearchMissingLocalization\u{1f}"
        let found = NSLocalizedString(key, tableName: "Localizable", bundle: bundle, value: missing, comment: "")
        if found == missing, let fallback = fallback { return fallback }
        let format = found == missing ? key : found
        guard !arguments.isEmpty else { return format }
        // Wire messages deliberately use only object placeholders. Validate the
        // translated format before entering Foundation's variadic formatter.
        let pattern = try! NSRegularExpression(pattern: #"%%|%(?:([1-9][0-9]*)\$)?@"#)
        let range = NSRange(format.startIndex..., in: format)
        let tokens = pattern.matches(in: format, range: range)
        let remaining = pattern.stringByReplacingMatches(in: format, range: range, withTemplate: "")
        guard !remaining.contains("%") else { return fallback ?? key }
        var next = 0
        var used = Set<Int>()
        for token in tokens {
            guard let full = Range(token.range, in: format), format[full] != "%%" else { continue }
            let index: Int
            if let position = Range(token.range(at: 1), in: format), let explicit = Int(format[position]) {
                index = explicit - 1
            } else { index = next; next += 1 }
            guard arguments.indices.contains(index) else { return fallback ?? key }
            used.insert(index)
        }
        guard used.count == arguments.count else { return fallback ?? key }
        let values: [CVarArg] = arguments.map { $0.render(bundle: bundle, locale: locale) }
        return String(format: format, locale: locale, arguments: values)
    }
    func adding(to response: [String: Any], field: String = "message") -> [String: Any] {
        var result = response
        result[field] = render()
        result[field + "_key"] = key
        result[field + "_args"] = arguments.map(\.wire)
        return result
    }
    static func decode(_ wire: [String: Any], depth: Int = 0) -> LocalizedText? {
        guard depth <= 4, let key = wire["key"] as? String, !key.isEmpty,
              let values = wire["args"] as? [[String: Any]], values.count <= 16 else { return nil }
        let arguments = values.compactMap { LocalizedArgument.decode($0, depth: depth) }
        guard arguments.count == values.count else { return nil }
        return LocalizedText(key: key, arguments: arguments)
    }
}

/// A source-explicit catalog reference, not a reverse lookup of rendered text.
func LT(_ key: String, _ arguments: LocalizedArgument...) -> LocalizedText {
    LocalizedText(key: key, arguments: arguments)
}
func localizedErrorResponse(_ error: Error) -> [String: Any] {
    if let text = error as? LocalizedText { return text.adding(to: ["success": false], field: "error") }
    return ["success": false, "error": error.localizedDescription]
}

/// Only the documented presentation fields are interpreted. File metadata,
/// search syntax, OS errors and arbitrary JSON strings are never translated.
func localizedServiceResponse(_ response: [String: Any], bundle: Bundle = .main, locale: Locale = .current) -> [String: Any] {
    func message(_ object: [String: Any]) -> String {
        let fallback = object["text"] as? String ?? ""
        let text = LocalizedText.decode(object)?.render(bundle: bundle, locale: locale, fallback: fallback) ?? fallback
        return (object["path"] as? String).map { $0 + ": " + text } ?? text
    }
    func fields(_ value: [String: Any], names: [String]) -> [String: Any] {
        var result = value
        for field in names {
            guard let key = value[field + "_key"] as? String,
                  let args = value[field + "_args"] as? [[String: Any]],
                  let text = LocalizedText.decode(["key": key, "args": args]) else { continue }
            result[field] = text.render(bundle: bundle, locale: locale, fallback: value[field] as? String)
        }
        return result
    }
    var result = fields(response, names: ["message", "error"])
    for (source, destination) in [("warning_messages", "warnings"), ("conflict_messages", "conflicts")] {
        if let messages = response[source] as? [[String: Any]] { result[destination] = messages.map(message) }
    }
    if let errors = response["error_messages"] as? [[String: Any]] { result["error"] = errors.map(message).joined(separator: "\n") }
    if let skipped = response["skipped"] as? [[String: Any]] { result["skipped"] = skipped.map { fields($0, names: ["reason"]) } }
    if let preview = response["preview"] as? [[String: Any]] { result["preview"] = preview.map { fields($0, names: ["destination"]) } }
    return result
}
