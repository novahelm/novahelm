import CryptoKit
import Foundation

public enum ClientError: Error, Equatable, Sendable {
    case notRunning
    case impostor
    case tokenMissing
    case timedOut
    case api(status: Int, message: String)
    case other(String)
}

extension ClientError: LocalizedError {
    public var errorDescription: String? {
        switch self {
        case .notRunning: "NovaLink isn't running — choose Turn On NovaLink."
        case .impostor: "Something else is listening on 127.0.0.1:7979, so NovaLink won't send it the API token."
        case .tokenMissing: "The NovaLink API token is missing. Start the agent once to create it."
        case .timedOut: "The NovaLink agent didn't answer in time. Try again."
        case .api(let status, let message): "\(message) (HTTP \(status))"
        case .other(let text): text
        }
    }
}

extension Data {
    init?(hexString s: String) {
        guard s.count.isMultiple(of: 2), s.utf8.count == s.count else { return nil }
        var out = Data(capacity: s.count / 2)
        var i = s.startIndex
        while i < s.endIndex {
            let j = s.index(i, offsetBy: 2)
            guard let b = UInt8(s[i..<j], radix: 16) else { return nil }
            out.append(b)
            i = j
        }
        self = out
    }

    var hex: String { map { String(format: "%02x", $0) }.joined() }
}

/// Talks to the local agent. The token is only sent after the listener proves it knows it.
public actor APIClient {
    private static let host = "127.0.0.1:7979"
    private static let timeout: TimeInterval = 2
    /// Pairing waits for the session to come up, and each dead address costs a dial timeout.
    private static let pairTimeout: TimeInterval = 10

    private let base: URL
    private let session: URLSession
    private let token: @Sendable () throws -> String

    public init(
        base: URL = URL(string: "http://127.0.0.1:7979")!,
        session: URLSession = APIClient.makeSession(),
        token: @escaping @Sendable () throws -> String
    ) {
        self.base = base
        self.session = session
        self.token = token
    }

    /// No cache, cookies or redirects: the token must never be sent anywhere but the first URL.
    public static func makeSession() -> URLSession {
        let c = URLSessionConfiguration.ephemeral
        c.urlCache = nil
        c.httpCookieStorage = nil
        c.httpShouldSetCookies = false
        c.requestCachePolicy = .reloadIgnoringLocalCacheData
        return URLSession(configuration: c, delegate: NoRedirects(), delegateQueue: nil)
    }

    // MARK: Proof

    /// Checked before every authenticated request: a long-lived client must not trust a port
    /// whose owner may have changed.
    private func prove(_ token: String) async throws {
        // The agent requires exactly 16 bytes (32 hex characters).
        var nonce = Data(count: 16)
        for i in nonce.indices { nonce[i] = UInt8.random(in: .min ... .max) }
        var comps = URLComponents(url: base.appendingPathComponent("v1/hello"), resolvingAgainstBaseURL: false)!
        comps.queryItems = [URLQueryItem(name: "nonce", value: nonce.hex)]
        var req = URLRequest(url: comps.url!, timeoutInterval: Self.timeout)
        req.setValue(Self.host, forHTTPHeaderField: "Host")
        let (data, response): (Data, URLResponse)
        do {
            (data, response) = try await session.data(for: req)
        } catch let e as URLError {
            switch e.code {
            case .cannotConnectToHost, .networkConnectionLost, .notConnectedToInternet: throw ClientError.notRunning
            case .timedOut: throw ClientError.timedOut
            // Not speaking HTTP is as good as no proof.
            default: throw ClientError.impostor
            }
        }
        guard let http = response as? HTTPURLResponse else { throw ClientError.impostor }
        // The listener hasn't proven itself yet, so none of its text may reach the UI.
        guard (200..<300).contains(http.statusCode) else {
            throw ClientError.other(
                "NovaLink's agent refused the app's connection check (HTTP \(http.statusCode)). Make sure the app and NovaLink are the same version — reinstall NovaLink if needed.")
        }
        guard let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let proofHex = obj["proof"] as? String,
              let bytes = Data(hexString: proofHex),
              HMAC<SHA256>.isValidAuthenticationCode(
                  bytes, authenticating: Data("novalink-server-proof".utf8) + nonce,
                  using: SymmetricKey(data: Data(token.utf8)))  // constant time
        else { throw ClientError.impostor }
    }

    // MARK: Requests

    private func request(_ method: String, _ path: String, body: [String: Any]? = nil,
                         timeout: TimeInterval = APIClient.timeout) async throws -> Data {
        let token = try self.token()
        try await prove(token)
        var req = URLRequest(url: URL(string: base.absoluteString.trimmingSuffix("/") + path)!, timeoutInterval: timeout)
        req.httpMethod = method
        req.setValue(Self.host, forHTTPHeaderField: "Host")
        req.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        if let body {
            req.httpBody = try JSONSerialization.data(withJSONObject: body)
            req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        }
        let (data, response): (Data, URLResponse)
        do {
            (data, response) = try await session.data(for: req)
        } catch let e as URLError {
            switch e.code {
            case .cannotConnectToHost: throw ClientError.notRunning
            case .timedOut: throw ClientError.timedOut
            default: throw ClientError.other(e.localizedDescription)
            }
        }
        let status = (response as? HTTPURLResponse)?.statusCode ?? 0
        guard (200..<300).contains(status) else {
            let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
            throw ClientError.api(status: status, message: obj?["error"] as? String ?? "request failed")
        }
        return data
    }

    private func typed<T: Decodable>(_ data: Data) throws -> T {
        do { return try JSONDecoder().decode(T.self, from: data) }
        catch { throw ClientError.other("unexpected reply: \(error)") }
    }

    public func status() async throws -> AgentStatus { try typed(await request("GET", "/v1/status")) }
    public func pending() async throws -> [PendingPair] { try typed(await request("GET", "/v1/pair/pending")) }
    public func discovered() async throws -> [DiscoveredDevice] { try typed(await request("GET", "/v1/discovered")) }

    public func doctor() async throws -> [Check] {
        struct Reply: Decodable { var checks: [Check] }
        let r: Reply = try typed(await request("GET", "/v1/doctor"))
        return r.checks
    }

    /// Starts pairing with a discovered device, by name or id.
    public func pair(_ target: String) async throws {
        _ = try await request("POST", "/v1/pair", body: ["name": target], timeout: Self.pairTimeout)
    }

    public func answer(_ id: String, accept: Bool) async throws {
        _ = try await request("POST", "/v1/pair/answer", body: ["id": id, "accept": accept])
    }

    public func unpair(_ id: String) async throws { _ = try await request("DELETE", "/v1/devices/\(id)") }

    public func layout(edge: Edge, peer: String) async throws {
        _ = try await request("PUT", "/v1/layout", body: ["edge": edge.rawValue, "peer": peer])
    }

    public func setSwapCtrlCmd(_ on: Bool) async throws {
        _ = try await request("PUT", "/v1/settings", body: ["swap_ctrl_cmd": on])
    }

    public func requestPermissions() async throws { _ = try await request("POST", "/v1/permissions/request") }
    public func shutdown() async throws { _ = try await request("POST", "/v1/shutdown") }
}

private extension String {
    func trimmingSuffix(_ s: String) -> String { hasSuffix(s) ? String(dropLast(s.count)) : self }
}

private final class NoRedirects: NSObject, URLSessionTaskDelegate, Sendable {
    func urlSession(_ session: URLSession, task: URLSessionTask,
                    willPerformHTTPRedirection response: HTTPURLResponse,
                    newRequest request: URLRequest) async -> URLRequest? { nil }
}
