-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Proof.InsnAluEndExit

/-!
# Conditional Jump & Branch Constraint Soundness

Proves forward abstract-state validity and backward 2-safety / termination
transfer (`SemTerminatesIn`) for all 22 conditional jump instructions
(`jeq`, `jne`, `jgt`, `jge`, `jlt`, `jle`, `jsgt`, `jsge`, `jslt`, `jsle`,
`jset` in 64-bit and 32-bit widths), including:
1. `verifier.comp_eq64`, `verifier.comp_eq32`, `verifier.BranchCondOp.eval`,
   `verifier.ComputationContext.eval_same_region_ptr`, and
   `verifier.ComputationContext.compute_branch` (including 2-safety branch
   determinism across arbitrary `ValidHostLayout`s).
2. `verifier.Type.constrain_equal_offsets`, `verifier.Type.constraint`, and
   `verifier.ComputationContext.apply_constraints_and_register` (scalar range &
   tnum intersection, 32-bit low-half equality refinement, `NullOr` splitting
   with `unregister_resource` / `set_null`, same-region pointer offset equality,
   and packet `PtrToArray` / `PtrToEndArray` `update_array_bounds`).
3. `verifier.ComputationContext.conditional_jump` and visitor dispatchers
   `visit_condJump_forward` and `visit_condJump_backward`.
-/

set_option linter.unusedTactic false
set_option linter.unnecessarySimpa false
set_option linter.unnecessarySeqFocus false
set_option maxHeartbeats 1600000

open Aeneas Aeneas.Std Result

namespace StarnixEbpf.Proof

open StarnixEbpf.Assumptions
open StarnixEbpf.Spec
open StarnixEbpf.LoopLemmas
open ebpf_core
open ebpf_core.scalar_value

/-! ## 1. Opcode Mappings & Unified Semantic Branch Condition -/

def jumpWidthOfBool (is64 : Bool) : verifier.JumpWidth :=
  if is64 then .W64 else .W32

def jumpTypeOfOp : JumpOp → verifier.JumpType
  | .eq  => .Eq
  | .ne  => .Ne
  | .gt  => .Gt
  | .ge  => .Ge
  | .lt  => .Lt
  | .le  => .Le
  | .sgt => .StrictComparaison
  | .sge => .LooseComparaison
  | .slt => .StrictComparaison
  | .sle => .LooseComparaison
  | .set => .Unknown

def condOpOfJump (is64 : Bool) : JumpOp → verifier.BranchCondOp :=
  if is64 then
    fun op => match op with
      | .eq  => .Eq64  | .ne  => .Ne64
      | .gt  => .Gt64  | .ge  => .Ge64
      | .lt  => .Lt64  | .le  => .Le64
      | .sgt => .Sgt64 | .sge => .Sge64
      | .slt => .Slt64 | .sle => .Sle64
      | .set => .Set64
  else
    fun op => match op with
      | .eq  => .Eq32  | .ne  => .Ne32
      | .gt  => .Gt32  | .ge  => .Ge32
      | .lt  => .Lt32  | .le  => .Le32
      | .sgt => .Sgt32 | .sge => .Sge32
      | .slt => .Slt32 | .sle => .Sle32
      | .set => .Set32

def jmpLogOpOfJump (is64 : Bool) : JumpOp → verifier.JmpLogOp :=
  if is64 then
    fun op => match op with
      | .eq  => .Jeq64  | .ne  => .Jne64
      | .gt  => .Jgt64  | .ge  => .Jge64
      | .lt  => .Jlt64  | .le  => .Jle64
      | .sgt => .Jsgt64 | .sge => .Jsge64
      | .slt => .Jslt64 | .sle => .Jsle64
      | .set => .Jset64
  else
    fun op => match op with
      | .eq  => .Jeq32  | .ne  => .Jne32
      | .gt  => .Jgt32  | .ge  => .Jge32
      | .lt  => .Jlt32  | .le  => .Jle32
      | .sgt => .Jsgt32 | .sge => .Jsge32
      | .slt => .Jslt32 | .sle => .Jsle32
      | .set => .Jset32

def pureInvertJumpType : verifier.JumpType → verifier.JumpType
  | .Eq => .Ne
  | .Ge => .Lt
  | .Gt => .Le
  | .Le => .Gt
  | .LooseComparaison => .StrictComparaison
  | .Lt => .Ge
  | .Ne => .Eq
  | .StrictComparaison => .LooseComparaison
  | .Unknown => .Unknown

theorem JumpType_invert_eq_ok (jt : verifier.JumpType) :
    verifier.JumpType.invert jt = ok (pureInvertJumpType jt) := by
  cases jt <;> rfl

def pureJumpTypeNeUnknown : verifier.JumpType → Bool
  | .Unknown => false
  | _ => true

theorem JumpType_ne_unknown_eq_ok (jt : verifier.JumpType) :
    core.cmp.PartialEq.ne.trait_default
      verifier.JumpType.Insts.CoreCmpPartialEqJumpType jt .Unknown =
      ok (pureJumpTypeNeUnknown jt) := by
  unfold core.cmp.PartialEq.ne.trait_default core.cmp.PartialEq.ne.default
    verifier.JumpType.Insts.CoreCmpPartialEqJumpType
    verifier.JumpType.Insts.CoreCmpPartialEqJumpType.eq
    verifier.JumpType.tag pureJumpTypeNeUnknown
  cases jt <;> simp [bind_ok]

theorem condJumpMethod_compCtx_eq (is64 : Bool) (op : JumpOp)
    (self : verifier.ComputationContext) (vctx : verifier.VerificationContext)
    (dst : Std.U8) (src : visitor.Source) (off : Std.I16) :
    condJumpMethod CompCtxVisitorInst is64 op self vctx dst src off =
      (do
        let (r, vctx1) ←
          verifier.ComputationContext.conditional_jump self
            (jmpLogOpOfJump is64 op) vctx dst src off
            (jumpTypeOfOp op) (jumpWidthOfBool is64) (condOpOfJump is64 op)
        ok (r, self, vctx1)) := by
  cases is64 <;> cases op <;> rfl

theorem condJumpMethod_depTracker_eq (is64 : Bool) (op : JumpOp)
    (self : verifier.DependencyTracker) (vctx : verifier.VerificationContext)
    (dst : Std.U8) (src : visitor.Source) (off : Std.I16) :
    condJumpMethod DepTrackerVisitorInst is64 op self vctx dst src off =
      (do
        let (r, dd) ← verifier.DataDependencies.jmp self.deps dst src
        ok (r, { self with deps := dd }, vctx)) := by
  cases is64 <;> cases op <;> rfl

/-- Semantic relationship between operand bitvectors `b1, b2` guaranteed along a
    branch with `JumpType` `jt` and `JumpWidth` `jw`. -/
def JumpTypeHolds (jt : verifier.JumpType) (jw : verifier.JumpWidth)
    (b1 b2 : BitVec 64) : Prop :=
  match jw with
  | .W32 =>
      match jt with
      | .Eq => b1.setWidth 32 = b2.setWidth 32
      | _ => True
  | .W64 =>
      match jt with
      | .Eq => b1 = b2
      | .Ne => b1 ≠ b2
      | .Ge => b2 ≤ b1
      | .Gt => b2 < b1
      | .Le => b1 ≤ b2
      | .Lt => b1 < b2
      | .StrictComparaison => b1 ≠ b2
      | .LooseComparaison => True
      | .Unknown => True

theorem JumpTypeHolds_taken (is64 : Bool) (op : JumpOp) (b1 b2 : BitVec 64)
    (h : (if is64 then evalJump64 op b1 b2 else evalJump32 op b1 b2) = true) :
    JumpTypeHolds (jumpTypeOfOp op) (jumpWidthOfBool is64) b1 b2 := by
  cases is64 <;> cases op <;>
    simp only [Bool.false_eq_true, ↓reduceIte, evalJump64, evalJump32,
      jumpTypeOfOp, jumpWidthOfBool, JumpTypeHolds] at h ⊢
  all_goals
    try trivial
    try bv_decide

theorem JumpTypeHolds_skipped (is64 : Bool) (op : JumpOp) (b1 b2 : BitVec 64)
    (h : (if is64 then evalJump64 op b1 b2 else evalJump32 op b1 b2) = false) :
    JumpTypeHolds (pureInvertJumpType (jumpTypeOfOp op)) (jumpWidthOfBool is64) b1 b2 := by
  cases is64 <;> cases op <;>
    simp only [Bool.false_eq_true, ↓reduceIte, evalJump64, evalJump32,
      jumpTypeOfOp, pureInvertJumpType, jumpWidthOfBool, JumpTypeHolds] at h ⊢
  all_goals
    try trivial
    try bv_decide

/-! ## 2. `comp_eq64`, `comp_eq32`, `BranchCondOp.eval`, & `compute_branch` Soundness -/

private theorem not_uninit_of_fully_init_64 {d : U64ScalarValueData}
    (h : U64ScalarValueData.is_fully_initialized d = ok true) :
    d.unwritten_mask = 0#u64 ∧ d ≠ uninitScalar64 := by
  unfold U64ScalarValueData.is_fully_initialized at h
  have hunw : d.unwritten_mask = 0#u64 := of_decide_eq_true (ok_inj h)
  refine ⟨hunw, fun hc => ?_⟩
  rw [hc] at hunw
  unfold uninitScalar64 at hunw
  cases (u64_eq_iff_bv _ _).mp hunw

private theorem not_uninit_of_fully_init_32 {d : U32ScalarValueData}
    (h : U32ScalarValueData.is_fully_initialized d = ok true) :
    d.unwritten_mask = 0#u32 := by
  unfold U32ScalarValueData.is_fully_initialized at h
  exact of_decide_eq_true (ok_inj h)

private theorem not_uninit_of_is_zero_64 {d : U64ScalarValueData}
    (h : U64ScalarValueData.is_zero d = ok true) :
    d ≠ uninitScalar64 := by
  rw [is_zero_64_eq_ok] at h
  have hb := ok_inj h
  simp only [Bool.and_eq_true, decide_eq_true_eq] at hb
  intro hc
  have hunk : d.unknown_mask = 0#u64 := hb.1
  rw [hc] at hunk
  unfold uninitScalar64 at hunk
  cases (u64_eq_iff_bv _ _).mp hunk

private theorem not_uninit_of_from_u64_init_32 {d : U64ScalarValueData} {d32 : U32ScalarValueData}
    (hf : U32ScalarValueData.from_u64_scalar d = ok d32)
    (hi : U32ScalarValueData.is_fully_initialized d32 = ok true) :
    d ≠ uninitScalar64 := by
  intro hc
  subst d
  obtain ⟨r32, hr32, hrel32⟩ :=
    from_u64_scalar_sound_2safety uninitScalar64 (concretizesScalar64Rel2_uninit 0#64 1#64)
  rw [hf] at hr32
  cases ok_inj hr32
  have heq := fully_initialized_eq_of_rel2_32 (not_uninit_of_fully_init_32 hi) hrel32
  revert heq; decide

private theorem not_uninit_of_from_into_u64_init_64
    {d : U64ScalarValueData} {d32 : U32ScalarValueData} {d64 : U64ScalarValueData}
    (hf : U32ScalarValueData.from_u64_scalar d = ok d32)
    (hi64 : U32ScalarValueData.into_u64_scalar d32 = ok d64)
    (hinit : U64ScalarValueData.is_fully_initialized d64 = ok true) :
    d ≠ uninitScalar64 := by
  intro hc
  subst d
  obtain ⟨r32, hr32, hrel32⟩ :=
    from_u64_scalar_sound_2safety uninitScalar64 (concretizesScalar64Rel2_uninit 0#64 1#64)
  rw [hf] at hr32
  cases ok_inj hr32
  obtain ⟨r64, hr64, hrel64⟩ := into_u64_scalar_sound_2safety d32 hrel32
  rw [hi64] at hr64
  cases ok_inj hr64
  have heq := fully_initialized_eq_of_rel2_64 (not_uninit_of_fully_init_64 hinit).1 hrel64
  revert heq; decide

theorem comp_eq64_not_uninit {x y : U64ScalarValueData} {opt : Option Bool}
    (h : verifier.comp_eq64 x y = ok (.Ok opt)) :
    x ≠ uninitScalar64 ∧ y ≠ uninitScalar64 := by
  unfold verifier.comp_eq64 at h
  rcases bind_eq_ok h with ⟨bx, hbx, h⟩
  cases bx with
  | false => cases ok_inj h
  | true =>
    simp only [↓reduceIte] at h
    rcases bind_eq_ok h with ⟨by_, hby, _⟩
    cases by_ with
    | false => cases ok_inj ‹_›
    | true =>
      exact ⟨(not_uninit_of_fully_init_64 hbx).2, (not_uninit_of_fully_init_64 hby).2⟩

theorem comp_eq64_ok_sound {x y : U64ScalarValueData} {opt : Option Bool}
    {x₁ x₂ y₁ y₂ : BitVec 64}
    (h : verifier.comp_eq64 x y = ok (.Ok opt))
    (hx : ConcretizesScalar64Rel2 x x₁ x₂)
    (hy : ConcretizesScalar64Rel2 y y₁ y₂) :
    x₁ = x₂ ∧ y₁ = y₂ ∧ (∀ b, opt = some b → (x₁ == y₁) = b) := by
  unfold verifier.comp_eq64 at h
  rcases bind_eq_ok h with ⟨bx, hbx, h⟩
  cases bx with
  | false => cases ok_inj h
  | true =>
    simp only [↓reduceIte] at h
    rcases bind_eq_ok h with ⟨by_, hby, h⟩
    cases by_ with
    | false => cases ok_inj h
    | true =>
      simp only [↓reduceIte] at h
      have hx_eq := fully_initialized_eq_of_rel2_64 (not_uninit_of_fully_init_64 hbx).1 hx
      have hy_eq := fully_initialized_eq_of_rel2_64 (not_uninit_of_fully_init_64 hby).1 hy
      have hminx : U64ScalarValueData.min x = ok x.urange.min := rfl
      have hmaxx : U64ScalarValueData.max x = ok x.urange.max := rfl
      have hminy : U64ScalarValueData.min y = ok y.urange.min := rfl
      have hmaxy : U64ScalarValueData.max y = ok y.urange.max := rfl
      rw [bind_ok_eq hminx, bind_ok_eq hmaxx] at h
      refine ⟨hx_eq, hy_eq, ?_⟩
      have hne_branch :
          ∀ {opt0 : Option Bool},
            (do
              let b2 ← U64ScalarValueData.can_be_equal x y
              if b2 then ok (core.result.Result.Ok none : core.result.Result (Option Bool) Unit)
              else ok (core.result.Result.Ok (some false) : core.result.Result (Option Bool) Unit)) =
              ok (core.result.Result.Ok opt0 : core.result.Result (Option Bool) Unit) →
            ∀ b, opt0 = some b → (x₁ == y₁) = b := by
        intro opt0 hcbe b hb
        rcases bind_eq_ok hcbe with ⟨b2, hb2, hcbe⟩
        cases b2 with
        | true =>
          simp only [↓reduceIte] at hcbe
          cases core.result.Result.Ok.inj (ok_inj hcbe)
          cases hb
        | false =>
          simp only [Bool.false_eq_true, ↓reduceIte] at hcbe
          cases core.result.Result.Ok.inj (ok_inj hcbe)
          cases Option.some.inj hb
          exact beq_eq_false_iff_ne.mpr (can_be_equal_64_false_sound hb2 hx.1 hy.1)
      by_cases h1 : x.urange.min = x.urange.max
      · simp only [h1, ↓reduceIte] at h
        rw [bind_ok_eq hminy] at h
        by_cases h2 : x.urange.max = y.urange.min
        · simp only [h2, ↓reduceIte] at h
          rw [bind_ok_eq hmaxy] at h
          by_cases h3 : y.urange.min = y.urange.max
          · simp only [h3, ↓reduceIte] at h
            cases core.result.Result.Ok.inj (ok_inj h)
            intro b hb
            cases Option.some.inj hb
            have h1_bv := (u64_eq_iff_bv _ _).mp h1
            have h2_bv := (u64_eq_iff_bv _ _).mp h2
            have h3_bv := (u64_eq_iff_bv _ _).mp h3
            have hxmin := hx.1.2.2.1
            have hxmax := hx.1.2.2.2
            have hymin := hy.1.2.2.1
            have hymax := hy.1.2.2.2
            have hxy : x₁ = y₁ := by revert h1_bv h2_bv h3_bv hxmin hxmax hymin hymax; bv_decide
            exact beq_iff_eq.mpr hxy
          · simp only [h3, ↓reduceIte] at h
            exact hne_branch h
        · simp only [h2, ↓reduceIte] at h
          exact hne_branch h
      · simp only [h1, ↓reduceIte] at h
        exact hne_branch h

theorem comp_eq32_not_uninit {x y : U64ScalarValueData} {opt : Option Bool}
    (h : verifier.comp_eq32 x y = ok (.Ok opt)) :
    x ≠ uninitScalar64 ∧ y ≠ uninitScalar64 := by
  unfold verifier.comp_eq32 at h
  rcases bind_eq_ok h with ⟨x32, hx32, h⟩
  rcases bind_eq_ok h with ⟨x64, hx64, h⟩
  rcases bind_eq_ok h with ⟨y32, hy32, h⟩
  rcases bind_eq_ok h with ⟨y64, hy64, h⟩
  unfold verifier.comp_eq64 at h
  rcases bind_eq_ok h with ⟨bx, hbx, h⟩
  cases bx with
  | false => cases ok_inj h
  | true =>
    simp only [↓reduceIte] at h
    rcases bind_eq_ok h with ⟨by_, hby, h⟩
    cases by_ with
    | false => cases ok_inj h
    | true =>
      exact ⟨not_uninit_of_from_into_u64_init_64 hx32 hx64 hbx,
             not_uninit_of_from_into_u64_init_64 hy32 hy64 hby⟩

theorem comp_eq32_ok_sound {x y : U64ScalarValueData} {opt : Option Bool}
    {x₁ x₂ y₁ y₂ : BitVec 64}
    (h : verifier.comp_eq32 x y = ok (.Ok opt))
    (hx : ConcretizesScalar64Rel2 x x₁ x₂)
    (hy : ConcretizesScalar64Rel2 y y₁ y₂) :
    x₁.setWidth 32 = x₂.setWidth 32 ∧
    y₁.setWidth 32 = y₂.setWidth 32 ∧
    (∀ b, opt = some b → (x₁.setWidth 32 == y₁.setWidth 32) = b) := by
  unfold verifier.comp_eq32 at h
  obtain ⟨x32, hx32_eq, hx32_rel⟩ := from_u64_scalar_sound_2safety x hx
  obtain ⟨x64, hx64_eq, hx64_rel⟩ := into_u64_scalar_sound_2safety x32 hx32_rel
  obtain ⟨y32, hy32_eq, hy32_rel⟩ := from_u64_scalar_sound_2safety y hy
  obtain ⟨y64, hy64_eq, hy64_rel⟩ := into_u64_scalar_sound_2safety y32 hy32_rel
  rw [bind_ok_eq hx32_eq, bind_ok_eq hx64_eq, bind_ok_eq hy32_eq, bind_ok_eq hy64_eq] at h
  obtain ⟨hx_zext, hy_zext, hopt⟩ := comp_eq64_ok_sound h hx64_rel hy64_rel
  have hx32 : x₁.setWidth 32 = x₂.setWidth 32 := by revert hx_zext; bv_decide
  have hy32 : y₁.setWidth 32 = y₂.setWidth 32 := by revert hy_zext; bv_decide
  refine ⟨hx32, hy32, fun b hb => ?_⟩
  have h64 := hopt b hb
  have hz : ((x₁.setWidth 32).zeroExtend 64 == (y₁.setWidth 32).zeroExtend 64) =
      (x₁.setWidth 32 == y₁.setWidth 32) := by bv_decide
  rw [← hz]
  exact h64

private theorem iscalar_i64_slt_eq (a b : Std.I64) :
    decide (a < b) = BitVec.slt a.bv b.bv := by
  unfold BitVec.slt
  change decide (a.val < b.val) = decide (a.bv.toInt < b.bv.toInt)
  rw [IScalar.bv_toInt_eq, IScalar.bv_toInt_eq]

private theorem iscalar_i64_sle_eq (a b : Std.I64) :
    decide (a <= b) = BitVec.sle a.bv b.bv := by
  unfold BitVec.sle
  change decide (a.val ≤ b.val) = decide (a.bv.toInt ≤ b.bv.toInt)
  rw [IScalar.bv_toInt_eq, IScalar.bv_toInt_eq]

private theorem iscalar_i64_sgt_eq (a b : Std.I64) :
    decide (a > b) = BitVec.slt b.bv a.bv :=
  iscalar_i64_slt_eq b a

private theorem iscalar_i64_sge_eq (a b : Std.I64) :
    decide (a >= b) = BitVec.sle b.bv a.bv :=
  iscalar_i64_sle_eq b a

private theorem iscalar_i32_slt_eq (a b : Std.I32) :
    decide (a < b) = BitVec.slt a.bv b.bv := by
  unfold BitVec.slt
  change decide (a.val < b.val) = decide (a.bv.toInt < b.bv.toInt)
  rw [IScalar.bv_toInt_eq, IScalar.bv_toInt_eq]

private theorem iscalar_i32_sle_eq (a b : Std.I32) :
    decide (a <= b) = BitVec.sle a.bv b.bv := by
  unfold BitVec.sle
  change decide (a.val ≤ b.val) = decide (a.bv.toInt ≤ b.bv.toInt)
  rw [IScalar.bv_toInt_eq, IScalar.bv_toInt_eq]

private theorem iscalar_i32_sgt_eq (a b : Std.I32) :
    decide (a > b) = BitVec.slt b.bv a.bv :=
  iscalar_i32_slt_eq b a

private theorem iscalar_i32_sge_eq (a b : Std.I32) :
    decide (a >= b) = BitVec.sle b.bv a.bv :=
  iscalar_i32_sle_eq b a

theorem BranchCondOp_eval_not_uninit {is64 : Bool} {op : JumpOp}
    {x y : U64ScalarValueData} {opt : Option Bool}
    (h : verifier.BranchCondOp.eval (condOpOfJump is64 op) x y = ok (.Ok opt)) :
    x ≠ uninitScalar64 ∧ y ≠ uninitScalar64 := by
  cases is64 <;> cases op <;>
    simp only [condOpOfJump, verifier.BranchCondOp.eval] at h
  -- Eq32 / Eq64
  · exact comp_eq32_not_uninit h
  -- Ne32
  · rcases bind_eq_ok h with ⟨r, hr, h⟩
    cases r with
    | Err _ => cases ok_inj h
    | Ok o => exact comp_eq32_not_uninit hr
  -- Gt32, Ge32, Lt32, Le32, Sgt32, Sge32, Slt32, Sle32, Set32
  case false.gt | false.ge | false.lt | false.le | false.sgt | false.sge |
       false.slt | false.sle | false.set =>
    rcases bind_eq_ok h with ⟨x32, hx32, h⟩
    rcases bind_eq_ok h with ⟨y32, hy32, h⟩
    rcases bind_eq_ok h with ⟨bx, hbx, h⟩
    cases bx with
    | false => cases ok_inj h
    | true =>
      simp only [↓reduceIte] at h
      rcases bind_eq_ok h with ⟨by_, hby, h⟩
      cases by_ with
      | false => cases ok_inj h
      | true =>
        exact ⟨not_uninit_of_from_u64_init_32 hx32 hbx,
               not_uninit_of_from_u64_init_32 hy32 hby⟩
  -- Eq64
  · exact comp_eq64_not_uninit h
  -- Ne64
  · rcases bind_eq_ok h with ⟨r, hr, h⟩
    cases r with
    | Err _ => cases ok_inj h
    | Ok o => exact comp_eq64_not_uninit hr
  -- Gt64, Ge64, Lt64, Le64, Sgt64, Sge64, Slt64, Sle64, Set64
  case true.gt | true.ge | true.lt | true.le | true.sgt | true.sge |
       true.slt | true.sle | true.set =>
    rcases bind_eq_ok h with ⟨bx, hbx, h⟩
    cases bx with
    | false => cases ok_inj h
    | true =>
      simp only [↓reduceIte] at h
      rcases bind_eq_ok h with ⟨by_, hby, h⟩
      cases by_ with
      | false => cases ok_inj h
      | true =>
        exact ⟨(not_uninit_of_fully_init_64 hbx).2,
               (not_uninit_of_fully_init_64 hby).2⟩

theorem BranchCondOp_eval_ok_sound {is64 : Bool} {op : JumpOp}
    {x y : U64ScalarValueData} {opt : Option Bool}
    {x₁ x₂ y₁ y₂ : BitVec 64}
    (h : verifier.BranchCondOp.eval (condOpOfJump is64 op) x y = ok (.Ok opt))
    (hx : ConcretizesScalar64Rel2 x x₁ x₂)
    (hy : ConcretizesScalar64Rel2 y y₁ y₂) :
    let c₁ := if is64 then evalJump64 op x₁ y₁ else evalJump32 op x₁ y₁
    let c₂ := if is64 then evalJump64 op x₂ y₂ else evalJump32 op x₂ y₂
    c₁ = c₂ ∧ (∀ b, opt = some b → c₁ = b) := by
  dsimp only
  cases is64 with
  | false =>
    simp only [Bool.false_eq_true, ↓reduceIte]
    cases op <;> simp only [condOpOfJump, verifier.BranchCondOp.eval] at h
    · obtain ⟨hx32, hy32, hopt⟩ := comp_eq32_ok_sound h hx hy
      exact ⟨by simp [evalJump32, hx32, hy32], fun b hb => by simpa [evalJump32] using hopt b hb⟩
    · rcases bind_eq_ok h with ⟨r, hr, h⟩
      cases r with
      | Err _ => cases ok_inj h
      | Ok o =>
        obtain ⟨hx32, hy32, hopt⟩ := comp_eq32_ok_sound hr hx hy
        refine ⟨by simp [evalJump32, hx32, hy32], fun b hb => ?_⟩
        cases o with
        | none =>
          cases core.result.Result.Ok.inj (ok_inj h)
          cases hb
        | some eq =>
          cases core.result.Result.Ok.inj (ok_inj h)
          cases Option.some.inj hb
          have heq := hopt eq rfl
          simp [evalJump32, bne, heq]
    case gt | ge | lt | le =>
      rcases bind_eq_ok h with ⟨x32, hx32, h⟩
      rcases bind_eq_ok h with ⟨y32, hy32, h⟩
      obtain ⟨_, hx32_eq, hx32_rel⟩ := from_u64_scalar_sound_2safety x hx
      obtain ⟨_, hy32_eq, hy32_rel⟩ := from_u64_scalar_sound_2safety y hy
      rw [hx32] at hx32_eq; cases ok_inj hx32_eq
      rw [hy32] at hy32_eq; cases ok_inj hy32_eq
      rcases bind_eq_ok h with ⟨bx, hbx, h⟩
      cases bx with
      | false => cases ok_inj h
      | true =>
        simp only [↓reduceIte] at h
        rcases bind_eq_ok h with ⟨by_, hby, h⟩
        cases by_ with
        | false => cases ok_inj h
        | true =>
          simp only [↓reduceIte] at h
          have hx_eq := fully_initialized_eq_of_rel2_32 (not_uninit_of_fully_init_32 hbx) hx32_rel
          have hy_eq := fully_initialized_eq_of_rel2_32 (not_uninit_of_fully_init_32 hby) hy32_rel
          refine ⟨by simp [evalJump32, hx_eq, hy_eq], fun b hb => ?_⟩
          have hxmin := hx32_rel.1.2.2.1
          have hxmax := hx32_rel.1.2.2.2
          have hymin := hy32_rel.1.2.2.1
          have hymax := hy32_rel.1.2.2.2
          split_ifs at h with hc1 hc2
          · cases core.result.Result.Ok.inj (ok_inj h)
            cases Option.some.inj hb
            simp only [evalJump32]
            first
              | (have hc1' := (u32_gt_iff_bv _ _).mp hc1; revert hc1' hxmin hxmax hymin hymax; bv_decide)
              | (have hc1' := (u32_ge_iff_bv _ _).mp hc1; revert hc1' hxmin hxmax hymin hymax; bv_decide)
          · cases core.result.Result.Ok.inj (ok_inj h)
            cases Option.some.inj hb
            simp only [evalJump32]
            first
              | (have hc2' := (u32_ge_iff_bv _ _).mp hc2; revert hc2' hxmin hxmax hymin hymax; bv_decide)
              | (have hc2' := (u32_gt_iff_bv _ _).mp hc2; revert hc2' hxmin hxmax hymin hymax; bv_decide)
          · cases core.result.Result.Ok.inj (ok_inj h)
            cases hb
    case sgt | sge | slt | sle =>
      rcases bind_eq_ok h with ⟨x32, hx32, h⟩
      rcases bind_eq_ok h with ⟨y32, hy32, h⟩
      obtain ⟨_, hx32_eq, hx32_rel⟩ := from_u64_scalar_sound_2safety x hx
      obtain ⟨_, hy32_eq, hy32_rel⟩ := from_u64_scalar_sound_2safety y hy
      rw [hx32] at hx32_eq; cases ok_inj hx32_eq
      rw [hy32] at hy32_eq; cases ok_inj hy32_eq
      rcases bind_eq_ok h with ⟨bx, hbx, h⟩
      cases bx with
      | false => cases ok_inj h
      | true =>
        simp only [↓reduceIte] at h
        rcases bind_eq_ok h with ⟨by_, hby, h⟩
        cases by_ with
        | false => cases ok_inj h
        | true =>
          simp only [↓reduceIte] at h
          have hx_eq := fully_initialized_eq_of_rel2_32 (not_uninit_of_fully_init_32 hbx) hx32_rel
          have hy_eq := fully_initialized_eq_of_rel2_32 (not_uninit_of_fully_init_32 hby) hy32_rel
          refine ⟨by simp [evalJump32, hx_eq, hy_eq], fun b hb => ?_⟩
          unfold U32ScalarValueData.is_known at h
          rw [bind_ok_eq rfl] at h
          by_cases hkx : x32.unknown_mask = 0#u32
          · simp only [hkx, decide_true, ↓reduceIte] at h
            rw [bind_ok_eq rfl] at h
            by_cases hky : y32.unknown_mask = 0#u32
            · simp only [hky, decide_true, ↓reduceIte, lift, bind_ok] at h
              cases core.result.Result.Ok.inj (ok_inj h)
              cases Option.some.inj hb
              have hxv := eq_value_of_known_32 hkx hx32_rel.1
              have hyv := eq_value_of_known_32 hky hy32_rel.1
              simp only [evalJump32, hxv, hyv]
              first
                | exact (iscalar_i32_sgt_eq (UScalar.hcast .I32 x32.value) (UScalar.hcast .I32 y32.value)).symm
                | exact (iscalar_i32_sge_eq (UScalar.hcast .I32 x32.value) (UScalar.hcast .I32 y32.value)).symm
                | exact (iscalar_i32_slt_eq (UScalar.hcast .I32 x32.value) (UScalar.hcast .I32 y32.value)).symm
                | exact (iscalar_i32_sle_eq (UScalar.hcast .I32 x32.value) (UScalar.hcast .I32 y32.value)).symm
            · simp only [hky, decide_false, Bool.false_eq_true, ↓reduceIte] at h
              cases core.result.Result.Ok.inj (ok_inj h); cases hb
          · simp only [hkx, decide_false, Bool.false_eq_true, ↓reduceIte] at h
            cases core.result.Result.Ok.inj (ok_inj h); cases hb
    case set =>
      rcases bind_eq_ok h with ⟨x32, hx32, h⟩
      rcases bind_eq_ok h with ⟨y32, hy32, h⟩
      obtain ⟨_, hx32_eq, hx32_rel⟩ := from_u64_scalar_sound_2safety x hx
      obtain ⟨_, hy32_eq, hy32_rel⟩ := from_u64_scalar_sound_2safety y hy
      rw [hx32] at hx32_eq; cases ok_inj hx32_eq
      rw [hy32] at hy32_eq; cases ok_inj hy32_eq
      rcases bind_eq_ok h with ⟨bx, hbx, h⟩
      cases bx with
      | false => cases ok_inj h
      | true =>
        simp only [↓reduceIte] at h
        rcases bind_eq_ok h with ⟨by_, hby, h⟩
        cases by_ with
        | false => cases ok_inj h
        | true =>
          simp only [↓reduceIte] at h
          have hx_eq := fully_initialized_eq_of_rel2_32 (not_uninit_of_fully_init_32 hbx) hx32_rel
          have hy_eq := fully_initialized_eq_of_rel2_32 (not_uninit_of_fully_init_32 hby) hy32_rel
          refine ⟨by simp [evalJump32, hx_eq, hy_eq], fun b hb => ?_⟩
          by_cases h1 : (x32.urange.min != x32.urange.max) = true
          · simp only [h1, ↓reduceIte] at h
            cases core.result.Result.Ok.inj (ok_inj h); cases hb
          · simp only [h1, Bool.false_eq_true, ↓reduceIte] at h
            by_cases h2 : (y32.urange.min != y32.urange.max) = true
            · simp only [h2, ↓reduceIte] at h
              cases core.result.Result.Ok.inj (ok_inj h); cases hb
            · simp only [h2, Bool.false_eq_true, ↓reduceIte, lift, bind_ok] at h
              cases core.result.Result.Ok.inj (ok_inj h)
              cases Option.some.inj hb
              have hx_mm : x32.urange.min.bv = x32.urange.max.bv :=
                (u32_eq_iff_bv _ _).mp (Classical.byContradiction fun hc => h1 (bne_iff_ne.mpr hc))
              have hy_mm : y32.urange.min.bv = y32.urange.max.bv :=
                (u32_eq_iff_bv _ _).mp (Classical.byContradiction fun hc => h2 (bne_iff_ne.mpr hc))
              have hxmin := hx32_rel.1.2.2.1
              have hxmax := hx32_rel.1.2.2.2
              have hymin := hy32_rel.1.2.2.1
              have hymax := hy32_rel.1.2.2.2
              have hxv : x₁.setWidth 32 = x32.urange.min.bv := by revert hx_mm hxmin hxmax; bv_decide
              have hyv : y₁.setWidth 32 = y32.urange.min.bv := by revert hy_mm hymin hymax; bv_decide
              simp only [evalJump32, hxv, hyv]
              cases hbeq : ((x32.urange.min.bv &&& y32.urange.min.bv) == 0#32)
              · have hne : (x32.urange.min &&& y32.urange.min) ≠ 0#u32 := by
                  rw [ne_eq, u32_eq_iff_bv]; exact beq_eq_false_iff_ne.mp hbeq
                simp [hne, bne, hbeq]
              · have heq : (x32.urange.min &&& y32.urange.min) = 0#u32 := by
                  rw [u32_eq_iff_bv]; exact beq_iff_eq.mp hbeq
                simp [heq, bne, hbeq]
  | true =>
    simp only [↓reduceIte]
    cases op <;> simp only [condOpOfJump, verifier.BranchCondOp.eval] at h
    · obtain ⟨hx64, hy64, hopt⟩ := comp_eq64_ok_sound h hx hy
      exact ⟨by simp [evalJump64, hx64, hy64], fun b hb => by simpa [evalJump64] using hopt b hb⟩
    · rcases bind_eq_ok h with ⟨r, hr, h⟩
      cases r with
      | Err _ => cases ok_inj h
      | Ok o =>
        obtain ⟨hx64, hy64, hopt⟩ := comp_eq64_ok_sound hr hx hy
        refine ⟨by simp [evalJump64, hx64, hy64], fun b hb => ?_⟩
        cases o with
        | none =>
          cases core.result.Result.Ok.inj (ok_inj h)
          cases hb
        | some eq =>
          cases core.result.Result.Ok.inj (ok_inj h)
          cases Option.some.inj hb
          have heq := hopt eq rfl
          simp [evalJump64, bne, heq]
    case gt | ge | lt | le =>
      rcases bind_eq_ok h with ⟨bx, hbx, h⟩
      cases bx with
      | false => cases ok_inj h
      | true =>
        simp only [↓reduceIte] at h
        rcases bind_eq_ok h with ⟨by_, hby, h⟩
        cases by_ with
        | false => cases ok_inj h
        | true =>
          simp only [↓reduceIte] at h
          have hx_eq := fully_initialized_eq_of_rel2_64 (not_uninit_of_fully_init_64 hbx).1 hx
          have hy_eq := fully_initialized_eq_of_rel2_64 (not_uninit_of_fully_init_64 hby).1 hy
          refine ⟨by simp [evalJump64, hx_eq, hy_eq], fun b hb => ?_⟩
          have hxmin := hx.1.2.2.1
          have hxmax := hx.1.2.2.2
          have hymin := hy.1.2.2.1
          have hymax := hy.1.2.2.2
          split_ifs at h with hc1 hc2
          · cases core.result.Result.Ok.inj (ok_inj h)
            cases Option.some.inj hb
            simp only [evalJump64]
            first
              | (have hc1' := (u64_gt_iff_bv _ _).mp hc1; revert hc1' hxmin hxmax hymin hymax; bv_decide)
              | (have hc1' := (u64_ge_iff_bv _ _).mp hc1; revert hc1' hxmin hxmax hymin hymax; bv_decide)
          · cases core.result.Result.Ok.inj (ok_inj h)
            cases Option.some.inj hb
            simp only [evalJump64]
            first
              | (have hc2' := (u64_ge_iff_bv _ _).mp hc2; revert hc2' hxmin hxmax hymin hymax; bv_decide)
              | (have hc2' := (u64_gt_iff_bv _ _).mp hc2; revert hc2' hxmin hxmax hymin hymax; bv_decide)
          · cases core.result.Result.Ok.inj (ok_inj h)
            cases hb
    case sgt | sge | slt | sle =>
      rcases bind_eq_ok h with ⟨bx, hbx, h⟩
      cases bx with
      | false => cases ok_inj h
      | true =>
        simp only [↓reduceIte] at h
        rcases bind_eq_ok h with ⟨by_, hby, h⟩
        cases by_ with
        | false => cases ok_inj h
        | true =>
          simp only [↓reduceIte] at h
          have hx_eq := fully_initialized_eq_of_rel2_64 (not_uninit_of_fully_init_64 hbx).1 hx
          have hy_eq := fully_initialized_eq_of_rel2_64 (not_uninit_of_fully_init_64 hby).1 hy
          refine ⟨by simp [evalJump64, hx_eq, hy_eq], fun b hb => ?_⟩
          unfold U64ScalarValueData.is_known at h
          rw [bind_ok_eq rfl] at h
          by_cases hkx : x.unknown_mask = 0#u64
          · simp only [hkx, decide_true, ↓reduceIte] at h
            rw [bind_ok_eq rfl] at h
            by_cases hky : y.unknown_mask = 0#u64
            · simp only [hky, decide_true, ↓reduceIte, lift, bind_ok] at h
              cases core.result.Result.Ok.inj (ok_inj h)
              cases Option.some.inj hb
              have hxv := eq_value_of_known_64 hkx hx.1
              have hyv := eq_value_of_known_64 hky hy.1
              simp only [evalJump64, hxv, hyv]
              first
                | exact (iscalar_i64_sgt_eq (UScalar.hcast .I64 x.value) (UScalar.hcast .I64 y.value)).symm
                | exact (iscalar_i64_sge_eq (UScalar.hcast .I64 x.value) (UScalar.hcast .I64 y.value)).symm
                | exact (iscalar_i64_slt_eq (UScalar.hcast .I64 x.value) (UScalar.hcast .I64 y.value)).symm
                | exact (iscalar_i64_sle_eq (UScalar.hcast .I64 x.value) (UScalar.hcast .I64 y.value)).symm
            · simp only [hky, decide_false, Bool.false_eq_true, ↓reduceIte] at h
              cases core.result.Result.Ok.inj (ok_inj h); cases hb
          · simp only [hkx, decide_false, Bool.false_eq_true, ↓reduceIte] at h
            cases core.result.Result.Ok.inj (ok_inj h); cases hb
    case set =>
      rcases bind_eq_ok h with ⟨bx, hbx, h⟩
      cases bx with
      | false => cases ok_inj h
      | true =>
        simp only [↓reduceIte] at h
        rcases bind_eq_ok h with ⟨by_, hby, h⟩
        cases by_ with
        | false => cases ok_inj h
        | true =>
          simp only [↓reduceIte] at h
          have hx_eq := fully_initialized_eq_of_rel2_64 (not_uninit_of_fully_init_64 hbx).1 hx
          have hy_eq := fully_initialized_eq_of_rel2_64 (not_uninit_of_fully_init_64 hby).1 hy
          refine ⟨by simp [evalJump64, hx_eq, hy_eq], fun b hb => ?_⟩
          by_cases h1 : (x.urange.min != x.urange.max) = true
          · simp only [h1, ↓reduceIte] at h
            cases core.result.Result.Ok.inj (ok_inj h); cases hb
          · simp only [h1, Bool.false_eq_true, ↓reduceIte] at h
            by_cases h2 : (y.urange.min != y.urange.max) = true
            · simp only [h2, ↓reduceIte] at h
              cases core.result.Result.Ok.inj (ok_inj h); cases hb
            · simp only [h2, Bool.false_eq_true, ↓reduceIte, lift, bind_ok] at h
              cases core.result.Result.Ok.inj (ok_inj h)
              cases Option.some.inj hb
              have hx_mm : x.urange.min.bv = x.urange.max.bv :=
                (u64_eq_iff_bv _ _).mp (Classical.byContradiction fun hc => h1 (bne_iff_ne.mpr hc))
              have hy_mm : y.urange.min.bv = y.urange.max.bv :=
                (u64_eq_iff_bv _ _).mp (Classical.byContradiction fun hc => h2 (bne_iff_ne.mpr hc))
              have hxmin := hx.1.2.2.1
              have hxmax := hx.1.2.2.2
              have hymin := hy.1.2.2.1
              have hymax := hy.1.2.2.2
              have hxv : x₁ = x.urange.min.bv := by revert hx_mm hxmin hxmax; bv_decide
              have hyv : y₁ = y.urange.min.bv := by revert hy_mm hymin hymax; bv_decide
              simp only [evalJump64, hxv, hyv]
              cases hbeq : ((x.urange.min.bv &&& y.urange.min.bv) == 0#64)
              · have hne : (x.urange.min &&& y.urange.min) ≠ 0#u64 := by
                  rw [ne_eq, u64_eq_iff_bv]; exact beq_eq_false_iff_ne.mp hbeq
                simp [hne, bne, hbeq]
              · have heq : (x.urange.min &&& y.urange.min) = 0#u64 := by
                  rw [u64_eq_iff_bv]; exact beq_iff_eq.mp hbeq
                simp [heq, bne, hbeq]

/-! ### Pointer Bitvector Layout-Invariance & `compute_branch` -/

private theorem canonical_ptr_bounds {base off : BitVec 64}
    (hbase_lo : 0#64 < base) (hbase_hi : base ≤ CANONICAL_USER_MAX)
    (hoff : off ≤ 0xFFFF_FFFF#64) :
    0#64 < base + off ∧ base + off ≤ 0x0100_0000_FFFF_FFFF#64 := by
  unfold CANONICAL_USER_MAX at hbase_hi
  constructor <;> bv_decide

private theorem evalJump64_zero_vs_pos_eq {op : JumpOp} (hop : op ≠ .set)
    {p₁ p₂ : BitVec 64}
    (h1_lo : 0#64 < p₁) (h1_hi : p₁ ≤ 0x0100_0000_FFFF_FFFF#64)
    (h2_lo : 0#64 < p₂) (h2_hi : p₂ ≤ 0x0100_0000_FFFF_FFFF#64) :
    evalJump64 op 0#64 p₁ = evalJump64 op 0#64 p₂ ∧
    evalJump64 op p₁ 0#64 = evalJump64 op p₂ 0#64 := by
  cases op with
  | set => exact False.elim (hop rfl)
  | _ =>
    simp only [evalJump64]
    constructor <;> bv_decide

private theorem evalJump64_same_base_eq {op : JumpOp} (hop : op ≠ .set)
    {base ox oy : BitVec 64}
    (hbase_lo : 0#64 < base) (hbase_hi : base ≤ CANONICAL_USER_MAX)
    (hox : ox ≤ 0xFFFF_FFFF#64) (hoy : oy ≤ 0xFFFF_FFFF#64) :
    evalJump64 op (base + ox) (base + oy) = evalJump64 op ox oy := by
  unfold CANONICAL_USER_MAX at hbase_hi
  cases op with
  | set => exact False.elim (hop rfl)
  | _ =>
    simp only [evalJump64]
    bv_decide

private theorem eval_same_region_ptr_ok_sound {op : JumpOp}
    {sx sy : U64ScalarValueData} {in_bounds : Bool} {opt : Option Bool}
    {ox oy : BitVec 64}
    (h : verifier.ComputationContext.eval_same_region_ptr
          (jumpTypeOfOp op) (condOpOfJump true op) sx sy in_bounds = ok (.Ok opt))
    (hsx : ConcretizesScalar64Rel2 sx ox ox)
    (hsy : ConcretizesScalar64Rel2 sy oy oy) :
    op ≠ .set ∧ (∀ b, opt = some b → evalJump64 op ox oy = b) := by
  unfold verifier.ComputationContext.eval_same_region_ptr at h
  rcases bind_eq_ok h with ⟨bx, _, h⟩
  cases bx with
  | false => cases ok_inj h
  | true =>
    simp only [↓reduceIte] at h
    rcases bind_eq_ok h with ⟨by_, _, h⟩
    cases by_ with
    | false => cases ok_inj h
    | true =>
      simp only [↓reduceIte] at h
      cases op <;> simp only [jumpTypeOfOp] at h
      · exact ⟨by decide, (BranchCondOp_eval_ok_sound h hsx hsy).2⟩
      · exact ⟨by decide, (BranchCondOp_eval_ok_sound h hsx hsy).2⟩
      case gt | ge | lt | le =>
        refine ⟨by decide, fun b hb => ?_⟩
        cases in_bounds with
        | false =>
          simp only [Bool.false_eq_true, ↓reduceIte] at h
          cases core.result.Result.Ok.inj (ok_inj h); cases hb
        | true =>
          simp only [↓reduceIte] at h
          exact (BranchCondOp_eval_ok_sound h hsx hsy).2 b hb
      case sgt | sge | slt | sle =>
        refine ⟨by decide, fun b hb => ?_⟩
        cases core.result.Result.Ok.inj (ok_inj h); cases hb
      case set =>
        cases ok_inj h

private theorem non_zero_scalar_eval_lhs_sound {op : JumpOp}
    {data1 : U64ScalarValueData} {inner2 : verifier.Type}
    {opt : Option Bool} {b1 p₁ p₂ : BitVec 64}
    (hop : op ≠ .set)
    (hz1 : U64ScalarValueData.is_zero data1 = ok true)
    (hd1 : ConcretizesScalar64Rel2 data1 b1 b1)
    (hp1_lo : 0#64 < p₁) (hp1_hi : p₁ ≤ 0x0100_0000_FFFF_FFFF#64)
    (hp2_lo : 0#64 < p₂) (hp2_hi : p₂ ≤ 0x0100_0000_FFFF_FFFF#64)
    (heval :
      (do
        let b2 ← verifier.Type.is_non_zero inner2
        if b2 then
          let ur ← U64Range.new 1#u64 core.num.U64.MAX
          let usvd ← U64ScalarValueData.UNKNOWN_WRITTEN
          let non_zero ← U64ScalarValueData.update_range usvd ur
          let usvd1 ← U64ScalarValueData.from_known 0#u64
          verifier.BranchCondOp.eval (condOpOfJump true op) usvd1 non_zero
        else ok (core.result.Result.Err ())) = ok (.Ok opt)) :
    evalJump64 op b1 p₁ = evalJump64 op b1 p₂ ∧
    (∀ b, opt = some b → evalJump64 op b1 p₁ = b) := by
  have hb1_zero : b1 = 0#64 := is_zero_64_sound hz1 hd1.1
  subst hb1_zero
  rcases bind_eq_ok heval with ⟨b2, _, heval⟩
  cases b2 with
  | false => cases ok_inj heval
  | true =>
    simp only [↓reduceIte] at heval
    rcases bind_eq_ok heval with ⟨ur, hur, heval⟩
    rcases bind_eq_ok heval with ⟨usvd, husvd, heval⟩
    rcases bind_eq_ok heval with ⟨non_zero, hnz, heval⟩
    rcases bind_eq_ok heval with ⟨usvd1, husvd1, heval⟩
    cases ok_inj hur
    rw [UNKNOWN_WRITTEN_64_eq] at husvd; cases ok_inj husvd
    unfold U64ScalarValueData.update_range unknownWrittenScalar64 at hnz
    dsimp only at hnz
    have hp1_range : ConcretizesRange64 { min := 1#u64, max := core.num.U64.MAX } p₁ := by
      change 1#64 ≤ p₁ ∧ p₁ ≤ 0xFFFF_FFFF_FFFF_FFFF#64
      constructor <;> [revert hp1_lo; skip] <;> bv_decide
    obtain ⟨nz, hnz_eq, hnz_rel⟩ :=
      new64_sound_2safety 0#u64 core.num.U64.MAX 0#u64
        { min := 1#u64, max := core.num.U64.MAX } p₁ p₁
        hp1_range (by simp) hp1_range (by simp) rfl
    rw [hnz] at hnz_eq; cases ok_inj hnz_eq
    rw [from_known_64_eq 0#u64] at husvd1; cases ok_inj husvd1
    have h0_rel := concretizesScalar64Rel2_known 0#u64
    obtain ⟨_, hopt⟩ := BranchCondOp_eval_ok_sound heval h0_rel hnz_rel
    exact ⟨(evalJump64_zero_vs_pos_eq hop hp1_lo hp1_hi hp2_lo hp2_hi).1, hopt⟩

private theorem non_zero_scalar_eval_rhs_sound {op : JumpOp}
    {inner1 : verifier.Type} {data2 : U64ScalarValueData}
    {opt : Option Bool} {p₁ p₂ b2 : BitVec 64}
    (hop : op ≠ .set)
    (hz2 : U64ScalarValueData.is_zero data2 = ok true)
    (hd2 : ConcretizesScalar64Rel2 data2 b2 b2)
    (hp1_lo : 0#64 < p₁) (hp1_hi : p₁ ≤ 0x0100_0000_FFFF_FFFF#64)
    (hp2_lo : 0#64 < p₂) (hp2_hi : p₂ ≤ 0x0100_0000_FFFF_FFFF#64)
    (heval :
      (do
        let b2 ← verifier.Type.is_non_zero inner1
        if b2 then
          let ur ← U64Range.new 1#u64 core.num.U64.MAX
          let usvd ← U64ScalarValueData.UNKNOWN_WRITTEN
          let non_zero ← U64ScalarValueData.update_range usvd ur
          let usvd1 ← U64ScalarValueData.from_known 0#u64
          verifier.BranchCondOp.eval (condOpOfJump true op) non_zero usvd1
        else ok (core.result.Result.Err ())) = ok (.Ok opt)) :
    evalJump64 op p₁ b2 = evalJump64 op p₂ b2 ∧
    (∀ b, opt = some b → evalJump64 op p₁ b2 = b) := by
  have hb2_zero : b2 = 0#64 := is_zero_64_sound hz2 hd2.1
  subst hb2_zero
  rcases bind_eq_ok heval with ⟨b_nz, _, heval⟩
  cases b_nz with
  | false => cases ok_inj heval
  | true =>
    simp only [↓reduceIte] at heval
    rcases bind_eq_ok heval with ⟨ur, hur, heval⟩
    rcases bind_eq_ok heval with ⟨usvd, husvd, heval⟩
    rcases bind_eq_ok heval with ⟨non_zero, hnz, heval⟩
    rcases bind_eq_ok heval with ⟨usvd1, husvd1, heval⟩
    cases ok_inj hur
    rw [UNKNOWN_WRITTEN_64_eq] at husvd; cases ok_inj husvd
    unfold U64ScalarValueData.update_range unknownWrittenScalar64 at hnz
    dsimp only at hnz
    have hp1_range : ConcretizesRange64 { min := 1#u64, max := core.num.U64.MAX } p₁ := by
      change 1#64 ≤ p₁ ∧ p₁ ≤ 0xFFFF_FFFF_FFFF_FFFF#64
      constructor <;> [revert hp1_lo; skip] <;> bv_decide
    obtain ⟨nz, hnz_eq, hnz_rel⟩ :=
      new64_sound_2safety 0#u64 core.num.U64.MAX 0#u64
        { min := 1#u64, max := core.num.U64.MAX } p₁ p₁
        hp1_range (by simp) hp1_range (by simp) rfl
    rw [hnz] at hnz_eq; cases ok_inj hnz_eq
    rw [from_known_64_eq 0#u64] at husvd1; cases ok_inj husvd1
    have h0_rel := concretizesScalar64Rel2_known 0#u64
    obtain ⟨_, hopt⟩ := BranchCondOp_eval_ok_sound heval hnz_rel h0_rel
    exact ⟨(evalJump64_zero_vs_pos_eq hop hp1_lo hp1_hi hp2_lo hp2_hi).2, hopt⟩

private theorem TypeRel2_non_map_ptr_bounds
    {env : ExecEnv} {vCounter : Nat} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {arrayLen : MemoryId → BitVec 64}
    {l : HostLayout} (hl : ValidHostLayout l)
    {t : verifier.Type} {v₁ v₂ : RuntimeVal}
    (ht : TypeValid env vCounter t)
    (hrel : TypeRel2 env ρ resActive isNullId arrayLen t v₁ v₂)
    (hshape : match t with
      | .PtrToStack _ | .PtrToMemory _ _ _ _ | .PtrToStruct _ _ _
      | .PtrToArray _ _ | .PtrToEndArray _ | .Releasable _ _ => True
      | .NullOr nullId _ => isNullId (renameMemId ρ nullId) = false
      | _ => False) :
    v₁ = v₂ ∧
    0#64 < v₁.toBits l arrayLen ∧
    v₁.toBits l arrayLen ≤ 0x0100_0000_FFFF_FFFF#64 := by
  induction t with
  | PtrToStack off =>
    rcases hrel with ⟨hp, o, hconc, rfl, rfl⟩
    have ho : o ≤ 0xFFFF_FFFF#64 := BitVec.le_trans hconc.1.2.2.2 hp.2.2
    exact ⟨rfl, canonical_ptr_bounds hl.1.1 hl.1.2 ho⟩
  | PtrToMemory id off sz orig =>
    rcases hrel with ⟨hp, o, hconc, rfl, rfl⟩
    have ho : o ≤ 0xFFFF_FFFF#64 := BitVec.le_trans hconc.1.2.2.2 hp.2.2
    have hb := hl.2.1 (renameMemId ρ id)
    exact ⟨rfl, canonical_ptr_bounds hb.1 hb.2 ho⟩
  | PtrToStruct id off sd =>
    rcases hrel with ⟨hp, _, o, hconc, rfl, rfl⟩
    have ho : o ≤ 0xFFFF_FFFF#64 := BitVec.le_trans hconc.1.2.2.2 hp.2.2
    have hb := hl.2.2.1 (renameMemId ρ id)
    exact ⟨rfl, canonical_ptr_bounds hb.1 hb.2 ho⟩
  | PtrToArray id off =>
    rcases hrel with ⟨hp, _, o, hconc, rfl, rfl⟩
    have ho : o ≤ 0xFFFF_FFFF#64 :=
      BitVec.le_trans hconc.1.2.2.2 (BitVec.le_trans hp.2.2 (by decide))
    have hb := hl.2.2.2.1 (renameMemId ρ id)
    exact ⟨rfl, canonical_ptr_bounds hb.1 hb.2 ho⟩
  | PtrToEndArray id =>
    rcases hrel with ⟨hlen, rfl, rfl⟩
    have hb := hl.2.2.2.1 (renameMemId ρ id)
    exact ⟨rfl, canonical_ptr_bounds hb.1 hb.2 hlen⟩
  | Releasable relId inner =>
    cases inner with
    | PtrToMemory id off sz orig =>
      rcases hrel with ⟨_, hp, o, hconc, rfl, rfl⟩
      have ho : o ≤ 0xFFFF_FFFF#64 := BitVec.le_trans hconc.1.2.2.2 hp.2.2
      have hb := hl.2.1 (renameMemId ρ id)
      exact ⟨rfl, canonical_ptr_bounds hb.1 hb.2 ho⟩
    | PtrToStruct id off sd =>
      rcases hrel with ⟨_, hp, _, o, hconc, rfl, rfl⟩
      have ho : o ≤ 0xFFFF_FFFF#64 := BitVec.le_trans hconc.1.2.2.2 hp.2.2
      have hb := hl.2.2.1 (renameMemId ρ id)
      exact ⟨rfl, canonical_ptr_bounds hb.1 hb.2 ho⟩
    | _ => exact False.elim hrel
  | NullOr nullId inner ih =>
    rcases ht with ⟨_, hinnerShape, hinnerValid⟩
    simp only [TypeRel2, hshape, Bool.false_eq_true, ↓reduceIte] at hrel
    cases inner <;> try exact False.elim hinnerShape
    all_goals exact ih hinnerValid hrel trivial
  | _ => exact False.elim hshape

private theorem Type_is_non_zero_false_of_map {m s} :
    verifier.Type.is_non_zero (.ConstPtrToMap m s) = ok false := by
  rw [verifier.Type.is_non_zero.eq_def]

theorem unwrap_ne_uninit_iff {env : ExecEnv} {vCounter : Nat} {t : verifier.Type}
    (ht : TypeValid env vCounter t) :
    t ≠ .ScalarValue uninitScalar64 ↔ pureUnwrapReleasable t ≠ .ScalarValue uninitScalar64 := by
  cases t with
  | ScalarValue d => simp [pureUnwrapReleasable]
  | Releasable relId inner =>
    cases inner <;> try exact False.elim ht
    all_goals simp [pureUnwrapReleasable]
  | _ => simp [pureUnwrapReleasable]

private theorem jt_unknown_guard_eq (op : JumpOp) :
    (match jumpTypeOfOp op with
     | verifier.JumpType.Eq => ok false
     | verifier.JumpType.Ge => ok false
     | verifier.JumpType.Gt => ok false
     | verifier.JumpType.Le => ok false
     | verifier.JumpType.LooseComparaison => ok false
     | verifier.JumpType.Lt => ok false
     | verifier.JumpType.Ne => ok false
     | verifier.JumpType.StrictComparaison => ok false
     | verifier.JumpType.Unknown => ok true) =
    (ok (decide (op = .set)) : Result Bool) := by
  cases op <;> rfl

private theorem stripResource_toBits (v : RuntimeVal) (l : HostLayout)
    (arrayLen : MemoryId → BitVec 64) :
    (v.stripResource).toBits l arrayLen = v.toBits l arrayLen := by
  cases v <;> rfl

private def unwrapRuntimeVal (op : verifier.Type) (v : RuntimeVal) : RuntimeVal :=
  match op with
  | .Releasable _ _ => v.stripResource
  | _ => v

private theorem unwrap_toBits_eq (op : verifier.Type) (v : RuntimeVal)
    (l : HostLayout) (arrayLen : MemoryId → BitVec 64) :
    (unwrapRuntimeVal op v).toBits l arrayLen = v.toBits l arrayLen := by
  cases op <;> simp [unwrapRuntimeVal, stripResource_toBits]

private theorem Type_unwrap_releasable_rel2_clean
    {env : ExecEnv} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {arrayLen : MemoryId → BitVec 64}
    {op : verifier.Type} {v₁ v₂ : RuntimeVal}
    (h : TypeRel2 env ρ resActive isNullId arrayLen op v₁ v₂) :
    TypeRel2 env ρ resActive isNullId arrayLen (pureUnwrapReleasable op)
      (unwrapRuntimeVal op v₁) (unwrapRuntimeVal op v₂) := by
  have hraw := Type_unwrap_releasable_rel2 h
  cases op <;> simpa [unwrapRuntimeVal] using hraw

theorem compute_branch_ok_sound
    {env : ExecEnv} {vCounter : Nat} {self : verifier.ComputationContext}
    {deps : verifier.DataDependencies} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {st₁ st₂ : MachineState}
    {l₁ l₂ : HostLayout} {is64 : Bool} {op : JumpOp}
    {op1 op2 : verifier.Type} {opt : Option Bool}
    {vDst₁ vDst₂ vSrc₁ vSrc₂ : RuntimeVal}
    (hctx : CompCtxValid env vCounter self)
    (hrel_ctx : StateRel2Deps env self deps ρ resActive isNullId st₁ st₂)
    (hl₁ : ValidHostLayout l₁) (hl₂ : ValidHostLayout l₂)
    (hop1_valid : TypeValid env vCounter op1)
    (hop2_valid : TypeValid env vCounter op2)
    (hrel1 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen op1 vDst₁ vDst₂)
    (hrel2 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen op2 vSrc₁ vSrc₂)
    (hbranch : verifier.ComputationContext.compute_branch self
      (jumpTypeOfOp op) (jumpWidthOfBool is64) op1 op2 (condOpOfJump is64 op) = ok (.Ok opt)) :
    checkAlive st₁.world.liveResources vDst₁ = none ∧
    checkAlive st₂.world.liveResources vDst₂ = none ∧
    checkAlive st₁.world.liveResources vSrc₁ = none ∧
    checkAlive st₂.world.liveResources vSrc₂ = none ∧
    (let b1_1 := vDst₁.toBits l₁ st₁.world.arrayLen
     let b2_1 := vSrc₁.toBits l₁ st₁.world.arrayLen
     let b1_2 := vDst₂.toBits l₂ st₂.world.arrayLen
     let b2_2 := vSrc₂.toBits l₂ st₂.world.arrayLen
     let c₁ := if is64 then evalJump64 op b1_1 b2_1 else evalJump32 op b1_1 b2_1
     let c₂ := if is64 then evalJump64 op b1_2 b2_2 else evalJump32 op b1_2 b2_2
     c₁ = c₂ ∧ (∀ b, opt = some b → c₁ = b)) := by
  unfold verifier.ComputationContext.compute_branch at hbranch
  rcases bind_eq_ok hbranch with ⟨r1, hinner1, hbranch⟩
  cases r1 with
  | Err _ => dsimp only at hbranch; cases ok_inj hbranch
  | Ok t1 =>
    dsimp only at hbranch
    rw [bind_ok_eq (Type_clone_spec t1)] at hbranch
    rcases bind_eq_ok hbranch with ⟨r2, hinner2, hbranch⟩
    cases r2 with
    | Err _ => dsimp only at hbranch; cases ok_inj hbranch
    | Ok t2 =>
      dsimp only at hbranch
      rw [bind_ok_eq (Type_clone_spec t2)] at hbranch
      have ht1_eq : t1 = pureUnwrapReleasable op1 := by
        unfold verifier.Type.inner at hinner1
        obtain ⟨rc, hrc, _⟩ := Type_check_not_released_spec hctx.2.2.2 op1
        rw [bind_ok_eq hrc] at hinner1
        cases rc with
        | Err _ => cases ok_inj hinner1
        | Ok _ =>
          dsimp only at hinner1
          rw [bind_ok_eq (Type_unwrap_releasable_spec op1)] at hinner1
          exact (core.result.Result.Ok.inj (ok_inj hinner1)).symm
      have ht2_eq : t2 = pureUnwrapReleasable op2 := by
        unfold verifier.Type.inner at hinner2
        obtain ⟨rc, hrc, _⟩ := Type_check_not_released_spec hctx.2.2.2 op2
        rw [bind_ok_eq hrc] at hinner2
        cases rc with
        | Err _ => cases ok_inj hinner2
        | Ok _ =>
          dsimp only at hinner2
          rw [bind_ok_eq (Type_unwrap_releasable_spec op2)] at hinner2
          exact (core.result.Result.Ok.inj (ok_inj hinner2)).symm
      have ht1_valid : TypeValid env vCounter t1 := ht1_eq ▸ Type_unwrap_releasable_valid hop1_valid
      have ht2_valid : TypeValid env vCounter t2 := ht2_eq ▸ Type_unwrap_releasable_valid hop2_valid
      have hrel_t1 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen t1
          (unwrapRuntimeVal op1 vDst₁) (unwrapRuntimeVal op1 vDst₂) :=
        ht1_eq ▸ Type_unwrap_releasable_rel2_clean hrel1
      have hrel_t2 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen t2
          (unwrapRuntimeVal op2 vSrc₁) (unwrapRuntimeVal op2 vSrc₂) :=
        ht2_eq ▸ Type_unwrap_releasable_rel2_clean hrel2
      generalize hwDst₁ : unwrapRuntimeVal op1 vDst₁ = wDst₁ at hrel_t1
      generalize hwDst₂ : unwrapRuntimeVal op1 vDst₂ = wDst₂ at hrel_t1
      generalize hwSrc₁ : unwrapRuntimeVal op2 vSrc₁ = wSrc₁ at hrel_t2
      generalize hwSrc₂ : unwrapRuntimeVal op2 vSrc₂ = wSrc₂ at hrel_t2
      have harr_eq : st₂.world.arrayLen = st₁.world.arrayLen := by rw [hrel_ctx.2.2.1]
      -- First prove `t1 ≠ .ScalarValue uninitScalar64 ∧ t2 ≠ .ScalarValue uninitScalar64`
      -- AND the branch condition result together by case-splitting on `t1` and `t2`.
      suffices hmain :
          t1 ≠ .ScalarValue uninitScalar64 ∧
          t2 ≠ .ScalarValue uninitScalar64 ∧
          (let b1_1 := wDst₁.toBits l₁ st₁.world.arrayLen
           let b2_1 := wSrc₁.toBits l₁ st₁.world.arrayLen
           let b1_2 := wDst₂.toBits l₂ st₂.world.arrayLen
           let b2_2 := wSrc₂.toBits l₂ st₂.world.arrayLen
           let c₁ := if is64 then evalJump64 op b1_1 b2_1 else evalJump32 op b1_1 b2_1
           let c₂ := if is64 then evalJump64 op b1_2 b2_2 else evalJump32 op b1_2 b2_2
           c₁ = c₂ ∧ (∀ b, opt = some b → c₁ = b)) by
        have hop1_ne : op1 ≠ .ScalarValue uninitScalar64 :=
          (unwrap_ne_uninit_iff hop1_valid).mpr (ht1_eq ▸ hmain.1)
        have hop2_ne : op2 ≠ .ScalarValue uninitScalar64 :=
          (unwrap_ne_uninit_iff hop2_valid).mpr (ht2_eq ▸ hmain.2.1)
        obtain ⟨_, _, hca1_1, hca1_2⟩ :=
          Type_inner_ok_valid_and_checkAlive_none hctx hrel_ctx hop1_valid hop1_ne hinner1 hrel1
        obtain ⟨_, _, hca2_1, hca2_2⟩ :=
          Type_inner_ok_valid_and_checkAlive_none hctx hrel_ctx hop2_valid hop2_ne hinner2 hrel2
        have hm := hmain.2.2
        dsimp only at hm
        rw [← hwDst₁, ← hwDst₂, ← hwSrc₁, ← hwSrc₂,
            unwrap_toBits_eq, unwrap_toBits_eq, unwrap_toBits_eq, unwrap_toBits_eq] at hm
        exact ⟨hca1_1, hca1_2, hca2_1, hca2_2, hm⟩
      clear hwDst₁ hwDst₂ hwSrc₁ hwSrc₂
      cases t1 with
      | ScalarValue data1 =>
        cases t2 with
        | ScalarValue data2 =>
          dsimp only at hbranch
          rcases bind_eq_ok hbranch with ⟨r3, heval, hbranch⟩
          cases r3 with
          | Err _ => cases ok_inj hbranch
          | Ok v =>
            cases core.result.Result.Ok.inj (ok_inj hbranch)
            obtain ⟨hne1, hne2⟩ := BranchCondOp_eval_not_uninit heval
            have ht1_ne : verifier.Type.ScalarValue data1 ≠ .ScalarValue uninitScalar64 :=
              fun hc => hne1 (verifier.Type.ScalarValue.inj hc)
            have ht2_ne : verifier.Type.ScalarValue data2 ≠ .ScalarValue uninitScalar64 :=
              fun hc => hne2 (verifier.Type.ScalarValue.inj hc)
            simp only [TypeRel2, hne1, hne2, ↓reduceIte] at hrel_t1 hrel_t2
            rcases hrel_t1 with ⟨x₁, x₂, rfl, rfl, hx_rel⟩
            rcases hrel_t2 with ⟨y₁, y₂, rfl, rfl, hy_rel⟩
            exact ⟨ht1_ne, ht2_ne, BranchCondOp_eval_ok_sound heval hx_rel hy_rel⟩
        | NullOr id2 sub2 =>
          dsimp only at hbranch
          rcases bind_eq_ok hbranch with ⟨b_unk, hb_unk, hbranch⟩; have hb_unk_eq : b_unk = decide (op = .set) := by { cases op <;> cases ok_inj hb_unk <;> rfl }; subst b_unk
          by_cases hop_set : op = .set
          · simp [hop_set] at hbranch
          · simp only [hop_set, decide_false, Bool.false_eq_true, ↓reduceIte] at hbranch
            cases is64 with
            | false =>
              simp only [jumpWidthOfBool, Bool.false_eq_true, ↓reduceIte, bind_ok] at hbranch
              cases ok_inj hbranch
            | true =>
              simp only [jumpWidthOfBool, ↓reduceIte] at hbranch
              rcases bind_eq_ok hbranch with ⟨res, hres, hbranch⟩
              cases res with
              | Err _ => cases ok_inj hbranch
              | Ok v =>
                cases core.result.Result.Ok.inj (ok_inj hbranch)
                rcases bind_eq_ok hres with ⟨bz, hbz, hres⟩
                cases bz with
                | false =>
                  simp only [Bool.false_eq_true, ↓reduceIte] at hres
                  cases ok_inj hres
                | true =>
                  simp only [↓reduceIte] at hres
                  cases core.result.Result.Ok.inj (ok_inj hres)
                  have hne1 := not_uninit_of_is_zero_64 hbz
                  simp only [TypeRel2, hne1, ↓reduceIte] at hrel_t1
                  rcases hrel_t1 with ⟨x₁, x₂, rfl, rfl, hx_rel⟩
                  have hx1_0 := is_zero_64_sound hbz hx_rel.1
                  have hx2_0 := is_zero_64_sound hbz hx_rel.2.1
                  subst hx1_0; subst hx2_0
                  refine ⟨fun hc => hne1 (verifier.Type.ScalarValue.inj hc), by nofun, ?_⟩
                  simp only [↓reduceIte, harr_eq]
                  refine ⟨?_, fun b hb => by cases hb⟩
                  cases hnull : isNullId (renameMemId ρ id2)
                  · obtain ⟨rfl, hp1_lo, hp1_hi⟩ :=
                      TypeRel2_non_map_ptr_bounds hl₁ ht2_valid hrel_t2 hnull
                    obtain ⟨_, hp2_lo, hp2_hi⟩ :=
                      TypeRel2_non_map_ptr_bounds hl₂ ht2_valid hrel_t2 hnull
                    exact (evalJump64_zero_vs_pos_eq hop_set hp1_lo hp1_hi hp2_lo hp2_hi).1
                  · simp only [TypeRel2, hnull, ↓reduceIte] at hrel_t2
                    rcases hrel_t2 with ⟨_, rfl, rfl⟩
                    rfl
        | ConstPtrToMap m s =>
          dsimp only at hbranch
          rcases bind_eq_ok hbranch with ⟨b_unk, hb_unk, hbranch⟩; have hb_unk_eq : b_unk = decide (op = .set) := by { cases op <;> cases ok_inj hb_unk <;> rfl }; subst b_unk
          by_cases hop_set : op = .set
          · simp [hop_set] at hbranch
          · simp only [hop_set, decide_false, Bool.false_eq_true, ↓reduceIte] at hbranch
            cases is64 with
            | false => simp [jumpWidthOfBool] at hbranch
            | true =>
              simp only [jumpWidthOfBool, ↓reduceIte] at hbranch
              rcases bind_eq_ok hbranch with ⟨res, hres, hbranch⟩
              cases res with
              | Err _ => cases ok_inj hbranch
              | Ok v =>
                rcases bind_eq_ok hres with ⟨bz, _, hres⟩
                cases bz with
                | false => simp at hres
                | true =>
                  simp only [↓reduceIte, Type_is_non_zero_false_of_map, bind_ok] at hres
                  cases ok_inj hres
        | PtrToStack _ | PtrToMemory _ _ _ _ | PtrToStruct _ _ _ | PtrToArray _ _ | PtrToEndArray _ =>
          dsimp only at hbranch
          rcases bind_eq_ok hbranch with ⟨b_unk, hb_unk, hbranch⟩; have hb_unk_eq : b_unk = decide (op = .set) := by { cases op <;> cases ok_inj hb_unk <;> rfl }; subst b_unk
          by_cases hop_set : op = .set
          · simp [hop_set] at hbranch
          · simp only [hop_set, decide_false, Bool.false_eq_true, ↓reduceIte] at hbranch
            cases is64 with
            | false => simp [jumpWidthOfBool] at hbranch
            | true =>
              simp only [jumpWidthOfBool, ↓reduceIte] at hbranch
              rcases bind_eq_ok hbranch with ⟨res, hres, hbranch⟩
              cases res with
              | Err _ => cases ok_inj hbranch
              | Ok v =>
                cases core.result.Result.Ok.inj (ok_inj hbranch)
                rcases bind_eq_ok hres with ⟨bz, hbz, hres⟩
                cases bz with
                | false => simp at hres
                | true =>
                  simp only [↓reduceIte] at hres
                  have hne1 := not_uninit_of_is_zero_64 hbz
                  simp only [TypeRel2, hne1, ↓reduceIte] at hrel_t1
                  rcases hrel_t1 with ⟨x₁, x₂, rfl, rfl, hx_rel⟩
                  have hx_eq : x₁ = x₂ :=
                    (is_zero_64_sound hbz hx_rel.1).trans (is_zero_64_sound hbz hx_rel.2.1).symm
                  subst hx_eq
                  obtain ⟨rfl, hp1_lo, hp1_hi⟩ :=
                    TypeRel2_non_map_ptr_bounds hl₁ ht2_valid hrel_t2 trivial
                  obtain ⟨_, hp2_lo, hp2_hi⟩ :=
                    TypeRel2_non_map_ptr_bounds hl₂ ht2_valid hrel_t2 trivial
                  refine ⟨fun hc => hne1 (verifier.Type.ScalarValue.inj hc), by nofun, ?_⟩
                  simp only [↓reduceIte, harr_eq]
                  exact non_zero_scalar_eval_lhs_sound hop_set hbz hx_rel hp1_lo hp1_hi hp2_lo hp2_hi hres
        | Releasable _ _ =>
          cases op2 with
          | Releasable _ sub =>
            cases sub <;> try exact False.elim hop2_valid
            all_goals cases ht2_eq
          | _ => cases ht2_eq
        | _ => exact False.elim ht2_valid
      | ConstPtrToMap m s =>
        dsimp only at hbranch
        rcases bind_eq_ok hbranch with ⟨b_unk, hb_unk, hbranch⟩; have hb_unk_eq : b_unk = decide (op = .set) := by { cases op <;> cases ok_inj hb_unk <;> rfl }; subst b_unk
        by_cases hop_set : op = .set
        · simp [hop_set] at hbranch
        · simp only [hop_set, decide_false, Bool.false_eq_true, ↓reduceIte] at hbranch
          cases is64 with
          | false => simp [jumpWidthOfBool] at hbranch
          | true =>
            simp only [jumpWidthOfBool, ↓reduceIte] at hbranch
            cases t2 with
            | ScalarValue data =>
              rcases bind_eq_ok hbranch with ⟨res, hres, hbranch⟩
              cases res with
              | Err _ => cases ok_inj hbranch
              | Ok v =>
                rcases bind_eq_ok hres with ⟨bz, _, hres⟩
                cases bz with
                | false => simp at hres
                | true =>
                  simp only [↓reduceIte, Type_is_non_zero_false_of_map, bind_ok] at hres
                  cases ok_inj hres
            | _ => simp at hbranch
      | NullOr id1 sub1 =>
        dsimp only at hbranch
        rcases bind_eq_ok hbranch with ⟨b_unk, hb_unk, hbranch⟩; have hb_unk_eq : b_unk = decide (op = .set) := by { cases op <;> cases ok_inj hb_unk <;> rfl }; subst b_unk
        by_cases hop_set : op = .set
        · simp [hop_set] at hbranch
        · simp only [hop_set, decide_false, Bool.false_eq_true, ↓reduceIte] at hbranch
          cases is64 with
          | false => simp [jumpWidthOfBool] at hbranch
          | true =>
            simp only [jumpWidthOfBool, ↓reduceIte] at hbranch
            cases t2 with
            | ScalarValue data2 =>
              rcases bind_eq_ok hbranch with ⟨res, hres, hbranch⟩
              cases res with
              | Err _ => cases ok_inj hbranch
              | Ok v =>
                cases core.result.Result.Ok.inj (ok_inj hbranch)
                rcases bind_eq_ok hres with ⟨bz, hbz, hres⟩
                cases bz with
                | false => simp at hres
                | true =>
                  simp only [↓reduceIte] at hres
                  cases core.result.Result.Ok.inj (ok_inj hres)
                  have hne2 := not_uninit_of_is_zero_64 hbz
                  simp only [TypeRel2, hne2, ↓reduceIte] at hrel_t2
                  rcases hrel_t2 with ⟨y₁, y₂, rfl, rfl, hy_rel⟩
                  have hy1_0 := is_zero_64_sound hbz hy_rel.1
                  have hy2_0 := is_zero_64_sound hbz hy_rel.2.1
                  subst hy1_0; subst hy2_0
                  refine ⟨by nofun, fun hc => hne2 (verifier.Type.ScalarValue.inj hc), ?_⟩
                  simp only [↓reduceIte, harr_eq]
                  refine ⟨?_, fun b hb => by cases hb⟩
                  cases hnull : isNullId (renameMemId ρ id1)
                  · obtain ⟨rfl, hp1_lo, hp1_hi⟩ :=
                      TypeRel2_non_map_ptr_bounds hl₁ ht1_valid hrel_t1 hnull
                    obtain ⟨_, hp2_lo, hp2_hi⟩ :=
                      TypeRel2_non_map_ptr_bounds hl₂ ht1_valid hrel_t1 hnull
                    exact (evalJump64_zero_vs_pos_eq hop_set hp1_lo hp1_hi hp2_lo hp2_hi).2
                  · simp only [TypeRel2, hnull, ↓reduceIte] at hrel_t1
                    rcases hrel_t1 with ⟨_, rfl, rfl⟩
                    rfl
            | _ => simp at hbranch
      | PtrToStack x =>
        dsimp only at hbranch
        rcases bind_eq_ok hbranch with ⟨b_unk, hb_unk, hbranch⟩; have hb_unk_eq : b_unk = decide (op = .set) := by { cases op <;> cases ok_inj hb_unk <;> rfl }; subst b_unk
        by_cases hop_set : op = .set
        · simp [hop_set] at hbranch
        · simp only [hop_set, decide_false, Bool.false_eq_true, ↓reduceIte] at hbranch
          cases is64 with
          | false => simp [jumpWidthOfBool] at hbranch
          | true =>
            simp only [jumpWidthOfBool, ↓reduceIte] at hbranch
            cases t2 with
            | ScalarValue data2 =>
              rcases bind_eq_ok hbranch with ⟨res, hres, hbranch⟩
              cases res with
              | Err _ => cases ok_inj hbranch
              | Ok v =>
                cases core.result.Result.Ok.inj (ok_inj hbranch)
                rcases bind_eq_ok hres with ⟨bz, hbz, hres⟩
                cases bz with
                | false => simp at hres
                | true =>
                  simp only [↓reduceIte] at hres
                  have hne2 := not_uninit_of_is_zero_64 hbz
                  simp only [TypeRel2, hne2, ↓reduceIte] at hrel_t2
                  rcases hrel_t2 with ⟨y₁, y₂, rfl, rfl, hy_rel⟩
                  have hy_eq : y₁ = y₂ :=
                    (is_zero_64_sound hbz hy_rel.1).trans (is_zero_64_sound hbz hy_rel.2.1).symm
                  subst hy_eq
                  obtain ⟨rfl, hp1_lo, hp1_hi⟩ :=
                    TypeRel2_non_map_ptr_bounds hl₁ ht1_valid hrel_t1 trivial
                  obtain ⟨_, hp2_lo, hp2_hi⟩ :=
                    TypeRel2_non_map_ptr_bounds hl₂ ht1_valid hrel_t1 trivial
                  refine ⟨by nofun, fun hc => hne2 (verifier.Type.ScalarValue.inj hc), ?_⟩
                  simp only [↓reduceIte, harr_eq]
                  exact non_zero_scalar_eval_rhs_sound hop_set hbz hy_rel hp1_lo hp1_hi hp2_lo hp2_hi hres
            | PtrToStack y =>
              rcases bind_eq_ok hbranch with ⟨res, hres, hbranch⟩
              cases res with
              | Err _ => cases ok_inj hbranch
              | Ok v =>
                cases core.result.Result.Ok.inj (ok_inj hbranch)
                dsimp only at hres
                rw [bind_ok_eq (StackOffset_reg_spec x)] at hres
                rcases bind_eq_ok hres with ⟨ix, _, hres⟩
                rcases bind_eq_ok hres with ⟨ib_lim, _, hres⟩
                rcases bind_eq_ok hres with ⟨in_b, _, hres⟩
                rw [bind_ok_eq (StackOffset_reg_spec y)] at hres
                rcases hrel_t1 with ⟨hpx, ox, hox_rel, rfl, rfl⟩
                rcases hrel_t2 with ⟨hpy, oy, hoy_rel, rfl, rfl⟩
                have hox_le : ox ≤ 0xFFFF_FFFF#64 := BitVec.le_trans hox_rel.1.2.2.2 hpx.2.2
                have hoy_le : oy ≤ 0xFFFF_FFFF#64 := BitVec.le_trans hoy_rel.1.2.2.2 hpy.2.2
                obtain ⟨_, hopt⟩ := eval_same_region_ptr_ok_sound hres hox_rel hoy_rel
                have hb1 := evalJump64_same_base_eq hop_set hl₁.1.1 hl₁.1.2 hox_le hoy_le
                have hb2 := evalJump64_same_base_eq hop_set hl₂.1.1 hl₂.1.2 hox_le hoy_le
                refine ⟨by nofun, by nofun, ?_⟩
                simp only [↓reduceIte, RuntimeVal.toBits, hb1, hb2]
                exact ⟨trivial, hopt⟩
            | _ => simp at hbranch
      | PtrToMemory id1 x b1 m1 =>
        dsimp only at hbranch
        rcases bind_eq_ok hbranch with ⟨b_unk, hb_unk, hbranch⟩; have hb_unk_eq : b_unk = decide (op = .set) := by { cases op <;> cases ok_inj hb_unk <;> rfl }; subst b_unk
        by_cases hop_set : op = .set
        · simp [hop_set] at hbranch
        · simp only [hop_set, decide_false, Bool.false_eq_true, ↓reduceIte] at hbranch
          cases is64 with
          | false => simp [jumpWidthOfBool] at hbranch
          | true =>
            simp only [jumpWidthOfBool, ↓reduceIte] at hbranch
            cases t2 with
            | ScalarValue data2 =>
              rcases bind_eq_ok hbranch with ⟨res, hres, hbranch⟩
              cases res with
              | Err _ => cases ok_inj hbranch
              | Ok v =>
                cases core.result.Result.Ok.inj (ok_inj hbranch)
                rcases bind_eq_ok hres with ⟨bz, hbz, hres⟩
                cases bz with
                | false => simp at hres
                | true =>
                  simp only [↓reduceIte] at hres
                  have hne2 := not_uninit_of_is_zero_64 hbz
                  simp only [TypeRel2, hne2, ↓reduceIte] at hrel_t2
                  rcases hrel_t2 with ⟨y₁, y₂, rfl, rfl, hy_rel⟩
                  have hy_eq : y₁ = y₂ :=
                    (is_zero_64_sound hbz hy_rel.1).trans (is_zero_64_sound hbz hy_rel.2.1).symm
                  subst hy_eq
                  obtain ⟨rfl, hp1_lo, hp1_hi⟩ :=
                    TypeRel2_non_map_ptr_bounds hl₁ ht1_valid hrel_t1 trivial
                  obtain ⟨_, hp2_lo, hp2_hi⟩ :=
                    TypeRel2_non_map_ptr_bounds hl₂ ht1_valid hrel_t1 trivial
                  refine ⟨by nofun, fun hc => hne2 (verifier.Type.ScalarValue.inj hc), ?_⟩
                  simp only [↓reduceIte, harr_eq]
                  exact non_zero_scalar_eval_rhs_sound hop_set hbz hy_rel hp1_lo hp1_hi hp2_lo hp2_hi hres
            | PtrToMemory id2 y b2 m2 =>
              rcases bind_eq_ok hbranch with ⟨res, hres, hbranch⟩
              cases res with
              | Err _ => cases ok_inj hbranch
              | Ok v =>
                cases core.result.Result.Ok.inj (ok_inj hbranch)
                dsimp only at hres
                rw [bind_ok_eq (MemoryId_eq_spec id1 id2)] at hres
                by_cases hid : id1 = id2
                · subst hid
                  simp only [decide_true, ↓reduceIte] at hres
                  rcases bind_eq_ok hres with ⟨ix, _, hres⟩
                  rcases bind_eq_ok hres with ⟨in_b, _, hres⟩
                  rcases hrel_t1 with ⟨hpx, ox, hox_rel, rfl, rfl⟩
                  rcases hrel_t2 with ⟨hpy, oy, hoy_rel, rfl, rfl⟩
                  have hox_le : ox ≤ 0xFFFF_FFFF#64 := BitVec.le_trans hox_rel.1.2.2.2 hpx.2.2
                  have hoy_le : oy ≤ 0xFFFF_FFFF#64 := BitVec.le_trans hoy_rel.1.2.2.2 hpy.2.2
                  obtain ⟨_, hopt⟩ := eval_same_region_ptr_ok_sound hres hox_rel hoy_rel
                  have hb1 := evalJump64_same_base_eq hop_set (hl₁.2.1 (renameMemId ρ id1)).1 (hl₁.2.1 (renameMemId ρ id1)).2 hox_le hoy_le
                  have hb2 := evalJump64_same_base_eq hop_set (hl₂.2.1 (renameMemId ρ id1)).1 (hl₂.2.1 (renameMemId ρ id1)).2 hox_le hoy_le
                  refine ⟨by nofun, by nofun, ?_⟩
                  simp only [↓reduceIte, RuntimeVal.toBits, hb1, hb2]
                  exact ⟨trivial, hopt⟩
                · simp [hid] at hres
            | _ => simp at hbranch
      | PtrToStruct id1 x sd1 =>
        dsimp only at hbranch
        rcases bind_eq_ok hbranch with ⟨b_unk, hb_unk, hbranch⟩; have hb_unk_eq : b_unk = decide (op = .set) := by { cases op <;> cases ok_inj hb_unk <;> rfl }; subst b_unk
        by_cases hop_set : op = .set
        · simp [hop_set] at hbranch
        · simp only [hop_set, decide_false, Bool.false_eq_true, ↓reduceIte] at hbranch
          cases is64 with
          | false => simp [jumpWidthOfBool] at hbranch
          | true =>
            simp only [jumpWidthOfBool, ↓reduceIte] at hbranch
            cases t2 with
            | ScalarValue data2 =>
              rcases bind_eq_ok hbranch with ⟨res, hres, hbranch⟩
              cases res with
              | Err _ => cases ok_inj hbranch
              | Ok v =>
                cases core.result.Result.Ok.inj (ok_inj hbranch)
                rcases bind_eq_ok hres with ⟨bz, hbz, hres⟩
                cases bz with
                | false => simp at hres
                | true =>
                  simp only [↓reduceIte] at hres
                  have hne2 := not_uninit_of_is_zero_64 hbz
                  simp only [TypeRel2, hne2, ↓reduceIte] at hrel_t2
                  rcases hrel_t2 with ⟨y₁, y₂, rfl, rfl, hy_rel⟩
                  have hy_eq : y₁ = y₂ :=
                    (is_zero_64_sound hbz hy_rel.1).trans (is_zero_64_sound hbz hy_rel.2.1).symm
                  subst hy_eq
                  obtain ⟨rfl, hp1_lo, hp1_hi⟩ :=
                    TypeRel2_non_map_ptr_bounds hl₁ ht1_valid hrel_t1 trivial
                  obtain ⟨_, hp2_lo, hp2_hi⟩ :=
                    TypeRel2_non_map_ptr_bounds hl₂ ht1_valid hrel_t1 trivial
                  refine ⟨by nofun, fun hc => hne2 (verifier.Type.ScalarValue.inj hc), ?_⟩
                  simp only [↓reduceIte, harr_eq]
                  exact non_zero_scalar_eval_rhs_sound hop_set hbz hy_rel hp1_lo hp1_hi hp2_lo hp2_hi hres
            | PtrToStruct id2 y sd2 =>
              rcases bind_eq_ok hbranch with ⟨res, hres, hbranch⟩
              cases res with
              | Err _ => cases ok_inj hbranch
              | Ok v =>
                cases core.result.Result.Ok.inj (ok_inj hbranch)
                dsimp only at hres
                rw [bind_ok_eq (MemoryId_eq_spec id1 id2)] at hres
                by_cases hid : id1 = id2
                · subst hid
                  simp only [decide_true, ↓reduceIte] at hres
                  rcases bind_eq_ok hres with ⟨bzx, _, hres⟩
                  rcases bind_eq_ok hres with ⟨in_b, _, hres⟩
                  rcases hrel_t1 with ⟨hpx, _, ox, hox_rel, rfl, rfl⟩
                  rcases hrel_t2 with ⟨hpy, _, oy, hoy_rel, rfl, rfl⟩
                  have hox_le : ox ≤ 0xFFFF_FFFF#64 := BitVec.le_trans hox_rel.1.2.2.2 hpx.2.2
                  have hoy_le : oy ≤ 0xFFFF_FFFF#64 := BitVec.le_trans hoy_rel.1.2.2.2 hpy.2.2
                  obtain ⟨_, hopt⟩ := eval_same_region_ptr_ok_sound hres hox_rel hoy_rel
                  have hb1 := evalJump64_same_base_eq hop_set (hl₁.2.2.1 (renameMemId ρ id1)).1 (hl₁.2.2.1 (renameMemId ρ id1)).2 hox_le hoy_le
                  have hb2 := evalJump64_same_base_eq hop_set (hl₂.2.2.1 (renameMemId ρ id1)).1 (hl₂.2.2.1 (renameMemId ρ id1)).2 hox_le hoy_le
                  refine ⟨by nofun, by nofun, ?_⟩
                  simp only [↓reduceIte, RuntimeVal.toBits, hb1, hb2]
                  exact ⟨trivial, hopt⟩
                · simp [hid] at hbranch hres
            | _ => simp at hbranch
      | PtrToArray id1 x =>
        dsimp only at hbranch
        rcases bind_eq_ok hbranch with ⟨b_unk, hb_unk, hbranch⟩; have hb_unk_eq : b_unk = decide (op = .set) := by { cases op <;> cases ok_inj hb_unk <;> rfl }; subst b_unk
        by_cases hop_set : op = .set
        · simp [hop_set] at hbranch
        · simp only [hop_set, decide_false, Bool.false_eq_true, ↓reduceIte] at hbranch
          cases is64 with
          | false => simp [jumpWidthOfBool] at hbranch
          | true =>
            simp only [jumpWidthOfBool, ↓reduceIte] at hbranch
            cases t2 with
            | ScalarValue data2 =>
              rcases bind_eq_ok hbranch with ⟨res, hres, hbranch⟩
              cases res with
              | Err _ => cases ok_inj hbranch
              | Ok v =>
                cases core.result.Result.Ok.inj (ok_inj hbranch)
                rcases bind_eq_ok hres with ⟨bz, hbz, hres⟩
                cases bz with
                | false => simp at hres
                | true =>
                  simp only [↓reduceIte] at hres
                  have hne2 := not_uninit_of_is_zero_64 hbz
                  simp only [TypeRel2, hne2, ↓reduceIte] at hrel_t2
                  rcases hrel_t2 with ⟨y₁, y₂, rfl, rfl, hy_rel⟩
                  have hy_eq : y₁ = y₂ :=
                    (is_zero_64_sound hbz hy_rel.1).trans (is_zero_64_sound hbz hy_rel.2.1).symm
                  subst hy_eq
                  obtain ⟨rfl, hp1_lo, hp1_hi⟩ :=
                    TypeRel2_non_map_ptr_bounds hl₁ ht1_valid hrel_t1 trivial
                  obtain ⟨_, hp2_lo, hp2_hi⟩ :=
                    TypeRel2_non_map_ptr_bounds hl₂ ht1_valid hrel_t1 trivial
                  refine ⟨by nofun, fun hc => hne2 (verifier.Type.ScalarValue.inj hc), ?_⟩
                  simp only [↓reduceIte, harr_eq]
                  exact non_zero_scalar_eval_rhs_sound hop_set hbz hy_rel hp1_lo hp1_hi hp2_lo hp2_hi hres
            | PtrToArray id2 y =>
              rcases bind_eq_ok hbranch with ⟨res, hres, hbranch⟩
              cases res with
              | Err _ => cases ok_inj hbranch
              | Ok v =>
                cases core.result.Result.Ok.inj (ok_inj hbranch)
                dsimp only at hres
                rw [bind_ok_eq (MemoryId_eq_spec id1 id2)] at hres
                by_cases hid : id1 = id2
                · subst hid
                  simp only [decide_true, ↓reduceIte] at hres
                  rcases bind_eq_ok hres with ⟨bound, _, hres⟩
                  rcases bind_eq_ok hres with ⟨ix, _, hres⟩
                  rcases bind_eq_ok hres with ⟨in_b, _, hres⟩
                  rcases hrel_t1 with ⟨hpx, _, ox, hox_rel, rfl, rfl⟩
                  rcases hrel_t2 with ⟨hpy, _, oy, hoy_rel, rfl, rfl⟩
                  have hox_le : ox ≤ 0xFFFF_FFFF#64 :=
                    BitVec.le_trans hox_rel.1.2.2.2 (BitVec.le_trans hpx.2.2 (by decide))
                  have hoy_le : oy ≤ 0xFFFF_FFFF#64 :=
                    BitVec.le_trans hoy_rel.1.2.2.2 (BitVec.le_trans hpy.2.2 (by decide))
                  obtain ⟨_, hopt⟩ := eval_same_region_ptr_ok_sound hres hox_rel hoy_rel
                  have hb1 := evalJump64_same_base_eq hop_set (hl₁.2.2.2.1 (renameMemId ρ id1)).1 (hl₁.2.2.2.1 (renameMemId ρ id1)).2 hox_le hoy_le
                  have hb2 := evalJump64_same_base_eq hop_set (hl₂.2.2.2.1 (renameMemId ρ id1)).1 (hl₂.2.2.2.1 (renameMemId ρ id1)).2 hox_le hoy_le
                  refine ⟨by nofun, by nofun, ?_⟩
                  simp only [↓reduceIte, RuntimeVal.toBits, hb1, hb2]
                  exact ⟨trivial, hopt⟩
                · simp [hid] at hres
            | PtrToEndArray id2 =>
              rcases bind_eq_ok hbranch with ⟨res, hres, hbranch⟩
              cases res with
              | Err _ => cases ok_inj hbranch
              | Ok v =>
                cases core.result.Result.Ok.inj (ok_inj hbranch)
                dsimp only at hres
                rw [bind_ok_eq (MemoryId_eq_spec id1 id2)] at hres
                by_cases hid : id1 = id2
                · subst hid
                  simp only [decide_true, ↓reduceIte] at hres
                  rcases bind_eq_ok hres with ⟨bx_init, _, hres⟩
                  cases bx_init with
                  | false => simp at hres
                  | true =>
                    simp only [↓reduceIte] at hres
                    cases core.result.Result.Ok.inj (ok_inj hres)
                    rcases hrel_t1 with ⟨hpx, hlen, ox, hox_rel, rfl, rfl⟩
                    rcases hrel_t2 with ⟨_, rfl, rfl⟩
                    have hox_le : ox ≤ 0xFFFF_FFFF#64 :=
                      BitVec.le_trans hox_rel.1.2.2.2 (BitVec.le_trans hpx.2.2 (by decide))
                    have hb1 := evalJump64_same_base_eq hop_set (hl₁.2.2.2.1 (renameMemId ρ id1)).1 (hl₁.2.2.2.1 (renameMemId ρ id1)).2 hox_le hlen
                    have hb2 := evalJump64_same_base_eq hop_set (hl₂.2.2.2.1 (renameMemId ρ id1)).1 (hl₂.2.2.2.1 (renameMemId ρ id1)).2 hox_le hlen
                    refine ⟨by nofun, by nofun, ?_⟩
                    simp only [↓reduceIte, RuntimeVal.toBits, harr_eq, hb1, hb2]
                    exact ⟨trivial, fun b hb => by cases hb⟩
                · simp [hid] at hres
            | _ => simp at hbranch
      | PtrToEndArray id1 =>
        dsimp only at hbranch
        rcases bind_eq_ok hbranch with ⟨b_unk, hb_unk, hbranch⟩; have hb_unk_eq : b_unk = decide (op = .set) := by { cases op <;> cases ok_inj hb_unk <;> rfl }; subst b_unk
        by_cases hop_set : op = .set
        · simp [hop_set] at hbranch
        · simp only [hop_set, decide_false, Bool.false_eq_true, ↓reduceIte] at hbranch
          cases is64 with
          | false => simp [jumpWidthOfBool] at hbranch
          | true =>
            simp only [jumpWidthOfBool, ↓reduceIte] at hbranch
            cases t2 with
            | ScalarValue data2 =>
              rcases bind_eq_ok hbranch with ⟨res, hres, hbranch⟩
              cases res with
              | Err _ => cases ok_inj hbranch
              | Ok v =>
                cases core.result.Result.Ok.inj (ok_inj hbranch)
                rcases bind_eq_ok hres with ⟨bz, hbz, hres⟩
                cases bz with
                | false => simp at hres
                | true =>
                  simp only [↓reduceIte] at hres
                  have hne2 := not_uninit_of_is_zero_64 hbz
                  simp only [TypeRel2, hne2, ↓reduceIte] at hrel_t2
                  rcases hrel_t2 with ⟨y₁, y₂, rfl, rfl, hy_rel⟩
                  have hy_eq : y₁ = y₂ :=
                    (is_zero_64_sound hbz hy_rel.1).trans (is_zero_64_sound hbz hy_rel.2.1).symm
                  subst hy_eq
                  obtain ⟨rfl, hp1_lo, hp1_hi⟩ :=
                    TypeRel2_non_map_ptr_bounds hl₁ ht1_valid hrel_t1 trivial
                  obtain ⟨_, hp2_lo, hp2_hi⟩ :=
                    TypeRel2_non_map_ptr_bounds hl₂ ht1_valid hrel_t1 trivial
                  refine ⟨by nofun, fun hc => hne2 (verifier.Type.ScalarValue.inj hc), ?_⟩
                  simp only [↓reduceIte, harr_eq]
                  exact non_zero_scalar_eval_rhs_sound hop_set hbz hy_rel hp1_lo hp1_hi hp2_lo hp2_hi hres
            | PtrToArray id2 y =>
              rcases bind_eq_ok hbranch with ⟨res, hres, hbranch⟩
              cases res with
              | Err _ => cases ok_inj hbranch
              | Ok v =>
                cases core.result.Result.Ok.inj (ok_inj hbranch)
                dsimp only at hres
                rw [bind_ok_eq (MemoryId_eq_spec id1 id2)] at hres
                by_cases hid : id1 = id2
                · subst hid
                  simp only [decide_true, ↓reduceIte] at hres
                  rcases bind_eq_ok hres with ⟨by_init, _, hres⟩
                  cases by_init with
                  | false => simp at hres
                  | true =>
                    simp only [↓reduceIte] at hres
                    cases core.result.Result.Ok.inj (ok_inj hres)
                    rcases hrel_t1 with ⟨hlen, rfl, rfl⟩
                    rcases hrel_t2 with ⟨hpy, _, oy, hoy_rel, rfl, rfl⟩
                    have hoy_le : oy ≤ 0xFFFF_FFFF#64 :=
                      BitVec.le_trans hoy_rel.1.2.2.2 (BitVec.le_trans hpy.2.2 (by decide))
                    have hb1 := evalJump64_same_base_eq hop_set (hl₁.2.2.2.1 (renameMemId ρ id1)).1 (hl₁.2.2.2.1 (renameMemId ρ id1)).2 hlen hoy_le
                    have hb2 := evalJump64_same_base_eq hop_set (hl₂.2.2.2.1 (renameMemId ρ id1)).1 (hl₂.2.2.2.1 (renameMemId ρ id1)).2 hlen hoy_le
                    refine ⟨by nofun, by nofun, ?_⟩
                    simp only [↓reduceIte, RuntimeVal.toBits, harr_eq, hb1, hb2]
                    exact ⟨trivial, fun b hb => by cases hb⟩
                · simp [hid] at hres
            | _ => simp at hbranch
      | Releasable _ _ =>
        cases op1 with
        | Releasable _ sub =>
          cases sub <;> try exact False.elim hop1_valid
          all_goals cases ht1_eq
        | _ => cases ht1_eq
      | _ => exact False.elim ht1_valid

/-! ## 3. `Type.constrain_equal_offsets`, `Type.constraint`, & `apply_constraints_and_register` -/

private theorem verifier_U32_MAX_eq_ok :
    verifier.U32_MAX = ok 0xFFFF_FFFF#u64 := by
  unfold verifier.U32_MAX
  exact congrArg ok ((u64_eq_iff_bv _ _).mpr (by simp))

private theorem u32_max_shl_32_eq_ok :
    (0xFFFF_FFFF#u64 <<< 32#i32 : Result Std.U64) = ok 0xFFFF_FFFF_0000_0000#u64 := by
  have hge : (32#i32).val ≥ 0 := by decide
  have hlt : (32#i32).toNat < 64 := by decide
  change UScalar.shiftLeft_IScalar 0xFFFF_FFFF#u64 32#i32 = ok 0xFFFF_FFFF_0000_0000#u64
  simp only [UScalar.shiftLeft_IScalar, hge, ↓reduceIte, UScalar.shiftLeft, UScalarTy.numBits, hlt]
  exact congrArg ok ((u64_eq_iff_bv _ _).mpr (by decide))

private theorem verifier_max_u64_eq_ok (a b : Std.U64) :
    verifier.max_u64 a b = ok ⟨if b.bv ≤ a.bv then a.bv else b.bv⟩ := by
  unfold verifier.max_u64
  by_cases h : a ≥ b
  · have hbv : b.bv ≤ a.bv := (u64_ge_iff_bv a b).mp h
    simp [h, hbv]
  · have hbv : ¬(b.bv ≤ a.bv) := fun hc => h ((u64_ge_iff_bv a b).mpr hc)
    simp [h, hbv]

private theorem verifier_min_u64_eq_ok (a b : Std.U64) :
    verifier.min_u64 a b = ok ⟨if a.bv ≤ b.bv then a.bv else b.bv⟩ := by
  unfold verifier.min_u64
  by_cases h : a ≤ b
  · have hbv : a.bv ≤ b.bv := (u64_le_iff_bv a b).mp h
    simp [h, hbv]
  · have hbv : ¬(a.bv ≤ b.bv) := fun hc => h ((u64_le_iff_bv a b).mpr hc)
    simp [h, hbv]

private theorem pureNew64_max_le_of_contains (umin umax val unk : BitVec 64)
    (hc : pureContainsMaskedValue64 umin umax val unk = true) :
    (pureNew64 val unk 0#64 umin umax).urange.max.bv ≤ umax := by
  unfold pureContainsMaskedValue64 pureNew64 at *
  simp only [u64_mk_uscalar_bv] at *
  split_ifs at hc ⊢ <;> bv_decide

private theorem can_be_equal_64_contains {d1 d2 : U64ScalarValueData}
    (h : U64ScalarValueData.can_be_equal d1 d2 = ok true) :
    pureContainsMaskedValue64
      (if d2.urange.min.bv ≤ d1.urange.min.bv then d1.urange.min.bv else d2.urange.min.bv)
      (if d1.urange.max.bv ≤ d2.urange.max.bv then d1.urange.max.bv else d2.urange.max.bv)
      (d1.value.bv ||| d2.value.bv)
      (d1.unknown_mask.bv &&& d2.unknown_mask.bv) = true := by
  unfold U64ScalarValueData.can_be_equal at h
  simp only [lift, bind_ok] at h
  split_ifs at h with hne
  · cases ok_inj h
  · have hmin1 : U64ScalarValueData.min d1 = ok d1.urange.min := rfl
    have hmin2 : U64ScalarValueData.min d2 = ok d2.urange.min := rfl
    have hmax1 : U64ScalarValueData.max d1 = ok d1.urange.max := rfl
    have hmax2 : U64ScalarValueData.max d2 = ok d2.urange.max := rfl
    simp only [u64_guard_eta] at h
    rw [bind_ok_eq hmin1, bind_ok_eq hmin2,
        bind_ok_eq (max_u64_eq_ok d1.urange.min d2.urange.min),
        bind_ok_eq hmax1, bind_ok_eq hmax2,
        bind_ok_eq (min_u64_eq_ok d1.urange.max d2.urange.max)] at h
    have hur : U64Range.new
        ⟨if d2.urange.min.bv ≤ d1.urange.min.bv then d1.urange.min.bv else d2.urange.min.bv⟩
        ⟨if d1.urange.max.bv ≤ d2.urange.max.bv then d1.urange.max.bv else d2.urange.max.bv⟩ =
      ok { min := ⟨if d2.urange.min.bv ≤ d1.urange.min.bv then d1.urange.min.bv else d2.urange.min.bv⟩,
           max := ⟨if d1.urange.max.bv ≤ d2.urange.max.bv then d1.urange.max.bv else d2.urange.max.bv⟩ } := rfl
    rw [bind_ok_eq hur] at h
    rw [contains_masked_value_64_eq_ok] at h
    simp only [u64_mk_bv, u64_or_bv, u64_and_bv] at h
    exact ok_inj h

private theorem w64_eq_tnum_sound {x v1 u1 v2 u2 : BitVec 64}
    (h1 : x &&& ~~~u1 = v1) (h2 : x &&& ~~~u2 = v2) :
    x &&& ~~~(u1 &&& u2) = (v1 ||| v2) &&& ~~~(u1 &&& u2) := by
  bv_decide

private theorem w32_eq_tnum_sound {x y v1 u1 v2 u2 : BitVec 64}
    (h1 : x &&& ~~~u1 = v1) (h2 : y &&& ~~~u2 = v2)
    (heq32 : x.setWidth 32 = y.setWidth 32) :
    x &&& ~~~(u1 &&& (u2 ||| 0xFFFF_FFFF_0000_0000#64)) =
      (v1 ||| (v2 &&& 0xFFFF_FFFF#64)) &&& ~~~(u1 &&& (u2 ||| 0xFFFF_FFFF_0000_0000#64)) := by
  bv_decide

private theorem w32_eq_fit_u32_eq {x y : BitVec 64}
    (hx : x ≤ 0xFFFF_FFFF#64) (hy : y ≤ 0xFFFF_FFFF#64)
    (heq32 : x.setWidth 32 = y.setWidth 32) :
    x = y := by
  bv_decide

private theorem bv_cancel_add_left {base o1 o2 : BitVec 64}
    (h : base + o1 = base + o2) : o1 = o2 := by
  bv_decide

private theorem array_end_le_of_ptr_le {base o len : BitVec 64}
    (hlo : 0#64 < base) (hhi : base ≤ CANONICAL_USER_MAX)
    (ho : o ≤ 0xFFFF_FFFF#64) (hlen : len ≤ 0xFFFF_FFFF#64)
    (hle : base + o ≤ base + len) :
    o ≤ len := by
  unfold CANONICAL_USER_MAX at hhi
  bv_decide

private theorem array_end_add1_le_of_ptr_lt {base o len : BitVec 64}
    (hlo : 0#64 < base) (hhi : base ≤ CANONICAL_USER_MAX)
    (ho : o ≤ 0xFFFF_FFFF#64) (hlen : len ≤ 0xFFFF_FFFF#64)
    (hlt : base + o < base + len) :
    o + 1#64 ≤ len := by
  unfold CANONICAL_USER_MAX at hhi
  bv_decide

private def typePtrOffset? (t : verifier.Type) : Option U64ScalarValueData :=
  match pureUnwrapReleasable t with
  | .PtrToStack off => some off
  | .PtrToMemory _ off _ _ => some off
  | .PtrToStruct _ off _ => some off
  | .PtrToArray _ off => some off
  | _ => none

private theorem Type_with_offset_valid_le
    {env : ExecEnv} {vCounter : Nat} {t : verifier.Type} {off off0 : U64ScalarValueData}
    (ht : TypeValid env vCounter t)
    (hoff0 : typePtrOffset? t = some off0)
    (hv : Scalar64Valid off)
    (hunw : off.unwritten_mask.bv = 0#64)
    (hle : off.urange.max.bv ≤ off0.urange.max.bv) :
    TypeValid env vCounter (pureTypeWithOffset t off) := by
  cases t with
  | PtrToStack d =>
    cases Option.some.inj hoff0
    exact ⟨hv, hunw, BitVec.le_trans hle ht.2.2⟩
  | PtrToMemory id d sz orig =>
    cases Option.some.inj hoff0
    exact ⟨ht.1, hv, hunw, BitVec.le_trans hle ht.2.2.2⟩
  | PtrToStruct id d sd =>
    cases Option.some.inj hoff0
    exact ⟨ht.1, ⟨hv, hunw, BitVec.le_trans hle ht.2.1.2.2⟩, ht.2.2⟩
  | PtrToArray id d =>
    cases Option.some.inj hoff0
    exact ⟨ht.1, hv, hunw, BitVec.le_trans hle ht.2.2.2⟩
  | Releasable relId inner =>
    cases inner <;> try exact False.elim ht
    · cases Option.some.inj hoff0
      exact ⟨ht.1, ht.2.1, hv, hunw, BitVec.le_trans hle ht.2.2.2.2⟩
    · cases Option.some.inj hoff0
      exact ⟨ht.1, ht.2.1, ⟨hv, hunw, BitVec.le_trans hle ht.2.2.1.2.2⟩, ht.2.2.2⟩
  | _ => simp [typePtrOffset?, pureUnwrapReleasable] at hoff0

private theorem Type_constraint_scalar_eq64_spec
    {context context1 : verifier.ComputationContext}
    {d1 d2 : U64ScalarValueData} {new1 new2 : verifier.Type}
    (h : verifier.Type.constraint context .Eq .W64
      (.ScalarValue d1) (.ScalarValue d2) = ok (.Ok (new1, new2), context1)) :
    context1 = context ∧
    ((new1 = .ScalarValue d1 ∧ new2 = .ScalarValue d2) ∨
     (∃ usvd : U64ScalarValueData,
        d1.unwritten_mask = 0#u64 ∧ d1 ≠ uninitScalar64 ∧
        d2.unwritten_mask = 0#u64 ∧ d2 ≠ uninitScalar64 ∧
        new1 = .ScalarValue usvd ∧ new2 = .ScalarValue usvd ∧
        Scalar64Valid usvd ∧
        usvd.unwritten_mask.bv = 0#64 ∧
        usvd.urange.max.bv ≤ d1.urange.max.bv ∧
        usvd.urange.max.bv ≤ d2.urange.max.bv ∧
        (∀ x : BitVec 64,
          ConcretizesScalar64Rel2 d1 x x →
          ConcretizesScalar64Rel2 d2 x x →
          ConcretizesScalar64Rel2 usvd x x))) := by
  rw [verifier.Type.constraint.eq_def] at h
  have hc1 : verifier.Type.check_not_released (.ScalarValue d1) context.resources = ok (.Ok ()) := by
    rw [verifier.Type.check_not_released.eq_def]
  have hc2 : verifier.Type.check_not_released (.ScalarValue d2) context.resources = ok (.Ok ()) := by
    rw [verifier.Type.check_not_released.eq_def]
  rw [bind_ok_eq hc1] at h
  dsimp only at h
  rw [bind_ok_eq hc2] at h
  dsimp only at h
  rw [bind_ok_eq (Type_unwrap_releasable_spec (.ScalarValue d1)),
      bind_ok_eq (Type_unwrap_releasable_spec (.ScalarValue d2))] at h
  dsimp only [pureUnwrapReleasable] at h
  rcases bind_eq_ok h with ⟨b, hb, h⟩
  cases b with
  | false =>
    simp only [Bool.false_eq_true, ↓reduceIte] at h
    rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨rfl, Or.inl ⟨rfl, rfl⟩⟩
  | true =>
    simp only [↓reduceIte] at h
    rcases bind_eq_ok h with ⟨b1, hb1, h⟩
    cases b1 with
    | false =>
      simp only [Bool.false_eq_true, ↓reduceIte] at h
      rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
      rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
      exact ⟨rfl, Or.inl ⟨rfl, rfl⟩⟩
    | true =>
      simp only [↓reduceIte] at h
      rcases bind_eq_ok h with ⟨b2, hb2, h⟩
      cases b2 with
      | false =>
        simp only [Bool.false_eq_true, ↓reduceIte] at h
        rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
        rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
        exact ⟨rfl, Or.inl ⟨rfl, rfl⟩⟩
      | true =>
        simp only [↓reduceIte] at h
        have ⟨hunw1, hne1⟩ := not_uninit_of_fully_init_64 hb
        have ⟨hunw2, hne2⟩ := not_uninit_of_fully_init_64 hb1
        have hcont := can_be_equal_64_contains hb2
        have hmin1 : U64ScalarValueData.min d1 = ok d1.urange.min := rfl
        have hmin2 : U64ScalarValueData.min d2 = ok d2.urange.min := rfl
        have hmax1 : U64ScalarValueData.max d1 = ok d1.urange.max := rfl
        have hmax2 : U64ScalarValueData.max d2 = ok d2.urange.max := rfl
        rw [bind_ok_eq hmin1, bind_ok_eq hmin2,
            bind_ok_eq (verifier_max_u64_eq_ok d1.urange.min d2.urange.min),
            bind_ok_eq hmax1, bind_ok_eq hmax2,
            bind_ok_eq (verifier_min_u64_eq_ok d1.urange.max d2.urange.max)] at h
        simp only [lift, bind_ok] at h
        set umin_bv := if d2.urange.min.bv ≤ d1.urange.min.bv then d1.urange.min.bv else d2.urange.min.bv
        set umax_bv := if d1.urange.max.bv ≤ d2.urange.max.bv then d1.urange.max.bv else d2.urange.max.bv
        set ur : U64Range := { min := ⟨umin_bv⟩, max := ⟨umax_bv⟩ }
        have hur : U64Range.new ⟨umin_bv⟩ ⟨umax_bv⟩ = ok ur := rfl
        rw [bind_ok_eq hur,
            bind_ok_eq (new64_eq_ok (d1.value ||| d2.value) (d1.unknown_mask &&& d2.unknown_mask) 0#u64 ur)] at h
        set usvd := pureNew64 (d1.value.bv ||| d2.value.bv)
          (d1.unknown_mask.bv &&& d2.unknown_mask.bv) 0#64 umin_bv umax_bv
        rcases bind_eq_ok h with ⟨t2, ht2_cl, h⟩
        rw [Type_clone_spec] at ht2_cl
        have husvd_eq : pureNew64 (U64.bv (d1.value ||| d2.value))
            (U64.bv (d1.unknown_mask &&& d2.unknown_mask)) (U64.bv 0#u64) ur.min.bv ur.max.bv = usvd := by
          simp only [u64_or_bv, u64_and_bv, u64_zero_bv, ur, u64_mk_uscalar_bv, usvd]
        rw [husvd_eq] at ht2_cl h
        cases ok_inj ht2_cl
        rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
        rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
        have hmax_le := pureNew64_max_le_of_contains umin_bv umax_bv
          (d1.value.bv ||| d2.value.bv) (d1.unknown_mask.bv &&& d2.unknown_mask.bv) hcont
        have humax1 : umax_bv ≤ d1.urange.max.bv := by dsimp [umax_bv]; split_ifs <;> bv_decide
        have humax2 : umax_bv ≤ d2.urange.max.bv := by dsimp [umax_bv]; split_ifs <;> bv_decide
        refine ⟨rfl, Or.inr ⟨usvd, hunw1, hne1, hunw2, hne2, rfl, rfl,
          pureNew64_valid _ _ _ _ _, (by dsimp [usvd, pureNew64]; exact BitVec.zero_and),
          BitVec.le_trans hmax_le humax1,
          BitVec.le_trans hmax_le humax2, ?_⟩⟩
        intro x hx1 hx2
        have hr : ConcretizesRange64 ur x := by
          refine ⟨?_, ?_⟩
          · show umin_bv ≤ x
            have h1 := hx1.1.2.2.1; have h2 := hx2.1.2.2.1
            dsimp [umin_bv]; split_ifs <;> bv_decide
          · show x ≤ umax_bv
            have h1 := hx1.1.2.2.2; have h2 := hx2.1.2.2.2
            dsimp [umax_bv]; split_ifs <;> bv_decide
        have ht := w64_eq_tnum_sound hx1.1.2.1 hx2.1.2.1
        obtain ⟨res, hres, hc⟩ := new64_sound_2safety
          (d1.value ||| d2.value) (d1.unknown_mask &&& d2.unknown_mask) 0#u64 ur x x
          hr ht hr ht rfl
        rw [new64_eq_ok] at hres
        cases ok_inj hres
        exact hc

theorem Type_constrain_equal_offsets_ok_valid
    {env : ExecEnv} {vCounter : Nat}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {offset1 offset2 : U64ScalarValueData}
    (hctx : CompCtxValid env vCounter context)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (hoff1 : typePtrOffset? type1 = some offset1)
    (hoff2 : typePtrOffset? type2 = some offset2)
    (h : verifier.Type.constrain_equal_offsets context type1 type2 offset1 offset2 =
      ok (.Ok (new1, new2), context1)) :
    context1.pc = context.pc ∧
    CompCtxValid env vCounter context1 ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 := by
  rw [verifier.Type.constrain_equal_offsets.eq_def] at h
  rcases bind_eq_ok h with ⟨⟨r, ctx'⟩, hc, h⟩
  cases r with
  | Err _ => cases (Prod.mk.inj (ok_inj h)).1
  | Ok pair =>
    rcases pair with ⟨no1, no2⟩
    obtain ⟨rfl, hcases⟩ := Type_constraint_scalar_eq64_spec hc
    have hoff1_v : Scalar64Valid offset1 ∧ offset1.unwritten_mask.bv = 0#64 := by
      cases type1 with
      | PtrToStack d =>
        cases Option.some.inj hoff1
        exact ⟨ht1.1, ht1.2.1⟩
      | PtrToMemory id d sz orig =>
        cases Option.some.inj hoff1
        exact ⟨ht1.2.1, ht1.2.2.1⟩
      | PtrToStruct id d sd =>
        cases Option.some.inj hoff1
        exact ⟨ht1.2.1.1, ht1.2.1.2.1⟩
      | PtrToArray id d =>
        cases Option.some.inj hoff1
        exact ⟨ht1.2.1, ht1.2.2.1⟩
      | Releasable relId inner =>
        cases inner <;> try exact False.elim ht1
        · cases Option.some.inj hoff1
          exact ⟨ht1.2.2.1, ht1.2.2.2.1⟩
        · cases Option.some.inj hoff1
          exact ⟨ht1.2.2.1.1, ht1.2.2.1.2.1⟩
      | _ => simp [typePtrOffset?, pureUnwrapReleasable] at hoff1
    have hoff2_v : Scalar64Valid offset2 ∧ offset2.unwritten_mask.bv = 0#64 := by
      cases type2 with
      | PtrToStack d =>
        cases Option.some.inj hoff2
        exact ⟨ht2.1, ht2.2.1⟩
      | PtrToMemory id d sz orig =>
        cases Option.some.inj hoff2
        exact ⟨ht2.2.1, ht2.2.2.1⟩
      | PtrToStruct id d sd =>
        cases Option.some.inj hoff2
        exact ⟨ht2.2.1.1, ht2.2.1.2.1⟩
      | PtrToArray id d =>
        cases Option.some.inj hoff2
        exact ⟨ht2.2.1, ht2.2.2.1⟩
      | Releasable relId inner =>
        cases inner <;> try exact False.elim ht2
        · cases Option.some.inj hoff2
          exact ⟨ht2.2.2.1, ht2.2.2.2.1⟩
        · cases Option.some.inj hoff2
          exact ⟨ht2.2.2.1.1, ht2.2.2.1.2.1⟩
      | _ => simp [typePtrOffset?, pureUnwrapReleasable] at hoff2
    rcases hcases with ⟨rfl, rfl⟩ | ⟨usvd, _, _, _, _, rfl, rfl, husvd_v, husvd_unw, hle1, hle2, _⟩
    · rcases bind_eq_ok h with ⟨res1, hres1, h⟩; cases ok_inj hres1
      rcases bind_eq_ok h with ⟨res2, hres2, h⟩; cases ok_inj hres2
      rcases bind_eq_ok h with ⟨t, htw1, h⟩; rw [Type_with_offset_spec] at htw1; cases ok_inj htw1
      rcases bind_eq_ok h with ⟨t1, htw2, h⟩; rw [Type_with_offset_spec] at htw2; cases ok_inj htw2
      rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
      rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
      exact ⟨rfl, hctx,
        Type_with_offset_valid_le ht1 hoff1 hoff1_v.1 hoff1_v.2 (BitVec.le_refl _),
        Type_with_offset_valid_le ht2 hoff2 hoff2_v.1 hoff2_v.2 (BitVec.le_refl _)⟩
    · rcases bind_eq_ok h with ⟨res1, hres1, h⟩; cases ok_inj hres1
      rcases bind_eq_ok h with ⟨res2, hres2, h⟩; cases ok_inj hres2
      rcases bind_eq_ok h with ⟨t, htw1, h⟩; rw [Type_with_offset_spec] at htw1; cases ok_inj htw1
      rcases bind_eq_ok h with ⟨t1, htw2, h⟩; rw [Type_with_offset_spec] at htw2; cases ok_inj htw2
      rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
      rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
      exact ⟨rfl, hctx,
        Type_with_offset_valid_le ht1 hoff1 husvd_v husvd_unw hle1,
        Type_with_offset_valid_le ht2 hoff2 husvd_v husvd_unw hle2⟩

private theorem Type_constrain_equal_offsets_spec_rel2
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {offset1 offset2 : U64ScalarValueData} {o : BitVec 64}
    (hu1 : offset1.unwritten_mask.bv = 0#64)
    (hu2 : offset2.unwritten_mask.bv = 0#64)
    (hc1 : ConcretizesScalar64Rel2 offset1 o o)
    (hc2 : ConcretizesScalar64Rel2 offset2 o o)
    (h : verifier.Type.constrain_equal_offsets context type1 type2 offset1 offset2 =
      ok (.Ok (new1, new2), context1)) :
    context1 = context ∧
    ∃ resolved1 resolved2 : U64ScalarValueData,
      new1 = pureTypeWithOffset type1 resolved1 ∧
      new2 = pureTypeWithOffset type2 resolved2 ∧
      Scalar64Valid resolved1 ∧ resolved1.unwritten_mask.bv = 0#64 ∧
      resolved1.urange.max.bv ≤ offset1.urange.max.bv ∧
      ConcretizesScalar64Rel2 resolved1 o o ∧
      Scalar64Valid resolved2 ∧ resolved2.unwritten_mask.bv = 0#64 ∧
      resolved2.urange.max.bv ≤ offset2.urange.max.bv ∧
      ConcretizesScalar64Rel2 resolved2 o o := by
  rw [verifier.Type.constrain_equal_offsets.eq_def] at h
  rcases bind_eq_ok h with ⟨⟨r, ctx'⟩, hc, h⟩
  cases r with
  | Err _ => cases (Prod.mk.inj (ok_inj h)).1
  | Ok pair =>
    rcases pair with ⟨no1, no2⟩
    obtain ⟨rfl, hcases⟩ := Type_constraint_scalar_eq64_spec hc
    have hv1 : Scalar64Valid offset1 := Scalar64Valid.of_consistent hc1.1.1
    have hv2 : Scalar64Valid offset2 := Scalar64Valid.of_consistent hc2.1.1
    rcases hcases with ⟨rfl, rfl⟩ | ⟨usvd, _, _, _, _, rfl, rfl, husvd_v, husvd_unw, hle1, hle2, hconc⟩
    · rcases bind_eq_ok h with ⟨res1, hres1, h⟩; cases ok_inj hres1
      rcases bind_eq_ok h with ⟨res2, hres2, h⟩; cases ok_inj hres2
      rcases bind_eq_ok h with ⟨t, htw1, h⟩; rw [Type_with_offset_spec] at htw1; cases ok_inj htw1
      rcases bind_eq_ok h with ⟨t1, htw2, h⟩; rw [Type_with_offset_spec] at htw2; cases ok_inj htw2
      rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
      rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
      exact ⟨rfl, offset1, offset2, rfl, rfl,
        hv1, hu1, BitVec.le_refl _, hc1,
        hv2, hu2, BitVec.le_refl _, hc2⟩
    · rcases bind_eq_ok h with ⟨res1, hres1, h⟩; cases ok_inj hres1
      rcases bind_eq_ok h with ⟨res2, hres2, h⟩; cases ok_inj hres2
      rcases bind_eq_ok h with ⟨t, htw1, h⟩; rw [Type_with_offset_spec] at htw1; cases ok_inj htw1
      rcases bind_eq_ok h with ⟨t1, htw2, h⟩; rw [Type_with_offset_spec] at htw2; cases ok_inj htw2
      rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
      rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
      have husvd_c := hconc o hc1 hc2
      exact ⟨rfl, usvd, usvd, rfl, rfl,
        husvd_v, husvd_unw, hle1, husvd_c,
        husvd_v, husvd_unw, hle2, husvd_c⟩

private theorem NullOr_zero_iff_isNull
    {env : ExecEnv} {vCounter : Nat} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {arr : MemoryId → BitVec 64}
    {l : HostLayout} {id : MemoryId} {inner : verifier.Type}
    {v₁ v₂ : RuntimeVal}
    (hl : ValidHostLayout l)
    (ht : TypeValid env vCounter (.NullOr id inner))
    (hrel : TypeRel2 env ρ resActive isNullId arr (.NullOr id inner) v₁ v₂)
    (hz : v₁.toBits l arr = 0#64) :
    isNullId (renameMemId ρ id) = true ∧
    nullInnerInactive ρ resActive inner ∧
    v₁ = .scalar 0#64 ∧ v₂ = .scalar 0#64 := by
  cases h : isNullId (renameMemId ρ id) with
  | false =>
    exact False.elim ((TypeRel2_ptr_nonscalar_and_nonzero hl ht hrel h).2.2 hz)
  | true =>
    simp only [TypeRel2, h, ↓reduceIte] at hrel
    exact ⟨rfl, hrel⟩

private theorem NullOr_nonzero_iff_not_isNull
    {env : ExecEnv} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {arr : MemoryId → BitVec 64}
    {l : HostLayout} {id : MemoryId} {inner : verifier.Type}
    {v₁ v₂ : RuntimeVal}
    (hrel : TypeRel2 env ρ resActive isNullId arr (.NullOr id inner) v₁ v₂)
    (hne : v₁.toBits l arr ≠ 0#64) :
    isNullId (renameMemId ρ id) = false ∧
    TypeRel2 env ρ resActive isNullId arr inner v₁ v₂ := by
  cases h : isNullId (renameMemId ρ id) with
  | true =>
    simp only [TypeRel2, h, ↓reduceIte] at hrel
    rcases hrel with ⟨_, rfl, _⟩
    exact False.elim (hne rfl)
  | false =>
    simp only [TypeRel2, h, Bool.false_eq_true, ↓reduceIte] at hrel
    exact ⟨rfl, hrel⟩

private theorem unwrap_eq_of_scalar {env : ExecEnv} {vCounter : Nat}
    {op : verifier.Type} {d : U64ScalarValueData}
    (ht : TypeValid env vCounter op) (hu : pureUnwrapReleasable op = .ScalarValue d) :
    op = .ScalarValue d := by
  cases op with
  | ScalarValue _ => exact hu
  | Releasable _ sub =>
    cases sub <;> try exact False.elim ht
    all_goals cases hu
  | _ => cases hu

private theorem unwrap_eq_of_nullOr {env : ExecEnv} {vCounter : Nat}
    {op : verifier.Type} {id : MemoryId} {inner : verifier.Type}
    (ht : TypeValid env vCounter op) (hu : pureUnwrapReleasable op = .NullOr id inner) :
    op = .NullOr id inner := by
  cases op with
  | NullOr _ _ => exact hu
  | Releasable _ sub =>
    cases sub <;> try exact False.elim ht
    all_goals cases hu
  | _ => cases hu

private theorem unwrap_eq_of_stack {env : ExecEnv} {vCounter : Nat}
    {op : verifier.Type} {so : U64ScalarValueData}
    (ht : TypeValid env vCounter op) (hu : pureUnwrapReleasable op = .PtrToStack so) :
    op = .PtrToStack so := by
  cases op with
  | PtrToStack _ => exact hu
  | Releasable _ sub =>
    cases sub <;> try exact False.elim ht
    all_goals cases hu
  | _ => cases hu

private theorem unwrap_eq_of_array {env : ExecEnv} {vCounter : Nat}
    {op : verifier.Type} {id : MemoryId} {off : U64ScalarValueData}
    (ht : TypeValid env vCounter op) (hu : pureUnwrapReleasable op = .PtrToArray id off) :
    op = .PtrToArray id off := by
  cases op with
  | PtrToArray _ _ => exact hu
  | Releasable _ sub =>
    cases sub <;> try exact False.elim ht
    all_goals cases hu
  | _ => cases hu

private theorem unwrap_eq_of_endArray {env : ExecEnv} {vCounter : Nat}
    {op : verifier.Type} {id : MemoryId}
    (ht : TypeValid env vCounter op) (hu : pureUnwrapReleasable op = .PtrToEndArray id) :
    op = .PtrToEndArray id := by
  cases op with
  | PtrToEndArray _ => exact hu
  | Releasable _ sub =>
    cases sub <;> try exact False.elim ht
    all_goals cases hu
  | _ => cases hu

private theorem mem_ptr_extract_rel2
    {env : ExecEnv} {vCounter : Nat} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {arr : MemoryId → BitVec 64}
    {l : HostLayout} {op : verifier.Type}
    {id : MemoryId} {off : U64ScalarValueData} {sz : Std.U64} {orig : verifier.PointerOrigin}
    {v₁ v₂ : RuntimeVal}
    (ht : TypeValid env vCounter op)
    (hu : pureUnwrapReleasable op = .PtrToMemory id off sz orig)
    (hrel : TypeRel2 env ρ resActive isNullId arr op v₁ v₂) :
    typePtrOffset? op = some off ∧
    off.unwritten_mask.bv = 0#64 ∧
    ∃ o : BitVec 64,
      ConcretizesScalar64Rel2 off o o ∧
      v₁.toBits l arr = l.bufBase (renameMemId ρ id) + o ∧
      (∀ r_off : U64ScalarValueData,
        Scalar64Valid r_off → r_off.unwritten_mask.bv = 0#64 →
        r_off.urange.max.bv ≤ off.urange.max.bv →
        ConcretizesScalar64Rel2 r_off o o →
        TypeValid env vCounter (pureTypeWithOffset op r_off) ∧
        TypeRel2 env ρ resActive isNullId arr (pureTypeWithOffset op r_off) v₁ v₂) := by
  cases op with
  | PtrToMemory id' off' sz' orig' =>
    cases hu
    rcases hrel with ⟨hp, o, hconc, rfl, rfl⟩
    refine ⟨rfl, hp.2.1, o, hconc, rfl, fun r_off hv hunw hle hc => ?_⟩
    have hp' : PtrOffsetValid 0xFFFF_FFFF#64 r_off := ⟨hv, hunw, BitVec.le_trans hle hp.2.2⟩
    exact ⟨⟨ht.1, hp'⟩, ⟨hp', o, hc, rfl, rfl⟩⟩
  | Releasable relId sub =>
    cases sub <;> try exact False.elim ht
    · cases hu
      rcases hrel with ⟨hact, hp, o, hconc, rfl, rfl⟩
      refine ⟨rfl, hp.2.1, o, hconc, rfl, fun r_off hv hunw hle hc => ?_⟩
      have hp' : PtrOffsetValid 0xFFFF_FFFF#64 r_off := ⟨hv, hunw, BitVec.le_trans hle hp.2.2⟩
      exact ⟨⟨ht.1, ht.2.1, hp'⟩, ⟨hact, hp', o, hc, rfl, rfl⟩⟩
    · cases hu
  | _ => cases hu

private theorem struct_ptr_extract_rel2
    {env : ExecEnv} {vCounter : Nat} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {arr : MemoryId → BitVec 64}
    {l : HostLayout} {op : verifier.Type}
    {id : MemoryId} {off : U64ScalarValueData} {sd : verifier.StructDescriptor}
    {v₁ v₂ : RuntimeVal}
    (ht : TypeValid env vCounter op)
    (hu : pureUnwrapReleasable op = .PtrToStruct id off sd)
    (hrel : TypeRel2 env ρ resActive isNullId arr op v₁ v₂) :
    typePtrOffset? op = some off ∧
    off.unwritten_mask.bv = 0#64 ∧
    ∃ o : BitVec 64,
      ConcretizesScalar64Rel2 off o o ∧
      v₁.toBits l arr = l.structBase (renameMemId ρ id) + o ∧
      (∀ r_off : U64ScalarValueData,
        Scalar64Valid r_off → r_off.unwritten_mask.bv = 0#64 →
        r_off.urange.max.bv ≤ off.urange.max.bv →
        ConcretizesScalar64Rel2 r_off o o →
        TypeValid env vCounter (pureTypeWithOffset op r_off) ∧
        TypeRel2 env ρ resActive isNullId arr (pureTypeWithOffset op r_off) v₁ v₂) := by
  cases op with
  | PtrToStruct id' off' sd' =>
    cases hu
    rcases hrel with ⟨hp, hsd, o, hconc, rfl, rfl⟩
    refine ⟨rfl, hp.2.1, o, hconc, rfl, fun r_off hv hunw hle hc => ?_⟩
    have hp' : PtrOffsetValid 0xFFFF_FFFF#64 r_off := ⟨hv, hunw, BitVec.le_trans hle hp.2.2⟩
    exact ⟨⟨ht.1, hp', ht.2.2⟩, ⟨hp', hsd, o, hc, rfl, rfl⟩⟩
  | Releasable relId sub =>
    cases sub <;> try exact False.elim ht
    · cases hu
    · cases hu
      rcases hrel with ⟨hact, hp, hsd, o, hconc, rfl, rfl⟩
      refine ⟨rfl, hp.2.1, o, hconc, rfl, fun r_off hv hunw hle hc => ?_⟩
      have hp' : PtrOffsetValid 0xFFFF_FFFF#64 r_off := ⟨hv, hunw, BitVec.le_trans hle hp.2.2⟩
      exact ⟨⟨ht.1, ht.2.1, hp', ht.2.2.2⟩, ⟨hact, hp', hsd, o, hc, rfl, rfl⟩⟩
  | _ => cases hu

private theorem constraint_w32_eq_scalars_ok_valid
    {env : ExecEnv} {vCounter : Nat}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {data1 data2 : U64ScalarValueData}
    (hctx : CompCtxValid env vCounter context)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (h : (do
      let b ← U64ScalarValueData.is_fully_initialized data1
      if b then
        let b1 ← U64ScalarValueData.is_fully_initialized data2
        if b1 then
          let usvd ← U32ScalarValueData.from_u64_scalar data1
          let usvd1 ← U32ScalarValueData.from_u64_scalar data2
          let b2 ← U32ScalarValueData.can_be_equal usvd usvd1
          if b2 then
            let i ← U64ScalarValueData.max data1
            let i1 ← verifier.U32_MAX
            let both_fit_u32 ←
              if i <= i1 then do
                let i2 ← U64ScalarValueData.max data2
                ok (i2 <= i1)
              else ok false
            let (data11, data21, urange1, urange2) ←
              if both_fit_u32 then do
                let i2 ← U64ScalarValueData.min data1
                let i3 ← U64ScalarValueData.min data2
                let umin ← verifier.max_u64 i2 i3
                let i4 ← U64ScalarValueData.max data2
                let umax ← verifier.min_u64 i i4
                let urange ← U64Range.new umin umax
                ok (data1, data2, urange, urange)
              else
                ok ({ data1 with _guard := () }, { data2 with _guard := () },
                  data1.urange, data2.urange)
            let i2 ← lift (data21.value &&& i1)
            let value1 ← lift (data11.value ||| i2)
            let i3 ← i1 <<< 32#i32
            let i4 ← lift (data21.unknown_mask ||| i3)
            let unknown_mask1 ← lift (data11.unknown_mask &&& i4)
            let i5 ← lift (data11.value &&& i1)
            let value2 ← lift (data21.value ||| i5)
            let i6 ← i1 <<< 32#i32
            let i7 ← lift (data11.unknown_mask ||| i6)
            let unknown_mask2 ← lift (data21.unknown_mask &&& i7)
            let o ← U64ScalarValueData.try_new value1 unknown_mask1 0#u64 urange1
            let o1 ← U64ScalarValueData.try_new value2 unknown_mask2 0#u64 urange2
            match o with
            | none => ok (core.result.Result.Ok (type1, type2), context)
            | some s1 =>
              match o1 with
              | none => ok (core.result.Result.Ok (type1, type2), context)
              | some s2 =>
                ok (core.result.Result.Ok (verifier.Type.ScalarValue s1,
                  verifier.Type.ScalarValue s2), context)
          else ok (core.result.Result.Ok (type1, type2), context)
        else ok (core.result.Result.Ok (type1, type2), context)
      else ok (core.result.Result.Ok (type1, type2), context) :
        Result ((core.result.Result (verifier.Type × verifier.Type) error.VerifierErrorKind) ×
          verifier.ComputationContext)) =
      ok (.Ok (new1, new2), context1)) :
    context1.pc = context.pc ∧
    CompCtxValid env vCounter context1 ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 := by
  rcases bind_eq_ok h with ⟨b, _, h⟩
  cases b with
  | false =>
    simp only [Bool.false_eq_true, ↓reduceIte] at h
    rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨rfl, hctx, ht1, ht2⟩
  | true =>
    simp only [↓reduceIte] at h
    rcases bind_eq_ok h with ⟨b1, _, h⟩
    cases b1 with
    | false =>
      simp only [Bool.false_eq_true, ↓reduceIte] at h
      rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
      rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
      exact ⟨rfl, hctx, ht1, ht2⟩
    | true =>
      simp only [↓reduceIte] at h
      rcases bind_eq_ok h with ⟨usvd, _, h⟩
      rcases bind_eq_ok h with ⟨usvd1, _, h⟩
      rcases bind_eq_ok h with ⟨b2, _, h⟩
      cases b2 with
      | false =>
        simp only [Bool.false_eq_true, ↓reduceIte] at h
        rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
        rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
        exact ⟨rfl, hctx, ht1, ht2⟩
      | true =>
        simp only [↓reduceIte] at h
        rcases bind_eq_ok h with ⟨i, _, h⟩
        rcases bind_eq_ok h with ⟨i1, _, h⟩
        rcases bind_eq_ok h with ⟨bfit, _, h⟩
        rcases bind_eq_ok h with ⟨⟨d11, d21, ur1, ur2⟩, _, h⟩
        simp only [lift, bind_ok] at h
        rcases bind_eq_ok h with ⟨i3, _, h⟩
        rcases bind_eq_ok h with ⟨i6, _, h⟩
        rcases bind_eq_ok h with ⟨o, ho, h⟩
        rcases bind_eq_ok h with ⟨o1, ho1, h⟩
        cases o with
        | none =>
          dsimp only at h
          rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
          rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
          exact ⟨rfl, hctx, ht1, ht2⟩
        | some s1 =>
          cases o1 with
          | none =>
            dsimp only at h
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            exact ⟨rfl, hctx, ht1, ht2⟩
          | some s2 =>
            dsimp only at h
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            exact ⟨rfl, hctx, try_new_64_valid ho, try_new_64_valid ho1⟩

private theorem constraint_w32_eq_scalars_ok_sound
    {env : ExecEnv} {vCounter : Nat} {deps : verifier.DataDependencies}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {st₁ st₂ : MachineState} {l₁ : HostLayout}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {data1 data2 : U64ScalarValueData}
    {v1_1 v1_2 v2_1 v2_2 : RuntimeVal}
    (hctx : CompCtxValid env vCounter context)
    (hrel_ctx : StateRel2Deps env context deps ρ resActive isNullId st₁ st₂)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (hu1 : pureUnwrapReleasable type1 = .ScalarValue data1)
    (hu2 : pureUnwrapReleasable type2 = .ScalarValue data2)
    (hrel1 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type1 v1_1 v1_2)
    (hrel2 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type2 v2_1 v2_2)
    (hjt : JumpTypeHolds .Eq .W32
      (v1_1.toBits l₁ st₁.world.arrayLen) (v2_1.toBits l₁ st₁.world.arrayLen))
    (h : (do
      let b ← U64ScalarValueData.is_fully_initialized data1
      if b then
        let b1 ← U64ScalarValueData.is_fully_initialized data2
        if b1 then
          let usvd ← U32ScalarValueData.from_u64_scalar data1
          let usvd1 ← U32ScalarValueData.from_u64_scalar data2
          let b2 ← U32ScalarValueData.can_be_equal usvd usvd1
          if b2 then
            let i ← U64ScalarValueData.max data1
            let i1 ← verifier.U32_MAX
            let both_fit_u32 ←
              if i <= i1 then do
                let i2 ← U64ScalarValueData.max data2
                ok (i2 <= i1)
              else ok false
            let (data11, data21, urange1, urange2) ←
              if both_fit_u32 then do
                let i2 ← U64ScalarValueData.min data1
                let i3 ← U64ScalarValueData.min data2
                let umin ← verifier.max_u64 i2 i3
                let i4 ← U64ScalarValueData.max data2
                let umax ← verifier.min_u64 i i4
                let urange ← U64Range.new umin umax
                ok (data1, data2, urange, urange)
              else
                ok ({ data1 with _guard := () }, { data2 with _guard := () },
                  data1.urange, data2.urange)
            let i2 ← lift (data21.value &&& i1)
            let value1 ← lift (data11.value ||| i2)
            let i3 ← i1 <<< 32#i32
            let i4 ← lift (data21.unknown_mask ||| i3)
            let unknown_mask1 ← lift (data11.unknown_mask &&& i4)
            let i5 ← lift (data11.value &&& i1)
            let value2 ← lift (data21.value ||| i5)
            let i6 ← i1 <<< 32#i32
            let i7 ← lift (data11.unknown_mask ||| i6)
            let unknown_mask2 ← lift (data21.unknown_mask &&& i7)
            let o ← U64ScalarValueData.try_new value1 unknown_mask1 0#u64 urange1
            let o1 ← U64ScalarValueData.try_new value2 unknown_mask2 0#u64 urange2
            match o with
            | none => ok (core.result.Result.Ok (type1, type2), context)
            | some s1 =>
              match o1 with
              | none => ok (core.result.Result.Ok (type1, type2), context)
              | some s2 =>
                ok (core.result.Result.Ok (verifier.Type.ScalarValue s1,
                  verifier.Type.ScalarValue s2), context)
          else ok (core.result.Result.Ok (type1, type2), context)
        else ok (core.result.Result.Ok (type1, type2), context)
      else ok (core.result.Result.Ok (type1, type2), context) :
        Result ((core.result.Result (verifier.Type × verifier.Type) error.VerifierErrorKind) ×
          verifier.ComputationContext)) =
      ok (.Ok (new1, new2), context1)) :
    CompCtxValid env vCounter context1 ∧
    StateRel2Deps env context1 deps ρ resActive isNullId st₁ st₂ ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new1 v1_1 v1_2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new2 v2_1 v2_2 := by
  have hop1 := unwrap_eq_of_scalar ht1 hu1; subst hop1
  have hop2 := unwrap_eq_of_scalar ht2 hu2; subst hop2
  rcases bind_eq_ok h with ⟨b, hb, h⟩
  cases b with
  | false =>
    simp only [Bool.false_eq_true, ↓reduceIte] at h
    rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
  | true =>
    simp only [↓reduceIte] at h
    rcases bind_eq_ok h with ⟨b1, hb1, h⟩
    cases b1 with
    | false =>
      simp only [Bool.false_eq_true, ↓reduceIte] at h
      rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
      rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
      exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
    | true =>
      simp only [↓reduceIte] at h
      rcases bind_eq_ok h with ⟨usvd, _, h⟩
      rcases bind_eq_ok h with ⟨usvd1, _, h⟩
      rcases bind_eq_ok h with ⟨b2, _, h⟩
      cases b2 with
      | false =>
        simp only [Bool.false_eq_true, ↓reduceIte] at h
        rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
        rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
        exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
      | true =>
        simp only [↓reduceIte] at h
        have ⟨hunw1, hne1⟩ := not_uninit_of_fully_init_64 hb
        have ⟨hunw2, hne2⟩ := not_uninit_of_fully_init_64 hb1
        have hrel1' := hrel1; simp only [TypeRel2, hne1, ↓reduceIte] at hrel1'
        have hrel2' := hrel2; simp only [TypeRel2, hne2, ↓reduceIte] at hrel2'
        rcases hrel1' with ⟨x₁, x₂, rfl, rfl, hx⟩
        rcases hrel2' with ⟨y₁, y₂, rfl, rfl, hy⟩
        have hx_eq := fully_initialized_eq_of_rel2_64 hunw1 hx; subst x₂
        have hy_eq := fully_initialized_eq_of_rel2_64 hunw2 hy; subst y₂
        change x₁.setWidth 32 = y₁.setWidth 32 at hjt
        have hmax1 : U64ScalarValueData.max data1 = ok data1.urange.max := rfl
        have hmax2 : U64ScalarValueData.max data2 = ok data2.urange.max := rfl
        have hmin1 : U64ScalarValueData.min data1 = ok data1.urange.min := rfl
        have hmin2 : U64ScalarValueData.min data2 = ok data2.urange.min := rfl
        rw [bind_ok_eq hmax1, bind_ok_eq verifier_U32_MAX_eq_ok] at h
        rcases bind_eq_ok h with ⟨bfit, hbfit, h⟩
        rcases bind_eq_ok h with ⟨⟨data11, data21, urange1, urange2⟩, hquad, h⟩
        have hquad_spec :
            data11 = data1 ∧ data21 = data2 ∧
            ConcretizesRange64 urange1 x₁ ∧ ConcretizesRange64 urange2 y₁ := by
          cases bfit with
          | false =>
            simp only [Bool.false_eq_true, ↓reduceIte, u64_guard_eta] at hquad
            have hq0 := ok_inj hquad
            have hd11 : data11 = data1 := (Prod.mk.inj hq0).1.symm
            have hd21 : data21 = data2 := (Prod.mk.inj (Prod.mk.inj hq0).2).1.symm
            have hur1_eq : urange1 = data1.urange := (Prod.mk.inj (Prod.mk.inj (Prod.mk.inj hq0).2).2).1.symm
            have hur2_eq : urange2 = data2.urange := (Prod.mk.inj (Prod.mk.inj (Prod.mk.inj hq0).2).2).2.symm
            subst hd11; subst hd21; subst hur1_eq; subst hur2_eq
            exact ⟨rfl, rfl, hx.1.2.2, hy.1.2.2⟩
          | true =>
            by_cases hf1 : data1.urange.max <= 0xFFFF_FFFF#u64
            · simp only [hf1, ↓reduceIte] at hbfit
              rw [bind_ok_eq hmax2] at hbfit
              have hf2 : data2.urange.max <= 0xFFFF_FFFF#u64 := of_decide_eq_true (ok_inj hbfit)
              simp only [↓reduceIte] at hquad
              rw [bind_ok_eq hmin1, bind_ok_eq hmin2,
                  bind_ok_eq (verifier_max_u64_eq_ok data1.urange.min data2.urange.min),
                  bind_ok_eq hmax2,
                  bind_ok_eq (verifier_min_u64_eq_ok data1.urange.max data2.urange.max)] at hquad
              set umin_bv := if data2.urange.min.bv ≤ data1.urange.min.bv then data1.urange.min.bv else data2.urange.min.bv
              set umax_bv := if data1.urange.max.bv ≤ data2.urange.max.bv then data1.urange.max.bv else data2.urange.max.bv
              set ur : U64Range := { min := ⟨umin_bv⟩, max := ⟨umax_bv⟩ }
              have hur : U64Range.new ⟨umin_bv⟩ ⟨umax_bv⟩ = ok ur := rfl
              rw [bind_ok_eq hur] at hquad
              have hq0 := ok_inj hquad
              have hd11 : data11 = data1 := (Prod.mk.inj hq0).1.symm
              have hd21 : data21 = data2 := (Prod.mk.inj (Prod.mk.inj hq0).2).1.symm
              have hur1_eq : urange1 = ur := (Prod.mk.inj (Prod.mk.inj (Prod.mk.inj hq0).2).2).1.symm
              have hur2_eq : urange2 = ur := (Prod.mk.inj (Prod.mk.inj (Prod.mk.inj hq0).2).2).2.symm
              subst hd11; subst hd21; subst hur1_eq; subst hur2_eq
              have hx_fit : x₁ ≤ 0xFFFF_FFFF#64 :=
                BitVec.le_trans hx.1.2.2.2 ((u64_le_iff_bv _ _).mp hf1)
              have hy_fit : y₁ ≤ 0xFFFF_FFFF#64 :=
                BitVec.le_trans hy.1.2.2.2 ((u64_le_iff_bv _ _).mp hf2)
              have hxy : x₁ = y₁ := w32_eq_fit_u32_eq hx_fit hy_fit hjt
              have hr1 : ConcretizesRange64 ur x₁ := by
                have h1 := hx.1.2.2.1; have h2 := hx.1.2.2.2
                have h3 := hy.1.2.2.1; have h4 := hy.1.2.2.2
                subst hxy
                refine ⟨?_, ?_⟩
                · show umin_bv ≤ x₁; dsimp [umin_bv]; split_ifs <;> bv_decide
                · show x₁ ≤ umax_bv; dsimp [umax_bv]; split_ifs <;> bv_decide
              exact ⟨rfl, rfl, hr1, hxy ▸ hr1⟩
            · simp [hf1] at hbfit
        obtain ⟨hd11, hd21, hur1, hur2⟩ := hquad_spec
        subst data11; subst data21
        simp only [lift, bind_ok] at h
        rcases bind_eq_ok h with ⟨i3, hi3, h⟩
        rw [u32_max_shl_32_eq_ok] at hi3; cases ok_inj hi3
        rcases bind_eq_ok h with ⟨i6, hi6, h⟩
        rw [u32_max_shl_32_eq_ok] at hi6; cases ok_inj hi6
        rcases bind_eq_ok h with ⟨o, ho, h⟩
        rcases bind_eq_ok h with ⟨o1, ho1, h⟩
        cases o with
        | none =>
          dsimp only at h
          rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
          rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
          exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
        | some s1 =>
          cases o1 with
          | none =>
            dsimp only at h
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | some s2 =>
            dsimp only at h
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            have ht_s1 : x₁ &&& ~~~U64.bv (data1.unknown_mask &&& (data2.unknown_mask ||| 0xFFFF_FFFF_0000_0000#u64)) =
                U64.bv (data1.value ||| (data2.value &&& 0xFFFF_FFFF#u64)) &&&
                  ~~~U64.bv (data1.unknown_mask &&& (data2.unknown_mask ||| 0xFFFF_FFFF_0000_0000#u64)) :=
              w32_eq_tnum_sound hx.1.2.1 hy.1.2.1 hjt
            have ht_s2 : y₁ &&& ~~~U64.bv (data2.unknown_mask &&& (data1.unknown_mask ||| 0xFFFF_FFFF_0000_0000#u64)) =
                U64.bv (data2.value ||| (data1.value &&& 0xFFFF_FFFF#u64)) &&&
                  ~~~U64.bv (data2.unknown_mask &&& (data1.unknown_mask ||| 0xFFFF_FFFF_0000_0000#u64)) :=
              w32_eq_tnum_sound hy.1.2.1 hx.1.2.1 hjt.symm
            have hs1_rel := try_new_64_some_sound_2safety ho hur1 ht_s1 hur1 ht_s1 rfl
            have hs2_rel := try_new_64_some_sound_2safety ho1 hur2 ht_s2 hur2 ht_s2 rfl
            exact ⟨hctx, hrel_ctx, try_new_64_valid ho, try_new_64_valid ho1,
              TypeRel2_scalar_rel2_of_concretizes hs1_rel,
              TypeRel2_scalar_rel2_of_concretizes hs2_rel⟩

private theorem tnum_mask_rhs_of_concretizes {d : U64ScalarValueData} {x : BitVec 64}
    (hc : ConcretizesScalar64 d x) :
    x &&& ~~~d.unknown_mask.bv = d.value.bv &&& ~~~d.unknown_mask.bv := by
  have h2 := hc.2.1
  bv_decide

private theorem constraint_w64_eq_scalars_ok_valid
    {env : ExecEnv} {vCounter : Nat}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {data1 data2 : U64ScalarValueData}
    (hctx : CompCtxValid env vCounter context)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (h : (do
      let b ← U64ScalarValueData.is_fully_initialized data1
      if b then
        let b1 ← U64ScalarValueData.is_fully_initialized data2
        if b1 then
          let b2 ← U64ScalarValueData.can_be_equal data1 data2
          if b2 then
            let i ← U64ScalarValueData.min data1
            let i1 ← U64ScalarValueData.min data2
            let umin ← verifier.max_u64 i i1
            let i2 ← U64ScalarValueData.max data1
            let i3 ← U64ScalarValueData.max data2
            let umax ← verifier.min_u64 i2 i3
            let i4 ← lift (data1.value ||| data2.value)
            let i5 ← lift (data1.unknown_mask &&& data2.unknown_mask)
            let ur ← U64Range.new umin umax
            let usvd ← U64ScalarValueData.new i4 i5 0#u64 ur
            let t2 ← verifier.Type.Insts.CoreCloneClone.clone (verifier.Type.ScalarValue usvd)
            ok (core.result.Result.Ok (t2, verifier.Type.ScalarValue usvd), context)
          else ok (core.result.Result.Ok (type1, type2), context)
        else ok (core.result.Result.Ok (type1, type2), context)
      else ok (core.result.Result.Ok (type1, type2), context) :
        Result ((core.result.Result (verifier.Type × verifier.Type) error.VerifierErrorKind) ×
          verifier.ComputationContext)) =
      ok (.Ok (new1, new2), context1)) :
    context1.pc = context.pc ∧
    CompCtxValid env vCounter context1 ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 := by
  rcases bind_eq_ok h with ⟨b, _, h⟩
  cases b with
  | false =>
    simp only [Bool.false_eq_true, ↓reduceIte] at h
    rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨rfl, hctx, ht1, ht2⟩
  | true =>
    simp only [↓reduceIte] at h
    rcases bind_eq_ok h with ⟨b1, _, h⟩
    cases b1 with
    | false =>
      simp only [Bool.false_eq_true, ↓reduceIte] at h
      rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
      rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
      exact ⟨rfl, hctx, ht1, ht2⟩
    | true =>
      simp only [↓reduceIte] at h
      rcases bind_eq_ok h with ⟨b2, _, h⟩
      cases b2 with
      | false =>
        simp only [Bool.false_eq_true, ↓reduceIte] at h
        rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
        rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
        exact ⟨rfl, hctx, ht1, ht2⟩
      | true =>
        simp only [↓reduceIte] at h
        rcases bind_eq_ok h with ⟨i, _, h⟩
        rcases bind_eq_ok h with ⟨i1, _, h⟩
        rcases bind_eq_ok h with ⟨umin, _, h⟩
        rcases bind_eq_ok h with ⟨i2, _, h⟩
        rcases bind_eq_ok h with ⟨i3, _, h⟩
        rcases bind_eq_ok h with ⟨umax, _, h⟩
        simp only [lift, bind_ok] at h
        rcases bind_eq_ok h with ⟨ur, _, h⟩
        rw [bind_ok_eq (new64_eq_ok _ _ 0#u64 ur)] at h
        rw [bind_ok_eq (Type_clone_spec _)] at h
        rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
        rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
        exact ⟨rfl, hctx, pureNew64_valid _ _ _ _ _, pureNew64_valid _ _ _ _ _⟩

private theorem constraint_w64_eq_scalars_ok_sound
    {env : ExecEnv} {vCounter : Nat} {deps : verifier.DataDependencies}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {st₁ st₂ : MachineState} {l₁ : HostLayout}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {data1 data2 : U64ScalarValueData}
    {v1_1 v1_2 v2_1 v2_2 : RuntimeVal}
    (hctx : CompCtxValid env vCounter context)
    (hrel_ctx : StateRel2Deps env context deps ρ resActive isNullId st₁ st₂)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (hu1 : pureUnwrapReleasable type1 = .ScalarValue data1)
    (hu2 : pureUnwrapReleasable type2 = .ScalarValue data2)
    (hrel1 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type1 v1_1 v1_2)
    (hrel2 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type2 v2_1 v2_2)
    (hjt : JumpTypeHolds .Eq .W64
      (v1_1.toBits l₁ st₁.world.arrayLen) (v2_1.toBits l₁ st₁.world.arrayLen))
    (h : (do
      let b ← U64ScalarValueData.is_fully_initialized data1
      if b then
        let b1 ← U64ScalarValueData.is_fully_initialized data2
        if b1 then
          let b2 ← U64ScalarValueData.can_be_equal data1 data2
          if b2 then
            let i ← U64ScalarValueData.min data1
            let i1 ← U64ScalarValueData.min data2
            let umin ← verifier.max_u64 i i1
            let i2 ← U64ScalarValueData.max data1
            let i3 ← U64ScalarValueData.max data2
            let umax ← verifier.min_u64 i2 i3
            let i4 ← lift (data1.value ||| data2.value)
            let i5 ← lift (data1.unknown_mask &&& data2.unknown_mask)
            let ur ← U64Range.new umin umax
            let usvd ← U64ScalarValueData.new i4 i5 0#u64 ur
            let t2 ← verifier.Type.Insts.CoreCloneClone.clone (verifier.Type.ScalarValue usvd)
            ok (core.result.Result.Ok (t2, verifier.Type.ScalarValue usvd), context)
          else ok (core.result.Result.Ok (type1, type2), context)
        else ok (core.result.Result.Ok (type1, type2), context)
      else ok (core.result.Result.Ok (type1, type2), context) :
        Result ((core.result.Result (verifier.Type × verifier.Type) error.VerifierErrorKind) ×
          verifier.ComputationContext)) =
      ok (.Ok (new1, new2), context1)) :
    CompCtxValid env vCounter context1 ∧
    StateRel2Deps env context1 deps ρ resActive isNullId st₁ st₂ ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new1 v1_1 v1_2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new2 v2_1 v2_2 := by
  have hop1 := unwrap_eq_of_scalar ht1 hu1; subst hop1
  have hop2 := unwrap_eq_of_scalar ht2 hu2; subst hop2
  have hc_full : verifier.Type.constraint context .Eq .W64 (.ScalarValue data1) (.ScalarValue data2) =
      ok (.Ok (new1, new2), context1) := by
    rw [verifier.Type.constraint.eq_def]
    have hc1 : verifier.Type.check_not_released (.ScalarValue data1) context.resources = ok (.Ok ()) := by
      rw [verifier.Type.check_not_released.eq_def]
    have hc2 : verifier.Type.check_not_released (.ScalarValue data2) context.resources = ok (.Ok ()) := by
      rw [verifier.Type.check_not_released.eq_def]
    rw [bind_ok_eq hc1]; dsimp only
    rw [bind_ok_eq hc2]; dsimp only
    rw [bind_ok_eq (Type_unwrap_releasable_spec (.ScalarValue data1)),
        bind_ok_eq (Type_unwrap_releasable_spec (.ScalarValue data2))]
    exact h
  obtain ⟨rfl, hcases⟩ := Type_constraint_scalar_eq64_spec hc_full
  rcases hcases with ⟨rfl, rfl⟩ | ⟨usvd, hunw1, hne1, hunw2, hne2, rfl, rfl, husvd_v, _, _, _, hconc⟩
  · exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
  · simp only [TypeRel2, hne1, hne2, ↓reduceIte] at hrel1 hrel2
    rcases hrel1 with ⟨x₁, x₂, rfl, rfl, hx⟩
    rcases hrel2 with ⟨y₁, y₂, rfl, rfl, hy⟩
    have hx_eq := fully_initialized_eq_of_rel2_64 hunw1 hx; subst x₂
    have hy_eq := fully_initialized_eq_of_rel2_64 hunw2 hy; subst y₂
    change x₁ = y₁ at hjt; subst y₁
    have hc := hconc x₁ hx hy
    exact ⟨hctx, hrel_ctx, husvd_v, husvd_v,
      TypeRel2_scalar_rel2_of_concretizes hc,
      TypeRel2_scalar_rel2_of_concretizes hc⟩

private theorem constraint_w64_ge_scalars_ok_valid
    {env : ExecEnv} {vCounter : Nat}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {lhs rhs : U64ScalarValueData}
    (hctx : CompCtxValid env vCounter context)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (h : (do
      let b ← U64ScalarValueData.is_fully_initialized lhs
      if b then
        let b1 ← U64ScalarValueData.is_fully_initialized rhs
        if b1 then
          let i ← U64ScalarValueData.min rhs
          let i1 ← U64ScalarValueData.max lhs
          if i <= i1 then
            let i2 ← U64ScalarValueData.min lhs
            let new_min_lhs ← verifier.max_u64 i2 i
            let i3 ← U64ScalarValueData.max rhs
            let new_max_rhs ← verifier.min_u64 i3 i1
            let new_range_lhs ← U64Range.new new_min_lhs i1
            let new_range_rhs ← U64Range.new i new_max_rhs
            let o ← U64ScalarValueData.try_new lhs.value lhs.unknown_mask 0#u64 new_range_lhs
            let o1 ← U64ScalarValueData.try_new rhs.value rhs.unknown_mask 0#u64 new_range_rhs
            match o with
            | none => ok (core.result.Result.Ok (type1, type2), context)
            | some s1 =>
              match o1 with
              | none => ok (core.result.Result.Ok (type1, type2), context)
              | some s2 =>
                ok (core.result.Result.Ok (verifier.Type.ScalarValue s1,
                  verifier.Type.ScalarValue s2), context)
          else ok (core.result.Result.Ok (type1, type2), context)
        else ok (core.result.Result.Ok (type1, type2), context)
      else ok (core.result.Result.Ok (type1, type2), context) :
        Result ((core.result.Result (verifier.Type × verifier.Type) error.VerifierErrorKind) ×
          verifier.ComputationContext)) =
      ok (.Ok (new1, new2), context1)) :
    context1.pc = context.pc ∧
    CompCtxValid env vCounter context1 ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 := by
  rcases bind_eq_ok h with ⟨b, _, h⟩
  cases b with
  | false =>
    simp only [Bool.false_eq_true, ↓reduceIte] at h
    rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨rfl, hctx, ht1, ht2⟩
  | true =>
    simp only [↓reduceIte] at h
    rcases bind_eq_ok h with ⟨b1, _, h⟩
    cases b1 with
    | false =>
      simp only [Bool.false_eq_true, ↓reduceIte] at h
      rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
      rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
      exact ⟨rfl, hctx, ht1, ht2⟩
    | true =>
      simp only [↓reduceIte] at h
      rcases bind_eq_ok h with ⟨i, _, h⟩
      rcases bind_eq_ok h with ⟨i1, _, h⟩
      split_ifs at h with hle
      · rcases bind_eq_ok h with ⟨i2, _, h⟩
        rcases bind_eq_ok h with ⟨nml, _, h⟩
        rcases bind_eq_ok h with ⟨i3, _, h⟩
        rcases bind_eq_ok h with ⟨nmr, _, h⟩
        rcases bind_eq_ok h with ⟨nrl, _, h⟩
        rcases bind_eq_ok h with ⟨nrr, _, h⟩
        rcases bind_eq_ok h with ⟨o, ho, h⟩
        rcases bind_eq_ok h with ⟨o1, ho1, h⟩
        cases o with
        | none =>
          dsimp only at h
          rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
          rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
          exact ⟨rfl, hctx, ht1, ht2⟩
        | some s1 =>
          cases o1 with
          | none =>
            dsimp only at h
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            exact ⟨rfl, hctx, ht1, ht2⟩
          | some s2 =>
            dsimp only at h
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            exact ⟨rfl, hctx, try_new_64_valid ho, try_new_64_valid ho1⟩
      · rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
        rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
        exact ⟨rfl, hctx, ht1, ht2⟩

private theorem constraint_w64_ge_scalars_ok_sound
    {env : ExecEnv} {vCounter : Nat} {deps : verifier.DataDependencies}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {st₁ st₂ : MachineState} {l₁ : HostLayout}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {lhs rhs : U64ScalarValueData}
    {v1_1 v1_2 v2_1 v2_2 : RuntimeVal}
    (hctx : CompCtxValid env vCounter context)
    (hrel_ctx : StateRel2Deps env context deps ρ resActive isNullId st₁ st₂)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (hu1 : pureUnwrapReleasable type1 = .ScalarValue lhs)
    (hu2 : pureUnwrapReleasable type2 = .ScalarValue rhs)
    (hrel1 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type1 v1_1 v1_2)
    (hrel2 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type2 v2_1 v2_2)
    (hjt : JumpTypeHolds .Ge .W64
      (v1_1.toBits l₁ st₁.world.arrayLen) (v2_1.toBits l₁ st₁.world.arrayLen))
    (h : (do
      let b ← U64ScalarValueData.is_fully_initialized lhs
      if b then
        let b1 ← U64ScalarValueData.is_fully_initialized rhs
        if b1 then
          let i ← U64ScalarValueData.min rhs
          let i1 ← U64ScalarValueData.max lhs
          if i <= i1 then
            let i2 ← U64ScalarValueData.min lhs
            let new_min_lhs ← verifier.max_u64 i2 i
            let i3 ← U64ScalarValueData.max rhs
            let new_max_rhs ← verifier.min_u64 i3 i1
            let new_range_lhs ← U64Range.new new_min_lhs i1
            let new_range_rhs ← U64Range.new i new_max_rhs
            let o ← U64ScalarValueData.try_new lhs.value lhs.unknown_mask 0#u64 new_range_lhs
            let o1 ← U64ScalarValueData.try_new rhs.value rhs.unknown_mask 0#u64 new_range_rhs
            match o with
            | none => ok (core.result.Result.Ok (type1, type2), context)
            | some s1 =>
              match o1 with
              | none => ok (core.result.Result.Ok (type1, type2), context)
              | some s2 =>
                ok (core.result.Result.Ok (verifier.Type.ScalarValue s1,
                  verifier.Type.ScalarValue s2), context)
          else ok (core.result.Result.Ok (type1, type2), context)
        else ok (core.result.Result.Ok (type1, type2), context)
      else ok (core.result.Result.Ok (type1, type2), context) :
        Result ((core.result.Result (verifier.Type × verifier.Type) error.VerifierErrorKind) ×
          verifier.ComputationContext)) =
      ok (.Ok (new1, new2), context1)) :
    CompCtxValid env vCounter context1 ∧
    StateRel2Deps env context1 deps ρ resActive isNullId st₁ st₂ ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new1 v1_1 v1_2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new2 v2_1 v2_2 := by
  have hop1 := unwrap_eq_of_scalar ht1 hu1; subst hop1
  have hop2 := unwrap_eq_of_scalar ht2 hu2; subst hop2
  rcases bind_eq_ok h with ⟨b, hb, h⟩
  cases b with
  | false =>
    simp only [Bool.false_eq_true, ↓reduceIte] at h
    rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
  | true =>
    simp only [↓reduceIte] at h
    rcases bind_eq_ok h with ⟨b1, hb1, h⟩
    cases b1 with
    | false =>
      simp only [Bool.false_eq_true, ↓reduceIte] at h
      rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
      rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
      exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
    | true =>
      simp only [↓reduceIte] at h
      have ⟨hunw1, hne1⟩ := not_uninit_of_fully_init_64 hb
      have ⟨hunw2, hne2⟩ := not_uninit_of_fully_init_64 hb1
      have hrel1' := hrel1; simp only [TypeRel2, hne1, ↓reduceIte] at hrel1'
      have hrel2' := hrel2; simp only [TypeRel2, hne2, ↓reduceIte] at hrel2'
      rcases hrel1' with ⟨x₁, x₂, rfl, rfl, hx⟩
      rcases hrel2' with ⟨y₁, y₂, rfl, rfl, hy⟩
      have hx_eq := fully_initialized_eq_of_rel2_64 hunw1 hx; subst x₂
      have hy_eq := fully_initialized_eq_of_rel2_64 hunw2 hy; subst y₂
      change y₁ ≤ x₁ at hjt
      have hmin_l : U64ScalarValueData.min lhs = ok lhs.urange.min := rfl
      have hmax_l : U64ScalarValueData.max lhs = ok lhs.urange.max := rfl
      have hmin_r : U64ScalarValueData.min rhs = ok rhs.urange.min := rfl
      have hmax_r : U64ScalarValueData.max rhs = ok rhs.urange.max := rfl
      rw [bind_ok_eq hmin_r, bind_ok_eq hmax_l] at h
      split_ifs at h with hle
      · rw [bind_ok_eq hmin_l,
            bind_ok_eq (verifier_max_u64_eq_ok lhs.urange.min rhs.urange.min),
            bind_ok_eq hmax_r,
            bind_ok_eq (verifier_min_u64_eq_ok rhs.urange.max lhs.urange.max)] at h
        set nml_bv := if rhs.urange.min.bv ≤ lhs.urange.min.bv then lhs.urange.min.bv else rhs.urange.min.bv
        set nmr_bv := if rhs.urange.max.bv ≤ lhs.urange.max.bv then rhs.urange.max.bv else lhs.urange.max.bv
        set nrl : U64Range := { min := ⟨nml_bv⟩, max := lhs.urange.max }
        set nrr : U64Range := { min := rhs.urange.min, max := ⟨nmr_bv⟩ }
        have hnrl : U64Range.new ⟨nml_bv⟩ lhs.urange.max = ok nrl := rfl
        have hnrr : U64Range.new rhs.urange.min ⟨nmr_bv⟩ = ok nrr := rfl
        rw [bind_ok_eq hnrl, bind_ok_eq hnrr] at h
        rcases bind_eq_ok h with ⟨o, ho, h⟩
        rcases bind_eq_ok h with ⟨o1, ho1, h⟩
        cases o with
        | none =>
          dsimp only at h
          rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
          rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
          exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
        | some s1 =>
          cases o1 with
          | none =>
            dsimp only at h
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | some s2 =>
            dsimp only at h
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            have hrl : ConcretizesRange64 nrl x₁ := by
              have h1 := hx.1.2.2.1; have h2 := hx.1.2.2.2
              have h3 := hy.1.2.2.1
              refine ⟨?_, h2⟩
              show nml_bv ≤ x₁; dsimp [nml_bv]; split_ifs <;> bv_decide
            have hrr : ConcretizesRange64 nrr y₁ := by
              have h2 := hx.1.2.2.2
              have h3 := hy.1.2.2.1; have h4 := hy.1.2.2.2
              refine ⟨h3, ?_⟩
              show y₁ ≤ nmr_bv; dsimp [nmr_bv]; split_ifs <;> bv_decide
            have htl := tnum_mask_rhs_of_concretizes hx.1
            have htr := tnum_mask_rhs_of_concretizes hy.1
            have hs1_rel := try_new_64_some_sound_2safety ho hrl htl hrl htl rfl
            have hs2_rel := try_new_64_some_sound_2safety ho1 hrr htr hrr htr rfl
            exact ⟨hctx, hrel_ctx, try_new_64_valid ho, try_new_64_valid ho1,
              TypeRel2_scalar_rel2_of_concretizes hs1_rel,
              TypeRel2_scalar_rel2_of_concretizes hs2_rel⟩
      · rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
        rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
        exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩

private theorem constraint_w64_gt_scalars_ok_valid
    {env : ExecEnv} {vCounter : Nat}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {lhs rhs : U64ScalarValueData}
    (hctx : CompCtxValid env vCounter context)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (h : (do
      let b ← U64ScalarValueData.is_fully_initialized lhs
      if b then
        let b1 ← U64ScalarValueData.is_fully_initialized rhs
        if b1 then
          let i ← U64ScalarValueData.min rhs
          let i1 ← U64ScalarValueData.max lhs
          if i < i1 then
            let i2 ← U64ScalarValueData.min lhs
            let i3 ← i + 1#u64
            let new_min_lhs ← verifier.max_u64 i2 i3
            let i4 ← U64ScalarValueData.max rhs
            let i5 ← i1 - 1#u64
            let new_max_rhs ← verifier.min_u64 i4 i5
            let new_range_lhs ← U64Range.new new_min_lhs i1
            let new_range_rhs ← U64Range.new i new_max_rhs
            let o ← U64ScalarValueData.try_new lhs.value lhs.unknown_mask 0#u64 new_range_lhs
            let o1 ← U64ScalarValueData.try_new rhs.value rhs.unknown_mask 0#u64 new_range_rhs
            match o with
            | none => ok (core.result.Result.Ok (type1, type2), context)
            | some s1 =>
              match o1 with
              | none => ok (core.result.Result.Ok (type1, type2), context)
              | some s2 =>
                ok (core.result.Result.Ok (verifier.Type.ScalarValue s1,
                  verifier.Type.ScalarValue s2), context)
          else ok (core.result.Result.Ok (type1, type2), context)
        else ok (core.result.Result.Ok (type1, type2), context)
      else ok (core.result.Result.Ok (type1, type2), context) :
        Result ((core.result.Result (verifier.Type × verifier.Type) error.VerifierErrorKind) ×
          verifier.ComputationContext)) =
      ok (.Ok (new1, new2), context1)) :
    context1.pc = context.pc ∧
    CompCtxValid env vCounter context1 ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 := by
  rcases bind_eq_ok h with ⟨b, _, h⟩
  cases b with
  | false =>
    simp only [Bool.false_eq_true, ↓reduceIte] at h
    rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨rfl, hctx, ht1, ht2⟩
  | true =>
    simp only [↓reduceIte] at h
    rcases bind_eq_ok h with ⟨b1, _, h⟩
    cases b1 with
    | false =>
      simp only [Bool.false_eq_true, ↓reduceIte] at h
      rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
      rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
      exact ⟨rfl, hctx, ht1, ht2⟩
    | true =>
      simp only [↓reduceIte] at h
      rcases bind_eq_ok h with ⟨i, _, h⟩
      rcases bind_eq_ok h with ⟨i1, _, h⟩
      split_ifs at h with hlt
      · rcases bind_eq_ok h with ⟨i2, _, h⟩
        rcases bind_eq_ok h with ⟨i3, _, h⟩
        rcases bind_eq_ok h with ⟨nml, _, h⟩
        rcases bind_eq_ok h with ⟨i4, _, h⟩
        rcases bind_eq_ok h with ⟨i5, _, h⟩
        rcases bind_eq_ok h with ⟨nmr, _, h⟩
        rcases bind_eq_ok h with ⟨nrl, _, h⟩
        rcases bind_eq_ok h with ⟨nrr, _, h⟩
        rcases bind_eq_ok h with ⟨o, ho, h⟩
        rcases bind_eq_ok h with ⟨o1, ho1, h⟩
        cases o with
        | none =>
          dsimp only at h
          rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
          rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
          exact ⟨rfl, hctx, ht1, ht2⟩
        | some s1 =>
          cases o1 with
          | none =>
            dsimp only at h
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            exact ⟨rfl, hctx, ht1, ht2⟩
          | some s2 =>
            dsimp only at h
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            exact ⟨rfl, hctx, try_new_64_valid ho, try_new_64_valid ho1⟩
      · rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
        rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
        exact ⟨rfl, hctx, ht1, ht2⟩

private theorem constraint_w64_gt_scalars_ok_sound
    {env : ExecEnv} {vCounter : Nat} {deps : verifier.DataDependencies}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {st₁ st₂ : MachineState} {l₁ : HostLayout}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {lhs rhs : U64ScalarValueData}
    {v1_1 v1_2 v2_1 v2_2 : RuntimeVal}
    (hctx : CompCtxValid env vCounter context)
    (hrel_ctx : StateRel2Deps env context deps ρ resActive isNullId st₁ st₂)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (hu1 : pureUnwrapReleasable type1 = .ScalarValue lhs)
    (hu2 : pureUnwrapReleasable type2 = .ScalarValue rhs)
    (hrel1 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type1 v1_1 v1_2)
    (hrel2 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type2 v2_1 v2_2)
    (hjt : JumpTypeHolds .Gt .W64
      (v1_1.toBits l₁ st₁.world.arrayLen) (v2_1.toBits l₁ st₁.world.arrayLen))
    (h : (do
      let b ← U64ScalarValueData.is_fully_initialized lhs
      if b then
        let b1 ← U64ScalarValueData.is_fully_initialized rhs
        if b1 then
          let i ← U64ScalarValueData.min rhs
          let i1 ← U64ScalarValueData.max lhs
          if i < i1 then
            let i2 ← U64ScalarValueData.min lhs
            let i3 ← i + 1#u64
            let new_min_lhs ← verifier.max_u64 i2 i3
            let i4 ← U64ScalarValueData.max rhs
            let i5 ← i1 - 1#u64
            let new_max_rhs ← verifier.min_u64 i4 i5
            let new_range_lhs ← U64Range.new new_min_lhs i1
            let new_range_rhs ← U64Range.new i new_max_rhs
            let o ← U64ScalarValueData.try_new lhs.value lhs.unknown_mask 0#u64 new_range_lhs
            let o1 ← U64ScalarValueData.try_new rhs.value rhs.unknown_mask 0#u64 new_range_rhs
            match o with
            | none => ok (core.result.Result.Ok (type1, type2), context)
            | some s1 =>
              match o1 with
              | none => ok (core.result.Result.Ok (type1, type2), context)
              | some s2 =>
                ok (core.result.Result.Ok (verifier.Type.ScalarValue s1,
                  verifier.Type.ScalarValue s2), context)
          else ok (core.result.Result.Ok (type1, type2), context)
        else ok (core.result.Result.Ok (type1, type2), context)
      else ok (core.result.Result.Ok (type1, type2), context) :
        Result ((core.result.Result (verifier.Type × verifier.Type) error.VerifierErrorKind) ×
          verifier.ComputationContext)) =
      ok (.Ok (new1, new2), context1)) :
    CompCtxValid env vCounter context1 ∧
    StateRel2Deps env context1 deps ρ resActive isNullId st₁ st₂ ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new1 v1_1 v1_2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new2 v2_1 v2_2 := by
  have hop1 := unwrap_eq_of_scalar ht1 hu1; subst hop1
  have hop2 := unwrap_eq_of_scalar ht2 hu2; subst hop2
  rcases bind_eq_ok h with ⟨b, hb, h⟩
  cases b with
  | false =>
    simp only [Bool.false_eq_true, ↓reduceIte] at h
    rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
  | true =>
    simp only [↓reduceIte] at h
    rcases bind_eq_ok h with ⟨b1, hb1, h⟩
    cases b1 with
    | false =>
      simp only [Bool.false_eq_true, ↓reduceIte] at h
      rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
      rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
      exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
    | true =>
      simp only [↓reduceIte] at h
      have ⟨hunw1, hne1⟩ := not_uninit_of_fully_init_64 hb
      have ⟨hunw2, hne2⟩ := not_uninit_of_fully_init_64 hb1
      have hrel1' := hrel1; simp only [TypeRel2, hne1, ↓reduceIte] at hrel1'
      have hrel2' := hrel2; simp only [TypeRel2, hne2, ↓reduceIte] at hrel2'
      rcases hrel1' with ⟨x₁, x₂, rfl, rfl, hx⟩
      rcases hrel2' with ⟨y₁, y₂, rfl, rfl, hy⟩
      have hx_eq := fully_initialized_eq_of_rel2_64 hunw1 hx; subst x₂
      have hy_eq := fully_initialized_eq_of_rel2_64 hunw2 hy; subst y₂
      change y₁ < x₁ at hjt
      have hmin_l : U64ScalarValueData.min lhs = ok lhs.urange.min := rfl
      have hmax_l : U64ScalarValueData.max lhs = ok lhs.urange.max := rfl
      have hmin_r : U64ScalarValueData.min rhs = ok rhs.urange.min := rfl
      have hmax_r : U64ScalarValueData.max rhs = ok rhs.urange.max := rfl
      rw [bind_ok_eq hmin_r, bind_ok_eq hmax_l] at h
      split_ifs at h with hlt
      · have hi_le : rhs.urange.min.val + (1#u64).val ≤ UScalar.max .U64 := by
          have hb_max := lhs.urange.max.hmax
          have hlt_val : rhs.urange.min.val < lhs.urange.max.val := hlt
          have h1 : (1#u64).val = 1 := rfl
          simp [U64.max, U64.numBits] at hb_max ⊢
          omega
        have hi_add := uscalar_add_eq_ok rhs.urange.min 1#u64 hi_le
        simp only [u64_one_uscalar_bv] at hi_add
        have hi1_ge : (1#u64).val ≤ lhs.urange.max.val := by
          have hlt_val : rhs.urange.min.val < lhs.urange.max.val := hlt
          have h1 : (1#u64).val = 1 := rfl
          omega
        have hi1_sub := uscalar_sub_eq_ok lhs.urange.max 1#u64 hi1_ge
        simp only [u64_one_uscalar_bv] at hi1_sub
        rw [bind_ok_eq hmin_l, bind_ok_eq hi_add,
            bind_ok_eq (verifier_max_u64_eq_ok lhs.urange.min ⟨rhs.urange.min.bv + 1#64⟩),
            bind_ok_eq hmax_r, bind_ok_eq hi1_sub,
            bind_ok_eq (verifier_min_u64_eq_ok rhs.urange.max ⟨lhs.urange.max.bv - 1#64⟩)] at h
        set nml_bv := if rhs.urange.min.bv + 1#64 ≤ lhs.urange.min.bv then lhs.urange.min.bv else rhs.urange.min.bv + 1#64
        set nmr_bv := if rhs.urange.max.bv ≤ lhs.urange.max.bv - 1#64 then rhs.urange.max.bv else lhs.urange.max.bv - 1#64
        set nrl : U64Range := { min := ⟨nml_bv⟩, max := lhs.urange.max }
        set nrr : U64Range := { min := rhs.urange.min, max := ⟨nmr_bv⟩ }
        have hnrl : U64Range.new ⟨nml_bv⟩ lhs.urange.max = ok nrl := rfl
        have hnrr : U64Range.new rhs.urange.min ⟨nmr_bv⟩ = ok nrr := rfl
        rw [bind_ok_eq hnrl, bind_ok_eq hnrr] at h
        rcases bind_eq_ok h with ⟨o, ho, h⟩
        rcases bind_eq_ok h with ⟨o1, ho1, h⟩
        cases o with
        | none =>
          dsimp only at h
          rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
          rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
          exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
        | some s1 =>
          cases o1 with
          | none =>
            dsimp only at h
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | some s2 =>
            dsimp only at h
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            have hlt_bv : rhs.urange.min.bv < lhs.urange.max.bv := (u64_lt_iff_bv _ _).mp hlt
            have hrl : ConcretizesRange64 nrl x₁ := by
              have h1 := hx.1.2.2.1; have h2 := hx.1.2.2.2
              have h3 := hy.1.2.2.1
              refine ⟨?_, h2⟩
              show nml_bv ≤ x₁; dsimp [nml_bv]; split_ifs <;> bv_decide
            have hrr : ConcretizesRange64 nrr y₁ := by
              have h2 := hx.1.2.2.2
              have h3 := hy.1.2.2.1; have h4 := hy.1.2.2.2
              refine ⟨h3, ?_⟩
              show y₁ ≤ nmr_bv; dsimp [nmr_bv]; split_ifs <;> bv_decide
            have htl := tnum_mask_rhs_of_concretizes hx.1
            have htr := tnum_mask_rhs_of_concretizes hy.1
            have hs1_rel := try_new_64_some_sound_2safety ho hrl htl hrl htl rfl
            have hs2_rel := try_new_64_some_sound_2safety ho1 hrr htr hrr htr rfl
            exact ⟨hctx, hrel_ctx, try_new_64_valid ho, try_new_64_valid ho1,
              TypeRel2_scalar_rel2_of_concretizes hs1_rel,
              TypeRel2_scalar_rel2_of_concretizes hs2_rel⟩
      · rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
        rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
        exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩

private theorem constraint_w64_le_scalars_ok_valid
    {env : ExecEnv} {vCounter : Nat}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {lhs rhs : U64ScalarValueData}
    (hctx : CompCtxValid env vCounter context)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (h : (do
      let b ← U64ScalarValueData.is_fully_initialized lhs
      if b then
        let b1 ← U64ScalarValueData.is_fully_initialized rhs
        if b1 then
          let i ← U64ScalarValueData.min lhs
          let i1 ← U64ScalarValueData.max rhs
          if i <= i1 then
            let i2 ← U64ScalarValueData.max lhs
            let new_max_lhs ← verifier.min_u64 i2 i1
            let i3 ← U64ScalarValueData.min rhs
            let new_min_rhs ← verifier.max_u64 i3 i
            let new_range_lhs ← U64Range.new i new_max_lhs
            let new_range_rhs ← U64Range.new new_min_rhs i1
            let o ← U64ScalarValueData.try_new lhs.value lhs.unknown_mask 0#u64 new_range_lhs
            let o1 ← U64ScalarValueData.try_new rhs.value rhs.unknown_mask 0#u64 new_range_rhs
            match o with
            | none => ok (core.result.Result.Ok (type1, type2), context)
            | some s1 =>
              match o1 with
              | none => ok (core.result.Result.Ok (type1, type2), context)
              | some s2 =>
                ok (core.result.Result.Ok (verifier.Type.ScalarValue s1,
                  verifier.Type.ScalarValue s2), context)
          else ok (core.result.Result.Ok (type1, type2), context)
        else ok (core.result.Result.Ok (type1, type2), context)
      else ok (core.result.Result.Ok (type1, type2), context) :
        Result ((core.result.Result (verifier.Type × verifier.Type) error.VerifierErrorKind) ×
          verifier.ComputationContext)) =
      ok (.Ok (new1, new2), context1)) :
    context1.pc = context.pc ∧
    CompCtxValid env vCounter context1 ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 := by
  rcases bind_eq_ok h with ⟨b, _, h⟩
  cases b with
  | false =>
    simp only [Bool.false_eq_true, ↓reduceIte] at h
    rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨rfl, hctx, ht1, ht2⟩
  | true =>
    simp only [↓reduceIte] at h
    rcases bind_eq_ok h with ⟨b1, _, h⟩
    cases b1 with
    | false =>
      simp only [Bool.false_eq_true, ↓reduceIte] at h
      rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
      rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
      exact ⟨rfl, hctx, ht1, ht2⟩
    | true =>
      simp only [↓reduceIte] at h
      rcases bind_eq_ok h with ⟨i, _, h⟩
      rcases bind_eq_ok h with ⟨i1, _, h⟩
      split_ifs at h with hle
      · rcases bind_eq_ok h with ⟨i2, _, h⟩
        rcases bind_eq_ok h with ⟨nml, _, h⟩
        rcases bind_eq_ok h with ⟨i3, _, h⟩
        rcases bind_eq_ok h with ⟨nmr, _, h⟩
        rcases bind_eq_ok h with ⟨nrl, _, h⟩
        rcases bind_eq_ok h with ⟨nrr, _, h⟩
        rcases bind_eq_ok h with ⟨o, ho, h⟩
        rcases bind_eq_ok h with ⟨o1, ho1, h⟩
        cases o with
        | none =>
          dsimp only at h
          rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
          rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
          exact ⟨rfl, hctx, ht1, ht2⟩
        | some s1 =>
          cases o1 with
          | none =>
            dsimp only at h
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            exact ⟨rfl, hctx, ht1, ht2⟩
          | some s2 =>
            dsimp only at h
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            exact ⟨rfl, hctx, try_new_64_valid ho, try_new_64_valid ho1⟩
      · rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
        rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
        exact ⟨rfl, hctx, ht1, ht2⟩

private theorem constraint_w64_le_scalars_ok_sound
    {env : ExecEnv} {vCounter : Nat} {deps : verifier.DataDependencies}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {st₁ st₂ : MachineState} {l₁ : HostLayout}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {lhs rhs : U64ScalarValueData}
    {v1_1 v1_2 v2_1 v2_2 : RuntimeVal}
    (hctx : CompCtxValid env vCounter context)
    (hrel_ctx : StateRel2Deps env context deps ρ resActive isNullId st₁ st₂)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (hu1 : pureUnwrapReleasable type1 = .ScalarValue lhs)
    (hu2 : pureUnwrapReleasable type2 = .ScalarValue rhs)
    (hrel1 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type1 v1_1 v1_2)
    (hrel2 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type2 v2_1 v2_2)
    (hjt : JumpTypeHolds .Le .W64
      (v1_1.toBits l₁ st₁.world.arrayLen) (v2_1.toBits l₁ st₁.world.arrayLen))
    (h : (do
      let b ← U64ScalarValueData.is_fully_initialized lhs
      if b then
        let b1 ← U64ScalarValueData.is_fully_initialized rhs
        if b1 then
          let i ← U64ScalarValueData.min lhs
          let i1 ← U64ScalarValueData.max rhs
          if i <= i1 then
            let i2 ← U64ScalarValueData.max lhs
            let new_max_lhs ← verifier.min_u64 i2 i1
            let i3 ← U64ScalarValueData.min rhs
            let new_min_rhs ← verifier.max_u64 i3 i
            let new_range_lhs ← U64Range.new i new_max_lhs
            let new_range_rhs ← U64Range.new new_min_rhs i1
            let o ← U64ScalarValueData.try_new lhs.value lhs.unknown_mask 0#u64 new_range_lhs
            let o1 ← U64ScalarValueData.try_new rhs.value rhs.unknown_mask 0#u64 new_range_rhs
            match o with
            | none => ok (core.result.Result.Ok (type1, type2), context)
            | some s1 =>
              match o1 with
              | none => ok (core.result.Result.Ok (type1, type2), context)
              | some s2 =>
                ok (core.result.Result.Ok (verifier.Type.ScalarValue s1,
                  verifier.Type.ScalarValue s2), context)
          else ok (core.result.Result.Ok (type1, type2), context)
        else ok (core.result.Result.Ok (type1, type2), context)
      else ok (core.result.Result.Ok (type1, type2), context) :
        Result ((core.result.Result (verifier.Type × verifier.Type) error.VerifierErrorKind) ×
          verifier.ComputationContext)) =
      ok (.Ok (new1, new2), context1)) :
    CompCtxValid env vCounter context1 ∧
    StateRel2Deps env context1 deps ρ resActive isNullId st₁ st₂ ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new1 v1_1 v1_2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new2 v2_1 v2_2 := by
  have hop1 := unwrap_eq_of_scalar ht1 hu1; subst hop1
  have hop2 := unwrap_eq_of_scalar ht2 hu2; subst hop2
  rcases bind_eq_ok h with ⟨b, hb, h⟩
  cases b with
  | false =>
    simp only [Bool.false_eq_true, ↓reduceIte] at h
    rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
  | true =>
    simp only [↓reduceIte] at h
    rcases bind_eq_ok h with ⟨b1, hb1, h⟩
    cases b1 with
    | false =>
      simp only [Bool.false_eq_true, ↓reduceIte] at h
      rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
      rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
      exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
    | true =>
      simp only [↓reduceIte] at h
      have ⟨hunw1, hne1⟩ := not_uninit_of_fully_init_64 hb
      have ⟨hunw2, hne2⟩ := not_uninit_of_fully_init_64 hb1
      have hrel1' := hrel1; simp only [TypeRel2, hne1, ↓reduceIte] at hrel1'
      have hrel2' := hrel2; simp only [TypeRel2, hne2, ↓reduceIte] at hrel2'
      rcases hrel1' with ⟨x₁, x₂, rfl, rfl, hx⟩
      rcases hrel2' with ⟨y₁, y₂, rfl, rfl, hy⟩
      have hx_eq := fully_initialized_eq_of_rel2_64 hunw1 hx; subst x₂
      have hy_eq := fully_initialized_eq_of_rel2_64 hunw2 hy; subst y₂
      change x₁ ≤ y₁ at hjt
      have hmin_l : U64ScalarValueData.min lhs = ok lhs.urange.min := rfl
      have hmax_l : U64ScalarValueData.max lhs = ok lhs.urange.max := rfl
      have hmin_r : U64ScalarValueData.min rhs = ok rhs.urange.min := rfl
      have hmax_r : U64ScalarValueData.max rhs = ok rhs.urange.max := rfl
      rw [bind_ok_eq hmin_l, bind_ok_eq hmax_r] at h
      split_ifs at h with hle
      · rw [bind_ok_eq hmax_l,
            bind_ok_eq (verifier_min_u64_eq_ok lhs.urange.max rhs.urange.max),
            bind_ok_eq hmin_r,
            bind_ok_eq (verifier_max_u64_eq_ok rhs.urange.min lhs.urange.min)] at h
        set nml_bv := if lhs.urange.max.bv ≤ rhs.urange.max.bv then lhs.urange.max.bv else rhs.urange.max.bv
        set nmr_bv := if lhs.urange.min.bv ≤ rhs.urange.min.bv then rhs.urange.min.bv else lhs.urange.min.bv
        set nrl : U64Range := { min := lhs.urange.min, max := ⟨nml_bv⟩ }
        set nrr : U64Range := { min := ⟨nmr_bv⟩, max := rhs.urange.max }
        have hnrl : U64Range.new lhs.urange.min ⟨nml_bv⟩ = ok nrl := rfl
        have hnrr : U64Range.new ⟨nmr_bv⟩ rhs.urange.max = ok nrr := rfl
        rw [bind_ok_eq hnrl, bind_ok_eq hnrr] at h
        rcases bind_eq_ok h with ⟨o, ho, h⟩
        rcases bind_eq_ok h with ⟨o1, ho1, h⟩
        cases o with
        | none =>
          dsimp only at h
          rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
          rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
          exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
        | some s1 =>
          cases o1 with
          | none =>
            dsimp only at h
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | some s2 =>
            dsimp only at h
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            have hrl : ConcretizesRange64 nrl x₁ := by
              have h1 := hx.1.2.2.1; have h2 := hx.1.2.2.2
              have h4 := hy.1.2.2.2
              refine ⟨h1, ?_⟩
              show x₁ ≤ nml_bv; dsimp [nml_bv]; split_ifs <;> bv_decide
            have hrr : ConcretizesRange64 nrr y₁ := by
              have h1 := hx.1.2.2.1
              have h3 := hy.1.2.2.1; have h4 := hy.1.2.2.2
              refine ⟨?_, h4⟩
              show nmr_bv ≤ y₁; dsimp [nmr_bv]; split_ifs <;> bv_decide
            have htl := tnum_mask_rhs_of_concretizes hx.1
            have htr := tnum_mask_rhs_of_concretizes hy.1
            have hs1_rel := try_new_64_some_sound_2safety ho hrl htl hrl htl rfl
            have hs2_rel := try_new_64_some_sound_2safety ho1 hrr htr hrr htr rfl
            exact ⟨hctx, hrel_ctx, try_new_64_valid ho, try_new_64_valid ho1,
              TypeRel2_scalar_rel2_of_concretizes hs1_rel,
              TypeRel2_scalar_rel2_of_concretizes hs2_rel⟩
      · rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
        rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
        exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩

private theorem constraint_w64_lt_scalars_ok_valid
    {env : ExecEnv} {vCounter : Nat}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {lhs rhs : U64ScalarValueData}
    (hctx : CompCtxValid env vCounter context)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (h : (do
      let b ← U64ScalarValueData.is_fully_initialized lhs
      if b then
        let b1 ← U64ScalarValueData.is_fully_initialized rhs
        if b1 then
          let i ← U64ScalarValueData.min lhs
          let i1 ← U64ScalarValueData.max rhs
          if i < i1 then
            let i2 ← U64ScalarValueData.max lhs
            let i3 ← i1 - 1#u64
            let new_max_lhs ← verifier.min_u64 i2 i3
            let i4 ← U64ScalarValueData.min rhs
            let i5 ← i + 1#u64
            let new_min_rhs ← verifier.max_u64 i4 i5
            let new_range_lhs ← U64Range.new i new_max_lhs
            let new_range_rhs ← U64Range.new new_min_rhs i1
            let o ← U64ScalarValueData.try_new lhs.value lhs.unknown_mask 0#u64 new_range_lhs
            let o1 ← U64ScalarValueData.try_new rhs.value rhs.unknown_mask 0#u64 new_range_rhs
            match o with
            | none => ok (core.result.Result.Ok (type1, type2), context)
            | some s1 =>
              match o1 with
              | none => ok (core.result.Result.Ok (type1, type2), context)
              | some s2 =>
                ok (core.result.Result.Ok (verifier.Type.ScalarValue s1,
                  verifier.Type.ScalarValue s2), context)
          else ok (core.result.Result.Ok (type1, type2), context)
        else ok (core.result.Result.Ok (type1, type2), context)
      else ok (core.result.Result.Ok (type1, type2), context) :
        Result ((core.result.Result (verifier.Type × verifier.Type) error.VerifierErrorKind) ×
          verifier.ComputationContext)) =
      ok (.Ok (new1, new2), context1)) :
    context1.pc = context.pc ∧
    CompCtxValid env vCounter context1 ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 := by
  rcases bind_eq_ok h with ⟨b, _, h⟩
  cases b with
  | false =>
    simp only [Bool.false_eq_true, ↓reduceIte] at h
    rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨rfl, hctx, ht1, ht2⟩
  | true =>
    simp only [↓reduceIte] at h
    rcases bind_eq_ok h with ⟨b1, _, h⟩
    cases b1 with
    | false =>
      simp only [Bool.false_eq_true, ↓reduceIte] at h
      rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
      rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
      exact ⟨rfl, hctx, ht1, ht2⟩
    | true =>
      simp only [↓reduceIte] at h
      rcases bind_eq_ok h with ⟨i, _, h⟩
      rcases bind_eq_ok h with ⟨i1, _, h⟩
      split_ifs at h with hlt
      · rcases bind_eq_ok h with ⟨i2, _, h⟩
        rcases bind_eq_ok h with ⟨i3, _, h⟩
        rcases bind_eq_ok h with ⟨nml, _, h⟩
        rcases bind_eq_ok h with ⟨i4, _, h⟩
        rcases bind_eq_ok h with ⟨i5, _, h⟩
        rcases bind_eq_ok h with ⟨nmr, _, h⟩
        rcases bind_eq_ok h with ⟨nrl, _, h⟩
        rcases bind_eq_ok h with ⟨nrr, _, h⟩
        rcases bind_eq_ok h with ⟨o, ho, h⟩
        rcases bind_eq_ok h with ⟨o1, ho1, h⟩
        cases o with
        | none =>
          dsimp only at h
          rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
          rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
          exact ⟨rfl, hctx, ht1, ht2⟩
        | some s1 =>
          cases o1 with
          | none =>
            dsimp only at h
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            exact ⟨rfl, hctx, ht1, ht2⟩
          | some s2 =>
            dsimp only at h
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            exact ⟨rfl, hctx, try_new_64_valid ho, try_new_64_valid ho1⟩
      · rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
        rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
        exact ⟨rfl, hctx, ht1, ht2⟩

private theorem constraint_w64_lt_scalars_ok_sound
    {env : ExecEnv} {vCounter : Nat} {deps : verifier.DataDependencies}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {st₁ st₂ : MachineState} {l₁ : HostLayout}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {lhs rhs : U64ScalarValueData}
    {v1_1 v1_2 v2_1 v2_2 : RuntimeVal}
    (hctx : CompCtxValid env vCounter context)
    (hrel_ctx : StateRel2Deps env context deps ρ resActive isNullId st₁ st₂)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (hu1 : pureUnwrapReleasable type1 = .ScalarValue lhs)
    (hu2 : pureUnwrapReleasable type2 = .ScalarValue rhs)
    (hrel1 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type1 v1_1 v1_2)
    (hrel2 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type2 v2_1 v2_2)
    (hjt : JumpTypeHolds .Lt .W64
      (v1_1.toBits l₁ st₁.world.arrayLen) (v2_1.toBits l₁ st₁.world.arrayLen))
    (h : (do
      let b ← U64ScalarValueData.is_fully_initialized lhs
      if b then
        let b1 ← U64ScalarValueData.is_fully_initialized rhs
        if b1 then
          let i ← U64ScalarValueData.min lhs
          let i1 ← U64ScalarValueData.max rhs
          if i < i1 then
            let i2 ← U64ScalarValueData.max lhs
            let i3 ← i1 - 1#u64
            let new_max_lhs ← verifier.min_u64 i2 i3
            let i4 ← U64ScalarValueData.min rhs
            let i5 ← i + 1#u64
            let new_min_rhs ← verifier.max_u64 i4 i5
            let new_range_lhs ← U64Range.new i new_max_lhs
            let new_range_rhs ← U64Range.new new_min_rhs i1
            let o ← U64ScalarValueData.try_new lhs.value lhs.unknown_mask 0#u64 new_range_lhs
            let o1 ← U64ScalarValueData.try_new rhs.value rhs.unknown_mask 0#u64 new_range_rhs
            match o with
            | none => ok (core.result.Result.Ok (type1, type2), context)
            | some s1 =>
              match o1 with
              | none => ok (core.result.Result.Ok (type1, type2), context)
              | some s2 =>
                ok (core.result.Result.Ok (verifier.Type.ScalarValue s1,
                  verifier.Type.ScalarValue s2), context)
          else ok (core.result.Result.Ok (type1, type2), context)
        else ok (core.result.Result.Ok (type1, type2), context)
      else ok (core.result.Result.Ok (type1, type2), context) :
        Result ((core.result.Result (verifier.Type × verifier.Type) error.VerifierErrorKind) ×
          verifier.ComputationContext)) =
      ok (.Ok (new1, new2), context1)) :
    CompCtxValid env vCounter context1 ∧
    StateRel2Deps env context1 deps ρ resActive isNullId st₁ st₂ ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new1 v1_1 v1_2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new2 v2_1 v2_2 := by
  have hop1 := unwrap_eq_of_scalar ht1 hu1; subst hop1
  have hop2 := unwrap_eq_of_scalar ht2 hu2; subst hop2
  rcases bind_eq_ok h with ⟨b, hb, h⟩
  cases b with
  | false =>
    simp only [Bool.false_eq_true, ↓reduceIte] at h
    rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
  | true =>
    simp only [↓reduceIte] at h
    rcases bind_eq_ok h with ⟨b1, hb1, h⟩
    cases b1 with
    | false =>
      simp only [Bool.false_eq_true, ↓reduceIte] at h
      rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
      rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
      exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
    | true =>
      simp only [↓reduceIte] at h
      have ⟨hunw1, hne1⟩ := not_uninit_of_fully_init_64 hb
      have ⟨hunw2, hne2⟩ := not_uninit_of_fully_init_64 hb1
      have hrel1' := hrel1; simp only [TypeRel2, hne1, ↓reduceIte] at hrel1'
      have hrel2' := hrel2; simp only [TypeRel2, hne2, ↓reduceIte] at hrel2'
      rcases hrel1' with ⟨x₁, x₂, rfl, rfl, hx⟩
      rcases hrel2' with ⟨y₁, y₂, rfl, rfl, hy⟩
      have hx_eq := fully_initialized_eq_of_rel2_64 hunw1 hx; subst x₂
      have hy_eq := fully_initialized_eq_of_rel2_64 hunw2 hy; subst y₂
      change x₁ < y₁ at hjt
      have hmin_l : U64ScalarValueData.min lhs = ok lhs.urange.min := rfl
      have hmax_l : U64ScalarValueData.max lhs = ok lhs.urange.max := rfl
      have hmin_r : U64ScalarValueData.min rhs = ok rhs.urange.min := rfl
      have hmax_r : U64ScalarValueData.max rhs = ok rhs.urange.max := rfl
      rw [bind_ok_eq hmin_l, bind_ok_eq hmax_r] at h
      split_ifs at h with hlt
      · have hi_le : lhs.urange.min.val + (1#u64).val ≤ UScalar.max .U64 := by
          have hb_max := rhs.urange.max.hmax
          have hlt_val : lhs.urange.min.val < rhs.urange.max.val := hlt
          have h1 : (1#u64).val = 1 := rfl
          simp [U64.max, U64.numBits] at hb_max ⊢
          omega
        have hi_add := uscalar_add_eq_ok lhs.urange.min 1#u64 hi_le
        simp only [u64_one_uscalar_bv] at hi_add
        have hi1_ge : (1#u64).val ≤ rhs.urange.max.val := by
          have hlt_val : lhs.urange.min.val < rhs.urange.max.val := hlt
          have h1 : (1#u64).val = 1 := rfl
          omega
        have hi1_sub := uscalar_sub_eq_ok rhs.urange.max 1#u64 hi1_ge
        simp only [u64_one_uscalar_bv] at hi1_sub
        rw [bind_ok_eq hmax_l, bind_ok_eq hi1_sub,
            bind_ok_eq (verifier_min_u64_eq_ok lhs.urange.max ⟨rhs.urange.max.bv - 1#64⟩),
            bind_ok_eq hmin_r, bind_ok_eq hi_add,
            bind_ok_eq (verifier_max_u64_eq_ok rhs.urange.min ⟨lhs.urange.min.bv + 1#64⟩)] at h
        set nml_bv := if lhs.urange.max.bv ≤ rhs.urange.max.bv - 1#64 then lhs.urange.max.bv else rhs.urange.max.bv - 1#64
        set nmr_bv := if lhs.urange.min.bv + 1#64 ≤ rhs.urange.min.bv then rhs.urange.min.bv else lhs.urange.min.bv + 1#64
        set nrl : U64Range := { min := lhs.urange.min, max := ⟨nml_bv⟩ }
        set nrr : U64Range := { min := ⟨nmr_bv⟩, max := rhs.urange.max }
        have hnrl : U64Range.new lhs.urange.min ⟨nml_bv⟩ = ok nrl := rfl
        have hnrr : U64Range.new ⟨nmr_bv⟩ rhs.urange.max = ok nrr := rfl
        rw [bind_ok_eq hnrl, bind_ok_eq hnrr] at h
        rcases bind_eq_ok h with ⟨o, ho, h⟩
        rcases bind_eq_ok h with ⟨o1, ho1, h⟩
        cases o with
        | none =>
          dsimp only at h
          rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
          rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
          exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
        | some s1 =>
          cases o1 with
          | none =>
            dsimp only at h
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | some s2 =>
            dsimp only at h
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            have hlt_bv : lhs.urange.min.bv < rhs.urange.max.bv := (u64_lt_iff_bv _ _).mp hlt
            have hrl : ConcretizesRange64 nrl x₁ := by
              have h1 := hx.1.2.2.1; have h2 := hx.1.2.2.2
              have h4 := hy.1.2.2.2
              refine ⟨h1, ?_⟩
              show x₁ ≤ nml_bv; dsimp [nml_bv]; split_ifs <;> bv_decide
            have hrr : ConcretizesRange64 nrr y₁ := by
              have h1 := hx.1.2.2.1
              have h3 := hy.1.2.2.1; have h4 := hy.1.2.2.2
              refine ⟨?_, h4⟩
              show nmr_bv ≤ y₁; dsimp [nmr_bv]; split_ifs <;> bv_decide
            have htl := tnum_mask_rhs_of_concretizes hx.1
            have htr := tnum_mask_rhs_of_concretizes hy.1
            have hs1_rel := try_new_64_some_sound_2safety ho hrl htl hrl htl rfl
            have hs2_rel := try_new_64_some_sound_2safety ho1 hrr htr hrr htr rfl
            exact ⟨hctx, hrel_ctx, try_new_64_valid ho, try_new_64_valid ho1,
              TypeRel2_scalar_rel2_of_concretizes hs1_rel,
              TypeRel2_scalar_rel2_of_concretizes hs2_rel⟩
      · rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
        rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
        exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩

private theorem nullOr_set_zero_valid
    {env : ExecEnv} {vCounter : Nat}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {jt : verifier.JumpType} {data : U64ScalarValueData}
    {id : MemoryId} {inner fallback_other : verifier.Type}
    (hctx : CompCtxValid env vCounter context)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (hnot_strict : verifier.JumpType.is_strict jt = ok false)
    (h : (do
      let b ← U64ScalarValueData.is_zero data
      if b then
        let id_cloned ← verifier.MemoryId.Insts.CoreCloneClone.clone id
        let c1 ← verifier.Type.unregister_resource inner context
        let c2 ← verifier.ComputationContext.set_null c1 id_cloned true
        let usvd ← U64ScalarValueData.from_known 0#u64
        let t2 ← verifier.Type.Insts.CoreCloneClone.clone (verifier.Type.ScalarValue usvd)
        ok (core.result.Result.Ok (t2, verifier.Type.ScalarValue usvd), c2)
      else
        let b1 ← verifier.JumpType.is_strict jt
        if b1 then
          if b then
            let id_cloned ← verifier.MemoryId.Insts.CoreCloneClone.clone id
            let inner_cloned ← verifier.Type.Insts.CoreCloneClone.clone inner
            let c1 ← verifier.ComputationContext.set_null context id_cloned false
            ok (core.result.Result.Ok (fallback_other, inner_cloned), c1)
          else ok (core.result.Result.Ok (type1, type2), context)
        else ok (core.result.Result.Ok (type1, type2), context) :
        Result ((core.result.Result (verifier.Type × verifier.Type) error.VerifierErrorKind) ×
          verifier.ComputationContext)) =
      ok (.Ok (new1, new2), context1)) :
    context1.pc = context.pc ∧
    CompCtxValid env vCounter context1 ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 := by
  rcases bind_eq_ok h with ⟨b, _, h⟩
  cases b with
  | false =>
    simp only [Bool.false_eq_true, ↓reduceIte] at h
    rw [bind_ok_eq hnot_strict] at h
    simp only [Bool.false_eq_true, ↓reduceIte] at h
    rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨rfl, hctx, ht1, ht2⟩
  | true =>
    simp only [↓reduceIte] at h
    rw [bind_ok_eq (MemoryId_clone_spec id)] at h
    obtain ⟨c1, hunreg, hpc1, hregs1, hstk1, hbm1, hresV1, _⟩ :=
      Type_unregister_resource_spec hctx.2.2.2 inner
    rw [bind_ok_eq hunreg] at h
    have hc1_valid : CompCtxValid env vCounter c1 :=
      ⟨hregs1 ▸ hctx.1, hstk1 ▸ hctx.2.1, hbm1 ▸ hctx.2.2.1, hresV1⟩
    obtain ⟨c2, hsn, hpc2, _, _, _, _, hc2_valid⟩ :=
      ComputationContext_set_null_spec hc1_valid id true
    rw [bind_ok_eq hsn, bind_ok_eq (from_known_64_eq 0#u64),
        bind_ok_eq (Type_clone_spec (.ScalarValue (knownScalar64 0#u64)))] at h
    rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨hpc2.trans hpc1, hc2_valid, knownScalar64_valid 0#u64, knownScalar64_valid 0#u64⟩

private theorem nullOr_lhs_set_zero_valid
    {env : ExecEnv} {vCounter : Nat}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {jt : verifier.JumpType} {data : U64ScalarValueData}
    {id : MemoryId} {inner : verifier.Type}
    (hctx : CompCtxValid env vCounter context)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (hnot_strict : verifier.JumpType.is_strict jt = ok false)
    (h : (do
      let b ← U64ScalarValueData.is_zero data
      if b then
        let id_cloned ← verifier.MemoryId.Insts.CoreCloneClone.clone id
        let c1 ← verifier.Type.unregister_resource inner context
        let c2 ← verifier.ComputationContext.set_null c1 id_cloned true
        let usvd ← U64ScalarValueData.from_known 0#u64
        let t2 ← verifier.Type.Insts.CoreCloneClone.clone (verifier.Type.ScalarValue usvd)
        ok (core.result.Result.Ok (t2, verifier.Type.ScalarValue usvd), c2)
      else
        let b1 ← verifier.JumpType.is_strict jt
        if b1 then
          if b then
            let id_cloned ← verifier.MemoryId.Insts.CoreCloneClone.clone id
            let inner_cloned ← verifier.Type.Insts.CoreCloneClone.clone inner
            let c1 ← verifier.ComputationContext.set_null context id_cloned false
            ok (core.result.Result.Ok (inner_cloned, type2), c1)
          else ok (core.result.Result.Ok (type1, type2), context)
        else ok (core.result.Result.Ok (type1, type2), context) :
        Result ((core.result.Result (verifier.Type × verifier.Type) error.VerifierErrorKind) ×
          verifier.ComputationContext)) =
      ok (.Ok (new1, new2), context1)) :
    context1.pc = context.pc ∧
    CompCtxValid env vCounter context1 ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 := by
  rcases bind_eq_ok h with ⟨b, _, h⟩
  cases b with
  | false =>
    simp only [Bool.false_eq_true, ↓reduceIte] at h
    rw [bind_ok_eq hnot_strict] at h
    simp only [Bool.false_eq_true, ↓reduceIte] at h
    rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨rfl, hctx, ht1, ht2⟩
  | true =>
    simp only [↓reduceIte] at h
    rw [bind_ok_eq (MemoryId_clone_spec id)] at h
    obtain ⟨c1, hunreg, hpc1, hregs1, hstk1, hbm1, hresV1, _⟩ :=
      Type_unregister_resource_spec hctx.2.2.2 inner
    rw [bind_ok_eq hunreg] at h
    have hc1_valid : CompCtxValid env vCounter c1 :=
      ⟨hregs1 ▸ hctx.1, hstk1 ▸ hctx.2.1, hbm1 ▸ hctx.2.2.1, hresV1⟩
    obtain ⟨c2, hsn, hpc2, _, _, _, _, hc2_valid⟩ :=
      ComputationContext_set_null_spec hc1_valid id true
    rw [bind_ok_eq hsn, bind_ok_eq (from_known_64_eq 0#u64),
        bind_ok_eq (Type_clone_spec (.ScalarValue (knownScalar64 0#u64)))] at h
    rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨hpc2.trans hpc1, hc2_valid, knownScalar64_valid 0#u64, knownScalar64_valid 0#u64⟩

private theorem nullOr_rhs_set_zero_sound
    {env : ExecEnv} {vCounter : Nat} {deps : verifier.DataDependencies}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {st₁ st₂ : MachineState} {l₁ : HostLayout}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {jt : verifier.JumpType} {data : U64ScalarValueData}
    {id : MemoryId} {inner : verifier.Type}
    {v1_1 v1_2 v2_1 v2_2 : RuntimeVal}
    (hl₁ : ValidHostLayout l₁)
    (hctx : CompCtxValid env vCounter context)
    (hrel_ctx : StateRel2Deps env context deps ρ resActive isNullId st₁ st₂)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (hu1 : pureUnwrapReleasable type1 = .ScalarValue data)
    (hu2 : pureUnwrapReleasable type2 = .NullOr id inner)
    (hrel1 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type1 v1_1 v1_2)
    (hrel2 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type2 v2_1 v2_2)
    (hnot_strict : verifier.JumpType.is_strict jt = ok false)
    (himp : v1_1.toBits l₁ st₁.world.arrayLen = 0#64 →
      v2_1.toBits l₁ st₁.world.arrayLen = 0#64)
    (h : (do
      let b ← U64ScalarValueData.is_zero data
      if b then
        let id_cloned ← verifier.MemoryId.Insts.CoreCloneClone.clone id
        let c1 ← verifier.Type.unregister_resource inner context
        let c2 ← verifier.ComputationContext.set_null c1 id_cloned true
        let usvd ← U64ScalarValueData.from_known 0#u64
        let t2 ← verifier.Type.Insts.CoreCloneClone.clone (verifier.Type.ScalarValue usvd)
        ok (core.result.Result.Ok (t2, verifier.Type.ScalarValue usvd), c2)
      else
        let b1 ← verifier.JumpType.is_strict jt
        if b1 then
          if b then
            let id_cloned ← verifier.MemoryId.Insts.CoreCloneClone.clone id
            let inner_cloned ← verifier.Type.Insts.CoreCloneClone.clone inner
            let c1 ← verifier.ComputationContext.set_null context id_cloned false
            ok (core.result.Result.Ok (type1, inner_cloned), c1)
          else ok (core.result.Result.Ok (type1, type2), context)
        else ok (core.result.Result.Ok (type1, type2), context) :
        Result ((core.result.Result (verifier.Type × verifier.Type) error.VerifierErrorKind) ×
          verifier.ComputationContext)) =
      ok (.Ok (new1, new2), context1)) :
    CompCtxValid env vCounter context1 ∧
    StateRel2Deps env context1 deps ρ resActive isNullId st₁ st₂ ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new1 v1_1 v1_2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new2 v2_1 v2_2 := by
  have hop1 := unwrap_eq_of_scalar ht1 hu1; subst hop1
  have hop2 := unwrap_eq_of_nullOr ht2 hu2; subst hop2
  rcases bind_eq_ok h with ⟨b, hb, h⟩
  cases b with
  | false =>
    simp only [Bool.false_eq_true, ↓reduceIte] at h
    rw [bind_ok_eq hnot_strict] at h
    simp only [Bool.false_eq_true, ↓reduceIte] at h
    rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
  | true =>
    simp only [↓reduceIte] at h
    have hne := not_uninit_of_is_zero_64 hb
    have hrel1' := hrel1; simp only [TypeRel2, hne, ↓reduceIte] at hrel1'
    rcases hrel1' with ⟨x₁, x₂, rfl, rfl, hx⟩
    have hx1_0 : x₁ = 0#64 := is_zero_64_sound hb hx.1
    have hx2_0 : x₂ = 0#64 := is_zero_64_sound hb hx.2.1
    subst hx1_0; subst hx2_0
    obtain ⟨hisNull, hinact, rfl, rfl⟩ := NullOr_zero_iff_isNull hl₁ ht2 hrel2 (himp rfl)
    rw [bind_ok_eq (MemoryId_clone_spec id)] at h
    obtain ⟨c1, hunreg, _, _, _, _, hc1_valid, hc1_rel⟩ :=
      StateRel2Deps_unregister_resource_inactive hctx hinact hrel_ctx
    rw [bind_ok_eq hunreg] at h
    obtain ⟨c2, hsn, _, _, hc2_valid, hc2_rel⟩ :=
      StateRel2Deps_set_null hc1_valid hisNull hc1_rel
    rw [bind_ok_eq hsn, bind_ok_eq (from_known_64_eq 0#u64),
        bind_ok_eq (Type_clone_spec (.ScalarValue (knownScalar64 0#u64)))] at h
    rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨hc2_valid, hc2_rel, knownScalar64_valid 0#u64, knownScalar64_valid 0#u64,
      TypeRel2_scalar_rel2_of_concretizes (concretizesScalar64Rel2_known 0#u64),
      TypeRel2_scalar_rel2_of_concretizes (concretizesScalar64Rel2_known 0#u64)⟩

private theorem nullOr_lhs_set_zero_sound
    {env : ExecEnv} {vCounter : Nat} {deps : verifier.DataDependencies}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {st₁ st₂ : MachineState} {l₁ : HostLayout}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {jt : verifier.JumpType} {data : U64ScalarValueData}
    {id : MemoryId} {inner : verifier.Type}
    {v1_1 v1_2 v2_1 v2_2 : RuntimeVal}
    (hl₁ : ValidHostLayout l₁)
    (hctx : CompCtxValid env vCounter context)
    (hrel_ctx : StateRel2Deps env context deps ρ resActive isNullId st₁ st₂)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (hu1 : pureUnwrapReleasable type1 = .NullOr id inner)
    (hu2 : pureUnwrapReleasable type2 = .ScalarValue data)
    (hrel1 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type1 v1_1 v1_2)
    (hrel2 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type2 v2_1 v2_2)
    (hnot_strict : verifier.JumpType.is_strict jt = ok false)
    (himp : v2_1.toBits l₁ st₁.world.arrayLen = 0#64 →
      v1_1.toBits l₁ st₁.world.arrayLen = 0#64)
    (h : (do
      let b ← U64ScalarValueData.is_zero data
      if b then
        let id_cloned ← verifier.MemoryId.Insts.CoreCloneClone.clone id
        let c1 ← verifier.Type.unregister_resource inner context
        let c2 ← verifier.ComputationContext.set_null c1 id_cloned true
        let usvd ← U64ScalarValueData.from_known 0#u64
        let t2 ← verifier.Type.Insts.CoreCloneClone.clone (verifier.Type.ScalarValue usvd)
        ok (core.result.Result.Ok (t2, verifier.Type.ScalarValue usvd), c2)
      else
        let b1 ← verifier.JumpType.is_strict jt
        if b1 then
          if b then
            let id_cloned ← verifier.MemoryId.Insts.CoreCloneClone.clone id
            let inner_cloned ← verifier.Type.Insts.CoreCloneClone.clone inner
            let c1 ← verifier.ComputationContext.set_null context id_cloned false
            ok (core.result.Result.Ok (inner_cloned, type2), c1)
          else ok (core.result.Result.Ok (type1, type2), context)
        else ok (core.result.Result.Ok (type1, type2), context) :
        Result ((core.result.Result (verifier.Type × verifier.Type) error.VerifierErrorKind) ×
          verifier.ComputationContext)) =
      ok (.Ok (new1, new2), context1)) :
    CompCtxValid env vCounter context1 ∧
    StateRel2Deps env context1 deps ρ resActive isNullId st₁ st₂ ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new1 v1_1 v1_2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new2 v2_1 v2_2 := by
  have hop1 := unwrap_eq_of_nullOr ht1 hu1; subst hop1
  have hop2 := unwrap_eq_of_scalar ht2 hu2; subst hop2
  rcases bind_eq_ok h with ⟨b, hb, h⟩
  cases b with
  | false =>
    simp only [Bool.false_eq_true, ↓reduceIte] at h
    rw [bind_ok_eq hnot_strict] at h
    simp only [Bool.false_eq_true, ↓reduceIte] at h
    rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
  | true =>
    simp only [↓reduceIte] at h
    have hne := not_uninit_of_is_zero_64 hb
    have hrel2' := hrel2; simp only [TypeRel2, hne, ↓reduceIte] at hrel2'
    rcases hrel2' with ⟨y₁, y₂, rfl, rfl, hy⟩
    have hy1_0 : y₁ = 0#64 := is_zero_64_sound hb hy.1
    have hy2_0 : y₂ = 0#64 := is_zero_64_sound hb hy.2.1
    subst hy1_0; subst hy2_0
    obtain ⟨hisNull, hinact, rfl, rfl⟩ := NullOr_zero_iff_isNull hl₁ ht1 hrel1 (himp rfl)
    rw [bind_ok_eq (MemoryId_clone_spec id)] at h
    obtain ⟨c1, hunreg, _, _, _, _, hc1_valid, hc1_rel⟩ :=
      StateRel2Deps_unregister_resource_inactive hctx hinact hrel_ctx
    rw [bind_ok_eq hunreg] at h
    obtain ⟨c2, hsn, _, _, hc2_valid, hc2_rel⟩ :=
      StateRel2Deps_set_null hc1_valid hisNull hc1_rel
    rw [bind_ok_eq hsn, bind_ok_eq (from_known_64_eq 0#u64),
        bind_ok_eq (Type_clone_spec (.ScalarValue (knownScalar64 0#u64)))] at h
    rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨hc2_valid, hc2_rel, knownScalar64_valid 0#u64, knownScalar64_valid 0#u64,
      TypeRel2_scalar_rel2_of_concretizes (concretizesScalar64Rel2_known 0#u64),
      TypeRel2_scalar_rel2_of_concretizes (concretizesScalar64Rel2_known 0#u64)⟩

private theorem nullOr_rhs_set_nonnull_valid
    {env : ExecEnv} {vCounter : Nat}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {jt : verifier.JumpType} {data : U64ScalarValueData}
    {id : MemoryId} {inner : verifier.Type}
    (hctx : CompCtxValid env vCounter context)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (hu2 : pureUnwrapReleasable type2 = .NullOr id inner)
    (h : (do
      let b ← verifier.JumpType.is_strict jt
      if b then
        let b1 ← U64ScalarValueData.is_zero data
        if b1 then
          let id_cloned ← verifier.MemoryId.Insts.CoreCloneClone.clone id
          let inner_cloned ← verifier.Type.Insts.CoreCloneClone.clone inner
          let c1 ← verifier.ComputationContext.set_null context id_cloned false
          ok (core.result.Result.Ok (type1, inner_cloned), c1)
        else ok (core.result.Result.Ok (type1, type2), context)
      else ok (core.result.Result.Ok (type1, type2), context) :
        Result ((core.result.Result (verifier.Type × verifier.Type) error.VerifierErrorKind) ×
          verifier.ComputationContext)) =
      ok (.Ok (new1, new2), context1)) :
    context1.pc = context.pc ∧
    CompCtxValid env vCounter context1 ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 := by
  have hop2 := unwrap_eq_of_nullOr ht2 hu2; subst hop2
  rcases bind_eq_ok h with ⟨b, _, h⟩
  cases b with
  | false =>
    simp only [Bool.false_eq_true, ↓reduceIte] at h
    rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨rfl, hctx, ht1, ht2⟩
  | true =>
    simp only [↓reduceIte] at h
    rcases bind_eq_ok h with ⟨b1, _, h⟩
    cases b1 with
    | false =>
      simp only [Bool.false_eq_true, ↓reduceIte] at h
      rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
      rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
      exact ⟨rfl, hctx, ht1, ht2⟩
    | true =>
      simp only [↓reduceIte] at h
      rw [bind_ok_eq (MemoryId_clone_spec id), bind_ok_eq (Type_clone_spec inner)] at h
      obtain ⟨c1, hsn, hpc1, _, _, _, _, hc1_valid⟩ :=
        ComputationContext_set_null_spec hctx id false
      rw [bind_ok_eq hsn] at h
      rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
      rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
      exact ⟨hpc1, hc1_valid, ht1, ht2.2.2⟩

private theorem nullOr_lhs_set_nonnull_valid
    {env : ExecEnv} {vCounter : Nat}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {jt : verifier.JumpType} {data : U64ScalarValueData}
    {id : MemoryId} {inner : verifier.Type}
    (hctx : CompCtxValid env vCounter context)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (hu1 : pureUnwrapReleasable type1 = .NullOr id inner)
    (h : (do
      let b ← verifier.JumpType.is_strict jt
      if b then
        let b1 ← U64ScalarValueData.is_zero data
        if b1 then
          let id_cloned ← verifier.MemoryId.Insts.CoreCloneClone.clone id
          let inner_cloned ← verifier.Type.Insts.CoreCloneClone.clone inner
          let c1 ← verifier.ComputationContext.set_null context id_cloned false
          ok (core.result.Result.Ok (inner_cloned, type2), c1)
        else ok (core.result.Result.Ok (type1, type2), context)
      else ok (core.result.Result.Ok (type1, type2), context) :
        Result ((core.result.Result (verifier.Type × verifier.Type) error.VerifierErrorKind) ×
          verifier.ComputationContext)) =
      ok (.Ok (new1, new2), context1)) :
    context1.pc = context.pc ∧
    CompCtxValid env vCounter context1 ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 := by
  have hop1 := unwrap_eq_of_nullOr ht1 hu1; subst hop1
  rcases bind_eq_ok h with ⟨b, _, h⟩
  cases b with
  | false =>
    simp only [Bool.false_eq_true, ↓reduceIte] at h
    rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨rfl, hctx, ht1, ht2⟩
  | true =>
    simp only [↓reduceIte] at h
    rcases bind_eq_ok h with ⟨b1, _, h⟩
    cases b1 with
    | false =>
      simp only [Bool.false_eq_true, ↓reduceIte] at h
      rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
      rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
      exact ⟨rfl, hctx, ht1, ht2⟩
    | true =>
      simp only [↓reduceIte] at h
      rw [bind_ok_eq (MemoryId_clone_spec id), bind_ok_eq (Type_clone_spec inner)] at h
      obtain ⟨c1, hsn, hpc1, _, _, _, _, hc1_valid⟩ :=
        ComputationContext_set_null_spec hctx id false
      rw [bind_ok_eq hsn] at h
      rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
      rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
      exact ⟨hpc1, hc1_valid, ht1.2.2, ht2⟩

private theorem nullOr_rhs_set_nonnull_sound
    {env : ExecEnv} {vCounter : Nat} {deps : verifier.DataDependencies}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {st₁ st₂ : MachineState} {l₁ : HostLayout}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {jt : verifier.JumpType} {data : U64ScalarValueData}
    {id : MemoryId} {inner : verifier.Type}
    {v1_1 v1_2 v2_1 v2_2 : RuntimeVal}
    (hctx : CompCtxValid env vCounter context)
    (hrel_ctx : StateRel2Deps env context deps ρ resActive isNullId st₁ st₂)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (hu1 : pureUnwrapReleasable type1 = .ScalarValue data)
    (hu2 : pureUnwrapReleasable type2 = .NullOr id inner)
    (hrel1 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type1 v1_1 v1_2)
    (hrel2 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type2 v2_1 v2_2)
    (himp : verifier.JumpType.is_strict jt = ok true →
      v1_1.toBits l₁ st₁.world.arrayLen = 0#64 →
      v2_1.toBits l₁ st₁.world.arrayLen ≠ 0#64)
    (h : (do
      let b ← verifier.JumpType.is_strict jt
      if b then
        let b1 ← U64ScalarValueData.is_zero data
        if b1 then
          let id_cloned ← verifier.MemoryId.Insts.CoreCloneClone.clone id
          let inner_cloned ← verifier.Type.Insts.CoreCloneClone.clone inner
          let c1 ← verifier.ComputationContext.set_null context id_cloned false
          ok (core.result.Result.Ok (type1, inner_cloned), c1)
        else ok (core.result.Result.Ok (type1, type2), context)
      else ok (core.result.Result.Ok (type1, type2), context) :
        Result ((core.result.Result (verifier.Type × verifier.Type) error.VerifierErrorKind) ×
          verifier.ComputationContext)) =
      ok (.Ok (new1, new2), context1)) :
    CompCtxValid env vCounter context1 ∧
    StateRel2Deps env context1 deps ρ resActive isNullId st₁ st₂ ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new1 v1_1 v1_2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new2 v2_1 v2_2 := by
  have hop1 := unwrap_eq_of_scalar ht1 hu1; subst hop1
  have hop2 := unwrap_eq_of_nullOr ht2 hu2; subst hop2
  rcases bind_eq_ok h with ⟨b, hb, h⟩
  cases b with
  | false =>
    simp only [Bool.false_eq_true, ↓reduceIte] at h
    rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
  | true =>
    simp only [↓reduceIte] at h
    rcases bind_eq_ok h with ⟨b1, hb1, h⟩
    cases b1 with
    | false =>
      simp only [Bool.false_eq_true, ↓reduceIte] at h
      rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
      rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
      exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
    | true =>
      simp only [↓reduceIte] at h
      have hne := not_uninit_of_is_zero_64 hb1
      have hrel1' := hrel1; simp only [TypeRel2, hne, ↓reduceIte] at hrel1'
      rcases hrel1' with ⟨x₁, x₂, rfl, rfl, hx⟩
      have hx1_0 : x₁ = 0#64 := is_zero_64_sound hb1 hx.1
      subst hx1_0
      obtain ⟨hnotNull, hinner_rel⟩ := NullOr_nonzero_iff_not_isNull hrel2 (himp hb rfl)
      rw [bind_ok_eq (MemoryId_clone_spec id), bind_ok_eq (Type_clone_spec inner)] at h
      obtain ⟨c1, hsn, _, _, hc1_valid, hc1_rel⟩ :=
        StateRel2Deps_set_null hctx hnotNull hrel_ctx
      rw [bind_ok_eq hsn] at h
      rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
      rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
      exact ⟨hc1_valid, hc1_rel, ht1, ht2.2.2, hrel1, hinner_rel⟩

private theorem nullOr_lhs_set_nonnull_sound
    {env : ExecEnv} {vCounter : Nat} {deps : verifier.DataDependencies}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {st₁ st₂ : MachineState} {l₁ : HostLayout}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {jt : verifier.JumpType} {data : U64ScalarValueData}
    {id : MemoryId} {inner : verifier.Type}
    {v1_1 v1_2 v2_1 v2_2 : RuntimeVal}
    (hctx : CompCtxValid env vCounter context)
    (hrel_ctx : StateRel2Deps env context deps ρ resActive isNullId st₁ st₂)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (hu1 : pureUnwrapReleasable type1 = .NullOr id inner)
    (hu2 : pureUnwrapReleasable type2 = .ScalarValue data)
    (hrel1 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type1 v1_1 v1_2)
    (hrel2 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type2 v2_1 v2_2)
    (himp : verifier.JumpType.is_strict jt = ok true →
      v2_1.toBits l₁ st₁.world.arrayLen = 0#64 →
      v1_1.toBits l₁ st₁.world.arrayLen ≠ 0#64)
    (h : (do
      let b ← verifier.JumpType.is_strict jt
      if b then
        let b1 ← U64ScalarValueData.is_zero data
        if b1 then
          let id_cloned ← verifier.MemoryId.Insts.CoreCloneClone.clone id
          let inner_cloned ← verifier.Type.Insts.CoreCloneClone.clone inner
          let c1 ← verifier.ComputationContext.set_null context id_cloned false
          ok (core.result.Result.Ok (inner_cloned, type2), c1)
        else ok (core.result.Result.Ok (type1, type2), context)
      else ok (core.result.Result.Ok (type1, type2), context) :
        Result ((core.result.Result (verifier.Type × verifier.Type) error.VerifierErrorKind) ×
          verifier.ComputationContext)) =
      ok (.Ok (new1, new2), context1)) :
    CompCtxValid env vCounter context1 ∧
    StateRel2Deps env context1 deps ρ resActive isNullId st₁ st₂ ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new1 v1_1 v1_2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new2 v2_1 v2_2 := by
  have hop1 := unwrap_eq_of_nullOr ht1 hu1; subst hop1
  have hop2 := unwrap_eq_of_scalar ht2 hu2; subst hop2
  rcases bind_eq_ok h with ⟨b, hb, h⟩
  cases b with
  | false =>
    simp only [Bool.false_eq_true, ↓reduceIte] at h
    rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
  | true =>
    simp only [↓reduceIte] at h
    rcases bind_eq_ok h with ⟨b1, hb1, h⟩
    cases b1 with
    | false =>
      simp only [Bool.false_eq_true, ↓reduceIte] at h
      rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
      rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
      exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
    | true =>
      simp only [↓reduceIte] at h
      have hne := not_uninit_of_is_zero_64 hb1
      have hrel2' := hrel2; simp only [TypeRel2, hne, ↓reduceIte] at hrel2'
      rcases hrel2' with ⟨y₁, y₂, rfl, rfl, hy⟩
      have hy1_0 : y₁ = 0#64 := is_zero_64_sound hb1 hy.1
      subst hy1_0
      obtain ⟨hnotNull, hinner_rel⟩ := NullOr_nonzero_iff_not_isNull hrel1 (himp hb rfl)
      rw [bind_ok_eq (MemoryId_clone_spec id), bind_ok_eq (Type_clone_spec inner)] at h
      obtain ⟨c1, hsn, _, _, hc1_valid, hc1_rel⟩ :=
        StateRel2Deps_set_null hctx hnotNull hrel_ctx
      rw [bind_ok_eq hsn] at h
      rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
      rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
      exact ⟨hc1_valid, hc1_rel, ht1.2.2, ht2, hinner_rel, hrel2⟩

/-! ### Pointer Offset & Array Bounds Constraint Helpers & `Type.constraint` -/

private theorem same_id_constrain_offsets_valid
    {env : ExecEnv} {vCounter : Nat}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {id1 id2 : MemoryId} {off1 off2 : U64ScalarValueData}
    (hctx : CompCtxValid env vCounter context)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (hoff1 : typePtrOffset? type1 = some off1)
    (hoff2 : typePtrOffset? type2 = some off2)
    (h : (do
      let b ← verifier.MemoryId.Insts.CoreCmpPartialEqMemoryId.eq id1 id2
      if b then verifier.Type.constrain_equal_offsets context type1 type2 off1 off2
      else ok (core.result.Result.Ok (type1, type2), context)) =
      ok (.Ok (new1, new2), context1)) :
    context1.pc = context.pc ∧
    CompCtxValid env vCounter context1 ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 := by
  rw [bind_ok_eq (MemoryId_eq_spec id1 id2)] at h
  split_ifs at h with heq
  · exact Type_constrain_equal_offsets_ok_valid hctx ht1 ht2 hoff1 hoff2 h
  · rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨rfl, hctx, ht1, ht2⟩

private theorem array_bounds_loose_valid
    {env : ExecEnv} {vCounter : Nat}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {id1 id2 : MemoryId} {offset : U64ScalarValueData}
    (hctx : CompCtxValid env vCounter context)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (hid1 : memIdBounded vCounter id1 = true)
    (h : (do
      let b ← verifier.MemoryId.Insts.CoreCmpPartialEqMemoryId.eq id1 id2
      if b then
        let id_cloned ← verifier.MemoryId.Insts.CoreCloneClone.clone id1
        let c1 ← verifier.ComputationContext.update_array_bounds context id_cloned offset
        ok (core.result.Result.Ok (type1, type2), c1)
      else ok (core.result.Result.Ok (type1, type2), context) :
        Result ((core.result.Result (verifier.Type × verifier.Type) error.VerifierErrorKind) ×
          verifier.ComputationContext)) =
      ok (.Ok (new1, new2), context1)) :
    context1.pc = context.pc ∧
    CompCtxValid env vCounter context1 ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 := by
  rw [bind_ok_eq (MemoryId_eq_spec id1 id2)] at h
  split_ifs at h with heq
  · rw [bind_ok_eq (MemoryId_clone_spec id1)] at h
    rcases bind_eq_ok h with ⟨c1, hupd, h⟩
    rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    obtain ⟨hpc1, _, _, _, hc1_valid, _, _⟩ :=
      ComputationContext_update_array_bounds_ok_spec hctx hid1 hupd
    exact ⟨hpc1, hc1_valid, ht1, ht2⟩
  · rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨rfl, hctx, ht1, ht2⟩

private theorem array_bounds_strict_valid
    {env : ExecEnv} {vCounter : Nat}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {id1 id2 : MemoryId} {offset : U64ScalarValueData}
    (hctx : CompCtxValid env vCounter context)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (hid1 : memIdBounded vCounter id1 = true)
    (h : (do
      let b ← verifier.MemoryId.Insts.CoreCmpPartialEqMemoryId.eq id1 id2
      if b then
        let o ← U64ScalarValueData.checked_add_u64 offset 1#u64
        match o with
        | none =>
          ok (core.result.Result.Err error.VerifierErrorKind.PointerOffsetOverflow, context)
        | some bound =>
          let id_cloned ← verifier.MemoryId.Insts.CoreCloneClone.clone id1
          let c1 ← verifier.ComputationContext.update_array_bounds context id_cloned bound
          ok (core.result.Result.Ok (type1, type2), c1)
      else ok (core.result.Result.Ok (type1, type2), context)) =
      ok (.Ok (new1, new2), context1)) :
    context1.pc = context.pc ∧
    CompCtxValid env vCounter context1 ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 := by
  rw [bind_ok_eq (MemoryId_eq_spec id1 id2)] at h
  split_ifs at h with heq
  · rcases bind_eq_ok h with ⟨o, _, h⟩
    cases o with
    | none => cases (Prod.mk.inj (ok_inj h)).1
    | some bound =>
      dsimp only at h
      rw [bind_ok_eq (MemoryId_clone_spec id1)] at h
      rcases bind_eq_ok h with ⟨c1, hupd, h⟩
      rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
      rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
      obtain ⟨hpc1, _, _, _, hc1_valid, _, _⟩ :=
        ComputationContext_update_array_bounds_ok_spec hctx hid1 hupd
      exact ⟨hpc1, hc1_valid, ht1, ht2⟩
  · rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨rfl, hctx, ht1, ht2⟩

private theorem stack_eq_offsets_sound
    {env : ExecEnv} {vCounter : Nat} {deps : verifier.DataDependencies}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {st₁ st₂ : MachineState} {l₁ : HostLayout}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {so so1 : U64ScalarValueData}
    {v1_1 v1_2 v2_1 v2_2 : RuntimeVal}
    (hctx : CompCtxValid env vCounter context)
    (hrel_ctx : StateRel2Deps env context deps ρ resActive isNullId st₁ st₂)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (hu1 : pureUnwrapReleasable type1 = .PtrToStack so)
    (hu2 : pureUnwrapReleasable type2 = .PtrToStack so1)
    (hrel1 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type1 v1_1 v1_2)
    (hrel2 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type2 v2_1 v2_2)
    (hjt : JumpTypeHolds .Eq .W64
      (v1_1.toBits l₁ st₁.world.arrayLen) (v2_1.toBits l₁ st₁.world.arrayLen))
    (h : verifier.Type.constrain_equal_offsets context type1 type2 so so1 =
      ok (.Ok (new1, new2), context1)) :
    CompCtxValid env vCounter context1 ∧
    StateRel2Deps env context1 deps ρ resActive isNullId st₁ st₂ ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new1 v1_1 v1_2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new2 v2_1 v2_2 := by
  have hop1 := unwrap_eq_of_stack ht1 hu1; subst hop1
  have hop2 := unwrap_eq_of_stack ht2 hu2; subst hop2
  rcases hrel1 with ⟨hp1, o1, hc1, rfl, rfl⟩
  rcases hrel2 with ⟨hp2, o2, hc2, rfl, rfl⟩
  have ho_eq : o1 = o2 := bv_cancel_add_left hjt
  subst o2
  obtain ⟨rfl, r1, r2, rfl, rfl, hv1, hu1', hle1, hcr1, hv2, hu2', hle2, hcr2⟩ :=
    Type_constrain_equal_offsets_spec_rel2 hp1.2.1 hp2.2.1 hc1 hc2 h
  have hp1' : PtrOffsetValid 0xFFFF_FFFF#64 r1 := ⟨hv1, hu1', BitVec.le_trans hle1 hp1.2.2⟩
  have hp2' : PtrOffsetValid 0xFFFF_FFFF#64 r2 := ⟨hv2, hu2', BitVec.le_trans hle2 hp2.2.2⟩
  exact ⟨hctx, hrel_ctx, hp1', hp2', ⟨hp1', o1, hcr1, rfl, rfl⟩, ⟨hp2', o1, hcr2, rfl, rfl⟩⟩

private theorem mem_eq_offsets_sound
    {env : ExecEnv} {vCounter : Nat} {deps : verifier.DataDependencies}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {st₁ st₂ : MachineState} {l₁ : HostLayout}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {id1 id2 : MemoryId} {off1 off2 : U64ScalarValueData}
    {sz1 sz2 : Std.U64} {orig1 orig2 : verifier.PointerOrigin}
    {v1_1 v1_2 v2_1 v2_2 : RuntimeVal}
    (hctx : CompCtxValid env vCounter context)
    (hrel_ctx : StateRel2Deps env context deps ρ resActive isNullId st₁ st₂)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (hu1 : pureUnwrapReleasable type1 = .PtrToMemory id1 off1 sz1 orig1)
    (hu2 : pureUnwrapReleasable type2 = .PtrToMemory id2 off2 sz2 orig2)
    (hrel1 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type1 v1_1 v1_2)
    (hrel2 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type2 v2_1 v2_2)
    (hjt : JumpTypeHolds .Eq .W64
      (v1_1.toBits l₁ st₁.world.arrayLen) (v2_1.toBits l₁ st₁.world.arrayLen))
    (h : (do
      let b ← verifier.MemoryId.Insts.CoreCmpPartialEqMemoryId.eq id1 id2
      if b then verifier.Type.constrain_equal_offsets context type1 type2 off1 off2
      else ok (core.result.Result.Ok (type1, type2), context)) =
      ok (.Ok (new1, new2), context1)) :
    CompCtxValid env vCounter context1 ∧
    StateRel2Deps env context1 deps ρ resActive isNullId st₁ st₂ ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new1 v1_1 v1_2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new2 v2_1 v2_2 := by
  rw [bind_ok_eq (MemoryId_eq_spec id1 id2)] at h
  split_ifs at h with heq
  · have hid : id1 = id2 := of_decide_eq_true heq; subst hid
    obtain ⟨_, hu1', o1, hc1, hbits1, hk1⟩ := mem_ptr_extract_rel2 (l := l₁) ht1 hu1 hrel1
    obtain ⟨_, hu2', o2, hc2, hbits2, hk2⟩ := mem_ptr_extract_rel2 (l := l₁) ht2 hu2 hrel2
    change v1_1.toBits l₁ st₁.world.arrayLen = v2_1.toBits l₁ st₁.world.arrayLen at hjt
    rw [hbits1, hbits2] at hjt
    have ho_eq : o1 = o2 := bv_cancel_add_left hjt
    subst o2
    obtain ⟨rfl, r1, r2, rfl, rfl, hv1, hr1_u, hle1, hcr1, hv2, hr2_u, hle2, hcr2⟩ :=
      Type_constrain_equal_offsets_spec_rel2 hu1' hu2' hc1 hc2 h
    obtain ⟨ht1', hrel1'⟩ := hk1 r1 hv1 hr1_u hle1 hcr1
    obtain ⟨ht2', hrel2'⟩ := hk2 r2 hv2 hr2_u hle2 hcr2
    exact ⟨hctx, hrel_ctx, ht1', ht2', hrel1', hrel2'⟩
  · rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩

private theorem struct_eq_offsets_sound
    {env : ExecEnv} {vCounter : Nat} {deps : verifier.DataDependencies}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {st₁ st₂ : MachineState} {l₁ : HostLayout}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {id1 id2 : MemoryId} {off1 off2 : U64ScalarValueData}
    {sd1 sd2 : verifier.StructDescriptor}
    {v1_1 v1_2 v2_1 v2_2 : RuntimeVal}
    (hctx : CompCtxValid env vCounter context)
    (hrel_ctx : StateRel2Deps env context deps ρ resActive isNullId st₁ st₂)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (hu1 : pureUnwrapReleasable type1 = .PtrToStruct id1 off1 sd1)
    (hu2 : pureUnwrapReleasable type2 = .PtrToStruct id2 off2 sd2)
    (hrel1 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type1 v1_1 v1_2)
    (hrel2 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type2 v2_1 v2_2)
    (hjt : JumpTypeHolds .Eq .W64
      (v1_1.toBits l₁ st₁.world.arrayLen) (v2_1.toBits l₁ st₁.world.arrayLen))
    (h : (do
      let b ← verifier.MemoryId.Insts.CoreCmpPartialEqMemoryId.eq id1 id2
      if b then verifier.Type.constrain_equal_offsets context type1 type2 off1 off2
      else ok (core.result.Result.Ok (type1, type2), context)) =
      ok (.Ok (new1, new2), context1)) :
    CompCtxValid env vCounter context1 ∧
    StateRel2Deps env context1 deps ρ resActive isNullId st₁ st₂ ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new1 v1_1 v1_2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new2 v2_1 v2_2 := by
  rw [bind_ok_eq (MemoryId_eq_spec id1 id2)] at h
  split_ifs at h with heq
  · have hid : id1 = id2 := of_decide_eq_true heq; subst hid
    obtain ⟨_, hu1', o1, hc1, hbits1, hk1⟩ := struct_ptr_extract_rel2 (l := l₁) ht1 hu1 hrel1
    obtain ⟨_, hu2', o2, hc2, hbits2, hk2⟩ := struct_ptr_extract_rel2 (l := l₁) ht2 hu2 hrel2
    change v1_1.toBits l₁ st₁.world.arrayLen = v2_1.toBits l₁ st₁.world.arrayLen at hjt
    rw [hbits1, hbits2] at hjt
    have ho_eq : o1 = o2 := bv_cancel_add_left hjt
    subst o2
    obtain ⟨rfl, r1, r2, rfl, rfl, hv1, hr1_u, hle1, hcr1, hv2, hr2_u, hle2, hcr2⟩ :=
      Type_constrain_equal_offsets_spec_rel2 hu1' hu2' hc1 hc2 h
    obtain ⟨ht1', hrel1'⟩ := hk1 r1 hv1 hr1_u hle1 hcr1
    obtain ⟨ht2', hrel2'⟩ := hk2 r2 hv2 hr2_u hle2 hcr2
    exact ⟨hctx, hrel_ctx, ht1', ht2', hrel1', hrel2'⟩
  · rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩

private theorem array_eq_offsets_sound
    {env : ExecEnv} {vCounter : Nat} {deps : verifier.DataDependencies}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {st₁ st₂ : MachineState} {l₁ : HostLayout}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {id1 id2 : MemoryId} {off1 off2 : U64ScalarValueData}
    {v1_1 v1_2 v2_1 v2_2 : RuntimeVal}
    (hctx : CompCtxValid env vCounter context)
    (hrel_ctx : StateRel2Deps env context deps ρ resActive isNullId st₁ st₂)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (hu1 : pureUnwrapReleasable type1 = .PtrToArray id1 off1)
    (hu2 : pureUnwrapReleasable type2 = .PtrToArray id2 off2)
    (hrel1 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type1 v1_1 v1_2)
    (hrel2 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type2 v2_1 v2_2)
    (hjt : JumpTypeHolds .Eq .W64
      (v1_1.toBits l₁ st₁.world.arrayLen) (v2_1.toBits l₁ st₁.world.arrayLen))
    (h : (do
      let b ← verifier.MemoryId.Insts.CoreCmpPartialEqMemoryId.eq id1 id2
      if b then verifier.Type.constrain_equal_offsets context type1 type2 off1 off2
      else ok (core.result.Result.Ok (type1, type2), context)) =
      ok (.Ok (new1, new2), context1)) :
    CompCtxValid env vCounter context1 ∧
    StateRel2Deps env context1 deps ρ resActive isNullId st₁ st₂ ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new1 v1_1 v1_2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new2 v2_1 v2_2 := by
  have hop1 := unwrap_eq_of_array ht1 hu1; subst hop1
  have hop2 := unwrap_eq_of_array ht2 hu2; subst hop2
  rw [bind_ok_eq (MemoryId_eq_spec id1 id2)] at h
  split_ifs at h with heq
  · have hid : id1 = id2 := of_decide_eq_true heq; subst hid
    rcases hrel1 with ⟨hp1, hlen1, o1, hc1, rfl, rfl⟩
    rcases hrel2 with ⟨hp2, _, o2, hc2, rfl, rfl⟩
    have ho_eq : o1 = o2 := bv_cancel_add_left hjt
    subst o2
    obtain ⟨rfl, r1, r2, rfl, rfl, hv1, hr1_u, hle1, hcr1, hv2, hr2_u, hle2, hcr2⟩ :=
      Type_constrain_equal_offsets_spec_rel2 hp1.2.1 hp2.2.1 hc1 hc2 h
    have hp1' : PtrOffsetValid MAX_PACKET_OFFSET r1 := ⟨hv1, hr1_u, BitVec.le_trans hle1 hp1.2.2⟩
    have hp2' : PtrOffsetValid MAX_PACKET_OFFSET r2 := ⟨hv2, hr2_u, BitVec.le_trans hle2 hp2.2.2⟩
    exact ⟨hctx, hrel_ctx, ⟨ht1.1, hp1'⟩, ⟨ht2.1, hp2'⟩,
      ⟨hp1', hlen1, o1, hcr1, rfl, rfl⟩, ⟨hp2', hlen1, o1, hcr2, rfl, rfl⟩⟩
  · rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩

private theorem array_end_loose_lhs_sound
    {env : ExecEnv} {vCounter : Nat} {deps : verifier.DataDependencies}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {st₁ st₂ : MachineState} {l₁ : HostLayout}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {id1 id2 : MemoryId} {offset : U64ScalarValueData}
    {v1_1 v1_2 v2_1 v2_2 : RuntimeVal}
    (hl₁ : ValidHostLayout l₁)
    (hctx : CompCtxValid env vCounter context)
    (hrel_ctx : StateRel2Deps env context deps ρ resActive isNullId st₁ st₂)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (hu1 : pureUnwrapReleasable type1 = .PtrToArray id1 offset)
    (hu2 : pureUnwrapReleasable type2 = .PtrToEndArray id2)
    (hrel1 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type1 v1_1 v1_2)
    (hrel2 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type2 v2_1 v2_2)
    (hle : v1_1.toBits l₁ st₁.world.arrayLen ≤ v2_1.toBits l₁ st₁.world.arrayLen)
    (h : (do
      let b ← verifier.MemoryId.Insts.CoreCmpPartialEqMemoryId.eq id1 id2
      if b then
        let id_cloned ← verifier.MemoryId.Insts.CoreCloneClone.clone id1
        let c1 ← verifier.ComputationContext.update_array_bounds context id_cloned offset
        ok (core.result.Result.Ok (type1, type2), c1)
      else ok (core.result.Result.Ok (type1, type2), context) :
        Result ((core.result.Result (verifier.Type × verifier.Type) error.VerifierErrorKind) ×
          verifier.ComputationContext)) =
      ok (.Ok (new1, new2), context1)) :
    CompCtxValid env vCounter context1 ∧
    StateRel2Deps env context1 deps ρ resActive isNullId st₁ st₂ ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new1 v1_1 v1_2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new2 v2_1 v2_2 := by
  have hop1 := unwrap_eq_of_array ht1 hu1; subst hop1
  have hop2 := unwrap_eq_of_endArray ht2 hu2; subst hop2
  rw [bind_ok_eq (MemoryId_eq_spec id1 id2)] at h
  split_ifs at h with heq
  · have hid : id1 = id2 := of_decide_eq_true heq; subst hid
    rw [bind_ok_eq (MemoryId_clone_spec id1)] at h
    rcases bind_eq_ok h with ⟨c1, hupd, h⟩
    rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    rcases hrel1 with ⟨hp, hlen, ox, hconc, rfl, rfl⟩
    rcases hrel2 with ⟨_, rfl, rfl⟩
    have hb := hl₁.2.2.2.1 (renameMemId ρ id1)
    have hox_le : ox ≤ 0xFFFF_FFFF#64 :=
      BitVec.le_trans hconc.1.2.2.2 (BitVec.le_trans hp.2.2 (by decide))
    have hox_len := array_end_le_of_ptr_le hb.1 hb.2 hox_le hlen hle
    have hmin_len := BitVec.le_trans hconc.1.2.2.1 hox_len
    obtain ⟨hc1_valid, hc1_rel⟩ :=
      StateRel2Deps_update_array_bounds hctx ht1.1 hmin_len hupd hrel_ctx
    exact ⟨hc1_valid, hc1_rel, ht1, ht2, ⟨hp, hlen, ox, hconc, rfl, rfl⟩, ⟨hlen, rfl, rfl⟩⟩
  · rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩

private theorem array_end_loose_rhs_sound
    {env : ExecEnv} {vCounter : Nat} {deps : verifier.DataDependencies}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {st₁ st₂ : MachineState} {l₁ : HostLayout}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {id1 id2 : MemoryId} {offset : U64ScalarValueData}
    {v1_1 v1_2 v2_1 v2_2 : RuntimeVal}
    (hl₁ : ValidHostLayout l₁)
    (hctx : CompCtxValid env vCounter context)
    (hrel_ctx : StateRel2Deps env context deps ρ resActive isNullId st₁ st₂)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (hu1 : pureUnwrapReleasable type1 = .PtrToEndArray id1)
    (hu2 : pureUnwrapReleasable type2 = .PtrToArray id2 offset)
    (hrel1 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type1 v1_1 v1_2)
    (hrel2 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type2 v2_1 v2_2)
    (hle : v2_1.toBits l₁ st₁.world.arrayLen ≤ v1_1.toBits l₁ st₁.world.arrayLen)
    (h : (do
      let b ← verifier.MemoryId.Insts.CoreCmpPartialEqMemoryId.eq id1 id2
      if b then
        let id_cloned ← verifier.MemoryId.Insts.CoreCloneClone.clone id1
        let c1 ← verifier.ComputationContext.update_array_bounds context id_cloned offset
        ok (core.result.Result.Ok (type1, type2), c1)
      else ok (core.result.Result.Ok (type1, type2), context) :
        Result ((core.result.Result (verifier.Type × verifier.Type) error.VerifierErrorKind) ×
          verifier.ComputationContext)) =
      ok (.Ok (new1, new2), context1)) :
    CompCtxValid env vCounter context1 ∧
    StateRel2Deps env context1 deps ρ resActive isNullId st₁ st₂ ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new1 v1_1 v1_2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new2 v2_1 v2_2 := by
  have hop1 := unwrap_eq_of_endArray ht1 hu1; subst hop1
  have hop2 := unwrap_eq_of_array ht2 hu2; subst hop2
  rw [bind_ok_eq (MemoryId_eq_spec id1 id2)] at h
  split_ifs at h with heq
  · have hid : id1 = id2 := of_decide_eq_true heq; subst hid
    rw [bind_ok_eq (MemoryId_clone_spec id1)] at h
    rcases bind_eq_ok h with ⟨c1, hupd, h⟩
    rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    rcases hrel1 with ⟨hlen, rfl, rfl⟩
    rcases hrel2 with ⟨hp, _, ox, hconc, rfl, rfl⟩
    have hb := hl₁.2.2.2.1 (renameMemId ρ id1)
    have hox_le : ox ≤ 0xFFFF_FFFF#64 :=
      BitVec.le_trans hconc.1.2.2.2 (BitVec.le_trans hp.2.2 (by decide))
    have hox_len := array_end_le_of_ptr_le hb.1 hb.2 hox_le hlen hle
    have hmin_len := BitVec.le_trans hconc.1.2.2.1 hox_len
    obtain ⟨hc1_valid, hc1_rel⟩ :=
      StateRel2Deps_update_array_bounds hctx ht1 hmin_len hupd hrel_ctx
    exact ⟨hc1_valid, hc1_rel, ht1, ht2, ⟨hlen, rfl, rfl⟩, ⟨hp, hlen, ox, hconc, rfl, rfl⟩⟩
  · rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩

private theorem array_end_strict_lhs_sound
    {env : ExecEnv} {vCounter : Nat} {deps : verifier.DataDependencies}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {st₁ st₂ : MachineState} {l₁ : HostLayout}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {id1 id2 : MemoryId} {offset : U64ScalarValueData}
    {v1_1 v1_2 v2_1 v2_2 : RuntimeVal}
    (hl₁ : ValidHostLayout l₁)
    (hctx : CompCtxValid env vCounter context)
    (hrel_ctx : StateRel2Deps env context deps ρ resActive isNullId st₁ st₂)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (hu1 : pureUnwrapReleasable type1 = .PtrToArray id1 offset)
    (hu2 : pureUnwrapReleasable type2 = .PtrToEndArray id2)
    (hrel1 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type1 v1_1 v1_2)
    (hrel2 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type2 v2_1 v2_2)
    (hlt : v1_1.toBits l₁ st₁.world.arrayLen < v2_1.toBits l₁ st₁.world.arrayLen)
    (h : (do
      let b ← verifier.MemoryId.Insts.CoreCmpPartialEqMemoryId.eq id1 id2
      if b then
        let o ← U64ScalarValueData.checked_add_u64 offset 1#u64
        match o with
        | none =>
          ok (core.result.Result.Err error.VerifierErrorKind.PointerOffsetOverflow, context)
        | some bound =>
          let id_cloned ← verifier.MemoryId.Insts.CoreCloneClone.clone id1
          let c1 ← verifier.ComputationContext.update_array_bounds context id_cloned bound
          ok (core.result.Result.Ok (type1, type2), c1)
      else ok (core.result.Result.Ok (type1, type2), context)) =
      ok (.Ok (new1, new2), context1)) :
    CompCtxValid env vCounter context1 ∧
    StateRel2Deps env context1 deps ρ resActive isNullId st₁ st₂ ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new1 v1_1 v1_2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new2 v2_1 v2_2 := by
  have hop1 := unwrap_eq_of_array ht1 hu1; subst hop1
  have hop2 := unwrap_eq_of_endArray ht2 hu2; subst hop2
  rw [bind_ok_eq (MemoryId_eq_spec id1 id2)] at h
  split_ifs at h with heq
  · have hid : id1 = id2 := of_decide_eq_true heq; subst hid
    rcases hrel1 with ⟨hp, hlen, ox, hconc, rfl, rfl⟩
    rcases hrel2 with ⟨_, rfl, rfl⟩
    obtain ⟨opt_add, hchk_eq, hchk_sound⟩ := checked_add_u64_sound_2safety offset 1#u64 hconc
    rw [bind_ok_eq hchk_eq] at h
    cases opt_add with
    | none => cases (Prod.mk.inj (ok_inj h)).1
    | some bound =>
      dsimp only at h
      rw [bind_ok_eq (MemoryId_clone_spec id1)] at h
      rcases bind_eq_ok h with ⟨c1, hupd, h⟩
      rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
      rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
      have hb := hl₁.2.2.2.1 (renameMemId ρ id1)
      have hox_le : ox ≤ 0xFFFF_FFFF#64 :=
        BitVec.le_trans hconc.1.2.2.2 (BitVec.le_trans hp.2.2 (by decide))
      have hox1_len := array_end_add1_le_of_ptr_lt hb.1 hb.2 hox_le hlen hlt
      have hb_conc := (hchk_sound bound rfl).2.2
      have hmin_len : bound.urange.min.bv ≤ st₁.world.arrayLen (renameMemId ρ id1) :=
        BitVec.le_trans hb_conc.1.2.2.1 hox1_len
      obtain ⟨hc1_valid, hc1_rel⟩ :=
        StateRel2Deps_update_array_bounds hctx ht1.1 hmin_len hupd hrel_ctx
      exact ⟨hc1_valid, hc1_rel, ht1, ht2, ⟨hp, hlen, ox, hconc, rfl, rfl⟩, ⟨hlen, rfl, rfl⟩⟩
  · rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩

private theorem array_end_strict_rhs_sound
    {env : ExecEnv} {vCounter : Nat} {deps : verifier.DataDependencies}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {st₁ st₂ : MachineState} {l₁ : HostLayout}
    {context context1 : verifier.ComputationContext}
    {type1 type2 new1 new2 : verifier.Type}
    {id1 id2 : MemoryId} {offset : U64ScalarValueData}
    {v1_1 v1_2 v2_1 v2_2 : RuntimeVal}
    (hl₁ : ValidHostLayout l₁)
    (hctx : CompCtxValid env vCounter context)
    (hrel_ctx : StateRel2Deps env context deps ρ resActive isNullId st₁ st₂)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (hu1 : pureUnwrapReleasable type1 = .PtrToEndArray id1)
    (hu2 : pureUnwrapReleasable type2 = .PtrToArray id2 offset)
    (hrel1 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type1 v1_1 v1_2)
    (hrel2 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type2 v2_1 v2_2)
    (hlt : v2_1.toBits l₁ st₁.world.arrayLen < v1_1.toBits l₁ st₁.world.arrayLen)
    (h : (do
      let b ← verifier.MemoryId.Insts.CoreCmpPartialEqMemoryId.eq id1 id2
      if b then
        let o ← U64ScalarValueData.checked_add_u64 offset 1#u64
        match o with
        | none =>
          ok (core.result.Result.Err error.VerifierErrorKind.PointerOffsetOverflow, context)
        | some bound =>
          let id_cloned ← verifier.MemoryId.Insts.CoreCloneClone.clone id1
          let c1 ← verifier.ComputationContext.update_array_bounds context id_cloned bound
          ok (core.result.Result.Ok (type1, type2), c1)
      else ok (core.result.Result.Ok (type1, type2), context)) =
      ok (.Ok (new1, new2), context1)) :
    CompCtxValid env vCounter context1 ∧
    StateRel2Deps env context1 deps ρ resActive isNullId st₁ st₂ ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new1 v1_1 v1_2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new2 v2_1 v2_2 := by
  have hop1 := unwrap_eq_of_endArray ht1 hu1; subst hop1
  have hop2 := unwrap_eq_of_array ht2 hu2; subst hop2
  rw [bind_ok_eq (MemoryId_eq_spec id1 id2)] at h
  split_ifs at h with heq
  · have hid : id1 = id2 := of_decide_eq_true heq; subst hid
    rcases hrel1 with ⟨hlen, rfl, rfl⟩
    rcases hrel2 with ⟨hp, _, ox, hconc, rfl, rfl⟩
    obtain ⟨opt_add, hchk_eq, hchk_sound⟩ := checked_add_u64_sound_2safety offset 1#u64 hconc
    rw [bind_ok_eq hchk_eq] at h
    cases opt_add with
    | none => cases (Prod.mk.inj (ok_inj h)).1
    | some bound =>
      dsimp only at h
      rw [bind_ok_eq (MemoryId_clone_spec id1)] at h
      rcases bind_eq_ok h with ⟨c1, hupd, h⟩
      rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
      rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
      have hb := hl₁.2.2.2.1 (renameMemId ρ id1)
      have hox_le : ox ≤ 0xFFFF_FFFF#64 :=
        BitVec.le_trans hconc.1.2.2.2 (BitVec.le_trans hp.2.2 (by decide))
      have hox1_len := array_end_add1_le_of_ptr_lt hb.1 hb.2 hox_le hlen hlt
      have hb_conc := (hchk_sound bound rfl).2.2
      have hmin_len : bound.urange.min.bv ≤ st₁.world.arrayLen (renameMemId ρ id1) :=
        BitVec.le_trans hb_conc.1.2.2.1 hox1_len
      obtain ⟨hc1_valid, hc1_rel⟩ :=
        StateRel2Deps_update_array_bounds hctx ht1 hmin_len hupd hrel_ctx
      exact ⟨hc1_valid, hc1_rel, ht1, ht2, ⟨hlen, rfl, rfl⟩, ⟨hp, hlen, ox, hconc, rfl, rfl⟩⟩
  · rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
    rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
    exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩

theorem Type_constraint_ok_valid
    {env : ExecEnv} {vCounter : Nat}
    {context context1 : verifier.ComputationContext}
    {jump_type : verifier.JumpType} {jump_width : verifier.JumpWidth}
    {type1 type2 new1 new2 : verifier.Type}
    (hctx : CompCtxValid env vCounter context)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (h : verifier.Type.constraint context jump_type jump_width type1 type2 =
      ok (.Ok (new1, new2), context1)) :
    context1.pc = context.pc ∧
    CompCtxValid env vCounter context1 ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 := by
  rw [verifier.Type.constraint.eq_def] at h
  rcases bind_eq_ok h with ⟨r, _, h⟩
  cases r with
  | Err _ => cases (Prod.mk.inj (ok_inj h)).1
  | Ok _ =>
    dsimp only at h
    rcases bind_eq_ok h with ⟨r1, _, h⟩
    cases r1 with
    | Err _ => cases (Prod.mk.inj (ok_inj h)).1
    | Ok _ =>
      dsimp only at h
      rcases bind_eq_ok h with ⟨t, ht_unw, h⟩
      rw [Type_unwrap_releasable_spec] at ht_unw
      have hu1 : pureUnwrapReleasable type1 = t := ok_inj ht_unw
      rcases bind_eq_ok h with ⟨t1, ht1_unw, h⟩
      rw [Type_unwrap_releasable_spec] at ht1_unw
      have hu2 : pureUnwrapReleasable type2 = t1 := ok_inj ht1_unw
      have ht_valid : TypeValid env vCounter t := hu1 ▸ Type_unwrap_releasable_valid ht1
      have ht1_valid : TypeValid env vCounter t1 := hu2 ▸ Type_unwrap_releasable_valid ht2
      cases jump_width with
      | W32 =>
        dsimp only at h
        cases jump_type <;>
          try (rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩;
               rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩;
               exact ⟨rfl, hctx, ht1, ht2⟩)
        dsimp only at h
        cases t <;>
          try (rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩;
               rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩;
               exact ⟨rfl, hctx, ht1, ht2⟩)
        dsimp only at h
        cases t1 <;>
          try (rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩;
               rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩;
               exact ⟨rfl, hctx, ht1, ht2⟩)
        exact constraint_w32_eq_scalars_ok_valid hctx ht1 ht2 h
      | W64 =>
        dsimp only at h
        cases jump_type with
        | Eq =>
          dsimp only at h
          cases t with
          | ScalarValue data1 =>
            dsimp only at h
            cases t1 with
            | ScalarValue data2 => exact constraint_w64_eq_scalars_ok_valid hctx ht1 ht2 h
            | NullOr id inner => exact nullOr_set_zero_valid hctx ht1 ht2 rfl h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨rfl, hctx, ht1, ht2⟩
          | PtrToStack so =>
            dsimp only at h
            cases t1 with
            | PtrToStack so1 =>
              exact Type_constrain_equal_offsets_ok_valid hctx ht1 ht2
                (by unfold typePtrOffset?; rw [hu1]) (by unfold typePtrOffset?; rw [hu2]) h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨rfl, hctx, ht1, ht2⟩
          | PtrToMemory id1 off1 sz1 orig1 =>
            dsimp only at h
            cases t1 with
            | PtrToMemory id2 off2 sz2 orig2 =>
              exact same_id_constrain_offsets_valid hctx ht1 ht2
                (by unfold typePtrOffset?; rw [hu1]) (by unfold typePtrOffset?; rw [hu2]) h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨rfl, hctx, ht1, ht2⟩
          | PtrToStruct id1 off1 sd1 =>
            dsimp only at h
            cases t1 with
            | PtrToStruct id2 off2 sd2 =>
              exact same_id_constrain_offsets_valid hctx ht1 ht2
                (by unfold typePtrOffset?; rw [hu1]) (by unfold typePtrOffset?; rw [hu2]) h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨rfl, hctx, ht1, ht2⟩
          | PtrToArray id1 off1 =>
            dsimp only at h
            cases t1 with
            | PtrToArray id2 off2 =>
              exact same_id_constrain_offsets_valid hctx ht1 ht2
                (by unfold typePtrOffset?; rw [hu1]) (by unfold typePtrOffset?; rw [hu2]) h
            | PtrToEndArray id2 =>
              exact array_bounds_loose_valid hctx ht1 ht2 ht_valid.1 h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨rfl, hctx, ht1, ht2⟩
          | PtrToEndArray id1 =>
            dsimp only at h
            cases t1 with
            | PtrToArray id2 offset =>
              exact array_bounds_loose_valid hctx ht1 ht2 ht_valid h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨rfl, hctx, ht1, ht2⟩
          | NullOr id inner =>
            dsimp only at h
            cases t1 with
            | ScalarValue data => exact nullOr_lhs_set_zero_valid hctx ht1 ht2 rfl h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨rfl, hctx, ht1, ht2⟩
          | _ =>
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            exact ⟨rfl, hctx, ht1, ht2⟩
        | Ge =>
          dsimp only at h
          cases t with
          | ScalarValue lhs =>
            dsimp only at h
            cases t1 with
            | ScalarValue rhs => exact constraint_w64_ge_scalars_ok_valid hctx ht1 ht2 h
            | NullOr id inner => exact nullOr_set_zero_valid hctx ht1 ht2 rfl h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨rfl, hctx, ht1, ht2⟩
          | PtrToEndArray id1 =>
            dsimp only at h
            cases t1 with
            | PtrToArray id2 offset =>
              exact array_bounds_loose_valid hctx ht1 ht2 ht_valid h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨rfl, hctx, ht1, ht2⟩
          | NullOr id inner =>
            dsimp only at h
            cases t1 with
            | ScalarValue data => exact nullOr_lhs_set_nonnull_valid hctx ht1 ht2 hu1 h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨rfl, hctx, ht1, ht2⟩
          | _ =>
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            exact ⟨rfl, hctx, ht1, ht2⟩
        | Gt =>
          dsimp only at h
          cases t with
          | ScalarValue lhs =>
            dsimp only at h
            cases t1 with
            | ScalarValue rhs => exact constraint_w64_gt_scalars_ok_valid hctx ht1 ht2 h
            | NullOr id inner => exact nullOr_rhs_set_nonnull_valid hctx ht1 ht2 hu2 h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨rfl, hctx, ht1, ht2⟩
          | PtrToEndArray id1 =>
            dsimp only at h
            cases t1 with
            | PtrToArray id2 offset =>
              exact array_bounds_strict_valid hctx ht1 ht2 ht_valid h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨rfl, hctx, ht1, ht2⟩
          | NullOr id inner =>
            dsimp only at h
            cases t1 with
            | ScalarValue data => exact nullOr_lhs_set_nonnull_valid hctx ht1 ht2 hu1 h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨rfl, hctx, ht1, ht2⟩
          | _ =>
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            exact ⟨rfl, hctx, ht1, ht2⟩
        | Le =>
          dsimp only at h
          cases t with
          | ScalarValue lhs =>
            dsimp only at h
            cases t1 with
            | ScalarValue rhs => exact constraint_w64_le_scalars_ok_valid hctx ht1 ht2 h
            | NullOr id inner => exact nullOr_rhs_set_nonnull_valid hctx ht1 ht2 hu2 h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨rfl, hctx, ht1, ht2⟩
          | PtrToArray id1 offset =>
            dsimp only at h
            cases t1 with
            | PtrToEndArray id2 =>
              exact array_bounds_loose_valid hctx ht1 ht2 ht_valid.1 h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨rfl, hctx, ht1, ht2⟩
          | NullOr id inner =>
            dsimp only at h
            cases t1 with
            | ScalarValue data => exact nullOr_lhs_set_zero_valid hctx ht1 ht2 rfl h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨rfl, hctx, ht1, ht2⟩
          | _ =>
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            exact ⟨rfl, hctx, ht1, ht2⟩
        | Lt =>
          dsimp only at h
          cases t with
          | ScalarValue lhs =>
            dsimp only at h
            cases t1 with
            | ScalarValue rhs => exact constraint_w64_lt_scalars_ok_valid hctx ht1 ht2 h
            | NullOr id inner => exact nullOr_rhs_set_nonnull_valid hctx ht1 ht2 hu2 h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨rfl, hctx, ht1, ht2⟩
          | PtrToArray id1 offset =>
            dsimp only at h
            cases t1 with
            | PtrToEndArray id2 =>
              exact array_bounds_strict_valid hctx ht1 ht2 ht_valid.1 h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨rfl, hctx, ht1, ht2⟩
          | NullOr id inner =>
            dsimp only at h
            cases t1 with
            | ScalarValue data => exact nullOr_lhs_set_nonnull_valid hctx ht1 ht2 hu1 h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨rfl, hctx, ht1, ht2⟩
          | _ =>
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            exact ⟨rfl, hctx, ht1, ht2⟩
        | LooseComparaison | Ne | StrictComparaison | Unknown =>
          dsimp only at h
          cases t with
          | ScalarValue data =>
            dsimp only at h
            cases t1 with
            | NullOr id inner => exact nullOr_rhs_set_nonnull_valid hctx ht1 ht2 hu2 h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨rfl, hctx, ht1, ht2⟩
          | NullOr id inner =>
            dsimp only at h
            cases t1 with
            | ScalarValue data => exact nullOr_lhs_set_nonnull_valid hctx ht1 ht2 hu1 h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨rfl, hctx, ht1, ht2⟩
          | _ =>
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            exact ⟨rfl, hctx, ht1, ht2⟩

theorem Type_constraint_ok_sound
    {env : ExecEnv} {vCounter : Nat} {deps : verifier.DataDependencies}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {st₁ st₂ : MachineState} {l₁ : HostLayout}
    {context context1 : verifier.ComputationContext}
    {jump_type : verifier.JumpType} {jump_width : verifier.JumpWidth}
    {type1 type2 new1 new2 : verifier.Type}
    {v1_1 v1_2 v2_1 v2_2 : RuntimeVal}
    (hl₁ : ValidHostLayout l₁)
    (hctx : CompCtxValid env vCounter context)
    (hrel_ctx : StateRel2Deps env context deps ρ resActive isNullId st₁ st₂)
    (ht1 : TypeValid env vCounter type1)
    (ht2 : TypeValid env vCounter type2)
    (hrel1 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type1 v1_1 v1_2)
    (hrel2 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen type2 v2_1 v2_2)
    (hjt : JumpTypeHolds jump_type jump_width
      (v1_1.toBits l₁ st₁.world.arrayLen) (v2_1.toBits l₁ st₁.world.arrayLen))
    (h : verifier.Type.constraint context jump_type jump_width type1 type2 =
      ok (.Ok (new1, new2), context1)) :
    CompCtxValid env vCounter context1 ∧
    StateRel2Deps env context1 deps ρ resActive isNullId st₁ st₂ ∧
    TypeValid env vCounter new1 ∧
    TypeValid env vCounter new2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new1 v1_1 v1_2 ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new2 v2_1 v2_2 := by
  rw [verifier.Type.constraint.eq_def] at h
  rcases bind_eq_ok h with ⟨r, _, h⟩
  cases r with
  | Err _ => cases (Prod.mk.inj (ok_inj h)).1
  | Ok _ =>
    dsimp only at h
    rcases bind_eq_ok h with ⟨r1, _, h⟩
    cases r1 with
    | Err _ => cases (Prod.mk.inj (ok_inj h)).1
    | Ok _ =>
      dsimp only at h
      rcases bind_eq_ok h with ⟨t, ht_unw, h⟩
      rw [Type_unwrap_releasable_spec] at ht_unw
      have hu1 : pureUnwrapReleasable type1 = t := ok_inj ht_unw
      rcases bind_eq_ok h with ⟨t1, ht1_unw, h⟩
      rw [Type_unwrap_releasable_spec] at ht1_unw
      have hu2 : pureUnwrapReleasable type2 = t1 := ok_inj ht1_unw
      cases jump_width with
      | W32 =>
        dsimp only at h
        cases jump_type <;>
          try (rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩;
               rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩;
               exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩)
        dsimp only at h
        cases t <;>
          try (rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩;
               rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩;
               exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩)
        dsimp only at h
        cases t1 <;>
          try (rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩;
               rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩;
               exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩)
        exact constraint_w32_eq_scalars_ok_sound hctx hrel_ctx ht1 ht2 hu1 hu2 hrel1 hrel2 hjt h
      | W64 =>
        dsimp only at h
        cases jump_type with
        | Eq =>
          dsimp only at h
          cases t with
          | ScalarValue data1 =>
            dsimp only at h
            cases t1 with
            | ScalarValue data2 =>
              exact constraint_w64_eq_scalars_ok_sound hctx hrel_ctx ht1 ht2 hu1 hu2 hrel1 hrel2 hjt h
            | NullOr id inner =>
              exact nullOr_rhs_set_zero_sound hl₁ hctx hrel_ctx ht1 ht2 hu1 hu2 hrel1 hrel2 rfl
                (fun hz => hjt.symm.trans hz) h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | PtrToStack so =>
            dsimp only at h
            cases t1 with
            | PtrToStack so1 =>
              exact stack_eq_offsets_sound hctx hrel_ctx ht1 ht2 hu1 hu2 hrel1 hrel2 hjt h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | PtrToMemory id1 off1 sz1 orig1 =>
            dsimp only at h
            cases t1 with
            | PtrToMemory id2 off2 sz2 orig2 =>
              exact mem_eq_offsets_sound hctx hrel_ctx ht1 ht2 hu1 hu2 hrel1 hrel2 hjt h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | PtrToStruct id1 off1 sd1 =>
            dsimp only at h
            cases t1 with
            | PtrToStruct id2 off2 sd2 =>
              exact struct_eq_offsets_sound hctx hrel_ctx ht1 ht2 hu1 hu2 hrel1 hrel2 hjt h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | PtrToArray id1 off1 =>
            dsimp only at h
            cases t1 with
            | PtrToArray id2 off2 =>
              exact array_eq_offsets_sound hctx hrel_ctx ht1 ht2 hu1 hu2 hrel1 hrel2 hjt h
            | PtrToEndArray id2 =>
              exact array_end_loose_lhs_sound hl₁ hctx hrel_ctx ht1 ht2 hu1 hu2 hrel1 hrel2
                (hjt ▸ BitVec.le_refl _) h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | PtrToEndArray id1 =>
            dsimp only at h
            cases t1 with
            | PtrToArray id2 offset =>
              exact array_end_loose_rhs_sound hl₁ hctx hrel_ctx ht1 ht2 hu1 hu2 hrel1 hrel2
                (hjt ▸ BitVec.le_refl _) h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | NullOr id inner =>
            dsimp only at h
            cases t1 with
            | ScalarValue data =>
              exact nullOr_lhs_set_zero_sound hl₁ hctx hrel_ctx ht1 ht2 hu1 hu2 hrel1 hrel2 rfl
                (fun hz => hjt.trans hz) h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | _ =>
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
        | Ge =>
          dsimp only at h
          cases t with
          | ScalarValue lhs =>
            dsimp only at h
            cases t1 with
            | ScalarValue rhs =>
              exact constraint_w64_ge_scalars_ok_sound hctx hrel_ctx ht1 ht2 hu1 hu2 hrel1 hrel2 hjt h
            | NullOr id inner =>
              exact nullOr_rhs_set_zero_sound hl₁ hctx hrel_ctx ht1 ht2 hu1 hu2 hrel1 hrel2 rfl
                (fun hz => by change _ ≤ _ at hjt; rw [hz] at hjt; revert hjt; bv_decide) h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | PtrToEndArray id1 =>
            dsimp only at h
            cases t1 with
            | PtrToArray id2 offset =>
              exact array_end_loose_rhs_sound hl₁ hctx hrel_ctx ht1 ht2 hu1 hu2 hrel1 hrel2 hjt h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | NullOr id inner =>
            dsimp only at h
            cases t1 with
            | ScalarValue data =>
              exact nullOr_lhs_set_nonnull_sound (l₁ := l₁) (jt := .Ge) hctx hrel_ctx ht1 ht2 hu1 hu2 hrel1 hrel2
                (fun hs => by cases ok_inj hs) h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | _ =>
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
        | Gt =>
          dsimp only at h
          cases t with
          | ScalarValue lhs =>
            dsimp only at h
            cases t1 with
            | ScalarValue rhs =>
              exact constraint_w64_gt_scalars_ok_sound hctx hrel_ctx ht1 ht2 hu1 hu2 hrel1 hrel2 hjt h
            | NullOr id inner =>
              exact nullOr_rhs_set_nonnull_sound (l₁ := l₁) (jt := .Gt) hctx hrel_ctx ht1 ht2 hu1 hu2 hrel1 hrel2
                (fun _ hz => by change _ < _ at hjt; rw [hz] at hjt; revert hjt; bv_decide) h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | PtrToEndArray id1 =>
            dsimp only at h
            cases t1 with
            | PtrToArray id2 offset =>
              exact array_end_strict_rhs_sound hl₁ hctx hrel_ctx ht1 ht2 hu1 hu2 hrel1 hrel2 hjt h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | NullOr id inner =>
            dsimp only at h
            cases t1 with
            | ScalarValue data =>
              exact nullOr_lhs_set_nonnull_sound (l₁ := l₁) (jt := .Gt) hctx hrel_ctx ht1 ht2 hu1 hu2 hrel1 hrel2
                (fun _ hz => by change _ < _ at hjt; rw [hz] at hjt; revert hjt; bv_decide) h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | _ =>
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
        | Le =>
          dsimp only at h
          cases t with
          | ScalarValue lhs =>
            dsimp only at h
            cases t1 with
            | ScalarValue rhs =>
              exact constraint_w64_le_scalars_ok_sound hctx hrel_ctx ht1 ht2 hu1 hu2 hrel1 hrel2 hjt h
            | NullOr id inner =>
              exact nullOr_rhs_set_nonnull_sound (l₁ := l₁) (jt := .Le) hctx hrel_ctx ht1 ht2 hu1 hu2 hrel1 hrel2
                (fun hs => by cases ok_inj hs) h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | PtrToArray id1 offset =>
            dsimp only at h
            cases t1 with
            | PtrToEndArray id2 =>
              exact array_end_loose_lhs_sound hl₁ hctx hrel_ctx ht1 ht2 hu1 hu2 hrel1 hrel2 hjt h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | NullOr id inner =>
            dsimp only at h
            cases t1 with
            | ScalarValue data =>
              exact nullOr_lhs_set_zero_sound hl₁ hctx hrel_ctx ht1 ht2 hu1 hu2 hrel1 hrel2 rfl
                (fun hz => by change _ ≤ _ at hjt; rw [hz] at hjt; revert hjt; bv_decide) h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | _ =>
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
        | Lt =>
          dsimp only at h
          cases t with
          | ScalarValue lhs =>
            dsimp only at h
            cases t1 with
            | ScalarValue rhs =>
              exact constraint_w64_lt_scalars_ok_sound hctx hrel_ctx ht1 ht2 hu1 hu2 hrel1 hrel2 hjt h
            | NullOr id inner =>
              exact nullOr_rhs_set_nonnull_sound (l₁ := l₁) (jt := .Lt) hctx hrel_ctx ht1 ht2 hu1 hu2 hrel1 hrel2
                (fun _ hz => by change _ < _ at hjt; rw [hz] at hjt; revert hjt; bv_decide) h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | PtrToArray id1 offset =>
            dsimp only at h
            cases t1 with
            | PtrToEndArray id2 =>
              exact array_end_strict_lhs_sound hl₁ hctx hrel_ctx ht1 ht2 hu1 hu2 hrel1 hrel2 hjt h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | NullOr id inner =>
            dsimp only at h
            cases t1 with
            | ScalarValue data =>
              exact nullOr_lhs_set_nonnull_sound (l₁ := l₁) (jt := .Lt) hctx hrel_ctx ht1 ht2 hu1 hu2 hrel1 hrel2
                (fun _ hz => by change _ < _ at hjt; rw [hz] at hjt; revert hjt; bv_decide) h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | _ =>
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
        | Ne =>
          dsimp only at h
          cases t with
          | ScalarValue data =>
            dsimp only at h
            cases t1 with
            | NullOr id inner =>
              exact nullOr_rhs_set_nonnull_sound (l₁ := l₁) (jt := .Ne) hctx hrel_ctx ht1 ht2 hu1 hu2 hrel1 hrel2
                (fun _ hz hc => hjt (hz.trans hc.symm)) h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | NullOr id inner =>
            dsimp only at h
            cases t1 with
            | ScalarValue data =>
              exact nullOr_lhs_set_nonnull_sound (l₁ := l₁) (jt := .Ne) hctx hrel_ctx ht1 ht2 hu1 hu2 hrel1 hrel2
                (fun _ hz hc => hjt (hc.trans hz.symm)) h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | _ =>
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
        | StrictComparaison =>
          dsimp only at h
          cases t with
          | ScalarValue data =>
            dsimp only at h
            cases t1 with
            | NullOr id inner =>
              exact nullOr_rhs_set_nonnull_sound (l₁ := l₁) (jt := .StrictComparaison) hctx hrel_ctx ht1 ht2
                hu1 hu2 hrel1 hrel2 (fun _ hz hc => hjt (hz.trans hc.symm)) h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | NullOr id inner =>
            dsimp only at h
            cases t1 with
            | ScalarValue data =>
              exact nullOr_lhs_set_nonnull_sound (l₁ := l₁) (jt := .StrictComparaison) hctx hrel_ctx ht1 ht2
                hu1 hu2 hrel1 hrel2 (fun _ hz hc => hjt (hc.trans hz.symm)) h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | _ =>
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
        | LooseComparaison =>
          dsimp only at h
          cases t with
          | ScalarValue data =>
            dsimp only at h
            cases t1 with
            | NullOr id inner =>
              exact nullOr_rhs_set_nonnull_sound (l₁ := l₁) (jt := .LooseComparaison) hctx hrel_ctx ht1 ht2
                hu1 hu2 hrel1 hrel2 (fun hs => by cases ok_inj hs) h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | NullOr id inner =>
            dsimp only at h
            cases t1 with
            | ScalarValue data =>
              exact nullOr_lhs_set_nonnull_sound (l₁ := l₁) (jt := .LooseComparaison) hctx hrel_ctx ht1 ht2
                hu1 hu2 hrel1 hrel2 (fun hs => by cases ok_inj hs) h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | _ =>
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
        | Unknown =>
          dsimp only at h
          cases t with
          | ScalarValue data =>
            dsimp only at h
            cases t1 with
            | NullOr id inner =>
              exact nullOr_rhs_set_nonnull_sound (l₁ := l₁) (jt := .Unknown) hctx hrel_ctx ht1 ht2
                hu1 hu2 hrel1 hrel2 (fun hs => by cases ok_inj hs) h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | NullOr id inner =>
            dsimp only at h
            cases t1 with
            | ScalarValue data =>
              exact nullOr_lhs_set_nonnull_sound (l₁ := l₁) (jt := .Unknown) hctx hrel_ctx ht1 ht2
                hu1 hu2 hrel1 hrel2 (fun hs => by cases ok_inj hs) h
            | _ =>
              rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
              rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
              exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩
          | _ =>
            rcases Prod.mk.inj (ok_inj h) with ⟨hok, rfl⟩
            rcases Prod.mk.inj (core.result.Result.Ok.inj hok) with ⟨rfl, rfl⟩
            exact ⟨hctx, hrel_ctx, ht1, ht2, hrel1, hrel2⟩

/-! ## 4. `apply_constraints_and_register`, `conditional_jump`, & Visitor Dispatch -/

theorem computeTargetPc_zero_of_lt {progLen pc : Nat} (hlt : pc + 1 < progLen) :
    computeTargetPc progLen pc 1 0 = some (pc + 1) := by
  unfold computeTargetPc
  dsimp only
  simp only [Int.ofNat_eq_natCast, Nat.cast_add, Nat.cast_one, Int.add_zero]
  have hnlt : ¬ ((pc : Int) + 1 < 0) := by omega
  have htoNat : ((pc : Int) + 1).toNat = pc + 1 := by omega
  simp [hnlt, htoNat, hlt]

theorem StateRel2Deps_jump_pc
    {env : ExecEnv} {self next : verifier.ComputationContext}
    {dPre dPost : verifier.DataDependencies} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {st₁ st₂ : MachineState}
    (hnext_eq : next = { self with pc := next.pc, path_depth := next.path_depth, jmp_seq := next.jmp_seq })
    (hsub : DepsSubset dPost dPre)
    (hrel : StateRel2Deps env self dPre ρ resActive isNullId st₁ st₂) :
    StateRel2Deps env next dPost ρ resActive isNullId
      { st₁ with pc := next.pc.val, world := bumpWorldStep st₁.world }
      { st₂ with pc := next.pc.val, world := bumpWorldStep st₂.world } := by
  rcases hrel with ⟨_, _, hw, harr, hnull, hnodup, hlive, hbnds, hr10_1, hr10_2, hregs, hstk⟩
  rw [hnext_eq]
  exact ⟨rfl, rfl, by simp [bumpWorldStep, hw], harr, hnull, hnodup,
    by simpa [bumpWorldStep] using hlive,
    by simpa [bumpWorldStep] using hbnds,
    hr10_1, hr10_2,
    fun r hr hlen hd => hregs r hr hlen (hsub.1 r hr hd),
    fun s hs hd => hstk s hs (hsub.2 s hs hd)⟩

theorem StateRel2Deps_refine_reg
    {env : ExecEnv} {vCounter : Nat} {ctx ctx' : verifier.ComputationContext}
    {deps : verifier.DataDependencies} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {st₁ st₂ : MachineState}
    {idx : Std.U8} {reg_type : verifier.Type}
    (hctx : CompCtxValid env vCounter ctx)
    (ht : TypeValid env vCounter reg_type)
    (hrel : StateRel2Deps env ctx deps ρ resActive isNullId st₁ st₂)
    (hv_rel : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen reg_type
      (st₁.regs idx.val) (st₂.regs idx.val))
    (hset : verifier.ComputationContext.set_reg ctx idx reg_type = ok (.Ok (), ctx')) :
    ctx'.pc = ctx.pc ∧
    CompCtxValid env vCounter ctx' ∧
    StateRel2Deps env ctx' deps ρ resActive isNullId st₁ st₂ := by
  obtain ⟨_, hpc, hregs_eq, hstk_eq, hbm_eq, hres_eq, hctx'⟩ :=
    ComputationContext_set_reg_ok_spec hctx ht hset
  rcases hrel with ⟨hpc1, hpc2, hw, harr, hnull, hnodup, hlive, hbnds, hr10_1, hr10_2, hregs, hstk⟩
  refine ⟨hpc, hctx', hpc ▸ hpc1, hpc ▸ hpc2, hw, harr, hnull, hnodup,
    hres_eq ▸ hlive, hbm_eq ▸ hbnds, hr10_1, hr10_2, ?_, hstk_eq ▸ hstk⟩
  intro r hr hlen hd
  by_cases hri : r = idx.val
  · subst hri
    have hget : (ctx'.registers.val[idx.val]'hlen) = reg_type := by
      simp [hregs_eq, List.getElem_set_self]
    rw [hget]
    exact hv_rel
  · have hlen_pre : r < ctx.registers.val.length := by rw [array10_len]; exact hr
    have hget : (ctx'.registers.val[r]'hlen) = (ctx.registers.val[r]'hlen_pre) := by
      simp [hregs_eq, List.getElem_set_ne (Ne.symm hri)]
    rw [hget]
    exact hregs r hr hlen_pre hd

private theorem apply_src_reg_ok_valid
    {env : ExecEnv} {vCounter : Nat} {base out : verifier.ComputationContext}
    {src : visitor.Source} {new_op2 : verifier.Type}
    (hbase : CompCtxValid env vCounter base)
    (ht2 : TypeValid env vCounter new_op2)
    (h : (match src with
          | visitor.Source.Reg r1 =>
            if r1 < api.GENERAL_REGISTER_COUNT then
              do
                let (r2, next2) ← verifier.ComputationContext.set_reg base r1 new_op2
                match r2 with
                | core.result.Result.Ok _ => ok (core.result.Result.Ok next2)
                | core.result.Result.Err err => ok (core.result.Result.Err err)
            else ok (core.result.Result.Ok base)
          | visitor.Source.Value _ => ok (core.result.Result.Ok base)) =
         ok (.Ok out)) :
    out.pc = base.pc ∧ CompCtxValid env vCounter out := by
  cases src with
  | Value _ =>
    cases core.result.Result.Ok.inj (ok_inj h)
    exact ⟨rfl, hbase⟩
  | Reg r1 =>
    dsimp only at h
    by_cases hr1 : r1 < api.GENERAL_REGISTER_COUNT
    · simp only [hr1, ↓reduceIte] at h
      rcases bind_eq_ok h with ⟨p, hset, h⟩
      rcases p with ⟨r2, next2⟩
      cases r2 with
      | Err _ => cases (ok_inj h)
      | Ok u0 =>
        cases u0
        cases core.result.Result.Ok.inj (ok_inj h)
        obtain ⟨_, hpc, _, _, _, _, hvalid2⟩ := ComputationContext_set_reg_ok_spec hbase ht2 hset
        exact ⟨hpc, hvalid2⟩
    · simp only [hr1, ↓reduceIte] at h
      cases core.result.Result.Ok.inj (ok_inj h)
      exact ⟨rfl, hbase⟩

private theorem apply_src_reg_ok_sound
    {env : ExecEnv} {vCounter : Nat} {deps : verifier.DataDependencies}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {st₁ st₂ : MachineState} {base out : verifier.ComputationContext}
    {src : visitor.Source} {new_op2 : verifier.Type}
    (hbase : CompCtxValid env vCounter base)
    (hrel_base : StateRel2Deps env base deps ρ resActive isNullId st₁ st₂)
    (ht2 : TypeValid env vCounter new_op2)
    (hrel2 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen new_op2
      (evalOperand st₁ (operandOfSource src)) (evalOperand st₂ (operandOfSource src)))
    (h : (match src with
          | visitor.Source.Reg r1 =>
            if r1 < api.GENERAL_REGISTER_COUNT then
              do
                let (r2, next2) ← verifier.ComputationContext.set_reg base r1 new_op2
                match r2 with
                | core.result.Result.Ok _ => ok (core.result.Result.Ok next2)
                | core.result.Result.Err err => ok (core.result.Result.Err err)
            else ok (core.result.Result.Ok base)
          | visitor.Source.Value _ => ok (core.result.Result.Ok base)) =
         ok (.Ok out)) :
    out.pc = base.pc ∧
    CompCtxValid env vCounter out ∧
    StateRel2Deps env out deps ρ resActive isNullId st₁ st₂ := by
  cases src with
  | Value _ =>
    cases core.result.Result.Ok.inj (ok_inj h)
    exact ⟨rfl, hbase, hrel_base⟩
  | Reg r1 =>
    dsimp only at h
    by_cases hr1 : r1 < api.GENERAL_REGISTER_COUNT
    · simp only [hr1, ↓reduceIte] at h
      rcases bind_eq_ok h with ⟨p, hset, h⟩
      rcases p with ⟨r2, next2⟩
      cases r2 with
      | Err _ => cases (ok_inj h)
      | Ok u0 =>
        cases u0
        cases core.result.Result.Ok.inj (ok_inj h)
        exact StateRel2Deps_refine_reg hbase ht2 hrel_base hrel2 hset
    · simp only [hr1, ↓reduceIte] at h
      cases core.result.Result.Ok.inj (ok_inj h)
      exact ⟨rfl, hbase, hrel_base⟩

theorem apply_constraints_and_register_ok_valid
    {env : ExecEnv} {vCounter : Nat} {next out : verifier.ComputationContext}
    {jump_type : verifier.JumpType} {jump_width : verifier.JumpWidth}
    {dst : Std.U8} {src : visitor.Source} {op1 op2 : verifier.Type}
    (hctx : CompCtxValid env vCounter next)
    (ht1 : TypeValid env vCounter op1)
    (ht2 : TypeValid env vCounter op2)
    (h : verifier.ComputationContext.apply_constraints_and_register next
      jump_type jump_width dst src op1 op2 = ok (.Ok out)) :
    out.pc = next.pc ∧ CompCtxValid env vCounter out := by
  unfold verifier.ComputationContext.apply_constraints_and_register at h
  rw [bind_ok_eq (JumpType_ne_unknown_eq_ok jump_type)] at h
  cases hb : pureJumpTypeNeUnknown jump_type
  · rw [hb] at h
    simp only [Bool.false_eq_true, ↓reduceIte] at h
    cases core.result.Result.Ok.inj (ok_inj h)
    exact ⟨rfl, hctx⟩
  · rw [hb] at h
    simp only [↓reduceIte] at h
    rcases bind_eq_ok h with ⟨p, hcon, h⟩
    rcases p with ⟨r, next1⟩
    cases r with
    | Err _ => cases (ok_inj h)
    | Ok pair =>
      rcases pair with ⟨new_op1, new_op2⟩
      obtain ⟨hpc1, hctx1, hnew1, hnew2⟩ := Type_constraint_ok_valid hctx ht1 ht2 hcon
      by_cases hdst : dst < api.GENERAL_REGISTER_COUNT
      · simp only [hdst, ↓reduceIte] at h
        rcases bind_eq_ok h with ⟨p1, hset1, h⟩
        rcases p1 with ⟨r1, next2⟩
        cases r1 with
        | Err _ => cases (ok_inj h)
        | Ok u0 =>
          cases u0
          obtain ⟨_, hpc2, _, _, _, _, hctx2⟩ := ComputationContext_set_reg_ok_spec hctx1 hnew1 hset1
          obtain ⟨hpc3, hctx3⟩ := apply_src_reg_ok_valid hctx2 hnew2 h
          exact ⟨hpc3.trans (hpc2.trans hpc1), hctx3⟩
      · simp only [hdst, ↓reduceIte] at h
        obtain ⟨hpc2, hctx2⟩ := apply_src_reg_ok_valid hctx1 hnew2 h
        exact ⟨hpc2.trans hpc1, hctx2⟩

theorem apply_constraints_and_register_ok_sound
    {env : ExecEnv} {vCounter : Nat} {deps : verifier.DataDependencies}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {st₁ st₂ : MachineState} {l₁ : HostLayout}
    {next out : verifier.ComputationContext}
    {jump_type : verifier.JumpType} {jump_width : verifier.JumpWidth}
    {dst : Std.U8} {src : visitor.Source} {op1 op2 : verifier.Type}
    (hl₁ : ValidHostLayout l₁)
    (hctx : CompCtxValid env vCounter next)
    (hrel_ctx : StateRel2Deps env next deps ρ resActive isNullId st₁ st₂)
    (ht1 : TypeValid env vCounter op1)
    (ht2 : TypeValid env vCounter op2)
    (hrel1 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen op1
      (st₁.regs dst.val) (st₂.regs dst.val))
    (hrel2 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen op2
      (evalOperand st₁ (operandOfSource src)) (evalOperand st₂ (operandOfSource src)))
    (hjt : JumpTypeHolds jump_type jump_width
      ((st₁.regs dst.val).toBits l₁ st₁.world.arrayLen)
      ((evalOperand st₁ (operandOfSource src)).toBits l₁ st₁.world.arrayLen))
    (h : verifier.ComputationContext.apply_constraints_and_register next
      jump_type jump_width dst src op1 op2 = ok (.Ok out)) :
    out.pc = next.pc ∧
    CompCtxValid env vCounter out ∧
    StateRel2Deps env out deps ρ resActive isNullId st₁ st₂ := by
  obtain ⟨hpc_out, _⟩ := apply_constraints_and_register_ok_valid hctx ht1 ht2 h
  unfold verifier.ComputationContext.apply_constraints_and_register at h
  rw [bind_ok_eq (JumpType_ne_unknown_eq_ok jump_type)] at h
  cases hb : pureJumpTypeNeUnknown jump_type
  · rw [hb] at h
    simp only [Bool.false_eq_true, ↓reduceIte] at h
    cases core.result.Result.Ok.inj (ok_inj h)
    exact ⟨rfl, hctx, hrel_ctx⟩
  · rw [hb] at h
    simp only [↓reduceIte] at h
    rcases bind_eq_ok h with ⟨p, hcon, h⟩
    rcases p with ⟨r, next1⟩
    cases r with
    | Err _ => cases (ok_inj h)
    | Ok pair =>
      rcases pair with ⟨new_op1, new_op2⟩
      obtain ⟨hctx1, hrel_ctx1, hnew1, hnew2, hrel_new1, hrel_new2⟩ :=
        Type_constraint_ok_sound hl₁ hctx hrel_ctx ht1 ht2 hrel1 hrel2 hjt hcon
      by_cases hdst : dst < api.GENERAL_REGISTER_COUNT
      · simp only [hdst, ↓reduceIte] at h
        rcases bind_eq_ok h with ⟨p1, hset1, h⟩
        rcases p1 with ⟨r1, next2⟩
        cases r1 with
        | Err _ => cases (ok_inj h)
        | Ok u0 =>
          cases u0
          obtain ⟨_, hctx2, hrel_ctx2⟩ :=
            StateRel2Deps_refine_reg hctx1 hnew1 hrel_ctx1 hrel_new1 hset1
          obtain ⟨_, hctx3, hrel_ctx3⟩ :=
            apply_src_reg_ok_sound hctx2 hrel_ctx2 hnew2 hrel_new2 h
          exact ⟨hpc_out, hctx3, hrel_ctx3⟩
      · simp only [hdst, ↓reduceIte] at h
        obtain ⟨_, hctx2, hrel_ctx2⟩ :=
          apply_src_reg_ok_sound hctx1 hrel_ctx1 hnew2 hrel_new2 h
        exact ⟨hpc_out, hctx2, hrel_ctx2⟩

private theorem skip_branch_forward
    {env : ExecEnv} {ctx : verifier.ComputationContext}
    {vctx0 vc_base vc_out : verifier.VerificationContext}
    {is64 : Bool} {op : JumpOp} {dst8 : Std.U8} {vsrc : visitor.Source}
    {v v1 : verifier.Type}
    (hctx : CompCtxValid env vctx0.counter.bv.toNat ctx)
    (hv_valid : TypeValid env vctx0.counter.bv.toNat v)
    (hv1_valid : TypeValid env vctx0.counter.bv.toNat v1)
    (hbase_post : ForwardVisitPost env vctx0 vc_base ctx)
    (h : (do
      let r5 ← verifier.ComputationContext.jump_with_offset ctx 0#i16
      match r5 with
      | core.result.Result.Ok f =>
        let i ← verifier.saturating_inc_u32 f.jmp_seq
        let jt ← verifier.JumpType.invert (jumpTypeOfOp op)
        let r6 ←
          verifier.ComputationContext.apply_constraints_and_register { f with jmp_seq := i } jt
            (jumpWidthOfBool is64) dst8 vsrc v v1
        match r6 with
        | core.result.Result.Ok c1 =>
          let verification_context3 ← verifier.VerificationContext.push_successor vc_base c1
          ok (core.result.Result.Ok (), verification_context3)
        | core.result.Result.Err err => ok (core.result.Result.Err err, vc_base)
      | core.result.Result.Err err => ok (core.result.Result.Err err, vc_base)) =
      ok (.Ok (), vc_out)) :
    ForwardVisitPost env vctx0 vc_out ctx := by
  rcases bind_eq_ok h with ⟨r5, hjmp0, h⟩
  cases r5 with
  | Err _ => cases (Prod.mk.inj (ok_inj h)).1
  | Ok f =>
    dsimp only at h
    rcases bind_eq_ok h with ⟨i_jmp, _, h⟩
    rw [bind_ok_eq (JumpType_invert_eq_ok (jumpTypeOfOp op))] at h
    rcases bind_eq_ok h with ⟨r6, happly, h⟩
    cases r6 with
    | Err _ => cases (Prod.mk.inj (ok_inj h)).1
    | Ok c1 =>
      dsimp only at h
      rcases bind_eq_ok h with ⟨vc3, hpush, h⟩
      rcases Prod.mk.inj (ok_inj h) with ⟨_, rfl⟩
      obtain ⟨_, _, hf_eq⟩ := ComputationContext_jump_with_offset_ok_spec hjmp0
      have hf_valid : CompCtxValid env vctx0.counter.bv.toNat { f with jmp_seq := i_jmp } := by
        change CompCtxValid env vctx0.counter.bv.toNat f
        rw [hf_eq]; exact hctx
      obtain ⟨_, hc1_valid⟩ :=
        apply_constraints_and_register_ok_valid hf_valid hv_valid hv1_valid happly
      obtain ⟨hp1, hp2, hp3, hp4, hp5, _, hp7⟩ := VerificationContext_push_successor_ok_spec hpush
      rcases hbase_post with ⟨hb1, hb2, hb3, hb4, hb5, hb6, hb7⟩
      refine ⟨hp1.trans hb1, hp2.trans hb2, hp4.trans hb3, hp5.trans hb4, ?_, ?_, ?_⟩
      · rw [hp3]; exact hb5
      · rw [hp3]; exact hb6
      · intro opt hopt
        rw [hp7, List.mem_append] at hopt
        rcases hopt with hmem | hmem
        · rw [hp3]; exact hb7 opt hmem
        · rw [List.mem_singleton] at hmem
          subst hmem
          exact ⟨c1, rfl, CompCtxValid_mono (hp3 ▸ hb5) hc1_valid⟩

theorem visit_condJump_forward
    {env : ExecEnv} {ctx ctx_out : verifier.ComputationContext}
    {vctx0 vctx1 : verifier.VerificationContext}
    {is64 : Bool} {op : JumpOp} {dst : Nat} {src : Operand} {offInt : Int}
    (hctx : CompCtxValid env vctx0.counter.bv.toNat ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hfwd : VisitorDispatchNonLddw CompCtxVisitorInst ctx vctx0
      (.ok (.Ok (), ctx_out, vctx1)) (.condJump is64 op dst src offInt)) :
    ctx_out = ctx ∧ ForwardVisitPost env vctx0 vctx1 ctx_out := by
  rcases hfwd with ⟨dst8, vsrc, off, _, _, _, hcall⟩
  have hcall' := hcall.symm
  rw [condJumpMethod_compCtx_eq] at hcall'
  rcases bind_eq_ok hcall' with ⟨p_out, hcj, hcall'⟩
  rcases p_out with ⟨res_out, vc_out⟩
  rcases Prod.mk.inj (ok_inj hcall') with ⟨rfl, hpair⟩
  rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
  refine ⟨rfl, ?_⟩
  unfold verifier.ComputationContext.conditional_jump at hcj
  rcases bind_eq_ok hcj with ⟨vc1, hemit, hcj⟩
  obtain ⟨he1, he2, he3, he4, he5, he6⟩ := VerificationContext_emit_log_ok_spec hemit
  have hvc1_post : ForwardVisitPost env vctx0 vc1 ctx := by
    refine ⟨he1, he2, he4, he5, by rw [he3], by rwa [he3], ?_⟩
    intro opt hopt
    rw [he6, hpending] at hopt
    cases hopt
  rcases bind_eq_ok hcj with ⟨r, hreg, hcj⟩
  cases r with
  | Err _ => cases (Prod.mk.inj (ok_inj hcj)).1
  | Ok v =>
    dsimp only at hcj
    obtain ⟨_, hv_valid⟩ := ComputationContext_reg_ok_valid hctx hreg
    rcases bind_eq_ok hcj with ⟨r1, hsrc, hcj⟩
    cases r1 with
    | Err _ => cases (Prod.mk.inj (ok_inj hcj)).1
    | Ok v1 =>
      dsimp only at hcj
      obtain ⟨_, hv1_valid⟩ := ComputationContext_compute_source_ok_valid hctx hsrc
      rcases bind_eq_ok hcj with ⟨r2, _, hcj⟩
      cases r2 with
      | Err _ => cases (Prod.mk.inj (ok_inj hcj)).1
      | Ok b =>
        dsimp only at hcj
        cases b with
        | none =>
          simp only [bind_ok, ↓reduceIte] at hcj
          rcases bind_eq_ok hcj with ⟨r3, hjmp, hcj⟩
          cases r3 with
          | Err _ => cases (Prod.mk.inj (ok_inj hcj)).1
          | Ok j =>
            dsimp only at hcj
            rw [bind_ok_eq (Type_clone_spec v), bind_ok_eq (Type_clone_spec v1)] at hcj
            rcases bind_eq_ok hcj with ⟨r4, happly, hcj⟩
            cases r4 with
            | Err _ => cases (Prod.mk.inj (ok_inj hcj)).1
            | Ok c =>
              dsimp only at hcj
              rcases bind_eq_ok hcj with ⟨vc2, hpush, hcj⟩
              obtain ⟨_, hj_eq⟩ := ComputationContext_checked_jump_with_offset_ok_spec hjmp
              have hj_valid : CompCtxValid env vctx0.counter.bv.toNat j := by rw [hj_eq]; exact hctx
              obtain ⟨_, hc_valid⟩ :=
                apply_constraints_and_register_ok_valid hj_valid hv_valid hv1_valid happly
              obtain ⟨hp1, hp2, hp3, hp4, hp5, _, hp7⟩ := VerificationContext_push_successor_ok_spec hpush
              have hcnt2 : vc2.counter = vctx0.counter := hp3.trans he3
              have hvc2_post : ForwardVisitPost env vctx0 vc2 ctx := by
                refine ⟨hp1.trans he1, hp2.trans he2, hp4.trans he4, hp5.trans he5,
                  by rw [hcnt2], by rwa [hcnt2], ?_⟩
                intro opt hopt
                rw [hp7, he6, hpending, List.nil_append, List.mem_singleton] at hopt
                subst hopt
                exact ⟨c, rfl, by rwa [hcnt2]⟩
              exact skip_branch_forward hctx hv_valid hv1_valid hvc2_post hcj
        | some b1 =>
          cases b1 with
          | true =>
            simp only [bind_ok, ↓reduceIte] at hcj
            rcases bind_eq_ok hcj with ⟨r3, hjmp, hcj⟩
            cases r3 with
            | Err _ => cases (Prod.mk.inj (ok_inj hcj)).1
            | Ok j =>
              dsimp only at hcj
              rw [bind_ok_eq (Type_clone_spec v), bind_ok_eq (Type_clone_spec v1)] at hcj
              rcases bind_eq_ok hcj with ⟨r4, happly, hcj⟩
              cases r4 with
              | Err _ => cases (Prod.mk.inj (ok_inj hcj)).1
              | Ok c =>
                dsimp only at hcj
                rcases bind_eq_ok hcj with ⟨vc2, hpush, hcj⟩
                rcases Prod.mk.inj (ok_inj hcj) with ⟨_, rfl⟩
                obtain ⟨_, hj_eq⟩ := ComputationContext_checked_jump_with_offset_ok_spec hjmp
                have hj_valid : CompCtxValid env vctx0.counter.bv.toNat j := by rw [hj_eq]; exact hctx
                obtain ⟨_, hc_valid⟩ :=
                  apply_constraints_and_register_ok_valid hj_valid hv_valid hv1_valid happly
                obtain ⟨hp1, hp2, hp3, hp4, hp5, _, hp7⟩ := VerificationContext_push_successor_ok_spec hpush
                have hcnt2 : vc2.counter = vctx0.counter := hp3.trans he3
                refine ⟨hp1.trans he1, hp2.trans he2, hp4.trans he4, hp5.trans he5,
                  by rw [hcnt2], by rwa [hcnt2], ?_⟩
                intro opt hopt
                rw [hp7, he6, hpending, List.nil_append, List.mem_singleton] at hopt
                subst hopt
                exact ⟨c, rfl, by rwa [hcnt2]⟩
          | false =>
            simp only [bind_ok] at hcj
            exact skip_branch_forward hctx hv_valid hv1_valid hvc1_post hcj

private theorem condJump_step_taken
    {prog : Program} {env : ExecEnv} {vCounter F : Nat}
    {ctx j c : verifier.ComputationContext}
    {dd dPost : verifier.DataDependencies}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {l₁ l₂ : HostLayout} {s₁ s₂ : SecretInit} {st₁ st₂ : MachineState}
    {is64 : Bool} {op : JumpOp} {dst8 : Std.U8} {vsrc : visitor.Source} {off : Std.I16}
    {v v1 : verifier.Type}
    (hlt_pc : ctx.pc.val < prog.size)
    (hinsn_at : prog[ctx.pc.val]'hlt_pc = .condJump is64 op dst8.val (operandOfSource vsrc) off.val)
    (hdst_lt : dst8.val < REGISTER_COUNT)
    (hsrc_reg : ∀ r, operandOfSource vsrc = .reg r → r < REGISTER_COUNT)
    (hctx : CompCtxValid env vCounter ctx)
    (hv_valid : TypeValid env vCounter v)
    (hv1_valid : TypeValid env vCounter v1)
    (hsub : DepsSubset dPost dd)
    (hl₁ : ValidHostLayout l₁) (hl₂ : ValidHostLayout l₂)
    (hleak : (!env.allowPtrLeaks) = true ∨ l₁ = l₂ ∧ s₁ = s₂)
    (halloc : st₁.world.nextAllocId + 3 * (F + 1) < 2 ^ 63)
    (hρ : RhoValid vCounter st₁.world.nextAllocId ρ)
    (hrel : StateRel2Deps env ctx dd ρ resActive isNullId st₁ st₂)
    (hrel1 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen v
      (st₁.regs dst8.val) (st₂.regs dst8.val))
    (hrel2 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen v1
      (evalOperand st₁ (operandOfSource vsrc)) (evalOperand st₂ (operandOfSource vsrc)))
    (hca1_1 : checkAlive st₁.world.liveResources (st₁.regs dst8.val) = none)
    (hca1_2 : checkAlive st₂.world.liveResources (st₂.regs dst8.val) = none)
    (hca2_1 : checkAlive st₁.world.liveResources (evalOperand st₁ (operandOfSource vsrc)) = none)
    (hca2_2 : checkAlive st₂.world.liveResources (evalOperand st₂ (operandOfSource vsrc)) = none)
    (hcond₁ : (if is64 then
                 evalJump64 op ((st₁.regs dst8.val).toBits l₁ st₁.world.arrayLen)
                   ((evalOperand st₁ (operandOfSource vsrc)).toBits l₁ st₁.world.arrayLen)
               else
                 evalJump32 op ((st₁.regs dst8.val).toBits l₁ st₁.world.arrayLen)
                   ((evalOperand st₁ (operandOfSource vsrc)).toBits l₁ st₁.world.arrayLen)) = true)
    (hcond₂ : (if is64 then
                 evalJump64 op ((st₂.regs dst8.val).toBits l₂ st₂.world.arrayLen)
                   ((evalOperand st₂ (operandOfSource vsrc)).toBits l₂ st₂.world.arrayLen)
               else
                 evalJump32 op ((st₂.regs dst8.val).toBits l₂ st₂.world.arrayLen)
                   ((evalOperand st₂ (operandOfSource vsrc)).toBits l₂ st₂.world.arrayLen)) = true)
    (htgt : computeTargetPc prog.size ctx.pc.val 1 off.val = some j.pc.val)
    (hj_eq : j = { ctx with pc := j.pc, path_depth := j.path_depth, jmp_seq := j.jmp_seq })
    (happly : verifier.ComputationContext.apply_constraints_and_register j
      (jumpTypeOfOp op) (jumpWidthOfBool is64) dst8 vsrc v v1 = ok (.Ok c))
    (hsem_c : SemTerminatesIn prog env vCounter c dPost F) :
    ∃ (ret : BitVec 64) (finalWorld : WorldState),
      runSteps prog env l₁ s₁ (F + 1) st₁ = some (.inl (ret, finalWorld)) ∧
      runSteps prog env l₂ s₂ (F + 1) st₂ = some (.inl (ret, finalWorld)) ∧
      finalWorld.liveResources = [] := by
  have hj_valid : CompCtxValid env vCounter j := by rw [hj_eq]; exact hctx
  let st₁_j : MachineState := { st₁ with pc := j.pc.val, world := bumpWorldStep st₁.world }
  let st₂_j : MachineState := { st₂ with pc := j.pc.val, world := bumpWorldStep st₂.world }
  have hrel_j : StateRel2Deps env j dPost ρ resActive isNullId st₁_j st₂_j :=
    StateRel2Deps_jump_pc hj_eq hsub hrel
  have hjt : JumpTypeHolds (jumpTypeOfOp op) (jumpWidthOfBool is64)
      ((st₁_j.regs dst8.val).toBits l₁ st₁_j.world.arrayLen)
      ((evalOperand st₁_j (operandOfSource vsrc)).toBits l₁ st₁_j.world.arrayLen) := by
    cases vsrc <;> exact JumpTypeHolds_taken is64 op _ _ hcond₁
  have hrel2_j : TypeRel2 env ρ resActive isNullId st₁_j.world.arrayLen v1
      (evalOperand st₁_j (operandOfSource vsrc)) (evalOperand st₂_j (operandOfSource vsrc)) := by
    cases vsrc <;> exact hrel2
  obtain ⟨hpc_cj, _, hrel_c⟩ :=
    apply_constraints_and_register_ok_sound hl₁ hj_valid hrel_j hv_valid hv1_valid
      hrel1 hrel2_j hjt happly
  have halloc' : st₁_j.world.nextAllocId + 3 * F < 2 ^ 63 := by
    dsimp only [st₁_j, bumpWorldStep]; omega
  obtain ⟨ret, finalWorld, hr₁, hr₂, hlive_fin⟩ :=
    hsem_c l₁ l₂ s₁ s₂ ρ resActive isNullId st₁_j st₂_j hl₁ hl₂ hleak halloc' hρ hrel_c
  have hn_ge : ¬ (dst8.val ≥ REGISTER_COUNT) := by omega
  refine ⟨ret, finalWorld, ?_, ?_, hlive_fin⟩
  · unfold runSteps step
    rcases hop : operandOfSource vsrc with r | imm
    · have hca2 : checkAlive st₁.world.liveResources (st₁.regs r) = none := by
        simpa only [hop, evalOperand] using hca2_1
      have hc1 : (if is64 then
                    evalJump64 op ((st₁.regs dst8.val).toBits l₁ st₁.world.arrayLen)
                      ((st₁.regs r).toBits l₁ st₁.world.arrayLen)
                  else
                    evalJump32 op ((st₁.regs dst8.val).toBits l₁ st₁.world.arrayLen)
                      ((st₁.regs r).toBits l₁ st₁.world.arrayLen)) = true := by
        simpa only [hop, evalOperand] using hcond₁
      simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_ge, ↓reduceIte,
        hop, hsrc_reg r hop, hca1_1, hca2, hc1, htgt]
      rw [stepNextIte_eq_next rfl hr₁]
      exact hr₁
    · have hca2 : checkAlive st₁.world.liveResources (.scalar imm) = none := by
        simpa only [hop, evalOperand] using hca2_1
      have hc1 : (if is64 then
                    evalJump64 op ((st₁.regs dst8.val).toBits l₁ st₁.world.arrayLen)
                      ((RuntimeVal.scalar imm).toBits l₁ st₁.world.arrayLen)
                  else
                    evalJump32 op ((st₁.regs dst8.val).toBits l₁ st₁.world.arrayLen)
                      ((RuntimeVal.scalar imm).toBits l₁ st₁.world.arrayLen)) = true := by
        simpa only [hop, evalOperand] using hcond₁
      simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_ge, ↓reduceIte,
        hop, hca1_1, hca2, hc1, htgt]
      rw [stepNextIte_eq_next rfl hr₁]
      exact hr₁
  · unfold runSteps step
    rcases hop : operandOfSource vsrc with r | imm
    · have hca2 : checkAlive st₂.world.liveResources (st₂.regs r) = none := by
        simpa only [hop, evalOperand] using hca2_2
      have hc2 : (if is64 then
                    evalJump64 op ((st₂.regs dst8.val).toBits l₂ st₂.world.arrayLen)
                      ((st₂.regs r).toBits l₂ st₂.world.arrayLen)
                  else
                    evalJump32 op ((st₂.regs dst8.val).toBits l₂ st₂.world.arrayLen)
                      ((st₂.regs r).toBits l₂ st₂.world.arrayLen)) = true := by
        simpa only [hop, evalOperand] using hcond₂
      simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_ge, ↓reduceIte,
        hop, hsrc_reg r hop, hca1_2, hca2, hc2, htgt]
      rw [stepNextIte_eq_next rfl hr₂]
      exact hr₂
    · have hca2 : checkAlive st₂.world.liveResources (.scalar imm) = none := by
        simpa only [hop, evalOperand] using hca2_2
      have hc2 : (if is64 then
                    evalJump64 op ((st₂.regs dst8.val).toBits l₂ st₂.world.arrayLen)
                      ((RuntimeVal.scalar imm).toBits l₂ st₂.world.arrayLen)
                  else
                    evalJump32 op ((st₂.regs dst8.val).toBits l₂ st₂.world.arrayLen)
                      ((RuntimeVal.scalar imm).toBits l₂ st₂.world.arrayLen)) = true := by
        simpa only [hop, evalOperand] using hcond₂
      simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_ge, ↓reduceIte,
        hop, hca1_2, hca2, hc2, htgt]
      rw [stepNextIte_eq_next rfl hr₂]
      exact hr₂

private theorem condJump_step_skipped
    {prog : Program} {env : ExecEnv} {vCounter F : Nat}
    {ctx f c1 : verifier.ComputationContext}
    {dd dPost : verifier.DataDependencies}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {l₁ l₂ : HostLayout} {s₁ s₂ : SecretInit} {st₁ st₂ : MachineState}
    {is64 : Bool} {op : JumpOp} {dst8 : Std.U8} {vsrc : visitor.Source} {off : Std.I16}
    {v v1 : verifier.Type}
    (hlt_pc : ctx.pc.val < prog.size)
    (hinsn_at : prog[ctx.pc.val]'hlt_pc = .condJump is64 op dst8.val (operandOfSource vsrc) off.val)
    (hdst_lt : dst8.val < REGISTER_COUNT)
    (hsrc_reg : ∀ r, operandOfSource vsrc = .reg r → r < REGISTER_COUNT)
    (hctx : CompCtxValid env vCounter ctx)
    (hv_valid : TypeValid env vCounter v)
    (hv1_valid : TypeValid env vCounter v1)
    (hsub : DepsSubset dPost dd)
    (hl₁ : ValidHostLayout l₁) (hl₂ : ValidHostLayout l₂)
    (hleak : (!env.allowPtrLeaks) = true ∨ l₁ = l₂ ∧ s₁ = s₂)
    (halloc : st₁.world.nextAllocId + 3 * (F + 1) < 2 ^ 63)
    (hρ : RhoValid vCounter st₁.world.nextAllocId ρ)
    (hrel : StateRel2Deps env ctx dd ρ resActive isNullId st₁ st₂)
    (hrel1 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen v
      (st₁.regs dst8.val) (st₂.regs dst8.val))
    (hrel2 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen v1
      (evalOperand st₁ (operandOfSource vsrc)) (evalOperand st₂ (operandOfSource vsrc)))
    (hca1_1 : checkAlive st₁.world.liveResources (st₁.regs dst8.val) = none)
    (hca1_2 : checkAlive st₂.world.liveResources (st₂.regs dst8.val) = none)
    (hca2_1 : checkAlive st₁.world.liveResources (evalOperand st₁ (operandOfSource vsrc)) = none)
    (hca2_2 : checkAlive st₂.world.liveResources (evalOperand st₂ (operandOfSource vsrc)) = none)
    (hcond₁ : (if is64 then
                 evalJump64 op ((st₁.regs dst8.val).toBits l₁ st₁.world.arrayLen)
                   ((evalOperand st₁ (operandOfSource vsrc)).toBits l₁ st₁.world.arrayLen)
               else
                 evalJump32 op ((st₁.regs dst8.val).toBits l₁ st₁.world.arrayLen)
                   ((evalOperand st₁ (operandOfSource vsrc)).toBits l₁ st₁.world.arrayLen)) = false)
    (hcond₂ : (if is64 then
                 evalJump64 op ((st₂.regs dst8.val).toBits l₂ st₂.world.arrayLen)
                   ((evalOperand st₂ (operandOfSource vsrc)).toBits l₂ st₂.world.arrayLen)
               else
                 evalJump32 op ((st₂.regs dst8.val).toBits l₂ st₂.world.arrayLen)
                   ((evalOperand st₂ (operandOfSource vsrc)).toBits l₂ st₂.world.arrayLen)) = false)
    (hjmp0 : verifier.ComputationContext.jump_with_offset ctx 0#i16 = ok (.Ok f))
    {i_jmp : Std.U32}
    (happly : verifier.ComputationContext.apply_constraints_and_register { f with jmp_seq := i_jmp }
      (pureInvertJumpType (jumpTypeOfOp op)) (jumpWidthOfBool is64) dst8 vsrc v v1 = ok (.Ok c1))
    (hsem_c1 : SemTerminatesIn prog env vCounter c1 dPost F) :
    ∃ (ret : BitVec 64) (finalWorld : WorldState),
      runSteps prog env l₁ s₁ (F + 1) st₁ = some (.inl (ret, finalWorld)) ∧
      runSteps prog env l₂ s₂ (F + 1) st₂ = some (.inl (ret, finalWorld)) ∧
      finalWorld.liveResources = [] := by
  obtain ⟨_, hf_pc_int, hf_eq⟩ := ComputationContext_jump_with_offset_ok_spec hjmp0
  have h0_val : (0#i16).val = 0 := rfl
  rw [h0_val, Int.add_zero] at hf_pc_int
  have hf_pc : f.pc.val = ctx.pc.val + 1 := Int.ofNat.inj hf_pc_int
  have hf_valid : CompCtxValid env vCounter { f with jmp_seq := i_jmp } := by
    change CompCtxValid env vCounter f
    rw [hf_eq]; exact hctx
  let st₁_f : MachineState := { st₁ with pc := f.pc.val, world := bumpWorldStep st₁.world }
  let st₂_f : MachineState := { st₂ with pc := f.pc.val, world := bumpWorldStep st₂.world }
  have hrel_f : StateRel2Deps env { f with jmp_seq := i_jmp } dPost ρ resActive isNullId st₁_f st₂_f := by
    change StateRel2Deps env f dPost ρ resActive isNullId st₁_f st₂_f
    exact StateRel2Deps_jump_pc hf_eq hsub hrel
  have hjt : JumpTypeHolds (pureInvertJumpType (jumpTypeOfOp op)) (jumpWidthOfBool is64)
      ((st₁_f.regs dst8.val).toBits l₁ st₁_f.world.arrayLen)
      ((evalOperand st₁_f (operandOfSource vsrc)).toBits l₁ st₁_f.world.arrayLen) := by
    cases vsrc <;> exact JumpTypeHolds_skipped is64 op _ _ hcond₁
  have hrel2_f : TypeRel2 env ρ resActive isNullId st₁_f.world.arrayLen v1
      (evalOperand st₁_f (operandOfSource vsrc)) (evalOperand st₂_f (operandOfSource vsrc)) := by
    cases vsrc <;> exact hrel2
  obtain ⟨_, _, hrel_c1⟩ :=
    apply_constraints_and_register_ok_sound hl₁ hf_valid hrel_f hv_valid hv1_valid
      hrel1 hrel2_f hjt happly
  have halloc' : st₁_f.world.nextAllocId + 3 * F < 2 ^ 63 := by
    dsimp only [st₁_f, bumpWorldStep]; omega
  obtain ⟨ret, finalWorld, hr₁, hr₂, hlive_fin⟩ :=
    hsem_c1 l₁ l₂ s₁ s₂ ρ resActive isNullId st₁_f st₂_f hl₁ hl₂ hleak halloc' hρ hrel_c1
  obtain ⟨_, hpc_next_lt, _⟩ := runSteps_pc_valid hr₁
  dsimp only [st₁_f] at hpc_next_lt
  rw [hf_pc] at hpc_next_lt
  have htgt0 : computeTargetPc prog.size ctx.pc.val 1 0 = some f.pc.val := by
    rw [hf_pc]
    exact computeTargetPc_zero_of_lt hpc_next_lt
  have hn_ge : ¬ (dst8.val ≥ REGISTER_COUNT) := by omega
  refine ⟨ret, finalWorld, ?_, ?_, hlive_fin⟩
  · unfold runSteps step
    rcases hop : operandOfSource vsrc with r | imm
    · have hca2 : checkAlive st₁.world.liveResources (st₁.regs r) = none := by
        simpa only [hop, evalOperand] using hca2_1
      have hc1 : (if is64 then
                    evalJump64 op ((st₁.regs dst8.val).toBits l₁ st₁.world.arrayLen)
                      ((st₁.regs r).toBits l₁ st₁.world.arrayLen)
                  else
                    evalJump32 op ((st₁.regs dst8.val).toBits l₁ st₁.world.arrayLen)
                      ((st₁.regs r).toBits l₁ st₁.world.arrayLen)) = false := by
        simpa only [hop, evalOperand] using hcond₁
      simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_ge, ↓reduceIte,
        hop, hsrc_reg r hop, hca1_1, hca2, hc1, Bool.false_eq_true, htgt0]
      rw [stepNextIte_eq_next rfl hr₁]
      exact hr₁
    · have hca2 : checkAlive st₁.world.liveResources (.scalar imm) = none := by
        simpa only [hop, evalOperand] using hca2_1
      have hc1 : (if is64 then
                    evalJump64 op ((st₁.regs dst8.val).toBits l₁ st₁.world.arrayLen)
                      ((RuntimeVal.scalar imm).toBits l₁ st₁.world.arrayLen)
                  else
                    evalJump32 op ((st₁.regs dst8.val).toBits l₁ st₁.world.arrayLen)
                      ((RuntimeVal.scalar imm).toBits l₁ st₁.world.arrayLen)) = false := by
        simpa only [hop, evalOperand] using hcond₁
      simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_ge, ↓reduceIte,
        hop, hca1_1, hca2, hc1, Bool.false_eq_true, htgt0]
      rw [stepNextIte_eq_next rfl hr₁]
      exact hr₁
  · unfold runSteps step
    rcases hop : operandOfSource vsrc with r | imm
    · have hca2 : checkAlive st₂.world.liveResources (st₂.regs r) = none := by
        simpa only [hop, evalOperand] using hca2_2
      have hc2 : (if is64 then
                    evalJump64 op ((st₂.regs dst8.val).toBits l₂ st₂.world.arrayLen)
                      ((st₂.regs r).toBits l₂ st₂.world.arrayLen)
                  else
                    evalJump32 op ((st₂.regs dst8.val).toBits l₂ st₂.world.arrayLen)
                      ((st₂.regs r).toBits l₂ st₂.world.arrayLen)) = false := by
        simpa only [hop, evalOperand] using hcond₂
      simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_ge, ↓reduceIte,
        hop, hsrc_reg r hop, hca1_2, hca2, hc2, Bool.false_eq_true, htgt0]
      rw [stepNextIte_eq_next rfl hr₂]
      exact hr₂
    · have hca2 : checkAlive st₂.world.liveResources (.scalar imm) = none := by
        simpa only [hop, evalOperand] using hca2_2
      have hc2 : (if is64 then
                    evalJump64 op ((st₂.regs dst8.val).toBits l₂ st₂.world.arrayLen)
                      ((RuntimeVal.scalar imm).toBits l₂ st₂.world.arrayLen)
                  else
                    evalJump32 op ((st₂.regs dst8.val).toBits l₂ st₂.world.arrayLen)
                      ((RuntimeVal.scalar imm).toBits l₂ st₂.world.arrayLen)) = false := by
        simpa only [hop, evalOperand] using hcond₂
      simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_ge, ↓reduceIte,
        hop, hca1_2, hca2, hc2, Bool.false_eq_true, htgt0]
      rw [stepNextIte_eq_next rfl hr₂]
      exact hr₂

theorem visit_condJump_backward
    {prog : Program} {env : ExecEnv} {vCounter F : Nat}
    {ctx ctx_out : verifier.ComputationContext}
    {vctx0 vctx1 vctx_back vctx_back1 : verifier.VerificationContext}
    {dPost : verifier.DataDependencies} {tracker : verifier.DependencyTracker}
    {is64 : Bool} {op : JumpOp} {dst : Nat} {src : Operand} {offInt : Int}
    (hcode_len : vctx0.code.val.length = prog.size)
    (hpc : ∃ hlt : ctx.pc.val < prog.size,
      prog[ctx.pc.val]'hlt = .condJump is64 op dst src offInt)
    (hctx : CompCtxValid env vCounter ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hfwd : VisitorDispatchNonLddw CompCtxVisitorInst ctx vctx0
      (.ok (.Ok (), ctx_out, vctx1)) (.condJump is64 op dst src offInt))
    (hchildren : ∀ succ, some succ ∈ vctx1.pending_successors.val →
      SemTerminatesIn prog env vCounter succ dPost F)
    (hback : VisitorDispatchNonLddw DepTrackerVisitorInst
      { context := ctx_out, deps := dPost } vctx_back
      (.ok (.Ok (), tracker, vctx_back1)) (.condJump is64 op dst src offInt)) :
    ctx_out = ctx ∧
    tracker.context = ctx ∧
    vctx_back1 = vctx_back ∧
    SemTerminatesIn prog env vCounter ctx tracker.deps (F + 1) := by
  rcases hfwd with ⟨dst8, vsrc, off, hdst, hsrc_eq, hoff, hcall⟩
  have hcall' := hcall.symm
  rw [condJumpMethod_compCtx_eq] at hcall'
  rcases bind_eq_ok hcall' with ⟨p_out, hcj, hcall'⟩
  rcases p_out with ⟨res_out, vc_out⟩
  rcases Prod.mk.inj (ok_inj hcall') with ⟨rfl, hpair⟩
  rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
  rcases hback with ⟨dst8_b, vsrc_b, off_b, hdst_b, hsrc_b, _, hback_call⟩
  have hdst_u8 : dst8_b = dst8 := UScalar.eq_of_val_eq (hdst_b.symm.trans hdst)
  subst dst8_b
  have hvsrc_eq : vsrc_b = vsrc := by
    have heq := hsrc_b.symm.trans hsrc_eq
    cases vsrc_b <;> cases vsrc <;> simp [operandOfSource] at heq
    · exact congrArg visitor.Source.Reg (UScalar.eq_of_val_eq heq)
    · exact congrArg visitor.Source.Value ((u64_eq_iff_bv _ _).mpr heq)
  subst vsrc_b
  subst dst; subst src; subst offInt
  have hback' := hback_call.symm
  rw [condJumpMethod_depTracker_eq] at hback'
  rcases bind_eq_ok hback' with ⟨p_b, hjmp_dep, hback'⟩
  rcases p_b with ⟨res_b, dd⟩
  obtain ⟨rfl, _, _, hsub, hdst_live, hsrc_live_raw⟩ := DataDependencies_jmp_ok_spec hjmp_dep
  rcases Prod.mk.inj (ok_inj hback') with ⟨_, hpair_b⟩
  rcases Prod.mk.inj hpair_b with ⟨rfl, rfl⟩
  have hsrc_live : ∀ r, vsrc = .Reg r → r.val = 10 ∨ DataDepsHasReg dd r.val = true := by
    intro r hr
    subst hr
    exact Or.inr hsrc_live_raw.2
  refine ⟨rfl, rfl, rfl, ?_⟩
  obtain ⟨hlt_pc, hinsn_at⟩ := hpc
  unfold verifier.ComputationContext.conditional_jump at hcj
  rcases bind_eq_ok hcj with ⟨vc1, hemit, hcj⟩
  obtain ⟨_, he2, _, _, _, he6⟩ := VerificationContext_emit_log_ok_spec hemit
  rcases bind_eq_ok hcj with ⟨r, hreg, hcj⟩
  cases r with
  | Err _ => cases (Prod.mk.inj (ok_inj hcj)).1
  | Ok v =>
    dsimp only at hcj
    obtain ⟨hdst_lt, hv_valid⟩ := ComputationContext_reg_ok_valid hctx hreg
    rcases bind_eq_ok hcj with ⟨r1, hsrc, hcj⟩
    cases r1 with
    | Err _ => cases (Prod.mk.inj (ok_inj hcj)).1
    | Ok v1 =>
      dsimp only at hcj
      obtain ⟨hsrc_reg, hv1_valid⟩ := ComputationContext_compute_source_ok_valid hctx hsrc
      rcases bind_eq_ok hcj with ⟨r2, hbranch, hcj⟩
      cases r2 with
      | Err _ => cases (Prod.mk.inj (ok_inj hcj)).1
      | Ok b =>
        dsimp only at hcj
        cases b with
        | none =>
          simp only [bind_ok, ↓reduceIte] at hcj
          rcases bind_eq_ok hcj with ⟨r3, hjmp, hcj⟩
          cases r3 with
          | Err _ => cases (Prod.mk.inj (ok_inj hcj)).1
          | Ok j =>
            dsimp only at hcj
            rw [bind_ok_eq (Type_clone_spec v), bind_ok_eq (Type_clone_spec v1)] at hcj
            rcases bind_eq_ok hcj with ⟨r4, happly, hcj⟩
            cases r4 with
            | Err _ => cases (Prod.mk.inj (ok_inj hcj)).1
            | Ok c =>
              dsimp only at hcj
              rcases bind_eq_ok hcj with ⟨vc2, hpush, hcj⟩
              rcases bind_eq_ok hcj with ⟨r5, hjmp0, hcj⟩
              cases r5 with
              | Err _ => cases (Prod.mk.inj (ok_inj hcj)).1
              | Ok f =>
                dsimp only at hcj
                rcases bind_eq_ok hcj with ⟨i_jmp, _, hcj⟩
                rw [bind_ok_eq (JumpType_invert_eq_ok (jumpTypeOfOp op))] at hcj
                rcases bind_eq_ok hcj with ⟨r6, happly0, hcj⟩
                cases r6 with
                | Err _ => cases (Prod.mk.inj (ok_inj hcj)).1
                | Ok c1 =>
                  dsimp only at hcj
                  rcases bind_eq_ok hcj with ⟨vc3, hpush0, hcj⟩
                  rcases Prod.mk.inj (ok_inj hcj) with ⟨_, rfl⟩
                  obtain ⟨htgt, hj_eq⟩ := ComputationContext_checked_jump_with_offset_ok_spec hjmp
                  rw [he2, hcode_len] at htgt
                  obtain ⟨_, _, _, _, _, _, hp7⟩ := VerificationContext_push_successor_ok_spec hpush
                  obtain ⟨_, _, _, _, _, _, hp7_0⟩ := VerificationContext_push_successor_ok_spec hpush0
                  have hc_mem : some c ∈ vc3.pending_successors.val := by
                    rw [hp7_0, hp7, he6, hpending]; simp
                  have hc1_mem : some c1 ∈ vc3.pending_successors.val := by
                    rw [hp7_0, hp7, he6, hpending]; simp
                  have hsem_c := hchildren c hc_mem
                  have hsem_c1 := hchildren c1 hc1_mem
                  intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
                  obtain ⟨_, _, hrel1⟩ :=
                    ComputationContext_reg_ok_valid_and_rel2 hctx hrel (Or.inr hdst_live) hreg
                  have hrel2 :=
                    ComputationContext_compute_source_evalOperand_rel2 hctx hrel hsrc_live hsrc
                  obtain ⟨hca1_1, hca1_2, hca2_1, hca2_2, hc_eq, _⟩ :=
                    compute_branch_ok_sound hctx hrel hl₁ hl₂ hv_valid hv1_valid hrel1 hrel2 hbranch
                  cases hcond₁ :
                    (if is64 then
                       evalJump64 op ((st₁.regs dst8.val).toBits l₁ st₁.world.arrayLen)
                         ((evalOperand st₁ (operandOfSource vsrc)).toBits l₁ st₁.world.arrayLen)
                     else
                       evalJump32 op ((st₁.regs dst8.val).toBits l₁ st₁.world.arrayLen)
                         ((evalOperand st₁ (operandOfSource vsrc)).toBits l₁ st₁.world.arrayLen))
                  · exact condJump_step_skipped hlt_pc hinsn_at hdst_lt hsrc_reg hctx hv_valid hv1_valid
                      hsub hl₁ hl₂ hleak halloc hρ hrel hrel1 hrel2 hca1_1 hca1_2 hca2_1 hca2_2
                      hcond₁ (hc_eq.symm.trans hcond₁) hjmp0 happly0 hsem_c1
                  · exact condJump_step_taken hlt_pc hinsn_at hdst_lt hsrc_reg hctx hv_valid hv1_valid
                      hsub hl₁ hl₂ hleak halloc hρ hrel hrel1 hrel2 hca1_1 hca1_2 hca2_1 hca2_2
                      hcond₁ (hc_eq.symm.trans hcond₁) htgt hj_eq happly hsem_c
        | some b1 =>
          cases b1 with
          | true =>
            simp only [bind_ok, ↓reduceIte] at hcj
            rcases bind_eq_ok hcj with ⟨r3, hjmp, hcj⟩
            cases r3 with
            | Err _ => cases (Prod.mk.inj (ok_inj hcj)).1
            | Ok j =>
              dsimp only at hcj
              rw [bind_ok_eq (Type_clone_spec v), bind_ok_eq (Type_clone_spec v1)] at hcj
              rcases bind_eq_ok hcj with ⟨r4, happly, hcj⟩
              cases r4 with
              | Err _ => cases (Prod.mk.inj (ok_inj hcj)).1
              | Ok c =>
                dsimp only at hcj
                rcases bind_eq_ok hcj with ⟨vc2, hpush, hcj⟩
                rcases Prod.mk.inj (ok_inj hcj) with ⟨_, rfl⟩
                obtain ⟨htgt, hj_eq⟩ := ComputationContext_checked_jump_with_offset_ok_spec hjmp
                rw [he2, hcode_len] at htgt
                obtain ⟨_, _, _, _, _, _, hp7⟩ := VerificationContext_push_successor_ok_spec hpush
                have hc_mem : some c ∈ vc2.pending_successors.val := by
                  rw [hp7, he6, hpending]; simp
                have hsem_c := hchildren c hc_mem
                intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
                obtain ⟨_, _, hrel1⟩ :=
                  ComputationContext_reg_ok_valid_and_rel2 hctx hrel (Or.inr hdst_live) hreg
                have hrel2 :=
                  ComputationContext_compute_source_evalOperand_rel2 hctx hrel hsrc_live hsrc
                obtain ⟨hca1_1, hca1_2, hca2_1, hca2_2, hc_eq, hopt_b⟩ :=
                  compute_branch_ok_sound hctx hrel hl₁ hl₂ hv_valid hv1_valid hrel1 hrel2 hbranch
                have hcond₁ := hopt_b true rfl
                exact condJump_step_taken hlt_pc hinsn_at hdst_lt hsrc_reg hctx hv_valid hv1_valid
                  hsub hl₁ hl₂ hleak halloc hρ hrel hrel1 hrel2 hca1_1 hca1_2 hca2_1 hca2_2
                  hcond₁ (hc_eq.symm.trans hcond₁) htgt hj_eq happly hsem_c
          | false =>
            simp only [bind_ok] at hcj
            rcases bind_eq_ok hcj with ⟨r3, hjmp0, hcj⟩
            cases r3 with
            | Err _ => cases (Prod.mk.inj (ok_inj hcj)).1
            | Ok f =>
              dsimp only at hcj
              rcases bind_eq_ok hcj with ⟨i_jmp, _, hcj⟩
              rw [bind_ok_eq (JumpType_invert_eq_ok (jumpTypeOfOp op))] at hcj
              rcases bind_eq_ok hcj with ⟨r4, happly0, hcj⟩
              cases r4 with
              | Err _ => cases (Prod.mk.inj (ok_inj hcj)).1
              | Ok c1 =>
                dsimp only at hcj
                rcases bind_eq_ok hcj with ⟨vc2, hpush0, hcj⟩
                rcases Prod.mk.inj (ok_inj hcj) with ⟨_, rfl⟩
                obtain ⟨_, _, _, _, _, _, hp7_0⟩ := VerificationContext_push_successor_ok_spec hpush0
                have hc1_mem : some c1 ∈ vc2.pending_successors.val := by
                  rw [hp7_0, he6, hpending]; simp
                have hsem_c1 := hchildren c1 hc1_mem
                intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
                obtain ⟨_, _, hrel1⟩ :=
                  ComputationContext_reg_ok_valid_and_rel2 hctx hrel (Or.inr hdst_live) hreg
                have hrel2 :=
                  ComputationContext_compute_source_evalOperand_rel2 hctx hrel hsrc_live hsrc
                obtain ⟨hca1_1, hca1_2, hca2_1, hca2_2, hc_eq, hopt_b⟩ :=
                  compute_branch_ok_sound hctx hrel hl₁ hl₂ hv_valid hv1_valid hrel1 hrel2 hbranch
                have hcond₁ := hopt_b false rfl
                exact condJump_step_skipped hlt_pc hinsn_at hdst_lt hsrc_reg hctx hv_valid hv1_valid
                  hsub hl₁ hl₂ hleak halloc hρ hrel hrel1 hrel2 hca1_1 hca1_2 hca2_1 hca2_2
                  hcond₁ (hc_eq.symm.trans hcond₁) hjmp0 happly0 hsem_c1

end StarnixEbpf.Proof
