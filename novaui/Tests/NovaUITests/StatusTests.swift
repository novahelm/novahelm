import SwiftUI
import Testing
@testable import NovaUI

@Test func statusColours() {
    #expect(NovaStatus.ok.color == .green)
    #expect(NovaStatus.warn.color == .orange)
    #expect(NovaStatus.bad.color == .red)
    #expect(NovaStatus.info.color == .blue)
    #expect(NovaStatus.idle.color == .gray)
    #expect(NovaStatus.unknown.color == .secondary)
}
