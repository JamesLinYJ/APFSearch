import AppKit

extension SearchWindowTests {
    static func featureIntegrationRegression() async {
        let c = await controller((0..<20).map { row($0) }, count: 2202)
        c.offlineListID = nil
        c.roots = ["/UIRegression"]
        c.currentLease = SearchSnapshotLease(token: "selection-fixture", listID: nil)
        c.table.selectRowIndexes(IndexSet([1, 1500]), byExtendingSelection: false)
        SearchClient.shared.clear()
        let rename = NSMenuItem(title: "", action: #selector(SearchWindowController.renameSelection), keyEquivalent: "")
        check("cross_page_multi_selection_enables_batch_rename", c.validateMenuItem(rename) && !c.selectionIsComplete)
        var resolved = [[String: Any]]()
        c.withResolvedRows { resolved = $0 }
        let request = SearchClient.shared.takeQuery()
        check("cross_page_action_keeps_lease_and_sort", request?.request["snapshot_lease"] as? String == "selection-fixture" && request?.request["offset"] as? Int == 1000)
        request?.completion(reply((1000..<2000).map { row($0) }, count: 2202, offset: 1000))
        await pump()
        check("cross_page_action_resolves_every_selected_row", resolved.compactMap { $0["path"] as? String } == [row(1)["path"] as! String, row(1500)["path"] as! String])
        c.operationReviewPending = true
        check("pending_operation_review_disables_duplicate_submission", !c.validateMenuItem(rename))
        c.operationReviewPending = false
        c.cancelQueries(); SearchClient.shared.clear()
        c.runQuery()
        let stale = SearchClient.shared.takeQuery()
        check("initial_window_lease_uses_window_capacity", stale?.request["snapshot_owner"] as? String == "window" && stale?.request["retain_snapshot"] as? Bool == true)
        c.cancelQueries(); c.querySequence += 1
        var answer = reply([row(0)]); answer["snapshot_lease"] = "stale-fixture"
        stale?.completion(answer)
        check("discarded_initial_page_releases_its_incoming_lease", SearchClient.shared.take("release_snapshot")?.request["snapshot_lease"] as? String == "stale-fixture")
        c.runQuery(); let deferred = SearchClient.shared.takeQuery()
        c.resultAnimationInFlight = true
        answer["snapshot_lease"] = "animation-fixture"
        deferred?.completion(answer)
        c.cancelQueries(); c.resultAnimationInFlight = false
        check("cleared_animation_reply_releases_its_incoming_lease", SearchClient.shared.take("release_snapshot")?.request["snapshot_lease"] as? String == "animation-fixture")
        SearchClient.shared.clear()
        c.currentStatus = ["success": true, "status_revision": 7, "roots": ["/UIRegression"], "generation": 1]
        c.pollStatus(); let status = SearchClient.shared.take("wait_status")
        check("status_observation_uses_bounded_wait_not_periodic_status_poll", (status?.request["after"] as? NSNumber)?.uint64Value == 7 && status?.request["timeout_ms"] as? Int == 30_000)
        c.stopStatusObservation()
        status?.completion(["success": true, "status_revision": 8, "roots": ["/UIRegression"]])
        await pump()
        check("closed_status_observer_drops_late_reply_without_rearming", c.currentStatus["status_revision"] as? Int == 7 && SearchClient.shared.take("wait_status") == nil && !c.statusRequestPending)
        c.cancelQueries(); c.pendingQuery?.cancel(); c.pendingQuery = nil; c.historyTimer?.invalidate()
        SearchClient.shared.clear()

        let groups = (0..<105).map { group -> [String: Any] in
            ["kind": "same_content", "rows": [row(group * 2), row(group * 2 + 1)], "distinct_files": 2]
        }
        var cleanup = [String: Any]()
        let duplicates = DuplicateResultsWindowController(result: ["groups": groups, "hardlinks": [], "partial": false]) { cleanup = $0 }
        duplicates.showWindow(nil)
        _ = duplicates.perform(NSSelectorFromString("selectExtras"))
        _ = duplicates.perform(NSSelectorFromString("nextPage"))
        _ = duplicates.perform(NSSelectorFromString("previousPage"))
        _ = duplicates.perform(NSSelectorFromString("manageSelected"))
        let paths = cleanup["paths"] as? [String] ?? []
        let keepers = cleanup["keepers"] as? [[String: Any]] ?? []
        check("duplicate_selection_survives_page_navigation", paths.count == 105 && Set(paths).count == 105)
        check("duplicate_cleanup_keeps_one_verified_survivor_per_group", keepers.count == 105 && Set(keepers.compactMap { $0["path"] as? String }).isDisjoint(with: paths))
        check("duplicate_cleanup_carries_original_hash_identities", (cleanup["expected"] as? [[String: Any]])?.count == 105)
        duplicates.close()
        c.window?.orderOut(nil)
        SearchClient.shared.clear()
    }
}
