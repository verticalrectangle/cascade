//  Screens.swift
//  ActivityView — live subagent fan-out for the connected session (agents +
//  task:subagent:* bus), with transcript drill-in (fetch-transcript) and
//  kill/revive/chat (agent-cmd). PairView — paste a /collab link and join.
//  (The lock-screen surface is now a real ActivityKit Live Activity — see
//  Shared/CascadeActivity.swift + the CascadeWidgets extension.)

import SwiftUI
import UIKit

// MARK: - Activity (live subagents)

struct ActivityView: View {
    @EnvironmentObject var theme: ThemeStore
    @EnvironmentObject var app: AppModel
    private var t: Theme { theme.t }

    var body: some View {
            ScrollView {
                VStack(alignment: .leading, spacing: 11) {
                    VStack(alignment: .leading, spacing: 2) {
                        Text("LIVE · THIS SESSION").font(.labl(9)).tracking(1.6).foregroundStyle(t.txtLabel)
                        Text("Activity").font(.disp(40)).foregroundStyle(t.txt).textCase(.uppercase)
                    }.padding(.bottom, 3)
                    if let client = app.active {
                        ActivityLive(client: client, t: t)
                    } else {
                        VStack(spacing: 10) {
                            Image(systemName: "waveform.path.ecg").font(.system(size: 30)).foregroundStyle(t.txtGhost)
                            Text("NOT CONNECTED").font(.labl(10)).tracking(2).foregroundStyle(t.txtMuted)
                            Text("Join a session to watch its agents and subagent fan-out.")
                                .font(.bodyF(13)).foregroundStyle(t.txtMuted).multilineTextAlignment(.center)
                        }.frame(maxWidth: .infinity).padding(.vertical, 48)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)   // fill width + left-align like Sessions/Trust
                .padding(16)
            }
            .background(t.bg.ignoresSafeArea())
            .tint(t.accent)
    }
}

struct ActivityLive: View {
    @ObservedObject var client: CascadeClient
    let t: Theme
    @State private var drill: AgentInfo?

    // Roots first, each followed by its sub-agents; orphans (parent absent) last.
    private var orderedAgents: [AgentInfo] {
        var out: [AgentInfo] = []
        for r in client.agents where r.parentId == nil {
            out.append(r)
            out.append(contentsOf: client.agents.filter { $0.parentId == r.id })
        }
        out.append(contentsOf: client.agents.filter { a in a.parentId != nil && !client.agents.contains { $0.id == a.parentId } })
        return out
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 11) {
            ForEach(orderedAgents) { a in
                AgentRow(agent: a, progress: client.progress.first { $0.id == a.id || $0.task == a.displayName }, t: t)
                    .padding(.leading, a.parentId != nil ? 18 : 0)   // nest sub-agents under their parent
                    .contentShape(Rectangle())
                    .onTapGesture { if a.hasSessionFile { drill = a } }
            }
            if client.agents.isEmpty && client.progress.isEmpty {
                Text("No agents running.").font(.term(14)).foregroundStyle(t.txtMuted).padding(.vertical, 20)
            }
            // Subagent progress not tied to a registered agent row.
            ForEach(client.progress.filter { p in !client.agents.contains { $0.id == p.id } }) { p in
                ProgressRow(p: p, t: t)
            }
        }
        // Subagent drill-in returns with cascade's future agent bus.
    }
}

struct AgentRow: View {
    let agent: AgentInfo; let progress: SubagentProgress?; let t: Theme
    private var running: Bool { agent.status == "running" }
    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 9) {
                Image(systemName: agent.kind == "main" ? "cpu" : "circle.grid.cross").font(.system(size: 16)).foregroundStyle(running ? t.accent : t.txtMuted)
                Text(agent.displayName).font(.disp(15)).foregroundStyle(t.txt).textCase(.uppercase).lineLimit(1)
                Spacer()
                if running { LiveDot(t: t) } else { Text(agent.status).font(.labl(9)).foregroundStyle(agent.status == "aborted" ? t.cAdvisor : t.txtMuted) }
            }.padding(.bottom, progress == nil ? 0 : 9)
            if let p = progress {
                Text(p.currentTool.map { "› \($0)" } ?? p.task).font(.term(13)).foregroundStyle(t.txtMuted).lineLimit(1).padding(.bottom, 6)
                if !p.recentOutput.isEmpty {
                    VStack(alignment: .leading, spacing: 1) {
                        ForEach(Array(p.recentOutput.suffix(4).enumerated()), id: \.offset) { _, line in
                            Text(line).font(.term(11)).foregroundStyle(t.txtGhost).lineLimit(1).frame(maxWidth: .infinity, alignment: .leading)
                        }
                    }
                    .padding(.horizontal, 8).padding(.vertical, 6)
                    .background(t.bg2).clipShape(RoundedRectangle(cornerRadius: 12)).padding(.bottom, 6)
                }
                HStack(spacing: 8) {
                    Text("\(p.toolCount) tools").font(.term(12)).foregroundStyle(t.txtGhost)
                    Text("\(p.tokens >= 1000 ? "\(p.tokens/1000)K" : "\(p.tokens)") tok").font(.term(12)).foregroundStyle(t.txtGhost)
                    if let ct = p.contextTokens, let cw = p.contextWindow, cw > 0 {
                        Text("\(min(100, ct * 100 / cw))% ctx").font(.term(12)).foregroundStyle(t.txtGhost)
                    }
                    if p.cost > 0 { Text(String(format: "$%.2f", p.cost)).font(.term(12)).foregroundStyle(t.txtGhost) }
                    Spacer()
                    if agent.hasSessionFile { Text("transcript ›").font(.labl(9)).foregroundStyle(t.accent) }
                }
            }
        }.padding(13).glass(t, 16)
    }
}

struct ProgressRow: View {
    let p: SubagentProgress; let t: Theme
    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 9) {
                Image(systemName: "circle.grid.cross").font(.system(size: 15)).foregroundStyle(t.cTask)
                Text(p.task).font(.disp(14)).foregroundStyle(t.txt).lineLimit(1)
                Spacer()
                Text(p.status).font(.labl(9)).foregroundStyle(p.status == "failed" || p.status == "aborted" ? t.cAdvisor : p.status == "completed" ? t.cOk : t.txtMuted)
            }
            if let d = p.description { Text(d).font(.term(13)).foregroundStyle(t.txtMuted).lineLimit(2) }
        }.padding(13).glass(t, 16, flat: true)
    }
}



// MARK: - Login (host + account) and Spawn (new session) — replaces link Pairing

struct LoginGate: View {
    @EnvironmentObject var app: AppModel
    @EnvironmentObject var theme: ThemeStore
    private var t: Theme { theme.t }
    @State private var host = ""
    @State private var email = ""
    @State private var password = ""
    @State private var busy = false
    @State private var error: String?

    var body: some View {
        ZStack {
            t.bg.ignoresSafeArea()
            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    Text("CASCADE").font(.labl(10)).tracking(2).foregroundStyle(t.txtLabel).padding(.bottom, 18)
                    Text("Sign in to\nyour daemon.").font(.disp(34)).foregroundStyle(t.txt).textCase(.uppercase).padding(.bottom, 12)
                    Text("Point the app at a cascaded cloud host. Your JWT stays on-device; sessions stream from the daemon over WSS.")
                        .font(.bodyF(14)).foregroundStyle(t.txtBody).padding(.bottom, 22)

                    field(icon: "server.rack", placeholder: "host — e.g. wickrunner.com or 192.168.1.10:7700",
                          text: $host, error: error != nil && email.isEmpty)
                        .padding(.bottom, 10)
                    field(icon: "envelope", placeholder: "email", text: $email, keyboard: .emailAddress)
                        .padding(.bottom, 10)
                    SecureField("", prompt: Text("password").foregroundStyle(t.txtMuted), text: $password)
                        .font(.term(14)).foregroundStyle(t.txt).tint(t.accent)
                        .textInputAutocapitalization(.never).autocorrectionDisabled()
                        .padding(.horizontal, 12).padding(.vertical, 11).glass(t, 16, flat: true)
                        .padding(.bottom, 8)

                    if let err = error { Text(err).font(.term(12)).foregroundStyle(t.cAdvisor).padding(.bottom, 16) }

                    Button {
                        guard !busy else { return }
                        busy = true; error = nil
                        Task {
                            error = await app.signIn(base: host, email: email, password: password)
                            busy = false
                        }
                    } label: {
                        HStack(spacing: 8) {
                            if busy { ProgressView().tint(t.accent) } else { Image(systemName: "bolt.fill") }
                            Text("SIGN IN").font(.labl(11))
                        }
                        .foregroundStyle(t.accent).frame(maxWidth: .infinity).padding(.vertical, 14)
                        .glass(t, 16, active: true, border: false, interactive: false)
                    }.press()
                    .disabled(email.trimmingCharacters(in: .whitespaces).isEmpty || password.isEmpty)
                    .padding(.top, 8)

                    Text("Run cascaded with CASCADE_ALLOW_PASSWORDS=email:password to seed an account.")
                        .font(.bodyF(12)).foregroundStyle(t.txtGhost).padding(.top, 20)
                }.padding(22)
            }
        }
        .preferredColorScheme(theme.preferredScheme)
    }

    private func field(icon: String, placeholder: String, text: Binding<String>,
                       keyboard: UIKeyboardType = .default, error: Bool = false) -> some View {
        HStack(spacing: 8) {
            Image(systemName: icon).font(.system(size: 15)).foregroundStyle(t.txtMuted)
            TextField("", prompt: Text(placeholder).foregroundStyle(t.txtMuted), text: text)
                .font(.term(14)).foregroundStyle(t.txt).tint(t.accent)
                .keyboardType(keyboard).textInputAutocapitalization(.never).autocorrectionDisabled()
        }
        .padding(.horizontal, 12).padding(.vertical, 11).glass(t, 16, flat: true)
    }
}

struct SpawnView: View {
    @EnvironmentObject var app: AppModel
    @EnvironmentObject var theme: ThemeStore
    let onClose: () -> Void
    private var t: Theme { theme.t }
    @State private var cwd = "~/dev"
    @State private var model = ""
    @State private var busy = false
    @State private var error: String?

    var body: some View {
        ZStack {
            t.bg.ignoresSafeArea()
            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    HStack {
                        Text("NEW SESSION").font(.labl(10)).tracking(2).foregroundStyle(t.txtLabel)
                        Spacer()
                        Button(action: onClose) { Image(systemName: "xmark").font(.system(size: 20)).foregroundStyle(t.txt) }
                    }.padding(.bottom, 18)

                    Text("Spawn a\nsession.").font(.disp(34)).foregroundStyle(t.txt).textCase(.uppercase).padding(.bottom, 12)
                    Text("The daemon spawns omp in the working directory you name, then you attach to its live transcript.")
                        .font(.bodyF(14)).foregroundStyle(t.txtBody).padding(.bottom, 22)

                    Text("WORKING DIRECTORY").font(.labl(9)).tracking(2).foregroundStyle(t.txtMuted).padding(.bottom, 8)
                    TextField("", prompt: Text("path on the daemon machine").foregroundStyle(t.txtMuted), text: $cwd)
                        .font(.term(14)).foregroundStyle(t.txt).tint(t.accent)
                        .textInputAutocapitalization(.never).autocorrectionDisabled()
                        .padding(.horizontal, 12).padding(.vertical, 11).glass(t, 16, flat: true).padding(.bottom, 14)

                    Text("MODEL — OPTIONAL").font(.labl(9)).tracking(2).foregroundStyle(t.txtMuted).padding(.bottom, 8)
                    TextField("", prompt: Text("provider/model id").foregroundStyle(t.txtMuted), text: $model)
                        .font(.term(14)).foregroundStyle(t.txt).tint(t.accent)
                        .textInputAutocapitalization(.never).autocorrectionDisabled()
                        .padding(.horizontal, 12).padding(.vertical, 11).glass(t, 16, flat: true).padding(.bottom, 8)

                    if let err = error { Text(err).font(.term(12)).foregroundStyle(t.cAdvisor).padding(.bottom, 16) }

                    Button {
                        guard !busy, !cwd.trimmingCharacters(in: .whitespaces).isEmpty else { return }
                        busy = true; error = nil
                        Task {
                            error = await app.spawn(cwd: cwd.trimmingCharacters(in: .whitespaces),
                                                    model: model.trimmingCharacters(in: .whitespaces).isEmpty ? nil : model)
                            busy = false
                            if error == nil { onClose() }
                        }
                    } label: {
                        HStack(spacing: 8) {
                            if busy { ProgressView().tint(t.accent) } else { Image(systemName: "bolt.fill") }
                            Text("SPAWN & ATTACH").font(.labl(11))
                        }
                        .foregroundStyle(t.accent).frame(maxWidth: .infinity).padding(.vertical, 14)
                        .glass(t, 16, active: true, border: false, interactive: false)
                    }.press()
                    .disabled(cwd.trimmingCharacters(in: .whitespaces).isEmpty)
                    .padding(.bottom, 20)

                    Text("cwd must exist on the daemon host. Sessions run as real omp processes there.")
                        .font(.bodyF(12.5)).foregroundStyle(t.txtMuted)
                }.padding(22)
            }
        }
        .preferredColorScheme(theme.preferredScheme)
    }
}
