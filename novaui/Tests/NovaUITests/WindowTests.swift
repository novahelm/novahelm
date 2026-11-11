import AppKit
import Testing
@testable import NovaUI

@MainActor
@Test func specConfiguresTheWindow() {
    let spec = WindowSpec(title: "Manage Apps", styleMask: [.titled, .closable, .resizable],
                          contentSize: NSSize(width: 1040, height: 700), contentMinSize: NSSize(width: 960, height: 640))
    let window = NSWindow(contentRect: .zero, styleMask: [.titled], backing: .buffered, defer: true)
    spec.apply(to: window)
    #expect(window.title == "Manage Apps")
    #expect(window.styleMask == [.titled, .closable, .resizable])
    #expect(window.contentRect(forFrameRect: window.frame).size == NSSize(width: 1040, height: 700))
    #expect(window.contentMinSize == NSSize(width: 960, height: 640))
    #expect(!window.isReleasedWhenClosed)
    #expect(window.frameAutosaveName.isEmpty)
}

@MainActor
@Test func specSetsTheFrameMinimum() {
    let window = NSWindow(contentRect: .zero, styleMask: [.titled], backing: .buffered, defer: true)
    WindowSpec(title: "T", styleMask: [.titled], minSize: NSSize(width: 760, height: 520)).apply(to: window)
    #expect(window.minSize == NSSize(width: 760, height: 520))
}

@MainActor
@Test func specSavesTheFrameUnderItsName() {
    let name = "NovaUITests.\(UUID().uuidString)"
    defer { UserDefaults.standard.removeObject(forKey: "NSWindow Frame \(name)") }
    let window = NSWindow(contentRect: .zero, styleMask: [.titled], backing: .buffered, defer: true)
    WindowSpec(title: "T", styleMask: [.titled], autosaveName: name).apply(to: window)
    #expect(window.frameAutosaveName == name)
}

@MainActor
@Test func slotReusesTheOpenWindowAndRebuildsAfterClose() {
    var presented = 0
    let slot = WindowSlot(keepsClosedWindow: false) { _ in presented += 1 }
    var closes = 0
    slot.onClose = { closes += 1 }
    var built = 0
    let spec = WindowSpec(title: "T", styleMask: [.titled, .closable], contentSize: NSSize(width: 100, height: 80))
    let make = { () -> NSView in built += 1; return NSView() }
    let first = slot.show(spec, contentView: make)
    let again = slot.show(spec, contentView: make)
    #expect(first === again)
    #expect(built == 1 && presented == 2)
    slot.close()
    #expect(slot.window == nil && closes == 1)
    let rebuilt = slot.show(spec, contentView: make)
    #expect(rebuilt !== first && built == 2)
    rebuilt.close()
}

@MainActor
@Test func slotCanKeepAClosedWindow() {
    let slot = WindowSlot(keepsClosedWindow: true) { _ in }
    let spec = WindowSpec(title: "T", styleMask: [.titled, .closable])
    let first = slot.show(spec) { NSView() }
    slot.close()
    #expect(slot.window === first)
    #expect(slot.show(spec) { NSView() } === first)
    first.close()
}

@Test func alertResponseMapsToButtonIndex() {
    let first = NSApplication.ModalResponse.alertFirstButtonReturn.rawValue
    #expect(NovaAlert.index(of: .alertFirstButtonReturn, buttonCount: 2) == 0)
    #expect(NovaAlert.index(of: .alertSecondButtonReturn, buttonCount: 2) == 1)
    #expect(NovaAlert.index(of: .alertThirdButtonReturn, buttonCount: 2) == nil)
    #expect(NovaAlert.index(of: NSApplication.ModalResponse(first + 3), buttonCount: 4) == 3)
    #expect(NovaAlert.index(of: .abort, buttonCount: 2) == nil)
    #expect(NovaAlert.index(of: .stop, buttonCount: 2) == nil)
}

@MainActor
@Test func alertRolesSetKeysAndEnabledState() {
    let alert = NovaAlert(title: "Save?", message: "Body", buttons: [
        .init("Pair"), .init("Save", role: .primary, isEnabled: false), .init("Cancel", role: .cancel),
    ]).makeAlert()
    #expect(alert.messageText == "Save?" && alert.informativeText == "Body")
    #expect(alert.buttons.map(\.title) == ["Pair", "Save", "Cancel"])
    #expect(alert.buttons.map(\.keyEquivalent) == ["", "\r", "\u{1b}"])
    #expect(alert.buttons.map(\.isEnabled) == [true, false, true])
}
