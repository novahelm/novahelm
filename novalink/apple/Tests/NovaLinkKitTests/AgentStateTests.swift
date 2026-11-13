import Foundation
import ServiceManagement
import Testing
@testable import NovaLinkKit

@Test func neverRegisteredReadsNotRegisteredWhenTheHelperShips() {
    // macOS answers .notFound for an agent that was never registered (a first launch).
    #expect(AgentState.of(.notFound, helperPresent: true) == .notRegistered)
    #expect(AgentState.of(.notRegistered, helperPresent: true) == .notRegistered)
}

@Test func missingHelperFilesReadNotFound() {
    #expect(AgentState.of(.notFound, helperPresent: false) == .notFound)
    #expect(AgentState.of(.notRegistered, helperPresent: false) == .notFound)
}

@Test func enabledAndApprovalPassThrough() {
    #expect(AgentState.of(.enabled, helperPresent: true) == .enabled)
    #expect(AgentState.of(.requiresApproval, helperPresent: true) == .requiresApproval)
}

@Test func helperPresentNeedsBothThePlistAndTheAgent() throws {
    let app = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString + ".app")
    defer { try? FileManager.default.removeItem(at: app) }
    let plist = app.appendingPathComponent("Contents/Library/LaunchAgents/com.novalink.agent.plist")
    let agent = app.appendingPathComponent("Contents/Helpers/novalink-agent")
    #expect(!AgentState.helperPresent(in: app))
    try FileManager.default.createDirectory(at: plist.deletingLastPathComponent(), withIntermediateDirectories: true)
    try Data().write(to: plist)
    #expect(!AgentState.helperPresent(in: app))
    try FileManager.default.createDirectory(at: agent.deletingLastPathComponent(), withIntermediateDirectories: true)
    try Data().write(to: agent)
    #expect(AgentState.helperPresent(in: app))
}
