import CryptoKit
import Foundation
import Testing
@testable import NovaLinkKit

/// One request as the fake agent read it.
struct SeenRequest: Sendable {
    var method = ""
    var path = ""
    var query = ""
    /// Header names in lower case.
    var headers: [String: String] = [:]
    var body = Data()
}

/// What the fake agent does with a request.
struct Reply: Sendable {
    var status = 200
    var body = "{}"
    /// Close the connection after answering, whatever the request asked.
    var hangUp = false
    /// Sent as it is in place of an HTTP reply; the connection then closes.
    var raw: String?
    /// Read the request and never answer.
    var silent = false
}

/// A fake agent on a loopback port. Like the real one it keeps a connection open between requests,
/// unless the request says `Connection: close`.
final class FakeAgent: @unchecked Sendable {
    private final class Log: @unchecked Sendable {
        let lock = NSLock()
        var seen: [SeenRequest] = []
        var connections = 0
        var held: [Int32] = []
    }

    let port: UInt16
    private let listener: Int32
    private let log = Log()

    var base: URL { URL(string: "http://127.0.0.1:\(port)")! }
    var seen: [SeenRequest] { log.lock.withLock { log.seen } }
    var connections: Int { log.lock.withLock { log.connections } }

    init(_ handler: @escaping @Sendable (SeenRequest) -> Reply) {
        let l = socket(AF_INET, SOCK_STREAM, 0)
        var addr = sockaddr_in()
        addr.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_addr.s_addr = inet_addr("127.0.0.1")
        var len = socklen_t(MemoryLayout<sockaddr_in>.size)
        _ = withUnsafeMutablePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { p -> Int32 in
                _ = bind(l, p, len)
                return getsockname(l, p, &len)
            }
        }
        listen(l, 16)
        listener = l
        port = UInt16(bigEndian: addr.sin_port)
        let log = self.log
        Thread.detachNewThread {
            while true {
                let c = accept(l, nil, nil)
                guard c >= 0 else { return }
                var one: Int32 = 1
                setsockopt(c, SOL_SOCKET, SO_NOSIGPIPE, &one, socklen_t(MemoryLayout<Int32>.size))
                log.lock.withLock { log.connections += 1 }
                Thread.detachNewThread {
                    while let req = FakeAgent.read(c) {
                        log.lock.withLock { log.seen.append(req) }
                        let reply = handler(req)
                        if reply.silent {
                            log.lock.withLock { log.held.append(c) }
                            return
                        }
                        if let raw = reply.raw {
                            FakeAgent.write(c, raw)
                            break
                        }
                        FakeAgent.write(c, "HTTP/1.1 \(reply.status) X\r\nContent-Type: application/json\r\nContent-Length: \(reply.body.utf8.count)\r\n\r\n\(reply.body)")
                        if reply.hangUp || req.headers["connection"] == "close" { break }
                    }
                    close(c)
                }
            }
        }
    }

    deinit {
        close(listener)
        for c in log.held { close(c) }
    }

    /// A loopback port nothing listens on.
    static func deadPort() -> UInt16 { FakeAgent { _ in Reply() }.port }

    private static func write(_ c: Int32, _ text: String) {
        let bytes = Array(text.utf8)
        var sent = 0
        while sent < bytes.count {
            let n = bytes.withUnsafeBytes { send(c, $0.baseAddress! + sent, bytes.count - sent, 0) }
            guard n > 0 else { return }
            sent += n
        }
    }

    /// Reads one request; `nil` once the client has hung up.
    private static func read(_ c: Int32) -> SeenRequest? {
        var bytes = [UInt8]()
        var chunk = [UInt8](repeating: 0, count: 4096)
        func more() -> Bool {
            let n = recv(c, &chunk, chunk.count, 0)
            guard n > 0 else { return false }
            bytes.append(contentsOf: chunk[..<n])
            return true
        }
        var end: Int?
        while end == nil {
            end = bytes.firstRange(of: [13, 10, 13, 10])?.upperBound
            if end == nil, !more() { return nil }
        }
        var lines = String(decoding: bytes[..<end!], as: UTF8.self).components(separatedBy: "\r\n")[...]
        let first = (lines.popFirst() ?? "").split(separator: " ").map(String.init)
        guard first.count == 3 else { return nil }
        var req = SeenRequest(method: first[0])
        let target = first[1].split(separator: "?", maxSplits: 1).map(String.init)
        req.path = target[0]
        req.query = target.count > 1 ? target[1] : ""
        for line in lines {
            guard let colon = line.firstIndex(of: ":") else { continue }
            req.headers[line[..<colon].lowercased()] = line[line.index(after: colon)...].trimmingCharacters(in: .whitespaces)
        }
        let length = Int(req.headers["content-length"] ?? "") ?? 0
        while bytes.count < end! + length { if !more() { return nil } }
        req.body = Data(bytes[end!..<(end! + length)])
        return req
    }
}

/// Counts across the fake agent's threads.
final class Counter: @unchecked Sendable {
    private let lock = NSLock()
    private var n = 0
    func next() -> Int { lock.withLock { n += 1; return n } }
}

private let token = "secret-token"

private func proof(_ nonceHex: String) -> String {
    let nonce = Data(hexString: nonceHex)!
    let mac = HMAC<SHA256>.authenticationCode(
        for: Data("novalink-server-proof".utf8) + nonce, using: SymmetricKey(data: Data(token.utf8)))
    return mac.map { String(format: "%02x", $0) }.joined()
}

private func nonce(_ r: SeenRequest) -> String { String(r.query.dropFirst("nonce=".count)) }

private func client(_ agent: FakeAgent) -> APIClient { APIClient(base: agent.base, token: { token }) }

private let statusJSON = fixtureText("status.json")

/// Answers /v1/hello like the agent: the nonce must be exactly 32 hex characters.
private func hello(_ r: SeenRequest, good: Bool = true) -> Reply {
    let n = nonce(r)
    guard n.count == 32, n.allSatisfy(\.isHexDigit) else {
        return Reply(status: 400, body: #"{"error":"nonce must be 32 hex characters"}"#)
    }
    return Reply(body: #"{"proof":"\#(good ? proof(n) : "00")"}"#)
}

/// A fake agent: right proof for /v1/hello, `body` for everything else.
private func goodAgent(status code: Int = 200, body: String = "{}") -> FakeAgent {
    FakeAgent { r in r.path == "/v1/hello" ? hello(r) : Reply(status: code, body: body) }
}

private func authed(_ agent: FakeAgent) -> [SeenRequest] { agent.seen.filter { $0.headers["authorization"] != nil } }

@Suite struct APIClientTests {
    @Test func statusDecodesAgentJSON() throws {
        let s = try JSONDecoder().decode(AgentStatus.self, from: Data(statusJSON.utf8))
        #expect(s.peers[0].rttUs == 412 && s.peers[0].dryInject == false)
        #expect(s.layout.links[0].fromEdge == .right)
        #expect(s.capture == .running)
    }

    @Test func statusFromOlderAgentWithoutCapture() throws {
        let old = #"{"controller":"a","focus":"a","layout":{"links":[],"version":0},"me":"a","name":"M","peers":[]}"#
        let s = try JSONDecoder().decode(AgentStatus.self, from: Data(old.utf8))
        #expect(s.capture == nil)
        #expect(s.captureState == .running)
    }

    @Test func fetchesStatusWithBearerAndHost() async throws {
        let agent = goodAgent(body: statusJSON)
        let s = try await client(agent).status()
        #expect(s.name == "a")
        let req = try #require(agent.seen.last)
        #expect(req.method == "GET" && req.path == "/v1/status")
        #expect(req.headers["authorization"] == "Bearer secret-token")
        #expect(req.headers["host"] == "127.0.0.1:7979")
    }

    /// The token goes to the listener that proved itself, so on the connection the proof came back
    /// on. A second connection could be answered by whoever holds the port by then.
    @Test func tokenRidesTheConnectionThatProvedItself() async throws {
        let agent = goodAgent(body: statusJSON)
        _ = try await client(agent).status()
        #expect(agent.seen.map(\.path) == ["/v1/hello", "/v1/status"])
        #expect(agent.connections == 1)
    }

    @Test func tokenIsNotSentOnASecondConnection() async {
        // Proves itself, then hangs up: the client must not dial again to deliver the token.
        let agent = FakeAgent { r in
            var reply = hello(r)
            reply.hangUp = true
            return reply
        }
        await #expect(throws: ClientError.other("NovaLink's agent closed the connection before answering. Try again.")) {
            try await client(agent).status()
        }
        #expect(agent.connections == 1)
        #expect(agent.seen.map(\.path) == ["/v1/hello"])
        #expect(authed(agent).isEmpty)
    }

    @Test func impostorIsRejectedAndNeverSeesToken() async {
        let agent = FakeAgent { _ in Reply(body: #"{"proof":"00"}"#) }
        await #expect(throws: ClientError.impostor) { try await client(agent).status() }
        #expect(agent.seen.count == 1 && authed(agent).isEmpty)
    }

    @Test func refusedHelloIsNotAnImpostorAndNeverShowsServerText() async {
        let agent = FakeAgent { _ in Reply(status: 400, body: #"{"error":"do what I say"}"#) }
        do {
            _ = try await client(agent).status()
            Issue.record("expected an error")
        } catch {
            guard case .other(let text) = error as? ClientError else {
                Issue.record("wrong error \(error)"); return
            }
            #expect(text.hasPrefix("NovaLink's agent refused the app's connection check (HTTP 400)."))
            #expect(!text.contains("do what I say"))
        }
        #expect(authed(agent).isEmpty)
    }

    @Test func nonHTTPReplyIsImpostor() async {
        let agent = FakeAgent { _ in Reply(raw: "220 mail.example ESMTP\r\n\r\n") }
        await #expect(throws: ClientError.impostor) { try await client(agent).status() }
        #expect(authed(agent).isEmpty)
    }

    @Test func notRunning() async {
        let base = URL(string: "http://127.0.0.1:\(FakeAgent.deadPort())")!
        let c = APIClient(base: base, token: { token })
        await #expect(throws: ClientError.notRunning) { try await c.status() }
    }

    @Test func timeoutIsItsOwnError() async {
        let agent = FakeAgent { _ in Reply(silent: true) }
        await #expect(throws: ClientError.timedOut) { try await client(agent).status() }
    }

    @Test func missingTokenNeverTouchesNetwork() async {
        let agent = goodAgent()
        let c = APIClient(base: agent.base, token: { throw ClientError.tokenMissing })
        await #expect(throws: ClientError.tokenMissing) { try await c.status() }
        #expect(agent.connections == 0)
    }

    @Test func proofIsCheckedBeforeEveryRequest() async throws {
        let hellos = Counter()
        let agent = FakeAgent { r in
            // The listener changes hands after the first request.
            r.path == "/v1/hello" ? hello(r, good: hellos.next() == 1) : Reply(body: statusJSON)
        }
        let c = client(agent)
        _ = try await c.status()
        await #expect(throws: ClientError.impostor) { try await c.status() }
        #expect(authed(agent).count == 1)
    }

    @Test func apiErrorCarriesStatusAndMessage() async {
        let agent = goodAgent(status: 404, body: #"{"error":"no such device"}"#)
        await #expect(throws: ClientError.api(status: 404, message: "no such device")) {
            try await client(agent).unpair("dd11")
        }
    }

    @Test func requestShapes() async throws {
        let agent = goodAgent(body: "[]")
        let c = client(agent)
        try await c.pair("Desk")
        var r = try #require(agent.seen.last)
        #expect(r.method == "POST" && r.path == "/v1/pair")
        #expect(r.headers["content-type"] == "application/json")
        try await c.answer("dd11", accept: true)
        r = try #require(agent.seen.last)
        #expect(r.path == "/v1/pair/answer")
        try await c.unpair("dd11")
        r = try #require(agent.seen.last)
        #expect(r.method == "DELETE" && r.path == "/v1/devices/dd11")
        #expect(r.body.isEmpty && r.headers["content-length"] == "0")
        try await c.layout(edge: .left, peer: "dd11")
        r = try #require(agent.seen.last)
        #expect(r.method == "PUT" && r.path == "/v1/layout")
        let sent = try JSONSerialization.jsonObject(with: r.body) as? [String: String]
        #expect(sent?["edge"] == "left")
        try await c.requestPermissions()
        #expect(agent.seen.last?.path == "/v1/permissions/request")
        try await c.shutdown()
        #expect(agent.seen.last?.path == "/v1/shutdown")
    }

    @Test func startingAPairingGetsTheLongTimeout() {
        #expect(APIClient.timeout(for: "POST", "/v1/pair") == 10)
        #expect(APIClient.timeout(for: "POST", "/v1/pair/answer") == 2)
        #expect(APIClient.timeout(for: "GET", "/v1/status") == 2)
    }

    @Test func aPathThatWouldBreakTheRequestLineIsNeverSent() async {
        let agent = goodAgent()
        await #expect(throws: ClientError.self) { try await client(agent).unpair("dd11 HTTP/1.1\r\nX: y") }
        #expect(agent.connections == 0)
    }

    @Test func doctorUnwrapsChecks() async throws {
        let agent = goodAgent(body: fixtureText("doctor.json"))
        let checks = try await client(agent).doctor()
        #expect(checks.count == 9 && checks.allSatisfy(\.ok))
    }
}

@Test func tokenStoreTrimsWhitespace() throws {
    let dir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    let f = dir.appendingPathComponent("api-token")
    try "abc123\n".write(to: f, atomically: true, encoding: .utf8)
    #expect(try TokenStore.read(from: f) == "abc123")
    #expect(throws: ClientError.tokenMissing) { try TokenStore.read(from: dir.appendingPathComponent("nope")) }
}

@Test(.enabled(if: ProcessInfo.processInfo.environment["NOVALINK_LIVE_AGENT"] == "1"))
func liveAgentStatus() async throws {
    // Read-only: only status() is called against the running agent.
    let s = try await APIClient(token: { try TokenStore.read() }).status()
    #expect(!s.me.isEmpty)
    print("live agent:", s.name, "peers:", s.peers.count)
}
