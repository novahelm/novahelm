import AppKit
import SwiftUI

/// How a window looks when it first opens. Sizes left nil keep AppKit's (or the content's) own.
public struct WindowSpec {
    public var title: String
    public var styleMask: NSWindow.StyleMask
    public var contentSize: NSSize?
    public var contentMinSize: NSSize?
    public var minSize: NSSize?
    /// Saves the frame under this name and restores it on the next open; without one (or nothing saved) it centres.
    public var autosaveName: String?

    public init(title: String, styleMask: NSWindow.StyleMask, contentSize: NSSize? = nil,
                contentMinSize: NSSize? = nil, minSize: NSSize? = nil, autosaveName: String? = nil) {
        self.title = title
        self.styleMask = styleMask
        self.contentSize = contentSize
        self.contentMinSize = contentMinSize
        self.minSize = minSize
        self.autosaveName = autosaveName
    }

    @MainActor
    func apply(to window: NSWindow) {
        window.title = title
        window.styleMask = styleMask
        if let contentMinSize { window.contentMinSize = contentMinSize }
        if let minSize { window.minSize = minSize }
        if let contentSize { window.setContentSize(contentSize) }
        window.isReleasedWhenClosed = false
        var restored = false
        if let autosaveName {
            window.setFrameAutosaveName(autosaveName)
            restored = window.setFrameUsingName(autosaveName)
        }
        if !restored { window.center() }
    }
}

/// One window an app opens on demand: `show` brings it forward if it is open, otherwise builds it.
@MainActor
public final class WindowSlot {
    public private(set) var window: NSWindow?
    /// Runs as the window closes, after the slot has let go of it (unless it keeps closed windows).
    public var onClose: (() -> Void)?
    private let keepsClosedWindow: Bool
    private let present: (NSWindow) -> Void
    private var closeObserver: NSObjectProtocol?

    /// With `keepsClosedWindow`, a closed window is reopened as it was instead of being rebuilt.
    public convenience init(keepsClosedWindow: Bool = false) {
        self.init(keepsClosedWindow: keepsClosedWindow) { window in
            NSApp.activate()
            window.makeKeyAndOrderFront(nil)
        }
    }

    init(keepsClosedWindow: Bool, present: @escaping (NSWindow) -> Void) {
        self.keepsClosedWindow = keepsClosedWindow
        self.present = present
    }

    /// A SwiftUI window, sized by its view unless `spec` gives a content size.
    @discardableResult
    public func show<Content: View>(_ spec: WindowSpec, delegate: NSWindowDelegate? = nil,
                                    content: () -> Content) -> NSWindow {
        show(spec, delegate: delegate) { NSWindow(contentViewController: NSHostingController(rootView: content())) }
    }

    /// A window around an AppKit view.
    @discardableResult
    public func show(_ spec: WindowSpec, delegate: NSWindowDelegate? = nil, contentView: () -> NSView) -> NSWindow {
        show(spec, delegate: delegate) {
            let window = NSWindow(contentRect: NSRect(origin: .zero, size: spec.contentSize ?? .zero),
                                  styleMask: spec.styleMask, backing: .buffered, defer: false)
            window.contentView = contentView()
            return window
        }
    }

    public func close() { window?.close() }

    private func show(_ spec: WindowSpec, delegate: NSWindowDelegate?, make: () -> NSWindow) -> NSWindow {
        if let window {
            present(window)
            return window
        }
        let window = make()
        spec.apply(to: window)
        window.delegate = delegate
        closeObserver = NotificationCenter.default.addObserver(
            forName: NSWindow.willCloseNotification, object: window, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated { self?.didClose() }
        }
        self.window = window
        present(window)
        return window
    }

    private func didClose() {
        if !keepsClosedWindow {
            if let closeObserver { NotificationCenter.default.removeObserver(closeObserver) }
            closeObserver = nil
            window = nil
        }
        onClose?()
    }
}
