import AppKit

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    private var store: Store!
    private var controller: StatusItemController!
    private let pairAlert = PairAlert()

    func applicationDidFinishLaunching(_ notification: Notification) {
        let agent = AgentService()
        let store = Store(agent: agent)
        let controller = StatusItemController(store: store)
        self.store = store
        self.controller = controller
        pairAlert.onAnswer = { p, accept in
            Task {
                await store.answer(p, accept: accept)
                // The popover is the only place the error can be read.
                if store.banner?.isError == true { controller.show() }
            }
        }
        store.onPrompt = { [pairAlert] in pairAlert.enqueue($0) }
        store.onWithdraw = { [pairAlert] in pairAlert.withdraw($0) }
        store.start()

        // First launch: start the agent and show the menu so the next steps are visible.
        let key = "didFirstLaunch"
        if !UserDefaults.standard.bool(forKey: key) {
            UserDefaults.standard.set(true, forKey: key)
            Task {
                if agent.state == .notRegistered { await store.turnOn() }
                controller.show()
            }
        }
    }
}
