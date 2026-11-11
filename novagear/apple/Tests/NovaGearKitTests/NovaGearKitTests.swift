import Foundation
import Testing
@testable import NovaGearKit

@Test func proofMatchesTheRustAgent() {
    // Same vector as crates/core/src/proof.rs `matches_the_shared_vector`.
    #expect(agentProof(token: "test-token", nonce: "00112233445566778899aabbccddeeff")
        == "497365c0eb80adc31462d13f0537c71930645cb7d67b3112472900796e485e8c")
    #expect(same("abc", "abc"))
    #expect(!same("abc", "abd"))
    #expect(!same("abc", "ab"))
    #expect(newNonce().count == 32)
}

@Test func decodesTheAgentsDeviceList() throws {
    let json = """
    [{"id":"light-1","kind":"light","name":"Key Light 1","vendor":"Elgato","status":{"state":"ready"},
      "state":{"on":true,"brightness":40,"kelvin":5300,"address":"192.168.0.125:9123"}},
     {"id":"deck","kind":"stream_deck","name":"Stream Deck Plus","vendor":"Elgato","usb":{"vid":4057,"pid":132},
      "status":{"state":"busy","reason":"held"},"state":{}},
     {"id":"m","kind":"mouse","name":"Basilisk","vendor":"Razer","status":{"state":"ready"},"state":{"dpi":6400}},
     {"id":"k","kind":"keyboard","name":"8BitDo","vendor":"8BitDo","status":{"state":"listed"},"state":{}}]
    """
    let d = try JSONDecoder().decode([Device].self, from: Data(json.utf8))
    #expect(d.count == 4)
    #expect(d[0].kind == .light && d[0].on == true && d[0].brightness == 40 && d[0].kelvin == 5300)
    #expect(d[1].kind == .streamDeck && d[1].status == .busy("held"))
    #expect(d[2].dpi == 6400)
    #expect(d[3].status == .listed)
}

@Test func menuTextForASampleDesk() {
    let s = Snapshot(devices: [
        Device(id: "l1", kind: .light, name: "L1", on: false, brightness: 10, kelvin: 5200),
        Device(id: "l2", kind: .light, name: "L2", on: true, brightness: 30, kelvin: 5400),
        Device(id: "d", kind: .streamDeck, name: "Deck", control: true),
        Device(id: "m", kind: .mouse, name: "Basilisk", dpi: 6400),
        Device(id: "k", kind: .keyboard, name: "8BitDo", status: .listed),
    ], deckControl: true, prompterSpeed: 70, prompterPlaying: true)
    #expect(MenuText.header(s) == "4 of 5 devices ready")
    #expect(MenuText.headerDetail(s) == "2 lights on · prompter playing")
    #expect(MenuText.average(s, \.brightness) == 20)
    #expect(MenuText.deckValue(s) == "NovaGear")
    #expect(MenuText.mouseValue(s) == "6400 DPI")
    #expect(MenuText.prompterValue(s) == "70 px/s")
}

@Test func emptyAndHeldStates() {
    let s = Snapshot(devices: [Device(id: "d", kind: .streamDeck, name: "Deck", status: .busy("x"))])
    #expect(MenuText.headerDetail(s) == "Stream Deck held by another app")
    #expect(MenuText.deckValue(s) == "Held")
    #expect(MenuText.mouseValue(s) == "Not found")
    #expect(MenuText.average(s, \.brightness) == nil)
    #expect(MenuText.describe(AgentError.impostor).contains("port 47820"))
}

@Test func decodesNewKindsAndFirmware() throws {
    let json = """
    [{"id":"x","kind":"hovercraft","name":"Hover","vendor":"V","status":{"state":"listed"},"state":{}},
     {"id":"c","kind":"controller","name":"DualSense","vendor":"Sony","status":{"state":"ready"},"state":{}},
     {"id":"l","kind":"light","name":"Key Light","vendor":"Elgato","status":{"state":"ready"},"state":{},
      "firmware":{"installed":"1.0.3 (200)","policy":"native","latest":"1.0.3 (222)","update_available":true}},
     {"id":"m","kind":"mouse","name":"M","vendor":"Razer","status":{"state":"ready"},"state":{},
      "firmware":{"installed":"1.0","policy":"vendor_tool","update_available":false}},
     {"id":"n","kind":"mouse","name":"N","vendor":"Razer","status":{"state":"ready"},"state":{}}]
    """
    let d = try JSONDecoder().decode([Device].self, from: Data(json.utf8))
    #expect(d[0].kind == .other)
    #expect(d[1].kind == .controller)
    #expect(d[2].updateAvailable == true)
    #expect(d[3].updateAvailable == false)
    #expect(d[4].updateAvailable == false)
}

@Test func decodesStatus() throws {
    let json = #"{"version":"0.1.0","network_blocked":true,"problems":[],"firmware_updates":2}"#
    let s = try JSONDecoder().decode(AgentStatus.self, from: Data(json.utf8))
    #expect(s.version == "0.1.0")
    #expect(s.networkBlocked)
    #expect(s.problems.isEmpty)
    #expect(s.firmwareUpdates == 2)
    // An older agent without the newer fields still decodes.
    let old = try JSONDecoder().decode(AgentStatus.self, from: Data(#"{"version":"0.0.9"}"#.utf8))
    #expect(!old.networkBlocked && old.firmwareUpdates == 0)
}

@Test func unknownDevicesAreCountedInTheHeaderDetail() throws {
    let json = """
    [{"id":"u","kind":"other","name":"Mystery","vendor":"","status":{"state":"listed"},"state":{"unknown":true}},
     {"id":"v","kind":"other","name":"Mystery 2","vendor":"","status":{"state":"listed"},"state":{"unknown":true}},
     {"id":"w","kind":"other","name":"Known","vendor":"","status":{"state":"listed"},"state":{}}]
    """
    let devices = try JSONDecoder().decode([Device].self, from: Data(json.utf8))
    #expect(devices.map(\.unknown) == [true, true, false])
    #expect(MenuText.headerDetail(Snapshot(devices: devices)).contains("2 unknown devices"))
    #expect(MenuText.headerDetail(Snapshot(devices: [devices[0]])) == "1 unknown device")
}

@Test func decodesModeAndApprovals() throws {
    let json = """
    {"version":"0.1.0","mode":{"id":"dev","name":"Developer","accent":"#2563eb"},
     "modes":[{"id":"dev","name":"Developer"},{"id":"stream","name":"Stream"}],
     "approvals":{"pending":2,"top":{"id":"a1","title":"**Run** [rm](http://x)","source":"Claude Code"}}}
    """
    let s = try JSONDecoder().decode(AgentStatus.self, from: Data(json.utf8))
    #expect(s.mode == ModeInfo(id: "dev", name: "Developer"))
    #expect(s.modes.map(\.id) == ["dev", "stream"])
    #expect(s.approvals?.pending == 2)
    #expect(s.approvals?.top == ApprovalTop(id: "a1", title: "**Run** [rm](http://x)", source: "Claude Code"))
    // Older agents, and a null mode or top request, still decode.
    let old = try JSONDecoder().decode(AgentStatus.self, from: Data(#"{"version":"0.0.9"}"#.utf8))
    #expect(old.mode == nil && old.modes.isEmpty && old.approvals == nil)
    let none = try JSONDecoder().decode(
        AgentStatus.self, from: Data(#"{"mode":null,"modes":[],"approvals":{"pending":0,"top":null}}"#.utf8))
    #expect(none.mode == nil && none.approvals?.pending == 0 && none.approvals?.top == nil)
}

@Test func modeAndApprovalWords() {
    #expect(MenuText.approvalsLabel(0) == "No approvals waiting")
    #expect(MenuText.approvalsLabel(1) == "1 approval waiting")
    #expect(MenuText.approvalsLabel(3) == "3 approvals waiting")
}
