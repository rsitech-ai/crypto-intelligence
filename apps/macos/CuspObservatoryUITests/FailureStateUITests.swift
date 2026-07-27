import XCTest

@MainActor
final class FailureStateUITests: XCTestCase {
  func testInvalidDaemonPathShowsAccessibleRecoveryState() {
    let app = XCUIApplication()
    app.launchEnvironment["CMTI_DAEMON_PATH"] = "/invalid/cryptoriskd"
    app.launchEnvironment["CMTI_TEST_RUN_ID"] = UUID().uuidString
    app.launchEnvironment["CMTI_UI_TEST_MODE"] = "1"
    app.launch()

    let recovery = app.descendants(matching: .any)["recovery-state"]
    XCTAssertTrue(recovery.waitForExistence(timeout: 5))
    let message = app.descendants(matching: .any)["recovery-message"]
    XCTAssertTrue(message.exists)
    XCTAssertEqual(
      message.value as? String,
      "The local market service could not start."
    )
    XCTAssertTrue(app.buttons["retry-button"].isEnabled)
  }
}
