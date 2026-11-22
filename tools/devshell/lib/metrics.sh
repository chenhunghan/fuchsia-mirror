#!/bin/bash
# Copyright 2019 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Common methods for metrics collection, backed by tools/devshell/fx_cli/metrics.py.
#
# Note: For non-shell programs, use metrics_custom_report.sh instead.
#
# How to use it: This file is sourced in //scripts/fx for tracking command
# execution and in //tools/devshell/metrics for managing the metrics collection
# settings. Developers of shell-based subcommands can source this file in their
# subcommand if they need custom event tracking, but only the method
# track-subcommand-custom-event can be used in this context.

# Capture the script's repository root at source time so callers/tests that
# temporarily override FUCHSIA_DIR to an empty or mock tree still resolve
# tools/devshell/fx_cli/metrics.py and host Python reliably.
_METRICS_SCRIPT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." >/dev/null 2>&1 && pwd)"
_METRICS_PY="${_METRICS_SCRIPT_ROOT}/tools/devshell/fx_cli/metrics.py"

if [[ -z "${FUCHSIA_DIR:-}" ]]; then
  FUCHSIA_DIR="${_METRICS_SCRIPT_ROOT}"
fi

# Define globals before marking readonly so sourcing inside a function does not
# scope them locally.
if [[ -z "${_lib_metrics_sh_sourced:-}" ]]; then
  _lib_metrics_sh_sourced=1
  # LINT.IfChange(allows_custom_reporting)
  _METRICS_ALLOWS_CUSTOM_REPORTING=( "test" "worktree" )
  # LINT.ThenChange(//tools/devshell/fx_cli/metrics.py:allows_custom_reporting)
  readonly _METRICS_ALLOWS_CUSTOM_REPORTING
fi

# To properly enable unit testing, METRICS_CONFIG is not read-only.
METRICS_CONFIG="${METRICS_CONFIG:-${FUCHSIA_DIR}/.fx/config/metrics}"
METRICS_LEVEL="${METRICS_LEVEL:-0}"
METRICS_ENABLED="${METRICS_ENABLED:-0}"
METRICS_UUID="${METRICS_UUID:-}"
OTHER_TOOLS_ANALYTICS_UUID="${OTHER_TOOLS_ANALYTICS_UUID:-}"
_METRICS_DEBUG="${_METRICS_DEBUG:-0}"
_METRICS_DEBUG_LOG_FILE="${_METRICS_DEBUG_LOG_FILE:-}"
_METRICS_USE_VALIDATION_SERVER="${_METRICS_USE_VALIDATION_SERVER:-0}"
_METRICS_TESTING_MODE="${_METRICS_TESTING_MODE:-0}"
_METRICS_TESTING_DEBUG_LOG_FILE="${_METRICS_TESTING_DEBUG_LOG_FILE:-}"

function _metrics_host_platform {
  if [[ -n "${HOST_PLATFORM:-}" ]]; then
    printf '%s\n' "${HOST_PLATFORM}"
    return 0
  fi
  local _m_os _m_arch
  _m_os="$(uname -s 2>/dev/null || true)"
  _m_arch="$(uname -m 2>/dev/null || true)"
  case "${_m_os}" in
    Linux) _m_os="linux" ;;
    Darwin) _m_os="mac" ;;
    *) _m_os="unknown" ;;
  esac
  case "${_m_arch}" in
    x86_64|amd64) _m_arch="x64" ;;
    arm64|aarch64) _m_arch="arm64" ;;
  esac
  printf '%s-%s\n' "${_m_os}" "${_m_arch}"
}

function _metrics_find_python {
  local _m_py_candidate="${PREBUILT_PYTHON3:-}"
  if [[ -n "${_m_py_candidate}" ]]; then
    if declare -F fx-ensure-prebuilt >/dev/null 2>&1; then
      fx-ensure-prebuilt "${_m_py_candidate}" >/dev/null 2>&1 || true
    fi
    if [[ -x "${_m_py_candidate}" ]]; then
      _METRICS_RESOLVED_PYTHON="${_m_py_candidate}"
      return 0
    fi
  fi
  local _m_plat
  _m_plat="$(_metrics_host_platform)"
  local _m_rel_subpath="prebuilt/third_party/python3/${_m_plat}/bin/python3"
  if declare -F fx-ensure-prebuilt >/dev/null 2>&1; then
    fx-ensure-prebuilt "${_m_rel_subpath}" >/dev/null 2>&1 || true
  fi
  for _m_py_candidate in \
    "${_METRICS_SCRIPT_ROOT}/${_m_rel_subpath}" \
    "${FUCHSIA_DIR:-}/${_m_rel_subpath}"; do
    if [[ -n "${_m_py_candidate}" && -x "${_m_py_candidate}" ]]; then
      _METRICS_RESOLVED_PYTHON="${_m_py_candidate}"
      return 0
    fi
  done
  _m_py_candidate="$(command -v python3 2>/dev/null || true)"
  if [[ -n "${_m_py_candidate}" && -x "${_m_py_candidate}" ]] && \
    "${_m_py_candidate}" -B -S -c \
      'import sys; raise SystemExit(0 if sys.version_info >= (3, 11) else 1)' \
      >/dev/null 2>&1; then
    _METRICS_RESOLVED_PYTHON="${_m_py_candidate}"
    return 0
  fi
  _METRICS_RESOLVED_PYTHON=""
  return 1
}

function _metrics_can_run {
  [[ -f "${_METRICS_PY}" ]] || return 1
  _metrics_find_python || return 1
}

function _metrics_reset_disabled_state {
  METRICS_LEVEL=0
  METRICS_ENABLED=0
  METRICS_UUID=""
  OTHER_TOOLS_ANALYTICS_UUID=""
  _METRICS_DEBUG_LOG_FILE=""
}

function _metrics_run {
  if [[ -z "${_METRICS_RESOLVED_PYTHON:-}" || ! -x "${_METRICS_RESOLVED_PYTHON}" ]]; then
    _metrics_find_python || return 1
  fi
  local _m_py_bin="${_METRICS_RESOLVED_PYTHON}"
  local _m_py_env_args=()
  case "${PYTHONHOME:-}:${PYTHONPATH:-}" in
    */_fx_host_tools/*)
      _m_py_env_args+=(-u PYTHONHOME -u PYTHONPATH -u PYTHONSAFEPATH)
      ;;
  esac
  # Forward shell state that callers/tests commonly mutate without `export`
  # (notably HOSTNAME, PWD, METRICS_CONFIG, and bash builtins BASH_VERSION /
  # ZSH_VERSION). Under _enable_testing, forward _METRICS_TESTING_DEBUG_LOG_FILE
  # (and unset any inherited _METRICS_DEBUG_LOG_FILE) so metrics.py redirects
  # debug logging without force-enabling analytics when config is disabled.
  if [[ "${_METRICS_TESTING_MODE:-0}" -eq 1 ]]; then
    _m_py_env_args+=(
      -u _METRICS_DEBUG_LOG_FILE
      "_METRICS_TESTING_DEBUG_LOG_FILE=${_METRICS_TESTING_DEBUG_LOG_FILE:-}"
    )
  fi
  env \
    ${_m_py_env_args[@]+"${_m_py_env_args[@]}"} \
    PYTHONDONTWRITEBYTECODE=1 \
    HOSTNAME="${HOSTNAME:-}" \
    BASH_VERSION="${BASH_VERSION:-}" \
    ZSH_VERSION="${ZSH_VERSION:-}" \
    PWD="${PWD:-}" \
    USER="${USER:-}" \
    FUCHSIA_DIR="${FUCHSIA_DIR:-}" \
    METRICS_CONFIG="${METRICS_CONFIG:-}" \
    NINJA_PERSISTENT_MODE="${NINJA_PERSISTENT_MODE:-}" \
    FUCHSIA_FX_INVOKER="${FUCHSIA_FX_INVOKER:-}" \
    FUCHSIA_METRICS_CALL_STACK="${FUCHSIA_METRICS_CALL_STACK:-}" \
    FUCHSIA_FX_ITERATIVE="${FUCHSIA_FX_ITERATIVE:-}" \
    FUCHSIA_FX_TEST_RUN="${FUCHSIA_FX_TEST_RUN:-}" \
    FUCHSIA_FX_MULTI_RUN="${FUCHSIA_FX_MULTI_RUN:-}" \
    _METRICS_DEBUG="${_METRICS_DEBUG:-0}" \
    _METRICS_USE_VALIDATION_SERVER="${_METRICS_USE_VALIDATION_SERVER:-0}" \
    ${FUCHSIA_BUILD_DIR:+FUCHSIA_BUILD_DIR="${FUCHSIA_BUILD_DIR}"} \
    ${_FX_BUILD_DIR:+_FX_BUILD_DIR="${_FX_BUILD_DIR}"} \
    ${HOST_OUT_DIR:+HOST_OUT_DIR="${HOST_OUT_DIR}"} \
    ${FUCHSIA_DEVICE_ADDR:+FUCHSIA_DEVICE_ADDR="${FUCHSIA_DEVICE_ADDR}"} \
    ${FUCHSIA_NODENAME:+FUCHSIA_NODENAME="${FUCHSIA_NODENAME}"} \
    "${_m_py_bin}" -S -B "${_METRICS_PY}" "$@"
}

function metrics-is-internal-user {
  local _m_host="${HOSTNAME:-}"
  if [[ -z "${_m_host}" ]] && command -v hostname >/dev/null 2>&1; then
    _m_host="$(hostname 2>/dev/null || true)"
  fi
  [[ "${_m_host}" == *.c.googlers.com || "${_m_host}" == *.corp.google.com ]]
}

function metrics-sanitize-string {
  local _m_raw="${1:-}"
  if _metrics_can_run; then
    _metrics_run sanitize-string --fuchsia-dir="${FUCHSIA_DIR}" -- "${_m_raw}"
    return $?
  fi
  local _m_sanitized="${_m_raw}"
  if [[ -n "${FUCHSIA_DIR:-}" ]]; then
    _m_sanitized="${_m_sanitized//${FUCHSIA_DIR}/\$FUCHSIA_DIR}"
  fi
  if [[ -n "${USER:-}" ]]; then
    _m_sanitized="${_m_sanitized//${USER}/\$USER}"
  fi
  local _m_host="${HOSTNAME:-}"
  if [[ -z "${_m_host}" ]] && command -v hostname >/dev/null 2>&1; then
    _m_host="$(hostname 2>/dev/null || true)"
  fi
  if [[ -n "${_m_host}" ]]; then
    _m_sanitized="${_m_sanitized//${_m_host}/\$HOSTNAME}"
  fi
  printf '%s\n' "${_m_sanitized}"
}

function metrics-read-config {
  if ! _metrics_can_run; then
    _metrics_reset_disabled_state
    return 1
  fi
  local _m_out _m_rc=0
  _m_out="$(_metrics_run read-config --fuchsia-dir="${FUCHSIA_DIR}")" || _m_rc=$?
  if [[ -n "${_m_out}" ]]; then
    eval "${_m_out}"
  fi
  return "${_m_rc}"
}

function metrics-write-config {
  local _m_enabled="${1:-0}"
  local _m_uuid="${2:-}"
  local _m_debug_logfile="${3:-}"
  if ! _metrics_can_run; then
    echo "ERROR: Unable to write metrics config: Python >= 3.11 or metrics.py is unavailable." >&2
    return 1
  fi
  _metrics_run write-config \
    --fuchsia-dir="${FUCHSIA_DIR}" \
    --enabled="${_m_enabled}" \
    --uuid="${_m_uuid}" \
    --debug-log-file="${_m_debug_logfile}"
}

function metrics-write-config-internal {
  local _m_uuid="${1:-}"
  local _m_debug_logfile="${2:-}"
  if ! _metrics_can_run; then
    echo "ERROR: Unable to write metrics config: Python >= 3.11 or metrics.py is unavailable." >&2
    return 1
  fi
  _metrics_run write-config-internal \
    --fuchsia-dir="${FUCHSIA_DIR}" \
    --uuid="${_m_uuid}" \
    --debug-log-file="${_m_debug_logfile}"
}

function metrics-read-and-validate {
  local _m_hide_init_warning="${1:-0}"
  if ! _metrics_can_run; then
    _metrics_reset_disabled_state
    return 0
  fi
  local _m_extra_args=()
  if [[ "${_m_hide_init_warning}" -eq 1 ]]; then
    _m_extra_args+=(--hide-init-warning)
  fi
  local _m_out _m_rc=0
  _m_out="$(_metrics_run read-and-validate --fuchsia-dir="${FUCHSIA_DIR}" ${_m_extra_args[@]+"${_m_extra_args[@]}"})" || _m_rc=$?
  if [[ "${_m_rc}" -ne 0 ]]; then
    _metrics_reset_disabled_state
    return 0
  fi
  if [[ -n "${_m_out}" ]]; then
    eval "${_m_out}"
  fi
}

function metrics-set-debug-logfile {
  _METRICS_DEBUG_LOG_FILE="${1:-}"
  return 0
}

function metrics-get-debug-logfile {
  if [[ -n "${_METRICS_DEBUG_LOG_FILE:-}" ]]; then
    printf '%s\n' "${_METRICS_DEBUG_LOG_FILE}"
  fi
}

function metrics-maybe-log {
  if ! _metrics_can_run; then
    return 0
  fi
  _metrics_run maybe-log \
    --fuchsia-dir="${FUCHSIA_DIR}" \
    --log-file="${_METRICS_DEBUG_LOG_FILE:-}" \
    -- "$@"
}

function metrics-init {
  local _m_subcommand="${1:-}"
  if [[ $# -gt 0 ]]; then
    shift
  fi
  if ! _metrics_can_run; then
    _metrics_reset_disabled_state
    export FUCHSIA_METRICS_CALL_STACK="${FUCHSIA_METRICS_CALL_STACK:+${FUCHSIA_METRICS_CALL_STACK} }fx"
    return 0
  fi
  local _m_out _m_rc=0
  _m_out="$(_metrics_run init --fuchsia-dir="${FUCHSIA_DIR}" --command-name="${_m_subcommand}" -- "$@")" || _m_rc=$?
  if [[ "${_m_rc}" -ne 0 ]]; then
    _metrics_reset_disabled_state
  elif [[ -n "${_m_out}" ]]; then
    eval "${_m_out}"
  fi
  export FUCHSIA_METRICS_CALL_STACK="${FUCHSIA_METRICS_CALL_STACK:+${FUCHSIA_METRICS_CALL_STACK} }fx"
}

function track-feature-status {
  exec </dev/null >/dev/null 2>&1
  if [[ $# -lt 2 ]]; then
    return 0
  fi
  local _m_feature="$1"
  local _m_val="$2"
  local _m_is_disabled=0
  if [[ "${_m_val}" =~ ^-?[0-9]+$ ]]; then
    _m_is_disabled="${_m_val}"
  elif [[ "${_m_val}" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]]; then
    _m_is_disabled="${!_m_val:-0}"
  fi
  if ! _metrics_can_run; then
    return 0
  fi
  _metrics_run track-feature-status \
    --fuchsia-dir="${FUCHSIA_DIR}" \
    --feature="${_m_feature}" \
    --is-disabled="${_m_is_disabled}"
}

function track-command-execution {
  exec </dev/null >/dev/null 2>&1
  if [[ $# -lt 1 ]]; then
    return 0
  fi
  local _m_subcommand="$1"
  shift
  if ! _metrics_can_run; then
    return 0
  fi
  _metrics_run track-command-execution \
    --fuchsia-dir="${FUCHSIA_DIR}" \
    --subcommand="${_m_subcommand}" \
    -- "$@"
}

function track-command-start {
  exec </dev/null >/dev/null 2>&1
  if [[ $# -lt 1 ]]; then
    return 0
  fi
  local _m_command_name="$1"
  shift
  if ! _metrics_can_run; then
    return 0
  fi
  _metrics_run report-start \
    --fuchsia-dir="${FUCHSIA_DIR}" \
    --command-name="${_m_command_name}" \
    -- "$@"
}

function track-command-finished {
  exec </dev/null >/dev/null 2>&1
  if [[ $# -lt 5 ]]; then
    return 0
  fi
  local _m_start_time="$1"
  local _m_end_time="$2"
  local _m_exit_status="$3"
  local _m_subcommand="$4"
  local _m_is_remote="$5"
  shift 5
  if ! _metrics_can_run; then
    return 0
  fi
  _metrics_run track-command-finished \
    --fuchsia-dir="${FUCHSIA_DIR}" \
    --start-time-micros="${_m_start_time}" \
    --end-time-micros="${_m_end_time}" \
    --exit-status="${_m_exit_status}" \
    --subcommand="${_m_subcommand}" \
    --is-remote="${_m_is_remote}" \
    -- "$@"
}

function track-build-event {
  exec </dev/null >/dev/null 2>&1
  if [[ $# -lt 8 ]]; then
    return 0
  fi
  local _m_start_time="$1"
  local _m_end_time="$2"
  local _m_exit_status="$3"
  local _m_ninja_switches="$4"
  local _m_fuchsia_targets="$5"
  local _m_build_dir="$6"
  local _m_is_no_op="$7"
  local _m_is_clean_build="$8"
  local _m_quiet="${9:-false}"
  if ! _metrics_can_run; then
    return 0
  fi
  _metrics_run track-build-event \
    --fuchsia-dir="${FUCHSIA_DIR}" \
    --start-time-micros="${_m_start_time}" \
    --end-time-micros="${_m_end_time}" \
    --exit-status="${_m_exit_status}" \
    --ninja-switches="${_m_ninja_switches}" \
    --fuchsia-targets="${_m_fuchsia_targets}" \
    --build-dir="${_m_build_dir}" \
    --is-no-op="${_m_is_no_op}" \
    --is-clean-build="${_m_is_clean_build}" \
    --quiet="${_m_quiet}"
}

function track-test-event {
  exec </dev/null >/dev/null 2>&1
  if [[ $# -lt 5 ]]; then
    return 0
  fi
  local _m_start_time="$1"
  local _m_end_time="$2"
  local _m_exit_status="$3"
  local _m_was_aborted="$4"
  local _m_stats_json="$5"
  local _m_build_dir="${6:-}"
  if ! _metrics_can_run; then
    return 0
  fi
  local _m_build_dir_args=()
  if [[ -n "${_m_build_dir}" ]]; then
    _m_build_dir_args=(--build-dir="${_m_build_dir}")
  fi
  _metrics_run track-test-event \
    --fuchsia-dir="${FUCHSIA_DIR}" \
    --start-time-micros="${_m_start_time}" \
    --end-time-micros="${_m_end_time}" \
    --exit-status="${_m_exit_status}" \
    --was-aborted="${_m_was_aborted}" \
    --stats-json="${_m_stats_json}" \
    ${_m_build_dir_args[@]+"${_m_build_dir_args[@]}"}
}

function track-subcommand-custom-event {
  exec </dev/null >/dev/null 2>&1
  if [[ $# -lt 2 ]]; then
    return 1
  fi
  local _m_subcommand="$1"
  local _m_action="$2"
  shift 2
  if ! _metrics_can_run; then
    return 0
  fi
  _metrics_run track-custom-event \
    --fuchsia-dir="${FUCHSIA_DIR}" \
    --subcommand="${_m_subcommand}" \
    --action="${_m_action}" \
    -- "$@"
}

function _metrics-service {
  # Drain stdin (e.g. test harness FIFOs) until EOF.
  exec 1>/dev/null
  exec 2>/dev/null
  cat >/dev/null
}

function _enable_testing {
  _METRICS_TESTING_MODE=1
  _METRICS_TESTING_DEBUG_LOG_FILE="${1:-}"
  _METRICS_DEBUG_LOG_FILE="${1:-}"
  _METRICS_USE_VALIDATION_SERVER="${2:-0}"
  if [[ $# -gt 2 ]]; then
    METRICS_CONFIG="$3"
  fi
  _METRICS_DEBUG=1
  METRICS_UUID="TEST"
}
