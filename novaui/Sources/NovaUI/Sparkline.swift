import SwiftUI

/// A small trend line over a faint fill, scaled from zero so small wobbles stay small.
/// A flat line along the bottom when every value is zero; empty with fewer than two points.
public struct NovaSparkline: View {
    let values: [Double]
    var color: Color = .secondary

    public init(values: [Double], color: Color = .secondary) {
        self.values = values; self.color = color
    }

    public var body: some View {
        GeometryReader { geo in
            let points = Self.points(values, in: geo.size)
            ZStack {
                if let first = points.first, let last = points.last {
                    Path { path in
                        path.move(to: CGPoint(x: first.x, y: geo.size.height))
                        points.forEach { path.addLine(to: $0) }
                        path.addLine(to: CGPoint(x: last.x, y: geo.size.height))
                        path.closeSubpath()
                    }
                    .fill(color.opacity(0.14))
                    Path { path in path.addLines(points) }
                        .stroke(color, style: StrokeStyle(lineWidth: 1.25, lineCap: .round, lineJoin: .round))
                }
            }
        }
        .accessibilityHidden(true)
    }

    nonisolated static func points(_ values: [Double], in size: CGSize) -> [CGPoint] {
        guard values.count >= 2 else { return [] }
        let hi = max(values.max() ?? 0, 0)
        let usable = size.height - 1.5
        return values.enumerated().map { i, v in
            let x = size.width * Double(i) / Double(values.count - 1)
            let y = hi > 0 ? usable * (1 - max(v, 0) / hi) + 0.75 : usable + 0.75
            return CGPoint(x: x, y: y)
        }
    }
}
