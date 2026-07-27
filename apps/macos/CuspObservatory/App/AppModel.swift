import Observation
import TransitionClient

enum AppPhase: Equatable {
  case disconnected
  case loading
  case healthy
  case degraded
  case recovery
}

@MainActor
@Observable
final class AppModel {
  private(set) var phase: AppPhase = .disconnected
  private(set) var overview: MarketOverviewModel?
  private(set) var recoveryMessage: String?

  func reflect(lifecycle: DaemonState) {
    switch lifecycle {
    case .stopped:
      phase = .disconnected
      overview = nil
      recoveryMessage = nil
    case .starting, .stopping:
      phase = .loading
      recoveryMessage = nil
    case .healthy:
      phase = .healthy
    case .degraded:
      phase = .degraded
    case .failed:
      phase = .recovery
      recoveryMessage = "The local market service could not start."
    }
  }

  func apply(snapshot: MarketSnapshot) throws {
    let model = try MarketOverviewModel(snapshot: snapshot)
    overview = model
    recoveryMessage = nil
    phase = model.presentation == .healthy ? .healthy : .degraded
  }
}
