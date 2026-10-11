# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rules and provider describing where Fuchsia device tests run."""

FxTestEnvironmentInfo = provider(
    doc = "An infra test environment that an `fx_test()` can run in.",
    fields = {
        "environment": "An environment dict, in the form tests.json expects.",
        "is_variant": "Whether this came from `fx_test_environment_variant()`.",
    },
)

# Only the string-valued fields of tests.json's emulator config. `uefi` (a
# bool) and `kernel_args` (a list) can't go in a `string_dict`, and no Bazel
# test needs them yet.
_ALLOWED_EMULATOR_KEYS = ("accel", "device", "name")

def _fx_test_environment_impl(ctx):
    dimensions = ctx.attr.dimensions
    if not dimensions:
        fail("`dimensions` must not be empty.")
    if "tags" in dimensions:
        fail("Use `env_tags` instead of a 'tags' dimension.")
    env = {"dimensions": dimensions}

    if ctx.attr.emulator:
        for key in ctx.attr.emulator:
            if key not in _ALLOWED_EMULATOR_KEYS:
                fail("Unknown `emulator` field '{}'; allowed fields are {}.".format(
                    key,
                    ", ".join(_ALLOWED_EMULATOR_KEYS),
                ))
        if not ctx.attr.emulator.get("name"):
            fail("`emulator` requires a unique 'name'.")
        env["emulator"] = ctx.attr.emulator
    if ctx.attr.env_tags:
        env["tags"] = ctx.attr.env_tags
    if ctx.attr.service_account:
        env["service_account"] = ctx.attr.service_account
    if ctx.attr.netboot:
        env["netboot"] = True
    return [FxTestEnvironmentInfo(environment = env, is_variant = False)]

_ENV_TAGS_DOC = """Infra tags for the environment (e.g. `e2e-isolated`).

Emitted as `tags` in tests.json. `tags` itself is a common attribute of every
Bazel rule, with an unrelated meaning.
"""

_SERVICE_ACCOUNT_DOC = "The service account that the test's Swarming task runs as."

_NETBOOT_DOC = "Whether to netboot the device instead of paving it."

fx_test_environment = rule(
    doc = """Defines an infra test environment for `fx_test(environments = ...)`.

    Most tests should use one of the existing environments in
    //build/testing/environments, and new environments should be rare.
    New ones belong in that same package, kept in sync with
    //build/testing/environments.gni. Their Swarming dimensions are an
    interface with infra, so they're kept in one place rather than
    defined next to individual tests. A test that needs extra settings,
    such as tags, on an existing environment should use
    `fx_test_environment_variant()` instead.

    Example usage, in //build/testing/environments/BUILD.bazel:

    ```bazel
    fx_test_environment(
        name = "nuc11_env",
        dimensions = {"device_type": device_types.nuc11},
    )
    ```
    """,
    implementation = _fx_test_environment_impl,
    provides = [FxTestEnvironmentInfo],
    attrs = {
        "dimensions": attr.string_dict(
            doc = "The Swarming dimensions of the bots that can run the test.",
            mandatory = True,
        ),
        "emulator": attr.string_dict(
            doc = """Emulator settings, for emulator device types.

            Supports the `name` (required), `device` and `accel` fields of the
            tests.json emulator config.
            """,
        ),
        "env_tags": attr.string_list(doc = _ENV_TAGS_DOC),
        "service_account": attr.string(doc = _SERVICE_ACCOUNT_DOC),
        "netboot": attr.bool(doc = _NETBOOT_DOC),
    },
)

def _fx_test_environment_variant_impl(ctx):
    base_info = ctx.attr.base[FxTestEnvironmentInfo]
    if base_info.is_variant:
        fail("`base` ({}) is itself a variant. Derive from the original environment instead.".format(
            ctx.attr.base.label,
        ))
    if not (ctx.attr.env_tags or ctx.attr.service_account or ctx.attr.netboot):
        fail("A variant must change something. Use `base` ({}) directly instead.".format(
            ctx.attr.base.label,
        ))

    # Provider contents are frozen, so copy before changing fields.
    env = dict(base_info.environment)
    if ctx.attr.env_tags:
        tags = list(env.get("tags", []))
        tags.extend([t for t in ctx.attr.env_tags if t not in tags])
        env["tags"] = tags
    if ctx.attr.service_account:
        env["service_account"] = ctx.attr.service_account
    if ctx.attr.netboot:
        env["netboot"] = True
    return [FxTestEnvironmentInfo(environment = env, is_variant = True)]

fx_test_environment_variant = rule(
    doc = """Adds infra settings, such as tags, to an existing environment.

    `base` can't itself be a variant, so that environments don't grow into
    deep hierarchies that are hard to follow.

    Example usage:

    ```bazel
    fx_test_environment_variant(
        name = "nuc11_isolated_env",
        base = "//build/testing/environments:nuc11_env",
        env_tags = ["e2e-isolated"],
    )
    ```
    """,
    implementation = _fx_test_environment_variant_impl,
    provides = [FxTestEnvironmentInfo],
    attrs = {
        "base": attr.label(
            doc = "The environment to start from. Its dimensions and emulator are kept.",
            providers = [FxTestEnvironmentInfo],
            mandatory = True,
        ),
        "env_tags": attr.string_list(
            doc = _ENV_TAGS_DOC + "\nAppended to the tags of `base`.",
        ),
        "service_account": attr.string(
            doc = _SERVICE_ACCOUNT_DOC + " Replaces the service account of `base`.",
        ),
        "netboot": attr.bool(doc = _NETBOOT_DOC),
    },
)
