# Verification debt

The following commands are mandatory before a stable release but may require
tooling unavailable in the current execution environment:

```bash
cargo generate-lockfile
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
cargo deny check
buf lint
scripts/generate-proto.sh --check
xcodebuild test -project apps/macos/CuspObservatory.xcodeproj \
  -scheme CuspObservatory -destination 'platform=macOS'
scripts/run-soak.sh
scripts/package-macos.sh
scripts/notarize-macos.sh <artifact>
scripts/verify-release.sh
```

Additional evidence: live connector schema/reconnect certification, local Bitcoin
and Ethereum node fixtures, Core ML/Core AI parity, Foundation Models extraction,
clean install, interrupted upgrade, rollback, recovery, chaos, signing-key
separation, and sufficient immutable live-shadow events.
