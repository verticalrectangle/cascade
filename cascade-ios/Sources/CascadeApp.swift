//
//  CascadeApp.swift — @main entry. One ThemeStore, one AppModel.
//

import SwiftUI

@main
struct CascadeApp: App {
    @UIApplicationDelegateAdaptor(AppDelegate.self) var delegate
    @StateObject private var theme = ThemeStore()
    @StateObject private var app = AppModel()

    var body: some Scene {
        WindowGroup {
            RootView()
                .environmentObject(theme)
                .environmentObject(app)
        }
    }
}
