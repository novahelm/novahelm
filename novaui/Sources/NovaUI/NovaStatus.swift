import SwiftUI

/// The shared status palette. `.unknown` is "no verdict" and draws secondary; `.idle` is a grey dot.
public enum NovaStatus: Sendable, Equatable {
    case ok, warn, bad, info, idle, unknown

    public var color: Color {
        switch self {
        case .ok: .green
        case .warn: .orange
        case .bad: .red
        case .info: .blue
        case .idle: .gray
        case .unknown: .secondary
        }
    }
}
