import Foundation
import ServiceManagement

/// Where the agent's LaunchAgent stands, as the menu shows it.
public enum AgentState: Equatable, Sendable {
    case notRegistered, enabled, requiresApproval, notFound

    /// macOS answers `.notFound` both for an agent that was never registered (every first
    /// launch) and for one whose plist is gone, so the files in the app decide which.
    public static func of(_ status: SMAppService.Status, helperPresent: Bool) -> AgentState {
        switch status {
        case .enabled: .enabled
        case .requiresApproval: .requiresApproval
        default: helperPresent ? .notRegistered : .notFound
        }
    }

    /// Whether `app` carries the LaunchAgent plist and the agent it starts.
    public static func helperPresent(in app: URL) -> Bool {
        let contents = app.appendingPathComponent("Contents")
        return ["Library/LaunchAgents/com.novalink.agent.plist", "Helpers/novalink-agent"].allSatisfy {
            FileManager.default.fileExists(atPath: contents.appendingPathComponent($0).path)
        }
    }
}
