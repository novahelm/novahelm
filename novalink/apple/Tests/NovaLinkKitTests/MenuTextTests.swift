import Testing
@testable import NovaLinkKit

private let me = String(repeating: "a", count: 64)
private let desk = String(repeating: "b", count: 64)
private let laptop = String(repeating: "c", count: 64)

private func peer(_ id: String, _ name: String, connected: Bool, rtt: UInt64? = nil) -> Peer {
    Peer(id: id, name: name, connected: connected, rttUs: rtt, path: "Ethernet", dryInject: nil)
}

private func status(controller: String, peers: [Peer]) -> AgentStatus {
    AgentStatus(me: me, name: "Studio", controller: controller, focus: controller, peers: peers,
                layout: Layout(version: 0, links: []), capture: nil, displays: nil)
}

@Test func headerSaysWhoControls() {
    let peers = [peer(desk, "Desk", connected: true)]
    #expect(MenuText.header(status(controller: me, peers: peers)) == "This Mac is in control")
    #expect(MenuText.header(status(controller: desk, peers: peers)) == "Desk is in control")
}

@Test func headerCoversUnpairedAndDisconnected() {
    #expect(MenuText.header(status(controller: me, peers: [])) == "Not paired yet")
    let off = [peer(desk, "Desk", connected: false)]
    #expect(MenuText.header(status(controller: me, peers: off)) == "Not connected to any device")
}

private func checks(ok: Int, failed: Int) -> [Check] {
    (0..<ok).map { Check(name: "ok\($0)", ok: true, detail: "", advice: "") }
        + (0..<failed).map { Check(name: "bad\($0)", ok: false, detail: "", advice: "") }
}

@Test func headerDetailCountsAndShowsLatency() {
    let peers = [peer(desk, "Desk", connected: true, rtt: 2100), peer(laptop, "Laptop", connected: false)]
    #expect(MenuText.headerDetail(status(controller: me, peers: peers)) == "1 of 2 devices connected · 2.1 ms")
    #expect(MenuText.headerDetail(status(controller: me, peers: [])) == nil)
}
@Test func headerDetailLeavesOutMissingLatency() {
    #expect(MenuText.headerDetail(status(controller: me, peers: [peer(desk, "Desk", connected: true)]))
            == "1 of 1 device connected")
    #expect(MenuText.headerDetail(status(controller: me, peers: [peer(laptop, "Laptop", connected: false)]))
            == "0 of 1 device connected")
}
@Test func deviceDetailReadsConnectionLatencyAndPosition() {
    #expect(MenuText.deviceDetail(peer(desk, "Desk", connected: true, rtt: 2100), position: .left)
            == "Connected · 2.1 ms · left of this Mac")
    #expect(MenuText.deviceDetail(peer(desk, "Desk", connected: true), position: .top) == "Connected · above this Mac")
    #expect(MenuText.deviceDetail(peer(laptop, "Laptop", connected: false), position: nil)
            == "Not connected · position not set")
}
@Test func pairingTitleKeepsTheShortId() {
    let p = PendingPair(peer: "4f9a21c0deadbeef", name: "Studio PC", code: "482 913")
    #expect(MenuText.pairingTitle(p) == "Studio PC (4f9a21c0) wants to pair")
    #expect(MenuText.pairingDetail(p) == "Code 482 913 · pair only if it matches on both screens")
}
@Test func diagnosticsSummaryCountsFailures() {
    #expect(MenuText.diagnosticsSummary([]) == nil)
    #expect(MenuText.diagnosticsSummary(checks(ok: 6, failed: 0)) == "All 6 checks pass")
    #expect(MenuText.diagnosticsSummary(checks(ok: 1, failed: 0)) == "1 check passes")
    #expect(MenuText.diagnosticsSummary(checks(ok: 4, failed: 2)) == "2 of 6 checks fail")
    #expect(MenuText.diagnosticsSummary(checks(ok: 5, failed: 1)) == "1 of 6 checks fails")
}

@Test func headerDetailIgnoresLatencyOfDisconnectedController() {
    let peers = [peer(desk, "Desk", connected: false, rtt: 2100)]
    #expect(MenuText.headerDetail(status(controller: desk, peers: peers)) == "0 of 1 device connected")
}
@Test func headerDetailShowsLatencyOfConnectedController() {
    let peers = [peer(desk, "Desk", connected: true, rtt: 3400), peer(laptop, "Laptop", connected: true, rtt: 900)]
    #expect(MenuText.headerDetail(status(controller: desk, peers: peers)) == "2 of 2 devices connected · 3.4 ms")
}
