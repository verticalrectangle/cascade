//  RootView.swift
//  Native TabView — Sessions / Activity / Trust. AppModel holds your cascade
//  account (one host + JWT), the daemon's session list, and the one live
//  connection. Sessions are enumerated by cascaded (GET /sessions) and watched
//  with background stream clients.

import SwiftUI
import UIKit
import Combine

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
                SessionsView(query: $searchText)
                    .background(t.bg.ignoresSafeArea())
                    .searchable(text: $searchText, prompt: "Search sessions")
                    .searchToolbarBehavior(.minimize)
                    .overlay(alignment: .topLeading) {
                        Text("DBG acct:\(app.account != nil ? "yes" : "no") sess:\(app.sessions.count)")
                            .font(.system(size: 10))
                            .foregroundStyle(.red)
                            .padding(4)
                            .background(Color.yellow.opacity(0.8))
                            .allowsHitTesting(false)
                    }
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
        .tint(t.accent)
        .overlay(alignment: .topLeading) {
            Text("DBG2 acct:\(app.account != nil ? "yes" : "no") sess:\(app.sessions.count) active:\(app.active != nil ? "yes" : "no")")
                .font(.system(size: 9))
                .foregroundStyle(.red)
                .padding(4)
                .background(Color.yellow.opacity(0.9))
        }
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
                // Never await refresh before push — warm cache (or the listed
                // row) already has the title; trailing adoptRow fills gaps.
                _ = app.connect(sessionId: id)
            } else if let latest = app.sessions.max(by: { $0.savedAt < $1.savedAt }) {
                _ = app.connect(sessionId: latest.id)
            }
        }
    }

}
