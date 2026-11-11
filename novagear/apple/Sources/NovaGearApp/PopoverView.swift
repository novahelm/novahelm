import NovaGearKit
import NovaUI
import SwiftUI

// The menu bar popover, laid out like NovaGuard's: a status header, grouped cards, value tiles
// and quick actions, all from the shared NovaUI package.

struct PopoverView: View {
    let store: Store

    var body: some View {
        VStack(alignment: .leading, spacing: NovaPopover.spacing) {
            StatusHeader(store: store)
            if let s = store.snapshot {
                ModeCard(store: store, status: s.status)
                LightsCard(store: store, snapshot: s)
                tiles(s)
                quickActions
            } else {
                problemCard
            }
            if store.snapshot != nil, let p = store.problem {
                Text(p).font(.caption).foregroundStyle(.orange).fixedSize(horizontal: false, vertical: true)
            }
            footer
        }
        .padding(NovaPopover.padding)
        .frame(width: NovaPopover.standardWidth)
    }

    private var problemCard: some View {
        VStack(alignment: .leading, spacing: 8) {
            if let p = store.problem {
                Text(p).font(.callout).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                Button("Start agent") { store.ensureAgent() }.buttonStyle(.borderedProminent).controlSize(.small)
            } else {
                HStack { ProgressView().controlSize(.small); Text("Connecting to the agent…").foregroundStyle(.secondary) }
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading).padding(12).groupCard()
    }

    private func tiles(_ s: Snapshot) -> some View {
        LazyVGrid(columns: [GridItem(.flexible(), spacing: 8), GridItem(.flexible(), spacing: 8)], spacing: 8) {
            NovaReadingTile(icon: s.prompterPlaying ? "play.fill" : "text.alignleft", label: "Prompter",
                        value: MenuText.prompterValue(s), tint: .blue) { store.prompterPlayPause() }
                .help("Play or pause the prompter")
            NovaReadingTile(icon: "square.grid.2x2.fill", label: "Stream Deck", value: MenuText.deckValue(s),
                        tint: deckTint(s)) { store.toggleDeckControl() }
                .help(s.deckControl ? "Hand the Stream Deck back" : "Let NovaGear drive the Stream Deck")
            NovaReadingTile(icon: "computermouse.fill", label: "Mouse", value: MenuText.mouseValue(s), tint: .green) {
                store.nextDpiStage()
            }
            .help("Next DPI stage")
            NovaReadingTile(icon: "video.fill", label: "Capture",
                        value: "\(s.devices.filter { $0.kind == .capture }.count) connected", tint: .pink) { store.open() }
                .help("Open the panel to preview")
        }
    }

    private func deckTint(_ s: Snapshot) -> Color {
        switch MenuText.deck(s)?.status {
        case .busy?: .orange
        case nil: .gray
        default: .indigo
        }
    }

    private var quickActions: some View {
        VStack(spacing: 2) {
            if store.launchedAgent && store.snapshot?.status.networkBlocked == true {
                action("network", "Allow Local Network", .orange) { store.allowLocalNetwork() }
                    .help("macOS is blocking local network access for NovaGear, so it can't reach the lights")
            }
            if let n = store.snapshot?.status.firmwareUpdates, n > 0 {
                action("arrow.down.circle", "\(n) firmware update\(n == 1 ? "" : "s") available", .blue) {
                    store.open(section: "devices")
                }
            }
            if !store.keysAllowed {
                action("keyboard", "Allow keys and typing", .orange) { store.allowKeys() }
                    .help("Deck keys that press hotkeys or type text need Accessibility permission")
            }
            action("square.grid.2x2", "Open NovaGear panel", .blue) { store.open() }
            action("text.alignleft", "Open prompter window", .teal) { store.open("prompter") }
            action("backward.end.fill", "Restart prompter", .gray) { store.prompterRestart() }
        }.padding(4).groupCard()
    }

    private func action(_ icon: String, _ title: String, _ tint: Color, _ run: @escaping () -> Void) -> some View {
        Button(action: run) { NovaRow(icon: icon, tint: tint, title: title, detail: nil, chevron: true) }
            .buttonStyle(HoverRowStyle())
    }

    private var footer: some View {
        NovaFooter(quitHelp: "Quit NovaGear (stops the agent it started)", onQuit: { store.quit() }) {
            if let t = store.lastUpdated {
                Text("Updated \(t.formatted(date: .omitted, time: .shortened))").font(.caption).foregroundStyle(.tertiary)
            }
        } settings: {
            NovaIconButton(systemName: "doc.text", help: "Open logs") { store.open(section: "logs") }
            Toggle(isOn: Binding(get: { store.loginItemEnabled }, set: { store.setStartAtLogin($0) })) {
                Text("Open at login").font(.caption)
            }
            .toggleStyle(.switch).controlSize(.mini)
        }
    }
}

struct StatusHeader: View {
    let store: Store

    var body: some View {
        NovaStatusHeader(
            status: store.snapshot == nil ? .unknown : (lightsOn ? .warn : .idle),
            symbol: lightsOn ? "lightbulb.led.fill" : "lightbulb.led",
            title: store.snapshot.map(MenuText.header) ?? (store.problem == nil ? "Connecting…" : "Agent unavailable"),
            detail: store.snapshot.map(MenuText.headerDetail))
    }

    private var lightsOn: Bool { store.snapshot.map(MenuText.lightsOn) ?? false }
}

/// Like Control Center's display card: a toggle row and two sliders for all lights at once.
struct LightsCard: View {
    let store: Store
    let snapshot: Snapshot
    @State private var brightness: Double = 50
    @State private var kelvin: Double = 5000
    @State private var editing = false

    var body: some View {
        let lights = MenuText.lights(snapshot)
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                NovaRow(icon: "lightbulb.fill", tint: .orange, title: lights.isEmpty ? "No lights found" : "Key Lights",
                         detail: lights.isEmpty ? "Looking on your network…" : "\(lights.count) on your network")
                Toggle("", isOn: Binding(get: { MenuText.lightsOn(snapshot) }, set: { _ in store.toggleLights() }))
                    .toggleStyle(.switch).labelsHidden().disabled(lights.isEmpty)
            }
            slider("sun.min", "sun.max", value: $brightness, in: 3...100) { store.setLights(brightness: Int(brightness)) }
            slider("flame", "snowflake", value: $kelvin, in: 2900...7000) { store.setLights(kelvin: Int(kelvin)) }
        }
        .padding(12).groupCard()
        .disabled(lights.isEmpty)
        .onAppear(perform: sync)
        .onChange(of: snapshot) { if !editing { sync() } }
    }

    private func sync() {
        if let b = MenuText.average(snapshot, \.brightness) { brightness = Double(b) }
        if let k = MenuText.average(snapshot, \.kelvin) { kelvin = Double(k) }
    }

    private func slider(_ low: String, _ high: String, value: Binding<Double>, in range: ClosedRange<Double>,
                        send: @escaping () -> Void) -> some View {
        HStack(spacing: 8) {
            Image(systemName: low).font(.caption).foregroundStyle(.secondary).frame(width: 16)
            Slider(value: value, in: range) { editing = $0 }
                .controlSize(.small)
                .onChange(of: value.wrappedValue) { if editing { send() } }
            Image(systemName: high).font(.caption).foregroundStyle(.secondary).frame(width: 16)
        }
    }
}

/// The current mode with a menu to switch, and the approvals waiting. A request can only be
/// denied here; approving happens on the deck.
struct ModeCard: View {
    let store: Store
    let status: AgentStatus

    var body: some View {
        if status.mode != nil || status.approvals != nil {
            VStack(alignment: .leading, spacing: 10) {
                if let mode = status.mode {
                    HStack {
                        NovaRow(icon: "square.grid.2x2.fill", tint: .indigo, title: "Mode", detail: nil)
                        Spacer()
                        Menu {
                            ForEach(status.modes) { m in
                                Button { store.goToMode(m.id) } label: {
                                    if m.id == mode.id { Label { Text(verbatim: m.name) } icon: { Image(systemName: "checkmark") } }
                                    else { Text(verbatim: m.name) }
                                }
                            }
                        } label: { Text(verbatim: mode.name) }
                        .fixedSize().disabled(status.modes.count < 2)
                    }
                }
                if let ap = status.approvals { approvals(ap) }
            }
            .padding(12).groupCard()
        }
    }

    private func approvals(_ ap: ApprovalsInfo) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            NovaRow(icon: "checkmark.shield", tint: ap.pending > 0 ? .orange : .gray,
                    title: MenuText.approvalsLabel(ap.pending), detail: nil)
            if let top = ap.top {
                // Written by other programs: verbatim, so markup or links in them stay text.
                Text(verbatim: top.title).font(.callout).lineLimit(3).fixedSize(horizontal: false, vertical: true)
                HStack {
                    Text(verbatim: top.source).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                    Spacer()
                    Button("Deny", role: .destructive) { store.denyApproval(top.id) }
                        .controlSize(.small).help("Deny this request (approve it on the Stream Deck)")
                }
            }
        }
    }
}
