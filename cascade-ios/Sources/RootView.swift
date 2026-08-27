import SwiftUI
struct RootView: View {
    @EnvironmentObject var app: AppModel
    @EnvironmentObject var theme: ThemeStore
    @State private var searchText = ""
    var body: some View {
        NavigationStack {
            SessionsView(query: $searchText)
                .background(theme.t.bg.ignoresSafeArea())
        }
        .tint(theme.t.accent)
    }
}
