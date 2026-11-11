import Foundation
import ServiceManagement
import Testing
@testable import NovaUI

@Test func clickRightAfterCloseDoesNotReopen() {
    let closed = Date(timeIntervalSince1970: 1000)
    #expect(!MenuBarPopover.shouldReopen(closedAt: closed, now: closed.addingTimeInterval(0.1)))
    #expect(MenuBarPopover.shouldReopen(closedAt: closed, now: closed.addingTimeInterval(0.3)))
    #expect(MenuBarPopover.shouldReopen(closedAt: .distantPast, now: closed))
}

@Test func requiresApprovalReadsNeedsApproval() {
    #expect(LoginItem.State.of(.requiresApproval) == .needsApproval)
    #expect(LoginItem.State.of(.enabled) == .enabled)
    #expect(LoginItem.State.of(.notRegistered) == .off)
    #expect(LoginItem.State.of(.notFound) == .off)
}
