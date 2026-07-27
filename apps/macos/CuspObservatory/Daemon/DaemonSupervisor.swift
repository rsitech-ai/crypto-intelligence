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

struct DaemonSupervisorEvent: Sendable, Equatable {
  let state: DaemonState
  let snapshot: MarketSnapshot?
}

private enum RuntimeTerminationResult: Equatable {
  case graceful
  case forced
  case unresolved
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
  private var exitWaiters: [UUID: CheckedContinuation<Int32, any Error>] = [:]

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
    let identifier = UUID()
    return try await withTaskCancellationHandler {
      try await withCheckedThrowingContinuation { continuation in
        if let exitStatus {
          continuation.resume(returning: exitStatus)
        } else if Task.isCancelled {
          continuation.resume(throwing: CancellationError())
        } else {
          exitWaiters[identifier] = continuation
        }
      }
    } onCancel: {
      Task {
        await self.cancelExitWaiter(identifier)
      }
    }
  }

  private func cancelExitWaiter(_ identifier: UUID) {
    guard
      let waiter = exitWaiters.removeValue(
        forKey: identifier
      )
    else {
      return
    }
    waiter.resume(throwing: CancellationError())
  }

  private func recordExit(_ status: Int32) {
    guard exitStatus == nil else {
      return
    }
    exitStatus = status
    let waiters = Array(exitWaiters.values)
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
  private struct StartupOwnership {
    let runID: UUID
    let task: Task<MarketSnapshot, Error>
  }

  private struct RuntimeOwnership {
    let runID: UUID
    let runtime: any DaemonRuntime
  }

  private struct StopOwnership {
    let stopID: UUID
    let runtime: RuntimeOwnership
    let task: Task<RuntimeTerminationResult, Never>
  }

  private let configuration: DaemonConfiguration
  private let launcher: any DaemonLaunching
  private let transportFactory: any RPCTransportBuilding
  private let startupTimeout: Duration
  private let shutdownTimeout: Duration
  private let nowUnixSeconds: @Sendable () -> Int64

  private var runtimeOwnership: RuntimeOwnership?
  private var startupOwnership: StartupOwnership?
  private var stopOwnership: StopOwnership?
  private var monitorTask: Task<Void, Never>?
  private var activeRunID: UUID?
  private var eventContinuations: [UUID: AsyncStream<DaemonSupervisorEvent>.Continuation] = [:]

  private(set) var currentState: DaemonState = .stopped
  private(set) var latestSnapshot: MarketSnapshot?
  private(set) var activeStopCallerCount = 0

  var ownedTaskCount: Int {
    (startupOwnership == nil ? 0 : 1)
      + (monitorTask == nil ? 0 : 1)
      + (stopOwnership == nil ? 0 : 1)
  }

  func events() -> AsyncStream<DaemonSupervisorEvent> {
    let identifier = UUID()
    let (stream, continuation) = AsyncStream.makeStream(
      of: DaemonSupervisorEvent.self,
      bufferingPolicy: .bufferingNewest(1)
    )
    eventContinuations[identifier] = continuation
    continuation.yield(
      DaemonSupervisorEvent(
        state: currentState,
        snapshot: latestSnapshot
      )
    )
    continuation.onTermination = { [weak self] _ in
      Task {
        await self?.removeEventContinuation(identifier)
      }
    }
    return stream
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
    guard
      currentState == .stopped || currentState == .failed,
      runtimeOwnership == nil,
      startupOwnership == nil,
      stopOwnership == nil,
      monitorTask == nil
    else {
      throw DaemonSupervisorError.alreadyRunning
    }

    let runID = UUID()
    activeRunID = runID
    publish(state: .starting, snapshot: nil)
    let deadline = ContinuousClock.now.advanced(by: startupTimeout)
    let configuration = configuration
    let launcher = launcher
    let transportFactory = transportFactory
    let nowUnixSeconds = nowUnixSeconds
    let shutdownTimeout = shutdownTimeout
    let startup = Task { [self] in
      let bootstrap = try SessionBootstrap()
      try Task.checkCancellation()
      let launched = try await launcher.launch(
        configuration: configuration,
        bootstrap: bootstrap
      )
      do {
        try Task.checkCancellation()
        try registerRuntime(launched, for: runID)
        try Task.checkCancellation()
        try ensureActive(runID)

        let data = try await launched.readinessLine()
        try Task.checkCancellation()
        try ensureActive(runID)

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
        try Task.checkCancellation()
        try ensureActive(runID)

        let remaining = try Self.remaining(until: deadline)
        let snapshot = try await transport.getSnapshot(
          using: credentials,
          timeout: remaining
        )
        try Task.checkCancellation()
        try ensureActive(runID)
        return snapshot
      } catch {
        if ownsRuntime(launched, for: runID) {
          throw error
        }
        await Self.stopUnregisteredRuntime(
          launched,
          timeout: shutdownTimeout
        )
        throw error
      }
    }
    startupOwnership = StartupOwnership(runID: runID, task: startup)

    do {
      let remaining = try Self.remaining(until: deadline)
      let snapshot = try await Self.value(
        from: startup,
        before: remaining
      )
      clearStartupOwnership(for: runID)
      try ensureActive(runID)
      guard let launched = runtimeOwnership(for: runID)?.runtime else {
        throw DaemonSupervisorError.launchFailed
      }
      publish(
        state: snapshot.health == .healthy ? .healthy : .degraded,
        snapshot: snapshot
      )
      beginMonitoring(launched, runID: runID)
    } catch {
      startup.cancel()
      clearStartupOwnership(for: runID)
      let callerCancelled = Task.isCancelled
      guard activeRunID == runID,
        currentState != .stopping,
        currentState != .stopped
      else {
        throw CancellationError()
      }
      activeRunID = nil
      publish(state: .failed, snapshot: nil)
      await terminateRuntime(for: runID)
      if callerCancelled {
        throw CancellationError()
      }
      throw Self.publicStartupError(error)
    }
  }

  func stop() async throws {
    guard currentState != .stopped else {
      return
    }
    activeStopCallerCount += 1
    defer {
      activeStopCallerCount -= 1
    }
    let stopping: StopOwnership
    if let activeStop = stopOwnership {
      stopping = activeStop
    } else {
      activeRunID = nil
      publish(state: .stopping, snapshot: nil)
      startupOwnership?.task.cancel()
      startupOwnership = nil
      monitorTask?.cancel()
      monitorTask = nil

      guard let active = runtimeOwnership else {
        publish(state: .stopped, snapshot: nil)
        return
      }
      let runtime = active.runtime
      let timeout = shutdownTimeout
      stopping = StopOwnership(
        stopID: UUID(),
        runtime: active,
        task: Task {
          await Self.terminate(runtime, timeout: timeout)
        }
      )
      stopOwnership = stopping
    }

    let result = await stopping.task.value
    try completeStop(result, ownership: stopping)
  }

  private func completeStop(
    _ result: RuntimeTerminationResult,
    ownership: StopOwnership
  ) throws {
    let ownsCompletion = stopOwnership?.stopID == ownership.stopID
    if ownsCompletion {
      stopOwnership = nil
    }
    switch result {
    case .graceful:
      if ownsCompletion {
        clearRuntimeOwnership(for: ownership.runtime.runID)
        publish(state: .stopped, snapshot: nil)
      }
    case .forced:
      if ownsCompletion {
        clearRuntimeOwnership(for: ownership.runtime.runID)
        publish(state: .stopped, snapshot: nil)
      }
      throw DaemonSupervisorError.shutdownTimeout
    case .unresolved:
      if ownsCompletion {
        publish(state: .failed, snapshot: nil)
      }
      throw DaemonSupervisorError.shutdownTimeout
    }
  }

  private func beginMonitoring(
    _ monitored: any DaemonRuntime,
    runID: UUID
  ) {
    monitorTask = Task { [weak self] in
      let status = try? await monitored.waitForExit()
      guard !Task.isCancelled else {
        return
      }
      await self?.processExited(status: status, runID: runID)
    }
  }

  private func processExited(
    status: Int32?,
    runID: UUID
  ) {
    guard activeRunID == runID else {
      return
    }
    monitorTask = nil
    clearRuntimeOwnership(for: runID)
    activeRunID = nil
    if currentState != .stopping && currentState != .stopped {
      publish(state: .failed, snapshot: nil)
    }
  }

  private func registerRuntime(
    _ launched: any DaemonRuntime,
    for runID: UUID
  ) throws {
    try ensureActive(runID)
    runtimeOwnership = RuntimeOwnership(runID: runID, runtime: launched)
  }

  private func ownsRuntime(
    _ launched: any DaemonRuntime,
    for runID: UUID
  ) -> Bool {
    activeRunID == runID
      && runtimeOwnership?.runID == runID
      && runtimeOwnership?.runtime.processID == launched.processID
  }

  private func ensureActive(_ runID: UUID) throws {
    guard activeRunID == runID, currentState == .starting else {
      throw CancellationError()
    }
  }

  private func clearStartupOwnership(for runID: UUID) {
    guard startupOwnership?.runID == runID else {
      return
    }
    startupOwnership = nil
  }

  private func runtimeOwnership(for runID: UUID) -> RuntimeOwnership? {
    guard runtimeOwnership?.runID == runID else {
      return nil
    }
    return runtimeOwnership
  }

  private func clearRuntimeOwnership(for runID: UUID) {
    guard runtimeOwnership?.runID == runID else {
      return
    }
    runtimeOwnership = nil
  }

  private func publish(
    state: DaemonState,
    snapshot: MarketSnapshot?
  ) {
    currentState = state
    latestSnapshot = snapshot
    let event = DaemonSupervisorEvent(
      state: state,
      snapshot: snapshot
    )
    for continuation in eventContinuations.values {
      continuation.yield(event)
    }
  }

  private func removeEventContinuation(_ identifier: UUID) {
    eventContinuations[identifier] = nil
  }

  private func terminateRuntime(for runID: UUID) async {
    guard let active = runtimeOwnership(for: runID) else {
      return
    }
    let result = await Self.terminate(
      active.runtime,
      timeout: shutdownTimeout
    )
    if result != .unresolved {
      clearRuntimeOwnership(for: runID)
    }
  }

  private static func terminate(
    _ active: any DaemonRuntime,
    timeout: Duration
  ) async -> RuntimeTerminationResult {
    await active.terminate()
    do {
      _ = try await Self.value(
        before: timeout,
        operation: {
          try await active.waitForExit()
        }
      )
      return .graceful
    } catch {
      await active.forceTerminate()
      do {
        _ = try await Self.value(
          before: timeout,
          operation: {
            try await active.waitForExit()
          }
        )
        return .forced
      } catch {
        return .unresolved
      }
    }
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
    if error as? GRPCTransportError == .timeout {
      return .startupTimeout
    }
    return .snapshotUnavailable
  }

  private static func remaining(
    until deadline: ContinuousClock.Instant
  ) throws -> Duration {
    let remaining = ContinuousClock.now.duration(to: deadline)
    guard remaining > .zero else {
      throw DaemonSupervisorError.startupTimeout
    }
    return remaining
  }

  private static func stopUnregisteredRuntime(
    _ runtime: any DaemonRuntime,
    timeout: Duration
  ) async {
    await runtime.terminate()
    do {
      _ = try await value(
        before: timeout,
        operation: {
          try await runtime.waitForExit()
        }
      )
    } catch {
      await runtime.forceTerminate()
      _ = try? await value(
        before: timeout,
        operation: {
          try await runtime.waitForExit()
        }
      )
    }
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
      if case .cancelled = first {
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
