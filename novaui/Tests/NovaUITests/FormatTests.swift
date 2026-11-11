import Testing
@testable import NovaUI

@Test func compactBytesPicksTheUnit() {
    #expect(compactBytes(212_000_000_000) == "212 GB")
    #expect(compactBytes(9_200_000_000) == "9.2 GB")
    #expect(compactBytes(500_000_000) == "500 MB")
}
