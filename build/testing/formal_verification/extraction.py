# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Toolchain discovery and Charon/Aeneas Lean extraction.

Resolves paths to external verification toolchains (`charon`, `charon-driver`,
`aeneas`, `lean`, `lake`, `lean4checker`) from CLI flags or environment
variables, runs `charon rustc --preset=aeneas` and `aeneas -backend lean` to
extract `Extracted.lean` on the fly, and stages the Lake overlay workspace.
"""

import dataclasses
import os
import pathlib
import shutil
import subprocess
from collections.abc import Sequence


@dataclasses.dataclass(frozen=True)
class ToolchainConfig:
    """Resolved paths to Charon, Aeneas, and Lean 4 toolchains."""

    lean_bin_dir: pathlib.Path
    aeneas_bin_dir: pathlib.Path
    aeneas_lean_dir: pathlib.Path
    charon_rust_toolchain_dir: pathlib.Path

    @property
    def charon_bin(self) -> pathlib.Path:
        flat = self.aeneas_bin_dir / "charon"
        if flat.is_file():
            return flat
        return self.aeneas_bin_dir / "charon" / "bin" / "charon"

    @property
    def charon_driver_bin(self) -> pathlib.Path:
        flat = self.aeneas_bin_dir / "charon-driver"
        if flat.is_file():
            return flat
        return self.aeneas_bin_dir / "charon" / "bin" / "charon-driver"

    @property
    def aeneas_bin(self) -> pathlib.Path:
        flat = self.aeneas_bin_dir / "aeneas"
        if flat.is_file():
            return flat
        return self.aeneas_bin_dir / "bin" / "aeneas"

    @property
    def lean_bin(self) -> pathlib.Path:
        return self.lean_bin_dir / "lean"

    @property
    def lake_bin(self) -> pathlib.Path:
        return self.lean_bin_dir / "lake"

    @property
    def lean4checker_bin(self) -> pathlib.Path:
        lean4checker = self.lean_bin_dir / "lean4checker"
        if lean4checker.is_file():
            return lean4checker
        return self.lean_bin_dir / "leanchecker"

    @classmethod
    def resolve(
        cls,
        lean_bin_dir: str | None = None,
        aeneas_bin_dir: str | None = None,
        aeneas_lean_dir: str | None = None,
        charon_rust_toolchain_dir: str | None = None,
    ) -> ToolchainConfig | None:
        """Resolves toolchain paths from explicit arguments or environment variables."""
        lean_dir = lean_bin_dir or os.environ.get("LEAN_BIN_DIR")
        aeneas_dir = aeneas_bin_dir or os.environ.get("AENEAS_BIN_DIR")
        aeneas_lean = aeneas_lean_dir or os.environ.get("AENEAS_LEAN_DIR")
        rust_dir = charon_rust_toolchain_dir or os.environ.get(
            "CHARON_RUST_TOOLCHAIN_DIR"
        )

        if not (lean_dir and aeneas_dir and rust_dir):
            return None

        resolved_aeneas_dir = pathlib.Path(aeneas_dir).resolve()
        resolved_aeneas_lean = (
            pathlib.Path(aeneas_lean).resolve()
            if aeneas_lean
            else (resolved_aeneas_dir / "backends" / "lean").resolve()
        )
        cfg = cls(
            lean_bin_dir=pathlib.Path(lean_dir).resolve(),
            aeneas_bin_dir=resolved_aeneas_dir,
            aeneas_lean_dir=resolved_aeneas_lean,
            charon_rust_toolchain_dir=pathlib.Path(rust_dir).resolve(),
        )
        for required_bin in (
            cfg.charon_bin,
            cfg.charon_driver_bin,
            cfg.aeneas_bin,
            cfg.lean_bin,
            cfg.lake_bin,
        ):
            if not required_bin.is_file():
                raise ValueError(
                    f"Configured verification toolchain binary does not exist: {required_bin}"
                )
        if not cfg.aeneas_lean_dir.is_dir():
            raise ValueError(
                f"Configured Aeneas Lean library directory does not exist: {cfg.aeneas_lean_dir}"
            )
        return cfg


def default_extracted_lean_name(crate_name: str) -> str:
    """Derives the default Aeneas Lean output filename (e.g. `ebpf_core` -> `EbpfCore.lean`)."""
    return (
        "".join(part.capitalize() for part in crate_name.split("_")) + ".lean"
    )


def stage_lake_overlay_files(
    overlay_dir: pathlib.Path,
    lakefile_path: pathlib.Path,
    assumptions_path: pathlib.Path,
    spec_path: pathlib.Path,
    contract_path: pathlib.Path,
    proof_paths: Sequence[pathlib.Path],
    extracted_lean_source: pathlib.Path,
    aeneas_lean_dir: pathlib.Path,
) -> pathlib.Path:
    """Stages the Lake overlay directory and links/copies the Aeneas Lean library cache."""
    (overlay_dir / "spec").mkdir(parents=True, exist_ok=True)
    (overlay_dir / "proof").mkdir(parents=True, exist_ok=True)

    extracted_dest = overlay_dir / "proof" / "Extracted.lean"
    shutil.copy2(extracted_lean_source, extracted_dest)

    shutil.copy2(lakefile_path, overlay_dir / "lakefile.lean")
    shutil.copy2(assumptions_path, overlay_dir / "spec" / "Assumptions.lean")
    shutil.copy2(spec_path, overlay_dir / "spec" / "Spec.lean")
    shutil.copy2(contract_path, overlay_dir / "spec" / "Contract.lean")
    for proof_path in proof_paths:
        parts = proof_path.parts
        if "proof" in parts:
            idx = len(parts) - 1 - parts[::-1].index("proof")
            rel_proof = pathlib.Path(*parts[idx + 1 :])
        else:
            rel_proof = pathlib.Path(proof_path.name)
        dest_proof = overlay_dir / "proof" / rel_proof
        dest_proof.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(proof_path, dest_proof)

    for entry in (
        "Aeneas",
        "Aeneas.lean",
        "AeneasMeta",
        "AeneasMeta.lean",
        "lake-manifest.json",
        "lean-toolchain",
        "aeneas-lean",
    ):
        src_entry = aeneas_lean_dir / entry
        dest_entry = overlay_dir / entry
        if src_entry.exists() and not dest_entry.exists():
            os.symlink(src_entry, dest_entry)

    # If `aeneas_lean_dir` has a prebuilt `.lake` directory, symlink `.lake/packages`
    # (read-only external dependencies like mathlib) and copy `.lake/build` so Lake
    # neither recompiles Aeneas from scratch nor copies gigabytes of package `.olean`s.
    prebuilt_lake = aeneas_lean_dir / ".lake"
    if prebuilt_lake.is_dir():
        dest_lake = overlay_dir / ".lake"
        dest_lake.mkdir(parents=True, exist_ok=True)
        for child in prebuilt_lake.iterdir():
            dest_child = dest_lake / child.name
            if dest_child.exists() or dest_child.is_symlink():
                continue
            if child.name == "packages":
                os.symlink(child, dest_child)
            elif child.is_dir():
                shutil.copytree(child, dest_child, symlinks=True)
            else:
                shutil.copy2(child, dest_child)

    return overlay_dir


def prepare_lake_overlay(
    workdir: pathlib.Path,
    crate_name: str,
    rust_edition: str,
    crate_srcs: Sequence[pathlib.Path],
    lakefile_path: pathlib.Path,
    assumptions_path: pathlib.Path,
    spec_path: pathlib.Path,
    contract_path: pathlib.Path,
    proof_paths: Sequence[pathlib.Path],
    toolchains: ToolchainConfig,
    charon_excludes: Sequence[str],
    charon_extra_args: Sequence[str],
    aeneas_max_recdepth: int,
    aeneas_extra_args: Sequence[str],
    extracted_lean_name: str,
) -> pathlib.Path:
    """Runs `charon rustc` and `aeneas` and stages the Lake overlay workspace."""
    lib_rs_candidate = next((p for p in crate_srcs if p.name == "lib.rs"), None)
    if lib_rs_candidate is None:
        raise ValueError(
            f"No `lib.rs` found in crate sources for `{crate_name}`: {[str(p) for p in crate_srcs]}"
        )
    lib_rs = lib_rs_candidate.resolve()
    llbc_path = workdir / f"{crate_name}.llbc"
    rmeta_path = workdir / f"lib{crate_name}.rmeta"
    extracted_dir = workdir / "extracted"
    extracted_dir.mkdir(parents=True, exist_ok=True)
    overlay_dir = workdir / "lean-overlay"

    env = os.environ.copy()
    rust_bin_dir = toolchains.charon_rust_toolchain_dir / "bin"
    rust_lib_dir = toolchains.charon_rust_toolchain_dir / "lib"
    env["PATH"] = (
        f"{toolchains.lean_bin_dir}:{rust_bin_dir}:{toolchains.aeneas_bin_dir}:"
        f"{toolchains.charon_bin.parent}:{env.get('PATH', '')}"
    )
    env["CHARON_TOOLCHAIN_IS_IN_PATH"] = "1"
    env.setdefault("CHARON_CACHE_DIR", str(workdir / "charon-cache"))
    env["RUSTC"] = str(rust_bin_dir / "rustc")
    ld_lib_path = env.get("LD_LIBRARY_PATH", "")
    env["LD_LIBRARY_PATH"] = (
        f"{rust_lib_dir}:{ld_lib_path}" if ld_lib_path else str(rust_lib_dir)
    )

    charon_cmd = [
        str(toolchains.charon_bin),
        "rustc",
        "--preset=aeneas",
        "--dest-file",
        str(llbc_path),
    ]
    for pattern in charon_excludes:
        charon_cmd.extend(["--exclude", pattern])
    charon_cmd.extend(charon_extra_args)
    if not any(
        arg == "--sysroot" or arg.startswith("--sysroot=")
        for arg in charon_extra_args
    ):
        miri_sysroot = toolchains.charon_rust_toolchain_dir / "miri-sysroot"
        charon_cmd.extend(
            [
                "--sysroot",
                str(miri_sysroot) if miri_sysroot.is_dir() else "default",
            ]
        )
    rustc_args = [
        "--crate-name",
        crate_name,
        "--crate-type",
        "rlib",
        "--edition",
        rust_edition,
        str(lib_rs),
        "--emit=metadata",
        "-o",
        str(rmeta_path),
    ]
    charon_cmd.extend(["--", *rustc_args])

    print(f"Extracting LLBC with Charon for `{crate_name}`...")
    subprocess.run(charon_cmd, cwd=workdir, env=env, check=True)

    aeneas_cmd = [
        str(toolchains.aeneas_bin),
        "-backend",
        "lean",
        "-dest",
        str(extracted_dir),
        "-max-recdepth",
        str(aeneas_max_recdepth),
    ]
    aeneas_cmd.extend(aeneas_extra_args)
    aeneas_cmd.append(str(llbc_path))

    print("Translating LLBC to Lean 4 with Aeneas...")
    subprocess.run(aeneas_cmd, cwd=workdir, env=env, check=True)

    generated_lean = extracted_dir / extracted_lean_name
    if not generated_lean.is_file():
        raise ValueError(
            f"Aeneas did not generate expected file `{generated_lean}` "
            f"(files in {extracted_dir}: {[p.name for p in extracted_dir.iterdir()]})."
        )

    stage_lake_overlay_files(
        overlay_dir=overlay_dir,
        lakefile_path=lakefile_path,
        assumptions_path=assumptions_path,
        spec_path=spec_path,
        contract_path=contract_path,
        proof_paths=proof_paths,
        extracted_lean_source=generated_lean,
        aeneas_lean_dir=toolchains.aeneas_lean_dir,
    )
    return overlay_dir
