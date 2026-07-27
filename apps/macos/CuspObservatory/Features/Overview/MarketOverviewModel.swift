import TransitionClient

enum MarketOverviewError: Error, Equatable {
  case invalidPriceDisplay
}

enum MarketHealthPresentation: String, Equatable, Sendable {
  case healthy
  case degraded
  case stale
  case unavailable
}

enum DecimalDisplay {
  static func format(_ value: String, scale: Int) throws -> String {
    guard (0...18).contains(scale) else {
      throw MarketOverviewError.invalidPriceDisplay
    }
    let parts = value.split(
      separator: ".",
      omittingEmptySubsequences: false
    )
    guard parts.count == 1 || parts.count == 2,
      let integral = parts.first,
      !integral.isEmpty,
      integral.utf8.allSatisfy(isDigit),
      integral == "0" || integral.first != "0"
    else {
      throw MarketOverviewError.invalidPriceDisplay
    }

    let fractional = parts.count == 2 ? String(parts[1]) : ""
    guard fractional.count <= scale,
      fractional.utf8.allSatisfy(isDigit)
    else {
      throw MarketOverviewError.invalidPriceDisplay
    }
    if scale == 0 {
      guard fractional.isEmpty else {
        throw MarketOverviewError.invalidPriceDisplay
      }
      return String(integral)
    }
    return "\(integral).\(fractional)\(String(repeating: "0", count: scale - fractional.count))"
  }

  private static func isDigit(_ byte: UInt8) -> Bool {
    (UInt8(ascii: "0")...UInt8(ascii: "9")).contains(byte)
  }
}

struct MarketOverviewModel: Equatable, Sendable {
  let symbol: String
  let sequence: UInt64
  let source: String
  let bestBid: String
  let bestAsk: String
  let freshnessMillis: UInt64
  let presentation: MarketHealthPresentation

  var accessibilitySummary: String {
    "\(symbol), \(presentation.rawValue), best bid \(bestBid), best ask \(bestAsk), sequence \(sequence), source \(source), freshness \(freshnessMillis) milliseconds"
  }

  init(snapshot: MarketSnapshot) throws {
    symbol = snapshot.symbol
    sequence = snapshot.sequence
    source = snapshot.source
    bestBid = try DecimalDisplay.format(
      snapshot.bestBid,
      scale: snapshot.priceDisplayScale
    )
    bestAsk = try DecimalDisplay.format(
      snapshot.bestAsk,
      scale: snapshot.priceDisplayScale
    )
    freshnessMillis = snapshot.freshnessMillis
    switch snapshot.health {
    case .healthy:
      presentation = .healthy
    case .degraded:
      presentation = .degraded
    case .stale:
      presentation = .stale
    case .unavailable:
      presentation = .unavailable
    }
  }
}
