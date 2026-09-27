#!/bin/bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Runs usb_virtual_disconnect_test on host attached with fuchsia dut
#
# The test sets `use_virtual_usb_hub`, so the Mobly driver discovers the
# attached device and no need of testbed config.
# Only setup is the udev rules by udev.sh, which this script runs

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"
TEST_TARGET=\
"//src/tests/end_to_end/usb/stress:usb_virtual_disconnect_test"
# Wrapper group that pulls in TEST_TARGET under the host toolchain. This is the
# label to add to the build graph; the bare TEST_TARGET is not in the graph.
TEST_GROUP="//src/tests/end_to_end/usb:usb_virtual_disconnect_test"
UDEV_SCRIPT="${SCRIPT_DIR}/udev.sh"

usage() {
    cat <<USAGE_EOF
Usage: $0 [options] [-- <extra fx test args>]

Options:
  -t, --target <name>   Fuchsia target name. Defaults to the device \`fx\`
                        is already pointed at.
  -h, --help            Show this message.

Anything after \`--\` is forwarded verbatim to \`fx test\`.

Examples:
  $0 -t fuchsia-1234-5678-9abc
  $0 -- --test-filter test_usb_disconnect_1 --min-severity-logs DEBUG
USAGE_EOF
}

# `fx test` can only run targets that are in the build graph. Add the test if
# the current build dir was not configured with it. `fx add-test` only appends
# to `target_labels` in args.gn, so it preserves the rest of the build config.
ensure_test_in_build_graph() {
    local fx="$1"
    local build_dir
    build_dir="$("${fx}" get-build-dir)"

    if [[ -f "${build_dir}/tests.json" ]] \
        && grep -q "${TEST_TARGET}" "${build_dir}/tests.json"; then
        return 0
    fi

    echo "${fx} add-test ${TEST_GROUP}"
    "${fx}" add-test "${TEST_GROUP}"
}

main() {
    local target_name=""
    local -a extra_test_args=()

    while [[ $# -gt 0 ]]; do
        case "$1" in
            -t|--target)
                target_name="$2"
                shift 2
                ;;
            --target=*)
                target_name="${1#*=}"
                shift
                ;;
            -h|--help)
                usage
                exit 0
                ;;
            --)
                shift
                extra_test_args+=("$@")
                break
                ;;
            *)
                echo "[!] ERROR: Unknown argument: $1" >&2
                usage >&2
                return 1
                ;;
        esac
    done

    local fuchsia_dir="${FUCHSIA_DIR:-}"
    if [[ -z "${fuchsia_dir}" ]]; then
        # SCRIPT_DIR is //src/tests/end_to_end/usb/lib/disconnect, which is six
        # levels below the checkout root.
        fuchsia_dir="$(cd "${SCRIPT_DIR}/../../../../../.." >/dev/null 2>&1 \
            && pwd)"
    fi

    # `fx` is only on PATH after sourcing scripts/fx-env.sh, which not everyone
    # has in their shell rc, so invoke it by its canonical path instead.
    local fx="${fuchsia_dir}/scripts/fx"
    if [[ ! -x "${fx}" ]]; then
        echo "[!] ERROR: could not locate the Fuchsia checkout root." >&2
        echo "    Derived fuchsia_dir=${fuchsia_dir}" >&2
        echo "    but ${fx} is not executable." >&2
        return 1
    fi

    # The test toggles the USB 'authorized' attribute without sudo.
    # udev.sh is idempotent and exits early if already done.
    echo "${UDEV_SCRIPT}"
    bash "${UDEV_SCRIPT}"

    ensure_test_in_build_graph "${fx}"

    local -a test_cmd=("${fx}")
    if [[ -n "${target_name}" ]]; then
        test_cmd+=(-t "${target_name}")
    fi
    test_cmd+=(
        test
        -o
        --e2e
        --no-allow-temporary-emulator
        "${TEST_TARGET}"
    )
    if [[ ${#extra_test_args[@]} -gt 0 ]]; then
        test_cmd+=("${extra_test_args[@]}")
    fi

    echo "${test_cmd[@]}"
    "${test_cmd[@]}"
}

main "$@"
