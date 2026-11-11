import AppKit
import SwiftUI

/// A menu-bar status item with a transient popover: the shared plumbing behind the NovaGuard, NovaLink and NovaHelm menus.
/// The app sets the item's image, title, tooltip and accessibility label through `item.button`.
@MainActor
public final class MenuBarPopover: NSObject, NSPopoverDelegate {
    public let item: NSStatusItem
    public var onShow: () -> Void = {}
    public var onClose: () -> Void = {}

    private let popover = NSPopover()
    private var closedAt = Date.distantPast

    public var isShown: Bool { popover.isShown }

    /// - Parameter followsContentSize: whether the popover grows and shrinks with the SwiftUI content.
    public init<Content: View>(autosaveName: String, followsContentSize: Bool, content: Content) {
        item = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        super.init()
        item.autosaveName = autosaveName
        item.isVisible = true
        item.behavior = []   // can't be dragged out of the menu bar by accident
        if let button = item.button {
            button.target = self
            button.action = #selector(buttonClicked)
            button.sendAction(on: [.leftMouseUp, .rightMouseUp])
        }
        popover.behavior = .transient
        popover.animates = false
        popover.delegate = self
        let hosting = NSHostingController(rootView: content)
        if followsContentSize { hosting.sizingOptions = [.preferredContentSize] }
        popover.contentViewController = hosting
    }

    public func toggle() {
        if popover.isShown {
            close()
        } else if Self.shouldReopen(closedAt: closedAt, now: Date()) {
            show()
        }
    }

    public func show() {
        guard let button = item.button, !popover.isShown else { return }
        NSApp.activate()
        popover.show(relativeTo: button.bounds, of: button, preferredEdge: .minY)
        popover.contentViewController?.view.window?.makeKey()
    }

    public func close() {
        popover.performClose(nil)
    }

    /// A click on the icon that dismissed a transient popover arrives just after the close; it must not reopen it.
    nonisolated static func shouldReopen(closedAt: Date, now: Date) -> Bool {
        now.timeIntervalSince(closedAt) > 0.2
    }

    @objc private func buttonClicked() { toggle() }

    // A transient popover also closes on a click elsewhere, which never goes through toggle.
    public func popoverDidShow(_ notification: Notification) { onShow() }

    public func popoverDidClose(_ notification: Notification) {
        closedAt = Date()
        onClose()
    }
}
