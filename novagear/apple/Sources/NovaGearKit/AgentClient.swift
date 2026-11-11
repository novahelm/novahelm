import CryptoKit
import Darwin
import Foundation

/// The agent's local API on 127.0.0.1:47820, spoken over a plain socket so the identity check
/// and the token-bearing request share one connection, exactly as the Rust client does
/// (`crates/core/src/client.rs`). URLSession pools connections, so it can't promise that.
public enum AgentError: Error, Equatable, Sendable {
    case notRunning
    case tokenMissing
    /// Something else answered on the port and couldn't prove it knows the token.
    case impostor
    case http(Int, String)
    case badResponse
}

public let apiPort: UInt16 = 47820

public func tokenURL() -> URL {
    FileManager.default.homeDirectoryForCurrentUser
        .appending(path: "Library/Application Support/NovaGear/token")
}

public func readToken() throws -> String {
    guard let t = try? String(contentsOf: tokenURL(), encoding: .utf8) else { throw AgentError.tokenMissing }
    let token = t.trimmingCharacters(in: .whitespacesAndNewlines)
    guard !token.isEmpty else { throw AgentError.tokenMissing }
    return token
}

/// HMAC-SHA256 of the nonce keyed by the token, hex: what the real agent answers on /api/hello.
public func agentProof(token: String, nonce: String) -> String {
    var message = Data("novagear-agent-proof:".utf8)
    message.append(Data(nonce.utf8))
    let code = HMAC<SHA256>.authenticationCode(for: message, using: SymmetricKey(data: Data(token.utf8)))
    return code.map { String(format: "%02x", $0) }.joined()
}

func newNonce() -> String {
    (0..<16).map { _ in String(format: "%02x", UInt8.random(in: 0...255)) }.joined()
}

/// Equal-time compare, so a proof can't be guessed byte by byte.
func same(_ a: String, _ b: String) -> Bool {
    let x = Array(a.utf8), y = Array(b.utf8)
    guard x.count == y.count else { return false }
    return zip(x, y).reduce(0) { $0 | ($1.0 ^ $1.1) } == 0
}

/// One blocking connection with a small read buffer.
final class Connection {
    private let fd: Int32
    private var buffer = [UInt8]()

    init(timeout: TimeInterval) throws {
        fd = socket(AF_INET, SOCK_STREAM, 0)
        guard fd >= 0 else { throw AgentError.notRunning }
        var tv = timeval(tv_sec: Int(timeout), tv_usec: 0)
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
        setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
        var one: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, socklen_t(MemoryLayout<Int32>.size))
        var addr = sockaddr_in()
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_port = apiPort.bigEndian
        addr.sin_addr.s_addr = inet_addr("127.0.0.1")
        let ok = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        guard ok == 0 else {
            close(fd)
            throw AgentError.notRunning
        }
    }

    deinit { close(fd) }

    func setTimeout(_ seconds: Int) {
        var tv = timeval(tv_sec: seconds, tv_usec: 0)
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
    }

    func write(_ s: String) throws {
        let bytes = Array(s.utf8)
        var sent = 0
        while sent < bytes.count {
            let n = bytes[sent...].withUnsafeBytes { Darwin.write(fd, $0.baseAddress, $0.count) }
            guard n > 0 else { throw AgentError.notRunning }
            sent += n
        }
    }

    private func fill() throws -> Bool {
        var chunk = [UInt8](repeating: 0, count: 4096)
        let n = chunk.withUnsafeMutableBytes { read(fd, $0.baseAddress, $0.count) }
        if n <= 0 { return false }
        buffer.append(contentsOf: chunk[0..<n])
        return true
    }

    /// One line without its CRLF, or nil at end of stream. Lines over `limit` are an error.
    func readLine(limit: Int = 8192) throws -> String? {
        while true {
            if let i = buffer.firstIndex(of: UInt8(ascii: "\n")) {
                var line = Array(buffer[..<i])
                buffer.removeSubrange(...i)
                if line.last == UInt8(ascii: "\r") { line.removeLast() }
                return String(decoding: line, as: UTF8.self)
            }
            if buffer.count > limit { throw AgentError.badResponse }
            if try !fill() { return nil }
        }
    }

    func readExact(_ n: Int) throws -> [UInt8] {
        guard n >= 0 else { throw AgentError.badResponse }
        while buffer.count < n {
            if try !fill() { throw AgentError.badResponse }
        }
        let out = Array(buffer[..<n])
        buffer.removeSubrange(..<n)
        return out
    }

    func readToEnd(limit: Int = 4 << 20) throws -> [UInt8] {
        while try fill() {
            if buffer.count > limit { throw AgentError.badResponse }
        }
        defer { buffer.removeAll() }
        return buffer
    }
}

/// Reads a status line and headers; returns the status code and Content-Length.
func readHead(_ c: Connection) throws -> (Int, Int?) {
    guard let status = try c.readLine() else { throw AgentError.badResponse }
    let code = status.split(separator: " ").dropFirst().first.flatMap { Int($0) } ?? 0
    var length: Int?
    var lines = 0
    while let line = try c.readLine(), !line.isEmpty {
        lines += 1
        if lines > 64 { throw AgentError.badResponse }
        let lower = line.lowercased()
        if lower.hasPrefix("content-length:") {
            // Unsigned only: a negative length from an impostor must not reach readExact.
            length = UInt(lower.dropFirst("content-length:".count).trimmingCharacters(in: .whitespaces)).flatMap { Int(exactly: $0) }
        }
    }
    return (code, length)
}

/// Connects and proves, on that connection, that the listener knows the token.
func verifiedConnection(token: String, timeout: TimeInterval = 5) throws -> Connection {
    let c = try Connection(timeout: timeout)
    let nonce = newNonce()
    try c.write("GET /api/hello?nonce=\(nonce) HTTP/1.1\r\nHost: 127.0.0.1:\(apiPort)\r\nConnection: keep-alive\r\n\r\n")
    let (_, length) = try readHead(c)
    guard let length, (0...1024).contains(length) else { throw AgentError.impostor }
    let body = try c.readExact(length)
    let proof = (try? JSONSerialization.jsonObject(with: Data(body)) as? [String: Any])?["proof"] as? String ?? ""
    guard same(proof, agentProof(token: token, nonce: nonce)) else { throw AgentError.impostor }
    return c
}

func requestHead(_ method: String, _ path: String, token: String, bodyLength: Int) -> String {
    "\(method) \(path) HTTP/1.1\r\nHost: 127.0.0.1:\(apiPort)\r\nAuthorization: Bearer \(token)\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: \(bodyLength)\r\n\r\n"
}

public enum AgentClient {
    /// One API call. Blocking: call it off the main thread.
    public static func call(_ method: String, _ path: String, body: Data? = nil) throws -> Data {
        let token = try readToken()
        let c = try verifiedConnection(token: token)
        c.setTimeout(10)
        let text = body.map { String(decoding: $0, as: UTF8.self) } ?? ""
        try c.write(requestHead(method, path, token: token, bodyLength: text.utf8.count) + text)
        let (code, _) = try readHead(c)
        let data = Data(try c.readToEnd())
        guard (200..<300).contains(code) else {
            let message = (try? JSONSerialization.jsonObject(with: data) as? [String: Any])?["error"] as? String
            throw AgentError.http(code, message ?? String(decoding: data, as: UTF8.self))
        }
        return data
    }

    public static func action(_ json: [String: Any]) throws {
        _ = try call("POST", "/api/action", body: try JSONSerialization.data(withJSONObject: json))
    }

    /// Says no to a waiting approval. There is deliberately no approve call: only the deck can.
    public static func denyApproval(_ id: String) throws {
        let safe = id.addingPercentEncoding(withAllowedCharacters: .alphanumerics) ?? ""
        _ = try call("POST", "/api/approvals/\(safe)/deny")
    }

    /// Follows the event stream until it drops, calling `onEvent` with each event's JSON.
    /// Blocking: run it on its own thread.
    public static func followEvents(_ onEvent: (String) -> Void) throws {
        let token = try readToken()
        let c = try verifiedConnection(token: token)
        // Longer than the agent's 15 s keep-alive, so a silent hang shows up as an error.
        c.setTimeout(45)
        try c.write(requestHead("GET", "/api/events", token: token, bodyLength: 0))
        let (code, _) = try readHead(c)
        guard code == 200 else { throw AgentError.http(code, "event stream refused") }
        while let line = try c.readLine(limit: 1 << 20) {
            if line.hasPrefix("data:") {
                onEvent(String(line.dropFirst(5)).trimmingCharacters(in: .whitespaces))
            }
        }
        throw AgentError.notRunning
    }

    /// The panel's address with a single-use, one-minute code from the verified agent (never
    /// the token, which would land in browser history); the page trades it for a session.
    public static func panelURL(_ page: String = "", section: String? = nil) throws -> URL {
        let data = try call("POST", "/api/panel-code")
        guard let code = (try? JSONSerialization.jsonObject(with: data) as? [String: Any])?["code"] as? String,
              let url = URL(string: "http://127.0.0.1:\(apiPort)/\(page)#code=\(code)\(section.map { "&go=\($0)" } ?? "")")
        else { throw AgentError.badResponse }
        return url
    }
}
