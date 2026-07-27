public protocol RPCTransport: Sendable {
  func getSnapshot(
    using credentials: SessionCredentials
  ) async throws -> MarketSnapshot
}
