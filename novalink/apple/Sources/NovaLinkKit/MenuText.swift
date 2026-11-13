import Foundation

public enum MenuText {
    public static func header(_ s: AgentStatus) -> String {
        if s.peers.isEmpty { return "Not paired yet" }
        if !s.peers.contains(where: \.connected) { return "Not connected to any device" }
        if s.controller == s.me { return "This Mac is in control" }
        let names = displayNames(s.peers)
        let name = zip(s.peers, names).first { $0.0.id == s.controller }?.1 ?? "Another device"
        return "\(name) is in control"
    }

    /// The first 8 hex characters of an id: enough to tell two devices apart.
    public static func shortId(_ id: String) -> String { String(id.prefix(8)) }

    /// Each device's name, with the short id added where two share a name.
    public static func displayNames(_ peers: [Peer]) -> [String] {
        var counts: [String: Int] = [:]
        for p in peers { counts[p.name, default: 0] += 1 }
        return peers.map { counts[$0.name, default: 0] > 1 ? "\($0.name) (\(shortId($0.id)))" : $0.name }
    }

    /// Turn On registered the agent, but macOS wants the user to allow it first.
    /// Turn On registered the agent, but it hadn't answered after 15 s. Cleared once it does.
    public static let agentNotAnswering =
        "NovaLink was switched on but still isn't answering. Choose Open Logs to see why, or restart your Mac and try again."
    public static let agentNeedsApproval =
        "Allow NovaLink in System Settings → General → Login Items, then choose Turn On again."

    /// Start at Login was switched on, but macOS wants the user to allow it first.
    public static let loginItemNeedsApproval =
        "Allow NovaLink in System Settings → General → Login Items to finish turning on Start at Login."

    /// A section showing an older value because the latest fetch failed.
    public static func staleNote(_ reason: String) -> String {
        "Couldn't refresh this: \(reason)"
    }

    private static func milliseconds(_ us: UInt64) -> String { String(format: "%.1f ms", Double(us) / 1000) }

    /// Under the header: how many devices are connected, and the latency of the one in use.
    public static func headerDetail(_ s: AgentStatus) -> String? {
        if s.peers.isEmpty { return nil }
        let connected = s.peers.filter(\.connected).count
        var text = "\(connected) of \(s.peers.count) \(s.peers.count == 1 ? "device" : "devices") connected"
        let inUse = s.controller == s.me ? nil : s.peers.first { $0.id == s.controller && $0.connected }
        if let us = (inUse ?? s.peers.first(where: \.connected))?.rttUs { text += " · \(milliseconds(us))" }
        return text
    }

    /// A device row's second line: connection, latency and where it sits.
    public static func deviceDetail(_ p: Peer, position: Edge?) -> String {
        var parts = [p.connected ? "Connected" : "Not connected"]
        if p.connected, let us = p.rttUs { parts.append(milliseconds(us)) }
        if let position {
            let label = positionLabel(position)
            parts.append(label.prefix(1).lowercased() + label.dropFirst())
        } else {
            parts.append("position not set")
        }
        return parts.joined(separator: " · ")
    }

    public static func pairingTitle(_ p: PendingPair) -> String {
        "\(p.name) (\(shortId(p.peer))) wants to pair"
    }

    public static func pairingDetail(_ p: PendingPair) -> String {
        "Code \(p.code) · pair only if it matches on both screens"
    }

    /// One line for the diagnostics row; nil before the first doctor run.
    public static func diagnosticsSummary(_ checks: [Check]) -> String? {
        if checks.isEmpty { return nil }
        let failed = checks.filter { !$0.ok }.count
        if failed == 0 { return checks.count == 1 ? "1 check passes" : "All \(checks.count) checks pass" }
        return "\(failed) of \(checks.count) \(failed == 1 ? "checks fails" : "checks fail")"
    }
}
