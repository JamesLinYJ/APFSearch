import AppKit

/// Checks are opt-in, at most daily when the app becomes active. Downloads and
/// installation always require a user decision; no silent executable replacement.
final class UpdateController: NSObject, NSMenuItemValidation {
  static let shared = UpdateController()
  private var cancellation: (() -> Void)?
  private var progress: NSWindow?
  private var automaticItem: NSMenuItem?
  private var pendingInstaller: URL?
  private var busy = false

  func install(in menu: NSMenu) {
    let check = NSMenuItem(title: L("update.check"), action: #selector(checkNow), keyEquivalent: "")
    check.target = self; menu.addItem(check)
    let automatic = NSMenuItem(title: L("update.automatic"), action: #selector(toggleAutomatic), keyEquivalent: "")
    automatic.target = self; automatic.state = UserDefaults.standard.bool(forKey: "FileSearch.AutomaticUpdates") ? .on : .off
    automaticItem = automatic; menu.addItem(automatic)
  }
  func applicationBecameActive() {
    guard UpdateManager.shared.configured, UserDefaults.standard.bool(forKey: "FileSearch.AutomaticUpdates"), !busy else { return }
    let previous = UserDefaults.standard.double(forKey: "FileSearch.LastUpdateCheck")
    guard Date().timeIntervalSince1970 - previous >= 86_400 else { return }
    check(interactive: false)
  }
  @objc private func toggleAutomatic() {
    let enabled = !UserDefaults.standard.bool(forKey: "FileSearch.AutomaticUpdates")
    UserDefaults.standard.set(enabled, forKey: "FileSearch.AutomaticUpdates")
    automaticItem?.state = enabled ? .on : .off
    if enabled { applicationBecameActive() }
  }
  @objc private func checkNow() { check(interactive: true) }
  func validateMenuItem(_ item: NSMenuItem) -> Bool { item.action != #selector(checkNow) || !busy }
  private func check(interactive: Bool) {
    guard !busy else { return }; busy = true
    if UpdateManager.shared.configured { UserDefaults.standard.set(Date().timeIntervalSince1970, forKey: "FileSearch.LastUpdateCheck") }
    cancellation = UpdateManager.shared.check { [weak self] result in
      guard let self else { return }; self.busy = false; self.cancellation = nil
      switch result {
      case .failure(let error): if interactive { self.message(error.localizedDescription) }
      case .success(nil): if interactive { self.message(L("update.up_to_date")) }
      case .success(let manifest?): self.offer(manifest)
      }
    }
  }
  private func offer(_ manifest: UpdateManifest) {
    let alert = NSAlert(); alert.messageText = L("update.available", manifest.version)
    alert.informativeText = L("update.download_notice", ByteCountFormatter.string(fromByteCount: manifest.size, countStyle: .file))
    alert.addButton(withTitle: L("update.download")); alert.addButton(withTitle: L("action.cancel"))
    guard alert.runModal() == .alertFirstButtonReturn else { return }
    busy = true
    let panel = NSPanel(contentRect: NSRect(x: 0, y: 0, width: 420, height: 145), styleMask: [.titled], backing: .buffered, defer: false)
    panel.title = L("update.downloading")
    let label = NSTextField(wrappingLabelWithString: L("update.verification_notice"))
    let indicator = NSProgressIndicator(); indicator.style = .bar; indicator.isIndeterminate = true; indicator.startAnimation(nil)
    let cancel = NSButton(title: L("action.cancel"), target: self, action: #selector(cancelDownload))
    let stack = NSStackView(views: [label, indicator, cancel]); stack.orientation = .vertical; stack.spacing = 12
    stack.frame = NSRect(x: 20, y: 16, width: 380, height: 110); panel.contentView?.addSubview(stack)
    panel.center(); panel.makeKeyAndOrderFront(nil); progress = panel
    cancellation = UpdateManager.shared.download(manifest) { [weak self] result in
      guard let self else { return }; self.busy = false; self.cancellation = nil; self.progress?.close(); self.progress = nil
      switch result {
      case .failure(let error):
        if (error as NSError).code != NSURLErrorCancelled { self.message(error.localizedDescription) }
      case .success(let package):
        self.pendingInstaller = package
        let alert = NSAlert(); alert.messageText = L("update.ready"); alert.informativeText = L("update.install_notice")
        alert.addButton(withTitle: L("update.open_installer")); alert.addButton(withTitle: L("action.cancel"))
        if alert.runModal() == .alertFirstButtonReturn {
          if !NSWorkspace.shared.open(package) { self.message(L("update.transport_error")); self.removeInstaller() }
        } else { self.removeInstaller() }
      }
    }
  }
  @objc private func cancelDownload() { cancellation?() }
  private func message(_ text: String) {
    let alert = NSAlert(); alert.messageText = L("update.title"); alert.informativeText = text
    alert.addButton(withTitle: L("action.ok")); alert.runModal()
  }
  private func removeInstaller() {
    guard let package = pendingInstaller else { return }
    try? FileManager.default.removeItem(at: package.deletingLastPathComponent()); pendingInstaller = nil
  }
}
