import SwiftUI

struct MarketOverviewView: View {
  let model: AppModel
  let retry: () -> Void

  var body: some View {
    NavigationSplitView {
      sidebar
    } detail: {
      VStack(spacing: 0) {
        header
        Divider()
        content
      }
      .background(Color(nsColor: .windowBackgroundColor))
    }
  }

  private var sidebar: some View {
    List {
      Label("Market overview", systemImage: "waveform.path.ecg")
        .fontWeight(.semibold)
      Section("Runtime") {
        Label("Local daemon", systemImage: "desktopcomputer")
        Label("Offline fixture", systemImage: "externaldrive")
      }
    }
    .navigationSplitViewColumnWidth(min: 188, ideal: 210, max: 240)
  }

  private var header: some View {
    HStack(alignment: .firstTextBaseline) {
      VStack(alignment: .leading, spacing: 3) {
        Text("Market overview")
          .font(.title2.weight(.semibold))
        Text("Supervised, local-only market state")
          .font(.subheadline)
          .foregroundStyle(.secondary)
      }
      Spacer()
      phaseLabel
    }
    .padding(.horizontal, ObservatorySpacing.page)
    .padding(.vertical, ObservatorySpacing.standard)
  }

  @ViewBuilder
  private var phaseLabel: some View {
    switch model.phase {
    case .healthy:
      Label("Connected", systemImage: "checkmark.circle.fill")
        .foregroundStyle(ObservatoryColor.healthy)
    case .degraded:
      Label("Attention", systemImage: "exclamationmark.triangle.fill")
        .foregroundStyle(ObservatoryColor.caution)
    case .loading:
      Label("Starting", systemImage: "clock")
        .foregroundStyle(.secondary)
    case .disconnected:
      Label("Disconnected", systemImage: "circle.dashed")
        .foregroundStyle(.secondary)
    case .recovery:
      Label("Recovery needed", systemImage: "exclamationmark.circle.fill")
        .foregroundStyle(.red)
    }
  }

  @ViewBuilder
  private var content: some View {
    switch model.phase {
    case .disconnected:
      stateView(
        icon: "bolt.horizontal.circle",
        title: "Local service disconnected",
        message: "The market service is not running."
      )
    case .loading:
      VStack(spacing: ObservatorySpacing.standard) {
        ProgressView()
          .controlSize(.large)
        Text("Starting local market service…")
          .font(.headline)
        Text("Validating the session and loading the fixture.")
          .foregroundStyle(.secondary)
      }
      .frame(maxWidth: .infinity, maxHeight: .infinity)
      .accessibilityElement(children: .combine)
      .accessibilityLabel("Starting local market service")
    case .healthy, .degraded:
      if let overview = model.overview {
        overviewContent(overview)
      } else {
        stateView(
          icon: "ellipsis.circle",
          title: "Snapshot unavailable",
          message: "Waiting for validated local market data."
        )
      }
    case .recovery:
      recoveryContent
    }
  }

  private func overviewContent(
    _ overview: MarketOverviewModel
  ) -> some View {
    ScrollView {
      VStack(alignment: .leading, spacing: ObservatorySpacing.section) {
        HStack {
          Text("Markets")
            .font(.headline)
          Spacer()
          DataHealthView(
            presentation: overview.presentation,
            freshnessMillis: overview.freshnessMillis
          )
        }

        VStack(spacing: 0) {
          HStack {
            columnLabel("Instrument")
              .frame(maxWidth: .infinity, alignment: .leading)
            columnLabel("Best bid")
              .frame(width: 128, alignment: .trailing)
            columnLabel("Best ask")
              .frame(width: 128, alignment: .trailing)
            columnLabel("Sequence")
              .frame(width: 110, alignment: .trailing)
          }
          .padding(.horizontal, ObservatorySpacing.standard)
          .padding(.bottom, ObservatorySpacing.compact)

          Divider()

          HStack(spacing: ObservatorySpacing.standard) {
            VStack(alignment: .leading, spacing: 4) {
              Text(overview.symbol)
                .font(.body.weight(.semibold))
              Text(overview.source)
                .font(.caption)
                .foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, alignment: .leading)

            Text(overview.bestBid)
              .monospacedDigit()
              .frame(width: 128, alignment: .trailing)
            Text(overview.bestAsk)
              .monospacedDigit()
              .frame(width: 128, alignment: .trailing)
            Text("Sequence \(overview.sequence)")
              .font(.callout)
              .monospacedDigit()
              .frame(width: 110, alignment: .trailing)
          }
          .padding(ObservatorySpacing.standard)
          .accessibilityElement(children: .contain)
          .accessibilityIdentifier(
            "market-overview-row-\(overview.symbol)"
          )
        }
        .observatoryPanel()

        Color.clear
          .frame(width: 1, height: 1)
          .accessibilityElement(children: .ignore)
          .accessibilityIdentifier("market-overview-summary")
          .accessibilityLabel(overview.accessibilitySummary)

        Text(
          "Values are presented exactly as validated by the local daemon. The app does not derive market health or prices."
        )
        .font(.caption)
        .foregroundStyle(.secondary)
      }
      .padding(ObservatorySpacing.page)
    }
  }

  private var recoveryContent: some View {
    let message =
      model.recoveryMessage
      ?? "The local market service could not start."
    return VStack(spacing: ObservatorySpacing.standard) {
      Image(systemName: "exclamationmark.triangle")
        .font(.system(size: 34, weight: .medium))
        .foregroundStyle(ObservatoryColor.caution)
      Text("Local service unavailable")
        .font(.title3.weight(.semibold))
      Text(message)
        .foregroundStyle(.secondary)
        .multilineTextAlignment(.center)
        .fixedSize(horizontal: false, vertical: true)
        .frame(maxWidth: 420)
        .accessibilityIdentifier("recovery-message")
        .accessibilityLabel(message)
      Button("Retry", action: retry)
        .buttonStyle(.borderedProminent)
        .accessibilityIdentifier("retry-button")
    }
    .frame(maxWidth: .infinity, maxHeight: .infinity)
    .accessibilityElement(children: .contain)
    .accessibilityIdentifier("recovery-state")
  }

  private func stateView(
    icon: String,
    title: String,
    message: String
  ) -> some View {
    VStack(spacing: ObservatorySpacing.standard) {
      Image(systemName: icon)
        .font(.system(size: 34))
        .foregroundStyle(.secondary)
      Text(title)
        .font(.title3.weight(.semibold))
      Text(message)
        .foregroundStyle(.secondary)
    }
    .frame(maxWidth: .infinity, maxHeight: .infinity)
    .accessibilityElement(children: .combine)
  }

  private func columnLabel(_ title: String) -> some View {
    Text(title.uppercased())
      .font(.caption2.weight(.semibold))
      .foregroundStyle(.secondary)
  }
}
