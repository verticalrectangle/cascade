//  RootView.swift
//  Native TabView — Sessions / Activity / Trust. AppModel holds your cascade
//  account (one host + JWT), the daemon's session list, and the one live
//  connection. Sessions are enumerated by cascaded (GET /sessions) and watched
//  with background stream clients.

import SwiftUI
import UIKit
import Combine

@MainActor
final class AppModel: ObservableObject {
    @Published var sessions: [JoinedSession] = []      // mirror of GET /sessions
    private var refreshing = false
    @Published var account: CascadeClient.Account?
    @Published var active: CascadeClient?
    @Published var showEditor = false
    @Published var tab = 0          // selected main tab; the logo button jumps here to 0
    @Published var live: [String: Bool] = [:]   // session.id → host currently connected
    @Published var state: [String: SessionState] = [:] // session.id → richer background state

    private let key = "cascade.account"
    private let tagKey = "cascade.tags"
    @Published var mutedSessions: Set<String> = []

    private let muteKey = "cascade.muted"
    private let liveActivity = LiveActivityController()
    private let notify = NotificationPolicy()
    private var cancellable: AnyCancellable?
    private var clients: [String: CascadeClient] = [:]    // background watchers (by session id)
    private var watchers: [String: AnyCancellable] = [:]  // objectWillChange subscriptions
    private var welcomeTimeouts: [String: Task<Void, Never>] = [:]
    private var activeWelcomeTimeout: Task<Void, Never>?
    private let welcomeGrace: TimeInterval = 8

    init() {
        if let data = UserDefaults.standard.data(forKey: key),
           let acct = try? JSONDecoder().decode(CascadeClient.Account.self, from: data) {
            account = acct
        }
        tab = Int(ProcessInfo.processInfo.environment["CASCADE_TAB"] ?? "") ?? 0
        if let data = UserDefaults.standard.data(forKey: muteKey),
           let muted = try? JSONDecoder().decode(Set<String>.self, from: data) {
            mutedSessions = muted
        }
        if account != nil { Task { await refreshSessions() } }
    }

    func muteSession(_ id: String) {
        mutedSessions.insert(id)
        if let data = try? JSONEncoder().encode(mutedSessions) { UserDefaults.standard.set(data, forKey: muteKey) }
    }

    func unmuteSession(_ id: String) {
        mutedSessions.remove(id)
        if let data = try? JSONEncoder().encode(mutedSessions) { UserDefaults.standard.set(data, forKey: muteKey) }
    }

    /// session id → live view-share (in-memory; refreshed on connect via GET /sessions/{id}/share).
    @Published var sessionShares: [String: CascadeClient.ShareInfo] = [:]

    func signOut() {
        UserDefaults.standard.removeObject(forKey: key)
        leave()
        for s in sessions { stopWatcher(for: s.id) }
        sessions = []
        sessionShares = [:]
        account = nil
    }

    private func adopt(_ acct: CascadeClient.Account) async {
        account = acct
        if let data = try? JSONEncoder().encode(acct) { UserDefaults.standard.set(data, forKey: key) }
        await refreshSessions()
    }

    /// Login against the daemon and remember it.
    func signIn(base rawBase: String, email: String, password: String) async -> String? {
        guard let base = CascadeClient.normalizeBase(rawBase) else { return "that doesn't look like a host url" }
        do {
            let acct = try await CascadeClient.login(base: base, email: email, password: password)
            await adopt(acct)
            return nil
        } catch {
            return (error as? CascadeClient.BridgeError)?.errorDescription ?? error.localizedDescription
        }
    }

    /// Create an account (invite + password) and remember it, same as sign-in.
    func register(base rawBase: String, email: String, password: String, invite: String) async -> String? {
        guard let base = CascadeClient.normalizeBase(rawBase) else { return "that doesn't look like a host url" }
        do {
            let acct = try await CascadeClient.register(base: base, email: email, password: password, invite: invite)
            await adopt(acct)
            return nil
        } catch {
            return (error as? CascadeClient.BridgeError)?.errorDescription ?? error.localizedDescription
        }
    }


    /// Pull GET /sessions into the local card list.
    func refreshSessions() async {
        // Overlapping refreshes coalesce-cancel each other's URLSession tasks;
        // the catch below used to read that as a dead token and signOut(),
        // looping login → refresh → cancel → signOut forever.
        guard !refreshing, let account else { return }
        refreshing = true
        defer { refreshing = false }
        do {
            let metas = try await CascadeClient.listSessions(account: account)
            let machineNames: [String: String] = Dictionary(
                uniqueKeysWithValues: ((try? await CascadeClient.listMachines(account: account)) ?? []).map { ($0.id, $0.name) })
            var next: [JoinedSession] = metas.map { m in
                let dirName = (m.cwd as NSString).lastPathComponent
                let discovered = m.origin == "discovered"
                let isTerminal = (m.kind ?? "") == "terminal"
                    || (!discovered && (m.join_handle != nil || m.view_handle != nil))
                let readOnly = (discovered && m.join_handle == nil)
                    || (m.join_handle == nil && m.view_handle != nil)
                return JoinedSession(
                    id: m.id,
                    link: m.join_handle ?? m.view_handle ?? m.id,
                    title: m.name ?? (dirName.isEmpty ? "session" : dirName),
                    relay: machineNames[m.machine] ?? m.machine,
                    readOnly: readOnly,
                    savedAt: m.last_active ?? m.created_at ?? .distantPast,
                    enhanced: !isTerminal,
                    kind: isTerminal ? "terminal" : (m.kind ?? "managed"),
                    joinHandle: m.join_handle,
                    viewHandle: m.view_handle,
                    origin: m.origin)
                .withLive(m.live, empty: m.empty)
            }
            // Preserve user color tags across refreshes.
            let oldTags = loadTags()
            for i in next.indices { next[i].tagColor = oldTags[next[i].id] ?? .default }
            sessions = next
                .filter { $0.empty != true }   // zero-content rows never render
                .sorted { lhs, rhs in
                    let l = lhs.live == true, r = rhs.live == true
                    if l != r { return l }      // live first
                    return lhs.savedAt > rhs.savedAt
                }
            syncWatchers()
        } catch {
            // Cancellation is a superseded refresh, not an auth failure.
            if (error as NSError).code == NSURLErrorCancelled { return }
            if ProcessInfo.processInfo.environment["CASCADE_DEBUG_FRAMES"] == "1" {
                print("[frames] refreshSessions FAILED: \(error)")
            }
            // A dead/expired token drops you back on the login screen.
            signOut()
        }
    }
    /// Attach to a session (from the directory or a fresh spawn).
    @discardableResult
    func connect(sessionId: String, paired: Bool = false) -> Bool {
        guard let account else { return false }
        stopWatcher(for: sessionId)
        let listed = sessions.first(where: { $0.id == sessionId })
        let discovered = listed?.origin == "discovered"
        let client: CascadeClient
        if !discovered, let terminalLink = listed?.joinHandle ?? listed?.viewHandle,
           let guest = CascadeClient(terminalLink: terminalLink, name: UIDevice.current.name) {
            client = guest
        } else {
            let readOnly = discovered && listed?.joinHandle == nil
            let config = CascadeClient.Config(base: account.base, token: account.token,
                                              sessionId: sessionId, name: UIDevice.current.name,
                                              readOnly: readOnly, title: listed?.title ?? "")
            client = CascadeClient(config: config)
            if discovered, let handle = listed?.joinHandle, !handle.isEmpty {
                client.attachPromptChannel(handle)
            }
        }
        client.justPaired = paired
        active = client
        client.connect()
        showEditor = ProcessInfo.processInfo.environment["CASCADE_SHOWCASE"] != "1"
        connectedId = sessionId
        if ProcessInfo.processInfo.environment["CASCADE_SCREENSHOT"] != "1" { Notifier.requestAuth() }
        notify.reset()
        cancellable = client.objectWillChange.receive(on: RunLoop.main).sink { [weak self] in self?.onClientChanged() }
        scheduleActiveWelcomeTimeout(client)
        Task {
            await refreshSessions()
            await refreshShare(for: sessionId)
            if let row = sessions.first(where: { $0.id == sessionId }) {
                client.adoptRow(row)
            }
        }
        return true
    }

    /// Read-only attach using a view-share token as the stream Bearer.
    func attachShared(base: URL, token: String, sessionId: String) {
        if active != nil { leave() }
        stopWatcher(for: sessionId)
        let config = CascadeClient.Config(base: base, token: token,
                                          sessionId: sessionId, name: UIDevice.current.name,
                                          readOnly: true, paged: false)
        let client = CascadeClient(config: config)
        active = client
        client.connect()
        showEditor = ProcessInfo.processInfo.environment["CASCADE_SHOWCASE"] != "1"
        connectedId = sessionId
        if ProcessInfo.processInfo.environment["CASCADE_SCREENSHOT"] != "1" { Notifier.requestAuth() }
        notify.reset()
        cancellable = client.objectWillChange.receive(on: RunLoop.main).sink { [weak self] in self?.onClientChanged() }
        scheduleActiveWelcomeTimeout(client)
    }

    func openSharedLink(_ raw: String) async -> String? {
        guard let parsed = CascadeClient.parseShareLink(raw) else {
            return "that doesn't look like a view link"
        }
        do {
            let sessionId = try await CascadeClient.resolveShare(base: parsed.base, token: parsed.token)
            attachShared(base: parsed.base, token: parsed.token, sessionId: sessionId)
            return nil
        } catch {
            return (error as? CascadeClient.BridgeError)?.errorDescription ?? error.localizedDescription
        }
    }

    /// Mint a view link for the connected session. Returns the share URL, or nil on failure.
    /// `expiresInHours` nil means forever (until revoked). UI default is 24.
    func shareSession(expiresInHours: Int?) async -> String? {
        guard let account, let id = connectedId, !id.isEmpty else { return nil }
        do {
            let info = try await CascadeClient.createShare(account: account, sessionId: id,
                                                           expiresInHours: expiresInHours)
            sessionShares[id] = info
            return info.url
        } catch {
            return nil
        }
    }

    func stopSharing() async {
        guard let account, let id = connectedId else { return }
        try? await CascadeClient.deleteShare(account: account, sessionId: id)
        sessionShares.removeValue(forKey: id)
    }

    func refreshShare(for id: String) async {
        guard let account else { return }
        do {
            if let info = try await CascadeClient.getShare(account: account, sessionId: id) {
                sessionShares[id] = info.mergingExpiry(sessionShares[id])
            } else {
                sessionShares.removeValue(forKey: id)
            }
        } catch {
            // Older daemons have no GET /sessions/{id}/share — keep local-only state.
        }
    }

    /// Spawn a new cloud session in `cwd`, then attach to it.
    func spawn(machine: String? = nil, cwd: String, model: String?) async -> String? {
        guard let account else { return "not signed in" }
        do {
            let id = try await CascadeClient.createSession(account: account, machine: machine, cwd: cwd, model: model)
            connect(sessionId: id, paired: true)
            await refreshSessions()
            return nil
        } catch {
            return (error as? CascadeClient.BridgeError)?.errorDescription ?? error.localizedDescription
        }
    }

    func deleteSession(_ s: JoinedSession) async {
        guard let account else { return }
        try? await CascadeClient.deleteSession(account: account, id: s.id)
        remove(s)
    }

    private func onClientChanged() {
        guard let c = active, let id = connectedId else { return }
        updateState(id, from: c)
        if c.phase == "ended" { liveActivity.end() }
        else { liveActivity.sync(sessionId: c.sessionId, state: LiveActivityController.state(from: c)) }
        // A welcomed connection upgrades liveness and pulls fresh titles.
        if c.welcomed {
            activeWelcomeTimeout?.cancel(); activeWelcomeTimeout = nil
            live[id] = true
            if !c.title.isEmpty, !c.title.hasPrefix("session "), let i = sessions.firstIndex(where: { $0.id == id }),
               sessions[i].title != c.title { sessions[i].title = c.title }
        }
        // Only notify while you're AWAY (locked / another app).
        let away = UIApplication.shared.applicationState != .active
        notify.update(c, away: away, muted: mutedSessions.contains(id))
    }

    func leave() {
        guard active != nil else { return }   // idempotent: Leave button + onDisappear both call this
        cancellable?.cancel(); cancellable = nil
        activeWelcomeTimeout?.cancel(); activeWelcomeTimeout = nil
        notify.reset()
        liveActivity.end()
        active?.close()
        active = nil
        showEditor = false
        connectedId = nil
        syncWatchers()
    }

    func remove(_ s: JoinedSession) {
        sessions.removeAll { $0.id == s.id }
        live[s.id] = nil
        state[s.id] = nil
        sessionShares.removeValue(forKey: s.id)
        stopWatcher(for: s.id)
    }

    func setTagColor(_ color: SessionColor, for id: String) {
        guard let i = sessions.firstIndex(where: { $0.id == id }) else { return }
        sessions[i].tagColor = color
        var tags = loadTags(); tags[id] = color
        if let data = try? JSONEncoder().encode(tags) { UserDefaults.standard.set(data, forKey: tagKey) }
    }

    private func loadTags() -> [String: SessionColor] {
        guard let data = UserDefaults.standard.data(forKey: tagKey),
              let tags = try? JSONDecoder().decode([String: SessionColor].self, from: data) else { return [:] }
        return tags
    }


    /// Drop every offline session card (keeps the one you're connected to).
    func clearOffline() {
        let keep = connectedId
        for s in sessions where live[s.id] != true && s.id != keep {
            live[s.id] = nil
            state[s.id] = nil
            stopWatcher(for: s.id)
        }
        sessions.removeAll { live[$0.id] != true && $0.id != keep }
    }

    // MARK: - Background session watchers

    private var deviceName: String { UIDevice.current.name }

    private func startWatcher(for s: JoinedSession) {
        guard clients[s.id] == nil, let account else { return }
        let client: CascadeClient
        if s.origin != "discovered", let link = s.joinHandle ?? s.viewHandle,
           let guest = CascadeClient(terminalLink: link, name: deviceName) {
            client = guest
        } else {
            let config = CascadeClient.Config(base: account.base, token: account.token,
                                              sessionId: s.id, name: deviceName,
                                              readOnly: s.origin == "discovered" && s.joinHandle == nil)
            client = CascadeClient(config: config)
        }
        clients[s.id] = client
        client.connect()
        watchers[s.id] = client.objectWillChange.receive(on: RunLoop.main).sink { [weak self, weak client] in
            guard let self, let client else { return }
            self.updateState(s.id, from: client)
        }
    }

    private func stopWatcher(for id: String) {
        cancelWelcomeTimeout(for: id)
        clients[id]?.close()
        clients[id] = nil
        watchers[id]?.cancel()
        watchers[id] = nil
    }

    private func scheduleWelcomeTimeout(for id: String, client: CascadeClient) {
        // Only background watchers can strand in "connected, never welcomed". The
        // active editor client shows its own connecting state and is excluded.
        guard clients[id] === client else { return }
        cancelWelcomeTimeout(for: id)
        welcomeTimeouts[id] = Task { [weak self, weak client] in
            try? await Task.sleep(nanoseconds: UInt64((self?.welcomeGrace ?? 8) * 1_000_000_000))
            guard !Task.isCancelled, let self = self else { return }
            await MainActor.run {
                guard let c = client, !c.welcomed, c.phase != "ended" else { return }
                // Still no snapshot after the grace window — daemon unreachable.
                self.live[id] = false
                self.state[id] = SessionState()
                self.stopWatcher(for: id)
            }
        }
    }
    private func scheduleActiveWelcomeTimeout(_ client: CascadeClient) {
        activeWelcomeTimeout?.cancel()
        activeWelcomeTimeout = Task { [weak self, weak client] in
            try? await Task.sleep(nanoseconds: UInt64((self?.welcomeGrace ?? 8) * 1_000_000_000))
            guard !Task.isCancelled else { return }
            await MainActor.run {
                guard let self, let client,
                      self.active === client,
                      !client.welcomed,
                      client.phase != "ended" else { return }
                // A slow first attach used to evict the session outright —
                // the user watched an empty transcript with a working
                // composer. Retry instead; the user can still Leave.
                client.resync(force: true)
                self.scheduleActiveWelcomeTimeout(client)
            }
        }
    }
    private func cancelWelcomeTimeout(for id: String) {
        welcomeTimeouts[id]?.cancel(); welcomeTimeouts[id] = nil
    }

    private func syncWatchers() {
        for s in sessions {
            // Never run a background watcher for the currently active editor session.
            if s.id == connectedId || clients[s.id] === active { stopWatcher(for: s.id); continue }
            if live[s.id] == true { startWatcher(for: s) } else { stopWatcher(for: s.id) }
        }
    }

    private func updateState(_ id: String, from client: CascadeClient) {
        let welcomed = client.welcomed
        let phase = client.phase

        // Definitive offline: ended / reconnect exhausted.
        if phase == "ended" {
            cancelWelcomeTimeout(for: id)
            live[id] = false
            state[id] = SessionState()
            stopWatcher(for: id)
            return
        }

        // Confirmed live: full snapshot received.
        if welcomed {
            cancelWelcomeTimeout(for: id)
            state[id] = SessionState(
                live: true, working: client.working, phase: phase,
                title: client.title, mode: client.currentMode, lastSeen: Date())
            live[id] = true
            return
        }

        // Pre-snapshot handshake. Preserve prior liveness; refresh volatile fields.
        if var s = state[id] {
            s.working = client.working
            s.phase = phase
            if let m = client.currentMode { s.mode = m }
            s.lastSeen = Date()
            state[id] = s
        } else {
            state[id] = SessionState(
                live: false, working: client.working, phase: phase,
                title: client.title, mode: client.currentMode, lastSeen: Date())
        }
        scheduleWelcomeTimeout(for: id, client: client)
    }

    private var connectedId: String?
}
struct RootView: View {
    @EnvironmentObject var theme: ThemeStore
    @EnvironmentObject var app: AppModel
    @Environment(\.colorScheme) private var colorScheme
    @State private var showPair = ProcessInfo.processInfo.environment["CASCADE_SHOW_PAIR"] == "1"
    @State private var showOpenLink = false
    @State private var searchText = ""
    @State private var didAutoAttach = false
    private var t: Theme { theme.t }

    var body: some View {
        NavigationStack {
            if app.account == nil {
                LoginGate()
                    .environmentObject(app)
                    .environmentObject(theme)
            } else {
                SessionsView(query: $searchText)
                    .background(t.bg.ignoresSafeArea())
                    .searchable(text: $searchText, prompt: "Search sessions")
                    .searchToolbarBehavior(.minimize)
                    .toolbar {
                        ToolbarItem(placement: .topBarTrailing) {
                            Button { theme.toggle() } label: {
                                Image(systemName: theme.effective == .dark ? "sun.max" : "moon")
                                    .font(.system(size: 17, weight: .semibold))
                                    .foregroundStyle(t.txt)
                                    .frame(width: 38, height: 38)
                            }
                            .press()
                        }
                        DefaultToolbarItem(kind: .search, placement: .bottomBar)
                        ToolbarSpacer(.flexible, placement: .bottomBar)
                        ToolbarItem(placement: .bottomBar) {
                            Button { showOpenLink = true } label: {
                                Image(systemName: "link")
                                    .font(.system(size: 17, weight: .semibold))
                            }
                            .accessibilityLabel("Open a view link")
                        }
                        ToolbarItem(placement: .bottomBar) {
                            Button { showPair = true } label: {
                                Image(systemName: "plus")
                                    .font(.system(size: 17, weight: .semibold))
                            }
                            .buttonStyle(.glassProminent)
                            .tint(t.accent)
                            .accessibilityLabel("New session")
                        }
                    }
                    // Native push: tapping a session (→ showEditor) slides the editor in
                    // from the right; Leave / back-swipe pops it left.
                    .navigationDestination(isPresented: $app.showEditor) {
                        if let client = app.active {
                            EditorView(client: client)
                                .environmentObject(theme)
                                .navigationBarBackButtonHidden(true)
                                .toolbar {
                                    ToolbarItem(placement: .topBarLeading) {
                                        Button { app.leave() } label: {
                                            HStack(spacing: 5) { Image(systemName: "chevron.left"); Text("Leave") }.foregroundStyle(t.accent)
                                        }
                                    }
                                }
                                .onDisappear { app.leave() }   // covers the native back-swipe
                        }
                    }
                    .navigationDestination(isPresented: $showPair) {
                        SpawnView(onClose: { showPair = false })
                            .environmentObject(app)
                            .environmentObject(theme)
                            .toolbar(.hidden, for: .navigationBar)
                    }
                    .sheet(isPresented: $showOpenLink) {
                        OpenShareView(onClose: { showOpenLink = false })
                            .environmentObject(app)
                            .environmentObject(theme)
                    }
            }
        }
        .tint(t.accent)
        .onChange(of: colorScheme, initial: true) { _, new in theme.systemDark = (new == .dark) }
        .task {
            // Launch seam / deep-link: auto-attach to CASCADE_SESSION from an env var,
            // or fall back to the most recent daemon session after a cold launch.
            // Fire once per launch — a failed attach must not re-trigger on
            // every navigation pop (that looped: attach → welcome timeout →
            // leave → view re-appears → .task again).
            guard !didAutoAttach, app.active == nil, app.account != nil else { return }
            didAutoAttach = true
            if let id = ProcessInfo.processInfo.environment["CASCADE_SESSION"] {
                if app.sessions.isEmpty { await app.refreshSessions() }
                _ = app.connect(sessionId: id)
            } else if let latest = app.sessions.max(by: { $0.savedAt < $1.savedAt }) {
                _ = app.connect(sessionId: latest.id)
            }
        }
    }

}
