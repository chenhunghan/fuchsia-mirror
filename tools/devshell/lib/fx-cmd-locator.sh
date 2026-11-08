#!/bin/bash
# Copyright 2020 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

function get_host_tools_dir {
  local -r build_dir="$(fx-build-dir-if-present && echo "${FUCHSIA_BUILD_DIR}")"
  local -r host_tools="${build_dir}/host-tools"
  if [[ -d "${build_dir}" && -d "${host_tools}"  ]]; then
    echo "${host_tools}"
  fi
}

function fx-ensure-prebuilt {
  if [[ $# -eq 0 ]]; then
    fx-error "fx-ensure-prebuilt requires at least one argument"
    return 1
  fi

  local item rel_path env_var
  local prebuilt_root="${FUCHSIA_DIR}/prebuilt"
  local packages_to_fetch=()

  for item in "$@"; do
    if [[ -z "${item}" ]]; then
      fx-error "Empty path passed to fx-ensure-prebuilt"
      return 1
    fi

    if [[ "${item}" == "${prebuilt_root}/"* ]]; then
      rel_path="prebuilt/${item#"${prebuilt_root}/"}"
    elif [[ "${item}" == //prebuilt/* ]]; then
      rel_path="prebuilt/${item#//prebuilt/}"
    elif [[ "${item}" == prebuilt/* ]]; then
      rel_path="${item}"
    else
      fx-error "Invalid prebuilt path '${item}': paths passed to fx-ensure-prebuilt must be in //prebuilt"
      return 1
    fi

    # Skip if already loaded earlier in this fx process tree or earlier in "$@"
    env_var="_FX_LOADED_DEP_${rel_path//[^a-zA-Z0-9_]/_}"
    if [[ -n "${!env_var:-}" ]]; then
      continue
    fi
    export "${env_var}=1"
    packages_to_fetch+=("${rel_path}")
  done

  if [[ ${#packages_to_fetch[@]} -eq 0 ]]; then
    return 0
  fi

  # TODO: FX_ON_DEMAND_PREBUILTS_LOG_FETCHES is a temporary placeholder used to
  # verify on-demand prebuilt resolution before `jiri fetch-package` is
  # implemented. Remove this variable and log file once `fx-ensure-prebuilt`
  # calls `jiri fetch-package` directly.
  if [[ -n "${FX_ON_DEMAND_PREBUILTS_LOG_FETCHES:-}" && "${FX_ON_DEMAND_PREBUILTS_LOG_FETCHES}" != "0" && "${FX_ON_DEMAND_PREBUILTS_LOG_FETCHES}" != "false" ]]; then
    local log_file="${FUCHSIA_DIR}/out/.on-demand-fetch-log"
    mkdir -p "${FUCHSIA_DIR}/out"
    echo "jiri fetch-package ${packages_to_fetch[*]}" >> "${log_file}"
  fi
  return 0
}

function _relative {
  local -r cmd="$1"
  if [[ "${cmd}" == *"${FUCHSIA_DIR}"* ]]; then
    echo "//${cmd#${FUCHSIA_DIR}/}"
  else
    echo "${cmd}"
  fi
}

function _fx_registry_log_enabled {
  [[ -n "${FX_OUTPUT_REGISTRY_ACTIONS:-}" && \
     "${FX_OUTPUT_REGISTRY_ACTIONS}" != "0" && \
     "${FX_OUTPUT_REGISTRY_ACTIONS}" != "false" ]]
}

function _fx_registry_log {
  if _fx_registry_log_enabled; then
    printf '[fx-registry] %s\n' "$*" >&2
  fi
}

function _fx_all_prebuilt_paths {
  local -r var="PREBUILT_""ALL_PATHS"
  printf '%s' "${!var:-}"
}

function _fx_ensure_python3_bin {
  local -r var="PREBUILT_""PYTHON3_DIR"
  local -r py_dir="${!var:-}"
  if [[ -n "${py_dir}" && -x "${py_dir}/bin/python3" ]]; then
    fx-ensure-prebuilt "${py_dir}" || return $?
    printf '%s' "${py_dir}/bin/python3"
    return 0
  fi
  command -v python3
}

function _fx_cli_locator {
  local -r locator_py="${FUCHSIA_DIR}/tools/devshell/fx_cli/locator.py"
  [[ -f "${locator_py}" ]] || return 127
  local py
  py="$(_fx_ensure_python3_bin)" || return 1
  "${py}" "${locator_py}" --fuchsia-dir "${FUCHSIA_DIR}" "$@"
}

function _fx_cli_registry {
  local -r registry_py="${FUCHSIA_DIR}/tools/devshell/fx_cli/registry.py"
  [[ -f "${registry_py}" ]] || return 127
  local py
  py="$(_fx_ensure_python3_bin)" || return 1
  "${py}" "${registry_py}" "$@"
}

function is_registered_host_tool {
  local -r cmd_name="$1"
  local -r vendor="${2:-}"
  _get_registered_host_tool_lines "${cmd_name}" "${vendor}" >/dev/null 2>&1
}

function _get_registered_host_tool_lines {
  local -r cmd_name="$1"
  local -r vendor="${2:-}"
  local -a args=("resolve" "${cmd_name}")
  if [[ -n "${vendor}" ]]; then
    args+=("--vendor" "${vendor}")
  fi
  local -a out_lines=()
  local out_str="" rc=0
  out_str="$(_fx_cli_locator "${args[@]}")" || rc=$?
  if [[ ${rc} -eq 2 ]]; then
    return 2
  elif [[ ${rc} -ne 0 || -z "${out_str}" ]]; then
    return 1
  fi
  mapfile -t out_lines <<< "${out_str}"
  local kind="" target="" script="" inject="0"
  for line in "${out_lines[@]}"; do
    case "${line}" in
      KIND=*) kind="${line#KIND=}" ;;
      TARGET=*) target="${line#TARGET=}" ;;
      SCRIPT=*) script="${line#SCRIPT=}" ;;
      INJECT_PREBUILT_PATHS=*) inject="${line#INJECT_PREBUILT_PATHS=}" ;;
    esac
  done
  if [[ "${kind}" != registered_* ]]; then
    return 1
  fi
  local inj_str="false"
  [[ "${inject}" == "1" ]] && inj_str="true"
  printf '%s\n%s\n%s\n' "${target}" "${script}" "${inj_str}"
  return 0
}

function get_registered_host_tool_target {
  local -r cmd_name="$1"
  local -r vendor="${2:-}"
  local -a args=("resolve" "${cmd_name}")
  if [[ -n "${vendor}" ]]; then
    args+=("--vendor" "${vendor}")
  fi
  local -a out_lines=()
  local out_str="" rc=0
  out_str="$(_fx_cli_locator "${args[@]}")" || rc=$?
  if [[ ${rc} -eq 2 ]]; then
    return 2
  elif [[ ${rc} -ne 0 || -z "${out_str}" ]]; then
    return 1
  fi
  mapfile -t out_lines <<< "${out_str}"
  local target=""
  for line in "${out_lines[@]}"; do
    if [[ "${line}" == TARGET=* ]]; then
      target="${line#TARGET=}"
    fi
  done
  if [[ -n "${target}" ]]; then
    echo "${target}"
    return 0
  fi
  return 1
}

function get_registered_host_tool_script {
  local -r cmd_name="$1"
  local -r vendor="${2:-}"
  local -a args=("resolve" "${cmd_name}")
  if [[ -n "${vendor}" ]]; then
    args+=("--vendor" "${vendor}")
  fi
  local -a out_lines=()
  local out_str="" rc=0
  out_str="$(_fx_cli_locator "${args[@]}")" || rc=$?
  if [[ ${rc} -eq 2 ]]; then
    return 2
  elif [[ ${rc} -ne 0 || -z "${out_str}" ]]; then
    return 1
  fi
  mapfile -t out_lines <<< "${out_str}"
  local script=""
  for line in "${out_lines[@]}"; do
    if [[ "${line}" == SCRIPT=* ]]; then
      script="${line#SCRIPT=}"
    fi
  done
  if [[ -n "${script}" ]]; then
    echo "${script}"
    return 0
  fi
  return 1
}

function _registered_host_tool_install_dir {
  local -r vendor="${1:-}"
  local -r out_root="${FUCHSIA_OUT_DIR:-${FUCHSIA_DIR}/out}"
  if [[ -n "${vendor}" ]]; then
    echo "${out_root}/_fx_host_tools/vendor/${vendor}"
  else
    echo "${out_root}/_fx_host_tools"
  fi
}

function get_registered_host_tool_install_path {
  local -r cmd_name="$1"
  local -r vendor="${2:-}"
  local -a args=("resolve" "${cmd_name}")
  if [[ -n "${vendor}" ]]; then
    args+=("--vendor" "${vendor}")
  fi
  local -a out_lines=()
  local out_str="" rc=0
  out_str="$(_fx_cli_locator "${args[@]}")" || rc=$?
  if [[ ${rc} -eq 2 ]]; then
    return 2
  elif [[ ${rc} -ne 0 || -z "${out_str}" ]]; then
    return 1
  fi
  mapfile -t out_lines <<< "${out_str}"
  local target="" script=""
  for line in "${out_lines[@]}"; do
    case "${line}" in
      TARGET=*) target="${line#TARGET=}" ;;
      SCRIPT=*) script="${line#SCRIPT=}" ;;
    esac
  done
  if [[ -n "${script}" ]]; then
    local clean_script="${script#@//}"
    clean_script="${clean_script#//}"
    echo "${FUCHSIA_DIR}/${clean_script}"
    return 0
  fi
  [[ -n "${target}" ]] || return 1
  local -r install_dir="$(_registered_host_tool_install_dir "${vendor}")"
  echo "${install_dir}/${cmd_name}"
}

function list_registered_host_tools {
  local -r vendor="${1:-}"
  if [[ -n "${vendor}" ]]; then
    _fx_cli_registry list --format=names --vendor "${vendor}" || true
  else
    _fx_cli_registry list --format=names || true
  fi
}

function list_registered_host_tools_help_records {
  local -r vendor="${1:-}"
  if [[ -n "${vendor}" ]]; then
    _fx_cli_registry list --format=help-tsv --vendor "${vendor}" || true
  else
    _fx_cli_registry list --format=help-tsv || true
  fi
}

function _fx_strip_prebuilt_paths_from_path {
  local in_path="$1"
  local -r all_prebuilt="$(_fx_all_prebuilt_paths)"
  if [[ -z "${in_path}" ]]; then
    printf '%s' "${in_path}"
    return 0
  fi
  local -r wrapped_prebuilt=":${all_prebuilt}:"
  local -a parts kept=()
  local part
  IFS=':' read -r -a parts <<< "${in_path}"
  for part in "${parts[@]}"; do
    [[ -n "${part}" ]] || continue
    if [[ -n "${all_prebuilt}" && "${wrapped_prebuilt}" == *":${part}:"* ]]; then
      continue
    fi
    if [[ "${part}" == *".runtime_dir/.bin"* || "${part}" == *".runtime_dir/bin"* ]]; then
      continue
    fi
    kept+=("${part}")
  done
  local IFS=':'
  printf '%s' "${kept[*]}"
}

function _fx_compute_command_path {
  local -r inject="$1"
  local -r base_path="${2:-${PATH}}"
  local stripped
  stripped="$(_fx_strip_prebuilt_paths_from_path "${base_path}")"
  if [[ "${inject}" == "1" ]]; then
    local -r all_prebuilt="$(_fx_all_prebuilt_paths)"
    if [[ -n "${all_prebuilt}" ]]; then
      printf '%s' "${all_prebuilt}${stripped:+:${stripped}}"
    else
      printf '%s' "${stripped}"
    fi
  else
    printf '%s' "${stripped}"
  fi
}

function _fx_apply_prebuilt_paths {
  local -r inject="$1"
  if [[ "${inject}" == "1" ]]; then
    export FX_INJECT_PREBUILT_PATHS=1
  else
    export FX_INJECT_PREBUILT_PATHS=0
  fi
  local new_path
  new_path="$(_fx_compute_command_path "${FX_INJECT_PREBUILT_PATHS}" "${PATH}")"
  export PATH="${new_path}"
}

function resolve_fx_command_dispatch {
  local -r cmd_name="$1"
  local -r req_vendor="${2:-}"
  [[ -n "${cmd_name}" ]] || return 1

  local build_dir=""
  if fx-build-dir-if-present >/dev/null 2>&1; then
    build_dir="${FUCHSIA_BUILD_DIR:-}"
  fi

  local -a args=("resolve" "${cmd_name}")
  if [[ -n "${req_vendor}" ]]; then
    args+=("--vendor" "${req_vendor}")
  fi
  if [[ -n "${build_dir}" ]]; then
    args+=("--build-dir" "${build_dir}")
  fi

  local -a out_lines=()
  local out_str="" rc=0
  out_str="$(_fx_cli_locator "${args[@]}")" || rc=$?
  if [[ ${rc} -eq 127 ]]; then
    local legacy_cmd is_ht=0
    if [[ -n "${req_vendor}" ]]; then
      legacy_cmd="$(commands vendor "${req_vendor}" "${cmd_name}")"
      if [[ -n "${legacy_cmd}" ]]; then
        if _fx_registry_log_enabled; then
          _fx_registry_log "lookup '${cmd_name}': not in registry; falling back to legacy vendor path $(_relative "${legacy_cmd%%$'\n'*}")"
        fi
        printf 'legacy\n%s\n\n%s\n1\n' "${req_vendor}" "${legacy_cmd%%$'\n'*}"
        return 0
      fi
    else
      legacy_cmd="$(commands "${cmd_name}")"
      if [[ -z "${legacy_cmd}" ]]; then
        legacy_cmd="$(find_host_tools "${cmd_name}")"
        is_ht=1
      fi
      if [[ -n "${legacy_cmd}" ]]; then
        if _fx_registry_log_enabled; then
          if [[ ${is_ht} -eq 1 ]]; then
            _fx_registry_log "lookup '${cmd_name}': not in registry; falling back to legacy host-tools binary $(_relative "${legacy_cmd%%$'\n'*}")"
          else
            _fx_registry_log "lookup '${cmd_name}': not in registry; falling back to legacy path $(_relative "${legacy_cmd%%$'\n'*}")"
          fi
        fi
        printf 'legacy\n\n\n%s\n1\n' "${legacy_cmd%%$'\n'*}"
        return 0
      fi
    fi
    _fx_registry_log "lookup '${cmd_name}': not found in registry or legacy paths"
    return 1
  elif [[ ${rc} -eq 2 ]]; then
    return 2
  elif [[ ${rc} -ne 0 || -z "${out_str}" ]]; then
    _fx_registry_log "lookup '${cmd_name}': not found in registry or legacy paths"
    return 1
  fi
  mapfile -t out_lines <<< "${out_str}"

  local kind="" vendor="" target="" script="" exec_path="" inject="1" category="" summary="" mode=""
  local line
  for line in "${out_lines[@]}"; do
    case "${line}" in
      KIND=*) kind="${line#KIND=}" ;;
      VENDOR=*) vendor="${line#VENDOR=}" ;;
      TARGET=*) target="${line#TARGET=}" ;;
      SCRIPT=*) script="${line#SCRIPT=}" ;;
      EXEC_PATH=*) exec_path="${line#EXEC_PATH=}" ;;
      INJECT_PREBUILT_PATHS=*) inject="${line#INJECT_PREBUILT_PATHS=}" ;;
      CATEGORY=*) category="${line#CATEGORY=}" ;;
      SUMMARY=*) summary="${line#SUMMARY=}" ;;
      MODE=*) mode="${line#MODE=}" ;;
    esac
  done

  if [[ "${kind}" == "not_found" ]]; then
    _fx_registry_log "lookup '${cmd_name}': not found in registry or legacy paths"
    return 1
  fi

  if [[ "${kind}" == "registered_script" || "${kind}" == "registered_target" ]]; then
    if [[ -n "${vendor}" ]]; then
      if [[ "${kind}" == "registered_target" ]]; then
        _fx_registry_log "lookup '${cmd_name}': found in vendor '${vendor}' registry (mode=target, target=${target})"
      else
        _fx_registry_log "lookup '${cmd_name}': found in vendor '${vendor}' registry (mode=script, script=${script})"
      fi
    else
      if [[ "${kind}" == "registered_target" ]]; then
        _fx_registry_log "lookup '${cmd_name}': found in core registry (mode=target, target=${target})"
      else
        _fx_registry_log "lookup '${cmd_name}': found in core registry (mode=script, script=${script})"
      fi
    fi
    printf 'registered\n%s\n%s\n%s\n%s\n%s\n%s\n' "${vendor}" "${target}" "${script}" "${inject}" "${category}" "${summary}"
  else
    if _fx_registry_log_enabled; then
      if [[ -n "${vendor}" ]]; then
        _fx_registry_log "lookup '${cmd_name}': not in vendor registry; falling back to legacy vendor path $(_relative "${exec_path}")"
      elif [[ "${kind}" == "legacy_host_tool" || "${mode:-}" == "legacy_host_tool" ]]; then
        _fx_registry_log "lookup '${cmd_name}': not in registry; falling back to legacy host-tools binary $(_relative "${exec_path}")"
      else
        _fx_registry_log "lookup '${cmd_name}': not in registry; falling back to legacy path $(_relative "${exec_path}")"
      fi
    fi
    printf 'legacy\n%s\n\n%s\n1\n' "${vendor}" "${exec_path}"
  fi
  return 0
}

function _find_registered_executable_from_entry {
  local -r cmd_name="$1"
  local -r target="$2"
  local -r script_rel="$3"
  local -r vendor="${4:-}"
  if [[ -n "${script_rel}" ]]; then
    local clean_script="${script_rel#@//}"
    clean_script="${clean_script#//}"
    local script_path="${FUCHSIA_DIR}/${clean_script}"
    if [[ "${script_path}" == *.py ]]; then
      local py_bin
      py_bin="$(_fx_ensure_python3_bin)" || return 1
      printf '%s\n' "${py_bin}" "${script_path}"
      [[ -x "${py_bin}" && -f "${script_path}" ]]
      return $?
    fi
    printf '%s\n' "${script_path}"
    [[ -x "${script_path}" ]]
    return $?
  fi
  if [[ -n "${target}" ]]; then
    local registered_path="$(_registered_host_tool_install_dir "${vendor}")/${cmd_name}"
    printf '%s\n' "${registered_path}"
    [[ -x "${registered_path}" ]]
    return $?
  fi
  return 1
}

function _fx_cli_builder {
  local -r builder_py="${FUCHSIA_DIR}/tools/devshell/fx_cli/builder.py"
  [[ -f "${builder_py}" ]] || return 127
  local py
  py="$(_fx_ensure_python3_bin)" || return 1
  "${py}" "${builder_py}" --fuchsia-dir "${FUCHSIA_DIR}" "$@"
}

function fx-ensure-registered-host-tool {
  local -r cmd_name="$1"
  local -r vendor="${2:-}"
  local target="${3:-}"
  local script_rel="${4:-}"
  if [[ -z "${target}" && -z "${script_rel}" ]]; then
    local lines
    lines="$(_get_registered_host_tool_lines "${cmd_name}" "${vendor}")" || return $?
    local -a fields
    mapfile -t fields <<< "${lines}"
    target="${fields[0]:-}"
    script_rel="${fields[1]:-}"
  fi
  if [[ -n "${script_rel}" ]]; then
    local clean_script="${script_rel#@//}"
    clean_script="${clean_script#//}"
    local script_path="${FUCHSIA_DIR}/${clean_script}"
    if [[ "${script_path}" == *.py ]]; then
      if [[ ! -f "${script_path}" ]]; then
        fx-error "Registered script '${script_rel}' for '${cmd_name}' does not exist at ${script_path}"
        return 1
      fi
      _fx_ensure_python3_bin >/dev/null || return $?
      _fx_registry_log "skip build '${cmd_name}': script mode, running $(_relative "${script_path}") directly"
      return 0
    fi
    if [[ ! -x "${script_path}" ]]; then
      fx-error "Registered script '${script_rel}' for '${cmd_name}' is missing or not executable at ${script_path}"
      return 1
    fi
    _fx_registry_log "skip build '${cmd_name}': script mode, running $(_relative "${script_path}") directly"
    return 0
  fi

  if [[ -n "${target}" ]]; then
    local -a builder_args=(--target "${target}")
    if fx-build-dir-if-present && [[ -n "${FUCHSIA_BUILD_DIR:-}" ]]; then
      [[ "${FUCHSIA_BUILD_DIR:0:1}" != "/" ]] && FUCHSIA_BUILD_DIR="${FUCHSIA_DIR}/${FUCHSIA_BUILD_DIR}"
      builder_args+=(--build-dir "${FUCHSIA_BUILD_DIR}")
    fi
    if [[ -n "${vendor}" ]]; then
      builder_args+=(--vendor "${vendor}")
    fi
    _fx_cli_builder "${builder_args[@]}" "${cmd_name}" >/dev/null
    return $?
  fi

  return 1
}

function _get_metadata_values {
  local -r key=$1
  local -r metadata_file=$2
  local -r unknown_vars=$3

  export FUCHSIA_DIR HOST_PLATFORM
  if [[ -n "${!PREBUILT_*}" ]]; then
    export "${!PREBUILT_@}"
  fi

  awk -F ' *= *' -v key="${key}" -v unknown_vars="${unknown_vars}" \
      -f - "${metadata_file}" <<'EOF'
    $0 ~ ("^#### +" key " *=") {
      line = $2;
      out = "";
      while (match(line, /\$\{[A-Za-z0-9_]+\}/)) {
        var = substr(line, RSTART + 2, RLENGTH - 3);
        if (var in ENVIRON) {
          val = ENVIRON[var];
        } else if (unknown_vars == "keep") {
          val = substr(line, RSTART, RLENGTH);
        } else {
          val = "";
        }
        out = out substr(line, 1, RSTART - 1) val;
        line = substr(line, RSTART + RLENGTH);
      }
      print out line;
    }

    !/^#/ && NF > 0 { exit }
EOF
}

function get_exec_from_metadata {
  local -r metadata_file=$1

  export HOST_TOOLS_DIR="$(get_host_tools_dir)"
  _get_metadata_values EXECUTABLE "${metadata_file}" keep
}

function get_prebuilts_from_metadata {
  local -r metadata_file=$1
  [[ -f "${metadata_file}" ]] || return 0

  _get_metadata_values PREBUILTS "${metadata_file}" empty | tr -s '[:space:],;' '\n'
}

function find_executable {
  local cmd_name="$1"
  local vendor="${2:-}"
  local dispatch
  dispatch="$(resolve_fx_command_dispatch "${cmd_name}" "${vendor}")" || return $?
  local -a df
  mapfile -t df <<< "${dispatch}"
  if [[ "${df[0]:-}" == "registered" ]]; then
    _find_registered_executable_from_entry "${cmd_name}" "${df[2]:-}" "${df[3]:-}" "${df[1]:-}"
    return $?
  fi
  find_exec_from_path "${df[3]:-}"
}

function find_exec_from_path {
  local cmd_path="$1"
  cmd_path="${cmd_path%.fx}"
  local fx_file_path="${cmd_path}.fx"
  local prebuilts_to_ensure=(
    $(get_prebuilts_from_metadata "${fx_file_path}")
    $(get_prebuilts_from_metadata "${cmd_path}")
  )

  local target_exec="${cmd_path}"
  local from_metadata=""
  if [[ -f "${fx_file_path}" ]]; then
    from_metadata="$(get_exec_from_metadata "${fx_file_path}")"
    if [[ -n "${from_metadata}" ]]; then
      target_exec="${from_metadata%% *}"
    fi
  fi

  if [[ "${target_exec}" == "${FUCHSIA_DIR}/prebuilt/"* ]]; then
    prebuilts_to_ensure+=("${target_exec}")
  fi
  if [[ ${#prebuilts_to_ensure[@]} -gt 0 ]]; then
    fx-ensure-prebuilt "${prebuilts_to_ensure[@]}" || return 1
  fi

  if [[ -n "${from_metadata}" ]]; then
    echo "${from_metadata}"
    if [[ -f "${cmd_path}" && ! "${cmd_path}" -ef "${from_metadata}" ]]; then
      fx-error "Invalid ${fx_file_path}: if both ${basename_exec} and EXECUTABLE metadata exist, they must point to the same file"
      return 1
    fi
  else
    echo "${cmd_path}"
    if [[ ! -x "${cmd_path}" ]]; then
      return 1
    fi
  fi
}

function find_execs_and_metadata {
  local cmd_name=$1
  shift

  if [[ -z "${cmd_name}" ]]; then
    cmd_name="*"
  fi
  local dirs=()
  for d in "$@"; do
    if [[ -d "${d}" ]]; then
      dirs+=( "${d}" )
    fi
  done
  if [[ ${#dirs[@]} -eq 0 ]]; then
    return 0
  fi

  cmds="$( find "${dirs[@]}" -maxdepth 1 -type f \
    \( -executable -name "${cmd_name}" \) -o \
    \( -name "${cmd_name}.fx" \) 2>/dev/null )"

  if [[ $? -ne 0 ]]; then
    cmds="$( find "${dirs[@]}" -maxdepth 1 -type f \
      \( -perm +100 -name "${cmd_name}" \) -o \
      \( -name "${cmd_name}.fx" \) 2>/dev/null )"
    if [[ $? -ne 0 ]]; then
      {
        echo "ERROR: 'find' failed unexpectedly, please execute fx with '-x' and report a bug."
      } >&2
      exit 1
    fi
  fi
  echo "${cmds}"
}

function find_host_tools {
  local cmd_name=$1
  local -r host_tools="$(get_host_tools_dir)"
  if [[ -z "${host_tools}" ]]; then
    return
  fi
  cmds="$(commands | tr '\n' ';')"
  local registered_names
  if [[ $# -ge 2 ]]; then
    registered_names=";""$(printf '%s' "$2" | tr '\n' ';')"
  else
    registered_names=";""$(list_registered_host_tools | tr '\n' ';')"
  fi

  binaries=()
  for binary in $(find_execs_and_metadata "${cmd_name}" "${host_tools}"); do
    name="${binary##*/}"
    if [[ "${registered_names}" == *";${name};"* ]]; then
      continue
    fi
    if [[ "${cmds}" != */${name}.fx\;* && "${cmds}" != */${name}\;* ]]; then
      binaries+=( "${binary}" )
    fi
  done
  echo "${binaries[@]}"
}

function commands {
  local cmd_name=${1:-}
  local dirs
  if [[ "${cmd_name}" == "vendor" && $# -eq 3 ]]; then
    vendor=$2
    cmd_name=$3
    dirs=("${FUCHSIA_DIR}"/vendor/${vendor}/scripts/devshell)
  elif [[ "${cmd_name}" == "vendor" && $# -eq 2 ]]; then
    vendor=$2
    cmd_name=""
    dirs=("${FUCHSIA_DIR}"/vendor/${vendor}/scripts/devshell)
  else
    dirs=("${FUCHSIA_DIR}"/vendor/*/scripts/devshell "${FUCHSIA_DIR}"/tools/devshell{,/contrib})
  fi

  find_execs_and_metadata "${cmd_name}" "${dirs[@]}"
}
