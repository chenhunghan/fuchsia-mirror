# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for `get_hlcpp_visibility()`."""

load("@bazel_skylib//lib:unittest.bzl", "asserts", "unittest")
load("//build/cpp:hlcpp_visibility.bzl", "get_hlcpp_visibility", "hlcpp_visibility")

visibility("private")

def _hlcpp_visibility_test_impl(ctx):
    env = unittest.begin(ctx)

    # 1. testonly bypasses hlcpp allowlist
    asserts.equals(
        env,
        [Label("//visibility:private")],
        get_hlcpp_visibility([Label("//visibility:private")], testonly = True),
        "testonly = True should preserve visibility unchanged",
    )

    # 2. public visibility expands to all allowlisted labels
    public_label = Label("//visibility:" + "public")
    asserts.equals(
        env,
        [Label(a) for a in hlcpp_visibility],
        get_hlcpp_visibility([public_label], testonly = False),
        "visibility:public should return full hlcpp_visibility allowlist",
    )

    # 3. private visibility only gets macro_visibility
    asserts.equals(
        env,
        [Label("//build/bazel/rules/fidl:__subpackages__")],
        get_hlcpp_visibility([Label("//visibility:private")], testonly = False),
        "visibility:private should only receive macro_visibility",
    )

    # 4. root-package recursive visibility //:__subpackages__ matches allowlist entries in the main repository
    root_result = get_hlcpp_visibility([Label("//:__subpackages__")], testonly = False)
    asserts.true(
        env,
        Label("//build/bazel/rules/fidl:__subpackages__") in root_result,
        "root visibility should include macro_visibility",
    )
    asserts.true(
        env,
        Label("//sdk/fidl:__subpackages__") in root_result,
        "root visibility should match in-repo allowlist entries",
    )
    asserts.true(
        env,
        Label("@abseil-cpp//:__subpackages__") not in root_result,
        "root visibility of main repo should not match external repo allowlist entries",
    )

    # 5. disjoint package gets only macro_visibility
    asserts.equals(
        env,
        [Label("//build/bazel/rules/fidl:__subpackages__")],
        get_hlcpp_visibility([Label("//unknown/pkg:__pkg__")], testonly = False),
        "disjoint package should only receive macro_visibility",
    )

    # 6. allowlisted package in visibility matches
    fostr_result = get_hlcpp_visibility([Label("//src/lib/fostr:__subpackages__")], testonly = False)
    asserts.true(
        env,
        Label("//src/lib/fostr:__subpackages__") in fostr_result,
        "allowlisted package should be present",
    )
    asserts.true(
        env,
        Label("//sdk/fidl:__subpackages__") not in fostr_result,
        "unrelated allowlist entries should not be present",
    )

    # 7. exact __pkg__ match and child package under allowlisted __subpackages__
    sub_result = get_hlcpp_visibility(
        [
            Label("//src/virtualization/bin/guest_manager:__pkg__"),
            Label("//sdk/fidl/fuchsia.intl:__pkg__"),
        ],
        testonly = False,
    )
    asserts.equals(
        env,
        [
            Label("//build/bazel/rules/fidl:__subpackages__"),
            Label("//src/virtualization/bin/guest_manager:__pkg__"),
            Label("//sdk/fidl/fuchsia.intl:__pkg__"),
        ],
        sub_result,
        "exact __pkg__ and child package under allowlisted __subpackages__ should be preserved",
    )

    # 8. broad subtree __subpackages__ expands to allowlist entries within that subtree
    vmm_result = get_hlcpp_visibility([Label("//src/virtualization:__subpackages__")], testonly = False)
    asserts.true(
        env,
        Label("//src/virtualization/bin/guest_manager:__pkg__") in vmm_result,
        "broad subtree should include child __pkg__ allowlist entries",
    )
    asserts.true(
        env,
        Label("//src/virtualization/tests:__subpackages__") in vmm_result,
        "broad subtree should include child __subpackages__ allowlist entries",
    )
    asserts.true(
        env,
        Label("//sdk/fidl:__subpackages__") not in vmm_result,
        "broad subtree should not include entries outside the subtree",
    )

    if env.failures:
        fail("\n".join(env.failures))
    return unittest.end(env)

hlcpp_visibility_test = unittest.make(_hlcpp_visibility_test_impl)
