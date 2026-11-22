#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for `extraction.py`."""

import os
import pathlib
import tempfile
import unittest
from unittest import mock

import extraction


class ExtractionTest(unittest.TestCase):
    def test_default_extracted_lean_name(self) -> None:
        self.assertEqual(
            extraction.default_extracted_lean_name("ebpf_core"), "EbpfCore.lean"
        )
        self.assertEqual(
            extraction.default_extracted_lean_name("fuchsia_merkle_core"),
            "FuchsiaMerkleCore.lean",
        )

    def test_toolchain_config_resolve_returns_none_when_unset(self) -> None:
        saved = {
            k: os.environ.pop(k, None)
            for k in (
                "LEAN_BIN_DIR",
                "AENEAS_BIN_DIR",
                "AENEAS_LEAN_DIR",
                "CHARON_RUST_TOOLCHAIN_DIR",
            )
        }
        try:
            self.assertIsNone(extraction.ToolchainConfig.resolve())
        finally:
            for k, v in saved.items():
                if v is not None:
                    os.environ[k] = v

    def test_toolchain_config_resolve_validates_flat_and_nested_binaries(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            lean_bin = root / "lean_bin"
            aeneas_dir = root / "aeneas"
            aeneas_lean = aeneas_dir / "backends" / "lean"
            rust_dir = root / "rust"

            lean_bin.mkdir(parents=True)
            (aeneas_dir / "charon" / "bin").mkdir(parents=True)
            (aeneas_dir / "bin").mkdir(parents=True)
            aeneas_lean.mkdir(parents=True)
            rust_dir.mkdir(parents=True)

            (lean_bin / "lean").write_text("")
            (lean_bin / "lake").write_text("")
            (aeneas_dir / "charon" / "bin" / "charon").write_text("")
            (aeneas_dir / "charon" / "bin" / "charon-driver").write_text("")
            (aeneas_dir / "bin" / "aeneas").write_text("")

            cfg = extraction.ToolchainConfig.resolve(
                lean_bin_dir=str(lean_bin),
                aeneas_bin_dir=str(aeneas_dir),
                charon_rust_toolchain_dir=str(rust_dir),
            )
            self.assertIsNotNone(cfg)
            assert cfg is not None
            self.assertEqual(
                cfg.charon_bin, aeneas_dir / "charon" / "bin" / "charon"
            )
            self.assertEqual(cfg.aeneas_bin, aeneas_dir / "bin" / "aeneas")
            self.assertEqual(cfg.lean4checker_bin, lean_bin / "leanchecker")

            (lean_bin / "lean4checker").write_text("")
            self.assertEqual(cfg.lean4checker_bin, lean_bin / "lean4checker")

    def test_stage_lake_overlay_files(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            lakefile = root / "lakefile.lean"
            assumptions = root / "Assumptions.lean"
            spec = root / "Spec.lean"
            contract = root / "Contract.lean"
            proof = root / "proof" / "Proof.lean"
            sub_proof = root / "proof" / "Proof" / "LoopLemmas.lean"
            extracted = root / "EbpfCore.lean"
            aeneas_lean = root / "aeneas_lean"

            proof.parent.mkdir(parents=True)
            sub_proof.parent.mkdir(parents=True)
            lakefile.write_text("-- lakefile\n")
            assumptions.write_text("-- assumptions\n")
            spec.write_text("-- spec\n")
            contract.write_text("-- contract\n")
            proof.write_text("-- proof\n")
            sub_proof.write_text("-- loop lemmas\n")
            extracted.write_text("namespace ebpf_core\ndef x : Nat := 1\n")

            (aeneas_lean / "Aeneas").mkdir(parents=True)
            (aeneas_lean / "Aeneas.lean").write_text("-- Aeneas\n")
            (aeneas_lean / ".lake" / "packages").mkdir(parents=True)
            (aeneas_lean / ".lake" / "build").mkdir(parents=True)
            (aeneas_lean / ".lake" / "build" / "placeholder.olean").write_text(
                "olean"
            )

            overlay = root / "overlay"
            extraction.stage_lake_overlay_files(
                overlay_dir=overlay,
                lakefile_path=lakefile,
                assumptions_path=assumptions,
                spec_path=spec,
                contract_path=contract,
                proof_paths=[proof, sub_proof],
                extracted_lean_source=extracted,
                aeneas_lean_dir=aeneas_lean,
            )

            self.assertTrue((overlay / "proof" / "Extracted.lean").is_file())
            self.assertTrue((overlay / "spec" / "Assumptions.lean").is_file())
            self.assertTrue((overlay / "spec" / "Spec.lean").is_file())
            self.assertTrue((overlay / "spec" / "Contract.lean").is_file())
            self.assertTrue((overlay / "proof" / "Proof.lean").is_file())
            self.assertTrue(
                (overlay / "proof" / "Proof" / "LoopLemmas.lean").is_file()
            )
            self.assertTrue((overlay / "Aeneas.lean").is_symlink())
            self.assertTrue((overlay / ".lake" / "packages").is_symlink())
            self.assertFalse((overlay / ".lake" / "build").is_symlink())
            self.assertTrue(
                (overlay / ".lake" / "build" / "placeholder.olean").is_file()
            )

    def test_prepare_lake_overlay_passes_charon_sysroot_before_separator(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            lib_rs = root / "lib.rs"
            lakefile = root / "lakefile.lean"
            assumptions = root / "Assumptions.lean"
            spec = root / "Spec.lean"
            contract = root / "Contract.lean"
            proof = root / "Proof.lean"
            for p in (lib_rs, lakefile, assumptions, spec, contract, proof):
                p.write_text("-- placeholder\n")

            toolchains = extraction.ToolchainConfig(
                lean_bin_dir=root / "lean_bin",
                aeneas_bin_dir=root / "aeneas",
                aeneas_lean_dir=root / "aeneas" / "backends" / "lean",
                charon_rust_toolchain_dir=root / "rust",
            )
            toolchains.aeneas_lean_dir.mkdir(parents=True)
            workdir = root / "workdir"
            workdir.mkdir()

            def fake_run(
                cmd: list[str],
                cwd: pathlib.Path,
                env: dict[str, str],
                check: bool,
            ) -> None:
                del cwd, env, check
                if "aeneas" in pathlib.Path(cmd[0]).name:
                    (workdir / "extracted" / "EbpfCore.lean").write_text(
                        "namespace ebpf_core\n"
                    )

            with mock.patch(
                "extraction.subprocess.run", side_effect=fake_run
            ) as mock_run:
                extraction.prepare_lake_overlay(
                    workdir=workdir,
                    crate_name="ebpf_core",
                    rust_edition="2024",
                    crate_srcs=[lib_rs],
                    lakefile_path=lakefile,
                    assumptions_path=assumptions,
                    spec_path=spec,
                    contract_path=contract,
                    proof_paths=[proof],
                    toolchains=toolchains,
                    charon_excludes=["core::fmt"],
                    charon_extra_args=[],
                    aeneas_max_recdepth=8192,
                    aeneas_extra_args=[],
                    extracted_lean_name="EbpfCore.lean",
                )
                charon_cmd = mock_run.call_args_list[0].args[0]
                sep_idx = charon_cmd.index("--")
                sysroot_idx = charon_cmd.index("--sysroot")
                self.assertLess(sysroot_idx, sep_idx)
                self.assertEqual(charon_cmd[sysroot_idx + 1], "default")

    def test_prepare_lake_overlay_raises_without_lib_rs(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            other_rs = root / "other.rs"
            lakefile = root / "lakefile.lean"
            assumptions = root / "Assumptions.lean"
            spec = root / "Spec.lean"
            contract = root / "Contract.lean"
            proof = root / "Proof.lean"
            for p in (other_rs, lakefile, assumptions, spec, contract, proof):
                p.write_text("-- placeholder\n")

            toolchains = extraction.ToolchainConfig(
                lean_bin_dir=root / "lean_bin",
                aeneas_bin_dir=root / "aeneas",
                aeneas_lean_dir=root / "aeneas" / "backends" / "lean",
                charon_rust_toolchain_dir=root / "rust",
            )
            workdir = root / "workdir"
            workdir.mkdir()

            with self.assertRaisesRegex(
                ValueError, "No `lib.rs` found in crate sources"
            ):
                extraction.prepare_lake_overlay(
                    workdir=workdir,
                    crate_name="ebpf_core",
                    rust_edition="2024",
                    crate_srcs=[other_rs],
                    lakefile_path=lakefile,
                    assumptions_path=assumptions,
                    spec_path=spec,
                    contract_path=contract,
                    proof_paths=[proof],
                    toolchains=toolchains,
                    charon_excludes=[],
                    charon_extra_args=[],
                    aeneas_max_recdepth=8192,
                    aeneas_extra_args=[],
                    extracted_lean_name="EbpfCore.lean",
                )


if __name__ == "__main__":
    unittest.main()
