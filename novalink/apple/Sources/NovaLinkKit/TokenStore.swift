import Foundation

public enum TokenStore {
    public static var defaultURL: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Application Support/NovaLink/api-token")
    }

    /// The agent's API token, without surrounding whitespace.
    public static func read(from url: URL = defaultURL) throws -> String {
        guard let text = try? String(contentsOf: url, encoding: .utf8) else {
            throw ClientError.tokenMissing
        }
        let token = text.trimmingCharacters(in: .whitespacesAndNewlines)
        if token.isEmpty { throw ClientError.tokenMissing }
        return token
    }

    public static func read() throws -> String { try read(from: defaultURL) }
}
