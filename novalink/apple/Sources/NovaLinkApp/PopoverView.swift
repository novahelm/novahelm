import AppKit
import NovaLinkKit
import NovaUI
import SwiftUI

private typealias Edge = NovaLinkKit.Edge

private extension HeaderTone {
    var status: NovaStatus {
        switch self {
        case .ok: .ok
        case .info: .info
        case .idle: .idle
        case .bad: .bad
        }
    }
}

struct PopoverView: View {
    let store: Store
    @State private var showDiagnostics = false
    @State private var showingSettings = false

    var body: some View {
        VStack(alignment: .leading, spacing: NovaPopover.spacing) {
            if let status = store.status {
                NovaStatusHeader(status: store.icon.tone.status, symbol: store.icon.symbolName,
                                 title: MenuText.header(status), detail: MenuText.headerDetail(status))
            }
            if let banner = store.banner {
                NovaBanner(banner.isError ? .error : .success, banner.text, onDismiss: { store.banner = nil })
            }
            if let status = store.status {
                StaleNote(reason: store.poll.stale[.status])
                if status.captureState != .running { PermissionsCard(store: store) }
                if !store.pending.isEmpty { PairingRequestsCard(store: store) }
                DevicesCard(store: store, status: status)
                PairNewCard(store: store)
                if showingSettings {
                    SettingsCard(store: store)
                } else {
                    ActionsCard(store: store, showDiagnostics: $showDiagnostics)
                }
            } else {
                OfflineView(store: store)
                if showingSettings { SettingsCard(store: store) }
            }
            footer
        }
        .padding(NovaPopover.padding)
        .frame(width: NovaPopover.standardWidth)
        .animation(.snappy(duration: 0.2), value: showingSettings)
        .sheet(isPresented: $showDiagnostics) {
            DiagnosticsView(checks: store.checks, stale: store.poll.stale[.checks])
        }
    }

    private var footer: some View {
        NovaFooter(quitHelp: "Quit the NovaLink menu (sharing keeps running)", onQuit: { NSApp.terminate(nil) }) {
            if let t = store.lastUpdated {
                Text("Updated \(t.formatted(date: .omitted, time: .shortened))")
                    .font(.caption).foregroundStyle(.tertiary)
            }
        } settings: {
            NovaIconButton(systemName: showingSettings ? "checkmark.circle.fill" : "slider.horizontal.3",
                           help: showingSettings ? "Done" : "Settings") { showingSettings.toggle() }
        }
    }
}

/// Says a section shows an older value because its latest fetch failed.
private struct StaleNote: View {
    let reason: String?

    var body: some View {
        if let reason { NovaBanner(.warning, MenuText.staleNote(reason)) }
    }
}

/// Shown when the agent doesn't answer.
private struct OfflineView: View {
    let store: Store

    var body: some View {
        VStack(alignment: .leading, spacing: NovaPopover.spacing) {
            NovaStatusHeader(status: tone, symbol: store.icon.symbolName, title: title, detail: nil)
            VStack(alignment: .leading, spacing: 8) {
                Text(advice).font(.callout).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                if showTurnOn {
                    Button("Turn On NovaLink") { Task { await store.turnOn() } }
                        .buttonStyle(.borderedProminent).controlSize(.small)
                }
            }
            .padding(12).frame(maxWidth: .infinity, alignment: .leading).groupCard()
        }
    }

    private var turnedOff: Bool { store.agent.state == .notRegistered }

    private var tone: NovaStatus {
        switch store.agent.state {
        case .notFound, .requiresApproval: return .bad
        default: break
        }
        switch store.connectionError {
        case .impostor?: return .bad
        case .notRunning?, nil: return .idle
        default: return .bad
        }
    }

    private var showTurnOn: Bool {
        switch store.connectionError {
        case .impostor?, .tokenMissing?: false
        default: store.agent.state != .notFound
        }
    }

    private var title: String {
        switch store.agent.state {
        case .notFound: return "NovaLink's background helper is missing"
        case .requiresApproval: return "NovaLink needs your approval"
        default: break
        }
        switch store.connectionError {
        case .impostor?: return "Something else is using NovaLink's port"
        case .notRunning?, nil: return turnedOff ? "NovaLink is turned off" : "NovaLink isn't running"
        default: return "Can't reach NovaLink"
        }
    }

    private var advice: String {
        switch store.agent.state {
        case .notFound: return "This copy of the app is incomplete. Reinstall NovaLink from the disk image."
        case .requiresApproval:
            return "Allow NovaLink under System Settings > General > Login Items, then choose Turn On NovaLink."
        default: break
        }
        switch store.connectionError {
        case .impostor?:
            return "Another program is listening on port 7979, so NovaLink won't talk to it. Close that program, then quit and reopen this menu."
        case .notRunning?, nil:
            return turnedOff ? "Choose Turn On NovaLink to share your keyboard and mouse."
                : "Choose Turn On NovaLink to start it again."
        case .some(let e): return describe(e)
        }
    }
}

private struct PermissionsCard: View {
    let store: Store

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("NovaLink needs two permissions to share your keyboard and mouse.")
                .font(.callout).fixedSize(horizontal: false, vertical: true).padding(.horizontal, 8).padding(.top, 4)
            row("Accessibility", check: "accessibility", pane: "Privacy_Accessibility")
            row("Input Monitoring", check: "input_monitoring", pane: "Privacy_ListenEvent")
            Button("Request Access") { Task { await store.requestPermissions() } }
                .buttonStyle(.borderedProminent).controlSize(.small).padding(.horizontal, 8).padding(.bottom, 4)
        }
        .padding(4).frame(maxWidth: .infinity, alignment: .leading).groupCard()
    }

    private func row(_ title: String, check: String, pane: String) -> some View {
        let ok = store.checks.first { $0.name == check }?.ok
        let tint: Color = ok == true ? .green : ok == false ? .red : .gray
        let icon = ok == true ? "checkmark" : ok == false ? "xmark" : "questionmark"
        let state = ok == true ? "Allowed" : ok == false ? "Not allowed" : "Unknown"
        return NovaRow(icon: icon, tint: tint, title: title, detail: state) {
            Button("Open Settings") {
                NSWorkspace.shared.open(URL(string: "x-apple.systempreferences:com.apple.preference.security?\(pane)")!)
            }
            .buttonStyle(.link).font(.caption)
        }
        .padding(.horizontal, 8).padding(.vertical, 6)
        .accessibilityElement(children: .contain)
    }
}

private struct PairingRequestsCard: View {
    let store: Store

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            StaleNote(reason: store.poll.stale[.pending])
            ForEach(store.pending, id: \.peer) { p in
                NovaRow(icon: "person.badge.plus", tint: .orange,
                        title: MenuText.pairingTitle(p), detail: MenuText.pairingDetail(p)) {
                    HStack(spacing: 6) {
                        Button("Pair") { Task { await store.answer(p, accept: true) } }
                            .buttonStyle(.borderedProminent)
                        Button("Reject") { Task { await store.answer(p, accept: false) } }
                    }
                    .controlSize(.small)
                }
            }
        }
        .padding(10).frame(maxWidth: .infinity, alignment: .leading)
        .background(RoundedRectangle(cornerRadius: 12, style: .continuous).fill(Color.orange.opacity(0.12)))
    }
}

private struct DevicesCard: View {
    let store: Store
    let status: AgentStatus
    @State private var unpairing: Peer?

    var body: some View {
        if !status.peers.isEmpty {
            VStack(alignment: .leading, spacing: 4) {
                Text("Devices").font(.caption).foregroundStyle(.secondary).padding(.horizontal, 6)
                VStack(spacing: 2) {
                    let names = MenuText.displayNames(status.peers)
                    ForEach(Array(zip(status.peers, names)), id: \.0.id) { peer, name in
                        Menu {
                            Picker("Position", selection: position(peer, name)) {
                                Text("Not set").tag(nil as Edge?)
                                ForEach(Edge.allCases, id: \.self) { e in
                                    Text(MenuText.positionLabel(e)).tag(e as Edge?)
                                }
                            }
                            .pickerStyle(.inline)
                            Divider()
                            Button("Unpair…") { unpairing = peer }
                        } label: {
                            NovaRow(icon: peer.connected ? "desktopcomputer" : "desktopcomputer.slash",
                                    tint: peer.connected ? .green : .gray, title: name,
                                    detail: MenuText.deviceDetail(peer, position: status.position(of: peer.id)),
                                    chevron: true)
                                .padding(.horizontal, 8).padding(.vertical, 6)
                                .opacity(peer.connected ? 1 : 0.55)
                        }
                        .menuStyle(.button).buttonStyle(.plain).menuIndicator(.hidden)
                    }
                }
                .padding(4).groupCard()
            }
            .confirmationDialog(
                "Unpair \(unpairing.map { name(of: $0) } ?? "this device")?",
                isPresented: Binding(get: { unpairing != nil }, set: { if !$0 { unpairing = nil } }),
                titleVisibility: .visible, presenting: unpairing
            ) { peer in
                Button("Unpair", role: .destructive) { Task { await store.unpair(peer, name: name(of: peer)) } }
            } message: { _ in
                Text("You can pair it again later.")
            }
        }
    }

    private func name(of peer: Peer) -> String {
        let names = MenuText.displayNames(status.peers)
        return zip(status.peers, names).first { $0.0.id == peer.id }?.1 ?? peer.name
    }

    private func position(_ peer: Peer, _ name: String) -> Binding<Edge?> {
        Binding(
            get: { status.position(of: peer.id) },
            set: { if let e = $0 { Task { await store.setPosition(e, of: peer, name: name) } } })
    }
}

private struct PairNewCard: View {
    let store: Store

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("Pair a new device").font(.caption).foregroundStyle(.secondary).padding(.horizontal, 6)
            StaleNote(reason: store.poll.stale[.discovered])
            VStack(spacing: 2) {
                if store.discovered.isEmpty {
                    Text("Looking for devices on your network…").font(.callout).foregroundStyle(.secondary)
                        .padding(.horizontal, 8).padding(.vertical, 6).frame(maxWidth: .infinity, alignment: .leading)
                } else {
                    let names = MenuText.displayNames(store.discovered.map {
                        Peer(id: $0.id, name: $0.name, connected: false, rttUs: nil, path: "", dryInject: nil)
                    })
                    ForEach(Array(zip(store.discovered, names)), id: \.0.id) { d, name in
                        NovaRow(icon: "desktopcomputer", tint: .blue, title: name, detail: "Found on your network") {
                            if store.pairing.contains(d.id) {
                                ProgressView().controlSize(.small)
                            } else {
                                Button("Pair") { Task { await store.pair(d, name: name) } }.controlSize(.small)
                            }
                        }
                        .padding(.horizontal, 8).padding(.vertical, 6)
                    }
                }
            }
            .padding(4).groupCard()
        }
    }
}

private struct ActionsCard: View {
    let store: Store
    @Binding var showDiagnostics: Bool

    var body: some View {
        VStack(spacing: 2) {
            action("stethoscope", "Diagnostics", .teal, MenuText.diagnosticsSummary(store.checks)) { showDiagnostics = true }
            action("doc.text.magnifyingglass", "Open logs", .gray, nil) { store.openLogs() }
            action("terminal", "Install command-line tool…", .indigo, nil) { Task { await store.installCommandLineTool() } }
        }
        .padding(4).groupCard()
    }

    private func action(_ icon: String, _ title: String, _ tint: Color, _ detail: String?,
                        _ run: @escaping () -> Void) -> some View {
        Button(action: run) {
            NovaRow(icon: icon, tint: tint, title: title, detail: detail, chevron: true)
        }
        .buttonStyle(HoverRowStyle())
    }
}

private struct SettingsCard: View {
    let store: Store

    var body: some View {
        VStack(spacing: 0) {
            // Needs the agent's answer, so it stays visible but disabled while offline.
            let settings = store.status?.settings
            Toggle(isOn: Binding(get: { settings?.swapCtrlCmd ?? false },
                                 set: { on in Task { await store.setSwapCtrlCmd(on) } })) {
                Label("Swap Ctrl and Cmd for other keyboards", systemImage: "command")
            }
            .toggleStyle(.switch).controlSize(.mini).padding(.horizontal, 8).padding(.vertical, 5)
            .disabled(settings == nil)
            .help("Keys typed on another computer's keyboard reach this Mac with Ctrl and Cmd (Windows key) swapped, so Ctrl+C copies.")
            Toggle(isOn: Binding(get: { store.loginItemEnabled }, set: { store.setStartAtLogin($0) })) {
                Label("Start at login", systemImage: "power.circle")
            }
            .toggleStyle(.switch).controlSize(.mini).padding(.horizontal, 8).padding(.vertical, 5)
            if store.status == nil {
                Button { store.openLogs() } label: {
                    NovaRow(icon: "doc.text.magnifyingglass", tint: .gray, title: "Open logs", detail: nil)
                }
                .buttonStyle(HoverRowStyle())
                Button { Task { await store.installCommandLineTool() } } label: {
                    NovaRow(icon: "terminal", tint: .indigo, title: "Install command-line tool…", detail: nil)
                }
                .buttonStyle(HoverRowStyle())
            }
            Button {
                Task { if store.agent.state == .notRegistered { await store.turnOn() } else { await store.turnOff() } }
            } label: {
                NovaRow(icon: "power", tint: store.agent.state == .notRegistered ? .green : .red,
                        title: store.agent.state == .notRegistered ? "Turn on NovaLink" : "Turn off NovaLink", detail: nil)
            }
            .buttonStyle(HoverRowStyle())
        }
        .padding(4).groupCard()
    }
}

private struct DiagnosticsView: View {
    let checks: [Check]
    let stale: String?
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("Diagnostics").font(.headline)
            StaleNote(reason: stale)
            if checks.isEmpty {
                Text("No results yet. NovaLink isn't answering, so there is nothing to check.").font(.callout)
            }
            ForEach(checks, id: \.name) { c in
                HStack(alignment: .top, spacing: 8) {
                    Image(systemName: c.ok ? "checkmark.circle.fill" : "xmark.octagon.fill")
                        .foregroundStyle((c.ok ? NovaStatus.ok : .bad).color)
                        .frame(width: 18)
                        .accessibilityLabel(c.ok ? "Passed" : "Failed")
                    VStack(alignment: .leading, spacing: 2) {
                        Text(c.name.replacingOccurrences(of: "_", with: " ").capitalized).font(.callout.weight(.medium))
                        Text(c.detail).font(.caption).foregroundStyle(.secondary)
                        if !c.ok && !c.advice.isEmpty { Text(c.advice).font(.caption) }
                    }
                    .fixedSize(horizontal: false, vertical: true)
                }
            }
            HStack { Spacer(); Button("Done") { dismiss() }.keyboardShortcut(.defaultAction) }
        }
        .padding(16)
        .frame(width: 320)
    }
}
