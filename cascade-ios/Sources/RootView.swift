import SwiftUI
struct RootView: View {
    @EnvironmentObject var app: AppModel
    @EnvironmentObject var theme: ThemeStore
    var body: some View {
        NavigationStack {
            if app.account == nil {
                Text("LOGIN NIL")
                    .font(.largeTitle)
                    .foregroundStyle(.white)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                    .background(Color.red)
            } else {
                Text("HAS \(app.sessions.count)")
                    .font(.largeTitle)
                    .foregroundStyle(.red)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                    .background(Color.yellow)
            }
        }
    }
}
