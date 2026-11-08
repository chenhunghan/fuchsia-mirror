# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Security-owned allowlists of restricted CML features for Bazel.

These mirror the GN `group()` allowlists in
//tools/cmc/build/restricted_features/security_owned/BUILD.gn and are merged
into `RESTRICTED_FEATURE_ALLOWLISTS` by
//tools/cmc/build/restricted_features/allowlists.bzl, which documents the
pattern syntax.
"""

visibility(["//tools/cmc/build/restricted_features"])

SECURITY_OWNED_RESTRICTED_FEATURE_ALLOWLISTS = {
    # LINT.IfChange(use_dictionaries)
    # The allowlist of components that can use a dictionary in CML.
    "use_dictionaries": [
        "//bundles/assembly:embeddable_userdebug.compiled_package.toolbox.toolbox.cmc_features",
        "//src/developer/console/test:*",
        "//src/sys/component_manager/tests/*",
    ],
    # LINT.ThenChange(//tools/cmc/build/restricted_features/security_owned/BUILD.gn:use_dictionaries)
}
