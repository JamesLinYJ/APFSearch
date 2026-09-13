import AppKit
import CryptoKit
import Darwin
import Foundation
import PDFKit

@main struct FeatureTests {
  static func main() throws {
    let temporaryRoot = ProcessInfo.processInfo.environment["APFSEARCH_FILE_TEST_ROOT"].map { URL(fileURLWithPath: $0, isDirectory: true) }
      ?? FileManager.default.temporaryDirectory
    let root = temporaryRoot.appendingPathComponent("APFSearch-features-" + UUID().uuidString)
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: false)
    defer { try? FileManager.default.removeItem(at: root) }
    setenv("APFSEARCH_DATA_DIR", root.appendingPathComponent("engine").path, 1)
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
    check("new operation history writes schema version one", (history["operations"] as? [[String: Any]])?.first?["schema_version"] as? Int == 1)
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

    // Authority stays bound to the reviewed/opened parent objects, even when
    // another process changes the spelling that originally reached them.
    func directory(_ name: String) throws -> URL {
      let path = root.appendingPathComponent(name, isDirectory: true)
      try FileManager.default.createDirectory(at: path, withIntermediateDirectories: true)
      return path
    }
    func canonicalDirectory(_ url: URL) throws -> URL {
      guard let path = url.path.withCString({ realpath($0, nil) }) else { throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno)) }
      defer { free(path) }
      return URL(fileURLWithPath: String(cString: path), isDirectory: true)
    }
    let unavailable = "files.undo_authority_unavailable"
    let unsupported = "files.undo_version_unsupported"
    let historyCases: [(String, String?)] = [
      ("current", nil), ("move", nil),
      ("replaced-parent", "error.undo_destination_changed"), ("changed-leaf", "error.undo_destination_changed"),
      ("missing-destination-parent", unavailable), ("missing-parent-path", unavailable),
      ("unversioned", unsupported), ("unsupported-version", unsupported),
      ("boolean-version", unsupported), ("string-version", unsupported),
      ("unknown-action", unavailable), ("missing-parent", unavailable), ("invalid-parent", unavailable),
      ("missing-after", unavailable), ("incomplete-after", unavailable), ("invalid-path", unavailable),
      ("empty-id", unavailable), ("invalid-bookmark", unavailable), ("unsupported-controls", unsupported)]
    for (scenario, expectedError) in historyCases {
      let journalDirectory = try directory("record-journal-" + scenario)
      let parent = try canonicalDirectory(directory("record-parent-" + scenario))
      let targetParent = scenario == "move" ? try canonicalDirectory(directory("record-target-" + scenario)) : parent
      let original = parent.appendingPathComponent("original.txt")
      let renamed = targetParent.appendingPathComponent("renamed.txt")
      try Data("recorded original".utf8).write(to: original)
      let parentAuthority = try OperationIdentity(parent)
      let destinationAuthority = try OperationIdentity(targetParent)
      try FileManager.default.moveItem(at: original, to: renamed)
      var leafAuthority = try OperationIdentity(renamed)
      if scenario == "replaced-parent" {
        let held = root.appendingPathComponent("record-parent-held")
        try FileManager.default.moveItem(at: parent, to: held)
        try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: false)
        try FileManager.default.moveItem(at: held.appendingPathComponent("renamed.txt"), to: renamed)
        leafAuthority = try OperationIdentity(renamed)
      }
      var record: [String: Any] = ["schema_version": 1, "kind": "operation", "id": "record-" + scenario,
        "action": scenario == "move" ? "move" : "rename", "source": original.path, "destination": renamed.path,
        "source_parent": parentAuthority.wire, "destination_parent": destinationAuthority.wire,
        "source_parent_path": parent.path, "destination_parent_path": targetParent.path,
        "after": leafAuthority.wire, "undone": false]
      switch scenario {
      case "missing-destination-parent": record["destination_parent"] = nil
      case "missing-parent-path": record["source_parent_path"] = nil
      case "unversioned": record["schema_version"] = nil
      case "unsupported-version": record["schema_version"] = 2
      case "boolean-version": record["schema_version"] = true
      case "string-version": record["schema_version"] = "1"
      case "unknown-action": record["action"] = "unknown"
      case "missing-parent": record["source_parent"] = nil
      case "invalid-parent": record["source_parent"] = leafAuthority.wire
      case "missing-after": record["after"] = nil
      case "incomplete-after": var after = leafAuthority.wire; after["changed_nsec"] = nil; record["after"] = after
      case "invalid-path": record["destination"] = "relative/renamed.txt"
      case "empty-id": record["id"] = ""
      case "invalid-bookmark": record["destination_bookmark"] = "not base64"
      default: break
      }
      var journalData = try JSONSerialization.data(withJSONObject: record, options: [.sortedKeys])
      journalData.append(0x0A)
      if ["changed-leaf", "unsupported-controls"].contains(scenario) {
        try Data("recorded modified".utf8).write(to: renamed)
        var times = [timespec(tv_sec: Int(leafAuthority.modified), tv_nsec: Int(leafAuthority.modifiedNS)),
          timespec(tv_sec: Int(leafAuthority.modified), tv_nsec: Int(leafAuthority.modifiedNS))]
        let restored = renamed.path.withCString { path in times.withUnsafeMutableBufferPointer { utimensat(AT_FDCWD, path, $0.baseAddress, 0) } }
        guard restored == 0 else { throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno)) }
        if scenario == "unsupported-controls" {
          let changed = try OperationIdentity(renamed)
          let controls: [[String: Any]] = [
            ["kind": "identity_change", "before": leafAuthority.wire, "after": changed.wire],
            ["kind": "undo", "target_id": record["id"]!], ["kind": "undo_intent", "target_id": record["id"]!]]
          for var control in controls {
            control["schema_version"] = 2
            journalData.append(try JSONSerialization.data(withJSONObject: control, options: [.sortedKeys])); journalData.append(0x0A)
          }
        }
      }
      let journalPath = journalDirectory.appendingPathComponent("file-operations.jsonl")
      try journalData.write(to: journalPath)
      let recordFiles = FileOperations(directory: journalDirectory)
      let response = recordFiles.undo()
      if let expectedError {
        try check("undo rejects unsupported or incomplete authority: " + scenario,
          response["success"] as? Bool == false && response["error_key"] as? String == expectedError
            && FileManager.default.fileExists(atPath: renamed.path) && !FileManager.default.fileExists(atPath: original.path)
            && Data(contentsOf: journalPath) == journalData)
      } else {
        try check("format one undo uses the recorded authorities: " + scenario,
          response["success"] as? Bool == true && String(contentsOf: original, encoding: .utf8) == "recorded original"
            && OperationIdentity(original).inode == leafAuthority.inode && !FileManager.default.fileExists(atPath: renamed.path))
        let updated = try Data(contentsOf: journalPath)
        let appended = updated.dropFirst(journalData.count).split(separator: 0x0A)
        let versioned = appended.allSatisfy { ((try? JSONSerialization.jsonObject(with: Data($0))) as? [String: Any])?["schema_version"] as? Int == 1 }
        check("undo appends format one without rewriting completed records: " + scenario,
          updated.starts(with: journalData) && !appended.isEmpty && versioned)
      }
    }
    let oldHistoryDirectory = try directory("unsupported-history-file")
    let oldHistory = oldHistoryDirectory.appendingPathComponent("file-operations.json")
    let oldBytes = Data("[{\"id\":\"unversioned\",\"action\":\"rename\"}]".utf8)
    try oldBytes.write(to: oldHistory)
    try check("previous JSON journal is neither loaded nor changed", FileOperations(directory: oldHistoryDirectory).log().isEmpty && Data(contentsOf: oldHistory) == oldBytes)

    let reviewedSource = try fixture("review-source.txt")
    let reviewedDestination = try directory("review-destination")
    let reviewOutside = try directory("review-outside")
    let reviewRequest: [String: Any] = ["action": "copy", "paths": [reviewedSource.path], "destination": reviewedDestination.path]
    var reviewDry = reviewRequest; reviewDry["dry_run"] = true
    let reviewReply = files.perform(reviewDry)
    var reviewExecute = reviewRequest; reviewExecute["expected"] = reviewReply["expected"]
    reviewExecute["expected_parents"] = reviewReply["expected_parents"]
    try FileManager.default.moveItem(at: reviewedDestination, to: root.appendingPathComponent("review-destination-held"))
    try FileManager.default.createSymbolicLink(at: reviewedDestination, withDestinationURL: reviewOutside)
    let reviewRejected = files.perform(reviewExecute)
    check("destination replacement after review is rejected before copying", reviewRejected["success"] as? Bool == false
      && !FileManager.default.fileExists(atPath: reviewOutside.appendingPathComponent(reviewedSource.lastPathComponent).path))

    let raceSourceDirectory = try directory("race-source")
    let raceTargetDirectory = try directory("race-target")
    let raceOutside = try directory("race-outside")
    let raceSource = raceSourceDirectory.appendingPathComponent("selected.txt")
    try Data("authorized payload".utf8).write(to: raceSource)
    try Data("outside payload".utf8).write(to: raceOutside.appendingPathComponent("selected.txt"))
    let heldSource = root.appendingPathComponent("race-source-held")
    let heldTarget = root.appendingPathComponent("race-target-held")
    files.testHooks.afterParentsOpened = {
      try FileManager.default.moveItem(at: raceSourceDirectory, to: heldSource)
      try FileManager.default.createSymbolicLink(at: raceSourceDirectory, withDestinationURL: raceOutside)
      try FileManager.default.moveItem(at: raceTargetDirectory, to: heldTarget)
      try FileManager.default.createSymbolicLink(at: raceTargetDirectory, withDestinationURL: raceOutside)
    }
    let anchoredCopy = files.perform(["action": "copy", "paths": [raceSource.path], "destination": raceTargetDirectory.path])
    files.testHooks = OperationTestHooks()
    try check("copy pins both source and destination across ancestor replacement", anchoredCopy["success"] as? Bool == true
      && String(contentsOf: heldTarget.appendingPathComponent("selected.txt"), encoding: .utf8) == "authorized payload"
      && String(contentsOf: raceOutside.appendingPathComponent("selected.txt"), encoding: .utf8) == "outside payload")
    check("completed journal records the current path of the pinned destination", (anchoredCopy["results"] as? [[String: Any]])?.first?["destination"] as? String
      == heldTarget.resolvingSymlinksInPath().appendingPathComponent("selected.txt").path
      || ((anchoredCopy["results"] as? [[String: Any]])?.first?["destination"] as? String)?.hasSuffix("/race-target-held/selected.txt") == true)

    let leafSource = try fixture("capture-source.txt", "authorized inode")
    let leafParked = root.appendingPathComponent("capture-original-held.txt")
    files.testHooks.beforeCapture = {
      try FileManager.default.moveItem(at: leafSource, to: leafParked)
      try Data("replacement inode".utf8).write(to: leafSource)
    }
    let leafRejected = files.perform(["action": "rename", "paths": [leafSource.path], "new_name": "capture-target.txt"])
    files.testHooks = OperationTestHooks()
    try check("raced source leaf is restored without being published or destroyed", leafRejected["success"] as? Bool == false
      && String(contentsOf: leafSource, encoding: .utf8) == "replacement inode"
      && String(contentsOf: leafParked, encoding: .utf8) == "authorized inode"
      && !FileManager.default.fileExists(atPath: root.appendingPathComponent("capture-target.txt").path))

    let changedCopySource = try fixture("copy-change-source.txt", "before")
    let changedCopyDestination = try directory("copy-change-destination")
    let changedCopyStamp = try OperationIdentity(changedCopySource)
    files.testHooks.afterParentsOpened = {
      try Data("edited".utf8).write(to: changedCopySource)
      var times = [timespec(tv_sec: Int(changedCopyStamp.modified), tv_nsec: Int(changedCopyStamp.modifiedNS)),
        timespec(tv_sec: Int(changedCopyStamp.modified), tv_nsec: Int(changedCopyStamp.modifiedNS))]
      let restored = changedCopySource.path.withCString { path in times.withUnsafeMutableBufferPointer { utimensat(AT_FDCWD, path, $0.baseAddress, 0) } }
      guard restored == 0 else { throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno)) }
    }
    let changedCopyRejected = files.perform(["action": "copy", "paths": [changedCopySource.path], "destination": changedCopyDestination.path])
    files.testHooks = OperationTestHooks()
    check("copy never recaptures a changed source as a new authorization", changedCopyRejected["success"] as? Bool == false
      && !FileManager.default.fileExists(atPath: changedCopyDestination.appendingPathComponent(changedCopySource.lastPathComponent).path))

    let searchOnlyDirectory = try directory("search-only-parent")
    let searchOnlySource = searchOnlyDirectory.appendingPathComponent("before.txt")
    try Data("search-only rename".utf8).write(to: searchOnlySource)
    guard chmod(searchOnlyDirectory.path, 0o300) == 0 else { throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno)) }
    let searchOnlyRename = files.perform(["action": "rename", "paths": [searchOnlySource.path], "new_name": "after.txt"])
    guard chmod(searchOnlyDirectory.path, 0o700) == 0 else { throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno)) }
    check("parent authority requires search permission without directory enumeration", searchOnlyRename["success"] as? Bool == true
      && FileManager.default.fileExists(atPath: searchOnlyDirectory.appendingPathComponent("after.txt").path))

    let publishSource = try fixture("publish-source.txt", "retain source")
    let publishTarget = root.appendingPathComponent("publish-target.txt")
    files.testHooks.beforePublish = { try Data("new occupant".utf8).write(to: publishTarget) }
    let publishRejected = files.perform(["action": "rename", "paths": [publishSource.path], "new_name": publishTarget.lastPathComponent])
    files.testHooks = OperationTestHooks()
    try check("atomic publication never overwrites a raced destination", publishRejected["success"] as? Bool == false
      && String(contentsOf: publishSource, encoding: .utf8) == "retain source"
      && String(contentsOf: publishTarget, encoding: .utf8) == "new occupant")

    let recoverySource = try fixture("recovery-source.txt", "recover authorized data")
    let recoveryTarget = root.appendingPathComponent("recovery-target.txt")
    files.testHooks.afterCapture = { try Data("new source occupant".utf8).write(to: recoverySource) }
    files.testHooks.beforePublish = { try Data("new destination occupant".utf8).write(to: recoveryTarget) }
    let needsRecovery = files.perform(["action": "rename", "paths": [recoverySource.path], "new_name": recoveryTarget.lastPathComponent])
    files.testHooks = OperationTestHooks()
    let recoveries = ((needsRecovery["results"] as? [[String: Any]])?.first?["recovery_paths"] as? [String]) ?? []
    try check("failed rollback preserves both occupants and the captured payload", needsRecovery["success"] as? Bool == false && recoveries.count == 1
      && String(contentsOfFile: recoveries.first ?? "", encoding: .utf8) == "recover authorized data"
      && String(contentsOf: recoverySource, encoding: .utf8) == "new source occupant"
      && String(contentsOf: recoveryTarget, encoding: .utf8) == "new destination occupant")
    let durableLog = try String(contentsOf: files.journal, encoding: .utf8)
    check("recovery location is present in the durable pre-mutation intent", recoveries.first.map { durableLog.contains($0.replacingOccurrences(of: "/", with: "\\/")) || durableLog.contains($0) } == true)

    let cloneSource = try fixture("clone-source.txt", String(repeating: "cloned data", count: 512))
    let cloneDestination = try directory("clone-output")
    let cloneResult = files.perform(["action": "copy", "paths": [cloneSource.path], "destination": cloneDestination.path])
    try check("APFS regular copy uses a clone and preserves bytes with a distinct inode", cloneResult["success"] as? Bool == true
      && files.testHooks.clonedFiles == 1 && files.testHooks.copiedFiles == 0
      && OperationIdentity(cloneSource).inode != OperationIdentity(cloneDestination.appendingPathComponent(cloneSource.lastPathComponent)).inode
      && Data(contentsOf: cloneSource) == Data(contentsOf: cloneDestination.appendingPathComponent(cloneSource.lastPathComponent)))
    files.testHooks = OperationTestHooks(); files.testHooks.forceCopyFallback = true
    let fallbackDestination = try directory("fallback-output")
    let fallbackResult = files.perform(["action": "copy", "paths": [cloneSource.path], "destination": fallbackDestination.path])
    try check("unsupported clone falls back once to a descriptor copy", fallbackResult["success"] as? Bool == true
      && files.testHooks.clonedFiles == 0 && files.testHooks.copiedFiles == 1
      && Data(contentsOf: cloneSource) == Data(contentsOf: fallbackDestination.appendingPathComponent(cloneSource.lastPathComponent)))
    files.testHooks = OperationTestHooks()

    let tree = try directory("copy-tree")
    let treeChild = tree.appendingPathComponent("nested", isDirectory: true)
    try FileManager.default.createDirectory(at: treeChild, withIntermediateDirectories: false)
    let treeFile = treeChild.appendingPathComponent("entry.txt")
    try Data("tree bytes".utf8).write(to: treeFile)
    try FileManager.default.linkItem(at: treeFile, to: tree.appendingPathComponent("hardlink.txt"))
    try FileManager.default.createSymbolicLink(atPath: tree.appendingPathComponent("dangling-link").path, withDestinationPath: "does-not-exist")
    try FileManager.default.createSymbolicLink(at: tree.appendingPathComponent("directory-link"), withDestinationURL: raceOutside)
    let treeOutput = try directory("tree-output")
    let treeResult = files.perform(["action": "copy", "paths": [tree.path], "destination": treeOutput.path])
    let copiedTree = treeOutput.appendingPathComponent(tree.lastPathComponent)
    try check("recursive descriptor copy preserves hard links and symlink entries", treeResult["success"] as? Bool == true
      && OperationIdentity(copiedTree.appendingPathComponent("nested/entry.txt")).inode == OperationIdentity(copiedTree.appendingPathComponent("hardlink.txt")).inode
      && FileManager.default.destinationOfSymbolicLink(atPath: copiedTree.appendingPathComponent("dangling-link").path) == "does-not-exist"
      && FileManager.default.destinationOfSymbolicLink(atPath: copiedTree.appendingPathComponent("directory-link").path) == raceOutside.path)

    let undoDirectory = try directory("undo-parent")
    let undoOutside = try directory("undo-outside")
    let undoFile = undoDirectory.appendingPathComponent("before.txt")
    try Data("undo payload".utf8).write(to: undoFile)
    let undoFiles = FileOperations(directory: root)
    let undoRename = undoFiles.perform(["action": "rename", "paths": [undoFile.path], "new_name": "after.txt"])
    let undoHeld = root.appendingPathComponent("undo-parent-held")
    undoFiles.testHooks.afterParentsOpened = {
      try FileManager.default.moveItem(at: undoDirectory, to: undoHeld)
      try FileManager.default.createSymbolicLink(at: undoDirectory, withDestinationURL: undoOutside)
    }
    let anchoredUndo = undoFiles.undo()
    try check("undo restores through pinned parents after ancestor replacement", undoRename["success"] as? Bool == true && anchoredUndo["success"] as? Bool == true
      && String(contentsOf: undoHeld.appendingPathComponent("before.txt"), encoding: .utf8) == "undo payload"
      && !FileManager.default.fileExists(atPath: undoOutside.appendingPathComponent("before.txt").path))

    // Native Trash acceptance is an isolated opt-in because sandboxed test
    // runners cannot access the user's Trash. Every item is created here and
    // restored or removed by this fixture, never selected from user data.
    if ProcessInfo.processInfo.environment["APFSEARCH_TEST_NATIVE_TRASH"] == "1" {
      let trashParent = try directory("native-trash-source")
      let trashOutside = try directory("native-trash-outside")
      let trashSource = trashParent.appendingPathComponent("APFSearch-trash-fixture-" + UUID().uuidString + ".txt")
      try Data("native trash fixture".utf8).write(to: trashSource)
      let trashFiles = FileOperations(directory: root)
      let trashHeld = root.appendingPathComponent("native-trash-source-held")
      trashFiles.testHooks.beforeNativeTrash = {
        try FileManager.default.moveItem(at: trashParent, to: trashHeld)
        try FileManager.default.createSymbolicLink(at: trashParent, withDestinationURL: trashOutside)
      }
      let trashed = trashFiles.perform(["action": "trash", "paths": [trashSource.path]])
      trashFiles.testHooks = OperationTestHooks()
      let trashRow = (trashed["results"] as? [[String: Any]])?.first ?? [:]
      let trashPath = trashRow["destination"] as? String ?? ""
      try check("native Trash acts on private captured source across ancestor replacement", trashed["success"] as? Bool == true
        && String(contentsOfFile: trashPath, encoding: .utf8) == "native trash fixture"
        && !FileManager.default.fileExists(atPath: trashOutside.appendingPathComponent(trashSource.lastPathComponent).path))
      try FileManager.default.removeItem(at: trashParent)
      try FileManager.default.moveItem(at: trashHeld, to: trashParent)
      let trashUndone = trashed["success"] as? Bool == true ? trashFiles.undo() : ["success": false]
      try check("native Trash undo restores the original fixture", trashUndone["success"] as? Bool == true
        && String(contentsOf: trashSource, encoding: .utf8) == "native trash fixture")
      let trashCopyDirectory = try directory("native-copy-undo")
      // A just-restored Trash item can receive OS metadata updates during its
      // first clone. Keep the exact ctime guard; test copy-undo independently.
      let copyUndoSource = try fixture("APFSearch-copy-undo-fixture-" + UUID().uuidString + ".txt", "native copy undo fixture")
      let copied = trashFiles.perform(["action": "copy", "paths": [copyUndoSource.path], "destination": trashCopyDirectory.path])
      let copyUndone = copied["success"] as? Bool == true ? trashFiles.undo() : ["success": false]
      check("copy undo retains native Trash behavior", copied["success"] as? Bool == true && copyUndone["success"] as? Bool == true
        && !FileManager.default.fileExists(atPath: trashCopyDirectory.appendingPathComponent(copyUndoSource.lastPathComponent).path))
      // Remove only the exact native result, using its returned capability;
      // the fixture never requests enumeration of the user's protected Trash.
      if let bookmark = copyUndone["destination_bookmark"] as? String, let data = Data(base64Encoded: bookmark) {
        var stale = false
        let url = try URL(resolvingBookmarkData: data, options: [.withSecurityScope, .withoutUI, .withoutMounting], relativeTo: nil, bookmarkDataIsStale: &stale)
        let scoped = url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
        try FileManager.default.removeItem(at: url)
      }
    }

    if let secondVolumePath = ProcessInfo.processInfo.environment["APFSEARCH_FILE_TEST_SECOND_VOLUME"] {
      let volume = URL(fileURLWithPath: secondVolumePath, isDirectory: true)
      let secondRoot = volume.appendingPathComponent("APFSearch-cross-volume-" + UUID().uuidString, isDirectory: true)
      try FileManager.default.createDirectory(at: secondRoot, withIntermediateDirectories: false)
      defer { try? FileManager.default.removeItem(at: secondRoot) }
      let localDevice = try OperationIdentity(root).device
      let remoteDevice = try OperationIdentity(secondRoot).device
      guard localDevice != remoteDevice else {
        throw NSError(domain: "APFSearch cross-volume fixture requires two different mounted filesystems", code: Int(EINVAL))
      }
      let crossParent = try directory("cross-volume-source-parent")
      let crossTree = crossParent.appendingPathComponent("selected-tree", isDirectory: true)
      let crossNested = crossTree.appendingPathComponent("nested", isDirectory: true)
      try FileManager.default.createDirectory(at: crossNested, withIntermediateDirectories: true)
      let crossFile = crossNested.appendingPathComponent("payload.txt")
      try Data("cross-volume directory payload".utf8).write(to: crossFile)
      try FileManager.default.linkItem(at: crossFile, to: crossTree.appendingPathComponent("hardlink.txt"))
      try FileManager.default.createSymbolicLink(atPath: crossTree.appendingPathComponent("dangling-link").path, withDestinationPath: "missing-target")
      let crossOutside = try fixture("cross-volume-outside.txt", "outside must remain unchanged")
      try FileManager.default.createSymbolicLink(at: crossTree.appendingPathComponent("outside-link"), withDestinationURL: crossOutside)
      let crossOperations = FileOperations(directory: root)
      guard chmod(crossParent.path, 0o300) == 0 else { throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno)) }
      defer { _ = chmod(crossParent.path, 0o700) }
      let crossMove = crossOperations.perform(["action": "move", "paths": [crossTree.path], "destination": secondRoot.path])
      let movedTree = secondRoot.appendingPathComponent(crossTree.lastPathComponent)
      check("actual cross-volume directory move succeeds with a search-only source parent", crossMove["success"] as? Bool == true
        && crossMove["completed"] as? Int == 1 && !FileManager.default.fileExists(atPath: crossTree.path))
      if crossMove["success"] as? Bool == true {
        try check("cross-volume directory move preserves every hard-link and symlink entry", OperationIdentity(movedTree).device == remoteDevice
          && OperationIdentity(movedTree.appendingPathComponent("nested/payload.txt")).inode == OperationIdentity(movedTree.appendingPathComponent("hardlink.txt")).inode
          && String(contentsOf: movedTree.appendingPathComponent("nested/payload.txt"), encoding: .utf8) == "cross-volume directory payload"
          && FileManager.default.destinationOfSymbolicLink(atPath: movedTree.appendingPathComponent("dangling-link").path) == "missing-target"
          && FileManager.default.destinationOfSymbolicLink(atPath: movedTree.appendingPathComponent("outside-link").path) == crossOutside.path
          && String(contentsOf: crossOutside, encoding: .utf8) == "outside must remain unchanged")
        let reopenedCrossOperations = FileOperations(directory: root)
        let crossRestored = reopenedCrossOperations.undo()
        try check("cross-volume directory undo works after journal reopen and restores the original parent", crossRestored["success"] as? Bool == true
          && OperationIdentity(crossTree).device == localDevice && !FileManager.default.fileExists(atPath: movedTree.path)
          && OperationIdentity(crossTree.appendingPathComponent("nested/payload.txt")).inode == OperationIdentity(crossTree.appendingPathComponent("hardlink.txt")).inode
          && String(contentsOf: crossFile, encoding: .utf8) == "cross-volume directory payload")
      }
      guard chmod(crossParent.path, 0o700) == 0 else { throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno)) }

      let crossCopySource = try fixture("APFSearch-cross-copy-" + UUID().uuidString + ".txt", "cross-volume copy metadata")
      guard chmod(crossCopySource.path, 0o640) == 0 else { throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno)) }
      let attributeName = "com.apfsearch.fixture"
      let attributeValue = Array("fixture-xattr".utf8)
      let attributeResult = crossCopySource.path.withCString { path in attributeValue.withUnsafeBytes {
        setxattr(path, attributeName, $0.baseAddress, $0.count, 0, 0)
      } }
      guard attributeResult == 0 else { throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno)) }
      let copyStamp = try OperationIdentity(crossCopySource)
      let crossCopyDestination = secondRoot.appendingPathComponent("search-only-copy-parent", isDirectory: true)
      try FileManager.default.createDirectory(at: crossCopyDestination, withIntermediateDirectories: false)
      guard chmod(crossCopyDestination.path, 0o300) == 0 else { throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno)) }
      crossOperations.testHooks = OperationTestHooks()
      let crossCopy = crossOperations.perform(["action": "copy", "paths": [crossCopySource.path], "destination": crossCopyDestination.path])
      guard chmod(crossCopyDestination.path, 0o700) == 0 else { throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno)) }
      let crossCopiedFile = crossCopyDestination.appendingPathComponent(crossCopySource.lastPathComponent)
      check("actual cross-volume copy uses the regular fallback with a search-only destination parent", crossCopy["success"] as? Bool == true
        && crossOperations.testHooks.clonedFiles == 0 && crossOperations.testHooks.copiedFiles == 1)
      if crossCopy["success"] as? Bool == true {
        let copiedStamp = try OperationIdentity(crossCopiedFile)
        var attribute = [UInt8](repeating: 0, count: 64)
        let attributeCount = crossCopiedFile.path.withCString { path in attribute.withUnsafeMutableBytes {
          getxattr(path, attributeName, $0.baseAddress, $0.count, 0, 0)
        } }
        try check("cross-volume fallback preserves bytes mode timestamps and extended attributes", copiedStamp.device == remoteDevice
          && copiedStamp.mode == copyStamp.mode && copiedStamp.modified == copyStamp.modified && copiedStamp.modifiedNS == copyStamp.modifiedNS
          && Data(contentsOf: crossCopiedFile) == Data(contentsOf: crossCopySource)
          && attributeCount == attributeValue.count && Array(attribute.prefix(max(0, attributeCount))) == attributeValue)
        let crossCopyUndone = FileOperations(directory: root).undo()
        check("cross-volume copy undo uses native Trash on the destination volume", crossCopyUndone["success"] as? Bool == true
          && !FileManager.default.fileExists(atPath: crossCopiedFile.path) && FileManager.default.fileExists(atPath: crossCopySource.path))
        if let bookmark = crossCopyUndone["destination_bookmark"] as? String, let data = Data(base64Encoded: bookmark) {
          var stale = false
          let url = try URL(resolvingBookmarkData: data, options: [.withSecurityScope, .withoutUI, .withoutMounting], relativeTo: nil, bookmarkDataIsStale: &stale)
          let scoped = url.startAccessingSecurityScopedResource()
          defer { if scoped { url.stopAccessingSecurityScopedResource() } }
          try check("native copy-undo Trash item stays on its source volume", OperationIdentity(url).device == remoteDevice
            && String(contentsOf: url, encoding: .utf8) == "cross-volume copy metadata")
          try FileManager.default.removeItem(at: url)
        }
      }
    }

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
      var result: Result<DownloadedInstaller, Error>?
      let value = try signedPackage(path, digest: path == "digest" ? String(repeating: "b", count: 64) : nil)
      let cancel = downloadManager.download(value) { result = $0 }
      if path == "cancel" { cancel() }
      let timeout = Date().addingTimeInterval(5)
      while result == nil && Date() < timeout { RunLoop.current.run(until: Date().addingTimeInterval(0.002)) }
      if path == "valid", case .success(let package)? = result {
        try check("streamed package is exact and readable only after verification", Data(contentsOf: package.url) == packageBytes)
        package.discard()
      } else if path != "valid", case .failure? = result { passed.append("stream transport rejects " + path) }
      else { failures.append("stream transport case " + path) }
    }

    let downloads = root.appendingPathComponent("owned-updates")
    func downloadedPackage() throws -> DownloadedInstaller {
      let directory = downloads.appendingPathComponent("download-" + UUID().uuidString)
      try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
      let url = directory.appendingPathComponent("installer.pkg")
      try packageBytes.write(to: url)
      return DownloadedInstaller(url: url)
    }
    var pendingPackage: DownloadedInstaller? = try downloadedPackage()
    let discardedPath = pendingPackage!.url.path
    pendingPackage = try downloadedPackage()
    check("replacing an unopened download removes its previous directory", !FileManager.default.fileExists(atPath: discardedPath))
    let cancelledPath = pendingPackage!.url.path
    pendingPackage = nil
    check("cancelling a pending package releases its disk storage", !FileManager.default.fileExists(atPath: cancelledPath))
    var openedPackage: DownloadedInstaller? = try downloadedPackage()
    let openedPath = openedPackage!.url.path
    openedPackage!.handOff(); openedPackage = nil
    check("handed off package survives application owner teardown", FileManager.default.fileExists(atPath: openedPath))
    DownloadedInstaller.removeAbandoned(installerIsRunning: true, in: downloads)
    check("startup cleanup preserves files while Installer is running", FileManager.default.fileExists(atPath: openedPath))
    let unrelated = downloads.appendingPathComponent("unrelated.txt")
    try packageBytes.write(to: unrelated)
    DownloadedInstaller.removeAbandoned(installerIsRunning: false, in: downloads)
    check("restart reclaims abandoned packages after Installer exits", !FileManager.default.fileExists(atPath: openedPath) && FileManager.default.fileExists(atPath: unrelated.path))
    let completedPackage = try downloadedPackage()
    completedPackage.handOff(); completedPackage.discard()
    check("Installer completion releases a handed off package", !FileManager.default.fileExists(atPath: completedPackage.url.path))

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
