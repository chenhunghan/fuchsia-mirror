#!/bin/bash
# Copyright 2022 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# A wrapper script used to invoke Bazel in the Fuchsia build's workspace.
# This requires a file named "bazel.sh.config" to be located in the same
# directory containing specific variable definitions (see below).
#
# Do not use directly, the //build/regenerator script will copy this
# file to a specific location in the build directory and generate the
# appropriate configuration file.
#
# Environment variables that influence this script:
#
# FX_INTERNAL_RESULTSTORE_BAZEL: Set to "resultstore" or "resultstore_infra"
#   to dynamically append the corresponding --config= flag at runtime.
#   Set to "0" or leave unset to disable.

_SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"
readonly _SCRIPT_DIR

_SRCDIR="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" >/dev/null 2>&1 && pwd)"
readonly _SRCDIR

_FUCHSIA_DIR="${FUCHSIA_DIR:-$(cd "${_SRCDIR}/../.." >/dev/null 2>&1 && pwd)}"
readonly _FUCHSIA_DIR

function die {
  echo >&2 "ERROR: $*"
  exit 1
}

function msg {
  # identify using basename of this script
  echo >&2 "[${BASH_SOURCE[0]##*/}] $*"
}

# Read the configuration file. This should define the following variables:
#
# _BAZEL_BIN: Path to Bazel launcher script.
# _BAZEL_OUTPUT_BASE: Bazel output base directory path.
# _BAZEL_OUTPUT_USER_ROOT: Bazel output user root directory path.
# _BAZEL_WORKSPACE: Bazel workspace directory path.
# _NINJA_BUILD_DIR: Ninja build directory path.
# _PREBUILT_NINJA: Prebuilt Ninja binary path.
# _PREBUILT_PYTHON_DIR: Prebuilt python binary path.
#
# All paths should be absolute.
#
# LINT.IfChange(bazel.sh.config)
readonly _SCRIPT_ARGS_FILE="${_SCRIPT_DIR}/bazel.sh.config"
# LINT.ThenChange(//build/bazel/scripts/workspace_utils.py:bazel.sh.config)
[[ -f "${_SCRIPT_ARGS_FILE}" ]] || die "Missing Bazel script config file: ${_SCRIPT_ARGS_FILE}"

# shellcheck source=/dev/null
source "${_SCRIPT_ARGS_FILE}"

[[ -n "${_BAZEL_WORKSPACE}" ]] || die "Missing _BAZEL_WORKSPACE config variable"
[[ -d "${_BAZEL_WORKSPACE}" ]] || die "_BAZEL_WORKSPACE should be a directory: ${_BAZEL_WORKSPACE}"

# These directories are created on demand.
[[ -n "${_BAZEL_OUTPUT_BASE}" ]] || die "Missing _BAZEL_OUTPUT_BASE config variable"
[[ -n "${_BAZEL_OUTPUT_USER_ROOT}" ]] || die "Missing _BAZEL_OUTPUT_USER_ROOT config variable"

[[ -n "${_BAZEL_BIN}" ]] || die "Missing _BAZEL_BIN config variable"
[[ -f "${_BAZEL_BIN}" ]] || die "_BAZEL_BIN should be a file: ${_BAZEL_BIN}"

[[ -n "${_NINJA_BUILD_DIR}" ]] || die "Missing _NINJA_BUILD_DIR config variable"
[[ -d "${_NINJA_BUILD_DIR}" ]] || die "_NINJA_BUILD_DIR should be a directory: ${_NINJA_BUILD_DIR}"

[[ -n "${_PREBUILT_NINJA}" ]] || die "Missing _PREBUILT_NINJA config variable"
[[ -f "${_PREBUILT_NINJA}" ]] || die "_PREBUILT_NINJA should be a file: ${_PREBUILT_NINJA}"

[[ -n "${_PREBUILT_PYTHON_DIR}" ]] || die "Missing _PREBUILT_PYTHON_DIR config variable"
[[ -d "${_PREBUILT_PYTHON_DIR}" ]] || die "_PREBUILT_PYTHON_DIR should be a directory: ${_PREBUILT_PYTHON_DIR}"

[[ -n "${_BAZEL_LOGS_DIRNAME}" ]] || die "Missing _BAZEL_LOGS_DIRNAME config variable"

readonly _REMOTE_SERVICES_BAZELRC="${_NINJA_BUILD_DIR}/regenerator_outputs/remote_services.bazelrc"

readonly _GENERATE_INVOCATION_BAZELRC="${_SRCDIR}/scripts/generate_invocation_bazelrc.py"
readonly _INVOCATION_BAZELRC="${_BAZEL_WORKSPACE}/invocation.bazelrc"
readonly _VENDORING_BAZELRC="${_SRCDIR}/enable_vendor_mode.bazelrc"

# Exported explicitly to be used by repository rules to reference the
# Ninja output directory and binary.
export BAZEL_FUCHSIA_NINJA_OUTPUT_DIR="${_NINJA_BUILD_DIR}"
export BAZEL_FUCHSIA_NINJA_PREBUILT="${_NINJA_PREBUILT}"

# Ensure our prebuilt Python3 executable is in the PATH to run repository
# rules that invoke Python programs correctly in containers or jails that
# do not expose the system-installed one.
export PATH="${_PREBUILT_PYTHON_DIR}/bin:${PATH}"

# Provide a direct path to the prebuilt Python3 executable as well.
readonly _PREBUILT_PYTHON3="${_PREBUILT_PYTHON_DIR}/bin/python3"

# An undocumented, but widely used, environment variable that tells Bazel to
# not auto-detect the host C++ installation. This makes workspace setup faster
# and ensures this can be used on containers where GCC or Clang are not
# installed (Bazel would complain otherwise with an error).
export BAZEL_DO_NOT_DETECT_CPP_TOOLCHAIN=1

_BAZEL_PRE_COMMAND_ARGS=()

# We generate our own invocation id, to make it easier to propagate its value
# to sub-build invocations.
# If top-level fx bazel build, use FX_BUILD_UUID as the invocation id.
# If sub-build under Ninja, generate a unique child invocation id.
if [[ -z "${RESULTSTORE_PARENT_BUILD_ID:-}" && -n "${FX_BUILD_UUID:-}" ]]; then
  readonly RESULTSTORE_invocation_id="${FX_BUILD_UUID}"
else
  readonly RESULTSTORE_invocation_id="${RESULTSTORE_invocation_id:-$("${_PREBUILT_PYTHON3}" -S -c 'import uuid; print(uuid.uuid4())')}"
fi

# The following step is sensitive to special environment variables:
#   * per-invocation build metadata
#       When uploading results to build event services like ResultStore or
#       Sponge, include extra metadata that links related invocations.
#   * proxy overrides sockets for remote services
#       For infra builds, connections to various remote services are tunneled
#       through local socket relays, launched by
#       [infra/infra]/cmd/buildproxywrap/main.go.
# These environment-sensitive modifications to config definitions
# manifest in the short-lived 'invocation.bazelrc'.
# Cleanup handler called on script exit.
# Save per-invocation logs.
# Log directories are generated dynamically per fx-build invocation,
# organized by bazel invocations.
readonly _BAZEL_INVOCATION_DATE="$(date +%Y%m%d-%H%M%S)"
readonly _BAZEL_INVOCATION_LOG_DIR_NAME="invocation-${_BAZEL_INVOCATION_DATE}--${RESULTSTORE_invocation_id}"

if [[ -n "$FX_BUILD_LOGDIR" ]]; then
  readonly _BAZEL_INVOCATION_LOG_PARENT="$FX_BUILD_LOGDIR/${_BAZEL_LOGS_DIRNAME}"
else
  # This can happen if this script is run outside of `fx build`.
  readonly _BAZEL_INVOCATION_LOG_PARENT="${_NINJA_BUILD_DIR}/${_BAZEL_LOGS_DIRNAME}"
  echo >&2 "WARNING: FX_BUILD_LOGDIR is not set. Using fallback log directory: ${_BAZEL_INVOCATION_LOG_PARENT}"
fi
readonly _BAZEL_INVOCATION_LOG_DIR="${_BAZEL_INVOCATION_LOG_PARENT}/${_BAZEL_INVOCATION_LOG_DIR_NAME}"
mkdir -p "${_BAZEL_INVOCATION_LOG_DIR}"

function cleanup {
  local exit_code=$?
  rm -f "${_INVOCATION_BAZELRC}"
  # Preserve the completed invocation's command.log (stdout/stderr stream from Bazel).
  if [[ -n "${_BAZEL_INVOCATION_LOG_DIR:-}" && -d "${_BAZEL_INVOCATION_LOG_DIR}" && -f "${_BAZEL_OUTPUT_BASE}/command.log" ]]; then
    cp -f "${_BAZEL_OUTPUT_BASE}/command.log" "${_BAZEL_INVOCATION_LOG_DIR}/command.log" 2>/dev/null || true
  fi
  return "$exit_code"
}
trap cleanup EXIT

PYTHONPATH="${_FUCHSIA_DIR}${PYTHONPATH:+:${PYTHONPATH}}" \
"${_PREBUILT_PYTHON3}" -S "${_GENERATE_INVOCATION_BAZELRC}" \
  --output-bazelrc="${_INVOCATION_BAZELRC}" \
  --invocation-id="${RESULTSTORE_invocation_id}" \
  --log-dir="${_BAZEL_INVOCATION_LOG_DIR}" \
  || die "Failed to generate ${_INVOCATION_BAZELRC}"

# Maintain a 'recent' symlink to the latest log directory.
# Use a relative path for the symlink target so it works in archives.
ln -snf "${_BAZEL_INVOCATION_LOG_DIR_NAME}" "${_BAZEL_INVOCATION_LOG_PARENT}/recent"

# For convenience, link from the top_dir to the most recent "bazel_logs" dir.
# Note that _BAZEL_WORKSPACE is top_dir/workspace.
ln -snf "${_BAZEL_INVOCATION_LOG_PARENT}" "${_BAZEL_WORKSPACE}/../${_BAZEL_LOGS_DIRNAME}"

# In Corp environments with valid gcert credentials, use the credential helper
# to automatically exchange LOAS for OAuth (Google Cloud Platform) tokens.
# Note: --config=gcertauth is generated into _INVOCATION_BAZELRC when
# FX_BUILD_RBE_AUTH_TYPE or FX_BUILD_RS_AUTH_TYPE is "loas".
if [[ "${FX_BUILD_RBE_AUTH_TYPE:-}" == "loas" || "${FX_BUILD_RS_AUTH_TYPE:-}" == "loas" ]]; then
  # Don't set this, otherwise bazel will look for it
  # (and fail if it doesn't exist).
  unset GOOGLE_APPLICATION_CREDENTIALS
fi

_BAZEL_PRE_COMMAND_ARGS+=(

  # Set a max idle duration of 10 minutes (default is 3 hours)
  # To avoid Ninja build timeouts on CI. See https://fxbug.dev/498320348
  --max_idle_secs=600

  # Do not parse $HOME/.bazelrc
  --nohome_rc

  # --nosystem_rc prevent parsing /etc/bazel.bazelrc, but prints a WARNING
  # to stderr on each invocation which is annoyingly noisy. Uncomment the
  # line below once this is fixed. See https://fxbug.dev/445090005.
  # --nosystem_rc

  # Use the output base and user root specific to the current
  # Fuchsia build directory, instead of default locations that
  # are under $HOME/.bazel/
  --output_base="${_BAZEL_OUTPUT_BASE}"
  --output_user_root="${_BAZEL_OUTPUT_USER_ROOT}"

  # Parse the .bazelrc defining configs related to remote builds.
  # TODO(digit): Import this from the workspace's .bazelrc file.
  --bazelrc="${_REMOTE_SERVICES_BAZELRC}"

  # For ephemeral configuration that amends remote_services.bazelrc:
  --bazelrc="${_INVOCATION_BAZELRC}"
)

# Except when running the 'vendor' command itself, include
# .bazelrc arguments to enable the use of our custom
# Bazel registry and vendor directory.
is_vendor_command=false
for arg in "$@"; do
  case "$arg" in
    --) break ;;
    -*) ;;
    vendor) is_vendor_command=true; break ;;
    *) break ;;
  esac
done
if [[ "$is_vendor_command" == "false" ]]; then
  _BAZEL_PRE_COMMAND_ARGS+=(
    --bazelrc="${_VENDORING_BAZELRC}"
  )
fi

# Setting $USER so `bazel` won't fail in environments with fake UIDs. Even if
# the USER is not actually used. See https://fxbug.dev/42063551#c9.
# In developer environments, use the real username so that authentication
# and credential helpers will work, e.g. go/bazel-google-sso.
_user="${USER:-unused-bazel-build-user}"

_bazel_env=(
    env USER="${_user}"
    RESULTSTORE_PARENT_BUILD_ID="$RESULTSTORE_invocation_id"
)

_bazel_exec="${_BAZEL_BIN}"
_bazel_install_dir="$(dirname "${_BAZEL_BIN}")"
_bazel_install_base="${_bazel_install_dir}/install_base"
if [[ -f "${_BAZEL_BIN}-real" && -d "${_bazel_install_base}" ]]; then
  # The prebuilt `${_BAZEL_BIN}` wrapper script in CIPD unconditionally runs
  # `touch $(find "${BAZEL_INSTALL_BASE}")` on every invocation. Even when
  # mtimes are already in the future, `touch` updates ctime, which Bazel
  # includes in its FileStateValue for `@bazel_tools`
  # (`install_base/embedded_tools`) and which forces Bzlmod module resolution
  # and full package reloading on every Bazel command. Only touch files whose
  # mtime is not already in the future, and invoke `bazel-real` directly.
  # See //build/bootstrap_scripts/bazel/bazel_wrapper_script.bash.
  #
  # TODO(b/567975215): Remove this bypass once the prebuilt Bazel CIPD
  # package includes the updated bazel_wrapper_script.bash.
  #
  # The timestamps and cutoffs must match bazel_wrapper_script.bash.
  case "$OSTYPE" in
    linux*)
      find "${_bazel_install_base}" ! -newermt "2042-07-28" \
        -exec touch -d "2042-07-29 00:00:00" {} +
      ;;
    darwin*|*bsd)
      find "${_bazel_install_base}" ! -newermt "2040-02-04" \
        -exec touch -t 204002050000 {} +
      ;;
  esac
  _bazel_exec="${_BAZEL_BIN}-real"
  # These must exactly match the flags passed by bazel_wrapper_script.bash,
  # otherwise the Bazel server restarts whenever a command is run through
  # the other entry point.
  _BAZEL_PRE_COMMAND_ARGS=(
    --install_base="${_bazel_install_base}"
    --server_javabase="${_bazel_install_base}/embedded_tools/jdk"
    "${_BAZEL_PRE_COMMAND_ARGS[@]}"
  )
fi

_bazel_command=(
    "${_bazel_env[@]}"
    "${_bazel_exec}"
    "${_BAZEL_PRE_COMMAND_ARGS[@]}"
    "$@"
)

# Save the final invocation to a log file.
echo "${_bazel_command[*]}" >> "${_BAZEL_INVOCATION_LOG_DIR}/bazel_invocation"

if [[ "${FUCHSIA_BAZEL_PRINT_COMMANDS}" == "1" ]]; then
  # Print to stderr to avoid polluting stdout when command output
  # (e.g. from bazel query) is piped or redirected.
  # LINT.IfChange(bazel_command_prefix)
  _BAZEL_COMMAND_PREFIX="[bazel-command]"
  # LINT.ThenChange(//build/bazel/scripts/build_utils.py:bazel_command_prefix)
  echo >&2 "${_BAZEL_COMMAND_PREFIX} ${_bazel_command[*]}"
fi

# Wait for a process to finish, even if wait is interrupted by signals.
# Returns the exit code of the process.
function wait_for_process_exit() {
  local target_pid="$1"
  local exit_code=0
  # Loop indefinitely to keep waiting for the process to fully exit even if individual
  # wait calls are interrupted by incoming/forwarded signals. Waiting indefinitely
  # is correct because it behaves identically to standard 'wait', ensuring the wrapper
  # does not exit prematurely and leave orphaned zombie/running subprocesses.
  # If the user wishes to terminate forcefully, they can send a second SIGINT (Ctrl-C)
  # which propagates down to the child process group.
  while true; do
    # Try to wait for the child process.
    if wait "${target_pid}"; then
      # If wait succeeded (returned 0), the process exited successfully.
      exit_code=0
      break
    fi
    # If wait returned non-zero, it could be because the child failed or
    # because the wait itself was interrupted by a signal.
    exit_code=$?

    # Verify if the process is actually still alive.
    if ! kill -0 "${target_pid}" 2>/dev/null; then
      # If the child process is no longer running, wait one last time to reap
      # the zombie process and retrieve its actual exit status.
      wait "${target_pid}" && exit_code=0 || exit_code=$?
      break
    fi
    # If the process is still running, the signal merely interrupted our wait
    # call. Loop back and continue waiting.
  done
  return "${exit_code}"
}

# Wait for a command while ignoring signals to ensure the parent outlives the child.
# This prevents the shell from exiting prematurely and orphaning backgrounded
# subprocesses during a signal (like Ctrl-C).
function wait-ignoring-signals {
  local child_pid=""
  local sig_count=0

  # Acknowledge signals and forward them to the child.
  function _signal_acknowledgement_handler {
    local sig="$1"
    sig_count=$((sig_count + 1))

    if [[ $sig_count -eq 1 ]]; then
      msg "Received ${sig}. Forwarding to child and waiting for graceful shutdown..."
    else
      msg "Received ${sig} again (${sig_count}). Still waiting for cleanup..."
    fi

    if [[ -n "${child_pid}" ]]; then
       # Signal the child process group. Since we are likely in a separate
       # process group from the TTY (due to wrappers above us), we must
       # explicitly forward signals to the sub-tree.
       kill -"${sig}" "-${child_pid}" 2>/dev/null || true
    fi
  }
  trap '_signal_acknowledgement_handler SIGINT' INT
  trap '_signal_acknowledgement_handler SIGTERM' TERM
  trap '_signal_acknowledgement_handler SIGHUP' HUP

  # Run the command in its own process group.
  # This makes it easier to signal the entire sub-tree (including Bazel).
  set -m
  "$@" &
  child_pid=$!
  set +m

  local status=0
  wait_for_process_exit "${child_pid}" && status=0 || status=$?

  trap - INT TERM HUP
  return "$status"
}

cd "${_BAZEL_WORKSPACE}" && wait-ignoring-signals "${_bazel_command[@]}"
