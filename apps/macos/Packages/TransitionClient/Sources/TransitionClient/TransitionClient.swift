import Foundation

public enum MarketHealth: String, Sendable, Equatable {
  case healthy
  case degraded
  case stale
  case unavailable
}

public struct MarketSnapshot: Sendable, Equatable {
  public let source: String
  public let symbol: String
  public let generation: UInt32
  public let sequence: UInt64
  public let bestBid: String
  public let bestAsk: String
  public let health: MarketHealth
  public let eventUnixNanos: Int64
  public let receiveUnixNanos: Int64
  public let freshnessMillis: UInt64
  public let priceDisplayScale: Int

  public init(
    source: String,
    symbol: String,
    generation: UInt32,
    sequence: UInt64,
    bestBid: String,
    bestAsk: String,
    health: MarketHealth,
    eventUnixNanos: Int64,
    receiveUnixNanos: Int64,
    freshnessMillis: UInt64,
    priceDisplayScale: Int
  ) {
    self.source = source
    self.symbol = symbol
    self.generation = generation
    self.sequence = sequence
    self.bestBid = bestBid
    self.bestAsk = bestAsk
    self.health = health
    self.eventUnixNanos = eventUnixNanos
    self.receiveUnixNanos = receiveUnixNanos
    self.freshnessMillis = freshnessMillis
    self.priceDisplayScale = priceDisplayScale
  }
}

public actor TransitionClient {
  private let transport: any RPCTransport

  public init(transport: any RPCTransport) {
    self.transport = transport
  }

  public func snapshot(
    using credentials: SessionCredentials,
    timeout: Duration
  ) async throws -> MarketSnapshot {
    try await transport.getSnapshot(
      using: credentials,
      timeout: timeout
    )
  }
}
