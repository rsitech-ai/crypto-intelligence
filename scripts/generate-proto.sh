#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" -gt 1 ]] || { [[ "$#" -eq 1 ]] && [[ "$1" != "--check" ]]; }; then
  printf 'usage: %s [--check]\n' "$0" >&2
  exit 2
fi

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
workspace_root="$(cd -- "${script_dir}/.." && pwd)"

for prerequisite in cargo protoc swift; do
  if ! command -v "${prerequisite}" >/dev/null 2>&1; then
    printf 'error: required protobuf prerequisite is missing: %s\n' "${prerequisite}" >&2
    exit 1
  fi
done

cd -- "${workspace_root}"
cargo run --locked -p xtask -- generate-proto "$@"

swift_version="$(swift --version | head -n 1)"
if [[ "${swift_version}" != *"Apple Swift version 6.3."* ]]; then
  printf 'error: Swift 6.3 is required, found: %s\n' "${swift_version}" >&2
  exit 1
fi

package_dir="${workspace_root}/apps/macos/Packages/TransitionClient"
resolved_file="${package_dir}/Package.resolved"
generated_dir="${package_dir}/Sources/TransitionClient/Generated"
if [[ ! -f "${resolved_file}" ]]; then
  printf 'error: locked Swift dependencies are missing: %s\n' "${resolved_file}" >&2
  exit 1
fi

temporary_root="$(mktemp -d "${TMPDIR:-/tmp}/cmti-swift-proto.XXXXXX")"
trap 'rm -rf -- "${temporary_root}"' EXIT

proto_files=()
while IFS= read -r proto_file; do
  proto_files+=("${proto_file}")
done < <(find "${workspace_root}/proto/cmti" -type f -name '*.proto' -print | LC_ALL=C sort)
if [[ "${#proto_files[@]}" -ne 7 ]]; then
  printf 'error: expected exactly 7 canonical protobuf inputs, found: %s\n' \
    "${#proto_files[@]}" >&2
  exit 1
fi

swift package \
  --package-path "${package_dir}" \
  --allow-writing-to-package-directory \
  --allow-writing-to-directory "${temporary_root}" \
  generate-grpc-code-from-protos \
  --no-servers \
  --access-level public \
  --access-level-on-imports \
  --file-naming pathToUnderscores \
  --import-path "${workspace_root}/proto" \
  --output-path "${temporary_root}" \
  -- \
  "${proto_files[@]}"

if [[ "${1:-}" == "--check" ]]; then
  if ! diff -ruN -- "${generated_dir}" "${temporary_root}"; then
    printf 'error: generated Swift protobuf/gRPC sources are stale\n' >&2
    exit 1
  fi
else
  mkdir -p -- "${generated_dir}"
  rsync -a --delete -- "${temporary_root}/" "${generated_dir}/"
fi
