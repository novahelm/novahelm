import Foundation

public func compactBytes(_ b: Int64) -> String {
    let gb = Double(b) / 1e9
    if gb >= 100 { return String(format: "%.0f GB", gb) }
    if gb >= 1 { return String(format: "%.1f GB", gb) }
    return String(format: "%.0f MB", Double(b) / 1e6)
}
