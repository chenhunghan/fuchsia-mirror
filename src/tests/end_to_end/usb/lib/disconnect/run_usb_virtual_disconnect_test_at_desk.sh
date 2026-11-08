#!/bin/bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Runs usb_virtual_disconnect_test (or usb_reset_test with
# `--mode reset`) on host attached with fuchsia dut
#
# The tests set `use_virtual_usb_hub`, so the Mobly driver discovers the
# attached device and no need of testbed config.
# Only setup is the udev rules by udev.sh, which this script runs

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"
UDEV_SCRIPT="${SCRIPT_DIR}/udev.sh"

# Provides FUCHSIA_DIR, FX and the usb_desk_* helpers.
# shellcheck source=src/tests/end_to_end/usb/lib/desk_test_common.sh
source "${SCRIPT_DIR}/../desk_test_common.sh"

# Reuse TARGET_VID, TARGET_PIDS, and is_target_pid() from udev.sh.
# shellcheck source=/dev/null
source "${UDEV_SCRIPT}"

usage() {
    cat <<USAGE_EOF
Usage: $0 [options] [-- <extra fx test args>]

Options:
  -t, --target <name>   Fuchsia target name. Defaults to the device \`fx\`
                        is already pointed at.
  -m, --mode <mode>     How to virtually disconnect the device:
                          authorize  toggle its USB \`authorized\` attribute
                                     (usb_virtual_disconnect_test, default)
                          reset      reset its USB port with USBDEVFS_RESET
                                     (usb_reset_test)
  -h, --help            Show this message.

Anything after \`--\` is forwarded verbatim to \`fx test\`.

Examples:
  $0 -t fuchsia-1234-5678-9abc
  $0 --mode reset
  $0 -- --test-filter test_usb_disconnect_1 --min-severity-logs DEBUG
USAGE_EOF
}

# The virtual hub disconnects a real DUT over USB, so this
# runner only works where a Fuchsia USB device is physically attached. Fail fast
# when the DUT is unplugged or on remote environments (e.g. Cloudtops/VMs)
# before prompting for sudo in udev.sh or running a build.
require_attached_device() {
    local dev pid
    for dev in /sys/bus/usb/devices/*; do
        [[ -f "${dev}/idVendor" ]] || continue
        [[ "$(cat "${dev}/idVendor" 2>/dev/null)" == "${TARGET_VID}" ]] || continue
        pid="$(cat "${dev}/idProduct" 2>/dev/null || echo "unknown")"
        if is_target_pid "${pid}"; then
            return 0
        fi
    done

    echo "[!] ERROR: No supported Fuchsia USB device (VID ${TARGET_VID}) found on this host (DUT unplugged or running on a Cloudtop/VM)." >&2
    return 1
}

main() {
    local target_name=""
    local mode="authorize"
    local -a extra_test_args=()

    while [[ $# -gt 0 ]]; do
        case "$1" in
            -t|--target)
                usb_desk_require_value "$1" "$#" "${2-}"
                target_name="$2"
                shift 2
                ;;
            --target=*)
                target_name="${1#*=}"
                shift
                ;;
            -m|--mode)
                usb_desk_require_value "$1" "$#" "${2-}"
                mode="$2"
                shift 2
                ;;
            --mode=*)
                mode="${1#*=}"
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

    local test_name
    case "${mode}" in
        authorize) test_name="usb_virtual_disconnect_test" ;;
        reset) test_name="usb_reset_test" ;;
        *)
            echo "[!] ERROR: Unknown --mode: ${mode}" >&2
            usage >&2
            return 1
            ;;
    esac

    require_attached_device

    # The tests write `/sys/bus/usb/devices/<bus_id>/authorized` or the usbfs
    # device node `/dev/bus/usb/BBB/DDD` without sudo. On its first run on a
    # workstation, `udev.sh` requires `sudo` to write
    # `/etc/udev/rules.d/99-fuchsia-usb-authorize.rules` and run `udevadm` to
    # reload/trigger the rules (granting `a+w` on `authorized` and on the
    # device node). Subsequent runs detect that the rules and write permission
    # are already in place and exit without needing `sudo`.
    echo "${UDEV_SCRIPT}"
    bash "${UDEV_SCRIPT}"

    # `fx test` can only run targets that are in the build graph. The wrapper
    # group pulls the test in under the host toolchain; the bare test target
    # is not in the graph, so the group is the label to add.
    local test_target="//src/tests/end_to_end/usb/stress:${test_name}"
    usb_desk_ensure_test_in_build_graph \
        "${test_target}" "//src/tests/end_to_end/usb:${test_name}"

    usb_desk_build_fx_test_cmd "${target_name}" "true" "${test_target}" \
        ${extra_test_args[@]+"${extra_test_args[@]}"}
    echo "${USB_DESK_TEST_CMD[@]}"
    "${USB_DESK_TEST_CMD[@]}"
}

main "$@"
