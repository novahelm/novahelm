// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "NovaLink",
    platforms: [.macOS(.v15)],
    products: [
        .library(name: "NovaLinkKit", targets: ["NovaLinkKit"]),
        .executable(name: "NovaLinkApp", targets: ["NovaLinkApp"]),
    ],
    dependencies: [
        .package(path: "../../novaui"),
    ],
    targets: [
        .target(name: "NovaLinkKit"),
        .executableTarget(name: "NovaLinkApp", dependencies: ["NovaLinkKit", .product(name: "NovaUI", package: "novaui")]),
        .testTarget(name: "NovaLinkKitTests", dependencies: ["NovaLinkKit"],
                    // Read by path from the source tree; written by crates/client/tests/golden.rs.
                    exclude: ["Fixtures"]),
    ]
)
