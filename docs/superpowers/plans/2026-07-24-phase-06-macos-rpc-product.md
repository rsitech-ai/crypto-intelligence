# Phase 06 — Native macOS Product and Local RPC Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver the native, accessible macOS product that safely supervises the Rust daemon and exposes market overview, asset, structural, leverage, contagion, alert, replay, model, health, settings, and export workflows without becoming a second numerical authority.

**Architecture:** A Swift 6.3 strict-concurrency client package owns generated protobuf mapping and actor-isolated gRPC streams. A separate daemon supervisor performs inherited-descriptor session bootstrap and lifecycle control. A MainActor Observation model renders immutable server snapshots through focused SwiftUI feature modules; stale/degraded/experimental states remain visible, and replay state is isolated from live state.

**Tech Stack:** Swift 6.3, SwiftUI, Observation, Charts, Foundation, OSLog, Security/Keychain, ServiceManagement, UserNotifications, SwiftProtobuf, gRPC Swift 2/NIO transport, Swift Testing/XCTest/XCUITest.

## Global Constraints

- Rust production code uses Rust 1.97.1 initially, edition 2024, with MSRV 1.88; `Cargo.lock` is committed.
- Tokio stays on the tested 1.51 LTS minor line; Tonic stays on the tested 0.14 minor line.
- Swift code uses Swift 6.3 strict concurrency. The app baseline is Apple Silicon macOS 26; Core AI is optional and capability-gated on macOS 27 or later.
- SQLite is bundled at 3.53.3 or later; 3.51.3 is the absolute floor. SQLite stores metadata and audit state only, never high-rate books or trades.
- Source monetary values use checked `i128` fixed-point wrappers. `f64` begins only at an explicitly tested analytical boundary.
- One Rust modular-monolith daemon owns authoritative state and probabilities. Swift renders values received through versioned protobuf contracts and never recalculates production forecasts.
- Raw capture, replay, feature materialization, training, inference, and audit are local-first. Remote telemetry and hosted inference are disabled by default.
- No trading or withdrawal credentials, order placement, or automated execution are introduced in v1.
- Every bounded queue has a declared capacity and overflow policy. No order-book delta is silently dropped.
- Every production forecast is persisted with model, feature, label, quality, evidence, and calibration identifiers before alert delivery.
- Development follows test-driven increments, warnings-as-errors where practical, frequent focused commits, and deterministic fixtures.
- No task may weaken point-in-time correctness, abstention, source-quality gating, artifact signing, or audit lineage to make a demo pass.

---

## Scope and completion boundary

This plan implements the macOS application and Swift client layer, including authenticated daemon startup, all primary v1 screens, alert/replay workflows, settings/health/model audit, local notification, export, accessibility, and failure recovery. It excludes Core ML/Core AI/Foundation Models implementation, which is isolated in Phase 7, and excludes signing/notarization/update release automation, which is completed in Phase 8.

## File and module map

- `apps/macos/Packages/TransitionClient`: generated RPC/domain mapping, actor transport, compatibility, mocks, and formatters.
- `apps/macos/CuspObservatory/Daemon`: interactive/continuous helper lifecycle and secure session bootstrap.
- `apps/macos/CuspObservatory/App`: observable app model, navigation, subscriptions, and stale policy.
- `apps/macos/CuspObservatory/Features/*`: overview, asset, cusp, liquidity, contagion, alerts, replay, model, health, and settings modules.
- `apps/macos/CuspObservatory/Notifications`: local notification permission/routing.
- `apps/macos/CuspObservatory/Export`: secure user-initiated local exports.
- `apps/macos/CuspObservatoryTests` and `CuspObservatoryUITests`: concurrency, model, workflow, accessibility, and failure tests.

## Exit gate

The app builds with Swift strict concurrency; session bootstrap and compatibility fail closed; all primary workflows pass unit/UI/accessibility tests; daemon/source/model failure produces safe visible state; no Swift code authors or recalculates production probabilities; replay cannot contaminate live state or alerts; exports are local and auditable; and every chart has an accessible textual/table equivalent.

---

### Task 1: Create the Swift 6.3 macOS workspace, client package, design tokens, and strict-concurrency test target

 **Files:**
 - Create: `apps/macos/CuspObservatory.xcodeproj`
- Create: `apps/macos/CuspObservatory/App/CuspObservatoryApp.swift`
- Create: `apps/macos/CuspObservatory/App/AppEnvironment.swift`
- Create: `apps/macos/CuspObservatory/Design/DesignTokens.swift`
- Create: `apps/macos/Packages/TransitionClient/Package.swift`
- Create: `apps/macos/Packages/TransitionClient/Sources/TransitionClient/TransitionClient.swift`
- Create: `apps/macos/Packages/TransitionClient/Tests/TransitionClientTests/PackageContractTests.swift`

 **Interfaces:**
 - Consumes: approved platform/dependency policy and generated protobuf directory
 - Produces: a buildable native macOS 26 app, Swift package for RPC/domain mapping, strict concurrency, no analytics SDK, deterministic preview fixtures, and shared visual/accessibility tokens

 **Implementation notes**

 Use SwiftUI, Observation, Charts, Foundation, OSLog, Security, ServiceManagement, UserNotifications, SwiftProtobuf, and gRPC Swift only. Avoid global singletons; dependencies enter through `AppEnvironment` and test fixtures.

 - [ ] **Step 1: Write the failing test**

 Create or replace `apps/macos/Packages/TransitionClient/Tests/TransitionClientTests/PackageContractTests.swift` with:

 ```text
 import Testing
@testable import TransitionClient

@Test func packageDefaultsAreLocalOnly() {
    let policy = ClientPrivacyPolicy.productionDefault
    #expect(policy.remoteTelemetry == false)
    #expect(policy.automaticCrashUpload == false)
    #expect(policy.remoteBindingAllowed == false)
}

@Test func probabilityDisplayNeverFormatsAsCertainty() {
    #expect(ProbabilityFormatter.string(0.312) == "31.2%")
    #expect(ProbabilityFormatter.string(1.0) == "100.0%")
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `swift test --package-path apps/macos/Packages/TransitionClient`

 Expected: FAIL because the Swift package and domain formatters do not exist

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `apps/macos/Packages/TransitionClient/Sources/TransitionClient/TransitionClient.swift` with:

 ```text
 import Foundation

public struct ClientPrivacyPolicy: Sendable, Equatable {
    public let remoteTelemetry: Bool
    public let automaticCrashUpload: Bool
    public let remoteBindingAllowed: Bool

    public static let productionDefault = ClientPrivacyPolicy(
        remoteTelemetry: false, automaticCrashUpload: false, remoteBindingAllowed: false
    )
}

public enum ProbabilityFormatter {
    public static func string(_ value: Double) -> String {
        let bounded = min(max(value, 0), 1)
        return String(format: "%.1f%%", bounded * 100)
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `swift test --package-path apps/macos/Packages/TransitionClient`

 Expected: PASS under Swift 6.3 strict concurrency

 - [ ] **Step 5: Run the Swift/UI subsystem verification command**

 Run: `xcodebuild build -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS' SWIFT_STRICT_CONCURRENCY=complete`

 Expected: app and package compile without concurrency warnings or third-party analytics/crash dependencies

 - [ ] **Step 6: Inspect concurrency, stale-state, and numerical-authority boundaries**

 Run: `git diff --check && git status --short`

 Expected: no production probability is recomputed in Swift; no unstructured task escapes actor ownership; every stale/degraded/experimental state is visible and accessible.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add apps/macos/CuspObservatory.xcodeproj apps/macos/CuspObservatory apps/macos/Packages/TransitionClient Package.swift Package.resolved
 git commit -m "build: bootstrap native macOS application"
 ```
### Task 2: Generate Swift protobufs and implement the actor-isolated gRPC transport

 **Files:**
 - Modify: `proto/buf.gen.yaml`
- Create: `apps/macos/Packages/TransitionClient/Sources/TransitionClient/RPC/RPCTransport.swift`
- Create: `apps/macos/Packages/TransitionClient/Sources/TransitionClient/RPC/GRPCTransport.swift`
- Create: `apps/macos/Packages/TransitionClient/Sources/TransitionClient/RPC/Compatibility.swift`
- Create: `apps/macos/Packages/TransitionClient/Tests/TransitionClientTests/RPCTransportTests.swift`
- Modify: `scripts/generate-proto.sh`

 **Interfaces:**
 - Consumes: Phase 0 protobuf contracts, gRPC Swift 2, SwiftProtobuf, and session metadata fields
 - Produces: `RPCTransport` protocol, `GRPCTransport` actor, generated clients/messages, typed error mapping, cancellation, stream sequence/resume validation, and compatibility decision

 **Implementation notes**

 Map stable server codes to localized-ready Swift enums; preserve structured details separately from user text. Async streams cancel their underlying RPC on task cancellation and enforce per-stream sequence monotonicity.

 - [ ] **Step 1: Write the failing test**

 Create or replace `apps/macos/Packages/TransitionClient/Tests/TransitionClientTests/RPCTransportTests.swift` with:

 ```text
 import Testing
@testable import TransitionClient

@Test func majorVersionMismatchFailsClosed() {
    let result = Compatibility.evaluate(app: .init(major: 2, minor: 0), daemon: .init(major: 1, minor: 9))
    #expect(result == .incompatibleMajor)
}

@Test func streamSequenceRegressionIsRejected() async throws {
    let transport = MockRPCTransport(messages: [.fixture(sequence: 2), .fixture(sequence: 1)])
    await #expect(throws: StreamError.sequenceRegression) {
        for try await _ in transport.forecasts(request: .fixture()) {}
    }
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `swift test --package-path apps/macos/Packages/TransitionClient --filter RPCTransportTests`

 Expected: FAIL because transport, compatibility, and generated types are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `apps/macos/Packages/TransitionClient/Sources/TransitionClient/RPC/RPCTransport.swift` with:

 ```text
 import Foundation

public protocol RPCTransport: Sendable {
    func checkHealth() async throws -> HealthSnapshot
    func compatibility() async throws -> CompatibilityResult
    func forecastSnapshot(_ request: ForecastRequest) async throws -> ForecastSnapshot
    func forecasts(_ request: ForecastSubscription) -> AsyncThrowingStream<ForecastSnapshot, Error>
    func quality(_ request: QualitySubscription) -> AsyncThrowingStream<QualitySnapshot, Error>
    func alerts(_ request: AlertSubscription) -> AsyncThrowingStream<AlertEvent, Error>
    func shutdown() async throws
}

public actor StreamSequenceValidator {
    private var last: UInt64?
    public func accept(_ sequence: UInt64) throws {
        if let last, sequence <= last { throw StreamError.sequenceRegression }
        last = sequence
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `swift test --package-path apps/macos/Packages/TransitionClient --filter RPCTransportTests`

 Expected: PASS for version skew, metadata auth, cancellation, sequence/resume, typed errors, and max-message fixtures

 - [ ] **Step 5: Run the Swift/UI subsystem verification command**

 Run: `scripts/generate-proto.sh && git diff --exit-code -- apps/macos/GeneratedProto && swift test --package-path apps/macos/Packages/TransitionClient`

 Expected: Swift generation is reproducible and all transport tests pass

 - [ ] **Step 6: Inspect concurrency, stale-state, and numerical-authority boundaries**

 Run: `git diff --check && git status --short`

 Expected: no production probability is recomputed in Swift; no unstructured task escapes actor ownership; every stale/degraded/experimental state is visible and accessible.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add proto/buf.gen.yaml apps/macos/GeneratedProto apps/macos/Packages/TransitionClient/Sources/TransitionClient/RPC apps/macos/Packages/TransitionClient/Tests/TransitionClientTests/RPCTransportTests.swift scripts/generate-proto.sh Package.resolved
 git commit -m "feat: add Swift gRPC client transport"
 ```
### Task 3: Implement daemon launch, inherited-descriptor session bootstrap, supervision, and user-controlled continuous mode

 **Files:**
 - Create: `apps/macos/CuspObservatory/Daemon/DaemonSupervisor.swift`
- Create: `apps/macos/CuspObservatory/Daemon/SessionBootstrap.swift`
- Create: `apps/macos/CuspObservatory/Daemon/DaemonState.swift`
- Create: `apps/macos/CuspObservatory/Daemon/ContinuousModeController.swift`
- Create: `apps/macos/CuspObservatoryTests/DaemonSupervisorTests.swift`

 **Interfaces:**
 - Consumes: cryptoriskd executable, RPC transport factory, Keychain/service policy, and compatibility handshake
 - Produces: `DaemonSupervisor` actor with interactive process launch, protected pipe secret exchange, port/nonce read, health/compatibility, exponential recovery, graceful shutdown, crash state, and opt-in ServiceManagement mode

 **Implementation notes**

 Interactive mode is the default. Continuous mode is explicit, reversible, and Keychain-scoped to signed components. The app never silently relaunches indefinitely; retry budget and user-visible failure reason are preserved.

 - [ ] **Step 1: Write the failing test**

 Create or replace `apps/macos/CuspObservatoryTests/DaemonSupervisorTests.swift` with:

 ```text
 import XCTest
@testable import CuspObservatory

final class DaemonSupervisorTests: XCTestCase {
    func testSecretIsPassedThroughInheritedDescriptorNotArgumentsOrEnvironment() async throws {
        let launcher = MockDaemonLauncher()
        let supervisor = DaemonSupervisor(launcher: launcher, transportFactory: MockTransportFactory())
        try await supervisor.startInteractive()
        XCTAssertFalse(launcher.lastArguments.joined().contains("secret"))
        XCTAssertNil(launcher.lastEnvironment["SESSION_SECRET"])
        XCTAssertEqual(launcher.inheritedSecretByteCount, 32)
    }

    func testMajorMismatchStopsAtIncompatibleState() async throws {
        let supervisor = DaemonSupervisor.fixture(compatibility: .incompatibleMajor)
        await XCTAssertThrowsErrorAsync { try await supervisor.startInteractive() }
        XCTAssertEqual(await supervisor.state, .incompatible)
    }
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS' -only-testing:CuspObservatoryTests/DaemonSupervisorTests`

 Expected: FAIL because daemon supervisor and bootstrap do not exist

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `apps/macos/CuspObservatory/Daemon/DaemonSupervisor.swift` with:

 ```text
 import Foundation
import Observation

actor DaemonSupervisor {
    private let launcher: any DaemonLaunching
    private let transportFactory: any RPCTransportFactory
    private(set) var state: DaemonState = .stopped
    private var process: Process?
    private var transport: (any RPCTransport)?

    func startInteractive() async throws {
        guard state == .stopped || state == .failed else { return }
        state = .starting
        let bootstrap = try SessionBootstrap.make()
        let launch = try launcher.launch(secretWriteDescriptor: bootstrap.writeDescriptor)
        process = launch.process
        let endpoint = try await bootstrap.readEndpointAndNonce()
        let candidate = try await transportFactory.make(endpoint: endpoint, secret: bootstrap.secret)
        let compatibility = try await candidate.compatibility()
        guard compatibility.isCompatible else { state = .incompatible; throw DaemonError.incompatible }
        transport = candidate
        state = .running
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS' -only-testing:CuspObservatoryTests/DaemonSupervisorTests`

 Expected: PASS for secret path, endpoint/nonce, health, incompatibility, crash, restart, graceful shutdown, and continuous-mode consent tests

 - [ ] **Step 5: Run the Swift/UI subsystem verification command**

 Run: `xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS'`

 Expected: all daemon lifecycle tests pass and an unexpected helper exit becomes a visible disconnected/failed state

 - [ ] **Step 6: Inspect concurrency, stale-state, and numerical-authority boundaries**

 Run: `git diff --check && git status --short`

 Expected: no production probability is recomputed in Swift; no unstructured task escapes actor ownership; every stale/degraded/experimental state is visible and accessible.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add apps/macos/CuspObservatory/Daemon apps/macos/CuspObservatoryTests/DaemonSupervisorTests.swift apps/macos/CuspObservatory.xcodeproj/project.pbxproj
 git commit -m "feat: supervise the local Rust daemon"
 ```
### Task 4: Create the observable app model, navigation, subscription coordinator, and stale-state policy

 **Files:**
 - Create: `apps/macos/CuspObservatory/App/AppModel.swift`
- Create: `apps/macos/CuspObservatory/App/NavigationRoute.swift`
- Create: `apps/macos/CuspObservatory/App/SubscriptionCoordinator.swift`
- Create: `apps/macos/CuspObservatory/App/StalenessPolicy.swift`
- Create: `apps/macos/CuspObservatoryTests/AppModelTests.swift`

 **Interfaces:**
 - Consumes: daemon supervisor, RPC transport, catalog/forecast/quality streams, and Swift Observation
 - Produces: `@Observable @MainActor AppModel`, typed navigation routes, actor-owned subscriptions, watchlist/filter state, snapshot replacement/delta handling, reconnect/resume, and visible stale/degraded state

 **Implementation notes**

 Stale data remains visible with age, quality, and reason; it is not erased into an empty chart. UI sampling never changes the forecast ledger or server stream semantics. All long-lived stream tasks live inside one actor and are cancelled on session replacement.

 - [ ] **Step 1: Write the failing test**

 Create or replace `apps/macos/CuspObservatoryTests/AppModelTests.swift` with:

 ```text
 import XCTest
@testable import CuspObservatory

@MainActor final class AppModelTests: XCTestCase {
    func testStaleForecastRemainsVisibleWithStaleBadge() async throws {
        let model = AppModel.fixture(now: 1_000)
        model.apply(.forecast(.fixture(asOf: 100)))
        XCTAssertNotNil(model.forecasts["BTC"])
        XCTAssertEqual(model.forecasts["BTC"]?.displayState, .stale)
    }

    func testSnapshotReplacesPriorStreamStateBeforeDeltas() async throws {
        let model = AppModel.fixture()
        model.apply(.forecast(.fixture(sequence: 10, snapshot: true, probability: 0.2)))
        model.apply(.forecast(.fixture(sequence: 11, snapshot: false, probability: 0.3)))
        XCTAssertEqual(model.forecasts["BTC"]?.probability, 0.3)
    }
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS' -only-testing:CuspObservatoryTests/AppModelTests`

 Expected: FAIL because the app model and subscription coordinator are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `apps/macos/CuspObservatory/App/AppModel.swift` with:

 ```text
 import Observation

@MainActor @Observable
final class AppModel {
    var daemonState: DaemonState = .stopped
    var selectedRoute: NavigationRoute?
    var watchlist: [AssetID] = []
    var forecasts: [AssetID: ForecastViewState] = [:]
    var quality: [SourceID: QualityViewState] = [:]
    var activeAlerts: [AlertEventViewState] = []
    var lastError: AppError?

    private let staleness: StalenessPolicy

    init(staleness: StalenessPolicy) { self.staleness = staleness }

    func apply(_ event: AppEvent) {
        switch event {
        case .forecast(let snapshot): forecasts[snapshot.assetID] = ForecastViewState(snapshot, staleness: staleness)
        case .quality(let snapshot): quality[snapshot.sourceID] = QualityViewState(snapshot)
        case .alert(let event): activeAlerts = AlertEventViewState.reduce(activeAlerts, event)
        }
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS' -only-testing:CuspObservatoryTests/AppModelTests`

 Expected: PASS for snapshot/delta, resume, reconnect, cancellation, route restoration, stale/degraded/experimental, and empty-state tests

 - [ ] **Step 5: Run the Swift/UI subsystem verification command**

 Run: `swift test --package-path apps/macos/Packages/TransitionClient && xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination "platform=macOS"`

 Expected: client and app-model suites pass under strict concurrency

 - [ ] **Step 6: Inspect concurrency, stale-state, and numerical-authority boundaries**

 Run: `git diff --check && git status --short`

 Expected: no production probability is recomputed in Swift; no unstructured task escapes actor ownership; every stale/degraded/experimental state is visible and accessible.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add apps/macos/CuspObservatory/App/AppModel.swift apps/macos/CuspObservatory/App/NavigationRoute.swift apps/macos/CuspObservatory/App/SubscriptionCoordinator.swift apps/macos/CuspObservatory/App/StalenessPolicy.swift apps/macos/CuspObservatoryTests/AppModelTests.swift
 git commit -m "feat: add observable app state and subscriptions"
 ```
### Task 5: Build the Market Overview screener and probability/quality heatmap

 **Files:**
 - Create: `apps/macos/CuspObservatory/Features/Overview/MarketOverviewView.swift`
- Create: `apps/macos/CuspObservatory/Features/Overview/MarketOverviewModel.swift`
- Create: `apps/macos/CuspObservatory/Features/Overview/AssetRiskRow.swift`
- Create: `apps/macos/CuspObservatoryTests/MarketOverviewModelTests.swift`
- Create: `apps/macos/CuspObservatoryUITests/MarketOverviewUITests.swift`

 **Interfaces:**
 - Consumes: AppModel forecast/quality/catalog state and design/accessibility tokens
 - Produces: sortable/filterable screener for assets, horizons, event probabilities, base rate, structural/liquidity/leverage/contagion state, availability badges, and compact sparkline history

 **Implementation notes**

 Heatmap cells encode value through text and accessible labels, not color alone. Avoid displaying one undifferentiated score: event/horizon is always named, and base rate/quality/availability remain visible.

 - [ ] **Step 1: Write the failing test**

 Create or replace `apps/macos/CuspObservatoryTests/MarketOverviewModelTests.swift` with:

 ```text
 import XCTest
@testable import CuspObservatory

@MainActor final class MarketOverviewModelTests: XCTestCase {
    func testSortUsesRequestedProbabilityButKeepsUnavailableRows() {
        let model = MarketOverviewModel.fixture()
        let rows = model.rows(sortedBy: .probability(event: .downside, horizon: .fourHours))
        XCTAssertEqual(rows.first?.assetID, "ETH")
        XCTAssertTrue(rows.contains(where: { $0.availability == .outOfDistribution }))
    }

    func testRiskDeltaIsAgainstDisplayedBaseRate() {
        let row = MarketOverviewModel.fixture().rows.first!
        XCTAssertEqual(row.probabilityMinusBaseRate, row.probability - row.baseRate, accuracy: 1e-12)
    }
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS' -only-testing:CuspObservatoryTests/MarketOverviewModelTests`

 Expected: FAIL because overview model/view are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `apps/macos/CuspObservatory/Features/Overview/MarketOverviewModel.swift` with:

 ```text
 import Observation

@MainActor @Observable
final class MarketOverviewModel {
    var selectedHorizon: ForecastHorizon = .fourHours
    var selectedEvent: EventType = .downside
    var searchText = ""
    var availabilityFilter: Set<AvailabilityState> = Set(AvailabilityState.allCases)
    private(set) var rows: [AssetRiskRowModel] = []

    func update(from app: AppModel) {
        rows = app.forecasts.values.map(AssetRiskRowModel.init).filter { row in
            searchText.isEmpty || row.assetID.localizedCaseInsensitiveContains(searchText)
        }
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS' -only-testing:CuspObservatoryTests/MarketOverviewModelTests`

 Expected: PASS for sorting, filtering, base-rate comparison, unavailable rows, staleness, and watchlist behavior

 - [ ] **Step 5: Run the Swift/UI subsystem verification command**

 Run: `xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS' -only-testing:CuspObservatoryUITests/MarketOverviewUITests`

 Expected: overview launches, filters by keyboard, exposes VoiceOver labels, and unmistakably distinguishes available/degraded/experimental/unavailable states

 - [ ] **Step 6: Inspect concurrency, stale-state, and numerical-authority boundaries**

 Run: `git diff --check && git status --short`

 Expected: no production probability is recomputed in Swift; no unstructured task escapes actor ownership; every stale/degraded/experimental state is visible and accessible.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add apps/macos/CuspObservatory/Features/Overview apps/macos/CuspObservatoryTests/MarketOverviewModelTests.swift apps/macos/CuspObservatoryUITests/MarketOverviewUITests.swift
 git commit -m "feat: add market risk overview"
 ```
### Task 6: Build Asset Detail, forecast curves, evidence, and scenario views

 **Files:**
 - Create: `apps/macos/CuspObservatory/Features/Asset/AssetDetailView.swift`
- Create: `apps/macos/CuspObservatory/Features/Asset/AssetDetailModel.swift`
- Create: `apps/macos/CuspObservatory/Features/Asset/ForecastCurveChart.swift`
- Create: `apps/macos/CuspObservatory/Features/Asset/EvidencePanel.swift`
- Create: `apps/macos/CuspObservatory/Features/Asset/ScenarioFanChart.swift`
- Create: `apps/macos/CuspObservatoryTests/AssetDetailModelTests.swift`

 **Interfaces:**
 - Consumes: forecast snapshots, evidence bundles, scenario distributions, market state, and local RPC retrieval
 - Produces: asset-level event/horizon curves, uncertainty/base rate, price/volatility context, scenario fan/thresholds, deterministic evidence contributions, model/version/quality audit, and explicit simulated-path labeling

 **Implementation notes**

 Feature contributions are labeled contributions/associations, not causes. Numeric values are copied from typed records. Charts provide accessible summaries and data tables for screen-reader and export parity.

 - [ ] **Step 1: Write the failing test**

 Create or replace `apps/macos/CuspObservatoryTests/AssetDetailModelTests.swift` with:

 ```text
 import XCTest
@testable import CuspObservatory

@MainActor final class AssetDetailModelTests: XCTestCase {
    func testForecastCurvePreservesServerProbabilitiesExactly() {
        let source = ForecastSnapshot.fixture(horizons: [900: 0.1, 3600: 0.2, 14400: 0.3, 86400: 0.4])
        let model = AssetDetailModel(snapshot: source)
        XCTAssertEqual(model.curve.map(\.probability), [0.1, 0.2, 0.3, 0.4])
    }

    func testScenarioCopyStatesSimulationNotObservedPrediction() {
        let model = AssetDetailModel.fixtureWithScenario()
        XCTAssertTrue(model.scenarioDisclosure.contains("simulated"))
    }
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS' -only-testing:CuspObservatoryTests/AssetDetailModelTests`

 Expected: FAIL because asset detail components are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `apps/macos/CuspObservatory/Features/Asset/AssetDetailModel.swift` with:

 ```text
 import Observation

@MainActor @Observable
final class AssetDetailModel {
    let assetID: AssetID
    private(set) var forecast: ForecastViewState
    private(set) var evidence: EvidenceBundleViewState?
    private(set) var scenario: ScenarioViewState?
    let scenarioDisclosure = "Paths are conditional simulated scenarios, not observed future prices."

    var curve: [ForecastPoint] { forecast.horizons.map { ForecastPoint(horizon: $0.horizon, probability: $0.probability, lower: $0.lower, upper: $0.upper, baseRate: $0.baseRate) } }

    init(snapshot: ForecastSnapshot) {
        self.assetID = snapshot.assetID
        self.forecast = ForecastViewState(snapshot)
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS' -only-testing:CuspObservatoryTests/AssetDetailModelTests`

 Expected: PASS for exact values, curve order, uncertainty, base rate, evidence loading, scenario disclosure, stale state, and failed-detail RPC tests

 - [ ] **Step 5: Run the Swift/UI subsystem verification command**

 Run: `xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS' -only-testing:CuspObservatoryUITests/AssetDetailUITests`

 Expected: asset detail charts and audit panels pass UI/VoiceOver/keyboard tests without recalculating probabilities

 - [ ] **Step 6: Inspect concurrency, stale-state, and numerical-authority boundaries**

 Run: `git diff --check && git status --short`

 Expected: no production probability is recomputed in Swift; no unstructured task escapes actor ownership; every stale/degraded/experimental state is visible and accessible.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add apps/macos/CuspObservatory/Features/Asset apps/macos/CuspObservatoryTests/AssetDetailModelTests.swift apps/macos/CuspObservatoryUITests/AssetDetailUITests.swift
 git commit -m "feat: add asset forecast and evidence detail"
 ```
### Task 7: Build Cusp & Regime and Liquidity & Leverage analytical workspaces

 **Files:**
 - Create: `apps/macos/CuspObservatory/Features/Cusp/CuspRegimeView.swift`
- Create: `apps/macos/CuspObservatory/Features/Cusp/CuspControlPlaneChart.swift`
- Create: `apps/macos/CuspObservatory/Features/Cusp/CuspRegimeModel.swift`
- Create: `apps/macos/CuspObservatory/Features/Liquidity/LiquidityLeverageView.swift`
- Create: `apps/macos/CuspObservatory/Features/Liquidity/LiquidityLeverageModel.swift`
- Create: `apps/macos/CuspObservatoryTests/StructuralViewsTests.swift`

 **Interfaces:**
 - Consumes: CuspService state/history, regime posteriors, market books, funding/OI/basis/liquidation features, and production-eligibility status
 - Produces: control-plane fold/path/uncertainty chart, branch/barrier/restoring-force cards, regime probabilities, and cross-venue depth/funding/OI/basis/liquidation panels with research-only disclosure

 **Implementation notes**

 The control-plane chart plots the approved alpha/beta convention and server-supplied fold/path data; Swift may generate display samples of the fold curve only from a server-declared sign/version and tests compare them to fixtures. Any research-only module has a persistent status banner.

 - [ ] **Step 1: Write the failing test**

 Create or replace `apps/macos/CuspObservatoryTests/StructuralViewsTests.swift` with:

 ```text
 import XCTest
@testable import CuspObservatory

@MainActor final class StructuralViewsTests: XCTestCase {
    func testResearchOnlyCuspDisplaysNotUsedInProductionProbability() {
        let model = CuspRegimeModel.fixture(status: .experimental)
        XCTAssertEqual(model.productionUseText, "Not used in production probability")
    }

    func testFoldChartUsesServerCoordinatesWithoutChangingSign() {
        let model = CuspRegimeModel.fixture(alpha: -2, beta: 3, foldDistance: 0)
        XCTAssertEqual(model.currentPoint.alpha, -2)
        XCTAssertEqual(model.currentPoint.beta, 3)
        XCTAssertEqual(model.foldDistance, 0)
    }
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS' -only-testing:CuspObservatoryTests/StructuralViewsTests`

 Expected: FAIL because structural/liquidity view models are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `apps/macos/CuspObservatory/Features/Cusp/CuspRegimeModel.swift` with:

 ```text
 import Observation

@MainActor @Observable
final class CuspRegimeModel {
    private(set) var snapshot: CuspSnapshotViewState
    private(set) var history: [CuspPoint] = []

    var currentPoint: CuspPoint { CuspPoint(alpha: snapshot.alpha, beta: snapshot.beta, asOf: snapshot.asOf) }
    var foldDistance: Double? { snapshot.foldDistance }
    var productionUseText: String { snapshot.productionEligible ? "Used by production ensemble" : "Not used in production probability" }

    init(snapshot: CuspSnapshotViewState) { self.snapshot = snapshot }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS' -only-testing:CuspObservatoryTests/StructuralViewsTests`

 Expected: PASS for sign coordinates, fold path, uncertainty, status disclosure, branch probability, stale state, liquidation completeness, and source-quality tests

 - [ ] **Step 5: Run the Swift/UI subsystem verification command**

 Run: `xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS' -only-testing:CuspObservatoryUITests/StructuralViewsUITests`

 Expected: charts, disclosures, tabs, data tables, and accessible alternatives pass UI tests

 - [ ] **Step 6: Inspect concurrency, stale-state, and numerical-authority boundaries**

 Run: `git diff --check && git status --short`

 Expected: no production probability is recomputed in Swift; no unstructured task escapes actor ownership; every stale/degraded/experimental state is visible and accessible.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add apps/macos/CuspObservatory/Features/Cusp apps/macos/CuspObservatory/Features/Liquidity apps/macos/CuspObservatoryTests/StructuralViewsTests.swift apps/macos/CuspObservatoryUITests/StructuralViewsUITests.swift
 git commit -m "feat: add structural and leverage workspaces"
 ```
### Task 8: Build the Contagion workspace with source uncertainty and stablecoin state

 **Files:**
 - Create: `apps/macos/CuspObservatory/Features/Contagion/ContagionView.swift`
- Create: `apps/macos/CuspObservatory/Features/Contagion/ContagionModel.swift`
- Create: `apps/macos/CuspObservatory/Features/Contagion/ContagionGraphView.swift`
- Create: `apps/macos/CuspObservatoryTests/ContagionModelTests.swift`

 **Interfaces:**
 - Consumes: coupled-cusp/contagion/stablecoin RPC records and availability/quality state
 - Produces: BTC–ETH joint instability, minimum Hessian eigenvalue, likely propagation source with uncertainty, affected assets/delays, stablecoin deviations, systemic/idiosyncratic components, and experimental disclosure

 **Implementation notes**

 Provide a table alternative to the graph. Node size/color are not the only encodings. “Likely source” always includes probability/uncertainty and is described as a model candidate, not proven causation.

 - [ ] **Step 1: Write the failing test**

 Create or replace `apps/macos/CuspObservatoryTests/ContagionModelTests.swift` with:

 ```text
 import XCTest
@testable import CuspObservatory

@MainActor final class ContagionModelTests: XCTestCase {
    func testSourceIsDisplayedAsProbabilityNotFact() {
        let model = ContagionModel.fixture(source: "BTC", probability: 0.62)
        XCTAssertEqual(model.sourceSummary, "BTC is the leading source candidate (62.0%)")
    }

    func testInsufficientEventCountKeepsExperimentalBadge() {
        let model = ContagionModel.fixture(availability: .experimental)
        XCTAssertTrue(model.badges.contains(.experimental))
    }
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS' -only-testing:CuspObservatoryTests/ContagionModelTests`

 Expected: FAIL because contagion UI/model is absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `apps/macos/CuspObservatory/Features/Contagion/ContagionModel.swift` with:

 ```text
 import Observation

@MainActor @Observable
final class ContagionModel {
    private(set) var state: ContagionViewState

    var sourceSummary: String {
        guard let source = state.likelySource, let probability = state.sourceProbability else { return "No reliable source candidate" }
        return "\(source) is the leading source candidate (\(ProbabilityFormatter.string(probability)))"
    }

    var badges: Set<StatusBadge> { StatusBadge.from(availability: state.availability, quality: state.quality) }

    init(state: ContagionViewState) { self.state = state }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS' -only-testing:CuspObservatoryTests/ContagionModelTests`

 Expected: PASS for probabilistic source wording, stablecoin state, missing nodes, delays, quality, and experimental status

 - [ ] **Step 5: Run the Swift/UI subsystem verification command**

 Run: `xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS' -only-testing:CuspObservatoryUITests/ContagionUITests`

 Expected: graph/list/table alternatives and VoiceOver labels pass; no causal statement is presented

 - [ ] **Step 6: Inspect concurrency, stale-state, and numerical-authority boundaries**

 Run: `git diff --check && git status --short`

 Expected: no production probability is recomputed in Swift; no unstructured task escapes actor ownership; every stale/degraded/experimental state is visible and accessible.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add apps/macos/CuspObservatory/Features/Contagion apps/macos/CuspObservatoryTests/ContagionModelTests.swift apps/macos/CuspObservatoryUITests/ContagionUITests.swift
 git commit -m "feat: add contagion and stablecoin workspace"
 ```
### Task 9: Build alert rule editing, lifecycle history, acknowledgment, and local notification routing

 **Files:**
 - Create: `apps/macos/CuspObservatory/Features/Alerts/AlertsView.swift`
- Create: `apps/macos/CuspObservatory/Features/Alerts/AlertRuleEditor.swift`
- Create: `apps/macos/CuspObservatory/Features/Alerts/AlertsModel.swift`
- Create: `apps/macos/CuspObservatory/Notifications/NotificationRouter.swift`
- Create: `apps/macos/CuspObservatoryTests/AlertsModelTests.swift`

 **Interfaces:**
 - Consumes: AlertService catalog/CRUD/events, server rule validation errors, local UserNotifications permission, and evidence routes
 - Produces: typed rule builder plus advanced text, validation preview, persistence/cooldown/recovery/budget controls, lifecycle/history/outcome review, acknowledgment, and local notification deep links

 **Implementation notes**

 The server remains authoritative for parsing/validation. The Swift builder uses the same catalog and generates rule text; it does not evaluate rules locally. Notifications contain no secret or raw sensitive payload.

 - [ ] **Step 1: Write the failing test**

 Create or replace `apps/macos/CuspObservatoryTests/AlertsModelTests.swift` with:

 ```text
 import XCTest
@testable import CuspObservatory

@MainActor final class AlertsModelTests: XCTestCase {
    func testRuleIsSavedOnlyAfterServerValidation() async throws {
        let service = MockAlertService(validation: .invalid(field: "unknown_score"))
        let model = AlertsModel(service: service)
        await XCTAssertThrowsErrorAsync { try await model.save(.fixture(text: "unknown_score > 0.1")) }
        XCTAssertEqual(service.upsertCount, 0)
    }

    func testNotificationDeepLinkCarriesAlertAndForecastIDs() {
        let request = NotificationRouter.request(for: .fixture(alertID: "a", forecastID: "f"))
        XCTAssertEqual(request.content.userInfo["alert_id"] as? String, "a")
        XCTAssertEqual(request.content.userInfo["forecast_id"] as? String, "f")
    }
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS' -only-testing:CuspObservatoryTests/AlertsModelTests`

 Expected: FAIL because alert UI/routing is absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `apps/macos/CuspObservatory/Features/Alerts/AlertsModel.swift` with:

 ```text
 import Observation

@MainActor @Observable
final class AlertsModel {
    private let service: any AlertServicing
    private(set) var rules: [AlertRuleViewState] = []
    private(set) var events: [AlertEventViewState] = []
    var validation: RuleValidationState = .idle

    init(service: any AlertServicing) { self.service = service }

    func validate(_ draft: AlertRuleDraft) async {
        validation = await service.validate(draft)
    }

    func save(_ draft: AlertRuleDraft) async throws {
        let result = await service.validate(draft)
        guard result.isValid else { validation = result; throw AlertsUIError.invalidRule }
        try await service.upsert(draft)
        rules = try await service.listRules()
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS' -only-testing:CuspObservatoryTests/AlertsModelTests`

 Expected: PASS for validation, save, edit, experimental opt-in, history, acknowledgment, budget, notification permission/denial, and deep-link tests

 - [ ] **Step 5: Run the Swift/UI subsystem verification command**

 Run: `xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS' -only-testing:CuspObservatoryUITests/AlertsUITests`

 Expected: keyboard/VoiceOver rule creation and lifecycle review pass; invalid rules cannot be saved

 - [ ] **Step 6: Inspect concurrency, stale-state, and numerical-authority boundaries**

 Run: `git diff --check && git status --short`

 Expected: no production probability is recomputed in Swift; no unstructured task escapes actor ownership; every stale/degraded/experimental state is visible and accessible.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add apps/macos/CuspObservatory/Features/Alerts apps/macos/CuspObservatory/Notifications apps/macos/CuspObservatoryTests/AlertsModelTests.swift apps/macos/CuspObservatoryUITests/AlertsUITests.swift
 git commit -m "feat: add alert workflows and notifications"
 ```
### Task 10: Build deterministic replay controls and isolated replay workspace

 **Files:**
 - Create: `apps/macos/CuspObservatory/Features/Replay/ReplayView.swift`
- Create: `apps/macos/CuspObservatory/Features/Replay/ReplayModel.swift`
- Create: `apps/macos/CuspObservatory/Features/Replay/ReplayTimeline.swift`
- Create: `apps/macos/CuspObservatoryTests/ReplayModelTests.swift`

 **Interfaces:**
 - Consumes: ReplayService create/control/state streams, replay namespace IDs, and forecast/evidence/alert streams
 - Produces: create/select interval, play/pause/step/speed/seek, progress/health, replay-only state stores, side-by-side live/replay indicators, raw event/book/feature/model timeline, and deterministic digest display

 **Implementation notes**

 Replay data always has a distinct namespace, accent/banner, and process state. It cannot trigger real local notifications unless an explicit test-notification mode is enabled and visibly labeled.

 - [ ] **Step 1: Write the failing test**

 Create or replace `apps/macos/CuspObservatoryTests/ReplayModelTests.swift` with:

 ```text
 import XCTest
@testable import CuspObservatory

@MainActor final class ReplayModelTests: XCTestCase {
    func testReplayEventsNeverMutateLiveForecastStore() async throws {
        let app = AppModel.fixtureWithLiveForecast(probability: 0.2)
        let replay = ReplayModel.fixture(app: app)
        replay.apply(.forecast(.fixture(probability: 0.8, namespace: .replay("r1"))))
        XCTAssertEqual(app.forecasts["BTC"]?.probability, 0.2)
        XCTAssertEqual(replay.forecasts["BTC"]?.probability, 0.8)
    }

    func testSeekRequestsSnapshotBeforeApplyingDeltas() async throws {
        let model = ReplayModel.fixture()
        try await model.seek(to: 1_000)
        XCTAssertTrue(model.lastSeekRequestedSnapshot)
    }
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS' -only-testing:CuspObservatoryTests/ReplayModelTests`

 Expected: FAIL because replay UI/model is absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `apps/macos/CuspObservatory/Features/Replay/ReplayModel.swift` with:

 ```text
 import Observation

@MainActor @Observable
final class ReplayModel {
    private let service: any ReplayServicing
    private(set) var sessionID: String?
    private(set) var state: ReplayState = .idle
    private(set) var forecasts: [AssetID: ForecastViewState] = [:]
    private(set) var timeline: [ReplayTimelineEvent] = []

    init(service: any ReplayServicing) { self.service = service }

    func seek(to eventTimeNS: Int64) async throws {
        guard let sessionID else { throw ReplayUIError.noSession }
        try await service.control(.seek(sessionID: sessionID, eventTimeNS: eventTimeNS, requireSnapshot: true))
        forecasts.removeAll(keepingCapacity: true)
        timeline.removeAll(keepingCapacity: true)
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS' -only-testing:CuspObservatoryTests/ReplayModelTests`

 Expected: PASS for namespace isolation, controls, seek snapshot, cancellation, error, digest, and live/replay simultaneous display tests

 - [ ] **Step 5: Run the Swift/UI subsystem verification command**

 Run: `xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS' -only-testing:CuspObservatoryUITests/ReplayUITests`

 Expected: replay control/timeline workflows pass keyboard, VoiceOver, and empty/error state tests

 - [ ] **Step 6: Inspect concurrency, stale-state, and numerical-authority boundaries**

 Run: `git diff --check && git status --short`

 Expected: no production probability is recomputed in Swift; no unstructured task escapes actor ownership; every stale/degraded/experimental state is visible and accessible.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add apps/macos/CuspObservatory/Features/Replay apps/macos/CuspObservatoryTests/ReplayModelTests.swift apps/macos/CuspObservatoryUITests/ReplayUITests.swift
 git commit -m "feat: add isolated historical replay workspace"
 ```
### Task 11: Build Model Lab, Data Health, Settings, network activity, and local export

 **Files:**
 - Create: `apps/macos/CuspObservatory/Features/ModelLab/ModelLabView.swift`
- Create: `apps/macos/CuspObservatory/Features/Health/DataHealthView.swift`
- Create: `apps/macos/CuspObservatory/Features/Settings/SettingsView.swift`
- Create: `apps/macos/CuspObservatory/Features/Settings/NetworkActivityView.swift`
- Create: `apps/macos/CuspObservatory/Export/ExportCoordinator.swift`
- Create: `apps/macos/CuspObservatoryTests/OperationsViewsTests.swift`

 **Interfaces:**
 - Consumes: model registry/cards/metrics, DataQualityService, Admin/Settings/Export services, config schema/change impact, and network destination catalog
 - Produces: model cards/status/metrics/ablation/drift views; source/book/storage/model health/incidents; validated settings with restart indication; local outbound destination page; user-initiated CSV/JSON/Parquet report export

 **Implementation notes**

 The network activity page lists exchange/node/update/webhook destinations by category and current enabled state. Export warns when source licensing restricts raw redistribution and defaults to derived/user-owned records.

 - [ ] **Step 1: Write the failing test**

 Create or replace `apps/macos/CuspObservatoryTests/OperationsViewsTests.swift` with:

 ```text
 import XCTest
@testable import CuspObservatory

@MainActor final class OperationsViewsTests: XCTestCase {
    func testRestartRequiredSettingIsLabeledBeforeApply() async throws {
        let model = SettingsModel.fixture(changeImpact: .restartRequired)
        model.set(path: "daemon.data_dir", value: "/tmp/new")
        XCTAssertTrue(model.pendingChangeSummary.contains("Restart required"))
    }

    func testExportIsUserInitiatedAndContainsModelAndLineageIDs() async throws {
        let export = try await ExportCoordinator.fixture().exportForecasts(.fixture())
        let text = try String(contentsOf: export.url)
        XCTAssertTrue(text.contains("model_bundle_id"))
        XCTAssertTrue(text.contains("evidence_bundle_id"))
    }
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS' -only-testing:CuspObservatoryTests/OperationsViewsTests`

 Expected: FAIL because operational views/export are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `apps/macos/CuspObservatory/Export/ExportCoordinator.swift` with:

 ```text
 import Foundation

actor ExportCoordinator {
    private let service: any ExportServicing
    private let fileManager: FileManager

    func exportForecasts(_ request: ForecastExportRequest) async throws -> ExportResult {
        let temporary = try secureTemporaryDirectory(fileManager: fileManager)
        let stream = try await service.exportForecasts(request)
        let destination = temporary.appending(path: request.filename)
        try await writeAtomically(stream: stream, destination: destination)
        return ExportResult(url: destination, checksum: try sha256(destination))
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS' -only-testing:CuspObservatoryTests/OperationsViewsTests`

 Expected: PASS for model status, revoked model, incidents, stale sources, config validation, restart/rollback, destination catalog, secure export, and error tests

 - [ ] **Step 5: Run the Swift/UI subsystem verification command**

 Run: `xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS' -only-testing:CuspObservatoryUITests/OperationsUITests`

 Expected: operational workflows pass and exports are local, explicit, atomic, checksummed, and auditable

 - [ ] **Step 6: Inspect concurrency, stale-state, and numerical-authority boundaries**

 Run: `git diff --check && git status --short`

 Expected: no production probability is recomputed in Swift; no unstructured task escapes actor ownership; every stale/degraded/experimental state is visible and accessible.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add apps/macos/CuspObservatory/Features/ModelLab apps/macos/CuspObservatory/Features/Health apps/macos/CuspObservatory/Features/Settings apps/macos/CuspObservatory/Export apps/macos/CuspObservatoryTests/OperationsViewsTests.swift apps/macos/CuspObservatoryUITests/OperationsUITests.swift
 git commit -m "feat: add model health settings and export workflows"
 ```
### Task 12: Complete accessibility, localization, reduced motion, large-data, and daemon-failure end-to-end QA

 **Files:**
 - Create: `apps/macos/CuspObservatory/Resources/Localizable.xcstrings`
- Create: `apps/macos/CuspObservatory/Accessibility/AccessibilitySummaries.swift`
- Create: `apps/macos/CuspObservatoryUITests/AccessibilityUITests.swift`
- Create: `apps/macos/CuspObservatoryUITests/FailureStateUITests.swift`
- Create: `apps/macos/CuspObservatoryTests/LocalizationTests.swift`
- Create: `docs/operations/macos-user-workflows.md`

 **Interfaces:**
 - Consumes: all Phase 6 views, daemon supervisor, fixture RPC server, and accessibility requirements
 - Produces: complete keyboard/VoiceOver/dynamic type/reduced-motion/localization behavior, accessible chart summaries/tables, large-data performance tests, and safe disconnected/degraded/recovery workflows

 **Implementation notes**

 English is the initial shipped localization, but all user-visible text uses string catalogs. Europe/Warsaw is a display default only; internal UTC model windows and timestamps remain unchanged.

 - [ ] **Step 1: Write the failing test**

 Create or replace `apps/macos/CuspObservatoryUITests/AccessibilityUITests.swift` with:

 ```text
 import XCTest

final class AccessibilityUITests: XCTestCase {
    func testPrimaryNavigationAndRiskValuesAreKeyboardAndVoiceOverAddressable() {
        let app = XCUIApplication()
        app.launchArguments = ["--ui-fixture", "healthy"]
        app.launch()
        XCTAssertTrue(app.outlines["Primary navigation"].exists)
        XCTAssertTrue(app.tables["Market risk screener"].exists)
        XCTAssertTrue(app.staticTexts["BTC downside probability, 4 hours, 31.2 percent, base rate 9.0 percent, data healthy"].exists)
    }

    func testReducedMotionDisablesNonessentialChartAnimation() {
        let app = XCUIApplication()
        app.launchArguments = ["--ui-fixture", "healthy", "--reduce-motion"]
        app.launch()
        XCTAssertEqual(app.otherElements["Forecast curve"].value as? String, "animation disabled")
    }
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS' -only-testing:CuspObservatoryUITests/AccessibilityUITests`

 Expected: FAIL because labels, summaries, localization, and fixtures are incomplete

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `apps/macos/CuspObservatory/Accessibility/AccessibilitySummaries.swift` with:

 ```text
 import Foundation

enum AccessibilitySummaries {
    static func forecast(_ value: ForecastViewState, horizon: ForecastHorizon) -> String {
        guard let point = value.horizon(horizon) else { return "\(value.assetID) forecast unavailable, \(value.availability.accessibleDescription)" }
        return "\(value.assetID) \(value.eventType.accessibleName) probability, \(horizon.accessibleName), \(ProbabilityFormatter.string(point.probability)), base rate \(ProbabilityFormatter.string(point.baseRate)), \(value.quality.accessibleDescription)"
    }

    static func chartTableDescription(title: String, rows: Int) -> String {
        "\(title), chart with accessible data table containing \(rows) rows"
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS' -only-testing:CuspObservatoryUITests/AccessibilityUITests`

 Expected: PASS for keyboard, VoiceOver, chart summaries/tables, reduced motion, large text, localization keys, contrast semantics, and stale/degraded descriptions

 - [ ] **Step 5: Run the Swift/UI subsystem verification command**

 Run: `xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS' && xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS' -only-testing:CuspObservatoryUITests/FailureStateUITests`

 Expected: all unit/UI/failure-state tests pass; app survives daemon kill/source degradation and restores state without showing fabricated values

 - [ ] **Step 6: Inspect concurrency, stale-state, and numerical-authority boundaries**

 Run: `git diff --check && git status --short`

 Expected: no production probability is recomputed in Swift; no unstructured task escapes actor ownership; every stale/degraded/experimental state is visible and accessible.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add apps/macos/CuspObservatory/Resources/Localizable.xcstrings apps/macos/CuspObservatory/Accessibility apps/macos/CuspObservatoryUITests/AccessibilityUITests.swift apps/macos/CuspObservatoryUITests/FailureStateUITests.swift apps/macos/CuspObservatoryTests/LocalizationTests.swift docs/operations/macos-user-workflows.md
 git commit -m "test: complete macOS accessibility and failure QA"
 ```
