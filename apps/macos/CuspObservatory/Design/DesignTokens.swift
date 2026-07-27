import SwiftUI

enum ObservatorySpacing {
  static let compact: CGFloat = 8
  static let standard: CGFloat = 16
  static let section: CGFloat = 24
  static let page: CGFloat = 28
}

enum ObservatoryColor {
  static let healthy = Color(
    red: 0.20,
    green: 0.65,
    blue: 0.44
  )
  static let caution = Color(
    red: 0.88,
    green: 0.58,
    blue: 0.20
  )
  static let unavailable = Color.secondary
}

struct ObservatoryPanel: ViewModifier {
  func body(content: Content) -> some View {
    content
      .padding(ObservatorySpacing.section)
      .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 14))
      .overlay {
        RoundedRectangle(cornerRadius: 14)
          .stroke(.separator.opacity(0.45), lineWidth: 1)
      }
  }
}

extension View {
  func observatoryPanel() -> some View {
    modifier(ObservatoryPanel())
  }
}
