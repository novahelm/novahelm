import SwiftUI

public struct NovaReadingTile: View {
    let icon: String
    let label: String
    let value: String
    let tint: Color
    let action: () -> Void
    @State private var hovering = false

    public init(icon: String, label: String, value: String, tint: Color, action: @escaping () -> Void) {
        self.icon = icon; self.label = label; self.value = value; self.tint = tint; self.action = action
    }

    public var body: some View {
        Button(action: action) {
            VStack(alignment: .leading, spacing: 6) {
                HStack(spacing: 6) {
                    Image(systemName: icon).font(.caption.weight(.semibold)).foregroundStyle(tint)
                    Text(label).font(.caption).foregroundStyle(.secondary)
                }
                Text(value).font(.system(.title3, design: .rounded).weight(.semibold)).monospacedDigit().lineLimit(1)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(10)
            .background(RoundedRectangle(cornerRadius: 12, style: .continuous).fill(Color.primary.opacity(hovering ? 0.09 : 0.05)))
            .scaleEffect(hovering ? 1.015 : 1)
            .contentShape(RoundedRectangle(cornerRadius: 12))
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
        .animation(.easeOut(duration: 0.12), value: hovering)
    }
}

public struct NovaIconButton: View {
    let systemName: String
    let help: String
    let action: () -> Void
    @State private var hovering = false

    public init(systemName: String, help: String, action: @escaping () -> Void) {
        self.systemName = systemName; self.help = help; self.action = action
    }

    public var body: some View {
        Button(action: action) {
            Image(systemName: systemName).font(.system(size: 13, weight: .medium))
                .frame(width: 26, height: 26)
                .background(Circle().fill(Color.primary.opacity(hovering ? 0.1 : 0)))
                .contentShape(Circle())
        }
        .buttonStyle(.plain).foregroundStyle(.secondary).help(help)
        .onHover { hovering = $0 }
    }
}
