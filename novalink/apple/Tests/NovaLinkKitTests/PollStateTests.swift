import Foundation
import Testing
@testable import NovaLinkKit

private func status() throws -> AgentStatus {
    try JSONDecoder().decode(AgentStatus.self, from: fixture("status.json"))
}

private let request = PendingPair(peer: String(repeating: "b", count: 64), name: "Desk", code: "123456")
private let device = DiscoveredDevice(id: String(repeating: "c", count: 64), name: "Laptop")
private let check = Check(name: "capture", ok: true, detail: "", advice: "")

private func good(_ s: inout PollState) throws {
    s.apply(status: .success(try status()), pending: .success([request]),
            discovered: .success([device]), checks: .success([check]))
}

@Suite struct PollStateTests {
    @Test func oneFailingSectionKeepsItsLastValueAndTheRest() throws {
        var s = PollState()
        try good(&s)
        s.apply(status: .success(try status()), pending: .failure(.timedOut),
                discovered: .success([]), checks: .failure(.other("bad reply")))
        // The pairing alert must not be withdrawn because one poll failed.
        #expect(s.pending == [request])
        #expect(s.checks == [check])
        #expect(s.discovered.isEmpty)
        #expect(s.status != nil && s.connectionError == nil)
        #expect(s.stale[.pending] != nil && s.stale[.checks] == "bad reply")
        #expect(s.stale[.discovered] == nil)
    }

    @Test func transientStatusFailureKeepsTheLastStatus() throws {
        var s = PollState()
        try good(&s)
        s.apply(status: .failure(.timedOut), pending: .failure(.timedOut),
                discovered: .failure(.timedOut), checks: .failure(.timedOut))
        #expect(s.status != nil && s.pending == [request] && s.connectionError == nil)
        #expect(s.stale[.status] != nil)
        try good(&s)
        #expect(s.stale.isEmpty)
    }

    @Test func goneAgentClearsEverything() throws {
        for e in [ClientError.notRunning, .impostor, .tokenMissing] {
            var s = PollState()
            try good(&s)
            s.agentGone(e)
            #expect(s.status == nil && s.pending.isEmpty && s.discovered.isEmpty && s.checks.isEmpty)
            #expect(s.connectionError == e && s.stale.isEmpty)
            #expect(e.meansAgentGone)
        }
        #expect(!ClientError.timedOut.meansAgentGone)
    }

    @Test func failureBeforeAnyStatusIsAConnectionError() {
        var s = PollState()
        s.apply(status: .failure(.timedOut), pending: .failure(.timedOut),
                discovered: .failure(.timedOut), checks: .failure(.timedOut))
        #expect(s.status == nil && s.connectionError == .timedOut)
    }

    @Test func notRunningWordingPointsAtTheMenu() {
        #expect(ClientError.notRunning.errorDescription == "NovaLink isn't running — choose Turn On NovaLink.")
    }
}
