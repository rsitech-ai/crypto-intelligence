// swift-tools-version: 6.2

import PackageDescription

let package = Package(
  name: "TransitionClient",
  platforms: [
    .macOS("26.0")
  ],
  products: [
    .library(name: "TransitionClient", targets: ["TransitionClient"])
  ],
  dependencies: [
    .package(
      url: "https://github.com/grpc/grpc-swift-2.git",
      exact: "2.4.2"
    ),
    .package(path: "../../Vendor/grpc-swift-nio-transport"),
    .package(
      url: "https://github.com/grpc/grpc-swift-protobuf.git",
      exact: "2.4.1"
    ),
    .package(
      url: "https://github.com/apple/swift-protobuf.git",
      exact: "1.38.1",
      traits: []
    ),
    .package(
      url: "https://github.com/apple/swift-nio-transport-services.git",
      exact: "1.28.0"
    ),
  ],
  targets: [
    .target(
      name: "TransitionClient",
      dependencies: [
        .product(name: "GRPCCore", package: "grpc-swift-2"),
        .product(
          name: "GRPCNIOTransportHTTP2TransportServices",
          package: "grpc-swift-nio-transport"
        ),
        .product(
          name: "GRPCProtobuf",
          package: "grpc-swift-protobuf"
        ),
        .product(
          name: "NIOTransportServices",
          package: "swift-nio-transport-services"
        ),
        .product(name: "SwiftProtobuf", package: "swift-protobuf"),
      ],
      swiftSettings: [
        .swiftLanguageMode(.v6)
      ]
    ),
    .testTarget(
      name: "TransitionClientTests",
      dependencies: ["TransitionClient"],
      swiftSettings: [
        .swiftLanguageMode(.v6)
      ]
    ),
  ],
  swiftLanguageModes: [.v6]
)
