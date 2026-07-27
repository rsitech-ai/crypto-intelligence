import Foundation
import TransitionClient

public enum WorkspaceMode: String, Codable, Sendable {
    case live
    case replay
}

public struct WorkspaceSnapshot: Sendable {
    public let mode: WorkspaceMode
    public let forecasts: [TransitionForecast]

    public init(mode: WorkspaceMode, forecasts: [TransitionForecast]) {
        self.mode = mode
        self.forecasts = forecasts
    }
}

public actor ObservatoryStore {
    private var liveForecasts: [UUID: TransitionForecast] = [:]
    private var replayForecasts: [UUID: TransitionForecast] = [:]
    private var mode: WorkspaceMode = .live

    public init() {}

    public func ingestLive(_ values: [TransitionForecast]) {
        for value in values {
            liveForecasts[value.id] = value
        }
    }

    public func replaceReplay(_ values: [TransitionForecast]) {
        replayForecasts = Dictionary(uniqueKeysWithValues: values.map { ($0.id, $0) })
    }

    public func setMode(_ value: WorkspaceMode) {
        mode = value
    }

    public func snapshot() -> WorkspaceSnapshot {
        let active = mode == .live ? liveForecasts : replayForecasts
        return WorkspaceSnapshot(
            mode: mode,
            forecasts: active.values.sorted { $0.asset < $1.asset }
        )
    }
}
