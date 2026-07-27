import Darwin
import Foundation
import TransitionClient

struct DaemonConfiguration: Sendable, Equatable {
  let executable: URL
  let approvedRoot: URL
  let configFile: URL
}

struct DaemonLaunchCommand: Sendable, Equatable {
  let executable: URL
  let arguments: [String]
  let environment: [String: String]
}

protocol DaemonLaunching: Sendable {
  func launch(
    configuration: DaemonConfiguration,
    bootstrap: SessionBootstrap
  ) async throws -> any DaemonRuntime
}

protocol DaemonRuntime: Actor {
  nonisolated var processID: Int32 { get }

  func readinessLine() async throws -> Data
  func terminate() async
  func forceTerminate() async
  func waitForExit() async throws -> Int32
}

protocol RPCTransportBuilding: Sendable {
  func makeTransport(
    for descriptor: ReadinessDescriptor
  ) async throws -> any RPCTransport
}

enum DaemonSupervisorError: Error, Sendable, Equatable {
  case alreadyRunning
  case startupTimeout
  case incompatibleProtocol
  case launchFailed
  case invalidReadiness
  case snapshotUnavailable
  case shutdownTimeout
}

struct ProcessDaemonLauncher: DaemonLaunching {
  static func command(
    configuration: DaemonConfiguration
  ) -> DaemonLaunchCommand {
    DaemonLaunchCommand(
      executable: URL(fileURLWithPath: "/bin/sh"),
      arguments: [
        "-c",
        """
        exec 3<&0
        exec 4<&2
        exec 2>/dev/null
        parent_pid=$$
        (
          while IFS= read -r _ <&4; do :; done
          /bin/kill -TERM "$parent_pid" 2>/dev/null
        ) >/dev/null 2>&1 &
        exec "$1" --approved-root "$2" --config "$3"
        """,
        "cmti-daemon",
        configuration.executable.path,
        configuration.approvedRoot.path,
        configuration.configFile.path,
      ],
      environment: [
        "LC_ALL": "C",
        "PATH": "/usr/bin:/bin",
      ]
    )
  }

  func launch(
    configuration: DaemonConfiguration,
    bootstrap: SessionBootstrap
  ) async throws -> any DaemonRuntime {
    let specification = Self.command(configuration: configuration)
    let input = Pipe()
    let output = Pipe()
    let parentLiveness = Pipe()
    let process = Process()
    process.executableURL = specification.executable
    process.arguments = specification.arguments
    process.environment = specification.environment
    process.standardInput = input.fileHandleForReading
    process.standardOutput = output.fileHandleForWriting
    process.standardError = parentLiveness.fileHandleForReading

    do {
      try process.run()
      try input.fileHandleForReading.close()
      try output.fileHandleForWriting.close()
      try parentLiveness.fileHandleForReading.close()
      try bootstrap.writeSecret(to: input.fileHandleForWriting)
      return ProcessDaemonRuntime(
        process: process,
        standardOutput: output.fileHandleForReading,
        parentLiveness: parentLiveness.fileHandleForWriting
      )
    } catch {
      if process.isRunning {
        process.terminate()
      }
      try? input.fileHandleForReading.close()
      try? input.fileHandleForWriting.close()
      try? output.fileHandleForReading.close()
      try? output.fileHandleForWriting.close()
      try? parentLiveness.fileHandleForReading.close()
      try? parentLiveness.fileHandleForWriting.close()
      throw DaemonSupervisorError.launchFailed
    }
  }
}

actor ProcessDaemonRuntime: DaemonRuntime {
  nonisolated let processID: Int32

  private let process: Process
  private let standardOutput: FileHandle
  private let parentLiveness: FileHandle
  private var exitStatus: Int32?
  private var exitWaiters: [CheckedContinuation<Int32, Never>] = []

  init(
    process: Process,
    standardOutput: FileHandle,
    parentLiveness: FileHandle
  ) {
    self.process = process
    processID = process.processIdentifier
    self.standardOutput = standardOutput
    self.parentLiveness = parentLiveness
    if process.isRunning {
      process.terminationHandler = { [weak self] terminated in
        let status = terminated.terminationStatus
        Task {
          await self?.recordExit(status)
        }
      }
    } else {
      exitStatus = process.terminationStatus
    }
  }

  deinit {
    try? standardOutput.close()
    try? parentLiveness.close()
  }

  func readinessLine() async throws -> Data {
    var line = Data()
    for try await byte in standardOutput.bytes {
      if byte == UInt8(ascii: "\n") {
        guard !line.isEmpty else {
          throw DaemonSupervisorError.invalidReadiness
        }
        return line
      }
      guard line.count < 16_384 else {
        throw DaemonSupervisorError.invalidReadiness
      }
      line.append(byte)
    }
    throw DaemonSupervisorError.invalidReadiness
  }

  func terminate() {
    if process.isRunning {
      process.terminate()
    }
  }

  func forceTerminate() {
    if process.isRunning {
      Darwin.kill(process.processIdentifier, SIGKILL)
    }
  }

  func simulateSupervisorLoss() {
    try? parentLiveness.close()
  }

  func waitForExit() async throws -> Int32 {
    if let exitStatus {
      return exitStatus
    }
    return await withCheckedContinuation { continuation in
      exitWaiters.append(continuation)
    }
  }

  private func recordExit(_ status: Int32) {
    guard exitStatus == nil else {
      return
    }
    exitStatus = status
    let waiters = exitWaiters
    exitWaiters.removeAll(keepingCapacity: false)
    for waiter in waiters {
      waiter.resume(returning: status)
    }
  }
}

struct GRPCTransportFactory: RPCTransportBuilding {
  let priceDisplayScale: Int

  init(priceDisplayScale: Int = 2) {
    self.priceDisplayScale = priceDisplayScale
  }

  func makeTransport(
    for descriptor: ReadinessDescriptor
  ) async throws -> any RPCTransport {
    try GRPCTransport(
      endpoint: descriptor.endpoint,
      priceDisplayScale: priceDisplayScale
    )
  }
}

actor DaemonSupervisor {
  private let configuration: DaemonConfiguration
  private let launcher: any DaemonLaunching
  private let transportFactory: any RPCTransportBuilding
  private let startupTimeout: Duration
  private let shutdownTimeout: Duration
  private let nowUnixSeconds: @Sendable () -> Int64

  private var runtime: (any DaemonRuntime)?
  private var startupTask: Task<Data, Error>?
  private var monitorTask: Task<Void, Never>?

  private(set) var currentState: DaemonState = .stopped
  private(set) var latestSnapshot: MarketSnapshot?

  var ownedTaskCount: Int {
    (startupTask == nil ? 0 : 1) + (monitorTask == nil ? 0 : 1)
  }

  init(
    configuration: DaemonConfiguration,
    launcher: any DaemonLaunching,
    transportFactory: any RPCTransportBuilding,
    startupTimeout: Duration,
    shutdownTimeout: Duration,
    nowUnixSeconds: @escaping @Sendable () -> Int64
  ) {
    self.configuration = configuration
    self.launcher = launcher
    self.transportFactory = transportFactory
    self.startupTimeout = startupTimeout
    self.shutdownTimeout = shutdownTimeout
    self.nowUnixSeconds = nowUnixSeconds
  }

  func start() async throws {
    guard currentState == .stopped || currentState == .failed else {
      throw DaemonSupervisorError.alreadyRunning
    }

    currentState = .starting
    latestSnapshot = nil
    let bootstrap: SessionBootstrap
    do {
      bootstrap = try SessionBootstrap()
      let launched = try await launcher.launch(
        configuration: configuration,
        bootstrap: bootstrap
      )
      runtime = launched
      let readiness = Task {
        try await launched.readinessLine()
      }
      startupTask = readiness
      let data = try await Self.value(
        from: readiness,
        before: startupTimeout
      )
      startupTask = nil

      let descriptor = try ReadinessDescriptor(
        jsonData: data,
        expectedDaemonPID: UInt32(launched.processID),
        nowUnixSeconds: nowUnixSeconds()
      )
      do {
        try ProtocolCompatibility.validate(descriptor)
      } catch {
        throw DaemonSupervisorError.incompatibleProtocol
      }
      let credentials = try bootstrap.credentials(for: descriptor)
      let transport = try await transportFactory.makeTransport(
        for: descriptor
      )
      let snapshot = try await transport.getSnapshot(
        using: credentials
      )
      latestSnapshot = snapshot
      currentState = snapshot.health == .healthy ? .healthy : .degraded
      beginMonitoring(launched)
    } catch {
      startupTask?.cancel()
      startupTask = nil
      if currentState == .stopping || currentState == .stopped {
        throw error
      }
      currentState = .failed
      await terminateRuntime()
      throw Self.publicStartupError(error)
    }
  }

  func stop() async throws {
    guard currentState != .stopped else {
      return
    }
    currentState = .stopping
    startupTask?.cancel()
    startupTask = nil
    monitorTask?.cancel()
    monitorTask = nil

    guard let active = runtime else {
      latestSnapshot = nil
      currentState = .stopped
      return
    }
    await active.terminate()
    do {
      _ = try await Self.value(
        before: shutdownTimeout,
        operation: {
          try await active.waitForExit()
        }
      )
    } catch {
      await active.forceTerminate()
      _ = try? await active.waitForExit()
      runtime = nil
      latestSnapshot = nil
      currentState = .stopped
      throw DaemonSupervisorError.shutdownTimeout
    }
    runtime = nil
    latestSnapshot = nil
    currentState = .stopped
  }

  private func beginMonitoring(_ monitored: any DaemonRuntime) {
    monitorTask = Task { [weak self] in
      let status = try? await monitored.waitForExit()
      guard !Task.isCancelled else {
        return
      }
      await self?.processExited(status: status)
    }
  }

  private func processExited(status: Int32?) {
    monitorTask = nil
    runtime = nil
    latestSnapshot = nil
    if currentState != .stopping && currentState != .stopped {
      currentState = .failed
    }
  }

  private func terminateRuntime() async {
    guard let active = runtime else {
      return
    }
    await active.terminate()
    do {
      _ = try await Self.value(
        before: shutdownTimeout,
        operation: {
          try await active.waitForExit()
        }
      )
    } catch {
      await active.forceTerminate()
      _ = try? await active.waitForExit()
    }
    runtime = nil
  }

  private static func publicStartupError(
    _ error: any Error
  ) -> DaemonSupervisorError {
    if let publicError = error as? DaemonSupervisorError {
      return publicError
    }
    if error is CancellationError {
      return .startupTimeout
    }
    if let clientError = error as? ClientContractError {
      switch clientError {
      case .incompatibleProtocol:
        return .incompatibleProtocol
      default:
        return .invalidReadiness
      }
    }
    return .snapshotUnavailable
  }

  private static func value<T: Sendable>(
    from task: Task<T, Error>,
    before timeout: Duration
  ) async throws -> T {
    try await withThrowingTaskGroup(of: TaskRace<T>.self) { group in
      group.addTask {
        .result(await task.result)
      }
      group.addTask {
        do {
          try await Task.sleep(for: timeout)
          return .timeout
        } catch {
          return .cancelled
        }
      }
      guard let first = try await group.next() else {
        task.cancel()
        throw DaemonSupervisorError.startupTimeout
      }
      if case .timeout = first {
        task.cancel()
      }
      group.cancelAll()
      switch first {
      case .result(.success(let value)):
        return value
      case .result(.failure(let error)):
        throw error
      case .timeout:
        throw DaemonSupervisorError.startupTimeout
      case .cancelled:
        throw CancellationError()
      }
    }
  }

  private static func value<T: Sendable>(
    before timeout: Duration,
    operation: @escaping @Sendable () async throws -> T
  ) async throws -> T {
    let task = Task {
      try await operation()
    }
    return try await value(from: task, before: timeout)
  }
}

private enum TaskRace<Value: Sendable>: @unchecked Sendable {
  case result(Result<Value, any Error>)
  case timeout
  case cancelled
}
