#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
workspace_root="$(cd -- "${script_dir}/.." && pwd)"
project="${workspace_root}/apps/macos/CuspObservatory.xcodeproj"

"${workspace_root}/scripts/generate-proto.sh" --check
"${workspace_root}/scripts/generate-xcode-project.sh" --check

if [[ "$#" -eq 0 ]]; then
  set -- test
fi

exec xcodebuild \
  -quiet \
  -project "${project}" \
  -scheme CuspObservatory \
  -destination "platform=macOS,arch=arm64" \
  -jobs 1 \
  SWIFT_ENABLE_EXPLICIT_MODULES=NO \
  "$@"
