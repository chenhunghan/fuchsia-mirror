#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for `lake_audit.py`."""

import pathlib
import tempfile
import unittest

import lake_audit


class LakeAuditTest(unittest.TestCase):
    def test_check_axiom_audit_output_accepts_single_and_multiline_output(
        self,
    ) -> None:
        output = (
            "info: spec/Contract.lean:10:0: 'StarnixEbpf.Contract.thm1' depends on axioms: [propext, Classical.choice]\n"
            "info: spec/Contract.lean:20:0: 'StarnixEbpf.Contract.thm2' depends on axioms: [propext,\n"
            " Classical.choice,\n"
            " Quot.sound]\n"
            "info: spec/Contract.lean:30:0: 'StarnixEbpf.Contract.thm3' does not depend on any axioms\n"
        )
        audited = lake_audit.check_axiom_audit_output(
            output,
            allow_sorry=False,
        )
        self.assertEqual(
            audited,
            [
                "StarnixEbpf.Contract.thm1",
                "StarnixEbpf.Contract.thm2",
                "StarnixEbpf.Contract.thm3",
            ],
        )

    def test_check_axiom_audit_output_handles_sorry_ax_and_warnings(
        self,
    ) -> None:
        output = (
            "warning: proof/Proof.lean:33:8: declaration uses `sorry`\n"
            "info: spec/Contract.lean:10:0: 'StarnixEbpf.Contract.thm1' depends on axioms: [\n"
            " propext,\n"
            " sorryAx,\n"
            " Quot.sound]\n"
        )
        with self.assertRaisesRegex(ValueError, "uses `sorry`"):
            lake_audit.check_axiom_audit_output(
                output,
                allow_sorry=False,
            )
        audited = lake_audit.check_axiom_audit_output(
            output,
            allow_sorry=True,
        )
        self.assertEqual(audited, ["StarnixEbpf.Contract.thm1"])

    def test_check_axiom_audit_output_rejects_sorry_in_spec_or_extracted(
        self,
    ) -> None:
        spec_sorry_output = (
            "warning: spec/Spec.lean:12:4: declaration uses `sorry`\n"
            "info: spec/Contract.lean:10:0: 'StarnixEbpf.Contract.thm1' depends on axioms: [sorryAx]\n"
        )
        with self.assertRaisesRegex(
            ValueError, "never permitted in `spec/Spec.lean`"
        ):
            lake_audit.check_axiom_audit_output(
                spec_sorry_output,
                allow_sorry=True,
            )

        extracted_sorry_output = (
            "warning: proof/Extracted.lean:99:2: declaration uses `sorry`\n"
            "info: spec/Contract.lean:10:0: 'StarnixEbpf.Contract.thm1' depends on axioms: [sorryAx]\n"
        )
        with self.assertRaisesRegex(
            ValueError, "never permitted in `proof/Extracted.lean`"
        ):
            lake_audit.check_axiom_audit_output(
                extracted_sorry_output,
                allow_sorry=True,
            )

    def test_check_axiom_audit_output_rejects_custom_axioms_and_empty_output(
        self,
    ) -> None:
        output = "info: 'StarnixEbpf.Contract.thm1' depends on axioms: [propext, MyCustomUnsoundAxiom]\n"
        with self.assertRaisesRegex(ValueError, "MyCustomUnsoundAxiom"):
            lake_audit.check_axiom_audit_output(
                output,
                allow_sorry=False,
            )

        with self.assertRaisesRegex(
            ValueError, "Did not find any `#print axioms` output"
        ):
            lake_audit.check_axiom_audit_output(
                "Build completed successfully.\n",
                allow_sorry=False,
            )

    def test_check_axiom_audit_output_accepts_bv_decide_reflection_axioms(
        self,
    ) -> None:
        output = (
            "info: spec/Contract.lean:10:0: 'StarnixEbpf.Contract.thm_bv' depends on axioms: [\n"
            " propext,\n"
            " Classical.choice,\n"
            " StarnixEbpf.Proof.set_code_bv._native.bv_decide.ax_1_5✝,\n"
            " StarnixEbpf.ScalarProofs.helper._native.bv_decide.ax_2,\n"
            " Quot.sound]\n"
        )
        audited = lake_audit.check_axiom_audit_output(
            output,
            allow_sorry=False,
        )
        self.assertEqual(audited, ["StarnixEbpf.Contract.thm_bv"])

        bad_output = (
            "info: 'StarnixEbpf.Contract.thm_bad' depends on axioms: "
            "[«bad._native.bv_decide.ax_1»]\n"
        )
        with self.assertRaisesRegex(ValueError, "disallowed axioms"):
            lake_audit.check_axiom_audit_output(
                bad_output,
                allow_sorry=False,
            )

    def test_check_forbidden_lean_constructs(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            overlay = pathlib.Path(tmp)
            (overlay / "spec").mkdir()
            (overlay / "proof").mkdir()

            (overlay / "spec" / "Spec.lean").write_text(
                "/-- **Informal**: opaque handle /- nested axiom -/ partial load. -/\n"
                "-- line comment with unsafe and sorry\n"
                'def msg : String := "axiom sorry native_decide"\n'
                "#print axioms my_thm\n",
                encoding="utf-8",
            )
            (overlay / "proof" / "Extracted.lean").write_text(
                "-- Auto-extracted\n",
                encoding="utf-8",
            )
            (overlay / "proof" / "Proof.lean").write_text(
                "theorem t : True := by sorry\n",
                encoding="utf-8",
            )

            lake_audit.check_forbidden_lean_constructs(
                overlay,
                allow_sorry=True,
            )
            with self.assertRaisesRegex(ValueError, "Forbidden `sorry`"):
                lake_audit.check_forbidden_lean_constructs(
                    overlay,
                    allow_sorry=False,
                )

            (overlay / "proof" / "Proof.lean").write_text(
                "axiom spoofed : False\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(
                ValueError, "Forbidden construct `axiom`"
            ):
                lake_audit.check_forbidden_lean_constructs(
                    overlay,
                    allow_sorry=True,
                )

            (overlay / "proof" / "Proof.lean").write_text(
                "theorem t : True := trivial\n",
                encoding="utf-8",
            )
            (overlay / "spec" / "Spec.lean").write_text(
                "partial def bad (x : Nat) : Nat := bad x\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(
                ValueError, "Forbidden construct `partial`"
            ):
                lake_audit.check_forbidden_lean_constructs(
                    overlay,
                    allow_sorry=True,
                )

            (overlay / "spec" / "Spec.lean").write_text(
                "-- clean\n",
                encoding="utf-8",
            )
            (overlay / "proof" / "Extracted.lean").write_text(
                "axiom external_fmt : Type\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(
                ValueError, "Forbidden construct `axiom`.*Extracted.lean"
            ):
                lake_audit.check_forbidden_lean_constructs(
                    overlay,
                    allow_sorry=True,
                )

    def test_check_forbidden_lean_constructs_sorry_ax_debug_sorry_and_partial(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            overlay = pathlib.Path(tmp)
            (overlay / "spec").mkdir()
            (overlay / "proof").mkdir()

            (overlay / "spec" / "Spec.lean").write_text(
                "theorem bad : False := sorryAx _ false\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(
                ValueError, "Forbidden construct `sorryAx`"
            ):
                lake_audit.check_forbidden_lean_constructs(
                    overlay, allow_sorry=True
                )

            (overlay / "spec" / "Spec.lean").write_text(
                "theorem bad2 : False := debug_sorry\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(
                ValueError, "Forbidden construct `debug_sorry`"
            ):
                lake_audit.check_forbidden_lean_constructs(
                    overlay, allow_sorry=True
                )

            (overlay / "spec" / "Spec.lean").write_text(
                "-- clean\n", encoding="utf-8"
            )
            (overlay / "proof" / "Proof.lean").write_text(
                "partial def loop (n : Nat) : Nat := loop n\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(
                ValueError, "Forbidden construct `partial`.*Proof.lean"
            ):
                lake_audit.check_forbidden_lean_constructs(
                    overlay, allow_sorry=True
                )

            for term in ("sorryAx", "debug_sorry"):
                (overlay / "proof" / "Proof.lean").write_text(
                    f"theorem t : False := {term}\n",
                    encoding="utf-8",
                )
                lake_audit.check_forbidden_lean_constructs(
                    overlay, allow_sorry=True
                )
                with self.assertRaisesRegex(ValueError, f"Forbidden `{term}`"):
                    lake_audit.check_forbidden_lean_constructs(
                        overlay, allow_sorry=False
                    )

    def test_check_axiom_audit_output_handles_dot_slash_and_single_quotes(
        self,
    ) -> None:
        output = (
            "warning: ./proof/Proof.lean:26:2: declaration uses 'sorry'\n"
            "info: spec/Contract.lean:10:0: 'StarnixEbpf.Contract.thm1' depends on axioms: [sorryAx]\n"
        )
        audited = lake_audit.check_axiom_audit_output(
            output,
            allow_sorry=True,
        )
        self.assertEqual(audited, ["StarnixEbpf.Contract.thm1"])
        with self.assertRaisesRegex(ValueError, "uses `sorry`"):
            lake_audit.check_axiom_audit_output(
                output,
                allow_sorry=False,
            )

        bad_spec_output = (
            "warning: ./spec/Spec.lean:4:0: declaration uses 'sorry'\n"
            "info: spec/Contract.lean:10:0: 'StarnixEbpf.Contract.thm1' depends on axioms: [sorryAx]\n"
        )
        with self.assertRaisesRegex(
            ValueError, "never permitted in `spec/Spec.lean`"
        ):
            lake_audit.check_axiom_audit_output(
                bad_spec_output,
                allow_sorry=True,
            )

    def test_check_axiom_audit_output_verifies_all_contract_declarations(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            contract = pathlib.Path(tmp) / "Contract.lean"
            contract.write_text(
                "namespace StarnixEbpf.Contract\n"
                "theorem thm1 : True := trivial\n"
                "#print axioms thm1\n"
                "section Inner\n"
                "lemma lem2 : True := trivial\n"
                "end Inner\n"
                "end StarnixEbpf.Contract\n",
                encoding="utf-8",
            )
            partial_output = (
                "info: spec/Contract.lean:3:0: 'StarnixEbpf.Contract.thm1' "
                "does not depend on any axioms\n"
            )
            with self.assertRaisesRegex(
                ValueError, "StarnixEbpf\\.Contract\\.lem2"
            ):
                lake_audit.check_axiom_audit_output(
                    partial_output,
                    allow_sorry=False,
                    contract_path=contract,
                )

            full_output = (
                partial_output
                + "info: spec/Contract.lean:6:0: 'StarnixEbpf.Contract.lem2' "
                "does not depend on any axioms\n"
            )
            audited = lake_audit.check_axiom_audit_output(
                full_output,
                allow_sorry=False,
                contract_path=contract,
            )
            self.assertEqual(
                audited,
                [
                    "StarnixEbpf.Contract.thm1",
                    "StarnixEbpf.Contract.lem2",
                ],
            )

    def test_check_axiom_audit_output_handles_primed_declaration_names(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            contract = pathlib.Path(tmp) / "Contract.lean"
            contract.write_text(
                "namespace StarnixEbpf.Contract\n"
                "theorem thm' : True := trivial\n"
                "lemma lem'' : True := trivial\n"
                "end StarnixEbpf.Contract\n",
                encoding="utf-8",
            )
            output = (
                "warning: ./proof/Proof.lean:1:0: declaration uses 'sorry'\n"
                "info: spec/Contract.lean:2:0: 'StarnixEbpf.Contract.thm'' "
                "depends on axioms: [sorryAx]\n"
                "info: spec/Contract.lean:3:0: 'StarnixEbpf.Contract.lem''' "
                "does not depend on any axioms\n"
            )
            audited = lake_audit.check_axiom_audit_output(
                output,
                allow_sorry=True,
                contract_path=contract,
            )
            self.assertEqual(
                audited,
                [
                    "StarnixEbpf.Contract.thm'",
                    "StarnixEbpf.Contract.lem''",
                ],
            )

    def test_extract_contract_theorems_compound_namespace_and_end(
        self,
    ) -> None:
        source = (
            "namespace Outer.Inner\n"
            "theorem t1 : True := trivial\n"
            "end\n"
            "theorem t_top : True := trivial\n"
            "namespace A.B\n"
            "theorem t_ab : True := trivial\n"
            "end B\n"
            "theorem t_a : True := trivial\n"
            "end A\n"
            "theorem t_root : True := trivial\n"
        )
        self.assertEqual(
            lake_audit.extract_contract_theorems(source),
            [
                ("Outer.Inner.t1", False),
                ("t_top", False),
                ("A.B.t_ab", False),
                ("A.t_a", False),
                ("t_root", False),
            ],
        )


if __name__ == "__main__":
    unittest.main()
