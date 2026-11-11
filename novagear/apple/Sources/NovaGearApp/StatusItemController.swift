import AppKit
import NovaGearKit
import NovaUI

/// The menu bar item and its popover, on the shared NovaUI plumbing like NovaLink's.
@MainActor
final class StatusItemController {
    private let store: Store
    private let menu: MenuBarPopover

    init(store: Store) {
        self.store = store
        menu = MenuBarPopover(autosaveName: "NovaGearItem", followsContentSize: true, content: PopoverView(store: store))
        menu.onShow = { store.refresh() }
        store.onChange = { [weak self] in self?.updateIcon() }
        updateIcon()
    }

    private func updateIcon() {
        let on = store.snapshot.map(MenuText.lightsOn) ?? false
        let description = store.snapshot == nil ? "NovaGear: agent unavailable" : (on ? "NovaGear: lights on" : "NovaGear")
        let image = NSImage(systemSymbolName: on ? "lightbulb.led.fill" : "lightbulb.led", accessibilityDescription: description)
        image?.isTemplate = true
        menu.item.button?.image = image
        menu.item.button?.setAccessibilityLabel(description)
    }
}
