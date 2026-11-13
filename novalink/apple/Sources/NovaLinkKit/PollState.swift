import Foundation

public extension ClientError {
    /// Nothing usable answers on the agent's port, so nothing from the last poll still holds.
    var meansAgentGone: Bool {
        switch self {
        case .notRunning, .impostor, .tokenMissing: true
        case .timedOut, .api, .other: false
        }
    }
}

/// What the menu shows from the agent's replies. A section whose fetch fails keeps its last
/// good value (so an open pairing alert isn't withdrawn by one bad poll) and is marked
/// stale; only an agent that is gone clears everything.
public struct PollState: Equatable, Sendable {
    public enum Section: Hashable, Sendable { case status, pending, discovered, checks }

    public private(set) var status: AgentStatus?
    public private(set) var pending: [PendingPair] = []
    public private(set) var discovered: [DiscoveredDevice] = []
    public private(set) var checks: [Check] = []
    /// Why there is no status to show.
    public private(set) var connectionError: ClientError?
    /// Sections showing an older value, with why the latest fetch failed.
    public private(set) var stale: [Section: String] = [:]

    public init() {}

    public mutating func agentGone(_ error: ClientError) {
        self = PollState()
        connectionError = error
    }

    public mutating func apply(status s: Result<AgentStatus, ClientError>,
                               pending p: Result<[PendingPair], ClientError>,
                               discovered d: Result<[DiscoveredDevice], ClientError>,
                               checks c: Result<[Check], ClientError>) {
        if case .failure(let e) = s, e.meansAgentGone { return agentGone(e) }
        if case .failure(let e) = s, status == nil {
            connectionError = e
            return
        }
        connectionError = nil
        (status, stale[.status]) = Self.pick(s.map(Optional.some), status)
        (pending, stale[.pending]) = Self.pick(p, pending)
        (discovered, stale[.discovered]) = Self.pick(d, discovered)
        (checks, stale[.checks]) = Self.pick(c, checks)
    }

    /// The new value, or the old one and why it wasn't replaced.
    private static func pick<T>(_ r: Result<T, ClientError>, _ old: T) -> (T, String?) {
        switch r {
        case .success(let v): (v, nil)
        case .failure(let e): (old, e.errorDescription ?? "\(e)")
        }
    }
}
