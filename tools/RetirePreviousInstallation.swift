// A signed maintenance executable for a previous installation's app bundle.
// It never edits user data, changes permissions, or registers a service.
import Foundation
import ServiceManagement
import Security

private struct MigrationFailure: LocalizedError, CustomNSError {
    let message: String
    var errorDescription: String? { message }
    static var errorDomain: String { "local.filesearch.installation-maintenance" }
    var errorCode: Int { 1 }
    var errorUserInfo: [String: Any] { [NSLocalizedDescriptionKey: message] }
}

private struct Options {
    enum Mode: String { case status, unregister }
    let mode: Mode
    let expectedBundle: URL
    let expectedIdentifier: String
    let expectedTeam: String
    let agentPlist: String

    init(_ arguments: [String]) throws {
        var values: [String: String] = [:]
        var selectedMode: Mode?
        var index = 0
        while index < arguments.count {
            let argument = arguments[index]
            if argument == "--status" || argument == "--unregister" {
                guard selectedMode == nil else { throw MigrationFailure(message: "Select exactly one mode.") }
                selectedMode = argument == "--status" ? .status : .unregister
                index += 1
                continue
            }
            guard ["--expected-bundle", "--expected-bundle-id", "--expected-team-id", "--agent-plist"].contains(argument),
                  values[argument] == nil, index + 1 < arguments.count else {
                throw MigrationFailure(message: "Unknown, duplicate, or incomplete argument: \(argument)")
            }
            values[argument] = arguments[index + 1]
            index += 2
        }
        guard let mode = selectedMode,
              let path = values["--expected-bundle"], path.hasPrefix("/"), path.hasSuffix(".app"),
              let identifier = values["--expected-bundle-id"], !identifier.isEmpty,
              let team = values["--expected-team-id"], !team.isEmpty,
              let plist = values["--agent-plist"], plist.hasSuffix(".plist"),
              !plist.contains("/"), !plist.contains("\\"), !plist.contains("\0") else {
            throw MigrationFailure(message: "Required: --status|--unregister --expected-bundle /absolute/Application.app --expected-bundle-id ID --expected-team-id TEAM --agent-plist NAME.plist")
        }
        self.mode = mode
        expectedBundle = URL(fileURLWithPath: path).standardizedFileURL.resolvingSymlinksInPath()
        expectedIdentifier = identifier
        expectedTeam = team
        agentPlist = plist
    }
}

private func state(_ service: SMAppService) -> [String: Any] {
    let status = service.status
    let name: String
    switch status {
    case .notRegistered: name = "not_registered"
    case .enabled: name = "enabled"
    case .requiresApproval: name = "requires_approval"
    case .notFound: name = "not_found"
    @unknown default: name = "unknown"
    }
    return ["name": name, "raw_value": status.rawValue]
}

private func validateSignature(at bundle: URL, options: Options) throws -> [String: Any] {
    var staticCode: SecStaticCode?
    var result = SecStaticCodeCreateWithPath(bundle as CFURL, SecCSFlags(rawValue: 0), &staticCode)
    guard result == errSecSuccess, let staticCode else {
        throw MigrationFailure(message: "Cannot inspect the containing app signature (OSStatus \(result)).")
    }
    result = SecStaticCodeCheckValidity(staticCode, SecCSFlags(rawValue: kSecCSStrictValidate), nil)
    guard result == errSecSuccess else {
        throw MigrationFailure(message: "Containing app signature is invalid (OSStatus \(result)).")
    }
    var information: CFDictionary?
    result = SecCodeCopySigningInformation(staticCode, SecCSFlags(rawValue: kSecCSSigningInformation), &information)
    guard result == errSecSuccess, let information = information as? [String: Any],
          let identifier = information[kSecCodeInfoIdentifier as String] as? String,
          let team = information[kSecCodeInfoTeamIdentifier as String] as? String,
          identifier == options.expectedIdentifier, team == options.expectedTeam else {
        throw MigrationFailure(message: "Containing app signature does not match the expected identifier and team.")
    }
    return ["valid": true, "identifier": identifier, "team_id": team]
}

private func unregisterAndWait(_ service: SMAppService) async throws {
    // The completion API waits until a running helper has been terminated. The
    // synchronous overload may return before launchd has reaped the process.
    try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
        service.unregister { error in
            if let error { continuation.resume(throwing: error) }
            else { continuation.resume() }
        }
    }
}

@main
private struct RetirePreviousInstallation {
    static func main() async {
        let actualBundle = Bundle.main.bundleURL.standardizedFileURL.resolvingSymlinksInPath()
        var report: [String: Any] = [
            "success": false,
            "timestamp": ISO8601DateFormatter().string(from: Date()),
            "actual_bundle_path": actualBundle.path,
            "actual_bundle_id": Bundle.main.bundleIdentifier as Any? ?? NSNull(),
            "registered_new_services": false,
            "modified_user_data": false,
        ]
        do {
            let options = try Options(Array(CommandLine.arguments.dropFirst()))
            report["mode"] = options.mode.rawValue
            report["expected_bundle_path"] = options.expectedBundle.path
            report["expected_bundle_id"] = options.expectedIdentifier
            report["expected_team_id"] = options.expectedTeam
            report["agent_plist"] = options.agentPlist
            guard actualBundle == options.expectedBundle,
                  Bundle.main.bundleIdentifier == options.expectedIdentifier else {
                throw MigrationFailure(message: "Refusing: the executable is not running inside the explicitly expected app bundle.")
            }
            report["signature"] = try validateSignature(at: actualBundle, options: options)
            let plist = actualBundle.appendingPathComponent("Contents/Library/LaunchAgents").appendingPathComponent(options.agentPlist)
            guard FileManager.default.fileExists(atPath: plist.path) else {
                throw MigrationFailure(message: "Expected agent plist is missing from the containing app.")
            }
            let agent = SMAppService.agent(plistName: options.agentPlist)
            let mainApp = SMAppService.mainApp
            report["agent_before"] = state(agent)
            report["main_app_before"] = state(mainApp)
            report["login_was_enabled"] = mainApp.status == .enabled
            report["login_was_registered"] = mainApp.status == .enabled || mainApp.status == .requiresApproval
            if options.mode == .status {
                report["agent_after"] = state(agent)
                report["main_app_after"] = state(mainApp)
                report["success"] = true
            } else {
                var errors: [[String: Any]] = []
                var unregistration: [String: Bool] = [:]
                for (name, service) in [("agent", agent), ("main_app", mainApp)] {
                    let before = service.status
                    // A missing main-app login record means it was never enabled.
                    // A missing agent is ambiguous despite an existing plist, so
                    // retain an explicit failure and do not call it unregistered.
                    if before == .notRegistered || (name == "main_app" && before == .notFound) {
                        unregistration[name] = true
                        continue
                    }
                    do {
                        try await unregisterAndWait(service)
                        guard service.status == .notRegistered else {
                            throw MigrationFailure(message: "Unregister completed but \(name) status is not notRegistered.")
                        }
                        unregistration[name] = true
                    } catch {
                        let error = error as NSError
                        // A concurrent retirement can win the same operation.
                        // Accept only an observed unregistered state afterward.
                        if service.status == .notRegistered {
                            unregistration[name] = true
                        } else {
                            unregistration[name] = false
                            errors.append(["service": name, "domain": error.domain, "code": error.code, "message": error.localizedDescription])
                        }
                    }
                }
                report["unregistration"] = unregistration
                report["agent_after"] = state(agent)
                report["main_app_after"] = state(mainApp)
                report["errors"] = errors
                report["success"] = errors.isEmpty && unregistration["agent"] == true && unregistration["main_app"] == true
            }
        } catch {
            let error = error as NSError
            report["error"] = ["domain": error.domain, "code": error.code, "message": error.localizedDescription]
        }
        let success = report["success"] as? Bool == true
        do {
            let data = try JSONSerialization.data(withJSONObject: report, options: [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes])
            FileHandle.standardOutput.write(data)
            FileHandle.standardOutput.write(Data([10]))
        } catch {
            FileHandle.standardError.write(Data("Unable to serialize migration report: \(error)\n".utf8))
            exit(2)
        }
        exit(success ? 0 : 1)
    }
}
