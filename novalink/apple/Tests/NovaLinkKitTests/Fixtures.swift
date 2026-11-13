import Foundation

/// JSON captured from the real agent router by crates/client/tests/golden.rs.
func fixture(_ name: String) -> Data {
    let url = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
        .appendingPathComponent("Fixtures").appendingPathComponent(name)
    return try! Data(contentsOf: url)
}

func fixtureText(_ name: String) -> String { String(decoding: fixture(name), as: UTF8.self) }
