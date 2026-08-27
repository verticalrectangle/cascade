//  SessionVM.swift
//  View model for one open session's transcript + composer. Backed entirely by
//  the live CascadeClient: `turns` mirrors the projected transcript, and send/
//  stop/answer map to CloudCommands (prompt / abort / answer_ui).

import SwiftUI
import Combine

@MainActor
final class SessionVM: ObservableObject {
    @Published var turns: [UITurn] = []
    @Published private(set) var session: Session
    @Published private(set) var historyHasMore = false
    @Published private(set) var historyLoading = false

    let live: CascadeClient
    private let seed: Session

    var isRunning: Bool { live.working }
    var readOnly: Bool { live.readOnly }

    // Daemon capabilities — all off over cascade cloud (no collab plugin).
    var enhanced: Bool { live.enhanced }
    var canSendImages: Bool { live.canSendImages }
    var viaVisionModel: Bool { false }
    var imagePossible: Bool { false }
    var commands: [CascadeCommand] { live.commands }
    var plan: [PlanPhase] { live.plan }
    var currentMode: String? { live.currentMode }
    var goal: GoalInfo? { live.goal }
    var models: [ModelOption] { live.models }
    var providerName: String { live.providerName }
    var modelName: String { live.modelName }
    var thinkingLevel: String { live.thinkingLevel }
    var availableModels: [ModelOption] { live.availableModels }
    var thinkingLevels: [String] { live.thinkingLevels }
    var joinLink: String { "" }   // no invite links over the cloud transport

    init(live client: CascadeClient, seed s: Session) {
        session = s
        seed = s
        live = client
        client.onChange = { [weak self] in self?.syncLive() }
        syncLive()
        // Dev seam: auto-send a prompt shortly after connect (streaming test / demo).
        if let p = ProcessInfo.processInfo.environment["CASCADE_PROMPT"], !p.isEmpty {
            Task { try? await Task.sleep(nanoseconds: 1_500_000_000); client.sendPrompt(p) }
        }
    }

    func loadHistoryPage() {
        live.loadHistoryPage()
        if historyLoading != live.historyLoading { historyLoading = live.historyLoading }
    }

    private func syncLive() {
        if turns != live.turns { turns = live.turns }
        if historyHasMore != live.historyHasMore { historyHasMore = live.historyHasMore }
        if historyLoading != live.historyLoading { historyLoading = live.historyLoading }
        if live.working { live.sawWorking = true } else if live.sawWorking { live.awaitingVision = false; live.sawWorking = false }
        let waiting = turns.contains { $0.type == .ask }
        let status = CascadeStatus.from(phase: live.phase, working: live.working, waiting: waiting)
        let action: String
        if let a = live.activity {                       // aborting / tool running
            action = a
        } else if status == .ended {
            action = "Ended · \(live.endedReason ?? "session closed")"
        } else if status == .connecting || status == .working {
            action = status.label + "…"
        } else {
            action = status.label                        // Live / Needs you
        }
        session = Session(id: seed.id, repo: live.title, branch: live.readOnly ? "watch" : "control",
                          dir: live.cwd, model: live.modelName,
                          status: live.working ? .running : (live.phase == "ended" ? .idle : .waiting),
                          lastSeen: "live", action: action, tokens: live.tokensLabel, cost: live.costLabel)
    }

    func send(_ text: String, images: [(mime: String, base64: String)] = []) {
        guard !readOnly else { return }
        live.sendPrompt(text)   // daemon echoes the user message back into the stream
    }

    func stop() { guard !readOnly else { return }; live.sendAbort() }

    /// Answer a live ask (select). `idx` is the chosen option — send its label.
    func answer(_ turn: UITurn, _ idx: Int) {
        guard !readOnly, !turn.reqKey.isEmpty, idx < turn.options.count else { return }
        live.answer(reqId: turn.reqKey, value: turn.options[idx])
    }
    /// Answer a live ask (editor/input) — send the typed text.
    func answer(_ turn: UITurn, _ text: String) {
        guard !readOnly, !turn.reqKey.isEmpty else { return }
        live.answer(reqId: turn.reqKey, value: text)
    }
    /// Cancel/skip a live ask.
    func skip(_ turn: UITurn) {
        guard !readOnly, !turn.reqKey.isEmpty else { return }
        live.answer(reqId: turn.reqKey, value: nil)
    }
}
