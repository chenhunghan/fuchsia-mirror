-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Proof.ApiConverterLinkerExec
import Proof.ScalarAlu
import Proof.DecodeAndPrescan
import Proof.InsnAluEndExit
import Proof.CondJumpSoundness
import Proof.MemLoadStoreSoundness
import Proof.AtomicPacketSoundness
import Proof.HelperAndLddwSoundness

/-!
# Subsumption, Explorer Loop Invariant, & Top-Level Verifier Theorems

Proves:
1. Abstract state subsumption (`PartialOrd` on `Type`, `Stack`, `BoundsMap`,
   `ComputationContext`) and pruning/insertion soundness for `TerminatingContexts`.
2. Unified single-instruction forward validity, counter-step bound (`≤ 3`), and
   backward 2-safety transfer (`SemTerminatesIn`) across all eBPF instructions.
3. Worklist and `ExplorerArena` invariants (`verify_worklist_entry`,
   `terminate_context_chain`, `verify_program_core_loop5`).
4. Top-level verifier theorems (`verify_program_core_implies_safe_and_terminating`
   and `verify_program_core_implies_non_interfering`).
-/

set_option linter.unusedTactic false
set_option linter.unnecessarySimpa false
set_option linter.unnecessarySeqFocus false
set_option maxHeartbeats 800000

open Aeneas Aeneas.Std Result ControlFlow

namespace StarnixEbpf.Proof

open StarnixEbpf.Assumptions
open StarnixEbpf.Spec
open StarnixEbpf.LoopLemmas
open ebpf_core
open ebpf_core.scalar_value

/-! ## 1. Abstract State Subsumption (`PartialOrd` & `TerminatingContexts`) -/

/-- Predicate expressing that `Ordering` represents `≥` (`.eq` or `.gt`). -/
def OrdIsGe (o : Ordering) : Prop :=
  o = .eq ∨ o = .gt

theorem associate_orderings_ge_iff {o1 o2 out : Ordering}
    (h : verifier.associate_orderings o1 o2 = ok (some out))
    (hge : OrdIsGe out) :
    OrdIsGe o1 ∧ OrdIsGe o2 := by
  unfold verifier.associate_orderings at h
  cases o1 <;> cases o2 <;> simp only [ok.injEq, Option.some.injEq, reduceCtorEq] at h
  · subst h; rcases hge with h1 | h1 <;> cases h1
  · subst h; rcases hge with h1 | h1 <;> cases h1
  · subst h; rcases hge with h1 | h1 <;> cases h1
  · subst h; exact ⟨Or.inl rfl, Or.inl rfl⟩
  · subst h; exact ⟨Or.inl rfl, Or.inr rfl⟩
  · subst h; exact ⟨Or.inr rfl, Or.inl rfl⟩
  · subst h; exact ⟨Or.inr rfl, Or.inr rfl⟩

theorem associate_orderings_lt_not_ge {o2 : Ordering} {opt : Option Ordering}
    (h : verifier.associate_orderings .lt o2 = ok opt) :
    ∀ out, opt = some out → ¬ OrdIsGe out := by
  unfold verifier.associate_orderings at h
  intro out hout hge
  cases o2 <;> simp only [ok.injEq] at h
  · rw [← h] at hout; cases Option.some.inj hout; rcases hge with hc | hc <;> cases hc
  · rw [← h] at hout; cases Option.some.inj hout; rcases hge with hc | hc <;> cases hc
  · rw [← h] at hout; cases hout

theorem eq_uninit_of_broaderOrEqual_uninit {d1 : U64ScalarValueData}
    (hv1 : Scalar64Valid d1)
    (hbr : BroaderOrEqual64 d1 uninitScalar64) :
    d1 = uninitScalar64 := by
  rcases hv1 with ⟨hdisj, _, _, _⟩
  rcases hbr with ⟨hunwr, hunk, _, hmin, hmax⟩
  dsimp only [uninitScalar64] at hunwr hunk hmin hmax
  rw [u64_max_bv] at hunwr hunk hmax
  have hunwr_eq : d1.unwritten_mask.bv = ~~~0#64 := by revert hunwr; bv_decide
  have hunk_eq : d1.unknown_mask.bv = ~~~0#64 := by revert hunk; bv_decide
  have hval_eq : d1.value.bv = 0#64 := by
    rw [hunk_eq] at hdisj
    revert hdisj; bv_decide
  have hmin_eq : d1.urange.min.bv = 0#64 := by
    change d1.urange.min.bv ≤ 0#64 at hmin
    revert hmin; bv_decide
  have hmax_eq : d1.urange.max.bv = ~~~0#64 := by
    revert hmax; bv_decide
  cases d1 with
  | mk val unk unwr ur =>
    cases ur with
    | mk umin umax =>
      dsimp only at hval_eq hunk_eq hunwr_eq hmin_eq hmax_eq
      have e1 : val = 0#u64 := (u64_eq_iff_bv _ _).mpr hval_eq
      have e2 : unk = core.num.U64.MAX := (u64_eq_iff_bv _ _).mpr (by rwa [u64_max_bv])
      have e3 : unwr = core.num.U64.MAX := (u64_eq_iff_bv _ _).mpr (by rwa [u64_max_bv])
      have e4 : umin = 0#u64 := (u64_eq_iff_bv _ _).mpr hmin_eq
      have e5 : umax = core.num.U64.MAX := (u64_eq_iff_bv _ _).mpr (by rwa [u64_max_bv])
      subst e1; subst e2; subst e3; subst e4; subst e5
      rfl

private theorem scalar_vs_nonscalar_ge {d1 : U64ScalarValueData} {ord : Ordering}
    (h : (do
      let usvd ← U64ScalarValueData.UNINITIALIZED
      let b ← U64ScalarValueData.Insts.CoreCmpPartialEqU64ScalarValueData.eq d1 usvd
      if b then ok (some Ordering.gt) else ok none) = ok (some ord)) :
    d1 = uninitScalar64 := by
  rw [bind_ok_eq UNINITIALIZED_64_eq] at h
  obtain ⟨b, hb, hiff⟩ := u64_scalar_eq_sound d1 uninitScalar64
  rw [bind_ok_eq hb] at h
  cases b with
  | false => simp at h
  | true => exact hiff.mp rfl

private theorem nonscalar_vs_scalar_not_ge {d2 : U64ScalarValueData} {ord : Ordering}
    (h : (do
      let usvd ← U64ScalarValueData.UNINITIALIZED
      let b ← U64ScalarValueData.Insts.CoreCmpPartialEqU64ScalarValueData.eq d2 usvd
      if b then ok (some Ordering.lt) else ok none) = ok (some ord))
    (hge : OrdIsGe ord) : False := by
  rw [bind_ok_eq UNINITIALIZED_64_eq] at h
  obtain ⟨b, hb, _⟩ := u64_scalar_eq_sound d2 uninitScalar64
  rw [bind_ok_eq hb] at h
  cases b with
  | false => simp at h
  | true =>
    simp only [↓reduceIte, ok.injEq, Option.some.injEq] at h
    subst h
    rcases hge with hc | hc <;> cases hc

private theorem nonscalar_vs_nonscalar_ge {t1 t2 : verifier.Type} {ord : Ordering}
    (h : (do
      let b ← verifier.Type.Insts.CoreCmpPartialEqType.eq t1 t2
      if b then ok (some Ordering.eq) else ok none) = ok (some ord)) :
    t1 = t2 := by
  obtain ⟨b, hb, hiff⟩ := Type_eq_spec t1 t2
  rw [bind_ok_eq hb] at h
  cases b with
  | false => simp at h
  | true => exact hiff.mp rfl

theorem Type_partial_cmp_ge_spec {t1 t2 : verifier.Type} {ord : Ordering}
    (h : verifier.Type.Insts.CoreCmpPartialOrdType.partial_cmp t1 t2 = ok (some ord))
    (hge : OrdIsGe ord) :
    (∃ d1 d2, t1 = .ScalarValue d1 ∧ t2 = .ScalarValue d2 ∧ BroaderOrEqual64 d1 d2) ∨
    t1 = .ScalarValue uninitScalar64 ∨
    t1 = t2 := by
  unfold verifier.Type.Insts.CoreCmpPartialOrdType.partial_cmp at h
  cases t1 with
  | ScalarValue d1 =>
    cases t2 with
    | ScalarValue d2 =>
      dsimp only at h
      obtain ⟨opt, hopt, heq_c, hgt_c, _⟩ := u64_scalar_partial_cmp_sound d1 d2
      rw [h] at hopt
      cases ok_inj hopt
      rcases hge with rfl | rfl
      · exact Or.inl ⟨d1, d2, rfl, rfl, (heq_c rfl).2.1⟩
      · exact Or.inl ⟨d1, d2, rfl, rfl, hgt_c rfl⟩
    | _ =>
      exact Or.inr (Or.inl (congrArg verifier.Type.ScalarValue (scalar_vs_nonscalar_ge h)))
  | _ =>
    cases t2 with
    | ScalarValue d2 =>
      exact False.elim (nonscalar_vs_scalar_not_ge h hge)
    | _ =>
      exact Or.inr (Or.inr (nonscalar_vs_nonscalar_ge h))

theorem Type_subsumes_rel2
    {env : ExecEnv} {vCounter : Nat}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {arrLen : MemoryId → BitVec 64}
    {t1 t2 : verifier.Type} {ord : Ordering} {v₁ v₂ : RuntimeVal}
    (ht1 : TypeValid env vCounter t1)
    (hcmp : verifier.Type.Insts.CoreCmpPartialOrdType.partial_cmp t1 t2 = ok (some ord))
    (hge : OrdIsGe ord)
    (hrel : TypeRel2 env ρ resActive isNullId arrLen t2 v₁ v₂) :
    TypeRel2 env ρ resActive isNullId arrLen t1 v₁ v₂ := by
  rcases Type_partial_cmp_ge_spec hcmp hge with
    ⟨d1, d2, rfl, rfl, hbr⟩ | rfl | rfl
  · by_cases hd1 : d1 = uninitScalar64
    · subst hd1; simp [TypeRel2]
    · have hd2 : d2 ≠ uninitScalar64 := by
        intro hc; subst hc; exact hd1 (eq_uninit_of_broaderOrEqual_uninit ht1 hbr)
      simp only [TypeRel2, hd2, ↓reduceIte] at hrel
      rcases hrel with ⟨x₁, x₂, rfl, rfl, hc2⟩
      simp only [TypeRel2, hd1, ↓reduceIte]
      exact ⟨x₁, x₂, rfl, rfl, BroaderOrEqual64.monotone_rel2 ht1 hbr hc2⟩
  · simp [TypeRel2]
  · exact hrel

theorem Type_subsumes_stackSlotRel2
    {env : ExecEnv} {vCounter : Nat}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {arrLen : MemoryId → BitVec 64}
    {t1 t2 : verifier.Type} {ord : Ordering} {s₁ s₂ : StackSlot}
    (ht1 : TypeValid env vCounter t1)
    (hcmp : verifier.Type.Insts.CoreCmpPartialOrdType.partial_cmp t1 t2 = ok (some ord))
    (hge : OrdIsGe ord)
    (hrel : StackSlotRel2 env ρ resActive isNullId arrLen t2 s₁ s₂) :
    StackSlotRel2 env ρ resActive isNullId arrLen t1 s₁ s₂ := by
  rcases Type_partial_cmp_ge_spec hcmp hge with
    ⟨d1, d2, rfl, rfl, hbr⟩ | rfl | rfl
  · by_cases hd1 : d1 = uninitScalar64
    · subst hd1; simp [StackSlotRel2]
    · have hd2 : d2 ≠ uninitScalar64 := by
        intro hc; subst hc; exact hd1 (eq_uninit_of_broaderOrEqual_uninit ht1 hbr)
      simp only [StackSlotRel2, hd2, ↓reduceIte] at hrel
      rcases hrel with ⟨val₁, init₁, val₂, init₂, rfl, rfl, hc_v, hc_1, hc_2⟩
      simp only [StackSlotRel2, hd1, ↓reduceIte]
      exact ⟨val₁, init₁, val₂, init₂, rfl, rfl,
        BroaderOrEqual64.monotone_rel2 ht1 hbr hc_v,
        BroaderOrEqual64.monotone_rel2 ht1 hbr hc_1,
        BroaderOrEqual64.monotone_rel2 ht1 hbr hc_2⟩
  · simp [StackSlotRel2]
  · exact hrel

theorem compare_registers_ge_spec
    {r1 r2 : Std.Array verifier.Type 10#usize} {ord : Ordering}
    (h : verifier.Type.compare_registers r1 r2 = ok (some ord))
    (hge : OrdIsGe ord) :
    ∀ r (hr : r < 10),
      ∃ o,
        verifier.Type.Insts.CoreCmpPartialOrdType.partial_cmp
          (r1.val[r]'(by rw [array10_len]; exact hr))
          (r2.val[r]'(by rw [array10_len]; exact hr)) = ok (some o) ∧
        OrdIsGe o := by
  unfold verifier.Type.compare_registers verifier.Type.compare_registers_loop at h
  have hloop := loop_ok_pre_post
    (fun (res1, i1) => verifier.Type.compare_registers_loop.body r1 r2 res1 i1)
    (fun p => p.2.val ≤ 10)
    (fun p opt =>
      ∀ out, opt = some out → OrdIsGe out →
        OrdIsGe p.1 ∧
        ∀ r (hr : r < 10), p.2.val ≤ r →
          ∃ o,
            verifier.Type.Insts.CoreCmpPartialOrdType.partial_cmp
              (r1.val[r]'(by rw [array10_len]; exact hr))
              (r2.val[r]'(by rw [array10_len]; exact hr)) = ok (some o) ∧
            OrdIsGe o)
    (by
      rintro ⟨res1, i1⟩ ⟨res2, i2⟩ opt hpre hbody
      dsimp only at hpre ⊢
      unfold verifier.Type.compare_registers_loop.body at hbody
      simp only [lift_eq_ok, bind_ok] at hbody
      have hcast10 : (UScalar.cast .Usize api.GENERAL_REGISTER_COUNT).val = 10 := by
        rw [uscalar_cast_usize_of_u8_val, general_register_count_val]
      by_cases hlt : i1 < UScalar.cast .Usize api.GENERAL_REGISTER_COUNT
      · simp only [hlt, ↓reduceIte] at hbody
        have hi1_lt10 : i1.val < 10 := by
          have : i1.val < (UScalar.cast .Usize api.GENERAL_REGISTER_COUNT).val := hlt
          omega
        rw [bind_ok_eq (array_index_usize_10_ok r1 i1 hi1_lt10),
            bind_ok_eq (array_index_usize_10_ok r2 i1 hi1_lt10)] at hbody
        rcases bind_eq_ok hbody with ⟨o, hcmp, hbody⟩
        cases o with
        | none => cases ok_inj hbody
        | some cur_o =>
          dsimp only at hbody
          rcases bind_eq_ok hbody with ⟨o1, hassoc, hbody⟩
          cases o1 with
          | none => cases ok_inj hbody
          | some next =>
            dsimp only at hbody
            rcases bind_eq_ok hbody with ⟨i_next, hadd, hbody⟩
            cases ok_inj hbody
            have hi2_val : i2.val = i1.val + 1 := by
              simpa [usize_one_val] using (uscalar_add_ok_val hadd).1
            refine ⟨by omega, fun hIH out hout hge_out => ?_⟩
            obtain ⟨hge_next, htail⟩ := hIH out hout hge_out
            obtain ⟨hge_res1, hge_cur⟩ := associate_orderings_ge_iff hassoc hge_next
            refine ⟨hge_res1, fun r hr hle_r => ?_⟩
            by_cases hri : r = i1.val
            · subst hri; exact ⟨cur_o, hcmp, hge_cur⟩
            · exact htail r hr (by omega)
      · simp only [hlt, ↓reduceIte] at hbody
        cases ok_inj hbody)
    (by
      rintro ⟨res1, i1⟩ opt hpre hbody
      dsimp only at hpre ⊢
      unfold verifier.Type.compare_registers_loop.body at hbody
      simp only [lift_eq_ok, bind_ok] at hbody
      have hcast10 : (UScalar.cast .Usize api.GENERAL_REGISTER_COUNT).val = 10 := by
        rw [uscalar_cast_usize_of_u8_val, general_register_count_val]
      by_cases hlt : i1 < UScalar.cast .Usize api.GENERAL_REGISTER_COUNT
      · simp only [hlt, ↓reduceIte] at hbody
        have hi1_lt10 : i1.val < 10 := by
          have : i1.val < (UScalar.cast .Usize api.GENERAL_REGISTER_COUNT).val := hlt
          omega
        rw [bind_ok_eq (array_index_usize_10_ok r1 i1 hi1_lt10),
            bind_ok_eq (array_index_usize_10_ok r2 i1 hi1_lt10)] at hbody
        rcases bind_eq_ok hbody with ⟨o, _, hbody⟩
        cases o with
        | none =>
          cases ok_inj hbody
          intro _ hout; cases hout
        | some cur_o =>
          rcases bind_eq_ok hbody with ⟨o1, _, hbody⟩
          cases o1 with
          | none =>
            cases ok_inj hbody
            intro _ hout; cases hout
          | some _ =>
            rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
            cases ok_inj hbody
      · simp only [hlt, ↓reduceIte] at hbody
        cases ok_inj hbody
        have hi1_eq : i1.val = 10 := by
          have hn : ¬ (i1.val < (UScalar.cast .Usize api.GENERAL_REGISTER_COUNT).val) := hlt
          omega
        intro out hout hge_out
        cases Option.some.inj hout
        exact ⟨hge_out, fun r hr hle_r => by omega⟩)
    (Ordering.eq, 0#usize) (some ord) (by simp) h
  intro r hr
  exact (hloop ord rfl hge).2 r hr (by simp)

private def OptNotGe : Option Ordering → Prop
  | none => True
  | some o => ¬ OrdIsGe o

private theorem combine_slot_ordering_lt_not_ge
    {opt_res next_opt : Option Ordering} {lhs rhs : verifier.Type}
    (hpre : OptNotGe opt_res)
    (hc : verifier.Stack.combine_slot_ordering opt_res lhs rhs = ok next_opt) :
    OptNotGe next_opt := by
  unfold verifier.Stack.combine_slot_ordering at hc
  cases opt_res with
  | none => cases ok_inj hc; trivial
  | some cur =>
    have hlt_cur : cur = .lt := by
      cases cur with
      | lt => rfl
      | eq => exact False.elim (hpre (Or.inl rfl))
      | gt => exact False.elim (hpre (Or.inr rfl))
    subst hlt_cur
    dsimp only at hc
    rcases bind_eq_ok hc with ⟨o, _, hc⟩
    cases o with
    | none => cases ok_inj hc; trivial
    | some ord_c =>
      have hnot := associate_orderings_lt_not_ge hc
      cases next_opt with
      | none => trivial
      | some out_c => exact hnot out_c rfl

private theorem combine_slot_ordering_ge_spec
    {opt_res : Option Ordering} {c2 : Ordering} {lhs rhs : verifier.Type}
    (hc : verifier.Stack.combine_slot_ordering opt_res lhs rhs = ok (some c2))
    (hge_c2 : OrdIsGe c2) :
    (∃ c1, opt_res = some c1 ∧ OrdIsGe c1) ∧
    (∃ o, verifier.Type.Insts.CoreCmpPartialOrdType.partial_cmp lhs rhs = ok (some o) ∧ OrdIsGe o) := by
  unfold verifier.Stack.combine_slot_ordering at hc
  cases opt_res with
  | none => cases ok_inj hc
  | some c1 =>
    dsimp only at hc
    rcases bind_eq_ok hc with ⟨o, hcmp, hc⟩
    cases o with
    | none => cases ok_inj hc
    | some ord_k =>
      obtain ⟨hge_c1, hge_k⟩ := associate_orderings_ge_iff hc hge_c2
      exact ⟨⟨c1, rfl, hge_c1⟩, ⟨ord_k, hcmp, hge_k⟩⟩

private theorem stack_loop0_lt_not_ge
    (v v1 : alloc.vec.Vec verifier.Type) (opt_res : Option Ordering) (i : Std.Usize)
    (out : Option Ordering)
    (hpre : OptNotGe opt_res)
    (hloop : verifier.Stack.Insts.CoreCmpPartialOrdStack.partial_cmp_loop0 v v1 opt_res i = ok out) :
    OptNotGe out := by
  unfold verifier.Stack.Insts.CoreCmpPartialOrdStack.partial_cmp_loop0 at hloop
  refine loop_ok_induct
    (fun (opt1, i1) => verifier.Stack.Insts.CoreCmpPartialOrdStack.partial_cmp_loop0.body v v1 opt1 i1)
    (fun (opt1, _) => OptNotGe opt1)
    OptNotGe
    ?_ (opt_res, i) out hpre hloop
  rintro ⟨opt1, i1⟩ hinv
  unfold verifier.Stack.Insts.CoreCmpPartialOrdStack.partial_cmp_loop0.body
  dsimp only
  constructor
  · rintro ⟨opt2, i2⟩ hbody
    split at hbody
    · split at hbody
      · rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
        rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
        rcases bind_eq_ok hbody with ⟨next_opt, hnext, hbody⟩
        rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
        cases ok_inj hbody
        exact combine_slot_ordering_lt_not_ge hinv hnext
      · cases ok_inj hbody
    · cases ok_inj hbody
  · intro res hbody
    split at hbody
    · split at hbody
      · rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
        rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
        rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
        rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
        cases ok_inj hbody
      · cases ok_inj hbody; exact hinv
    · cases ok_inj hbody; exact hinv

private theorem stack_loop1_lt_not_ge
    (m1 : Std.U64) (v : alloc.vec.Vec verifier.Type)
    (m2 : Std.U64) (v1 : alloc.vec.Vec verifier.Type)
    (opt_res : Option Ordering) (ps po idx : Std.Usize) (out : Option Ordering)
    (hpre : OptNotGe opt_res)
    (hloop : verifier.Stack.Insts.CoreCmpPartialOrdStack.partial_cmp_loop1 m1 v m2 v1 opt_res ps po idx = ok out) :
    OptNotGe out := by
  unfold verifier.Stack.Insts.CoreCmpPartialOrdStack.partial_cmp_loop1 at hloop
  refine loop_ok_induct
    (fun (opt1, ps1, po1, idx1) =>
      verifier.Stack.Insts.CoreCmpPartialOrdStack.partial_cmp_loop1.body m1 v m2 v1 opt1 ps1 po1 idx1)
    (fun (opt1, _, _, _) => OptNotGe opt1)
    OptNotGe
    ?_ (opt_res, ps, po, idx) out hpre hloop
  rintro ⟨opt1, ps1, po1, idx1⟩ hinv
  unfold verifier.Stack.Insts.CoreCmpPartialOrdStack.partial_cmp_loop1.body
  dsimp only
  constructor
  · rintro ⟨opt2, ps2, po2, idx2⟩ hbody
    split at hbody
    · split at hbody
      · rcases bind_eq_ok hbody with ⟨b1, _, hbody⟩
        simp only [lift_eq_ok, bind_ok] at hbody
        rcases bind_eq_ok hbody with ⟨next_opt, hnext, hbody⟩
        rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
        rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
        rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
        cases ok_inj hbody
        dsimp only
        split at hnext
        · split at hnext
          · rcases bind_eq_ok hnext with ⟨_, _, hnext⟩
            rcases bind_eq_ok hnext with ⟨_, _, hnext⟩
            exact combine_slot_ordering_lt_not_ge hinv hnext
          · cases ok_inj hnext; exact hinv
        · cases ok_inj hnext; exact hinv
      · cases ok_inj hbody
    · cases ok_inj hbody
  · intro res hbody
    split at hbody
    · split at hbody
      · rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
        simp only [lift_eq_ok, bind_ok] at hbody
        rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
        rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
        rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
        rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
        cases ok_inj hbody
      · cases ok_inj hbody; exact hinv
    · cases ok_inj hbody; exact hinv

private theorem stack_loop2_ge_spec
    {v v1 : alloc.vec.Vec verifier.Type} {cur : Ordering} {ord : Ordering}
    (hlen : v.val.length = v1.val.length)
    (hloop : verifier.Stack.Insts.CoreCmpPartialOrdStack.partial_cmp_loop2 v v1 (some cur) 0#usize = ok (some ord))
    (hge : OrdIsGe ord) :
    ∀ k (hk : k < v.val.length),
      ∃ o,
        verifier.Type.Insts.CoreCmpPartialOrdType.partial_cmp
          (v.val[k]'hk) (v1.val[k]'(hlen ▸ hk)) = ok (some o) ∧
        OrdIsGe o := by
  unfold verifier.Stack.Insts.CoreCmpPartialOrdStack.partial_cmp_loop2 at hloop
  have hmain := loop_ok_pre_post
    (fun (opt1, i1) => verifier.Stack.Insts.CoreCmpPartialOrdStack.partial_cmp_loop2.body v v1 opt1 i1)
    (fun (_, i1) => i1.val ≤ v.val.length)
    (fun (opt1, i1) out =>
      ∀ fin_o, out = some fin_o → OrdIsGe fin_o →
        (∃ c1, opt1 = some c1 ∧ OrdIsGe c1) ∧
        ∀ k (hk : k < v.val.length), i1.val ≤ k →
          ∃ o,
            verifier.Type.Insts.CoreCmpPartialOrdType.partial_cmp
              (v.val[k]'hk) (v1.val[k]'(hlen ▸ hk)) = ok (some o) ∧
            OrdIsGe o)
    (by
      rintro ⟨opt1, i1⟩ ⟨opt2, i2⟩ out hpre hbody
      unfold verifier.Stack.Insts.CoreCmpPartialOrdStack.partial_cmp_loop2.body at hbody
      by_cases hlt : i1 < alloc.vec.Vec.len v
      · simp only [hlt, ↓reduceIte] at hbody
        have hi1_lt : i1.val < v.val.length := hlt
        by_cases hsome : core.option.Option.is_some opt1 = true
        · simp only [hsome, ↓reduceIte] at hbody
          rcases bind_eq_ok hbody with ⟨t, ht, hbody⟩
          rcases bind_eq_ok hbody with ⟨t1, ht1, hbody⟩
          rcases bind_eq_ok hbody with ⟨next_opt, hnext, hbody⟩
          rcases bind_eq_ok hbody with ⟨i_next, hadd, hbody⟩
          cases ok_inj hbody
          have hi2_val : i2.val = i1.val + 1 := by
            simpa [usize_one_val] using (uscalar_add_ok_val hadd).1
          have ht_eq : t = v.val[i1.val]'hi1_lt := by
            have := (vec_index_usize_ok ht).2
            rw [List.getElem?_eq_getElem hi1_lt] at this
            exact Option.some.inj this.symm
          have ht1_eq : t1 = v1.val[i1.val]'(hlen ▸ hi1_lt) := by
            have := (vec_index_usize_ok ht1).2
            rw [List.getElem?_eq_getElem (hlen ▸ hi1_lt)] at this
            exact Option.some.inj this.symm
          subst ht_eq; subst ht1_eq
          refine ⟨by omega, fun hIH fin_o hout hge_fin => ?_⟩
          obtain ⟨⟨c2, hopt2, hge_c2⟩, htail⟩ := hIH fin_o hout hge_fin
          subst hopt2
          obtain ⟨⟨c1, rfl, hge_c1⟩, ⟨ord_k, hcmp, hge_k⟩⟩ :=
            combine_slot_ordering_ge_spec hnext hge_c2
          refine ⟨⟨c1, rfl, hge_c1⟩, fun k hk hle_k => ?_⟩
          by_cases hki : k = i1.val
          · subst hki; exact ⟨ord_k, hcmp, hge_k⟩
          · exact htail k hk (by omega)
        · simp only [hsome, Bool.false_eq_true, ↓reduceIte] at hbody
          cases ok_inj hbody
      · simp only [hlt, ↓reduceIte] at hbody
        cases ok_inj hbody)
    (by
      rintro ⟨opt1, i1⟩ out hpre hbody
      unfold verifier.Stack.Insts.CoreCmpPartialOrdStack.partial_cmp_loop2.body at hbody
      by_cases hlt : i1 < alloc.vec.Vec.len v
      · simp only [hlt, ↓reduceIte] at hbody
        by_cases hsome : core.option.Option.is_some opt1 = true
        · simp only [hsome, ↓reduceIte] at hbody
          rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
          rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
          rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
          rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
          cases ok_inj hbody
        · simp only [hsome, Bool.false_eq_true, ↓reduceIte] at hbody
          cases ok_inj hbody
          cases opt1 with
          | none => intro _ hout; cases hout
          | some _ => simp [core.option.Option.is_some] at hsome
      · simp only [hlt, ↓reduceIte] at hbody
        cases ok_inj hbody
        have hi1_eq : i1.val = v.val.length := by
          have hn : ¬ (i1.val < v.val.length) := hlt
          omega
        intro fin_o hout hge_fin
        subst hout
        exact ⟨⟨fin_o, rfl, hge_fin⟩, fun k hk hle_k => by omega⟩)
    (some cur, 0#usize) (some ord) (by simp) hloop
  intro k hk
  exact (hmain ord rfl hge).2 k hk (by simp)

private theorem bv64_one_shl_getLsbD_local {k j : Nat} (hk : k < 64) (hj : j < 64) :
    ((1#64) <<< k).getLsbD j = decide (j = k) := by
  rw [BitVec.getLsbD_shiftLeft, BitVec.getLsbD_one]
  by_cases hlt : j < k
  · have hne : j ≠ k := by omega
    simp [hj, hlt, hne]
  · by_cases heq : j = k
    · subst heq; simp [hj]
    · have hsub : j - k ≠ 0 := by omega
      simp [hj, hlt, hsub, heq]

private theorem u64_and_bit_ne_zero_iff (m : Std.U64) {idx : Nat} (hidx : idx < 64) :
    ((m &&& (⟨(1#64) <<< idx⟩ : Std.U64)) != 0#u64) = m.bv.getLsbD idx := by
  have hb : m.bv.getLsbD idx = decide (m.bv &&& ((1#64) <<< idx) ≠ 0#64) := by
    have h1 : ∀ j < 64, (m.bv &&& ((1#64) <<< idx)).getLsbD j =
        (m.bv.getLsbD idx && decide (j = idx)) := by
      intro j hj
      rw [BitVec.getLsbD_and, bv64_one_shl_getLsbD_local hidx hj]
      by_cases hji : j = idx
      · subst hji; simp
      · simp [hji]
    by_cases hset : m.bv.getLsbD idx = true
    · have hnz : m.bv &&& ((1#64) <<< idx) ≠ 0#64 := by
        intro hz
        have := h1 idx hidx
        rw [hz, BitVec.getLsbD_zero, hset] at this
        simp at this
      simp [hset, hnz]
    · simp only [Bool.not_eq_true] at hset
      have hz : m.bv &&& ((1#64) <<< idx) = 0#64 := by
        apply BitVec.eq_of_getLsbD_eq
        intro j hj
        rw [h1 j hj, hset]
        simp
      simp [hset, hz]
  rw [hb]
  by_cases hne : (m &&& (⟨(1#64) <<< idx⟩ : Std.U64)) = 0#u64
  · have hbv : m.bv &&& ((1#64) <<< idx) = 0#64 := congrArg U64.bv hne
    simp [hne, hbv]
  · have hbv : m.bv &&& ((1#64) <<< idx) ≠ 0#64 := by
      intro hc
      apply hne
      exact (u64_eq_iff_bv _ _).mpr hc
    simp [hne, hbv]

private theorem stack_loop3_ge_spec
    {m1 m2 : Std.U64} {v v1 : alloc.vec.Vec verifier.Type}
    {cur ord : Ordering}
    (hsub_mask : ∀ s < 64, m1.bv.getLsbD s = true → m2.bv.getLsbD s = true)
    (hlen1 : v.val.length = popcount64 m1.bv)
    (hlen2 : v1.val.length = popcount64 m2.bv)
    (hloop : verifier.Stack.Insts.CoreCmpPartialOrdStack.partial_cmp_loop3
      m1 v m2 v1 (some cur) 0#usize 0#usize 0#usize = ok (some ord))
    (hge : OrdIsGe ord) :
    ∀ s (hs : s < 64) (hb1 : m1.bv.getLsbD s = true),
      ∃ o,
        verifier.Type.Insts.CoreCmpPartialOrdType.partial_cmp
          (v.val[stackPos m1.bv s]'(by rw [hlen1]; exact stackPos_lt_popcount64 m1.bv hs hb1))
          (v1.val[stackPos m2.bv s]'(by
            rw [hlen2]; exact stackPos_lt_popcount64 m2.bv hs (hsub_mask s hs hb1))) = ok (some o) ∧
        OrdIsGe o := by
  unfold verifier.Stack.Insts.CoreCmpPartialOrdStack.partial_cmp_loop3 at hloop
  have hmain := loop_ok_pre_post
    (fun (opt1, ps1, po1, idx1) =>
      verifier.Stack.Insts.CoreCmpPartialOrdStack.partial_cmp_loop3.body m1 v m2 v1 opt1 ps1 po1 idx1)
    (fun p =>
      p.2.2.2.val ≤ 64 ∧
      p.2.1.val = popcountRange m1.bv p.2.2.2.val ∧
      p.2.2.1.val = popcountRange m2.bv p.2.2.2.val)
    (fun p out =>
      ∀ fin_o, out = some fin_o → OrdIsGe fin_o →
        (∃ c1, p.1 = some c1 ∧ OrdIsGe c1) ∧
        ∀ s (hs : s < 64) (hb1 : m1.bv.getLsbD s = true), p.2.2.2.val ≤ s →
          ∃ o,
            verifier.Type.Insts.CoreCmpPartialOrdType.partial_cmp
              (v.val[stackPos m1.bv s]'(by rw [hlen1]; exact stackPos_lt_popcount64 m1.bv hs hb1))
              (v1.val[stackPos m2.bv s]'(by
                rw [hlen2]; exact stackPos_lt_popcount64 m2.bv hs (hsub_mask s hs hb1))) = ok (some o) ∧
            OrdIsGe o)
    (by
      rintro ⟨opt1, ps1, po1, idx1⟩ ⟨opt2, ps2, po2, idx2⟩ out hpre hbody
      dsimp only at hpre ⊢
      rcases hpre with ⟨hidx_le, hps_eq, hpo_eq⟩
      unfold verifier.Stack.Insts.CoreCmpPartialOrdStack.partial_cmp_loop3.body at hbody
      by_cases hlt : idx1 < 64#usize
      · simp only [hlt, ↓reduceIte] at hbody
        have hidx_lt64 : idx1.val < 64 := by
          have : idx1.val < (64#usize).val := hlt
          rwa [usize_const_val_64] at this
        by_cases hsome : core.option.Option.is_some opt1 = true
        · simp only [hsome, ↓reduceIte] at hbody
          rw [bind_ok_eq (Stack_bit_spec hidx_lt64)] at hbody
          simp only [lift_eq_ok, bind_ok] at hbody
          rcases bind_eq_ok hbody with ⟨opt_res1, hopt1, hbody⟩
          rcases bind_eq_ok hbody with ⟨ps_next, hps_next, hbody⟩
          rcases bind_eq_ok hbody with ⟨po_next, hpo_next, hbody⟩
          rcases bind_eq_ok hbody with ⟨idx_next, hidx_add, hbody⟩
          cases ok_inj hbody
          have hidx2_val : idx2.val = idx1.val + 1 := by
            simpa [usize_one_val] using (uscalar_add_ok_val hidx_add).1
          have hb1_eq := u64_and_bit_ne_zero_iff m1 hidx_lt64
          have hb2_eq := u64_and_bit_ne_zero_iff m2 hidx_lt64
          have hps2_val : ps2.val = popcountRange m1.bv idx2.val := by
            rw [hidx2_val, popcountRange]
            rw [hb1_eq] at hps_next
            cases hb_m1 : m1.bv.getLsbD idx1.val <;> simp only [hb_m1, Bool.false_eq_true, ↓reduceIte] at hps_next ⊢
            · cases ok_inj hps_next; omega
            · have := (uscalar_add_ok_val hps_next).1
              rw [usize_one_val] at this; omega
          have hpo2_val : po2.val = popcountRange m2.bv idx2.val := by
            rw [hidx2_val, popcountRange]
            rw [hb2_eq] at hpo_next
            cases hb_m2 : m2.bv.getLsbD idx1.val <;> simp only [hb_m2, Bool.false_eq_true, ↓reduceIte] at hpo_next ⊢
            · cases ok_inj hpo_next; omega
            · have := (uscalar_add_ok_val hpo_next).1
              rw [usize_one_val] at this; omega
          refine ⟨⟨by omega, hps2_val, hpo2_val⟩, fun hIH fin_o hout hge_fin => ?_⟩
          obtain ⟨⟨c2, hopt2_eq, hge_c2⟩, htail⟩ := hIH fin_o hout hge_fin
          subst hopt2_eq
          rw [hb1_eq, hb2_eq] at hopt1
          cases hb_m1 : m1.bv.getLsbD idx1.val
          · simp only [hb_m1, Bool.false_eq_true, ↓reduceIte] at hopt1
            cases ok_inj hopt1
            refine ⟨⟨c2, rfl, hge_c2⟩, fun s hs hb1_s hle_s => ?_⟩
            have hne : s ≠ idx1.val := by intro hc; subst hc; rw [hb_m1] at hb1_s; cases hb1_s
            exact htail s hs hb1_s (by omega)
          · have hb_m2 : m2.bv.getLsbD idx1.val = true := hsub_mask idx1.val hidx_lt64 hb_m1
            simp only [hb_m1, hb_m2, ↓reduceIte] at hopt1
            rcases bind_eq_ok hopt1 with ⟨t, ht, hopt1⟩
            rcases bind_eq_ok hopt1 with ⟨t1, ht1, hopt1⟩
            have hps_lt : ps1.val < v.val.length := by
              rw [hps_eq, hlen1]; exact stackPos_lt_popcount64 m1.bv hidx_lt64 hb_m1
            have hpo_lt : po1.val < v1.val.length := by
              rw [hpo_eq, hlen2]; exact stackPos_lt_popcount64 m2.bv hidx_lt64 hb_m2
            have ht_eq : t = v.val[stackPos m1.bv idx1.val]'(by
                rw [hlen1]; exact stackPos_lt_popcount64 m1.bv hidx_lt64 hb_m1) := by
              have := (vec_index_usize_ok ht).2
              rw [List.getElem?_eq_getElem hps_lt] at this
              have h1 := Option.some.inj this.symm
              simpa [hps_eq, stackPos] using h1
            have ht1_eq : t1 = v1.val[stackPos m2.bv idx1.val]'(by
                rw [hlen2]; exact stackPos_lt_popcount64 m2.bv hidx_lt64 hb_m2) := by
              have := (vec_index_usize_ok ht1).2
              rw [List.getElem?_eq_getElem hpo_lt] at this
              have h1 := Option.some.inj this.symm
              simpa [hpo_eq, stackPos] using h1
            subst ht_eq; subst ht1_eq
            obtain ⟨⟨c1, rfl, hge_c1⟩, ⟨ord_idx, hcmp, hge_idx⟩⟩ :=
              combine_slot_ordering_ge_spec hopt1 hge_c2
            refine ⟨⟨c1, rfl, hge_c1⟩, fun s hs hb1_s hle_s => ?_⟩
            by_cases hsi : s = idx1.val
            · subst hsi; exact ⟨ord_idx, hcmp, hge_idx⟩
            · exact htail s hs hb1_s (by omega)
        · simp only [hsome, Bool.false_eq_true, ↓reduceIte] at hbody
          cases ok_inj hbody
      · simp only [hlt, ↓reduceIte] at hbody
        cases ok_inj hbody)
    (by
      rintro ⟨opt1, ps1, po1, idx1⟩ out hpre hbody
      dsimp only at hpre ⊢
      rcases hpre with ⟨hidx_le, _, _⟩
      unfold verifier.Stack.Insts.CoreCmpPartialOrdStack.partial_cmp_loop3.body at hbody
      by_cases hlt : idx1 < 64#usize
      · simp only [hlt, ↓reduceIte] at hbody
        by_cases hsome : core.option.Option.is_some opt1 = true
        · simp only [hsome, ↓reduceIte] at hbody
          rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
          simp only [lift_eq_ok, bind_ok] at hbody
          rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
          rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
          rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
          rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
          cases ok_inj hbody
        · simp only [hsome, Bool.false_eq_true, ↓reduceIte] at hbody
          cases ok_inj hbody
          cases opt1 with
          | none => intro _ hout; cases hout
          | some _ => simp [core.option.Option.is_some] at hsome
      · simp only [hlt, ↓reduceIte] at hbody
        cases ok_inj hbody
        have hidx64 : idx1.val = 64 := by
          have hn : ¬ (idx1.val < (64#usize).val) := hlt
          rw [usize_const_val_64] at hn
          omega
        intro fin_o hout hge_fin
        subst hout
        exact ⟨⟨fin_o, rfl, hge_fin⟩, fun s hs _ hle_s => by omega⟩)
    (some cur, 0#usize, 0#usize, 0#usize) (some ord) (by simp [popcountRange]) hloop
  intro s hs hb1
  exact (hmain ord rfl hge).2 s hs hb1 (by simp)

theorem Stack_partial_cmp_ge_spec
    {env : ExecEnv} {v1 v2 : Nat} {s1 s2 : verifier.Stack} {ord : Ordering}
    (hs1 : StackValid env v1 s1)
    (hs2 : StackValid env v2 s2)
    (hcmp : verifier.Stack.Insts.CoreCmpPartialOrdStack.partial_cmp s1 s2 = ok (some ord))
    (hge : OrdIsGe ord) :
    ∀ slot < 64,
      stackSlotType s1 slot = .ScalarValue uninitScalar64 ∨
      ∃ o,
        verifier.Type.Insts.CoreCmpPartialOrdType.partial_cmp
          (stackSlotType s1 slot) (stackSlotType s2 slot) = ok (some o) ∧
        OrdIsGe o := by
  unfold verifier.Stack.Insts.CoreCmpPartialOrdStack.partial_cmp at hcmp
  simp only [lift_eq_ok, bind_ok] at hcmp
  by_cases hi1 : (s1.mask &&& ~~~s2.mask) != 0#u64
  · simp only [hi1, ↓reduceIte] at hcmp
    by_cases hi3 : (~~~s1.mask &&& s2.mask) != 0#u64
    · simp only [hi3, ↓reduceIte] at hcmp; cases ok_inj hcmp
    · simp only [hi3, Bool.false_eq_true, ↓reduceIte] at hcmp
      by_cases hcom : (s1.mask &&& s2.mask) = 0#u64
      · simp only [hcom, ↓reduceIte, ok.injEq, Option.some.injEq] at hcmp
        subst hcmp; rcases hge with hc | hc <;> cases hc
      · simp only [hcom, ↓reduceIte] at hcmp
        by_cases hm_eq : s1.mask = s2.mask
        · simp only [hm_eq, ↓reduceIte] at hcmp
          have hnot := stack_loop0_lt_not_ge s1.data s2.data (some .lt) 0#usize (some ord)
            (by rintro (hc | hc) <;> cases hc) hcmp
          exact False.elim (hnot hge)
        · simp only [hm_eq, ↓reduceIte] at hcmp
          have hnot := stack_loop1_lt_not_ge s1.mask s1.data s2.mask s2.data (some .lt)
            0#usize 0#usize 0#usize (some ord) (by rintro (hc | hc) <;> cases hc) hcmp
          exact False.elim (hnot hge)
  · simp only [hi1, Bool.false_eq_true, ↓reduceIte] at hcmp
    have hi1_zero : s1.mask.bv &&& ~~~s2.mask.bv = 0#64 := by
      have hu : s1.mask &&& ~~~s2.mask = 0#u64 := by
        by_contra hne; exact hi1 (bne_iff_ne.mpr hne)
      exact congrArg U64.bv hu
    have hsub_mask : ∀ s < 64, s1.mask.bv.getLsbD s = true → s2.mask.bv.getLsbD s = true := by
      intro s hs_lt64 hbs1
      have hz : (s1.mask.bv &&& ~~~s2.mask.bv).getLsbD s = false := by
        rw [hi1_zero]; simp
      rw [BitVec.getLsbD_and, BitVec.getLsbD_not, hbs1] at hz
      cases hbs2 : s2.mask.bv.getLsbD s
      · rw [hbs2] at hz; simp [hs_lt64] at hz
      · rfl
    rcases bind_eq_ok hcmp with ⟨result, _, hcmp⟩
    by_cases hcom : (s1.mask &&& s2.mask) = 0#u64
    · have hcom_bv : s1.mask.bv &&& s2.mask.bv = 0#64 := congrArg U64.bv hcom
      have hm1_zero : s1.mask.bv = 0#64 := by
        generalize s1.mask.bv = a, s2.mask.bv = b at hi1_zero hcom_bv
        revert hi1_zero hcom_bv; bv_decide
      intro slot _
      left
      unfold stackSlotType
      rw [hm1_zero]
      simp
    · simp only [hcom, ↓reduceIte] at hcmp
      by_cases hm_eq : s1.mask = s2.mask
      · simp only [hm_eq, ↓reduceIte] at hcmp
        have hlen_eq : s1.data.val.length = s2.data.val.length := by
          rw [hs1.1, hs2.1, hm_eq]
        have hl2 := stack_loop2_ge_spec hlen_eq hcmp hge
        intro slot hslot
        by_cases hbs1 : s1.mask.bv.getLsbD slot = true
        · right
          have hbs2 : s2.mask.bv.getLsbD slot = true := hm_eq ▸ hbs1
          have hpos1 : stackPos s1.mask.bv slot < s1.data.val.length := by
            rw [hs1.1]; exact stackPos_lt_popcount64 s1.mask.bv hslot hbs1
          obtain ⟨o, ho, hge_o⟩ := hl2 (stackPos s1.mask.bv slot) hpos1
          refine ⟨o, ?_, hge_o⟩
          unfold stackSlotType
          simp only [hbs2, ↓reduceIte, hm_eq]
          have hpos2 : stackPos s2.mask.bv slot < s2.data.val.length := hlen_eq ▸ (hm_eq ▸ hpos1)
          rw [List.getD_eq_getElem _ _ (hm_eq ▸ hpos1), List.getD_eq_getElem _ _ hpos2]
          simpa [hm_eq] using ho
        · left
          unfold stackSlotType
          simp [hbs1]
      · simp only [hm_eq, ↓reduceIte] at hcmp
        have hl3 := stack_loop3_ge_spec hsub_mask hs1.1 hs2.1 hcmp hge
        intro slot hslot
        by_cases hbs1 : s1.mask.bv.getLsbD slot = true
        · right
          have hbs2 : s2.mask.bv.getLsbD slot = true := hsub_mask slot hslot hbs1
          obtain ⟨o, ho, hge_o⟩ := hl3 slot hslot hbs1
          refine ⟨o, ?_, hge_o⟩
          unfold stackSlotType
          simp only [hbs1, hbs2, ↓reduceIte]
          have hpos1 : stackPos s1.mask.bv slot < s1.data.val.length := by
            rw [hs1.1]; exact stackPos_lt_popcount64 s1.mask.bv hslot hbs1
          have hpos2 : stackPos s2.mask.bv slot < s2.data.val.length := by
            rw [hs2.1]; exact stackPos_lt_popcount64 s2.mask.bv hslot hbs2
          rw [List.getD_eq_getElem _ _ hpos1, List.getD_eq_getElem _ _ hpos2]
          exact ho
        · left
          unfold stackSlotType
          simp [hbs1]

private theorem drop_cons_of_lt {α : Type u} (l : List α) {i : Nat} (hlt : i < l.length) :
    l.drop i = (l[i]'hlt) :: l.drop (i + 1) :=
  List.drop_eq_getElem_cons hlt

theorem compare_bounds_maps_ge_spec
    {m1 m2 : verifier.BoundsMap} {res_in ord : Ordering}
    (hcmp : verifier.compare_bounds_maps m1 m2 res_in = ok (some ord))
    (hge : OrdIsGe ord) :
    OrdIsGe res_in ∧
    ∀ m b1, (m, b1) ∈ m1.entries.val →
      ∃ b2, (m, b2) ∈ m2.entries.val ∧ b1.bv ≤ b2.bv := by
  unfold verifier.compare_bounds_maps verifier.compare_bounds_maps_loop at hcmp
  set v := m1.entries
  set v1 := m2.entries
  set len1 := alloc.vec.Vec.len v
  set len2 := alloc.vec.Vec.len v1
  have hmain := loop_ok_pre_post
    (fun (r1, i1, i2) => verifier.compare_bounds_maps_loop.body v v1 len1 len2 r1 i1 i2)
    (fun p => p.2.1.val ≤ v.val.length ∧ p.2.2.val ≤ v1.val.length)
    (fun p out =>
      ∀ fin_o, out = some fin_o → OrdIsGe fin_o →
        OrdIsGe p.1 ∧
        ∀ m b1, (m, b1) ∈ v.val.drop p.2.1.val →
          ∃ b2, (m, b2) ∈ v1.val.drop p.2.2.val ∧ b1.bv ≤ b2.bv)
    (by
      rintro ⟨r1, i1, i2⟩ ⟨r2, j1, j2⟩ out hpre hbody
      dsimp only at hpre ⊢
      rcases hpre with ⟨hi1_le, hi2_le⟩
      unfold verifier.compare_bounds_maps_loop.body at hbody
      by_cases h1 : i1 < len1
      · simp only [h1, ↓reduceIte] at hbody
        have hi1_lt : i1.val < v.val.length := h1
        by_cases h2 : i2 < len2
        · simp only [h2, ↓reduceIte] at hbody
          have hi2_lt : i2.val < v1.val.length := h2
          rcases bind_eq_ok hbody with ⟨p1, hv_idx, _⟩
          rcases p1 with ⟨k1, val1⟩
          rw [bind_pair_ok_eq hv_idx] at hbody
          rcases bind_eq_ok hbody with ⟨p2, hv1_idx, _⟩
          rcases p2 with ⟨k2, val2⟩
          rw [bind_pair_ok_eq hv1_idx] at hbody
          have hk1_eq : (k1, val1) = v.val[i1.val]'hi1_lt := by
            have := (vec_index_usize_ok hv_idx).2
            rw [List.getElem?_eq_getElem hi1_lt] at this
            exact Option.some.inj this.symm
          have hk2_eq : (k2, val2) = v1.val[i2.val]'hi2_lt := by
            have := (vec_index_usize_ok hv1_idx).2
            rw [List.getElem?_eq_getElem hi2_lt] at this
            exact Option.some.inj this.symm
          rw [bind_ok_eq (MemoryId_cmp_id_spec k1 k2)] at hbody
          rcases bind_eq_ok hbody with ⟨p_step, hstep, _⟩
          rcases p_step with ⟨i11, i21, ord_step⟩
          rw [bind_triple_ok_eq hstep] at hbody
          rcases bind_eq_ok hbody with ⟨o1, hassoc, hbody⟩
          cases o1 with
          | none => cases ok_inj hbody
          | some next_r =>
            cases ok_inj hbody
            cases hcmp_k : pureMemoryIdCmp k1 k2 <;> simp only [hcmp_k] at hstep
            · -- `Ordering.lt`
              rcases bind_eq_ok hstep with ⟨i12, hadd1, hstep⟩
              rcases Prod.mk.inj (ok_inj hstep) with ⟨rfl, hp⟩
              rcases Prod.mk.inj hp with ⟨rfl, rfl⟩
              have hj1_val : i12.val = i1.val + 1 := by
                simpa [usize_one_val] using (uscalar_add_ok_val hadd1).1
              refine ⟨⟨by omega, hi2_le⟩, fun hIH fin_o hout hge_fin => ?_⟩
              obtain ⟨hge_next, _⟩ := hIH fin_o hout hge_fin
              obtain ⟨_, hge_lt⟩ := associate_orderings_ge_iff hassoc hge_next
              rcases hge_lt with hc | hc <;> cases hc
            · -- `Ordering.eq`
              rcases bind_eq_ok hstep with ⟨i12, hadd1, hstep⟩
              rcases bind_eq_ok hstep with ⟨i22, hadd2, hstep⟩
              rw [bind_ok_eq (verifier_cmp_u64_spec val2 val1)] at hstep
              rcases Prod.mk.inj (ok_inj hstep) with ⟨rfl, hp⟩
              rcases Prod.mk.inj hp with ⟨rfl, rfl⟩
              have hj1_val : i12.val = i1.val + 1 := by
                simpa [usize_one_val] using (uscalar_add_ok_val hadd1).1
              have hj2_val : i22.val = i2.val + 1 := by
                simpa [usize_one_val] using (uscalar_add_ok_val hadd2).1
              refine ⟨⟨by omega, by omega⟩, fun hIH fin_o hout hge_fin => ?_⟩
              obtain ⟨hge_next, htail⟩ := hIH fin_o hout hge_fin
              obtain ⟨hge_r1, hge_ord1⟩ := associate_orderings_ge_iff hassoc hge_next
              have hk_eq : k1 = k2 := (pureMemoryIdCmp_eq_iff k1 k2).mp hcmp_k
              have hval_le : val1.bv ≤ val2.bv := by
                unfold pureCmpNat at hge_ord1
                by_cases hlt_v : val2.bv.toNat < val1.bv.toNat
                · simp only [hlt_v, ↓reduceIte] at hge_ord1
                  rcases hge_ord1 with hc | hc <;> cases hc
                · rw [BitVec.le_def]; omega
              refine ⟨hge_r1, fun m b1 hmem => ?_⟩
              rw [drop_cons_of_lt v.val hi1_lt, ← hk1_eq, List.mem_cons] at hmem
              rcases hmem with hhead | hmem_tail
              · rcases Prod.mk.inj hhead with ⟨rfl, rfl⟩
                refine ⟨val2, ?_, hval_le⟩
                rw [drop_cons_of_lt v1.val hi2_lt, ← hk2_eq, hk_eq]
                exact List.mem_cons_self
              · rw [← hj1_val] at hmem_tail
                obtain ⟨b2, hb2_mem, hb2_le⟩ := htail m b1 hmem_tail
                rw [hj2_val] at hb2_mem
                refine ⟨b2, ?_, hb2_le⟩
                rw [drop_cons_of_lt v1.val hi2_lt]
                exact List.mem_cons_of_mem _ hb2_mem
            · -- `Ordering.gt`
              rcases bind_eq_ok hstep with ⟨i22, hadd2, hstep⟩
              rcases Prod.mk.inj (ok_inj hstep) with ⟨rfl, hp⟩
              rcases Prod.mk.inj hp with ⟨rfl, rfl⟩
              have hj2_val : i22.val = i2.val + 1 := by
                simpa [usize_one_val] using (uscalar_add_ok_val hadd2).1
              refine ⟨⟨hi1_le, by omega⟩, fun hIH fin_o hout hge_fin => ?_⟩
              obtain ⟨hge_next, htail⟩ := hIH fin_o hout hge_fin
              obtain ⟨hge_r1, _⟩ := associate_orderings_ge_iff hassoc hge_next
              refine ⟨hge_r1, fun m b1 hmem => ?_⟩
              obtain ⟨b2, hb2_mem, hb2_le⟩ := htail m b1 hmem
              rw [hj2_val] at hb2_mem
              refine ⟨b2, ?_, hb2_le⟩
              rw [drop_cons_of_lt v1.val hi2_lt]
              exact List.mem_cons_of_mem _ hb2_mem
        · simp only [h2, ↓reduceIte] at hbody
          rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
          cases ok_inj hbody
      · simp only [h1, ↓reduceIte] at hbody
        split at hbody
        · rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
          cases ok_inj hbody
        · cases ok_inj hbody)
    (by
      rintro ⟨r1, i1, i2⟩ out hpre hbody
      dsimp only at hpre ⊢
      rcases hpre with ⟨hi1_le, _⟩
      unfold verifier.compare_bounds_maps_loop.body at hbody
      by_cases h1 : i1 < len1
      · simp only [h1, ↓reduceIte] at hbody
        by_cases h2 : i2 < len2
        · simp only [h2, ↓reduceIte] at hbody
          rcases bind_eq_ok hbody with ⟨p1, hv_idx, _⟩
          rcases p1 with ⟨k1, val1⟩
          rw [bind_pair_ok_eq hv_idx] at hbody
          rcases bind_eq_ok hbody with ⟨p2, hv1_idx, _⟩
          rcases p2 with ⟨k2, val2⟩
          rw [bind_pair_ok_eq hv1_idx] at hbody
          rw [bind_ok_eq (MemoryId_cmp_id_spec k1 k2)] at hbody
          rcases bind_eq_ok hbody with ⟨p_step, hstep, _⟩
          rcases p_step with ⟨_, _, _⟩
          rw [bind_triple_ok_eq hstep] at hbody
          rcases bind_eq_ok hbody with ⟨o1, _, hbody⟩
          cases o1 with
          | none =>
            cases ok_inj hbody
            intro _ hout; cases hout
          | some _ => cases ok_inj hbody
        · simp only [h2, ↓reduceIte] at hbody
          rcases bind_eq_ok hbody with ⟨o, hassoc, hbody⟩
          cases ok_inj hbody
          intro fin_o hout hge_fin
          subst hout
          obtain ⟨_, hge_lt⟩ := associate_orderings_ge_iff hassoc hge_fin
          rcases hge_lt with hc | hc <;> cases hc
      · simp only [h1, ↓reduceIte] at hbody
        have hi1_eq : i1.val = v.val.length := by
          have hn : ¬ (i1.val < v.val.length) := h1
          omega
        have hdrop_nil : v.val.drop i1.val = [] := by
          rw [hi1_eq]; exact List.drop_length
        by_cases h2 : i2 < len2
        · simp only [h2, ↓reduceIte] at hbody
          rcases bind_eq_ok hbody with ⟨o, hassoc, hbody⟩
          cases ok_inj hbody
          intro fin_o hout hge_fin
          subst hout
          obtain ⟨hge_r1, _⟩ := associate_orderings_ge_iff hassoc hge_fin
          refine ⟨hge_r1, fun m b1 hmem => ?_⟩
          rw [hdrop_nil] at hmem; cases hmem
        · simp only [h2, ↓reduceIte] at hbody
          cases ok_inj hbody
          intro fin_o hout hge_fin
          cases Option.some.inj hout
          refine ⟨hge_fin, fun m b1 hmem => ?_⟩
          rw [hdrop_nil] at hmem; cases hmem)
    (res_in, 0#usize, 0#usize) (some ord) (by simp) hcmp
  obtain ⟨hge_in, hmem_drop⟩ := hmain ord rfl hge
  simp only [usize_const_val_0, List.drop_zero] at hmem_drop
  exact ⟨hge_in, hmem_drop⟩

theorem ComputationContext_ge_subsumes
    {env : ExecEnv} {v1 v2 : Nat}
    {broad narrow : verifier.ComputationContext}
    (hbroad : CompCtxValid env v1 broad)
    (hnarrow : CompCtxValid env v2 narrow)
    (hge_call : core.cmp.PartialOrd.ge.trait_default
      verifier.ComputationContext.Insts.CoreCmpPartialOrdComputationContext
      broad narrow = ok true) :
    ∀ {deps : verifier.DataDependencies} {ρ : Std.U64 → Std.U64}
      {resActive isNullId : MemoryId → Bool} {st₁ st₂ : MachineState},
      StateRel2Deps env narrow deps ρ resActive isNullId st₁ st₂ →
      StateRel2Deps env broad deps ρ resActive isNullId st₁ st₂ := by
  intro _ _ _ _ _ _ hrel
  unfold core.cmp.PartialOrd.ge.trait_default core.cmp.PartialOrd.ge.default
    core.cmp.PartialOrd.ge_body
    verifier.ComputationContext.Insts.CoreCmpPartialOrdComputationContext at hge_call
  dsimp only at hge_call
  rcases bind_eq_ok hge_call with ⟨cmp, hpcmp, hge_bool⟩
  have hge_opt : ∃ ord, cmp = some ord ∧ OrdIsGe ord := by
    cases cmp with
    | none => simp at hge_bool
    | some ord =>
      refine ⟨ord, rfl, ?_⟩
      have hprop : some ord = some Ordering.gt ∨ some ord = some Ordering.eq := by
        simpa using ok_inj hge_bool
      rcases hprop with h1 | h1
      · exact Or.inr (Option.some.inj h1)
      · exact Or.inl (Option.some.inj h1)
  rcases hge_opt with ⟨ord, rfl, hge_ord⟩
  unfold verifier.ComputationContext.Insts.CoreCmpPartialOrdComputationContext.partial_cmp at hpcmp
  by_cases hpc_ne : broad.pc != narrow.pc
  · simp [hpc_ne] at hpcmp
  · simp only [hpc_ne, Bool.false_eq_true, ↓reduceIte] at hpcmp
    have hpc_eq : broad.pc = narrow.pc := by
      by_contra hne; exact hpc_ne (bne_iff_ne.mpr hne)
    rw [bind_ok_eq (IdSet_len_spec broad.resources),
        bind_ok_eq (IdSet_len_spec narrow.resources)] at hpcmp
    by_cases hlen_ne : alloc.vec.Vec.len broad.resources.entries != alloc.vec.Vec.len narrow.resources.entries
    · simp [hlen_ne] at hpcmp
    · simp only [hlen_ne, Bool.false_eq_true, ↓reduceIte] at hpcmp
      rcases bind_eq_ok hpcmp with ⟨o_reg, hreg_cmp, hpcmp⟩
      cases o_reg with
      | none => simp at hpcmp
      | some reg_ord =>
        dsimp only at hpcmp
        rcases bind_eq_ok hpcmp with ⟨o_stk, hstk_cmp, hpcmp⟩
        cases o_stk with
        | none => simp at hpcmp
        | some stk_ord =>
          dsimp only at hpcmp
          rcases bind_eq_ok hpcmp with ⟨o_assoc, hassoc, hpcmp⟩
          cases o_assoc with
          | none => simp at hpcmp
          | some res2 =>
            dsimp only at hpcmp
            obtain ⟨r_eq, hr_eq, hr_iff⟩ := IdSet_eq_spec broad.resources narrow.resources
            obtain ⟨nr, hnr, hnr_iff⟩ :=
              PartialEq_ne_of_eq verifier.IdSet.Insts.CoreCmpPartialEqIdSet
                broad.resources narrow.resources hr_eq hr_iff
            rw [bind_ok_eq hnr] at hpcmp
            cases nr with
            | true => simp at hpcmp
            | false =>
              simp only [Bool.false_eq_true, ↓reduceIte] at hpcmp
              have hres_eq : broad.resources = narrow.resources := by
                by_contra hne; exact absurd (hnr_iff.mpr hne) (by simp)
              obtain ⟨hge_res2, hbnds_sub⟩ := compare_bounds_maps_ge_spec hpcmp hge_ord
              obtain ⟨hge_reg, hge_stk⟩ := associate_orderings_ge_iff hassoc hge_res2
              have hregs_sub := compare_registers_ge_spec hreg_cmp hge_reg
              have hstk_sub := Stack_partial_cmp_ge_spec hbroad.2.1 hnarrow.2.1 hstk_cmp hge_stk
              rcases hrel with ⟨hpc1, hpc2, hw, harr, hnull, hnd, hlive, hbnds, hr10_1, hr10_2, hrg, hsk⟩
              refine ⟨hpc_eq ▸ hpc1, hpc_eq ▸ hpc2, hw, harr, hnull, hnd,
                hres_eq ▸ hlive, ?_, hr10_1, hr10_2, ?_, ?_⟩
              · intro m b1 hm1
                obtain ⟨b2, hm2, hle_b⟩ := hbnds_sub m b1 hm1
                exact BitVec.le_trans hle_b (hbnds m b2 hm2)
              · intro r hr hlen_b hd
                have hlen_n : r < narrow.registers.val.length := by rw [array10_len]; exact hr
                obtain ⟨o_r, ho_r, hge_or⟩ := hregs_sub r hr
                have ht1 : TypeValid env v1 (broad.registers.val[r]'hlen_b) :=
                  hbroad.1 _ (List.getElem_mem hlen_b)
                exact Type_subsumes_rel2 ht1 ho_r hge_or (hrg r hr hlen_n hd)
              · intro slot hslot hd
                rcases hstk_sub slot hslot with huninit | ⟨o_s, ho_s, hge_os⟩
                · rw [huninit]; simp [StackSlotRel2]
                · have ht1 : TypeValid env v1 (stackSlotType broad.stack slot) :=
                    stackSlotType_valid hbroad.2.1 slot hslot
                  exact Type_subsumes_stackSlotRel2 ht1 ho_s hge_os (hsk slot hslot hd)

/-- Invariant on a `TerminatingContexts` entry list at a prune point. -/
def TerminatingContextsInv (prog : Program) (env : ExecEnv)
    (vCounter maxFuel : Nat) (tc : verifier.TerminatingContexts) : Prop :=
  ∀ entry ∈ tc.contexts.val,
    CompCtxValid env vCounter entry.computation_context ∧
    ∃ F ≤ maxFuel, SemTerminatesIn prog env vCounter entry.computation_context entry.dependencies F

theorem TerminatingContextsInv_mono
    {prog : Program} {env : ExecEnv} {v₁ v₂ m₁ m₂ : Nat}
    {tc : verifier.TerminatingContexts}
    (hv : v₁ ≤ v₂) (hm : m₁ ≤ m₂)
    (h : TerminatingContextsInv prog env v₁ m₁ tc) :
    TerminatingContextsInv prog env v₂ m₂ tc := by
  intro entry hentry
  obtain ⟨hvalid, F, hF_le, hsem⟩ := h entry hentry
  exact ⟨CompCtxValid_mono hv hvalid, F, Nat.le_trans hF_le hm,
    SemTerminatesIn_mono_vCounter hv hsem⟩

theorem find_greater_or_equal_deps_sound
    {prog : Program} {env : ExecEnv} {vCounter maxFuel : Nat}
    {tc : verifier.TerminatingContexts} {ctx : verifier.ComputationContext}
    {deps : verifier.DataDependencies}
    (htc : TerminatingContextsInv prog env vCounter maxFuel tc)
    (hctx : CompCtxValid env vCounter ctx)
    (hfind : verifier.TerminatingContexts.find_greater_or_equal_deps tc ctx = ok (some deps)) :
    ∃ F ≤ maxFuel, SemTerminatesIn prog env vCounter ctx deps F := by
  unfold verifier.TerminatingContexts.find_greater_or_equal_deps
    verifier.TerminatingContexts.find_greater_or_equal at hfind
  rcases bind_eq_ok hfind with ⟨opt_c, hge_loop, hfind⟩
  cases opt_c with
  | none => simp at hfind
  | some c_hit =>
    cases Option.some.inj (ok_inj hfind)
    by_cases hlen0 : alloc.vec.Vec.len tc.contexts = 0#usize
    · simp [hlen0] at hge_loop
    · simp only [hlen0, ↓reduceIte] at hge_loop
      rcases bind_eq_ok hge_loop with ⟨ks, _, hge_loop⟩
      unfold verifier.TerminatingContexts.find_greater_or_equal_loop at hge_loop
      have hmem_ge : c_hit ∈ tc.contexts.val ∧
          core.cmp.PartialOrd.ge.trait_default
            verifier.ComputationContext.Insts.CoreCmpPartialOrdComputationContext
            c_hit.computation_context ctx = ok true := by
        have hmain := loop_ok_induct
          (fun i1 => verifier.TerminatingContexts.find_greater_or_equal_loop.body
            tc.contexts ctx ks i1)
          (fun i1 => i1.val ≤ tc.contexts.val.length)
          (fun out => ∀ c, out = some c →
            c ∈ tc.contexts.val ∧
            core.cmp.PartialOrd.ge.trait_default
              verifier.ComputationContext.Insts.CoreCmpPartialOrdComputationContext
              c.computation_context ctx = ok true)
          ?_ (alloc.vec.Vec.len tc.contexts) (some c_hit) (Nat.le_refl _) hge_loop
        · exact hmain c_hit rfl
        · intro i1 hle_i
          unfold verifier.TerminatingContexts.find_greater_or_equal_loop.body
          constructor
          · intro i2 hbody
            by_cases hgt : i1 > 0#usize
            · simp only [hgt, ↓reduceIte] at hbody
              rcases bind_eq_ok hbody with ⟨i_prev, _, hbody⟩
              rcases bind_eq_ok hbody with ⟨_, hidx, hbody⟩
              have hi_lt : i_prev.val < tc.contexts.val.length := (vec_index_usize_ok hidx).1
              rcases bind_eq_ok hbody with ⟨b, _, hbody⟩
              cases b with
              | false => cases ok_inj hbody; omega
              | true =>
                simp only [↓reduceIte] at hbody
                rcases bind_eq_ok hbody with ⟨b1, _, hbody⟩
                cases b1 <;> simp only [Bool.false_eq_true, ↓reduceIte] at hbody <;>
                  cases ok_inj hbody
                omega
            · simp only [hgt, ↓reduceIte] at hbody
              cases ok_inj hbody
          · intro out hbody
            by_cases hgt : i1 > 0#usize
            · simp only [hgt, ↓reduceIte] at hbody
              rcases bind_eq_ok hbody with ⟨i_prev, hsub, hbody⟩
              rcases bind_eq_ok hbody with ⟨c_cur, hidx, hbody⟩
              have hc_mem : c_cur ∈ tc.contexts.val := by
                obtain ⟨hlt_idx, hget⟩ := vec_index_usize_ok hidx
                rw [List.getElem?_eq_getElem hlt_idx] at hget
                cases Option.some.inj hget
                exact List.getElem_mem hlt_idx
              rcases bind_eq_ok hbody with ⟨b, _, hbody⟩
              cases b with
              | false => cases ok_inj hbody
              | true =>
                simp only [↓reduceIte] at hbody
                rcases bind_eq_ok hbody with ⟨b1, hb1, hbody⟩
                cases b1 with
                | false => cases ok_inj hbody
                | true =>
                  simp only [↓reduceIte] at hbody
                  cases ok_inj hbody
                  intro c hc_eq
                  cases Option.some.inj hc_eq
                  exact ⟨hc_mem, hb1⟩
            · simp only [hgt, ↓reduceIte] at hbody
              cases ok_inj hbody
              intro _ hc_eq; cases hc_eq
      obtain ⟨hc_mem, hge_cc⟩ := hmem_ge
      obtain ⟨hbroad_valid, F, hF_le, hsem_broad⟩ := htc c_hit hc_mem
      refine ⟨F, hF_le, ?_⟩
      intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
      exact hsem_broad l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ
        (ComputationContext_ge_subsumes hbroad_valid hctx hge_cc hrel)

private theorem TerminatingContext_clone_spec (t : verifier.TerminatingContext) :
    verifier.TerminatingContext.Insts.CoreCloneClone.clone t = ok t := by
  unfold verifier.TerminatingContext.Insts.CoreCloneClone.clone
  rw [bind_ok_eq (ComputationContext_clone_spec t.computation_context)]

theorem TerminatingContexts_insert_sound
    {prog : Program} {env : ExecEnv} {vCounter maxFuel F : Nat}
    {tc tc' : verifier.TerminatingContexts}
    {ctx : verifier.ComputationContext} {deps : verifier.DataDependencies}
    (htc : TerminatingContextsInv prog env vCounter maxFuel tc)
    (hctx : CompCtxValid env vCounter ctx)
    (hF_le : F ≤ maxFuel)
    (hsem : SemTerminatesIn prog env vCounter ctx deps F)
    (hins : verifier.TerminatingContexts.insert tc ctx deps = ok tc') :
    TerminatingContextsInv prog env vCounter maxFuel tc' := by
  unfold verifier.TerminatingContexts.insert at hins
  rcases bind_eq_ok hins with ⟨ks, _, hins⟩
  rcases bind_eq_ok hins with ⟨v_out, hloop0, hins⟩
  cases ok_inj hins
  have hclone_sub : ∀ (s : Slice verifier.TerminatingContext) (j lim : Std.Usize)
      (out : alloc.vec.Vec verifier.TerminatingContext),
      verifier.clone_terminating_contexts_range s j lim = ok out →
      (∀ e ∈ out.val, e ∈ s.val) := by
    intro s j lim out hcp
    unfold verifier.clone_terminating_contexts_range verifier.clone_terminating_contexts_range_loop at hcp
    exact loop_ok_induct
      (fun (nc1, j1) => verifier.clone_terminating_contexts_range_loop.body s lim nc1 j1)
      (fun (nc1, _) => ∀ e ∈ nc1.val, e ∈ s.val)
      (fun res => ∀ e ∈ res.val, e ∈ s.val)
      (by
        rintro ⟨nc1, j1⟩ hinv
        unfold verifier.clone_terminating_contexts_range_loop.body
        dsimp only
        constructor
        · rintro ⟨nc2, j2⟩ hbody
          split at hbody
          · rcases bind_eq_ok hbody with ⟨elem, hidx, hbody⟩
            rw [bind_ok_eq (TerminatingContext_clone_spec elem)] at hbody
            rcases bind_eq_ok hbody with ⟨nc_p, hpush, hbody⟩
            rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
            cases ok_inj hbody
            have helem_mem : elem ∈ s.val := by
              obtain ⟨hlt_j, hget⟩ := slice_index_usize_ok hidx
              rw [List.getElem?_eq_getElem hlt_j] at hget
              cases Option.some.inj hget
              exact List.getElem_mem hlt_j
            intro e he
            rw [vec_push_ok_val hpush, List.mem_append, List.mem_singleton] at he
            rcases he with he1 | rfl
            · exact hinv e he1
            · exact helem_mem
          · cases ok_inj hbody
        · intro res hbody
          split at hbody
          · rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
            rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
            rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
            rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
            cases ok_inj hbody
          · cases ok_inj hbody; exact hinv)
      (alloc.vec.Vec.new verifier.TerminatingContext, j) out
      (by intro _ h; cases h) hcp
  have happend_sub : ∀ (ctxs out : alloc.vec.Vec verifier.TerminatingContext)
      (ent : verifier.TerminatingContext),
      verifier.append_bounded_terminating_context ctxs ent = ok out →
      ∀ e ∈ out.val, e ∈ ctxs.val ∨ e = ent := by
    intro ctxs out ent hap
    unfold verifier.append_bounded_terminating_context at hap
    rcases bind_eq_ok hap with ⟨ctxs1, hctxs1, hpush⟩
    have hsub1 : ∀ e ∈ ctxs1.val, e ∈ ctxs.val := by
      split at hctxs1
      · rcases bind_eq_ok hctxs1 with ⟨_, _, hctxs1⟩
        rcases bind_eq_ok hctxs1 with ⟨_, _, hctxs1⟩
        intro e he
        have hmem := hclone_sub (alloc.vec.Vec.deref ctxs) _ _ ctxs1 hctxs1 e he
        simpa [alloc.vec.Vec.deref] using hmem
      · cases ok_inj hctxs1
        exact fun _ h => h
    intro e he
    rw [vec_push_ok_val hpush, List.mem_append, List.mem_singleton] at he
    rcases he with he1 | rfl
    · exact Or.inl (hsub1 e he1)
    · exact Or.inr rfl
  unfold verifier.TerminatingContexts.insert_loop at hloop0
  have hout_mem : ∀ e ∈ v_out.val,
      e ∈ tc.contexts.val ∨ (e.computation_context = ctx ∧ e.dependencies = deps) := by
    exact loop_ok_induct
      (fun (ar1, nc1, i1) =>
        verifier.TerminatingContexts.insert_loop.body tc ctx deps ks ar1 nc1 i1)
      (fun (_, nc1, _) => ∀ e ∈ nc1.val, e ∈ tc.contexts.val)
      (fun res => ∀ e ∈ res.val,
        e ∈ tc.contexts.val ∨ (e.computation_context = ctx ∧ e.dependencies = deps))
      (by
        rintro ⟨ar1, nc1, i1⟩ hinv
        unfold verifier.TerminatingContexts.insert_loop.body
        dsimp only
        constructor
        · rintro ⟨ar2, nc2, i2⟩ hbody
          split at hbody
          · rcases bind_eq_ok hbody with ⟨elem, hidx, hbody⟩
            have helem_mem : elem ∈ tc.contexts.val := by
              obtain ⟨hlt_i, hget⟩ := vec_index_usize_ok hidx
              rw [List.getElem?_eq_getElem hlt_i] at hget
              cases Option.some.inj hget
              exact List.getElem_mem hlt_i
            rcases bind_eq_ok hbody with ⟨p_kd, hkd, _⟩
            rcases p_kd with ⟨keep, dom⟩
            rw [bind_pair_ok_eq hkd] at hbody
            cases dom with
            | true => simp at hbody
            | false =>
              simp only [Bool.false_eq_true, ↓reduceIte] at hbody
              rcases bind_eq_ok hbody with ⟨⟨ar_mid, nc_mid⟩, hstep, hbody⟩
              rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
              cases ok_inj hbody
              cases keep with
              | true =>
                simp only [↓reduceIte] at hstep
                rcases bind_eq_ok hstep with ⟨nc_v, hnc, hstep⟩
                rcases Prod.mk.inj (ok_inj hstep) with ⟨_, rfl⟩
                cases ar1 with
                | false => cases ok_inj hnc; exact hinv
                | true =>
                  simp only [↓reduceIte, TerminatingContext_clone_spec, bind_ok] at hnc
                  intro e he
                  rw [vec_push_ok_val hnc, List.mem_append, List.mem_singleton] at he
                  rcases he with he1 | rfl
                  · exact hinv e he1
                  · exact helem_mem
              | false =>
                simp only [Bool.false_eq_true, ↓reduceIte] at hstep
                cases ar1 with
                | true =>
                  simp only [↓reduceIte] at hstep
                  rcases Prod.mk.inj (ok_inj hstep) with ⟨_, rfl⟩
                  exact hinv
                | false =>
                  simp only [Bool.false_eq_true, ↓reduceIte] at hstep
                  rcases bind_eq_ok hstep with ⟨nc_cp, hcp, hstep⟩
                  rcases Prod.mk.inj (ok_inj hstep) with ⟨_, rfl⟩
                  intro e he
                  have hmem := hclone_sub (alloc.vec.Vec.deref tc.contexts) 0#usize i1 nc_cp hcp e he
                  simpa [alloc.vec.Vec.deref] using hmem
          · split at hbody <;> {
              rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
              cases ok_inj hbody
            }
        · intro res hbody
          split at hbody
          · rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
            rcases bind_eq_ok hbody with ⟨p_kd, hkd, _⟩
            rcases p_kd with ⟨keep, dom⟩
            rw [bind_pair_ok_eq hkd] at hbody
            cases dom with
            | true =>
              simp only [↓reduceIte] at hbody
              cases ok_inj hbody
              exact fun e he => Or.inl he
            | false =>
              simp only [Bool.false_eq_true, ↓reduceIte] at hbody
              rcases bind_eq_ok hbody with ⟨⟨_, _⟩, _, hbody⟩
              rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
              cases ok_inj hbody
          · split at hbody
            · rcases bind_eq_ok hbody with ⟨nc_p, hap, hbody⟩
              cases ok_inj hbody
              intro e he
              rcases happend_sub nc1 res _ hap e he with he1 | rfl
              · exact Or.inl (hinv e he1)
              · exact Or.inr ⟨rfl, rfl⟩
            · rcases bind_eq_ok hbody with ⟨nc_p, hap, hbody⟩
              cases ok_inj hbody
              intro e he
              rcases happend_sub tc.contexts res _ hap e he with he1 | rfl
              · exact Or.inl he1
              · exact Or.inr ⟨rfl, rfl⟩)
      (false, alloc.vec.Vec.new verifier.TerminatingContext, 0#usize) v_out
      (by intro _ h; cases h) hloop0
  intro entry hentry
  rcases hout_mem entry hentry with hold | ⟨hcc, hdd⟩
  · exact htc entry hold
  · rw [hcc, hdd]
    exact ⟨hctx, F, hF_le, hsem⟩

/-! ## 2. Unified Single-Instruction Forward, Counter-Step Bound, & Backward Dispatch -/

/-- Exact correspondence between raw instruction slot `code[pc]` and decoded `prog[pc]`
    established by `verify_program_core_loop6_zero_spec` at every visited PC. -/
def CodeMatchesProgAt (cctx : verifier.CoreCallingContext)
    (code : List api.EbpfInstruction) (prog : Program) (pc : Nat) : Prop :=
  ∃ raw insn,
    code[pc]? = some raw ∧
    prog[pc]? = some insn ∧
    insn ≠ .lddwCont ∧
    ((raw.bv.setWidth 8 = 0x18#8 →
        ∃ raw2 rlddw,
          code[pc + 1]? = some raw2 ∧
          raw2.bv.setWidth 32 = 0#32 ∧
          verifier.CoreCallingContext.resolve_lddw cctx
            ⟨((raw.bv >>> 12) &&& 0xF#64).setWidth 8⟩
            (UScalar.hcast .I32 (⟨(raw.bv >>> 32).setWidth 32⟩ : Std.U32))
            (UScalar.hcast .I32 (⟨(raw2.bv >>> 32).setWidth 32⟩ : Std.U32)) = .ok (.Ok rlddw) ∧
          insn = lddwInsnOfResolved (((raw.bv >>> 8) &&& 0xF#64).toNat) rlddw) ∧
     (raw.bv.setWidth 8 ≠ 0x18#8 →
        decodeSingleInsn raw = some insn))

/-- Structural upper bound on `VerificationContext.next_id` allocations in `resolve_return_value`. -/
def typeAllocBound : verifier.Type → Nat
  | .NullOrParameter inner => 1 + typeAllocBound inner
  | .ReleasableParameter _ inner => 1 + typeAllocBound inner
  | _ => 1

private theorem ptrReturnTypeMatches_allocBound_one {env : ExecEnv}
    {innerT : verifier.Type} {innerS : HelperPtrReturnKind}
    (h : PtrReturnTypeMatches env innerT innerS) :
    typeAllocBound innerT = 1 := by
  cases innerT <;> cases innerS <;> try exact False.elim h
  all_goals rfl

private theorem returnTypeMatches_allocBound_le_three {env : ExecEnv}
    {retTy : verifier.Type} {retSpec : HelperReturnSpec}
    (h : ReturnTypeMatches env retTy retSpec) :
    typeAllocBound retTy ≤ 3 := by
  cases retTy with
  | NullOrParameter sub =>
    unfold ReturnTypeMatches at h
    cases sub <;> cases retSpec with
    | scalar b => cases b <;> exact False.elim h
    | pointer nullable rel? innerS =>
      cases nullable <;> cases rel?
      · exact False.elim h
      · exact False.elim h
      · rw [typeAllocBound, ptrReturnTypeMatches_allocBound_one h]; omega
      · first
        | exact False.elim h
        | rw [typeAllocBound, typeAllocBound, ptrReturnTypeMatches_allocBound_one h.2.2]
  | ReleasableParameter _ innerT =>
    unfold ReturnTypeMatches at h
    cases retSpec with
    | scalar b => cases b <;> exact False.elim h
    | pointer nullable rel? innerS =>
      cases nullable <;> cases rel?
      · exact False.elim h
      · rw [typeAllocBound, ptrReturnTypeMatches_allocBound_one h.2.2]; omega
      · exact False.elim h
      · exact False.elim h
  | _ => simp [typeAllocBound]

private theorem resolve_return_value_counter_le
    (retTy : verifier.Type) :
    ∀ {ctx next next' : verifier.ComputationContext}
      {vctx vctx' : verifier.VerificationContext} {retVal : verifier.Type},
      verifier.ComputationContext.resolve_return_value ctx vctx retTy next =
        ok (.Ok retVal, vctx', next') →
      vctx'.counter.bv.toNat ≤ vctx.counter.bv.toNat + typeAllocBound retTy := by
  induction retTy with
  | NullOrParameter inner ih =>
    intro ctx next next' vctx vctx' retVal h
    rw [verifier.ComputationContext.resolve_return_value.eq_def] at h
    dsimp only at h
    rcases bind_eq_ok h with ⟨p1, hnid, _⟩
    rcases p1 with ⟨_, vc1⟩
    rw [bind_pair_ok_eq hnid] at h
    obtain ⟨_, _, hcnt1⟩ := VerificationContext_next_id_ok_spec hnid
    rcases bind_eq_ok h with ⟨p2, hrec, _⟩
    rcases p2 with ⟨r, vc2, n1⟩
    rw [bind_triple_ok_eq hrec] at h
    cases r with
    | Err _ => cases (Prod.mk.inj (ok_inj h)).1
    | Ok t1 =>
      rcases Prod.mk.inj (Prod.mk.inj (ok_inj h)).2 with ⟨rfl, _⟩
      have h2 := ih hrec
      simp only [typeAllocBound]; omega
  | ReleasableParameter relId inner ih =>
    intro ctx next next' vctx vctx' retVal h
    rw [verifier.ComputationContext.resolve_return_value.eq_def] at h
    dsimp only at h
    rcases bind_eq_ok h with ⟨p1, hnid, _⟩
    rcases p1 with ⟨mi, vc1⟩
    rw [bind_pair_ok_eq hnid] at h
    obtain ⟨_, _, hcnt1⟩ := VerificationContext_next_id_ok_spec hnid
    rw [bind_ok_eq (MemoryId_clone_spec relId),
        bind_ok_eq (MemoryId_prepended_spec mi relId),
        bind_ok_eq (MemoryId_clone_spec (memoryIdPrepend mi relId))] at h
    rcases bind_eq_ok h with ⟨p_ins, hins, _⟩
    rcases p_ins with ⟨_, _⟩
    rw [bind_pair_ok_eq hins] at h
    rcases bind_eq_ok h with ⟨p2, hrec, _⟩
    rcases p2 with ⟨r, vc2, n1⟩
    rw [bind_triple_ok_eq hrec] at h
    cases r with
    | Err _ => cases (Prod.mk.inj (ok_inj h)).1
    | Ok t1 =>
      rcases Prod.mk.inj (Prod.mk.inj (ok_inj h)).2 with ⟨rfl, _⟩
      have h2 := ih hrec
      simp only [typeAllocBound]; omega
  | PtrToMemory id off sz orig =>
    intro ctx next next' vctx vctx' retVal h
    rw [verifier.ComputationContext.resolve_return_value.eq_def] at h
    dsimp only at h
    rcases bind_eq_ok h with ⟨p1, hnid, _⟩
    rcases p1 with ⟨mi, vc1⟩
    rw [bind_pair_ok_eq hnid] at h
    obtain ⟨_, _, hcnt1⟩ := VerificationContext_next_id_ok_spec hnid
    rw [bind_ok_eq (MemoryId_prepended_spec id mi)] at h
    rcases Prod.mk.inj (Prod.mk.inj (ok_inj h)).2 with ⟨rfl, _⟩
    simp only [typeAllocBound]; omega
  | PtrToStruct id off sd =>
    intro ctx next next' vctx vctx' retVal h
    rw [verifier.ComputationContext.resolve_return_value.eq_def] at h
    dsimp only at h
    rcases bind_eq_ok h with ⟨p1, hnid, _⟩
    rcases p1 with ⟨mi, vc1⟩
    rw [bind_pair_ok_eq hnid] at h
    obtain ⟨_, _, hcnt1⟩ := VerificationContext_next_id_ok_spec hnid
    rw [bind_ok_eq (MemoryId_prepended_spec id mi),
        bind_ok_eq (StructDescriptor_clone_spec sd)] at h
    rcases Prod.mk.inj (Prod.mk.inj (ok_inj h)).2 with ⟨rfl, _⟩
    simp only [typeAllocBound]; omega
  | MapValueParameter mapIdx =>
    intro ctx next next' vctx vctx' retVal h
    rw [verifier.ComputationContext.resolve_return_value.eq_def] at h
    dsimp only at h
    rcases bind_eq_ok h with ⟨r_ms, _, h⟩
    cases r_ms with
    | Err _ => cases (Prod.mk.inj (ok_inj h)).1
    | Ok s =>
      dsimp only at h
      rcases bind_eq_ok h with ⟨p1, hnid, _⟩
      rcases p1 with ⟨_, vc1⟩
      rw [bind_pair_ok_eq hnid] at h
      obtain ⟨_, _, hcnt1⟩ := VerificationContext_next_id_ok_spec hnid
      obtain ⟨mf, b_ro, hmf, hcont, _⟩ := MapFlags_ProgReadOnly_contains_spec s
      rw [bind_ok_eq hmf, bind_ok_eq hcont] at h
      rcases bind_eq_ok h with ⟨_, _, h⟩
      simp only [from_known_64_eq, lift_eq_ok, bind_ok] at h
      rcases Prod.mk.inj (Prod.mk.inj (ok_inj h)).2 with ⟨rfl, _⟩
      simp only [typeAllocBound]; omega
  | MemoryParameter mps _ _ =>
    intro ctx next next' vctx vctx' retVal h
    rw [verifier.ComputationContext.resolve_return_value.eq_def] at h
    dsimp only at h
    rcases bind_eq_ok h with ⟨r_sz, _, h⟩
    cases r_sz with
    | Err _ => cases (Prod.mk.inj (ok_inj h)).1
    | Ok sz =>
      dsimp only at h
      rcases bind_eq_ok h with ⟨p1, hnid, _⟩
      rcases p1 with ⟨_, vc1⟩
      rw [bind_pair_ok_eq hnid] at h
      obtain ⟨_, _, hcnt1⟩ := VerificationContext_next_id_ok_spec hnid
      simp only [from_known_64_eq, bind_ok] at h
      rcases Prod.mk.inj (Prod.mk.inj (ok_inj h)).2 with ⟨rfl, _⟩
      simp only [typeAllocBound]; omega
  | _ =>
    intro ctx next next' vctx vctx' retVal h
    rw [verifier.ComputationContext.resolve_return_value.eq_def] at h
    simp only [Type_clone_spec, bind_ok] at h
    rcases Prod.mk.inj (Prod.mk.inj (ok_inj h)).2 with ⟨rfl, _⟩
    simp only [typeAllocBound]; omega

private theorem alu_counter_eq
    {ctx : verifier.ComputationContext}
    {vctx0 vctx1 : verifier.VerificationContext}
    {log_op : Option verifier.AluLogOp} {dst : Std.U8} {src : visitor.Source}
    {alu_type : verifier.AluType} {alu_op : verifier.ScalarAluOp}
    (hok : verifier.ComputationContext.alu ctx log_op vctx0 dst src alu_type alu_op =
      ok (.Ok (), vctx1)) :
    vctx1.counter = vctx0.counter := by
  unfold verifier.ComputationContext.alu at hok
  rcases bind_eq_ok hok with ⟨p_log, hlog, _⟩
  rcases p_log with ⟨_, vc_l⟩
  rw [bind_pair_ok_eq hlog] at hok
  have hrest := hok
  have hl_cnt : vc_l.counter = vctx0.counter := by
    cases log_op with
    | none =>
      rcases Prod.mk.inj (ok_inj hlog) with ⟨_, rfl⟩
      rfl
    | some op =>
      dsimp only at hlog
      rcases bind_eq_ok hlog with ⟨vc2, hemit, hlog⟩
      rcases Prod.mk.inj (ok_inj hlog) with ⟨_, rfl⟩
      exact (VerificationContext_emit_log_ok_spec hemit).2.2.1
  rcases bind_eq_ok hrest with ⟨r, _, hrest⟩
  cases r with
  | Err _ => cases (Prod.mk.inj (ok_inj hrest)).1
  | Ok _ =>
    dsimp only at hrest
    rcases bind_eq_ok hrest with ⟨r1, _, hrest⟩
    cases r1 with
    | Err _ => cases (Prod.mk.inj (ok_inj hrest)).1
    | Ok _ =>
      dsimp only at hrest
      rcases bind_eq_ok hrest with ⟨r2, _, hrest⟩
      cases r2 with
      | Err _ => cases (Prod.mk.inj (ok_inj hrest)).1
      | Ok _ =>
        dsimp only at hrest
        rcases bind_eq_ok hrest with ⟨r3, _, hrest⟩
        cases r3 with
        | Err _ => cases (Prod.mk.inj (ok_inj hrest)).1
        | Ok _ =>
          dsimp only at hrest
          rcases bind_eq_ok hrest with ⟨⟨r4, _⟩, _, hrest⟩
          cases r4 with
          | Err _ => cases (Prod.mk.inj (ok_inj hrest)).1
          | Ok _ =>
            rcases bind_eq_ok hrest with ⟨vc2, hpush, hrest⟩
            rcases Prod.mk.inj (ok_inj hrest) with ⟨_, rfl⟩
            exact (VerificationContext_push_successor_ok_spec hpush).2.2.1.trans hl_cnt

private theorem load_memory_counter_eq
    {self_m : verifier.ComputationContext} {vc_a vc_b : verifier.VerificationContext}
    {addr_m res_m : verifier.Type} {fld_m : verifier.Field}
    (hlm : verifier.ComputationContext.load_memory self_m vc_a addr_m fld_m = ok (.Ok res_m, vc_b)) :
    vc_b.counter = vc_a.counter := by
  unfold verifier.ComputationContext.load_memory at hlm
  rcases bind_eq_ok hlm with ⟨r_in, _, hlm⟩
  cases r_in with
  | Err _ => cases (Prod.mk.inj (ok_inj hlm)).1
  | Ok a =>
    dsimp only at hlm
    cases a <;> try (cases (Prod.mk.inj (ok_inj hlm)).1)
    · rcases bind_eq_ok hlm with ⟨_, _, hlm⟩
      rcases bind_eq_ok hlm with ⟨r1, _, hlm⟩
      cases r1 with
      | Err _ => cases (Prod.mk.inj (ok_inj hlm)).1
      | Ok _ =>
        rcases bind_eq_ok hlm with ⟨⟨r2, _⟩, hreg_ma, hlm⟩
        cases r2 with
        | Err _ => cases (Prod.mk.inj (ok_inj hlm)).1
        | Ok _ =>
          rcases Prod.mk.inj (ok_inj hlm) with ⟨_, rfl⟩
          exact (VerificationContext_register_memory_access_ok_spec hreg_ma).2.2.1
    · rcases bind_eq_ok hlm with ⟨_, _, hlm⟩
      rcases bind_eq_ok hlm with ⟨r1, _, hlm⟩
      cases r1 with
      | Err _ => cases (Prod.mk.inj (ok_inj hlm)).1
      | Ok _ =>
        dsimp only at hlm
        rw [bind_ok_eq (Type_unknown_written_scalar_eq fld_m.width)] at hlm
        rcases bind_eq_ok hlm with ⟨⟨r2, _⟩, hreg_ma, hlm⟩
        cases r2 with
        | Err _ => cases (Prod.mk.inj (ok_inj hlm)).1
        | Ok _ =>
          rcases Prod.mk.inj (ok_inj hlm) with ⟨_, rfl⟩
          exact (VerificationContext_register_memory_access_ok_spec hreg_ma).2.2.1
    · rcases bind_eq_ok hlm with ⟨o_f, _, hlm⟩
      cases o_f with
      | none => cases (Prod.mk.inj (ok_inj hlm)).1
      | some _ =>
        dsimp only at hlm
        rcases bind_eq_ok hlm with ⟨_, _, hlm⟩
        rcases bind_eq_ok hlm with ⟨_, _, hlm⟩
        rcases bind_eq_ok hlm with ⟨⟨r1, _⟩, hreg_ma, hlm⟩
        cases r1 with
        | Err _ => cases (Prod.mk.inj (ok_inj hlm)).1
        | Ok _ =>
          rcases Prod.mk.inj (ok_inj hlm) with ⟨_, rfl⟩
          exact (VerificationContext_register_memory_access_ok_spec hreg_ma).2.2.1
    · rcases bind_eq_ok hlm with ⟨_, _, hlm⟩
      rcases bind_eq_ok hlm with ⟨_, _, hlm⟩
      rcases bind_eq_ok hlm with ⟨r1, _, hlm⟩
      cases r1 with
      | Err _ => cases (Prod.mk.inj (ok_inj hlm)).1
      | Ok _ =>
        dsimp only at hlm
        rw [bind_ok_eq (Type_unknown_written_scalar_eq fld_m.width)] at hlm
        rcases bind_eq_ok hlm with ⟨⟨r2, _⟩, hreg_ma, hlm⟩
        cases r2 with
        | Err _ => cases (Prod.mk.inj (ok_inj hlm)).1
        | Ok _ =>
          rcases Prod.mk.inj (ok_inj hlm) with ⟨_, rfl⟩
          exact (VerificationContext_register_memory_access_ok_spec hreg_ma).2.2.1

private theorem store_memory_counter_eq
    {self_m n_m : verifier.ComputationContext}
    {vc_a vc_b : verifier.VerificationContext} {pc_m : Std.Usize}
    {addr_m val_m : verifier.Type} {fld_m : verifier.Field}
    (hsm : verifier.ComputationContext.store_memory self_m vc_a pc_m addr_m fld_m val_m =
      ok (.Ok (), n_m, vc_b)) :
    vc_b.counter = vc_a.counter := by
  unfold verifier.ComputationContext.store_memory at hsm
  rcases bind_eq_ok hsm with ⟨r_in, _, hsm⟩
  cases r_in with
  | Err _ => cases (Prod.mk.inj (ok_inj hsm)).1
  | Ok _ =>
    dsimp only at hsm
    rw [bind_ok_eq (Type_unwrap_releasable_spec addr_m)] at hsm
    rcases bind_eq_ok hsm with ⟨is_init, _, hsm⟩
    cases hu : pureUnwrapReleasable addr_m <;> rw [hu] at hsm
    all_goals try (cases (Prod.mk.inj (ok_inj hsm)).1)
    · dsimp only at hsm
      rcases bind_eq_ok hsm with ⟨_, _, hsm⟩
      rcases bind_eq_ok hsm with ⟨⟨r1, _⟩, _, hsm⟩
      cases r1 with
      | Err _ => cases (Prod.mk.inj (ok_inj hsm)).1
      | Ok _ =>
        rcases bind_eq_ok hsm with ⟨⟨r2, _⟩, hreg_ma, hsm⟩
        rcases Prod.mk.inj (ok_inj hsm) with ⟨rfl, hp⟩
        rcases Prod.mk.inj hp with ⟨_, rfl⟩
        exact (VerificationContext_register_memory_access_ok_spec hreg_ma).2.2.1
    · rename_i _ _ _ orig_m
      dsimp only at hsm
      rw [bind_ok_eq (PointerOrigin_is_writable_spec orig_m)] at hsm
      cases hwr : (MemoryOrigin.ofCore orig_m).isWritable <;>
        simp only [hwr, Bool.false_eq_true, ↓reduceIte] at hsm
      · cases (Prod.mk.inj (ok_inj hsm)).1
      · rcases bind_eq_ok hsm with ⟨_, _, hsm⟩
        rcases bind_eq_ok hsm with ⟨r1, _, hsm⟩
        cases r1 with
        | Err _ => cases (Prod.mk.inj (ok_inj hsm)).1
        | Ok _ =>
          cases is_init <;> simp only [Bool.false_eq_true, ↓reduceIte] at hsm
          · cases (Prod.mk.inj (ok_inj hsm)).1
          · rcases bind_eq_ok hsm with ⟨⟨r2, _⟩, hreg_ma, hsm⟩
            rcases Prod.mk.inj (ok_inj hsm) with ⟨rfl, hp⟩
            rcases Prod.mk.inj hp with ⟨_, rfl⟩
            exact (VerificationContext_register_memory_access_ok_spec hreg_ma).2.2.1
    · rename_i id_m _ _
      dsimp only at hsm
      rcases bind_eq_ok hsm with ⟨o_f, _, hsm⟩
      cases o_f with
      | none => cases (Prod.mk.inj (ok_inj hsm)).1
      | some _ =>
        dsimp only at hsm
        rcases bind_eq_ok hsm with ⟨b_mut, _, hsm⟩
        cases b_mut <;> simp only [Bool.false_eq_true, ↓reduceIte] at hsm
        · cases (Prod.mk.inj (ok_inj hsm)).1
        · cases is_init <;>
            simp only [Bool.false_eq_true, ↓reduceIte, bind_ok_eq (MemoryId_clone_spec id_m)] at hsm
          · cases (Prod.mk.inj (ok_inj hsm)).1
          · rcases bind_eq_ok hsm with ⟨⟨r1, _⟩, hreg_ma, hsm⟩
            rcases Prod.mk.inj (ok_inj hsm) with ⟨rfl, hp⟩
            rcases Prod.mk.inj hp with ⟨_, rfl⟩
            exact (VerificationContext_register_memory_access_ok_spec hreg_ma).2.2.1
    · dsimp only at hsm
      rcases bind_eq_ok hsm with ⟨_, _, hsm⟩
      rcases bind_eq_ok hsm with ⟨_, _, hsm⟩
      rcases bind_eq_ok hsm with ⟨r1, _, hsm⟩
      cases r1 with
      | Err _ => cases (Prod.mk.inj (ok_inj hsm)).1
      | Ok _ =>
        cases is_init <;> simp only [Bool.false_eq_true, ↓reduceIte] at hsm
        · cases (Prod.mk.inj (ok_inj hsm)).1
        · rcases bind_eq_ok hsm with ⟨⟨r2, _⟩, hreg_ma, hsm⟩
          rcases Prod.mk.inj (ok_inj hsm) with ⟨rfl, hp⟩
          rcases Prod.mk.inj hp with ⟨_, rfl⟩
          exact (VerificationContext_register_memory_access_ok_spec hreg_ma).2.2.1

/-- Unified forward soundness for `BpfVisitor.visit.default` on `ComputationContext`,
    establishing `ctx_out = ctx`, `ForwardVisitPost`, `vctx1.counter ≤ vctx0.counter + 3`,
    and the raw-opcode validity predicate required by `verify_program_core_loop6_zero_spec`. -/
theorem visit_any_forward
    {env : ExecEnv} {ctx ctx_out : verifier.ComputationContext}
    {vctx0 vctx1 : verifier.VerificationContext}
    {inst : api.EbpfInstruction}
    (hcctx : CallingContextFullMatches vctx0.calling_context env)
    (hctx : CompCtxValid env vctx0.counter.bv.toNat ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hvisit : visitor.BpfVisitor.visit.default CompCtxVisitorInst ctx vctx0 inst =
      .ok (.Ok (), ctx_out, vctx1)) :
    ctx_out = ctx ∧
    ForwardVisitPost env vctx0 vctx1 ctx ∧
    vctx1.counter.bv.toNat ≤ vctx0.counter.bv.toNat + 3 ∧
    inst.bv.setWidth 8 ≠ 0#8 ∧
    (inst.bv.setWidth 8 ≠ 0x18#8 →
      ∃ insn, insn ≠ .lddwCont ∧ decodeSingleInsn inst = some insn) := by
  obtain ⟨hnez, hcases⟩ := visit_default_ok_spec CompCtxVisitorInst ctx ctx_out vctx0 vctx1 inst hvisit
  rcases hcases with ⟨h18, hload64⟩ | ⟨hne18, insn, hdec, hdisp⟩
  · obtain ⟨hctx_eq, hpost⟩ := visit_load64_forward hcctx.1 hctx hpending hload64
    subst ctx_out
    refine ⟨rfl, hpost, ?_, hnez, fun hc => False.elim (hc h18)⟩
    -- Bound `counter` increase in `load64` by `1 ≤ 3`:
    dsimp [CompCtxVisitorInst] at hload64
    unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.load64 at hload64
    simp only [lift_eq_ok, bind_ok] at hload64
    split at hload64
    · cases (Prod.mk.inj (ok_inj hload64)).1
    · split at hload64
      · cases (Prod.mk.inj (ok_inj hload64)).1
      · rcases bind_eq_ok hload64 with ⟨ei, _, hload64⟩
        rw [bind_ok_eq (ebpf_imm_eq_ok ei)] at hload64
        rcases bind_eq_ok hload64 with ⟨r_res, _, hload64⟩
        cases r_res with
        | Err _ => cases (Prod.mk.inj (ok_inj hload64)).1
        | Ok r1 =>
          dsimp only at hload64
          rcases bind_eq_ok hload64 with ⟨pcv, hcv, _⟩
          rcases pcv with ⟨context1, _⟩
          rw [bind_pair_ok_eq hcv] at hload64
          have hcnt1 : context1.counter.bv.toNat ≤ vctx0.counter.bv.toNat + 1 := by
            cases r1 with
            | Imm _ =>
              dsimp only at hcv
              rcases bind_eq_ok hcv with ⟨c2, hemit, hcv⟩
              rw [bind_ok_eq (from_known_64_eq _)] at hcv
              rcases Prod.mk.inj (ok_inj hcv) with ⟨rfl, _⟩
              rw [(VerificationContext_emit_log_ok_spec hemit).2.2.1]; omega
            | MapPtr _ _ =>
              dsimp only at hcv
              rcases bind_eq_ok hcv with ⟨c2, hemit, hcv⟩
              rcases Prod.mk.inj (ok_inj hcv) with ⟨rfl, _⟩
              rw [(VerificationContext_emit_log_ok_spec hemit).2.2.1]; omega
            | MapValuePtr _ schema _ =>
              dsimp only at hcv
              rcases bind_eq_ok hcv with ⟨c2, hemit, hcv⟩
              obtain ⟨_, _, he3, _, _, _⟩ := VerificationContext_emit_log_ok_spec hemit
              rcases bind_eq_ok hcv with ⟨pnid, hnid, _⟩
              rcases pnid with ⟨_, c3⟩
              rw [bind_pair_ok_eq hnid] at hcv
              obtain ⟨_, _, hc3⟩ := VerificationContext_next_id_ok_spec hnid
              obtain ⟨mf, b_ro, hmf, hcont, _⟩ := MapFlags_ProgReadOnly_contains_spec schema
              rw [bind_ok_eq hmf, bind_ok_eq hcont] at hcv
              rcases bind_eq_ok hcv with ⟨_, _, hcv⟩
              simp only [from_known_64_eq, bind_ok] at hcv
              rcases Prod.mk.inj (ok_inj hcv) with ⟨rfl, _⟩
              rw [he3] at hc3; omega
          rcases bind_eq_ok hload64 with ⟨r2, _, hload64⟩
          cases r2 with
          | Err _ => cases (Prod.mk.inj (ok_inj hload64)).1
          | Ok _ =>
            dsimp only at hload64
            rcases bind_eq_ok hload64 with ⟨psr, hsr, _⟩
            rcases psr with ⟨r3, _⟩
            rw [bind_pair_ok_eq hsr] at hload64
            cases r3 with
            | Err _ => cases (Prod.mk.inj (ok_inj hload64)).1
            | Ok _ =>
              rcases bind_eq_ok hload64 with ⟨_, hpush, hload64⟩
              rcases Prod.mk.inj (Prod.mk.inj (ok_inj hload64)).2 with ⟨_, rfl⟩
              rw [(VerificationContext_push_successor_ok_spec hpush).2.2.1]; omega
  · have hnotCont : insn ≠ .lddwCont := VisitorDispatchNonLddw.not_lddwCont hdisp
    have hdec_wit : inst.bv.setWidth 8 ≠ 0x18#8 →
        ∃ i0, i0 ≠ .lddwCont ∧ decodeSingleInsn inst = some i0 :=
      fun _ => ⟨insn, hnotCont, hdec⟩
    cases insn with
    | alu64 op dst src =>
      obtain ⟨rfl, hpost⟩ := visit_alu64_forward hctx hpending hdisp
      refine ⟨rfl, hpost, ?_, hnez, hdec_wit⟩
      rcases hdisp with ⟨dst8, vsrc, _, _, _, hcall⟩
      have hc := hcall.symm
      cases op with
      | mov =>
        dsimp [alu64Method, CompCtxVisitorInst] at hc
        unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mov64 at hc
        rcases bind_eq_ok hc with ⟨vc1, hemit, hc⟩
        rcases bind_eq_ok hc with ⟨r, _, hc⟩
        cases r with
        | Err _ => cases (Prod.mk.inj (ok_inj hc)).1
        | Ok _ =>
          dsimp only at hc
          rcases bind_eq_ok hc with ⟨r1, _, hc⟩
          cases r1 with
          | Err _ => cases (Prod.mk.inj (ok_inj hc)).1
          | Ok _ =>
            dsimp only at hc
            rcases bind_eq_ok hc with ⟨⟨r2, _⟩, _, hc⟩
            cases r2 with
            | Err _ => cases (Prod.mk.inj (ok_inj hc)).1
            | Ok _ =>
              rcases bind_eq_ok hc with ⟨vc2, hpush, hc⟩
              rcases Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2 with ⟨_, rfl⟩
              rw [(VerificationContext_push_successor_ok_spec hpush).2.2.1,
                  (VerificationContext_emit_log_ok_spec hemit).2.2.1]; omega
      | _ =>
        dsimp [alu64Method, CompCtxVisitorInst] at hc
        first
        | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.add64 at hc
        | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.sub64 at hc
        | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mul64 at hc
        | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.div64 at hc
        | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mod64 at hc
        | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.and64 at hc
        | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.or64 at hc
        | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.xor64 at hc
        | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.lsh64 at hc
        | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.rsh64 at hc
        | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.arsh64 at hc
        rcases bind_eq_ok hc with ⟨⟨r0, _⟩, halu, hc⟩
        rcases Prod.mk.inj (ok_inj hc) with ⟨rfl, hp⟩
        rcases Prod.mk.inj hp with ⟨_, rfl⟩
        rw [alu_counter_eq halu]; omega
    | alu32 op dst src =>
      obtain ⟨rfl, hpost⟩ := visit_alu32_forward hctx hpending hdisp
      refine ⟨rfl, hpost, ?_, hnez, hdec_wit⟩
      rcases hdisp with ⟨dst8, vsrc, _, _, _, hcall⟩
      have hc := hcall.symm
      cases op with
      | mov =>
        dsimp [alu32Method, CompCtxVisitorInst] at hc
        unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mov at hc
        rcases bind_eq_ok hc with ⟨vc1, hemit, hc⟩
        rcases bind_eq_ok hc with ⟨r, _, hc⟩
        cases r with
        | Err _ => cases (Prod.mk.inj (ok_inj hc)).1
        | Ok _ =>
          dsimp only at hc
          rcases bind_eq_ok hc with ⟨_, _, hc⟩
          rcases bind_eq_ok hc with ⟨r1, _, hc⟩
          cases r1 with
          | Err _ => cases (Prod.mk.inj (ok_inj hc)).1
          | Ok _ =>
            dsimp only at hc
            rcases bind_eq_ok hc with ⟨⟨r2, _⟩, _, hc⟩
            cases r2 with
            | Err _ => cases (Prod.mk.inj (ok_inj hc)).1
            | Ok _ =>
              rcases bind_eq_ok hc with ⟨vc2, hpush, hc⟩
              rcases Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2 with ⟨_, rfl⟩
              rw [(VerificationContext_push_successor_ok_spec hpush).2.2.1,
                  (VerificationContext_emit_log_ok_spec hemit).2.2.1]; omega
      | _ =>
        dsimp [alu32Method, CompCtxVisitorInst] at hc
        first
        | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.add at hc
        | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.sub at hc
        | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mul at hc
        | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.div at hc
        | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mod at hc
        | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.and at hc
        | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.or at hc
        | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.xor at hc
        | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.lsh at hc
        | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.rsh at hc
        | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.arsh at hc
        rcases bind_eq_ok hc with ⟨⟨r0, _⟩, halu, hc⟩
        rcases Prod.mk.inj (ok_inj hc) with ⟨rfl, hp⟩
        rcases Prod.mk.inj hp with ⟨_, rfl⟩
        rw [alu_counter_eq halu]; omega
    | neg is64 dst =>
      obtain ⟨rfl, hpost⟩ := visit_neg_forward hctx hpending hdisp
      refine ⟨rfl, hpost, ?_, hnez, hdec_wit⟩
      rcases hdisp with ⟨dst8, _, hcall⟩
      have hc := hcall.symm
      cases is64 <;> {
        dsimp [CompCtxVisitorInst] at hc
        try unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.neg64 at hc
        try unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.neg at hc
        rcases bind_eq_ok hc with ⟨vc1, hemit, hc⟩
        rcases bind_eq_ok hc with ⟨⟨r0, _⟩, halu, hc⟩
        rcases Prod.mk.inj (ok_inj hc) with ⟨rfl, hp⟩
        rcases Prod.mk.inj hp with ⟨_, rfl⟩
        rw [alu_counter_eq halu, (VerificationContext_emit_log_ok_spec hemit).2.2.1]; omega
      }
    | endian doSwap ew dst =>
      obtain ⟨hctx_eq, hpost⟩ := visit_endian_forward hctx hpending hdisp
      subst ctx_out
      refine ⟨rfl, hpost, ?_, hnez, hdec_wit⟩
      rcases hdisp with ⟨dst8, _, hcall⟩
      have hc := hcall.symm
      have hend : verifier.ComputationContext.endianness ctx doSwap vctx0 dst8 (coreWidthOfEndian ew) =
          ok (.Ok (), vctx1) := by
        cases doSwap <;> {
          dsimp [CompCtxVisitorInst] at hc
          try unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.le at hc
          try unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.be at hc
          rcases bind_eq_ok hc with ⟨⟨r0, _⟩, he, hc⟩
          rcases Prod.mk.inj (ok_inj hc) with ⟨rfl, hp⟩
          rcases Prod.mk.inj hp with ⟨_, rfl⟩
          exact he
        }
      unfold verifier.ComputationContext.endianness at hend
      rcases bind_eq_ok hend with ⟨vc1, hemit, hend⟩
      rcases bind_eq_ok hend with ⟨r, _, hend⟩
      cases r with
      | Err _ => cases (Prod.mk.inj (ok_inj hend)).1
      | Ok _ =>
        dsimp only at hend
        rcases bind_eq_ok hend with ⟨r1, _, hend⟩
        cases r1 with
        | Err _ => cases (Prod.mk.inj (ok_inj hend)).1
        | Ok v1 =>
          dsimp only at hend
          have he3 := (VerificationContext_emit_log_ok_spec hemit).2.2.1
          cases v1 with
          | ScalarValue data =>
            dsimp only at hend
            rcases bind_eq_ok hend with ⟨b, _, hend⟩
            cases b with
            | false => cases (Prod.mk.inj (ok_inj hend)).1
            | true =>
              simp only [↓reduceIte] at hend
              rcases bind_eq_ok hend with ⟨_, _, hend⟩
              rcases bind_eq_ok hend with ⟨_, _, hend⟩
              rcases bind_eq_ok hend with ⟨_, _, hend⟩
              rcases bind_eq_ok hend with ⟨_, _, hend⟩
              rcases bind_eq_ok hend with ⟨r2, _, hend⟩
              cases r2 with
              | Err _ => cases (Prod.mk.inj (ok_inj hend)).1
              | Ok _ =>
                dsimp only at hend
                rcases bind_eq_ok hend with ⟨p3, hsr, _⟩
                rcases p3 with ⟨r3, _⟩
                rw [bind_pair_ok_eq hsr] at hend
                cases r3 with
                | Err _ => cases (Prod.mk.inj (ok_inj hend)).1
                | Ok _ =>
                  rcases bind_eq_ok hend with ⟨_, hpush, hend⟩
                  rcases Prod.mk.inj (ok_inj hend) with ⟨_, rfl⟩
                  rw [(VerificationContext_push_successor_ok_spec hpush).2.2.1, he3]; omega
          | _ =>
            dsimp only at hend
            by_cases hleak : vc1.calling_context.allow_ptr_leaks = true
            · simp only [hleak, ↓reduceIte] at hend
              rcases bind_eq_ok hend with ⟨_, _, hend⟩
              rcases bind_eq_ok hend with ⟨r2, _, hend⟩
              cases r2 with
              | Err _ => cases (Prod.mk.inj (ok_inj hend)).1
              | Ok _ =>
                dsimp only at hend
                rcases bind_eq_ok hend with ⟨p3, hsr, _⟩
                rcases p3 with ⟨r3, _⟩
                rw [bind_pair_ok_eq hsr] at hend
                cases r3 with
                | Err _ => cases (Prod.mk.inj (ok_inj hend)).1
                | Ok _ =>
                  rcases bind_eq_ok hend with ⟨_, hpush, hend⟩
                  rcases Prod.mk.inj (ok_inj hend) with ⟨_, rfl⟩
                  rw [(VerificationContext_push_successor_ok_spec hpush).2.2.1, he3]; omega
            · simp only [hleak, Bool.false_eq_true, ↓reduceIte] at hend
              cases (Prod.mk.inj (ok_inj hend)).1
    | jump offInt =>
      obtain ⟨rfl, hpost⟩ := visit_jump_forward hctx hpending hdisp
      refine ⟨rfl, hpost, ?_, hnez, hdec_wit⟩
      rcases hdisp with ⟨_, _, hcall⟩
      have hc := hcall.symm
      dsimp [CompCtxVisitorInst] at hc
      unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.jump at hc
      rcases bind_eq_ok hc with ⟨vc1, hemit, hc⟩
      rcases bind_eq_ok hc with ⟨r, _, hc⟩
      cases r with
      | Err _ => cases (Prod.mk.inj (ok_inj hc)).1
      | Ok _ =>
        dsimp only at hc
        rcases bind_eq_ok hc with ⟨_, hpush, hc⟩
        rcases Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2 with ⟨_, rfl⟩
        rw [(VerificationContext_push_successor_ok_spec hpush).2.2.1,
            (VerificationContext_emit_log_ok_spec hemit).2.2.1]; omega
    | exit =>
      obtain ⟨rfl, hpost⟩ := visit_exit_forward hctx hpending hdisp
      refine ⟨rfl, hpost, ?_, hnez, hdec_wit⟩
      have hc := hdisp.symm
      dsimp [VisitorDispatchNonLddw, CompCtxVisitorInst] at hc
      unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.exit at hc
      rcases bind_eq_ok hc with ⟨vc1, hemit, hc⟩
      rcases bind_eq_ok hc with ⟨r, _, hc⟩
      cases r with
      | Err _ => cases (Prod.mk.inj (ok_inj hc)).1
      | Ok _ =>
        dsimp only at hc
        rcases bind_eq_ok hc with ⟨b, _, hc⟩
        cases b with
        | false => cases (Prod.mk.inj (ok_inj hc)).1
        | true =>
          simp only [↓reduceIte] at hc
          rcases bind_eq_ok hc with ⟨i, _, hc⟩
          split at hc
          · cases (Prod.mk.inj (ok_inj hc)).1
          · rcases Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2 with ⟨_, rfl⟩
            rw [(VerificationContext_emit_log_ok_spec hemit).2.2.1]; omega
    | condJump is64 op dst src offInt =>
      obtain ⟨hctx_eq, hpost⟩ := visit_condJump_forward hctx hpending hdisp
      subst ctx_out
      refine ⟨rfl, hpost, ?_, hnez, hdec_wit⟩
      rcases hdisp with ⟨dst8, vsrc, off, _, _, _, hcall⟩
      have hc := hcall.symm
      rw [condJumpMethod_compCtx_eq] at hc
      rcases bind_eq_ok hc with ⟨⟨res_out, _⟩, hcj, hc⟩
      rcases Prod.mk.inj (ok_inj hc) with ⟨rfl, hp⟩
      rcases Prod.mk.inj hp with ⟨_, rfl⟩
      unfold verifier.ComputationContext.conditional_jump at hcj
      rcases bind_eq_ok hcj with ⟨vc1, hemit, hcj⟩
      have he3 := (VerificationContext_emit_log_ok_spec hemit).2.2.1
      rcases bind_eq_ok hcj with ⟨r, _, hcj⟩
      cases r with
      | Err _ => cases (Prod.mk.inj (ok_inj hcj)).1
      | Ok _ =>
        dsimp only at hcj
        rcases bind_eq_ok hcj with ⟨r1, _, hcj⟩
        cases r1 with
        | Err _ => cases (Prod.mk.inj (ok_inj hcj)).1
        | Ok _ =>
          dsimp only at hcj
          rcases bind_eq_ok hcj with ⟨r2, _, hcj⟩
          cases r2 with
          | Err _ => cases (Prod.mk.inj (ok_inj hcj)).1
          | Ok opt_br =>
            dsimp only at hcj
            cases opt_br with
            | none =>
              dsimp only at hcj
              simp only [bind_ok, ↓reduceIte] at hcj
              rcases bind_eq_ok hcj with ⟨r3, _, hcj⟩
              cases r3 with
              | Err _ => cases (Prod.mk.inj (ok_inj hcj)).1
              | Ok _ =>
                dsimp only at hcj
                rcases bind_eq_ok hcj with ⟨_, _, hcj⟩
                rcases bind_eq_ok hcj with ⟨_, _, hcj⟩
                rcases bind_eq_ok hcj with ⟨r4, _, hcj⟩
                cases r4 with
                | Err _ => cases (Prod.mk.inj (ok_inj hcj)).1
                | Ok _ =>
                  dsimp only at hcj
                  rcases bind_eq_ok hcj with ⟨vc2, hpush1, hcj⟩
                  have hp1 := (VerificationContext_push_successor_ok_spec hpush1).2.2.1
                  rcases bind_eq_ok hcj with ⟨r5, _, hcj⟩
                  cases r5 with
                  | Err _ => cases (Prod.mk.inj (ok_inj hcj)).1
                  | Ok _ =>
                    dsimp only at hcj
                    rcases bind_eq_ok hcj with ⟨_, _, hcj⟩
                    rcases bind_eq_ok hcj with ⟨_, _, hcj⟩
                    rcases bind_eq_ok hcj with ⟨r6, _, hcj⟩
                    cases r6 with
                    | Err _ => cases (Prod.mk.inj (ok_inj hcj)).1
                    | Ok _ =>
                      dsimp only at hcj
                      rcases bind_eq_ok hcj with ⟨_, hpush2, hcj⟩
                      rcases Prod.mk.inj (ok_inj hcj) with ⟨_, rfl⟩
                      rw [(VerificationContext_push_successor_ok_spec hpush2).2.2.1, hp1, he3]
                      omega
            | some b1 =>
              cases b1
              · -- `b1 = false`: `take_jump = false`, `skip_jump = true`
                dsimp only at hcj
                simp only [bind_ok, Bool.false_eq_true, not_false_eq_true, decide_true, ↓reduceIte] at hcj
                rcases bind_eq_ok hcj with ⟨r3, _, hcj⟩
                cases r3 with
                | Err _ => cases (Prod.mk.inj (ok_inj hcj)).1
                | Ok _ =>
                  dsimp only at hcj
                  rcases bind_eq_ok hcj with ⟨_, _, hcj⟩
                  rcases bind_eq_ok hcj with ⟨_, _, hcj⟩
                  rcases bind_eq_ok hcj with ⟨r4, _, hcj⟩
                  cases r4 with
                  | Err _ => cases (Prod.mk.inj (ok_inj hcj)).1
                  | Ok _ =>
                    dsimp only at hcj
                    rcases bind_eq_ok hcj with ⟨_, hpush, hcj⟩
                    rcases Prod.mk.inj (ok_inj hcj) with ⟨_, rfl⟩
                    rw [(VerificationContext_push_successor_ok_spec hpush).2.2.1, he3]; omega
              · -- `b1 = true`: `take_jump = true`, `skip_jump = false`
                dsimp only at hcj
                simp only [bind_ok, not_true_eq_false, decide_false, Bool.false_eq_true, ↓reduceIte] at hcj
                rcases bind_eq_ok hcj with ⟨r3, _, hcj⟩
                cases r3 with
                | Err _ => cases (Prod.mk.inj (ok_inj hcj)).1
                | Ok _ =>
                  dsimp only at hcj
                  rcases bind_eq_ok hcj with ⟨_, _, hcj⟩
                  rcases bind_eq_ok hcj with ⟨_, _, hcj⟩
                  rcases bind_eq_ok hcj with ⟨r4, _, hcj⟩
                  cases r4 with
                  | Err _ => cases (Prod.mk.inj (ok_inj hcj)).1
                  | Ok _ =>
                    dsimp only at hcj
                    rcases bind_eq_ok hcj with ⟨_, hpush, hcj⟩
                    rcases Prod.mk.inj (ok_inj hcj) with ⟨_, rfl⟩
                    rw [(VerificationContext_push_successor_ok_spec hpush).2.2.1, he3]; omega
    | load w dst base off16 =>
      obtain ⟨hctx_eq, hpost⟩ := visit_load_forward hctx hpending hdisp
      subst ctx_out
      refine ⟨rfl, hpost, ?_, hnez, hdec_wit⟩
      rcases hdisp with ⟨dst8, src8, off, _, _, _, hcall⟩
      have hc := hcall.symm
      dsimp [CompCtxVisitorInst] at hc
      unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.load at hc
      rcases bind_eq_ok hc with ⟨vc1, hemit, hc⟩
      obtain ⟨_, _, he3, _, _, _⟩ := VerificationContext_emit_log_ok_spec hemit
      rcases bind_eq_ok hc with ⟨r, _, hc⟩
      cases r with
      | Err _ => cases (Prod.mk.inj (ok_inj hc)).1
      | Ok _ =>
        dsimp only at hc
        rw [bind_ok_eq (Field_new_spec off (coreWidthOfSpec w))] at hc
        rcases bind_eq_ok hc with ⟨⟨r1, vc2⟩, hlm, hc⟩
        cases r1 with
        | Err _ => cases (Prod.mk.inj (ok_inj hc)).1
        | Ok _ =>
          have hm3 := load_memory_counter_eq hlm
          rcases bind_eq_ok hc with ⟨r2, _, hc⟩
          cases r2 with
          | Err _ => cases (Prod.mk.inj (ok_inj hc)).1
          | Ok _ =>
            dsimp only at hc
            rcases bind_eq_ok hc with ⟨⟨r3, _⟩, _, hc⟩
            cases r3 with
            | Err _ => cases (Prod.mk.inj (ok_inj hc)).1
            | Ok _ =>
              rcases bind_eq_ok hc with ⟨_, hpush, hc⟩
              rcases Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2 with ⟨_, rfl⟩
              rw [(VerificationContext_push_successor_ok_spec hpush).2.2.1, hm3, he3]; omega
    | store w base off16 src =>
      obtain ⟨hctx_eq, hpost⟩ := visit_store_forward hctx hpending hdisp
      subst ctx_out
      refine ⟨rfl, hpost, ?_, hnez, hdec_wit⟩
      rcases hdisp with ⟨dst8, off, vsrc, _, _, _, hcall⟩
      have hc := hcall.symm
      dsimp [CompCtxVisitorInst] at hc
      unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.store at hc
      cases vsrc with
      | Reg _ =>
        dsimp only at hc
        rcases bind_eq_ok hc with ⟨vc1, hemit, hc⟩
        obtain ⟨_, _, he3, _, _, _⟩ := VerificationContext_emit_log_ok_spec hemit
        rcases bind_eq_ok hc with ⟨r, _, hc⟩
        cases r with
        | Err _ => cases (Prod.mk.inj (ok_inj hc)).1
        | Ok _ =>
          dsimp only at hc
          rcases bind_eq_ok hc with ⟨r1, _, hc⟩
          cases r1 with
          | Err _ => cases (Prod.mk.inj (ok_inj hc)).1
          | Ok _ =>
            dsimp only at hc
            rcases bind_eq_ok hc with ⟨r2, _, hc⟩
            cases r2 with
            | Err _ => cases (Prod.mk.inj (ok_inj hc)).1
            | Ok _ =>
              dsimp only at hc
              rw [bind_ok_eq (Field_new_spec off (coreWidthOfSpec w))] at hc
              rcases bind_eq_ok hc with ⟨⟨r3, _, vc2⟩, hsm, hc⟩
              cases r3 with
              | Err _ => cases (Prod.mk.inj (ok_inj hc)).1
              | Ok _ =>
                have hm3 := store_memory_counter_eq hsm
                rcases bind_eq_ok hc with ⟨_, hpush, hc⟩
                rcases Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2 with ⟨_, rfl⟩
                rw [(VerificationContext_push_successor_ok_spec hpush).2.2.1, hm3, he3]; omega
      | Value _ =>
        dsimp only at hc
        rcases bind_eq_ok hc with ⟨vc1, hemit, hc⟩
        obtain ⟨_, _, he3, _, _, _⟩ := VerificationContext_emit_log_ok_spec hemit
        rw [bind_ok_eq (Type_mask_spec (coreWidthOfSpec w))] at hc
        simp only [lift_eq_ok, bind_ok] at hc
        rw [bind_ok_eq (from_known_64_eq _)] at hc
        rcases bind_eq_ok hc with ⟨r1, _, hc⟩
        cases r1 with
        | Err _ => cases (Prod.mk.inj (ok_inj hc)).1
        | Ok _ =>
          dsimp only at hc
          rcases bind_eq_ok hc with ⟨r2, _, hc⟩
          cases r2 with
          | Err _ => cases (Prod.mk.inj (ok_inj hc)).1
          | Ok _ =>
            dsimp only at hc
            rw [bind_ok_eq (Field_new_spec off (coreWidthOfSpec w))] at hc
            rcases bind_eq_ok hc with ⟨⟨r3, _, vc2⟩, hsm, hc⟩
            cases r3 with
            | Err _ => cases (Prod.mk.inj (ok_inj hc)).1
            | Ok _ =>
              have hm3 := store_memory_counter_eq hsm
              rcases bind_eq_ok hc with ⟨_, hpush, hc⟩
              rcases Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2 with ⟨_, rfl⟩
              rw [(VerificationContext_push_successor_ok_spec hpush).2.2.1, hm3, he3]; omega
    | atomic is64 op base off16 src =>
      obtain ⟨hctx_eq, hpost⟩ := visit_atomic_forward hctx hpending hdisp
      subst ctx_out
      refine ⟨rfl, hpost, ?_, hnez, hdec_wit⟩
      rcases hdisp with ⟨dst8, src8, off, _, _, _, hcall⟩
      have hc := hcall.symm
      have hat_cnt : ∀ {log_op : verifier.AtomicLogOp} {w_at : api.DataWidth}
          {fetch : Bool} {alu_op : verifier.ScalarAluOp},
          verifier.ComputationContext.atomic_operation ctx log_op vctx0 w_at fetch dst8 off src8 alu_op =
            ok (.Ok (), vctx1) →
          vctx1.counter = vctx0.counter := by
        intro log_op w_at fetch alu_op hop
        unfold verifier.ComputationContext.atomic_operation at hop
        rcases bind_eq_ok hop with ⟨vc1, hlog, hop⟩
        have hl3 := (log_atomic_operation_ok_spec hlog).2.2.1
        rcases bind_eq_ok hop with ⟨r, _, hop⟩
        cases r with
        | Err _ => cases (Prod.mk.inj (ok_inj hop)).1
        | Ok _ =>
          dsimp only at hop
          rcases bind_eq_ok hop with ⟨r1, _, hop⟩
          cases r1 with
          | Err _ => cases (Prod.mk.inj (ok_inj hop)).1
          | Ok _ =>
            dsimp only at hop
            rw [bind_ok_eq (Field_new_spec off w_at)] at hop
            rcases bind_eq_ok hop with ⟨r2, _, hop⟩
            cases r2 with
            | Err _ => cases (Prod.mk.inj (ok_inj hop)).1
            | Ok _ =>
              dsimp only at hop
              rcases bind_eq_ok hop with ⟨⟨r3, vc2⟩, hlm, hop⟩
              cases r3 with
              | Err _ => cases (Prod.mk.inj (ok_inj hop)).1
              | Ok _ =>
                have hm3 := load_memory_counter_eq hlm
                rcases bind_eq_ok hop with ⟨o, _, hop⟩
                cases o with
                | none => cases (Prod.mk.inj (ok_inj hop)).1
                | some _ =>
                  dsimp only at hop
                  rcases bind_eq_ok hop with ⟨o1, _, hop⟩
                  cases o1 with
                  | none => cases (Prod.mk.inj (ok_inj hop)).1
                  | some _ =>
                    dsimp only at hop
                    rcases bind_eq_ok hop with ⟨_, _, hop⟩
                    rcases bind_eq_ok hop with ⟨r4, _, hop⟩
                    cases r4 with
                    | Err _ => cases (Prod.mk.inj (ok_inj hop)).1
                    | Ok _ =>
                      dsimp only at hop
                      rcases bind_eq_ok hop with ⟨⟨r5, _, vc3⟩, hsm, hop⟩
                      cases r5 with
                      | Err _ => cases (Prod.mk.inj (ok_inj hop)).1
                      | Ok _ =>
                        have hs3 := store_memory_counter_eq hsm
                        cases fetch <;> simp only [Bool.false_eq_true, ↓reduceIte] at hop
                        · rcases bind_eq_ok hop with ⟨_, hpush, hop⟩
                          rcases Prod.mk.inj (ok_inj hop) with ⟨_, rfl⟩
                          exact (VerificationContext_push_successor_ok_spec hpush).2.2.1.trans
                            (hs3.trans (hm3.trans hl3))
                        · rcases bind_eq_ok hop with ⟨⟨r6, _⟩, _, hop⟩
                          cases r6 with
                          | Err _ => cases (Prod.mk.inj (ok_inj hop)).1
                          | Ok _ =>
                            rcases bind_eq_ok hop with ⟨_, hpush, hop⟩
                            rcases Prod.mk.inj (ok_inj hop) with ⟨_, rfl⟩
                            exact (VerificationContext_push_successor_ok_spec hpush).2.2.1.trans
                              (hs3.trans (hm3.trans hl3))
      have hcx_cnt : ∀ {log_op : verifier.AtomicLogOp} {jw : verifier.JumpWidth}
          {cond_op : verifier.BranchCondOp},
          verifier.ComputationContext.raw_atomic_cmpxchg ctx log_op vctx0 dst8 off src8 jw cond_op =
            ok (.Ok (), vctx1) →
          vctx1.counter = vctx0.counter := by
        intro log_op jw cond_op hcmp
        unfold verifier.ComputationContext.raw_atomic_cmpxchg at hcmp
        rcases bind_eq_ok hcmp with ⟨vc1, hlog, hcmp⟩
        have hl3 := (log_atomic_operation_ok_spec hlog).2.2.1
        rcases bind_eq_ok hcmp with ⟨_, _, hcmp⟩
        rcases bind_eq_ok hcmp with ⟨r, _, hcmp⟩
        cases r with
        | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
        | Ok _ =>
          dsimp only at hcmp
          rw [bind_ok_eq (Field_new_spec off _)] at hcmp
          rcases bind_eq_ok hcmp with ⟨r1, _, hcmp⟩
          cases r1 with
          | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
          | Ok _ =>
            dsimp only at hcmp
            rcases bind_eq_ok hcmp with ⟨⟨r2, vc2⟩, hlm, hcmp⟩
            cases r2 with
            | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
            | Ok _ =>
              have hm3 := load_memory_counter_eq hlm
              rcases bind_eq_ok hcmp with ⟨r3, _, hcmp⟩
              cases r3 with
              | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
              | Ok _ =>
                dsimp only at hcmp
                rcases bind_eq_ok hcmp with ⟨r4, _, hcmp⟩
                cases r4 with
                | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                | Ok _ =>
                  dsimp only at hcmp
                  rcases bind_eq_ok hcmp with ⟨b, _, hcmp⟩
                  cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hcmp
                  · cases (Prod.mk.inj (ok_inj hcmp)).1
                  · rcases bind_eq_ok hcmp with ⟨b1, _, hcmp⟩
                    cases b1 <;> simp only [Bool.false_eq_true, ↓reduceIte] at hcmp
                    · cases (Prod.mk.inj (ok_inj hcmp)).1
                    · rcases bind_eq_ok hcmp with ⟨b2, _, hcmp⟩
                      cases b2 <;> simp only [Bool.false_eq_true, ↓reduceIte] at hcmp
                      · cases (Prod.mk.inj (ok_inj hcmp)).1
                      · rcases bind_eq_ok hcmp with ⟨r5, _, hcmp⟩
                        cases r5 with
                        | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                        | Ok b3 =>
                          dsimp only at hcmp
                          rcases bind_eq_ok hcmp with ⟨_, hteq, hcmp⟩
                          rcases bind_eq_ok hcmp with ⟨_, htne, hcmp⟩
                          cases b3 with
                          | none =>
                            cases ok_inj hteq; cases ok_inj htne
                            simp only [↓reduceIte, Type_clone_spec, bind_ok] at hcmp
                            rcases bind_eq_ok hcmp with ⟨r6, _, hcmp⟩
                            cases r6 with
                            | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                            | Ok _ =>
                              rcases bind_eq_ok hcmp with ⟨⟨r7, _⟩, _, hcmp⟩
                              cases r7 with
                              | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                              | Ok _ =>
                                rcases bind_eq_ok hcmp with ⟨⟨r8, _⟩, _, hcmp⟩
                                cases r8 with
                                | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                                | Ok _ =>
                                  rcases bind_eq_ok hcmp with ⟨⟨r9, _, vc3⟩, hsm1, hcmp⟩
                                  cases r9 with
                                  | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                                  | Ok _ =>
                                    rcases bind_eq_ok hcmp with ⟨vc4, hpush1, hcmp⟩
                                    have ha3 := (VerificationContext_push_successor_ok_spec hpush1).2.2.1.trans
                                      (store_memory_counter_eq hsm1)
                                    rcases bind_eq_ok hcmp with ⟨r10, _, hcmp⟩
                                    cases r10 with
                                    | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                                    | Ok _ =>
                                      rcases bind_eq_ok hcmp with ⟨⟨r11, _⟩, _, hcmp⟩
                                      cases r11 with
                                      | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                                      | Ok _ =>
                                        rcases bind_eq_ok hcmp with ⟨⟨r12, _⟩, _, hcmp⟩
                                        cases r12 with
                                        | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                                        | Ok _ =>
                                          rcases bind_eq_ok hcmp with ⟨⟨r13, _, vc5⟩, hsm2, hcmp⟩
                                          cases r13 with
                                          | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                                          | Ok _ =>
                                            rcases bind_eq_ok hcmp with ⟨_, hpush2, hcmp⟩
                                            rcases Prod.mk.inj (ok_inj hcmp) with ⟨_, rfl⟩
                                            exact (VerificationContext_push_successor_ok_spec hpush2).2.2.1.trans
                                              ((store_memory_counter_eq hsm2).trans (ha3.trans (hm3.trans hl3)))
                          | some b4 =>
                            cases b4
                            · cases ok_inj hteq; cases ok_inj htne
                              simp only [Bool.false_eq_true, ↓reduceIte] at hcmp
                              rcases bind_eq_ok hcmp with ⟨r6, _, hcmp⟩
                              cases r6 with
                              | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                              | Ok _ =>
                                rcases bind_eq_ok hcmp with ⟨⟨r7, _⟩, _, hcmp⟩
                                cases r7 with
                                | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                                | Ok pair =>
                                  rcases pair with ⟨c_dst, _⟩
                                  rcases bind_eq_ok hcmp with ⟨t_cl, hcl, hcmp⟩
                                  cases ok_inj (hcl.symm.trans (Type_clone_spec c_dst))
                                  rcases bind_eq_ok hcmp with ⟨⟨r8, _⟩, _, hcmp⟩
                                  cases r8 with
                                  | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                                  | Ok _ =>
                                    rcases bind_eq_ok hcmp with ⟨⟨r9, _, vc3⟩, hsm1, hcmp⟩
                                    cases r9 with
                                    | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                                    | Ok _ =>
                                      rcases bind_eq_ok hcmp with ⟨_, hpush1, hcmp⟩
                                      rcases Prod.mk.inj (ok_inj hcmp) with ⟨_, rfl⟩
                                      exact (VerificationContext_push_successor_ok_spec hpush1).2.2.1.trans
                                        ((store_memory_counter_eq hsm1).trans (hm3.trans hl3))
                            · cases ok_inj hteq; cases ok_inj htne
                              simp only [↓reduceIte, Type_clone_spec, bind_ok] at hcmp
                              rcases bind_eq_ok hcmp with ⟨r6, _, hcmp⟩
                              cases r6 with
                              | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                              | Ok _ =>
                                rcases bind_eq_ok hcmp with ⟨⟨r7, _⟩, _, hcmp⟩
                                cases r7 with
                                | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                                | Ok _ =>
                                  rcases bind_eq_ok hcmp with ⟨⟨r8, _⟩, _, hcmp⟩
                                  cases r8 with
                                  | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                                  | Ok _ =>
                                    rcases bind_eq_ok hcmp with ⟨⟨r9, _, vc3⟩, hsm1, hcmp⟩
                                    cases r9 with
                                    | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                                    | Ok _ =>
                                      rcases bind_eq_ok hcmp with ⟨_, hpush1, hcmp⟩
                                      rcases Prod.mk.inj (ok_inj hcmp) with ⟨_, rfl⟩
                                      exact (VerificationContext_push_successor_ok_spec hpush1).2.2.1.trans
                                        ((store_memory_counter_eq hsm1).trans (hm3.trans hl3))
      cases is64 <;> cases op <;> (
        dsimp [atomicMethod, CompCtxVisitorInst,
          verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_add64,
          verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_add,
          verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_or64,
          verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_or,
          verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_and64,
          verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_and,
          verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_xor64,
          verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_xor,
          verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_xchg64,
          verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_xchg,
          verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_cmpxchg64,
          verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_cmpxchg] at hc
        rcases bind_eq_ok hc with ⟨⟨r0, _⟩, hop_c, hc⟩
        rcases Prod.mk.inj (ok_inj hc) with ⟨rfl, hp⟩
        rcases Prod.mk.inj hp with ⟨_, rfl⟩
        first
        | (rw [hat_cnt hop_c]; omega)
        | (rw [hcx_cnt hop_c]; omega))
    | loadPacket w imm32 indReg? =>
      obtain ⟨hctx_eq, hpost⟩ := visit_loadPacket_forward hctx hpending hdisp
      subst ctx_out
      refine ⟨rfl, hpost, ?_, hnez, hdec_wit⟩
      rcases hdisp with ⟨imm, _, hcases⟩
      have hpkt_cnt : ∀ (reg_off : Option Std.U8),
          CompCtxVisitorInst.load_from_packet ctx vctx0 0#u8 6#u8 imm reg_off (coreWidthOfSpec w) =
            ok (.Ok (), ctx, vctx1) →
          vctx1.counter = vctx0.counter := by
        intro reg_off hcall
        dsimp [CompCtxVisitorInst] at hcall
        unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.load_from_packet at hcall
        rcases bind_eq_ok hcall with ⟨vc1, hemit, hcall⟩
        obtain ⟨_, _, he3, _, _, _⟩ := VerificationContext_emit_log_ok_spec hemit
        rcases bind_eq_ok hcall with ⟨r, _, hcall⟩
        cases r with
        | Err _ => cases (Prod.mk.inj (ok_inj hcall)).1
        | Ok _ =>
          dsimp only at hcall
          rcases bind_eq_ok hcall with ⟨p_pkt, hpkt_br, _⟩
          rcases p_pkt with ⟨o, src_is_pkt⟩
          rw [bind_pair_ok_eq hpkt_br] at hcall
          cases src_is_pkt with
          | false => cases (Prod.mk.inj (ok_inj hcall)).1
          | true =>
            simp only [↓reduceIte] at hcall
            cases reg_off with
            | none =>
              rw [bind_ok_eq (IdSet_len_spec ctx.resources)] at hcall
              cases hnz : (alloc.vec.Vec.len ctx.resources.entries != 0#usize) <;>
                simp only [hnz, Bool.false_eq_true, ↓reduceIte] at hcall
              · rcases bind_eq_ok hcall with ⟨r1, hnext, hcall⟩
                cases r1 with
                | Err _ => cases (Prod.mk.inj (ok_inj hcall)).1
                | Ok n =>
                  dsimp only at hcall
                  obtain ⟨_, hn_eq⟩ := ComputationContext_next_ok_spec hnext
                  have hn_valid : CompCtxValid env vctx0.counter.bv.toNat n := by rw [hn_eq]; exact hctx
                  rw [bind_ok_eq Type_UNKNOWN_SCALAR_eq] at hcall
                  rcases bind_eq_ok hcall with ⟨⟨r2, n1⟩, hset, hcall⟩
                  cases r2 with
                  | Err _ => cases (Prod.mk.inj (ok_inj hcall)).1
                  | Ok _ =>
                    obtain ⟨_, _, _, _, _, _, hn1_valid⟩ :=
                      ComputationContext_set_reg_ok_spec hn_valid
                        (show TypeValid env vctx0.counter.bv.toNat (.ScalarValue unknownWrittenScalar64)
                          from unknownWrittenScalar64_valid) hset
                    obtain ⟨_, _, _, _, _, _, hc3, _, _, _⟩ := clobber_scratch_tail_spec hn1_valid hcall
                    exact hc3.trans he3
              · cases (Prod.mk.inj (ok_inj hcall)).1
            | some _ =>
              rcases bind_eq_ok hcall with ⟨r1, _, hcall⟩
              cases r1 with
              | Err _ => cases (Prod.mk.inj (ok_inj hcall)).1
              | Ok _ =>
                dsimp only at hcall
                rcases bind_eq_ok hcall with ⟨b, _, hcall⟩
                cases b with
                | false => cases (Prod.mk.inj (ok_inj hcall)).1
                | true =>
                  simp only [↓reduceIte, IdSet_len_spec, bind_ok] at hcall
                  cases hnz : (alloc.vec.Vec.len ctx.resources.entries != 0#usize) <;>
                    simp only [hnz, Bool.false_eq_true, ↓reduceIte] at hcall
                  · rcases bind_eq_ok hcall with ⟨r2, hnext, hcall⟩
                    cases r2 with
                    | Err _ => cases (Prod.mk.inj (ok_inj hcall)).1
                    | Ok n =>
                      dsimp only at hcall
                      obtain ⟨_, hn_eq⟩ := ComputationContext_next_ok_spec hnext
                      have hn_valid : CompCtxValid env vctx0.counter.bv.toNat n := by rw [hn_eq]; exact hctx
                      rw [bind_ok_eq Type_UNKNOWN_SCALAR_eq] at hcall
                      rcases bind_eq_ok hcall with ⟨⟨r3, n1⟩, hset, hcall⟩
                      cases r3 with
                      | Err _ => cases (Prod.mk.inj (ok_inj hcall)).1
                      | Ok _ =>
                        obtain ⟨_, _, _, _, _, _, hn1_valid⟩ :=
                          ComputationContext_set_reg_ok_spec hn_valid
                            (show TypeValid env vctx0.counter.bv.toNat (.ScalarValue unknownWrittenScalar64)
                              from unknownWrittenScalar64_valid) hset
                        obtain ⟨_, _, _, _, _, _, hc3, _, _, _⟩ := clobber_scratch_tail_spec hn1_valid hcall
                        exact hc3.trans he3
                  · cases (Prod.mk.inj (ok_inj hcall)).1
      rcases hcases with ⟨_, hcall⟩ | ⟨src8, _, hcall⟩
      · rw [hpkt_cnt none hcall.symm]; omega
      · rw [hpkt_cnt (some src8) hcall.symm]; omega
    | callExternal helperIdx =>
      obtain ⟨hctx_eq, hpost⟩ := visit_callExternal_forward hcctx hctx hpending hdisp
      subst ctx_out
      refine ⟨rfl, hpost, ?_, hnez, hdec_wit⟩
      rcases hdisp with ⟨idx32, _, hcall⟩
      have hcall' := hcall.symm
      dsimp [CompCtxVisitorInst] at hcall'
      unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.call_external at hcall'
      rcases bind_eq_ok hcall' with ⟨vc1, hemit, hcall'⟩
      obtain ⟨he1, _, he3, _, _, _⟩ := VerificationContext_emit_log_ok_spec hemit
      rcases bind_eq_ok hcall' with ⟨o_sig, hfind, hcall'⟩
      cases o_sig with
      | none => cases (Prod.mk.inj (ok_inj hcall')).1
      | some sig =>
        dsimp only at hcall'
        simp only [lift_eq_ok, bind_ok] at hcall'
        split at hcall'
        · rcases bind_eq_ok hcall' with ⟨_, _, hcall'⟩
          cases (Prod.mk.inj (ok_inj hcall')).1
        · rcases bind_eq_ok hcall' with ⟨r_next, hnext, hcall'⟩
          cases r_next with
          | Err _ => cases (Prod.mk.inj (ok_inj hcall')).1
          | Ok n =>
            dsimp only at hcall'
            obtain ⟨_, hn_eq⟩ := ComputationContext_next_ok_spec hnext
            have hcctx_vc1 : CallingContextEnvMatches vc1.calling_context env := by
              rw [he1]; exact hcctx.1
            obtain ⟨spec, _, hsig_match⟩ := CoreCallingContext_find_helper_ok_some_spec hcctx_vc1 hfind
            rcases hsig_match with ⟨_, hforall, _, hret_match, _, _⟩
            have hctx_vc1 : CompCtxValid env vc1.counter.bv.toNat ctx := by
              rw [he3]; exact hctx
            have hn_valid : CompCtxValid env vc1.counter.bv.toNat n := by
              rw [hn_eq]; exact hctx_vc1
            rcases bind_eq_ok hcall' with ⟨p_loop, hloop, _⟩
            rcases p_loop with ⟨next, loop_err⟩
            rw [bind_pair_ok_eq hloop] at hcall'
            cases loop_err with
            | some _ => cases (Prod.mk.inj (ok_inj hcall')).1
            | none =>
              obtain ⟨_, _, _, hnext_valid⟩ :=
                call_external_loop_ok_valid (vctx := vc1) hctx_vc1 hforall hn_valid hloop
              rw [bind_ok_eq (Type_clone_spec sig.return_value)] at hcall'
              rcases bind_eq_ok hcall' with ⟨next1, hinv_bm, hcall'⟩
              have hnext1_valid : CompCtxValid env vc1.counter.bv.toNat next1 := by
                by_cases hinv : sig.invalidate_array_bounds
                · simp only [hinv, ↓reduceIte] at hinv_bm
                  obtain ⟨bm, hbm_cl, _, hbm_valid⟩ := BoundsMap_clear_spec vc1.counter.bv.toNat next.array_bounds
                  rw [bind_ok_eq hbm_cl] at hinv_bm
                  cases ok_inj hinv_bm
                  exact ⟨hnext_valid.1, hnext_valid.2.1, hbm_valid, hnext_valid.2.2.2⟩
                · simp only [hinv, Bool.false_eq_true, ↓reduceIte] at hinv_bm
                  cases ok_inj hinv_bm
                  exact hnext_valid
              rcases bind_eq_ok hcall' with ⟨p_ret, hres_ret, _⟩
              rcases p_ret with ⟨r1, p_ret2⟩
              rcases p_ret2 with ⟨vc2, next2⟩
              rw [bind_triple_ok_eq hres_ret] at hcall'
              cases r1 with
              | Err _ => cases (Prod.mk.inj (ok_inj hcall')).1
              | Ok v =>
                dsimp only at hcall'
                obtain ⟨_, _, _, _, _, _, _, _, _, _, hnext2_valid, hv_valid⟩ :=
                  resolve_return_value_ok_valid hret_match hnext1_valid hres_ret
                have hcnt_ret := resolve_return_value_counter_le sig.return_value hres_ret
                have hbound3 := returnTypeMatches_allocBound_le_three hret_match
                rcases bind_eq_ok hcall' with ⟨p_set, hset, _⟩
                rcases p_set with ⟨r2, next3⟩
                rw [bind_pair_ok_eq hset] at hcall'
                cases r2 with
                | Err _ => cases (Prod.mk.inj (ok_inj hcall')).1
                | Ok _ =>
                  dsimp only at hcall'
                  obtain ⟨_, _, _, _, _, _, hnext3_valid⟩ :=
                    ComputationContext_set_reg_ok_spec hnext2_valid hv_valid hset
                  obtain ⟨_, _, _, _, _, _, ht_cnt, _, _, _⟩ :=
                    clobber_scratch_tail_spec hnext3_valid hcall'
                  rw [ht_cnt]
                  rw [he3] at hcnt_ret
                  omega
    | lddwImm _ _ | lddwMap _ _ | lddwMapValue _ _ _ | lddwCont =>
      exact False.elim hdisp

/-- Unified backward 2-safety transfer for `BpfVisitor.visit.default` on `DependencyTracker`. -/
theorem visit_any_backward
    {codeOut : List api.EbpfInstruction} {prog : Program} {env : ExecEnv} {F : Nat}
    {ctx ctx_out : verifier.ComputationContext}
    {vctx0 vctx1 vctx_back vctx_back1 : verifier.VerificationContext}
    {dPost : verifier.DataDependencies} {tracker : verifier.DependencyTracker}
    {inst : api.EbpfInstruction}
    (hdec : decodeVerifiedCode codeOut = some prog)
    (hcode_len : vctx0.code.val.length = prog.size)
    (hlt_pc : ctx.pc.val < prog.size)
    (hmatch_pc : CodeMatchesProgAt vctx0.calling_context vctx0.code.val prog ctx.pc.val)
    (hcctx : CallingContextFullMatches vctx0.calling_context env)
    (hcctx_back : vctx_back.calling_context = vctx0.calling_context)
    (hctx : CompCtxValid env vctx0.counter.bv.toNat ctx)
    (hv1_lt : vctx1.counter.bv.toNat < 2 ^ 63)
    (hpending : vctx0.pending_successors.val = [])
    (hinst_at : vctx0.code.val[ctx.pc.val]? = some inst)
    (hfwd : visitor.BpfVisitor.visit.default CompCtxVisitorInst ctx vctx0 inst =
      .ok (.Ok (), ctx_out, vctx1))
    (hchildren : ∀ succ, some succ ∈ vctx1.pending_successors.val →
      SemTerminatesIn prog env vctx1.counter.bv.toNat succ dPost F)
    (hback : visitor.BpfVisitor.visit.default DepTrackerVisitorInst
      { context := ctx_out, deps := dPost } vctx_back inst =
      .ok (.Ok (), tracker, vctx_back1)) :
    ctx_out = ctx ∧
    tracker.context = ctx ∧
    vctx_back1 = vctx_back ∧
    SemTerminatesIn prog env vctx0.counter.bv.toNat ctx tracker.deps (F + 1) := by
  obtain ⟨hctx_eq, hpost, _, _, _⟩ := visit_any_forward hcctx hctx hpending hfwd
  subst ctx_out
  have hcnt_le : vctx0.counter.bv.toNat ≤ vctx1.counter.bv.toNat := hpost.2.2.2.2.1
  have hctx_v1 : CompCtxValid env vctx1.counter.bv.toNat ctx := CompCtxValid_mono hcnt_le hctx
  obtain ⟨raw, insn_p, hraw_at, hprog_at, _, h18_spec, hne18_spec⟩ := hmatch_pc
  have hraw_eq : raw = inst := Option.some.inj (hraw_at.symm.trans hinst_at)
  subst raw
  have hinsn_get : prog[ctx.pc.val]'hlt_pc = insn_p := by
    rw [Array.getElem?_eq_getElem hlt_pc] at hprog_at
    exact Option.some.inj hprog_at
  obtain ⟨_, hcases_f⟩ := visit_default_ok_spec CompCtxVisitorInst ctx ctx vctx0 vctx1 inst hfwd
  obtain ⟨_, hcases_b⟩ := visit_default_ok_spec DepTrackerVisitorInst
    { context := ctx, deps := dPost } tracker vctx_back vctx_back1 inst hback
  rcases hcases_f with ⟨h18, hload64_f⟩ | ⟨hne18, insn_f, hdec_f, hdisp_f⟩
  · rcases hcases_b with ⟨_, hload64_b⟩ | ⟨hne18_b, _⟩
    · obtain ⟨raw2, rlddw, hcode2, _, hres_pre, hinsn_eq⟩ := h18_spec h18
      have hdst_val : (⟨((inst.bv >>> 8) &&& 0xF#64).setWidth 8⟩ : Std.U8).val =
          ((inst.bv >>> 8) &&& 0xF#64).toNat := nibble_setWidth8_val inst.bv 8
      have hinsn_lddw : prog[ctx.pc.val]'hlt_pc =
          lddwInsnOfResolved (⟨((inst.bv >>> 8) &&& 0xF#64).setWidth 8⟩ : Std.U8).val rlddw := by
        rw [hinsn_get, hinsn_eq, hdst_val]
      have hcont : ctx.pc.val + 1 < prog.size ∧ prog[ctx.pc.val + 1]? = some .lddwCont :=
        decodeVerifiedCode_lddw_has_cont hdec hlt_pc
          (by rw [hinsn_lddw]; exact lddwInsnOfResolved_isLddwPrimary _ rlddw)
      exact visit_load64_backward hcctx.1 hlt_pc hinsn_lddw hcont hcode2 hres_pre
        hctx hpending hload64_f hchildren hload64_b
    · exact False.elim (hne18_b h18)
  · rcases hcases_b with ⟨h18_b, _⟩ | ⟨_, insn_b, hdec_b, hdisp_b⟩
    · exact False.elim (hne18 h18_b)
    · have hdec_p := hne18_spec hne18
      have hf_eq : insn_p = insn_f := Option.some.inj (hdec_p.symm.trans hdec_f)
      have hb_eq : insn_b = insn_f := Option.some.inj (hdec_b.symm.trans hdec_f)
      rw [hb_eq] at hdisp_b
      have hpc_wit : ∃ hlt : ctx.pc.val < prog.size, prog[ctx.pc.val]'hlt = insn_f :=
        ⟨hlt_pc, hinsn_get.trans hf_eq⟩
      cases insn_f with
      | alu64 op dst src =>
        obtain ⟨_, ht_ctx, hv_b, hsem⟩ :=
          visit_alu64_backward hpc_wit hctx_v1 hpending hdisp_f hchildren hdisp_b
        exact ⟨rfl, ht_ctx, hv_b, SemTerminatesIn_shrink_vCounter hcnt_le hv1_lt hctx hsem⟩
      | alu32 op dst src =>
        obtain ⟨_, ht_ctx, hv_b, hsem⟩ :=
          visit_alu32_backward hpc_wit hctx_v1 hpending hdisp_f hchildren hdisp_b
        exact ⟨rfl, ht_ctx, hv_b, SemTerminatesIn_shrink_vCounter hcnt_le hv1_lt hctx hsem⟩
      | neg is64 dst =>
        obtain ⟨_, ht_ctx, hv_b, hsem⟩ :=
          visit_neg_backward hpc_wit hctx_v1 hpending hdisp_f hchildren hdisp_b
        exact ⟨rfl, ht_ctx, hv_b, SemTerminatesIn_shrink_vCounter hcnt_le hv1_lt hctx hsem⟩
      | endian doSwap ew dst =>
        obtain ⟨_, ht_ctx, hv_b, hsem⟩ :=
          visit_endian_backward hcctx.1 hpc_wit hctx_v1 hpending hdisp_f hchildren hdisp_b
        exact ⟨rfl, ht_ctx, hv_b, SemTerminatesIn_shrink_vCounter hcnt_le hv1_lt hctx hsem⟩
      | jump offInt =>
        obtain ⟨_, ht_ctx, hv_b, hsem⟩ :=
          visit_jump_backward hcode_len hpc_wit hpending hdisp_f hchildren hdisp_b
        exact ⟨rfl, ht_ctx, hv_b, SemTerminatesIn_shrink_vCounter hcnt_le hv1_lt hctx hsem⟩
      | exit =>
        obtain ⟨_, ht_ctx, hv_b, hsem⟩ :=
          visit_exit_backward (F := F) hpc_wit hctx_v1 hdisp_f hdisp_b
        exact ⟨rfl, ht_ctx, hv_b, SemTerminatesIn_shrink_vCounter hcnt_le hv1_lt hctx hsem⟩
      | condJump is64 op dst src offInt =>
        obtain ⟨_, ht_ctx, hv_b, hsem⟩ :=
          visit_condJump_backward hcode_len hpc_wit hctx_v1 hpending hdisp_f hchildren hdisp_b
        exact ⟨rfl, ht_ctx, hv_b, SemTerminatesIn_shrink_vCounter hcnt_le hv1_lt hctx hsem⟩
      | load w dst base off16 =>
        obtain ⟨_, ht_ctx, hv_b, hsem⟩ :=
          visit_load_backward hpc_wit hctx_v1 hpending hdisp_f hchildren hdisp_b
        exact ⟨rfl, ht_ctx, hv_b, SemTerminatesIn_shrink_vCounter hcnt_le hv1_lt hctx hsem⟩
      | store w base off16 src =>
        obtain ⟨_, ht_ctx, hv_b, hsem⟩ :=
          visit_store_backward hpc_wit hctx_v1 hpending hdisp_f hchildren hdisp_b
        exact ⟨rfl, ht_ctx, hv_b, SemTerminatesIn_shrink_vCounter hcnt_le hv1_lt hctx hsem⟩
      | atomic is64 op base off16 src =>
        obtain ⟨_, ht_ctx, hv_b, hsem⟩ :=
          visit_atomic_backward hpc_wit hctx_v1 hpending hdisp_f hchildren hdisp_b
        exact ⟨rfl, ht_ctx, hv_b, SemTerminatesIn_shrink_vCounter hcnt_le hv1_lt hctx hsem⟩
      | loadPacket w imm32 indReg? =>
        obtain ⟨_, ht_ctx, hv_b, hsem⟩ :=
          visit_loadPacket_backward hcctx.1 hpc_wit hctx_v1 hpending hdisp_f hchildren hdisp_b
        exact ⟨rfl, ht_ctx, hv_b, SemTerminatesIn_shrink_vCounter hcnt_le hv1_lt hctx hsem⟩
      | callExternal helperIdx =>
        exact visit_callExternal_backward hcctx hcctx_back hpc_wit hctx hpending hdisp_f hchildren hdisp_b
      | lddwImm _ _ | lddwMap _ _ | lddwMapValue _ _ _ | lddwCont =>
        exact False.elim hdisp_f

/-! ## 3. Worklist, ExplorerArena, & Main Verification Loop (`verify_program_core_loop5`) -/

/-! ### 3.1 `DataDependencies`, `Worklist`, & `ExplorerArena` Specifications -/

private theorem DataDependencies_default_zero :
    verifier.DataDependencies.Insts.CoreDefaultDefault.default =
      ok { registers := 0#u16, stack := 0#u64 } := by
  unfold verifier.DataDependencies.Insts.CoreDefaultDefault.default
    core.default.DefaultU16.default core.default.DefaultU64.default
  rfl

private theorem DataDependencies_merge_subset {d1 d2 out : verifier.DataDependencies}
    (h : verifier.DataDependencies.merge d1 d2 = ok out) :
    DepsSubset d1 out ∧ DepsSubset d2 out := by
  unfold verifier.DataDependencies.merge at h
  simp only [lift_eq_ok, bind_ok] at h
  cases ok_inj h
  refine ⟨⟨fun r hr hd => ?_, fun s hs hd => ?_⟩, ⟨fun r hr hd => ?_, fun s hs hd => ?_⟩⟩
  · unfold DataDepsHasReg at hd ⊢
    change (d1.registers.bv ||| d2.registers.bv).getLsbD r = true
    rw [BitVec.getLsbD_or, hd]; rfl
  · unfold DataDepsHasStack at hd ⊢
    change (d1.stack.bv ||| d2.stack.bv).getLsbD s = true
    rw [BitVec.getLsbD_or, hd]; rfl
  · unfold DataDepsHasReg at hd ⊢
    change (d1.registers.bv ||| d2.registers.bv).getLsbD r = true
    rw [BitVec.getLsbD_or, hd, Bool.or_true]
  · unfold DataDepsHasStack at hd ⊢
    change (d1.stack.bv ||| d2.stack.bv).getLsbD s = true
    rw [BitVec.getLsbD_or, hd, Bool.or_true]

private theorem vec_push_len_eq {α : Type} {v v' : alloc.vec.Vec α} {x : α}
    (h : alloc.vec.Vec.push v x = ok v') : v'.val.length = v.val.length + 1 := by
  rw [vec_push_ok_val h, List.length_append, List.length_singleton]

private theorem list_getElem?_set_eq {α : Type u} :
    ∀ (l : List α) (k : Nat) (x : α), k < l.length → (l.set k x)[k]? = some x
  | [], _, _, h => by simp at h
  | _ :: _, 0, x, _ => rfl
  | _ :: tl, k + 1, x, h => by
      simp only [List.length_cons] at h
      exact list_getElem?_set_eq tl k x (by omega)

private theorem list_getElem?_set_of_ne {α : Type u} :
    ∀ (l : List α) (k j : Nat) (x : α), k ≠ j → (l.set k x)[j]? = l[j]?
  | [], _, _, _, _ => rfl
  | _ :: _, 0, 0, _, hne => False.elim (hne rfl)
  | _ :: _, 0, _ + 1, _, _ => rfl
  | _ :: _, _ + 1, 0, _, _ => rfl
  | _ :: tl, k + 1, j + 1, x, hne => list_getElem?_set_of_ne tl k j x (by omega)

/-- Representation invariant linking `verifier.Worklist` to its active `List WorklistEntry`. -/
def WorklistRep (wl : verifier.Worklist) (es : List verifier.WorklistEntry) : Prop :=
  wl.len.val = es.length ∧
  wl.len.val ≤ wl.entries.val.length ∧
  wl.entries.val.take wl.len.val = es.map some

private theorem list_take_set_self {α : Type u} :
    ∀ (l : List α) (k : Nat) (x : α), k < l.length →
      (l.set k x).take (k + 1) = l.take k ++ [x]
  | [], _, _, h => by simp at h
  | _ :: tl, 0, x, _ => by simp
  | hd :: tl, k + 1, x, h => by
      simp only [List.length_cons] at h
      simp [List.set, List.take, list_take_set_self tl k x (by omega)]

private theorem list_take_set_ge {α : Type u} :
    ∀ (l : List α) (k n : Nat) (x : α), n ≤ k →
      (l.set k x).take n = l.take n
  | [], _, _, _, _ => by simp
  | _ :: _, _, 0, _, _ => by simp
  | _ :: _, 0, n + 1, _, h => by omega
  | hd :: tl, k + 1, n + 1, x, h => by
      simp [List.set, List.take, list_take_set_ge tl k n x (by omega)]

theorem Worklist_new_spec {wl : verifier.Worklist}
    (h : verifier.Worklist.new = ok wl) :
    WorklistRep wl [] := by
  unfold verifier.Worklist.new at h
  cases ok_inj h
  exact ⟨by simp, by simp [alloc.vec.Vec.new], by simp [alloc.vec.Vec.new]⟩

theorem Worklist_push_spec {wl wl' : verifier.Worklist}
    {es : List verifier.WorklistEntry} {entry : verifier.WorklistEntry}
    (hrep : WorklistRep wl es)
    (hpush : verifier.Worklist.push wl entry = ok wl') :
    WorklistRep wl' (es ++ [entry]) := by
  rcases hrep with ⟨hlen_eq, hlen_le, htake⟩
  unfold verifier.Worklist.push at hpush
  by_cases hlt : wl.len < alloc.vec.Vec.len wl.entries
  · simp only [hlt, ↓reduceIte] at hpush
    rcases bind_eq_ok hpush with ⟨pmb, hidx_mut, _⟩
    rcases pmb with ⟨_, back⟩
    rw [bind_pair_ok_eq hidx_mut] at hpush
    rcases bind_eq_ok hpush with ⟨i1, hadd, hpush⟩
    cases ok_inj hpush
    obtain ⟨hlt_v, _, hset⟩ := vec_index_mut_usize_ok hidx_mut
    obtain ⟨hval, hlen_b⟩ := hset (some entry)
    have hi1_val : i1.val = wl.len.val + 1 := by
      simpa [usize_one_val] using (uscalar_add_ok_val hadd).1
    refine ⟨by simp [hi1_val, hlen_eq], by dsimp only; rw [hi1_val, hlen_b]; omega, ?_⟩
    dsimp only
    rw [hval, hi1_val, list_take_set_self wl.entries.val wl.len.val (some entry) hlt_v,
        htake, List.map_append]
    rfl
  · simp only [hlt, ↓reduceIte] at hpush
    rcases bind_eq_ok hpush with ⟨v, hv_push, hpush⟩
    rcases bind_eq_ok hpush with ⟨i1, hadd, hpush⟩
    cases ok_inj hpush
    have hk_eq : wl.len.val = wl.entries.val.length := by
      have hn : ¬ (wl.len.val < wl.entries.val.length) := hlt
      omega
    have hi1_val : i1.val = wl.len.val + 1 := by
      simpa [usize_one_val] using (uscalar_add_ok_val hadd).1
    have hv_val := vec_push_ok_val hv_push
    have hv_len := vec_push_len_eq hv_push
    refine ⟨by simp [hi1_val, hlen_eq], by dsimp only; rw [hi1_val, hv_len]; omega, ?_⟩
    dsimp only
    have htake_all : wl.entries.val = es.map some := by
      simpa [hk_eq] using htake
    rw [hv_val, hi1_val, hk_eq, ← hv_len, ← hv_val, List.take_length, hv_val,
        htake_all, List.map_append]
    rfl

private theorem list_snoc_cases {α : Type u} (l : List α) :
    l = [] ∨ ∃ init last, l = init ++ [last] := by
  induction l with
  | nil => exact Or.inl rfl
  | cons hd tl ih =>
    rcases ih with rfl | ⟨init, last, rfl⟩
    · exact Or.inr ⟨[], hd, rfl⟩
    · exact Or.inr ⟨hd :: init, last, rfl⟩

private theorem usize_sub_one_ok_val {a b : Std.Usize}
    (h1 : 1 ≤ a.val) (h : a - 1#usize = ok b) : b.val + 1 = a.val := by
  have hle : (1#usize).val ≤ a.val := by rw [usize_one_val]; exact h1
  have hs := UScalar.sub_bv_spec hle
  rw [h] at hs; simp at hs
  have hv : b.val = a.val - (1#usize).val := hs.1
  rw [usize_one_val] at hv; omega

theorem Worklist_pop_spec {wl wl' : verifier.Worklist}
    {es : List verifier.WorklistEntry} {out : Option verifier.WorklistEntry}
    (hrep : WorklistRep wl es)
    (hpop : verifier.Worklist.pop wl = ok (out, wl')) :
    (es = [] ∧ out = none ∧ WorklistRep wl' []) ∨
    (∃ es_init last_e,
      es = es_init ++ [last_e] ∧
      out = some last_e ∧
      WorklistRep wl' es_init) := by
  rcases hrep with ⟨hlen_eq, hlen_le, htake⟩
  unfold verifier.Worklist.pop at hpop
  by_cases hz : wl.len = 0#usize
  · simp only [hz, ↓reduceIte] at hpop
    rcases Prod.mk.inj (ok_inj hpop) with ⟨rfl, rfl⟩
    have hes_nil : es = [] := by
      apply List.eq_nil_of_length_eq_zero
      rw [← hlen_eq, hz, usize_const_val_0]
    subst hes_nil
    exact Or.inl ⟨rfl, rfl, hlen_eq, hlen_le, htake⟩
  · simp only [hz, ↓reduceIte] at hpop
    rcases list_snoc_cases es with rfl | ⟨es_init, last_e, rfl⟩
    · exfalso; apply hz; apply usize_eq_of_val_eq
      rw [hlen_eq, List.length_nil, usize_const_val_0]
    · rcases bind_eq_ok hpop with ⟨i, hsub, hpop⟩
      have hwl_pos : 1 ≤ wl.len.val := by
        simp only [List.length_append, List.length_singleton] at hlen_eq; omega
      have hi_val : i.val = es_init.length := by
        have := usize_sub_one_ok_val hwl_pos hsub
        simp only [List.length_append, List.length_singleton] at hlen_eq
        omega
      rcases bind_eq_ok hpop with ⟨pmb, hidx_mut, _⟩
      rcases pmb with ⟨o, back⟩
      rw [bind_pair_ok_eq hidx_mut] at hpop
      dsimp [core.mem.replace] at hpop
      rcases Prod.mk.inj (ok_inj hpop) with ⟨rfl, rfl⟩
      obtain ⟨hlt_i, hget_i, hset_i⟩ := vec_index_mut_usize_ok hidx_mut
      obtain ⟨hval_b, hlen_b⟩ := hset_i none
      have hi_lt_len : i.val < wl.len.val := by
        simp only [List.length_append, List.length_singleton] at hlen_eq; omega
      have ho_eq : o = some last_e := by
        have h1 : (wl.entries.val.take wl.len.val)[i.val]? = wl.entries.val[i.val]? :=
          List.getElem?_take_of_lt hi_lt_len
        rw [htake, List.map_append, hi_val,
            List.getElem?_append_right (by simp),
            show es_init.length - (es_init.map some).length = 0 by simp] at h1
        have h2 : ([last_e].map some)[0]? = some (some last_e) := rfl
        rw [h2] at h1
        rw [← hi_val] at h1
        exact Option.some.inj (hget_i.symm.trans h1.symm)
      subst ho_eq
      refine Or.inr ⟨es_init, last_e, rfl, rfl, hi_val, by dsimp only; rw [hlen_b]; omega, ?_⟩
      dsimp only
      rw [hval_b, list_take_set_ge wl.entries.val i.val i.val none (Nat.le_refl _)]
      have htake2 : (wl.entries.val.take wl.len.val).take i.val = wl.entries.val.take i.val :=
        List.take_take.trans (congrArg (fun k => wl.entries.val.take k) (Nat.min_eq_left (Nat.le_of_lt hi_lt_len)))
      rw [← htake2, htake, List.map_append, hi_val]
      simp

theorem opt_list_eq_map_some {α : Type u} {P : α → Prop} :
    ∀ (l : List (Option α)),
      (∀ opt ∈ l, ∃ x, opt = some x ∧ P x) →
      ∃ xs : List α, l = xs.map some ∧ ∀ x ∈ xs, P x
  | [], _ => ⟨[], rfl, fun _ h => by cases h⟩
  | opt :: tl, hall => by
      obtain ⟨x, rfl, hx⟩ := hall opt List.mem_cons_self
      obtain ⟨xs, htl, hxs⟩ := opt_list_eq_map_some tl (fun o ho => hall o (List.mem_cons_of_mem _ ho))
      refine ⟨x :: xs, by simp [htl], fun y hy => ?_⟩
      rcases List.mem_cons.mp hy with rfl | hmem
      · exact hx
      · exact hxs y hmem

private theorem list_drop_set_gt_local {α : Type u} :
    ∀ (l : List α) (k n : Nat) (x : α), k < n → (l.set k x).drop n = l.drop n
  | [], _, _, _, _ => rfl
  | _ :: _, 0, 0, _, h => by omega
  | _ :: _, 0, _ + 1, _, _ => rfl
  | _ :: _, _ + 1, 0, _, h => by omega
  | _ :: tl, k + 1, n + 1, x, h => list_drop_set_gt_local tl k n x (by omega)

theorem verify_worklist_entry_loop_spec
    {wl wl' : verifier.Worklist}
    {es : List verifier.WorklistEntry}
    {v : alloc.vec.Vec (Option verifier.ComputationContext)}
    {succs : List verifier.ComputationContext}
    {num_succ node_idx : Std.Usize}
    (hrep : WorklistRep wl es)
    (hv_eq : v.val = succs.map some)
    (hnum : num_succ.val = succs.length)
    (hloop : verifier.verify_worklist_entry_loop wl v num_succ node_idx 0#usize = ok wl') :
    WorklistRep wl' (es ++ succs.map (fun s => { context := s, parent := some node_idx })) := by
  unfold verifier.verify_worklist_entry_loop at hloop
  have hmain := loop_ok_induct
    (fun (wl1, v1, s_i1) => verifier.verify_worklist_entry_loop.body num_succ node_idx wl1 v1 s_i1)
    (fun (wl1, v1, s_i1) =>
      s_i1.val ≤ succs.length ∧
      v1.val.length = succs.length ∧
      v1.val.drop s_i1.val = (succs.drop s_i1.val).map some ∧
      WorklistRep wl1 (es ++ (succs.take s_i1.val).map (fun s => { context := s, parent := some node_idx })))
    (fun out =>
      WorklistRep out (es ++ succs.map (fun s => { context := s, parent := some node_idx })))
    (by
      rintro ⟨wl1, v1, s_i1⟩ ⟨hle, hvlen, hdrop, hrep1⟩
      unfold verifier.verify_worklist_entry_loop.body
      constructor
      · rintro ⟨wl2, v2, s_i2⟩ hbody
        by_cases hlt : s_i1 < num_succ
        · simp only [hlt, ↓reduceIte] at hbody
          have hlt_s : s_i1.val < succs.length := by
            have : s_i1.val < num_succ.val := hlt
            omega
          rcases bind_eq_ok hbody with ⟨pmb, hidx_mut, _⟩
          rcases pmb with ⟨o, back⟩
          rw [bind_pair_ok_eq hidx_mut] at hbody
          dsimp [core.mem.replace] at hbody
          obtain ⟨hlt_v1, hget_o, hset_v1⟩ := vec_index_mut_usize_ok hidx_mut
          obtain ⟨hback_val, hback_len⟩ := hset_v1 none
          have ho_eq : o = some (succs[s_i1.val]'hlt_s) := by
            have hd := List.drop_eq_getElem_cons hlt_v1
            have hd_s := List.drop_eq_getElem_cons hlt_s
            rw [hd, hd_s, List.map_cons] at hdrop
            have hhd := (List.cons.inj hdrop).1
            rw [List.getElem?_eq_getElem hlt_v1] at hget_o
            exact (Option.some.inj hget_o).symm.trans hhd
          subst ho_eq
          rcases bind_eq_ok hbody with ⟨⟨wl_mid, v_mid⟩, hpw, hbody⟩
          dsimp only at hpw hbody
          rcases bind_eq_ok hpw with ⟨wl_p, hpush_wl, hpw⟩
          rcases Prod.mk.inj (ok_inj hpw) with ⟨rfl, rfl⟩
          rcases bind_eq_ok hbody with ⟨s_next, hadd, hbody⟩
          cases ok_inj hbody
          have hs_val : s_i2.val = s_i1.val + 1 := by
            simpa [usize_one_val] using (uscalar_add_ok_val hadd).1
          have hrep_p := Worklist_push_spec hrep1 hpush_wl
          refine ⟨by omega, hback_len.trans hvlen, ?_, ?_⟩
          · rw [hback_val, hs_val, list_drop_set_gt_local v1.val s_i1.val (s_i1.val + 1) none (by omega)]
            have hd1 := List.drop_eq_getElem_cons hlt_v1
            have hd2 := List.drop_eq_getElem_cons hlt_s
            rw [hd1, hd2, List.map_cons] at hdrop
            exact (List.cons.inj hdrop).2
          · have htake_step : succs.take (s_i1.val + 1) = succs.take s_i1.val ++ [succs[s_i1.val]'hlt_s] := by
              rw [List.take_add_one, List.getElem?_eq_getElem hlt_s]; rfl
            rw [hs_val, htake_step, List.map_append, List.map, ← List.append_assoc]
            exact hrep_p
        · simp only [hlt, ↓reduceIte] at hbody
          cases ok_inj hbody
      · intro out hbody
        by_cases hlt : s_i1 < num_succ
        · simp only [hlt, ↓reduceIte] at hbody
          rcases bind_eq_ok hbody with ⟨pmb, hidx_mut, _⟩
          rcases pmb with ⟨o, _⟩
          rw [bind_pair_ok_eq hidx_mut] at hbody
          dsimp [core.mem.replace] at hbody
          rcases bind_eq_ok hbody with ⟨⟨_, _⟩, _, hbody⟩
          rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
          cases ok_inj hbody
        · simp only [hlt, ↓reduceIte] at hbody
          cases ok_inj hbody
          have hs_eq : s_i1.val = succs.length := by
            have hn : ¬ (s_i1.val < num_succ.val) := hlt
            omega
          simpa [hs_eq] using hrep1)
    (wl, v, 0#usize) wl'
    ⟨by simp, by simp [hv_eq], by simp [hv_eq], by simpa using hrep⟩
    hloop
  exact hmain

/-- Structural free-list invariant on `verifier.ExplorerArena`. -/
def ArenaWf (ar : verifier.ExplorerArena) : Prop :=
  ar.free_len.val ≤ ar.free_indices.val.length ∧
  (ar.free_indices.val.take ar.free_len.val).Nodup ∧
  ∀ idx ∈ ar.free_indices.val.take ar.free_len.val,
    idx.val < ar.nodes.val.length ∧ ar.nodes.val[idx.val]? = some none

/-- Number of active (`some node`) slots in `ExplorerArena`. -/
def arenaActiveCount (ar : verifier.ExplorerArena) : Nat :=
  (ar.nodes.val.filter Option.isSome).length

private theorem filter_isSome_set_none_to_some {α : Type u} :
    ∀ (l : List (Option α)) (k : Nat) (x : α),
      k < l.length → l[k]? = some none →
      ((l.set k (some x)).filter Option.isSome).length = (l.filter Option.isSome).length + 1
  | [], _, _, h, _ => by simp at h
  | o :: tl, 0, x, _, h0 => by
      cases Option.some.inj h0
      simp [List.filter]
  | o :: tl, k + 1, x, h, hk => by
      simp only [List.length_cons] at h
      simp only [List.getElem?_cons_succ] at hk
      have ih := filter_isSome_set_none_to_some tl k x (by omega) hk
      cases o <;> simp [List.set, List.filter, ih]

private theorem filter_isSome_set_some_to_some {α : Type u} :
    ∀ (l : List (Option α)) (k : Nat) (y x : α),
      k < l.length → l[k]? = some (some y) →
      ((l.set k (some x)).filter Option.isSome).length = (l.filter Option.isSome).length
  | [], _, _, _, h, _ => by simp at h
  | o :: tl, 0, y, x, _, h0 => by
      cases Option.some.inj h0
      simp [List.filter]
  | o :: tl, k + 1, y, x, h, hk => by
      simp only [List.length_cons] at h
      simp only [List.getElem?_cons_succ] at hk
      have ih := filter_isSome_set_some_to_some tl k y x (by omega) hk
      cases o <;> simp [List.set, List.filter, ih]

private theorem filter_isSome_set_some_to_none {α : Type u} :
    ∀ (l : List (Option α)) (k : Nat) (y : α),
      k < l.length → l[k]? = some (some y) →
      ((l.set k none).filter Option.isSome).length + 1 = (l.filter Option.isSome).length
  | [], _, _, h, _ => by simp at h
  | o :: tl, 0, y, _, h0 => by
      cases Option.some.inj h0
      simp [List.filter]
  | o :: tl, k + 1, y, h, hk => by
      simp only [List.length_cons] at h
      simp only [List.getElem?_cons_succ] at hk
      have ih := filter_isSome_set_some_to_none tl k y (by omega) hk
      cases o <;> simp [List.set, List.filter] <;> omega

theorem ExplorerArena_new_spec {ar : verifier.ExplorerArena}
    (h : verifier.ExplorerArena.new = ok ar) :
    ArenaWf ar ∧ ar.nodes.val = [] ∧ arenaActiveCount ar = 0 := by
  unfold verifier.ExplorerArena.new at h
  cases ok_inj h
  refine ⟨⟨by simp [alloc.vec.Vec.new], by simp [alloc.vec.Vec.new], ?_⟩, rfl, rfl⟩
  intro _ hmem; simp [alloc.vec.Vec.new] at hmem

theorem ExplorerArena_push_spec {ar ar' : verifier.ExplorerArena}
    {node : verifier.ExplorerNode} {idx : Std.Usize}
    (hwf : ArenaWf ar)
    (hpush : verifier.ExplorerArena.push ar node = ok (idx, ar')) :
    ArenaWf ar' ∧
    (∀ n, ar.nodes.val[idx.val]? ≠ some (some n)) ∧
    idx.val < ar'.nodes.val.length ∧
    ar'.nodes.val[idx.val]? = some (some node) ∧
    (∀ k, k ≠ idx.val → ar'.nodes.val[k]? = ar.nodes.val[k]?) ∧
    arenaActiveCount ar' = arenaActiveCount ar + 1 := by
  rcases hwf with ⟨hflen_le, hnodup, hfree_none⟩
  unfold verifier.ExplorerArena.push at hpush
  by_cases hgt : ar.free_len > 0#usize
  · simp only [hgt, ↓reduceIte] at hpush
    have hfl_pos : 1 ≤ ar.free_len.val := by
      have : (0#usize).val < ar.free_len.val := hgt
      rw [usize_const_val_0] at this; omega
    rcases bind_eq_ok hpush with ⟨i, hsub, hpush⟩
    have hi_add := usize_sub_one_ok_val hfl_pos hsub
    rcases bind_eq_ok hpush with ⟨idx0, hidx_get, hpush⟩
    rcases bind_eq_ok hpush with ⟨pmb, hmut, _⟩
    rcases pmb with ⟨_, back⟩
    rw [bind_pair_ok_eq hmut] at hpush
    rcases Prod.mk.inj (ok_inj hpush) with ⟨rfl, rfl⟩
    obtain ⟨hlt_fi, hget_fi⟩ := vec_index_usize_ok hidx_get
    obtain ⟨hlt_nd, _, hset_nd⟩ := vec_index_mut_usize_ok hmut
    obtain ⟨hval_nd, hlen_nd⟩ := hset_nd (some node)
    have htake_split : ar.free_indices.val.take ar.free_len.val =
        ar.free_indices.val.take i.val ++ [idx0] := by
      have h1 : ar.free_len.val = i.val + 1 := hi_add.symm
      rw [h1, List.take_add_one, hget_fi]; simp
    have hmem_idx0 : idx0 ∈ ar.free_indices.val.take ar.free_len.val := by
      rw [htake_split]; simp
    obtain ⟨_, hwas_none⟩ := hfree_none idx0 hmem_idx0
    rw [htake_split, List.nodup_append] at hnodup
    rcases hnodup with ⟨hnodup_init, _, hdisj⟩
    refine ⟨⟨by dsimp only; omega, hnodup_init, ?_⟩, ?_, ?_, ?_, ?_, ?_⟩
    · intro j hj
      have hj_old : j ∈ ar.free_indices.val.take ar.free_len.val := by
        rw [htake_split]; exact List.mem_append_left _ hj
      obtain ⟨hj_lt, hj_none⟩ := hfree_none j hj_old
      have hj_ne : j ≠ idx0 := fun hc => hdisj j hj idx0 (List.mem_singleton.mpr rfl) hc
      have hj_val_ne : j.val ≠ idx0.val := fun hc => hj_ne (usize_eq_of_val_eq hc)
      refine ⟨by dsimp only; omega, ?_⟩
      dsimp only
      rw [hval_nd, list_getElem?_set_of_ne ar.nodes.val idx0.val j.val (some node) hj_val_ne.symm]
      exact hj_none
    · intro n hn; rw [hwas_none] at hn; cases Option.some.inj hn
    · dsimp only; omega
    · dsimp only; rw [hval_nd]; exact list_getElem?_set_eq ar.nodes.val idx0.val (some node) hlt_nd
    · intro k hk; dsimp only; rw [hval_nd]
      exact list_getElem?_set_of_ne ar.nodes.val idx0.val k (some node) hk.symm
    · unfold arenaActiveCount; dsimp only; rw [hval_nd]
      exact filter_isSome_set_none_to_some ar.nodes.val idx0.val node hlt_nd hwas_none
  · simp only [hgt, ↓reduceIte] at hpush
    rcases bind_eq_ok hpush with ⟨v, hv_push, hpush⟩
    rcases Prod.mk.inj (ok_inj hpush) with ⟨rfl, rfl⟩
    have hflen0 : ar.free_len.val = 0 := by
      have hn : ¬ (0 < ar.free_len.val) := hgt
      omega
    have hv_val := vec_push_ok_val hv_push
    have hv_len := vec_push_len_eq hv_push
    have hidx_val : (alloc.vec.Vec.len ar.nodes).val = ar.nodes.val.length := rfl
    refine ⟨⟨hflen_le, by simp [hflen0], fun _ hm => by simp [hflen0] at hm⟩, ?_, ?_, ?_, ?_, ?_⟩
    · intro n hn
      rw [hidx_val, List.getElem?_eq_none (Nat.le_refl _)] at hn; cases hn
    · dsimp only; rw [hv_len, hidx_val]; omega
    · dsimp only; rw [hv_val, hidx_val, List.getElem?_append_right (Nat.le_refl _)]
      simp
    · intro k hk; dsimp only; rw [hv_val]
      by_cases hlt_k : k < ar.nodes.val.length
      · exact List.getElem?_append_left hlt_k
      · have hgt_k : ar.nodes.val.length + 1 ≤ k := by rw [hidx_val] at hk; omega
        rw [List.getElem?_eq_none (by rw [hv_val] at *; simp; omega),
            List.getElem?_eq_none (Nat.not_lt.mp hlt_k)]
    · unfold arenaActiveCount; dsimp only; rw [hv_val, List.filter_append]
      simp [List.filter]

theorem ExplorerArena_complete_child_spec
    {ar ar' : verifier.ExplorerArena}
    {p_idx : Std.Usize} {child_deps : verifier.DataDependencies}
    {p_node : verifier.ExplorerNode}
    {out_opt : Option verifier.ExplorerNode}
    (hwf : ArenaWf ar)
    (hslot : p_idx.val < ar.nodes.val.length ∧ ar.nodes.val[p_idx.val]? = some (some p_node))
    (hpend_pos : 1 ≤ p_node.pending_children.val)
    (hcc : verifier.ExplorerArena.complete_child ar p_idx child_deps = ok (out_opt, ar')) :
    ArenaWf ar' ∧
    (∀ k, k ≠ p_idx.val → ar'.nodes.val[k]? = ar.nodes.val[k]?) ∧
    ar'.nodes.val.length = ar.nodes.val.length ∧
    ∃ dd : verifier.DataDependencies,
      DepsSubset p_node.dependencies dd ∧
      DepsSubset child_deps dd ∧
      ((2 ≤ p_node.pending_children.val ∧
        out_opt = none ∧
        (∃ i : Std.Usize,
          i.val + 1 = p_node.pending_children.val ∧
          1 ≤ i.val ∧
          ar'.nodes.val[p_idx.val]? =
            some (some { p_node with pending_children := i, dependencies := dd })) ∧
        arenaActiveCount ar' = arenaActiveCount ar) ∨
       (p_node.pending_children.val = 1 ∧
        out_opt = some { p_node with pending_children := 0#usize, dependencies := dd } ∧
        ar'.nodes.val[p_idx.val]? = some none ∧
        arenaActiveCount ar' + 1 = arenaActiveCount ar)) := by
  rcases hwf with ⟨hflen_le, hnodup, hfree_none⟩
  rcases hslot with ⟨hp_lt, hp_get⟩
  unfold verifier.ExplorerArena.complete_child at hcc
  rcases bind_eq_ok hcc with ⟨pmb, hmut0, _⟩
  rcases pmb with ⟨o, back0⟩
  rw [bind_pair_ok_eq hmut0] at hcc
  obtain ⟨_, hget0, hset0⟩ := vec_index_mut_usize_ok hmut0
  have ho_eq : o = some p_node := Option.some.inj (hget0.symm.trans hp_get)
  subst ho_eq
  dsimp only at hcc
  rcases bind_eq_ok hcc with ⟨pr, hpr, _⟩
  rcases pr with ⟨ready, o1⟩
  rw [bind_pair_ok_eq hpr] at hcc
  rcases bind_eq_ok hpr with ⟨dd, hmerge, hpr⟩
  obtain ⟨hsub_p, hsub_c⟩ := DataDependencies_merge_subset hmerge
  rcases bind_eq_ok hpr with ⟨i, hsub_i, hpr⟩
  have hi_add := usize_sub_one_ok_val hpend_pos hsub_i
  rcases Prod.mk.inj (ok_inj hpr) with ⟨rfl, rfl⟩
  by_cases hi0 : i = 0#usize
  · simp only [hi0, decide_true, ↓reduceIte] at hcc
    have hp1 : p_node.pending_children.val = 1 := by
      rw [hi0, usize_const_val_0] at hi_add; exact hi_add.symm
    obtain ⟨hv_val, hv_len⟩ := hset0 (some { p_node with pending_children := 0#usize, dependencies := dd })
    rcases bind_eq_ok hcc with ⟨pmb1, hmut1, _⟩
    rcases pmb1 with ⟨o2, back1⟩
    rw [bind_pair_ok_eq hmut1] at hcc
    obtain ⟨hlt_v, hget1, hset1⟩ := vec_index_mut_usize_ok hmut1
    have ho2_eq : o2 = some { p_node with pending_children := 0#usize, dependencies := dd } := by
      have hv_at : (back0 (some { p_node with pending_children := 0#usize, dependencies := dd })).val[p_idx.val]? =
          some (some { p_node with pending_children := 0#usize, dependencies := dd }) := by
        rw [hv_val]; exact list_getElem?_set_eq ar.nodes.val p_idx.val _ hp_lt
      exact Option.some.inj (hget1.symm.trans hv_at)
    subst ho2_eq
    simp only [core.mem.replace, core.option.Option.is_some] at hcc
    obtain ⟨hb1_val, hb1_len⟩ := hset1 none
    have hnodes_set_none : (back1 none).val = ar.nodes.val.set p_idx.val none := by
      rw [hb1_val, hv_val, List.set_set]
    have hp_not_in_free : p_idx ∉ ar.free_indices.val.take ar.free_len.val := by
      intro hmem
      obtain ⟨_, hnone⟩ := hfree_none p_idx hmem
      rw [hp_get] at hnone; cases Option.some.inj hnone
    rcases bind_eq_ok hcc with ⟨⟨v1, v2⟩, hpvv, hcc⟩
    rcases bind_eq_ok hcc with ⟨i1, hadd_fl, hcc⟩
    have hi1_val : i1.val = ar.free_len.val + 1 := by
      simpa [usize_one_val] using (uscalar_add_ok_val hadd_fl).1
    rcases Prod.mk.inj (ok_inj hcc) with ⟨rfl, rfl⟩
    have hv12_spec : v1.val = ar.nodes.val.set p_idx.val none ∧
        i1.val ≤ v2.val.length ∧
        v2.val.take i1.val = ar.free_indices.val.take ar.free_len.val ++ [p_idx] := by
      by_cases hfl_lt : ar.free_len < alloc.vec.Vec.len ar.free_indices
      · simp only [hfl_lt, ↓reduceIte] at hpvv
        rcases bind_eq_ok hpvv with ⟨pfm, hfm, _⟩
        rcases pfm with ⟨_, back2⟩
        rw [bind_pair_ok_eq hfm] at hpvv
        rcases Prod.mk.inj (ok_inj hpvv) with ⟨rfl, rfl⟩
        obtain ⟨hlt_fm, _, hset_fm⟩ := vec_index_mut_usize_ok hfm
        obtain ⟨hfm_val, hfm_len⟩ := hset_fm p_idx
        refine ⟨hnodes_set_none, by rw [hi1_val, hfm_len]; omega, ?_⟩
        rw [hfm_val, hi1_val]
        exact list_take_set_self ar.free_indices.val ar.free_len.val p_idx hlt_fm
      · simp only [hfl_lt, ↓reduceIte] at hpvv
        rcases bind_eq_ok hpvv with ⟨v3, hpush_fi, hpvv⟩
        rcases Prod.mk.inj (ok_inj hpvv) with ⟨rfl, rfl⟩
        have hfl_eq : ar.free_len.val = ar.free_indices.val.length := by
          have hn : ¬ (ar.free_len.val < ar.free_indices.val.length) := hfl_lt
          omega
        have hv3_val := vec_push_ok_val hpush_fi
        have hv3_len := vec_push_len_eq hpush_fi
        refine ⟨hnodes_set_none, by rw [hi1_val, hv3_len]; omega, ?_⟩
        rw [hv3_val, hi1_val, hfl_eq, ← hv3_len, ← hv3_val, List.take_length, hv3_val, List.take_length]
    rcases hv12_spec with ⟨hv1_val, hi1_le, hv2_take⟩
    refine ⟨⟨hi1_le, ?_, ?_⟩, ?_, ?_, dd, hsub_p, hsub_c, Or.inr ⟨hp1, rfl, ?_, ?_⟩⟩
    · dsimp only
      rw [hv2_take, List.nodup_append]
      refine ⟨hnodup, List.nodup_singleton _, fun x hx y hy hxy => ?_⟩
      rw [List.mem_singleton] at hy; subst hy; subst hxy
      exact hp_not_in_free hx
    · intro j hj
      dsimp only at hj ⊢
      rw [hv2_take, List.mem_append, List.mem_singleton] at hj
      rw [hv1_val]
      rcases hj with hj_old | hj_eq
      · obtain ⟨hj_lt, hj_none⟩ := hfree_none j hj_old
        have hj_ne : j.val ≠ p_idx.val := fun hc => hp_not_in_free (usize_eq_of_val_eq hc ▸ hj_old)
        exact ⟨by simp [hj_lt], list_getElem?_set_of_ne ar.nodes.val p_idx.val j.val none hj_ne.symm ▸ hj_none⟩
      · subst j
        exact ⟨by simp [hp_lt], list_getElem?_set_eq ar.nodes.val p_idx.val none hp_lt⟩
    · intro k hk; dsimp only; rw [hv1_val]
      exact list_getElem?_set_of_ne ar.nodes.val p_idx.val k none hk.symm
    · dsimp only; rw [hv1_val, List.length_set]
    · dsimp only; rw [hv1_val]
      exact list_getElem?_set_eq ar.nodes.val p_idx.val none hp_lt
    · unfold arenaActiveCount; dsimp only; rw [hv1_val]
      exact filter_isSome_set_some_to_none ar.nodes.val p_idx.val p_node hp_lt hp_get
  · simp only [hi0, decide_false, Bool.false_eq_true, ↓reduceIte] at hcc
    rcases Prod.mk.inj (ok_inj hcc) with ⟨rfl, rfl⟩
    have hi_pos : 1 ≤ i.val := by
      by_contra hlt1
      apply hi0; apply usize_eq_of_val_eq
      rw [usize_const_val_0]; omega
    obtain ⟨hv_val, hv_len⟩ := hset0 (some { p_node with pending_children := i, dependencies := dd })
    refine ⟨⟨hflen_le, hnodup, ?_⟩, ?_, ?_, dd, hsub_p, hsub_c,
      Or.inl ⟨by omega, rfl, ⟨i, hi_add, hi_pos, ?_⟩, ?_⟩⟩
    · intro j hj
      obtain ⟨hj_lt, hj_none⟩ := hfree_none j hj
      have hj_ne : j.val ≠ p_idx.val := by
        intro hc; rw [hc, hp_get] at hj_none; cases Option.some.inj hj_none
      refine ⟨by dsimp only; rw [hv_len]; exact hj_lt, ?_⟩
      dsimp only; rw [hv_val, list_getElem?_set_of_ne ar.nodes.val p_idx.val j.val _ hj_ne.symm]
      exact hj_none
    · intro k hk; dsimp only; rw [hv_val]
      exact list_getElem?_set_of_ne ar.nodes.val p_idx.val k _ hk.symm
    · dsimp only; exact hv_len
    · dsimp only; rw [hv_val]
      exact list_getElem?_set_eq ar.nodes.val p_idx.val _ hp_lt
    · unfold arenaActiveCount; dsimp only; rw [hv_val]
      exact filter_isSome_set_some_to_some ar.nodes.val p_idx.val p_node _ hp_lt hp_get

/-! ### 3.2 `DepTracker` Context Frame & Pass 1 (`verify_program_core_loop5_visited_spec`) -/

theorem DepTracker_visit_vctx_eq
    {trk trk' : verifier.DependencyTracker}
    {vctx vctx' : verifier.VerificationContext}
    {inst : api.EbpfInstruction}
    (h : visitor.BpfVisitor.visit.default DepTrackerVisitorInst trk vctx inst =
      ok (.Ok (), trk', vctx')) :
    vctx' = vctx := by
  obtain ⟨_, hcases⟩ := visit_default_ok_spec DepTrackerVisitorInst trk trk' vctx vctx' inst h
  rcases hcases with ⟨_, hl64⟩ | ⟨_, insn, _, hdisp⟩
  · dsimp [DepTrackerVisitorInst] at hl64
    unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.load64 at hl64
    rcases bind_eq_ok hl64 with ⟨p, hrem, _⟩
    rcases p with ⟨_, _⟩
    rw [bind_pair_ok_eq hrem] at hl64
    exact (Prod.mk.inj (Prod.mk.inj (ok_inj hl64)).2).2.symm
  · cases insn with
    | alu64 op _ _ =>
      rcases hdisp with ⟨_, _, _, _, _, hcall⟩
      have hc := hcall.symm
      cases op <;> (
        dsimp [alu64Method, DepTrackerVisitorInst,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mov64,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.add64,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.sub64,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mul64,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.div64,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.or64,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.and64,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.lsh64,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.rsh64,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mod64,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.xor64,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.arsh64] at hc
        first
        | exact (DependencyTracker_mov_ok_spec hc).2.2.1
        | (rcases bind_eq_ok hc with ⟨p, hp, _⟩
           rcases p with ⟨_, _⟩
           rw [bind_pair_ok_eq hp] at hc
           exact (Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2).2.symm))
    | alu32 op _ _ =>
      rcases hdisp with ⟨_, _, _, _, _, hcall⟩
      have hc := hcall.symm
      cases op <;> (
        dsimp [alu32Method, DepTrackerVisitorInst,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.add,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.sub,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mul,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.div,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.or,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.and,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.lsh,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.rsh,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mod,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.xor,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.arsh] at hc
        first
        | exact (DependencyTracker_mov_ok_spec hc).2.2.1
        | (rcases bind_eq_ok hc with ⟨p, hp, _⟩
           rcases p with ⟨_, _⟩
           rw [bind_pair_ok_eq hp] at hc
           exact (Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2).2.symm))
    | neg is64 _ =>
      rcases hdisp with ⟨_, _, hcall⟩
      have hc := hcall.symm
      cases is64 <;> {
        dsimp [DepTrackerVisitorInst,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.neg64,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.neg] at hc
        exact (Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2).2.symm
      }
    | endian doSwap _ _ =>
      rcases hdisp with ⟨_, _, hcall⟩
      have hc := hcall.symm
      cases doSwap <;> {
        dsimp [DepTrackerVisitorInst,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.be,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.le] at hc
        rcases bind_eq_ok hc with ⟨_, _, hc⟩
        exact (Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2).2.symm
      }
    | jump _ =>
      rcases hdisp with ⟨_, _, hcall⟩
      have hc := hcall.symm
      dsimp [DepTrackerVisitorInst,
        verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.jump] at hc
      exact (Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2).2.symm
    | exit =>
      have hc := hdisp.symm
      dsimp [VisitorDispatchNonLddw, DepTrackerVisitorInst,
        verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.exit] at hc
      rcases bind_eq_ok hc with ⟨_, _, hc⟩
      exact (Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2).2.symm
    | condJump is64 op _ _ _ =>
      rcases hdisp with ⟨_, _, _, _, _, _, hcall⟩
      have hc := hcall.symm
      rw [condJumpMethod_depTracker_eq] at hc
      rcases bind_eq_ok hc with ⟨p, hp, _⟩
      rcases p with ⟨_, _⟩
      rw [bind_pair_ok_eq hp] at hc
      exact (Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2).2.symm
    | load _ _ _ _ =>
      rcases hdisp with ⟨_, _, _, _, _, _, hcall⟩
      have hc := hcall.symm
      dsimp [DepTrackerVisitorInst] at hc
      unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.load at hc
      rcases bind_eq_ok hc with ⟨p, hp, _⟩
      rcases p with ⟨_, _⟩
      rw [bind_pair_ok_eq hp] at hc
      rcases bind_eq_ok hc with ⟨r, _, hc⟩
      cases r with
      | Err _ => cases (Prod.mk.inj (ok_inj hc)).1
      | Ok _ =>
        dsimp only at hc
        rcases bind_eq_ok hc with ⟨r2, _, hc⟩
        cases r2 with
        | Err _ => cases (Prod.mk.inj (ok_inj hc)).1
        | Ok a =>
          dsimp only at hc
          cases a with
          | PtrToStack _ =>
            dsimp only at hc
            rcases bind_eq_ok hc with ⟨_, _, hc⟩
            rcases bind_eq_ok hc with ⟨r3, _, hc⟩
            cases r3 with
            | Err _ => exact (Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2).2.symm
            | Ok _ =>
              dsimp only at hc
              rcases bind_eq_ok hc with ⟨_, _, hc⟩
              rcases bind_eq_ok hc with ⟨_, _, hc⟩
              rcases bind_eq_ok hc with ⟨_, _, hc⟩
              exact (Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2).2.symm
          | _ =>
            dsimp only at hc
            rcases bind_eq_ok hc with ⟨_, _, hc⟩
            exact (Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2).2.symm
    | store _ _ _ _ =>
      rcases hdisp with ⟨_, _, vsrc, _, _, _, hcall⟩
      have hc := hcall.symm
      dsimp [DepTrackerVisitorInst] at hc
      unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.store at hc
      rcases bind_eq_ok hc with ⟨r, _, hc⟩
      cases r with
      | Err _ => cases (Prod.mk.inj (ok_inj hc)).1
      | Ok _ =>
        dsimp only at hc
        rcases bind_eq_ok hc with ⟨r2, _, hc⟩
        cases r2 with
        | Err _ => cases (Prod.mk.inj (ok_inj hc)).1
        | Ok a =>
          dsimp only at hc
          rcases bind_eq_ok hc with ⟨_, _, hc⟩
          cases a with
          | PtrToStack _ =>
            dsimp only at hc
            rcases bind_eq_ok hc with ⟨_, _, hc⟩
            rcases bind_eq_ok hc with ⟨r3, _, hc⟩
            cases r3 with
            | Err _ => exact (Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2).2.symm
            | Ok _ =>
              dsimp only at hc
              rcases bind_eq_ok hc with ⟨_, _, hc⟩
              rcases bind_eq_ok hc with ⟨psl, hsl, _⟩
              rcases psl with ⟨_, src_live⟩
              rw [bind_pair_ok_eq hsl] at hc
              cases src_live <;> simp only [Bool.false_eq_true, ↓reduceIte] at hc
              · exact (Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2).2.symm
              · cases vsrc with
                | Reg _ =>
                  dsimp only at hc
                  rcases bind_eq_ok hc with ⟨_, _, hc⟩
                  exact (Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2).2.symm
                | Value _ =>
                  dsimp only at hc
                  exact (Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2).2.symm
          | _ =>
            dsimp only at hc
            cases vsrc with
            | Reg _ =>
              dsimp only at hc
              rcases bind_eq_ok hc with ⟨_, _, hc⟩
              exact (Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2).2.symm
            | Value _ =>
              dsimp only at hc
              exact (Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2).2.symm
    | atomic is64 op _ _ _ =>
      rcases hdisp with ⟨_, _, _, _, _, _, hcall⟩
      have hc := hcall.symm
      cases is64 <;> cases op <;> (
        dsimp [atomicMethod, DepTrackerVisitorInst,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_add64,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_add,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_or64,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_or,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_and64,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_and,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_xor64,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_xor,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_xchg64,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_xchg,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_cmpxchg64,
          verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_cmpxchg] at hc
        rcases bind_eq_ok hc with ⟨p, hp, _⟩
        rcases p with ⟨_, _⟩
        rw [bind_pair_ok_eq hp] at hc
        exact (Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2).2.symm)
    | loadPacket _ _ _ =>
      rcases hdisp with ⟨_, _, ⟨_, hcall⟩ | ⟨_, _, hcall⟩⟩
      · have hc := hcall.symm
        dsimp [DepTrackerVisitorInst] at hc
        unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.load_from_packet at hc
        simp only [lift_eq_ok, bind_ok] at hc
        rcases bind_eq_ok hc with ⟨p, hp, _⟩
        rcases p with ⟨_, _⟩
        rw [bind_pair_ok_eq hp] at hc
        rcases bind_eq_ok hc with ⟨_, _, hc⟩
        exact (Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2).2.symm
      · have hc := hcall.symm
        dsimp [DepTrackerVisitorInst] at hc
        unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.load_from_packet at hc
        simp only [lift_eq_ok, bind_ok] at hc
        rcases bind_eq_ok hc with ⟨p, hp, _⟩
        rcases p with ⟨_, _⟩
        rw [bind_pair_ok_eq hp] at hc
        rcases bind_eq_ok hc with ⟨_, _, hc⟩
        rcases bind_eq_ok hc with ⟨_, _, hc⟩
        exact (Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2).2.symm
    | callExternal _ =>
      rcases hdisp with ⟨_, _, hcall⟩
      have hc := hcall.symm
      dsimp [DepTrackerVisitorInst] at hc
      unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.call_external at hc
      rcases bind_eq_ok hc with ⟨o, _, hc⟩
      cases o with
      | none => cases (Prod.mk.inj (ok_inj hc)).1
      | some _ =>
        dsimp only at hc
        simp only [lift_eq_ok, bind_ok] at hc
        split at hc
        · rcases bind_eq_ok hc with ⟨_, _, hc⟩
          cases (Prod.mk.inj (ok_inj hc)).1
        · rcases bind_eq_ok hc with ⟨pl, hl, _⟩
          rcases pl with ⟨_, lerr⟩
          rw [bind_pair_ok_eq hl] at hc
          cases lerr with
          | some _ => cases (Prod.mk.inj (ok_inj hc)).1
          | none =>
            dsimp only at hc
            rcases bind_eq_ok hc with ⟨_, _, hc⟩
            rcases bind_eq_ok hc with ⟨_, _, hc⟩
            rcases bind_eq_ok hc with ⟨_, _, hc⟩
            exact (Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2).2.symm
    | lddwImm _ _ | lddwMap _ _ | lddwMapValue _ _ _ | lddwCont =>
      exact False.elim hdisp

theorem terminate_context_chain_vctx_frame
    {cur : verifier.ComputationContext} {deps : verifier.DataDependencies}
    {pidx : Option Std.Usize} {ar ar' : verifier.ExplorerArena}
    {vctx vctx' : verifier.VerificationContext}
    (h : verifier.terminate_context_chain cur deps pidx ar vctx = ok (.Ok (), ar', vctx')) :
    vctx'.calling_context = vctx.calling_context ∧
    vctx'.code = vctx.code ∧
    vctx'.counter = vctx.counter ∧
    vctx'.iteration = vctx.iteration ∧
    vctx'.terminating_contexts.val.length = vctx.terminating_contexts.val.length := by
  unfold verifier.terminate_context_chain at h
  rcases bind_eq_ok h with ⟨pt, hloop, _⟩
  rcases pt with ⟨ar1, vc1, lerr⟩
  rw [bind_triple_ok_eq hloop] at h
  cases lerr with
  | some _ => cases (Prod.mk.inj (ok_inj h)).1
  | none =>
    rcases Prod.mk.inj (Prod.mk.inj (ok_inj h)).2 with ⟨rfl, rfl⟩
    unfold verifier.terminate_context_chain_loop at hloop
    have hmain := loop_ok_pre_post
      (fun (cd1, pi1, a1, vc_in, co1, le1) =>
        verifier.terminate_context_chain_loop.body cd1 pi1 a1 vc_in co1 le1)
      (fun p =>
        p.2.2.2.2.2 = none →
          p.2.2.2.1.calling_context = vctx.calling_context ∧
          p.2.2.2.1.code = vctx.code ∧
          p.2.2.2.1.counter = vctx.counter ∧
          p.2.2.2.1.iteration = vctx.iteration ∧
          p.2.2.2.1.terminating_contexts.val.length = vctx.terminating_contexts.val.length)
      (fun p out =>
        (p.2.2.2.2.2 ≠ none → out.2.2 ≠ none) ∧
        (out.2.2 = none →
          out.2.1.calling_context = vctx.calling_context ∧
          out.2.1.code = vctx.code ∧
          out.2.1.counter = vctx.counter ∧
          out.2.1.iteration = vctx.iteration ∧
          out.2.1.terminating_contexts.val.length = vctx.terminating_contexts.val.length))
      (by
        rintro ⟨cd1, pi1, a1, vc_in, co1, le1⟩ ⟨cd2, pi2, a2, vc_nx, co2, le2⟩ ⟨ao, vco, leo⟩ hpre hbody
        dsimp only at hpre ⊢
        unfold verifier.terminate_context_chain_loop.body at hbody
        by_cases hb : core.option.Option.is_some co1 = true
        · simp only [hb, ↓reduceIte] at hbody
          by_cases hb1 : core.option.Option.is_none le1 = true
          · simp only [hb1, ↓reduceIte] at hbody
            have hle1 : le1 = none := by
              cases le1 <;> simp [core.option.Option.is_none] at hb1; rfl
            subst hle1
            have hpre0 := hpre rfl
            cases co1 with
            | none => simp [core.option.Option.is_some] at hb
            | some cur_in =>
              simp only [core.mem.replace] at hbody
              rcases bind_eq_ok hbody with ⟨inst, _, hbody⟩
              rcases bind_eq_ok hbody with ⟨pv, hvis, _⟩
              rcases pv with ⟨r_vis, tracker, vc_vis⟩
              rw [bind_triple_ok_eq hvis] at hbody
              cases r_vis with
              | Err _ =>
                cases ok_inj hbody
                refine ⟨(fun h => by cases h), ?_⟩
                intro hIH
                exact ⟨fun h => False.elim (h rfl), fun hleo => False.elim ((hIH.1 (by simp)) hleo)⟩
              | Ok _ =>
                have hv_eq := DepTracker_visit_vctx_eq hvis
                subst vc_vis
                rcases bind_eq_ok hbody with ⟨pmb, hmut_tc, _⟩
                rcases pmb with ⟨o_tc, back_tc⟩
                rw [bind_pair_ok_eq hmut_tc] at hbody
                obtain ⟨_, _, hset_tc⟩ := vec_index_mut_usize_ok hmut_tc
                rcases bind_eq_ok hbody with ⟨pvn, hpvn, _⟩
                rcases pvn with ⟨v_tc, next_deps⟩
                rw [bind_pair_ok_eq hpvn] at hbody
                have htc_len : v_tc.val.length = vc_in.terminating_contexts.val.length := by
                  cases o_tc with
                  | none =>
                    rcases Prod.mk.inj (ok_inj hpvn) with ⟨rfl, _⟩
                    exact (hset_tc none).2
                  | some tc =>
                    dsimp only at hpvn
                    rcases bind_eq_ok hpvn with ⟨_, _, hpvn⟩
                    rcases bind_eq_ok hpvn with ⟨_, _, hpvn⟩
                    rcases bind_eq_ok hpvn with ⟨tc1, _, hpvn⟩
                    rcases Prod.mk.inj (ok_inj hpvn) with ⟨rfl, _⟩
                    exact (hset_tc (some tc1)).2
                cases pi1 with
                | none =>
                  cases ok_inj hbody
                  exact ⟨fun _ => ⟨hpre0.1, hpre0.2.1, hpre0.2.2.1, hpre0.2.2.2.1, htc_len.trans hpre0.2.2.2.2⟩,
                    fun hIH => hIH⟩
                | some p_idx =>
                  dsimp only at hbody
                  rcases bind_eq_ok hbody with ⟨pcc, hcc, _⟩
                  rcases pcc with ⟨o2, _⟩
                  rw [bind_pair_ok_eq hcc] at hbody
                  cases o2 <;> cases ok_inj hbody <;>
                    exact ⟨fun _ => ⟨hpre0.1, hpre0.2.1, hpre0.2.2.1, hpre0.2.2.2.1, htc_len.trans hpre0.2.2.2.2⟩,
                      fun hIH => hIH⟩
          · simp only [hb1, Bool.false_eq_true, ↓reduceIte] at hbody
            cases ok_inj hbody
        · simp only [hb, Bool.false_eq_true, ↓reduceIte] at hbody
          cases ok_inj hbody)
      (by
        rintro ⟨cd1, pi1, a1, vc_in, co1, le1⟩ ⟨ao, vco, leo⟩ hpre hbody
        dsimp only at hpre ⊢
        unfold verifier.terminate_context_chain_loop.body at hbody
        by_cases hb : core.option.Option.is_some co1 = true
        · simp only [hb, ↓reduceIte] at hbody
          by_cases hb1 : core.option.Option.is_none le1 = true
          · simp only [hb1, ↓reduceIte] at hbody
            cases co1 with
            | none => simp [core.option.Option.is_some] at hb
            | some _ =>
              simp only [core.mem.replace] at hbody
              rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
              rcases bind_eq_ok hbody with ⟨pv, hvis, _⟩
              rcases pv with ⟨r_vis, _, _⟩
              rw [bind_triple_ok_eq hvis] at hbody
              cases r_vis with
              | Err _ => cases ok_inj hbody
              | Ok _ =>
                dsimp only at hbody
                rcases bind_eq_ok hbody with ⟨pmb, hmut_tc, _⟩
                rcases pmb with ⟨_, _⟩
                rw [bind_pair_ok_eq hmut_tc] at hbody
                rcases bind_eq_ok hbody with ⟨pvn, hpvn, _⟩
                rcases pvn with ⟨_, _⟩
                rw [bind_pair_ok_eq hpvn] at hbody
                cases pi1 with
                | none => cases ok_inj hbody
                | some _ =>
                  dsimp only at hbody
                  rcases bind_eq_ok hbody with ⟨pcc, hcc, _⟩
                  rcases pcc with ⟨o2, _⟩
                  rw [bind_pair_ok_eq hcc] at hbody
                  cases o2 <;> cases ok_inj hbody
          · simp only [hb1, Bool.false_eq_true, ↓reduceIte] at hbody
            cases ok_inj hbody
            have hne_le : le1 ≠ none := by
              intro hc; subst hc; simp [core.option.Option.is_none] at hb1
            exact ⟨fun _ => hne_le, fun hleo => False.elim (hne_le hleo)⟩
        · simp only [hb, Bool.false_eq_true, ↓reduceIte] at hbody
          cases ok_inj hbody
          exact ⟨id, hpre⟩)
      (deps, pidx, ar, vctx, some cur, none) (ar1, vc1, none)
      (fun _ => ⟨rfl, rfl, rfl, rfl, rfl⟩) hloop
    exact hmain.2 rfl

/-- Raw-instruction validity predicate at visited PCs needed by `verify_program_core_loop6_zero_spec`. -/
def VisitedValid (code : List api.EbpfInstruction) (vis : List Bool) : Prop :=
  ∀ k, k < code.length → vis[k]? = some true →
    ∃ raw, code[k]? = some raw ∧
      raw.bv.setWidth 8 ≠ 0#8 ∧
      (raw.bv.setWidth 8 ≠ 0x18#8 →
        ∃ insn, insn ≠ .lddwCont ∧ decodeSingleInsn raw = some insn)

theorem error_and_log_is_err {T L : Type}
    {inst : verifier.CoreVerifierLogger L} {logger logger' : L}
    {err : error.VerifierError} {res : core.result.Result T error.VerifierError}
    (h : verifier.error_and_log T inst logger err = ok (res, logger')) :
    res = .Err err := by
  unfold verifier.error_and_log at h
  rcases bind_eq_ok h with ⟨_, _, h⟩
  exact (Prod.mk.inj (ok_inj h)).1.symm

private theorem pending_log_step_eq {L α : Type}
    (loggerInst : verifier.CoreVerifierLogger L)
    (pl : Option (Std.Usize × Std.U8 × verifier.VerifierLogEvent))
    (vis_next : alloc.vec.Vec Bool) (logger : L)
    (k : alloc.vec.Vec Bool → L → Option (Std.Usize × Std.U8 × verifier.VerifierLogEvent) → Result α)
    {out : α}
    (h : (do
            let (o, o1) := core.mem.replace pl none
            let (visited1, logger1) ←
              match o with
              | none =>
                let visited2 := vis_next
                ok (visited2, logger)
              | some t => do
                let (log_pc, opcode, log_ev) := t
                let logger2 ← loggerInst.log_instruction logger log_pc opcode log_ev
                let visited2 := vis_next
                ok (visited2, logger2)
            k visited1 logger1 o1) = ok out) :
    ∃ logger1, k vis_next logger1 none = ok out := by
  cases pl with
  | none =>
    dsimp [core.mem.replace] at h
    rcases bind_eq_ok h with ⟨p, hp, hk⟩
    cases ok_inj hp
    exact ⟨logger, hk⟩
  | some t =>
    rcases t with ⟨lp, op, le⟩
    dsimp [core.mem.replace] at h
    rcases bind_eq_ok h with ⟨p, hp, hk⟩
    rcases bind_eq_ok hp with ⟨lg2, _, hp⟩
    cases ok_inj hp
    exact ⟨lg2, hk⟩

/-- Pass 1 of `verify_program_core_loop5`: when `verify_err = none`, `calling_context` and
    `code` are preserved and `visitedOut` satisfies `VisitedValid` for `loop6`. -/
theorem verify_program_core_loop5_visited_spec
    {L : Type} (loggerInst : verifier.CoreVerifierLogger L)
    {env : ExecEnv} {cctx : verifier.CoreCallingContext}
    {code : alloc.vec.Vec api.EbpfInstruction}
    {logger loggerOut : L}
    {vis visOut : alloc.vec.Vec Bool}
    {wl : verifier.Worklist} {es0 : List verifier.WorklistEntry}
    {ar : verifier.ExplorerArena}
    {vctx vctxOut : verifier.VerificationContext}
    (hcctx : CallingContextFullMatches cctx env)
    (hvc_cctx : vctx.calling_context = cctx)
    (hvc_code : vctx.code = code)
    (hvis_len : vis.val.length = code.val.length)
    (hvis_valid : VisitedValid code.val vis.val)
    (hwl_rep : WorklistRep wl es0)
    (hwl_valid : ∀ e ∈ es0, CompCtxValid env vctx.counter.bv.toNat e.context)
    (hloop : verifier.verify_program_core_loop5 loggerInst logger vis wl ar vctx none false =
      ok (loggerOut, visOut, vctxOut, none)) :
    vctxOut.calling_context = cctx ∧
    vctxOut.code = code ∧
    visOut.val.length = code.val.length ∧
    VisitedValid code.val visOut.val := by
  unfold verifier.verify_program_core_loop5 at hloop
  have hmain := loop_ok_pre_post
    (fun (l1, v1, w1, a1, vc1, e1, d1) =>
      verifier.verify_program_core_loop5.body loggerInst l1 v1 w1 a1 vc1 e1 d1)
    (fun s =>
      s.2.2.2.2.2.1 = none →
        s.2.2.2.2.1.calling_context = cctx ∧
        s.2.2.2.2.1.code = code ∧
        s.2.1.val.length = code.val.length ∧
        VisitedValid code.val s.2.1.val ∧
        ∃ es, WorklistRep s.2.2.1 es ∧
          ∀ e ∈ es, CompCtxValid env s.2.2.2.2.1.counter.bv.toNat e.context)
    (fun s out =>
      (s.2.2.2.2.2.1 ≠ none → out.2.2.2 ≠ none) ∧
      (out.2.2.2 = none →
        out.2.2.1.calling_context = cctx ∧
        out.2.2.1.code = code ∧
        out.2.1.val.length = code.val.length ∧
        VisitedValid code.val out.2.1.val))
    (by
      rintro ⟨l1, v1, w1, a1, vc1, e1, d1⟩ ⟨l2, v2, w2, a2, vc2, e2, d2⟩ ⟨lo, vo, vco, eo⟩ hpre hbody
      dsimp only at hpre ⊢
      unfold verifier.verify_program_core_loop5.body at hbody
      by_cases hd : d1 = true
      · simp only [hd, ↓reduceIte] at hbody; cases ok_inj hbody
      · simp only [hd, Bool.false_eq_true, ↓reduceIte] at hbody
        by_cases he : core.option.Option.is_none e1 = true
        · simp only [he, ↓reduceIte] at hbody
          have he1_none : e1 = none := by
            cases e1 <;> simp [core.option.Option.is_none] at he; rfl
          subst he1_none
          obtain ⟨hcc1, hcd1, hvlen1, hvv1, es, hrep1, hes_v1⟩ := hpre rfl
          rcases bind_eq_ok hbody with ⟨⟨o_pop, wl1⟩, hpop, hbody⟩
          rcases Worklist_pop_spec hrep1 hpop with ⟨_, rfl, hrep_empty⟩ | ⟨es_init, entry, rfl, rfl, hrep_init⟩
          · cases ok_inj hbody
            exact ⟨fun _ => ⟨hcc1, hcd1, hvlen1, hvv1, [], hrep_empty, fun _ hm => by cases hm⟩,
              fun hIH => hIH⟩
          · have hentry_v : CompCtxValid env vc1.counter.bv.toNat entry.context :=
              hes_v1 entry (by simp)
            have hinit_v : ∀ e ∈ es_init, CompCtxValid env vc1.counter.bv.toNat e.context :=
              fun e he => hes_v1 e (List.mem_append_left _ he)
            rcases bind_eq_ok hbody with ⟨⟨r_w, wl2, ar1, vis1, vc_next, lg1⟩, hw, hbody⟩
            cases r_w with
            | Err err =>
              cases ok_inj hbody
              exact ⟨(fun h => by cases h),
                fun hIH => ⟨fun h => False.elim (h rfl), fun heo => False.elim ((hIH.1 (by simp)) heo)⟩⟩
            | Ok _ =>
              cases ok_inj hbody
              refine ⟨fun _ => ?_, fun hIH => hIH⟩
              unfold verifier.verify_worklist_entry at hw
              rcases bind_eq_ok hw with ⟨pruned_deps, _, hw⟩
              cases pruned_deps with
              | some _ =>
                dsimp only at hw
                cases hp_e : entry.parent with
                | none =>
                  rw [hp_e] at hw
                  dsimp only at hw
                  rcases Prod.mk.inj (ok_inj hw) with ⟨_, hp⟩
                  rcases Prod.mk.inj hp with ⟨hwl_eq, hp⟩
                  rcases Prod.mk.inj hp with ⟨_, hp⟩
                  rcases Prod.mk.inj hp with ⟨hv_eq, hp⟩
                  rcases Prod.mk.inj hp with ⟨hvc_eq, _⟩
                  exact ⟨hvc_eq ▸ hcc1, hvc_eq ▸ hcd1, hv_eq ▸ hvlen1, hv_eq ▸ hvv1,
                    es_init, hwl_eq ▸ hrep_init, hvc_eq ▸ hinit_v⟩
                | some p_idx =>
                  rw [hp_e] at hw
                  dsimp only at hw
                  rcases bind_eq_ok hw with ⟨pcc, hcc, _⟩
                  rcases pcc with ⟨o_p, ar_mid⟩
                  rw [bind_pair_ok_eq hcc] at hw
                  cases o_p with
                  | none =>
                    dsimp only at hw
                    rcases Prod.mk.inj (ok_inj hw) with ⟨_, hp⟩
                    rcases Prod.mk.inj hp with ⟨hwl_eq, hp⟩
                    rcases Prod.mk.inj hp with ⟨_, hp⟩
                    rcases Prod.mk.inj hp with ⟨hv_eq, hp⟩
                    rcases Prod.mk.inj hp with ⟨hvc_eq, _⟩
                    exact ⟨hvc_eq ▸ hcc1, hvc_eq ▸ hcd1, hv_eq ▸ hvlen1, hv_eq ▸ hvv1,
                      es_init, hwl_eq ▸ hrep_init, hvc_eq ▸ hinit_v⟩
                  | some parent_node =>
                    dsimp only at hw
                    rcases bind_eq_ok hw with ⟨ptc, htc, _⟩
                    rcases ptc with ⟨r_tc, ar_out, vc_tc⟩
                    rw [bind_triple_ok_eq htc] at hw
                    cases r_tc with
                    | Err err =>
                      dsimp only at hw
                      rcases bind_eq_ok hw with ⟨pel, hel, _⟩
                      rcases pel with ⟨r1, _⟩
                      rw [bind_pair_ok_eq hel] at hw
                      have hr1 := error_and_log_is_err hel
                      subst hr1
                      cases (Prod.mk.inj (ok_inj hw)).1
                    | Ok _ =>
                      dsimp only at hw
                      rcases Prod.mk.inj (ok_inj hw) with ⟨_, hp⟩
                      rcases Prod.mk.inj hp with ⟨hwl_eq, hp⟩
                      rcases Prod.mk.inj hp with ⟨_, hp⟩
                      rcases Prod.mk.inj hp with ⟨hv_eq, hp⟩
                      rcases Prod.mk.inj hp with ⟨hvc_eq, _⟩
                      obtain ⟨hf1, hf2, hf3, _, _⟩ := terminate_context_chain_vctx_frame htc
                      exact ⟨hvc_eq ▸ hf1.trans hcc1, hvc_eq ▸ hf2.trans hcd1,
                        hv_eq ▸ hvlen1, hv_eq ▸ hvv1, es_init, hwl_eq ▸ hrep_init,
                        by rw [← hvc_eq, hf3]; exact hinit_v⟩
              | none =>
                dsimp only at hw
                rcases bind_eq_ok hw with ⟨ilim, _, hw⟩
                split at hw
                · rcases bind_eq_ok hw with ⟨pel, hel, _⟩
                  rcases pel with ⟨r1, _⟩
                  rw [bind_pair_ok_eq hel] at hw
                  have hr1 := error_and_log_is_err hel
                  subst hr1
                  cases (Prod.mk.inj (ok_inj hw)).1
                · simp only [lift_eq_ok, bind_ok] at hw
                  split at hw
                  · rcases bind_eq_ok hw with ⟨⟨r1, _⟩, hel, hw⟩
                    cases (Prod.mk.inj (ok_inj hw)).1 ▸ error_and_log_is_err hel
                  · split at hw
                    · rcases bind_eq_ok hw with ⟨⟨r1, _⟩, hel, hw⟩
                      cases (Prod.mk.inj (ok_inj hw)).1 ▸ error_and_log_is_err hel
                    · split at hw
                      · rcases bind_eq_ok hw with ⟨⟨r1, _⟩, hel, hw⟩
                        cases (Prod.mk.inj (ok_inj hw)).1 ▸ error_and_log_is_err hel
                      · split at hw
                        · rcases bind_eq_ok hw with ⟨pel, hel, _⟩
                          rcases pel with ⟨r1, _⟩
                          rw [bind_pair_ok_eq hel] at hw
                          have hr1 := error_and_log_is_err hel
                          subst hr1
                          cases (Prod.mk.inj (ok_inj hw)).1
                        · rcases bind_eq_ok hw with ⟨pmb, hmut_vis, _⟩
                          rcases pmb with ⟨_, back_vis⟩
                          rw [bind_pair_ok_eq hmut_vis] at hw
                          obtain ⟨hpc_lt_vis, _, hset_vis⟩ := vec_index_mut_usize_ok hmut_vis
                          obtain ⟨hvis_set_val, hvis_set_len⟩ := hset_vis true
                          rcases bind_eq_ok hw with ⟨b_en, _, hw⟩
                          rcases bind_eq_ok hw with ⟨inst, hinst_idx, hw⟩
                          obtain ⟨hpc_lt_cd, hinst_at⟩ := vec_index_usize_ok hinst_idx
                          rcases bind_eq_ok hw with ⟨pvis, hvisit, _⟩
                          rcases pvis with ⟨visit_res, ctx_after, vc_vis⟩
                          rw [bind_triple_ok_eq hvisit] at hw
                          obtain ⟨logger_mid, hw⟩ :=
                            pending_log_step_eq loggerInst vc_vis.pending_log (back_vis true) l1 _ hw
                          cases visit_res with
                          | Err _ =>
                            dsimp only at hw
                            rcases bind_eq_ok hw with ⟨pel, hel, _⟩
                            rcases pel with ⟨r1, _⟩
                            rw [bind_pair_ok_eq hel] at hw
                            have hr1 := error_and_log_is_err hel
                            subst hr1
                            cases (Prod.mk.inj (ok_inj hw)).1
                          | Ok _ =>
                            dsimp only at hw
                            rcases bind_eq_ok hw with ⟨i_pend, _, hw⟩
                            split at hw
                            · rcases bind_eq_ok hw with ⟨⟨r1, _⟩, hel, hw⟩
                              cases (Prod.mk.inj (ok_inj hw)).1 ▸ error_and_log_is_err hel
                            · let vctx_f0 : verifier.VerificationContext :=
                                { vc1 with
                                  log_enabled := b_en
                                  pending_log := none
                                  pending_successors := alloc.vec.Vec.new (Option verifier.ComputationContext) }
                              obtain ⟨rfl, hpost, _, hnez, hdec_wit⟩ :=
                                visit_any_forward (vctx0 := vctx_f0) (by simpa [vctx_f0, hcc1] using hcctx)
                                  hentry_v rfl hvisit
                              have hvv_next : VisitedValid code.val (back_vis true).val := by
                                intro k hk hk_true
                                rw [hvis_set_val] at hk_true
                                by_cases hk_eq : k = entry.context.pc.val
                                · subst hk_eq
                                  rw [hcd1] at hinst_at
                                  exact ⟨inst, hinst_at, hnez, hdec_wit⟩
                                · rw [list_getElem?_set_of_ne v1.val entry.context.pc.val k true
                                    (Ne.symm hk_eq)] at hk_true
                                  exact hvv1 k hk hk_true
                              have hinit_v_vis : ∀ e ∈ es_init, CompCtxValid env vc_vis.counter.bv.toNat e.context :=
                                fun e he => CompCtxValid_mono hpost.2.2.2.2.1 (hinit_v e he)
                              by_cases hns : alloc.vec.Vec.len vc_vis.pending_successors > 0#usize
                              · simp only [hns, ↓reduceIte] at hw
                                rw [bind_ok_eq DataDependencies_default_zero] at hw
                                rcases bind_eq_ok hw with ⟨par, har_push, _⟩
                                rcases par with ⟨node_idx, ar_p⟩
                                rw [bind_pair_ok_eq har_push] at hw
                                rcases bind_eq_ok hw with ⟨wl_p, hwl_loop, hw⟩
                                rcases bind_eq_ok hw with ⟨i1, _, hw⟩
                                rcases Prod.mk.inj (ok_inj hw) with ⟨_, hp⟩
                                rcases Prod.mk.inj hp with ⟨hwl_eq, hp⟩
                                rcases Prod.mk.inj hp with ⟨_, hp⟩
                                rcases Prod.mk.inj hp with ⟨hv_eq, hp⟩
                                rcases Prod.mk.inj hp with ⟨hvc_eq, _⟩
                                rw [← hwl_eq, ← hv_eq, ← hvc_eq]
                                obtain ⟨succs, hsuccs_eq, hsuccs_v⟩ :=
                                  opt_list_eq_map_some vc_vis.pending_successors.val hpost.2.2.2.2.2.2
                                have hnum_eq : (alloc.vec.Vec.len vc_vis.pending_successors).val = succs.length := by
                                  change vc_vis.pending_successors.val.length = succs.length
                                  rw [hsuccs_eq, List.length_map]
                                have hrep_p := verify_worklist_entry_loop_spec hrep_init hsuccs_eq hnum_eq hwl_loop
                                refine ⟨hpost.1.trans hcc1, hpost.2.1.trans hcd1,
                                  hvis_set_len.trans hvlen1, hvv_next,
                                  es_init ++ succs.map (fun s => { context := s, parent := some node_idx }),
                                  hrep_p, ?_⟩
                                intro e he
                                rw [List.mem_append] at he
                                rcases he with he1 | he2
                                · exact hinit_v_vis e he1
                                · rw [List.mem_map] at he2
                                  rcases he2 with ⟨s, hs_mem, rfl⟩
                                  exact hsuccs_v s hs_mem
                              · simp only [hns, ↓reduceIte] at hw
                                rw [bind_ok_eq DataDependencies_default_zero] at hw
                                rcases bind_eq_ok hw with ⟨ptc, htc, _⟩
                                rcases ptc with ⟨r_tc, ar_out, vc_tc⟩
                                rw [bind_triple_ok_eq htc] at hw
                                cases r_tc with
                                | Err _ =>
                                  dsimp only at hw
                                  rcases bind_eq_ok hw with ⟨pel, hel, _⟩
                                  rcases pel with ⟨r1, _⟩
                                  rw [bind_pair_ok_eq hel] at hw
                                  have hr1 := error_and_log_is_err hel
                                  subst hr1
                                  cases (Prod.mk.inj (ok_inj hw)).1
                                | Ok _ =>
                                  dsimp only at hw
                                  rcases bind_eq_ok hw with ⟨i1, _, hw⟩
                                  rcases Prod.mk.inj (ok_inj hw) with ⟨_, hp⟩
                                  rcases Prod.mk.inj hp with ⟨hwl_eq, hp⟩
                                  rcases Prod.mk.inj hp with ⟨_, hp⟩
                                  rcases Prod.mk.inj hp with ⟨hv_eq, hp⟩
                                  rcases Prod.mk.inj hp with ⟨hvc_eq, _⟩
                                  rw [← hwl_eq, ← hv_eq, ← hvc_eq]
                                  obtain ⟨hf1, hf2, hf3, _, _⟩ := terminate_context_chain_vctx_frame htc
                                  exact ⟨hf1.trans (hpost.1.trans hcc1), hf2.trans (hpost.2.1.trans hcd1),
                                    hvis_set_len.trans hvlen1, hvv_next, es_init, hrep_init,
                                    by dsimp only; rw [hf3]; exact hinit_v_vis⟩
        · simp only [he, Bool.false_eq_true, ↓reduceIte] at hbody
          cases ok_inj hbody)
    (by
      rintro ⟨l1, v1, w1, a1, vc1, e1, d1⟩ ⟨lo, vo, vco, eo⟩ hpre hbody
      dsimp only at hpre ⊢
      unfold verifier.verify_program_core_loop5.body at hbody
      by_cases hd : d1 = true
      · simp only [hd, ↓reduceIte] at hbody
        cases ok_inj hbody
        exact ⟨id, fun heo => ⟨(hpre heo).1, (hpre heo).2.1, (hpre heo).2.2.1, (hpre heo).2.2.2.1⟩⟩
      · simp only [hd, Bool.false_eq_true, ↓reduceIte] at hbody
        by_cases he : core.option.Option.is_none e1 = true
        · simp only [he, ↓reduceIte] at hbody
          rcases bind_eq_ok hbody with ⟨⟨o_pop, _⟩, _, hbody⟩
          cases o_pop with
          | none => cases ok_inj hbody
          | some _ =>
            rcases bind_eq_ok hbody with ⟨⟨r_w, _, _, _, _, _⟩, _, hbody⟩
            cases r_w <;> cases ok_inj hbody
        · simp only [he, Bool.false_eq_true, ↓reduceIte] at hbody
          cases ok_inj hbody
          have hne : e1 ≠ none := by
            intro hc; subst hc; simp [core.option.Option.is_none] at he
          exact ⟨fun _ => hne, fun heo => False.elim (hne heo)⟩)
    (logger, vis, wl, ar, vctx, none, false) (loggerOut, visOut, vctxOut, none)
    (fun _ => ⟨hvc_cctx, hvc_code, hvis_len, hvis_valid, es0, hwl_rep, hwl_valid⟩)
    hloop
  exact hmain.2 rfl

/-! ### 3.3 Pass 2: Per-Slot Refcounts, `ExplorerInv`, & `verify_program_core_loop5_soundness_spec` -/

def slotContextFor (p : Option Std.Usize) :
    Option verifier.ExplorerNode → Option verifier.ComputationContext
  | none => none
  | some n => if n.parent = p then some n.context else none

def arenaContextsFor (ar : verifier.ExplorerArena) (p : Option Std.Usize) :
    List verifier.ComputationContext :=
  ar.nodes.val.filterMap (slotContextFor p)

def worklistContextsFor (es : List verifier.WorklistEntry) (p : Option Std.Usize) :
    List verifier.ComputationContext :=
  es.filterMap (fun e => if e.parent = p then some e.context else none)

def inFlightContextsFor (inflight : Option (Option Std.Usize × verifier.ComputationContext))
    (p : Option Std.Usize) : List verifier.ComputationContext :=
  match inflight with
  | none => []
  | some (q, c) => if q = p then [c] else []

def totalLiveContexts (inflight : Option (Option Std.Usize × verifier.ComputationContext))
    (es : List verifier.WorklistEntry) (ar : verifier.ExplorerArena)
    (p : Option Std.Usize) : List verifier.ComputationContext :=
  inFlightContextsFor inflight p ++ worklistContextsFor es p ++ arenaContextsFor ar p

private theorem worklistContextsFor_append (es1 es2 : List verifier.WorklistEntry)
    (p : Option Std.Usize) :
    worklistContextsFor (es1 ++ es2) p = worklistContextsFor es1 p ++ worklistContextsFor es2 p :=
  List.filterMap_append

private theorem worklistContextsFor_singleton (e : verifier.WorklistEntry) (p : Option Std.Usize) :
    worklistContextsFor [e] p = inFlightContextsFor (some (e.parent, e.context)) p := by
  unfold worklistContextsFor inFlightContextsFor
  by_cases h : e.parent = p <;> simp [h]

private theorem worklistContextsFor_children_eq (succs : List verifier.ComputationContext)
    (node_idx : Std.Usize) :
    worklistContextsFor (succs.map (fun s => { context := s, parent := some node_idx })) (some node_idx) =
      succs := by
  induction succs with
  | nil => rfl
  | cons hd tl _ => simp [worklistContextsFor]

private theorem worklistContextsFor_children_ne (succs : List verifier.ComputationContext)
    {node_idx : Std.Usize} {p : Option Std.Usize} (hne : some node_idx ≠ p) :
    worklistContextsFor (succs.map (fun s => { context := s, parent := some node_idx })) p = [] := by
  induction succs with
  | nil => rfl
  | cons hd tl _ => simp [worklistContextsFor, hne]

private theorem filterMap_congr_set {α β : Type u} (f : α → Option β) :
    ∀ (l : List α) (k : Nat) (x y : α),
      k < l.length → l[k]? = some x → f y = f x →
      (l.set k y).filterMap f = l.filterMap f
  | [], _, _, _, h, _, _ => by simp at h
  | a :: tl, 0, x, y, _, h0, hfy => by
      cases Option.some.inj h0
      simp [List.set, List.filterMap, hfy]
  | a :: tl, k + 1, x, y, h, hk, hfy => by
      simp only [List.length_cons] at h
      simp only [List.getElem?_cons_succ] at hk
      have ih := filterMap_congr_set f tl k x y (by omega) hk hfy
      cases f a <;> simp [List.set, List.filterMap, ih]

private theorem filterMap_set_none_to_some {α β : Type u} (f : α → Option β) :
    ∀ (l : List α) (k : Nat) (x y : α) (b : β),
      k < l.length → l[k]? = some x → f x = none → f y = some b →
      ((l.set k y).filterMap f).length = (l.filterMap f).length + 1 ∧
      b ∈ (l.set k y).filterMap f ∧
      (∀ z, z ∈ l.filterMap f → z ∈ (l.set k y).filterMap f)
  | [], _, _, _, _, h, _, _, _ => by simp at h
  | a :: tl, 0, x, y, b, _, h0, hfx, hfy => by
      cases Option.some.inj h0
      simp only [List.set, List.filterMap, hfx, hfy, List.mem_cons]
      exact ⟨rfl, Or.inl trivial, fun _ hz => Or.inr hz⟩
  | a :: tl, k + 1, x, y, b, h, hk, hfx, hfy => by
      simp only [List.length_cons] at h
      simp only [List.getElem?_cons_succ] at hk
      obtain ⟨hlen, hb, hmem⟩ := filterMap_set_none_to_some f tl k x y b (by omega) hk hfx hfy
      cases hfa : f a with
      | none =>
        simp only [List.set, List.filterMap, hfa]
        exact ⟨hlen, hb, hmem⟩
      | some ba =>
        simp only [List.set, List.filterMap, hfa, List.length_cons, List.mem_cons]
        exact ⟨by omega, Or.inr hb, fun z hz => hz.imp id (hmem z)⟩

private theorem filterMap_set_some_to_none {α β : Type u} (f : α → Option β) :
    ∀ (l : List α) (k : Nat) (x y : α) (b : β),
      k < l.length → l[k]? = some x → f x = some b → f y = none →
      ((l.set k y).filterMap f).length + 1 = (l.filterMap f).length ∧
      (∀ z, z ∈ l.filterMap f → z = b ∨ z ∈ (l.set k y).filterMap f)
  | [], _, _, _, _, h, _, _, _ => by simp at h
  | a :: tl, 0, x, y, b, _, h0, hfx, hfy => by
      cases Option.some.inj h0
      simp only [List.set, List.filterMap, hfx, hfy, List.mem_cons]
      exact ⟨rfl, fun _ h => h⟩
  | a :: tl, k + 1, x, y, b, h, hk, hfx, hfy => by
      simp only [List.length_cons] at h
      simp only [List.getElem?_cons_succ] at hk
      obtain ⟨hlen, hmem⟩ := filterMap_set_some_to_none f tl k x y b (by omega) hk hfx hfy
      cases hfa : f a with
      | none =>
        simp only [List.set, List.filterMap, hfa]
        exact ⟨hlen, hmem⟩
      | some ba =>
        simp only [List.set, List.filterMap, hfa, List.length_cons, List.mem_cons]
        refine ⟨by omega, fun z hz => ?_⟩
        rcases hz with rfl | hztl
        · exact Or.inr (Or.inl rfl)
        · rcases hmem z hztl with rfl | hzset
          · exact Or.inl rfl
          · exact Or.inr (Or.inr hzset)

private theorem arenaContextsFor_push_conserve
    {ar ar' : verifier.ExplorerArena} {node : verifier.ExplorerNode} {idx : Std.Usize}
    {es : List verifier.WorklistEntry}
    (hwf : ArenaWf ar)
    (hpush : verifier.ExplorerArena.push ar node = ok (idx, ar'))
    (p : Option Std.Usize) :
    (totalLiveContexts none es ar' p).length =
      (totalLiveContexts (some (node.parent, node.context)) es ar p).length ∧
    (∀ z, z ∈ totalLiveContexts (some (node.parent, node.context)) es ar p →
      z ∈ totalLiveContexts none es ar' p) := by
  unfold verifier.ExplorerArena.push at hpush
  by_cases hgt : ar.free_len > 0#usize
  · simp only [hgt, ↓reduceIte] at hpush
    have hfl_pos : 1 ≤ ar.free_len.val := by
      have : (0#usize).val < ar.free_len.val := hgt
      rw [usize_const_val_0] at this; omega
    rcases bind_eq_ok hpush with ⟨i, hsub, hpush⟩
    have hi_add := usize_sub_one_ok_val hfl_pos hsub
    rcases bind_eq_ok hpush with ⟨idx0, hidx_get, hpush⟩
    rcases bind_eq_ok hpush with ⟨pmb, hmut, _⟩
    rcases pmb with ⟨_, back⟩
    rw [bind_pair_ok_eq hmut] at hpush
    rcases Prod.mk.inj (ok_inj hpush) with ⟨rfl, rfl⟩
    obtain ⟨_, hget_fi⟩ := vec_index_usize_ok hidx_get
    obtain ⟨hlt_nd, _, hset_nd⟩ := vec_index_mut_usize_ok hmut
    obtain ⟨hval_nd, _⟩ := hset_nd (some node)
    have hwas_none : ar.nodes.val[idx0.val]? = some none := by
      have htake_split : ar.free_indices.val.take ar.free_len.val =
          ar.free_indices.val.take i.val ++ [idx0] := by
        rw [hi_add.symm, List.take_add_one, hget_fi]; simp
      exact (hwf.2.2 idx0 (by rw [htake_split]; simp)).2
    by_cases hp_eq : node.parent = p
    · subst hp_eq
      have hf_new : slotContextFor node.parent (some node) = some node.context := by
        simp [slotContextFor]
      obtain ⟨hlen_fm, hb_fm, hmem_fm⟩ :=
        filterMap_set_none_to_some (slotContextFor node.parent) ar.nodes.val idx0.val none (some node)
          node.context hlt_nd hwas_none rfl hf_new
      unfold totalLiveContexts arenaContextsFor inFlightContextsFor
      dsimp only
      rw [hval_nd]
      simp only [↓reduceIte, List.nil_append, List.cons_append, List.length_append, List.length_cons]
      refine ⟨by omega, fun z hz => ?_⟩
      simp only [List.mem_cons, List.mem_append] at hz ⊢
      rcases hz with rfl | hz_wl | hz_ar
      · exact Or.inr hb_fm
      · exact Or.inl hz_wl
      · exact Or.inr (hmem_fm z hz_ar)
    · have hf_eq : slotContextFor p (some node) = slotContextFor p none := by
        simp [slotContextFor, hp_eq]
      have hfm_eq := filterMap_congr_set (slotContextFor p) ar.nodes.val idx0.val none (some node)
        hlt_nd hwas_none hf_eq
      unfold totalLiveContexts arenaContextsFor inFlightContextsFor
      dsimp only
      rw [hval_nd, hfm_eq]
      simp [hp_eq]
  · simp only [hgt, ↓reduceIte] at hpush
    rcases bind_eq_ok hpush with ⟨v, hv_push, hpush⟩
    rcases Prod.mk.inj (ok_inj hpush) with ⟨rfl, rfl⟩
    have hv_val := vec_push_ok_val hv_push
    unfold totalLiveContexts arenaContextsFor inFlightContextsFor
    dsimp only
    rw [hv_val, List.filterMap_append]
    by_cases hp_eq : node.parent = p
    · subst hp_eq
      have hfm_single : List.filterMap (slotContextFor node.parent) [some node] = [node.context] := by
        simp [slotContextFor]
      rw [hfm_single]
      simp only [↓reduceIte, List.nil_append, List.cons_append, List.length_append, List.length_cons, List.length_nil]
      refine ⟨by omega, fun z hz => ?_⟩
      simp only [List.mem_cons, List.mem_append] at hz ⊢
      rcases hz with rfl | hz_wl | hz_ar
      · exact Or.inr (Or.inr (Or.inl rfl))
      · exact Or.inl hz_wl
      · exact Or.inr (Or.inl hz_ar)
    · simp [slotContextFor, hp_eq]

private theorem arenaContextsFor_complete_keep
    {ar ar' : verifier.ExplorerArena} {p_idx i : Std.Usize}
    {p_node : verifier.ExplorerNode} {dd : verifier.DataDependencies}
    (hp_lt : p_idx.val < ar.nodes.val.length)
    (hp_get : ar.nodes.val[p_idx.val]? = some (some p_node))
    (h_same : ar'.nodes.val[p_idx.val]? =
      some (some { p_node with pending_children := i, dependencies := dd }))
    (h_other : ∀ k, k ≠ p_idx.val → ar'.nodes.val[k]? = ar.nodes.val[k]?)
    (h_len : ar'.nodes.val.length = ar.nodes.val.length)
    (p : Option Std.Usize) :
    arenaContextsFor ar' p = arenaContextsFor ar p := by
  have hnodes_eq : ar'.nodes.val =
      ar.nodes.val.set p_idx.val (some { p_node with pending_children := i, dependencies := dd }) := by
    apply List.ext_getElem
    · rw [h_len, List.length_set]
    · intro k hk1 hk2
      apply Option.some.inj
      rw [← List.getElem?_eq_getElem hk1, ← List.getElem?_eq_getElem hk2]
      by_cases hk : k = p_idx.val
      · subst hk; rw [h_same, list_getElem?_set_eq ar.nodes.val p_idx.val _ hp_lt]
      · rw [h_other k hk, list_getElem?_set_of_ne ar.nodes.val p_idx.val k _ (Ne.symm hk)]
  unfold arenaContextsFor
  rw [hnodes_eq]
  exact filterMap_congr_set (slotContextFor p) ar.nodes.val p_idx.val
    (some p_node) (some { p_node with pending_children := i, dependencies := dd })
    hp_lt hp_get rfl

private theorem arenaContextsFor_complete_free
    {ar ar' : verifier.ExplorerArena} {p_idx : Std.Usize}
    {p_node : verifier.ExplorerNode} {es : List verifier.WorklistEntry}
    (hp_lt : p_idx.val < ar.nodes.val.length)
    (hp_get : ar.nodes.val[p_idx.val]? = some (some p_node))
    (h_none : ar'.nodes.val[p_idx.val]? = some none)
    (h_other : ∀ k, k ≠ p_idx.val → ar'.nodes.val[k]? = ar.nodes.val[k]?)
    (h_len : ar'.nodes.val.length = ar.nodes.val.length)
    (p : Option Std.Usize) :
    (totalLiveContexts (some (p_node.parent, p_node.context)) es ar' p).length =
      (totalLiveContexts none es ar p).length ∧
    (∀ z, z ∈ totalLiveContexts none es ar p →
      z ∈ totalLiveContexts (some (p_node.parent, p_node.context)) es ar' p) := by
  have hnodes_eq : ar'.nodes.val = ar.nodes.val.set p_idx.val none := by
    apply List.ext_getElem
    · rw [h_len, List.length_set]
    · intro k hk1 hk2
      apply Option.some.inj
      rw [← List.getElem?_eq_getElem hk1, ← List.getElem?_eq_getElem hk2]
      by_cases hk : k = p_idx.val
      · subst hk; rw [h_none, list_getElem?_set_eq ar.nodes.val p_idx.val none hp_lt]
      · rw [h_other k hk, list_getElem?_set_of_ne ar.nodes.val p_idx.val k none (Ne.symm hk)]
  unfold totalLiveContexts arenaContextsFor inFlightContextsFor
  rw [hnodes_eq]
  by_cases hp_eq : p_node.parent = p
  · subst hp_eq
    have hf_some : slotContextFor p_node.parent (some p_node) = some p_node.context := by
      simp [slotContextFor]
    obtain ⟨hlen_fm, hmem_fm⟩ :=
      filterMap_set_some_to_none (slotContextFor p_node.parent) ar.nodes.val p_idx.val
        (some p_node) none p_node.context hp_lt hp_get hf_some rfl
    simp only [↓reduceIte, List.nil_append, List.cons_append, List.length_cons, List.length_append]
    refine ⟨by omega, fun z hz => ?_⟩
    simp only [List.mem_append, List.mem_cons] at hz ⊢
    rcases hz with hz_wl | hz_ar
    · exact Or.inr (Or.inl hz_wl)
    · rcases hmem_fm z hz_ar with rfl | hz_set
      · exact Or.inl rfl
      · exact Or.inr (Or.inr hz_set)
  · have hf_eq : slotContextFor p none = slotContextFor p (some p_node) := by
      simp [slotContextFor, hp_eq]
    rw [filterMap_congr_set (slotContextFor p) ar.nodes.val p_idx.val (some p_node) none
      hp_lt hp_get hf_eq]
    simp [hp_eq]

private def parentIdxLt (N : Nat) : Option verifier.ExplorerNode → Bool
  | none => false
  | some n => match n.parent with | none => false | some idx => decide (idx.val < N)

private def parentEqOpt (p : Option Std.Usize) : Option verifier.ExplorerNode → Bool
  | none => false
  | some n => decide (n.parent = p)

private theorem filterMap_slotContextFor_length (l : List (Option verifier.ExplorerNode))
    (p : Option Std.Usize) :
    (l.filterMap (slotContextFor p)).length = (l.filter (parentEqOpt p)).length := by
  induction l with
  | nil => rfl
  | cons o tl ih =>
    cases o with
    | none => simp [slotContextFor, parentEqOpt, List.filterMap, List.filter, ih]
    | some n =>
      by_cases h : n.parent = p <;>
        simp [slotContextFor, parentEqOpt, h, List.filterMap, List.filter, ih]

private theorem filter_disjoint_or_length {α : Type u} (p q : α → Bool) :
    ∀ (l : List α), (∀ x ∈ l, ¬(p x = true ∧ q x = true)) →
      (l.filter (fun x => p x || q x)).length = (l.filter p).length + (l.filter q).length
  | [], _ => rfl
  | a :: tl, hdisj => by
      have ha := hdisj a List.mem_cons_self
      have ih := filter_disjoint_or_length p q tl (fun x hx => hdisj x (List.mem_cons_of_mem _ hx))
      cases hp : p a <;> cases hq : q a
      · simp [List.filter, hp, hq, ih]
      · simp [List.filter, hp, hq, ih]; omega
      · simp [List.filter, hp, hq, ih]; omega
      · exact False.elim (ha ⟨hp, hq⟩)

private theorem empty_worklist_implies_rootCompleted
    (ar : verifier.ExplorerArena) (rootCompleted : Bool)
    (hroot : (arenaContextsFor ar none).length = (if rootCompleted then 0 else 1))
    (hslots : ∀ idx : Std.Usize,
      (idx.val < ar.nodes.val.length ∧
        ∃ node, ar.nodes.val[idx.val]? = some (some node) ∧
          (arenaContextsFor ar (some idx)).length = node.pending_children.val ∧
          1 ≤ node.pending_children.val) ∨
      ((idx.val ≥ ar.nodes.val.length ∨ ar.nodes.val[idx.val]? = some none) ∧
        (arenaContextsFor ar (some idx)).length = 0)) :
    rootCompleted = true := by
  have hmax_len : ar.nodes.val.length < 2 ^ UScalarTy.Usize.numBits :=
    (alloc.vec.Vec.len ar.nodes).hmax
  have hbound_k : ∀ k, k ≤ ar.nodes.val.length →
      ((ar.nodes.val.take k).filter Option.isSome).length ≤
        (ar.nodes.val.filter (parentIdxLt k)).length := by
    intro k
    induction k with
    | zero =>
      intro _
      have h0 : ar.nodes.val.filter (parentIdxLt 0) = [] := by
        apply List.filter_eq_nil_iff.mpr
        intro o _; cases o with
        | none => simp [parentIdxLt]
        | some n => cases hp : n.parent <;> simp [parentIdxLt, hp]
      simp [h0]
    | succ k ih =>
      intro hk_le
      have hk_lt : k < ar.nodes.val.length := by omega
      let idx_k : Std.Usize := UScalar.ofNatCore k (by omega)
      have hidx_k_val : idx_k.val = k := rfl
      have hfn_eq : parentIdxLt (k + 1) = (fun x => parentIdxLt k x || parentEqOpt (some idx_k) x) := by
        funext o
        cases o with
        | none => rfl
        | some n =>
          cases hp : n.parent with
          | none => simp [parentIdxLt, parentEqOpt, hp]
          | some idx =>
            simp only [parentIdxLt, parentEqOpt, hp]
            by_cases heq : idx = idx_k
            · subst idx; simp [hidx_k_val]
            · have hne_val : idx.val ≠ k := fun hc => heq (usize_eq_of_val_eq (hc.trans hidx_k_val.symm))
              by_cases hlt1 : idx.val < k
              · have hlt2 : idx.val < k + 1 := by omega
                simp [hlt1, hlt2, heq]
              · have hnlt2 : ¬ (idx.val < k + 1) := by omega
                simp [hlt1, hnlt2, heq]
      have hdisj : ∀ x ∈ ar.nodes.val, ¬(parentIdxLt k x = true ∧ parentEqOpt (some idx_k) x = true) := by
        intro o _ ⟨h1, h2⟩
        cases o with
        | none => cases h1
        | some n =>
          cases hp_n : n.parent with
          | none => simp [parentIdxLt, hp_n] at h1
          | some idx =>
            simp only [parentIdxLt, parentEqOpt, hp_n, decide_eq_true_eq, Option.some.injEq] at h1 h2
            subst h2; rw [hidx_k_val] at h1; omega
      have hsplit_len : (ar.nodes.val.filter (parentIdxLt (k + 1))).length =
          (ar.nodes.val.filter (parentIdxLt k)).length + (arenaContextsFor ar (some idx_k)).length := by
        rw [hfn_eq, filter_disjoint_or_length _ _ ar.nodes.val hdisj,
            ← filterMap_slotContextFor_length ar.nodes.val (some idx_k)]
        rfl
      have htake_step : ar.nodes.val.take (k + 1) =
          ar.nodes.val.take k ++ [ar.nodes.val[k]'hk_lt] := by
        rw [List.take_add_one, List.getElem?_eq_getElem hk_lt]; rfl
      have ih_k := ih (by omega)
      rw [htake_step, List.filter_append, List.length_append, hsplit_len]
      cases hget_k : ar.nodes.val[k]'hk_lt with
      | none =>
        simp only [List.filter, Option.isSome, List.length_nil, Nat.add_zero]
        omega
      | some node_k =>
        simp only [List.filter, Option.isSome, List.length_singleton]
        have hget_opt : ar.nodes.val[idx_k.val]? = some (some node_k) := by
          rw [hidx_k_val, List.getElem?_eq_getElem hk_lt, hget_k]
        rcases hslots idx_k with ⟨_, node', hget', hlen_eq, hpos⟩ | ⟨hbad, _⟩
        · have hnode_eq : node' = node_k := Option.some.inj (Option.some.inj (hget'.symm.trans hget_opt))
          subst hnode_eq
          omega
        · rcases hbad with hge | hnone
          · rw [hidx_k_val] at hge; omega
          · rw [hget_opt] at hnone; cases Option.some.inj hnone
  have hactive_le := hbound_k ar.nodes.val.length (Nat.le_refl _)
  rw [List.take_length] at hactive_le
  have hall_active_eq :
      ar.nodes.val.filter Option.isSome =
        ar.nodes.val.filter (fun x => parentEqOpt none x || parentIdxLt ar.nodes.val.length x) := by
    apply List.filter_congr
    intro o ho
    cases o with
    | none => rfl
    | some n =>
      simp only [Option.isSome, Bool.true_eq]
      cases hp_n : n.parent with
      | none => simp [parentEqOpt, hp_n]
      | some idx =>
        simp only [parentEqOpt, parentIdxLt, hp_n, reduceCtorEq, decide_false, Bool.false_or,
          decide_eq_true_eq]
        by_contra hge
        have hge' : idx.val ≥ ar.nodes.val.length := Nat.not_lt.mp hge
        rcases hslots idx with ⟨hlt, _⟩ | ⟨_, hzero⟩
        · omega
        · have hmem_ctx : n.context ∈ arenaContextsFor ar (some idx) := by
            unfold arenaContextsFor
            rw [List.mem_filterMap]
            exact ⟨some n, ho, by simp [slotContextFor, hp_n]⟩
          have hlen_pos : 0 < (arenaContextsFor ar (some idx)).length :=
            List.length_pos_of_mem hmem_ctx
          omega
  have hdisj_top : ∀ x ∈ ar.nodes.val,
      ¬(parentEqOpt none x = true ∧ parentIdxLt ar.nodes.val.length x = true) := by
    intro o _ ⟨h1, h2⟩
    cases o with
    | none => cases h1
    | some n =>
      cases hp_n : n.parent <;> simp [parentEqOpt, parentIdxLt, hp_n] at h1 h2
  have hactive_sum : arenaActiveCount ar =
      (arenaContextsFor ar none).length + (ar.nodes.val.filter (parentIdxLt ar.nodes.val.length)).length := by
    unfold arenaActiveCount
    rw [hall_active_eq, filter_disjoint_or_length _ _ ar.nodes.val hdisj_top,
        ← filterMap_slotContextFor_length ar.nodes.val none]
    rfl
  cases rootCompleted with
  | true => rfl
  | false =>
    simp only [Bool.false_eq_true, ↓reduceIte] at hroot
    rw [hroot] at hactive_sum
    change (ar.nodes.val.filter Option.isSome).length ≤ _ at hactive_le
    unfold arenaActiveCount at hactive_sum
    omega

/-- Semantic invariant for an active `ExplorerNode` at index `idx` in `ExplorerArena`. -/
def ActiveNodeSem (prog : Program) (env : ExecEnv)
    (cctx : verifier.CoreCallingContext) (code : alloc.vec.Vec api.EbpfInstruction)
    (ctxInit : verifier.ComputationContext)
    (inflight : Option (Option Std.Usize × verifier.ComputationContext))
    (es : List verifier.WorklistEntry) (ar : verifier.ExplorerArena)
    (v_cur M : Nat) (idx : Std.Usize) (node : verifier.ExplorerNode) : Prop :=
  (node.parent = none → node.context = ctxInit) ∧
  node.context.pc.val < prog.size ∧
  CodeMatchesProgAt cctx code.val prog node.context.pc.val ∧
  ∃ vctx0 vctx1 inst,
    vctx0.calling_context = cctx ∧
    vctx0.code = code ∧
    vctx0.pending_successors.val = [] ∧
    CompCtxValid env vctx0.counter.bv.toNat node.context ∧
    vctx1.counter.bv.toNat ≤ v_cur ∧
    code.val[node.context.pc.val]? = some inst ∧
    visitor.BpfVisitor.visit.default CompCtxVisitorInst node.context vctx0 inst =
      ok (.Ok (), node.context, vctx1) ∧
    (∀ opt ∈ vctx1.pending_successors.val,
      ∃ succ, opt = some succ ∧ CompCtxValid env vctx1.counter.bv.toNat succ) ∧
    (∀ succ, some succ ∈ vctx1.pending_successors.val →
      succ ∈ totalLiveContexts inflight es ar (some idx) ∨
      ∃ F, F ≤ M ∧ SemTerminatesIn prog env v_cur succ node.dependencies F)

/-- Precondition for a node ready for backward transfer in `terminate_context_chain`. -/
def NodeReadyToFinish (prog : Program) (env : ExecEnv)
    (cctx : verifier.CoreCallingContext) (code : alloc.vec.Vec api.EbpfInstruction)
    (ctx : verifier.ComputationContext) (deps : verifier.DataDependencies)
    (v_cur M : Nat) : Prop :=
  ctx.pc.val < prog.size ∧
  CodeMatchesProgAt cctx code.val prog ctx.pc.val ∧
  ∃ vctx0 vctx1 inst,
    vctx0.calling_context = cctx ∧
    vctx0.code = code ∧
    vctx0.pending_successors.val = [] ∧
    CompCtxValid env vctx0.counter.bv.toNat ctx ∧
    vctx1.counter.bv.toNat ≤ v_cur ∧
    code.val[ctx.pc.val]? = some inst ∧
    visitor.BpfVisitor.visit.default CompCtxVisitorInst ctx vctx0 inst =
      ok (.Ok (), ctx, vctx1) ∧
    (∀ opt ∈ vctx1.pending_successors.val,
      ∃ succ, opt = some succ ∧ CompCtxValid env vctx1.counter.bv.toNat succ) ∧
    (∀ succ, some succ ∈ vctx1.pending_successors.val →
      SemTerminatesIn prog env v_cur succ deps M)

/-- Full DFS explorer invariant parameterized by `inflight` (`none` between steps,
    `some (parent, ctx)` while processing a popped entry or walking `terminate_context_chain`),
    worklist `es`, `arena`, `vctx`, fuel high-water mark `M`, and `rootCompleted`. -/
def ExplorerInv (prog : Program) (env : ExecEnv)
    (cctx : verifier.CoreCallingContext) (code : alloc.vec.Vec api.EbpfInstruction)
    (ctxInit : verifier.ComputationContext)
    (inflight : Option (Option Std.Usize × verifier.ComputationContext))
    (es : List verifier.WorklistEntry)
    (ar : verifier.ExplorerArena)
    (vctx : verifier.VerificationContext)
    (M : Nat) (rootCompleted : Bool) : Prop :=
  vctx.calling_context = cctx ∧
  vctx.code = code ∧
  vctx.terminating_contexts.val.length = code.val.length ∧
  vctx.counter.bv.toNat < 2 ^ 63 ∧
  ArenaWf ar ∧
  (∀ tcOpt ∈ vctx.terminating_contexts.val, ∀ tc, tcOpt = some tc →
    TerminatingContextsInv prog env vctx.counter.bv.toNat M tc) ∧
  (∀ e ∈ es, CompCtxValid env vctx.counter.bv.toNat e.context ∧
    (e.parent = none → e.context = ctxInit)) ∧
  (totalLiveContexts inflight es ar none).length = (if rootCompleted then 0 else 1) ∧
  (rootCompleted = true → ∃ dRoot, SemTerminatesIn prog env 0 ctxInit dRoot M) ∧
  (∀ idx : Std.Usize,
    (idx.val < ar.nodes.val.length ∧
      ∃ node, ar.nodes.val[idx.val]? = some (some node) ∧
        (totalLiveContexts inflight es ar (some idx)).length = node.pending_children.val ∧
        1 ≤ node.pending_children.val ∧
        ActiveNodeSem prog env cctx code ctxInit inflight es ar vctx.counter.bv.toNat M idx node) ∨
    ((idx.val ≥ ar.nodes.val.length ∨ ar.nodes.val[idx.val]? = some none) ∧
      (totalLiveContexts inflight es ar (some idx)).length = 0))

/-- Monotonicity of `ActiveNodeSem` when raising `v_cur` and/or `M` with identical `totalLiveContexts`
    membership on `some idx`. -/
private theorem ActiveNodeSem_mono
    {prog : Program} {env : ExecEnv}
    {cctx : verifier.CoreCallingContext} {code : alloc.vec.Vec api.EbpfInstruction}
    {ctxInit : verifier.ComputationContext}
    {inf1 inf2 : Option (Option Std.Usize × verifier.ComputationContext)}
    {es1 es2 : List verifier.WorklistEntry} {ar1 ar2 : verifier.ExplorerArena}
    {v1 v2 M1 M2 : Nat} {idx : Std.Usize} {node : verifier.ExplorerNode}
    (hv_le : v1 ≤ v2) (hv2_lt : v2 < 2 ^ 63) (hM_le : M1 ≤ M2)
    (hlive_sub : ∀ z, z ∈ totalLiveContexts inf1 es1 ar1 (some idx) →
      z ∈ totalLiveContexts inf2 es2 ar2 (some idx))
    (hsem : ActiveNodeSem prog env cctx code ctxInit inf1 es1 ar1 v1 M1 idx node) :
    ActiveNodeSem prog env cctx code ctxInit inf2 es2 ar2 v2 M2 idx node := by
  rcases hsem with ⟨hp_root, hpc_lt, hmatch, vctx0, vctx1, inst, hc0, hcd0, hp0, hctx0,
    hv1_cnt, hinst, hfwd, hsucc_v, hdisj⟩
  refine ⟨hp_root, hpc_lt, hmatch, vctx0, vctx1, inst, hc0, hcd0, hp0, hctx0,
    Nat.le_trans hv1_cnt hv_le, hinst, hfwd, hsucc_v, fun succ hsucc => ?_⟩
  rcases hdisj succ hsucc with hin | ⟨F, hF_le, hterm⟩
  · exact Or.inl (hlive_sub succ hin)
  · obtain ⟨succ', hs_eq, hsucc_valid⟩ := hsucc_v (some succ) hsucc
    cases Option.some.inj hs_eq
    have hsucc_v2 : CompCtxValid env v2 succ :=
      CompCtxValid_mono (Nat.le_trans hv1_cnt hv_le) hsucc_valid
    exact Or.inr ⟨F, Nat.le_trans hF_le hM_le,
      SemTerminatesIn_change_vCounter (Nat.lt_of_le_of_lt hv_le hv2_lt) hsucc_v2 hterm⟩

/-- Core child-completion transition (`complete_child` or `parent = none`) from an in-flight
    context `c` that has already been proved `SemTerminatesIn prog env vctx.counter.bv.toNat c c_deps F_c`
    with `F_c ≤ M`. -/
private theorem complete_inflight_step
    {prog : Program} {env : ExecEnv}
    {cctx : verifier.CoreCallingContext} {code : alloc.vec.Vec api.EbpfInstruction}
    {ctxInit : verifier.ComputationContext}
    {es : List verifier.WorklistEntry}
    {ar : verifier.ExplorerArena}
    {vctx : verifier.VerificationContext}
    {M F_c : Nat} {rootCompleted : Bool}
    {parent_opt : Option Std.Usize} {c : verifier.ComputationContext}
    {c_deps : verifier.DataDependencies}
    (hinit_valid : CompCtxValid env 0 ctxInit)
    (hinv : ExplorerInv prog env cctx code ctxInit (some (parent_opt, c)) es ar vctx M rootCompleted)
    (hc_root : parent_opt = none → c = ctxInit)
    (hFc_le : F_c ≤ M)
    (hsem_c : SemTerminatesIn prog env vctx.counter.bv.toNat c c_deps F_c) :
    (parent_opt = none ∧
      ExplorerInv prog env cctx code ctxInit none es ar vctx M true) ∨
    (∃ p_idx, parent_opt = some p_idx ∧
      ∀ out_opt ar1,
        verifier.ExplorerArena.complete_child ar p_idx c_deps = ok (out_opt, ar1) →
        (out_opt = none ∧
          arenaActiveCount ar1 = arenaActiveCount ar ∧
          ExplorerInv prog env cctx code ctxInit none es ar1 vctx M rootCompleted) ∨
        (∃ parent_node, out_opt = some parent_node ∧
          arenaActiveCount ar1 + 1 = arenaActiveCount ar ∧
          (parent_node.parent = none → parent_node.context = ctxInit) ∧
          NodeReadyToFinish prog env cctx code parent_node.context parent_node.dependencies
            vctx.counter.bv.toNat M ∧
          ExplorerInv prog env cctx code ctxInit
            (some (parent_node.parent, parent_node.context)) es ar1 vctx M rootCompleted)) := by
  rcases hinv with ⟨hvc_cctx, hvc_code, htc_len, hvc_lt, hwf, htc_inv, hes_inv,
    hroot_len, hroot_sem, hslots⟩
  cases parent_opt with
  | none =>
    left
    refine ⟨rfl, hvc_cctx, hvc_code, htc_len, hvc_lt, hwf, htc_inv, hes_inv, ?_, ?_, ?_⟩
    · have hrc_false : rootCompleted = false := by
        have hpos : 1 ≤ (totalLiveContexts (some (none, c)) es ar none).length := by
          unfold totalLiveContexts inFlightContextsFor; simp
        cases rootCompleted <;> simp_all
      subst hrc_false
      unfold totalLiveContexts inFlightContextsFor at hroot_len ⊢
      simp only [Bool.false_eq_true, ↓reduceIte, List.cons_append, List.length_cons, List.nil_append] at hroot_len ⊢
      omega
    · intro _
      have hc_eq := hc_root rfl
      subst hc_eq
      have hsem0 := SemTerminatesIn_change_vCounter hvc_lt hinit_valid hsem_c
      exact ⟨c_deps, SemTerminatesIn_mono_fuel hFc_le hsem0⟩
    · intro idx
      have heq_tl : totalLiveContexts none es ar (some idx) =
          totalLiveContexts (some (none, c)) es ar (some idx) := by
        unfold totalLiveContexts inFlightContextsFor; simp
      rcases hslots idx with ⟨hlt, node, hget, hlen, hpos, hsem⟩ | ⟨hbad, hzero⟩
      · left
        refine ⟨hlt, node, hget, heq_tl ▸ hlen, hpos, ?_⟩
        exact ActiveNodeSem_mono (Nat.le_refl _) hvc_lt (Nat.le_refl _)
          (fun z hz => heq_tl ▸ hz) hsem
      · exact Or.inr ⟨hbad, heq_tl ▸ hzero⟩
  | some p_idx =>
    right
    refine ⟨p_idx, rfl, fun out_opt ar1 hcc => ?_⟩
    have hp_pos_live : 1 ≤ (totalLiveContexts (some (some p_idx, c)) es ar (some p_idx)).length := by
      unfold totalLiveContexts inFlightContextsFor; simp
    have hp_active : p_idx.val < ar.nodes.val.length ∧
        ∃ p_node, ar.nodes.val[p_idx.val]? = some (some p_node) ∧
          (totalLiveContexts (some (some p_idx, c)) es ar (some p_idx)).length = p_node.pending_children.val ∧
          1 ≤ p_node.pending_children.val ∧
          ActiveNodeSem prog env cctx code ctxInit (some (some p_idx, c)) es ar
            vctx.counter.bv.toNat M p_idx p_node := by
      rcases hslots p_idx with hact | ⟨_, hzero⟩
      · exact hact
      · omega
    rcases hp_active with ⟨hp_lt, p_node, hp_get, hp_len_eq, hp_pend_pos, hp_sem⟩
    obtain ⟨hwf1, h_other, h_len, dd, hsub_p, hsub_c, hcases_cc⟩ :=
      ExplorerArena_complete_child_spec hwf ⟨hp_lt, hp_get⟩ hp_pend_pos hcc
    rcases hcases_cc with ⟨_, rfl, ⟨i_new, hi_add, hi_pos, h_same⟩, hact_eq⟩ |
        ⟨hp_one, rfl, h_none, hact_dec⟩
    · left
      have har_ctx_eq : ∀ p, arenaContextsFor ar1 p = arenaContextsFor ar p :=
        arenaContextsFor_complete_keep hp_lt hp_get h_same h_other h_len
      refine ⟨rfl, hact_eq, hvc_cctx, hvc_code, htc_len, hvc_lt, hwf1, htc_inv, hes_inv, ?_, hroot_sem, ?_⟩
      · unfold totalLiveContexts inFlightContextsFor at hroot_len ⊢
        rw [har_ctx_eq none]; simpa using hroot_len
      · intro idx
        by_cases hidx_eq : idx = p_idx
        · subst idx
          left
          have htl_drop : totalLiveContexts (some (some p_idx, c)) es ar (some p_idx) =
              c :: totalLiveContexts none es ar1 (some p_idx) := by
            unfold totalLiveContexts inFlightContextsFor
            rw [har_ctx_eq (some p_idx)]; simp
          refine ⟨h_len ▸ hp_lt, { p_node with pending_children := i_new, dependencies := dd },
            h_same, ?_, hi_pos, ?_⟩
          · rw [htl_drop, List.length_cons] at hp_len_eq; dsimp only; omega
          · rcases hp_sem with ⟨hp_rt, hpc_p, hm_p, vc0, vc1, inst_p, hc0, hcd0, hp0, hctx0,
              hv1_c, hi_at, hfwd_p, hsucc_v, hdisj_p⟩
            refine ⟨hp_rt, hpc_p, hm_p, vc0, vc1, inst_p, hc0, hcd0, hp0, hctx0,
              hv1_c, hi_at, hfwd_p, hsucc_v, fun succ hsucc => ?_⟩
            dsimp only
            by_cases hsc : succ = c
            · subst hsc
              exact Or.inr ⟨F_c, hFc_le, SemTerminatesIn_mono_deps hsub_c hsem_c⟩
            · rcases hdisj_p succ hsucc with hin | ⟨F, hF_le, hterm⟩
              · rw [htl_drop, List.mem_cons] at hin
                exact Or.inl (hin.resolve_left hsc)
              · exact Or.inr ⟨F, hF_le, SemTerminatesIn_mono_deps hsub_p hterm⟩
        · have hne_val : idx.val ≠ p_idx.val := fun hc => hidx_eq (usize_eq_of_val_eq hc)
          have htl_same : totalLiveContexts none es ar1 (some idx) =
              totalLiveContexts (some (some p_idx, c)) es ar (some idx) := by
            unfold totalLiveContexts inFlightContextsFor
            rw [har_ctx_eq (some idx)]
            have hne_opt : (some p_idx : Option Std.Usize) ≠ some idx :=
              fun hc => hidx_eq (Option.some.inj hc).symm
            simp [hne_opt]
          rcases hslots idx with ⟨hlt_i, node_i, hget_i, hlen_i, hpos_i, hsem_i⟩ | ⟨hbad_i, hzero_i⟩
          · left
            refine ⟨h_len ▸ hlt_i, node_i, (h_other idx.val hne_val).trans hget_i,
              htl_same ▸ hlen_i, hpos_i, ?_⟩
            exact ActiveNodeSem_mono (Nat.le_refl _) hvc_lt (Nat.le_refl _)
              (fun z hz => htl_same ▸ hz) hsem_i
          · right
            refine ⟨?_, htl_same ▸ hzero_i⟩
            rcases hbad_i with hge_i | hnone_i
            · exact Or.inl (h_len ▸ hge_i)
            · exact Or.inr ((h_other idx.val hne_val).trans hnone_i)
    · right
      let p_out : verifier.ExplorerNode := { p_node with pending_children := 0#usize, dependencies := dd }
      have hconserve := arenaContextsFor_complete_free (es := es) hp_lt hp_get h_none h_other h_len
      rcases hp_sem with ⟨hp_rt, hpc_p, hm_p, vc0, vc1, inst_p, hc0, hcd0, hp0, hctx0,
        hv1_c, hi_at, hfwd_p, hsucc_v, hdisj_p⟩
      have htl_singleton : totalLiveContexts (some (some p_idx, c)) es ar (some p_idx) = [c] := by
        unfold totalLiveContexts inFlightContextsFor at hp_len_eq ⊢
        simp only [↓reduceIte, List.cons_append, List.nil_append, List.length_cons] at hp_len_eq ⊢
        rw [hp_one] at hp_len_eq
        have hnil : worklistContextsFor es (some p_idx) ++ arenaContextsFor ar (some p_idx) = [] :=
          List.eq_nil_of_length_eq_zero (by omega)
        rw [hnil]
      have hready : NodeReadyToFinish prog env cctx code p_out.context p_out.dependencies
          vctx.counter.bv.toNat M := by
        refine ⟨hpc_p, hm_p, vc0, vc1, inst_p, hc0, hcd0, hp0, hctx0, hv1_c, hi_at, hfwd_p,
          hsucc_v, fun succ hsucc => ?_⟩
        dsimp only [p_out]
        rcases hdisj_p succ hsucc with hin | ⟨F, hF_le, hterm⟩
        · rw [htl_singleton, List.mem_singleton] at hin
          subst hin
          exact SemTerminatesIn_mono_fuel hFc_le (SemTerminatesIn_mono_deps hsub_c hsem_c)
        · exact SemTerminatesIn_mono_fuel hF_le (SemTerminatesIn_mono_deps hsub_p hterm)
      refine ⟨p_out, rfl, hact_dec, hp_rt, hready,
        hvc_cctx, hvc_code, htc_len, hvc_lt, hwf1, htc_inv, hes_inv, ?_, hroot_sem, ?_⟩
      · have hc_none := (hconserve none).1
        have hdrop_none : totalLiveContexts (some (some p_idx, c)) es ar none =
            totalLiveContexts none es ar none := by
          unfold totalLiveContexts inFlightContextsFor; simp
        rw [hdrop_none] at hroot_len
        exact hc_none.trans hroot_len
      · intro idx
        by_cases hidx_eq : idx = p_idx
        · subst idx
          right
          refine ⟨Or.inr h_none, ?_⟩
          have hc_p := (hconserve (some p_idx)).1
          have hp_rest0 : (totalLiveContexts none es ar (some p_idx)).length = 0 := by
            have h1 := congrArg List.length htl_singleton
            unfold totalLiveContexts inFlightContextsFor at h1 ⊢
            simp only [↓reduceIte, List.cons_append, List.nil_append, List.length_cons, List.length_nil] at h1 ⊢
            omega
          exact hc_p.trans hp_rest0
        · have hne_val : idx.val ≠ p_idx.val := fun hc => hidx_eq (usize_eq_of_val_eq hc)
          have hne_opt : (some p_idx : Option Std.Usize) ≠ some idx :=
            fun hc => hidx_eq (Option.some.inj hc).symm
          have hdrop_idx : totalLiveContexts (some (some p_idx, c)) es ar (some idx) =
              totalLiveContexts none es ar (some idx) := by
            unfold totalLiveContexts inFlightContextsFor; simp [hne_opt]
          rcases hslots idx with ⟨hlt_i, node_i, hget_i, hlen_i, hpos_i, hsem_i⟩ | ⟨hbad_i, hzero_i⟩
          · left
            rw [hdrop_idx] at hlen_i
            refine ⟨h_len ▸ hlt_i, node_i, (h_other idx.val hne_val).trans hget_i,
              (hconserve (some idx)).1.trans hlen_i, hpos_i, ?_⟩
            exact ActiveNodeSem_mono (Nat.le_refl _) hvc_lt (Nat.le_refl _)
              (fun z hz => (hconserve (some idx)).2 z (hdrop_idx ▸ hz)) hsem_i
          · right
            rw [hdrop_idx] at hzero_i
            refine ⟨?_, (hconserve (some idx)).1.trans hzero_i⟩
            rcases hbad_i with hge_i | hnone_i
            · exact Or.inl (h_len ▸ hge_i)
            · exact Or.inr ((h_other idx.val hne_val).trans hnone_i)

/-- Soundness of `terminate_context_chain` across arbitrarily deep parent-completion cascades. -/
theorem terminate_context_chain_sound
    {codeOut : List api.EbpfInstruction} {prog : Program} {env : ExecEnv}
    {cctx : verifier.CoreCallingContext} {code : alloc.vec.Vec api.EbpfInstruction}
    {ctxInit initial_current : verifier.ComputationContext}
    {current_deps : verifier.DataDependencies}
    {parent_idx : Option Std.Usize}
    {es : List verifier.WorklistEntry}
    {ar ar_out : verifier.ExplorerArena}
    {vctx vctx_out : verifier.VerificationContext}
    {M : Nat} {rootCompleted : Bool}
    (hdec : decodeVerifiedCode codeOut = some prog)
    (hprog_len : prog.size = code.val.length)
    (hcctx : CallingContextFullMatches cctx env)
    (hinit_valid : CompCtxValid env 0 ctxInit)
    (hinv : ExplorerInv prog env cctx code ctxInit
      (some (parent_idx, initial_current)) es ar vctx M rootCompleted)
    (hroot_cur : parent_idx = none → initial_current = ctxInit)
    (hready : NodeReadyToFinish prog env cctx code initial_current current_deps
      vctx.counter.bv.toNat M)
    (htc : verifier.terminate_context_chain initial_current current_deps parent_idx ar vctx =
      ok (.Ok (), ar_out, vctx_out)) :
    vctx_out.iteration = vctx.iteration ∧
    ∃ M_out rootCompleted_out,
      M_out + arenaActiveCount ar_out = M + arenaActiveCount ar + 1 ∧
      ExplorerInv prog env cctx code ctxInit none es ar_out vctx_out M_out rootCompleted_out := by
  have hvframe := terminate_context_chain_vctx_frame htc
  refine ⟨hvframe.2.2.2.1, ?_⟩
  unfold verifier.terminate_context_chain at htc
  rcases bind_eq_ok htc with ⟨pt, hloop, _⟩
  rcases pt with ⟨ar1, vc1, lerr⟩
  rw [bind_triple_ok_eq hloop] at htc
  cases lerr with
  | some _ => cases (Prod.mk.inj (ok_inj htc)).1
  | none =>
    rcases Prod.mk.inj (Prod.mk.inj (ok_inj htc)).2 with ⟨rfl, rfl⟩
    unfold verifier.terminate_context_chain_loop at hloop
    set K := M + arenaActiveCount ar
    have hmain := loop_ok_pre_post
      (fun (cd1, pi1, a1, vc_in, co1, le1) =>
        verifier.terminate_context_chain_loop.body cd1 pi1 a1 vc_in co1 le1)
      (fun s =>
        s.2.2.2.2.2 = none →
          match s.2.2.2.2.1 with
          | none =>
            ∃ M1 rc1, M1 + arenaActiveCount s.2.2.1 = K + 1 ∧
              ExplorerInv prog env cctx code ctxInit none es s.2.2.1 s.2.2.2.1 M1 rc1
          | some cur_c =>
            ∃ M1 rc1, M1 + arenaActiveCount s.2.2.1 = K ∧
              (s.2.1 = none → cur_c = ctxInit) ∧
              NodeReadyToFinish prog env cctx code cur_c s.1 s.2.2.2.1.counter.bv.toNat M1 ∧
              ExplorerInv prog env cctx code ctxInit (some (s.2.1, cur_c)) es s.2.2.1 s.2.2.2.1 M1 rc1)
      (fun s out =>
        (s.2.2.2.2.2 ≠ none → out.2.2 ≠ none) ∧
        (out.2.2 = none →
          ∃ M_o rc_o, M_o + arenaActiveCount out.1 = K + 1 ∧
            ExplorerInv prog env cctx code ctxInit none es out.1 out.2.1 M_o rc_o))
      (by
        rintro ⟨cd1, pi1, a1, vc_in, co1, le1⟩ ⟨cd2, pi2, a2, vc_nx, co2, le2⟩ ⟨ao, vco, leo⟩ hpre hbody
        dsimp only at hpre ⊢
        unfold verifier.terminate_context_chain_loop.body at hbody
        by_cases hb : core.option.Option.is_some co1 = true
        · simp only [hb, ↓reduceIte] at hbody
          by_cases hb1 : core.option.Option.is_none le1 = true
          · simp only [hb1, ↓reduceIte] at hbody
            have hle1 : le1 = none := by
              cases le1 <;> simp [core.option.Option.is_none] at hb1; rfl
            subst hle1
            cases co1 with
            | none => simp [core.option.Option.is_some] at hb
            | some cur_in =>
              obtain ⟨M1, rc1, hpot1, hrc_in, hready1, hinv1⟩ := hpre rfl
              simp only [core.mem.replace] at hbody
              rcases bind_eq_ok hbody with ⟨inst, hinst_get, hbody⟩
              rcases bind_eq_ok hbody with ⟨pv, hback_vis, _⟩
              rcases pv with ⟨r_vis, tracker, vc_vis⟩
              rw [bind_triple_ok_eq hback_vis] at hbody
              cases r_vis with
              | Err _ =>
                cases ok_inj hbody
                refine ⟨(fun h => by cases h), ?_⟩
                intro hIH
                exact ⟨fun h => False.elim (h rfl), fun hleo => False.elim ((hIH.1 (by simp)) hleo)⟩
              | Ok _ =>
                rcases hready1 with ⟨hpc_lt, hmatch_pc, vc0, vc1_f, inst_f, hc0, hcd0, hp0,
                  hctx0, hv1_le, hinst_f, hfwd, hsucc_v, hchildren_cur⟩
                have hinst_eq : inst = inst_f := by
                  have hget_in := (vec_index_usize_ok hinst_get).2
                  rw [hinv1.2.1] at hget_in
                  exact Option.some.inj (hget_in.symm.trans hinst_f)
                subst hinst_eq
                have hchildren_v1 : ∀ succ, some succ ∈ vc1_f.pending_successors.val →
                    SemTerminatesIn prog env vc1_f.counter.bv.toNat succ cd1 M1 := by
                  intro succ hsucc
                  obtain ⟨s', hs_eq, hs_v1⟩ := hsucc_v (some succ) hsucc
                  cases Option.some.inj hs_eq
                  exact SemTerminatesIn_change_vCounter hinv1.2.2.2.1 hs_v1 (hchildren_cur succ hsucc)
                obtain ⟨_, htrk_ctx, hvc_vis_eq, hsem_v0⟩ :=
                  visit_any_backward hdec ((congrArg (fun v => v.val.length) hcd0).trans hprog_len.symm) hpc_lt
                    (by simpa [hc0, hcd0] using hmatch_pc)
                    (by simpa [hc0] using hcctx)
                    (hinv1.1.trans hc0.symm)
                    hctx0
                    (Nat.lt_of_le_of_lt hv1_le hinv1.2.2.2.1)
                    hp0
                    (by simpa [hcd0] using hinst_f)
                    hfwd hchildren_v1 hback_vis
                subst vc_vis
                have hcur_valid_vc : CompCtxValid env vc_in.counter.bv.toNat cur_in := by
                  have h0_le : vc0.counter.bv.toNat ≤ vc1_f.counter.bv.toNat := by
                    exact (visit_any_forward (by simpa [hc0] using hcctx) hctx0 hp0 hfwd).2.1.2.2.2.2.1
                  exact CompCtxValid_mono (Nat.le_trans h0_le hv1_le) hctx0
                have hsem_cur : SemTerminatesIn prog env vc_in.counter.bv.toNat cur_in
                    tracker.deps (M1 + 1) :=
                  SemTerminatesIn_change_vCounter
                    (Nat.lt_of_le_of_lt
                      ((visit_any_forward (by simpa [hc0] using hcctx) hctx0 hp0 hfwd).2.1.2.2.2.2.1.trans hv1_le)
                      hinv1.2.2.2.1)
                    hcur_valid_vc hsem_v0
                rcases bind_eq_ok hbody with ⟨pmb, hmut_tc, _⟩
                rcases pmb with ⟨o_tc, back_tc⟩
                rw [bind_pair_ok_eq hmut_tc] at hbody
                obtain ⟨hlt_tc, hget_tc, hset_tc⟩ := vec_index_mut_usize_ok hmut_tc
                rcases bind_eq_ok hbody with ⟨pvn, hpvn, _⟩
                rcases pvn with ⟨v_tc, next_deps⟩
                rw [bind_pair_ok_eq hpvn] at hbody
                have htc_upd : v_tc.val.length = vc_in.terminating_contexts.val.length ∧
                    next_deps = tracker.deps ∧
                    (∀ tcOpt ∈ v_tc.val, ∀ tc, tcOpt = some tc →
                      TerminatingContextsInv prog env vc_in.counter.bv.toNat (M1 + 1) tc) := by
                  cases o_tc with
                  | none =>
                    rcases Prod.mk.inj (ok_inj hpvn) with ⟨rfl, rfl⟩
                    obtain ⟨hval_b, hlen_b⟩ := hset_tc none
                    refine ⟨hlen_b, rfl, fun tcOpt hmem tc hsome => ?_⟩
                    rw [hval_b] at hmem
                    rcases List.mem_iff_getElem.mp hmem with ⟨k, hk, hk_eq⟩
                    by_cases hkp : k = cur_in.pc.val
                    · subst hkp
                      have := list_getElem?_set_eq vc_in.terminating_contexts.val cur_in.pc.val none hlt_tc
                      rw [List.getElem?_eq_getElem hk, hk_eq, hsome] at this
                      cases Option.some.inj this
                    · have h1 := list_getElem?_set_of_ne vc_in.terminating_contexts.val cur_in.pc.val k none (Ne.symm hkp)
                      rw [List.getElem?_eq_getElem hk, hk_eq] at h1
                      have hlt_orig : k < vc_in.terminating_contexts.val.length := by
                        rw [List.length_set] at hk; exact hk
                      rw [List.getElem?_eq_getElem hlt_orig] at h1
                      have hmem_orig : tcOpt ∈ vc_in.terminating_contexts.val := by
                        rw [Option.some.inj h1]; exact List.getElem_mem hlt_orig
                      exact TerminatingContextsInv_mono (Nat.le_refl _) (by omega)
                        (hinv1.2.2.2.2.2.1 tcOpt hmem_orig tc hsome)
                  | some tc =>
                    dsimp only at hpvn
                    rcases bind_eq_ok hpvn with ⟨ctx_clr, hclr, hpvn⟩
                    rcases bind_eq_ok hpvn with ⟨s_ret, hret, hpvn⟩
                    rcases bind_eq_ok hpvn with ⟨tc1, hins_tc, hpvn⟩
                    rcases Prod.mk.inj (ok_inj hpvn) with ⟨rfl, rfl⟩
                    obtain ⟨hval_b, hlen_b⟩ := hset_tc (some tc1)
                    rw [htrk_ctx] at hclr
                    let st_dum : MachineState :=
                      { pc := 0, regs := fun _ => .scalar 0#64, stack := fun _ => .bytes 0#64 0#64,
                        world := { bufBytes := fun _ _ => 0#8, structBytes := fun _ _ => 0#8,
                                   structFieldNull := fun _ => false, arrayLen := fun _ => 0#64,
                                   stepCount := 0, liveResources := [], nextAllocId := 0 } }
                    have hclr_v : CompCtxValid env vc_in.counter.bv.toNat ctx_clr :=
                      (clear_dead_registers_valid_and_rel2 (ρ := id) (resActive := fun _ => false)
                        (isNullId := fun _ => false) (st₁ := st_dum) (st₂ := st_dum)
                        hclr hcur_valid_vc).1
                    have hret_v : CompCtxValid env vc_in.counter.bv.toNat { ctx_clr with stack := s_ret } :=
                      (StateRel2Deps_stack_retain (ρ := id) (resActive := fun _ => false)
                        (isNullId := fun _ => false) (st₁ := st_dum) (st₂ := st_dum)
                        hclr_v hret).1
                    have hsem_pruned : SemTerminatesIn prog env vc_in.counter.bv.toNat
                        { ctx_clr with stack := s_ret } tracker.deps (M1 + 1) := by
                      intro l₁ l₂ s₁ s₂ ρ resAct isNull st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel_p
                      have hrel1 := (StateRel2Deps_stack_retain hclr_v hret).2.mpr hrel_p
                      have hrel0 := (clear_dead_registers_valid_and_rel2 hclr hcur_valid_vc).2.mpr hrel1
                      exact hsem_cur l₁ l₂ s₁ s₂ ρ resAct isNull st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel0
                    have htc_old : TerminatingContextsInv prog env vc_in.counter.bv.toNat (M1 + 1) tc := by
                      have hmem_tc : some tc ∈ vc_in.terminating_contexts.val := by
                        rw [List.getElem?_eq_getElem hlt_tc] at hget_tc
                        rw [← Option.some.inj hget_tc]; exact List.getElem_mem hlt_tc
                      exact TerminatingContextsInv_mono (Nat.le_refl _) (by omega)
                        (hinv1.2.2.2.2.2.1 (some tc) hmem_tc tc rfl)
                    have htc1_inv := TerminatingContexts_insert_sound htc_old hret_v
                      (Nat.le_refl _) hsem_pruned hins_tc
                    refine ⟨hlen_b, rfl, fun tcOpt hmem tc_q hsome => ?_⟩
                    rw [hval_b] at hmem
                    rcases List.mem_iff_getElem.mp hmem with ⟨k, hk, hk_eq⟩
                    by_cases hkp : k = cur_in.pc.val
                    · subst hkp
                      have h1 := list_getElem?_set_eq vc_in.terminating_contexts.val cur_in.pc.val (some tc1) hlt_tc
                      rw [List.getElem?_eq_getElem hk, hk_eq, hsome] at h1
                      cases Option.some.inj (Option.some.inj h1)
                      exact htc1_inv
                    · have h1 := list_getElem?_set_of_ne vc_in.terminating_contexts.val cur_in.pc.val k (some tc1) (Ne.symm hkp)
                      rw [List.getElem?_eq_getElem hk, hk_eq] at h1
                      have hlt_orig : k < vc_in.terminating_contexts.val.length := by
                        rw [List.length_set] at hk; exact hk
                      rw [List.getElem?_eq_getElem hlt_orig] at h1
                      have hmem_orig : tcOpt ∈ vc_in.terminating_contexts.val := by
                        rw [Option.some.inj h1]; exact List.getElem_mem hlt_orig
                      exact TerminatingContextsInv_mono (Nat.le_refl _) (by omega)
                        (hinv1.2.2.2.2.2.1 tcOpt hmem_orig tc_q hsome)
                rcases htc_upd with ⟨htc_len_new, hnext_deps_eq, htc_inv_new⟩
                subst hnext_deps_eq
                let vc_upd : verifier.VerificationContext :=
                  { vc_in with terminating_contexts := v_tc }
                have hinv_m1 : ExplorerInv prog env cctx code ctxInit
                    (some (pi1, cur_in)) es a1 vc_upd (M1 + 1) rc1 := by
                  rcases hinv1 with ⟨hc1, hcd1, hlen1, hlt1, hwf1, _, hes1, hrl1, hrs1, hsl1⟩
                  refine ⟨hc1, hcd1, htc_len_new.trans hlen1, hlt1, hwf1, htc_inv_new, hes1,
                    hrl1, fun hr => ?_, fun idx => ?_⟩
                  · obtain ⟨dR, hdR⟩ := hrs1 hr
                    exact ⟨dR, SemTerminatesIn_mono_fuel (by omega) hdR⟩
                  · rcases hsl1 idx with ⟨hi_lt, nd, hi_get, hi_len, hi_pos, hi_sem⟩ | hfree
                    · exact Or.inl ⟨hi_lt, nd, hi_get, hi_len, hi_pos,
                        ActiveNodeSem_mono (Nat.le_refl _) hlt1 (by omega) (fun _ hz => hz) hi_sem⟩
                    · exact Or.inr hfree
                have hstep := complete_inflight_step hinit_valid hinv_m1 hrc_in (Nat.le_refl _) hsem_cur
                cases pi1 with
                | none =>
                  cases ok_inj hbody
                  rcases hstep with ⟨_, hinv_done⟩ | ⟨_, hne, _⟩
                  · refine ⟨fun _ => ⟨M1 + 1, true, by omega, hinv_done⟩, fun hIH => hIH⟩
                  · cases hne
                | some p_idx =>
                  dsimp only at hbody
                  rcases bind_eq_ok hbody with ⟨pcc, hcc, _⟩
                  rcases pcc with ⟨o2, ar_nx⟩
                  rw [bind_pair_ok_eq hcc] at hbody
                  rcases hstep with ⟨hne, _⟩ | ⟨p_idx', hp_eq, hcc_spec⟩
                  · cases hne
                  · cases Option.some.inj hp_eq
                    rcases hcc_spec o2 ar_nx hcc with ⟨rfl, hact_eq, hinv_none⟩ |
                        ⟨parent_node, rfl, hact_dec, hp_root, hp_ready, hinv_some⟩
                    · cases ok_inj hbody
                      exact ⟨fun _ => ⟨M1 + 1, rc1, by omega, hinv_none⟩, fun hIH => hIH⟩
                    · cases ok_inj hbody
                      exact ⟨fun _ => ⟨M1 + 1, rc1, by omega, hp_root, hp_ready, hinv_some⟩,
                        fun hIH => hIH⟩
          · simp only [hb1, Bool.false_eq_true, ↓reduceIte] at hbody
            cases ok_inj hbody
        · simp only [hb, Bool.false_eq_true, ↓reduceIte] at hbody
          cases ok_inj hbody)
      (by
        rintro ⟨cd1, pi1, a1, vc_in, co1, le1⟩ ⟨ao, vco, leo⟩ hpre hbody
        dsimp only at hpre ⊢
        unfold verifier.terminate_context_chain_loop.body at hbody
        by_cases hb : core.option.Option.is_some co1 = true
        · simp only [hb, ↓reduceIte] at hbody
          by_cases hb1 : core.option.Option.is_none le1 = true
          · simp only [hb1, ↓reduceIte] at hbody
            cases co1 with
            | none => simp [core.option.Option.is_some] at hb
            | some _ =>
              simp only [core.mem.replace] at hbody
              rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
              rcases bind_eq_ok hbody with ⟨pv, hvis, _⟩
              rcases pv with ⟨r_vis, _, _⟩
              rw [bind_triple_ok_eq hvis] at hbody
              cases r_vis with
              | Err _ => cases ok_inj hbody
              | Ok _ =>
                dsimp only at hbody
                rcases bind_eq_ok hbody with ⟨pmb, hmut_tc, _⟩
                rcases pmb with ⟨_, _⟩
                rw [bind_pair_ok_eq hmut_tc] at hbody
                rcases bind_eq_ok hbody with ⟨pvn, hpvn, _⟩
                rcases pvn with ⟨_, _⟩
                rw [bind_pair_ok_eq hpvn] at hbody
                cases pi1 with
                | none => cases ok_inj hbody
                | some _ =>
                  dsimp only at hbody
                  rcases bind_eq_ok hbody with ⟨pcc, hcc, _⟩
                  rcases pcc with ⟨o2, _⟩
                  rw [bind_pair_ok_eq hcc] at hbody
                  cases o2 <;> cases ok_inj hbody
          · simp only [hb1, Bool.false_eq_true, ↓reduceIte] at hbody
            cases ok_inj hbody
            have hne_le : le1 ≠ none := by
              intro hc; subst hc; simp [core.option.Option.is_none] at hb1
            exact ⟨fun _ => hne_le, fun hleo => False.elim (hne_le hleo)⟩
        · simp only [hb, Bool.false_eq_true, ↓reduceIte] at hbody
          cases ok_inj hbody
          have hco_none : co1 = none := by
            cases co1 <;> simp [core.option.Option.is_some] at hb; rfl
          subst hco_none
          exact ⟨id, hpre⟩)
      (current_deps, parent_idx, ar, vctx, some initial_current, none) (ar1, vc1, none)
      (fun _ => ⟨M, rootCompleted, rfl, hroot_cur, hready, hinv⟩) hloop
    exact hmain.2 rfl

/-- Single-step soundness of `verify_worklist_entry` when it returns `Ok ()`. -/
theorem verify_worklist_entry_sound
    {L : Type} (loggerInst : verifier.CoreVerifierLogger L)
    {codeOut : List api.EbpfInstruction} {prog : Program} {env : ExecEnv}
    {cctx : verifier.CoreCallingContext} {code : alloc.vec.Vec api.EbpfInstruction}
    {ctxInit : verifier.ComputationContext}
    {outVis : alloc.vec.Vec Bool}
    {entry : verifier.WorklistEntry}
    {wl1 wl2 : verifier.Worklist} {es_init : List verifier.WorklistEntry}
    {ar ar_out : verifier.ExplorerArena}
    {vis vis_out : alloc.vec.Vec Bool}
    {vctx vctx_out : verifier.VerificationContext}
    {logger logger_out : L}
    {M : Nat} {rootCompleted : Bool}
    (hdec : decodeVerifiedCode codeOut = some prog)
    (hprog_len : prog.size = code.val.length)
    (houtVis_match : ∀ k, k < code.val.length → outVis.val[k]? = some true →
      CodeMatchesProgAt cctx code.val prog k)
    (hcctx : CallingContextFullMatches cctx env)
    (hinit_valid : CompCtxValid env 0 ctxInit)
    (hrep_init : WorklistRep wl1 es_init)
    (hvis_len : vis.val.length = code.val.length)
    (hcnt_iter : vctx.counter.bv.toNat ≤ 3 * vctx.iteration.val)
    (hpot_iter : M + arenaActiveCount ar ≤ vctx.iteration.val)
    (hiter_max : vctx.iteration.val ≤ VERIFIER_MAX_STEPS)
    (hentry_valid : CompCtxValid env vctx.counter.bv.toNat entry.context)
    (hentry_root : entry.parent = none → entry.context = ctxInit)
    (hinv : ExplorerInv prog env cctx code ctxInit
      (some (entry.parent, entry.context)) es_init ar vctx M rootCompleted)
    (hw : verifier.verify_worklist_entry loggerInst entry wl1 ar vis vctx logger =
      ok (.Ok (), wl2, ar_out, vis_out, vctx_out, logger_out))
    (hvis_sub_out : ∀ (k : Nat), vis_out.val[k]? = some true → outVis.val[k]? = some true) :
    vis_out.val.length = code.val.length ∧
    (∀ (k : Nat), vis.val[k]? = some true → vis_out.val[k]? = some true) ∧
    vctx_out.counter.bv.toNat ≤ 3 * vctx_out.iteration.val ∧
    vctx_out.iteration.val ≤ VERIFIER_MAX_STEPS ∧
    ∃ es_out M_out rc_out,
      WorklistRep wl2 es_out ∧
      M_out + arenaActiveCount ar_out ≤ vctx_out.iteration.val ∧
      ExplorerInv prog env cctx code ctxInit none es_out ar_out vctx_out M_out rc_out := by
  unfold verifier.verify_worklist_entry at hw
  rcases bind_eq_ok hw with ⟨pruned_deps, hprune, hw⟩
  cases pruned_deps with
  | some ending_deps =>
    dsimp only at hw
    have hprune_sem : ∃ F_c, F_c ≤ M ∧
        SemTerminatesIn prog env vctx.counter.bv.toNat entry.context ending_deps F_c := by
      by_cases hpc_lt : entry.context.pc < alloc.vec.Vec.len vctx.code
      · simp only [hpc_lt, ↓reduceIte] at hprune
        rcases bind_eq_ok hprune with ⟨o_tc, htc_idx, hprune⟩
        cases o_tc with
        | none => cases ok_inj hprune
        | some tc =>
          have hmem_tc : some tc ∈ vctx.terminating_contexts.val := by
            obtain ⟨hlt, hget⟩ := vec_index_usize_ok htc_idx
            rw [List.getElem?_eq_getElem hlt] at hget
            rw [← Option.some.inj hget]; exact List.getElem_mem hlt
          have htc_i := hinv.2.2.2.2.2.1 (some tc) hmem_tc tc rfl
          exact find_greater_or_equal_deps_sound htc_i hentry_valid hprune
      · simp only [hpc_lt, ↓reduceIte] at hprune
        cases ok_inj hprune
    obtain ⟨F_c, hFc_le, hsem_c⟩ := hprune_sem
    have hstep := complete_inflight_step hinit_valid hinv hentry_root hFc_le hsem_c
    cases hp_e : entry.parent with
    | none =>
      rw [hp_e] at hw hstep
      dsimp only at hw
      rcases Prod.mk.inj (ok_inj hw) with ⟨_, hp⟩
      rcases Prod.mk.inj hp with ⟨hwl_eq, hp⟩
      rcases Prod.mk.inj hp with ⟨har_eq, hp⟩
      rcases Prod.mk.inj hp with ⟨hv_eq, hp⟩
      rcases Prod.mk.inj hp with ⟨hvc_eq, _⟩
      rcases hstep with ⟨_, hinv_done⟩ | ⟨_, hne, _⟩
      · exact ⟨hv_eq ▸ hvis_len, fun _ h => hv_eq ▸ h, hvc_eq ▸ hcnt_iter, hvc_eq ▸ hiter_max,
          es_init, M, true, hwl_eq ▸ hrep_init, har_eq ▸ hvc_eq ▸ hpot_iter,
          har_eq ▸ hvc_eq ▸ hinv_done⟩
      · cases hne
    | some p_idx =>
      rw [hp_e] at hw hstep
      dsimp only at hw
      rcases bind_eq_ok hw with ⟨pcc, hcc, _⟩
      rcases pcc with ⟨o_p, ar1⟩
      rw [bind_pair_ok_eq hcc] at hw
      rcases hstep with ⟨hne, _⟩ | ⟨p_idx', hp_eq, hcc_spec⟩
      · cases hne
      · cases Option.some.inj hp_eq
        rcases hcc_spec o_p ar1 hcc with ⟨rfl, hact_eq, hinv_none⟩ |
            ⟨parent_node, rfl, hact_dec, hp_root, hp_ready, hinv_some⟩
        · dsimp only at hw
          rcases Prod.mk.inj (ok_inj hw) with ⟨_, hp⟩
          rcases Prod.mk.inj hp with ⟨hwl_eq, hp⟩
          rcases Prod.mk.inj hp with ⟨har_eq, hp⟩
          rcases Prod.mk.inj hp with ⟨hv_eq, hp⟩
          rcases Prod.mk.inj hp with ⟨hvc_eq, _⟩
          exact ⟨hv_eq ▸ hvis_len, fun _ h => hv_eq ▸ h, hvc_eq ▸ hcnt_iter, hvc_eq ▸ hiter_max,
            es_init, M, rootCompleted, hwl_eq ▸ hrep_init,
            by rw [← har_eq, ← hvc_eq]; omega, har_eq ▸ hvc_eq ▸ hinv_none⟩
        · dsimp only at hw
          rcases bind_eq_ok hw with ⟨ptc, htc, _⟩
          rcases ptc with ⟨r_tc, ar2, vc_tc⟩
          rw [bind_triple_ok_eq htc] at hw
          cases r_tc with
          | Err _ =>
            dsimp only at hw
            rcases bind_eq_ok hw with ⟨pel, hel, _⟩
            rcases pel with ⟨r1, _⟩
            rw [bind_pair_ok_eq hel] at hw
            have hr1 := error_and_log_is_err hel
            subst hr1
            cases (Prod.mk.inj (ok_inj hw)).1
          | Ok _ =>
            dsimp only at hw
            rcases Prod.mk.inj (ok_inj hw) with ⟨_, hp⟩
            rcases Prod.mk.inj hp with ⟨hwl_eq, hp⟩
            rcases Prod.mk.inj hp with ⟨har_eq, hp⟩
            rcases Prod.mk.inj hp with ⟨hv_eq, hp⟩
            rcases Prod.mk.inj hp with ⟨hvc_eq, _⟩
            have hfr := terminate_context_chain_vctx_frame htc
            obtain ⟨_, M_out, rc_out, hpot_out, hinv_out⟩ :=
              terminate_context_chain_sound hdec hprog_len hcctx hinit_valid
                hinv_some hp_root hp_ready htc
            exact ⟨hv_eq ▸ hvis_len, fun _ h => hv_eq ▸ h,
              by rw [← hvc_eq, hfr.2.2.1, hfr.2.2.2.1]; exact hcnt_iter,
              by rw [← hvc_eq, hfr.2.2.2.1]; exact hiter_max,
              es_init, M_out, rc_out, hwl_eq ▸ hrep_init,
              by rw [← har_eq, ← hvc_eq, hfr.2.2.2.1]; omega,
              har_eq ▸ hvc_eq ▸ hinv_out⟩
  | none =>
    dsimp only at hw
    rw [bind_ok_eq bpf_max_steps_mul_eq_ok] at hw
    by_cases hgt_iter : vctx.iteration > 655360#usize
    · simp only [hgt_iter, ↓reduceIte] at hw
      rcases bind_eq_ok hw with ⟨pel, hel, _⟩
      rcases pel with ⟨r1, _⟩
      rw [bind_pair_ok_eq hel] at hw
      have hr1 := error_and_log_is_err hel
      subst hr1
      cases (Prod.mk.inj (ok_inj hw)).1
    · simp only [hgt_iter, ↓reduceIte, lift_eq_ok, bind_ok] at hw
      have hiter_le_655360 : vctx.iteration.val ≤ 655360 := by
        have hn : ¬ ((655360#usize).val < vctx.iteration.val) := hgt_iter
        rw [usize_655360_val] at hn; omega
      have hng_pd : ¬ (entry.context.path_depth > UScalar.cast .U32 api.BPF_MAX_INSTS) := fun hc => by
        simp only [hc, ↓reduceIte] at hw
        rcases bind_eq_ok hw with ⟨⟨r1, _⟩, hel, hw⟩
        cases (Prod.mk.inj (ok_inj hw)).1 ▸ error_and_log_is_err hel
      simp only [hng_pd, ↓reduceIte] at hw
      have hng_js : ¬ (entry.context.jmp_seq > verifier.MAX_JMP_SEQ) := fun hc => by
        simp only [hc, ↓reduceIte] at hw
        rcases bind_eq_ok hw with ⟨⟨r1, _⟩, hel, hw⟩
        cases (Prod.mk.inj (ok_inj hw)).1 ▸ error_and_log_is_err hel
      simp only [hng_js, ↓reduceIte] at hw
      have hng_wl : ¬ (wl1.len > verifier.MAX_PENDING_STATES) := fun hc => by
        simp only [hc, ↓reduceIte] at hw
        rcases bind_eq_ok hw with ⟨⟨r1, _⟩, hel, hw⟩
        cases (Prod.mk.inj (ok_inj hw)).1 ▸ error_and_log_is_err hel
      simp only [hng_wl, ↓reduceIte] at hw
      by_cases hpc_ge : entry.context.pc >= alloc.vec.Vec.len vctx.code
      · simp only [hpc_ge, ↓reduceIte] at hw
        rcases bind_eq_ok hw with ⟨pel, hel, _⟩
        rcases pel with ⟨r1, _⟩
        rw [bind_pair_ok_eq hel] at hw
        have hr1 := error_and_log_is_err hel
        subst hr1
        cases (Prod.mk.inj (ok_inj hw)).1
      · simp only [hpc_ge, ↓reduceIte] at hw
        have hpc_lt_code : entry.context.pc.val < code.val.length := by
          have hn : ¬ (vctx.code.val.length ≤ entry.context.pc.val) := hpc_ge
          rw [hinv.2.1] at hn; omega
        rcases bind_eq_ok hw with ⟨pmb, hmut_vis, _⟩
        rcases pmb with ⟨_, back_vis⟩
        rw [bind_pair_ok_eq hmut_vis] at hw
        obtain ⟨hpc_lt_vis, _, hset_vis⟩ := vec_index_mut_usize_ok hmut_vis
        obtain ⟨hvis_set_val, hvis_set_len⟩ := hset_vis true
        rcases bind_eq_ok hw with ⟨b_en, _, hw⟩
        rcases bind_eq_ok hw with ⟨inst, hinst_idx, hw⟩
        obtain ⟨_, hinst_at⟩ := vec_index_usize_ok hinst_idx
        rcases bind_eq_ok hw with ⟨pvis, hvisit, _⟩
        rcases pvis with ⟨visit_res, ctx_after, vc_vis⟩
        rw [bind_triple_ok_eq hvisit] at hw
        obtain ⟨logger_mid, hw⟩ :=
          pending_log_step_eq loggerInst vc_vis.pending_log (back_vis true) logger _ hw
        cases visit_res with
        | Err _ =>
          dsimp only at hw
          rcases bind_eq_ok hw with ⟨pel, hel, _⟩
          rcases pel with ⟨r1, _⟩
          rw [bind_pair_ok_eq hel] at hw
          have hr1 := error_and_log_is_err hel
          subst hr1
          cases (Prod.mk.inj (ok_inj hw)).1
        | Ok _ =>
          dsimp only at hw
          rcases bind_eq_ok hw with ⟨i_pend, _, hw⟩
          have hng_pend : ¬ (i_pend > verifier.MAX_PENDING_STATES) := fun hc => by
            simp only [hc, ↓reduceIte] at hw
            rcases bind_eq_ok hw with ⟨⟨r1, _⟩, hel, hw⟩
            cases (Prod.mk.inj (ok_inj hw)).1 ▸ error_and_log_is_err hel
          simp only [hng_pend, ↓reduceIte] at hw
          let vctx0 : verifier.VerificationContext :=
            { vctx with
              log_enabled := b_en
              pending_log := none
              pending_successors := alloc.vec.Vec.new (Option verifier.ComputationContext) }
          obtain ⟨rfl, hpost, hcnt_step, _, _⟩ :=
            visit_any_forward (vctx0 := vctx0) (by simpa [vctx0, hinv.1] using hcctx)
              hentry_valid rfl hvisit
          have hcnt_vis_lt : vc_vis.counter.bv.toNat < 2 ^ 63 := by
            change vc_vis.counter.bv.toNat ≤ vctx.counter.bv.toNat + 3 at hcnt_step
            omega
          have hvis_mono : ∀ (k : Nat), vis.val[k]? = some true → (back_vis true).val[k]? = some true := by
            intro k hk_t
            rw [hvis_set_val]
            by_cases hk_eq : k = entry.context.pc.val
            · subst hk_eq; exact list_getElem?_set_eq vis.val entry.context.pc.val true hpc_lt_vis
            · rw [list_getElem?_set_of_ne vis.val entry.context.pc.val k true (Ne.symm hk_eq)]
              exact hk_t
          have hmatch_pc : CodeMatchesProgAt cctx code.val prog entry.context.pc.val := by
            have hpc_vis_true : (back_vis true).val[entry.context.pc.val]? = some true := by
              rw [hvis_set_val]
              exact list_getElem?_set_eq vis.val entry.context.pc.val true hpc_lt_vis
            by_cases hns0 : alloc.vec.Vec.len vc_vis.pending_successors > 0#usize
            · simp only [hns0, ↓reduceIte] at hw
              rw [bind_ok_eq DataDependencies_default_zero] at hw
              rcases bind_eq_ok hw with ⟨⟨_, _⟩, _, hw⟩
              rcases bind_eq_ok hw with ⟨_, _, hw⟩
              rcases bind_eq_ok hw with ⟨_, _, hw⟩
              rcases Prod.mk.inj (Prod.mk.inj (Prod.mk.inj (Prod.mk.inj (ok_inj hw)).2).2).2 with ⟨hvo, _⟩
              exact houtVis_match entry.context.pc.val hpc_lt_code
                (hvis_sub_out entry.context.pc.val (hvo ▸ hpc_vis_true))
            · simp only [hns0, ↓reduceIte] at hw
              rw [bind_ok_eq DataDependencies_default_zero] at hw
              rcases bind_eq_ok hw with ⟨⟨r_t, _, _⟩, _, hw⟩
              cases r_t with
              | Err _ =>
                rcases bind_eq_ok hw with ⟨⟨r1, _⟩, hel, hw⟩
                cases (Prod.mk.inj (ok_inj hw)).1 ▸ error_and_log_is_err hel
              | Ok _ =>
                rcases bind_eq_ok hw with ⟨_, _, hw⟩
                rcases Prod.mk.inj (Prod.mk.inj (Prod.mk.inj (Prod.mk.inj (ok_inj hw)).2).2).2 with ⟨hvo, _⟩
                exact houtVis_match entry.context.pc.val hpc_lt_code
                  (hvis_sub_out entry.context.pc.val (hvo ▸ hpc_vis_true))
          by_cases hns : alloc.vec.Vec.len vc_vis.pending_successors > 0#usize
          · simp only [hns, ↓reduceIte] at hw
            rw [bind_ok_eq DataDependencies_default_zero] at hw
            rcases bind_eq_ok hw with ⟨par, har_push, _⟩
            rcases par with ⟨node_idx, ar1⟩
            rw [bind_pair_ok_eq har_push] at hw
            rcases bind_eq_ok hw with ⟨wl_p, hwl_loop, hw⟩
            rcases bind_eq_ok hw with ⟨i1, hadd_i, hw⟩
            have hi1_val : i1.val = vctx.iteration.val + 1 := by
              have := (uscalar_add_ok_val hadd_i).1
              rw [hpost.2.2.1, usize_one_val] at this; exact this
            rcases Prod.mk.inj (ok_inj hw) with ⟨_, hp⟩
            rcases Prod.mk.inj hp with ⟨hwl_eq, hp⟩
            rcases Prod.mk.inj hp with ⟨har_eq, hp⟩
            rcases Prod.mk.inj hp with ⟨hv_eq, hp⟩
            rcases Prod.mk.inj hp with ⟨hvc_eq, _⟩
            rw [← hwl_eq, ← har_eq, ← hv_eq, ← hvc_eq]
            obtain ⟨succs, hsuccs_eq, hsuccs_v⟩ :=
              opt_list_eq_map_some vc_vis.pending_successors.val hpost.2.2.2.2.2.2
            have hnum_eq : (alloc.vec.Vec.len vc_vis.pending_successors).val = succs.length := by
              change vc_vis.pending_successors.val.length = succs.length
              rw [hsuccs_eq, List.length_map]
            have hnum_pos : 1 ≤ (alloc.vec.Vec.len vc_vis.pending_successors).val := by
              have : (0#usize).val < (alloc.vec.Vec.len vc_vis.pending_successors).val := hns
              rw [usize_const_val_0] at this; omega
            have hrep_p := verify_worklist_entry_loop_spec hrep_init hsuccs_eq hnum_eq hwl_loop
            obtain ⟨hwf1, hwas_free, hidx_lt1, hidx_get1, h_other1, hact_inc⟩ :=
              ExplorerArena_push_spec hinv.2.2.2.2.1 har_push
            have hconserve_p := arenaContextsFor_push_conserve (es := es_init) hinv.2.2.2.2.1 har_push
            let es_out := es_init ++ succs.map (fun s => { context := s, parent := some node_idx })
            refine ⟨hvis_set_len.trans hvis_len, hvis_mono, ?_, ?_,
              es_out, M, rootCompleted, hrep_p, ?_, ?_⟩
            · dsimp only
              change vc_vis.counter.bv.toNat ≤ vctx.counter.bv.toNat + 3 at hcnt_step
              rw [hi1_val]; omega
            · dsimp only; unfold VERIFIER_MAX_STEPS; rw [hi1_val]; omega
            · dsimp only; rw [hact_inc, hi1_val]; omega
            · rcases hinv with ⟨hc_i, hcd_i, htl_i, _, _, htc_i, hes_i, hrl_i, hrs_i, hsl_i⟩
              refine ⟨hpost.1.trans hc_i, hpost.2.1.trans hcd_i,
                by dsimp only; rw [hpost.2.2.2.1]; exact htl_i,
                hcnt_vis_lt, hwf1, ?_, ?_, ?_, hrs_i, ?_⟩
              · intro tcOpt hmem tc hsome
                dsimp only at hmem
                rw [hpost.2.2.2.1] at hmem
                exact TerminatingContextsInv_mono hpost.2.2.2.2.1 (Nat.le_refl _)
                  (htc_i tcOpt hmem tc hsome)
              · intro e he
                rw [List.mem_append] at he
                rcases he with he1 | he2
                · exact ⟨CompCtxValid_mono hpost.2.2.2.2.1 (hes_i e he1).1, (hes_i e he1).2⟩
                · rw [List.mem_map] at he2
                  rcases he2 with ⟨s, hs_mem, rfl⟩
                  exact ⟨hsuccs_v s hs_mem, fun hc => by cases hc⟩
              · have hwl_none : worklistContextsFor es_out none = worklistContextsFor es_init none := by
                  simp [es_out, worklistContextsFor_append, worklistContextsFor_children_ne]
                have htl_none : (totalLiveContexts none es_out ar1 none).length =
                    (totalLiveContexts none es_init ar1 none).length := by
                  unfold totalLiveContexts; rw [hwl_none]
                exact htl_none.trans ((hconserve_p none).1.trans hrl_i)
              · intro idx
                by_cases hidx_eq : idx = node_idx
                · subst idx
                  left
                  have hold_zero : (totalLiveContexts (some (entry.parent, entry.context))
                      es_init ar (some node_idx)).length = 0 := by
                    rcases hsl_i node_idx with ⟨_, n0, hget0, _, _⟩ | ⟨_, hz0⟩
                    · exact False.elim (hwas_free n0 hget0)
                    · exact hz0
                  have hmid_zero : (totalLiveContexts none es_init ar1 (some node_idx)).length = 0 :=
                    (hconserve_p (some node_idx)).1.trans hold_zero
                  have hnew_tl : totalLiveContexts none es_out ar1 (some node_idx) = succs := by
                    have hmid_nil : totalLiveContexts none es_init ar1 (some node_idx) = [] :=
                      List.eq_nil_of_length_eq_zero hmid_zero
                    unfold totalLiveContexts at hmid_nil ⊢
                    simp only [inFlightContextsFor, List.nil_append, List.append_eq_nil_iff] at hmid_nil ⊢
                    rw [worklistContextsFor_append, hmid_nil.1, hmid_nil.2,
                        worklistContextsFor_children_eq, List.nil_append, List.append_nil]
                  refine ⟨hidx_lt1, _, hidx_get1, by rw [hnew_tl, ← hnum_eq], hnum_pos, ?_⟩
                  refine ⟨hentry_root, hprog_len ▸ hpc_lt_code, hmatch_pc,
                    vctx0, vc_vis, inst, hc_i, hcd_i, rfl, hentry_valid,
                    Nat.le_refl _, by dsimp only; rw [← hcd_i]; exact hinst_at,
                    hvisit, hpost.2.2.2.2.2.2, fun succ hsucc => ?_⟩
                  left
                  rw [hnew_tl]
                  rw [hsuccs_eq, List.mem_map] at hsucc
                  rcases hsucc with ⟨s', hs'_mem, hs'_eq⟩
                  cases Option.some.inj hs'_eq
                  exact hs'_mem
                · have hne_val : idx.val ≠ node_idx.val := fun hc => hidx_eq (usize_eq_of_val_eq hc)
                  have hne_opt : (some node_idx : Option Std.Usize) ≠ some idx :=
                    fun hc => hidx_eq (Option.some.inj hc).symm
                  have hwl_same : worklistContextsFor es_out (some idx) =
                      worklistContextsFor es_init (some idx) := by
                    simp [es_out, worklistContextsFor_append,
                      worklistContextsFor_children_ne succs hne_opt]
                  have htl_eq_mid : totalLiveContexts none es_out ar1 (some idx) =
                      totalLiveContexts none es_init ar1 (some idx) := by
                    unfold totalLiveContexts; rw [hwl_same]
                  rcases hsl_i idx with ⟨hi_lt, nd, hi_get, hi_len, hi_pos, hi_sem⟩ | ⟨hbad, hz0⟩
                  · left
                    have hi_get1 : ar1.nodes.val[idx.val]? = some (some nd) :=
                      (h_other1 idx.val hne_val).trans hi_get
                    have hi_lt1 : idx.val < ar1.nodes.val.length := by
                      by_contra hge
                      rw [List.getElem?_eq_none (Nat.not_lt.mp hge)] at hi_get1
                      cases hi_get1
                    refine ⟨hi_lt1, nd, hi_get1,
                      (congrArg List.length htl_eq_mid).trans ((hconserve_p (some idx)).1.trans hi_len),
                      hi_pos, ?_⟩
                    exact ActiveNodeSem_mono hpost.2.2.2.2.1 hcnt_vis_lt (Nat.le_refl _)
                      (fun z hz => htl_eq_mid ▸ (hconserve_p (some idx)).2 z hz) hi_sem
                  · right
                    refine ⟨?_, (congrArg List.length htl_eq_mid).trans ((hconserve_p (some idx)).1.trans hz0)⟩
                    by_cases hlt1 : idx.val < ar1.nodes.val.length
                    · right
                      rcases hbad with hge_old | hnone_old
                      · have := h_other1 idx.val hne_val
                        rw [List.getElem?_eq_none hge_old] at this
                        rw [List.getElem?_eq_getElem hlt1] at this; cases this
                      · exact (h_other1 idx.val hne_val).trans hnone_old
                    · exact Or.inl (Nat.not_lt.mp hlt1)
          · simp only [hns, ↓reduceIte] at hw
            have hsucc_nil : vc_vis.pending_successors.val = [] := by
              apply List.eq_nil_of_length_eq_zero
              have hn : ¬ (0 < vc_vis.pending_successors.val.length) := hns
              omega
            rw [bind_ok_eq DataDependencies_default_zero] at hw
            rcases bind_eq_ok hw with ⟨ptc, htc, _⟩
            rcases ptc with ⟨r_tc, ar1, vc_tc⟩
            rw [bind_triple_ok_eq htc] at hw
            cases r_tc with
            | Err _ =>
              dsimp only at hw
              rcases bind_eq_ok hw with ⟨pel, hel, _⟩
              rcases pel with ⟨r1, _⟩
              rw [bind_pair_ok_eq hel] at hw
              have hr1 := error_and_log_is_err hel
              subst hr1
              cases (Prod.mk.inj (ok_inj hw)).1
            | Ok _ =>
              dsimp only at hw
              rcases bind_eq_ok hw with ⟨i1, hadd_i, hw⟩
              rcases Prod.mk.inj (ok_inj hw) with ⟨_, hp⟩
              rcases Prod.mk.inj hp with ⟨hwl_eq, hp⟩
              rcases Prod.mk.inj hp with ⟨har_eq, hp⟩
              rcases Prod.mk.inj hp with ⟨hv_eq, hp⟩
              rcases Prod.mk.inj hp with ⟨hvc_eq, _⟩
              rw [← hwl_eq, ← har_eq, ← hv_eq, ← hvc_eq]
              let vc_pre : verifier.VerificationContext := { vc_vis with pending_log := none }
              have hinv_pre : ExplorerInv prog env cctx code ctxInit
                  (some (entry.parent, entry.context)) es_init ar vc_pre M rootCompleted := by
                rcases hinv with ⟨hc_i, hcd_i, htl_i, _, hwf_i, htc_i, hes_i, hrl_i, hrs_i, hsl_i⟩
                refine ⟨hpost.1.trans hc_i, hpost.2.1.trans hcd_i,
                  (congrArg (fun v => v.val.length) hpost.2.2.2.1).trans htl_i, hcnt_vis_lt, hwf_i,
                  fun tcOpt hmem tc hsome =>
                    TerminatingContextsInv_mono hpost.2.2.2.2.1 (Nat.le_refl _)
                      (htc_i tcOpt (hpost.2.2.2.1 ▸ hmem) tc hsome),
                  fun e he => ⟨CompCtxValid_mono hpost.2.2.2.2.1 (hes_i e he).1, (hes_i e he).2⟩,
                  hrl_i, hrs_i, fun idx => ?_⟩
                rcases hsl_i idx with ⟨hi_lt, nd, hi_get, hi_len, hi_pos, hi_sem⟩ | hfree
                · exact Or.inl ⟨hi_lt, nd, hi_get, hi_len, hi_pos,
                    ActiveNodeSem_mono hpost.2.2.2.2.1 hcnt_vis_lt (Nat.le_refl _) (fun _ hz => hz) hi_sem⟩
                · exact Or.inr hfree
              have hready0 : NodeReadyToFinish prog env cctx code entry.context
                  { registers := 0#u16, stack := 0#u64 } vc_pre.counter.bv.toNat M := by
                refine ⟨hprog_len ▸ hpc_lt_code, hmatch_pc, vctx0, vc_vis, inst,
                  hinv.1, hinv.2.1, rfl, hentry_valid, Nat.le_refl _,
                  by rw [← hinv.2.1]; exact hinst_at,
                  hvisit, hpost.2.2.2.2.2.2, fun succ hsucc => ?_⟩
                rw [hsucc_nil] at hsucc; cases hsucc
              have hfr := terminate_context_chain_vctx_frame htc
              obtain ⟨_, M_out, rc_out, hpot_out, hinv_out⟩ :=
                terminate_context_chain_sound hdec hprog_len hcctx hinit_valid
                  hinv_pre hentry_root hready0 htc
              have hi1_val : i1.val = vctx.iteration.val + 1 := by
                have := (uscalar_add_ok_val hadd_i).1
                rw [hfr.2.2.2.1, usize_one_val] at this
                simpa [vc_pre, hpost.2.2.1] using this
              refine ⟨hvis_set_len.trans hvis_len, hvis_mono, ?_, ?_,
                es_init, M_out, rc_out, hrep_init, ?_, ?_⟩
              · dsimp only
                rw [hfr.2.2.1, hi1_val]
                dsimp only [vc_pre]
                change vc_vis.counter.bv.toNat ≤ vctx.counter.bv.toNat + 3 at hcnt_step
                omega
              · dsimp only; unfold VERIFIER_MAX_STEPS; rw [hi1_val]; omega
              · dsimp only; rw [hi1_val]; omega
              · rcases hinv_out with ⟨hc_o, hcd_o, htl_o, hlt_o, hwf_o, htc_o, hes_o, hrl_o, hrs_o, hsl_o⟩
                exact ⟨hc_o, hcd_o, htl_o, hlt_o, hwf_o, htc_o, hes_o, hrl_o, hrs_o, hsl_o⟩

private theorem totalLiveContexts_pop_conserve
    (es_init : List verifier.WorklistEntry) (entry : verifier.WorklistEntry)
    (ar : verifier.ExplorerArena) (p : Option Std.Usize) :
    (totalLiveContexts (some (entry.parent, entry.context)) es_init ar p).length =
      (totalLiveContexts none (es_init ++ [entry]) ar p).length ∧
    (∀ z, z ∈ totalLiveContexts none (es_init ++ [entry]) ar p →
      z ∈ totalLiveContexts (some (entry.parent, entry.context)) es_init ar p) := by
  unfold totalLiveContexts
  rw [worklistContextsFor_append, worklistContextsFor_singleton]
  have hnil : inFlightContextsFor none p = [] := rfl
  rw [hnil, List.nil_append]
  refine ⟨by simp [List.length_append]; omega, fun z hz => ?_⟩
  simp only [List.mem_append] at hz ⊢
  rcases hz with (h1 | h2) | h3
  · exact Or.inl (Or.inr h1)
  · exact Or.inl (Or.inl h2)
  · exact Or.inr h3

private theorem verify_worklist_entry_visited_mono
    {L : Type} (loggerInst : verifier.CoreVerifierLogger L)
    {entry : verifier.WorklistEntry} {wl1 wl2 : verifier.Worklist}
    {ar ar_out : verifier.ExplorerArena} {vis vis_out : alloc.vec.Vec Bool}
    {vctx vctx_out : verifier.VerificationContext} {logger logger_out : L}
    (hw : verifier.verify_worklist_entry loggerInst entry wl1 ar vis vctx logger =
      ok (.Ok (), wl2, ar_out, vis_out, vctx_out, logger_out)) :
    ∀ (k : Nat), vis.val[k]? = some true → vis_out.val[k]? = some true := by
  unfold verifier.verify_worklist_entry at hw
  dsimp (config := { zetaHave := true }) only at hw
  rcases bind_eq_ok hw with ⟨pruned_deps, _, hw⟩
  cases pruned_deps with
  | some _ =>
    dsimp only at hw
    cases hp_e : entry.parent with
    | none =>
      rw [hp_e] at hw
      dsimp only at hw
      rcases Prod.mk.inj (ok_inj hw) with ⟨_, hp⟩
      rcases Prod.mk.inj hp with ⟨_, hp⟩
      rcases Prod.mk.inj hp with ⟨_, hp⟩
      rcases Prod.mk.inj hp with ⟨rfl, _⟩
      exact fun _ h => h
    | some _ =>
      rw [hp_e] at hw
      dsimp only at hw
      rcases bind_eq_ok hw with ⟨pcc, hcc, _⟩
      rcases pcc with ⟨o_p, _⟩
      rw [bind_pair_ok_eq hcc] at hw
      cases o_p with
      | none =>
        rcases Prod.mk.inj (ok_inj hw) with ⟨_, hp⟩
        rcases Prod.mk.inj hp with ⟨_, hp⟩
        rcases Prod.mk.inj hp with ⟨_, hp⟩
        rcases Prod.mk.inj hp with ⟨rfl, _⟩
        exact fun _ h => h
      | some _ =>
        dsimp only at hw
        rcases bind_eq_ok hw with ⟨ptc, htc, _⟩
        rcases ptc with ⟨r_tc, _, _⟩
        rw [bind_triple_ok_eq htc] at hw
        cases r_tc with
        | Err _ =>
          dsimp only at hw
          rcases bind_eq_ok hw with ⟨pel, hel, _⟩
          rcases pel with ⟨r1, _⟩
          rw [bind_pair_ok_eq hel] at hw
          have hr1 := error_and_log_is_err hel
          subst hr1
          cases (Prod.mk.inj (ok_inj hw)).1
        | Ok _ =>
          rcases Prod.mk.inj (ok_inj hw) with ⟨_, hp⟩
          rcases Prod.mk.inj hp with ⟨_, hp⟩
          rcases Prod.mk.inj hp with ⟨_, hp⟩
          rcases Prod.mk.inj hp with ⟨rfl, _⟩
          exact fun _ h => h
  | none =>
    dsimp (config := { zetaHave := true }) only at hw
    rw [bind_ok_eq bpf_max_steps_mul_eq_ok] at hw
    by_cases hgt_iter : vctx.iteration > 655360#usize
    · simp only [hgt_iter, ↓reduceIte] at hw
      rcases bind_eq_ok hw with ⟨pel, hel, _⟩
      rcases pel with ⟨r1, _⟩
      rw [bind_pair_ok_eq hel] at hw
      have hr1 := error_and_log_is_err hel
      subst hr1
      cases (Prod.mk.inj (ok_inj hw)).1
    · simp only [hgt_iter, ↓reduceIte, lift_eq_ok, bind_ok] at hw
      have hng_pd : ¬ (entry.context.path_depth > UScalar.cast .U32 api.BPF_MAX_INSTS) := fun hc => by
        simp only [hc, ↓reduceIte] at hw
        rcases bind_eq_ok hw with ⟨⟨r1, _⟩, hel, hw⟩
        cases (Prod.mk.inj (ok_inj hw)).1 ▸ error_and_log_is_err hel
      simp only [hng_pd, ↓reduceIte] at hw
      have hng_js : ¬ (entry.context.jmp_seq > verifier.MAX_JMP_SEQ) := fun hc => by
        simp only [hc, ↓reduceIte] at hw
        rcases bind_eq_ok hw with ⟨⟨r1, _⟩, hel, hw⟩
        cases (Prod.mk.inj (ok_inj hw)).1 ▸ error_and_log_is_err hel
      simp only [hng_js, ↓reduceIte] at hw
      have hng_wl : ¬ (wl1.len > verifier.MAX_PENDING_STATES) := fun hc => by
        simp only [hc, ↓reduceIte] at hw
        rcases bind_eq_ok hw with ⟨⟨r1, _⟩, hel, hw⟩
        cases (Prod.mk.inj (ok_inj hw)).1 ▸ error_and_log_is_err hel
      simp only [hng_wl, ↓reduceIte] at hw
      by_cases hpc_ge : entry.context.pc >= alloc.vec.Vec.len vctx.code
      · simp only [hpc_ge, ↓reduceIte] at hw
        rcases bind_eq_ok hw with ⟨pel, hel, _⟩
        rcases pel with ⟨r1, _⟩
        rw [bind_pair_ok_eq hel] at hw
        have hr1 := error_and_log_is_err hel
        subst hr1
        cases (Prod.mk.inj (ok_inj hw)).1
      · simp only [hpc_ge, ↓reduceIte] at hw
        rcases bind_eq_ok hw with ⟨pmb, hmut_vis, _⟩
        rcases pmb with ⟨_, back_vis⟩
        rw [bind_pair_ok_eq hmut_vis] at hw
        obtain ⟨hpc_lt_vis, _, hset_vis⟩ := vec_index_mut_usize_ok hmut_vis
        obtain ⟨hvis_set_val, _⟩ := hset_vis true
        have hback_mono : ∀ (k : Nat), vis.val[k]? = some true → (back_vis true).val[k]? = some true := by
          intro k hk
          rw [hvis_set_val]
          by_cases heq : k = entry.context.pc.val
          · subst heq; exact list_getElem?_set_eq vis.val _ true hpc_lt_vis
          · rw [list_getElem?_set_of_ne vis.val _ k true (Ne.symm heq)]; exact hk
        rcases bind_eq_ok hw with ⟨_, _, hw⟩
        rcases bind_eq_ok hw with ⟨_, _, hw⟩
        rcases bind_eq_ok hw with ⟨pvis, hvisit, _⟩
        rcases pvis with ⟨visit_res, _, vc_vis⟩
        rw [bind_triple_ok_eq hvisit] at hw
        obtain ⟨logger_mid, hw⟩ :=
          pending_log_step_eq loggerInst vc_vis.pending_log (back_vis true) logger _ hw
        cases visit_res with
        | Err _ =>
          dsimp only at hw
          rcases bind_eq_ok hw with ⟨pel, hel, _⟩
          rcases pel with ⟨r1, _⟩
          rw [bind_pair_ok_eq hel] at hw
          have hr1 := error_and_log_is_err hel
          subst hr1
          cases (Prod.mk.inj (ok_inj hw)).1
        | Ok _ =>
          dsimp only at hw
          rcases bind_eq_ok hw with ⟨i_pend, _, hw⟩
          have hng_pend : ¬ (i_pend > verifier.MAX_PENDING_STATES) := fun hc => by
            simp only [hc, ↓reduceIte] at hw
            rcases bind_eq_ok hw with ⟨⟨r1, _⟩, hel, hw⟩
            cases (Prod.mk.inj (ok_inj hw)).1 ▸ error_and_log_is_err hel
          simp only [hng_pend, ↓reduceIte] at hw
          by_cases hns : alloc.vec.Vec.len vc_vis.pending_successors > 0#usize
          · simp only [hns, ↓reduceIte] at hw
            rw [bind_ok_eq DataDependencies_default_zero] at hw
            rcases bind_eq_ok hw with ⟨par, har_push, _⟩
            rcases par with ⟨_, _⟩
            rw [bind_pair_ok_eq har_push] at hw
            rcases bind_eq_ok hw with ⟨_, _, hw⟩
            rcases bind_eq_ok hw with ⟨_, _, hw⟩
            rcases Prod.mk.inj (ok_inj hw) with ⟨_, hp⟩
            rcases Prod.mk.inj hp with ⟨_, hp⟩
            rcases Prod.mk.inj hp with ⟨_, hp⟩
            rcases Prod.mk.inj hp with ⟨rfl, _⟩
            exact hback_mono
          · simp only [hns, ↓reduceIte] at hw
            rw [bind_ok_eq DataDependencies_default_zero] at hw
            rcases bind_eq_ok hw with ⟨ptc, htc, _⟩
            rcases ptc with ⟨r_tc, _, _⟩
            rw [bind_triple_ok_eq htc] at hw
            cases r_tc with
            | Err _ =>
              dsimp only at hw
              rcases bind_eq_ok hw with ⟨pel, hel, _⟩
              rcases pel with ⟨r1, _⟩
              rw [bind_pair_ok_eq hel] at hw
              have hr1 := error_and_log_is_err hel
              subst hr1
              cases (Prod.mk.inj (ok_inj hw)).1
            | Ok _ =>
              dsimp only at hw
              rcases bind_eq_ok hw with ⟨_, _, hw⟩
              rcases Prod.mk.inj (ok_inj hw) with ⟨_, hp⟩
              rcases Prod.mk.inj hp with ⟨_, hp⟩
              rcases Prod.mk.inj hp with ⟨_, hp⟩
              rcases Prod.mk.inj hp with ⟨rfl, _⟩
              exact hback_mono

/-- Pass 2 of `verify_program_core_loop5`: given `prog` decoded from `loop6` and instruction
    correspondence on `visOut`, establishes `SemTerminatesIn prog env 0 ctxInit dRoot VERIFIER_MAX_STEPS`. -/
theorem verify_program_core_loop5_soundness_spec
    {L : Type} (loggerInst : verifier.CoreVerifierLogger L)
    {codeOut : List api.EbpfInstruction} {prog : Program} {env : ExecEnv}
    {cctx : verifier.CoreCallingContext} {code : alloc.vec.Vec api.EbpfInstruction}
    {ctxInit : verifier.ComputationContext}
    {logger loggerOut : L}
    {vis visOut : alloc.vec.Vec Bool}
    {wl : verifier.Worklist} {es0 : List verifier.WorklistEntry}
    {ar : verifier.ExplorerArena}
    {vctx vctxOut : verifier.VerificationContext}
    {M0 : Nat} {rc0 : Bool}
    (hdec : decodeVerifiedCode codeOut = some prog)
    (hprog_len : prog.size = code.val.length)
    (houtVis_match : ∀ k, k < code.val.length → visOut.val[k]? = some true →
      CodeMatchesProgAt cctx code.val prog k)
    (hcctx : CallingContextFullMatches cctx env)
    (hinit_valid : CompCtxValid env 0 ctxInit)
    (hvis_len : vis.val.length = code.val.length)
    (hwl_rep : WorklistRep wl es0)
    (hcnt_iter : vctx.counter.bv.toNat ≤ 3 * vctx.iteration.val)
    (hpot_iter : M0 + arenaActiveCount ar ≤ vctx.iteration.val)
    (hiter_max : vctx.iteration.val ≤ VERIFIER_MAX_STEPS)
    (hinv0 : ExplorerInv prog env cctx code ctxInit none es0 ar vctx M0 rc0)
    (hloop : verifier.verify_program_core_loop5 loggerInst logger vis wl ar vctx none false =
      ok (loggerOut, visOut, vctxOut, none)) :
    ∃ dRoot, SemTerminatesIn prog env 0 ctxInit dRoot VERIFIER_MAX_STEPS := by
  unfold verifier.verify_program_core_loop5 at hloop
  have hmain := loop_ok_rel_induct
    (fun (l1, v1, w1, a1, vc1, e1, d1) =>
      verifier.verify_program_core_loop5.body loggerInst l1 v1 w1 a1 vc1 e1 d1)
    (fun s out =>
      (s.2.2.2.2.2.1 ≠ none → out.2.2.2 ≠ none) ∧
      (out.2.2.2 = none →
        (s.2.2.2.2.2.1 = none →
          ∀ (k : Nat), s.2.1.val[k]? = some true → out.2.1.val[k]? = some true) ∧
        ((∀ k, k < code.val.length → out.2.1.val[k]? = some true →
            CodeMatchesProgAt cctx code.val prog k) →
         ∀ es M rc,
           s.2.2.2.2.2.1 = none →
           s.2.1.val.length = code.val.length →
           s.2.2.2.2.1.counter.bv.toNat ≤ 3 * s.2.2.2.2.1.iteration.val →
           s.2.2.2.2.1.iteration.val ≤ VERIFIER_MAX_STEPS →
           WorklistRep s.2.2.1 es →
           (s.2.2.2.2.2.2 = true → es = []) →
           M + arenaActiveCount s.2.2.2.1 ≤ s.2.2.2.2.1.iteration.val →
           ExplorerInv prog env cctx code ctxInit none es s.2.2.2.1 s.2.2.2.2.1 M rc →
           ∃ dRoot, SemTerminatesIn prog env 0 ctxInit dRoot VERIFIER_MAX_STEPS)))
    (by
      rintro ⟨l1, v1, w1, a1, vc1, e1, d1⟩
      constructor
      · rintro ⟨l2, v2, w2, a2, vc2, e2, d2⟩ ⟨lo, vo, vco, eo⟩ hbody hIH
        dsimp only at hIH ⊢
        unfold verifier.verify_program_core_loop5.body at hbody
        by_cases hd : d1 = true
        · simp only [hd, ↓reduceIte] at hbody; cases ok_inj hbody
        · simp only [hd, Bool.false_eq_true, ↓reduceIte] at hbody
          by_cases he : core.option.Option.is_none e1 = true
          · simp only [he, ↓reduceIte] at hbody
            have he1 : e1 = none := by
              cases e1 <;> simp [core.option.Option.is_none] at he; rfl
            subst he1
            rcases bind_eq_ok hbody with ⟨⟨o_pop, wl1⟩, hpop, hbody⟩
            cases ho_pop : o_pop with
            | none =>
              rw [ho_pop] at hpop hbody
              cases ok_inj hbody
              refine ⟨fun h => False.elim (h rfl), fun heo => ⟨fun _ => (hIH.2 heo).1 rfl, ?_⟩⟩
              intro hout_match es M1 rc1 _ hvlen1 hcnt1 hmax1 hrep1 _ hpot1 hinv1
              rcases Worklist_pop_spec hrep1 hpop with ⟨rfl, _, hrep_empty⟩ | ⟨_, _, _, hsome, _⟩
              · exact (hIH.2 heo).2 hout_match [] M1 rc1 rfl hvlen1 hcnt1 hmax1 hrep_empty
                  (fun _ => rfl) hpot1 hinv1
              · cases hsome
            | some entry =>
              rw [ho_pop] at hpop hbody
              rcases bind_eq_ok hbody with ⟨⟨r_w, wl_nx, ar_nx, vis_nx, vc_nx, lg_nx⟩, hw, hbody⟩
              cases r_w with
              | Err err =>
                cases ok_inj hbody
                exact ⟨fun h => False.elim (h rfl),
                  fun heo => False.elim ((hIH.1 (by simp)) heo)⟩
              | Ok _ =>
                cases ok_inj hbody
                have hvis_step := verify_worklist_entry_visited_mono loggerInst hw
                refine ⟨fun h => False.elim (h rfl), fun heo => ?_⟩
                have hv2_le_vo := (hIH.2 heo).1 rfl
                refine ⟨fun _ k hk => hv2_le_vo k (hvis_step k hk), ?_⟩
                intro hout_match es M1 rc1 _ hvlen1 hcnt1 hmax1 hrep1 _ hpot1 hinv1
                rcases Worklist_pop_spec hrep1 hpop with ⟨_, hnone, _⟩ |
                    ⟨es_init, entry', rfl, heq_some, hrep_init⟩
                · cases hnone
                · cases Option.some.inj heq_some
                  have hentry_mem : entry ∈ es_init ++ [entry] := by simp
                  obtain ⟨hentry_valid, hentry_root⟩ := hinv1.2.2.2.2.2.2.1 entry hentry_mem
                  have hinv_pop : ExplorerInv prog env cctx code ctxInit
                      (some (entry.parent, entry.context)) es_init a1 vc1 M1 rc1 := by
                    rcases hinv1 with ⟨hc_i, hcd_i, htl_i, hlt_i, hwf_i, htc_i, hes_i,
                      hrl_i, hrs_i, hsl_i⟩
                    have hpop_c := totalLiveContexts_pop_conserve es_init entry a1
                    refine ⟨hc_i, hcd_i, htl_i, hlt_i, hwf_i, htc_i,
                      fun e he => hes_i e (by simp [he]),
                      (hpop_c none).1.trans hrl_i, hrs_i, fun idx => ?_⟩
                    rcases hsl_i idx with ⟨hi_lt, nd, hi_get, hi_len, hi_pos, hi_sem⟩ | ⟨hbad, hz0⟩
                    · exact Or.inl ⟨hi_lt, nd, hi_get, (hpop_c (some idx)).1.trans hi_len, hi_pos,
                        ActiveNodeSem_mono (Nat.le_refl _) hlt_i (Nat.le_refl _)
                          (hpop_c (some idx)).2 hi_sem⟩
                    · exact Or.inr ⟨hbad, (hpop_c (some idx)).1.trans hz0⟩
                  obtain ⟨hvlen2, _, hcnt2, hmax2, es_out, M_out, rc_out, hrep_out, hpot_out, hinv_out⟩ :=
                    verify_worklist_entry_sound loggerInst hdec hprog_len hout_match hcctx hinit_valid
                      hrep_init hvlen1 hcnt1 hpot1 hmax1 hentry_valid hentry_root hinv_pop hw hv2_le_vo
                  exact (hIH.2 heo).2 hout_match es_out M_out rc_out rfl hvlen2 hcnt2 hmax2 hrep_out
                    (fun h => by cases h) hpot_out hinv_out
          · simp only [he, Bool.false_eq_true, ↓reduceIte] at hbody
            cases ok_inj hbody
      · rintro ⟨lo, vo, vco, eo⟩ hbody
        dsimp only
        unfold verifier.verify_program_core_loop5.body at hbody
        by_cases hd : d1 = true
        · simp only [hd, ↓reduceIte] at hbody
          cases ok_inj hbody
          refine ⟨id, fun heo => ⟨fun _ _ hk => hk, ?_⟩⟩
          intro _ es M1 rc1 _ _ _ hmax1 _ hd_empty hpot1 hinv1
          have hes_nil := hd_empty hd
          subst hes_nil
          have htl_ar : ∀ p, totalLiveContexts none [] a1 p = arenaContextsFor a1 p := by
            intro p; unfold totalLiveContexts inFlightContextsFor worklistContextsFor; rfl
          have hrc_true : rc1 = true := by
            apply empty_worklist_implies_rootCompleted a1 rc1
            · rw [← htl_ar none]; exact hinv1.2.2.2.2.2.2.2.1
            · intro idx
              rcases hinv1.2.2.2.2.2.2.2.2.2 idx with
                ⟨hlt, nd, hget, hlen, hpos, _⟩ | ⟨hbad, hz0⟩
              · exact Or.inl ⟨hlt, nd, hget, (htl_ar (some idx)) ▸ hlen, hpos⟩
              · exact Or.inr ⟨hbad, (htl_ar (some idx)) ▸ hz0⟩
          obtain ⟨dRoot, hterm⟩ := hinv1.2.2.2.2.2.2.2.2.1 hrc_true
          exact ⟨dRoot, SemTerminatesIn_mono_fuel (by omega) hterm⟩
        · simp only [hd, Bool.false_eq_true, ↓reduceIte] at hbody
          by_cases he : core.option.Option.is_none e1 = true
          · simp only [he, ↓reduceIte] at hbody
            rcases bind_eq_ok hbody with ⟨⟨o_pop, _⟩, _, hbody⟩
            cases o_pop with
            | none => cases ok_inj hbody
            | some _ =>
              rcases bind_eq_ok hbody with ⟨⟨r_w, _, _, _, _, _⟩, _, hbody⟩
              cases r_w <;> cases ok_inj hbody
          · simp only [he, Bool.false_eq_true, ↓reduceIte] at hbody
            cases ok_inj hbody
            have hne : e1 ≠ none := by
              intro hc; subst hc; simp [core.option.Option.is_none] at he
            exact ⟨fun _ => hne, fun heo => False.elim (hne heo)⟩)
    (logger, vis, wl, ar, vctx, none, false) (loggerOut, visOut, vctxOut, none) hloop
  exact (hmain.2 rfl).2 houtVis_match es0 M0 rc0 rfl hvis_len hcnt_iter hiter_max hwl_rep
    (by intro h; cases h) hpot_iter hinv0

-- ============================================================================
-- Section 4: Top-Level Verification Contract Theorems (`verify_program_core`)
-- ============================================================================

/-- Core unified soundness theorem for `verify_program_core`: produces the decoded
    `Program` `prog` together with full relational 2-safety termination
    `SemTerminatesIn prog env 0 initial_context1 fullDataDeps VERIFIER_MAX_STEPS`
    and initial-state conformance. -/
theorem verify_program_core_sound
    {L : Type} (loggerInst : verifier.CoreVerifierLogger L)
    (rawCode : alloc.vec.Vec api.EbpfInstruction)
    (cctx : verifier.CoreCallingContext)
    (logger logger' : L)
    (verified : verifier.VerifiedEbpfProgram)
    (env : ExecEnv)
    (mkInit : HostLayout → SecretInit → MachineState)
    (_h_logger : ValidVerifierLogger loggerInst)
    (h_entry : EntryStatesConform cctx env mkInit)
    (h_verif : verifier.verify_program_core loggerInst rawCode cctx logger =
      .ok (.Ok verified, logger')) :
    ∃ prog ctxInit,
      decodeVerifiedCode verified.code.val = some prog ∧
      prog.size = verified.code.val.length ∧
      SemTerminatesIn prog env 0 ctxInit fullDataDeps VERIFIER_MAX_STEPS ∧
      (∀ layout₁ layout₂ secret₁ secret₂,
        ValidHostLayout layout₁ →
        ValidHostLayout layout₂ →
        let st₁ := mkInit layout₁ secret₁
        let st₂ := mkInit layout₂ secret₂
        st₁.world.nextAllocId = 0 ∧
        RhoValid 0 st₁.world.nextAllocId (fun x => x) ∧
        StateRel2Deps env ctxInit fullDataDeps (fun x => x) (fun _ => false)
          st₁.world.structFieldNull st₁ st₂) := by
  unfold verifier.verify_program_core at h_verif
  dsimp only at h_verif
  by_cases hbig : alloc.vec.Vec.len rawCode > api.BPF_MAX_INSTS
  · simp only [hbig, ↓reduceIte] at h_verif
    have herr := error_and_log_is_err h_verif
    cases herr
  · simp only [hbig, ↓reduceIte] at h_verif
    rcases bind_eq_ok h_verif with ⟨is_prune_point, hloop0, h_verif⟩
    have hpp0_len : is_prune_point.val.length = rawCode.val.length :=
      verify_program_core_loop0_spec (alloc.vec.Vec.len rawCode) (alloc.vec.Vec.new Bool)
        0#usize is_prune_point ⟨by simp [alloc.vec.Vec.new], by simp⟩ hloop0
    rcases bind_eq_ok h_verif with ⟨⟨is_prune_point1, scan_err⟩, hloop1, h_verif⟩
    cases scan_err with
    | some err =>
      have herr := error_and_log_is_err h_verif
      cases herr
    | none =>
      have ⟨_, hprescan0⟩ := verify_program_core_loop1_spec rawCode cctx
        (alloc.vec.Vec.len rawCode) is_prune_point none 0#usize is_prune_point1
        rfl (fun _ => ⟨hpp0_len, by simp⟩) hloop1
      have hprescan : PrescanValidTail cctx rawCode.val := by
        simpa using hprescan0
      rcases bind_eq_ok h_verif with ⟨terminating_contexts, hloop2, h_verif⟩
      obtain ⟨htc_len, htc_empty⟩ := verify_program_core_loop2_spec (alloc.vec.Vec.len rawCode)
        is_prune_point1 (alloc.vec.Vec.new (Option verifier.TerminatingContexts)) 0#usize
        terminating_contexts
        ⟨by simp [alloc.vec.Vec.new], by simp, fun _ hm => by simp [alloc.vec.Vec.new] at hm⟩
        hloop2
      rcases bind_eq_ok h_verif with ⟨initial_context, hdef_ctx, h_verif⟩
      simp only [lift_eq_ok, bind_ok] at h_verif
      by_cases hgt_args : alloc.vec.Vec.len cctx.args > UScalar.cast .Usize api.ARGUMENT_COUNT
      · simp only [hgt_args, ↓reduceIte] at h_verif
        rcases bind_eq_ok h_verif with ⟨_, _, h_verif⟩
        have herr := error_and_log_is_err h_verif
        cases herr
      · simp only [hgt_args, ↓reduceIte] at h_verif
        have hargs_le5 : cctx.args.val.length ≤ 5 := by
          have harg5 := bpf_argument_count_val
          have hn : ¬ ((UScalar.cast .Usize api.ARGUMENT_COUNT).val < cctx.args.val.length) := hgt_args
          omega
        rcases bind_eq_ok h_verif with ⟨initial_context1, hloop3, h_verif⟩
        obtain ⟨hinit_valid, hentry_rel2⟩ :=
          entry_context_valid_and_rel2 h_entry hargs_le5 hdef_ctx hloop3
        rcases bind_eq_ok h_verif with ⟨visited, hloop4, h_verif⟩
        obtain ⟨hvis0_len, hvis0_false⟩ := verify_program_core_loop4_spec
          (alloc.vec.Vec.len rawCode) (alloc.vec.Vec.new Bool) 0#usize visited
          ⟨by simp [alloc.vec.Vec.new], by simp⟩ hloop4
        have hvis0_valid : VisitedValid rawCode.val visited.val := by
          intro k _ hk_true
          have hmem : true ∈ visited.val := List.mem_of_getElem? hk_true
          have hf := hvis0_false true hmem (fun _ hm => by simp [alloc.vec.Vec.new] at hm)
          cases hf
        rcases bind_eq_ok h_verif with ⟨worklist, hwl_new, h_verif⟩
        rcases bind_eq_ok h_verif with ⟨worklist1, hwl_push, h_verif⟩
        have hwl1_rep : WorklistRep worklist1 [{ context := initial_context1, parent := none }] :=
          Worklist_push_spec (Worklist_new_spec hwl_new) hwl_push
        rcases bind_eq_ok h_verif with ⟨arena, har_new, h_verif⟩
        obtain ⟨har_wf, har_nodes, har_act0⟩ := ExplorerArena_new_spec har_new
        rcases bind_eq_ok h_verif with ⟨_, _, h_verif⟩
        rcases bind_eq_ok h_verif with ⟨vctx, hvctx_new, h_verif⟩
        obtain ⟨hvctx_cctx, hvctx_code, hvctx_cnt0, hvctx_iter0, hvctx_tcs, _, _⟩ :=
          VerificationContext_new_ok_spec hvctx_new
        rcases bind_eq_ok h_verif with ⟨⟨logger1, visited1, vctx1, verify_err⟩, hloop5, h_verif⟩
        cases verify_err with
        | some err =>
          cases (Prod.mk.inj (ok_inj h_verif)).1
        | none =>
          rcases bind_eq_ok h_verif with ⟨dead_insn, hdead, h_verif⟩
          have hdead_eq : dead_insn = deadEbpfInsn :=
            ok_inj (dead_insn_new_eq_ok.symm.trans hdead).symm
          rcases bind_eq_ok h_verif with ⟨code1, hloop6, h_verif⟩
          rcases bind_eq_ok h_verif with ⟨struct_access_instructions, _, h_verif⟩
          rcases Prod.mk.inj (ok_inj h_verif) with ⟨hok_ver, _⟩
          have hver_code : verified.code = code1 := by
            cases core.result.Result.Ok.inj hok_ver; rfl
          have hcctx : CallingContextFullMatches cctx env :=
            CallingContextFullMatches.of_entryStatesConform h_entry
          have hvctx_cnt_zero : vctx.counter.bv.toNat = 0 := by
            rw [hvctx_cnt0]; rfl
          have hvctx_iter_zero : vctx.iteration.val = 0 := by
            rw [hvctx_iter0, usize_const_val_0]
          obtain ⟨_, hvctx1_code, hvis1_len, hvis1_valid⟩ :=
            verify_program_core_loop5_visited_spec loggerInst hcctx hvctx_cctx hvctx_code
              hvis0_len hvis0_valid hwl1_rep
              (fun e he => by
                simp only [List.mem_singleton] at he
                subst he; rw [hvctx_cnt_zero]; exact hinit_valid)
              hloop5
          rw [hvctx1_code] at hloop6
          obtain ⟨prog, hdec, hprog_size_raw, hcorr⟩ :=
            verify_program_core_loop6_zero_spec visited1 rawCode code1 dead_insn
              hprescan hvis1_len hdead_eq hvis1_valid hloop6
          have hinv0 : ExplorerInv prog env cctx rawCode initial_context1 none
              [{ context := initial_context1, parent := none }] arena vctx 0 false := by
            refine ⟨hvctx_cctx, hvctx_code,
              (congrArg (fun v => v.val.length) hvctx_tcs).trans htc_len,
              by rw [hvctx_cnt_zero]; decide,
              har_wf, ?_, ?_, ?_, (fun h => by cases h), ?_⟩
            · intro tcOpt hmem tc hsome
              rw [hvctx_tcs] at hmem
              have hemp := htc_empty tcOpt hmem tc hsome
              intro p hp
              rw [hemp] at hp; cases hp
            · intro e he
              simp only [List.mem_singleton] at he
              subst he
              rw [hvctx_cnt_zero]
              exact ⟨hinit_valid, fun _ => rfl⟩
            · unfold totalLiveContexts inFlightContextsFor worklistContextsFor arenaContextsFor
              rw [har_nodes]; rfl
            · intro idx
              right
              refine ⟨Or.inl (by rw [har_nodes]; exact Nat.zero_le _), ?_⟩
              unfold totalLiveContexts inFlightContextsFor worklistContextsFor arenaContextsFor
              rw [har_nodes]; rfl
          obtain ⟨dRoot, hsem_root⟩ :=
            verify_program_core_loop5_soundness_spec loggerInst hdec hprog_size_raw hcorr
              hcctx hinit_valid hvis0_len hwl1_rep
              (by rw [hvctx_cnt_zero, hvctx_iter_zero])
              (by rw [har_act0, hvctx_iter_zero])
              (by rw [hvctx_iter_zero]; exact Nat.zero_le _)
              hinv0 hloop5
          have hprog_ver_len : prog.size = verified.code.val.length := by
            rw [hver_code]
            rcases Option.map_eq_some_iff.mp hdec with ⟨progList, hdecL, rfl⟩
            simpa using decodeVerifiedCodeList_length code1.val progList hdecL
          exact ⟨prog, initial_context1, hver_code ▸ hdec, hprog_ver_len,
            SemTerminatesIn_mono_deps (DepsSubset_full dRoot) hsem_root, hentry_rel2⟩

theorem verify_program_core_implies_safe_and_terminating
    {L : Type} (loggerInst : ebpf_core.verifier.CoreVerifierLogger L)
    (rawCode : alloc.vec.Vec ebpf_core.api.EbpfInstruction)
    (cctx : ebpf_core.verifier.CoreCallingContext)
    (logger logger' : L)
    (verified : ebpf_core.verifier.VerifiedEbpfProgram)
    (env : ExecEnv)
    (mkInit : HostLayout → SecretInit → MachineState)
    (h_logger : ValidVerifierLogger loggerInst)
    (h_entry : EntryStatesConform cctx env mkInit)
    (h_verif : ebpf_core.verifier.verify_program_core loggerInst rawCode cctx logger =
      .ok (.Ok verified, logger')) :
    ∃ prog,
      decodeVerifiedCode verified.code.val = some prog ∧
      prog.size = verified.code.val.length ∧
      SafeAndTerminating prog env VERIFIER_MAX_STEPS mkInit := by
  obtain ⟨prog, ctxInit, hdec, hsize, hsem, hentry_rel2⟩ :=
    verify_program_core_sound loggerInst rawCode cctx logger logger' verified env mkInit
      h_logger h_entry h_verif
  refine ⟨prog, hdec, hsize, fun layout secret hlayout => ?_⟩
  obtain ⟨halloc0, hρ0, hrel0⟩ := hentry_rel2 layout layout secret secret hlayout hlayout
  have halloc_bound : (mkInit layout secret).world.nextAllocId + 3 * VERIFIER_MAX_STEPS < 2 ^ 63 := by
    rw [halloc0]; unfold VERIFIER_MAX_STEPS; decide
  obtain ⟨ret, finalWorld, hrun, _, hlive⟩ :=
    hsem layout layout secret secret (fun x => x) (fun _ => false)
      (mkInit layout secret).world.structFieldNull
      (mkInit layout secret) (mkInit layout secret)
      hlayout hlayout (Or.inr ⟨rfl, rfl⟩) halloc_bound hρ0 hrel0
  exact ⟨ret, finalWorld, hrun, hlive⟩

theorem verify_program_core_implies_non_interfering
    {L : Type} (loggerInst : ebpf_core.verifier.CoreVerifierLogger L)
    (rawCode : alloc.vec.Vec ebpf_core.api.EbpfInstruction)
    (cctx : ebpf_core.verifier.CoreCallingContext)
    (logger logger' : L)
    (verified : ebpf_core.verifier.VerifiedEbpfProgram)
    (env : ExecEnv)
    (mkInit : HostLayout → SecretInit → MachineState)
    (h_logger : ValidVerifierLogger loggerInst)
    (h_entry : EntryStatesConform cctx env mkInit)
    (h_no_ptr_leaks : cctx.allow_ptr_leaks = false)
    (h_verif : ebpf_core.verifier.verify_program_core loggerInst rawCode cctx logger =
      .ok (.Ok verified, logger')) :
    ∃ prog,
      decodeVerifiedCode verified.code.val = some prog ∧
      prog.size = verified.code.val.length ∧
      NonInterfering prog env VERIFIER_MAX_STEPS mkInit := by
  obtain ⟨prog, ctxInit, hdec, hsize, hsem, hentry_rel2⟩ :=
    verify_program_core_sound loggerInst rawCode cctx logger logger' verified env mkInit
      h_logger h_entry h_verif
  refine ⟨prog, hdec, hsize, fun layout₁ layout₂ secret₁ secret₂ hl₁ hl₂ => ?_⟩
  obtain ⟨halloc0, hρ0, hrel0⟩ := hentry_rel2 layout₁ layout₂ secret₁ secret₂ hl₁ hl₂
  have halloc_bound : (mkInit layout₁ secret₁).world.nextAllocId + 3 * VERIFIER_MAX_STEPS < 2 ^ 63 := by
    rw [halloc0]; unfold VERIFIER_MAX_STEPS; decide
  have hleak : (!env.allowPtrLeaks) = true := by
    simp [h_entry.1, h_no_ptr_leaks]
  exact hsem layout₁ layout₂ secret₁ secret₂ (fun x => x) (fun _ => false)
    (mkInit layout₁ secret₁).world.structFieldNull
    (mkInit layout₁ secret₁) (mkInit layout₂ secret₂)
    hl₁ hl₂ (Or.inl hleak) halloc_bound hρ0 hrel0

end StarnixEbpf.Proof
