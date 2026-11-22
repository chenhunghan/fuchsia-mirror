#!/usr/bin/env bash
#
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Wraps a Python script for use via `fuchsia_post_processing_script()`.
#
# This serves two main purposes:
#  1. Calls the Python script with the correct build-time interpreter provided
#     by `fuchsia_post_processing_script()`
#  2. Translates paths so that the Python script can use all paths as-is without
#     having to apply any path manipulation.
#
# Usage:
#
# ```
# fuchsia_post_processing_script(
#     # Name can be anything.
#     name = "foo_post_processing",
#
#     # The first arg must be the Python script to call into, the remaining
#     # args are passed through.
#     #
#     # Assembly will additionally inject these args:
#     #  1. `-z <zbi>`: the path to the ZBI to post-process
#     #  2. `-o <output>`: the path to write the resulting image
#     post_processing_script_args = ["foo_script.py", "--foo", "bar"],
#
#     # Both this wrapper and the Python script must be provided as inputs, as
#     # well as any other resources the Python script needs.
#     post_processing_script_inputs = {
#         "//src/firmware/tools:post_processing_python_wrapper.sh": "post_processing_python_wrapper.sh",
#         "//path/to/script:foo_script.py": "foo_script.py",
#     },
#
#     # Point to this file as the entry point.
#     post_processing_script_path = "post_processing_python_wrapper.sh",
#
#     # Required for us to get the correct build-time Python interpreter.
#     use_python = True,
# )
# ````
#
# Chaining:
#
# Multiple Python scripts can be chained by separating them with `--then` in
# `post_processing_script_args`. The first script reads assembly's ZBI, each
# script's output is fed to the next script's input, and the last script writes
# assembly's output.
#
# There are two ways a script can receive and produce the image:
#
#  1. `-z`/`-o` (default): the wrapper appends `-z <input> -o <output>`, the
#     same contract as a standalone post-processing script.
#
#  2. In-place: if any of the script's args contain the `@IMAGE@` placeholder,
#     the wrapper copies the input to the output, substitutes `@IMAGE@` with the
#     output path, and does not pass `-z`/`-o`. This supports tools that modify
#     an image in place, e.g. `avbtool.py add_hash_footer --image <image>`.
#
# ```
# post_processing_script_args = [
#     # Uses -z/-o.
#     "foo_script.py", "--foo", "bar",
#     "--then",
#     # Modifies the image in place.
#     "avbtool.py", "add_hash_footer", "--image", "@IMAGE@", ...
# ],
# ```

set -e

# The first 2 args are always `-p <python_interpreter>`.
if [[ "$1" != "-p" || $# -lt 2 ]]; then
  echo "Error: -p <python_interpreter> is required as the first argument" >&2
  echo "Make sure 'use_python = True' in fuchsia_post_processing_script()" >&2
  exit 1
fi
python_interpreter="$2"
shift 2

if [[ $# -lt 1 ]]; then
  echo "Error: Python script path is required as first arg" >&2
  exit 1
fi

# Split the remaining args into stages on `--then`, and pull out the
# assembly-provided -z/-o paths so they can be routed to the first/last stage.
#
# One trick here is that Bazel and assembly provide paths differently:
#  * Bazel - relative to script dir
#  * assembly - relative to cwd
# In order to simplify life for the Python scripts, we convert the assembly
# -z/-o paths to be absolute, and then `cd` to the script dir. This makes it so
# that the Python scripts can use all paths directly without needing any
# special-case handling.
#
# Bash has no nested arrays, so all stage args are kept in one flat array and
# each stage is recorded as a (start, length) slice into it.
zbi=""
output=""
args=()
stage_starts=(0)
stage_lens=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    # Translate assembly paths to absolute.
    -z)
      zbi="$(readlink -f "$2")"
      shift 2
      ;;
    -o)
      output="$(readlink -f "$2")"
      shift 2
      ;;
    --then)
      stage_lens+=($(( ${#args[@]} - ${stage_starts[-1]} )))
      stage_starts+=(${#args[@]})
      shift
      ;;
    *)
      args+=("$1")
      shift
      ;;
  esac
done
stage_lens+=($(( ${#args[@]} - ${stage_starts[-1]} )))

for len in "${stage_lens[@]}"; do
  if [[ $len -eq 0 ]]; then
    echo "Error: empty stage in --then chain" >&2
    exit 1
  fi
done

if [[ -z "${zbi}" || -z "${output}" ]]; then
  echo "Error: -z <zbi> and -o <output> are required" >&2
  exit 1
fi

# Change directory so that all Bazel relative paths are valid as-is.
cd "$(dirname "${BASH_SOURCE[0]}")"

# Ensure that Python doesn't write bytecode into the board, which dirties
# the board and causes incremental build issues.
export PYTHONDONTWRITEBYTECODE="1"

num_stages=${#stage_lens[@]}

# Intermediate images live outside the board directory so we don't dirty it.
if [[ $num_stages -gt 1 ]]; then
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "${tmp_dir}"' EXIT
fi

# Run each stage using the resolved interpreter, chaining output to input.
# The first stage reads assembly's ZBI.
stage_input="${zbi}"
for (( i = 0; i < num_stages; i++ )); do
  # The last stage writes assembly's output; earlier stages write to a temp
  # file that becomes the next stage's input.
  if [[ $i -eq $(( num_stages - 1 )) ]]; then
    stage_output="${output}"
  else
    stage_output="${tmp_dir}/stage_${i}.img"
  fi

  # Slice this stage's args out of the flat `args` array.
  stage_args=("${args[@]:${stage_starts[$i]}:${stage_lens[$i]}}")

  # If any arg contains `@IMAGE@`, this is an in-place stage: substitute the
  # stage output path for the placeholder.
  in_place=false
  for j in "${!stage_args[@]}"; do
    if [[ "${stage_args[$j]}" == *@IMAGE@* ]]; then
      in_place=true
      # Substring substitution (rather than replacing the whole arg) so that
      # single-word args like `--foo=@IMAGE@` are supported too.
      stage_args[$j]="${stage_args[$j]//@IMAGE@/${stage_output}}"
    fi
  done

  if [[ "${in_place}" == true ]]; then
    # In-place stages modify the output directly, so seed it with a copy of the
    # input first (this also avoids ever modifying assembly's ZBI).
    cp -- "${stage_input}" "${stage_output}"
    "./${python_interpreter}" "${stage_args[@]}"
  else
    # Normal stages follow the `-z <input> -o <output>` contract.
    "./${python_interpreter}" "${stage_args[@]}" \
      -z "${stage_input}" -o "${stage_output}"
  fi

  # Feed this stage's output into the next stage.
  stage_input="${stage_output}"
done
