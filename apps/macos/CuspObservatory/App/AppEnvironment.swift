import Foundation
import OSLog
import Observation

enum AppEnvironmentError: Error, Equatable {
  case invalidTestRunIdentifier
  case missingApplicationSupport
  case missingDaemon
  case missingFixture
  case runtimePreparationFailed
}

struct AppEnvironment {
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
    let dataRoot = runtimeRoot.appending(
      path: "data",
      directoryHint: .isDirectory
    )
    let logRoot = runtimeRoot.appending(
      path: "logs",
      directoryHint: .isDirectory
    )
    do {
      for directory in [runtimeRoot, dataRoot, logRoot] {
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
    let runtimeFixture = runtimeRoot.appending(path: "fixture.jsonl")
    let configuration = runtimeRoot.appending(path: "config.toml")
    do {
      let fixtureData = try Data(contentsOf: fixture)
      try fixtureData.write(to: runtimeFixture, options: .atomic)
      try Data(Self.runtimeConfiguration.utf8).write(
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
        configFile: configuration
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
      daemonLog: logRoot.appending(path: "cmti.jsonl"),
      supervisor: supervisor
    )
  }

  private static func runtimeRoot(
    processInfo: ProcessInfo,
    fileManager: FileManager
  ) throws -> URL {
    if let identifier = processInfo.environment["CMTI_TEST_RUN_ID"] {
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
    if let identifier = environment["CMTI_TEST_RUN_ID"] {
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

  static let runtimeConfiguration = """
    schema_version = 1
    data_root = "data"
    log_root = "logs"
    fixture_input = "fixture.jsonl"
    bind_address = "127.0.0.1:0"
    session_secret_fd = 3
    ingestion_queue_capacity = 1024
    maximum_request_bytes = 8388608
    maximum_concurrent_requests = 128
    request_timeout_seconds = 30
    shutdown_grace_seconds = 5
    remote_export = false
    remote_telemetry = false

    """
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
    guard let supervisor else {
      model.reflect(lifecycle: .stopped)
      startupTask = nil
      return
    }
    logger.info("daemon_stop_requested")
    try? await supervisor.stop()
    _ = await starting?.result
    startupTask = nil
    model.reflect(lifecycle: .stopped)
    logger.info("daemon_stopped")
  }
}
