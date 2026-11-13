import Testing
@testable import NovaLinkKit

private func pair(_ peer: String, _ code: String) -> PendingPair {
    PendingPair(peer: peer, name: "Desk", code: code)
}

@Test func promptsOnceThenWithdrawsWhenGone() {
    var t = PairPromptTracker()
    let a = pair("a", "111111")
    var r = t.update([a])
    #expect(r.prompt.count == 1 && r.withdraw.isEmpty)
    #expect(r.prompt[0].peer == "a")
    r = t.update([a])
    #expect(r.prompt.isEmpty && r.withdraw.isEmpty)
    r = t.update([])
    #expect(r.prompt.isEmpty)
    #expect(r.withdraw == ["a"])
}

@Test func repromptsNewCodeFromSameDevice() {
    var t = PairPromptTracker()
    _ = t.update([pair("a", "111111")])
    let r = t.update([pair("a", "222222")])
    #expect(r.prompt.count == 1)
    #expect(r.prompt[0].code == "222222")
    #expect(r.withdraw.isEmpty)
}
