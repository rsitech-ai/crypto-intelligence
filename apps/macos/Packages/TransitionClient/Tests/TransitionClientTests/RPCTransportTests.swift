import GRPCCore
import Testing

@testable import TransitionClient

@Suite("Authenticated snapshot client")
struct RPCTransportTests {
  @Test("client returns the immutable authoritative server snapshot")
  func exactSnapshotPassesThrough() async throws {
    let expected = MarketSnapshot(
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
    let transport = SnapshotTransport(result: .success(expected))
    let client = TransitionClient(transport: transport)

    let actual = try await client.snapshot(
      using: credentials(),
      timeout: .seconds(1)
    )

    #expect(actual == expected)
    #expect(actual.bestBid == "60000.1")
    #expect(actual.bestAsk == "60000.2")
    #expect(await transport.callCount() == 1)
  }

  @Test("transport failure remains a recoverable client error")
  func transportFailureIsPreserved() async {
    let transport = SnapshotTransport(
      result: .failure(TestTransportError.disconnected)
    )
    let client = TransitionClient(transport: transport)

    await #expect(throws: TestTransportError.disconnected) {
      try await client.snapshot(
        using: credentials(),
        timeout: .seconds(1)
      )
    }
  }

  @Test("client forwards the caller's explicit RPC timeout")
  func explicitTimeoutIsForwarded() async throws {
    let transport = SnapshotTransport(
      result: .success(snapshot())
    )
    let client = TransitionClient(transport: transport)

    _ = try await client.snapshot(
      using: credentials(),
      timeout: .milliseconds(75)
    )

    #expect(await transport.lastTimeout() == .milliseconds(75))
  }

  @Test("generated snapshot maps exact RPC strings and display metadata")
  func generatedSnapshotMapsWithoutNumericRecalculation() throws {
    var response = Cmti_Market_V1_GetOrderBookSnapshotResponse()
    response.source = "binance-fixture"
    response.symbol = "BTCUSDT"
    response.generation = 1
    response.sequence = 102
    response.bestBid = "60000.1"
    response.bestAsk = "60000.2"
    response.health = .healthy
    response.eventUnixNanos = 1_700_000_000_200_000_000
    response.receiveUnixNanos = 1_700_000_000_205_000_000
    response.freshnessMillis = 5

    let snapshot = try GRPCTransport.map(
      response,
      priceDisplayScale: 2
    )

    #expect(snapshot.bestBid == "60000.1")
    #expect(snapshot.bestAsk == "60000.2")
    #expect(snapshot.priceDisplayScale == 2)
    #expect(snapshot.health == .healthy)
  }

  @Test("generated snapshot rejects unknown health and noncanonical prices")
  func generatedSnapshotFailsClosed() {
    for (health, bid) in [
      (Cmti_Market_V1_SnapshotHealth.unspecified, "60000.1"),
      (.UNRECOGNIZED(99), "60000.1"),
      (.healthy, "060000.1"),
      (.healthy, "60000.100"),
      (.healthy, "NaN"),
    ] {
      var response = Cmti_Market_V1_GetOrderBookSnapshotResponse()
      response.source = "binance-fixture"
      response.symbol = "BTCUSDT"
      response.generation = 1
      response.sequence = 102
      response.bestBid = bid
      response.bestAsk = "60000.2"
      response.health = health

      #expect(throws: GRPCTransportError.self) {
        try GRPCTransport.map(response, priceDisplayScale: 2)
      }
    }
  }

  @Test("gRPC metadata carries only the two binary authentication fields")
  func exactAuthenticationMetadata() {
    let metadata = GRPCTransport.authenticationMetadata(
      using: credentials()
    )

    #expect(
      Array(metadata[binaryValues: "cmti-session-bin"]) == [
        Array(repeating: 0x11, count: 76)
      ])
    #expect(
      Array(metadata[binaryValues: "cmti-token-bin"]) == [
        Array(repeating: 0x22, count: 32)
      ])
    #expect(metadata.count == 2)
  }
}

private enum TestTransportError: Error, Equatable {
  case disconnected
}

private actor SnapshotTransport: RPCTransport {
  private let result: Result<MarketSnapshot, TestTransportError>
  private var calls = 0
  private var timeout: Duration?

  init(result: Result<MarketSnapshot, TestTransportError>) {
    self.result = result
  }

  func getOrderBookSnapshot(
    using credentials: SessionCredentials,
    timeout: Duration
  ) async throws -> MarketSnapshot {
    calls += 1
    self.timeout = timeout
    return try result.get()
  }

  func callCount() -> Int {
    calls
  }

  func lastTimeout() -> Duration? {
    timeout
  }
}

private func credentials() -> SessionCredentials {
  SessionCredentials(
    descriptorBytes: Array(repeating: 0x11, count: 76),
    tokenBytes: Array(repeating: 0x22, count: 32)
  )
}

private func snapshot() -> MarketSnapshot {
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
