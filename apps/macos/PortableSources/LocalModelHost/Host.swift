import Foundation

public enum ModelBackend: String, Codable, Sendable {
    case coreAI
    case coreML
    case reference
}

public enum ModelHostError: Error, Equatable, Sendable {
    case invalidInput
    case invalidOutput
    case unknownModel
    case revokedModel
    case deadlineExceeded
    case circuitOpen
    case parityFailure
}

public struct InferenceRequest: Hashable, Codable, Sendable {
    public let requestID: UUID
    public let packageID: String
    public let features: [Float]
    public let deadlineMilliseconds: UInt64

    public init(
        requestID: UUID,
        packageID: String,
        features: [Float],
        deadlineMilliseconds: UInt64
    ) {
        self.requestID = requestID
        self.packageID = packageID
        self.features = features
        self.deadlineMilliseconds = deadlineMilliseconds
    }
}

public struct InferenceResponse: Hashable, Codable, Sendable {
    public let requestID: UUID
    public let packageID: String
    public let values: [Float]
    public let backend: ModelBackend
    public let fallbackUsed: Bool
    public let parityError: Float?
}

public protocol TemporalModelProvider: Sendable {
    var backend: ModelBackend { get }
    func infer(packageID: String, features: [Float]) async throws -> [Float]
}

public struct ReferenceTemporalProvider: TemporalModelProvider {
    public let backend: ModelBackend = .reference

    public init() {}

    public func infer(packageID: String, features: [Float]) async throws -> [Float] {
        guard !packageID.isEmpty,
              !features.isEmpty,
              features.allSatisfy(\.isFinite)
        else { throw ModelHostError.invalidInput }
        return [features.reduce(0, +) / Float(features.count)]
    }
}

public actor CircuitBreaker {
    private let capacity: Int
    private let failureThreshold: Int
    private var outcomes: [Bool] = []
    public private(set) var isOpen = false

    public init(capacity: Int, failureThreshold: Int) throws {
        guard capacity > 0,
              failureThreshold > 0,
              failureThreshold <= capacity
        else { throw ModelHostError.invalidInput }
        self.capacity = capacity
        self.failureThreshold = failureThreshold
    }

    public func record(failed: Bool) {
        outcomes.append(failed)
        if outcomes.count > capacity {
            outcomes.removeFirst(outcomes.count - capacity)
        }
        isOpen = outcomes.filter(\.self).count >= failureThreshold
    }

    public func reset() {
        outcomes.removeAll(keepingCapacity: true)
        isOpen = false
    }
}

public actor LocalModelHost {
    private let primary: any TemporalModelProvider
    private let reference: any TemporalModelProvider
    private let breaker: CircuitBreaker
    private let parityTolerance: Float

    public init(
        primary: any TemporalModelProvider,
        reference: any TemporalModelProvider = ReferenceTemporalProvider(),
        breaker: CircuitBreaker,
        parityTolerance: Float = 1e-4
    ) {
        self.primary = primary
        self.reference = reference
        self.breaker = breaker
        self.parityTolerance = parityTolerance
    }

    public func infer(_ request: InferenceRequest) async throws -> InferenceResponse {
        guard !request.packageID.isEmpty,
              !request.features.isEmpty,
              request.features.allSatisfy(\.isFinite),
              request.deadlineMilliseconds > 0
        else { throw ModelHostError.invalidInput }

        if await breaker.isOpen {
            return try await fallback(request)
        }

        let primary = self.primary
        let reference = self.reference
        do {
            let values = try await withThrowingTaskGroup(of: [Float].self) { group in
                group.addTask {
                    try await primary.infer(
                        packageID: request.packageID,
                        features: request.features
                    )
                }
                group.addTask {
                    try await Task.sleep(
                        for: .milliseconds(request.deadlineMilliseconds)
                    )
                    throw ModelHostError.deadlineExceeded
                }
                guard let first = try await group.next() else {
                    throw ModelHostError.invalidOutput
                }
                group.cancelAll()
                return first
            }

            let referenceValues = try await reference.infer(
                packageID: request.packageID,
                features: request.features
            )
            guard values.count == referenceValues.count,
                  values.allSatisfy(\.isFinite),
                  referenceValues.allSatisfy(\.isFinite)
            else { throw ModelHostError.invalidOutput }

            let parity = zip(values, referenceValues)
                .map { abs($0 - $1) }
                .max() ?? 0
            guard parity <= parityTolerance else {
                throw ModelHostError.parityFailure
            }
            await breaker.record(failed: false)
            return InferenceResponse(
                requestID: request.requestID,
                packageID: request.packageID,
                values: values,
                backend: primary.backend,
                fallbackUsed: false,
                parityError: parity
            )
        } catch {
            await breaker.record(failed: true)
            return try await fallback(request)
        }
    }

    private func fallback(_ request: InferenceRequest) async throws -> InferenceResponse {
        let values = try await reference.infer(
            packageID: request.packageID,
            features: request.features
        )
        guard values.allSatisfy(\.isFinite) else {
            throw ModelHostError.invalidOutput
        }
        return InferenceResponse(
            requestID: request.requestID,
            packageID: request.packageID,
            values: values,
            backend: .reference,
            fallbackUsed: true,
            parityError: nil
        )
    }
}

public struct SemanticDraft: Sendable {
    public let summaryTemplate: String

    public init(summaryTemplate: String) {
        self.summaryTemplate = summaryTemplate
    }
}

public struct DeterministicEvidence: Sendable {
    public let asset: String
    public let probability: String
    public let horizon: String

    public init(asset: String, probability: String, horizon: String) {
        self.asset = asset
        self.probability = probability
        self.horizon = horizon
    }
}

public struct EvidenceRenderer: Sendable {
    public init() {}

    public func render(
        _ draft: SemanticDraft,
        evidence: DeterministicEvidence
    ) throws -> String {
        let output = draft.summaryTemplate
            .replacingOccurrences(of: "{{asset}}", with: evidence.asset)
            .replacingOccurrences(of: "{{probability}}", with: evidence.probability)
            .replacingOccurrences(of: "{{horizon}}", with: evidence.horizon)
        guard !output.contains("{{") else { throw ModelHostError.invalidOutput }

        let allowed = Set(
            numericTokens(evidence.probability) + numericTokens(evidence.horizon)
        )
        let discovered = numericTokens(output)
        guard discovered.allSatisfy({ allowed.contains($0) }) else {
            throw ModelHostError.invalidOutput
        }
        return output
    }

    private func numericTokens(_ value: String) -> [String] {
        value.split(whereSeparator: {
            !$0.isNumber && $0 != "." && $0 != "%"
        })
        .map(String.init)
        .filter { $0.contains(where: \.isNumber) }
    }
}
