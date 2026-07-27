# Vendored gRPC Swift NIO transport

This directory is the unmodified source archive of
`grpc/grpc-swift-nio-transport` tag `2.9.0`, except for two direct target
dependencies in `Package.swift`.

- Upstream URL: `https://github.com/grpc/grpc-swift-nio-transport`
- Tag: `2.9.0`
- Commit: `2ca31f06658ed288a2560e23ad649acbb3d6b3a3`
- Original `git archive --format=tar 2.9.0` SHA-256:
  `e46175d46ecf6b752c4a90a2a141334cb65a164a715c5a3c7df0293b3b81b53a`
- Original `Package.swift` SHA-256:
  `84fbcf1d06074033d98c710a8277867c805c053242c283d1f6cacfe50d9d438d`

Xcode 26.6 links Swift package products as frameworks and does not infer the
transitive modules imported privately by `GRPCNIOTransportCore`. The local
patch declares `NIOHTTP1` and `NIOTLS` directly on that target. SwiftPM builds
were already green without the correction; the correction is required for the
same pinned source to link under Xcode.

`UPSTREAM_FILES.sha256` covers every upstream file except `Package.swift`.
Run `scripts/verify-vendored-grpc-transport.sh` to validate the exact source
archive and the two-line manifest patch.
