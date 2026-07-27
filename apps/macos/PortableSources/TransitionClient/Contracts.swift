import Foundation

public enum ProbabilityError: Error, Equatable, Sendable {
    case outOfRange
    case invalidInterval
    case nonMonotonicCurve
    case invalidHorizon
}

public struct ProbabilityPPM: Hashable, Codable, Comparable, Sendable {
    public static let scale: UInt32 = 1_000_000
    public let rawValue: UInt32

    public init(rawValue: UInt32) throws {
        guard rawValue <= Self.scale else { throw ProbabilityError.outOfRange }
        self.rawValue = rawValue
    }

    public var displayDouble: Double {
        Double(rawValue) / Double(Self.scale)
    }

    public static func < (lhs: Self, rhs: Self) -> Bool {
        lhs.rawValue < rhs.rawValue
    }
}

public struct ProbabilityPoint: Hashable, Codable, Sendable {
    public let horizonSeconds: UInt64
    public let probability: ProbabilityPPM
    public let lowerBound: ProbabilityPPM
    public let upperBound: ProbabilityPPM
    public let baseline: ProbabilityPPM

    public init(
        horizonSeconds: UInt64,
        probability: ProbabilityPPM,
        lowerBound: ProbabilityPPM,
        upperBound: ProbabilityPPM,
        baseline: ProbabilityPPM
    ) throws {
        guard horizonSeconds > 0 else { throw ProbabilityError.invalidHorizon }
        guard lowerBound <= probability, probability <= upperBound else {
            throw ProbabilityError.invalidInterval
        }
        self.horizonSeconds = horizonSeconds
        self.probability = probability
        self.lowerBound = lowerBound
        self.upperBound = upperBound
        self.baseline = baseline
    }
}

public struct ProbabilityCurve: Hashable, Codable, Sendable {
    public let points: [ProbabilityPoint]

    public init(points: [ProbabilityPoint]) throws {
        guard !points.isEmpty else { throw ProbabilityError.invalidHorizon }
        for pair in zip(points, points.dropFirst()) {
            guard pair.0.horizonSeconds < pair.1.horizonSeconds,
                  pair.0.probability <= pair.1.probability
            else { throw ProbabilityError.nonMonotonicCurve }
        }
        self.points = points
    }
}

public enum TransitionEventKind: String, Codable, CaseIterable, Sendable {
    case downsideTransition
    case upsideSqueeze
    case volatilityExplosion
    case liquidityVacuum
    case liquidationCascade
    case basisDislocation
    case stablecoinDislocation
    case contagion
}

public enum ForecastStatus: String, Codable, Sendable {
    case active
    case abstained
    case experimental
    case suppressed
}

public struct ModelLineage: Hashable, Codable, Sendable {
    public let packageID: String
    public let semanticVersion: String
    public let artifactHash: String
    public let featureSchemaHash: String
    public let trainingCutoffNanos: Int64
    public let codeCommit: String
    public let calibrationID: String
    public let labelDefinitionID: String

    public init(
        packageID: String,
        semanticVersion: String,
        artifactHash: String,
        featureSchemaHash: String,
        trainingCutoffNanos: Int64,
        codeCommit: String,
        calibrationID: String,
        labelDefinitionID: String
    ) throws {
        guard !packageID.isEmpty,
              !semanticVersion.isEmpty,
              Self.isHash(artifactHash),
              Self.isHash(featureSchemaHash),
              codeCommit.count >= 7,
              !calibrationID.isEmpty,
              !labelDefinitionID.isEmpty
        else { throw ProbabilityError.outOfRange }
        self.packageID = packageID
        self.semanticVersion = semanticVersion
        self.artifactHash = artifactHash
        self.featureSchemaHash = featureSchemaHash
        self.trainingCutoffNanos = trainingCutoffNanos
        self.codeCommit = codeCommit
        self.calibrationID = calibrationID
        self.labelDefinitionID = labelDefinitionID
    }

    private static func isHash(_ value: String) -> Bool {
        value.count == 64 && value.utf8.allSatisfy {
            (48 ... 57).contains($0) || (97 ... 102).contains($0)
        }
    }
}

public struct EvidenceBundle: Hashable, Codable, Sendable {
    public let bundleID: String
    public let mechanisms: [String]
    public let canonicalHash: String

    public init(bundleID: String, mechanisms: [String], canonicalHash: String) {
        self.bundleID = bundleID
        self.mechanisms = mechanisms
        self.canonicalHash = canonicalHash
    }
}

public struct TransitionForecast: Hashable, Codable, Sendable, Identifiable {
    public let id: UUID
    public let asset: String
    public let eventKind: TransitionEventKind
    public let status: ForecastStatus
    public let curve: ProbabilityCurve
    public let qualityPPM: UInt32
    public let issuedAtNanos: Int64
    public let validUntilNanos: Int64
    public let lineage: ModelLineage
    public let evidence: EvidenceBundle

    public init(
        id: UUID,
        asset: String,
        eventKind: TransitionEventKind,
        status: ForecastStatus,
        curve: ProbabilityCurve,
        qualityPPM: UInt32,
        issuedAtNanos: Int64,
        validUntilNanos: Int64,
        lineage: ModelLineage,
        evidence: EvidenceBundle
    ) throws {
        guard !asset.isEmpty,
              qualityPPM <= ProbabilityPPM.scale,
              issuedAtNanos < validUntilNanos
        else { throw ProbabilityError.outOfRange }
        self.id = id
        self.asset = asset
        self.eventKind = eventKind
        self.status = status
        self.curve = curve
        self.qualityPPM = qualityPPM
        self.issuedAtNanos = issuedAtNanos
        self.validUntilNanos = validUntilNanos
        self.lineage = lineage
        self.evidence = evidence
    }
}

public enum ContractError: Error, Equatable, Sendable {
    case protocolNotNegotiated
    case sequenceRegression
    case invalidEndpoint
}

public struct ProtocolInfo: Hashable, Codable, Sendable {
    public let minimumVersion: UInt32
    public let maximumVersion: UInt32
    public let selectedVersion: UInt32
    public let daemonVersion: String
    public let serverNonce: Data

    public init(
        minimumVersion: UInt32,
        maximumVersion: UInt32,
        selectedVersion: UInt32,
        daemonVersion: String,
        serverNonce: Data
    ) {
        self.minimumVersion = minimumVersion
        self.maximumVersion = maximumVersion
        self.selectedVersion = selectedVersion
        self.daemonVersion = daemonVersion
        self.serverNonce = serverNonce
    }
}

public struct ForecastBatch: Hashable, Codable, Sendable {
    public let sequence: UInt64
    public let forecasts: [TransitionForecast]

    public init(sequence: UInt64, forecasts: [TransitionForecast]) {
        self.sequence = sequence
        self.forecasts = forecasts
    }
}

public protocol RPCTransport: Sendable {
    func negotiate(clientVersion: UInt32, clientNonce: Data) async throws -> ProtocolInfo
    func nextForecastBatch(after sequence: UInt64?) async throws -> ForecastBatch?
    func close() async
}

public actor TransitionRPCClient {
    private let transport: any RPCTransport
    private var selectedProtocol: UInt32?
    private var lastSequence: UInt64?

    public init(transport: any RPCTransport) {
        self.transport = transport
    }

    public func negotiate(clientVersion: UInt32 = 1) async throws -> ProtocolInfo {
        var nonce = Data(count: 32)
        for index in nonce.indices {
            nonce[index] = UInt8.random(in: .min ... .max)
        }
        let info = try await transport.negotiate(
            clientVersion: clientVersion,
            clientNonce: nonce
        )
        guard info.minimumVersion <= info.selectedVersion,
              info.selectedVersion <= info.maximumVersion,
              info.selectedVersion == clientVersion,
              info.serverNonce.count == 32
        else { throw ContractError.protocolNotNegotiated }
        selectedProtocol = info.selectedVersion
        return info
    }

    public func nextForecastBatch() async throws -> ForecastBatch? {
        guard selectedProtocol != nil else {
            throw ContractError.protocolNotNegotiated
        }
        guard let batch = try await transport.nextForecastBatch(after: lastSequence)
        else { return nil }
        if let previous = lastSequence, batch.sequence <= previous {
            throw ContractError.sequenceRegression
        }
        lastSequence = batch.sequence
        return batch
    }

    public func close() async {
        await transport.close()
    }
}
