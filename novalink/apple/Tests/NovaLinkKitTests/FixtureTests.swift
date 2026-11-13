import Foundation
import Testing
@testable import NovaLinkKit

/// The app must decode exactly what the agent sends.
@Suite struct FixtureTests {
    @Test func statusWithPeerAndLayoutLink() throws {
        let s = try JSONDecoder().decode(AgentStatus.self, from: fixture("status.json"))
        #expect(s.peers.count == 1 && s.peers[0].connected)
        #expect(s.layout.links.count == 1)
        #expect(s.layout.links[0].fromEdge == .right)
        #expect(s.layout.links[0].from == s.me && s.layout.links[0].to == s.peers[0].id)
        #expect(s.captureState == .running)
        #expect(s.settings == AgentSettings(swapCtrlCmd: false))
    }

    @Test func pendingPairs() throws {
        let p = try JSONDecoder().decode([PendingPair].self, from: fixture("pending.json"))
        #expect(p.count == 1 && p[0].code.count == 6 && p[0].peer.count == 64)
    }

    @Test func discoveredDevices() throws {
        let d = try JSONDecoder().decode([DiscoveredDevice].self, from: fixture("discovered.json"))
        #expect(d.count == 1 && d[0].name == "Desk-PC")
    }

    @Test func doctorChecks() throws {
        struct Reply: Decodable { var checks: [Check] }
        let r = try JSONDecoder().decode(Reply.self, from: fixture("doctor.json"))
        #expect(r.checks.contains { $0.name == "capture" && $0.ok })
    }

    @Test func edgeDecodesAnyCaseAndEncodesLowercase() throws {
        for (text, edge) in [("Left", Edge.left), ("right", .right), ("TOP", .top), ("Bottom", .bottom)] {
            #expect(try JSONDecoder().decode(Edge.self, from: Data("\"\(text)\"".utf8)) == edge)
        }
        #expect(throws: DecodingError.self) { try JSONDecoder().decode(Edge.self, from: Data("\"Sideways\"".utf8)) }
        #expect(String(decoding: try JSONEncoder().encode(Edge.left), as: UTF8.self) == "\"left\"")
    }
}
