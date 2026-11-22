#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""CLI entry point for Fuchsia Rust + Lean 4 formal verification tests.

Coordinates Charon + Aeneas extraction (`extraction.py`) and Lake build,
`#print axioms` verification, and `lean4checker` replay (`lake_audit.py`).
"""

import argparse
import os
import pathlib
import shutil
import sys
import tempfile
from collections.abc import Sequence

import extraction
import lake_audit


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Fuchsia Rust + Lean 4 formal verification test runner."
    )
    parser.add_argument("--crate-name", required=True)
    parser.add_argument("--rust-edition", default="2024")
    parser.add_argument(
        "--crate-src", action="append", dest="crate_srcs", required=True
    )
    parser.add_argument("--lakefile", required=True)
    parser.add_argument("--assumptions-lean", required=True)
    parser.add_argument("--spec-lean", required=True)
    parser.add_argument("--contract-lean", required=True)
    parser.add_argument(
        "--proof-lean", action="append", dest="proof_leans", required=True
    )
    parser.add_argument("--contract-lean-target", required=True)
    parser.add_argument("--checker-module", default=None)
    parser.add_argument(
        "--extracted-lean-name",
        default=None,
        help="Filename produced by Aeneas (defaults to UpperCamelCase(crate_name).lean).",
    )
    parser.add_argument(
        "--charon-exclude",
        action="append",
        dest="charon_excludes",
        default=None,
    )
    parser.add_argument(
        "--charon-arg", action="append", dest="charon_args", default=None
    )
    parser.add_argument("--aeneas-max-recdepth", type=int, default=8192)
    parser.add_argument(
        "--aeneas-arg", action="append", dest="aeneas_args", default=None
    )
    parser.add_argument(
        "--allow-sorry-in-proof",
        action="store_true",
        default=False,
        help="Temporarily allow `sorry` in proof/*.lean while proofs are under construction.",
    )
    parser.add_argument(
        "--if-toolchains-present",
        action="store_true",
        default=False,
        help="Skip extraction and Lake build if prebuilt toolchains are not configured.",
    )
    parser.add_argument(
        "--prepare-only",
        action="store_true",
        default=False,
        help="Extract `Extracted.lean` and stage the Lake overlay without running `lake build`.",
    )
    parser.add_argument(
        "--keep-workdir",
        default=None,
        help="Persist the generated Lake overlay in this directory for IDE/agent use.",
    )
    parser.add_argument("--lean-bin-dir", default=None)
    parser.add_argument("--aeneas-bin-dir", default=None)
    parser.add_argument("--aeneas-lean-dir", default=None)
    parser.add_argument("--charon-rust-toolchain-dir", default=None)
    args = parser.parse_args(argv)
    if args.prepare_only and not args.keep_workdir:
        parser.error("--prepare-only requires --keep-workdir")
    return args


def _run_verification(
    workdir: pathlib.Path,
    args: argparse.Namespace,
    crate_srcs: Sequence[pathlib.Path],
    lakefile_path: pathlib.Path,
    assumptions_path: pathlib.Path,
    spec_path: pathlib.Path,
    contract_path: pathlib.Path,
    proof_paths: Sequence[pathlib.Path],
    toolchains: extraction.ToolchainConfig,
    extracted_lean_name: str,
) -> None:
    overlay_dir = extraction.prepare_lake_overlay(
        workdir=workdir,
        crate_name=args.crate_name,
        rust_edition=args.rust_edition,
        crate_srcs=crate_srcs,
        lakefile_path=lakefile_path,
        assumptions_path=assumptions_path,
        spec_path=spec_path,
        contract_path=contract_path,
        proof_paths=proof_paths,
        toolchains=toolchains,
        charon_excludes=args.charon_excludes or (),
        charon_extra_args=args.charon_args or (),
        aeneas_max_recdepth=args.aeneas_max_recdepth,
        aeneas_extra_args=args.aeneas_args or (),
        extracted_lean_name=extracted_lean_name,
    )
    if args.prepare_only:
        print(f"Prepared Lake overlay workspace at: {overlay_dir}")
        return
    lake_audit.run_lake_and_audit(
        overlay_dir=overlay_dir,
        contract_target=args.contract_lean_target,
        checker_module=args.checker_module,
        allow_sorry_in_proof=args.allow_sorry_in_proof,
        toolchains=toolchains,
    )


def _resolve_input_path(raw_path: str) -> pathlib.Path:
    path = pathlib.Path(raw_path)
    if path.exists():
        return path
    candidates: list[pathlib.Path] = []
    for env_var in ("RUNFILES_DIR", "TEST_SRCDIR"):
        if (
            runfiles_root := os.environ.get(env_var)
        ) is not None and runfiles_root:
            candidates.append(pathlib.Path(runfiles_root) / "_main" / raw_path)
            candidates.append(pathlib.Path(runfiles_root) / raw_path)
    candidates.extend(
        (
            pathlib.Path(__file__).resolve().parents[3] / raw_path,
            pathlib.Path("verify.runfiles/_main") / raw_path,
        )
    )
    for candidate in candidates:
        if candidate.exists():
            return candidate
    return path


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)

    crate_srcs = [_resolve_input_path(p) for p in args.crate_srcs]
    lakefile_path = _resolve_input_path(args.lakefile)
    assumptions_path = _resolve_input_path(args.assumptions_lean)
    spec_path = _resolve_input_path(args.spec_lean)
    contract_path = _resolve_input_path(args.contract_lean)
    proof_paths = [_resolve_input_path(p) for p in args.proof_leans]

    for required_file in [
        lakefile_path,
        assumptions_path,
        spec_path,
        contract_path,
        *crate_srcs,
        *proof_paths,
    ]:
        if not required_file.is_file():
            raise ValueError(
                f"Required input file does not exist: {required_file}"
            )

    toolchains = extraction.ToolchainConfig.resolve(
        lean_bin_dir=args.lean_bin_dir,
        aeneas_bin_dir=args.aeneas_bin_dir,
        aeneas_lean_dir=args.aeneas_lean_dir,
        charon_rust_toolchain_dir=args.charon_rust_toolchain_dir,
    )

    if toolchains is None:
        if args.if_toolchains_present:
            lake_audit.check_forbidden_lean_source_files(
                spec_paths=[assumptions_path, spec_path, contract_path],
                proof_paths=proof_paths,
                allow_sorry=args.allow_sorry_in_proof,
            )
            print(
                "Prebuilt verification toolchains (Charon, Aeneas, Lean 4) are not "
                "configured in this environment; skipping full extraction/Lake verification "
                "after running static Lean construct checks.\n"
                "To run full extraction + Lean 4 proof verification locally, set:\n"
                "  LEAN_BIN_DIR=/path/to/lean/bin \\\n"
                "  AENEAS_BIN_DIR=/path/to/aeneas \\\n"
                "  AENEAS_LEAN_DIR=/path/to/aeneas/backends/lean \\\n"
                "  CHARON_RUST_TOOLCHAIN_DIR=/path/to/rustup/toolchains/nightly-..."
            )
            return 0
        raise ValueError(
            "Verification toolchains (LEAN_BIN_DIR, AENEAS_BIN_DIR, CHARON_RUST_TOOLCHAIN_DIR) "
            "must be configured unless `--if-toolchains-present` is passed."
        )

    extracted_lean_name = (
        args.extracted_lean_name
        or extraction.default_extracted_lean_name(args.crate_name)
    )

    if args.keep_workdir:
        workdir = pathlib.Path(args.keep_workdir).resolve()
        if workdir.exists():
            shutil.rmtree(workdir)
        workdir.mkdir(parents=True, exist_ok=True)
        _run_verification(
            workdir=workdir,
            args=args,
            crate_srcs=crate_srcs,
            lakefile_path=lakefile_path,
            assumptions_path=assumptions_path,
            spec_path=spec_path,
            contract_path=contract_path,
            proof_paths=proof_paths,
            toolchains=toolchains,
            extracted_lean_name=extracted_lean_name,
        )
        return 0

    with tempfile.TemporaryDirectory(
        prefix=f"fuchsia_verify_{args.crate_name}_"
    ) as tmp:
        _run_verification(
            workdir=pathlib.Path(tmp),
            args=args,
            crate_srcs=crate_srcs,
            lakefile_path=lakefile_path,
            assumptions_path=assumptions_path,
            spec_path=spec_path,
            contract_path=contract_path,
            proof_paths=proof_paths,
            toolchains=toolchains,
            extracted_lean_name=extracted_lean_name,
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
