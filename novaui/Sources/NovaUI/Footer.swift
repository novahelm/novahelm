import SwiftUI

/// The popover's bottom row: leading status text, a spacer, settings controls and the quit button.
public struct NovaFooter<Leading: View, Settings: View>: View {
    let quitHelp: String
    let onQuit: () -> Void
    let leading: Leading
    let settings: Settings

    public init(quitHelp: String, onQuit: @escaping () -> Void,
                @ViewBuilder leading: () -> Leading, @ViewBuilder settings: () -> Settings) {
        self.quitHelp = quitHelp; self.onQuit = onQuit; self.leading = leading(); self.settings = settings()
    }

    public var body: some View {
        HStack(spacing: 6) {
            leading
            Spacer()
            settings
            NovaIconButton(systemName: "power", help: quitHelp, action: onQuit)
        }
    }
}

/// Shared popover metrics. NovaHelm keeps its own wider width.
public enum NovaPopover {
    public static let padding: CGFloat = 14
    public static let spacing: CGFloat = 12
    public static let standardWidth: CGFloat = 360
}
