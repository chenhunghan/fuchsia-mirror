# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Bazel macro for formally verifying Fuchsia Rust core crates with Charon, Aeneas, and Lean 4."""

load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")
load("//build/bazel/rules/host_tests:host_test.bzl", "host_test")

visibility(["//src/..."])

def rust_lean_verification_test(
        name,
        crate_name,
        crate_srcs,
        contract_lean_target,
        rust_edition = "2024",
        lakefile = "lakefile.lean",
        assumptions_lean = "spec/Assumptions.lean",
        spec_lean = "spec/Spec.lean",
        contract_lean = "spec/Contract.lean",
        proof_lean = ["proof/Proof.lean"],
        checker_module = "Contract",
        allow_sorry_in_proof = False,
        charon_excludes = [],
        charon_args = [],
        aeneas_max_recdepth = 8192,
        aeneas_args = [],
        timeout = "long",
        generate_test_suite = False,
        visibility = ["//:__subpackages__"]):
    """Defines a formal verification host test for a pure-safe-Rust core crate.

    When local or prebuilt Charon, Aeneas, and Lean 4 toolchains are provided
    (via environment variables or CLI arguments), this test extracts
    `Extracted.lean` on the fly via Charon + Aeneas, builds the Lake proof
    package, audits `#print axioms`, and replays proofs with `lean4checker`.
    When prebuilt toolchains are not configured in the environment, it skips
    cleanly (`--if-toolchains-present`).

    Args:
        name: Name of the generated `host_test` target.
        crate_name: Rust crate name (e.g. `"ebpf_core"`).
        crate_srcs: List of labels for the Rust source files in the core crate.
        contract_lean_target: Lake target name for `Contract.lean` (e.g. `"StarnixEbpfContract"`).
        rust_edition: Rust edition passed to Charon (default `"2024"`).
        lakefile: Label for `lakefile.lean`.
        assumptions_lean: Label for `spec/Assumptions.lean`.
        spec_lean: Label for `spec/Spec.lean`.
        contract_lean: Label for `spec/Contract.lean`.
        proof_lean: List of labels for `proof/*.lean` files.
        checker_module: Module name for `lean4checker --fresh` (default `"Contract"`).
        allow_sorry_in_proof: Whether `sorry` placeholders are temporarily permitted in
            `proof/*.lean` while proofs are under active construction. Never permits
            `sorry` or `axiom` in `spec/*.lean` or `Extracted.lean`.
        charon_excludes: Additional `--exclude` patterns for `charon rustc`.
        charon_args: Additional arguments for `charon rustc`.
        aeneas_max_recdepth: Recursion depth limit for Aeneas (default `8192`).
        aeneas_args: Additional arguments for `aeneas`.
        timeout: Bazel test timeout category (default `"long"`).
        generate_test_suite: Whether to generate a `host_tests` `test_suite`
            wrapping the verification test target (default `False`).
        visibility: Target visibility.
    """
    all_inputs = list(crate_srcs) + [
        lakefile,
        assumptions_lean,
        spec_lean,
        contract_lean,
    ] + list(proof_lean)

    verify_args = [
        "--crate-name",
        crate_name,
        "--rust-edition",
        rust_edition,
        "--lakefile",
        "$(location {})".format(lakefile),
        "--assumptions-lean",
        "$(location {})".format(assumptions_lean),
        "--spec-lean",
        "$(location {})".format(spec_lean),
        "--contract-lean",
        "$(location {})".format(contract_lean),
        "--contract-lean-target",
        contract_lean_target,
        "--checker-module",
        checker_module,
        "--aeneas-max-recdepth",
        str(aeneas_max_recdepth),
    ]

    for src in crate_srcs:
        verify_args.extend(["--crate-src", "$(location {})".format(src)])

    for p in proof_lean:
        verify_args.extend(["--proof-lean", "$(location {})".format(p)])

    for pattern in charon_excludes:
        verify_args.extend(["--charon-exclude", pattern])

    for arg in charon_args:
        verify_args.extend(["--charon-arg", arg])

    for arg in aeneas_args:
        verify_args.extend(["--aeneas-arg", arg])

    if allow_sorry_in_proof:
        verify_args.append("--allow-sorry-in-proof")

    host_test(
        name = name,
        binary = "//build/testing/formal_verification:verify",
        test_args = verify_args + ["--if-toolchains-present"],
        data = all_inputs + [
            "//build/bazel/rules/python:python_interpreter_as_test_data",
        ],
        host_test_wrapper_template = "//build/bazel:templates/template.host_py_test_wrapper.sh",
        host_test_wrapper_generator_args = [
            "--python-test-interpreter=test_data/python3",
        ],
        target_compatible_with = HOST_OS_CONSTRAINTS,
        timeout = timeout,
        visibility = visibility,
    )

    if generate_test_suite:
        native.test_suite(
            name = "host_tests",
            tests = [":" + name],
            target_compatible_with = HOST_OS_CONSTRAINTS,
            visibility = visibility,
        )
