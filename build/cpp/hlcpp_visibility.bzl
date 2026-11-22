# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Allowlist for deprecated HLCPP FIDL bindings."""

visibility(["//build/..."])

# LINT.IfChange
hlcpp_visibility = [
    # Dependencies between SDK FIDL libraries.
    "//sdk/fidl:__subpackages__",

    # Unusually located FIDL libraries.
    "//src/camera/bin/factory:__pkg__",

    # FIDL implementation, tests, examples.
    "//build/bazel/rules/fidl:__subpackages__",
    "//examples/fidl/fuchsia.examples:__pkg__",
    "//sdk/ctf/tests/pkg/fidl/cpp:__pkg__",
    "//src/lib/fostr:__subpackages__",
    "//src/sys/fuzzing/fidl:__pkg__",
    "//src/testing/fidl:__subpackages__",
    "//src/tests/fidl:__subpackages__",
    "//tools/fidl/fidlc/testdata:__pkg__",

    # Misc buildsystem stuff.
    "//sdk/ctf:__subpackages__",

    # Existing in-tree uses of HLCPP that should be removed.
    "//examples/components/config/integration_test:__pkg__",
    "//examples/components/config_from_parent/integration_test:__pkg__",
    "//examples/components/lifecycle/cpp:__pkg__",
    "//examples/components/routing/fidl:__pkg__",
    "//examples/components/routing/integration_tests/cpp:__pkg__",
    "//examples/components/services/cpp:__pkg__",
    "//examples/components/services/fidl:__pkg__",
    "//examples/diagnostics/inspect/codelab/cpp/client:__pkg__",
    "//examples/diagnostics/inspect/codelab/cpp/fizzbuzz:__pkg__",
    "//examples/diagnostics/inspect/codelab/cpp/part_1:__pkg__",
    "//examples/diagnostics/inspect/codelab/cpp/part_2:__pkg__",
    "//examples/diagnostics/inspect/codelab/cpp/part_3:__pkg__",
    "//examples/diagnostics/inspect/codelab/cpp/part_4:__pkg__",
    "//examples/diagnostics/inspect/codelab/cpp/part_5:__pkg__",
    "//examples/diagnostics/inspect/codelab/cpp/testing:__pkg__",
    "//examples/diagnostics/inspect/codelab/fidl:__pkg__",
    "//examples/intl/wisdom:__subpackages__",
    "//sdk/lib/component/tests:__pkg__",
    "//sdk/lib/driver/component/cpp:__pkg__",
    "//sdk/lib/driver/devfs/cpp:__pkg__",
    "//sdk/lib/fuchsia-mem-ext:__pkg__",
    "//sdk/lib/images/cpp:__pkg__",
    "//sdk/lib/inspect/component/cpp/tests:__pkg__",
    "//sdk/lib/media/audio/cpp:__pkg__",
    "//sdk/lib/media/cpp:__pkg__",
    "//sdk/lib/sys/component/cpp/testing:__pkg__",
    "//sdk/lib/sys/component/cpp/tests:__pkg__",
    "//sdk/lib/sys/cpp:__subpackages__",
    "//sdk/lib/sys/service/cpp:__pkg__",
    "//sdk/lib/ui/scenic/cpp:__subpackages__",
    "//sdk/lib/vfs/cpp:__subpackages__",
    "//sdk/lib/virtualization/testing:__pkg__",
    "//src/camera/bin/benchmark:__pkg__",
    "//src/camera/bin/camera-gym:__subpackages__",
    "//src/camera/bin/device:__subpackages__",
    # Bazel-only target (GN twin removed in https://fxrev.dev/1988774).
    "//src/camera/bin/device_watcher:__pkg__",
    "//src/camera/bin/factory_cli:__pkg__",
    "//src/camera/bin/sensor_cli:__pkg__",
    "//src/camera/bin/usb_device:__pkg__",
    "//src/camera/bin/virtual_camera:__pkg__",
    "//src/camera/drivers/controller:__subpackages__",
    "//src/camera/drivers/sensors/imx227:__pkg__",
    "//src/camera/drivers/usb_video/uvc-tester:__pkg__",
    "//src/camera/drivers/usb_video:__pkg__",
    "//src/camera/lib/cobalt_logger:__pkg__",
    "//src/camera/lib/fake_camera:__pkg__",
    "//src/camera/lib/fake_controller:__pkg__",
    "//src/camera/lib/fake_legacy_stream:__pkg__",
    "//src/camera/lib/fake_logical_buffer_collection:__pkg__",
    "//src/camera/lib/fake_stream:__pkg__",
    "//src/camera/lib/format_conversion:__pkg__",
    "//src/camera/lib/formatting:__pkg__",
    "//src/camera/lib/image_utils:__pkg__",
    "//src/camera/lib/stream_utils:__pkg__",
    "//src/camera/lib/virtual_camera:__pkg__",
    "//src/chromium/web_runner_tests:__pkg__",
    "//src/cobalt/bin/app:__pkg__",
    "//src/cobalt/bin/system-metrics:__subpackages__",
    "//src/cobalt/bin/testapp:__pkg__",
    "//src/cobalt/bin/testing:__pkg__",
    "//src/cobalt/bin/utils:__pkg__",
    # Bazel-only target (GN twin removed in https://fxrev.dev/1988774).
    "//src/connectivity/bluetooth/examples/bt-beacon-reader:__pkg__",
    # Bazel-only target (GN twin removed in https://fxrev.dev/1988774).
    "//src/connectivity/bluetooth/examples/bt-le-heart-rate-peripheral:__pkg__",
    "//src/connectivity/network/lib/net_interfaces/cpp:__pkg__",
    "//src/connectivity/network/mdns/service:__pkg__",
    "//src/connectivity/network/mdns/util:__pkg__",
    "//src/connectivity/network/testing/netemul/network-context/fidl:__pkg__",
    "//src/connectivity/network/testing/netemul/network-context/lib:__pkg__",
    "//src/connectivity/network/testing/netemul/sync-manager/fidl:__pkg__",
    "//src/connectivity/network/testing/netemul/sync-manager/lib:__pkg__",
    "//src/connectivity/network/tests/socket:__pkg__",
    "//src/connectivity/openthread/third_party/openthread/platform:__pkg__",
    "//src/connectivity/weave/adaptation:__pkg__",
    "//src/connectivity/weave/weavestack:__pkg__",
    "//src/connectivity/wlan/drivers/lib/log/cpp:__subpackages__",
    "//src/connectivity/wlan/testing/wlantap-driver:__pkg__",
    "//src/developer/adb/bin/adb-file-sync:__pkg__",
    "//src/developer/adb/bin/adb-reboot:__pkg__",
    "//src/developer/adb/bin/adb:__pkg__",
    "//src/developer/debug/debug_agent:__subpackages__",
    "//src/developer/process_explorer:__subpackages__",
    "//src/devices/lib/sysmem:__pkg__",
    "//src/devices/securemem/drivers/aml-securemem:__pkg__",
    "//src/devices/testing/devmgr-integration-test-shim:__pkg__",
    "//src/devices/testing/driver-integration-test-shim:__pkg__",
    "//src/devices/tests/bindlib-codegen-test:__pkg__",
    "//src/devices/tests/device-controller-fidl:__pkg__",
    "//src/devices/tests/driver-runtime:__pkg__",
    "//src/devices/tests/driver-transport:__pkg__",
    "//src/diagnostics/validator/inspect/fidl:__pkg__",
    "//src/diagnostics/validator/inspect/lib/cpp:__pkg__",
    "//src/diagnostics/validator/logs/fidl:__pkg__",
    "//src/diagnostics/validator/logs/sink/puppet/cpp:__pkg__",
    "//src/graphics/bin/agis:__pkg__",
    "//src/graphics/tests/libvulkan:__pkg__",
    "//src/graphics/tests/vkext:__pkg__",
    "//src/intl/example:__pkg__",
    "//src/lib/cobalt/cpp:__pkg__",
    "//src/lib/fake-clock/fidl:__pkg__",
    "//src/lib/fake-clock/lib:__pkg__",
    "//src/lib/fake-clock/named-timer:__pkg__",
    "//src/lib/fidl_codec:__subpackages__",
    "//src/lib/fidl_fuchsia_intl_ext/cpp:__pkg__",
    "//src/lib/fsl/vmo:__pkg__",
    "//src/lib/inet:__pkg__",
    "//src/lib/intl/intl_property_provider_impl:__pkg__",
    "//src/lib/intl/lookup/rust:__pkg__",
    "//src/lib/intl/time_zone_info:__pkg__",
    "//src/lib/test-suite:__pkg__",
    "//src/media/audio/audio_core:__pkg__",
    "//src/media/audio/audio_core/mixer:__pkg__",
    "//src/media/audio/audio_core/test/api:__pkg__",
    "//src/media/audio/audio_core/test/hardware:__pkg__",
    "//src/media/audio/audio_core/testing:__pkg__",
    "//src/media/audio/audio_core/testing/integration:__pkg__",
    "//src/media/audio/audio_core/tools/output_pipeline_benchmark:__pkg__",
    "//src/media/audio/drivers/aml-g12-tdm:__subpackages__",
    "//src/media/audio/drivers/tests:__pkg__",
    "//src/media/audio/examples/simple_capture:__pkg__",
    "//src/media/audio/examples/simple_sine:__pkg__",
    "//src/media/audio/examples/sync_sine:__pkg__",
    "//src/media/audio/examples/tones:__pkg__",
    "//src/media/audio/lib/analysis:__pkg__",
    "//src/media/audio/lib/clock:__pkg__",
    "//src/media/audio/lib/effects_loader:__pkg__",
    "//src/media/audio/lib/format:__pkg__",
    "//src/media/audio/lib/simple-codec:__pkg__",
    "//src/media/audio/lib/test:__pkg__",
    "//src/media/audio/lib/wav:__pkg__",
    "//src/media/audio/services/device_registry/testing/fakes:__pkg__",
    "//src/media/audio/services/mixer/mix:__pkg__",
    "//src/media/audio/tools/audio_listener:__pkg__",
    "//src/media/audio/tools/audio-capture-timestamp-validator:__pkg__",
    "//src/media/audio/tools/audio-effects-ctl:__pkg__",
    "//src/media/audio/tools/signal_generator:__pkg__",
    "//src/media/audio/tools/vol:__pkg__",
    "//src/media/audio/tools/wav_player:__pkg__",
    "//src/media/audio/tools/wav_recorder:__pkg__",
    "//src/media/codec/examples/encode_camera:__pkg__",
    "//src/media/codec/examples/encode_file:__pkg__",
    "//src/media/codec/examples/use_media_decoder:__pkg__",
    "//src/media/codec/factory:__pkg__",
    "//src/media/drivers/amlogic_decoder:__pkg__",
    "//src/media/lib/codec_impl:__subpackages__",
    "//src/media/lib/internal_buffer:__pkg__",
    "//src/media/lib/test:__pkg__",
    "//src/media/sounds/soundplayer:__pkg__",
    # Bazel-only target (GN twin removed in https://fxrev.dev/1956372).
    "//src/media/testing/mock_profile_provider:__pkg__",
    "//src/power/fidl/testing:__pkg__",
    "//src/power/testing/thermal-test-control:__pkg__",
    "//src/security/bin/tee_manager:__pkg__",
    "//src/sys/activity:__pkg__",
    "//src/sys/fuzzing/common:__subpackages__",
    "//src/sys/fuzzing/libfuzzer:__pkg__",
    "//src/sys/fuzzing/realmfuzzer/adapters:__pkg__",
    "//src/sys/fuzzing/realmfuzzer/engine:__pkg__",
    "//src/sys/fuzzing/realmfuzzer/target:__pkg__",
    "//src/sys/fuzzing/realmfuzzer/tests:__pkg__",
    "//src/sys/fuzzing/registry/tests:__pkg__",
    "//src/sys/test_manager/debug_data_processor:__pkg__",
    "//src/sys/test_manager/fidl/fuchsia.test.debug:__pkg__",
    "//src/sys/test_runners/gtest/test_data:__pkg__",
    "//src/sys/tools/activity-ctl:__pkg__",
    "//src/testing/loadbench:__pkg__",
    "//src/testing/system-validation/web/apps:__pkg__",
    "//src/tests/benchmarks:__pkg__",
    "//src/tests/microbenchmarks:__pkg__",
    "//src/ui/a11y/bin/a11y_manager:__subpackages__",
    "//src/ui/a11y/lib/annotation:__pkg__",
    "//src/ui/a11y/lib/configuration:__subpackages__",
    "//src/ui/a11y/lib/focus_chain:__subpackages__",
    "//src/ui/a11y/lib/gesture_manager:__subpackages__",
    "//src/ui/a11y/lib/input_injection:__pkg__",
    "//src/ui/a11y/lib/magnifier:__subpackages__",
    "//src/ui/a11y/lib/screen_reader:__subpackages__",
    "//src/ui/a11y/lib/semantics:__subpackages__",
    "//src/ui/a11y/lib/testing:__pkg__",
    "//src/ui/a11y/lib/tts:__subpackages__",
    "//src/ui/a11y/lib/util:__subpackages__",
    "//src/ui/a11y/lib/view:__subpackages__",
    "//src/ui/a11y/testing:__pkg__",
    "//src/ui/bin/system_monitor:__pkg__",
    "//src/ui/bin/virtual_keyboard_manager:__subpackages__",
    "//src/ui/examples/simplest_sysmem:__pkg__",
    "//src/ui/input/lib/injector:__subpackages__",
    "//src/ui/input/testing/fake_input_report_device:__pkg__",
    "//src/ui/lib/escher:__pkg__",
    "//src/ui/testing:__subpackages__",
    "//src/ui/tests/integration_graphics_tests/carnelian-pixel-tests:__pkg__",
    "//src/ui/tests/integration_graphics_tests/display-pixel-ratio:__pkg__",
    "//src/ui/tests/integration_graphics_tests/web-pixel-tests:__pkg__",
    "//src/ui/tests/integration_graphics_tests/web-pixel-tests/chromium_pixel_client:__pkg__",
    "//src/ui/tests/integration_input_tests/mouse:__subpackages__",
    "//src/ui/tests/integration_input_tests/text-input:__subpackages__",
    "//src/ui/tests/integration_input_tests/touch:__subpackages__",
    "//src/ui/tests/integration_input_tests/touch/touch-flatland-client:__pkg__",
    "//src/ui/tests/integration_input_tests/virtual-keyboard:__subpackages__",
    "//src/ui/tests/integration_input_tests/web-touch-input:__subpackages__",
    "//src/virtualization/bin/guest_manager:__pkg__",
    "//src/virtualization/bin/vmm:__pkg__",
    "//src/virtualization/bin/vmm/device:__pkg__",
    "//src/virtualization/bin/vmm/device/tests:__pkg__",
    "//src/virtualization/bin/vmm/device/virtio_net:__pkg__",
    "//src/virtualization/lib/grpc:__pkg__",
    "//src/virtualization/lib/guest_config:__pkg__",
    "//src/virtualization/tests:__subpackages__",
    "//src/zircon/bin/hwstress:__pkg__",
    "//src/zircon/testing/mutex_pi_exerciser:__pkg__",

    # TODO(https://fxbug.dev/376322253): remove abseil from hlcpp allowlist once new C++ binding is supported in cctz and rolled in Fuchsia.
    "//third_party/abseil-cpp:__subpackages__",
    "@abseil-cpp//:__subpackages__",
    "//third_party/mesa-migrating/build/src/util/tests/inflight_list:__pkg__",
    "//third_party/openweave-core/src/include:__pkg__",
    "//third_party/openweave-core/src/inet:__pkg__",
    "//tools/create/goldens/my-component-test-cpp:__pkg__",
    "//sdk/lib/devicetree/tests:__pkg__",

    # Extra vendor-specific targets that may use the deprecated bindings.
    # TODO(https://fxbug.dev/454977301): Restrict vendor HLCPP visibility once vendor builds support fine-grained Bazel visibility.
    "//vendor:__subpackages__",
]
# LINT.ThenChange(hlcpp_visibility.gni)

def _package_prefixes(pkg):
    """Returns all ancestor package prefixes of `pkg`, from `""` to `pkg`."""
    if not pkg:
        return [""]
    parts = pkg.split("/")
    return [""] + ["/".join(parts[:i + 1]) for i in range(len(parts))]

# `get_hlcpp_visibility()` runs for every `fidl_library()` target with HLCPP
# enabled, and `hlcpp_visibility` contains ~300 entries. Pre-parse the allowlist
# `Label`s and index them by `(repo_name, package)` and ancestor directory
# prefixes once at `.bzl` load time so that:
#   1. `//visibility:public` returns the pre-parsed `Label` list in O(1) without
#      re-allocating ~300 `Label`s per macro invocation.
#   2. Checking whether a target visibility entry `v` falls within an allowlisted
#      `__pkg__` or `__subpackages__` entry takes O(path depth) dict lookups
#      instead of scanning the full allowlist.
#   3. Finding all allowlist entries under a broad `//foo/bar:__subpackages__`
#      (or `//:__subpackages__`) target visibility entry is a single O(1) dict
#      lookup in `by_prefix`.
def _build_hlcpp_visibility_index():
    labels = [Label(a) for a in hlcpp_visibility]
    pkgs = {}
    subpkgs = {}
    by_prefix = {}
    for a in labels:
        if a.name == "__pkg__":
            pkgs[(a.repo_name, a.package)] = True
        elif a.name == "__subpackages__":
            subpkgs[(a.repo_name, a.package)] = True
        for prefix in _package_prefixes(a.package):
            by_prefix.setdefault((a.repo_name, prefix), []).append(a)
    return struct(
        labels = labels,
        pkgs = pkgs,
        subpkgs = subpkgs,
        by_prefix = by_prefix,
    )

_HLCPP_VISIBILITY_INDEX = _build_hlcpp_visibility_index()

def get_hlcpp_visibility(visibility, testonly):
    """Intersects a FIDL library's `visibility` with `hlcpp_visibility`.

    Args:
        visibility: List of `Label`s representing the FIDL library's visibility.
        testonly: Boolean indicating whether the FIDL library is `testonly`.

    Returns:
        List of `Label`s representing the visibility for the `_hlcpp` target.
    """

    # Match GN's `fidl_cpp_family` (`//build/cpp/fidl_cpp.gni`), which does not
    # restrict `visibility` to `hlcpp_visibility` when `testonly` is true.
    if testonly:
        return visibility

    if Label("//visibility:public") in visibility:
        return _HLCPP_VISIBILITY_INDEX.labels

    # Allow the FIDL symbolic macro package to depend on `_hlcpp` targets
    # when generating bindings for dependent FIDL libraries or `_cpp_hlcpp_conversion`.
    # Use a dict as an insertion-ordered set for O(1) deduplication.
    result = {Label("//build/bazel/rules/fidl:__subpackages__"): True}
    for v in visibility:
        if v == Label("//visibility:private"):
            continue

        # 1. Check if `v` is covered by an exact `__pkg__` or an ancestor `__subpackages__`.
        covered = (
            v.name == "__pkg__" and (v.repo_name, v.package) in _HLCPP_VISIBILITY_INDEX.pkgs
        )
        if not covered:
            for prefix in _package_prefixes(v.package):
                if (v.repo_name, prefix) in _HLCPP_VISIBILITY_INDEX.subpkgs:
                    covered = True
                    break
        if covered:
            result[v] = True
            continue

        # 2. Otherwise, if `v` is `__subpackages__`, include all allowlist entries under `v`.
        if v.name == "__subpackages__":
            for a in _HLCPP_VISIBILITY_INDEX.by_prefix.get((v.repo_name, v.package), ()):
                result[a] = True

    return list(result.keys())
