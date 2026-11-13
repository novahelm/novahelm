import Foundation

public extension Edge {
    var opposite: Edge {
        switch self {
        case .left: .right
        case .right: .left
        case .top: .bottom
        case .bottom: .top
        }
    }
}

public extension AgentStatus {
    /// Which side of this Mac a peer sits on: a link from either end says so.
    func position(of peer: String) -> Edge? {
        for l in layout.links {
            if l.from == me && l.to == peer { return l.fromEdge }
            if l.from == peer && l.to == me { return l.fromEdge.opposite }
        }
        return nil
    }
}

public enum HeaderTone: Sendable, Equatable { case ok, info, idle, bad }

public enum IconState: Sendable, Equatable {
    case inControl, remote, disconnected, problem

    /// `status` is nil when the agent didn't answer. A failing doctor check alone is not a
    /// problem; capture that isn't running is.
    public static func of(_ status: AgentStatus?, turnedOff: Bool) -> IconState {
        guard let status else { return turnedOff ? .disconnected : .problem }
        if status.captureState != .running { return .problem }
        if !status.peers.contains(where: \.connected) { return .disconnected }
        return status.controller == status.me ? .inControl : .remote
    }

    /// How the popover header colours this state; the app maps it onto its status palette.
    public var tone: HeaderTone {
        switch self {
        case .inControl: .info
        case .remote: .ok
        case .disconnected: .idle
        case .problem: .bad
        }
    }

    public var symbolName: String {
        switch self {
        case .inControl: "cursorarrow.rays"
        case .remote: "display.2"
        case .disconnected: "bolt.horizontal.circle"
        case .problem: "exclamationmark.triangle"
        }
    }
}

public extension MenuText {
    static func positionLabel(_ e: Edge) -> String {
        switch e {
        case .left: "Left of this Mac"
        case .right: "Right of this Mac"
        case .top: "Above this Mac"
        case .bottom: "Below this Mac"
        }
    }
}

public extension MenuText {
    /// The pairing question. The short id tells apart two devices with the same name.
    static func pairPrompt(_ p: PendingPair) -> (title: String, body: String) {
        let who = "\(p.name) (\(shortId(p.peer)))"
        return ("Pair with \(who)?",
                "Does \(who) show the code \(p.code)? Choose Pair only if the codes match on both screens.")
    }
}

public enum InstallLocation {
    /// False for an app running from a disk image or translocated: a link into it would dangle.
    public static func isStable(_ appPath: String) -> Bool {
        !appPath.contains("/AppTranslocation/") && !appPath.hasPrefix("/Volumes/")
    }
}
