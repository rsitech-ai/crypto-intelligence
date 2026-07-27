import XCTest

@MainActor
final class MarketOverviewUITests: XCTestCase {
  func testRealDaemonPublishesAccessibleHealthyBTCRow() {
    let app = XCUIApplication()
    app.launchEnvironment["CMTI_TEST_RUN_ID"] = UUID().uuidString
    app.launch()

    let row = app.descendants(matching: .any)[
      "market-overview-row-BTCUSDT"
    ]
    XCTAssertTrue(row.waitForExistence(timeout: 10))
    XCTAssertTrue(app.staticTexts["60000.10"].exists)
    XCTAssertTrue(app.staticTexts["60000.20"].exists)
    XCTAssertTrue(app.staticTexts["Sequence 102"].exists)
    XCTAssertTrue(app.staticTexts["binance-fixture"].exists)

    let summary = app.descendants(matching: .any)["market-overview-summary"]
    XCTAssertTrue(summary.exists)
    XCTAssertEqual(
      summary.label,
      "BTCUSDT, healthy, best bid 60000.10, best ask 60000.20, sequence 102, source binance-fixture, freshness 0 milliseconds"
    )
  }
}
