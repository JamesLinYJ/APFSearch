import AppKit
import SwiftUI
import ServiceManagement

extension Notification.Name {
    static let searchPreferencesChanged = Notification.Name("APFSearch.preferencesChanged")
    static let searchShortcutChanged = Notification.Name("APFSearch.shortcutChanged")
}

final class SearchSettingsModel: ObservableObject {
    @Published var macros = ""
    @Published var bookmarks = ""
    @Published var exclusions = ""
    @Published var history: [String] = []
    var historyCleared = false
    @Published var message = L("status.loading_settings")
    @Published var error = false
    @Published var loading = true
    @Published var saving = false
    @Published var roots: [String] = []
    @Published var uncovered: [String] = []
    @Published var itemCount = 0
    @Published var scanning = false
    @Published var loginEnabled = SMAppService.mainApp.status == .enabled
    @Published var shortcutEnabled = UserDefaults.standard.object(forKey: "shortcutEnabled") == nil || UserDefaults.standard.bool(forKey: "shortcutEnabled")
    @Published var shortcutChoice = UserDefaults.standard.integer(forKey: "shortcutChoice")
    private var values: [String: Any] = [:]
    init() { reload(); refreshStatus() }
    func reload() {
        loading = true
        SearchClient.shared.call(["op": "preferences", "action": "get"]) { [weak self] reply in
            guard let self else { return }; self.loading = false
            guard reply["success"] as? Bool != false else { self.error = true; self.message = reply["error"] as? String ?? L("error.settings_unreadable"); return }
            self.values = (reply["values"] as? [String: Any] ?? reply).filter { ["macros", "bookmarks", "exclusions", "history", "settings"].contains($0.key) }
            self.macros = (self.values["macros"] as? [String: String] ?? [:]).sorted { $0.key < $1.key }.map { "\($0.key) = \($0.value)" }.joined(separator: "\n")
            self.bookmarks = (self.values["bookmarks"] as? [[String: Any]] ?? []).map { "\($0["name"] as? String ?? $0["title"] as? String ?? L("search.bookmarks")) = \($0["query"] as? String ?? $0["text"] as? String ?? "")" }.joined(separator: "\n")
            self.exclusions = (self.values["exclusions"] as? [String] ?? []).joined(separator: "\n")
            self.history = (self.values["history"] as? [String]) ?? (self.values["history"] as? [[String: Any]])?.compactMap { $0["query"] as? String ?? $0["text"] as? String } ?? []
            self.historyCleared = false
            self.message = ""; self.error = false
        }
    }
    private func pairs(_ text: String, field: String) throws -> [(String, String)] {
        var output: [(String, String)] = []; var names = Set<String>()
        for (index, source) in text.components(separatedBy: .newlines).enumerated() {
            let line = source.trimmingCharacters(in: .whitespacesAndNewlines); if line.isEmpty { continue }
            guard let separator = line.firstIndex(of: "=") else { throw SettingsError.text(L("error.missing_rule_separator", String(describing: field), (index + 1).formatted())) }
            let name = String(line[..<separator]).trimmingCharacters(in: .whitespaces)
            let query = String(line[line.index(after: separator)...]).trimmingCharacters(in: .whitespaces)
            guard !name.isEmpty, !query.isEmpty else { throw SettingsError.text(L("error.empty_rule_fields", String(describing: field), (index + 1).formatted())) }
            guard names.insert(name).inserted else { throw SettingsError.text(L("error.duplicate_rule_name", String(describing: field), String(describing: name))) }
            output.append((name, query))
        }
        return output
    }
    func save() {
        do {
            let macroPairs = try pairs(macros, field: L("search.macros"))
            for (name, _) in macroPairs where name.contains(":") || name.contains(where: { $0.isWhitespace }) { throw SettingsError.text(L("error.invalid_macro_name", String(describing: name))) }
            let bookmarkPairs = try pairs(bookmarks, field: L("search.bookmarks"))
            var next: [String: Any] = [:]
            next["macros"] = Dictionary(uniqueKeysWithValues: macroPairs)
            next["bookmarks"] = bookmarkPairs.map { ["name": $0.0, "query": $0.1] }
            next["exclusions"] = exclusions.components(separatedBy: .newlines).map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }.filter { !$0.isEmpty }
            if historyCleared { next["history"] = history }
            saving = true; message = L("status.saving"); error = false
            SearchClient.shared.call(["op": "preferences", "action": "set", "values": next]) { [weak self] reply in
                guard let self else { return }; self.saving = false
                if reply["success"] as? Bool == false { self.error = true; self.message = reply["error"] as? String ?? L("error.save_failed") }
                else { self.values.merge(next) { _, new in new }; self.historyCleared = false; self.message = L("settings.saved"); NotificationCenter.default.post(name: .searchPreferencesChanged, object: nil) }
            }
        } catch { self.error = true; message = error.localizedDescription }
    }
    func refreshStatus() {
        SearchClient.shared.call(["op": "status"]) { [weak self] reply in
            guard let self else { return }
            self.roots = reply["roots"] as? [String] ?? []
            self.itemCount = (reply["count"] as? NSNumber)?.intValue ?? 0
            self.scanning = reply["scanning"] as? Bool ?? false
            self.uncovered = (reply["uncovered"] as? [String]) ?? (reply["uncovered"] as? [[String: Any]])?.map { ($0["path"] as? String ?? "") + "：" + ($0["reason"] as? String ?? L("error.unavailable")) } ?? []
        }
    }
    func chooseScope() {
        let panel = NSOpenPanel(); panel.title = L("index.choose_index_scope"); panel.prompt = L("index.start_indexing")
        panel.canChooseFiles = false; panel.canChooseDirectories = true; panel.allowsMultipleSelection = true
        guard panel.runModal() == .OK else { return }
        SearchClient.shared.call(["op": "scan", "roots": panel.urls.map(\.path)]) { [weak self] reply in
            guard let self else { return }
            if reply["success"] as? Bool == false { self.error = true; self.message = reply["error"] as? String ?? L("error.scan_start") }
            else { self.message = L("status.scanning_new_scope"); UserDefaults.standard.set(true, forKey: "APFSearch.ScopeChosen"); self.refreshStatus(); NotificationCenter.default.post(name: .searchPreferencesChanged, object: nil) }
        }
    }
    func updateShortcut() {
        UserDefaults.standard.set(shortcutEnabled, forKey: "shortcutEnabled")
        UserDefaults.standard.set(shortcutChoice, forKey: "shortcutChoice")
        NotificationCenter.default.post(name: .searchShortcutChanged, object: nil)
    }
    func updateLogin(_ enabled: Bool) {
        do {
            if enabled { try SMAppService.mainApp.register() } else { try SMAppService.mainApp.unregister() }
            loginEnabled = enabled; error = false
            message = SMAppService.mainApp.status == .requiresApproval ? L("settings.enable_login_hint") : L("settings.login_settings_updated")
        } catch { self.error = true; message = error.localizedDescription; loginEnabled = SMAppService.mainApp.status == .enabled }
    }
}

enum SettingsError: LocalizedError {
    case text(String)
    var errorDescription: String? { if case .text(let text) = self { return text }; return nil }
}

struct SearchSettingsView: View {
    @StateObject private var model = SearchSettingsModel()
    var body: some View {
        VStack(spacing: 0) {
            TabView {
                general.tabItem { Label(L("settings.general"), systemImage: "gearshape") }
                indexing.tabItem { Label(L("settings.indexing"), systemImage: "externaldrive") }
                searches.tabItem { Label(L("settings.search_rules"), systemImage: "line.3.horizontal.decrease.circle") }
                saved.tabItem { Label(L("search.bookmarks_and_history"), systemImage: "bookmark") }
            }
            .padding(18)
            Divider()
            HStack {
                if model.loading || model.saving { ProgressView().controlSize(.small) }
                Text(model.message).font(.callout).foregroundStyle(model.error ? .red : .secondary).lineLimit(2)
                Spacer()
                Button(L("action.reload")) { model.reload(); model.refreshStatus() }.disabled(model.saving)
                Button(L("settings.save_search_settings")) { model.save() }.keyboardShortcut("s", modifiers: .command).disabled(model.loading || model.saving)
            }.padding(16)
        }.frame(width: 700, height: 620)
    }
    var general: some View {
        Form {
            Section(L("settings.startup_and_shortcuts")) {
                Toggle(L("settings.open_at_login"), isOn: Binding(get: { model.loginEnabled }, set: { model.updateLogin($0) }))
                Toggle(L("settings.enable_global_search_shortcut"), isOn: $model.shortcutEnabled).onChange(of: model.shortcutEnabled) { _, _ in model.updateShortcut() }
                Picker(L("shortcut.show_search"), selection: $model.shortcutChoice) {
                    Text(L("shortcut.control_option_space")).tag(0)
                    Text("⌘⇧F").tag(1)
                }.disabled(!model.shortcutEnabled).onChange(of: model.shortcutChoice) { _, _ in model.updateShortcut() }
                Text(L("settings.shortcut_login_hint")).font(.caption).foregroundStyle(.secondary)
            }
            Section(L("index.local_index_service")) {
                Text(L("settings.background_service_hint")).font(.callout)
                Button(L("settings.open_login_items_extensions")) { SMAppService.openSystemSettingsLoginItems() }
            }
            Section(L("shortcut.section_title")) {
                LabeledContent(L("search.search_field"), value: "⌘F")
                LabeledContent(L("shortcut.open_preview"), value: L("shortcut.return_space"))
                LabeledContent(L("shortcut.reveal_copy_path"), value: "⌘⇧R / ⌘⇧C")
                LabeledContent(L("shortcut.new_tab_bookmark"), value: "⌘T / ⌘D")
            }
        }.formStyle(.grouped)
    }
    var indexing: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text(L("status.indexed_count", String(describing: model.itemCount.formatted()))).font(.headline)
                if model.scanning { ProgressView().controlSize(.small); Text(L("status.scanning")).foregroundStyle(.secondary) }
                Spacer()
                Button(L("action.refresh")) { model.refreshStatus() }
                Button(L("index.change_scope")) { model.chooseScope() }
            }
            Text(L("index.current_scope")).font(.subheadline).bold()
            ScrollView {
                VStack(alignment: .leading, spacing: 7) {
                    if model.roots.isEmpty { Text(L("index.no_scope")).foregroundStyle(.secondary) }
                    ForEach(model.roots, id: \.self) { Text($0).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading) }
                }.padding(10)
            }.frame(height: 105).background(.quaternary.opacity(0.5), in: RoundedRectangle(cornerRadius: 6))
            Text(L("index.uncovered_locations")).font(.subheadline).bold()
            ScrollView {
                VStack(alignment: .leading, spacing: 8) {
                    if model.uncovered.isEmpty { Text(L("index.no_uncovered_yet_notice")).foregroundStyle(.secondary) }
                    ForEach(Array(model.uncovered.enumerated()), id: \.offset) { _, text in Text(text).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading) }
                }.padding(10)
            }.frame(maxHeight: .infinity).background(.quaternary.opacity(0.5), in: RoundedRectangle(cornerRadius: 6))
            Text(L("index.coverage_limitations")).font(.caption).foregroundStyle(.secondary)
            Button(L("settings.open_full_disk_access_settings")) { NSWorkspace.shared.open(URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles")!) }
        }.padding(14)
    }
    var searches: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text(L("settings.search_macros")).font(.headline)
            Text(L("settings.macros_hint")).font(.caption).foregroundStyle(.secondary)
            editor($model.macros, placeholder: "work = path:Documents ext:pdf;docx\nlarge = size:>100mb")
            Text(L("settings.result_exclusions")).font(.headline)
            Text(L("settings.exclusions_hint")).font(.caption).foregroundStyle(.secondary)
            editor($model.exclusions, placeholder: L("settings.exclusion_example"))
            Text(L("help.query_syntax_summary")).font(.caption).foregroundStyle(.secondary)
        }.padding(14)
    }
    var saved: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text(L("search.bookmarks")).font(.headline)
            Text(L("settings.bookmarks_hint")).font(.caption).foregroundStyle(.secondary)
            editor($model.bookmarks, placeholder: L("settings.bookmark_example"))
            HStack {
                Text(L("settings.search_history")).font(.headline)
                Spacer()
                Button(L("action.clear_history")) { model.history = []; model.historyCleared = true }
            }
            List(Array(model.history.enumerated()), id: \.offset) { _, query in Text(query).textSelection(.enabled) }.frame(height: 145)
            Text(L("settings.bookmarks_history_hint")).font(.caption).foregroundStyle(.secondary)
        }.padding(14)
    }
    @ViewBuilder func editor(_ binding: Binding<String>, placeholder: String) -> some View {
        ZStack(alignment: .topLeading) {
            TextEditor(text: binding).font(.system(size: 12, design: .monospaced)).scrollContentBackground(.hidden).padding(5)
            if binding.wrappedValue.isEmpty { Text(placeholder).font(.system(size: 12, design: .monospaced)).foregroundStyle(.tertiary).padding(.leading, 10).padding(.top, 11).allowsHitTesting(false) }
        }.frame(minHeight: 100).background(.background, in: RoundedRectangle(cornerRadius: 6)).overlay(RoundedRectangle(cornerRadius: 6).stroke(.quaternary))
    }
}

func makeSearchSettingsWindow() -> NSWindowController {
    let controller = NSHostingController(rootView: SearchSettingsView())
    let window = NSWindow(contentViewController: controller)
    window.title = L("settings.window_title"); window.styleMask = [.titled, .closable]
    window.center(); window.setFrameAutosaveName("APFSearch.Settings")
    return NSWindowController(window: window)
}
