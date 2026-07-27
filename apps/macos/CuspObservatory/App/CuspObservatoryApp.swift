import AppKit
import SwiftUI

@main
struct CuspObservatoryApp: App {
  @NSApplicationDelegateAdaptor(AppDelegate.self)
  private var appDelegate
  @State private var coordinator = AppCoordinator()

  var body: some Scene {
    WindowGroup {
      MarketOverviewView(model: coordinator.model) {
        coordinator.retry()
      }
      .frame(minWidth: 760, minHeight: 500)
      .onAppear {
        appDelegate.coordinator = coordinator
        if AppEnvironment.shouldStartSupervisor(
          environment: ProcessInfo.processInfo.environment
        ) {
          coordinator.start()
        }
      }
    }
    .defaultSize(width: 920, height: 620)
    .windowResizability(.contentMinSize)
  }
}

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
  weak var coordinator: AppCoordinator?
  private var terminationPending = false

  func applicationShouldTerminate(
    _ sender: NSApplication
  ) -> NSApplication.TerminateReply {
    guard !terminationPending,
      let coordinator,
      coordinator.isActive
    else {
      return .terminateNow
    }
    terminationPending = true
    Task {
      await coordinator.stop()
      sender.reply(toApplicationShouldTerminate: true)
    }
    return .terminateLater
  }
}
