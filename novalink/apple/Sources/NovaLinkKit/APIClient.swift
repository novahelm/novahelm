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

/// Talks to the local agent. The token is only sent after the listener proves it knows it, and
/// only on the connection the proof came back on.
public actor APIClient {
    private static let host = "127.0.0.1:7979"
    private static let timeout: TimeInterval = 2
    /// The agent is on this Mac, so a connection opens in milliseconds or not at all.
    private static let connectTimeout: TimeInterval = 0.75
    /// Pairing waits for the session to come up, and each dead address costs a dial timeout.
    private static let pairTimeout: TimeInterval = 10

    /// `nil` when the base address made no sense; every request then fails as not running.
    private let address: Address?
    private let token: @Sendable () throws -> String

    struct Address: Sendable {
        var host: String
        var port: UInt16
    }

    public init(
        base: URL = URL(string: "http://127.0.0.1:7979")!,
        token: @escaping @Sendable () throws -> String
    ) {
        address = base.host.flatMap { host in
            base.port.flatMap(UInt16.init(exactly:)).map { Address(host: host, port: $0) }
        }
        self.token = token
    }

    /// How long a request may take. Starting a pairing waits for the session to come up.
    static func timeout(for method: String, _ path: String) -> TimeInterval {
        method == "POST" && path == "/v1/pair" ? pairTimeout : timeout
    }

    // MARK: Proof

    /// Checked before every authenticated request: a long-lived client must not trust a port
    /// whose owner may have changed. The token follows on `conn`, and only on `conn`.
    private static func prove(_ token: String, on conn: AgentConnection) throws {
        // The agent requires exactly 16 bytes (32 hex characters).
        var nonce = Data(count: 16)
        for i in nonce.indices { nonce[i] = UInt8.random(in: .min ... .max) }
        let head = "GET /v1/hello?nonce=\(nonce.hex) HTTP/1.1\r\nHost: \(host)\r\nAccept: application/json\r\n\r\n"
        let reply: (status: Int, body: Data)
        do {
            reply = try conn.exchange(head, untilClose: false, deadline: Date() + timeout)
        } catch AgentConnection.Failure.timedOut {
            throw ClientError.timedOut
        } catch {
            // Not speaking HTTP is as good as no proof.
            throw ClientError.impostor
        }
        // The listener hasn't proven itself yet, so none of its text may reach the UI.
        guard (200..<300).contains(reply.status) else {
            throw ClientError.other(
                "NovaLink's agent refused the app's connection check (HTTP \(reply.status)). Make sure the app and NovaLink are the same version — reinstall NovaLink if needed.")
        }
        guard let obj = try? JSONSerialization.jsonObject(with: reply.body) as? [String: Any],
              let proofHex = obj["proof"] as? String,
              let bytes = Data(hexString: proofHex),
              HMAC<SHA256>.isValidAuthenticationCode(
                  bytes, authenticating: Data("novalink-server-proof".utf8) + nonce,
                  using: SymmetricKey(data: Data(token.utf8)))  // constant time
        else { throw ClientError.impostor }
    }

    // MARK: Requests

    private func request(_ method: String, _ path: String, body: [String: Any]? = nil) async throws -> Data {
        let token = try self.token()
        let payload = try body.map { try JSONSerialization.data(withJSONObject: $0) } ?? Data()
        guard let address else { throw ClientError.notRunning }
        // The request line and headers are written by hand, so nothing in them may break a line.
        guard Self.fitsOnALine(path), Self.fitsOnALine(token) else {
            throw ClientError.other("NovaLink can't send that request: it has characters a request can't carry.")
        }
        let timeout = Self.timeout(for: method, path)
        // The socket calls block, so they run off the actor and off Swift's own threads.
        return try await withCheckedThrowingContinuation { (done: CheckedContinuation<Data, Error>) in
            DispatchQueue.global(qos: .userInitiated).async {
                done.resume(with: Result {
                    try Self.send(method, path, payload, token: token, to: address, timeout: timeout)
                })
            }
        }
    }

    /// Visible ASCII with no spaces: safe in a request line or a header value.
    private static func fitsOnALine(_ s: String) -> Bool {
        !s.isEmpty && s.utf8.allSatisfy { $0 > 0x20 && $0 < 0x7f }
    }

    /// One connection: the proof, then the request with the token.
    private static func send(_ method: String, _ path: String, _ payload: Data, token: String,
                             to address: Address, timeout: TimeInterval) throws -> Data {
        let conn: AgentConnection
        do {
            conn = try AgentConnection(host: address.host, port: address.port, timeout: connectTimeout)
        } catch AgentConnection.Failure.timedOut {
            throw ClientError.timedOut
        } catch {
            throw ClientError.notRunning
        }
        try prove(token, on: conn)
        var head = "\(method) \(path) HTTP/1.1\r\nHost: \(host)\r\nAuthorization: Bearer \(token)\r\n"
            + "Accept: application/json\r\nConnection: close\r\nContent-Length: \(payload.count)\r\n"
        if !payload.isEmpty { head += "Content-Type: application/json\r\n" }
        head += "\r\n"
        let reply: (status: Int, body: Data)
        do {
            reply = try conn.exchange(head, body: payload, untilClose: true, deadline: Date() + timeout)
        } catch AgentConnection.Failure.timedOut {
            throw ClientError.timedOut
        } catch AgentConnection.Failure.closed {
            // No second connection: whoever answers a new one hasn't proved anything.
            throw ClientError.other("NovaLink's agent closed the connection before answering. Try again.")
        } catch {
            throw ClientError.other("NovaLink's agent sent a reply the app couldn't read.")
        }
        guard (200..<300).contains(reply.status) else {
            let obj = try? JSONSerialization.jsonObject(with: reply.body) as? [String: Any]
            throw ClientError.api(status: reply.status, message: obj?["error"] as? String ?? "request failed")
        }
        return reply.body
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
        _ = try await request("POST", "/v1/pair", body: ["name": target])
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
