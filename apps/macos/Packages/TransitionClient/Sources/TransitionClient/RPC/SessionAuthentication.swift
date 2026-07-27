import CryptoKit
import Foundation

public enum ClientContractError: Error, Sendable, Equatable {
  case invalidEndpoint
  case invalidReadiness
  case incompatibleProtocol
  case daemonIdentityMismatch
  case invalidSessionLifetime
  case sessionNotYetValid
  case sessionExpired
  case invalidSecretLength
}

extension ClientContractError: CustomStringConvertible {
  public var description: String {
    switch self {
    case .invalidEndpoint:
      "daemon endpoint is not the required IPv4 loopback address"
    case .invalidReadiness:
      "daemon readiness descriptor is invalid"
    case .incompatibleProtocol:
      "daemon protocol is incompatible"
    case .daemonIdentityMismatch:
      "daemon process identity does not match"
    case .invalidSessionLifetime:
      "daemon session lifetime is invalid"
    case .sessionNotYetValid:
      "daemon session is not yet valid"
    case .sessionExpired:
      "daemon session has expired"
    case .invalidSecretLength:
      "inherited session material has an invalid length"
    }
  }
}

public struct LocalEndpoint: Sendable, Equatable {
  public let host: String
  public let port: Int

  public init(validating value: String) throws {
    guard let components = URLComponents(string: value),
      components.scheme == "http",
      components.host == "127.0.0.1",
      let port = components.port,
      (1...65_535).contains(port),
      components.user == nil,
      components.password == nil,
      components.path.isEmpty,
      components.query == nil,
      components.fragment == nil
    else {
      throw ClientContractError.invalidEndpoint
    }

    self.host = "127.0.0.1"
    self.port = port
  }
}

public struct ReadinessDescriptor: Sendable, Equatable {
  public let endpoint: LocalEndpoint
  public let protocolMajor: UInt32
  public let protocolMinor: UInt32
  public let daemonPID: UInt32
  public let processNonce: [UInt8]
  public let serverNonce: [UInt8]
  public let issuedUnixSeconds: Int64
  public let expiryUnixSeconds: Int64

  public var canonicalBytes: [UInt8] {
    var bytes = Array("cmti:session:v1\0".utf8)
    bytes.append(contentsOf: bigEndianBytes(protocolMajor))
    bytes.append(contentsOf: bigEndianBytes(protocolMinor))
    bytes.append(contentsOf: bigEndianBytes(daemonPID))
    bytes.append(contentsOf: processNonce)
    bytes.append(contentsOf: serverNonce)
    bytes.append(contentsOf: bigEndianBytes(issuedUnixSeconds))
    bytes.append(contentsOf: bigEndianBytes(expiryUnixSeconds))
    return bytes
  }

  public init(
    jsonData: Data,
    expectedDaemonPID: UInt32,
    nowUnixSeconds: Int64
  ) throws {
    let wire: ReadinessWire
    do {
      wire = try JSONDecoder().decode(ReadinessWire.self, from: jsonData)
    } catch {
      throw ClientContractError.invalidReadiness
    }

    try self.init(
      endpoint: wire.endpoint,
      protocolMajor: wire.protocolMajor,
      protocolMinor: wire.protocolMinor,
      daemonPID: wire.daemonPID,
      processNonceHex: wire.processNonce,
      serverNonceHex: wire.serverNonce,
      issuedUnixSeconds: wire.issuedUnixSeconds,
      expiryUnixSeconds: wire.expiryUnixSeconds,
      expectedDaemonPID: expectedDaemonPID,
      nowUnixSeconds: nowUnixSeconds
    )
  }

  public init(
    endpoint: String,
    protocolMajor: UInt32,
    protocolMinor: UInt32,
    daemonPID: UInt32,
    processNonceHex: String,
    serverNonceHex: String,
    issuedUnixSeconds: Int64,
    expiryUnixSeconds: Int64,
    expectedDaemonPID: UInt32,
    nowUnixSeconds: Int64
  ) throws {
    guard protocolMajor == ProtocolCompatibility.supportedMajor else {
      throw ClientContractError.incompatibleProtocol
    }
    guard daemonPID != 0, daemonPID == expectedDaemonPID else {
      throw ClientContractError.daemonIdentityMismatch
    }
    guard expiryUnixSeconds.checkedSubtracting(issuedUnixSeconds) == 60 else {
      throw ClientContractError.invalidSessionLifetime
    }
    guard issuedUnixSeconds <= nowUnixSeconds else {
      throw ClientContractError.sessionNotYetValid
    }
    guard nowUnixSeconds < expiryUnixSeconds else {
      throw ClientContractError.sessionExpired
    }
    guard let processNonce = Self.decodeNonce(processNonceHex),
      let serverNonce = Self.decodeNonce(serverNonceHex)
    else {
      throw ClientContractError.invalidReadiness
    }

    self.endpoint = try LocalEndpoint(validating: endpoint)
    self.protocolMajor = protocolMajor
    self.protocolMinor = protocolMinor
    self.daemonPID = daemonPID
    self.processNonce = processNonce
    self.serverNonce = serverNonce
    self.issuedUnixSeconds = issuedUnixSeconds
    self.expiryUnixSeconds = expiryUnixSeconds
  }

  private static func decodeNonce(_ hex: String) -> [UInt8]? {
    guard hex.utf8.count == 32,
      hex.utf8.allSatisfy({
        (UInt8(ascii: "0")...UInt8(ascii: "9")).contains($0)
          || (UInt8(ascii: "a")...UInt8(ascii: "f")).contains($0)
      })
    else {
      return nil
    }

    var decoded: [UInt8] = []
    decoded.reserveCapacity(16)
    var index = hex.startIndex
    for _ in 0..<16 {
      let next = hex.index(index, offsetBy: 2)
      guard let byte = UInt8(hex[index..<next], radix: 16) else {
        return nil
      }
      decoded.append(byte)
      index = next
    }
    return decoded
  }
}

public final class SessionCredentials: @unchecked Sendable {
  public let descriptorBytes: [UInt8]
  private var token: [UInt8]

  var tokenBytes: [UInt8] {
    token
  }

  init(descriptorBytes: [UInt8], tokenBytes: [UInt8]) {
    self.descriptorBytes = descriptorBytes
    token = tokenBytes
  }

  deinit {
    _ = token.withUnsafeMutableBytes { bytes in
      bytes.initializeMemory(as: UInt8.self, repeating: 0)
    }
  }
}

public enum SessionAuthentication {
  public static func credentials(
    secret: consuming [UInt8],
    descriptor: ReadinessDescriptor
  ) throws -> SessionCredentials {
    var secret = secret
    defer {
      _ = secret.withUnsafeMutableBytes { bytes in
        bytes.initializeMemory(as: UInt8.self, repeating: 0)
      }
    }
    guard secret.count == 32 else {
      throw ClientContractError.invalidSecretLength
    }

    let key = SymmetricKey(data: secret)
    let authentication = HMAC<SHA256>.authenticationCode(
      for: descriptor.canonicalBytes,
      using: key
    )
    return SessionCredentials(
      descriptorBytes: descriptor.canonicalBytes,
      tokenBytes: Array(authentication)
    )
  }
}

private struct ReadinessWire: Decodable {
  let endpoint: String
  let protocolMajor: UInt32
  let protocolMinor: UInt32
  let daemonPID: UInt32
  let processNonce: String
  let serverNonce: String
  let issuedUnixSeconds: Int64
  let expiryUnixSeconds: Int64

  enum CodingKeys: String, CodingKey {
    case endpoint
    case protocolMajor = "protocol_major"
    case protocolMinor = "protocol_minor"
    case daemonPID = "daemon_pid"
    case processNonce = "process_nonce"
    case serverNonce = "server_nonce"
    case issuedUnixSeconds = "issued_unix_seconds"
    case expiryUnixSeconds = "expiry_unix_seconds"
  }
}

private func bigEndianBytes<T: FixedWidthInteger>(_ value: T) -> [UInt8] {
  withUnsafeBytes(of: value.bigEndian) { Array($0) }
}

extension Int64 {
  fileprivate func checkedSubtracting(_ other: Int64) -> Int64? {
    let (value, overflow) = subtractingReportingOverflow(other)
    return overflow ? nil : value
  }
}
