#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" -gt 1 ]] || { [[ "$#" -eq 1 ]] && [[ "$1" != "--check" ]]; }; then
  printf 'usage: %s [--check]\n' "$0" >&2
  exit 2
fi

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
workspace_root="$(cd -- "${script_dir}/.." && pwd)"
macos_root="${workspace_root}/apps/macos"
spec="${macos_root}/project.yml"
project="${macos_root}/CuspObservatory.xcodeproj"

for prerequisite in xcodegen swift; do
  if ! command -v "${prerequisite}" >/dev/null 2>&1; then
    printf 'error: required Xcode project prerequisite is missing: %s\n' \
      "${prerequisite}" >&2
    exit 1
  fi
done

if [[ "$(xcodegen --version)" != "Version: 2.45.4" ]]; then
  printf 'error: XcodeGen 2.45.4 is required\n' >&2
  exit 1
fi
swift_version="$(swift --version | head -n 1)"
if [[ "${swift_version}" != *"Apple Swift version 6.3."* ]]; then
  printf 'error: Swift 6.3 is required, found: %s\n' "${swift_version}" >&2
  exit 1
fi
if [[ ! -x "${workspace_root}/target/debug/cryptoriskd" ]]; then
  printf 'error: built Task 4 daemon is missing; run cargo build -p cryptoriskd\n' >&2
  exit 1
fi
"${workspace_root}/scripts/verify-vendored-grpc-transport.sh"

if ! grep -q 'SWIFT_ENABLE_EXPLICIT_MODULES: false' "${spec}"; then
  printf 'error: Xcode 26 Swift package compatibility setting is missing\n' >&2
  exit 1
fi
if ! grep -q 'SWIFT_ENABLE_EXPLICIT_MODULES=NO' \
  "${workspace_root}/scripts/test-macos.sh"; then
  printf 'error: canonical macOS test command must disable explicit modules globally\n' >&2
  exit 1
fi

backup_root="$(mktemp -d "${TMPDIR:-/tmp}/cmti-xcode-project.XXXXXX")"
trap 'rm -rf -- "${backup_root}"' EXIT
if [[ -d "${project}" ]]; then
  cp -R -- "${project}" "${backup_root}/CuspObservatory.xcodeproj"
fi

xcodegen generate \
  --no-env \
  --quiet \
  --spec "${spec}" \
  --project "${macos_root}" \
  --project-root "${macos_root}"

if [[ "${1:-}" == "--check" ]]; then
  status=0
  if ! diff -ruN -- \
    "${backup_root}/CuspObservatory.xcodeproj" \
    "${project}"; then
    status=1
  fi
  rsync -a --delete -- \
    "${backup_root}/CuspObservatory.xcodeproj/" \
    "${project}/"
  if [[ "${status}" -ne 0 ]]; then
    printf 'error: generated Xcode project is stale\n' >&2
    exit "${status}"
  fi
fi
