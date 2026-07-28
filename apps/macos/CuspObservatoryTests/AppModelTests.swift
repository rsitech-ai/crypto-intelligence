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

  @Test("child exit clears the overview and retry starts a fresh run")
  func childExitPublishesRecoveryAndRetry() async throws {
    let firstRuntime = CoordinatorFakeRuntime(processID: 41)
    let secondRuntime = CoordinatorFakeRuntime(processID: 42)
    let launcher = CoordinatorRuntimeQueue(
      runtimes: [firstRuntime, secondRuntime]
    )
    let root = URL(fileURLWithPath: "/tmp/cmti-coordinator-events")
    let supervisor = DaemonSupervisor(
      configuration: DaemonConfiguration(
        executable: root.appending(path: "cryptoriskd"),
        approvedRoot: root,
        configFile: root.appending(path: "config.toml")
      ),
      launcher: launcher,
      transportFactory: CoordinatorTransportFactory(),
      startupTimeout: .seconds(1),
      shutdownTimeout: .milliseconds(100),
      nowUnixSeconds: { 1_700_000_000 }
    )
    let coordinator = AppCoordinator(
      environment: AppEnvironment(
        runtimeRoot: root,
        daemonLog: root.appending(path: "cmti.jsonl"),
        supervisor: supervisor
      )
    )

    coordinator.start()
    try await waitUntil {
      coordinator.model.phase == .healthy
    }
    #expect(coordinator.model.overview?.sequence == 102)

    await firstRuntime.exit(status: 17)
    try await waitUntil {
      coordinator.model.phase == .recovery
    }
    #expect(coordinator.model.overview == nil)
    #expect(
      coordinator.model.recoveryMessage
        == "The local market service could not start."
    )

    coordinator.retry()
    try await waitUntil {
      coordinator.model.phase == .healthy
        && coordinator.model.overview?.sequence == 102
    }
    #expect(await launcher.launchCount == 2)

    await coordinator.stop()
    #expect(coordinator.model.phase == .disconnected)
    #expect(await supervisor.ownedTaskCount == 0)
  }

  @Test("unconfirmed shutdown remains visible as recovery")
  func unconfirmedShutdownRemainsFailed() async throws {
    let runtime = CoordinatorUnkillableRuntime(processID: 43)
    let root = URL(fileURLWithPath: "/tmp/cmti-coordinator-unconfirmed-stop")
    let supervisor = DaemonSupervisor(
      configuration: DaemonConfiguration(
        executable: root.appending(path: "cryptoriskd"),
        approvedRoot: root,
        configFile: root.appending(path: "config.toml")
      ),
      launcher: CoordinatorUnkillableLauncher(runtime: runtime),
      transportFactory: CoordinatorTransportFactory(),
      startupTimeout: .seconds(1),
      shutdownTimeout: .milliseconds(20),
      nowUnixSeconds: { 1_700_000_000 }
    )
    let coordinator = AppCoordinator(
      environment: AppEnvironment(
        runtimeRoot: root,
        daemonLog: root.appending(path: "cmti.jsonl"),
        supervisor: supervisor
      )
    )

    coordinator.start()
    try await waitUntil {
      coordinator.model.phase == .healthy
    }

    await coordinator.stop()

    #expect(coordinator.model.phase == .recovery)
    #expect(await supervisor.currentState == .failed)
    #expect(await runtime.forceTerminationCount == 1)
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

  @Test("only the compiled UI-test harness accepts a daemon override")
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
    let productionShaped = try AppEnvironment.daemonURL(
      bundleExecutableURL: bundleExecutable,
      environment: [
        "CMTI_DAEMON_PATH": "/tmp/untrusted-daemon",
        "CMTI_TEST_RUN_ID": "A1B2-C3D4",
      ]
    )
    let uiTest = try AppEnvironment.daemonURL(
      bundleExecutableURL: bundleExecutable,
      environment: [
        "CMTI_DAEMON_PATH": "/tmp/fixture-daemon",
        "CMTI_TEST_RUN_ID": "A1B2-C3D4",
        "CMTI_UI_TEST_MODE": "1",
      ]
    )

    #expect(
      production.path
        == "/Applications/Cusp Observatory.app/Contents/MacOS/cryptoriskd"
    )
    #expect(
      productionShaped.path
        == "/Applications/Cusp Observatory.app/Contents/MacOS/cryptoriskd"
    )
    #expect(uiTest.path == "/tmp/fixture-daemon")
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
        environment: [
          "CMTI_TEST_RUN_ID": "UI-TEST",
          "CMTI_UI_TEST_MODE": "1",
        ]
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

private enum CoordinatorTestError: Error {
  case timedOut
  case noRuntime
}

private func waitUntil(
  timeout: Duration = .milliseconds(300),
  condition: @MainActor () async -> Bool
) async throws {
  let deadline = ContinuousClock.now.advanced(by: timeout)
  while ContinuousClock.now < deadline {
    if await condition() {
      return
    }
    try await Task.sleep(for: .milliseconds(5))
  }
  throw CoordinatorTestError.timedOut
}

private actor CoordinatorRuntimeQueue: DaemonLaunching {
  private var runtimes: [CoordinatorFakeRuntime]
  private(set) var launchCount = 0

  init(runtimes: [CoordinatorFakeRuntime]) {
    self.runtimes = runtimes
  }

  func launch(
    configuration: DaemonConfiguration,
    bootstrap: SessionBootstrap
  ) throws -> any DaemonRuntime {
    guard !runtimes.isEmpty else {
      throw CoordinatorTestError.noRuntime
    }
    launchCount += 1
    return runtimes.removeFirst()
  }
}

private actor CoordinatorFakeRuntime: DaemonRuntime {
  nonisolated let processID: Int32
  private var status: Int32?
  private var exitWaiters: [CheckedContinuation<Int32, Never>] = []

  init(processID: Int32) {
    self.processID = processID
  }

  func readinessLine() -> Data {
    Data(
      """
      {"endpoint":"http://127.0.0.1:43127","protocol_major":1,\
      "protocol_minor":0,"daemon_pid":\(processID),\
      "process_nonce":"000102030405060708090a0b0c0d0e0f",\
      "server_nonce":"101112131415161718191a1b1c1d1e1f",\
      "issued_unix_seconds":1700000000,"expiry_unix_seconds":1700000060}
      """.utf8
    )
  }

  func terminate() {
    exit(status: 0)
  }

  func forceTerminate() {
    exit(status: SIGKILL)
  }

  func waitForExit() async throws -> Int32 {
    if let status {
      return status
    }
    return await withCheckedContinuation { continuation in
      exitWaiters.append(continuation)
    }
  }

  func exit(status: Int32) {
    guard self.status == nil else {
      return
    }
    self.status = status
    let waiters = exitWaiters
    exitWaiters.removeAll()
    for waiter in waiters {
      waiter.resume(returning: status)
    }
  }
}

private struct CoordinatorTransportFactory: RPCTransportBuilding {
  func makeTransport(
    for descriptor: ReadinessDescriptor
  ) -> any RPCTransport {
    CoordinatorTransport()
  }
}

private struct CoordinatorTransport: RPCTransport {
  func getOrderBookSnapshot(
    using credentials: SessionCredentials,
    timeout: Duration
  ) async throws -> MarketSnapshot {
    MarketSnapshot(
      source: "binance-fixture",
      symbol: "BTCUSDT",
      generation: 1,
      sequence: 102,
      bestBid: "60000.1",
      bestAsk: "60000.2",
      health: .healthy,
      eventUnixNanos: 1_700_000_000_200_000_000,
      receiveUnixNanos: 1_700_000_000_205_000_000,
      freshnessMillis: 5,
      priceDisplayScale: 2
    )
  }
}

private struct CoordinatorUnkillableLauncher: DaemonLaunching {
  let runtime: CoordinatorUnkillableRuntime

  func launch(
    configuration: DaemonConfiguration,
    bootstrap: SessionBootstrap
  ) -> any DaemonRuntime {
    runtime
  }
}

private actor CoordinatorUnkillableRuntime: DaemonRuntime {
  nonisolated let processID: Int32
  private(set) var forceTerminationCount = 0

  init(processID: Int32) {
    self.processID = processID
  }

  func readinessLine() -> Data {
    Data(
      """
      {"endpoint":"http://127.0.0.1:43127","protocol_major":1,\
      "protocol_minor":0,"daemon_pid":\(processID),\
      "process_nonce":"000102030405060708090a0b0c0d0e0f",\
      "server_nonce":"101112131415161718191a1b1c1d1e1f",\
      "issued_unix_seconds":1700000000,"expiry_unix_seconds":1700000060}
      """.utf8
    )
  }

  func terminate() {}

  func forceTerminate() {
    forceTerminationCount += 1
  }

  func waitForExit() async throws -> Int32 {
    try await Task.sleep(for: .seconds(60))
    throw CancellationError()
  }
}
