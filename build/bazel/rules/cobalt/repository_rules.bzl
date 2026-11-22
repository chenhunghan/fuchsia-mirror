# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Repository rule to expose //third_party/cobalt C++ targets to Bazel."""

visibility(["//", "//build/bazel"])

_COBALT_FILES = [
    "src/lib/client/cpp/buckets_config.cc",
    "src/lib/client/cpp/buckets_config.h",
    "src/logging.h",
    "src/registry/cobalt_registry.proto",
    "src/registry/metric_definition.proto",
    "src/registry/project.proto",
    "src/registry/report_definition.proto",
    "src/registry/window_size.proto",
]

def _cobalt_repository_impl(repo_ctx):
    workspace_dir = str(repo_ctx.workspace_root)
    cobalt_dir = "%s/third_party/cobalt" % workspace_dir

    for rel_path in _COBALT_FILES:
        repo_ctx.symlink("%s/%s" % (cobalt_dir, rel_path), rel_path)

    # Also expose headers under third_party/cobalt/... so in-tree consumers
    # can `#include "third_party/cobalt/src/lib/client/cpp/buckets_config.h"`.
    repo_ctx.symlink(
        "%s/src/lib/client/cpp/buckets_config.h" % cobalt_dir,
        "third_party/cobalt/src/lib/client/cpp/buckets_config.h",
    )

    repo_ctx.symlink(
        repo_ctx.path(Label("@//build/bazel/rules/cobalt:third-party-cobalt.BUILD.bazel")),
        "BUILD.bazel",
    )

cobalt_repository = repository_rule(
    implementation = _cobalt_repository_impl,
    doc = "Creates the @cobalt repository exposing //third_party/cobalt C++ and proto targets.",
)
