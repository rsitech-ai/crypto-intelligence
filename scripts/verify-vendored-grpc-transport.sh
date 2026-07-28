#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
workspace_root="$(cd -- "${script_dir}/.." && pwd)"
vendor_rel="apps/macos/Vendor/grpc-swift-nio-transport"
vendor="${workspace_root}/${vendor_rel}"
manifest="${vendor}/UPSTREAM_FILES.sha256"
package="${vendor}/Package.swift"
original_package_sha="84fbcf1d06074033d98c710a8277867c805c053242c283d1f6cacfe50d9d438d"
niohttp1_line='      .product(name: "NIOHTTP1", package: "swift-nio"),'
niotls_line='      .product(name: "NIOTLS", package: "swift-nio"),'

if [[ ! -f "${manifest}" ]] || [[ ! -f "${package}" ]]; then
  printf 'error: vendored gRPC transport provenance files are missing\n' >&2
  exit 1
fi

(
  cd "${workspace_root}"
  shasum -a 256 -c "${vendor_rel}/UPSTREAM_FILES.sha256" >/dev/null
)

scratch="$(mktemp -d "${TMPDIR:-/tmp}/cmti-grpc-vendor.XXXXXX")"
trap 'rm -rf -- "${scratch}"' EXIT
sed \
  -e '\|^      \.product(name: "NIOHTTP1", package: "swift-nio"),$|d' \
  -e '\|^      \.product(name: "NIOTLS", package: "swift-nio"),$|d' \
  "${package}" > "${scratch}/Package.swift"

actual_package_sha="$(shasum -a 256 "${scratch}/Package.swift" | awk '{print $1}')"
if [[ "${actual_package_sha}" != "${original_package_sha}" ]]; then
  printf 'error: vendored Package.swift differs beyond the approved patch\n' >&2
  exit 1
fi

for approved_line in "${niohttp1_line}" "${niotls_line}"; do
  exact_count="$(grep -Fxc -- "${approved_line}" "${package}" || true)"
  containing_count="$(grep -Fc -- "${approved_line#"${approved_line%%[! ]*}"}" "${package}" || true)"
  if [[ "${exact_count}" -ne 1 ]] || [[ "${containing_count}" -ne 1 ]]; then
    printf 'error: approved compatibility edge is not one exact canonical line: %s\n' \
      "${approved_line}" >&2
    exit 1
  fi
done
