import AppKit
import NovaLinkKit
import NovaUI

@MainActor
final class StatusItemController {
    private let store: Store
    private let menu: MenuBarPopover

    init(store: Store) {
        self.store = store
        menu = MenuBarPopover(autosaveName: "NovaLinkItem", followsContentSize: true,
                              content: PopoverView(store: store))
        menu.onShow = { store.popoverOpen = true }
        menu.onClose = { store.popoverOpen = false }
        store.onChange = { [weak self] in self?.updateIcon() }
        updateIcon()
    }

    private func updateIcon() {
        let icon = store.icon
        let description: String = switch icon {
        case .inControl: "NovaLink: this Mac is in control"
        case .remote: "NovaLink: another device is in control"
        case .disconnected: "NovaLink: not connected"
        case .problem: "NovaLink: needs attention"
        }
        let image = NSImage(systemSymbolName: icon.symbolName, accessibilityDescription: description)
        image?.isTemplate = true
        menu.item.button?.image = image
        menu.item.button?.setAccessibilityLabel(description)
    }

    func show() { menu.show() }
}
