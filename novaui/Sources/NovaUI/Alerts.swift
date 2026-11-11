import AppKit

/// The apps' one alert style: a title, a body and buttons whose roles set the keys. The answer is the chosen
/// button's index, or nil when the alert was taken down without one.
@MainActor
public struct NovaAlert {
    public struct Button {
        /// `.primary` answers Return, `.cancel` answers Escape, `.plain` needs a click.
        public enum Role { case primary, cancel, plain }

        public var title: String
        public var role: Role
        public var isEnabled: Bool

        public init(_ title: String, role: Role = .plain, isEnabled: Bool = true) {
            self.title = title
            self.role = role
            self.isEnabled = isEnabled
        }
    }

    public var title: String
    public var message: String
    public var buttons: [Button]

    public init(title: String, message: String = "", buttons: [Button]) {
        self.title = title
        self.message = message
        self.buttons = buttons
    }

    public func makeAlert() -> NSAlert {
        let alert = NSAlert()
        alert.messageText = title
        alert.informativeText = message
        for button in buttons {
            let control = alert.addButton(withTitle: button.title)
            control.isEnabled = button.isEnabled
            switch button.role {
            case .primary: control.keyEquivalent = "\r"
            case .cancel: control.keyEquivalent = "\u{1b}"
            case .plain: control.keyEquivalent = ""
            }
        }
        return alert
    }

    /// Brings the app forward and waits for an answer; `NSApp.abortModal()` takes it down with nil.
    public func runModal() -> Int? {
        let alert = makeAlert()
        NSApp.activate()
        return Self.index(of: alert.runModal(), buttonCount: buttons.count)
    }

    public func beginSheet(for window: NSWindow, completion: @escaping (Int?) -> Void) {
        let count = buttons.count
        makeAlert().beginSheetModal(for: window) { response in completion(Self.index(of: response, buttonCount: count)) }
    }

    nonisolated static func index(of response: NSApplication.ModalResponse, buttonCount: Int) -> Int? {
        let index = response.rawValue - NSApplication.ModalResponse.alertFirstButtonReturn.rawValue
        return (0..<buttonCount).contains(index) ? index : nil
    }
}
