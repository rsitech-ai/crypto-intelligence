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

  @Test("total startup deadline includes the first market RPC")
  func startupDeadlineIncludesFirstRPC() async throws {
    let clock = ContinuousClock()
    let started = clock.now
    let runtime = FakeDaemonRuntime(
      readiness: .data(readiness()),
      processID: 7
    )
    let supervisor = DaemonSupervisor(
      configuration: configuration(),
      launcher: FakeLauncher(runtime: runtime),
      transportFactory: DelayedTransportFactory(
        delay: .milliseconds(120)
      ),
      startupTimeout: .milliseconds(30),
      shutdownTimeout: .milliseconds(100),
      nowUnixSeconds: { 1_700_000_000 }
    )

    await #expect(throws: DaemonSupervisorError.startupTimeout) {
      try await supervisor.start()
    }
    #expect(await supervisor.currentState == .failed)
    #expect(await supervisor.latestSnapshot == nil)
    #expect(await runtime.wasTerminated)
    #expect(clock.now - started < .milliseconds(100))
  }

  @Test("stop during the first RPC prevents late healthy resurrection")
  func stopDuringRPCWinsRunRace() async throws {
    let runtime = FakeDaemonRuntime(
      readiness: .data(readiness()),
      processID: 7
    )
    let transport = BlockingTransport()
    let supervisor = DaemonSupervisor(
      configuration: configuration(),
      launcher: FakeLauncher(runtime: runtime),
      transportFactory: BlockingTransportFactory(
        transport: transport
      ),
      startupTimeout: .seconds(1),
      shutdownTimeout: .milliseconds(100),
      nowUnixSeconds: { 1_700_000_000 }
    )
    let starting = Task {
      try await supervisor.start()
    }
    try await transport.waitUntilStarted()

    try await supervisor.stop()
    #expect(await supervisor.currentState == .stopped)
    await transport.release()
    _ = await starting.result
    try await Task.sleep(for: .milliseconds(20))

    #expect(await supervisor.currentState == .stopped)
    #expect(await supervisor.latestSnapshot == nil)
    #expect(await supervisor.ownedTaskCount == 0)
  }

  @Test("late run cleanup preserves the replacement startup owner")
  func lateRunCleanupPreservesReplacementStartup() async throws {
    let firstGate = SequencedLaunchGate()
    let secondGate = SequencedLaunchGate()
    let launcher = SequencedLauncher(
      steps: [
        SequencedLaunchStep(
          gate: firstGate,
          runtime: FakeDaemonRuntime(
            readiness: .data(readiness()),
            processID: 7
          ),
          ignoresCancellation: true
        ),
        SequencedLaunchStep(
          gate: secondGate,
          runtime: FakeDaemonRuntime(
            readiness: .data(readiness()),
            processID: 8
          ),
          ignoresCancellation: false
        ),
      ]
    )
    let supervisor = DaemonSupervisor(
      configuration: configuration(),
      launcher: launcher,
      transportFactory: StaticTransportFactory(),
      startupTimeout: .seconds(2),
      shutdownTimeout: .milliseconds(100),
      nowUnixSeconds: { 1_700_000_000 }
    )
    let firstCompletion = CompletionProbe()
    let firstStart = Task {
      _ = try? await supervisor.start()
      await firstCompletion.markCompleted()
    }
    try await launcher.waitUntilLaunchCount(1)

    try await supervisor.stop()
    let secondCompletion = CompletionProbe()
    let secondStart = Task {
      _ = try? await supervisor.start()
      await secondCompletion.markCompleted()
    }
    try await launcher.waitUntilLaunchCount(2)

    await firstGate.release()
    try await waitUntilCompleted(firstCompletion)

    #expect(await supervisor.ownedTaskCount == 1)
    await #expect(throws: DaemonSupervisorError.alreadyRunning) {
      try await supervisor.start()
    }
    #expect(await launcher.launchCount == 2)

    try await supervisor.stop()
    try? await waitUntilCompleted(
      secondCompletion,
      timeout: .milliseconds(50)
    )
    let secondWasCancelled = await secondCompletion.isCompleted
    if !secondWasCancelled {
      await secondGate.release()
    }
    _ = await firstStart.result
    _ = await secondStart.result

    #expect(secondWasCancelled)
    #expect(await supervisor.currentState == .stopped)
    #expect(await supervisor.ownedTaskCount == 0)
    #expect(await launcher.launchCount == 2)
  }

  @Test("late termination cleanup preserves the replacement runtime")
  func lateTerminationCleanupPreservesReplacementRuntime() async throws {
    let firstWait = SequencedLaunchGate()
    let secondWait = SequencedLaunchGate()
    let firstRuntime = ReentrantTerminationRuntime(
      processID: 7,
      waitGates: [firstWait, secondWait]
    )
    let secondRuntime = FakeDaemonRuntime(
      readiness: .data(readiness()),
      processID: 7
    )
    let transport = BlockingTransport()
    let supervisor = DaemonSupervisor(
      configuration: configuration(),
      launcher: SequencedRuntimeLauncher(
        runtimes: [firstRuntime, secondRuntime]
      ),
      transportFactory: SequencedTransportFactory(
        transports: [FailingTransport(), transport]
      ),
      startupTimeout: .seconds(2),
      shutdownTimeout: .seconds(1),
      nowUnixSeconds: { 1_700_000_000 }
    )
    let firstCompletion = CompletionProbe()
    let firstStart = Task {
      _ = try? await supervisor.start()
      await firstCompletion.markCompleted()
    }
    try await firstRuntime.waitUntilWaitCount(1)

    let stopping = Task {
      try await supervisor.stop()
    }
    try await firstRuntime.waitUntilWaitCount(2)
    await secondWait.release()
    try await stopping.value

    let secondStart = Task {
      try await supervisor.start()
    }
    try await transport.waitUntilStarted()

    await firstWait.release()
    try await waitUntilCompleted(firstCompletion)
    await transport.release()
    let secondResult = await secondStart.result

    #expect(throws: Never.self) {
      try secondResult.get()
    }
    #expect(await supervisor.currentState == .healthy)
    try await supervisor.stop()
    _ = await firstStart.result

    #expect(await secondRuntime.wasTerminated)
    #expect(await supervisor.currentState == .stopped)
    #expect(await supervisor.ownedTaskCount == 0)
  }

  @Test("concurrent graceful stop preserves the replacement monitor lifecycle")
  func concurrentGracefulStopPreservesReplacementLifecycle() async throws {
    let monitorGate = SequencedLaunchGate()
    let stopGate = SequencedLaunchGate()
    let duplicateStopGate = SequencedLaunchGate()
    let firstRuntime = ReentrantTerminationRuntime(
      processID: 7,
      waitGates: [monitorGate, stopGate, duplicateStopGate]
    )
    let secondRuntime = FakeDaemonRuntime(
      readiness: .data(readiness()),
      processID: 7
    )
    let replacementSnapshot = makeSnapshot()
    let supervisor = DaemonSupervisor(
      configuration: configuration(),
      launcher: SequencedRuntimeLauncher(
        runtimes: [firstRuntime, secondRuntime]
      ),
      transportFactory: SequencedTransportFactory(
        transports: [
          StaticTransport(snapshot: makeSnapshot()),
          StaticTransport(snapshot: replacementSnapshot),
        ]
      ),
      startupTimeout: .seconds(2),
      shutdownTimeout: .seconds(1),
      nowUnixSeconds: { 1_700_000_000 }
    )
    try await supervisor.start()
    try await firstRuntime.waitUntilWaitCount(1)

    let firstStop = Task { await stopResult(from: supervisor) }
    try await firstRuntime.waitUntilWaitCount(2)
    let secondStop = Task { await stopResult(from: supervisor) }
    try await waitUntilActiveStopCallerCount(2, supervisor: supervisor)

    await stopGate.release()
    await duplicateStopGate.release()
    #expect(await firstStop.value == .success)
    #expect(await secondStop.value == .success)
    #expect(await firstRuntime.waitCount == 2)
    #expect(await supervisor.currentState == .stopped)
    #expect(await supervisor.activeStopCallerCount == 0)

    try await supervisor.start()
    #expect(await supervisor.currentState == .healthy)
    #expect(await supervisor.latestSnapshot == replacementSnapshot)
    #expect(!(await secondRuntime.wasTerminated))
    #expect(await supervisor.ownedTaskCount == 1)

    await secondRuntime.terminate()
    try await waitUntilState(.failed, supervisor: supervisor)
    #expect(await supervisor.latestSnapshot == nil)
    #expect(await supervisor.ownedTaskCount == 0)
    await monitorGate.release()
  }

  @Test("concurrent stop callers share one forced termination operation")
  func concurrentStopsShareForcedTermination() async throws {
    let monitorGate = SequencedLaunchGate()
    let forcedWaitGate = SequencedLaunchGate()
    let runtime = CountedStopRuntime(
      processID: 7,
      monitorGate: monitorGate,
      forcedWaitGate: forcedWaitGate,
      duplicateWaitGate: SequencedLaunchGate(),
      firstForcedWaitIsUnresolved: false
    )
    let supervisor = DaemonSupervisor(
      configuration: configuration(),
      launcher: SequencedRuntimeLauncher(runtimes: [runtime]),
      transportFactory: StaticTransportFactory(),
      startupTimeout: .seconds(1),
      shutdownTimeout: .seconds(1),
      nowUnixSeconds: { 1_700_000_000 }
    )
    try await supervisor.start()
    try await runtime.waitUntilWaitCount(1)

    let firstStop = Task { await stopResult(from: supervisor) }
    try await runtime.waitUntilWaitCount(3)
    let secondStop = Task { await stopResult(from: supervisor) }
    try await waitUntilActiveStopCallerCount(2, supervisor: supervisor)

    firstStop.cancel()
    #expect(await supervisor.ownedTaskCount == 1)
    #expect(await supervisor.activeStopCallerCount == 2)
    await forcedWaitGate.release()
    await runtime.releaseDuplicateWait()

    let expected = StopResult.failure(.shutdownTimeout)
    #expect(await firstStop.value == expected)
    #expect(await secondStop.value == expected)
    #expect(await runtime.terminateCount == 1)
    #expect(await runtime.forceTerminationCount == 1)
    #expect(await runtime.waitCount == 3)
    #expect(await supervisor.currentState == .stopped)
    #expect(await supervisor.ownedTaskCount == 0)
    #expect(await supervisor.activeStopCallerCount == 0)
    await monitorGate.release()
  }

  @Test("unresolved shared stop is released and a retry owns a fresh operation")
  func unresolvedSharedStopCanBeRetried() async throws {
    let monitorGate = SequencedLaunchGate()
    let unresolvedWaitGate = SequencedLaunchGate()
    let runtime = CountedStopRuntime(
      processID: 7,
      monitorGate: monitorGate,
      forcedWaitGate: unresolvedWaitGate,
      duplicateWaitGate: SequencedLaunchGate(),
      firstForcedWaitIsUnresolved: true
    )
    let supervisor = DaemonSupervisor(
      configuration: configuration(),
      launcher: SequencedRuntimeLauncher(runtimes: [runtime]),
      transportFactory: StaticTransportFactory(),
      startupTimeout: .seconds(1),
      shutdownTimeout: .seconds(1),
      nowUnixSeconds: { 1_700_000_000 }
    )
    try await supervisor.start()
    try await runtime.waitUntilWaitCount(1)
    let recorder = StateRecorder()
    let events = await supervisor.events()
    let observing = Task {
      for await event in events {
        await recorder.record(event.state)
      }
    }
    defer { observing.cancel() }

    let firstStop = Task { await stopResult(from: supervisor) }
    try await runtime.waitUntilWaitCount(3)
    let secondStop = Task { await stopResult(from: supervisor) }
    try await waitUntilActiveStopCallerCount(2, supervisor: supervisor)

    #expect(await supervisor.ownedTaskCount == 1)
    await unresolvedWaitGate.release()
    await runtime.releaseDuplicateWait()

    let expected = StopResult.failure(.shutdownTimeout)
    #expect(await firstStop.value == expected)
    #expect(await secondStop.value == expected)
    try await recorder.waitUntilCount(of: .failed, isAtLeast: 1)
    #expect(await recorder.count(of: .failed) == 1)
    #expect(await runtime.terminateCount == 1)
    #expect(await runtime.forceTerminationCount == 1)
    #expect(await runtime.waitCount == 3)
    #expect(await supervisor.currentState == .failed)
    #expect(await supervisor.latestSnapshot == nil)
    #expect(await supervisor.ownedTaskCount == 0)
    #expect(await supervisor.activeStopCallerCount == 0)
    await #expect(throws: DaemonSupervisorError.alreadyRunning) {
      try await supervisor.start()
    }

    await runtime.resetDuplicateWait()
    let retryStop = Task { await stopResult(from: supervisor) }
    try await runtime.waitUntilWaitCount(4)
    try await waitUntilActiveStopCallerCount(1, supervisor: supervisor)
    #expect(await supervisor.ownedTaskCount == 1)
    await runtime.releaseDuplicateWait()
    #expect(await retryStop.value == .success)
    #expect(await runtime.terminateCount == 2)
    #expect(await runtime.forceTerminationCount == 1)
    #expect(await runtime.waitCount == 4)
    #expect(await supervisor.currentState == .stopped)
    #expect(await supervisor.ownedTaskCount == 0)
    await monitorGate.release()
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

  @Test("cancelling the start caller cancels the owned startup pipeline")
  func callerCancellationStopsStartupPipeline() async throws {
    let runtime = FakeDaemonRuntime(
      readiness: .suspended,
      processID: 7
    )
    let supervisor = DaemonSupervisor(
      configuration: configuration(),
      launcher: FakeLauncher(runtime: runtime),
      transportFactory: StaticTransportFactory(),
      startupTimeout: .seconds(10),
      shutdownTimeout: .milliseconds(100),
      nowUnixSeconds: { 1_700_000_000 }
    )
    let completion = CompletionProbe()
    let starting = Task {
      try await supervisor.start()
    }
    let observing = Task {
      _ = await starting.result
      await completion.markCompleted()
    }
    try await Task.sleep(for: .milliseconds(20))

    starting.cancel()
    try await Task.sleep(for: .milliseconds(50))
    let completedBeforeExplicitStop = await completion.isCompleted
    #expect(completedBeforeExplicitStop)

    try await supervisor.stop()
    _ = await observing.result
    #expect(await runtime.wasTerminated)
    #expect(await supervisor.ownedTaskCount == 0)
  }

  @Test("SIGTERM-resistant child escalates and reports shutdown timeout")
  func resistantChildShutdownIsBounded() async throws {
    let clock = ContinuousClock()
    let fixture = try await launchProcessFixture(
      named: "term-resistant-daemon",
      beforeReadiness: "trap '' TERM",
      afterReadiness: "while true; do sleep 1; done"
    )
    defer {
      Darwin.kill(fixture.runtime.processID, SIGKILL)
      try? FileManager.default.removeItem(at: fixture.root)
    }
    let supervisor = DaemonSupervisor(
      configuration: fixture.configuration,
      launcher: ExistingRuntimeLauncher(runtime: fixture.runtime),
      transportFactory: StaticTransportFactory(),
      startupTimeout: .seconds(1),
      shutdownTimeout: .milliseconds(40),
      nowUnixSeconds: {
        Int64(Date().timeIntervalSince1970)
      }
    )
    try await supervisor.start()
    let started = clock.now
    let completion = CompletionProbe()
    let stopping = Task {
      let result: StopResult
      do {
        try await supervisor.stop()
        result = .success
      } catch let error as DaemonSupervisorError {
        result = .failure(error)
      } catch {
        result = .unexpectedFailure
      }
      await completion.markCompleted()
      return result
    }

    try await Task.sleep(for: .milliseconds(200))
    let completedWithinBound = await completion.isCompleted
    #expect(completedWithinBound)
    if !completedWithinBound {
      Darwin.kill(fixture.runtime.processID, SIGKILL)
    }
    let stopResult = await stopping.value
    let stopCompletedAt = clock.now
    #expect(stopResult == .failure(.shutdownTimeout))
    #expect(stopCompletedAt - started < .milliseconds(300))
    await fixture.runtime.simulateSupervisorLoss()

    let cleanupDeadline = clock.now.advanced(by: .seconds(2))
    var table = try processTable()
    while table.contains(fixture.root.path), clock.now < cleanupDeadline {
      try await Task.sleep(for: .milliseconds(10))
      table = try processTable()
    }
    #expect(Darwin.kill(fixture.runtime.processID, 0) == -1)
    #expect(errno == ESRCH)
    #expect(!table.contains(fixture.root.path))
  }

  @Test("unconfirmed force termination remains failed and owned")
  func unconfirmedForceTerminationFailsClosed() async throws {
    let runtime = UnkillableDaemonRuntime(processID: 7)
    let supervisor = DaemonSupervisor(
      configuration: configuration(),
      launcher: UnkillableRuntimeLauncher(runtime: runtime),
      transportFactory: StaticTransportFactory(),
      startupTimeout: .seconds(1),
      shutdownTimeout: .milliseconds(20),
      nowUnixSeconds: { 1_700_000_000 }
    )
    try await supervisor.start()

    await #expect(throws: DaemonSupervisorError.shutdownTimeout) {
      try await supervisor.stop()
    }

    #expect(await supervisor.currentState == .failed)
    #expect(await supervisor.latestSnapshot == nil)
    #expect(await runtime.forceTerminationCount == 1)
    await #expect(throws: DaemonSupervisorError.alreadyRunning) {
      try await supervisor.start()
    }
    #expect(await runtime.readinessCallCount == 1)
  }

  @Test("exit waiters complete once across terminate and exit races")
  func exitWaitersSurviveTerminationRace() async throws {
    let fixture = try await launchProcessFixture(
      named: "termination-race-daemon",
      afterReadiness: "while true; do sleep 1; done"
    )
    defer {
      Darwin.kill(fixture.runtime.processID, SIGKILL)
      try? FileManager.default.removeItem(at: fixture.root)
    }
    let waiters = (0..<24).map { _ in
      Task {
        try await fixture.runtime.waitForExit()
      }
    }

    await fixture.runtime.terminate()
    let results = await waiters.asyncMap { waiter in
      await waiter.result
    }
    let statuses = results.compactMap { try? $0.get() }
    #expect(statuses.count == waiters.count)
    #expect(Set(statuses).count == 1)
    await fixture.runtime.simulateSupervisorLoss()

    var table = try processTable()
    for _ in 0..<50 where table.contains(fixture.root.path) {
      try await Task.sleep(for: .milliseconds(10))
      table = try processTable()
    }
    #expect(!table.contains(fixture.root.path))
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

private actor CompletionProbe {
  private(set) var isCompleted = false

  func markCompleted() {
    isCompleted = true
  }
}

private func waitUntilCompleted(
  _ probe: CompletionProbe,
  timeout: Duration = .milliseconds(300)
) async throws {
  let deadline = ContinuousClock.now.advanced(by: timeout)
  while !(await probe.isCompleted), ContinuousClock.now < deadline {
    try await Task.sleep(for: .milliseconds(5))
  }
  guard await probe.isCompleted else {
    throw DaemonSupervisorError.startupTimeout
  }
}

private func waitUntilState(
  _ expected: DaemonState,
  supervisor: DaemonSupervisor,
  timeout: Duration = .milliseconds(300)
) async throws {
  let deadline = ContinuousClock.now.advanced(by: timeout)
  while await supervisor.currentState != expected,
    ContinuousClock.now < deadline
  {
    try await Task.sleep(for: .milliseconds(5))
  }
  guard await supervisor.currentState == expected else {
    throw DaemonSupervisorError.startupTimeout
  }
}

private func waitUntilActiveStopCallerCount(
  _ expected: Int,
  supervisor: DaemonSupervisor,
  timeout: Duration = .milliseconds(300)
) async throws {
  let deadline = ContinuousClock.now.advanced(by: timeout)
  while await supervisor.activeStopCallerCount != expected,
    ContinuousClock.now < deadline
  {
    try await Task.sleep(for: .milliseconds(5))
  }
  guard await supervisor.activeStopCallerCount == expected else {
    throw DaemonSupervisorError.shutdownTimeout
  }
}

private actor StateRecorder {
  private var states: [DaemonState] = []

  func record(_ state: DaemonState) {
    states.append(state)
  }

  func count(of state: DaemonState) -> Int {
    states.count { $0 == state }
  }

  func waitUntilCount(
    of state: DaemonState,
    isAtLeast expected: Int,
    timeout: Duration = .milliseconds(300)
  ) async throws {
    let deadline = ContinuousClock.now.advanced(by: timeout)
    while count(of: state) < expected, ContinuousClock.now < deadline {
      try await Task.sleep(for: .milliseconds(5))
    }
    guard count(of: state) >= expected else {
      throw DaemonSupervisorError.startupTimeout
    }
  }
}

private enum StopResult: Sendable, Equatable {
  case success
  case failure(DaemonSupervisorError)
  case unexpectedFailure
}

private func stopResult(
  from supervisor: DaemonSupervisor
) async -> StopResult {
  do {
    try await supervisor.stop()
    return .success
  } catch let error as DaemonSupervisorError {
    return .failure(error)
  } catch {
    return .unexpectedFailure
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

private struct SequencedLaunchStep: Sendable {
  let gate: SequencedLaunchGate
  let runtime: FakeDaemonRuntime
  let ignoresCancellation: Bool
}

private actor SequencedLauncher: DaemonLaunching {
  private var steps: [SequencedLaunchStep]
  private(set) var launchCount = 0

  init(steps: [SequencedLaunchStep]) {
    self.steps = steps
  }

  func launch(
    configuration: DaemonConfiguration,
    bootstrap: SessionBootstrap
  ) async throws -> any DaemonRuntime {
    guard !steps.isEmpty else {
      launchCount += 1
      throw DaemonSupervisorError.launchFailed
    }
    launchCount += 1
    let step = steps.removeFirst()
    if step.ignoresCancellation {
      await step.gate.waitIgnoringCancellation()
    } else {
      try await step.gate.waitCooperatively()
    }
    return step.runtime
  }

  func waitUntilLaunchCount(_ expected: Int) async throws {
    let deadline = ContinuousClock.now.advanced(by: .milliseconds(300))
    while launchCount < expected, ContinuousClock.now < deadline {
      try await Task.sleep(for: .milliseconds(5))
    }
    guard launchCount >= expected else {
      throw DaemonSupervisorError.launchFailed
    }
  }
}

private actor SequencedLaunchGate {
  private var isReleased = false
  private var waiter: CheckedContinuation<Void, Never>?

  func waitIgnoringCancellation() async {
    guard !isReleased else {
      return
    }
    await withCheckedContinuation { continuation in
      waiter = continuation
    }
  }

  func waitCooperatively() async throws {
    while !isReleased {
      try await Task.sleep(for: .milliseconds(5))
    }
  }

  func release() {
    isReleased = true
    waiter?.resume()
    waiter = nil
  }
}

private actor SequencedRuntimeLauncher: DaemonLaunching {
  private var runtimes: [any DaemonRuntime]

  init(runtimes: [any DaemonRuntime]) {
    self.runtimes = runtimes
  }

  func launch(
    configuration: DaemonConfiguration,
    bootstrap: SessionBootstrap
  ) throws -> any DaemonRuntime {
    guard !runtimes.isEmpty else {
      throw DaemonSupervisorError.launchFailed
    }
    return runtimes.removeFirst()
  }
}

private struct ExistingRuntimeLauncher: DaemonLaunching {
  let runtime: ProcessDaemonRuntime

  func launch(
    configuration: DaemonConfiguration,
    bootstrap: SessionBootstrap
  ) -> any DaemonRuntime {
    runtime
  }
}

private struct UnkillableRuntimeLauncher: DaemonLaunching {
  let runtime: UnkillableDaemonRuntime

  func launch(
    configuration: DaemonConfiguration,
    bootstrap: SessionBootstrap
  ) -> any DaemonRuntime {
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

private actor UnkillableDaemonRuntime: DaemonRuntime {
  nonisolated let processID: Int32
  private(set) var forceTerminationCount = 0
  private(set) var readinessCallCount = 0

  init(processID: Int32) {
    self.processID = processID
  }

  func readinessLine() -> Data {
    readinessCallCount += 1
    return readiness()
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

private actor ReentrantTerminationRuntime: DaemonRuntime {
  nonisolated let processID: Int32
  private let waitGates: [SequencedLaunchGate]
  private(set) var waitCount = 0

  init(processID: Int32, waitGates: [SequencedLaunchGate]) {
    self.processID = processID
    self.waitGates = waitGates
  }

  func readinessLine() -> Data {
    readiness()
  }

  func terminate() {}

  func forceTerminate() {}

  func waitForExit() async -> Int32 {
    let gate = waitGates[waitCount]
    waitCount += 1
    await gate.waitIgnoringCancellation()
    return 0
  }

  func waitUntilWaitCount(_ expected: Int) async throws {
    let deadline = ContinuousClock.now.advanced(by: .milliseconds(300))
    while waitCount < expected, ContinuousClock.now < deadline {
      try await Task.sleep(for: .milliseconds(5))
    }
    guard waitCount >= expected else {
      throw DaemonSupervisorError.shutdownTimeout
    }
  }
}

private actor CountedStopRuntime: DaemonRuntime {
  nonisolated let processID: Int32
  private let monitorGate: SequencedLaunchGate
  private let forcedWaitGate: SequencedLaunchGate
  private var duplicateWaitGate: SequencedLaunchGate
  private let firstForcedWaitIsUnresolved: Bool
  private(set) var terminateCount = 0
  private(set) var forceTerminationCount = 0
  private(set) var waitCount = 0

  init(
    processID: Int32,
    monitorGate: SequencedLaunchGate,
    forcedWaitGate: SequencedLaunchGate,
    duplicateWaitGate: SequencedLaunchGate,
    firstForcedWaitIsUnresolved: Bool
  ) {
    self.processID = processID
    self.monitorGate = monitorGate
    self.forcedWaitGate = forcedWaitGate
    self.duplicateWaitGate = duplicateWaitGate
    self.firstForcedWaitIsUnresolved = firstForcedWaitIsUnresolved
  }

  func readinessLine() -> Data {
    readiness()
  }

  func terminate() {
    terminateCount += 1
  }

  func forceTerminate() {
    forceTerminationCount += 1
  }

  func waitForExit() async throws -> Int32 {
    waitCount += 1
    switch waitCount {
    case 1:
      await monitorGate.waitIgnoringCancellation()
      return 0
    case 2:
      throw CancellationError()
    case 3:
      await forcedWaitGate.waitIgnoringCancellation()
      if firstForcedWaitIsUnresolved {
        throw CancellationError()
      }
      return 0
    default:
      await duplicateWaitGate.waitIgnoringCancellation()
      return 0
    }
  }

  func releaseDuplicateWait() async {
    await duplicateWaitGate.release()
  }

  func resetDuplicateWait() {
    duplicateWaitGate = SequencedLaunchGate()
  }

  func waitUntilWaitCount(_ expected: Int) async throws {
    let deadline = ContinuousClock.now.advanced(by: .milliseconds(300))
    while waitCount < expected, ContinuousClock.now < deadline {
      try await Task.sleep(for: .milliseconds(5))
    }
    guard waitCount >= expected else {
      throw DaemonSupervisorError.shutdownTimeout
    }
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

private actor SequencedTransportFactory: RPCTransportBuilding {
  private var transports: [any RPCTransport]

  init(transports: [any RPCTransport]) {
    self.transports = transports
  }

  func makeTransport(
    for descriptor: ReadinessDescriptor
  ) throws -> any RPCTransport {
    guard !transports.isEmpty else {
      throw DaemonSupervisorError.snapshotUnavailable
    }
    return transports.removeFirst()
  }
}

private struct FailingTransport: RPCTransport {
  func getOrderBookSnapshot(
    using credentials: SessionCredentials,
    timeout: Duration
  ) throws -> MarketSnapshot {
    throw DaemonSupervisorError.snapshotUnavailable
  }
}

private struct StaticTransport: RPCTransport {
  let snapshot: MarketSnapshot

  func getOrderBookSnapshot(
    using credentials: SessionCredentials,
    timeout: Duration
  ) async throws -> MarketSnapshot {
    snapshot
  }
}

private struct DelayedTransportFactory: RPCTransportBuilding {
  let delay: Duration

  func makeTransport(
    for descriptor: ReadinessDescriptor
  ) -> any RPCTransport {
    DelayedTransport(delay: delay)
  }
}

private struct DelayedTransport: RPCTransport {
  let delay: Duration

  func getOrderBookSnapshot(
    using credentials: SessionCredentials,
    timeout: Duration
  ) async throws -> MarketSnapshot {
    try await Task.sleep(for: delay)
    return makeSnapshot()
  }
}

private struct BlockingTransportFactory: RPCTransportBuilding {
  let transport: BlockingTransport

  func makeTransport(
    for descriptor: ReadinessDescriptor
  ) -> any RPCTransport {
    transport
  }
}

private actor BlockingTransport: RPCTransport {
  private var started = false
  private var released = false

  func getOrderBookSnapshot(
    using credentials: SessionCredentials,
    timeout: Duration
  ) async throws -> MarketSnapshot {
    started = true
    while !released {
      try await Task.sleep(for: .milliseconds(5))
    }
    return makeSnapshot()
  }

  func waitUntilStarted() async throws {
    let deadline = ContinuousClock.now.advanced(by: .milliseconds(300))
    while !started, ContinuousClock.now < deadline {
      try await Task.sleep(for: .milliseconds(5))
    }
    guard started else {
      throw DaemonSupervisorError.snapshotUnavailable
    }
  }

  func release() {
    released = true
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

private struct ProcessFixture {
  let root: URL
  let configuration: DaemonConfiguration
  let runtime: ProcessDaemonRuntime
}

private func launchProcessFixture(
  named name: String,
  beforeReadiness: String = "",
  afterReadiness: String
) async throws -> ProcessFixture {
  let root = FileManager.default.temporaryDirectory.appending(
    path: UUID().uuidString,
    directoryHint: .isDirectory
  )
  try FileManager.default.createDirectory(
    at: root,
    withIntermediateDirectories: true
  )
  let executable = root.appending(path: name)
  let script = """
    #!/bin/sh
    dd bs=32 count=1 <&3 >/dev/null 2>&1
    \(beforeReadiness)
    now=$(date +%s)
    expiry=$((now + 60))
    printf '{"endpoint":"http://127.0.0.1:43127","protocol_major":1,"protocol_minor":0,"daemon_pid":%s,"process_nonce":"000102030405060708090a0b0c0d0e0f","server_nonce":"101112131415161718191a1b1c1d1e1f","issued_unix_seconds":%s,"expiry_unix_seconds":%s}\\n' "$$" "$now" "$expiry"
    \(afterReadiness)
    """
  try Data(script.utf8).write(to: executable)
  try FileManager.default.setAttributes(
    [.posixPermissions: 0o700],
    ofItemAtPath: executable.path
  )
  let configuration = DaemonConfiguration(
    executable: executable,
    approvedRoot: root,
    configFile: root.appending(path: "unused.toml")
  )
  let launched = try await ProcessDaemonLauncher().launch(
    configuration: configuration,
    bootstrap: try SessionBootstrap(
      randomBytes: { Array(repeating: 0xa5, count: 32) }
    )
  )
  guard let runtime = launched as? ProcessDaemonRuntime else {
    throw DaemonSupervisorError.launchFailed
  }
  return ProcessFixture(
    root: root,
    configuration: configuration,
    runtime: runtime
  )
}

extension Array {
  fileprivate func asyncMap<T: Sendable>(
    _ transform: (Element) async -> T
  ) async -> [T] {
    var values: [T] = []
    values.reserveCapacity(count)
    for element in self {
      values.append(await transform(element))
    }
    return values
  }
}
