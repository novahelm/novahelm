import Foundation

/// The agent's device list entry (`DeviceInfo` in crates/core).
public struct Device: Decodable, Equatable, Sendable, Identifiable {
    public enum Kind: String, Decodable, Sendable {
        case light, streamDeck = "stream_deck", prompter, capture, mouse, keyboard, audio, controller, other
    }

    public enum Status: Equatable, Sendable {
        case ready, listed, busy(String)
    }

    public let id: String
    public let kind: Kind
    public let name: String
    public let vendor: String
    public let status: Status
    /// Driver state; only the few fields the menu reads.
    public let on: Bool?
    public let brightness: Int?
    public let kelvin: Int?
    public let dpi: Int?
    public let control: Bool?
    /// The agent says a newer firmware exists for this device.
    public let updateAvailable: Bool
    /// The agent lists it but knows nothing about it (`state.unknown`).
    public let unknown: Bool

    enum CodingKeys: String, CodingKey { case id, kind, name, vendor, status, state, firmware }
    enum FirmwareKeys: String, CodingKey { case updateAvailable = "update_available" }
    enum StatusKeys: String, CodingKey { case state, reason }
    enum StateKeys: String, CodingKey { case on, brightness, kelvin, dpi, control, unknown }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        kind = (try? c.decode(Kind.self, forKey: .kind)) ?? .other
        name = try c.decode(String.self, forKey: .name)
        vendor = (try? c.decode(String.self, forKey: .vendor)) ?? ""
        let s = try c.nestedContainer(keyedBy: StatusKeys.self, forKey: .status)
        switch try s.decode(String.self, forKey: .state) {
        case "ready": status = .ready
        case "busy": status = .busy((try? s.decode(String.self, forKey: .reason)) ?? "")
        default: status = .listed
        }
        let st = try? c.nestedContainer(keyedBy: StateKeys.self, forKey: .state)
        on = try? st?.decodeIfPresent(Bool.self, forKey: .on)
        brightness = try? st?.decodeIfPresent(Int.self, forKey: .brightness)
        kelvin = try? st?.decodeIfPresent(Int.self, forKey: .kelvin)
        dpi = try? st?.decodeIfPresent(Int.self, forKey: .dpi)
        control = try? st?.decodeIfPresent(Bool.self, forKey: .control)
        unknown = ((try? st?.decodeIfPresent(Bool.self, forKey: .unknown)) ?? nil) ?? false
        let fw = try? c.nestedContainer(keyedBy: FirmwareKeys.self, forKey: .firmware)
        updateAvailable = ((try? fw?.decodeIfPresent(Bool.self, forKey: .updateAvailable)) ?? nil) ?? false
    }

    public init(id: String, kind: Kind, name: String, status: Status = .ready, on: Bool? = nil,
                brightness: Int? = nil, kelvin: Int? = nil, dpi: Int? = nil, control: Bool? = nil, updateAvailable: Bool = false, unknown: Bool = false) {
        self.updateAvailable = updateAvailable; self.unknown = unknown
        self.id = id; self.kind = kind; self.name = name; vendor = ""; self.status = status
        self.on = on; self.brightness = brightness; self.kelvin = kelvin; self.dpi = dpi; self.control = control
    }
}

/// A mode as `/api/status` lists it (the current one also carries an accent, unused here).
public struct ModeInfo: Decodable, Equatable, Sendable, Identifiable {
    public let id: String
    public let name: String
    public init(id: String, name: String) { self.id = id; self.name = name }
}

/// The oldest waiting approval. Title and source come from other programs: show them as
/// plain text only.
public struct ApprovalTop: Decodable, Equatable, Sendable {
    public let id: String
    public let title: String
    public let source: String
    public init(id: String, title: String, source: String) { self.id = id; self.title = title; self.source = source }
}

public struct ApprovalsInfo: Decodable, Equatable, Sendable {
    public let pending: Int
    public let top: ApprovalTop?

    enum CodingKeys: String, CodingKey { case pending, top }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        pending = (try? c.decode(Int.self, forKey: .pending)) ?? 0
        top = try? c.decodeIfPresent(ApprovalTop.self, forKey: .top)
    }

    public init(pending: Int, top: ApprovalTop? = nil) { self.pending = pending; self.top = top }
}

/// The agent's `/api/status`. Older agents leave the newer fields out.
public struct AgentStatus: Decodable, Equatable, Sendable {
    public let version: String
    public let networkBlocked: Bool
    public let problems: [String]
    public let firmwareUpdates: Int
    /// Nil from an older agent, or while no mode is showing.
    public let mode: ModeInfo?
    public let modes: [ModeInfo]
    /// Nil from an older agent (and for a caller that isn't staff).
    public let approvals: ApprovalsInfo?

    enum CodingKeys: String, CodingKey {
        case version, problems, mode, modes, approvals
        case networkBlocked = "network_blocked", firmwareUpdates = "firmware_updates"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        version = (try? c.decode(String.self, forKey: .version)) ?? ""
        networkBlocked = (try? c.decode(Bool.self, forKey: .networkBlocked)) ?? false
        problems = (try? c.decode([String].self, forKey: .problems)) ?? []
        firmwareUpdates = (try? c.decode(Int.self, forKey: .firmwareUpdates)) ?? 0
        mode = (try? c.decodeIfPresent(ModeInfo.self, forKey: .mode)) ?? nil
        modes = (try? c.decode([ModeInfo].self, forKey: .modes)) ?? []
        approvals = (try? c.decodeIfPresent(ApprovalsInfo.self, forKey: .approvals)) ?? nil
    }

    public init(version: String = "", networkBlocked: Bool = false, problems: [String] = [], firmwareUpdates: Int = 0,
                mode: ModeInfo? = nil, modes: [ModeInfo] = [], approvals: ApprovalsInfo? = nil) {
        self.version = version; self.networkBlocked = networkBlocked
        self.problems = problems; self.firmwareUpdates = firmwareUpdates
        self.mode = mode; self.modes = modes; self.approvals = approvals
    }
}

/// Everything the popover shows, fetched in one go.
public struct Snapshot: Equatable, Sendable {
    public var devices: [Device]
    public var deckControl: Bool
    public var dpiStages: [Int]
    public var prompterSpeed: Int
    public var prompterPlaying: Bool
    public var status: AgentStatus

    public init(devices: [Device], deckControl: Bool = false, dpiStages: [Int] = [800, 1600, 3200],
                prompterSpeed: Int = 60, prompterPlaying: Bool = false, status: AgentStatus = AgentStatus()) {
        self.status = status
        self.devices = devices; self.deckControl = deckControl; self.dpiStages = dpiStages
        self.prompterSpeed = prompterSpeed; self.prompterPlaying = prompterPlaying
    }

    /// Blocking: four API calls.
    public static func fetch() throws -> Snapshot {
        let devices = try JSONDecoder().decode([Device].self, from: AgentClient.call("GET", "/api/devices"))
        let cfg = try JSONSerialization.jsonObject(with: AgentClient.call("GET", "/api/config")) as? [String: Any] ?? [:]
        let pr = try JSONSerialization.jsonObject(with: AgentClient.call("GET", "/api/prompter")) as? [String: Any] ?? [:]
        // An agent without /api/status still gives a menu.
        let status = (try? JSONDecoder().decode(AgentStatus.self, from: AgentClient.call("GET", "/api/status"))) ?? AgentStatus()
        return Snapshot(
            devices: devices,
            deckControl: cfg["deck_control"] as? Bool ?? false,
            dpiStages: cfg["dpi_stages"] as? [Int] ?? [],
            prompterSpeed: (cfg["prompter"] as? [String: Any])?["speed"] as? Int ?? 60,
            prompterPlaying: (pr["run"] as? [String: Any])?["playing"] as? Bool ?? false,
            status: status)
    }
}

/// The words and states the popover shows, kept free of SwiftUI so they can be tested.
public enum MenuText {
    public static func lights(_ s: Snapshot) -> [Device] { s.devices.filter { $0.kind == .light } }
    public static func lightsOn(_ s: Snapshot) -> Bool { lights(s).contains { $0.on == true } }

    public static func header(_ s: Snapshot) -> String {
        let ready = s.devices.filter { $0.status == .ready }.count
        return "\(ready) of \(s.devices.count) devices ready"
    }

    public static func headerDetail(_ s: Snapshot) -> String {
        let l = lights(s)
        var parts: [String] = []
        if !l.isEmpty { parts.append("\(l.count) light\(l.count == 1 ? "" : "s") \(lightsOn(s) ? "on" : "off")") }
        if s.prompterPlaying { parts.append("prompter playing") }
        if case .busy? = deck(s)?.status { parts.append("Stream Deck held by another app") }
        let unknown = s.devices.filter(\.unknown).count
        if unknown > 0 { parts.append("\(unknown) unknown device\(unknown == 1 ? "" : "s")") }
        return parts.isEmpty ? "No gear found yet" : parts.joined(separator: " · ")
    }

    public static func deck(_ s: Snapshot) -> Device? { s.devices.first { $0.kind == .streamDeck } }
    public static func mouse(_ s: Snapshot) -> Device? { s.devices.first { $0.kind == .mouse && $0.dpi != nil } ?? s.devices.first { $0.kind == .mouse } }

    public static func deckValue(_ s: Snapshot) -> String {
        switch deck(s)?.status {
        case nil: "Not found"
        case .busy?: "Held"
        default: s.deckControl ? "NovaGear" : "Read-only"
        }
    }

    public static func mouseValue(_ s: Snapshot) -> String {
        mouse(s)?.dpi.map { "\($0) DPI" } ?? (mouse(s) == nil ? "Not found" : "—")
    }

    public static func prompterValue(_ s: Snapshot) -> String {
        s.prompterPlaying ? "\(s.prompterSpeed) px/s" : "Paused"
    }

    /// Average of the lights that report it, as the one slider controls them all.
    public static func average(_ s: Snapshot, _ key: KeyPath<Device, Int?>) -> Int? {
        let v = lights(s).compactMap { $0[keyPath: key] }
        return v.isEmpty ? nil : v.reduce(0, +) / v.count
    }

    public static func approvalsLabel(_ n: Int) -> String {
        n == 0 ? "No approvals waiting" : "\(n) approval\(n == 1 ? "" : "s") waiting"
    }

    public static func describe(_ e: Error) -> String {
        switch e as? AgentError {
        case .notRunning?: "The NovaGear agent isn't running."
        case .tokenMissing?: "The agent hasn't started yet on this Mac."
        case .impostor?: "Something else is using NovaGear's port 47820, so NovaGear won't talk to it."
        case .http(_, let m)?: m
        case .badResponse?: "The agent sent an answer NovaGear couldn't read."
        case nil: e.localizedDescription
        }
    }
}
