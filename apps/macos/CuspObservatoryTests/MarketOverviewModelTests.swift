import Testing
import TransitionClient

@testable import CuspObservatory

@Suite("Immutable market overview mapping")
struct MarketOverviewModelTests {
  @Test("BTC fixture values use display scale without numeric recomputation")
  func exactBTCRow() throws {
    let model = try MarketOverviewModel(snapshot: snapshot())

    #expect(model.symbol == "BTCUSDT")
    #expect(model.sequence == 102)
    #expect(model.source == "binance-fixture")
    #expect(model.bestBid == "60000.10")
    #expect(model.bestAsk == "60000.20")
    #expect(model.presentation == .healthy)
    #expect(
      model.accessibilitySummary
        == "BTCUSDT, healthy, best bid 60000.10, best ask 60000.20, sequence 102, source binance-fixture, freshness 5 milliseconds"
    )
  }

  @Test("stale and degraded remain server-authored visible states")
  func visibleHealthStates() throws {
    let stale = try MarketOverviewModel(
      snapshot: snapshot(health: .stale)
    )
    let degraded = try MarketOverviewModel(
      snapshot: snapshot(health: .degraded)
    )

    #expect(stale.presentation == .stale)
    #expect(degraded.presentation == .degraded)
  }

  @Test("display formatter fails rather than truncate server precision")
  func displayFormattingFailsClosed() {
    #expect(throws: MarketOverviewError.invalidPriceDisplay) {
      try DecimalDisplay.format("60000.123", scale: 2)
    }
    #expect(throws: MarketOverviewError.invalidPriceDisplay) {
      try DecimalDisplay.format("60000.1x", scale: 2)
    }
  }
}

private func snapshot(
  health: MarketHealth = .healthy
) -> MarketSnapshot {
  MarketSnapshot(
    source: "binance-fixture",
    symbol: "BTCUSDT",
    generation: 1,
    sequence: 102,
    bestBid: "60000.1",
    bestAsk: "60000.2",
    health: health,
    eventUnixNanos: 1_700_000_000_200_000_000,
    receiveUnixNanos: 1_700_000_000_205_000_000,
    freshnessMillis: 5,
    priceDisplayScale: 2
  )
}
