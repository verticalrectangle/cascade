//  CollabGuest.swift
//  omp collab GUEST over cascade-relay: parse a join/view handle, open a
//  WebSocket as `?role=guest`, seal/open AES-256-GCM envelopes, and map host
//  frames onto cascade SessionEvent kinds so CascadeClient's existing
//  projection stays the cloud-path renderer.

import Combine
import CryptoKit
import Foundation

// MARK: - constants

private enum CollabWire {
    static let proto = 3
    static let envelopeHeader = 4
    static let roomKeyBytes = 32
    static let writeTokenBytes = 16
    static let defaultRelay = "wss://my.omp.sh"
    static let fatalCloseCodes: Set<Int> = [4001, 4004, 4009, 4029]
}

enum CollabBase64URL {
    static func decode(_ text: String) -> Data? {
        guard text.unicodeScalars.allSatisfy({
            CharacterSet.alphanumerics.contains($0) || $0 == "_" || $0 == "-"
        }) else { return nil }
        var s = text.replacingOccurrences(of: "-", with: "+")
            .replacingOccurrences(of: "_", with: "/")
        while s.count % 4 != 0 { s.append("=") }
        return Data(base64Encoded: s)
    }
    static func encode(_ data: Data) -> String {
        data.base64EncodedString()
            .replacingOccurrences(of: "+", with: "-")
            .replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
    }
}

// MARK: - link

struct CollabLink: Equatable {
    let wsURL: URL          // wss://host[:port]/r/<roomId>  (no query)
    let roomId: String
    let key: SymmetricKey
    let writeToken: Data?

    static func == (lhs: CollabLink, rhs: CollabLink) -> Bool {
        lhs.wsURL == rhs.wsURL && lhs.roomId == rhs.roomId && lhs.writeToken == rhs.writeToken
    }
}

enum CollabLinkParser {
    enum ParseError: Error, CustomStringConvertible {
        case empty
        case malformed
        case badScheme(String)
        case wsNotLocal
        case missingRoom
        case missingKey
        case badKey
        var description: String {
            switch self {
            case .empty: return "Paste a collab link."
            case .malformed: return "That doesn't look like a collab link."
            case .badScheme(let s): return "Unsupported scheme: \(s)"
            case .wsNotLocal: return "Plain ws:// is only allowed for localhost — use wss://."
            case .missingRoom: return "Link must contain a /r/<roomId> path."
            case .missingKey: return "Link is missing the key part."
            case .badKey: return "Key must be 32 (view) or 48 (full) bytes."
            }
        }
    }

    static func parse(_ raw: String) -> Result<CollabLink, ParseError> {
        do { return .success(try parseThrowing(raw)) }
        catch let e as ParseError { return .failure(e) }
        catch { return .failure(.malformed) }
    }

    static func parseThrowing(_ raw: String) throws -> CollabLink {
        var text = raw.trimmingCharacters(in: .whitespacesAndNewlines)
            .replacingOccurrences(of: "%23", with: "#", options: .caseInsensitive)
        if text.isEmpty { throw ParseError.empty }

        // Bare `<roomId>.<key>` / `<roomId>#<key>` → default relay.
        if isBareRoomSecret(text) {
            let parts = text.split(whereSeparator: { $0 == "#" || $0 == "." })
            if parts.count >= 2 {
                let id = parts[0]
                let secret = parts.dropFirst().joined(separator: ".")
                text = "\(CollabWire.defaultRelay)/r/\(id).\(secret)"
            }
        } else if !text.contains("://") {
            text = "wss://\(text)"
        }

        guard var comps = URLComponents(string: text), let scheme = comps.scheme, let host = comps.host else {
            throw ParseError.malformed
        }
        if scheme == "http" || scheme == "https", let frag = comps.fragment, !frag.isEmpty {
            return try parseThrowing(frag)
        }

        let wsScheme: String
        switch scheme {
        case "wss", "https": wsScheme = "wss"
        case "ws", "http":
            let local = host == "localhost" || host == "127.0.0.1" || host == "::1" || host == "[::1]"
            if !local { throw ParseError.wsNotLocal }
            wsScheme = "ws"
        default: throw ParseError.badScheme(scheme)
        }

        let path = comps.path
        guard let room = roomPathParts(path) else {
            if let frag = comps.fragment, !frag.isEmpty { return try parseThrowing(frag) }
            throw ParseError.missingRoom
        }
        let fragment = room.secret ?? comps.fragment
        guard let frag = fragment, !frag.isEmpty else { throw ParseError.missingKey }
        let (keyData, token) = try splitSecret(frag)
        let portPart = comps.port.map { ":\($0)" } ?? ""
        guard let ws = URL(string: "\(wsScheme)://\(host)\(portPart)/r/\(room.id)") else {
            throw ParseError.malformed
        }
        return CollabLink(wsURL: ws, roomId: room.id, key: SymmetricKey(data: keyData), writeToken: token)
    }

    private static func isBareRoomSecret(_ text: String) -> Bool {
        guard !text.contains("/"), !text.contains("://") else { return false }
        let sepIdx = text.firstIndex(of: ".") ?? text.firstIndex(of: "#")
        guard let i = sepIdx else { return false }
        let id = text[..<i]
        let key = text[text.index(after: i)...]
        let idOk = (10...64).contains(id.count) && id.unicodeScalars.allSatisfy {
            CharacterSet.alphanumerics.contains($0) || $0 == "_" || $0 == "-"
        }
        let keyOk = !key.isEmpty && key.unicodeScalars.allSatisfy {
            CharacterSet.alphanumerics.contains($0) || $0 == "_" || $0 == "-" || $0 == "."
        }
        return idOk && keyOk
    }

    private static func roomPathParts(_ pathname: String) -> (id: String, secret: String?)? {
        var rest = pathname
        if rest.hasPrefix("/r/") { rest = String(rest.dropFirst(3)) }
        else { return nil }
        while rest.hasSuffix("/") { rest.removeLast() }
        guard !rest.isEmpty else { return nil }
        if let dot = rest.firstIndex(of: ".") {
            let id = String(rest[..<dot])
            let secret = String(rest[rest.index(after: dot)...])
            guard (10...64).contains(id.count) else { return nil }
            return (id, secret)
        }
        guard (10...64).contains(rest.count) else { return nil }
        return (rest, nil)
    }

    private static func splitSecret(_ fragment: String) throws -> (Data, Data?) {
        let pieces = fragment.split(separator: ".", omittingEmptySubsequences: false).map(String.init)
        if pieces.count >= 2 {
            guard let key = CollabBase64URL.decode(pieces[0]), key.count == CollabWire.roomKeyBytes else {
                throw ParseError.badKey
            }
            guard let tok = CollabBase64URL.decode(pieces[1]), tok.count == CollabWire.writeTokenBytes else {
                throw ParseError.badKey
            }
            return (key, tok)
        }
        guard let secret = CollabBase64URL.decode(fragment) else { throw ParseError.missingKey }
        guard secret.count == CollabWire.roomKeyBytes
                || secret.count == CollabWire.roomKeyBytes + CollabWire.writeTokenBytes else {
            throw ParseError.badKey
        }
        let key = secret.prefix(CollabWire.roomKeyBytes)
        let token = secret.count > CollabWire.roomKeyBytes
            ? Data(secret.suffix(CollabWire.writeTokenBytes)) : nil
        return (Data(key), token)
    }
}

// MARK: - AES-GCM

private enum CollabSeal {
    static func open(_ key: SymmetricKey, _ data: Data) -> [String: Any]? {
        guard data.count > 12,
              let box = try? AES.GCM.SealedBox(combined: data),
              let plain = try? AES.GCM.open(box, using: key),
              let obj = try? JSONSerialization.jsonObject(with: plain) as? [String: Any]
        else { return nil }
        return obj
    }
    static func seal(_ key: SymmetricKey, _ frame: [String: Any]) -> Data? {
        guard let plain = try? JSONSerialization.data(withJSONObject: frame),
              let box = try? AES.GCM.seal(plain, using: key)
        else { return nil }
        return box.combined
    }
}

// MARK: - WebSocket guest

final class CollabGuestSocket: NSObject, URLSessionWebSocketDelegate {
    var onOpen: (() -> Void)?
    var onFrame: (([String: Any]) -> Void)?
    var onControl: (([String: Any]) -> Void)?
    /// `fatal` true → never reconnect (close codes 4001/4004/4009/4029 or bye).
    var onUnexpectedClose: ((_ reason: String, _ fatal: Bool) -> Void)?

    private let link: CollabLink
    private let displayName: String
    private var task: URLSessionWebSocketTask?
    private var session: URLSession?
    private var closed = false
    private var intentionalClose = false
    private var generation = 0
    private var reconnectAttempt = 0
    private var reconnectTask: Task<Void, Never>?
    private var terminated = false

    init(link: CollabLink, name: String) {
        self.link = link
        self.displayName = name
        super.init()
    }

    func connect() {
        reconnectTask?.cancel()
        intentionalClose = false
        closed = false
        terminated = false
        openOnce()
    }

    /// Drop the current socket and open a fresh one (resync).
    func reconnectNow() {
        guard !terminated else { return }
        reconnectAttempt = 0
        openOnce()
    }

    func close() {
        terminated = true
        intentionalClose = true
        closed = true
        reconnectTask?.cancel()
        task?.cancel(with: .goingAway, reason: nil)
        task = nil
        session?.invalidateAndCancel()
        session = nil
    }

    func send(_ frame: [String: Any]) {
        guard let sealed = CollabSeal.seal(link.key, frame) else { return }
        var env = Data(count: CollabWire.envelopeHeader)   // peerId 0 BE
        env.append(sealed)
        task?.send(.data(env)) { _ in }
    }

    private func openOnce() {
        task?.cancel(with: .goingAway, reason: nil)
        session?.invalidateAndCancel()
        generation += 1
        let gen = generation
        closed = false
        var comps = URLComponents(url: link.wsURL, resolvingAgainstBaseURL: false)!
        comps.queryItems = [URLQueryItem(name: "role", value: "guest")]
        let sess = URLSession(configuration: .default, delegate: self, delegateQueue: nil)
        session = sess
        let t = sess.webSocketTask(with: comps.url!)
        t.maximumMessageSize = 128 * 1024 * 1024
        task = t
        t.resume()
        receive(gen)
    }

    func urlSession(_ session: URLSession, webSocketTask: URLSessionWebSocketTask, didOpenWithProtocol proto: String?) {
        guard webSocketTask === task else { return }
        reconnectAttempt = 0
        sendHello()
        onOpen?()
    }

    func urlSession(_ session: URLSession, webSocketTask: URLSessionWebSocketTask, didCloseWith code: URLSessionWebSocketTask.CloseCode, reason: Data?) {
        guard webSocketTask === task else { return }
        let raw = Int(code.rawValue)
        let why = reason.flatMap { String(data: $0, encoding: .utf8) }.flatMap { $0.isEmpty ? nil : $0 }
            ?? "connection closed (\(raw))"
        fail(why, fatal: CollabWire.fatalCloseCodes.contains(raw))
    }

    private func sendHello() {
        var hello: [String: Any] = ["t": "hello", "proto": CollabWire.proto, "name": displayName]
        if let tok = link.writeToken { hello["writeToken"] = CollabBase64URL.encode(tok) }
        send(hello)
    }

    private func receive(_ gen: Int) {
        task?.receive { [weak self] result in
            guard let self, self.generation == gen, !self.closed else { return }
            switch result {
            case .failure(let err):
                self.fail(err.localizedDescription, fatal: false)
            case .success(let message):
                switch message {
                case .data(let data):
                    if data.count > CollabWire.envelopeHeader {
                        let payload = data.subdata(in: CollabWire.envelopeHeader..<data.count)
                        if let frame = CollabSeal.open(self.link.key, payload) {
                            self.onFrame?(frame)
                        } else {
                            self.fail("bad key or corrupted frame", fatal: true)
                            return
                        }
                    }
                case .string(let text):
                    if let obj = try? JSONSerialization.jsonObject(with: Data(text.utf8)) as? [String: Any] {
                        if obj["t"] as? String == "room-closed" {
                            self.fail("room closed", fatal: true)
                            return
                        }
                        self.onControl?(obj)
                    }
                @unknown default:
                    break
                }
                self.receive(gen)
            }
        }
    }

    private func fail(_ reason: String, fatal: Bool) {
        guard !intentionalClose, !closed else { return }
        closed = true
        if fatal || terminated {
            terminated = true
            onUnexpectedClose?(reason, true)
            return
        }
        onUnexpectedClose?(reason, false)
        scheduleReconnect(reason: reason)
    }

    private func scheduleReconnect(reason: String) {
        guard !terminated, !intentionalClose else { return }
        reconnectTask?.cancel()
        let steps: [TimeInterval] = [1, 2, 4, 8, 16]
        let base = steps[min(reconnectAttempt, steps.count - 1)]
        reconnectAttempt += 1
        let cap = min(base * pow(2, 0), 30)   // already stepped; hard cap 30s
        let delay = min(max(base, 1), 30)
        let jittered = delay * (0.75 + Double.random(in: 0...0.5))
        let wait = min(jittered, 30)
        reconnectTask = Task { [weak self] in
            try? await Task.sleep(nanoseconds: UInt64(wait * 1_000_000_000))
            if Task.isCancelled { return }
            await MainActor.run { self?.openOnce() }
        }
        _ = cap
        _ = reason
    }
}

// MARK: - FrameMapper (attach.rs semantics → cascade SessionEvent dicts)

struct CollabFrameMapper {
    private var lastText = ""
    private var lastThinking = ""

    mutating func reset() {
        lastText = ""
        lastThinking = ""
    }

    /// Project a collab host frame into cascade-core `kind` dicts consumed by `CascadeClient.applyFrame`.
    mutating func mapFrame(_ frame: [String: Any]) -> [[String: Any]] {
        let t = frame["t"] as? String ?? ""
        switch t {
        case "event":
            return mapAgentEvent(frame["event"] as? [String: Any])
        case "entry":
            if let e = frame["entry"] as? [String: Any] { return [mapEntry(e)] }
            return []
        case "state":
            var ev: [String: Any] = ["kind": "state_changed"]
            if let st = frame["state"] as? [String: Any] { ev["state"] = collabStateToRpc(st) }
            return [ev]
        case "welcome":
            reset()
            var out: [[String: Any]] = []
            if let header = frame["header"] as? [String: Any] {
                var info: [String: Any] = ["kind": "session_info"]
                if let title = header["title"] as? String { info["title"] = title }
                if let id = header["id"] as? String { info["session_id"] = id }
                out.append(info)
            }
            if let st = frame["state"] as? [String: Any] {
                var ev: [String: Any] = ["kind": "state_changed", "state": collabStateToRpc(st)]
                out.append(ev)
            }
            var snap: [String: Any] = ["kind": "snapshot", "messages": [], "streaming": false]
            if let n = frame["entryCount"] as? Int { snap["entry_count"] = n }
            out.append(snap)
            return out
        case "snapshot-chunk":
            var out: [[String: Any]] = []
            if let entries = frame["entries"] as? [[String: Any]] {
                for e in entries { out.append(mapEntry(e)) }
            }
            return out
        case "ui-request":
            if let req = frame["request"] as? [String: Any] { return [mapUiRequest(req)] }
            return []
        case "ui-request-end":
            let id = stringify(frame["reqId"])
            return [["kind": "ui_request_cancelled", "target_id": id]]
        case "error":
            let message = frame["message"] as? String ?? "collab error"
            return [["kind": "notice", "level": "error", "message": message]]
        case "bye":
            let message = frame["reason"] as? String ?? "bye"
            return [
                ["kind": "notice", "level": "info", "message": message],
                ["kind": "process_exited"],
            ]
        default:
            return []
        }
    }

    private mutating func mapAgentEvent(_ ev: [String: Any]?) -> [[String: Any]] {
        guard let ev else { return [] }
        let ty = ev["type"] as? String ?? ""
        switch ty {
        case "turn_start":
            return [["kind": "turn_started"]]
        case "agent_end":
            reset()
            return [["kind": "agent_end"]]
        case "agent_start":
            reset()
            return [["kind": "agent_start"]]
        case "message_start":
            lastText = ""
            lastThinking = ""
            let role = ((ev["message"] as? [String: Any])?["role"] as? String) ?? "assistant"
            return [["kind": "message_start", "role": role]]
        case "message_update":
            return mapMessageUpdate(ev["message"] as? [String: Any])
        case "message_end":
            lastText = ""
            lastThinking = ""
            var out: [String: Any] = ["kind": "message_end"]
            if let m = ev["message"] { out["message"] = m }
            return [out]
        case "tool_execution_start":
            return [[
                "kind": "tool_start",
                "tool_call_id": strField(ev, "toolCallId"),
                "tool_name": strField(ev, "toolName"),
                "args": ev["args"] ?? NSNull(),
                "intent": ev["intent"] as? String ?? "",
            ]]
        case "tool_execution_update":
            return [[
                "kind": "tool_update",
                "tool_call_id": strField(ev, "toolCallId"),
                "partial": ev["partialResult"] ?? NSNull(),
            ]]
        case "tool_execution_end":
            return [[
                "kind": "tool_end",
                "tool_call_id": strField(ev, "toolCallId"),
                "tool_name": strField(ev, "toolName"),
                "is_error": ev["isError"] as? Bool ?? false,
                "result": ev["result"] ?? NSNull(),
            ]]
        case "notice":
            return [["kind": "notice", "level": strField(ev, "level"), "message": strField(ev, "message")]]
        default:
            return []
        }
    }

    private mutating func mapMessageUpdate(_ message: [String: Any]?) -> [[String: Any]] {
        guard let message else { return [] }
        var out: [[String: Any]] = []
        if let s = message["content"] as? String {
            if let delta = suffixDelta(prev: lastText, full: s), !delta.isEmpty {
                out.append(["kind": "text_delta", "delta": delta, "content_index": 0])
            }
            lastText = s
        } else if let arr = message["content"] as? [[String: Any]] {
            for (i, block) in arr.enumerated() {
                let ty = block["type"] as? String ?? ""
                switch ty {
                case "text":
                    let s = block["text"] as? String ?? ""
                    if let delta = suffixDelta(prev: lastText, full: s), !delta.isEmpty {
                        out.append(["kind": "text_delta", "delta": delta, "content_index": i])
                    }
                    lastText = s
                case "thinking":
                    let s = block["thinking"] as? String ?? ""
                    if let delta = suffixDelta(prev: lastThinking, full: s), !delta.isEmpty {
                        out.append(["kind": "thinking_delta", "delta": delta, "content_index": i])
                    }
                    lastThinking = s
                default:
                    break
                }
            }
        }
        return out
    }

    private func mapEntry(_ entry: [String: Any]) -> [String: Any] {
        if (entry["type"] as? String) == "message", let message = entry["message"] {
            return ["kind": "message_end", "message": message]
        }
        return ["kind": "notice", "level": "info", "message": "entry"]
    }

    private func mapUiRequest(_ req: [String: Any]) -> [String: Any] {
        let id = stringify(req["reqId"])
        let kind = req["kind"] as? String ?? ""
        let method: String
        switch kind {
        case "select": method = "select"
        case "editor": method = "editor"
        case "confirm": method = "confirm"
        case "input": method = "input"
        default: method = "other"
        }
        var options: [String] = []
        if let arr = req["options"] as? [String] {
            options = arr
        } else if let arr = req["options"] as? [[String: Any]] {
            options = arr.compactMap { $0["label"] as? String }
        }
        return [
            "kind": "ui_request",
            "id": id,
            "method": method,
            "title": req["title"] as? String as Any,
            "message": req["helpText"] as? String as Any,
            "options": options,
            "prefill": req["prefill"] as? String as Any,
        ]
    }

    private func collabStateToRpc(_ st: [String: Any]) -> [String: Any] {
        var rpc: [String: Any] = [:]
        if let m = st["model"] as? [String: Any] { rpc["model"] = m }
        if let lvl = st["thinkingLevel"] as? String { rpc["thinkingLevel"] = lvl }
        return rpc
    }

    private func strField(_ v: [String: Any], _ k: String) -> String {
        v[k] as? String ?? ""
    }

    private func stringify(_ v: Any?) -> String {
        if let s = v as? String { return s }
        if let n = v as? Int { return String(n) }
        if let n = v as? NSNumber { return n.stringValue }
        return "0"
    }

    private func suffixDelta(prev: String, full: String) -> String? {
        if let rest = full.stripPrefix(prev) { return rest }
        if full == prev { return "" }
        return full
    }
}

private extension String {
    func stripPrefix(_ prefix: String) -> String? {
        guard hasPrefix(prefix) else { return nil }
        return String(dropFirst(prefix.count))
    }
}
