#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
workspace_root="$(cd -- "${script_dir}/.." && pwd)"
vendor_rel="vendor/libsqlite3-sys"
vendor="${workspace_root}/${vendor_rel}"
manifest="${vendor}/CMTI_FILES.sha256"
expected_version='3.53.4'
expected_source_id='2026-07-24 19:02:57 bf7c7f30031888f4e796e429ab3978879485813aaca6f641c7b33e4e09459bcc'

if [[ ! -f "${manifest}" ]]; then
  printf 'error: vendored SQLite integrity manifest is missing\n' >&2
  exit 1
fi

(
  cd "${workspace_root}"
  shasum -a 256 -c "${vendor_rel}/CMTI_FILES.sha256" >/dev/null
)

scratch="$(mktemp -d "${TMPDIR:-/tmp}/cmti-sqlite-vendor.XXXXXX")"
trap 'rm -rf -- "${scratch}"' EXIT

(
  cd "${workspace_root}"
  find "${vendor_rel}" -type f ! -name CMTI_FILES.sha256 -print |
    LC_ALL=C sort > "${scratch}/actual-files"
  sed -E 's/^[0-9a-f]{64}  //' "${vendor_rel}/CMTI_FILES.sha256" \
    > "${scratch}/manifest-files"
)

if ! cmp -s "${scratch}/actual-files" "${scratch}/manifest-files"; then
  printf 'error: vendored SQLite scope differs from its integrity manifest\n' >&2
  diff -u "${scratch}/manifest-files" "${scratch}/actual-files" >&2 || true
  exit 1
fi

header="${vendor}/sqlite3/sqlite3.h"
actual_version="$(sed -nE 's/^#define SQLITE_VERSION +\"([^\"]+)\"$/\1/p' "${header}")"
actual_source_id="$(sed -nE 's/^#define SQLITE_SOURCE_ID +\"([^\"]+)\"$/\1/p' "${header}")"

if [[ "${actual_version}" != "${expected_version}" ]]; then
  printf 'error: bundled SQLite version is %s, expected %s\n' \
    "${actual_version}" "${expected_version}" >&2
  exit 1
fi
if [[ "${actual_source_id}" != "${expected_source_id}" ]]; then
  printf 'error: bundled SQLite source ID does not match the audited release\n' >&2
  exit 1
fi

printf 'verify-vendored-sqlite: ok (%s)\n' "${actual_version}"
