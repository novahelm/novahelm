// swift-tools-version: 6.0
import PackageDescription

// NovaGear's Mac menu bar app, in the same shape as NovaLink's (novalink/apple): a kit with
// the agent client and the text the menu shows, and the app around it, drawn with the shared
// NovaUI building blocks like the other Nova menus.
let package = Package(
    name: "NovaGear",
    platforms: [.macOS(.v15)],
    products: [
        .library(name: "NovaGearKit", targets: ["NovaGearKit"]),
        .executable(name: "NovaGearApp", targets: ["NovaGearApp"]),
    ],
    dependencies: [
        .package(path: "../../novaui"),
    ],
    targets: [
        .target(name: "NovaGearKit"),
        .executableTarget(name: "NovaGearApp", dependencies: ["NovaGearKit", .product(name: "NovaUI", package: "novaui")]),
        .testTarget(name: "NovaGearKitTests", dependencies: ["NovaGearKit"]),
    ]
)
