import Foundation

// Typed views of the agent's JSON. The agent is the source of truth; these mirror what it
// emits. Device ids are 64-character hex strings.

/// The agent reports edges capitalized ("Right") and accepts any case in PUT /v1/layout;
/// the app reads either and sends lowercase.
public enum Edge: String, Codable, Sendable, CaseIterable {
    case left, right, top, bottom

    public init(from decoder: Decoder) throws {
        let text = try decoder.singleValueContainer().decode(String.self)
        guard let e = Edge(rawValue: text.lowercased()) else {
            throw DecodingError.dataCorrupted(.init(codingPath: decoder.codingPath,
                                                    debugDescription: "unknown edge \(text)"))
        }
        self = e
    }
}

public enum CaptureState: String, Codable, Sendable {
    case running
    case waitingForPermission = "waiting_for_permission"
    case failed
}

public struct AgentStatus: Codable, Sendable, Equatable {
    public var me: String
    public var name: String
    public var controller: String
    public var focus: String
    public var peers: [Peer]
    public var layout: Layout
    /// Absent from older agents, and from any value this app doesn't know.
    public var capture: CaptureState?
    public var displays: [Display]?
    /// Absent from agents that predate settings.
    public var settings: AgentSettings?

    public init(me: String, name: String, controller: String, focus: String, peers: [Peer],
                layout: Layout, capture: CaptureState?, displays: [Display]?,
                settings: AgentSettings? = nil) {
        self.me = me; self.name = name; self.controller = controller; self.focus = focus
        self.peers = peers; self.layout = layout; self.capture = capture; self.displays = displays
        self.settings = settings
    }

    /// An agent that predates `capture` is treated as capturing normally.
    public var captureState: CaptureState { capture ?? .running }

    enum CodingKeys: String, CodingKey {
        case me, name, controller, focus, peers, layout, capture, displays, settings
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        me = try c.decode(String.self, forKey: .me)
        name = try c.decode(String.self, forKey: .name)
        controller = try c.decode(String.self, forKey: .controller)
        focus = try c.decode(String.self, forKey: .focus)
        peers = try c.decode([Peer].self, forKey: .peers)
        layout = try c.decode(Layout.self, forKey: .layout)
        capture = (try? c.decodeIfPresent(CaptureState.self, forKey: .capture)) ?? nil
        displays = try? c.decodeIfPresent([Display].self, forKey: .displays)
        settings = try? c.decodeIfPresent(AgentSettings.self, forKey: .settings)
    }
}

public struct AgentSettings: Codable, Sendable, Equatable {
    /// Keys from other computers' keyboards arrive with Ctrl and Cmd swapped.
    public var swapCtrlCmd: Bool

    public init(swapCtrlCmd: Bool) { self.swapCtrlCmd = swapCtrlCmd }

    enum CodingKeys: String, CodingKey { case swapCtrlCmd = "swap_ctrl_cmd" }
}

public struct Display: Codable, Sendable, Equatable {
    public var id: Int
    public var x: Double
    public var y: Double
    public var width: Double
    public var height: Double
    public var scale: Double
}

public struct Peer: Codable, Sendable, Equatable {
    public var id: String
    public var name: String
    public var connected: Bool
    public var rttUs: UInt64?
    /// How the connection runs, e.g. "Ethernet".
    public var path: String
    public var dryInject: Bool?

    public init(id: String, name: String, connected: Bool, rttUs: UInt64?, path: String, dryInject: Bool?) {
        self.id = id; self.name = name; self.connected = connected
        self.rttUs = rttUs; self.path = path; self.dryInject = dryInject
    }

    enum CodingKeys: String, CodingKey {
        case id, name, connected, path
        case rttUs = "rtt_us"
        case dryInject = "dry_inject"
    }
}

public struct Layout: Codable, Sendable, Equatable {
    public var version: UInt64
    public var links: [Link]

    public init(version: UInt64, links: [Link]) {
        self.version = version; self.links = links
    }
}

/// The device `to` sits beyond `fromEdge` of device `from`.
public struct Link: Codable, Sendable, Equatable {
    public var from: String
    public var fromEdge: Edge
    public var to: String

    public init(from: String, fromEdge: Edge, to: String) {
        self.from = from; self.fromEdge = fromEdge; self.to = to
    }

    enum CodingKeys: String, CodingKey {
        case from, to
        case fromEdge = "from_edge"
    }
}

public struct PendingPair: Codable, Sendable, Equatable {
    public var peer: String
    public var name: String
    public var code: String

    public init(peer: String, name: String, code: String) {
        self.peer = peer; self.name = name; self.code = code
    }
}

public struct DiscoveredDevice: Codable, Sendable, Equatable {
    public var id: String
    public var name: String
}

public struct Check: Codable, Sendable, Equatable {
    public var name: String
    public var ok: Bool
    public var detail: String
    public var advice: String
}
