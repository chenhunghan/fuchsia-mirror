# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Default Rust and Clippy lint configuration dictionaries."""

visibility([
    "//build/config/rust/lints",
    "//src/starnix/config",
])

CLIPPY_WARN_DEFAULT = {
    # Allow everything by default, except for the ones explicitly configured.
    # LINT.IfChange(clippy_warn_default)
    "all": "allow",
    "await_holding_lock": "warn",
    "clear_with_drain": "warn",
    "confusing_method_to_numeric_cast": "warn",
    "correctness": "warn",
    "expect_fun_call": "warn",
    "fn_to_numeric_cast_with_truncation": "warn",
    "into_iter_on_ref": "warn",
    "large_enum_variant": "warn",
    "large_futures": "warn",
    "missing_safety_doc": "warn",
    "or_fun_call": "warn",
    "unnecessary_mut_passed": "warn",
    # LINT.ThenChange(BUILD.gn:clippy_warn_default)
}

CLIPPY_WARN_PRODUCTION = CLIPPY_WARN_DEFAULT | {
    # LINT.IfChange(clippy_warn_production)
    "collection_is_never_read": "warn",
    "needless_collect": "warn",
    "perf": "warn",
    "redundant_clone": "warn",
    "set_contains_or_insert": "warn",
    "unnecessary_lazy_evaluations": "warn",
    # LINT.ThenChange(BUILD.gn:clippy_warn_production)
}

# The same rustc lint configs are applied to all Rust targets.
RUSTC_LINT_CONFIGS = {
    # LINT.IfChange(rustc_allow_default)
    "unknown_lints": "allow",
    # LINT.ThenChange(BUILD.gn:allow_default)

    # LINT.IfChange(rustc_warn_default)
    "missing_unsafe_on_extern": "warn",
    "unsafe_attr_outside_unsafe": "warn",
    "unused_crate_dependencies": "warn",
    # LINT.ThenChange(BUILD.gn:warn_default)

    # LINT.IfChange(rustc_deny_default)
    "deref_nullptr": "deny",
    "invalid_value": "deny",
    "unused_must_use": "deny",
    # LINT.ThenChange(BUILD.gn:deny_default)

    # LINT.IfChange(rustc_toolchain_roll_exceptions)
    "mismatched_lifetime_syntaxes": "allow",
    "non_local_definitions": "allow",
    "renamed_and_removed_lints": "allow",
    "rustdoc::redundant_explicit_links": "allow",
    "uncommon_codepoints": "allow",
    # LINT.ThenChange(BUILD.gn:toolchain_roll_exceptions)
}
