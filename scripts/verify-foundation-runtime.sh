#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly script_dir
workspace_root="$(cd -- "${script_dir}/.." && pwd)"
readonly workspace_root
readonly target_readiness_label="runtime-proven foundation slice"
evidence_output="${workspace_root}/release/evidence/foundation-runtime-verification.json"
prior_evidence=""

usage() {
  printf 'usage: %s [--evidence-output PATH] [--prior-evidence PATH]\n' "$0"
}

while [[ "$#" -gt 0 ]]; do
  if [[ "$#" -lt 2 ]]; then
    usage >&2
    exit 2
  fi
  case "$1" in
    --evidence-output)
      evidence_output="$2"
      ;;
    --prior-evidence)
      prior_evidence="$2"
      ;;
    *)
      usage >&2
      exit 2
      ;;
  esac
  shift 2
done

cd -- "${workspace_root}"
if [[ -n "$(git status --porcelain=v1 --untracked-files=all)" ]]; then
  printf 'error: foundation verification requires an exact clean commit\n' >&2
  git status --short >&2
  exit 1
fi

verified_commit="$(git rev-parse HEAD)"
readonly verified_commit
temporary_root="$(mktemp -d "${TMPDIR:-/tmp}/cmti-foundation-runtime.XXXXXX")"
readonly temporary_root
readonly command_ledger="${temporary_root}/commands.tsv"
readonly tool_versions="${temporary_root}/tool-versions.tsv"
readonly blocker_file="${temporary_root}/blockers.txt"
readonly artifact_hashes="${temporary_root}/artifact-hashes.tsv"
active_pid=""
active_app_pid=""
active_app_daemon_pid=""
active_app_watcher_pid=""
active_app_topology_observed=0
app_runtime_roots=()
gate_status=0

process_is_live() {
  local pid="$1"
  local state
  state="$(ps -p "${pid}" -o stat= 2>/dev/null | awk '{$1=$1; print}' || true)"
  [[ -n "${state}" ]] && [[ "${state}" != Z* ]]
}

stop_owned_descendant() {
  local pid="$1"
  if [[ -z "${pid}" ]] || ! process_is_live "${pid}"; then
    return 0
  fi
  kill -TERM "${pid}" 2>/dev/null || true
  for _ in {1..40}; do
    if ! process_is_live "${pid}"; then
      return 0
    fi
    sleep 0.05
  done
  kill -KILL "${pid}" 2>/dev/null || true
  for _ in {1..40}; do
    if ! process_is_live "${pid}"; then
      return 0
    fi
    sleep 0.05
  done
  return 124
}

cleanup_generated_app_roots() {
  local generated_root
  for generated_root in "${app_runtime_roots[@]}"; do
    if [[ "${generated_root}" == */CuspObservatoryTests/Task6* ]]; then
      rm -rf -- "${generated_root}"
    fi
  done
}

cleanup() {
  local cleanup_status=$?
  trap - EXIT
  if [[ -n "${active_pid}" ]] && process_is_live "${active_pid}"; then
    kill -TERM "${active_pid}" 2>/dev/null || true
    for _ in {1..20}; do
      if ! process_is_live "${active_pid}"; then
        break
      fi
      sleep 0.1
    done
    if process_is_live "${active_pid}"; then
      kill -KILL "${active_pid}" 2>/dev/null || true
    fi
    wait "${active_pid}" 2>/dev/null || true
  fi
  if [[ -n "${active_app_pid}" ]] && process_is_live "${active_app_pid}"; then
    kill -KILL "${active_app_pid}" 2>/dev/null || true
    wait "${active_app_pid}" 2>/dev/null || true
  fi
  stop_owned_descendant "${active_app_watcher_pid}" || true
  stop_owned_descendant "${active_app_daemon_pid}" || true
  cleanup_generated_app_roots
  rm -rf -- "${temporary_root}"
  exit "${cleanup_status}"
}

exit_for_signal() {
  local status="$1"
  trap - INT TERM HUP
  exit "${status}"
}

trap cleanup EXIT
trap 'exit_for_signal 130' INT
trap 'exit_for_signal 143' TERM
trap 'exit_for_signal 129' HUP
git status --porcelain=v1 --untracked-files=all \
  > "${temporary_root}/start-tree-status.txt"

record_blocker() {
  printf '%s\n' "$1" >> "${blocker_file}"
  gate_status=1
}

require_tool() {
  if ! command -v "$1" >/dev/null 2>&1; then
    printf 'error: required verification tool is missing: %s\n' "$1" >&2
    exit 1
  fi
}

for tool in \
  awk bash basename buf cargo cat chmod cp date dd DevToolsSecurity find git head lsof mkdir \
  mktemp pgrep protoc ps python3 rg rm rustc rustup sed shasum shellcheck sleep stat swift \
  tail tr wc xcodebuild xcodegen xcrun
do
  require_tool "${tool}"
done

watcher_processes() {
  ps -axo pid=,ppid=,command= | rg '[c]mti-daemon'
}

if pgrep -x cryptoriskd >/dev/null 2>&1 \
  || pgrep -x CuspObservatory >/dev/null 2>&1 \
  || watcher_processes >/dev/null 2>&1
then
  printf 'error: foundation verification requires clean app, daemon, and watcher process state\n' >&2
  exit 1
fi

run_bounded() {
  local timeout_seconds="$1"
  shift
  "${workspace_root}/scripts/run-bounded.py" "${timeout_seconds}" "$@"
}

shell_join() {
  local result=""
  local quoted
  for argument in "$@"; do
    printf -v quoted '%q' "${argument}"
    result+="${result:+ }${quoted}"
  done
  printf '%s' "${result}"
}

run_gate() {
  local label="$1"
  local timeout_seconds="$2"
  shift 2
  local output="${temporary_root}/${label}.log"
  local command
  local started
  local ended
  local status
  command="$(shell_join "$@")"
  started="$(python3 -c 'import time; print(time.time_ns())')"
  set +e
  run_bounded "${timeout_seconds}" "$@" >"${output}" 2>&1
  status=$?
  set -e
  ended="$(python3 -c 'import time; print(time.time_ns())')"
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
    "${label}" "${status}" "${started}" "${ended}" \
    "$(shasum -a 256 "${output}" | awk '{print $1}')" "${command}" \
    >> "${command_ledger}"
  if [[ "${status}" -ne 0 ]]; then
    printf 'error: gate failed: %s (exit %s)\n' "${label}" "${status}" >&2
    tail -n 20 "${output}" >&2 || true
    record_blocker "${label}:exit-${status}"
  fi
  return 0
}

inspect_native_warnings() {
  local label="$1"
  local log_file="${temporary_root}/${label}.log"
  local warning_file="${temporary_root}/${label}-warnings.txt"
  if ! rg -n 'warning:' "${log_file}" > "${warning_file}"; then
    rm -f -- "${warning_file}"
    return 0
  fi
  if rg -v \
    "DEFINES_MODULE was set, but no umbrella header could be found to generate the module map \\(in target 'CNIOWindows' from project 'swift-nio'\\)" \
    "${warning_file}" > "${temporary_root}/${label}-unexpected-warnings.txt"
  then
    record_blocker "${label}:unexpected-warning"
  fi
  if rg -q \
    "DEFINES_MODULE was set, but no umbrella header could be found to generate the module map \\(in target 'CNIOWindows' from project 'swift-nio'\\)" \
    "${warning_file}"
  then
    record_blocker "external:swift-nio-cniowindows-warning"
  fi
}

record_version() {
  local label="$1"
  shift
  local output
  output="$("$@" 2>&1 | tr '\n' ';' | sed 's/;$//')"
  printf '%s\t%s\n' "${label}" "${output}" >> "${tool_versions}"
}

record_version bash bash --version
record_version buf buf --version
record_version cargo cargo --version
developer_mode_status="$(DevToolsSecurity -status 2>&1)"
printf '%s\t%s\n' developer-mode "${developer_mode_status}" >> "${tool_versions}"
record_version git git --version
record_version protoc protoc --version
record_version rustc rustc --version
record_version rustc-msrv rustc +1.88.0 --version
record_version shellcheck shellcheck --version
record_version swift swift --version
record_version xcodebuild xcodebuild -version
record_version xcodegen xcodegen --version

run_gate cargo-build 300 cargo build --workspace --all-targets --all-features --locked
run_gate cargo-msrv 300 cargo +1.88.0 check \
  --workspace --all-targets --all-features --locked --offline
run_gate cargo-format 120 cargo fmt --all -- --check
run_gate cargo-clippy 300 cargo clippy \
  --workspace --all-targets --all-features --locked -- -D warnings
run_gate cargo-tests 600 cargo test \
  --workspace --all-targets --all-features --locked
run_gate verifier-shellcheck 120 shellcheck scripts/verify-foundation-runtime.sh
run_gate buf-lint 120 buf lint proto
run_gate buf-build 120 buf build proto
run_gate proto-generation 300 scripts/generate-proto.sh --check
run_gate config-generation 180 cargo run --locked -p xtask -- generate-config-schema --check
run_gate xcode-generation 180 scripts/generate-xcode-project.sh --check
run_gate swift-package-tests 600 swift test --package-path apps/macos/Packages/TransitionClient

readonly runtime_root="${temporary_root}/runtime"
mkdir -p -- "${runtime_root}/data" "${runtime_root}/logs"
cp -- fixtures/binance/btcusdt-book-v1.jsonl "${runtime_root}/fixture.jsonl"
cat > "${runtime_root}/config.toml" <<'EOF'
schema_version = 1
data_root = "./data"
log_root = "./logs"
fixture_input = "./fixture.jsonl"
bind_address = "127.0.0.1:0"
session_secret_fd = 3
ingestion_queue_capacity = 1024
maximum_request_bytes = 8388608
maximum_concurrent_requests = 128
request_timeout_seconds = 30
shutdown_grace_seconds = 5
remote_export = false
remote_telemetry = false
EOF

wait_for_readiness() {
  local pid="$1"
  local stdout_file="$2"
  for _ in {1..100}; do
    if [[ -s "${stdout_file}" ]]; then
      set +e
      python3 - "${stdout_file}" "${stdout_file}.readiness" <<'PY'
import json
import pathlib
import sys

source = pathlib.Path(sys.argv[1]).read_bytes()
if b"\n" not in source:
    raise SystemExit(1)
line, _ = source.split(b"\n", 1)
try:
    readiness = json.loads(line)
except ValueError:
    raise SystemExit(2)
required = {
    "endpoint",
    "protocol_major",
    "protocol_minor",
    "daemon_pid",
    "process_nonce",
    "server_nonce",
    "issued_unix_seconds",
    "expiry_unix_seconds",
}
if set(readiness) != required:
    raise SystemExit(2)
pathlib.Path(sys.argv[2]).write_bytes(line + b"\n")
PY
      readiness_status=$?
      set -e
      if [[ "${readiness_status}" -eq 0 ]]; then
        return 0
      fi
      if [[ "${readiness_status}" -eq 2 ]]; then
        return 2
      fi
    fi
    if ! process_is_live "${pid}"; then
      return 1
    fi
    sleep 0.05
  done
  return 124
}

abort_direct_runtime() {
  local secret_file="$1"
  local pid="${active_pid}"
  if [[ -n "${pid}" ]] && process_is_live "${pid}"; then
    kill -TERM "${pid}" 2>/dev/null || true
    set +e
    wait_for_exit "${pid}" 100
    abort_status=$?
    set -e
    if [[ "${abort_status}" -eq 124 ]] && process_is_live "${pid}"; then
      kill -KILL "${pid}" 2>/dev/null || true
      wait "${pid}" 2>/dev/null || true
    fi
  elif [[ -n "${pid}" ]]; then
    wait "${pid}" 2>/dev/null || true
  fi
  active_pid=""
  rm -f -- "${secret_file}"
}

wait_for_exit() {
  local pid="$1"
  local timeout_ticks="$2"
  local tick
  for ((tick = 0; tick < timeout_ticks; tick += 1)); do
    if ! process_is_live "${pid}"; then
      wait "${pid}"
      return $?
    fi
    sleep 0.05
  done
  return 124
}

scan_runtime_material() {
  local secret_file="$1"
  local excluded_file="$2"
  shift 2
  python3 - "${secret_file}" "${excluded_file}" "$@" <<'PY'
import base64
import json
import pathlib
import sys

secret_path = pathlib.Path(sys.argv[1])
excluded = pathlib.Path(sys.argv[2]).resolve()
secret = secret_path.read_bytes()
patterns = {
    "raw": secret,
    "hex": secret.hex().encode(),
    "base64": base64.b64encode(secret),
}
violations = []
for raw_path in sys.argv[3:]:
    path = pathlib.Path(raw_path)
    if not path.exists():
        violations.append({"path": str(path), "kind": "missing"})
        continue
    candidates = [path] if path.is_file() else [item for item in path.rglob("*") if item.is_file()]
    for candidate in candidates:
        if candidate.resolve() in {secret_path.resolve(), excluded}:
            continue
        data = candidate.read_bytes()
        for kind, pattern in patterns.items():
            if pattern and pattern in data:
                violations.append({"path": str(candidate), "kind": kind})
print(json.dumps({"secret_material_findings": violations}, sort_keys=True))
raise SystemExit(1 if violations else 0)
PY
}

runtime_records="${temporary_root}/runtime-records.jsonl"
first_digest=""
first_wal_size=""
first_wal_hash=""
for run_number in 1 2; do
  secret_file="${temporary_root}/session-secret-${run_number}.bin"
  dd if=/dev/urandom of="${secret_file}" bs=32 count=1 status=none
  chmod 600 "${secret_file}"
  stdout_file="${temporary_root}/daemon-${run_number}.stdout"
  stderr_file="${temporary_root}/daemon-${run_number}.stderr"
  : > "${stdout_file}"
  : > "${stderr_file}"
  target/debug/cryptoriskd \
    --approved-root "${runtime_root}" \
    --config "${runtime_root}/config.toml" \
    3<"${secret_file}" >"${stdout_file}" 2>"${stderr_file}" &
  active_pid=$!
  if ! wait_for_readiness "${active_pid}" "${stdout_file}"; then
    record_blocker "runtime-${run_number}:readiness"
    abort_direct_runtime "${secret_file}"
    break
  fi

  ps -p "${active_pid}" -o pid=,ppid=,command= > "${temporary_root}/daemon-${run_number}.process"
  lsof -a -p "${active_pid}" -Fn > "${temporary_root}/daemon-${run_number}.lsof"
  set +e
  run_bounded 10 target/debug/foundation-runtime-probe \
    --readiness "${stdout_file}.readiness" \
    --secret "${secret_file}" \
    --mode healthy > "${temporary_root}/snapshot-${run_number}.json" \
    2> "${temporary_root}/snapshot-${run_number}.stderr"
  probe_status=$?
  set -e
  if [[ "${probe_status}" -ne 0 ]]; then
    record_blocker "runtime-${run_number}:authenticated-probe"
    abort_direct_runtime "${secret_file}"
    break
  fi
  digest="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["digest"])' \
    "${temporary_root}/snapshot-${run_number}.json")"
  if [[ "${run_number}" -eq 1 ]]; then
    first_digest="${digest}"
    set +e
    run_bounded 10 target/debug/foundation-runtime-probe \
      --readiness "${stdout_file}.readiness" \
      --secret "${secret_file}" \
      --mode unauthenticated > "${temporary_root}/authentication-failure.json" \
      2> "${temporary_root}/authentication-failure.stderr"
    auth_status=$?
    set -e
    if [[ "${auth_status}" -ne 0 ]]; then
      record_blocker "runtime-authentication-failure-probe"
    fi
  elif [[ "${digest}" != "${first_digest}" ]]; then
    record_blocker "runtime-digest-mismatch"
  fi

  set +e
  scan_runtime_material \
    "${secret_file}" "${secret_file}" \
    "${stdout_file}" "${stderr_file}" "${stdout_file}.readiness" \
    "${temporary_root}/snapshot-${run_number}.json" \
    "${temporary_root}/snapshot-${run_number}.stderr" \
    "${temporary_root}/daemon-${run_number}.process" \
    "${temporary_root}/daemon-${run_number}.lsof" \
    "${runtime_root}" > "${temporary_root}/secret-scan-${run_number}.json"
  secret_scan_status=$?
  set -e
  if [[ "${secret_scan_status}" -ne 0 ]]; then
    record_blocker "runtime-${run_number}:secret-material"
  fi

  if [[ -s "${stderr_file}" ]] \
    || rg -n '"level":"(error|warn)"|panic|crash|hang' \
      "${runtime_root}/logs/cmti.jsonl" "${stdout_file}" "${stderr_file}" \
      >/dev/null 2>&1
  then
    record_blocker "runtime-${run_number}:unexpected-diagnostic"
  fi
  kill -TERM "${active_pid}"
  stopped_pid="${active_pid}"
  set +e
  wait_for_exit "${active_pid}" 100
  shutdown_status=$?
  set -e
  if [[ "${shutdown_status}" -eq 124 ]]; then
    record_blocker "runtime-${run_number}:shutdown-timeout"
    kill -KILL "${active_pid}" 2>/dev/null || true
    wait "${active_pid}" 2>/dev/null || true
  elif [[ "${shutdown_status}" -ne 0 ]]; then
    record_blocker "runtime-${run_number}:shutdown-exit-${shutdown_status}"
  fi
  if process_is_live "${stopped_pid}"; then
    pid_gone=0
    record_blocker "runtime-${run_number}:pid-still-present"
  else
    pid_gone=1
  fi
  active_pid=""
  rm -f -- "${secret_file}"

  wal_size="$(stat -f '%z' "${runtime_root}/data/market.wal")"
  wal_hash="$(shasum -a 256 "${runtime_root}/data/market.wal" | awk '{print $1}')"
  if [[ "${run_number}" -eq 1 ]]; then
    first_wal_size="${wal_size}"
    first_wal_hash="${wal_hash}"
  elif [[ "${wal_size}" != "${first_wal_size}" || "${wal_hash}" != "${first_wal_hash}" ]]; then
    record_blocker "runtime-wal-recovery-mismatch"
  fi
  python3 - "${run_number}" "${stopped_pid}" "${shutdown_status}" "${pid_gone}" \
    "${digest}" "${wal_size}" "${wal_hash}" \
    "${temporary_root}/snapshot-${run_number}.json" >> "${runtime_records}" <<'PY'
import json
import pathlib
import sys

print(json.dumps({
    "run": int(sys.argv[1]),
    "stopped_pid": int(sys.argv[2]),
    "shutdown_exit_code": int(sys.argv[3]),
    "pid_absent_after_shutdown": sys.argv[4] == "1",
    "snapshot_digest": sys.argv[5],
    "wal_size_bytes": int(sys.argv[6]),
    "wal_sha256": sys.argv[7],
    "snapshot": json.loads(pathlib.Path(sys.argv[8]).read_text())["snapshot"],
}, sort_keys=True))
PY
done

if [[ -z "${first_digest}" ]] || [[ ! -s "${runtime_records}" ]] \
  || [[ "$(wc -l < "${runtime_records}" | tr -d ' ')" -ne 2 ]]
then
  record_blocker "runtime-two-run-proof-incomplete"
fi

if pgrep -x cryptoriskd >/dev/null 2>&1 || watcher_processes >/dev/null 2>&1; then
  {
    pgrep -alf cryptoriskd || true
    watcher_processes || true
  } > "${temporary_root}/stale-processes.txt"
  record_blocker "runtime-stale-process"
fi

readonly derived_data="${temporary_root}/DerivedData"
run_gate native-app-build 600 xcodebuild \
  -quiet \
  -project apps/macos/CuspObservatory.xcodeproj \
  -scheme CuspObservatory \
  -configuration Debug-UITesting \
  -destination "platform=macOS,arch=arm64" \
  -derivedDataPath "${derived_data}" \
  -jobs 1 \
  SWIFT_ENABLE_EXPLICIT_MODULES=NO \
  build
inspect_native_warnings native-app-build

readonly app_bundle="${derived_data}/Build/Products/Debug-UITesting/CuspObservatory.app"
readonly app_executable="${app_bundle}/Contents/MacOS/CuspObservatory"
readonly embedded_daemon="${app_bundle}/Contents/MacOS/cryptoriskd"
app_temporary_root="$(python3 -c 'import tempfile; print(tempfile.gettempdir())')"
readonly app_temporary_root
app_run_token="$(basename "${temporary_root}" | tr -cd '[:alnum:]')"
readonly app_run_token
app_runtime_records="${temporary_root}/app-runtime-records.jsonl"
app_runtime_log_start="$(date -u '+@%s')"

wait_for_pid_gone() {
  local pid="$1"
  local timeout_ticks="$2"
  local tick
  for ((tick = 0; tick < timeout_ticks; tick += 1)); do
    if ! process_is_live "${pid}"; then
      return 0
    fi
    sleep 0.05
  done
  return 124
}

exact_child_pid() {
  local parent_pid="$1"
  local executable_name="$2"
  ps -axo pid=,ppid=,ucomm= | awk \
    -v parent="${parent_pid}" -v executable="${executable_name}" \
    '$2 == parent && $3 == executable { print $1 }'
}

direct_child_pids() {
  local parent_pid="$1"
  ps -axo pid=,ppid= | awk -v parent="${parent_pid}" \
    '$2 == parent { print $1 }'
}

wait_for_app_runtime() {
  local app_pid="$1"
  local run_id="$2"
  local runtime_log="${app_temporary_root}/CuspObservatoryTests/${run_id}/logs/cmti.jsonl"
  local tick
  for ((tick = 0; tick < 120; tick += 1)); do
    if ! process_is_live "${app_pid}"; then
      return 1
    fi
    active_app_daemon_pid="$(exact_child_pid "${app_pid}" cryptoriskd | head -n 1)"
    if [[ -n "${active_app_daemon_pid}" ]] \
      && [[ -f "${runtime_log}" ]] \
      && rg -q '"event":"fixture_runtime_ready"' "${runtime_log}"
    then
      return 0
    fi
    sleep 0.05
  done
  return 124
}

wait_for_owned_cleanup() {
  local daemon_pid="$1"
  local watcher_pid="$2"
  if ! wait_for_pid_gone "${daemon_pid}" 120; then
    return 124
  fi
  if [[ -n "${watcher_pid}" ]] && ! wait_for_pid_gone "${watcher_pid}" 120; then
    return 124
  fi
}

abort_app_runtime() {
  local app_pid="${active_app_pid}"
  local daemon_pid="${active_app_daemon_pid}"
  local watcher_pid="${active_app_watcher_pid}"
  if [[ -n "${app_pid}" ]] && process_is_live "${app_pid}"; then
    kill -KILL "${app_pid}" 2>/dev/null || true
    wait "${app_pid}" 2>/dev/null || true
  fi
  stop_owned_descendant "${watcher_pid}" || true
  stop_owned_descendant "${daemon_pid}" || true
  active_app_pid=""
  active_app_daemon_pid=""
  active_app_watcher_pid=""
  active_app_topology_observed=0
}

launch_app_runtime() {
  local run_id="$1"
  local label="$2"
  local allow_existing="${3:-0}"
  local stdout_file="${temporary_root}/${label}.stdout"
  local stderr_file="${temporary_root}/${label}.stderr"
  local generated_root="${app_temporary_root}/CuspObservatoryTests/${run_id}"
  local watcher_candidates
  local watcher_count
  local watcher_command
  local daemon_parent
  local watcher_parent
  if [[ -e "${generated_root}" ]] && [[ "${allow_existing}" -ne 1 ]]; then
    record_blocker "${label}:runtime-root-preexists"
    return 1
  fi
  if [[ "${allow_existing}" -ne 1 ]]; then
    app_runtime_roots+=("${generated_root}")
  fi
  CMTI_UI_TEST_MODE=1 \
    CMTI_TEST_RUN_ID="${run_id}" \
    CMTI_DAEMON_PATH="${embedded_daemon}" \
    "${app_executable}" >"${stdout_file}" 2>"${stderr_file}" &
  active_app_pid=$!
  active_app_daemon_pid=""
  active_app_topology_observed=0
  if ! wait_for_app_runtime "${active_app_pid}" "${run_id}"; then
    record_blocker "${label}:startup"
    abort_app_runtime
    return 1
  fi
  watcher_candidates="$(direct_child_pids "${active_app_daemon_pid}")"
  watcher_count="$(printf '%s\n' "${watcher_candidates}" | awk 'NF { count += 1 } END { print count + 0 }')"
  if [[ "${watcher_count}" -eq 0 ]]; then
    record_blocker "${label}:watcher-missing"
    abort_app_runtime
    return 1
  fi
  if [[ "${watcher_count}" -ne 1 ]]; then
    record_blocker "${label}:watcher-ambiguous"
    abort_app_runtime
    return 1
  fi
  active_app_watcher_pid="$(printf '%s\n' "${watcher_candidates}" | awk 'NF { print; exit }')"
  watcher_command="$(ps -p "${active_app_watcher_pid}" -o command=)"
  if [[ "${watcher_command}" != *"/bin/sh -c"* ]] \
    || [[ "${watcher_command}" != *"cmti-daemon"* ]] \
    || [[ "${watcher_command}" != *"${embedded_daemon}"* ]] \
    || [[ "${watcher_command}" != *"${generated_root}"* ]] \
    || ! lsof -a -p "${active_app_watcher_pid}" -d 4 -Fn | rg -q '^n->'
  then
    record_blocker "${label}:watcher-identity-mismatch"
    abort_app_runtime
    return 1
  fi
  daemon_parent="$(ps -p "${active_app_daemon_pid}" -o ppid= | awk '{$1=$1; print}')"
  watcher_parent="$(ps -p "${active_app_watcher_pid}" -o ppid= | awk '{$1=$1; print}')"
  if [[ "${daemon_parent}" != "${active_app_pid}" ]] \
    || [[ "${watcher_parent}" != "${active_app_daemon_pid}" ]]
  then
    record_blocker "${label}:topology-mismatch"
    abort_app_runtime
    return 1
  fi
  active_app_topology_observed=1
  ps -p "${active_app_pid}" -o pid=,ppid=,ucomm=,command= \
    > "${temporary_root}/${label}.app-process"
  ps -p "${active_app_daemon_pid}" -o pid=,ppid=,ucomm=,command= \
    > "${temporary_root}/${label}.daemon-process"
  ps -p "${active_app_watcher_pid}" -o pid=,ppid=,ucomm=,command= \
    > "${temporary_root}/${label}.watcher-process"
  lsof -a -p "${active_app_pid}" -Fn > "${temporary_root}/${label}.app-lsof"
  lsof -a -p "${active_app_daemon_pid}" -Fn > "${temporary_root}/${label}.daemon-lsof"
}

controlled_app_quit() {
  local label="$1"
  local app_pid="${active_app_pid}"
  local daemon_pid="${active_app_daemon_pid}"
  local watcher_pid="${active_app_watcher_pid}"
  local topology_observed="${active_app_topology_observed}"
  run_gate "${label}-controlled-quit" 20 swift -e \
    "import AppKit; import Darwin; guard let app = NSRunningApplication(processIdentifier: ${app_pid}) else { exit(2) }; exit(app.terminate() ? 0 : 3)"
  if wait_for_pid_gone "${app_pid}" 160; then
    set +e
    wait "${app_pid}"
    app_status=$?
    set -e
  else
    record_blocker "${label}:controlled-app-exit-timeout"
    kill -KILL "${app_pid}" 2>/dev/null || true
    set +e
    wait "${app_pid}"
    app_status=$?
    set -e
  fi
  if [[ "${app_status}" -ne 0 ]]; then
    record_blocker "${label}:controlled-app-exit-${app_status}"
  fi
  if wait_for_owned_cleanup "${daemon_pid}" "${watcher_pid}"; then
    owned_processes_gone=1
    cleanup_required_escalation=0
  else
    record_blocker "${label}:controlled-owned-cleanup"
    cleanup_required_escalation=1
    stop_owned_descendant "${watcher_pid}" || true
    stop_owned_descendant "${daemon_pid}" || true
    if process_is_live "${watcher_pid}" || process_is_live "${daemon_pid}"; then
      owned_processes_gone=0
    else
      owned_processes_gone=1
    fi
  fi
  python3 - "${label}" controlled "${app_pid}" "${app_status}" \
    "${daemon_pid}" "${watcher_pid}" "${owned_processes_gone}" "${topology_observed}" \
    "${cleanup_required_escalation}" \
    >> "${app_runtime_records}" <<'PY'
import json
import sys
print(json.dumps({
    "label": sys.argv[1],
    "termination": sys.argv[2],
    "app_pid": int(sys.argv[3]),
    "app_exit_code": int(sys.argv[4]),
    "daemon_pid": int(sys.argv[5]),
    "watcher_pid": int(sys.argv[6]),
    "owned_processes_gone": sys.argv[7] == "1",
    "daemon_parent_was_app": sys.argv[8] == "1",
    "watcher_parent_was_daemon": sys.argv[8] == "1",
    "cleanup_required_escalation": sys.argv[9] == "1",
}, sort_keys=True))
PY
  active_app_pid=""
  if [[ "${owned_processes_gone}" -eq 1 ]]; then
    active_app_daemon_pid=""
    active_app_watcher_pid=""
  fi
  active_app_topology_observed=0
}

if [[ ! -x "${app_executable}" || ! -x "${embedded_daemon}" ]]; then
  record_blocker "native-app-build:missing-product"
else
  controlled_run_id="Task6Controlled-${app_run_token}"
  if launch_app_runtime "${controlled_run_id}" app-controlled; then
    controlled_app_quit app-controlled
  fi

  supervisor_loss_run_id="Task6SupervisorLoss-${app_run_token}"
  if launch_app_runtime "${supervisor_loss_run_id}" app-supervisor-loss; then
    forced_app_pid="${active_app_pid}"
    forced_daemon_pid="${active_app_daemon_pid}"
    forced_watcher_pid="${active_app_watcher_pid}"
    forced_topology_observed="${active_app_topology_observed}"
    kill -KILL "${forced_app_pid}"
    set +e
    wait "${forced_app_pid}"
    forced_app_status=$?
    set -e
    if [[ "${forced_app_status}" -ne 137 ]] && [[ "${forced_app_status}" -ne 9 ]]; then
      record_blocker "app-supervisor-loss:unexpected-app-exit-${forced_app_status}"
    fi
    if wait_for_owned_cleanup "${forced_daemon_pid}" "${forced_watcher_pid}"; then
      forced_owned_processes_gone=1
      forced_cleanup_required_escalation=0
    else
      record_blocker "app-supervisor-loss:owned-cleanup"
      forced_cleanup_required_escalation=1
      stop_owned_descendant "${forced_watcher_pid}" || true
      stop_owned_descendant "${forced_daemon_pid}" || true
      if process_is_live "${forced_watcher_pid}" \
        || process_is_live "${forced_daemon_pid}"
      then
        forced_owned_processes_gone=0
      else
        forced_owned_processes_gone=1
      fi
    fi
    active_app_pid=""
    if [[ "${forced_owned_processes_gone}" -eq 1 ]]; then
      active_app_daemon_pid=""
      active_app_watcher_pid=""
    fi
    active_app_topology_observed=0
    supervisor_loss_root="${app_temporary_root}/CuspObservatoryTests/${supervisor_loss_run_id}"
    python3 - "${forced_app_pid}" "${forced_app_status}" \
      "${forced_daemon_pid}" "${forced_watcher_pid}" "${forced_owned_processes_gone}" \
      "${forced_topology_observed}" "${forced_cleanup_required_escalation}" \
      >> "${app_runtime_records}" <<'PY'
import json
import sys
print(json.dumps({
    "label": "app-supervisor-loss",
    "termination": "SIGKILL",
    "app_pid": int(sys.argv[1]),
    "app_exit_code": int(sys.argv[2]),
    "daemon_pid": int(sys.argv[3]),
    "watcher_pid": int(sys.argv[4]),
    "owned_processes_gone": sys.argv[5] == "1",
    "daemon_parent_was_app": sys.argv[6] == "1",
    "watcher_parent_was_daemon": sys.argv[6] == "1",
    "cleanup_required_escalation": sys.argv[7] == "1",
}, sort_keys=True))
PY

    if [[ "${forced_owned_processes_gone}" -eq 1 ]]; then
      forced_wal_size="$(stat -f '%z' "${supervisor_loss_root}/data/market.wal")"
      forced_wal_hash="$(shasum -a 256 "${supervisor_loss_root}/data/market.wal" | awk '{print $1}')"
    fi
    if [[ "${forced_owned_processes_gone}" -eq 1 ]] \
      && launch_app_runtime "${supervisor_loss_run_id}" app-relaunch 1
    then
      relaunched_wal_size="$(stat -f '%z' "${supervisor_loss_root}/data/market.wal")"
      relaunched_wal_hash="$(shasum -a 256 "${supervisor_loss_root}/data/market.wal" | awk '{print $1}')"
      python3 - "${forced_wal_size}" "${forced_wal_hash}" \
        "${relaunched_wal_size}" "${relaunched_wal_hash}" \
        > "${temporary_root}/app-wal-recovery.json" <<'PY'
import json
import sys
print(json.dumps({
    "before_relaunch_size_bytes": int(sys.argv[1]),
    "before_relaunch_sha256": sys.argv[2],
    "after_relaunch_size_bytes": int(sys.argv[3]),
    "after_relaunch_sha256": sys.argv[4],
    "identical": sys.argv[1] == sys.argv[3] and sys.argv[2] == sys.argv[4],
}, sort_keys=True))
PY
      if [[ "${relaunched_wal_size}" != "${forced_wal_size}" ]] \
        || [[ "${relaunched_wal_hash}" != "${forced_wal_hash}" ]]
      then
        record_blocker "app-relaunch:wal-recovery-mismatch"
      fi
      controlled_app_quit app-relaunch
    fi
  fi
fi

if [[ -s "${app_runtime_records}" ]] \
  && [[ "$(wc -l < "${app_runtime_records}" | tr -d ' ')" -ne 3 ]]
then
  record_blocker "native-app-runtime-proof-incomplete"
elif [[ ! -s "${app_runtime_records}" ]]; then
  record_blocker "native-app-runtime-proof-missing"
fi
if find "${temporary_root}" -maxdepth 1 \
  \( -name 'app-*.stdout' -o -name 'app-*.stderr' \) \
  -type f -size +0c | rg . >/dev/null 2>&1
then
  find "${temporary_root}" -maxdepth 1 \
    \( -name 'app-*.stdout' -o -name 'app-*.stderr' \) \
    -type f -size +0c > "${temporary_root}/app-standard-stream-findings.txt"
  record_blocker "native-app-runtime:standard-stream-output"
fi
for generated_root in "${app_runtime_roots[@]}"; do
  if [[ -f "${generated_root}/logs/cmti.jsonl" ]] \
    && rg -n '"level":"(error|warn)"|panic|crash|hang|secret|token' \
      "${generated_root}/logs/cmti.jsonl" >/dev/null 2>&1
  then
    rg -n '"level":"(error|warn)"|panic|crash|hang|secret|token' \
      "${generated_root}/logs/cmti.jsonl" \
      >> "${temporary_root}/app-daemon-log-findings.txt"
    record_blocker "native-app-runtime:daemon-log-finding"
  fi
done

app_runtime_log_end="$(date -u '+@%s')"
runtime_pid_predicate="$(python3 - "${app_runtime_records}" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
pids = set()
if path.exists():
    for line in path.read_text().splitlines():
        if line:
            record = json.loads(line)
            pids.update((record["app_pid"], record["daemon_pid"]))
print(
    " OR ".join(f"processIdentifier == {pid}" for pid in sorted(pids))
    or "processIdentifier == -1"
)
PY
)"
run_gate unified-log-inspection 120 /usr/bin/log show \
  --style json \
  --start "${app_runtime_log_start}" \
  --end "${app_runtime_log_end}" \
  --predicate "${runtime_pid_predicate}"
run_gate unified-log-policy 30 python3 scripts/check-foundation-unified-log.py \
  "${temporary_root}/unified-log-inspection.log"

capture_xcresult_summary() {
  local label="$1"
  local bundle="$2"
  local output="$3"
  local external_when_automation_disabled="$4"
  local summary_status
  if [[ ! -d "${bundle}" ]]; then
    if [[ "${external_when_automation_disabled}" -eq 1 ]] \
      && [[ "${developer_mode_status}" == *"disabled"* ]]
    then
      record_blocker "external:${label}-missing-xcresult"
    else
      record_blocker "${label}:missing-xcresult"
    fi
    return 0
  fi
  set +e
  run_bounded 30 xcrun xcresulttool get test-results summary \
    --path "${bundle}" --compact \
    > "${output}" 2> "${output}.stderr"
  summary_status=$?
  set -e
  if [[ "${summary_status}" -ne 0 ]]; then
    rm -f -- "${output}"
    if [[ "${external_when_automation_disabled}" -eq 1 ]] \
      && [[ "${developer_mode_status}" == *"disabled"* ]]
    then
      record_blocker "external:${label}-invalid-xcresult"
    else
      record_blocker "${label}:invalid-xcresult-${summary_status}"
    fi
  fi
}

run_gate native-unit-tests 900 scripts/test-macos.sh \
  -resultBundlePath "${temporary_root}/native-unit.xcresult" \
  -only-testing:CuspObservatoryTests test

inspect_native_warnings native-unit-tests
capture_xcresult_summary \
  native-unit-tests \
  "${temporary_root}/native-unit.xcresult" \
  "${temporary_root}/native-unit-summary.json" \
  0

# This is intentionally the sole current XCUITest probe. The default verifier
# has no UI-skip mode and records the macOS authorization boundary as a failure.
run_gate native-ui-tests 120 scripts/test-macos.sh \
  -resultBundlePath "${temporary_root}/native-ui.xcresult" \
  -only-testing:CuspObservatoryUITests test

inspect_native_warnings native-ui-tests
if rg -q '^native-ui-tests:exit-' "${blocker_file}" \
  && [[ "${developer_mode_status}" == *"disabled"* ]]
then
  record_blocker "external:macos-automation-authorization-disabled"
fi
capture_xcresult_summary \
  native-ui-tests \
  "${temporary_root}/native-ui.xcresult" \
  "${temporary_root}/native-ui-summary.json" \
  1

git status --porcelain=v1 --untracked-files=all \
  > "${temporary_root}/pre-audit-tree-status.txt"
if [[ -s "${temporary_root}/pre-audit-tree-status.txt" ]]; then
  record_blocker "pre-audit-tree-dirty"
fi
run_gate static-audit 300 python3 scripts/static-audit.py
run_gate security-audit 300 python3 scripts/security-audit.py
git status --porcelain=v1 --untracked-files=all \
  > "${temporary_root}/post-audit-tree-status.txt"
if [[ -s "${temporary_root}/post-audit-tree-status.txt" ]]; then
  record_blocker "audit-mutated-tree"
fi
run_gate git-diff-check 60 git diff --check

if pgrep -x cryptoriskd >/dev/null 2>&1 \
  || pgrep -x CuspObservatory >/dev/null 2>&1 \
  || watcher_processes >/dev/null 2>&1
then
  {
    pgrep -alf cryptoriskd || true
    pgrep -alf CuspObservatory || true
    watcher_processes || true
  } > "${temporary_root}/final-stale-processes.txt"
  record_blocker "final-stale-process"
fi

git status --porcelain=v1 --untracked-files=all \
  > "${temporary_root}/pre-evidence-tree-status.txt"
if [[ -s "${temporary_root}/pre-evidence-tree-status.txt" ]]; then
  record_blocker "pre-evidence-tree-dirty"
fi

for artifact in \
  Cargo.lock \
  configs/schema.json \
  fixtures/binance/btcusdt-book-v1.jsonl \
  proto/buf.lock \
  apps/macos/CuspObservatory.xcodeproj/project.pbxproj \
  apps/macos/Packages/TransitionClient/Package.resolved \
  target/debug/cryptoriskd \
  target/debug/foundation-runtime-probe \
  "${app_executable}" \
  "${embedded_daemon}"
do
  if [[ ! -f "${artifact}" ]]; then
    record_blocker "missing-artifact:${artifact}"
    continue
  fi
  printf '%s\t%s\n' "${artifact}" "$(shasum -a 256 "${artifact}" | awk '{print $1}')" \
    >> "${artifact_hashes}"
done

if [[ "${gate_status}" -eq 0 ]]; then
  subgate_status="passed"
else
  subgate_status="blocked"
fi
full_gate_invocations=1
if [[ -n "${prior_evidence}" ]]; then
  set +e
  prior_count="$(python3 - "${prior_evidence}" "${verified_commit}" \
    "${temporary_root}/prior-evidence-summary.json" <<'PY'
import hashlib
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
commit = sys.argv[2]
summary_path = pathlib.Path(sys.argv[3])
try:
    raw = path.read_bytes()
    evidence = json.loads(raw)
except (OSError, ValueError):
    raise SystemExit(1)
if evidence.get("verified_commit") != commit:
    raise SystemExit(2)
if evidence.get("subgate_status") != "passed" or not evidence.get("tree_clean_at_start"):
    raise SystemExit(3)
if evidence.get("status") != "blocked":
    raise SystemExit(3)
count = evidence.get("full_gate_invocations_observed")
if not isinstance(count, int) or count < 1:
    raise SystemExit(4)
if evidence.get("blockers") != ["two-clean-full-gate-runs-not-observed"]:
    raise SystemExit(5)
commands = evidence.get("commands")
if not isinstance(commands, list) or not commands or any(
    command.get("exit_code") != 0 for command in commands
):
    raise SystemExit(6)
runtime = evidence.get("runtime", {})
if not all(
    runtime.get(field) is True
    for field in (
        "healthy_digests_identical",
        "deliberate_authentication_failure",
        "graceful_shutdown_observed",
        "wal_recovery_observed",
    )
):
    raise SystemExit(7)
if runtime.get("healthy_run_count") != 2:
    raise SystemExit(7)
native = evidence.get("native_app_runtime", {})
if not all(
    native.get(field) is True
    for field in (
        "controlled_quit_observed",
        "forced_supervisor_loss_observed",
        "relaunch_observed",
        "exact_watcher_topology_observed",
    )
):
    raise SystemExit(8)
if not native.get("wal_recovery", {}).get("identical"):
    raise SystemExit(8)
tree_checks = evidence.get("tree_mutation_checks", {})
if not tree_checks or any(tree_checks.values()):
    raise SystemExit(9)
if evidence.get("static_audit_current_error_count") != 0:
    raise SystemExit(10)
security_counts = evidence.get("security_audit_current_counts", {})
if any(security_counts.values()):
    raise SystemExit(10)
summary = {
    "sha256": hashlib.sha256(raw).hexdigest(),
    "verified_commit": commit,
    "status": evidence.get("status"),
    "subgate_status": evidence.get("subgate_status"),
    "tree_clean_at_start": evidence.get("tree_clean_at_start"),
    "blockers": evidence.get("blockers"),
    "full_gate_invocations_observed": count,
    "commands": [
        {
            "label": command.get("label"),
            "exit_code": command.get("exit_code"),
            "output_sha256": command.get("output_sha256"),
        }
        for command in commands
    ],
    "snapshot_digests": [
        run.get("snapshot_digest") for run in runtime.get("runs", [])
    ],
    "runtime_invariants": {
        field: runtime.get(field)
        for field in (
            "healthy_digests_identical",
            "deliberate_authentication_failure",
            "graceful_shutdown_observed",
            "wal_recovery_observed",
        )
    },
    "native_runtime_invariants": {
        field: native.get(field)
        for field in (
            "controlled_quit_observed",
            "forced_supervisor_loss_observed",
            "relaunch_observed",
            "exact_watcher_topology_observed",
        )
    },
}
summary_path.write_text(json.dumps(summary, sort_keys=True) + "\n")
print(count)
PY
  )"
  prior_status=$?
  set -e
  if [[ "${prior_status}" -ne 0 ]]; then
    record_blocker "prior-evidence-invalid-${prior_status}"
  else
    full_gate_invocations=$((prior_count + 1))
  fi
fi
if [[ "${full_gate_invocations}" -lt 2 ]]; then
  record_blocker "two-clean-full-gate-runs-not-observed"
fi

python3 - \
  "${verified_commit}" \
  "${target_readiness_label}" \
  "${subgate_status}" \
  "${full_gate_invocations}" \
  "${command_ledger}" \
  "${tool_versions}" \
  "${artifact_hashes}" \
  "${runtime_records}" \
  "${app_runtime_records}" \
  "${blocker_file}" \
  "${temporary_root}" \
  "${evidence_output}" <<'PY'
import hashlib
import json
import pathlib
import re
import sys
from datetime import datetime, timezone

(
    commit,
    target_label,
    subgate_status,
    full_gate_invocations,
    ledger_path,
    versions_path,
    hashes_path,
    runtime_path,
    app_runtime_path,
    blockers_path,
    temporary_root,
    output_path,
) = sys.argv[1:]
root = pathlib.Path(temporary_root)

commands = []
for line in pathlib.Path(ledger_path).read_text().splitlines():
    label, status, started, ended, output_sha256, command = line.split("\t", 5)
    log = root / f"{label}.log"
    text = log.read_text(errors="replace") if log.exists() else ""
    counts = []
    for pattern in (
        r"test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored",
        r"Executed (\d+) tests?, with (\d+) failures?",
        r"(\d+) tests passed",
    ):
        counts.extend([list(map(int, match)) for match in re.findall(pattern, text)])
    commands.append({
        "label": label,
        "command": command,
        "exit_code": int(status),
        "started_unix_nanos": int(started),
        "ended_unix_nanos": int(ended),
        "output_sha256": output_sha256,
        "test_count_matches": counts,
    })

versions = dict(
    line.split("\t", 1)
    for line in pathlib.Path(versions_path).read_text().splitlines()
)
hashes = dict(
    line.split("\t", 1)
    for line in pathlib.Path(hashes_path).read_text().splitlines()
) if pathlib.Path(hashes_path).exists() else {}
runtime = [
    json.loads(line)
    for line in pathlib.Path(runtime_path).read_text().splitlines()
] if pathlib.Path(runtime_path).exists() else []
app_runtime = [
    json.loads(line)
    for line in pathlib.Path(app_runtime_path).read_text().splitlines()
] if pathlib.Path(app_runtime_path).exists() else []
blockers = (
    pathlib.Path(blockers_path).read_text().splitlines()
    if pathlib.Path(blockers_path).exists()
    else []
)
warning_files = {}
for name in (
    "native-app-build-warnings.txt",
    "native-unit-tests-warnings.txt",
    "native-ui-tests-warnings.txt",
):
    path = root / name
    if path.exists():
        warning_files[name] = path.read_text().splitlines()
stale_files = {}
for name in ("stale-processes.txt", "final-stale-processes.txt"):
    path = root / name
    if path.exists():
        stale_files[name] = path.read_text().splitlines()
native_summaries = {}
for name in ("native-unit-summary.json", "native-ui-summary.json"):
    path = root / name
    if path.exists():
        native_summaries[name] = json.loads(path.read_text())
unified_findings = []
unified_path = root / "unified-log-policy.log"
if unified_path.exists():
    unified_result = json.loads(unified_path.read_text())
    unified_findings = unified_result.get("findings", [])
    if "parse_error" in unified_result:
        unified_findings.append({"parse_error": unified_result["parse_error"]})
app_wal_recovery_path = root / "app-wal-recovery.json"
app_wal_recovery = (
    json.loads(app_wal_recovery_path.read_text())
    if app_wal_recovery_path.exists()
    else None
)
app_log_findings = {}
for name in (
    "app-standard-stream-findings.txt",
    "app-daemon-log-findings.txt",
):
    path = root / name
    if path.exists():
        app_log_findings[name] = path.read_text().splitlines()
prior_summary_path = root / "prior-evidence-summary.json"
prior_summary = (
    json.loads(prior_summary_path.read_text())
    if prior_summary_path.exists()
    else None
)
static_log = (root / "static-audit.log").read_text(errors="replace")
static_match = re.search(r"static audit failed with (\d+) error\(s\)", static_log)
static_error_count = int(static_match.group(1)) if static_match else 0
security_evidence_path = pathlib.Path("release/evidence/security-audit.json")
security_counts = (
    json.loads(security_evidence_path.read_text()).get("counts", {})
    if security_evidence_path.exists()
    else {}
)
tree_status = {}
for name in (
    "start-tree-status.txt",
    "pre-audit-tree-status.txt",
    "post-audit-tree-status.txt",
    "pre-evidence-tree-status.txt",
):
    path = root / name
    tree_status[name] = path.read_text().splitlines() if path.exists() else ["missing"]
inspection_hashes = {}
for pattern in ("*.process", "*.lsof", "daemon-*.stdout", "daemon-*.stderr"):
    for path in root.glob(pattern):
        inspection_hashes[path.name] = hashlib.sha256(path.read_bytes()).hexdigest()
if not blockers:
    readiness_label = target_label
elif blockers == ["two-clean-full-gate-runs-not-observed"]:
    readiness_label = "blocked:verification-incomplete"
else:
    has_external = any(blocker.startswith("external:") for blocker in blockers)
    external_ui_boundary = (
        "external:macos-automation-authorization-disabled" in blockers
    )
    repo_blockers = [
        blocker
        for blocker in blockers
        if not blocker.startswith("external:")
        and blocker != "two-clean-full-gate-runs-not-observed"
        and not (
            external_ui_boundary
            and blocker.startswith("native-ui-tests:exit-")
        )
    ]
    if has_external and repo_blockers:
        readiness_label = "blocked:repo+external"
    elif has_external:
        readiness_label = "blocked:external"
    else:
        readiness_label = "blocked:repo"
limitations = [
    "This is not model, live-source, App Store, notarization, or release readiness."
]
if "external:macos-automation-authorization-disabled" in blockers:
    limitations.append(
        "Fresh XCUITest is blocked before product tests while macOS automation authorization is disabled."
    )
if "external:swift-nio-cniowindows-warning" in blockers:
    limitations.append(
        "The upstream SwiftNIO CNIOWindows umbrella-header warning remains a fail-closed warning boundary."
    )
if static_error_count:
    limitations.append(
        f"The static audit retains {static_error_count} later-phase scaffold findings."
    )
if security_counts.get("high"):
    limitations.append(
        "The security audit retains "
        f"{security_counts['high']} high findings in exact vendored public test fixtures."
    )

evidence = {
    "schema_version": 1,
    "generated_at": datetime.now(timezone.utc).isoformat(),
    "verified_commit": commit,
    "tree_clean_at_start": tree_status.get("start-tree-status.txt", ["missing"]) == [],
    "subgate_status": subgate_status,
    "full_gate_invocations_observed": int(full_gate_invocations),
    "two_clean_full_gate_runs_observed": int(full_gate_invocations) >= 2,
    "prior_run_summary": prior_summary,
    "provenance_invariant": (
        "This file was generated from the exact clean implementation commit named "
        "by verified_commit and is added, if tracked, only by a later evidence-only commit."
    ),
    "status": "passed" if not blockers else "blocked",
    "readiness_label": readiness_label,
    "target_readiness_label": target_label,
    "blockers": blockers,
    "tool_versions": versions,
    "commands": commands,
    "artifact_sha256": hashes,
    "process_and_stream_inspection_sha256": inspection_hashes,
    "runtime": {
        "healthy_run_count": len(runtime),
        "runs": runtime,
        "healthy_digests_identical": (
            len(runtime) == 2
            and runtime[0]["snapshot_digest"] == runtime[1]["snapshot_digest"]
        ),
        "deliberate_authentication_failure": (
            (root / "authentication-failure.json").exists()
            and json.loads((root / "authentication-failure.json").read_text())
            == {
                "authentication_failure": "rejected",
                "code": "Unauthenticated",
            }
        ),
        "graceful_shutdown_observed": (
            len(runtime) == 2
            and all(
                run["shutdown_exit_code"] == 0
                and run["pid_absent_after_shutdown"]
                for run in runtime
            )
        ),
        "wal_recovery_observed": (
            len(runtime) == 2
            and runtime[0]["wal_size_bytes"] == runtime[1]["wal_size_bytes"]
            and runtime[0]["wal_sha256"] == runtime[1]["wal_sha256"]
        ),
        "secret_scans": [
            json.loads((root / f"secret-scan-{number}.json").read_text())
            for number in (1, 2)
            if (root / f"secret-scan-{number}.json").exists()
        ],
        "warning_findings": warning_files,
        "stale_process_findings": stale_files,
        "unified_log_findings": unified_findings,
    },
    "native_app_runtime": {
        "records": app_runtime,
        "controlled_quit_observed": any(
            record["termination"] == "controlled"
            and record["app_exit_code"] == 0
            and record["owned_processes_gone"]
            for record in app_runtime
        ),
        "forced_supervisor_loss_observed": any(
            record["termination"] == "SIGKILL"
            and record["app_exit_code"] in (9, 137)
            and record["owned_processes_gone"]
            for record in app_runtime
        ),
        "relaunch_observed": any(
            record["label"] == "app-relaunch"
            and record["termination"] == "controlled"
            and record["app_exit_code"] == 0
            and record["owned_processes_gone"]
            for record in app_runtime
        ),
        "exact_watcher_topology_observed": (
            len(app_runtime) == 3
            and all(
                record["daemon_parent_was_app"]
                and record["watcher_parent_was_daemon"]
                for record in app_runtime
            )
        ),
        "wal_recovery": app_wal_recovery,
        "log_findings": app_log_findings,
    },
    "native_test_summaries": native_summaries,
    "static_audit_historical_baseline_error_count": 384,
    "static_audit_current_error_count": static_error_count,
    "security_audit_current_counts": security_counts,
    "tree_mutation_checks": tree_status,
    "limitations": limitations,
}
output = pathlib.Path(output_path)
output.parent.mkdir(parents=True, exist_ok=True)
output.write_text(json.dumps(evidence, indent=2, sort_keys=True) + "\n")
PY

cleanup_generated_app_roots
trap - EXIT INT TERM HUP
rm -rf -- "${temporary_root}"
if [[ "${gate_status}" -ne 0 ]]; then
  printf 'foundation runtime verification: BLOCKED (evidence: %s)\n' "${evidence_output}" >&2
  exit 1
fi
printf 'foundation runtime verification: PASS (%s)\n' "${target_readiness_label}"
