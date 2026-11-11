// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "NovaUI",
    platforms: [.macOS(.v15)],
    products: [
        .library(name: "NovaUI", targets: ["NovaUI"]),
    ],
    targets: [
        // Shared SwiftUI building blocks for the NovaGuard, NovaLink and NovaHelm menu-bar apps.
        .target(
            name: "NovaUI",
            linkerSettings: [
                .linkedFramework("AppKit"),
                .linkedFramework("ServiceManagement"),
                .linkedFramework("UserNotifications"),
            ]
        ),
        .testTarget(name: "NovaUITests", dependencies: ["NovaUI"]),
    ]
)
