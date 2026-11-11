import ServiceManagement

/// Whether the app opens at login, through `SMAppService.mainApp`.
@MainActor
public enum LoginItem {
    public nonisolated enum State: Equatable {
        case enabled, off, needsApproval

        public static func of(_ status: SMAppService.Status) -> State {
            switch status {
            case .enabled: .enabled
            case .requiresApproval: .needsApproval
            default: .off
            }
        }
    }

    public static var isEnabled: Bool { state == .enabled }

    /// True when the user must approve the app under System Settings → General → Login Items.
    public static var needsApproval: Bool { state == .needsApproval }

    private static var state: State { .of(SMAppService.mainApp.status) }

    /// Registers only when not already enabled, and unregisters only when registered.
    public static func setEnabled(_ on: Bool) throws {
        let status = SMAppService.mainApp.status
        if on {
            if status != .enabled { try SMAppService.mainApp.register() }
        } else {
            if status != .notRegistered { try SMAppService.mainApp.unregister() }
        }
    }
}
