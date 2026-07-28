public protocol RPCTransport: Sendable {
  func getOrderBookSnapshot(
    using credentials: SessionCredentials,
    timeout: Duration
  ) async throws -> MarketSnapshot
}
