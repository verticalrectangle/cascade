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
    var live: Bool?
    var working: Bool?
    var empty: Bool?
    var origin: String?
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
    static func getSnapshot(limit: UInt32 = 100, before: UInt64?) -> String {
        var o: [String: Any] = ["kind": "get_snapshot", "limit": Int(limit)]
        if let before { o["before"] = Int(before) }
        return body(o)
    }
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
    @Published private(set) var phase: String = "waiting"      // waiting/live/reconnecting/ended
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
    /// Guest is a prompt/abort/answer write channel; transcript stays on the cloud stream.
    private var guestWriteToken = false
    private let pagedSnapshot: Bool
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
    private var pendingSentAt: [String: Date] = [:]

    // MARK: message identity — snapshot, reconnect replay, room and cloud
    // frames must all collapse to ONE render. Port of the GTK fingerprint
    // dedupe: without it every resync appends another copy of the tail and
    // every dual-channel frame renders twice.
    private var seenFingerprints: Set<String> = []
    private var fingerprintOrder: [String] = []        // rolling eviction, cap 64
    private var streamOpen = false                     // framing: deltas render only between start/end
    private var roomLive = false                       // guest welcomed → cloud deltas filtered out
    private var roomDiedAt: Date?                      // guest died → watchdog catches up the cloud gap
    private var lastEventAt = Date()
    private var lastResyncAt = Date.distantPast
    private var watchdogTask: Task<Void, Never>?
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
    private var historyOldestIndex: UInt64 = 0
    private var historyTotal: UInt64 = 0
    @Published private(set) var historyHasMore = false
    private var historyLoading = false
    private var streamRebuildPending = false
    private var lastStreamRebuild: Date = .distantPast
    private let streamCoalesceInterval: TimeInterval = 1.0 / 30.0

    struct Config {
        let base: URL
        let token: String
        let sessionId: String
        let name: String
        let readOnly: Bool
        let paged: Bool
        let title: String
        let cwd: String
        /// Pre-set when attaching to an existing session (skips login+list).
        init(base: URL, token: String, sessionId: String, name: String, readOnly: Bool = false, paged: Bool = true, title: String = "", cwd: String = "") {
            self.base = base; self.token = token; self.sessionId = sessionId; self.name = name; self.title = title
            self.readOnly = readOnly
            self.paged = paged
            self.cwd = cwd
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

    enum ShareExpiry: Equatable {
        case hours(Int)
        case forever
        case unknown

        var stopSharingLabel: String {
            switch self {
            case .hours(let h): return "Stop sharing (\(h)h)"
            case .forever: return "Stop sharing (forever)"
            case .unknown: return "Stop sharing"
            }
        }
    }

    struct ShareInfo: Equatable {
        let token: String
        let url: String
        var expiry: ShareExpiry = .unknown

        func mergingExpiry(_ previous: ShareInfo?) -> ShareInfo {
            if expiry != .unknown { return self }
            guard let previous else { return self }
            return ShareInfo(token: token, url: url, expiry: previous.expiry)
        }
    }

    private static func shareExpiry(from obj: [String: Any], fallback: ShareExpiry) -> ShareExpiry {
        if obj["expires_in_hours"] is NSNull { return .forever }
        if let hours = obj["expires_in_hours"] as? Int { return .hours(hours) }
        if let hours = obj["expires_in_hours"] as? Double { return .hours(Int(hours)) }
        if obj["expires_at"] is NSNull { return .forever }
        if let raw = obj["expires_at"] as? String {
            let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
            if trimmed.isEmpty || trimmed.lowercased() == "never" { return .forever }
            return fallback == .unknown ? .hours(24) : fallback
        }
        return fallback
    }

    private static func decodeShareInfo(_ obj: [String: Any], base: URL, fallback: ShareExpiry) -> ShareInfo? {
        guard let token = obj["token"] as? String, let url = obj["url"] as? String else { return nil }
        return ShareInfo(token: token, url: absolutizeShareURL(url, base: base),
                         expiry: shareExpiry(from: obj, fallback: fallback))
    }

    static func createShare(account: Account, sessionId: String, expiresInHours: Int?) async throws -> ShareInfo {
        var req = URLRequest(url: account.base
            .appendingPathComponent("sessions")
            .appendingPathComponent(sessionId)
            .appendingPathComponent("share"))
        req.httpMethod = "POST"
        req.setValue("Bearer \(account.token)", forHTTPHeaderField: "Authorization")
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        let payload: [String: Any]
        if let hours = expiresInHours {
            payload = ["expires_in_hours": hours]
        } else {
            payload = ["expires_in_hours": NSNull()]
        }
        req.httpBody = Wire.body(payload).data(using: .utf8)
        let (data, resp): (Data, URLResponse)
        do { (data, resp) = try await URLSession.shared.data(for: req) }
        catch { throw BridgeError.network(String(describing: error)) }
        guard let http = resp as? HTTPURLResponse else { throw BridgeError.badResponse }
        guard http.statusCode == 200 else { throw daemonError(data, status: http.statusCode) }
        guard let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let info = decodeShareInfo(obj, base: account.base,
                                         fallback: expiresInHours.map { .hours($0) } ?? .forever)
        else { throw BridgeError.badResponse }
        return info
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
              let info = decodeShareInfo(obj, base: account.base, fallback: .unknown)
        else { throw BridgeError.badResponse }
        return info
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
        pagedSnapshot = config.paged
        base = config.base
        token = config.token
        targetId = config.sessionId
        deviceName = config.name
        relay = base.host ?? "—"
        if let port = base.port { relay += ":\(port)" }
        sessionId = targetId
        // GTK seeds the header from the sessions rail; the cloud snapshot and
        // the prompt-only guest channel carry no title, so without this the
        // editor header stays on the "new session" placeholder forever.
        title = config.title.isEmpty ? "new session" : config.title
        cwd = config.cwd.isEmpty ? "…" : config.cwd
        readOnly = config.readOnly
        phase = "waiting"
    }

    /// Attach to a `kind: terminal` session via an omp collab join/view handle.
    convenience init?(terminalLink: String, name: String) {
        guard case .success(let parsed) = CollabLinkParser.parse(terminalLink) else { return nil }
        self.init(config: Config(base: parsed.wsURL, token: "", sessionId: parsed.roomId, name: name))
        guestWriteToken = parsed.writeToken != nil
        readOnly = parsed.writeToken == nil
        joinLink = terminalLink
        relay = (parsed.wsURL.host ?? "—") + (parsed.wsURL.port.map { ":\($0)" } ?? "")
        title = parsed.roomId
        wireGuestSocket(CollabGuestSocket(link: parsed, name: name))
    }

    /// Dual-channel: keep the cloud transcript attach and open the collab room
    /// as a guest write channel for prompts. Composer stays parked (read-only)
    /// until the room welcomes a writable guest.
    func attachPromptChannel(_ raw: String) {
        guard case .success(let parsed) = CollabLinkParser.parse(raw) else { return }
        guestWriteToken = parsed.writeToken != nil
        joinLink = raw
        readOnly = true
        wireGuestSocket(CollabGuestSocket(link: parsed, name: deviceName))
        guestSocket?.connect()
    }

    /// Late adoption: the session row often resolves AFTER connect() fires
    /// (launch-attach races the list load). Fill the real title and wire the
    /// room guest if the row carries a handle the initial attach never saw.
    func adoptRow(_ row: JoinedSession) {
        guard !terminated, sessionId == row.id else { return }
        if !row.title.isEmpty { title = row.title }
        if !row.cwd.isEmpty { cwd = row.cwd }
        if !row.relay.isEmpty { relay = row.relay }
        if guestSocket == nil, let handle = row.joinHandle ?? row.viewHandle, !handle.isEmpty {
            attachPromptChannel(handle)
            rebuild()
        }
    }

    private func wireGuestSocket(_ socket: CollabGuestSocket) {
        guestSocket = socket
        socket.onOpen = { [weak self] in
            Task { @MainActor in
                guard let self else { return }
                if self.phase == "live" { self.phase = "reconnecting" }
                self.rebuild()
            }
        }
        socket.onFrame = { [weak self] frame in
            Task { @MainActor in self?.applyGuestFrame(frame) }
        }
        socket.onControl = { [weak self] ctrl in
            Task { @MainActor in
                guard let self, (ctrl["t"] as? String) == "room-closed" else { return }
                self.roomClosed()
            }
        }
        socket.onUnexpectedClose = { [weak self] _, fatal in
            Task { @MainActor in
                guard let self else { return }
                // The room is the primary live channel; its death un-filters
                // the cloud stream immediately (dedupe keeps the overlap
                // single-rendered). The watchdog pulls a catch-up snapshot
                // if the guest doesn't come back.
                self.roomLive = false
                self.roomDiedAt = Date()
                if fatal { self.readOnly = true }
                if self.phase == "live" && !fatal { self.phase = "reconnecting" }
                self.rebuild()
            }
        }
    }

    /// Room gone. Cloud-only fallback keeps the session usable; with no
    /// cloud stream there is nothing left to hold on to.
    private func roomClosed() {
        roomLive = false
        roomDiedAt = nil
        readOnly = true
        if hasCloudStream {
            resync()
        } else {
            end("room closed")
        }
        rebuild()
    }

    private var hasCloudStream: Bool { !token.isEmpty }

    func connect() {
        if hasCloudStream {
            openStream()
        }
        if let guestSocket {
            guestSocket.connect()
            if !hasCloudStream {
                phase = welcomed ? "reconnecting" : "waiting"
                rebuild()
            }
        }
        startWatchdog()
    }

    /// Health loop. Sockets die silently on iOS (backgrounding, network
    /// handoffs): no error surfaces, the receive loop just never fires.
    /// Symptoms rather than promises: a streaming agent that goes quiet, or
    /// a prompt with no echo, both mean the channel is a corpse — pull a
    /// fresh snapshot. A dead room guest gets the same catch-up if it
    /// doesn't reconnect on its own.
    private func startWatchdog() {
        watchdogTask?.cancel()
        watchdogTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: 4_000_000_000)
                await MainActor.run { [weak self] in
                    guard let self, !self.terminated, self.phase != "ended" else { return }
                    let now = Date()
                    if let died = self.roomDiedAt, now.timeIntervalSince(died) > 8 {
                        self.roomDiedAt = nil
                        self.resync(force: true)
                        return
                    }
                    if self.working, now.timeIntervalSince(self.lastEventAt) > 12 {
                        self.resync(force: true)
                        return
                    }
                    if let oldest = self.pendingSentAt.values.min(),
                       now.timeIntervalSince(oldest) > 6 {
                        self.resync(force: true)
                    }
                }
            }
        }
    }

    func close() {
        terminated = true
        watchdogTask?.cancel()
        reconnectTask?.cancel()
        receiveLoop = false
        guestSocket?.close()
        socketTask?.cancel(with: .goingAway, reason: nil)
        socketTask = nil
        socketSession?.invalidateAndCancel()
        socketSession = nil
    }

    private func backoff(for attempt: Int) -> TimeInterval {
        TimeInterval([1, 2, 4, 8, 16, 30][min(attempt, 5)])
    }

    private func scheduleReconnect(reason: String) {
        guard !terminated, phase != "ended" else { return }
        reconnectTask?.cancel()
        // Never give up: a phone outlives WiFi→cellular handoffs, sleep-wake
        // cycles, and dead NATs. Backoff caps at 30s and retries forever.
        let delay = backoff(for: reconnectAttempt)
        reconnectAttempt += 1
        if phase == "live" { phase = "reconnecting" }
        rebuild()
        reconnectTask = Task { [weak self] in
            try? await Task.sleep(nanoseconds: UInt64(delay * 1_000_000_000))
            guard !Task.isCancelled else { return }
            self?.openStream()
        }
    }

    // MARK: WebSocket — /sessions/{id}/stream

    private func openStream() {
        guard !terminated, hasCloudStream else { return }
        socketTask?.cancel(with: .goingAway, reason: nil)
        var comps = URLComponents(url: base.appendingPathComponent("sessions/\(targetId)/stream"), resolvingAgainstBaseURL: false)!
        comps.scheme = comps.scheme == "https" ? "wss" : "ws"
        // Tail-only initial snapshot: a full transcript frame blows past
        // URLSessionWebSocketTask's frame limit ("Message too long" → silent
        // empty editor). Share-token attaches keep the full snapshot.
        // Dual-channel discovered attach stays paged even while the composer
        // is parked waiting for the collab room.
        if pagedSnapshot {
            comps.queryItems = (comps.queryItems ?? []) + [URLQueryItem(name: "tail", value: "100")]
        }
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
        pendingSentAt[clean] = Date()
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

    /// Drop both channels and reattach, pulling a fresh snapshot. Menu
    /// action, and the foreground resync: a suspended app's sockets die
    /// silently (phase stays "live", the receive loop never fires again),
    /// so returning to the foreground must force a reopen.
    func resync(force: Bool = false) {
        guard !terminated else { return }
        if !force && Date().timeIntervalSince(lastResyncAt) < 10 { return }
        lastResyncAt = Date()
        reconnectTask?.cancel()
        reconnectAttempt = 0
        guestSocket?.reconnectNow()
        guard hasCloudStream else { return }
        receiveLoop = false
        socketTask?.cancel(with: .goingAway, reason: nil)
        socketTask = nil
        openStream()
    }

    /// Ask the daemon to re-emit state (model/thinking) on this stream.
    func refreshState() {
        send(Wire.getState())
    }

    /// Next older transcript page (`GetSnapshot` before `oldest_index`).
    func loadHistoryPage() {
        guard !terminated, !historyLoading, historyHasMore, hasCloudStream else { return }
        historyLoading = true
        send(Wire.getSnapshot(limit: 100, before: historyOldestIndex))
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

    /// Cloud frames while the room is alive: the room already renders turns,
    /// live deltas, and tool events. Snapshots stay (idempotent replace) as
    /// the catch-up source; chrome frames stay. Everything else drops or it
    /// double-renders — there is exactly one live channel at a time.
    private func cloudFrameAllowedWhileRoomLive(_ kind: String) -> Bool {
        ["snapshot", "session_info", "state_changed", "notice",
         "process_exited", "ui_request", "ui_request_cancelled"].contains(kind)
    }

    private func applyFrameJSON(_ s: String) {
        guard let d = s.data(using: .utf8), let f = Wire.event(from: d), let kind = f["kind"] as? String else { return }
        if roomLive && !cloudFrameAllowedWhileRoomLive(kind) { return }
        applyFrame(kind, f)
    }

    /// Collab host frame → existing SessionEvent projection.
    fileprivate func applyGuestFrame(_ frame: [String: Any]) {
        let t = frame["t"] as? String ?? ""
        if t == "welcome" {
            guestMapper.reset()
            welcomed = true
            roomLive = true          // room is now the authoritative live channel
            roomDiedAt = nil
            reconnectAttempt = 0
            endedReason = nil
            if let ro = frame["readOnly"] as? Bool {
                readOnly = ro || !guestWriteToken
            }
            if let header = frame["header"] as? [String: Any] {
                if let n = header["title"] as? String, !n.isEmpty { title = n }
                if let id = header["id"] as? String, !id.isEmpty { sessionId = id }
                if let c = header["cwd"] as? String, !c.isEmpty { cwd = c }
            }
            if let st = frame["state"] as? [String: Any] {
                absorbCollabState(st)
            }
            // Do NOT clear `messages` here: full guests get a transcript
            // replay immediately after welcome (the snapshot event replaces
            // anyway), but prompt-channel guests get none — clearing left
            // them with an empty transcript and a live cloud filter.
            streamText = ""; streamThinking = ""; streamDone = false
            activeTools = []; pendingRequest = nil
            streamOpen = false
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

    /// Legacy prompt-channel handler removed: the room guest renders ALL
    /// frames now. Do not ingest
    /// transcript events — those ride the cloud attach.
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
        lastEventAt = Date()
        switch kind {
        case "snapshot":
            if let msgs = f["messages"] as? [[String: Any]] {
                let oldest = Self.jsonUInt64(f["oldest_index"]) ?? 0
                let hasMore = f["has_more"] as? Bool ?? false
                let total = Self.jsonUInt64(f["total_messages"]) ?? 0
                // Older page (GetSnapshot while scrolling up) prepends; a
                // tail snapshot still replaces so reconnects don't stack.
                if historyLoading, oldest < historyOldestIndex, !msgs.isEmpty, msgs.count <= 150 {
                    let existing = Set(messages.compactMap { $0["id"] as? String })
                    let fresh = msgs.filter { m in
                        guard let id = m["id"] as? String, !id.isEmpty else { return true }
                        return !existing.contains(id)
                    }
                    messages = fresh + messages
                    for m in fresh { remember(m) }
                    historyOldestIndex = oldest
                    historyHasMore = hasMore
                    if total > 0 { historyTotal = total }
                    historyLoading = false
                    cachedStaticTurns = []
                    cachedMessageCount = 0
                } else {
                    messages = msgs
                    // Snapshot replaces the transcript wholesale; seed identity
                    // from it so the replayed deltas that follow don't append
                    // duplicates, and so any prompt the user sent clears.
                    seenFingerprints.removeAll()
                    fingerprintOrder.removeAll()
                    for m in msgs { remember(m) }
                    historyOldestIndex = oldest
                    historyHasMore = hasMore
                    historyTotal = total
                    historyLoading = false
                }
                reconcilePendingSends(with: msgs)
            }
            if let phases = f["todos"] as? [[String: Any]] { plan = Self.parsePlan(phases) }
            working = f["streaming"] as? Bool ?? false
            streamOpen = working
            welcomed = true
            phase = "live"
            if !roomLive { refreshState() }   // room has no get_state RPC
        case "message_start":
            streamOpen = true   // framing open: deltas may render
        case "text_delta":
            // Replayed deltas for an already-finalized turn must not append.
            guard streamOpen || working else { return }
            streamDone = false
            streamText += f["delta"] as? String ?? ""
            scheduleStreamRebuild()
            return   // delta frames are coalesced; skip the immediate rebuild below
        case "thinking_delta":
            guard streamOpen || working else { return }
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
            streamOpen = false
            if let m = f["message"] as? [String: Any] {
                if !alreadySeen(m) {          // reconnects replay ends; keep one copy
                    messages.append(m)
                    remember(m)
                    absorbMessage(m)
                }
                reconcilePendingSends(with: [m])
            }
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
                if !alreadySeen(msg) {          // tool ends replay on both channels
                    messages.append(msg)
                    remember(msg)
                    absorbToolResult(msg)
                }
                activeTools.removeAll { $0.id == id }
            }
        case "agent_start":
            if !working {
                working = true
                streamText = ""; streamThinking = ""; streamDone = false
            }
            streamOpen = true
            activity = nil
        case "agent_end":
            working = false
            streamOpen = false
            streamText = ""; streamThinking = ""
            activity = nil
            // pendingSends clear on user-message echo (fingerprint match), not
            // here — a foreign agent_end must not swallow an unacked prompt.
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
        watchdogTask?.cancel()
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

    // MARK: fingerprint identity (port of GTK seen_fingerprints)

    /// Hash of role + toolCallId + each content-part type/text/thinking
    /// (GTK `message_fingerprint` / `fingerprint_part` / `thinking_text`).
    /// Tool results stay `tr:<id>` so live tool_end frames match snapshot rows.
    /// Never empty: thinking-only and toolCall-only assistants still seed.
    private static func fingerprint(_ m: [String: Any]) -> String {
        let role = m["role"] as? String ?? "assistant"
        let toolCallId = strField(m, "toolCallId") ?? strField(m, "tool_call_id")
        if role == "toolResult", let id = toolCallId, !id.isEmpty {
            return "tr:\(id)"
        }
        var payload = role
        payload += "\u{1e}"
        payload += toolCallId ?? ""
        payload += "\u{1e}"
        if let arr = m["content"] as? [Any] {
            for part in arr {
                payload += fingerprintPart(part)
                payload += "\u{1e}"
            }
        } else if let s = m["content"] as? String {
            payload += s
        } else if let obj = m["content"] as? [String: Any], let t = obj["text"] as? String {
            payload += t
        } else if let t = m["text"] as? String {
            payload += t
        }
        return sha256Hex(payload)
    }

    private static func fingerprintPart(_ part: Any) -> String {
        if let s = part as? String { return ":\(s)" }
        guard let p = part as? [String: Any] else { return "" }
        let ty = p["type"] as? String ?? ""
        var out = ty
        if let text = p["text"] as? String {
            out += "\u{1f}"
            out += text
        }
        if ty == "toolCall" || ty == "tool_call" {
            if let id = strField(p, "id") ?? strField(p, "toolCallId") {
                out += "\u{1f}"
                out += id
            }
        }
        if let think = thinkingText(p) {
            out += "\u{1f}"
            out += think
        }
        return out
    }

    private static func thinkingText(_ part: [String: Any]) -> String? {
        let ty = part["type"] as? String ?? ""
        guard ty == "thinking" || ty == "redactedThinking" || ty == "reasoning" else { return nil }
        for key in ["thinking", "text", "data", "reasoning"] {
            if let s = part[key] as? String, !s.isEmpty { return s }
        }
        let nested = flattenContentValue(part["content"])
        return nested.isEmpty ? nil : nested
    }

    private static func flattenContentValue(_ content: Any?) -> String {
        if let s = content as? String { return s }
        if let arr = content as? [Any] {
            return arr.compactMap { partPlainText($0) }.joined()
        }
        if let obj = content as? [String: Any], let t = obj["text"] as? String { return t }
        return ""
    }

    private static func partPlainText(_ part: Any) -> String? {
        if let s = part as? String { return s }
        if let d = part as? [String: Any], let t = d["text"] as? String { return t }
        return nil
    }

    private static func messagePlainText(_ m: [String: Any]) -> String {
        let flat = flattenContentValue(m["content"])
        if !flat.isEmpty { return flat }
        return m["text"] as? String ?? ""
    }

    private static func contentParts(_ content: Any?) -> [[String: Any]] {
        if let arr = content as? [[String: Any]] { return arr }
        if let arr = content as? [Any] { return arr.compactMap { $0 as? [String: Any] } }
        return []
    }

    private static func strField(_ m: [String: Any], _ key: String) -> String? {
        if let s = m[key] as? String, !s.isEmpty { return s }
        return nil
    }

    private static func sha256Hex(_ s: String) -> String {
        SHA256.hash(data: Data(s.utf8)).map { String(format: "%02x", $0) }.joined()
    }

    private static func jsonUInt64(_ v: Any?) -> UInt64? {
        if let n = v as? UInt64 { return n }
        if let n = v as? Int, n >= 0 { return UInt64(n) }
        if let n = v as? Int64, n >= 0 { return UInt64(n) }
        if let n = v as? Double, n >= 0, n < Double(UInt64.max) { return UInt64(n) }
        if let n = v as? NSNumber { return n.uint64Value }
        return nil
    }

    private static func identityKeys(_ m: [String: Any]) -> [String] {
        var keys = [fingerprint(m)]
        if let id = m["id"] as? String, !id.isEmpty { keys.append("id:\(id)") }
        return keys
    }

    /// `call_xxx|fc_xxx` and either half all address the same tool chip.
    private static func toolIdAliases(_ id: String) -> [String] {
        let trimmed = id.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return [] }
        var out = [trimmed]
        if trimmed.contains("|") {
            for part in trimmed.split(separator: "|", omittingEmptySubsequences: true) {
                let s = String(part).trimmingCharacters(in: .whitespacesAndNewlines)
                if !s.isEmpty, !out.contains(s) { out.append(s) }
            }
        }
        return out
    }

    private func indexTool(_ id: String, at idx: Int, into map: inout [String: Int]) {
        for key in Self.toolIdAliases(id) where map[key] == nil {
            map[key] = idx
        }
    }

    private func lookupTool(_ id: String, in map: [String: Int]) -> Int? {
        for key in Self.toolIdAliases(id) {
            if let idx = map[key] { return idx }
        }
        return nil
    }

    private func markSeen(_ fp: String) {
        guard seenFingerprints.insert(fp).inserted else { return }
        fingerprintOrder.append(fp)
        if fingerprintOrder.count > 64 {
            seenFingerprints.remove(fingerprintOrder.removeFirst())
        }
    }

    private func remember(_ m: [String: Any]) {
        markSeen(Self.fingerprint(m))
        if let id = m["id"] as? String, !id.isEmpty {
            markSeen("id:\(id)")
        }
    }

    private func alreadySeen(_ m: [String: Any]) -> Bool {
        if let id = m["id"] as? String, !id.isEmpty, seenFingerprints.contains("id:\(id)") {
            return true
        }
        return seenFingerprints.contains(Self.fingerprint(m))
    }

    /// GTK `message_already_rendered` for the projection pass: first copy
    /// of a fingerprint (or message.id) renders; later copies in the same
    /// array are skipped. Local set — not the rolling 64 window — so a
    /// 100-message tail still unique-collapses without hiding earlier rows.
    private func messageAlreadyRendered(_ rendered: inout Set<String>, _ m: [String: Any]) -> Bool {
        let keys = Self.identityKeys(m)
        if keys.contains(where: { rendered.contains($0) }) { return true }
        for k in keys { rendered.insert(k) }
        return false
    }

    /// Your own prompt turns from grey to confirmed the moment any channel
    /// echoes it as a user message — not on the next agent_end.
    private func reconcilePendingSends(with msgs: [[String: Any]]) {
        guard !pendingSends.isEmpty else { return }
        let echoed = Set(msgs.filter { ($0["role"] as? String) == "user" }
            .map { Self.messagePlainText($0) })
        for p in pendingSends where echoed.contains(p) {
            pendingSends.remove(p)
            pendingSentAt[p] = nil
        }
    }

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
        }

        let tail = buildTail(staticTurns: staticTurns)

        // Model chips only earn their space when the session actually used >1 model.
        var combined = staticTurns + tail
        if Set(combined.compactMap { $0.model.isEmpty ? nil : $0.model }).count <= 1 {
            for i in combined.indices { combined[i].model = "" }
        }

        cachedStaticTurns = staticTurns
        cachedMessageCount = messages.count
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
        var rendered = Set<String>()

        if welcomed && justPaired { out.append(UITurn.sys("paired", "SESSION STARTED")) }

        for entry in messages {
            if messageAlreadyRendered(&rendered, entry) { continue }
            let eid = messageId(entry)
            let role = entry["role"] as? String ?? ""
            switch role {
            case "user":
                out.append(userTurn(id: eid, content: entry["content"]))
            case "assistant":
                let msgModel = entry["model"] as? String ?? ""
                let parts = Self.contentParts(entry["content"])
                if parts.isEmpty, let s = entry["content"] as? String, !s.isEmpty {
                    out.append(agentTurn(id: eid, text: s, model: msgModel))
                }
                for (i, block) in parts.enumerated() {
                    // thinking/redactedThinking/reasoning never become agentTurn.
                    if isThinkingBlock(block) {
                        if let think = Self.thinkingText(block) {
                            out.append(thinkingTurn(id: "\(eid)#\(i)", text: think, seconds: nil, model: msgModel))
                        }
                        continue
                    }
                    switch block["type"] as? String {
                    case "text":
                        let text = block["text"] as? String ?? ""
                        if !text.isEmpty { out.append(agentTurn(id: "\(eid)#\(i)", text: text, model: msgModel)) }
                    case "toolCall", "tool_call":
                        let name = block["name"] as? String ?? block["toolName"] as? String ?? "tool"
                        if name == "todo" { continue }
                        let id = block["id"] as? String ?? block["toolCallId"] as? String ?? "\(eid)#\(i)"
                        out.append(toolTurn(id: id, name: name, args: block["arguments"] ?? block["args"], intent: block["intent"] as? String))
                        indexTool(id, at: out.count - 1, into: &toolIndex)
                    default: break
                    }
                }
                if let err = entry["errorMessage"] as? String, !err.isEmpty {
                    out.append(UITurn.sys("error", "ERROR · " + err))
                } else if (entry["stopReason"] as? String) == "error" {
                    out.append(UITurn.sys("error", "TURN FAILED — SEE THE HOST"))
                }
            case "toolResult":
                let id = entry["toolCallId"] as? String ?? entry["tool_call_id"] as? String ?? eid
                if (entry["toolName"] as? String) == "todo" { break }   // plan handled via todo_changed
                let isError = entry["isError"] as? Bool ?? false
                let details = entry["details"] as? [String: Any]
                if let idx = lookupTool(id, in: toolIndex) {
                    fillResult(&out[idx], content: entry["content"], isError: isError, details: details, kind: out[idx].kind)
                } else {
                    var turn = toolTurn(id: id, name: entry["toolName"] as? String ?? "tool", args: nil, intent: nil)
                    fillResult(&turn, content: entry["content"], isError: isError, details: details, kind: turn.kind)
                    out.append(turn)
                    indexTool(id, at: out.count - 1, into: &toolIndex)
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
            indexTool(turn.id, at: i, into: &toolIndex)
        }

        // Optimistic echo of your own prompt until the daemon streams the user message back.
        for p in pendingSends.sorted() {
            var t = UITurn(id: "pending-\(p.hashValue)", type: .user)
            t.text = p
            t.pending = true
            out.append(t)
        }

        // Executing tools with no result yet.
        for (id, tool) in activeTools where lookupTool(id, in: toolIndex) == nil {
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
        t.argsText = jsonPretty(args)
        return t
    }

    private func fillResult(_ turn: inout UITurn, content: Any?, isError: Bool, details: [String: Any]?, kind: String) {
        turn.pending = false
        turn.isError = isError
        if let img = firstImage(content) { turn.image = img }
        let text = contentString(content)
        turn.resultText = text
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
        Self.thinkingText(b)
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

    private func jsonPretty(_ value: Any?) -> String {
        guard let value, !(value is NSNull) else { return "" }
        if let s = value as? String { return s }
        if JSONSerialization.isValidJSONObject(value),
           let data = try? JSONSerialization.data(withJSONObject: value, options: [.prettyPrinted, .sortedKeys]),
           let s = String(data: data, encoding: .utf8) {
            return s
        }
        return ""
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
