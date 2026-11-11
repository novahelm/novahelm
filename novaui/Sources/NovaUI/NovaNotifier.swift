import Foundation
import os
import UserNotifications

/// Posts alerts through UNUserNotificationCenter. Needs an app bundle; a bare executable (such as a test run) crashes,
/// so it isn't exercised in unit tests. It never sets the center's delegate: an app that wants banners while it is
/// frontmost, or wants to handle clicks, sets its own.
@MainActor
public final class NovaNotifier {
    public enum Sound { case none, standard, critical }

    private nonisolated let log: Logger

    public init(subsystem: String) {
        log = Logger(subsystem: subsystem, category: "notifications")
    }

    public func requestAuthorization() async -> Bool {
        (try? await UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound])) ?? false
    }

    /// A repeat of the same `id` replaces the earlier notification instead of stacking.
    public func post(id: String, title: String, body: String, timeSensitive: Bool,
                     sound: Sound = .none, userInfo: [String: String] = [:]) {
        let content = UNMutableNotificationContent()
        content.title = title
        content.body = body
        switch sound {
        case .none: break
        case .standard: content.sound = .default
        case .critical: content.sound = .defaultCritical
        }
        content.interruptionLevel = timeSensitive ? .timeSensitive : .active
        content.userInfo = userInfo
        let log = self.log
        UNUserNotificationCenter.current().add(UNNotificationRequest(identifier: id, content: content, trigger: nil)) { error in
            if let error {
                log.error("Failed to post notification \(id, privacy: .public): \(error.localizedDescription, privacy: .public)")
            }
        }
    }
}
