//  ToolStrip.swift
//  Horizontal chip strip for a contiguous burst of tool/thinking turns,
//  matching cascade-gtk ToolStrip: sharp glass chips, status dots, one
//  expansion card, 20-line cap.

import SwiftUI
import UIKit

enum TranscriptCaps {
    static let lines = 20
    static let followReengage: CGFloat = 40
    static let streamFade: Double = 0.18
}

private struct UnpinFollowKey: EnvironmentKey {
    static let defaultValue: () -> Void = {}
}

extension EnvironmentValues {
    var unpinFollow: () -> Void {
        get { self[UnpinFollowKey.self] }
        set { self[UnpinFollowKey.self] = newValue }
    }
}

enum TranscriptItem: Identifiable {
    case turn(UITurn)
    case strip(id: String, turns: [UITurn])

    var id: String {
        switch self {
        case .turn(let t): return t.id
        case .strip(let id, _): return id
        }
    }
}

func groupedTranscript(_ turns: [UITurn]) -> [TranscriptItem] {
    var items: [TranscriptItem] = []
    var burst: [UITurn] = []
    func flush() {
        guard !burst.isEmpty else { return }
        items.append(.strip(id: "strip-\(burst[0].id)", turns: burst))
        burst.removeAll(keepingCapacity: true)
    }
    for t in turns {
        if t.type == .tool || t.type == .thinking {
            burst.append(t)
        } else {
            flush()
            items.append(.turn(t))
        }
    }
    flush()
    return items
}

struct ToolStripView: View {
    let turns: [UITurn]
    let t: Theme
    var onImage: (String) -> Void = { _ in }

    @Environment(\.unpinFollow) private var unpinFollow
    @State private var openId: String?
    @State private var bodyExpanded = false

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 6) {
                    ForEach(turns) { turn in
                        ToolChipView(
                            turn: turn,
                            t: t,
                            selected: openId == turn.id,
                            onTap: { toggle(turn) }
                        )
                        .transition(.move(edge: .trailing).combined(with: .opacity))
                    }
                }
                .animation(.easeOut(duration: TranscriptCaps.streamFade), value: turns.map(\.id))
                .padding(.vertical, 2)
            }
            if let openId, let turn = turns.first(where: { $0.id == openId }) {
                ToolExpansionCard(turn: turn, t: t, expanded: $bodyExpanded, onImage: onImage)
                    .transition(.opacity)
            }
        }
        .padding(.bottom, 8)
        .animation(.easeOut(duration: TranscriptCaps.streamFade), value: openId)
        .onChange(of: openId) { _, _ in bodyExpanded = false }
    }

    private func toggle(_ turn: UITurn) {
        unpinFollow()
        withAnimation(.easeOut(duration: TranscriptCaps.streamFade)) {
            if openId == turn.id {
                openId = nil
            } else {
                openId = turn.id
            }
        }
    }
}

private enum ChipKind: Equatable {
    case thinking
    case running
    case done
    case error
}

private func chipKind(_ turn: UITurn) -> ChipKind {
    if turn.type == .thinking { return .thinking }
    if turn.isError { return .error }
    if turn.pending { return .running }
    return .done
}

private struct ToolChipView: View {
    let turn: UITurn
    let t: Theme
    let selected: Bool
    let onTap: () -> Void
    @State private var pulse = false

    private var kind: ChipKind { chipKind(turn) }

    var body: some View {
        Button(action: onTap) {
            HStack(spacing: 6) {
                if kind != .thinking {
                    Circle()
                        .fill(dotColor)
                        .frame(width: 8, height: 8)
                        .opacity(kind == .running ? (pulse ? 1 : 0.3) : 1)
                }
                if kind == .thinking {
                    Text("thinking")
                        .font(.system(size: 9, weight: .medium))
                        .italic()
                        .foregroundStyle(t.txtMuted)
                } else {
                    Text((turn.head.isEmpty ? turn.kind : turn.head).uppercased())
                        .font(.system(size: 9, weight: .bold))
                        .tracking(0.6)
                        .foregroundStyle(t.txt)
                    if !turn.meta.isEmpty {
                        Text(truncate(turn.meta, 30))
                            .font(.system(size: 10))
                            .foregroundStyle(t.txtMuted)
                            .lineLimit(1)
                            .fixedSize(horizontal: true, vertical: false)
                    }
                }
            }
            .padding(.horizontal, 11)
            .padding(.vertical, 4)
            .background(chipFill)
            .glassEffect(.regular, in: Rectangle())
            .overlay(chipBorder)
            .contentShape(Rectangle())
            .clipShape(Rectangle())
        }
        .buttonStyle(.plain)
        .onAppear { startPulseIfNeeded() }
        .onChange(of: turn.pending) { _, _ in startPulseIfNeeded() }
    }

    private func startPulseIfNeeded() {
        if kind == .running {
            pulse = false
            withAnimation(.easeInOut(duration: 1.1).repeatForever(autoreverses: true)) {
                pulse = true
            }
        } else {
            pulse = false
        }
    }

    private var dotColor: Color {
        switch kind {
        case .running: return Color(hex: 0xF6C177)
        case .done: return t.cOk
        case .error: return t.cAdvisor
        case .thinking: return .clear
        }
    }

    private var chipFill: Color {
        kind == .error ? t.cAdvisor.opacity(0.10) : t.glassFill
    }

    @ViewBuilder private var chipBorder: some View {
        switch kind {
        case .thinking:
            Rectangle().stroke(style: StrokeStyle(lineWidth: 1, dash: [4, 3])).foregroundStyle(t.line)
        case .error:
            Rectangle().stroke(t.cAdvisor.opacity(0.55), lineWidth: 1)
        default:
            Rectangle().stroke(selected ? t.lineStrong : t.glassBorder, lineWidth: 1)
        }
    }
}

private struct ToolExpansionCard: View {
    let turn: UITurn
    let t: Theme
    @Binding var expanded: Bool
    var onImage: (String) -> Void = { _ in }
    @Environment(\.unpinFollow) private var unpinFollow

    private var bodyText: String {
        if turn.type == .thinking { return turn.text }
        var full = turn.argsText
        if !turn.resultText.isEmpty {
            if !full.isEmpty { full += "\n" }
            full += turn.resultText
        } else if turn.pending {
            if !full.isEmpty { full += "\n" }
            full += "running…"
        }
        return full
    }

    private var lines: [String] {
        bodyText.split(separator: "\n", omittingEmptySubsequences: false).map(String.init)
    }

    private var hidden: Int { max(0, lines.count - TranscriptCaps.lines) }

    private var visible: String {
        if expanded || hidden == 0 { return bodyText }
        return lines.prefix(TranscriptCaps.lines).joined(separator: "\n")
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text(turn.type == .thinking ? "THINKING" : (turn.head.isEmpty ? turn.kind : turn.head).uppercased())
                .font(.labl(9))
                .tracking(1.2)
                .foregroundStyle(turn.isError ? t.cAdvisor : t.txtMuted)
                .padding(.horizontal, 10)
                .padding(.top, 8)
                .padding(.bottom, 4)
            ScrollView(.horizontal, showsIndicators: false) {
                Text(visible)
                    .font(.term(12))
                    .italic(turn.type == .thinking)
                    .foregroundStyle(turn.type == .thinking ? t.txtMuted : t.txtBody)
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 10)
                    .padding(.bottom, hidden == 0 ? 8 : 4)
            }
            if let img = turn.image, turn.type == .tool {
                Button { onImage(img) } label: {
                    HStack(spacing: 6) {
                        Image(systemName: "photo").font(.system(size: 12)).foregroundStyle(t.txtMuted)
                        Text("image result").font(.term(12)).foregroundStyle(t.txtBody)
                        Spacer(minLength: 0)
                    }
                    .padding(.horizontal, 10)
                    .padding(.bottom, 8)
                }
                .buttonStyle(.plain)
            }
            if hidden > 0 {
                Button {
                    unpinFollow()
                    expanded.toggle()
                } label: {
                    Text(expanded ? "show less ▴" : "\(hidden) more lines ▾")
                        .font(.labl(10))
                        .tracking(0.4)
                        .foregroundStyle(t.txtMuted)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.horizontal, 10)
                        .padding(.vertical, 6)
                }
                .buttonStyle(.plain)
                .overlay(Rectangle().frame(height: 0.5).foregroundStyle(t.lineFaint), alignment: .top)
            }
        }
        .background(t.bg2)
        .overlay(Rectangle().stroke(turn.isError ? t.cAdvisor.opacity(0.45) : t.lineFaint, lineWidth: 1))
        .contextMenu {
            Button { UIPasteboard.general.string = bodyText } label: {
                Label("Copy", systemImage: "doc.on.doc")
            }
        }
    }
}

private func truncate(_ s: String, _ n: Int) -> String {
    if s.count <= n { return s }
    return String(s.prefix(n)) + "…"
}

struct StreamFade: ViewModifier {
    let active: Bool
    let token: String
    @State private var shown = false
    @ViewBuilder
    func body(content: Content) -> some View {
        // Opacity-on-appear only. .transition(.opacity) inside a LazyVStack
        // replays on cell recycle and sticks at 0 — transcript text vanished
        // on scroll. Never transition a lazy cell.
        content
            .opacity(active && !shown ? 0.0 : 1.0)
            .onAppear {
                guard active, !shown else { return }
                withAnimation(.easeIn(duration: TranscriptCaps.streamFade)) { shown = true }
            }
    }
}
