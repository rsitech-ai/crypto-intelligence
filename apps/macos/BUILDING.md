# Cusp Observatory native build

The checked-in Xcode project is generated from `project.yml`. Validate generated
protobufs, the vendored transport provenance, and the Xcode project before any
native build:

```sh
scripts/generate-proto.sh --check
scripts/generate-xcode-project.sh --check
```

Run macOS tests through the canonical wrapper:

```sh
scripts/test-macos.sh
```

The wrapper passes `SWIFT_ENABLE_EXPLICIT_MODULES=NO` as a global Xcode build
override. The project-level setting alone does not propagate to every external
Swift package target under Xcode 26.6. A current upstream SwiftNIO package
manifest emits one build-system warning for the unused `CNIOWindows` target on
macOS:

```text
DEFINES_MODULE was set, but no umbrella header could be found to generate the module map
```

Any other compiler or build warning is unexpected.
