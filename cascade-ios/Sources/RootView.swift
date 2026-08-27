import SwiftUI
struct RootView: View {
    @EnvironmentObject var app: AppModel
    @EnvironmentObject var theme: ThemeStore
    var body: some View {
        VStack {
            Text("acct:\(app.account != nil ? "yes" : "no") sess:\(app.sessions.count)")
                .foregroundStyle(.red)
            Text("theme:\(theme.t.bg)")
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color.yellow)
    }
}
