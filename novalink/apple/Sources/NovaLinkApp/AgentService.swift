import AppKit
import NovaLinkKit
import Observation
import ServiceManagement

/// Registers and unregisters the agent's LaunchAgent, which ships inside the app bundle.
@MainActor @Observable
final class AgentService {
    typealias State = AgentState

    private(set) var state: State = .notRegistered
    @ObservationIgnored private let service = SMAppService.agent(plistName: "com.novalink.agent.plist")

    init() { refresh() }

    func refresh() {
        state = .of(service.status, helperPresent: AgentState.helperPresent(in: Bundle.main.bundleURL))
    }

    /// Starts the agent at login and now. If it is already registered (so the agent may be
    /// sitting stopped after a clean shutdown) it is registered afresh to start it again.
    /// When macOS wants the user's say-so, opens Login Items and returns without throwing;
    /// `state` then reads `requiresApproval`.
    func turnOn() async throws {
        defer { refresh() }
        if service.status == .enabled { try? await service.unregister() }
        do {
            try service.register()
        } catch {
            if service.status == .requiresApproval {
                SMAppService.openSystemSettingsLoginItems()
                return
            }
            throw error
        }
        if service.status == .requiresApproval { SMAppService.openSystemSettingsLoginItems() }
    }

    /// Asks the agent to stop (so it releases any held keys), then removes the LaunchAgent so
    /// it doesn't come back. Returns what went wrong, if anything; the agent is unregistered
    /// either way.
    @discardableResult
    func turnOff(using client: APIClient) async -> String? {
        var problem: String?
        do {
            try await client.shutdown()
        } catch ClientError.notRunning {
            // Already stopped.
        } catch {
            problem = "NovaLink didn't confirm it stopped cleanly: \(describe(error))"
        }
        do {
            try await service.unregister()
        } catch {
            problem = "Couldn't turn NovaLink off: \(describe(error))"
        }
        refresh()
        return problem
    }
}

func describe(_ error: Error) -> String {
    (error as? LocalizedError)?.errorDescription ?? error.localizedDescription
}
