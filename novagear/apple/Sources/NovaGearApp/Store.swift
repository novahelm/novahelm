import AppKit
import ApplicationServices
import NovaGearKit
import NovaUI
import Observation

/// The popover's state. Follows the agent's event stream on a background thread and refetches
/// a snapshot when something changes, so nothing polls while the desk is quiet.
@MainActor @Observable
final class Store {
    var snapshot: Snapshot?
    var problem: String?
    var lastUpdated: Date?
    var loginItemEnabled = LoginItem.isEnabled
    /// Whether macOS lets NovaGear (and the agent it starts) press keys and type for deck actions.
    var keysAllowed = AXIsProcessTrusted()
    var onChange: (() -> Void)?

    @ObservationIgnored private var agent: Process?
    @ObservationIgnored private var refreshTask: Task<Void, Never>?
    @ObservationIgnored private var sliderTask: Task<Void, Never>?

    func start() {
        ensureAgent()
        let thread = Thread { [weak self] in
            while true {
                do {
                    Task { @MainActor in self?.refresh() }
                    try AgentClient.followEvents { event in
                        // Live key and dial presses and log lines don't change what the menu shows.
                        if !event.contains(#""type":"input""#) && !event.contains(#""type":"log""#) {
                            Task { @MainActor in self?.refresh() }
                        }
                    }
                } catch {
                    let message = MenuText.describe(error)
                    Task { @MainActor in
                        self?.problem = message
                        self?.ensureAgent()
                    }
                }
                Thread.sleep(forTimeInterval: 2)
            }
        }
        thread.name = "novagear-events"
        thread.start()
    }

    /// Fetches a fresh snapshot after a short pause, so a burst (a dial turning) costs one fetch.
    func refresh() {
        refreshTask?.cancel()
        refreshTask = Task {
            try? await Task.sleep(for: .milliseconds(150))
            guard !Task.isCancelled else { return }
            let result = await Task.detached { Result { try Snapshot.fetch() } }.value
            keysAllowed = AXIsProcessTrusted()
            switch result {
            case .success(let s):
                snapshot = s
                problem = nil
                lastUpdated = Date()
            case .failure(let e):
                problem = MenuText.describe(e)
            }
            onChange?()
        }
    }

    // MARK: Actions

    func run(_ action: [String: Any]) {
        nonisolated(unsafe) let action = action
        Task.detached { [weak self] in
            do { try AgentClient.action(action) } catch {
                let message = MenuText.describe(error)
                await MainActor.run { self?.problem = message }
            }
        }
    }

    func goToMode(_ id: String) { run(["do": "mode_go", "mode": id]) }

    func denyApproval(_ id: String) {
        Task.detached { [weak self] in
            do { try AgentClient.denyApproval(id) } catch {
                let message = MenuText.describe(error)
                await MainActor.run { self?.problem = message }
            }
        }
    }

    func toggleLights() { run(["do": "light_toggle"]) }

    /// Sliders send many values while dragging; only the last one in each 80 ms goes out.
    func setLights(brightness: Int? = nil, kelvin: Int? = nil) {
        sliderTask?.cancel()
        sliderTask = Task {
            try? await Task.sleep(for: .milliseconds(80))
            guard !Task.isCancelled else { return }
            var a: [String: Any] = ["do": "light_set"]
            if let brightness { a["brightness"] = brightness; a["on"] = true }
            if let kelvin { a["kelvin"] = kelvin }
            run(a)
        }
    }

    func prompterPlayPause() { run(["do": "prompter_play_pause"]) }
    func prompterRestart() { run(["do": "prompter_restart"]) }
    func nextDpiStage() { run(["do": "mouse_dpi_step", "step": 1]) }

    func toggleDeckControl() {
        Task.detached { [weak self] in
            do {
                var cfg = try JSONSerialization.jsonObject(with: AgentClient.call("GET", "/api/config")) as? [String: Any] ?? [:]
                cfg["deck_control"] = !((cfg["deck_control"] as? Bool) ?? false)
                _ = try AgentClient.call("PUT", "/api/config", body: JSONSerialization.data(withJSONObject: cfg))
            } catch {
                let message = MenuText.describe(error)
                await MainActor.run { self?.problem = message }
            }
        }
    }

    /// True when the agent is the one this app started, so the Local Network prompt belongs to
    /// this app and its switch is the one to flip.
    var launchedAgent: Bool { agent?.isRunning ?? false }

    func allowLocalNetwork() {
        if let url = URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_LocalNetwork") {
            NSWorkspace.shared.open(url)
        }
    }

    func open(_ page: String = "", section: String? = nil) {
        Task.detached { [weak self] in
            do {
                let url = try AgentClient.panelURL(page, section: section)
                await MainActor.run { _ = NSWorkspace.shared.open(url) }
            } catch {
                let message = MenuText.describe(error)
                await MainActor.run { self?.problem = message }
            }
        }
    }

    func setStartAtLogin(_ on: Bool) {
        do {
            try LoginItem.setEnabled(on)
            if LoginItem.needsApproval {
                problem = "Allow NovaGear under System Settings › General › Login Items."
            }
        } catch {
            problem = "Open at login needs NovaGear.app in Applications: \(error.localizedDescription)"
        }
        loginItemEnabled = LoginItem.isEnabled
    }

    func quit() {
        agent?.terminate()
        NSApp.terminate(nil)
    }

    /// Asks macOS to add NovaGear to the Accessibility list (it only appears once asked) and
    /// opens that list so the switch is one click away.
    func allowKeys() {
        // kAXTrustedCheckOptionPrompt's value; the imported global isn't concurrency-safe in Swift 6.
        keysAllowed = AXIsProcessTrustedWithOptions(["AXTrustedCheckOptionPrompt": true] as CFDictionary)
        if !keysAllowed,
           let url = URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility") {
            NSWorkspace.shared.open(url)
        }
    }

    // MARK: Agent

    /// Starts the bundled agent (Contents/Helpers, or beside the app binary in a dev build)
    /// when nothing answers on the port. An agent started elsewhere is left alone.
    func ensureAgent() {
        if let agent, agent.isRunning { return }
        if agentAnswers() { return }
        let exe = Bundle.main.executableURL?.deletingLastPathComponent()
        let candidates = [
            Bundle.main.bundleURL.appending(path: "Contents/Helpers/novagear-agent"),
            exe?.appending(path: "novagear-agent"),
        ].compactMap { $0 }
        guard let path = candidates.first(where: { FileManager.default.isExecutableFile(atPath: $0.path) }) else {
            problem = "The NovaGear agent isn't running, and this copy of the app doesn't include it."
            return
        }
        let p = Process()
        p.executableURL = path
        do {
            try p.run()
            agent = p
        } catch {
            problem = "Couldn't start the agent: \(error.localizedDescription)"
        }
    }

    private func agentAnswers() -> Bool {
        let fd = socket(AF_INET, SOCK_STREAM, 0)
        guard fd >= 0 else { return false }
        defer { close(fd) }
        var addr = sockaddr_in()
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_port = apiPort.bigEndian
        addr.sin_addr.s_addr = inet_addr("127.0.0.1")
        return withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { connect(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size)) == 0 }
        }
    }
}
