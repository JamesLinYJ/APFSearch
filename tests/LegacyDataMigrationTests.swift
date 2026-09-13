import Foundation

@main struct LegacyDataMigrationTests {
    static func main() throws {
        let manager = FileManager.default
        let root = manager.temporaryDirectory.appendingPathComponent("FileSearch-migration-" + UUID().uuidString)
        try manager.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? manager.removeItem(at: root) }
        var passed = [String](), failures = [String]()
        func check(_ name: String, _ value: Bool) { (value ? passed.append(name) : failures.append(name)) }
        func fixture(_ name: String) throws -> URL {
            let support = root.appendingPathComponent(name)
            try manager.createDirectory(at: support, withIntermediateDirectories: true)
            return support
        }
        func legacyData(_ support: URL) throws -> URL {
            let legacy = support.appendingPathComponent(ApplicationIdentity.legacyDataDirectoryName)
            try manager.createDirectory(at: legacy.appendingPathComponent("lists"), withIntermediateDirectories: true)
            for name in ["index.sqlite", "index.sqlite-wal", "operations.json", "lists/saved.sqlite"] {
                try Data(("existing:" + name).utf8).write(to: legacy.appendingPathComponent(name))
            }
            return legacy
        }
        let first = try fixture("first")
        let legacy = try legacyData(first)
        let identity = try manager.attributesOfItem(atPath: legacy.path)[.systemFileNumber] as? NSNumber
        let migrated = try LegacyDataMigration.dataDirectory(in: first)
        check("first upgrade uses the FileSearch directory", migrated.lastPathComponent == "FileSearch")
        check("entire directory moved atomically with the same inode", (try manager.attributesOfItem(atPath: migrated.path)[.systemFileNumber] as? NSNumber) == identity && !manager.fileExists(atPath: legacy.path))
        check("index WAL journal and offline list bytes preserved", try ["index.sqlite", "index.sqlite-wal", "operations.json", "lists/saved.sqlite"].allSatisfy { try Data(contentsOf: migrated.appendingPathComponent($0)) == Data(("existing:" + $0).utf8) })
        check("repeat migration is idempotent", try LegacyDataMigration.dataDirectory(in: first) == migrated)

        let existing = try fixture("existing")
        let retainedLegacy = try legacyData(existing)
        let current = existing.appendingPathComponent("FileSearch")
        try manager.createDirectory(at: current, withIntermediateDirectories: true)
        try Data("new-user-data".utf8).write(to: current.appendingPathComponent("index.sqlite"))
        _ = try LegacyDataMigration.dataDirectory(in: existing)
        check("existing new index is never overwritten", try Data(contentsOf: current.appendingPathComponent("index.sqlite")) == Data("new-user-data".utf8))
        check("legacy data remains when the new directory exists", manager.fileExists(atPath: retainedLegacy.appendingPathComponent("index.sqlite-wal").path))
        check("migration does not merge old ancillary files into new data", !manager.fileExists(atPath: current.appendingPathComponent("lists").path))

        let vacant = try fixture("vacant")
        let proposed = try LegacyDataMigration.dataDirectory(in: vacant)
        check("fresh installation resolves new location without creating or deleting data", try proposed.lastPathComponent == "FileSearch" && manager.contentsOfDirectory(atPath: vacant.path).isEmpty)

        let race = try fixture("race")
        _ = try legacyData(race)
        final class Outcomes: @unchecked Sendable {
            let lock = NSLock(); var urls = [URL](); var errors = [String]()
            func append(_ result: Result<URL, Error>) {
                lock.lock(); defer { lock.unlock() }
                switch result { case .success(let url): urls.append(url); case .failure(let error): errors.append(error.localizedDescription) }
            }
        }
        let outcomes = Outcomes()
        DispatchQueue.concurrentPerform(iterations: 8) { _ in
            outcomes.append(Result { try LegacyDataMigration.dataDirectory(in: race) })
        }
        check("concurrent first launches converge on the same intact directory", outcomes.errors.isEmpty && Set(outcomes.urls).count == 1 && outcomes.urls.count == 8)
        check("concurrent migration preserves the original bytes", try Data(contentsOf: race.appendingPathComponent("FileSearch/index.sqlite")) == Data("existing:index.sqlite".utf8))

        let suffix = UUID().uuidString
        let legacyDomain = "local.filesearch.tests.legacy." + suffix
        let destinationDomain = "local.filesearch.tests.current." + suffix
        let defaults = UserDefaults(suiteName: destinationDomain)!
        defer { defaults.removePersistentDomain(forName: legacyDomain); defaults.removePersistentDomain(forName: destinationDomain) }
        let legacyValues: [String: Any] = [
            "APFSearch.SidebarVisible": true, "APFSearch.HiddenColumns": ["created"],
            "NSWindow Frame APFSearch.Main": "100 100 850 600 0 0 1920 1080",
            "NSTableView Columns v3 APFSearch.Columns": [["identifier": "name", "width": 320]],
            "NSNavLastRootDirectory": "/Users/example/APFSearch.important",
            "AppleLanguages": ["zh-Hans"]]
        defaults.setPersistentDomain(legacyValues, forName: legacyDomain)
        defaults.setPersistentDomain(["FileSearch.SidebarVisible": false, "custom.new.setting": 42], forName: destinationDomain)
        LegacyDataMigration.preferences(defaults: defaults, legacyDomain: legacyDomain, destinationDomain: destinationDomain)
        let preferences = defaults.persistentDomain(forName: destinationDomain)!
        check("new user preferences take precedence", preferences["FileSearch.SidebarVisible"] as? Bool == false && preferences["custom.new.setting"] as? Int == 42)
        check("missing preferences migrate to the new prefix", preferences["FileSearch.HiddenColumns"] as? [String] == ["created"] && preferences["APFSearch.HiddenColumns"] == nil)
        check("AppKit frame and table autosave identifiers migrate", preferences["NSWindow Frame FileSearch.Main"] as? String == legacyValues["NSWindow Frame APFSearch.Main"] as? String && preferences["NSTableView Columns v3 FileSearch.Columns"] != nil)
        check("user paths and per-app language remain unchanged", preferences["NSNavLastRootDirectory"] as? String == legacyValues["NSNavLastRootDirectory"] as? String && preferences["AppleLanguages"] as? [String] == ["zh-Hans"])
        check("legacy preferences remain recoverable", defaults.persistentDomain(forName: legacyDomain) as NSDictionary? == legacyValues as NSDictionary)
        var changed = preferences; changed.removeValue(forKey: "FileSearch.HiddenColumns")
        defaults.setPersistentDomain(changed, forName: destinationDomain)
        LegacyDataMigration.preferences(defaults: defaults, legacyDomain: legacyDomain, destinationDomain: destinationDomain)
        check("one-time migration does not resurrect deliberately removed settings", defaults.persistentDomain(forName: destinationDomain)?["FileSearch.HiddenColumns"] == nil)
        let report: [String: Any] = ["success": failures.isEmpty, "count": passed.count, "passed": passed, "failures": failures, "scope": "Temporary directories and UUID-isolated preferences domains; no installed app, actual index, or user preferences changed"]
        print(String(data: try JSONSerialization.data(withJSONObject: report, options: [.sortedKeys]), encoding: .utf8)!)
        exit(failures.isEmpty ? 0 : 1)
    }
}
