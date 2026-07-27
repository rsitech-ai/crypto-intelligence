enum DaemonState: String, CaseIterable, Sendable {
  case stopped
  case starting
  case healthy
  case degraded
  case failed
  case stopping
}
