import Foundation
import Darwin

/// Preserve existing data when upgrading from the previous bundle identity.
/// The installer stops the old indexer before launching the new service.
enum LegacyDataMigration {
    static func dataDirectory(in supportDirectory: URL) throws -> URL {
        let destination = supportDirectory.appendingPathComponent(ApplicationIdentity.dataDirectoryName, isDirectory: true)
        let legacy = supportDirectory.appendingPathComponent(ApplicationIdentity.legacyDataDirectoryName, isDirectory: true)
        if exists(destination) { return destination }
        guard exists(legacy) else { return destination }
        // Rename the entire directory on the same volume: database, WAL, file
        // operation journal and offline lists move together. RENAME_EXCL also
        // protects a destination created concurrently by another process.
        let result = legacy.path.withCString { from in
            destination.path.withCString { to in
                renameatx_np(AT_FDCWD, from, AT_FDCWD, to, UInt32(RENAME_EXCL))
            }
        }
        if result != 0 {
            let failure = errno
            if (failure == EEXIST || failure == ENOENT) && exists(destination) { return destination }
            throw NSError(domain: NSPOSIXErrorDomain, code: Int(failure), userInfo: [NSFilePathErrorKey: legacy.path])
        }
        return destination
    }

    private static func exists(_ url: URL) -> Bool {
        var metadata = stat()
        return url.path.withCString { lstat($0, &metadata) } == 0
    }

    static func preferences(
        defaults: UserDefaults = .standard,
        legacyDomain: String = ApplicationIdentity.legacyBundleIdentifier,
        destinationDomain: String = ApplicationIdentity.bundleIdentifier
    ) {
        var current = defaults.persistentDomain(forName: destinationDomain) ?? [:]
        guard current[ApplicationIdentity.preferencesMigrationKey] as? Bool != true else { return }
        if let legacy = defaults.persistentDomain(forName: legacyDomain) {
            for (key, value) in legacy {
                // Autosave keys have AppKit prefixes, e.g. "NSWindow Frame ...".
                // Translate identifiers only; user text and paths stay untouched.
                let destinationKey = key.replacingOccurrences(
                    of: ApplicationIdentity.legacyPreferencePrefix,
                    with: ApplicationIdentity.preferencePrefix)
                if current[destinationKey] == nil { current[destinationKey] = value }
            }
        }
        current[ApplicationIdentity.preferencesMigrationKey] = true
        defaults.setPersistentDomain(current, forName: destinationDomain)
    }
}
