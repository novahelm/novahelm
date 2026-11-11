import SwiftUI

/// A full-width inline message: success, warning or error, with optional trailing content and a dismiss button.
public struct NovaBanner<Trailing: View>: View {
    public enum Kind: Sendable { case success, warning, error }

    let kind: Kind
    let text: String
    let onDismiss: (() -> Void)?
    let trailing: Trailing

    public init(_ kind: Kind, _ text: String, onDismiss: (() -> Void)? = nil,
                @ViewBuilder trailing: () -> Trailing) {
        self.kind = kind; self.text = text; self.onDismiss = onDismiss; self.trailing = trailing()
    }

    private var icon: String {
        switch kind {
        case .success: "checkmark.circle.fill"
        case .warning: "exclamationmark.triangle.fill"
        case .error: "exclamationmark.octagon.fill"
        }
    }

    private var tint: Color {
        switch kind { case .success: .green; case .warning: .orange; case .error: .red }
    }

    public var body: some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: icon).foregroundStyle(tint)
            Text(text).font(.system(size: 12)).fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: 0)
            trailing
            if let onDismiss {
                Button(action: onDismiss) { Image(systemName: "xmark") }
                    .buttonStyle(.borderless)
                    .accessibilityLabel("Dismiss")
                    .help("Dismiss")
            }
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(RoundedRectangle(cornerRadius: 10).fill(tint.opacity(0.12)))
    }
}

extension NovaBanner where Trailing == EmptyView {
    public init(_ kind: Kind, _ text: String, onDismiss: (() -> Void)? = nil) {
        self.init(kind, text, onDismiss: onDismiss, trailing: { EmptyView() })
    }
}
