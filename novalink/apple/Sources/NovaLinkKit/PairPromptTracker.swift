/// Decides which pairing requests deserve a notification: each (device, code) once, a new
/// code from the same device again, and a withdrawal when the request disappears.
public struct PairPromptTracker: Sendable {
    private var shown: [String: String] = [:]

    public init() {}

    public mutating func update(_ pending: [PendingPair]) -> (prompt: [PendingPair], withdraw: [String]) {
        let new = pending.filter { shown[$0.peer] != $0.code }
        let live = Set(pending.map(\.peer))
        let gone = shown.keys.filter { !live.contains($0) }.sorted()
        shown = Dictionary(pending.map { ($0.peer, $0.code) }, uniquingKeysWith: { _, last in last })
        return (new, gone)
    }
}
