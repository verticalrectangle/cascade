//  CascadeBridge.swift
//  Native iOS client for cascade — talks to the `cascaded` cloud daemon.
//
//  Replaces Enclave's collab-guest transport (EngineBridge.swift) with the
//  cascade cloud API:
//    POST /auth/login            → {token}          (JWT, 30d)
//    POST /auth/register         → {token}          (invite-gated)
//    POST /sessions/{id}/share   → {token,url}      (view link)
//    GET  /s/{token}             → {session_id, read_only}
//    GET  /machines              → [MachineInfo]
//    GET  /sessions              → [SessionMeta]
//    POST /sessions              → {id}             (spawn on a machine)
//    DELETE /sessions/{id}
//    WS   /sessions/{id}/stream  (Bearer on handshake; first frame is a
//                                 `snapshot` SessionEvent, then one SessionEvent
//                                 JSON per TEXT frame; client sends CloudCommand)
//
//  The wire enum is cascade-core's SessionEvent: internally tagged with "kind",
//  snake_case. CloudCommand (prompt/abort/answer_ui) uses the same scheme. The
//  client projects events onto the same [UITurn] surface the Enclave views
//  already render — the view layer is unchanged.

import Combine
import CryptoKit
import Foundation

// MARK: - Wire types (cascade-core serde shapes)

struct MachineInfo: Codable, Equatable, Identifiable {
    let id: String
    let name: String
    let online: Bool
    let isCloud: Bool

    private enum CodingKeys: String, CodingKey {
        case id, name, online = "online", isCloud = "is_cloud"
    }
}

/// `GET /sessions` row (`ListedSession` in cascaded).
struct RemoteSessionMeta: Codable, Equatable {
    let id: String
    var omp_session_id: String?
    var name: String?
    let cwd: String
    var session_file: String?
    let machine: String
    let created_at: Date?
    let last_active: Date?
    var join_handle: String?
    var view_handle: String?
    var pid: Int?
    var kind: String?
}

/// cascade-core `UiRequest` (inlined next to `"kind":"ui_request"`).
struct CascadeUiRequest: Codable, Equatable {
    let id: String
    let method: String        // select | confirm | input | editor | open_url | notify | ...
    let title: String?
    let message: String?
    let options: [String]
    let placeholder: String?
    let prefill: String?
    let url: String?
    let timeout_secs: Int?
}

enum Wire {
    static func event(from data: Data) -> [String: Any]? {
        try? JSONSerialization.jsonObject(with: data) as? [String: Any]
    }

    /// CloudCommand JSON — client → server over the session stream.
    static func prompt(_ text: String) -> String {
        body(["kind": "prompt", "message": text])
    }
    static func abort() -> String { body(["kind": "abort"]) }
    static func setModel(provider: String, modelId: String) -> String {
        body(["kind": "set_model", "provider": provider, "model_id": modelId])
    }
    static func setThinking(_ level: String) -> String {
        body(["kind": "set_thinking", "level": level])
    }
    static func getState() -> String { body(["kind": "get_state"]) }
    static func answer(requestId: String, value: String? = nil, confirmed: Bool? = nil) -> String {
        var response: [String: Any] = [:]
        if let value { response["value"] = value }
        if let confirmed { response["confirmed"] = confirmed }
        if value == nil && confirmed == nil {
            return body(["kind": "answer_ui", "request_id": requestId, "response": "cancelled"])
        }
        return body(["kind": "answer_ui", "request_id": requestId, "response": response])
    }
    static func createSession(machine: String?, cwd: String, model: String?) -> String {
        var o: [String: Any] = ["cwd": cwd]
        if let machine, machine != "cloud" { o["machine"] = machine }
        if let model { o["model"] = model }
        return body(o)
    }
    static func body(_ o: [String: Any]) -> String {
        guard let d = try? JSONSerialization.data(withJSONObject: o), let s = String(data: d, encoding: .utf8) else { return "{}" }
        return s
    }
}

// MARK: - CascadeClient

@MainActor
final class CascadeClient: ObservableObject {
    // Published surface the UI observes.
    @Published private(set) var turns: [UITurn] = []
    @Published private(set) var phase: String = "connecting"   // connecting/waiting/live/ended
    @Published private(set) var working = false
    @Published private(set) var title = "session"
    @Published private(set) var cwd = "~"
    @Published private(set) var modelName = "—"               // display name only; provider split below
    @Published private(set) var tokensLabel = "—"
    @Published private(set) var costLabel = "—"
    @Published private(set) var endedReason: String?
    @Published private(set) var readOnly = false

    // Trust / Activity surfaces.
    @Published private(set) var sessionId = ""
    @Published private(set) var relay = "—"                    // reused: shows host + session id
    @Published private(set) var contextPercent: Double?
    @Published private(set) var queued = 0
    @Published private(set) var participants: [ParticipantInfo] = []
    @Published private(set) var agents: [AgentInfo] = []       // always empty: no subagent bus in cascade yet
    @Published private(set) var progress: [SubagentProgress] = []

    // Kept for view compatibility; permanently off over the cloud transport.
    @Published private(set) var enhanced = false
    @Published private(set) var canSendImages = false
    @Published private(set) var commands: [CascadeCommand] = []
    @Published var awaitingVision = false
    var sawWorking = false
    // Model identity, split for clean display: "provider / model".
    @Published private(set) var providerName = ""
    @Published private(set) var thinkingLevel = ""
    @Published private(set) var availableModels: [ModelOption] = []
    @Published private(set) var thinkingLevels: [String] = ["off", "minimal", "low", "medium", "high"]
    @Published private(set) var models: [ModelOption] = []   // legacy alias surface; kept empty
    private(set) var joinLink = ""

    /// Fired after every applied frame (SessionVM bridges this to its own publish).
    var onChange: (() -> Void)?

    private let base: URL                 // e.g. http://127.0.0.1:7700
    private let token: String             // bearer JWT
    private let targetId: String          // cascade session uuid
    private let deviceName: String
    private var guestSocket: CollabGuestSocket?
    private var guestMapper = CollabFrameMapper()
    private var socketTask: URLSessionWebSocketTask?
    private var socketSession: URLSession?
    private var receiveLoop = false
    private var terminated = false        // deliberate end (leave/bye/process exit) — never reconnect
    private var reconnectAttempt = 0
    private var reconnectTask: Task<Void, Never>?
    @Published private(set) var plan: [PlanPhase] = []
    private var messages: [[String: Any]] = []   // finalized AgentMessages (omp shape, verbatim)
    private var streamText = ""
    private var streamThinking = ""
    private var streamDone = false
    private var activeTools: [(id: String, tool: [String: Any])] = []
    private var pendingRequest: CascadeUiRequest?
    private var pendingSends: Set<String> = []   // optimistic prompts awaiting echo
    @Published private(set) var welcomed = false
    @Published private(set) var goal: GoalInfo?
    @Published private(set) var activity: String?
    @Published private(set) var currentMode: String?
    @Published private(set) var notices: [NoticeItem] = []
    var justPaired = false

    // Incremental rebuild caches (see EngineBridge notes — same rationale).
    private var cachedStaticTurns: [UITurn] = []
    private var cachedTailCount = 0
    private var cachedMessageCount = 0
    private var streamRebuildPending = false
    private var lastStreamRebuild: Date = .distantPast
    private let streamCoalesceInterval: TimeInterval = 1.0 / 30.0

    struct Config {
        let base: URL
        let token: String
        let sessionId: String
        let name: String
        let readOnly: Bool
        /// Pre-set when attaching to an existing session (skips login+list).
        init(base: URL, token: String, sessionId: String, name: String, readOnly: Bool = false) {
            self.base = base; self.token = token; self.sessionId = sessionId; self.name = name
            self.readOnly = readOnly
        }
    }

    // MARK: account / directory (replaces PairView's link grammar)

    struct Account: Codable, Equatable {
        var base: URL
        var token: String
        var email: String
    }

    /// Login and persist the resulting JWT. Throws a user-facing reason.
    static func login(base: URL, email: String, password: String) async throws -> Account {
        var req = URLRequest(url: base.appendingPathComponent("auth/login"))
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = Wire.body(["email": email, "password": password]).data(using: .utf8)
        let (data, resp): (Data, URLResponse)
        do { (data, resp) = try await URLSession.shared.data(for: req) }
        catch { throw BridgeError.network(String(describing: error)) }
        guard let http = resp as? HTTPURLResponse else { throw BridgeError.badResponse }
        guard http.statusCode == 200 else {
            if http.statusCode == 401 { throw BridgeError.invalidCredentials }
            throw BridgeError.server(httpStatusCode: http.statusCode)
        }
        guard let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let tok = obj["token"] as? String else { throw BridgeError.badResponse }
        return Account(base: base, token: tok, email: email)
    }

    /// Create an account with an invite, then persist like login.
    static func register(base: URL, email: String, password: String, invite: String) async throws -> Account {
        var req = URLRequest(url: base.appendingPathComponent("auth/register"))
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = Wire.body(["email": email, "password": password, "invite": invite]).data(using: .utf8)
        let (data, resp): (Data, URLResponse)
        do { (data, resp) = try await URLSession.shared.data(for: req) }
        catch { throw BridgeError.network(String(describing: error)) }
        guard let http = resp as? HTTPURLResponse else { throw BridgeError.badResponse }
        guard http.statusCode == 200 else { throw daemonError(data, status: http.statusCode) }
        guard let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let tok = obj["token"] as? String else { throw BridgeError.badResponse }
        return Account(base: base, token: tok, email: email)
    }

    struct ShareInfo: Equatable {
        let token: String
        let url: String
    }

    static func createShare(account: Account, sessionId: String) async throws -> ShareInfo {
        var req = URLRequest(url: account.base
            .appendingPathComponent("sessions")
            .appendingPathComponent(sessionId)
            .appendingPathComponent("share"))
        req.httpMethod = "POST"
        req.setValue("Bearer \(account.token)", forHTTPHeaderField: "Authorization")
        let (data, resp): (Data, URLResponse)
        do { (data, resp) = try await URLSession.shared.data(for: req) }
        catch { throw BridgeError.network(String(describing: error)) }
        guard let http = resp as? HTTPURLResponse else { throw BridgeError.badResponse }
        guard http.statusCode == 200 else { throw daemonError(data, status: http.statusCode) }
        guard let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let token = obj["token"] as? String,
              let url = obj["url"] as? String else { throw BridgeError.badResponse }
        return ShareInfo(token: token, url: absolutizeShareURL(url, base: account.base))
    }

    static func deleteShare(account: Account, sessionId: String) async throws {
        var req = URLRequest(url: account.base
            .appendingPathComponent("sessions")
            .appendingPathComponent(sessionId)
            .appendingPathComponent("share"))
        req.httpMethod = "DELETE"
        req.setValue("Bearer \(account.token)", forHTTPHeaderField: "Authorization")
        let (data, resp): (Data, URLResponse)
        do { (data, resp) = try await URLSession.shared.data(for: req) }
        catch { throw BridgeError.network(String(describing: error)) }
        guard let http = resp as? HTTPURLResponse else { throw BridgeError.badResponse }
        guard http.statusCode < 300 else { throw daemonError(data, status: http.statusCode) }
    }

    static func getShare(account: Account, sessionId: String) async throws -> ShareInfo? {
        var req = URLRequest(url: account.base
            .appendingPathComponent("sessions")
            .appendingPathComponent(sessionId)
            .appendingPathComponent("share"))
        req.setValue("Bearer \(account.token)", forHTTPHeaderField: "Authorization")
        let (data, resp): (Data, URLResponse)
        do { (data, resp) = try await URLSession.shared.data(for: req) }
        catch { throw BridgeError.network(String(describing: error)) }
        guard let http = resp as? HTTPURLResponse else { throw BridgeError.badResponse }
        if http.statusCode == 404 { return nil }
        guard http.statusCode == 200 else { throw daemonError(data, status: http.statusCode) }
        guard let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let token = obj["token"] as? String,
              let url = obj["url"] as? String else { throw BridgeError.badResponse }
        return ShareInfo(token: token, url: absolutizeShareURL(url, base: account.base))
    }

    static func resolveShare(base: URL, token: String) async throws -> String {
        var req = URLRequest(url: base.appendingPathComponent("s").appendingPathComponent(token))
        let (data, resp): (Data, URLResponse)
        do { (data, resp) = try await URLSession.shared.data(for: req) }
        catch { throw BridgeError.network(String(describing: error)) }
        guard let http = resp as? HTTPURLResponse else { throw BridgeError.badResponse }
        guard http.statusCode == 200 else { throw daemonError(data, status: http.statusCode) }
        guard let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let sessionId = obj["session_id"] as? String, !sessionId.isEmpty else { throw BridgeError.badResponse }
        return sessionId
    }

    static func parseShareLink(_ raw: String) -> (base: URL, token: String)? {
        var s = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        if s.isEmpty { return nil }
        if !s.contains("://") { s = "https://" + s }
        guard let u = URL(string: s) else { return nil }
        let parts = u.path.split(separator: "/").map(String.init)
        guard let i = parts.firstIndex(of: "s"), i + 1 < parts.count else { return nil }
        let token = parts[i + 1]
        guard !token.isEmpty, let base = normalizeBase(s) else { return nil }
        return (base, token)
    }

    static func absolutizeShareURL(_ url: String, base: URL) -> String {
        let trimmed = url.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.hasPrefix("http://") || trimmed.hasPrefix("https://") { return trimmed }
        let root = base.absoluteString.trimmingCharacters(in: CharacterSet(charactersIn: "/"))
        if trimmed.hasPrefix("/") { return root + trimmed }
        return root + "/" + trimmed
    }

    static func daemonError(_ data: Data, status: Int) -> BridgeError {
        if let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
           let msg = obj["error"] as? String, !msg.isEmpty {
            return .daemon(msg)
        }
        return .server(httpStatusCode: status)
    }

    static func listSessions(account: Account) async throws -> [RemoteSessionMeta] {
        var req = URLRequest(url: account.base.appendingPathComponent("sessions"))
        req.setValue("Bearer \(account.token)", forHTTPHeaderField: "Authorization")
        let (data, resp) = try await URLSession.shared.data(for: req)
        guard let http = resp as? HTTPURLResponse, http.statusCode == 200 else { throw BridgeError.badResponse }
        let dec = JSONDecoder()
        dec.dateDecodingStrategy = .custom { d in
            let s = try d.singleValueContainer().decode(String.self)
            if let t = Self.parseRFC3339(s) { return t }
            throw DecodingError.dataCorrupted(.init(codingPath: d.codingPath, debugDescription: "bad date \(s)"))
        }
        return try dec.decode([RemoteSessionMeta].self, from: data)
    }

    static func listMachines(account: Account) async throws -> [MachineInfo] {
        var req = URLRequest(url: account.base.appendingPathComponent("machines"))
        req.setValue("Bearer \(account.token)", forHTTPHeaderField: "Authorization")
        let (data, resp) = try await URLSession.shared.data(for: req)
        guard let http = resp as? HTTPURLResponse, http.statusCode == 200 else { throw BridgeError.badResponse }
        return try JSONDecoder().decode([MachineInfo].self, from: data)
    }

    static func createSession(account: Account, machine: String? = nil, cwd: String, model: String?) async throws -> String {
        var req = URLRequest(url: account.base.appendingPathComponent("sessions"))
        req.httpMethod = "POST"
        req.setValue("Bearer \(account.token)", forHTTPHeaderField: "Authorization")
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = Wire.createSession(machine: machine, cwd: cwd, model: model).data(using: .utf8)
        let (data, resp) = try await URLSession.shared.data(for: req)
        guard let http = resp as? HTTPURLResponse else { throw BridgeError.badResponse }
        guard http.statusCode == 200,
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let id = obj["id"] as? String else {
            // Surface the daemon's own error text when present.
            if let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
               let msg = obj["error"] as? String {
                throw BridgeError.daemon(msg)
            }
            throw BridgeError.server(httpStatusCode: http.statusCode)
        }
        return id
    }

    static func deleteSession(account: Account, id: String) async throws {
        var req = URLRequest(url: account.base.appendingPathComponent("sessions/\(id)"))
        req.httpMethod = "DELETE"
        req.setValue("Bearer \(account.token)", forHTTPHeaderField: "Authorization")
        let (_, resp) = try await URLSession.shared.data(for: req)
        guard let http = resp as? HTTPURLResponse, http.statusCode < 300 else { throw BridgeError.badResponse }
    }

    enum BridgeError: LocalizedError {
        case invalidURL
        case invalidCredentials
        case network(String)
        case badResponse
        case daemon(String)
        case server(httpStatusCode: Int)

        var errorDescription: String? {
            switch self {
            case .invalidURL: return "that doesn't look like a cascade host url"
            case .invalidCredentials: return "invalid email or password"
            case .network(let why): return "can't reach the host — \(why)"
            case .badResponse: return "the host sent something unexpected"
            case .daemon(let msg): return msg
            case .server(let code): return "host error · HTTP \(code)"
            }
        }
    }

    static func parseRFC3339(_ s: String) -> Date? {
        let f = ISO8601DateFormatter()
        f.formatOptions = [.withInternetDateTime]
        if let d = f.date(from: s) { return d }
        f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return f.date(from: s)
    }

    /// The default Cascade cloud — what everyone sees unless they self-host.
    static let defaultBase = URL(string: "https://wickrunner.com:7701")!

    /// Parse a user-typed host field into a base URL; empty → the default cloud.
    static func normalizeBase(_ raw: String) -> URL? {
        var s = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        if s.isEmpty { return defaultBase }
        if !s.contains("://") { s = "https://" + s }
        guard let u = URL(string: s), let host = u.host, !host.isEmpty else { return nil }
        var comps = URLComponents(url: u, resolvingAgainstBaseURL: false)!
        if comps.scheme == "ws" { comps.scheme = "http" }
        if comps.scheme == "wss" { comps.scheme = "https" }
        comps.path = ""
        comps.query = nil
        comps.fragment = nil
        return comps.url
    }

    // MARK: lifecycle

    init(config: Config) {
        base = config.base
        token = config.token
        targetId = config.sessionId
        deviceName = config.name
        relay = base.host ?? "—"
        if let port = base.port { relay += ":\(port)" }
        sessionId = targetId
        title = "new session"
        cwd = "…"
        readOnly = config.readOnly
    }

    /// Attach to a `kind: terminal` session via an omp collab join/view handle.
    convenience init?(terminalLink: String, name: String) {
        guard case .success(let parsed) = CollabLinkParser.parse(terminalLink) else { return nil }
        self.init(config: Config(base: parsed.wsURL, token: "", sessionId: parsed.roomId, name: name))
        readOnly = parsed.writeToken == nil
        joinLink = terminalLink
        relay = (parsed.wsURL.host ?? "—") + (parsed.wsURL.port.map { ":\($0)" } ?? "")
        title = parsed.roomId
        let socket = CollabGuestSocket(link: parsed, name: name)
        guestSocket = socket
        socket.onOpen = { [weak self] in
            Task { @MainActor in
                guard let self else { return }
                self.phase = self.welcomed ? "reconnecting" : "waiting"
                self.rebuild()
            }
        }
        socket.onFrame = { [weak self] frame in
            Task { @MainActor in self?.applyGuestFrame(frame) }
        }
        socket.onControl = { [weak self] ctrl in
            Task { @MainActor in
                if ctrl["t"] as? String == "room-closed" { self?.end("room closed") }
            }
        }
        socket.onUnexpectedClose = { [weak self] reason, fatal in
            Task { @MainActor in
                guard let self else { return }
                if fatal {
                    self.end(reason)
                } else {
                    self.phase = "reconnecting"
                    self.rebuild()
                }
            }
        }
    }

    func connect() {
        if let guestSocket {
            guestSocket.connect()
            phase = welcomed ? "reconnecting" : "waiting"
            rebuild()
            return
        }
        openStream()
    }

    func close() {
        terminated = true
        reconnectTask?.cancel()
        receiveLoop = false
        guestSocket?.close()
        socketTask?.cancel(with: .goingAway, reason: nil)
        socketTask = nil
        socketSession?.invalidateAndCancel()
        socketSession = nil
    }

    func reconnectIfNeeded() {
        guard !terminated, phase == "reconnecting" || phase == "ended" else { return }
        reconnectTask?.cancel()
        reconnectAttempt = 0
        phase = "reconnecting"
        rebuild()
        if let guestSocket { guestSocket.reconnectNow(); return }
        openStream()
    }

    private func backoff(for attempt: Int) -> TimeInterval {
        TimeInterval([1, 2, 4, 8, 16][min(attempt, 4)])
    }

    private func scheduleReconnect(reason: String) {
        guard !terminated, phase != "ended" else { return }
        reconnectTask?.cancel()
        guard reconnectAttempt < 5 else {
            phase = "ended"; endedReason = "reconnect failed · \(reason)"; working = false
            rebuild()
            return
        }
        let delay = backoff(for: reconnectAttempt)
        reconnectAttempt += 1
        phase = "reconnecting"
        rebuild()
        reconnectTask = Task { [weak self] in
            try? await Task.sleep(nanoseconds: UInt64(delay * 1_000_000_000))
            guard !Task.isCancelled else { return }
            self?.openStream()
        }
    }

    // MARK: WebSocket — /sessions/{id}/stream

    private func openStream() {
        guard !terminated, guestSocket == nil else { return }
        var comps = URLComponents(url: base.appendingPathComponent("sessions/\(targetId)/stream"), resolvingAgainstBaseURL: false)!
        comps.scheme = comps.scheme == "https" ? "wss" : "ws"
        guard let wsURL = comps.url else {
            phase = "ended"; endedReason = "bad host url"; rebuild()
            return
        }
        var req = URLRequest(url: wsURL)
        req.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        let session = URLSession(configuration: .default, delegate: nil, delegateQueue: nil)
        socketSession = session
        let task = session.webSocketTask(with: req)
        socketTask = task
        phase = welcomed ? "reconnecting" : "waiting"
        rebuild()
        receiveLoop = true
        task.resume()
        pump(task)
    }

    private func pump(_ task: URLSessionWebSocketTask) {
        task.receive { [weak self] result in
            Task { @MainActor [weak self] in
                guard let self, self.receiveLoop, self.socketTask === task else { return }
                switch result {
                case .failure(let err):
                    self.scheduleReconnect(reason: err.localizedDescription)
                case .success(let msg):
                    switch msg {
                    case .string(let s):
                        self.applyFrameJSON(s)
                    case .data(let d):
                        if let s = String(data: d, encoding: .utf8) { self.applyFrameJSON(s) }
                    @unknown default:
                        break
                    }
                    if self.receiveLoop { self.pump(task) }
                }
            }
        }
    }

    private func send(_ text: String) {
        socketTask?.send(.string(text)) { [weak self] err in
            guard let err else { return }
            Task { @MainActor [weak self] in
                self?.scheduleReconnect(reason: err.localizedDescription)
            }
        }
    }

    // MARK: commands

    func sendPrompt(_ text: String, images: [(mime: String, base64: String)] = []) {
        guard !readOnly else { return }
        let clean = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !clean.isEmpty, images.isEmpty else { return }   // image send not supported by cascade yet
        if let guestSocket {
            guestSocket.send(["t": "prompt", "text": clean])
        } else {
            send(Wire.prompt(clean))
        }
        pendingSends.insert(clean)
        rebuild()
    }

    func sendAbort() {
        if let guestSocket {
            guestSocket.send(["t": "abort"])
        } else {
            send(Wire.abort())
        }
        activity = "ABORTING…"
        rebuild()
    }

    /// Answer a pending ui_request by its cascade request id. `nil` value cancels.
    func answer(reqId: String, value: String?) {
        pendingRequest = nil
        if let guestSocket {
            var frame: [String: Any] = ["t": "ui-response"]
            if let n = Int(reqId) { frame["reqId"] = n } else { frame["reqId"] = reqId }
            if let value { frame["value"] = value }
            guestSocket.send(frame)
        } else {
            send(Wire.answer(requestId: reqId, value: value))
        }
        rebuild()
    }

    /// Confirm-style asks (method == "confirm"): yes/no without free text.
    func confirm(reqId: String, _ yes: Bool) {
        pendingRequest = nil
        if let guestSocket {
            var frame: [String: Any] = ["t": "ui-response", "value": yes ? "Yes" : "No"]
            if let n = Int(reqId) { frame["reqId"] = n } else { frame["reqId"] = reqId }
            guestSocket.send(frame)
        } else {
            send(Wire.answer(requestId: reqId, confirmed: yes))
        }
        rebuild()
    }

    // MARK: model / thinking control

    /// Switch model. `provider` + `modelId` map 1:1 to omp's set_model RPC.
    func setModel(provider: String, modelId: String) {
        guard !readOnly else { return }
        send(Wire.setModel(provider: provider, modelId: modelId))
    }

    func setThinking(_ level: String) {
        guard !readOnly else { return }
        send(Wire.setThinking(level))
    }

    /// Menu action: drop the stream and reattach, pulling a fresh snapshot.
    func resync() {
        guard !terminated else { return }
        if let guestSocket {
            guestSocket.reconnectNow()
            return
        }
        receiveLoop = false
        socketTask?.cancel(with: .goingAway, reason: nil)
        socketTask = nil
        openStream()
    }

    /// Ask the daemon to re-emit state (model/thinking) on this stream.
    func refreshState() {
        send(Wire.getState())
    }

    /// Adopt RpcSessionState JSON: model {provider,id,name…}, thinkingLevel.
    private func absorbState(_ st: [String: Any]?) {
        guard let st else { return }
        if let m = st["model"] as? [String: Any] {
            let provider = m["provider"] as? String ?? ""
            let name = m["name"] as? String ?? m["id"] as? String ?? ""
            if !provider.isEmpty { providerName = provider }
            if !name.isEmpty { modelName = name }
        }
        if let lvl = st["thinkingLevel"] as? String { thinkingLevel = lvl }
    }

    private func applyFrameJSON(_ s: String) {
        guard let d = s.data(using: .utf8), let f = Wire.event(from: d), let kind = f["kind"] as? String else { return }
        applyFrame(kind, f)
    }

    /// Collab host frame → existing SessionEvent projection.
    fileprivate func applyGuestFrame(_ frame: [String: Any]) {
        let t = frame["t"] as? String ?? ""
        if t == "welcome" {
            guestMapper.reset()
            welcomed = true
            reconnectAttempt = 0
            endedReason = nil
            if let header = frame["header"] as? [String: Any] {
                if let n = header["title"] as? String, !n.isEmpty { title = n }
                if let id = header["id"] as? String, !id.isEmpty { sessionId = id }
                if let c = header["cwd"] as? String, !c.isEmpty { cwd = c }
            }
            if let st = frame["state"] as? [String: Any] {
                absorbCollabState(st)
            }
            if let ro = frame["readOnly"] as? Bool { readOnly = ro }
            messages = []
            streamText = ""; streamThinking = ""; streamDone = false
            activeTools = []; pendingRequest = nil
            phase = "live"
        }
        if t == "state", let st = frame["state"] as? [String: Any] {
            absorbCollabState(st)
        }
        for ev in guestMapper.mapFrame(frame) {
            guard let kind = ev["kind"] as? String else { continue }
            if kind == "snapshot" && t == "welcome" {
                welcomed = true
                phase = "live"
                continue
            }
            applyFrame(kind, ev)
        }
    }

    private func absorbCollabState(_ st: [String: Any]) {
        working = st["isStreaming"] as? Bool ?? working
        if st["isAborting"] as? Bool == true { activity = "ABORTING…" }
        queued = st["queuedMessageCount"] as? Int ?? queued
        if let n = st["sessionName"] as? String, !n.isEmpty { title = n }
        if let c = st["cwd"] as? String, !c.isEmpty { cwd = c }
        if let level = st["thinkingLevel"] as? String { thinkingLevel = level }
        if let m = st["model"] as? [String: Any] {
            if let name = m["name"] as? String, !name.isEmpty { modelName = name }
            if let provider = m["provider"] as? String, !provider.isEmpty { providerName = provider }
        }
        if let list = st["participants"] as? [[String: Any]] {
            participants = list.map { p in
                ParticipantInfo(name: p["name"] as? String ?? "peer",
                                role: p["role"] as? String ?? "guest",
                                readOnly: p["readOnly"] as? Bool ?? false)
            }
        }
    }

    private func applyFrame(_ kind: String, _ f: [String: Any]) {
        switch kind {
        case "snapshot":
            if let msgs = f["messages"] as? [[String: Any]] { messages = msgs }
            if let phases = f["todos"] as? [[String: Any]] { plan = Self.parsePlan(phases) }
            working = f["streaming"] as? Bool ?? false
            welcomed = true
            phase = "live"
            if guestSocket == nil { refreshState() }   // cloud only — guest has no get_state RPC
        case "message_start":
            break   // role-only; content arrives via deltas or MessageEnd
        case "text_delta":
            streamDone = false
            streamText += f["delta"] as? String ?? ""
            scheduleStreamRebuild()
            return   // delta frames are coalesced; skip the immediate rebuild below
        case "thinking_delta":
            streamDone = false
            streamThinking += f["delta"] as? String ?? ""
            scheduleStreamRebuild()
            return
        case "state_changed":
            if let st = f["state"] as? [String: Any] { absorbState(st) }
            if let list = f["models"] as? [[String: Any]] {
                availableModels = list.compactMap { m in
                    guard let provider = m["provider"] as? String,
                          let id = m["id"] as? String else { return nil }
                    let name = m["name"] as? String ?? id
                    return ModelOption(modelId: id, name: name, provider: provider)
                }
            }
            rebuild()
            return
        case "message_end":
            if let m = f["message"] as? [String: Any] { messages.append(m); absorbMessage(m) }
            streamText = ""; streamThinking = ""; streamDone = true
        case "tool_start":
            if let id = f["tool_call_id"] as? String {
                activeTools.removeAll { $0.id == id }
                var tool: [String: Any] = [
                    "toolName": f["tool_name"] as? String ?? "tool",
                    "args": f["args"] ?? NSNull(),
                ]
                if let intent = f["intent"] as? String { tool["intent"] = intent }
                activeTools.append((id, tool))
            }
        case "tool_update":
            if let id = f["tool_call_id"] as? String,
               let i = activeTools.firstIndex(where: { $0.id == id }) {
                // partial results stream into the same slot; keep it simple — mark activity
                activity = activity ?? "TOOL RUNNING…"
            }
        case "tool_end":
            if let id = f["tool_call_id"] as? String {
                let name = f["tool_name"] as? String ?? "tool"
                let isError = f["is_error"] as? Bool ?? false
                let result = f["result"]
                // Finalized toolResult message in omp shape so the projection reuses it.
                let msg: [String: Any] = [
                    "role": "toolResult",
                    "toolCallId": id,
                    "toolName": name,
                    "isError": isError,
                    "content": (result as? [AnyHashable: Any])?["content"] ?? result ?? NSNull(),
                    "details": (result as? [AnyHashable: Any])?["details"] ?? NSNull(),
                ]
                messages.append(msg)
                absorbToolResult(msg)
                activeTools.removeAll { $0.id == id }
            }
        case "agent_start":
            if !working {
                working = true
                streamText = ""; streamThinking = ""; streamDone = false
            }
            activity = nil
        case "agent_end":
            working = false
            streamText = ""; streamThinking = ""
            activity = nil
            pendingSends.removeAll()
        case "todo_changed":
            if let phases = f["phases"] as? [[String: Any]] { plan = Self.parsePlan(phases) }
        case "ui_request":
            // Only real user-facing asks get cards: set_widget/set_title/notify and
            // friends are chrome-only side channels that must not block the transcript.
            if let req = decodeUiRequest(f), ["select", "confirm", "input", "editor"].contains(req.method) {
                pendingRequest = req
            }
        case "ui_request_cancelled":
            if let tid = f["target_id"] as? String, pendingRequest?.id == tid { pendingRequest = nil }
        case "notice":
            let level = f["level"] as? String ?? "info"
            let msg = f["message"] as? String ?? ""
            if !msg.isEmpty {
                notices.append(NoticeItem(id: "\(notices.count)-\(msg.hashValue)", level: level, message: msg))
                if notices.count > 20 { notices.removeFirst(notices.count - 20) }
            }
        case "session_info":
            if let n = f["title"] as? String, !n.isEmpty { title = n }
        case "turn_started":
            working = true
        case "process_exited":
            end("daemon exited the session process" + ((f["code"] as? Int).map { " (\($0))" } ?? ""))
        default:
            break
        }
        rebuild()
    }

    private func decodeUiRequest(_ f: [String: Any]) -> CascadeUiRequest? {
        guard let id = f["id"] as? String, !id.isEmpty else { return nil }
        return CascadeUiRequest(
            id: id,
            method: f["method"] as? String ?? "other",
            title: f["title"] as? String,
            message: f["message"] as? String,
            options: f["options"] as? [String] ?? [],
            placeholder: f["placeholder"] as? String,
            prefill: f["prefill"] as? String,
            url: f["url"] as? String,
            timeout_secs: f["timeout_secs"] as? Int)
    }

    private func end(_ reason: String) {
        if phase == "ended" { return }
        terminated = true
        reconnectTask?.cancel()
        phase = "ended"
        endedReason = reason
        working = false
        currentMode = nil
        receiveLoop = false
        guestSocket?.close()
        socketTask?.cancel(with: .goingAway, reason: nil)
        rebuild()
    }

    // MARK: projection — cascade transcript → [UITurn]

    /// Adopt per-message state from a finalized assistant/user message.
    private func absorbMessage(_ m: [String: Any]) {
        let role = m["role"] as? String ?? ""
        if role == "assistant", let model = m["model"] as? String, modelName == "—" { modelName = model }
        if let stop = m["stopReason"] as? String, stop == "error", working { working = false }
    }

    /// Adopt session metadata from a finished toolResult (cwd-ish info arrives this way).
    private func absorbToolResult(_ m: [String: Any]) {}

    private struct StaticRebuild {
        let turns: [UITurn]
        let latestPlanUsed = false
    }

    private func rebuild() {
        streamRebuildPending = false
        let entriesUnchanged = messages.count == cachedMessageCount && !cachedStaticTurns.isEmpty

        let staticTurns: [UITurn]
        if entriesUnchanged {
            staticTurns = cachedStaticTurns
        } else {
            staticTurns = buildStaticTurns()
            cachedStaticTurns = staticTurns
            cachedMessageCount = messages.count
        }

        let tail = buildTail(staticTurns: staticTurns)

        // Model chips only earn their space when the session actually used >1 model.
        var combined = staticTurns + tail
        if Set(combined.compactMap { $0.model.isEmpty ? nil : $0.model }).count <= 1 {
            for i in combined.indices { combined[i].model = "" }
        }

        cachedStaticTurns = Array(combined.prefix(staticTurns.count))
        cachedTailCount = tail.count
        if turns != combined { turns = combined }

        if working, let first = combined.last(where: { $0.type == .user }), tokensLabel == "—" {
            // cheap context hint until real usage numbers arrive over the wire
            tokensLabel = "—"
        }

        onChange?()
    }

    private func buildStaticTurns() -> [UITurn] {
        var out: [UITurn] = []
        var toolIndex: [String: Int] = [:]

        if welcomed && justPaired { out.append(UITurn.sys("paired", "SESSION STARTED")) }

        for entry in messages {
            let eid = messageId(entry)
            let role = entry["role"] as? String ?? ""
            switch role {
            case "user":
                out.append(userTurn(id: eid, content: entry["content"]))
            case "assistant":
                let msgModel = entry["model"] as? String ?? ""
                for (i, block) in (entry["content"] as? [[String: Any]] ?? []).enumerated() {
                    switch block["type"] as? String {
                    case "text":
                        let text = block["text"] as? String ?? ""
                        if !text.isEmpty { out.append(agentTurn(id: "\(eid)#\(i)", text: text, model: msgModel)) }
                    case "toolCall":
                        let name = block["name"] as? String ?? "tool"
                        if name == "todo" { break }
                        let id = block["id"] as? String ?? "\(eid)#\(i)"
                        out.append(toolTurn(id: id, name: name, args: block["arguments"], intent: block["intent"] as? String))
                        toolIndex[id] = out.count - 1
                    case "thinking", "redactedThinking", "reasoning":
                        if let think = thinkingContent(from: block) {
                            out.append(thinkingTurn(id: "\(eid)#\(i)", text: think, seconds: nil, model: msgModel))
                        }
                    default: break
                    }
                }
                if let err = entry["errorMessage"] as? String, !err.isEmpty {
                    out.append(UITurn.sys("error", "ERROR · " + err))
                } else if (entry["stopReason"] as? String) == "error" {
                    out.append(UITurn.sys("error", "TURN FAILED — SEE THE HOST"))
                }
            case "toolResult":
                let id = entry["toolCallId"] as? String ?? eid
                if (entry["toolName"] as? String) == "todo" { break }   // plan handled via todo_changed
                let isError = entry["isError"] as? Bool ?? false
                let details = entry["details"] as? [String: Any]
                if let idx = toolIndex[id] {
                    fillResult(&out[idx], content: entry["content"], isError: isError, details: details, kind: out[idx].kind)
                } else {
                    var turn = toolTurn(id: id, name: entry["toolName"] as? String ?? "tool", args: nil, intent: nil)
                    fillResult(&turn, content: entry["content"], isError: isError, details: details, kind: turn.kind)
                    out.append(turn)
                }
            default:
                // Unknown roles render as system notes rather than vanishing.
                if let text = contentString(entry["content"]).isEmpty ? nil : contentString(entry["content"]) {
                    out.append(UITurn.sys("note", text))
                }
            }
        }
        return out
    }

    private func buildTail(staticTurns: [UITurn]) -> [UITurn] {
        var out: [UITurn] = []
        var toolIndex: [String: Int] = [:]
        for (i, turn) in staticTurns.enumerated() where turn.type == .tool {
            toolIndex[turn.id] = i
        }

        // Optimistic echo of your own prompt until the daemon streams the user message back.
        for p in pendingSends.sorted() {
            var t = UITurn(id: "pending-\(p.hashValue)", type: .user)
            t.text = p
            t.pending = true
            out.append(t)
        }

        // Executing tools with no result yet.
        for (id, tool) in activeTools where toolIndex[id] == nil {
            var turn = toolTurn(id: id, name: tool["toolName"] as? String ?? "tool", args: tool["args"], intent: tool["intent"] as? String)
            turn.pending = true
            out.append(turn)
        }

        // Streaming assistant ghost.
        if working || !streamText.isEmpty {
            if !streamThinking.isEmpty {
                out.append(UIThinkingGhost(id: "stream-think", text: streamThinking))
            }
            if !streamText.isEmpty {
                var turn = agentTurn(id: "stream", text: streamText)
                turn.streaming = !streamDone
                out.append(turn)
            }
        }

        // Pending ui_request (select/confirm/input/editor) as an ask card.
        if let req = pendingRequest {
            let turn = askTurn(for: req)
            out.append(turn)
        }

        // Notices at the bottom of the scroll.
        for n in notices { out.append(UITurn.sys(n.level == "error" ? "error" : "notice", n.message)) }

        return out
    }

    private func UIThinkingGhost(id: String, text: String) -> UITurn {
        var t = UITurn(id: id, type: .thinking)
        t.text = text
        return t
    }

    /// Map a cascade UiRequest onto the existing AskCard vocabulary.
    private func askTurn(for req: CascadeUiRequest) -> UITurn {
        var t = UITurn(id: "ui-\(req.id)", type: .ask)
        t.reqKey = req.id
        t.askKind = askKind(for: req.method)
        t.question = req.title ?? req.message ?? "The agent is asking…"
        t.helpText = req.message ?? ""
        t.prefill = req.prefill ?? req.placeholder ?? ""
        if t.askKind == "confirm" {
            t.options = ["Yes", "No"]
            t.selectionMarker = "radio"
            t.initialIndex = nil
        } else if t.askKind == "editor" {
            t.selectionMarker = "radio"
        } else {
            t.options = req.options
            t.optionDescriptions = req.options.map { _ in "" }
            t.selectionMarker = "radio"
        }
        return t
    }

    private func askKind(for method: String) -> String {
        switch method {
        case "confirm": return "confirm"
        case "input", "editor": return "editor"
        case "select": return "select"
        default: return "select"
        }
    }

    // MARK: turn builders (shared vocabulary with the old projection)

    private func messageId(_ m: [String: Any]) -> String {
        if let id = m["id"] as? String { return id }
        return "msg-\(ObjectIdentifier(m as AnyObject).hashValue)"
    }

    private func userTurn(id: String, content: Any?) -> UITurn {
        var t = UITurn(id: id, type: .user)
        t.text = contentString(content)
        t.image = firstImage(content)
        return t
    }
    private func thinkingTurn(id: String, text: String, seconds: Int? = nil, model: String = "") -> UITurn {
        var t = UITurn(id: id, type: .thinking); t.text = text; t.thoughtSeconds = seconds; t.model = model; return t
    }
    private func agentTurn(id: String, text: String, model: String = "") -> UITurn {
        var t = UITurn(id: id, type: .agent); t.text = text; t.model = model; return t
    }
    private func toolTurn(id: String, name: String, args: Any?, intent: String?) -> UITurn {
        var t = UITurn(id: id, type: .tool)
        t.kind = toolKind(name)
        t.head = name
        t.meta = argSummary(args) ?? intent ?? ""
        return t
    }

    private func fillResult(_ turn: inout UITurn, content: Any?, isError: Bool, details: [String: Any]?, kind: String) {
        turn.pending = false
        if let img = firstImage(content) { turn.image = img }
        let text = contentString(content)
        if !text.isEmpty {
            let lines = text.split(separator: "\n", omittingEmptySubsequences: false).map(String.init)
            if lines.count == 1 && lines[0].count <= 80 && turn.meta.isEmpty { turn.meta = lines[0] }
            else { turn.lines = Array(lines.prefix(14)) }
        }
        if isError && turn.meta.isEmpty { turn.meta = "error" }
        if let details, kind == "edit" || kind == "ast_edit" {
            if let diff = details["diff"] as? String, !diff.isEmpty {
                turn.diff = diff
                turn.diffLang = SyntaxHighlighter.languageFromPath(details["path"] as? String ?? "") ?? ""
            } else if let displayContent = details["displayContent"] as? String, !displayContent.isEmpty {
                turn.diff = displayContent
                turn.diffLang = SyntaxHighlighter.languageFromPath(details["path"] as? String ?? "") ?? ""
            }
            if let perFileResults = details["perFileResults"] as? [[String: Any]] {
                turn.perFileDiffs = perFileResults.compactMap { entry in
                    let path = entry["path"] as? String ?? ""
                    let diff = entry["diff"] as? String ?? ""
                    guard !path.isEmpty || !diff.isEmpty else { return nil }
                    let isErr = entry["isError"] as? Bool ?? false
                    let errorText = entry["errorText"] as? String
                    let lang = SyntaxHighlighter.languageFromPath(path) ?? ""
                    return UIToolFileDiff(path: path, diff: diff, lang: lang, isError: isErr, errorText: errorText)
                }
            }
        }
    }

    // ── content helpers ───────────────────────────────────────────────────────

    private func contentString(_ content: Any?) -> String {
        if let s = content as? String { return s }
        if let arr = content as? [[String: Any]] {
            return arr.compactMap { $0["type"] as? String == "text" ? $0["text"] as? String : nil }.joined(separator: "\n")
        }
        return ""
    }

    private func firstImage(_ content: Any?) -> String? {
        guard let arr = content as? [[String: Any]] else { return nil }
        for block in arr where block["type"] as? String == "image" {
            if let data = block["data"] as? String, let mime = block["mimeType"] as? String {
                return "data:\(mime);base64,\(data)"
            }
        }
        return nil
    }

    private func isThinkingBlock(_ b: [String: Any]) -> Bool {
        switch b["type"] as? String {
        case "thinking", "redactedThinking", "reasoning": return true
        default: return false
        }
    }

    private func thinkingContent(from b: [String: Any]) -> String? {
        guard isThinkingBlock(b) else { return nil }
        let text = b["thinking"] as? String ?? b["text"] as? String ?? b["data"] as? String ?? b["content"] as? String ?? b["reasoning"] as? String
        return text?.isEmpty == false ? text : nil
    }

    private func toolKind(_ name: String) -> String {
        let n = name.lowercased()
        if n.contains("read") || n.contains("cat") { return "read" }
        if n.contains("grep") || n.contains("search") || n.contains("glob") || n.contains("find") { return "search" }
        if n.contains("ast_edit") { return "ast_edit" }
        if n.contains("edit") || n.contains("write") || n.contains("apply") { return "edit" }
        if n.contains("bash") || n.contains("shell") || n.contains("exec") || n.contains("eval") { return "bash" }
        if n.contains("lsp") || n.contains("diagnos") { return "lsp" }
        if n.contains("task") || n.contains("agent") || n.contains("spawn") { return "task" }
        if n.contains("debug") || n.contains("dap") || n.contains("lldb") { return "debug" }
        if n.contains("inspect") { return "inspect" }
        if n.contains("image") || n.contains("photo") { return "image" }
        return n
    }

    private func argSummary(_ args: Any?) -> String? {
        guard let dict = args as? [String: Any] else { return nil }
        for key in ["path", "file", "filePath", "file_path", "command", "cmd", "pattern", "query", "url", "name"] {
            if let v = dict[key] as? String, !v.isEmpty { return v }
        }
        if let data = try? JSONSerialization.data(withJSONObject: dict),
           let s = String(data: data, encoding: .utf8), s.count <= 60 { return s }
        return nil
    }

    static func parsePlan(_ phases: [[String: Any]]) -> [PlanPhase] {
        phases.map { ph in
            let tasks = (ph["tasks"] as? [[String: Any]] ?? []).map {
                PlanTask(content: $0["content"] as? String ?? "", status: $0["status"] as? String ?? "pending")
            }
            return PlanPhase(name: ph["name"] as? String ?? "", tasks: tasks)
        }
    }

    private func scheduleStreamRebuild() {
        let elapsed = Date().timeIntervalSince(lastStreamRebuild)
        if elapsed >= streamCoalesceInterval {
            lastStreamRebuild = Date()
            rebuild()
        } else if !streamRebuildPending {
            streamRebuildPending = true
            let delay = streamCoalesceInterval - elapsed
            Task { @MainActor [weak self] in
                try? await Task.sleep(nanoseconds: UInt64(delay * 1_000_000_000))
                guard let self, self.streamRebuildPending else { return }
                self.streamRebuildPending = false
                self.lastStreamRebuild = Date()
                self.rebuild()
            }
        }
    }
}

import CoreGraphics
