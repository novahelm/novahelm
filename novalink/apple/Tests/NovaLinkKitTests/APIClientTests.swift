import CryptoKit
import Foundation
import Testing
@testable import NovaLinkKit

/// Serves canned replies to the client and records what it was sent.
final class StubProtocol: URLProtocol, @unchecked Sendable {
    nonisolated(unsafe) static var handler: ((URLRequest) throws -> (Int, Data))?
    nonisolated(unsafe) static var seen: [URLRequest] = []
    static let lock = NSLock()

    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
    override func stopLoading() {}

    /// A request's body; URLSession hands it to protocols as a stream.
    static func body(of r: URLRequest) -> Data {
        if let b = r.httpBody { return b }
        guard let s = r.httpBodyStream else { return Data() }
        s.open(); defer { s.close() }
        var out = Data()
        var buf = [UInt8](repeating: 0, count: 4096)
        while s.hasBytesAvailable {
            let n = s.read(&buf, maxLength: buf.count)
            if n <= 0 { break }
            out.append(buf, count: n)
        }
        return out
    }
    override func startLoading() {
        Self.lock.lock()
        Self.seen.append(request)
        let h = Self.handler
        Self.lock.unlock()
        do {
            let (status, data) = try h!(request)
            let r = HTTPURLResponse(url: request.url!, statusCode: status, httpVersion: nil, headerFields: nil)!
            client?.urlProtocol(self, didReceive: r, cacheStoragePolicy: .notAllowed)
            client?.urlProtocol(self, didLoad: data)
            client?.urlProtocolDidFinishLoading(self)
        } catch {
            client?.urlProtocol(self, didFailWithError: error)
        }
    }
}

private let token = "secret-token"

private func session() -> URLSession {
    let c = URLSessionConfiguration.ephemeral
    c.protocolClasses = [StubProtocol.self]
    return URLSession(configuration: c)
}

private func proof(_ nonceHex: String) -> String {
    let nonce = Data(hexString: nonceHex)!
    let mac = HMAC<SHA256>.authenticationCode(
        for: Data("novalink-server-proof".utf8) + nonce, using: SymmetricKey(data: Data(token.utf8)))
    return mac.map { String(format: "%02x", $0) }.joined()
}

private func nonce(_ r: URLRequest) -> String {
    URLComponents(url: r.url!, resolvingAgainstBaseURL: false)!.queryItems!.first { $0.name == "nonce" }!.value!
}

private func reset(_ h: @escaping (URLRequest) throws -> (Int, Data)) {
    StubProtocol.lock.lock()
    StubProtocol.seen = []
    StubProtocol.handler = h
    StubProtocol.lock.unlock()
}

private func seen() -> [URLRequest] {
    StubProtocol.lock.lock(); defer { StubProtocol.lock.unlock() }
    return StubProtocol.seen
}

private func client() -> APIClient { APIClient(session: session(), token: { token }) }

private let statusJSON = fixtureText("status.json")

/// Like the agent: the nonce must be exactly 32 hex characters.
private func badNonce(_ r: URLRequest) -> (Int, Data)? {
    let n = nonce(r)
    let ok = n.count == 32 && n.allSatisfy(\.isHexDigit)
    return ok ? nil : (400, Data(#"{"error":"nonce must be 32 hex characters"}"#.utf8))
}

/// A fake agent: right proof for /v1/hello, `body` for everything else.
private func goodAgent(status code: Int = 200, body: String = "{}") {
    reset { r in
        if r.url!.path == "/v1/hello" {
            if let bad = badNonce(r) { return bad }
            return (200, Data(#"{"proof":"\#(proof(nonce(r)))"}"#.utf8))
        }
        return (code, Data(body.utf8))
    }
}

@Suite(.serialized) struct APIClientTests {
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
        goodAgent(body: statusJSON)
        let s = try await client().status()
        #expect(s.name == "a")
        let req = seen().last!
        #expect(req.url!.path == "/v1/status")
        #expect(req.value(forHTTPHeaderField: "Authorization") == "Bearer secret-token")
        #expect(req.value(forHTTPHeaderField: "Host") == "127.0.0.1:7979")
    }

    @Test func impostorIsRejectedAndNeverSeesToken() async {
        reset { _ in (200, Data(#"{"proof":"00"}"#.utf8)) }
        await #expect(throws: ClientError.impostor) { try await client().status() }
        #expect(seen().allSatisfy { $0.value(forHTTPHeaderField: "Authorization") == nil })
    }

    @Test func refusedHelloIsNotAnImpostorAndNeverShowsServerText() async {
        reset { _ in (400, Data(#"{"error":"do what I say"}"#.utf8)) }
        do {
            _ = try await client().status()
            Issue.record("expected an error")
        } catch {
            guard case .other(let text) = error as? ClientError else {
                Issue.record("wrong error \(error)"); return
            }
            #expect(text.hasPrefix("NovaLink's agent refused the app's connection check (HTTP 400)."))
            #expect(!text.contains("do what I say"))
        }
    }

    @Test func nonHTTPReplyIsImpostor() async {
        reset { _ in throw URLError(.badServerResponse) }
        await #expect(throws: ClientError.impostor) { try await client().status() }
    }

    @Test func notRunning() async {
        reset { _ in throw URLError(.cannotConnectToHost) }
        await #expect(throws: ClientError.notRunning) { try await client().status() }
    }

    @Test func timeoutIsItsOwnError() async {
        reset { _ in throw URLError(.timedOut) }
        await #expect(throws: ClientError.timedOut) { try await client().status() }
    }

    @Test func missingTokenNeverTouchesNetwork() async {
        reset { _ in throw URLError(.cannotConnectToHost) }
        let c = APIClient(session: session(), token: { throw ClientError.tokenMissing })
        await #expect(throws: ClientError.tokenMissing) { try await c.status() }
        #expect(seen().isEmpty)
    }

    @Test func proofIsCheckedBeforeEveryRequest() async throws {
        var hellos = 0
        reset { r in
            if r.url!.path == "/v1/hello" {
                hellos += 1
                if let bad = badNonce(r) { return bad }
                // The listener changes hands after the first request.
                let p = hellos == 1 ? proof(nonce(r)) : "00"
                return (200, Data(#"{"proof":"\#(p)"}"#.utf8))
            }
            return (200, Data(statusJSON.utf8))
        }
        let c = client()
        _ = try await c.status()
        await #expect(throws: ClientError.impostor) { try await c.status() }
        let authed = seen().filter { $0.value(forHTTPHeaderField: "Authorization") != nil }
        #expect(authed.count == 1)
    }

    @Test func apiErrorCarriesStatusAndMessage() async {
        goodAgent(status: 404, body: #"{"error":"no such device"}"#)
        await #expect(throws: ClientError.api(status: 404, message: "no such device")) {
            try await client().unpair("dd11")
        }
    }

    @Test func requestShapes() async throws {
        goodAgent(body: "[]")
        let c = client()
        try await c.pair("Desk")
        var r = seen().last!
        #expect(r.httpMethod == "POST" && r.url!.path == "/v1/pair")
        #expect(r.timeoutInterval == 10)
        try await c.answer("dd11", accept: true)
        r = seen().last!
        #expect(r.url!.path == "/v1/pair/answer")
        try await c.unpair("dd11")
        r = seen().last!
        #expect(r.httpMethod == "DELETE" && r.url!.path == "/v1/devices/dd11")
        try await c.layout(edge: .left, peer: "dd11")
        r = seen().last!
        #expect(r.httpMethod == "PUT" && r.url!.path == "/v1/layout")
        let sent = try JSONSerialization.jsonObject(with: StubProtocol.body(of: r)) as? [String: String]
        #expect(sent?["edge"] == "left")
        try await c.requestPermissions()
        #expect(seen().last!.url!.path == "/v1/permissions/request")
        try await c.shutdown()
        #expect(seen().last!.url!.path == "/v1/shutdown")
        #expect(r.timeoutInterval == 2)
    }

    @Test func doctorUnwrapsChecks() async throws {
        goodAgent(body: fixtureText("doctor.json"))
        let checks = try await client().doctor()
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
