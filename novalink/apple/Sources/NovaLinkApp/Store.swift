import AppKit
import NovaLinkKit
import NovaUI
import Observation
import ServiceManagement

struct Banner: Equatable {
    var text: String
    var isError: Bool
}

/// What the popover shows and polls. Everything the user can do goes through here so a
/// failure always ends up in `banner`.
@MainActor @Observable
final class Store {
    private(set) var poll = PollState()
    var status: AgentStatus? { poll.status }
    var pending: [PendingPair] { poll.pending }
    var discovered: [DiscoveredDevice] { poll.discovered }
    var checks: [Check] { poll.checks }
    var connectionError: ClientError? { poll.connectionError }
    private(set) var pairing: Set<String> = []
    private(set) var loginItemEnabled = false
    /// When the agent last answered a status request successfully.
    private(set) var lastUpdated: Date?
    var banner: Banner?

    let agent: AgentService
    @ObservationIgnored let client: APIClient
    @ObservationIgnored private var tracker = PairPromptTracker()
    @ObservationIgnored private var refreshing = false
    @ObservationIgnored private var loop: Task<Void, Never>?
    @ObservationIgnored var onPrompt: ([PendingPair]) -> Void = { _ in }
    @ObservationIgnored var onWithdraw: ([String]) -> Void = { _ in }
    @ObservationIgnored var onChange: () -> Void = {}

    var popoverOpen = false {
        didSet { if popoverOpen && !oldValue { Task { await refresh() } } }
    }

    init(agent: AgentService) {
        self.agent = agent
        client = APIClient(token: { try TokenStore.read() })
        loginItemEnabled = LoginItem.isEnabled
    }

    var icon: IconState { IconState.of(status, turnedOff: agent.state == .notRegistered) }

    // MARK: Polling

    func start() {
        guard loop == nil else { return }
        loop = Task { [weak self] in
            while !Task.isCancelled {
                guard let self else { return }
                await self.refresh()
                let seconds: UInt64 = self.popoverOpen ? 1 : 3
                try? await Task.sleep(nanoseconds: seconds * 1_000_000_000)
            }
        }
    }

    func refresh() async {
        guard !refreshing else { return }
        refreshing = true
        defer { refreshing = false; onChange() }
        agent.refresh()
        loginItemEnabled = LoginItem.isEnabled
        // Each section is fetched on its own: one failing endpoint doesn't blank the rest.
        let s = await fetch { try await client.status() }
        if case .failure(let e) = s, e.meansAgentGone {
            poll.agentGone(e)
        } else {
            let p = await fetch { try await client.pending() }
            let d = await fetch { try await client.discovered() }
            let c = await fetch { try await client.doctor() }
            poll.apply(status: s, pending: p, discovered: d, checks: c)
            if case .success = s { lastUpdated = Date() }
            if status != nil && banner?.text == MenuText.agentNotAnswering { banner = nil }
        }
        let r = tracker.update(pending)
        if !r.withdraw.isEmpty { onWithdraw(r.withdraw) }
        if !r.prompt.isEmpty { onPrompt(r.prompt) }
    }

    private func fetch<T>(_ body: () async throws -> T) async -> Result<T, ClientError> {
        do { return .success(try await body()) } catch {
            return .failure(error as? ClientError ?? .other(describe(error)))
        }
    }

    // MARK: Actions

    private func run(_ what: String, _ body: () async throws -> Void) async {
        do { try await body() } catch {
            banner = Banner(text: "Couldn't \(what): \(describe(error))", isError: true)
        }
        await refresh()
    }

    func answer(_ p: PendingPair, accept: Bool) async {
        await run(accept ? "pair with \(p.name)" : "turn down \(p.name)") { try await client.answer(p.peer, accept: accept) }
    }

    func pair(_ d: DiscoveredDevice, name: String) async {
        pairing.insert(d.id)
        await run("pair with \(name)") { try await client.pair(d.id) }
        pairing.remove(d.id)
    }

    func unpair(_ p: Peer, name: String) async {
        await run("unpair \(name)") { try await client.unpair(p.id) }
    }

    func setPosition(_ edge: Edge, of p: Peer, name: String) async {
        await run("move \(name)") { try await client.layout(edge: edge, peer: p.id) }
    }

    func setSwapCtrlCmd(_ on: Bool) async {
        await run("change the Ctrl and Cmd setting") { try await client.setSwapCtrlCmd(on) }
    }

    func requestPermissions() async {
        await run("ask for permissions") { try await client.requestPermissions() }
    }

    func turnOn() async {
        do { try await agent.turnOn() } catch {
            banner = Banner(text: "Couldn't turn NovaLink on: \(describe(error))", isError: true)
            await refresh()
            return
        }
        if agent.state == .requiresApproval {
            banner = Banner(text: MenuText.agentNeedsApproval, isError: true)
            await refresh()
            return
        }
        // Give the agent time to come up (a first start can take several seconds) before
        // calling it a failure; `refresh` clears the warning if it answers later.
        for _ in 0..<30 {
            await refresh()
            if status != nil { return }
            try? await Task.sleep(nanoseconds: 500_000_000)
        }
        banner = Banner(text: MenuText.agentNotAnswering, isError: true)
    }

    func turnOff() async {
        if let problem = await agent.turnOff(using: client) {
            banner = Banner(text: problem, isError: true)
        }
        await refresh()
    }

    func setStartAtLogin(_ on: Bool) {
        do { try LoginItem.setEnabled(on) } catch {
            if !(on && LoginItem.needsApproval) {
                banner = Banner(text: "Couldn't change Start at Login: \(describe(error))", isError: true)
            }
        }
        // macOS may hold the login item until the user allows it.
        if on && LoginItem.needsApproval {
            SMAppService.openSystemSettingsLoginItems()
            banner = Banner(text: MenuText.loginItemNeedsApproval, isError: true)
        }
        loginItemEnabled = LoginItem.isEnabled
    }

    func openLogs() {
        let dir = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/Logs/NovaLink")
        do {
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        } catch {
            banner = Banner(text: "Couldn't open the logs folder: \(describe(error))", isError: true)
            return
        }
        if !NSWorkspace.shared.open(dir) {
            banner = Banner(text: "Couldn't open the logs folder at \(dir.path).", isError: true)
        }
    }

    /// Links `novalink` into /usr/local/bin after an administrator prompt.
    func installCommandLineTool() async {
        let helper = Bundle.main.bundleURL.appendingPathComponent("Contents/Helpers/novalink")
        guard InstallLocation.isStable(Bundle.main.bundleURL.path) else {
            banner = Banner(text: "Move NovaLink to your Applications folder first, then try again.", isError: true)
            return
        }
        guard FileManager.default.isExecutableFile(atPath: helper.path) else {
            banner = Banner(text: "The command-line tool isn't inside this copy of NovaLink. Reinstall NovaLink from the disk image.", isError: true)
            return
        }
        let quoted = "'" + helper.path.replacingOccurrences(of: "'", with: "'\\''") + "'"
        let shell = "mkdir -p /usr/local/bin && ln -sfn \(quoted) /usr/local/bin/novalink"
        let script = "do shell script \"\(shell.replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: "\"", with: "\\\""))\" with administrator privileges"
        let result = await Task.detached { Self.osascript(script) }.value
        switch result {
        case .success:
            banner = Banner(text: "Done. You can now type novalink in Terminal.", isError: false)
        case .failure(let message):
            if message.contains("-128") { return }  // The user cancelled the password prompt.
            banner = Banner(text: "Couldn't install the command-line tool: \(message)", isError: true)
        }
    }

    private enum ScriptResult: Sendable { case success, failure(String) }

    private nonisolated static func osascript(_ script: String) -> ScriptResult {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/usr/bin/osascript")
        p.arguments = ["-e", script]
        let err = Pipe()
        p.standardError = err
        p.standardOutput = FileHandle.nullDevice
        do { try p.run() } catch { return .failure(error.localizedDescription) }
        let text = String(decoding: err.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
        p.waitUntilExit()
        return p.terminationStatus == 0 ? .success : .failure(text.trimmingCharacters(in: .whitespacesAndNewlines))
    }
}
