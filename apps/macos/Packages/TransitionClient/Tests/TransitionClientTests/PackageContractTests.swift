import Foundation
import Testing

@testable import TransitionClient

@Suite("Local package and session contract")
struct PackageContractTests {
  @Test("local endpoint accepts only the daemon IPv4 loopback shape")
  func localEndpointFailsClosed() throws {
    let endpoint = try LocalEndpoint(validating: "http://127.0.0.1:43127")

    #expect(endpoint.host == "127.0.0.1")
    #expect(endpoint.port == 43_127)

    for rejected in [
      "http://localhost:43127",
      "http://0.0.0.0:43127",
      "http://127.0.0.2:43127",
      "https://127.0.0.1:43127",
      "http://127.0.0.1:0",
      "http://127.0.0.1:43127/path",
      "http://user@127.0.0.1:43127",
    ] {
      #expect(throws: ClientContractError.self) {
        try LocalEndpoint(validating: rejected)
      }
    }
  }

  @Test("readiness descriptor matches the frozen Rust canonical encoding")
  func canonicalDescriptorParity() throws {
    let descriptor = try ReadinessDescriptor(
      endpoint: "http://127.0.0.1:43127",
      protocolMajor: 1,
      protocolMinor: 2,
      daemonPID: 0x0102_0304,
      processNonceHex: "000102030405060708090a0b0c0d0e0f",
      serverNonceHex: "101112131415161718191a1b1c1d1e1f",
      issuedUnixSeconds: 1_700_000_000,
      expiryUnixSeconds: 1_700_000_060,
      expectedDaemonPID: 0x0102_0304,
      nowUnixSeconds: 1_700_000_000
    )

    let expected =
      Array("cmti:session:v1\0".utf8)
      + bigEndianBytes(UInt32(1))
      + bigEndianBytes(UInt32(2))
      + bigEndianBytes(UInt32(0x0102_0304))
      + Array(0x00...0x0f)
      + Array(0x10...0x1f)
      + bigEndianBytes(Int64(1_700_000_000))
      + bigEndianBytes(Int64(1_700_000_060))

    #expect(descriptor.canonicalBytes == expected)
    #expect(descriptor.canonicalBytes.count == 76)
  }

  @Test("Swift HMAC-SHA256 matches the independent Rust vector")
  func authenticationVectorParity() throws {
    let descriptor = try ReadinessDescriptor(
      endpoint: "http://127.0.0.1:43127",
      protocolMajor: 1,
      protocolMinor: 2,
      daemonPID: 0x0102_0304,
      processNonceHex: "000102030405060708090a0b0c0d0e0f",
      serverNonceHex: "101112131415161718191a1b1c1d1e1f",
      issuedUnixSeconds: 1_700_000_000,
      expiryUnixSeconds: 1_700_000_060,
      expectedDaemonPID: 0x0102_0304,
      nowUnixSeconds: 1_700_000_000
    )
    let credentials = try SessionAuthentication.credentials(
      secret: Array(repeating: 0xa5, count: 32),
      descriptor: descriptor
    )

    #expect(
      credentials.tokenBytes == [
        0xad, 0xf6, 0x79, 0xf3, 0xa0, 0x68, 0x58, 0x98,
        0x7c, 0xb0, 0x65, 0x60, 0x21, 0x6b, 0x03, 0xc7,
        0xe2, 0x54, 0xf6, 0x42, 0xb4, 0x2b, 0xe8, 0xb6,
        0xb1, 0x5b, 0x8e, 0xde, 0x7d, 0xe8, 0xc4, 0xa8,
      ]
    )
    #expect(credentials.descriptorBytes == descriptor.canonicalBytes)
  }

  @Test("readiness rejects malformed, mismatched, future, and expired sessions")
  func readinessValidation() {
    let base = readinessJSON()
    let invalidDocuments: [[String: Any]] = [
      replacing(base, "protocol_major", with: 0),
      replacing(base, "protocol_major", with: 2),
      replacing(base, "daemon_pid", with: 8),
      replacing(base, "process_nonce", with: "00"),
      replacing(base, "server_nonce", with: String(repeating: "g", count: 32)),
      replacing(base, "expiry_unix_seconds", with: 1_700_000_061),
      replacing(base, "issued_unix_seconds", with: 1_700_000_001),
      replacing(base, "endpoint", with: "http://0.0.0.0:43127"),
    ]

    for document in invalidDocuments {
      #expect(throws: ClientContractError.self) {
        try ReadinessDescriptor(
          jsonData: try JSONSerialization.data(withJSONObject: document),
          expectedDaemonPID: 7,
          nowUnixSeconds: 1_700_000_000
        )
      }
    }

    #expect(throws: ClientContractError.self) {
      try ReadinessDescriptor(
        jsonData: try JSONSerialization.data(
          withJSONObject: readinessJSON()
        ),
        expectedDaemonPID: 7,
        nowUnixSeconds: 1_700_000_060
      )
    }
  }

  @Test("public errors never include secret, token, or readiness material")
  func errorsAreRedacted() {
    let sentinel = String(repeating: "a5", count: 32)
    let malformed = Data(
      #"{"endpoint":"\#(sentinel)","protocol_major":1}"#.utf8
    )

    do {
      _ = try ReadinessDescriptor(
        jsonData: malformed,
        expectedDaemonPID: 7,
        nowUnixSeconds: 1_700_000_000
      )
      Issue.record("malformed readiness must fail")
    } catch {
      #expect(!String(describing: error).contains(sentinel))
      #expect(!String(reflecting: error).contains(sentinel))
      #expect(!String(describing: error).contains("token"))
      #expect(!String(describing: error).contains("secret"))
    }
  }
}

private func readinessJSON() -> [String: Any] {
  [
    "endpoint": "http://127.0.0.1:43127",
    "protocol_major": 1,
    "protocol_minor": 0,
    "daemon_pid": 7,
    "process_nonce": "000102030405060708090a0b0c0d0e0f",
    "server_nonce": "101112131415161718191a1b1c1d1e1f",
    "issued_unix_seconds": 1_700_000_000,
    "expiry_unix_seconds": 1_700_000_060,
  ]
}

private func replacing(
  _ source: [String: Any],
  _ key: String,
  with value: Any
) -> [String: Any] {
  var copy = source
  copy[key] = value
  return copy
}

private func bigEndianBytes<T: FixedWidthInteger>(_ value: T) -> [UInt8] {
  withUnsafeBytes(of: value.bigEndian) { Array($0) }
}
