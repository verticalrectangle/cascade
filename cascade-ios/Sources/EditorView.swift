//  EditorView.swift
//  The hero: live streaming transcript + composer. Backed by a CascadeClient
//  via SessionVM — prompt, steer, abort, answer asks.

import SwiftUI
import PhotosUI
import UIKit

private struct BottomOffsetKey: PreferenceKey {
    static var defaultValue: CGFloat = .infinity
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) {
        value = min(value, nextValue())
    }
}

private struct ScrollHeightKey: PreferenceKey {
    static var defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) {
        value = max(value, nextValue())
    }
}

struct EditorView: View {
    @EnvironmentObject var theme: ThemeStore
    @Environment(\.scenePhase) private var scenePhase
    @StateObject var vm: SessionVM
    @StateObject private var dictation = Dictation()
    @State private var draft = ""
    @State private var viewer: String? = nil
    @State private var planExpanded = false
    @State private var showShare = false
    @State private var exportText = ""
    @FocusState private var composerFocused: Bool
    @State private var stickToBottom = true
    @State private var didInitialScroll = false
    @State private var scrollVisibleHeight: CGFloat = 0
    @EnvironmentObject var app: AppModel

    init(client: CascadeClient) {
        let seed = Session(id: "live", repo: client.title, branch: client.readOnly ? "watch" : "control",
                           dir: client.cwd, model: client.modelName, status: .waiting,
                           lastSeen: "live", action: "CONNECTING…", tokens: "—", cost: "—")
        _vm = StateObject(wrappedValue: SessionVM(live: client, seed: seed))
    }
    private var t: Theme { theme.t }

    var body: some View {
        ZStack {
            t.bg.ignoresSafeArea()
            transcript
        }
        .safeAreaInset(edge: .bottom, spacing: 0) { composerStack }
        .navigationTitle(vm.session.repo)
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .principal) {
                VStack(spacing: 1) {
                    Text(vm.session.repo).font(.disp(15)).foregroundStyle(t.txt).textCase(.uppercase).lineLimit(1)
                    Text(vm.session.dir).font(.term(12)).foregroundStyle(t.txtMuted).lineLimit(1)
                }
            }
            ToolbarItem(placement: .topBarTrailing) { sessionMenu }
        }
        .fullScreenCover(item: Binding(get: { viewer.map { IdStr($0) } }, set: { viewer = $0?.v })) { img in
            ImageViewer(src: img.v, label: "focused image") { viewer = nil }.environmentObject(theme)
        }
        .sheet(isPresented: $showShare) {
            ShareSheet(items: [exportText])
        }
        .onChange(of: scenePhase) { _, phase in
            if phase == .active { vm.live.reconnectIfNeeded() }
        }
    }

    // MARK: session menu (top-right …)

    @ViewBuilder private var sessionMenu: some View {
        Menu {
            Button { vm.live.resync() } label: {
                Label("Resync", systemImage: "arrow.triangle.2.circlepath")
            }
            Menu {
                ForEach(vm.availableModels) { m in
                    Button {
                        vm.live.setModel(provider: m.provider, modelId: m.modelId)
                    } label: {
                        if m.provider == vm.providerName && m.name == vm.modelName {
                            Label(m.name, systemImage: "checkmark")
                        } else {
                            Text(m.name)
                        }
                    }
                }
                if vm.availableModels.isEmpty {
                    Text("No models reported yet")
                }
            } label: {
                if vm.providerName.isEmpty {
                    Label("Model · \(vm.modelName)", systemImage: "cpu")
                } else {
                    Label("Model · \(vm.providerName) / \(vm.modelName)", systemImage: "cpu")
                }
            }
            Menu {
                ForEach(vm.thinkingLevels, id: \.self) { lvl in
                    Button { vm.live.setThinking(lvl) } label: {
                        if lvl == vm.thinkingLevel { Label(lvl, systemImage: "checkmark") } else { Text(lvl) }
                    }
                }
            } label: {
                Label("Thinking · \(vm.thinkingLevel.isEmpty ? "—" : vm.thinkingLevel)", systemImage: "brain")
            }
            Divider()
            Button {
                exportText = EditorView.markdownExport(turns: vm.turns)
                showShare = true
            } label: {
                Label("Export transcript…", systemImage: "square.and.arrow.up")
            }
            Toggle(isOn: Binding(
                get: { app.mutedSessions.contains(vm.live.sessionId) },
                set: { muted in
                    if muted { app.muteSession(vm.live.sessionId) } else { app.unmuteSession(vm.live.sessionId) }
                })) {
                Label("Mute notifications", systemImage: "bell.slash")
            }
            Divider()
            Button { UIPasteboard.general.string = vm.session.dir } label: { Label("Copy cwd", systemImage: "doc.on.doc") }
            Button { UIPasteboard.general.string = vm.live.sessionId } label: { Label("Copy session id", systemImage: "doc.on.doc") }
        } label: {
            Image(systemName: "ellipsis").font(.system(size: 16, weight: .semibold)).foregroundStyle(t.accent)
        }
    }

    static func markdownExport(turns: [UITurn]) -> String {
        var out = "# Cascade transcript\n\n"
        for t in turns {
            switch t.type {
            case .user: out += "**You:**\n\n\(t.text)\n\n"
            case .agent: out += "**Agent:**\n\n\(t.text)\n\n"
            case .thinking:
                out += "<details><summary>thinking</summary>\n\n\(t.text)\n\n</details>\n\n"
            case .tool:
                out += "**Tool · \(t.head)**\n\n```\n\(t.lines.joined(separator: "\n"))\n```\n\n"
            case .ask:
                out += "**Ask:** \(t.question)\n\n" + t.options.map { "- \($0)" }.joined(separator: "\n") + "\n\n"
            case .sys: out += "> \(t.text)\n\n"
            case .advisor: out += "**Advisor:**\n\n\(t.text)\n\n"
            }
        }
        return out
    }

    // MARK: transcript

    private var transcript: some View {
        ScrollViewReader { proxy in
            ScrollView(.vertical) {
                transcriptList
            }
            .coordinateSpace(name: "scroll")
            .background(
                GeometryReader { geo in
                    Color.clear
                        .preference(key: ScrollHeightKey.self, value: geo.size.height)
                }
            )
            .onPreferenceChange(ScrollHeightKey.self) { scrollVisibleHeight = $0 }
            .onPreferenceChange(BottomOffsetKey.self) { bottomY in
                // bottomY is the bottom spacer's maxY in the scroll coordinate space.
                // If it's within (or just below) the visible area, we're at the bottom.
                stickToBottom = bottomY <= scrollVisibleHeight + 50
            }
            .onChange(of: vm.turns) { _, _ in
                if didInitialScroll && stickToBottom {
                    proxy.scrollTo("bottom", anchor: .bottom)
                }
            }
            .onChange(of: vm.live.phase) { _, phase in
                if phase == "live" && !didInitialScroll {
                    didInitialScroll = true
                    proxy.scrollTo("bottom", anchor: .bottom)
                    Task { @MainActor in
                        try? await Task.sleep(nanoseconds: 100_000_000)
                        proxy.scrollTo("bottom", anchor: .bottom)
                    }
                }
            }
            .onChange(of: composerFocused) { _, focused in
                guard focused else { return }
                withAnimation(.easeInOut(duration: 0.2)) { planExpanded = false }
                if stickToBottom {
                    proxy.scrollTo("bottom", anchor: .bottom)
                }
            }
            .scrollDismissesKeyboard(.interactively)
            .simultaneousGesture(TapGesture().onEnded { hideKeyboard() })
        }
    }

    @ViewBuilder private var transcriptList: some View {
        LazyVStack(alignment: .leading, spacing: 0) {
            ForEach(vm.turns, id: \.id) { turn in
                TurnRow(turn: turn, t: t,
                        onImage: { viewer = $0 },
                        onAnswer: vm.readOnly ? nil : { vm.answer($0, $1) },
                        onAnswerText: vm.readOnly ? nil : { vm.answer($0, $1) },
                        onCancelAsk: vm.readOnly ? nil : { vm.skip($0) })
                    .id(turn.id)
            }
            if vm.isRunning { ThinkingLine(t: t).id("think") }
            Color.clear.frame(height: 8)
                .id("bottom")
                .background(
                    GeometryReader { geo in
                        Color.clear
                            .preference(key: BottomOffsetKey.self,
                                        value: geo.frame(in: .named("scroll")).maxY)
                    }
                )
        }
        .padding(16)
    }

    private func hideKeyboard() {
        UIApplication.shared.sendAction(#selector(UIResponder.resignFirstResponder), to: nil, from: nil, for: nil)
    }


    // MARK: composer

    private var composerStack: some View {
        GlassEffectContainer(spacing: 8) {
            VStack(spacing: 8) {
                if let g = vm.goal { goalBanner(g) }
                if !vm.plan.isEmpty {
                    PlanStrip(phases: vm.plan, t: t, expanded: $planExpanded)
                }
                if vm.isRunning {
                    HStack(spacing: 8) {
                        LiveDot(t: t)
                        Text(vm.session.action).font(.term(14)).foregroundStyle(t.accent).lineLimit(1)
                        Text("\(vm.session.tokens) · \(vm.session.model)").font(.term(13)).foregroundStyle(t.txtMuted).lineLimit(1)
                    }
                    .padding(.horizontal, 12).padding(.vertical, 7)
                    .glass(t, 16, flat: true)
                }
                if vm.readOnly { readOnlyBar } else { composer }
            }
        }
        .padding(.horizontal, 12)
        .padding(.bottom, 6)
    }

    private func goalBanner(_ g: GoalInfo) -> some View {
        HStack(spacing: 8) {
            Image(systemName: "target").font(.system(size: 12)).foregroundStyle(t.accent)
            Text("GOAL").font(.labl(10)).tracking(1.6).foregroundStyle(t.txt)
            Text(g.objective).font(.term(12)).foregroundStyle(t.txtBody).lineLimit(1)
            Spacer(minLength: 4)
            if let b = g.tokenBudget, b > 0 {
                Text("\(min(100, g.tokensUsed * 100 / b))%").font(.term(11)).foregroundStyle(t.txtMuted)
            }
        }
        .padding(.horizontal, 13).padding(.vertical, 9)
        .glass(t, 16, panel: true)
    }

    private var planReviewBanner: some View {
        HStack(spacing: 8) {
            Image(systemName: "checklist").font(.system(size: 13)).foregroundStyle(t.cAdvisor)
            Text("Plan ready — review it in the transcript above.")
                .font(.term(13)).foregroundStyle(t.cAdvisor).lineLimit(2)
            Spacer()
        }
        .padding(.horizontal, 12).padding(.vertical, 7)
        .glass(t, 16, flat: true)
    }

    private var readOnlyBar: some View {
        HStack(spacing: 8) {
            Image(systemName: "eye").font(.system(size: 15)).foregroundStyle(t.txtMuted)
            Text("WATCHING · READ-ONLY").font(.labl(10)).tracking(1.4).foregroundStyle(t.txtMuted)
            Spacer()
            Text("view link").font(.term(13)).foregroundStyle(t.txtGhost)
        }
        .padding(.horizontal, 12).padding(.vertical, 12)
        .glass(t, 16, flat: true)
    }

    private var composer: some View {
        VStack(spacing: 0) {
            HStack(spacing: 4) {
                TextField("", text: $draft, prompt: Text(placeholder).foregroundStyle(t.txtMuted), axis: .vertical)
                    .font(.bodyF(14)).foregroundStyle(t.txt).tint(t.accent)
                    .lineLimit(1...5)
                    .focused($composerFocused)
                    .onSubmit(doSend)
                Button { dictation.toggle() } label: {
                    Image(systemName: dictation.recording ? "mic.fill" : "mic").font(.system(size: 20))
                        .foregroundStyle(dictation.recording ? t.accent : t.txtMuted).frame(width: 34, height: 34)
                }
                sendOrStop
            }
            .padding(.horizontal, 8).padding(.vertical, 5)
            if draft.isEmpty && !dictation.recording {
                ComposerTips(t: t)
            }
        }
        .glass(t, 16)
    }

    private var placeholder: String {
        dictation.recording ? "Listening…" : (vm.isRunning ? "Steer the turn…" : "Message the agent…")
    }

    @ViewBuilder private var sendOrStop: some View {
        if vm.isRunning {
            Button { vm.stop() } label: {
                Image(systemName: "stop.fill").font(.system(size: 15)).foregroundStyle(t.txt)
            }
            .buttonStyle(.glass)
            .frame(width: 38, height: 38)
        } else {
            Button(action: doSend) {
                Image(systemName: "arrow.right").font(.system(size: 17, weight: .semibold)).foregroundStyle(.white)
            }
            .buttonStyle(.glassProminent)
            .tint(t.accent)
            .frame(width: 38, height: 38)
        }
    }

    private func doSend() {
        let x = draft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !x.isEmpty else { return }
        if dictation.recording { dictation.stop() }
        vm.send(x)
        draft = ""
    }

}

struct IdStr: Identifiable { let v: String; var id: String { v }; init(_ v: String) { self.v = v } }

/// The live plan (omp's `todo` tool): phases → tasks with status. A collapsed pill
/// above the composer showing progress + the current task; tap to slide up the full
/// tree. Expansion is bound so the composer can collapse it while you type.
struct PlanStrip: View {
    let phases: [PlanPhase]
    let t: Theme
    @Binding var expanded: Bool

    private var phasesDone: Int { phases.filter { !$0.tasks.isEmpty && $0.doneCount == $0.tasks.count }.count }
    private var currentTask: String? {
        let all = phases.flatMap { $0.tasks }
        return all.first { $0.status == "in_progress" }?.content ?? all.first { $0.status == "pending" }?.content
    }

    var body: some View {
        VStack(spacing: 0) {
            if expanded {
                ScrollView { planBody.padding(.horizontal, 13).padding(.top, 12).padding(.bottom, 8) }
                    .frame(maxHeight: 260)
                Rectangle().frame(height: 0.5).foregroundStyle(t.lineFaint)
            }
            Button { withAnimation(.easeInOut(duration: 0.22)) { expanded.toggle() } } label: {
                HStack(spacing: 7) {
                    Image(systemName: "checklist").font(.system(size: 12)).foregroundStyle(t.accent)
                    Text("PLAN").font(.labl(10)).tracking(1.6).foregroundStyle(t.txt)
                    Text("\(phasesDone)/\(phases.count)").font(.term(12)).foregroundStyle(t.txtMuted)
                    if let cur = currentTask {
                        Image(systemName: "circle.lefthalf.filled").font(.system(size: 9)).foregroundStyle(t.accent)
                        Text(cur).font(.term(12)).foregroundStyle(t.txtBody).lineLimit(1)
                    } else {
                        Image(systemName: "checkmark.circle.fill").font(.system(size: 9)).foregroundStyle(t.cOk)
                        Text("complete").font(.term(12)).foregroundStyle(t.cOk)
                    }
                    Spacer(minLength: 4)
                    Image(systemName: expanded ? "chevron.down" : "chevron.up").font(.system(size: 10, weight: .semibold)).foregroundStyle(t.txtMuted)
                }
                .padding(.horizontal, 13).padding(.vertical, 10)
            }
        }
        .glass(t, 16, panel: true)
    }

    private var planBody: some View {
        VStack(alignment: .leading, spacing: 11) {
            ForEach(phases) { phase in
                VStack(alignment: .leading, spacing: 6) {
                    HStack(spacing: 6) {
                        Text(phase.name).font(.labl(9.5)).tracking(1).foregroundStyle(t.txtBody).textCase(.uppercase)
                        Text("\(phase.doneCount)/\(phase.tasks.count)").font(.term(11)).foregroundStyle(t.txtMuted)
                    }
                    ForEach(phase.tasks) { task in
                        HStack(alignment: .top, spacing: 8) {
                            Image(systemName: glyph(task.status)).font(.system(size: 12)).foregroundStyle(color(task.status)).frame(width: 15)
                            Text(task.content).font(.bodyF(13))
                                .foregroundStyle(task.status == "completed" ? t.txtMuted : t.txtBody)
                                .strikethrough(task.status == "abandoned", color: t.txtGhost)
                        }
                    }
                }
            }
        }
    }

    private func glyph(_ s: String) -> String {
        switch s {
        case "completed": "checkmark.circle.fill"
        case "in_progress": "circle.lefthalf.filled"
        case "abandoned": "xmark.circle"
        default: "circle"
        }
    }
    private func color(_ s: String) -> Color {
        switch s {
        case "completed": t.cOk
        case "in_progress": t.accent
        case "abandoned": t.txtGhost
        default: t.txtMuted
        }
    }
}

/// UIActivityViewController bridge for the transcript export share sheet.
struct ShareSheet: UIViewControllerRepresentable {
    let items: [Any]

    func makeUIViewController(context: Context) -> UIActivityViewController {
        UIActivityViewController(activityItems: items, applicationActivities: nil)
    }

    func updateUIViewController(_ uiViewController: UIActivityViewController, context: Context) {}
}
