import Foundation

/// One TCP connection to the agent on this Mac.
///
/// The proof and the request that carries the token both travel on it, so the token can only reach
/// the listener that proved itself. `URLSession` can't promise that: it keeps a pool of connections,
/// and when the one that carried the proof is gone it quietly dials again. By then the port may
/// belong to someone else.
final class AgentConnection {
    enum Failure: Error {
        /// Nothing accepted the connection.
        case notListening
        case timedOut
        /// The listener hung up before a whole reply arrived.
        case closed
        /// What came back wasn't an HTTP reply this client reads.
        case malformed
    }

    static let maxHead = 64 * 1024
    /// The agent's own replies are a few kilobytes.
    static let maxReply = 16 * 1024 * 1024

    private let fd: Int32
    /// Bytes read past the end of the last reply.
    private var buffer = [UInt8]()

    /// Connects to an IPv4 address; `timeout` is how long the dial may take.
    init(host: String, port: UInt16, timeout: TimeInterval) throws {
        var addr = sockaddr_in()
        addr.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_port = port.bigEndian
        guard inet_pton(AF_INET, host, &addr.sin_addr) == 1 else { throw Failure.notListening }
        let s = socket(AF_INET, SOCK_STREAM, 0)
        guard s >= 0 else { throw Failure.notListening }
        var one: Int32 = 1
        setsockopt(s, SOL_SOCKET, SO_NOSIGPIPE, &one, socklen_t(MemoryLayout<Int32>.size))
        setsockopt(s, IPPROTO_TCP, TCP_NODELAY, &one, socklen_t(MemoryLayout<Int32>.size))
        // Non-blocking from here on: every wait is a poll with what is left of a deadline.
        _ = fcntl(s, F_SETFL, fcntl(s, F_GETFL) | O_NONBLOCK)
        let r = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(s, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        if r != 0 {
            var failure: Failure? = errno == EINPROGRESS ? nil : .notListening
            if failure == nil {
                var p = pollfd(fd: s, events: Int16(POLLOUT), revents: 0)
                let n = poll(&p, 1, Int32(timeout * 1000))
                var err: Int32 = 0
                var len = socklen_t(MemoryLayout<Int32>.size)
                if n == 0 {
                    failure = .timedOut
                } else if n < 0 || getsockopt(s, SOL_SOCKET, SO_ERROR, &err, &len) != 0 || err != 0 {
                    failure = .notListening
                }
            }
            if let failure {
                Darwin.close(s)
                throw failure
            }
        }
        fd = s
    }

    deinit { Darwin.close(fd) }

    /// Sends one request and reads its reply. `untilClose` says the request asked the agent to end
    /// the connection afterwards, which lets a reply without a length run to the end of the stream.
    func exchange(_ head: String, body: Data = Data(), untilClose: Bool, deadline: Date) throws -> (status: Int, body: Data) {
        try write(Array(head.utf8) + body, deadline)
        var end = 0
        while true {
            if let r = buffer.firstRange(of: [13, 10, 13, 10]) {
                end = r.upperBound
                break
            }
            guard buffer.count <= Self.maxHead else { throw Failure.malformed }
            // Nothing at all is a hang-up; half a head is not HTTP.
            if try fill(deadline) == 0 { throw buffer.isEmpty ? Failure.closed : Failure.malformed }
        }
        let text = String(decoding: buffer[..<end], as: UTF8.self)
        buffer.removeFirst(end)
        var lines = text.components(separatedBy: "\r\n")[...]
        guard let first = lines.popFirst(), first.hasPrefix("HTTP/1.1 "),
              let status = Int(first.dropFirst(9).prefix(3)) else { throw Failure.malformed }
        var length: Int?
        for line in lines {
            guard let colon = line.firstIndex(of: ":") else { continue }
            let name = line[..<colon].lowercased()
            if name == "content-length" {
                guard let n = Int(line[line.index(after: colon)...].trimmingCharacters(in: .whitespaces)), n >= 0 else {
                    throw Failure.malformed
                }
                length = n
            } else if name == "transfer-encoding" {
                // The agent always sends a length.
                throw Failure.malformed
            }
        }
        if let length {
            guard length <= Self.maxReply else { throw Failure.malformed }
            while buffer.count < length {
                if try fill(deadline) == 0 { throw Failure.closed }
            }
            let out = Data(buffer[..<length])
            buffer.removeFirst(length)
            return (status, out)
        }
        // Without a length there is no telling where this reply ends and the next begins.
        guard untilClose else { throw Failure.malformed }
        while try fill(deadline) > 0 {
            guard buffer.count <= Self.maxReply else { throw Failure.malformed }
        }
        defer { buffer = [] }
        return (status, Data(buffer))
    }

    /// Waits until the socket is ready for `events`, and no longer than `deadline`.
    private func wait(for events: Int32, _ deadline: Date) throws {
        while true {
            let left = deadline.timeIntervalSinceNow
            guard left > 0 else { throw Failure.timedOut }
            var p = pollfd(fd: fd, events: Int16(events), revents: 0)
            let n = poll(&p, 1, Int32((left * 1000).rounded(.up)))
            if n > 0 { return }
            if n == 0 { throw Failure.timedOut }
            guard errno == EINTR else { throw Failure.closed }
        }
    }

    private func write(_ bytes: [UInt8], _ deadline: Date) throws {
        var sent = 0
        while sent < bytes.count {
            let n = bytes.withUnsafeBytes { send(fd, $0.baseAddress! + sent, bytes.count - sent, 0) }
            if n > 0 {
                sent += n
            } else if errno == EAGAIN || errno == EWOULDBLOCK {
                try wait(for: POLLOUT, deadline)
            } else if errno != EINTR {
                throw Failure.closed
            }
        }
    }

    /// Reads more of the reply into `buffer`; 0 means the agent closed the connection.
    private func fill(_ deadline: Date) throws -> Int {
        var chunk = [UInt8](repeating: 0, count: 8192)
        while true {
            try wait(for: POLLIN, deadline)
            let n = recv(fd, &chunk, chunk.count, 0)
            if n >= 0 {
                buffer.append(contentsOf: chunk[..<n])
                return n
            }
            guard errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR else { throw Failure.closed }
        }
    }
}
