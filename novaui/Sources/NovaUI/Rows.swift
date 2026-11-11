import SwiftUI

/// An icon tile, a title with optional detail, an optional trailing view and an optional chevron.
public struct NovaRow<Trailing: View>: View {
    let icon: String
    let tint: Color
    let title: String
    let detail: String?
    var chevron: Bool
    let trailing: Trailing

    public init(icon: String, tint: Color, title: String, detail: String?, chevron: Bool = false,
                @ViewBuilder trailing: () -> Trailing) {
        self.icon = icon; self.tint = tint; self.title = title; self.detail = detail
        self.chevron = chevron; self.trailing = trailing()
    }

    public var body: some View {
        HStack(spacing: 10) {
            Image(systemName: icon).font(.system(size: 12, weight: .semibold)).foregroundStyle(.white)
                .frame(width: 24, height: 24).background(tint.gradient, in: RoundedRectangle(cornerRadius: 6, style: .continuous))
            VStack(alignment: .leading, spacing: 1) {
                Text(title).font(.callout).lineLimit(1)
                if let detail { Text(detail).font(.caption).foregroundStyle(.secondary).lineLimit(2) }
            }
            Spacer(minLength: 0)
            trailing
            if chevron { Image(systemName: "chevron.right").font(.caption.weight(.semibold)).foregroundStyle(.tertiary) }
        }
        .contentShape(Rectangle())
    }
}

public extension NovaRow where Trailing == EmptyView {
    init(icon: String, tint: Color, title: String, detail: String?, chevron: Bool = false) {
        self.init(icon: icon, tint: tint, title: title, detail: detail, chevron: chevron) { EmptyView() }
    }
}

/// A full-width row that highlights on hover and darkens when pressed, like macOS menus and Control Center.
public struct HoverRowStyle: ButtonStyle {
    public init() {}

    public func makeBody(configuration: Configuration) -> some View { HoverRow(configuration: configuration) }

    private struct HoverRow: View {
        let configuration: Configuration
        @State private var hovering = false
        var body: some View {
            configuration.label
                .padding(.horizontal, 8).padding(.vertical, 6)
                .background(RoundedRectangle(cornerRadius: 8, style: .continuous)
                    .fill(Color.primary.opacity(configuration.isPressed ? 0.14 : (hovering ? 0.07 : 0))))
                .onHover { hovering = $0 }
                .animation(.easeOut(duration: 0.12), value: hovering)
        }
    }
}

public extension View {
    func groupCard() -> some View {
        background(RoundedRectangle(cornerRadius: 12, style: .continuous).fill(Color.primary.opacity(0.05)))
    }
}
