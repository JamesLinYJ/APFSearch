import Foundation
import Security

/// Identifiers shared by the application, command-line client and indexer.
enum ApplicationIdentity {
    static let applicationExecutable = "APFSearch"
    static let serviceExecutable = "APFSearchService"
    static let cliExecutable = "apfsearch-cli"
    static let bundleIdentifier = "org.apfsearch.app"
    static let serviceIdentifier = "org.apfsearch.indexer"
    static let cliIdentifier = "org.apfsearch.cli"
    static let dataDirectoryName = "APFSearch"
    static let dataDirectoryEnvironment = "APFSEARCH_DATA_DIR"
    static let metricsEnvironment = "APFSEARCH_METRICS_PATH"
    static let preferencePrefix = "APFSearch."

    /// Derive trust from this indexer's verified signature, never preferences or
    /// an environment variable. Unsigned, ad-hoc, and untrusted builds fail closed.
    static let clientSecurityRequirement: String? = {
        var ownCode: SecCode?
        guard SecCodeCopySelf([], &ownCode) == errSecSuccess, let ownCode else { return nil }
        var ownRequirement: SecRequirement?
        let ownRule = "anchor apple generic and identifier \"\(serviceIdentifier)\""
        guard SecRequirementCreateWithString(ownRule as CFString, [], &ownRequirement) == errSecSuccess,
              let ownRequirement,
              SecCodeCheckValidity(ownCode, [], ownRequirement) == errSecSuccess else { return nil }

        var staticCode: SecStaticCode?
        var information: CFDictionary?
        guard SecCodeCopyStaticCode(ownCode, [], &staticCode) == errSecSuccess, let staticCode,
              SecCodeCopySigningInformation(staticCode, SecCSFlags(rawValue: kSecCSSigningInformation), &information) == errSecSuccess,
              let information = information as? [String: Any],
              let team = information[kSecCodeInfoTeamIdentifier as String] as? String,
              team.range(of: "^[A-Z0-9]{10}$", options: .regularExpression) != nil else { return nil }
        return "anchor apple generic and certificate leaf[subject.OU] = \"\(team)\" and (identifier \"\(bundleIdentifier)\" or identifier \"\(cliIdentifier)\")"
    }()
}
