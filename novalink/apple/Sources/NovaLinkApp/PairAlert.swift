import AppKit
import NovaLinkKit
import NovaUI

/// Asks "Pair with X?" in front of everything, one request at a time, even with the popover closed.
@MainActor
final class PairAlert {
    private var queue: [PendingPair] = []
    private var current: PendingPair?
    private var abandoned = false
    private var modalRunning = false
    var onAnswer: (PendingPair, Bool) -> Void = { _, _ in }

    func enqueue(_ requests: [PendingPair]) {
        queue.append(contentsOf: requests)
        next()
    }

    /// The request went away (timed out, cancelled by the other side): take its alert down.
    func withdraw(_ peers: [String]) {
        queue.removeAll { peers.contains($0.peer) }
        if let c = current, peers.contains(c.peer) {
            abandoned = true
            if modalRunning { NSApp.abortModal() }
        }
    }

    private func next() {
        guard current == nil, !queue.isEmpty else { return }
        let p = queue.removeFirst()
        current = p
        abandoned = false
        // Out of the caller's stack: the alert runs its own loop until it is answered.
        Task { @MainActor in
            if !abandoned {
                let answer = ask(p)
                if !abandoned, let answer { onAnswer(p, answer) }
            }
            current = nil
            next()
        }
    }

    /// true = Pair, false = Don't Pair, nil = taken down without an answer.
    private func ask(_ p: PendingPair) -> Bool? {
        let text = MenuText.pairPrompt(p)
        // Enter and Escape both refuse; accepting takes a deliberate click.
        let alert = NovaAlert(title: text.title, message: text.body,
                              buttons: [.init("Pair"), .init("Don't Pair", role: .primary)])
        modalRunning = true
        defer { modalRunning = false }
        switch alert.runModal() {
        case 0: return true
        case 1: return false
        default: return nil
        }
    }
}
