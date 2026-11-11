import SwiftUI

/// The popover's top row: a tinted circle with a symbol, a headline, an optional detail line and an accessory.
public struct NovaStatusHeader<Detail: View, Accessory: View>: View {
    let status: NovaStatus
    let symbol: String
    let title: String
    let detail: Detail
    let accessory: Accessory

    public init(status: NovaStatus, symbol: String, title: String,
                @ViewBuilder detail: () -> Detail, @ViewBuilder accessory: () -> Accessory) {
        self.status = status; self.symbol = symbol; self.title = title
        self.detail = detail(); self.accessory = accessory()
    }

    public var body: some View {
        HStack(spacing: 12) {
            ZStack {
                Circle().fill(status.color.opacity(0.18)).frame(width: 40, height: 40)
                Image(systemName: symbol).font(.system(size: 19, weight: .semibold)).foregroundStyle(status.color)
            }
            .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(.headline)
                detail
            }
            Spacer(minLength: 0)
            accessory
        }
    }
}

extension NovaStatusHeader where Detail == NovaHeaderDetailText {
    public init(status: NovaStatus, symbol: String, title: String, detail: String?,
                @ViewBuilder accessory: () -> Accessory) {
        self.init(status: status, symbol: symbol, title: title,
                  detail: { NovaHeaderDetailText(text: detail) }, accessory: accessory)
    }
}

extension NovaStatusHeader where Detail == NovaHeaderDetailText, Accessory == EmptyView {
    public init(status: NovaStatus, symbol: String, title: String, detail: String?) {
        self.init(status: status, symbol: symbol, title: title, detail: detail, accessory: { EmptyView() })
    }
}

/// The header's string detail line: caption, secondary, two lines at most; nothing when the text is nil.
public struct NovaHeaderDetailText: View {
    let text: String?

    public var body: some View {
        if let text {
            Text(text).font(.caption).foregroundStyle(.secondary).lineLimit(2)
        }
    }
}
