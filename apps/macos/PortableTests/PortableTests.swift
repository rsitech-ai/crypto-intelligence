import Foundation
import Testing
@testable import TransitionClient
@testable import LocalModelHost
@testable import CuspObservatoryCore

private actor FixtureTransport: RPCTransport {
    private let batches: [ForecastBatch]

    init(_ batches: [ForecastBatch]) {
        self.batches = batches
    }

    func negotiate(
        clientVersion: UInt32,
        clientNonce: Data
    ) async throws -> ProtocolInfo {
        #expect(clientNonce.count == 32)
        return ProtocolInfo(
            minimumVersion: 1,
            maximumVersion: 1,
            selectedVersion: clientVersion,
            daemonVersion: "0.1",
            serverNonce: Data(repeating: 1, count: 32)
        )
    }

    func nextForecastBatch(after sequence: UInt64?) async throws -> ForecastBatch? {
        batches.first { sequence == nil || $0.sequence > sequence! }
    }

    func close() async {}
}

private func fixtureForecast(_ asset: String) throws -> TransitionForecast {
    let point = try ProbabilityPoint(
        horizonSeconds: 60,
        probability: ProbabilityPPM(rawValue: 100_000),
        lowerBound: ProbabilityPPM(rawValue: 50_000),
        upperBound: ProbabilityPPM(rawValue: 150_000),
        baseline: ProbabilityPPM(rawValue: 40_000)
    )
    return try TransitionForecast(
        id: UUID(),
        asset: asset,
        eventKind: .liquidityVacuum,
        status: .active,
        curve: ProbabilityCurve(points: [point]),
        qualityPPM: 900_000,
        issuedAtNanos: 1,
        validUntilNanos: 2,
        lineage: ModelLineage(
            packageID: "risk-v1",
            semanticVersion: "1.0.0",
            artifactHash: String(repeating: "a", count: 64),
            featureSchemaHash: String(repeating: "b", count: 64),
            trainingCutoffNanos: 0,
            codeCommit: "abcdef1",
            calibrationID: "c",
            labelDefinitionID: "l"
        ),
        evidence: EvidenceBundle(
            bundleID: "e",
            mechanisms: ["depth loss"],
            canonicalHash: String(repeating: "c", count: 64)
        )
    )
}

@Test
func probabilityRejectsRegression() throws {
    let first = try ProbabilityPoint(
        horizonSeconds: 60,
        probability: ProbabilityPPM(rawValue: 300_000),
        lowerBound: ProbabilityPPM(rawValue: 200_000),
        upperBound: ProbabilityPPM(rawValue: 400_000),
        baseline: ProbabilityPPM(rawValue: 100_000)
    )
    let second = try ProbabilityPoint(
        horizonSeconds: 120,
        probability: ProbabilityPPM(rawValue: 200_000),
        lowerBound: ProbabilityPPM(rawValue: 100_000),
        upperBound: ProbabilityPPM(rawValue: 300_000),
        baseline: ProbabilityPPM(rawValue: 100_000)
    )
    #expect(throws: ProbabilityError.nonMonotonicCurve) {
        try ProbabilityCurve(points: [first, second])
    }
}

@Test
func rpcNegotiates() async throws {
    let client = TransitionRPCClient(
        transport: FixtureTransport([
            ForecastBatch(sequence: 1, forecasts: [try fixtureForecast("BTC")])
        ])
    )
    await #expect(throws: ContractError.protocolNotNegotiated) {
        try await client.nextForecastBatch()
    }
    _ = try await client.negotiate()
    #expect(try await client.nextForecastBatch()?.sequence == 1)
}

@Test
func modelHostMatchesReference() async throws {
    let breaker = try CircuitBreaker(capacity: 4, failureThreshold: 2)
    let host = LocalModelHost(
        primary: ReferenceTemporalProvider(),
        breaker: breaker
    )
    let response = try await host.infer(
        InferenceRequest(
            requestID: UUID(),
            packageID: "m",
            features: [1, 3],
            deadlineMilliseconds: 100
        )
    )
    #expect(response.values == [2])
    #expect(!response.fallbackUsed)
}

@Test
func rendererRejectsUngroundedNumber() throws {
    #expect(throws: ModelHostError.invalidOutput) {
        try EvidenceRenderer().render(
            SemanticDraft(
                summaryTemplate: "{{asset}} risk {{probability}} over {{horizon}} with 99% certainty"
            ),
            evidence: DeterministicEvidence(
                asset: "BTC",
                probability: "31%",
                horizon: "4h"
            )
        )
    }
}

@Test
func replayIsolation() async throws {
    let store = ObservatoryStore()
    await store.ingestLive([try fixtureForecast("BTC")])
    await store.replaceReplay([try fixtureForecast("ETH")])
    await store.setMode(.replay)
    #expect(await store.snapshot().forecasts.map(\.asset) == ["ETH"])
    await store.setMode(.live)
    #expect(await store.snapshot().forecasts.map(\.asset) == ["BTC"])
}
