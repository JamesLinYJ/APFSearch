# Retiring a previous installation

`RetirePreviousInstallation.swift` is an optional maintenance helper for developers migrating a signed installation. It supports read-only `--status` and explicit `--unregister` modes. It does not register services, change permissions, or modify index data.

The helper must run inside the expected app bundle. It validates the enclosing path, bundle identifier, signature, signing team, and agent plist before accessing Service Management. Build and sign the helper and its containing maintenance app with the identity authorized for that installation; do not copy credentials or machine-specific deployment receipts into this repository.

## Procedure

1. Back up the prior application and its preferences. Stop database writes before preserving its database together with any WAL/SHM files.
2. Close the old GUI and stop its scan worker through the supported interface. Prepare a same-identity maintenance app containing this helper; preserve the original executable in the backup.
3. Run the helper with `--status`, `--expected-bundle`, `--expected-bundle-id`, `--expected-team-id`, and `--agent-plist`, supplying the actual values for the installation. Inspect the identity and service state before proceeding.
4. Run the same command with `--unregister`. Require a successful exit and an observed `notRegistered` agent state. Independently verify the old service has stopped. Do not reopen the old GUI if its startup code re-registers the service.
5. Install the new application after the previous service has retired. Preserve existing destination data rather than overwriting it. Background-service and Full Disk Access permissions may need to be granted again.

The helper uses asynchronous `SMAppService.unregister(completionHandler:)` and waits for completion. It reports per-service outcomes as JSON. A missing main-app login record is accepted as already absent; an unexpectedly missing agent remains a visible failure. Local reports can contain installation details and should stay outside Git.

The shared source uses neutral legacy identifiers. It does not encode a developer's earlier private bundle identifiers. If migrating such an installation, configure its identity in a private deployment workflow; do not assume the generic source will discover every historical installation.

## API boundaries

An agent plist belongs in the calling app's `Contents/Library/LaunchAgents`; passing a different installation's label does not give a standalone helper control of it. See [Apple's agent API](https://developer.apple.com/documentation/servicemanagement/smappservice/agent(plistname:)) and [unregister API](https://developer.apple.com/documentation/servicemanagement/smappservice/unregister(completionhandler:)).

Historical background-item records may remain visible after retirement. Verify actual service registration and process state rather than treating disappearance from System Settings as the sole success condition. Do not reset the system background-item database, inject code, or disable signature checks.
