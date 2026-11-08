#!/bin/bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

#### CATEGORY=Build
### get the output path to the provided product bundle

## usage: fx get-main-pb-path //products/minimal:product_bundle.x64

set -euo pipefail

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  sed -n 's/^## //p' "$0"
  exit 0
fi

fx-error() {
  echo >&2 "ERROR: $*"
}

if [[ -z "${FUCHSIA_DIR:-}" ]]; then
  _dir="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" >/dev/null 2>&1 && pwd || pwd)"
  while [[ "${_dir}" != "/" ]]; do
    if [[ -d "${_dir}/.jiri_root" || -f "${_dir}/tools/devshell/lib/vars.sh" ]]; then
      FUCHSIA_DIR="${_dir}"
      break
    fi
    _dir="$(dirname "${_dir}")"
  done
  unset _dir
fi

rel_build_dir="${FUCHSIA_BUILD_DIR:-${_FX_BUILD_DIR:-${FUCHSIA_BUILD_DIR_FROM_FX:-}}}"
if [[ -z "${rel_build_dir}" && -n "${FUCHSIA_DIR:-}" && -f "${FUCHSIA_DIR}/.fx-build-dir" ]]; then
  rel_build_dir="$(tr -d '\r\n' < "${FUCHSIA_DIR}/.fx-build-dir")"
fi
rel_build_dir="${rel_build_dir#//}"
if [[ -z "${rel_build_dir}" ]]; then
  fx-error "Cannot find build directory, run \"fx set\" first."
  exit 1
fi
if [[ "${rel_build_dir}" == /* ]]; then
  FUCHSIA_BUILD_DIR="${rel_build_dir}"
elif [[ -n "${FUCHSIA_DIR:-}" ]]; then
  FUCHSIA_BUILD_DIR="${FUCHSIA_DIR}/${rel_build_dir}"
else
  fx-error "FUCHSIA_DIR is not set and build directory is relative: ${rel_build_dir}"
  exit 1
fi

if [[ ! -d "${FUCHSIA_BUILD_DIR}" ]]; then
  fx-error "Build directory ${FUCHSIA_BUILD_DIR} does not exist, run \"fx set\" first."
  exit 1
fi

if [[ -z "${FX_RUNTIME_TOOLS_DIR:-}" || ! -x "${FX_RUNTIME_TOOLS_DIR}/jq" ]]; then
  fx-error "Missing bundled jq runtime tool in FX_RUNTIME_TOOLS_DIR."
  exit 1
fi

readonly jq_bin="${FX_RUNTIME_TOOLS_DIR}/jq"
readonly product_bundles_path="${FUCHSIA_BUILD_DIR}/product_bundles.json"
readonly args_json_path="${FUCHSIA_BUILD_DIR}/args.json"

main_pb_label=""
product_bundle=""

# If an argument was provided, assume that it is the label for the main PB.
if [[ $# -gt 1 ]]; then
  fx-error "Too many arguments."
  echo >&2 "Usage: fx get-main-pb-path [main_pb_label]"
  exit 2
elif [[ $# -eq 1 ]]; then
  main_pb_label="$1"
else
  main_pb_label="$("${jq_bin}" -r '.main_pb_label // empty' "${args_json_path}")"
fi

# Find the product bundle that is built from main_pb_label.
if [[ -n "${main_pb_label}" ]]; then
  product_bundle="$("${jq_bin}" -r --arg label "${main_pb_label}" '.[] | select(.label | startswith($label)) | .path' "${product_bundles_path}")"
else
  # Look in product_bundles.json for a product bundle named build_info_product.build_info_board.
  product="$("${jq_bin}" -r '.build_info_product // empty' "${args_json_path}")"
  board="$("${jq_bin}" -r '.build_info_board // empty' "${args_json_path}")"
  product_name="${product}.${board}"
  product_bundle="$("${jq_bin}" -r --arg name "${product_name}" '.[] | select(.name == $name) | .path' "${product_bundles_path}")"
fi

if [[ -z "${product_bundle}" ]]; then
  echo "No matching product found!"
  exit 1
fi

printf '%s\n' "${product_bundle}"
