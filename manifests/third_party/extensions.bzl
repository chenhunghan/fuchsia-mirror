# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Module extension wrapping Bazel's built-in `git_repository()` rule.

Wrapping `git_repository()` in a named `third_party` module extension gives
vendored directories stable `+third_party+<name>` names instead of Bazel's
internal `+_repo_rules<N>+<name>` naming.

All attributes on `third_party.git_repository(...)` forward directly to the
standard `git_repository()` repository rule without introducing custom
attributes; see https://bazel.build/rules/lib/repo/git#git_repository for
attribute documentation.
"""

load("@bazel_tools//tools/build_defs/repo:git.bzl", "git_repository")

visibility("private")

def _third_party_impl(mctx):
    for mod in mctx.modules:
        for repo in mod.tags.git_repository:
            kwargs = {
                "name": repo.name,
                "remote": repo.remote,
            }
            if repo.commit:
                kwargs["commit"] = repo.commit
            if repo.tag:
                kwargs["tag"] = repo.tag
            if repo.branch:
                kwargs["branch"] = repo.branch
            if repo.strip_prefix:
                kwargs["strip_prefix"] = repo.strip_prefix
            if repo.init_submodules:
                kwargs["init_submodules"] = repo.init_submodules
            if repo.recursive_init_submodules:
                kwargs["recursive_init_submodules"] = repo.recursive_init_submodules
            if repo.sparse_checkout_patterns:
                kwargs["sparse_checkout_patterns"] = repo.sparse_checkout_patterns
            git_repository(**kwargs)

third_party = module_extension(
    doc = (
        "Wraps Bazel's standard `git_repository()` repository rule so vendored" +
        " repositories materialize under `+third_party+<name>`. See" +
        " https://bazel.build/rules/lib/repo/git#git_repository for attribute" +
        " documentation."
    ),
    implementation = _third_party_impl,
    tag_classes = {
        "git_repository": tag_class(
            doc = (
                "Forwards directly to `git_repository()`; see" +
                " https://bazel.build/rules/lib/repo/git#git_repository."
            ),
            attrs = {
                "branch": attr.string(),
                "commit": attr.string(),
                "init_submodules": attr.bool(),
                "name": attr.string(mandatory = True),
                "recursive_init_submodules": attr.bool(),
                "remote": attr.string(mandatory = True),
                "sparse_checkout_patterns": attr.string_list(),
                "strip_prefix": attr.string(),
                "tag": attr.string(),
            },
        ),
    },
)
