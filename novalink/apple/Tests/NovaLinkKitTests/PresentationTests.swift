import Testing
@testable import NovaLinkKit

private let me = String(repeating: "a", count: 64)
private let desk = String(repeating: "b", count: 64)

private func status(controller: String, connected: Bool = true, links: [Link] = [],
                    capture: CaptureState? = nil) -> AgentStatus {
    AgentStatus(me: me, name: "Studio", controller: controller, focus: controller,
                peers: [Peer(id: desk, name: "Desk", connected: connected, rttUs: nil, path: "Ethernet", dryInject: nil)],
                layout: Layout(version: 1, links: links), capture: capture, displays: nil)
}

@Test func edgesHaveOpposites() {
    #expect(Edge.left.opposite == .right)
    #expect(Edge.right.opposite == .left)
    #expect(Edge.top.opposite == .bottom)
    #expect(Edge.bottom.opposite == .top)
}

@Test func positionComesFromEitherDirection() {
    let mine = status(controller: me, links: [Link(from: me, fromEdge: .right, to: desk)])
    #expect(mine.position(of: desk) == .right)
    let theirs = status(controller: me, links: [Link(from: desk, fromEdge: .left, to: me)])
    #expect(theirs.position(of: desk) == .right)
    #expect(status(controller: me).position(of: desk) == nil)
}

@Test func iconFollowsState() {
    #expect(IconState.of(status(controller: me), turnedOff: false) == .inControl)
    #expect(IconState.of(status(controller: desk), turnedOff: false) == .remote)
    #expect(IconState.of(status(controller: me, connected: false), turnedOff: false) == .disconnected)
    #expect(IconState.of(nil, turnedOff: true) == .disconnected)
}

@Test func iconIsProblemOnlyWhenBrokenNotWhenDoctorChecksFail() {
    #expect(IconState.of(status(controller: me, capture: .waitingForPermission), turnedOff: false) == .problem)
    #expect(IconState.of(status(controller: me, capture: .failed), turnedOff: false) == .problem)
    #expect(IconState.of(nil, turnedOff: false) == .problem)
}

@Test func positionLabelsAreReadable() {
    #expect(MenuText.positionLabel(.left) == "Left of this Mac")
    #expect(MenuText.positionLabel(.bottom) == "Below this Mac")
}

@Test func pairPromptNamesDeviceWithShortId() {
    let p = PendingPair(peer: desk, name: "Desk", code: "123456")
    let t = MenuText.pairPrompt(p)
    #expect(t.title == "Pair with Desk (bbbbbbbb)?")
    #expect(t.body == "Does Desk (bbbbbbbb) show the code 123456? Choose Pair only if the codes match on both screens.")
}

@Test func installRefusesDiskImageAndTranslocation() {
    #expect(InstallLocation.isStable("/Applications/NovaLink.app"))
    #expect(InstallLocation.isStable("/Users/x/Applications/NovaLink.app"))
    #expect(!InstallLocation.isStable("/Volumes/NovaLink/NovaLink.app"))
    #expect(!InstallLocation.isStable("/private/var/folders/ab/T/AppTranslocation/ID/d/NovaLink.app"))
}

@Test func iconMapsToHeaderTone() {
    #expect(IconState.inControl.tone == .info)
    #expect(IconState.remote.tone == .ok)
    #expect(IconState.disconnected.tone == .idle)
    #expect(IconState.problem.tone == .bad)
}
