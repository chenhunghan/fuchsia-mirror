# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Lake build execution, `#print axioms` audit, and `lean4checker` replay.

Executes `lake build <contract_target>` in the staged Lake overlay directory,
inspects Lean compiler diagnostics for unauthorized `sorry` usage and
`#print axioms` results, and runs `lake env lean4checker --fresh <checker_module>`
when `lean4checker` is available in the Lean toolchain.
"""

import os
import pathlib
import re
import subprocess
import sys
from collections.abc import Collection, Sequence

import extraction

ALLOWED_AXIOMS = frozenset({"propext", "Classical.choice", "Quot.sound"})

# Lean 4's built-in `bv_decide` tactic verifies a CaDiCaL LRAT unsatisfiability
# certificate in Lean and records a kernel reflection axiom of the form
# `<Decl>._native.bv_decide.ax_<N>` (optionally suffixed with Lean's `✝` hygiene mark).
_BV_DECIDE_AXIOM_RE = re.compile(
    r"^[A-Za-z0-9_.'✝]+\._native\.bv_decide\.ax_[0-9_]+✝?$"
)

_SPEC_FORBIDDEN_CONSTRUCT_RE = re.compile(
    r"(?<![A-Za-z0-9_.'])(?:axiom|opaque|unsafe|implemented_by|extern|native_decide|partial|admit|sorry|sorryAx|debug_sorry)(?![A-Za-z0-9_'])"
)

_PROOF_FORBIDDEN_CONSTRUCT_RE = re.compile(
    r"(?<![A-Za-z0-9_.'])(?:axiom|opaque|unsafe|implemented_by|extern|native_decide|partial)(?![A-Za-z0-9_'])"
)

_PROOF_SORRY_CONSTRUCT_RE = re.compile(
    r"(?<![A-Za-z0-9_.'])(?:admit|sorry|sorryAx|debug_sorry)(?![A-Za-z0-9_'])"
)

_DEPENDS_ON_AXIOMS_RE = re.compile(
    r"'([^\n]+?)'\s+depends on axioms:\s*\[([^\]]*)\]",
    re.DOTALL,
)

_DOES_NOT_DEPEND_RE = re.compile(r"'([^\n]+?)'\s+does not depend on any axioms")

_SORRY_WARNING_RE = re.compile(
    r"warning:\s+([^\s:]+):\d+:\d+:\s+declaration uses [`']sorry[`']"
)

_NAMESPACE_DECL_RE = re.compile(r"^\s*namespace\s+([A-Za-z_][A-Za-z0-9_.']*)\b")
_SECTION_DECL_RE = re.compile(
    r"^\s*section(?:\s+([A-Za-z_][A-Za-z0-9_.']*))?\b"
)
_END_DECL_RE = re.compile(r"^\s*end(?:\s+([A-Za-z_][A-Za-z0-9_.']*))?\s*$")
_THEOREM_DECL_RE = re.compile(
    r"^\s*(?:@\[[^\]]*\]\s*)*(?P<modifiers>(?:(?:private|protected|noncomputable)\s+)*)(?:theorem|lemma)\s+(?P<name>[A-Za-z_«][A-Za-z0-9_.']*»?(?![A-Za-z0-9_']))"
)


def strip_lean_comments_and_strings(source: str) -> str:
    """Replaces Lean comments (`-- ...`, nested `/- ... -/`) and string literals with spaces, preserving newlines."""
    out: list[str] = []
    i = 0
    n = len(source)
    block_depth = 0
    in_line_comment = False
    in_string = False

    while i < n:
        ch = source[i]
        nxt = source[i + 1] if i + 1 < n else ""

        if in_line_comment:
            if ch == "\n":
                in_line_comment = False
                out.append("\n")
            else:
                out.append(" ")
            i += 1
            continue

        if block_depth > 0:
            if ch == "/" and nxt == "-":
                block_depth += 1
                out.append("  ")
                i += 2
            elif ch == "-" and nxt == "/":
                block_depth -= 1
                out.append("  ")
                i += 2
            elif ch == "\n":
                out.append("\n")
                i += 1
            else:
                out.append(" ")
                i += 1
            continue

        if in_string:
            if ch == "\\" and i + 1 < n:
                out.append("\n" if nxt == "\n" else "  ")
                i += 2
            elif ch == '"':
                in_string = False
                out.append(" ")
                i += 1
            elif ch == "\n":
                out.append("\n")
                i += 1
            else:
                out.append(" ")
                i += 1
            continue

        if ch == "/" and nxt == "-":
            block_depth = 1
            out.append("  ")
            i += 2
        elif ch == "-" and nxt == "-":
            in_line_comment = True
            out.append("  ")
            i += 2
        elif ch == '"':
            in_string = True
            out.append(" ")
            i += 1
        else:
            out.append(ch)
            i += 1

    return "".join(out)


def _check_lean_files(
    spec_files: Collection[tuple[str, pathlib.Path]],
    proof_files: Collection[tuple[str, pathlib.Path]],
    allow_sorry: bool = False,
) -> None:
    """Scans `spec_files` and `proof_files` for forbidden Lean constructs."""
    for rel_path, lean_file in spec_files:
        stripped = strip_lean_comments_and_strings(
            lean_file.read_text(encoding="utf-8")
        )
        m = _SPEC_FORBIDDEN_CONSTRUCT_RE.search(stripped)
        if m:
            line_no = stripped[: m.start()].count("\n") + 1
            raise ValueError(
                f"Forbidden construct `{m.group(0)}` found in `{rel_path}:{line_no}`."
            )

    for rel_path, lean_file in proof_files:
        stripped = strip_lean_comments_and_strings(
            lean_file.read_text(encoding="utf-8")
        )
        m = _PROOF_FORBIDDEN_CONSTRUCT_RE.search(stripped)
        if m:
            line_no = stripped[: m.start()].count("\n") + 1
            raise ValueError(
                f"Forbidden construct `{m.group(0)}` found in `{rel_path}:{line_no}`."
            )
        if lean_file.name == "Extracted.lean" or not allow_sorry:
            m_sorry = _PROOF_SORRY_CONSTRUCT_RE.search(stripped)
            if m_sorry:
                line_no = stripped[: m_sorry.start()].count("\n") + 1
                raise ValueError(
                    f"Forbidden `{m_sorry.group(0)}` found in `{rel_path}:{line_no}`."
                )


def check_forbidden_lean_constructs(
    overlay_dir: pathlib.Path,
    allow_sorry: bool = False,
) -> None:
    """Rejects forbidden Lean declarations/tactics (`axiom`, `opaque`, `unsafe`, `native_decide`, etc.) in `spec/` and `proof/`."""
    spec_dir = overlay_dir / "spec"
    spec_files = (
        tuple(
            (str(lean_file.relative_to(overlay_dir)), lean_file)
            for lean_file in sorted(spec_dir.rglob("*.lean"))
        )
        if spec_dir.is_dir()
        else ()
    )
    proof_dir = overlay_dir / "proof"
    proof_files = (
        tuple(
            (str(lean_file.relative_to(overlay_dir)), lean_file)
            for lean_file in sorted(proof_dir.rglob("*.lean"))
        )
        if proof_dir.is_dir()
        else ()
    )
    _check_lean_files(spec_files, proof_files, allow_sorry=allow_sorry)


def check_forbidden_lean_source_files(
    spec_paths: Sequence[pathlib.Path],
    proof_paths: Sequence[pathlib.Path],
    allow_sorry: bool = False,
) -> None:
    """Rejects forbidden Lean constructs directly in source spec and proof paths."""
    _check_lean_files(
        spec_files=tuple((str(p), p) for p in spec_paths),
        proof_files=tuple((str(p), p) for p in proof_paths),
        allow_sorry=allow_sorry,
    )


def extract_contract_theorems(contract_source: str) -> list[tuple[str, bool]]:
    """Extracts `(qualified_name, is_private)` pairs for `theorem`/`lemma` declarations in `Contract.lean`."""
    stripped = strip_lean_comments_and_strings(contract_source)
    scopes: list[tuple[str, tuple[str, ...]]] = []
    theorems: list[tuple[str, bool]] = []

    for line in stripped.splitlines():
        if (m_ns := _NAMESPACE_DECL_RE.match(line)) is not None:
            scopes.append(("namespace", tuple(m_ns.group(1).split("."))))
            continue

        if (m_sec := _SECTION_DECL_RE.match(line)) is not None:
            sec_name = m_sec.group(1)
            scopes.append(
                ("section", tuple(sec_name.split(".")) if sec_name else ())
            )
            continue

        if (m_end := _END_DECL_RE.match(line)) is not None:
            end_name = m_end.group(1)
            if end_name is None:
                if scopes:
                    scopes.pop()
            else:
                remaining = end_name.split(".")
                while remaining and scopes:
                    kind, parts = scopes.pop()
                    parts_list = list(parts)
                    while (
                        remaining
                        and parts_list
                        and parts_list[-1] == remaining[-1]
                    ):
                        remaining.pop()
                        parts_list.pop()
                    if parts_list and kind == "namespace":
                        scopes.append((kind, tuple(parts_list)))
            continue

        if (m_thm := _THEOREM_DECL_RE.match(line)) is not None:
            raw_name = m_thm.group("name")
            modifiers = m_thm.group("modifiers").split()
            is_private = "private" in modifiers
            if raw_name.startswith("_root_."):
                qualified = raw_name[len("_root_.") :]
            else:
                ns_parts = [
                    part
                    for kind, parts in scopes
                    if kind == "namespace"
                    for part in parts
                ]
                qualified = (
                    ".".join([*ns_parts, raw_name]) if ns_parts else raw_name
                )
            theorems.append((qualified, is_private))

    return theorems


def _is_theorem_observed(
    qualified_name: str,
    is_private: bool,
    observed: dict[str, list[str]],
) -> bool:
    if qualified_name in observed:
        return True
    if is_private and any(
        obs.startswith("_private.") and obs.endswith(f".{qualified_name}")
        for obs in observed
    ):
        return True
    return False


def is_allowed_axiom(axiom_name: str, allow_sorry: bool = False) -> bool:
    """Returns whether `axiom_name` is permitted in `#print axioms` output."""
    if axiom_name in ALLOWED_AXIOMS:
        return True
    if allow_sorry and axiom_name == "sorryAx":
        return True
    if _BV_DECIDE_AXIOM_RE.match(axiom_name) is not None:
        return True
    return False


def check_axiom_audit_output(
    lake_output: str,
    allow_sorry: bool = False,
    contract_path: pathlib.Path | None = None,
) -> list[str]:
    """Audits Lean `sorry` warnings and `#print axioms` output emitted by `lake build`."""
    for m_sorry in _SORRY_WARNING_RE.finditer(lake_output):
        rel_path = m_sorry.group(1).replace("\\", "/")
        while rel_path.startswith("./"):
            rel_path = rel_path[2:]
        if (
            not rel_path.startswith("proof/")
            or rel_path == "proof/Extracted.lean"
        ):
            raise ValueError(
                f"`sorry` is never permitted in `{rel_path}` (only in `proof/` when `--allow-sorry-in-proof` is set)."
            )
        if not allow_sorry:
            raise ValueError(
                f"Declaration in `{rel_path}` uses `sorry`, which is forbidden when `--allow-sorry-in-proof` is not set."
            )

    allowed = set(ALLOWED_AXIOMS)
    if allow_sorry:
        allowed.add("sorryAx")

    observed: dict[str, list[str]] = {}
    for m_dep in _DEPENDS_ON_AXIOMS_RE.finditer(lake_output):
        thm_name = m_dep.group(1).strip()
        axioms = [a.strip() for a in m_dep.group(2).split(",") if a.strip()]
        observed[thm_name] = axioms

    for m_none in _DOES_NOT_DEPEND_RE.finditer(lake_output):
        thm_name = m_none.group(1).strip()
        observed[thm_name] = []

    if not observed:
        raise ValueError(
            "Did not find any `#print axioms` output in `lake build` output. "
            "Ensure `spec/Contract.lean` emits `#print axioms` for its theorems."
        )

    if contract_path is not None:
        expected_theorems = extract_contract_theorems(
            contract_path.read_text(encoding="utf-8")
        )
        if not expected_theorems:
            raise ValueError(
                f"Did not find any `theorem` or `lemma` declarations in `{contract_path}`."
            )
        missing = [
            qualified_name
            for qualified_name, is_private in expected_theorems
            if not _is_theorem_observed(qualified_name, is_private, observed)
        ]
        if missing:
            raise ValueError(
                f"Missing `#print axioms` output for `spec/Contract.lean` declarations: "
                f"{missing}. Ensure every `theorem` and `lemma` in `spec/Contract.lean` "
                f"emits `#print axioms`."
            )

    for thm_name, axioms in observed.items():
        disallowed = [a for a in axioms if not is_allowed_axiom(a, allow_sorry)]
        if disallowed:
            raise ValueError(
                f"Theorem `{thm_name}` depends on disallowed axioms: {disallowed} "
                f"(allowed: {sorted(allowed)} + `._native.bv_decide.ax_*`)."
            )

    return list(observed.keys())


def run_lake_and_audit(
    overlay_dir: pathlib.Path,
    contract_target: str,
    checker_module: str | None,
    allow_sorry_in_proof: bool,
    toolchains: extraction.ToolchainConfig,
) -> None:
    """Runs `lake build`, audits `#print axioms` output, and runs kernel replay."""
    check_forbidden_lean_constructs(
        overlay_dir,
        allow_sorry=allow_sorry_in_proof,
    )

    env = os.environ.copy()
    env["PATH"] = f"{toolchains.lean_bin_dir}:{env.get('PATH', '')}"

    # Remove any stale build artifacts for our crate modules while keeping prebuilt Aeneas `.lake`.
    module_rel_stems = {
        pathlib.Path("Extracted"),
        pathlib.Path("Assumptions"),
        pathlib.Path("Spec"),
        pathlib.Path("Proof"),
        pathlib.Path("Contract"),
    }
    for sub in ("spec", "proof"):
        sub_dir = overlay_dir / sub
        if sub_dir.is_dir():
            for lean_file in sub_dir.rglob("*.lean"):
                module_rel_stems.add(
                    lean_file.relative_to(sub_dir).with_suffix("")
                )

    build_lib_dir = overlay_dir / ".lake" / "build" / "lib" / "lean"
    if build_lib_dir.is_dir():
        for rel_stem in sorted(module_rel_stems):
            for ext in (
                ".olean",
                ".olean.server",
                ".olean.private",
                ".ilean",
                ".ir",
                ".c",
                ".trace",
            ):
                candidate = build_lib_dir / f"{rel_stem}{ext}"
                if candidate.exists():
                    candidate.unlink()

    cmd = [str(toolchains.lake_bin), "build", contract_target]
    print(f"Building `{contract_target}` with Lake...")
    proc = subprocess.run(
        cmd,
        cwd=overlay_dir,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        check=False,
    )
    sys.stdout.write(proc.stdout)
    if proc.returncode != 0:
        raise ValueError(
            f"`lake build {contract_target}` failed with exit code {proc.returncode}."
        )

    contract_path = overlay_dir / "spec" / "Contract.lean"
    audited = check_axiom_audit_output(
        proc.stdout,
        allow_sorry=allow_sorry_in_proof,
        contract_path=contract_path,
    )
    print(
        f"Verified `#print axioms` for {len(audited)} theorems "
        f"(allow_sorry={allow_sorry_in_proof})."
    )

    checker_bin = toolchains.lean4checker_bin
    if checker_module and checker_bin.is_file():
        checker_cmd = [
            str(toolchains.lake_bin),
            "env",
            str(checker_bin),
        ]
        if checker_bin.name == "lean4checker":
            checker_cmd.append("--fresh")
        checker_cmd.append(checker_module)
        print(f"Running `{' '.join(checker_cmd[2:])}`...")
        subprocess.run(
            checker_cmd,
            cwd=overlay_dir,
            env=env,
            check=True,
        )
