import AppKit
import SwiftUI

final class AppCommands: NSObject {
    static let shared = AppCommands()

    @objc func sync(_ sender: Any?) {
        NotificationCenter.default.post(name: .siskinSync, object: nil)
    }

    @objc func reload(_ sender: Any?) {
        NotificationCenter.default.post(name: .siskinReload, object: nil)
    }

    @objc func focusSearch(_ sender: Any?) {
        NotificationCenter.default.post(name: .siskinFocusSearch, object: nil)
    }

    @objc func compose(_ sender: Any?) {
        NotificationCenter.default.post(name: .siskinCompose, object: nil)
    }
}

func buildMainMenu() -> NSMenu {
    let mainMenu = NSMenu()

    let appItem = NSMenuItem()
    mainMenu.addItem(appItem)
    let appMenu = NSMenu(title: L10n.t("app.name"))
    appMenu.addItem(
        withTitle: L10n.t("menu.about"),
        action: #selector(NSApplication.orderFrontStandardAboutPanel(_:)),
        keyEquivalent: ""
    )
    appMenu.addItem(.separator())
    appMenu.addItem(
        withTitle: L10n.t("menu.quit"),
        action: #selector(NSApplication.terminate(_:)),
        keyEquivalent: "q"
    )
    appItem.submenu = appMenu

    let fileItem = NSMenuItem()
    mainMenu.addItem(fileItem)
    let fileMenu = NSMenu(title: L10n.t("menu.file"))
    let composeItem = NSMenuItem(
        title: L10n.t("action.compose"),
        action: #selector(AppCommands.compose(_:)),
        keyEquivalent: "n"
    )
    composeItem.target = AppCommands.shared
    fileMenu.addItem(composeItem)
    fileItem.submenu = fileMenu

    let editItem = NSMenuItem()
    mainMenu.addItem(editItem)
    let editMenu = NSMenu(title: L10n.t("menu.edit"))
    editMenu.addItem(
        withTitle: L10n.t("menu.copy"),
        action: #selector(NSText.copy(_:)),
        keyEquivalent: "c"
    )
    editMenu.addItem(
        withTitle: L10n.t("menu.selectAll"),
        action: #selector(NSText.selectAll(_:)),
        keyEquivalent: "a"
    )
    editItem.submenu = editMenu

    let viewItem = NSMenuItem()
    mainMenu.addItem(viewItem)
    let viewMenu = NSMenu(title: L10n.t("menu.view"))
    let syncItem = NSMenuItem(
        title: L10n.t("menu.sync"),
        action: #selector(AppCommands.sync(_:)),
        keyEquivalent: "r"
    )
    syncItem.keyEquivalentModifierMask = [.command, .shift]
    syncItem.target = AppCommands.shared
    viewMenu.addItem(syncItem)
    let reloadItem = NSMenuItem(
        title: L10n.t("menu.reload"),
        action: #selector(AppCommands.reload(_:)),
        keyEquivalent: "r"
    )
    reloadItem.target = AppCommands.shared
    viewMenu.addItem(reloadItem)
    let searchItem = NSMenuItem(
        title: L10n.t("menu.search"),
        action: #selector(AppCommands.focusSearch(_:)),
        keyEquivalent: "f"
    )
    searchItem.target = AppCommands.shared
    viewMenu.addItem(searchItem)
    viewItem.submenu = viewMenu

    return mainMenu
}

let environment = ProcessInfo.processInfo.environment
let databasePath: String = {
    if let configured = environment["SISKIN_DB"], !configured.isEmpty {
        return configured
    }
    if let argument = CommandLine.arguments.dropFirst().first {
        return argument
    }
    let base = FileManager.default.urls(
        for: .applicationSupportDirectory,
        in: .userDomainMask
    )[0].appendingPathComponent("Siskin", isDirectory: true)
    try? FileManager.default.createDirectory(at: base, withIntermediateDirectories: true)
    return base.appendingPathComponent("mail.db").path
}()

let application = NSApplication.shared
application.setActivationPolicy(.regular)
application.mainMenu = buildMainMenu()

let window = NSWindow(
    contentRect: NSRect(x: 0, y: 0, width: 1120, height: 760),
    styleMask: [.titled, .closable, .miniaturizable, .resizable],
    backing: .buffered,
    defer: false
)
window.title = L10n.t("app.name")
window.minSize = NSSize(width: 900, height: 720)
window.backgroundColor = .windowBackgroundColor
window.isOpaque = true

let hostingView = NSHostingView(rootView: RootView(dbPath: databasePath))
hostingView.translatesAutoresizingMaskIntoConstraints = true
hostingView.autoresizingMask = [.width, .height]
window.contentView = hostingView
window.setContentSize(NSSize(width: 1120, height: 760))
DispatchQueue.main.async {
    window.setContentSize(NSSize(width: 1120, height: 760))
    if let view = window.contentView {
        view.frame = NSRect(origin: .zero, size: window.contentLayoutRect.size)
    }
}
DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) {
    window.setContentSize(NSSize(width: 1120, height: 760))
    window.center()
}
window.center()
window.makeKeyAndOrderFront(nil)
application.activate(ignoringOtherApps: true)

// Development hook: render the window to PNG without screen-recording access.
if let snapshot = environment["SISKIN_SNAPSHOT"], !snapshot.isEmpty {
    DispatchQueue.main.asyncAfter(deadline: .now() + 3) {
        let target = window.attachedSheet ?? window
        target.contentView?.layoutSubtreeIfNeeded()
        target.displayIfNeeded()
        print("window=\(target.frame) content=\(target.contentLayoutRect) sheet=\(window.attachedSheet != nil)")
        if let view = target.contentView,
           let rep = view.bitmapImageRepForCachingDisplay(in: view.bounds) {
            view.cacheDisplay(in: view.bounds, to: rep)
            if let data = rep.representation(using: .png, properties: [:]) {
                try? data.write(to: URL(fileURLWithPath: snapshot))
                print("snapshot: \(snapshot) \(data.count) bytes")
            }
        }
        exit(0)
    }
}

application.run()
