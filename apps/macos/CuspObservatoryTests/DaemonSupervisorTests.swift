import Darwin
import Foundation
import Testing
import TransitionClient

@testable import CuspObservatory

@Suite("Daemon supervision")
struct DaemonSupervisorTests {
  @Test("lifecycle exposes exactly the six product states")
  func exactLifecycleStates() {
    #expect(
      DaemonState.allCases.map(\.rawValue) == [
        "stopped",
        "starting",
        "healthy",
        "degraded",
        "failed",
        "stopping",
      ])
  }

  @Test("launch arguments and environment never contain session material")
  func launchSpecificationIsSecretFree() throws {
    let command = ProcessDaemonLauncher.command(
      configuration: configuration()
    )
    let forbidden = String(repeating: "a5", count: 32)
    let visible =
      ([command.executable.path] + command.arguments
      + command.environment.map { "\($0.key)=\($0.value)" }).joined(separator: "\n")

    #expect(command.executable.path == "/bin/sh")
    #expect(command.arguments.contains("/usr/local/bin/cryptoriskd"))
    #expect(command.arguments.contains("/tmp/cmti/config.toml"))
    #expect(!visible.contains(forbidden))
    #expect(!visible.localizedCaseInsensitiveContains("token"))
    #expect(!visible.localizedCaseInsensitiveContains("secret"))
    #expect(
      command.environment == [
        "LC_ALL": "C",
        "PATH": "/usr/bin:/bin",
      ])
  }

  @Test("bootstrap writes exactly 32 bytes and consumes them after HMAC")
  func bootstrapIsSingleUse() throws {
    let pipe = Pipe()
    let zeroization = ZeroizationProbe()
    let bootstrap = try SessionBootstrap(
      randomBytes: { Array(repeating: 0xa5, count: 32) },
      zeroizationProbe: zeroization.record
    )

    try bootstrap.writeSecret(to: pipe.fileHandleForWriting)
    let received = pipe.fileHandleForReading.readDataToEndOfFile()
    #expect(received.count == 32)
    #expect(zeroization.wasCleared)
    let secondPipe = Pipe()
    #expect(throws: SessionBootstrapError.consumed) {
      try bootstrap.writeSecret(to: secondPipe.fileHandleForWriting)
    }
    try secondPipe.fileHandleForWriting.close()

    _ = try bootstrap.credentials(for: descriptor())
    #expect(throws: SessionBootstrapError.consumed) {
      try bootstrap.credentials(for: descriptor())
    }
  }

  @Test("large child diagnostics cannot backpressure readiness or shutdown")
  func diagnosticBackpressureIsBounded() async throws {
    let clock = ContinuousClock()
    let started = clock.now
    let root = FileManager.default.temporaryDirectory.appending(
      path: UUID().uuidString,
      directoryHint: .isDirectory
    )
    try FileManager.default.createDirectory(
      at: root,
      withIntermediateDirectories: true
    )
    defer { try? FileManager.default.removeItem(at: root) }
    let executable = root.appending(path: "fixture-daemon")
    let script = """
      #!/bin/sh
      dd if=/dev/zero bs=1048576 count=4 1>/dev/stderr 2>/dev/null
      dd bs=32 count=1 <&3 >/dev/null 2>&1
      now=$(date +%s)
      expiry=$((now + 60))
      printf '{"endpoint":"http://127.0.0.1:43127","protocol_major":1,"protocol_minor":0,"daemon_pid":%s,"process_nonce":"000102030405060708090a0b0c0d0e0f","server_nonce":"101112131415161718191a1b1c1d1e1f","issued_unix_seconds":%s,"expiry_unix_seconds":%s}\\n' "$$" "$now" "$expiry"
      while true; do sleep 1; done
      """
    try Data(script.utf8).write(to: executable)
    try FileManager.default.setAttributes(
      [.posixPermissions: 0o700],
      ofItemAtPath: executable.path
    )
    let config = DaemonConfiguration(
      executable: executable,
      approvedRoot: root,
      configFile: root.appending(path: "unused.toml")
    )
    let bootstrap = try SessionBootstrap(
      randomBytes: { Array(repeating: 0xa5, count: 32) }
    )
    let runtime = try await ProcessDaemonLauncher().launch(
      configuration: config,
      bootstrap: bootstrap
    )

    let readiness = try await runtime.readinessLine()
    #expect(!readiness.isEmpty)
    await runtime.terminate()
    _ = try await runtime.waitForExit()
    #expect(clock.now - started < .seconds(2))
  }

  @Test("inherited liveness closes the child after supervisor loss")
  func parentLivenessStopsChild() async throws {
    let clock = ContinuousClock()
    let started = clock.now
    let root = FileManager.default.temporaryDirectory.appending(
      path: UUID().uuidString,
      directoryHint: .isDirectory
    )
    try FileManager.default.createDirectory(
      at: root,
      withIntermediateDirectories: true
    )
    defer { try? FileManager.default.removeItem(at: root) }
    let executable = root.appending(path: "fixture-daemon")
    let script = """
      #!/bin/sh
      dd bs=32 count=1 <&3 >/dev/null 2>&1
      now=$(date +%s)
      expiry=$((now + 60))
      printf '{"endpoint":"http://127.0.0.1:43127","protocol_major":1,"protocol_minor":0,"daemon_pid":%s,"process_nonce":"000102030405060708090a0b0c0d0e0f","server_nonce":"101112131415161718191a1b1c1d1e1f","issued_unix_seconds":%s,"expiry_unix_seconds":%s}\\n' "$$" "$now" "$expiry"
      while true; do sleep 1; done
      """
    try Data(script.utf8).write(to: executable)
    try FileManager.default.setAttributes(
      [.posixPermissions: 0o700],
      ofItemAtPath: executable.path
    )
    let config = DaemonConfiguration(
      executable: executable,
      approvedRoot: root,
      configFile: root.appending(path: "unused.toml")
    )
    let bootstrap = try SessionBootstrap(
      randomBytes: { Array(repeating: 0xa5, count: 32) }
    )
    let runtime = try await ProcessDaemonLauncher().launch(
      configuration: config,
      bootstrap: bootstrap
    )
    guard let processRuntime = runtime as? ProcessDaemonRuntime else {
      Issue.record("process launcher must return its concrete owned runtime")
      return
    }

    _ = try await runtime.readinessLine()
    await processRuntime.simulateSupervisorLoss()
    _ = try await runtime.waitForExit()

    #expect(Darwin.kill(runtime.processID, 0) == -1)
    #expect(errno == ESRCH)
    #expect(clock.now - started < .seconds(2))
  }

  @Test("child exit before readiness surfaces promptly without a watcher leak")
  func earlyExitClosesReadiness() async throws {
    let clock = ContinuousClock()
    let started = clock.now
    let root = FileManager.default.temporaryDirectory.appending(
      path: UUID().uuidString,
      directoryHint: .isDirectory
    )
    try FileManager.default.createDirectory(
      at: root,
      withIntermediateDirectories: true
    )
    defer { try? FileManager.default.removeItem(at: root) }
    let executable = root.appending(path: "early-exit-daemon")
    let script = """
      #!/bin/sh
      dd bs=32 count=1 <&3 >/dev/null 2>&1
      exit 17
      """
    try Data(script.utf8).write(to: executable)
    try FileManager.default.setAttributes(
      [.posixPermissions: 0o700],
      ofItemAtPath: executable.path
    )
    let config = DaemonConfiguration(
      executable: executable,
      approvedRoot: root,
      configFile: root.appending(path: "unused.toml")
    )
    let runtime = try await ProcessDaemonLauncher().launch(
      configuration: config,
      bootstrap: try SessionBootstrap(
        randomBytes: { Array(repeating: 0xa5, count: 32) }
      )
    )
    guard let processRuntime = runtime as? ProcessDaemonRuntime else {
      Issue.record("process launcher must return its concrete owned runtime")
      return
    }

    await #expect(throws: DaemonSupervisorError.invalidReadiness) {
      try await runtime.readinessLine()
    }
    #expect(try await runtime.waitForExit() == 17)
    await processRuntime.simulateSupervisorLoss()
    var table = try processTable()
    for _ in 0..<20 where table.contains(root.path) {
      try await Task.sleep(for: .milliseconds(10))
      table = try processTable()
    }

    #expect(!table.contains(root.path))
    #expect(clock.now - started < .seconds(2))
  }

  @Test("startup timeout fails and terminates the child")
  func startupTimeout() async throws {
    let clock = ContinuousClock()
    let started = clock.now
    let runtime = FakeDaemonRuntime(
      readiness: .suspended,
      processID: 7
    )
    let supervisor = DaemonSupervisor(
      configuration: configuration(),
      launcher: FakeLauncher(runtime: runtime),
      transportFactory: StaticTransportFactory(),
      startupTimeout: .milliseconds(30),
      shutdownTimeout: .milliseconds(100),
      nowUnixSeconds: { 1_700_000_000 }
    )

    await #expect(throws: DaemonSupervisorError.startupTimeout) {
      try await supervisor.start()
    }
    #expect(await supervisor.currentState == .failed)
    #expect(await runtime.wasTerminated)
    #expect(clock.now - started < .milliseconds(250))
  }

  @Test("incompatible protocol fails closed before market RPC")
  func incompatibleProtocol() async throws {
    let runtime = FakeDaemonRuntime(
      readiness: .data(readiness(protocolMinor: 1)),
      processID: 7
    )
    let transportFactory = StaticTransportFactory()
    let supervisor = DaemonSupervisor(
      configuration: configuration(),
      launcher: FakeLauncher(runtime: runtime),
      transportFactory: transportFactory,
      startupTimeout: .seconds(1),
      shutdownTimeout: .seconds(1),
      nowUnixSeconds: { 1_700_000_000 }
    )

    await #expect(throws: DaemonSupervisorError.incompatibleProtocol) {
      try await supervisor.start()
    }
    #expect(await supervisor.currentState == .failed)
    #expect(await transportFactory.buildCount == 0)
    #expect(await runtime.wasTerminated)
  }

  @Test("cancelling startup stops the child and every owned task")
  func cancellationStopsChild() async throws {
    let clock = ContinuousClock()
    let started = clock.now
    let runtime = FakeDaemonRuntime(
      readiness: .suspended,
      processID: 7
    )
    let supervisor = DaemonSupervisor(
      configuration: configuration(),
      launcher: FakeLauncher(runtime: runtime),
      transportFactory: StaticTransportFactory(),
      startupTimeout: .seconds(10),
      shutdownTimeout: .seconds(1),
      nowUnixSeconds: { 1_700_000_000 }
    )
    let starting = Task {
      try await supervisor.start()
    }
    try await Task.sleep(for: .milliseconds(20))

    try await supervisor.stop()
    _ = await starting.result

    #expect(await supervisor.currentState == .stopped)
    #expect(await runtime.wasTerminated)
    #expect(await supervisor.ownedTaskCount == 0)
    #expect(clock.now - started < .milliseconds(250))
  }

  @Test("healthy startup publishes exact snapshot and clean stop")
  func cleanTermination() async throws {
    let runtime = FakeDaemonRuntime(
      readiness: .data(readiness()),
      processID: 7
    )
    let expected = makeSnapshot()
    let supervisor = DaemonSupervisor(
      configuration: configuration(),
      launcher: FakeLauncher(runtime: runtime),
      transportFactory: StaticTransportFactory(snapshot: expected),
      startupTimeout: .seconds(1),
      shutdownTimeout: .seconds(1),
      nowUnixSeconds: { 1_700_000_000 }
    )

    try await supervisor.start()
    #expect(await supervisor.currentState == .healthy)
    #expect(await supervisor.latestSnapshot == expected)

    try await supervisor.stop()
    #expect(await supervisor.currentState == .stopped)
    #expect(await runtime.wasTerminated)
    #expect(await supervisor.ownedTaskCount == 0)
  }
}

private final class ZeroizationProbe: @unchecked Sendable {
  private let lock = NSLock()
  private var value = false

  var wasCleared: Bool {
    lock.withLock { value }
  }

  func record(_ cleared: Bool) {
    lock.withLock {
      value = cleared
    }
  }
}

private func configuration() -> DaemonConfiguration {
  DaemonConfiguration(
    executable: URL(fileURLWithPath: "/usr/local/bin/cryptoriskd"),
    approvedRoot: URL(fileURLWithPath: "/tmp/cmti"),
    configFile: URL(fileURLWithPath: "/tmp/cmti/config.toml")
  )
}

private func descriptor() throws -> ReadinessDescriptor {
  try ReadinessDescriptor(
    jsonData: readiness(),
    expectedDaemonPID: 7,
    nowUnixSeconds: 1_700_000_000
  )
}

private func readiness(protocolMinor: UInt32 = 0) -> Data {
  Data(
    """
    {"endpoint":"http://127.0.0.1:43127","protocol_major":1,\
    "protocol_minor":\(protocolMinor),"daemon_pid":7,\
    "process_nonce":"000102030405060708090a0b0c0d0e0f",\
    "server_nonce":"101112131415161718191a1b1c1d1e1f",\
    "issued_unix_seconds":1700000000,"expiry_unix_seconds":1700000060}
    """.utf8
  )
}

private func makeSnapshot() -> MarketSnapshot {
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

private struct FakeLauncher: DaemonLaunching {
  let runtime: FakeDaemonRuntime

  func launch(
    configuration: DaemonConfiguration,
    bootstrap: SessionBootstrap
  ) async throws -> any DaemonRuntime {
    runtime
  }
}

private actor FakeDaemonRuntime: DaemonRuntime {
  enum Readiness: Sendable {
    case data(Data)
    case suspended
  }

  nonisolated let processID: Int32
  private let readiness: Readiness
  private(set) var wasTerminated = false
  private var wasForceTerminated = false

  init(readiness: Readiness, processID: Int32) {
    self.readiness = readiness
    self.processID = processID
  }

  func readinessLine() async throws -> Data {
    switch readiness {
    case .data(let data):
      data
    case .suspended:
      try await Task.sleep(for: .seconds(60))
      throw CancellationError()
    }
  }

  func terminate() {
    wasTerminated = true
  }

  func forceTerminate() {
    wasForceTerminated = true
  }

  func waitForExit() async throws -> Int32 {
    while !wasTerminated, !wasForceTerminated {
      try await Task.sleep(for: .milliseconds(5))
    }
    return 0
  }
}

private actor StaticTransportFactory: RPCTransportBuilding {
  private let snapshot: MarketSnapshot
  private(set) var buildCount = 0

  init(snapshot: MarketSnapshot? = nil) {
    self.snapshot = snapshot ?? makeSnapshot()
  }

  func makeTransport(
    for descriptor: ReadinessDescriptor
  ) throws -> any RPCTransport {
    buildCount += 1
    return StaticTransport(snapshot: snapshot)
  }
}

private struct StaticTransport: RPCTransport {
  let snapshot: MarketSnapshot

  func getSnapshot(
    using credentials: SessionCredentials
  ) async throws -> MarketSnapshot {
    snapshot
  }
}

private func processTable() throws -> String {
  let output = Pipe()
  let process = Process()
  process.executableURL = URL(fileURLWithPath: "/bin/ps")
  process.arguments = ["-axo", "command="]
  process.standardOutput = output.fileHandleForWriting
  process.standardError = FileHandle.nullDevice
  try process.run()
  try output.fileHandleForWriting.close()
  let data = output.fileHandleForReading.readDataToEndOfFile()
  process.waitUntilExit()
  try output.fileHandleForReading.close()
  guard process.terminationStatus == 0,
    let table = String(data: data, encoding: .utf8)
  else {
    throw DaemonSupervisorError.snapshotUnavailable
  }
  return table
}
