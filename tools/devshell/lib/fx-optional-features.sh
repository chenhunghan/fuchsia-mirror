#!/bin/bash
# Copyright 2020 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Single source of truth for optional features, read by both bash functions
# below and tools/devshell/fx_cli/fx_builtins.py.
# Format: "<feature_name>|<default:enabled|disabled>|<help_line>"
# Repeat a feature row for multi-line help descriptions.
_FX_OPTIONAL_FEATURES_SPEC=(
  "fxtest_auto_publishes_packages|enabled|Enables 'fx test' to automatically publish packages to the package"
  "fxtest_auto_publishes_packages|enabled|repository server."
  "incremental|enabled|Packages are published and served incrementally by 'fx serve'"
  "incremental|enabled|as they are built. Explicit 'fx build' is not required for most operations."
  "incremental_legacy|disabled|Packages are published and served incrementally by 'fx serve'"
  "incremental_legacy|disabled|as they are built using the legacy incremental publisher. Explicit"
  "incremental_legacy|disabled|'fx build' is not required for most operations."
  "incremental_new|disabled|Packages are published and served incrementally by 'fx serve'"
  "incremental_new|disabled|as they are built. Explicit 'fx build' is not required for most operations."
  "legacy_fastboot|disabled|Device provisioning based on legacy fastboot instead of ffx."
)

function list_optional_features {
  local entry name prev="" out=()
  for entry in "${_FX_OPTIONAL_FEATURES_SPEC[@]}"; do
    name="${entry%%|*}"
    if [[ "${name}" != "${prev}" ]]; then
      out+=("${name}")
      prev="${name}"
    fi
  done
  echo "${out[@]}"
}

# Return 0 (true) if the default of the given feature is enabled, 1 (false) otherwise.
function is_feature_enabled_by_default {
  local entry name rest default_state
  for entry in "${_FX_OPTIONAL_FEATURES_SPEC[@]}"; do
    name="${entry%%|*}"
    if [[ "${name}" == "$1" ]]; then
      rest="${entry#*|}"
      default_state="${rest%%|*}"
      [[ "${default_state}" == "enabled" ]]
      return $?
    fi
  done

  # global default is enabled
  return 0
}

_FX_INDENT="      "
function help_optional_feature {
  local entry name rest help_line h=()
  for entry in "${_FX_OPTIONAL_FEATURES_SPEC[@]}"; do
    name="${entry%%|*}"
    if [[ "${name}" == "$1" ]]; then
      rest="${entry#*|}"
      help_line="${rest#*|}"
      h+=("${help_line}")
    fi
  done
  printf "${_FX_INDENT}${_FX_INDENT}%s\n" "${h[@]}"
}

function help_optional_features {
  for el in $(list_optional_features); do
    local default current
    is_feature_enabled_by_default $el && default="enabled" || default="disabled"
    is_feature_enabled $el && current="enabled" || current="disabled"
    echo "${_FX_INDENT}${el}"
    echo "${_FX_INDENT}${_FX_INDENT}(default=$default, current=$current)"
    help_optional_feature $el
  done
}

function warn_if_feature_enabled {
  local feature
  feature="$1"
  if is_feature_enabled "$feature"; then
    echo "Feature \"$feature\" is enabled."
  fi
}

function is_valid_feature {
  local el feature
  feature="$1"
  for el in $(list_optional_features); do
    if [[ "${el}" == "${feature}" ]]; then
      return 0
    fi
  done
  return 1
}

function get_fx_flags_non_default_features {
  local out=()
  for el in $(list_optional_features); do
    local default=false
    local current=false
    is_feature_enabled_by_default $el && default=true
    is_feature_enabled $el && current=true
    if $default && ! $current; then
      out+=("--disable=$el")
    elif ! $default && $current; then
      out+=("--enable=$el")
    fi
  done
  echo "${out[@]}"
}

function get_disable_feature_env_name {
  local feature
  feature="$1"
  echo "FUCHSIA_DISABLED_${feature}"
}

# Return code 0 (true) if the given feature is enabled, 1 (false) otherwise.
function is_feature_enabled {
  local feature
  feature="$1"
  if ! is_valid_feature "${feature}"; then
    return 1
  fi
  local disabled_env
  disabled_env="$(get_disable_feature_env_name "${feature}")"

  # if ${disabled_env} is not set, use the default
  if [[ -z ${!disabled_env+x} ]]; then
    is_feature_enabled_by_default "$feature"
  else
    test "${!disabled_env}" != "1"
  fi
}

function get_env_vars_non_default_features {
  local out=()
  for el in $(list_optional_features); do
    local default=false
    local current=false
    is_feature_enabled_by_default $el && default=true
    is_feature_enabled $el && current=true
    if $default && ! $current; then
      out+=("$(get_disable_feature_env_name $el)=1")
    elif ! $default && $current; then
      out+=("$(get_disable_feature_env_name $el)=0")
    fi
  done
  echo "${out[@]}"
}
