import GRPCCore
import GRPCNIOTransportHTTP2TransportServices

public enum GRPCTransportError: Error, Sendable, Equatable {
  case invalidDisplayScale
  case invalidSnapshot
  case timeout
}

extension GRPCTransportError: CustomStringConvertible {
  public var description: String {
    switch self {
    case .invalidDisplayScale:
      "market display metadata is invalid"
    case .invalidSnapshot:
      "market snapshot violates the local RPC contract"
    case .timeout:
      "market snapshot request timed out"
    }
  }
}

public struct GRPCTransport: RPCTransport {
  private let endpoint: LocalEndpoint
  private let priceDisplayScale: Int

  public init(
    endpoint: LocalEndpoint,
    priceDisplayScale: Int
  ) throws {
    guard (0...18).contains(priceDisplayScale) else {
      throw GRPCTransportError.invalidDisplayScale
    }
    self.endpoint = endpoint
    self.priceDisplayScale = priceDisplayScale
  }

  public func getOrderBookSnapshot(
    using credentials: SessionCredentials,
    timeout: Duration
  ) async throws -> MarketSnapshot {
    let endpoint = endpoint
    let displayScale = priceDisplayScale
    return try await withGRPCClient(
      transport: .http2NIOTS(
        target: .ipv4(address: endpoint.host, port: endpoint.port),
        transportSecurity: .plaintext
      )
    ) { client in
      let market = Cmti_Market_V1_MarketStateService.Client(
        wrapping: client
      )
      var options = CallOptions.defaults
      options.timeout = timeout
      let response: Cmti_Market_V1_GetOrderBookSnapshotResponse
      do {
        response = try await market.getOrderBookSnapshot(
          Cmti_Market_V1_GetOrderBookSnapshotRequest(),
          metadata: Self.authenticationMetadata(using: credentials),
          options: options
        )
      } catch let error as RPCError where error.code == .deadlineExceeded {
        throw GRPCTransportError.timeout
      }
      return try Self.map(
        response,
        priceDisplayScale: displayScale
      )
    }
  }

  static func authenticationMetadata(
    using credentials: SessionCredentials
  ) -> Metadata {
    var metadata = Metadata()
    metadata.addBinary(
      credentials.descriptorBytes,
      forKey: "cmti-session-bin"
    )
    metadata.addBinary(
      credentials.tokenBytes,
      forKey: "cmti-token-bin"
    )
    return metadata
  }

  static func map(
    _ response: Cmti_Market_V1_GetOrderBookSnapshotResponse,
    priceDisplayScale: Int
  ) throws -> MarketSnapshot {
    guard (0...18).contains(priceDisplayScale),
      !response.source.isEmpty,
      !response.symbol.isEmpty,
      response.generation > 0,
      response.sequence > 0,
      isCanonicalPrice(
        response.bestBid,
        displayScale: priceDisplayScale
      ),
      isCanonicalPrice(
        response.bestAsk,
        displayScale: priceDisplayScale
      ),
      let health = mapHealth(response.health)
    else {
      throw GRPCTransportError.invalidSnapshot
    }

    return MarketSnapshot(
      source: response.source,
      symbol: response.symbol,
      generation: response.generation,
      sequence: response.sequence,
      bestBid: response.bestBid,
      bestAsk: response.bestAsk,
      health: health,
      eventUnixNanos: response.eventUnixNanos,
      receiveUnixNanos: response.receiveUnixNanos,
      freshnessMillis: response.freshnessMillis,
      priceDisplayScale: priceDisplayScale
    )
  }

  private static func mapHealth(
    _ health: Cmti_Market_V1_SnapshotHealth
  ) -> MarketHealth? {
    switch health {
    case .healthy:
      .healthy
    case .degraded:
      .degraded
    case .stale:
      .stale
    case .unavailable:
      .unavailable
    case .unspecified, .UNRECOGNIZED:
      nil
    }
  }

  private static func isCanonicalPrice(
    _ value: String,
    displayScale: Int
  ) -> Bool {
    let parts = value.split(separator: ".", omittingEmptySubsequences: false)
    guard parts.count == 1 || parts.count == 2,
      let integral = parts.first,
      !integral.isEmpty,
      integral.utf8.allSatisfy(Self.isDigit),
      integral == "0" || integral.first != "0"
    else {
      return false
    }

    guard parts.count == 2 else {
      return true
    }
    let fractional = parts[1]
    return !fractional.isEmpty
      && fractional.count <= displayScale
      && fractional.utf8.allSatisfy(Self.isDigit)
      && fractional.last != "0"
  }

  private static func isDigit(_ byte: UInt8) -> Bool {
    (UInt8(ascii: "0")...UInt8(ascii: "9")).contains(byte)
  }
}
