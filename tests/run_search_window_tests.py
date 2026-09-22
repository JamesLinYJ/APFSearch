#!/usr/bin/env python3
"""Run production AppKit controller methods against controlled XPC replies.

The temporary build removes the application entry point, automatic startup
requests, and the sidebar preference write. Table instrumentation records public
AppKit reload calls.
It neither modifies the installed application nor talks to its indexing agent.
"""
from test_bundle import MINIMUM_MACOS_VERSION, swift_target
import argparse
import datetime
import hashlib
import json
import pathlib
import plistlib
import shutil
import subprocess
import tempfile
from test_bundle import create_test_bundle

parser = argparse.ArgumentParser()
parser.add_argument("--work", type=pathlib.Path)
parser.add_argument("--report", type=pathlib.Path)
parser.add_argument("--bundle", type=pathlib.Path, help="Build a standalone test app for launch from the GUI; requires --report, does not launch it")
args = parser.parse_args()
project = pathlib.Path(__file__).resolve().parents[1]
work = args.work or pathlib.Path(tempfile.mkdtemp(prefix="APFSearch-ui-tests-"))
work.mkdir(parents=True, exist_ok=True)
source = (project / "macos/Application.swift").read_text()
source_hash = hashlib.sha256(source.encode()).hexdigest()
source = source.replace("@main\nfinal class AppDelegate", "final class AppDelegate", 1)
startup = """        pollStatus()
        refreshShortcuts()
        runQuery()
"""
if source.count(startup) != 1:
    raise SystemExit("App initializer changed; update the test-only startup isolation boundary")
source = source.replace(startup, "", 1)
sidebar_preference = '        UserDefaults.standard.set(!sidebar.isCollapsed, forKey: ApplicationIdentity.preferencePrefix + "SidebarVisible")'
if source.count(sidebar_preference) != 1:
    raise SystemExit("Sidebar preference write changed; update the test-only persistence boundary")
source = source.replace(sidebar_preference, "        // User preference persistence is isolated in the test build.", 1)
table_class = "final class SearchResultsTable: NSTableView {"
instrumentation = """
    var testReloadedRows = [IndexSet]()
    var testFullReloads = 0
    override func reloadData() {
        testFullReloads += 1
        super.reloadData()
    }
    override func reloadData(forRowIndexes rows: IndexSet, columnIndexes columns: IndexSet) {
        testReloadedRows.append(rows)
        super.reloadData(forRowIndexes: rows, columnIndexes: columns)
    }
"""
if source.count(table_class) != 1:
    raise SystemExit("Table class changed; update test instrumentation")
source = source.replace(table_class, table_class + instrumentation, 1)
path_control = "    let pathControl = NSPathControl()"
if source.count(path_control) != 1:
    raise SystemExit("Path control changed; update the selection I/O instrumentation")
source = source.replace(path_control, "    let pathControl = ObservedPathControl()", 1)
(work / "ApplicationUnderTest.swift").write_text(source)
command = ["swiftc", "-module-cache-path", str(work / "ModuleCache"), "-swift-version", "5", "-O", "-target", swift_target()]
command += [str(project / "macos" / name) for name in ["ApplicationIdentity.swift", "SearchProtocol.swift", "Localization.swift", "SettingsWindow.swift", "SelectionResolver.swift", "FileOperationReview.swift", "DuplicateResultsWindow.swift", "UpdateManager.swift", "UpdateUI.swift", "ResultIconLoader.swift"]]
command += [str(work / "ApplicationUnderTest.swift"), str(project / "tests/SearchWindowTests.swift"), str(project / "tests/SearchFeatureWindowTests.swift")]
for framework in ["AppKit", "SwiftUI", "ServiceManagement", "Quartz", "Carbon", "CryptoKit"]:
    command += ["-framework", framework]
executable = work / "SearchWindowTests"
command += ["-o", str(executable)]
subprocess.run(command, check=True)
test_app = args.bundle.resolve() if args.bundle else work / 'SearchWindowTests.app'
bundled_executable = create_test_bundle(executable, test_app)
if args.bundle:
    if not args.report:
        raise SystemExit("--bundle requires --report")
    app = test_app
    args.report.resolve().parent.mkdir(parents=True, exist_ok=True)
    (app / "Contents/Resources/report-path.txt").write_text(str(args.report.resolve()) + "\n")
    (app / "Contents/Resources/source-sha256.txt").write_text(source_hash + "\n")
    subprocess.run(["codesign", "--force", "--sign", "-", str(app)], check=True)
    print(json.dumps({"test_app": str(app), "report": str(args.report.resolve()), "app_source_sha256": source_hash, "launched": False}, indent=2))
    raise SystemExit(0)
result = subprocess.run([str(bundled_executable), '-AppleLanguages', '(zh-Hans)'], text=True, capture_output=True)
(work / "stderr.log").write_text(result.stderr)
(work / "stdout.log").write_text(result.stdout)
try:
    report = json.loads(result.stdout)
except json.JSONDecodeError:
    raise SystemExit(f"AppKit test did not produce JSON (exit {result.returncode}); inspect {work}")
report["app_source_sha256"] = source_hash
report["executed_at_utc"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
if args.report:
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
print(json.dumps(report, ensure_ascii=False, indent=2))
raise SystemExit(result.returncode)
