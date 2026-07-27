public protocol RPCTransport: Sendable {
  func getSnapshot(
    using credentials: SessionCredentials,
    timeout: Duration
  ) async throws -> MarketSnapshot
}
