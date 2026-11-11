import CoreGraphics
import Testing
@testable import NovaUI

@Test func fewerThanTwoValuesDrawNothing() {
    #expect(NovaSparkline.points([], in: CGSize(width: 48, height: 16)).isEmpty)
    #expect(NovaSparkline.points([3], in: CGSize(width: 48, height: 16)).isEmpty)
}
@Test func allZeroOrNegativeIsAFlatBaseline() {
    let pts = NovaSparkline.points([0, -2, 0], in: CGSize(width: 48, height: 16))
    #expect(pts.map(\.y) == [15.25, 15.25, 15.25])
    #expect(pts.allSatisfy { $0.x.isFinite && $0.y.isFinite })
}
@Test func scalesFromZeroAcrossTheWidth() {
    let pts = NovaSparkline.points([0, 5, 10], in: CGSize(width: 48, height: 16))
    #expect(pts.map(\.x) == [0, 24, 48])
    #expect(pts.last?.y == 0.75)
}
