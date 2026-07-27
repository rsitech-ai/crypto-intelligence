import SwiftUI

struct DataHealthView: View {
  let presentation: MarketHealthPresentation
  let freshnessMillis: UInt64

  var body: some View {
    HStack(spacing: ObservatorySpacing.compact) {
      Circle()
        .fill(color)
        .frame(width: 8, height: 8)
      Text(title)
        .font(.subheadline.weight(.semibold))
      Text("Freshness \(freshnessMillis) ms")
        .font(.subheadline)
        .foregroundStyle(.secondary)
    }
    .accessibilityElement(children: .combine)
    .accessibilityLabel(
      "\(title), freshness \(freshnessMillis) milliseconds"
    )
  }

  private var title: String {
    switch presentation {
    case .healthy:
      "Healthy"
    case .degraded:
      "Degraded"
    case .stale:
      "Stale"
    case .unavailable:
      "Unavailable"
    }
  }

  private var color: Color {
    switch presentation {
    case .healthy:
      ObservatoryColor.healthy
    case .degraded, .stale:
      ObservatoryColor.caution
    case .unavailable:
      ObservatoryColor.unavailable
    }
  }
}
