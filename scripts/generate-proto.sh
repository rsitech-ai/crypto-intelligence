#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" -gt 1 ]] || { [[ "$#" -eq 1 ]] && [[ "$1" != "--check" ]]; }; then
  printf 'usage: %s [--check]\n' "$0" >&2
  exit 2
fi

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
workspace_root="$(cd -- "${script_dir}/.." && pwd)"

for prerequisite in cargo protoc; do
  if ! command -v "${prerequisite}" >/dev/null 2>&1; then
    printf 'error: required protobuf prerequisite is missing: %s\n' "${prerequisite}" >&2
    exit 1
  fi
done

cd -- "${workspace_root}"
exec cargo run --locked -p xtask -- generate-proto "$@"
