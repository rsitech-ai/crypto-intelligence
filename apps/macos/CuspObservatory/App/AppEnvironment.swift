import Foundation
import OSLog
import Observation

enum AppEnvironmentError: Error, Equatable {
  case invalidTestRunIdentifier
  case missingApplicationSupport
  case missingConfiguration
  case missingDaemon
  case missingFixture
  case runtimePreparationFailed
}

struct AppEnvironment {
  struct PreparedRuntime {
    let configuration: URL
    let daemonLog: URL
  }

  let runtimeRoot: URL
  let daemonLog: URL
  let supervisor: DaemonSupervisor

  static func live(
    bundle: Bundle = .main,
    processInfo: ProcessInfo = .processInfo,
    fileManager: FileManager = .default
  ) throws -> AppEnvironment {
    let runtimeRoot = try runtimeRoot(
      processInfo: processInfo,
      fileManager: fileManager
    )
    let prepared = try prepareRuntimeRoot(
      runtimeRoot,
      bundle: bundle,
      fileManager: fileManager
    )

    guard let bundleExecutable = bundle.executableURL else {
      throw AppEnvironmentError.missingDaemon
    }
    let daemon = try daemonURL(
      bundleExecutableURL: bundleExecutable,
      environment: processInfo.environment
    )

    let supervisor = DaemonSupervisor(
      configuration: DaemonConfiguration(
        executable: daemon,
        approvedRoot: runtimeRoot,
        configFile: prepared.configuration
      ),
      launcher: ProcessDaemonLauncher(),
      transportFactory: GRPCTransportFactory(priceDisplayScale: 2),
      startupTimeout: .seconds(5),
      shutdownTimeout: .seconds(6),
      nowUnixSeconds: {
        Int64(Date().timeIntervalSince1970)
      }
    )
    return AppEnvironment(
      runtimeRoot: runtimeRoot,
      daemonLog: prepared.daemonLog,
      supervisor: supervisor
    )
  }

  static func prepareRuntimeRoot(
    _ runtimeRoot: URL,
    bundle: Bundle,
    fileManager: FileManager
  ) throws -> PreparedRuntime {
    let dataRoot = runtimeRoot.appending(
      path: "data",
      directoryHint: .isDirectory
    )
    let logRoot = runtimeRoot.appending(
      path: "logs",
      directoryHint: .isDirectory
    )
    let fixtureRoot = runtimeRoot.appending(
      path: "fixtures/binance",
      directoryHint: .isDirectory
    )
    let modelRegistry = runtimeRoot.appending(
      path: "models/public-test-artifacts",
      directoryHint: .isDirectory
    )
    do {
      for directory in [
        runtimeRoot,
        dataRoot,
        logRoot,
        fixtureRoot,
        modelRegistry,
      ] {
        try ensureSecureDirectory(
          directory,
          fileManager: fileManager
        )
        try fileManager.setAttributes(
          [.posixPermissions: 0o700],
          ofItemAtPath: directory.path
        )
      }
    } catch {
      throw AppEnvironmentError.runtimePreparationFailed
    }

    guard
      let fixture = bundle.url(
        forResource: "btcusdt-book-v1",
        withExtension: "jsonl"
      )
    else {
      throw AppEnvironmentError.missingFixture
    }
    guard
      let bundledConfiguration = bundle.url(
        forResource: "default",
        withExtension: "toml"
      )
    else {
      throw AppEnvironmentError.missingConfiguration
    }
    let runtimeFixture = fixtureRoot.appending(
      path: "btcusdt-book-v1.jsonl"
    )
    let configuration = runtimeRoot.appending(path: "config.toml")
    do {
      let fixtureData = try Data(contentsOf: fixture)
      let configurationData = try Data(contentsOf: bundledConfiguration)
      try fixtureData.write(to: runtimeFixture, options: .atomic)
      try configurationData.write(
        to: configuration,
        options: .atomic
      )
      for file in [runtimeFixture, configuration] {
        try fileManager.setAttributes(
          [.posixPermissions: 0o600],
          ofItemAtPath: file.path
        )
      }
    } catch {
      throw AppEnvironmentError.runtimePreparationFailed
    }

    return PreparedRuntime(
      configuration: configuration,
      daemonLog: logRoot.appending(path: "cmti.jsonl")
    )
  }

  private static func runtimeRoot(
    processInfo: ProcessInfo,
    fileManager: FileManager
  ) throws -> URL {
    if isUITestEnvironment(processInfo.environment),
      let identifier = processInfo.environment["CMTI_TEST_RUN_ID"]
    {
      try validateTestRunIdentifier(identifier)
      return fileManager.temporaryDirectory
        .appending(path: "CuspObservatoryTests")
        .appending(path: identifier, directoryHint: .isDirectory)
    }

    guard
      let applicationSupport = fileManager.urls(
        for: .applicationSupportDirectory,
        in: .userDomainMask
      ).first
    else {
      throw AppEnvironmentError.missingApplicationSupport
    }
    return applicationSupport.appending(
      path: "CuspObservatory",
      directoryHint: .isDirectory
    )
  }

  static func daemonURL(
    bundleExecutableURL: URL,
    environment: [String: String]
  ) throws -> URL {
    if isUITestEnvironment(environment),
      let identifier = environment["CMTI_TEST_RUN_ID"]
    {
      try validateTestRunIdentifier(identifier)
      if let override = environment["CMTI_DAEMON_PATH"] {
        return URL(fileURLWithPath: override)
      }
    }
    return
      bundleExecutableURL
      .deletingLastPathComponent()
      .appending(path: "cryptoriskd")
  }

  static func shouldStartSupervisor(
    environment: [String: String]
  ) -> Bool {
    environment["XCTestConfigurationFilePath"] == nil
      && environment["XCInjectBundleInto"] == nil
  }

  private static func isUITestEnvironment(
    _ environment: [String: String]
  ) -> Bool {
    #if CMTI_UI_TEST_HARNESS
      environment["CMTI_UI_TEST_MODE"] == "1"
    #else
      false
    #endif
  }

  private static func validateTestRunIdentifier(
    _ identifier: String
  ) throws {
    guard !identifier.isEmpty,
      identifier.count <= 80,
      identifier.utf8.allSatisfy({
        $0 == UInt8(ascii: "-")
          || (UInt8(ascii: "0")...UInt8(ascii: "9"))
            .contains($0)
          || (UInt8(ascii: "A")...UInt8(ascii: "Z"))
            .contains($0)
          || (UInt8(ascii: "a")...UInt8(ascii: "z"))
            .contains($0)
      })
    else {
      throw AppEnvironmentError.invalidTestRunIdentifier
    }
  }

  static func ensureSecureDirectory(
    _ directory: URL,
    fileManager: FileManager
  ) throws {
    var isDirectory: ObjCBool = false
    if fileManager.fileExists(
      atPath: directory.path,
      isDirectory: &isDirectory
    ) {
      let attributes = try fileManager.attributesOfItem(
        atPath: directory.path
      )
      guard isDirectory.boolValue,
        attributes[.type] as? FileAttributeType
          != .typeSymbolicLink
      else {
        throw AppEnvironmentError.runtimePreparationFailed
      }
      return
    }
    try fileManager.createDirectory(
      at: directory,
      withIntermediateDirectories: true
    )
    let attributes = try fileManager.attributesOfItem(
      atPath: directory.path
    )
    guard attributes[.type] as? FileAttributeType == .typeDirectory else {
      throw AppEnvironmentError.runtimePreparationFailed
    }
  }

}

@MainActor
@Observable
final class AppCoordinator {
  typealias EnvironmentLoader = () throws -> AppEnvironment

  let model = AppModel()
  private(set) var runtimeRoot: URL?
  private(set) var daemonLog: URL?

  private let logger = Logger(
    subsystem: "ai.rsitech.CuspObservatory",
    category: "supervisor"
  )
  private let environmentLoader: EnvironmentLoader
  private var supervisor: DaemonSupervisor?
  private var startupTask: Task<Void, Never>?
  private var eventTask: Task<Void, Never>?

  init(
    environment: AppEnvironment? = nil,
    environmentLoader: @escaping EnvironmentLoader = {
      try AppEnvironment.live()
    }
  ) {
    self.environmentLoader = environmentLoader
    if let environment {
      install(environment)
    }
  }

  private func install(_ environment: AppEnvironment) {
    eventTask?.cancel()
    eventTask = nil
    runtimeRoot = environment.runtimeRoot
    daemonLog = environment.daemonLog
    supervisor = environment.supervisor
  }

  private func prepareEnvironmentIfNeeded() {
    guard supervisor == nil else {
      return
    }
    do {
      install(try environmentLoader())
    } catch {
      model.reflect(lifecycle: .failed)
      logger.error("runtime_environment_preparation_failed")
    }
  }

  var isActive: Bool {
    supervisor != nil && model.phase != .disconnected
  }

  func start() {
    prepareEnvironmentIfNeeded()
    guard startupTask == nil,
      model.phase == .disconnected || model.phase == .recovery,
      let supervisor
    else {
      return
    }
    model.reflect(lifecycle: .starting)
    logger.info("daemon_start_requested")
    startupTask = Task { [weak self] in
      guard let self else {
        return
      }
      do {
        try await supervisor.start()
        let lifecycle = await supervisor.currentState
        guard let snapshot = await supervisor.latestSnapshot else {
          throw DaemonSupervisorError.snapshotUnavailable
        }
        model.reflect(lifecycle: lifecycle)
        try model.apply(snapshot: snapshot)
        observe(supervisor)
        logger.info("daemon_snapshot_ready")
      } catch is CancellationError {
        logger.info("daemon_start_cancelled")
      } catch {
        model.reflect(lifecycle: .failed)
        logger.error("daemon_start_failed")
      }
      startupTask = nil
    }
  }

  func retry() {
    let previous = startupTask
    startupTask?.cancel()
    eventTask?.cancel()
    eventTask = nil
    startupTask = Task { [weak self] in
      guard let self else {
        return
      }
      _ = await previous?.result
      guard !Task.isCancelled else {
        startupTask = nil
        return
      }
      if let supervisor {
        try? await supervisor.stop()
      }
      guard !Task.isCancelled else {
        startupTask = nil
        return
      }
      startupTask = nil
      start()
    }
  }

  func stop() async {
    let starting = startupTask
    startupTask?.cancel()
    let observing = eventTask
    eventTask?.cancel()
    eventTask = nil
    guard let supervisor else {
      model.reflect(lifecycle: .stopped)
      startupTask = nil
      _ = await observing?.result
      return
    }
    logger.info("daemon_stop_requested")
    do {
      try await supervisor.stop()
    } catch {
      logger.error("daemon_stop_escalated")
    }
    _ = await starting?.result
    _ = await observing?.result
    startupTask = nil
    let lifecycle = await supervisor.currentState
    model.reflect(lifecycle: lifecycle)
    if lifecycle == .stopped {
      logger.info("daemon_stopped")
    } else {
      logger.error("daemon_stop_unconfirmed")
    }
  }

  private func observe(_ observed: DaemonSupervisor) {
    eventTask?.cancel()
    eventTask = Task { [weak self] in
      let events = await observed.events()
      for await event in events {
        guard !Task.isCancelled, let self else {
          return
        }
        model.reflect(lifecycle: event.state)
        if let snapshot = event.snapshot {
          do {
            try model.apply(snapshot: snapshot)
          } catch {
            model.reflect(lifecycle: .failed)
            logger.error("daemon_snapshot_invalid")
          }
        }
      }
    }
  }
}
