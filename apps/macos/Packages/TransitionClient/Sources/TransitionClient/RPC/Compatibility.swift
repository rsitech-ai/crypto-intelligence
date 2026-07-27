public enum ProtocolCompatibility {
  public static let supportedMajor: UInt32 = 1
  public static let maximumMinor: UInt32 = 0

  public static func validate(_ descriptor: ReadinessDescriptor) throws {
    guard descriptor.protocolMajor == supportedMajor,
      descriptor.protocolMinor <= maximumMinor
    else {
      throw ClientContractError.incompatibleProtocol
    }
  }
}
