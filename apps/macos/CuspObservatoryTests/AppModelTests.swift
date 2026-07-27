import Foundation
import Testing
import TransitionClient

@testable import CuspObservatory

@MainActor
@Suite("Application state mapping")
struct AppModelTests {
  @Test("coordinator construction is filesystem-side-effect free")
  func coordinatorConstructionIsLazy() {
    let coordinator = AppCoordinator()

    #expect(coordinator.runtimeRoot == nil)
    #expect(coordinator.daemonLog == nil)
    #expect(coordinator.model.phase == .disconnected)
  }

  @Test("retry re-prepares after an environment failure")
  func retryAfterEnvironmentFailure() async throws {
    let loader = EnvironmentLoaderProbe()
    let coordinator = AppCoordinator(environmentLoader: loader.load)

    coordinator.start()
    #expect(coordinator.model.phase == .recovery)
    #expect(loader.callCount == 1)

    coordinator.retry()
    try await Task.sleep(for: .milliseconds(50))

    #expect(loader.callCount == 2)
    #expect(coordinator.runtimeRoot?.path == "/tmp/cmti-retry")
    #expect(coordinator.model.phase == .recovery)
  }

  @Test("runtime privacy defaults remain local and remote-disabled")
  func privacyDefaultsAreLocalOnly() {
    let configuration = AppEnvironment.runtimeConfiguration

    #expect(configuration.contains(#"bind_address = "127.0.0.1:0""#))
    #expect(configuration.contains("session_secret_fd = 3"))
    #expect(configuration.contains("remote_export = false"))
    #expect(configuration.contains("remote_telemetry = false"))
    #expect(!configuration.contains("0.0.0.0"))
    #expect(!configuration.contains("https://"))
  }

  @Test("production ignores a standalone daemon path override")
  func productionDaemonCannotBeOverridden() throws {
    let bundleExecutable = URL(
      fileURLWithPath: "/Applications/Cusp Observatory.app/Contents/MacOS/CuspObservatory"
    )

    let production = try AppEnvironment.daemonURL(
      bundleExecutableURL: bundleExecutable,
      environment: [
        "CMTI_DAEMON_PATH": "/tmp/untrusted-daemon"
      ]
    )
    let test = try AppEnvironment.daemonURL(
      bundleExecutableURL: bundleExecutable,
      environment: [
        "CMTI_DAEMON_PATH": "/tmp/fixture-daemon",
        "CMTI_TEST_RUN_ID": "A1B2-C3D4",
      ]
    )

    #expect(
      production.path
        == "/Applications/Cusp Observatory.app/Contents/MacOS/cryptoriskd"
    )
    #expect(test.path == "/tmp/fixture-daemon")
  }

  @Test("runtime root symlinks fail before use")
  func runtimeRootSymlinkFailsClosed() throws {
    let root = FileManager.default.temporaryDirectory.appending(
      path: UUID().uuidString,
      directoryHint: .isDirectory
    )
    let target = root.appending(path: "target", directoryHint: .isDirectory)
    let link = root.appending(path: "runtime", directoryHint: .isDirectory)
    try FileManager.default.createDirectory(
      at: target,
      withIntermediateDirectories: true
    )
    defer { try? FileManager.default.removeItem(at: root) }
    try FileManager.default.createSymbolicLink(
      at: link,
      withDestinationURL: target
    )

    #expect(throws: AppEnvironmentError.runtimePreparationFailed) {
      try AppEnvironment.ensureSecureDirectory(
        link,
        fileManager: .default
      )
    }
  }

  @Test("unit test hosts never start the embedded daemon")
  func unitTestHostDoesNotStartSupervisor() {
    #expect(
      !AppEnvironment.shouldStartSupervisor(
        environment: [
          "XCTestConfigurationFilePath": "/tmp/test.xctestconfiguration"
        ]
      )
    )
    #expect(
      !AppEnvironment.shouldStartSupervisor(
        environment: [
          "XCInjectBundleInto": "/tmp/CuspObservatory.app"
        ]
      )
    )
    #expect(
      AppEnvironment.shouldStartSupervisor(
        environment: ["CMTI_TEST_RUN_ID": "UI-TEST"]
      )
    )
  }

  @Test("app starts disconnected without optimistic health")
  func initialStateIsDisconnected() {
    let model = AppModel()

    #expect(model.phase == .disconnected)
    #expect(model.overview == nil)
    #expect(model.recoveryMessage == nil)
  }

  @Test("lifecycle maps loading, stopping, and recovery states visibly")
  func lifecycleMapping() {
    let model = AppModel()

    model.reflect(lifecycle: .starting)
    #expect(model.phase == .loading)

    model.reflect(lifecycle: .stopping)
    #expect(model.phase == .loading)

    model.reflect(lifecycle: .failed)
    #expect(model.phase == .recovery)
    #expect(model.recoveryMessage == "The local market service could not start.")
  }

  @Test("server snapshot is the only source of overview health")
  func snapshotMapping() throws {
    let model = AppModel()
    let degraded = MarketSnapshot(
      source: "binance-fixture",
      symbol: "BTCUSDT",
      generation: 1,
      sequence: 102,
      bestBid: "60000.1",
      bestAsk: "60000.2",
      health: .degraded,
      eventUnixNanos: 1,
      receiveUnixNanos: 2,
      freshnessMillis: 5,
      priceDisplayScale: 2
    )

    try model.apply(snapshot: degraded)

    #expect(model.phase == .degraded)
    #expect(model.overview?.sequence == 102)
    #expect(model.overview?.bestBid == "60000.10")
  }
}

private enum EnvironmentLoaderProbeError: Error {
  case firstAttempt
}

@MainActor
private final class EnvironmentLoaderProbe {
  private(set) var callCount = 0

  func load() throws -> AppEnvironment {
    callCount += 1
    guard callCount > 1 else {
      throw EnvironmentLoaderProbeError.firstAttempt
    }
    let root = URL(fileURLWithPath: "/tmp/cmti-retry")
    return AppEnvironment(
      runtimeRoot: root,
      daemonLog: root.appending(path: "cmti.jsonl"),
      supervisor: DaemonSupervisor(
        configuration: DaemonConfiguration(
          executable: root.appending(path: "cryptoriskd"),
          approvedRoot: root,
          configFile: root.appending(path: "config.toml")
        ),
        launcher: EnvironmentFailureLauncher(),
        transportFactory: UnusedTransportFactory(),
        startupTimeout: .milliseconds(10),
        shutdownTimeout: .milliseconds(10),
        nowUnixSeconds: { 1_700_000_000 }
      )
    )
  }
}

private struct EnvironmentFailureLauncher: DaemonLaunching {
  func launch(
    configuration: DaemonConfiguration,
    bootstrap: SessionBootstrap
  ) async throws -> any DaemonRuntime {
    throw DaemonSupervisorError.launchFailed
  }
}

private struct UnusedTransportFactory: RPCTransportBuilding {
  func makeTransport(
    for descriptor: ReadinessDescriptor
  ) async throws -> any RPCTransport {
    throw DaemonSupervisorError.snapshotUnavailable
  }
}
