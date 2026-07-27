import Darwin
import Foundation
import Security
import TransitionClient

enum SessionBootstrapError: Error, Equatable {
  case randomGenerationFailed
  case invalidLength
  case consumed
  case writeFailed
}

final class SessionBootstrap: @unchecked Sendable {
  typealias RandomBytes = @Sendable () throws -> [UInt8]
  typealias ZeroizationProbe = @Sendable (Bool) -> Void

  private let lock = NSLock()
  private let zeroizationProbe: ZeroizationProbe?
  private var secret: [UInt8]?
  private var wasWritten = false

  init(
    randomBytes: RandomBytes = SessionBootstrap.secureRandomBytes,
    zeroizationProbe: ZeroizationProbe? = nil
  ) throws {
    let bytes = try randomBytes()
    guard bytes.count == 32 else {
      throw SessionBootstrapError.invalidLength
    }
    self.zeroizationProbe = zeroizationProbe
    secret = bytes
  }

  deinit {
    lock.lock()
    zeroizeSecret()
    lock.unlock()
  }

  func writeSecret(to handle: FileHandle) throws {
    var bytes = try lockedSecret()
    defer {
      Self.zeroize(&bytes)
      zeroizationProbe?(bytes.allSatisfy { $0 == 0 })
    }
    do {
      try bytes.withUnsafeBytes { storage in
        guard let baseAddress = storage.baseAddress else {
          throw SessionBootstrapError.writeFailed
        }
        var written = 0
        while written < storage.count {
          let count = Darwin.write(
            handle.fileDescriptor,
            baseAddress.advanced(by: written),
            storage.count - written
          )
          if count > 0 {
            written += count
          } else if count == -1, errno == EINTR {
            continue
          } else {
            throw SessionBootstrapError.writeFailed
          }
        }
      }
      try handle.close()
    } catch {
      try? handle.close()
      throw SessionBootstrapError.writeFailed
    }
  }

  func credentials(
    for descriptor: ReadinessDescriptor
  ) throws -> SessionCredentials {
    let bytes: [UInt8]
    lock.lock()
    guard let current = secret else {
      lock.unlock()
      throw SessionBootstrapError.consumed
    }
    bytes = current
    zeroizeSecret()
    lock.unlock()

    return try SessionAuthentication.credentials(
      secret: bytes,
      descriptor: descriptor
    )
  }

  private func lockedSecret() throws -> [UInt8] {
    lock.lock()
    defer { lock.unlock() }
    guard let secret, !wasWritten else {
      throw SessionBootstrapError.consumed
    }
    wasWritten = true
    return secret
  }

  private func zeroizeSecret() {
    guard var bytes = secret else {
      return
    }
    secret = nil
    Self.zeroize(&bytes)
  }

  private static func secureRandomBytes() throws -> [UInt8] {
    var bytes = [UInt8](repeating: 0, count: 32)
    let status = SecRandomCopyBytes(
      kSecRandomDefault,
      bytes.count,
      &bytes
    )
    guard status == errSecSuccess else {
      _ = bytes.withUnsafeMutableBytes { storage in
        storage.initializeMemory(as: UInt8.self, repeating: 0)
      }
      throw SessionBootstrapError.randomGenerationFailed
    }
    return bytes
  }

  private static func zeroize(_ bytes: inout [UInt8]) {
    _ = bytes.withUnsafeMutableBytes { storage in
      storage.initializeMemory(as: UInt8.self, repeating: 0)
    }
  }
}
