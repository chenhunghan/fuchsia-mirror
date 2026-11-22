-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Lean
import Proof.LoopLemmas
import Assumptions
import Spec
import Extracted
import Std.Tactic.BVDecide

set_option linter.unusedTactic false

namespace StarnixEbpf.ApiConverterLinkerExec

open Aeneas Aeneas.Std Result
open StarnixEbpf.Assumptions
open StarnixEbpf.Spec
open StarnixEbpf.LoopLemmas

-- ============================================================================
-- 1. Theorem 1: `ebpf_instruction_set_code_roundtrip`
-- ============================================================================

private theorem u8_eq_iff_bv (a b : U8) : a = b ↔ a.bv = b.bv := by
  constructor
  · intro h; rw [h]
  · intro h; cases a; cases b; simp_all

@[simp] private theorem u64_mk_bv (v : BitVec 64) : U64.bv ⟨v⟩ = v := rfl
@[simp] private theorem u64_and_bv (a b : U64) : U64.bv (a &&& b) = a.bv &&& b.bv := rfl
@[simp] private theorem u64_or_bv (a b : U64) : U64.bv (a ||| b) = a.bv ||| b.bv := rfl

theorem ebpf_instruction_set_code_roundtrip
    (insn : ebpf_core.api.EbpfInstruction) (opcode : U8) :
    ∃ insn',
      ebpf_core.api.EbpfInstruction.set_code insn opcode = .ok insn' ∧
      ebpf_core.api.EbpfInstruction.code insn' = .ok opcode := by
  let insn' : ebpf_core.api.EbpfInstruction :=
    (insn &&& ~~~255#u64) ||| UScalar.cast .U64 opcode
  refine ⟨insn', ?_, ?_⟩
  · unfold ebpf_core.api.EbpfInstruction.set_code
    simp [insn', lift]
  · unfold ebpf_core.api.EbpfInstruction.code
    simp only [Result.ok.injEq]
    rw [u8_eq_iff_bv]
    change BitVec.setWidth 8 ((insn.bv &&& ~~~255#64) ||| BitVec.setWidth 64 opcode.bv) = opcode.bv
    bv_decide

-- ============================================================================
-- 2. Theorem 2: `apply_endianness_be_involutive`
-- ============================================================================

private theorem u64_shl_i32_ok (x : U64) (s : I32)
    (hge : s.val ≥ 0) (hlt : s.toNat < 64) :
    (x <<< s : Result U64) = .ok ⟨x.bv <<< s.toNat⟩ := by
  change UScalar.shiftLeft_IScalar x s = _
  simp only [UScalar.shiftLeft_IScalar, hge, ↓reduceIte, UScalar.shiftLeft,
    UScalarTy.numBits, hlt]
  rfl

private theorem u64_shr_i32_ok (x : U64) (s : I32)
    (hge : s.val ≥ 0) (hlt : s.toNat < 64) :
    (x >>> s : Result U64) = .ok ⟨x.bv >>> s.toNat⟩ := by
  change UScalar.shiftRight_IScalar x s = _
  simp only [UScalar.shiftRight_IScalar, hge, ↓reduceIte, UScalar.shiftRight,
    UScalarTy.numBits, hlt]
  rfl

private theorem u64_shl_8  (x : U64) : (x <<< 8#i32  : Result U64) = .ok ⟨x.bv <<< 8⟩  :=
  u64_shl_i32_ok x 8#i32  (by decide) (by decide)
private theorem u64_shl_24 (x : U64) : (x <<< 24#i32 : Result U64) = .ok ⟨x.bv <<< 24⟩ :=
  u64_shl_i32_ok x 24#i32 (by decide) (by decide)
private theorem u64_shl_40 (x : U64) : (x <<< 40#i32 : Result U64) = .ok ⟨x.bv <<< 40⟩ :=
  u64_shl_i32_ok x 40#i32 (by decide) (by decide)
private theorem u64_shl_56 (x : U64) : (x <<< 56#i32 : Result U64) = .ok ⟨x.bv <<< 56⟩ :=
  u64_shl_i32_ok x 56#i32 (by decide) (by decide)

private theorem u64_shr_8  (x : U64) : (x >>> 8#i32  : Result U64) = .ok ⟨x.bv >>> 8⟩  :=
  u64_shr_i32_ok x 8#i32  (by decide) (by decide)
private theorem u64_shr_24 (x : U64) : (x >>> 24#i32 : Result U64) = .ok ⟨x.bv >>> 24⟩ :=
  u64_shr_i32_ok x 24#i32 (by decide) (by decide)
private theorem u64_shr_40 (x : U64) : (x >>> 40#i32 : Result U64) = .ok ⟨x.bv >>> 40⟩ :=
  u64_shr_i32_ok x 40#i32 (by decide) (by decide)
private theorem u64_shr_56 (x : U64) : (x >>> 56#i32 : Result U64) = .ok ⟨x.bv >>> 56⟩ :=
  u64_shr_i32_ok x 56#i32 (by decide) (by decide)

theorem apply_endianness_be_involutive
    (w : ebpf_core.api.DataWidth) (v : U64) :
    ∃ v1 v2,
      ebpf_core.api.apply_endianness true w v = .ok v1 ∧
      ebpf_core.api.apply_endianness true w v1 = .ok v2 ∧
      v2.bv = v.bv &&& (DataWidth.ofCore w).valMask64 := by
  cases w with
  | U8 =>
    refine ⟨v &&& 255#u64, (v &&& 255#u64) &&& 255#u64, ?_, ?_, ?_⟩
    · simp [ebpf_core.api.apply_endianness]
    · simp [ebpf_core.api.apply_endianness]
    · change (v.bv &&& 255#64) &&& 255#64 = v.bv &&& 0xFF#64
      bv_decide
  | U16 =>
    unfold ebpf_core.api.apply_endianness
    simp only [↓reduceIte, lift, bind_ok, u64_shl_8, u64_shr_8]
    refine ⟨_, _, rfl, rfl, ?_⟩
    dsimp only [DataWidth.ofCore, DataWidth.valMask64]
    simp only [u64_and_bv, u64_or_bv, u64_mk_bv, UScalar.ofNatCore_bv, UScalarTy.numBits]
    generalize v.bv = vbv
    bv_decide
  | U32 =>
    unfold ebpf_core.api.apply_endianness
    simp only [↓reduceIte, lift, bind_ok, u64_shl_8, u64_shl_24, u64_shr_8, u64_shr_24]
    refine ⟨_, _, rfl, rfl, ?_⟩
    dsimp only [DataWidth.ofCore, DataWidth.valMask64]
    simp only [u64_and_bv, u64_or_bv, u64_mk_bv, UScalar.ofNatCore_bv, UScalarTy.numBits]
    generalize v.bv = vbv
    bv_decide
  | U64 =>
    unfold ebpf_core.api.apply_endianness
    simp only [↓reduceIte, lift, bind_ok,
      u64_shl_8, u64_shl_24, u64_shl_40, u64_shl_56,
      u64_shr_8, u64_shr_24, u64_shr_40, u64_shr_56]
    refine ⟨_, _, rfl, rfl, ?_⟩
    dsimp only [DataWidth.ofCore, DataWidth.valMask64]
    simp only [u64_and_bv, u64_or_bv, u64_mk_bv, UScalar.ofNatCore_bv, UScalarTy.numBits]
    generalize v.bv = vbv
    bv_decide

-- ============================================================================
-- 3. `ResultOkPost` Combinators & `Vec` Length Lemmas (for Theorems 6 & 9)
-- ============================================================================

def ResultOkPost {α : Type u} (m : Result α) (P : α → Prop) : Prop :=
  ∀ x, m = Result.ok x → P x

theorem resultOkPost_ok {α : Type u} {v : α} {P : α → Prop} (h : P v) :
    ResultOkPost (Result.ok v) P := by
  intro x hx
  cases ok_inj hx
  exact h

theorem resultOkPost_bind {α β : Type u}
    {m : Result α} {f : α → Result β} {P : β → Prop}
    (hf : ∀ x, ResultOkPost (f x) P) :
    ResultOkPost (m >>= f) P := by
  intro y hy
  rcases bind_eq_ok hy with ⟨x, _, hfx⟩
  exact hf x y hfx

theorem resultOkPost_bind_pair {α₁ α₂ β : Type u}
    {m : Result (α₁ × α₂)} {f : α₁ × α₂ → Result β} {P : β → Prop}
    (hf : ∀ a₁ a₂, ResultOkPost (f (a₁, a₂)) P) :
    ResultOkPost (m >>= f) P := by
  intro y hy
  rcases bind_eq_ok hy with ⟨⟨a₁, a₂⟩, _, hfx⟩
  exact hf a₁ a₂ y hfx

theorem resultOkPost_bind_triple {α₁ α₂ α₃ β : Type u}
    {m : Result (α₁ × α₂ × α₃)} {f : α₁ × α₂ × α₃ → Result β} {P : β → Prop}
    (hf : ∀ a₁ a₂ a₃, ResultOkPost (f (a₁, a₂, a₃)) P) :
    ResultOkPost (m >>= f) P := by
  intro y hy
  rcases bind_eq_ok hy with ⟨⟨a₁, a₂, a₃⟩, _, hfx⟩
  exact hf a₁ a₂ a₃ y hfx

theorem resultOkPost_ite {α : Type u} {c : Prop} [Decidable c]
    {t e : Result α} {P : α → Prop}
    (ht : ResultOkPost t P) (he : ResultOkPost e P) :
    ResultOkPost (if c then t else e) P := by
  by_cases hc : c
  · simpa [hc] using ht
  · simpa [hc] using he

theorem resultOkPost_ite_bind {α β : Type u} {c : Prop} [Decidable c]
    {t e : Result α} {f : α → Result β} {P : β → Prop}
    (ht : ResultOkPost (t >>= f) P) (he : ResultOkPost (e >>= f) P) :
    ResultOkPost ((if c then t else e) >>= f) P := by
  by_cases hc : c
  · simpa [hc] using ht
  · simpa [hc] using he

theorem vec_new_len (T : Type u) : (alloc.vec.Vec.new T).val.length = 0 := rfl

theorem vec_push_ok_len {T : Type u} {v v' : alloc.vec.Vec T} {elem : T}
    (h : alloc.vec.Vec.push v elem = Result.ok v') :
    v'.val.length = v.val.length + 1 := by
  unfold alloc.vec.Vec.push at h
  dsimp only at h
  split at h
  · have hv : v' = alloc.vec.Vec.from (v.val.concat elem) _ := (ok_inj h).symm
    subst hv
    simp [alloc.vec.Vec.from, alloc.vec.Vec.val]
  · simp at h

theorem vec_index_mut_usize_ok_len {T : Type u} {v : alloc.vec.Vec T} {idx : Usize}
    {elem : T} {back : T → alloc.vec.Vec T}
    (h : alloc.vec.Vec.index_mut (core.slice.index.SliceIndexUsizeSlice T) v idx =
      Result.ok (elem, back)) :
    ∀ u, (back u).val.length = v.val.length := by
  unfold alloc.vec.Vec.index_mut core.slice.index.SliceIndexUsizeSlice
    core.slice.index.Usize.index_mut Slice.index_mut_usize at h
  rcases bind_eq_ok h with ⟨⟨o, sb⟩, h1, h2⟩
  rcases bind_eq_ok h1 with ⟨x, _, h1ok⟩
  have hsb : sb = v.slice.set idx := (Prod.mk.inj (ok_inj h1ok)).2.symm
  have hback : back = fun o' => { slice := sb o' } := (Prod.mk.inj (ok_inj h2)).2.symm
  intro u
  subst hback hsb
  change (v.slice.set idx u).val.length = v.slice.val.length
  simp [Slice.set]

theorem resultOkPost_push {T β : Type u}
    {v : alloc.vec.Vec T} {elem : T} {f : alloc.vec.Vec T → Result β} {P : β → Prop}
    (hf : ∀ v', v'.val.length = v.val.length + 1 → ResultOkPost (f v') P) :
    ResultOkPost (alloc.vec.Vec.push v elem >>= f) P := by
  intro y hy
  rcases bind_eq_ok hy with ⟨v', hv', hfy⟩
  exact hf v' (vec_push_ok_len hv') y hfy

theorem resultOkPost_index_mut {T β : Type u}
    {v : alloc.vec.Vec T} {idx : Usize}
    {f : (T × (T → alloc.vec.Vec T)) → Result β} {P : β → Prop}
    (hf : ∀ elem (back : T → alloc.vec.Vec T),
      (∀ u, (back u).val.length = v.val.length) → ResultOkPost (f (elem, back)) P) :
    ResultOkPost
      (alloc.vec.Vec.index_mut (core.slice.index.SliceIndexUsizeSlice T) v idx >>= f) P := by
  intro y hy
  rcases bind_eq_ok hy with ⟨⟨elem, back⟩, hm, hfy⟩
  exact hf elem back (vec_index_mut_usize_ok_len hm) y hfy

theorem resultOkPost_bind_assoc {α β γ : Type u}
    {m : Result α} {f : α → Result β} {g : β → Result γ} {P : γ → Prop}
    (h : ResultOkPost (m >>= fun x => f x >>= g) P) :
    ResultOkPost ((m >>= f) >>= g) P := by
  intro z hz
  rcases bind_eq_ok hz with ⟨y, hm, hg⟩
  rcases bind_eq_ok hm with ⟨x, hmx, hfy⟩
  apply h z
  rw [hmx]
  simpa [bind_ok, hfy] using hg

section VecPostTactic
open Lean Meta Elab Tactic

private def isBindExpr (e : Expr) : Option (Expr × Expr × Expr) :=
  let e := e.consumeMData
  if e.isAppOfArity ``Bind.bind 6 then
    some (e.getArg! 2, e.getArg! 4, e.getArg! 5)
  else if e.isAppOfArity ``Aeneas.Std.bind 4 then
    some (e.getArg! 0, e.getArg! 2, e.getArg! 3)
  else
    none

private def closeLeafGoal : TacticM Unit := do
  evalTactic (← `(tactic| (
    try simp_all (config := { zetaHave := true }) only [Aeneas.Std.uncurry]
    try omega)))

def proveVecPostLoop : Nat → TacticM Unit
  | 0 => throwError "prove_vec_post: out of fuel"
  | fuel + 1 => do
    withMainContext do
      let goals0 ← getGoals
      if goals0.isEmpty then
        return
      let goal ← getMainGoal
      let target ← instantiateMVars (← goal.getType)
      let target := target.consumeMData
      unless target.isAppOfArity ``ResultOkPost 3 do
        closeLeafGoal
        return
      let mRaw := (target.getArg! 1).consumeMData
      let m ← withTransparency .instances (whnf mRaw)
      if m != mRaw then
        let newTarget := mkAppN target.getAppFn #[target.getArg! 0, m, target.getArg! 2]
        replaceMainGoal [← goal.replaceTargetDefEq newTarget]
        proveVecPostLoop fuel
        return
      if m.isAppOfArity ``Result.ok 2 then
        evalTactic (← `(tactic| apply resultOkPost_ok))
        closeLeafGoal
      else if m.isAppOfArity ``Result.fail 2 then
        evalTactic (← `(tactic| (intro _ h; cases h)))
      else if let some (α, lhsRaw, rhs) := isBindExpr m then
        let lhs ← withTransparency .instances (whnf lhsRaw.consumeMData)
        if lhs != lhsRaw.consumeMData then
          let newBind := mkAppN m.getAppFn ((m.getAppArgs.set! (m.getAppNumArgs - 2) lhs).set! (m.getAppNumArgs - 1) rhs)
          let newTarget := mkAppN target.getAppFn #[target.getArg! 0, newBind, target.getArg! 2]
          replaceMainGoal [← goal.replaceTargetDefEq newTarget]
          proveVecPostLoop fuel
        else if (isBindExpr lhs).isSome then
          evalTactic (← `(tactic| apply resultOkPost_bind_assoc))
          proveVecPostLoop fuel
        else if lhs.isAppOfArity ``Result.ok 2 then
          evalTactic (← `(tactic| simp only [bind_ok]))
          proveVecPostLoop fuel
        else if lhs.isAppOfArity ``Result.fail 2 then
          evalTactic (← `(tactic| (intro _ h; cases h)))
        else if lhs.isAppOfArity ``ite 5 then
          evalTactic (← `(tactic| apply resultOkPost_ite_bind))
          let goals ← getGoals
          let mut rem : List MVarId := []
          for g in goals do
            setGoals [g]
            proveVecPostLoop fuel
            rem := rem ++ (← getGoals)
          setGoals rem
        else if lhs.isAppOfArity ``alloc.vec.Vec.push 3 then
          evalTactic (← `(tactic| (apply resultOkPost_push; intro _ _)))
          proveVecPostLoop fuel
        else if lhs.isAppOfArity ``alloc.vec.Vec.index_mut 6 then
          let inst := (lhs.getArg! 3).consumeMData
          if inst.isAppOfArity ``core.slice.index.SliceIndexUsizeSlice 1 then
            evalTactic (← `(tactic| (apply resultOkPost_index_mut; intro _ _ _)))
            proveVecPostLoop fuel
          else
            evalTactic (← `(tactic| (apply resultOkPost_bind; intro _)))
            proveVecPostLoop fuel
        else
          let α := α.consumeMData
          if α.isAppOfArity ``Prod 2 then
            let α₂ := (α.getArg! 1).consumeMData
            if α₂.isAppOfArity ``Prod 2 then
              evalTactic (← `(tactic| (apply resultOkPost_bind_triple; intro _ _ _)))
            else
              evalTactic (← `(tactic| (apply resultOkPost_bind_pair; intro _ _)))
            proveVecPostLoop fuel
          else
            evalTactic (← `(tactic| (apply resultOkPost_bind; intro _)))
            proveVecPostLoop fuel
      else if m.isAppOfArity ``ite 5 then
        evalTactic (← `(tactic| apply resultOkPost_ite))
        let goals ← getGoals
        let mut rem : List MVarId := []
        for g in goals do
          setGoals [g]
          proveVecPostLoop fuel
          rem := rem ++ (← getGoals)
        setGoals rem
      else
        let reduced ← observing? (
          evalTactic (← `(tactic| simp (config := { zetaHave := true }) only [Aeneas.Std.uncurry])))
        if reduced.isSome then
          proveVecPostLoop fuel
        else
          evalTactic (← `(tactic| split))
          let goals ← getGoals
          let mut rem : List MVarId := []
          for g in goals do
            setGoals [g]
            proveVecPostLoop fuel
            rem := rem ++ (← getGoals)
          setGoals rem

elab "prove_vec_post" : tactic => proveVecPostLoop 1024

end VecPostTactic

-- ============================================================================
-- 4. Theorem 6: `cbpf_to_ebpf_core_nonempty_on_ok`
-- ============================================================================

private theorem apply_patches_for_pc_loop_preserves_len
    (to_be_patched : Slice (alloc.vec.Vec Usize)) (i patch_count : Usize)
    (ebpf_code ebpf_code' : alloc.vec.Vec ebpf_core.api.EbpfInstruction) (p : Usize)
    (h : ebpf_core.converter.apply_patches_for_pc_loop to_be_patched i ebpf_code patch_count p =
      .ok ebpf_code') :
    ebpf_code'.val.length = ebpf_code.val.length := by
  unfold ebpf_core.converter.apply_patches_for_pc_loop at h
  refine loop_ok_induct
    (fun (ec, p1) => ebpf_core.converter.apply_patches_for_pc_loop.body
      to_be_patched i patch_count ec p1)
    (fun (ec, _) => ec.val.length = ebpf_code.val.length)
    (fun ec => ec.val.length = ebpf_code.val.length)
    ?_ (ebpf_code, p) ebpf_code' rfl h
  rintro ⟨ec, p1⟩ hinv
  dsimp only at hinv ⊢
  have hpost : ResultOkPost
      (ebpf_core.converter.apply_patches_for_pc_loop.body to_be_patched i patch_count ec p1)
      (fun cf => match cf with
        | .cont (ec', _) => ec'.val.length = ebpf_code.val.length
        | .done ec' => ec'.val.length = ebpf_code.val.length) := by
    unfold ebpf_core.converter.apply_patches_for_pc_loop.body
    prove_vec_post
  exact ⟨fun ⟨ec', p1'⟩ hbody => hpost (.cont (ec', p1')) hbody,
         fun ec' hbody => hpost (.done ec') hbody⟩

private theorem apply_patches_for_pc_preserves_len
    (to_be_patched : Slice (alloc.vec.Vec Usize)) (i : Usize)
    (ebpf_code ebpf_code' : alloc.vec.Vec ebpf_core.api.EbpfInstruction)
    (h : ebpf_core.converter.apply_patches_for_pc to_be_patched i ebpf_code = .ok ebpf_code') :
    ebpf_code'.val.length = ebpf_code.val.length := by
  unfold ebpf_core.converter.apply_patches_for_pc at h
  rcases bind_eq_ok h with ⟨v, _, hloop⟩
  dsimp (config := { zetaHave := true }) only at hloop
  exact apply_patches_for_pc_loop_preserves_len to_be_patched i (alloc.vec.Vec.len v)
    ebpf_code ebpf_code' 0#usize hloop

set_option maxHeartbeats 8000000 in
private theorem convert_single_cbpf_inst_len_mono
    (sf : ebpf_core.api.SockFilter) (bpf_len i : Usize)
    (config : ebpf_core.converter.CbpfConfig)
    (to_be_patched : alloc.vec.Vec (alloc.vec.Vec Usize))
    (ebpf_code : alloc.vec.Vec ebpf_core.api.EbpfInstruction) :
    ResultOkPost
      (ebpf_core.converter.convert_single_cbpf_inst sf bpf_len i config to_be_patched ebpf_code)
      (fun (_, _, ebpf_code') => ebpf_code.val.length ≤ ebpf_code'.val.length) := by
  unfold ebpf_core.converter.convert_single_cbpf_inst
  prove_vec_post

private theorem cbpf_to_ebpf_core_loop1_len_mono
    (bpf_code : Slice ebpf_core.api.SockFilter)
    (config : ebpf_core.converter.CbpfConfig)
    (to_be_patched : alloc.vec.Vec (alloc.vec.Vec Usize))
    (ebpf_code ebpf_code' : alloc.vec.Vec ebpf_core.api.EbpfInstruction)
    (loop_err loop_err' : Option ebpf_core.error.CbpfError) (i : Usize)
    (h : ebpf_core.converter.cbpf_to_ebpf_core_loop1 bpf_code config to_be_patched
      ebpf_code loop_err i = .ok (ebpf_code', loop_err')) :
    ebpf_code.val.length ≤ ebpf_code'.val.length := by
  unfold ebpf_core.converter.cbpf_to_ebpf_core_loop1 at h
  refine loop_ok_induct
    (fun (tbp, ec, err, idx) =>
      ebpf_core.converter.cbpf_to_ebpf_core_loop1.body bpf_code config tbp ec err idx)
    (fun (_, ec, _, _) => ebpf_code.val.length ≤ ec.val.length)
    (fun (ec, _) => ebpf_code.val.length ≤ ec.val.length)
    ?_ (to_be_patched, ebpf_code, loop_err, i) (ebpf_code', loop_err') (Nat.le_refl _) h
  rintro ⟨tbp, ec, err, idx⟩ hinv
  unfold ebpf_core.converter.cbpf_to_ebpf_core_loop1.body
  try dsimp (config := { zetaHave := true }) only at hinv ⊢
  constructor
  · rintro ⟨tbp', ec', err', idx'⟩ hbody
    try dsimp (config := { zetaHave := true }) only at hbody
    split at hbody
    · try dsimp (config := { zetaHave := true }) only at hbody
      split at hbody
      · try dsimp (config := { zetaHave := true }) only at hbody
        rcases bind_eq_ok hbody with ⟨ec1, hpatch, hbody⟩
        have hlen1 := apply_patches_for_pc_preserves_len _ _ _ _ hpatch
        rcases bind_eq_ok hbody with ⟨sf, _, hbody⟩
        try dsimp (config := { zetaHave := true }) only at hbody
        rcases bind_eq_ok hbody with ⟨⟨r, tbp2, ec2⟩, hconv, hbody⟩
        have hlen2 : ec1.val.length ≤ ec2.val.length :=
          convert_single_cbpf_inst_len_mono sf (Slice.len bpf_code) idx config tbp ec1
            (r, tbp2, ec2) hconv
        cases r with
        | Ok _ =>
          rcases bind_eq_ok hbody with ⟨idx3, _, hok⟩
          cases ok_inj hok
          dsimp only at *
          omega
        | Err _ =>
          cases ok_inj hbody
          dsimp only at *
          omega
      · simp at hbody
    · simp at hbody
  · rintro ⟨ec', err'⟩ hbody
    try dsimp (config := { zetaHave := true }) only at hbody
    split at hbody
    · try dsimp (config := { zetaHave := true }) only at hbody
      split at hbody
      · try dsimp (config := { zetaHave := true }) only at hbody
        rcases bind_eq_ok hbody with ⟨ec1, _, hbody⟩
        rcases bind_eq_ok hbody with ⟨sf, _, hbody⟩
        try dsimp (config := { zetaHave := true }) only at hbody
        rcases bind_eq_ok hbody with ⟨⟨r, tbp2, ec2⟩, _, hbody⟩
        cases r with
        | Ok _ =>
          rcases bind_eq_ok hbody with ⟨_, _, hok⟩
          simp at hok
        | Err _ =>
          simp at hbody
      · cases ok_inj hbody
        exact hinv
    · cases ok_inj hbody
      exact hinv

theorem cbpf_to_ebpf_core_nonempty_on_ok
    (bpf_code : Slice ebpf_core.api.SockFilter)
    (config : ebpf_core.converter.CbpfConfig)
    (ebpf_code : alloc.vec.Vec ebpf_core.api.EbpfInstruction)
    (h_ok : ebpf_core.converter.cbpf_to_ebpf_core bpf_code config =
      .ok (.Ok ebpf_code)) :
    0 < ebpf_code.val.length := by
  unfold ebpf_core.converter.cbpf_to_ebpf_core at h_ok
  try dsimp (config := { zetaHave := true }) only at h_ok
  rcases bind_eq_ok h_ok with ⟨i1, _, h_ok⟩
  try dsimp (config := { zetaHave := true }) only at h_ok
  split at h_ok
  · simp at h_ok
  · try dsimp (config := { zetaHave := true }) only at h_ok
    split at h_ok
    · simp at h_ok
    · rcases bind_eq_ok h_ok with ⟨to_be_patched, _, h_ok⟩
      rcases bind_eq_ok h_ok with ⟨i3, _, h_ok⟩
      rcases bind_eq_ok h_ok with ⟨i4, _, h_ok⟩
      rcases bind_eq_ok h_ok with ⟨ei, _, h_ok⟩
      rcases bind_eq_ok h_ok with ⟨ec0, hpush0, h_ok⟩
      rcases bind_eq_ok h_ok with ⟨i5, _, h_ok⟩
      rcases bind_eq_ok h_ok with ⟨i6, _, h_ok⟩
      rcases bind_eq_ok h_ok with ⟨ei1, _, h_ok⟩
      rcases bind_eq_ok h_ok with ⟨ec1, hpush1, h_ok⟩
      rcases bind_eq_ok h_ok with ⟨i7, _, h_ok⟩
      rcases bind_eq_ok h_ok with ⟨i8, _, h_ok⟩
      rcases bind_eq_ok h_ok with ⟨ei2, _, h_ok⟩
      rcases bind_eq_ok h_ok with ⟨ec2, hpush2, h_ok⟩
      rcases bind_eq_ok h_ok with ⟨⟨ec3, loop_err⟩, hloop1, h_ok⟩
      have h0 := vec_push_ok_len hpush0
      have h1 := vec_push_ok_len hpush1
      have h2 := vec_push_ok_len hpush2
      have hmono := cbpf_to_ebpf_core_loop1_len_mono bpf_code config to_be_patched
        ec2 ec3 none loop_err 0#usize hloop1
      cases loop_err with
      | some err => simp at h_ok
      | none =>
        try dsimp (config := { zetaHave := true }) only at h_ok
        rcases bind_eq_ok h_ok with ⟨i10, _, h_ok⟩
        rcases bind_eq_ok h_ok with ⟨sf, _, h_ok⟩
        rcases bind_eq_ok h_ok with ⟨i11, _, h_ok⟩
        rcases bind_eq_ok h_ok with ⟨i12, _, h_ok⟩
        split at h_ok
        · rcases bind_eq_ok h_ok with ⟨i13, _, h_ok⟩
          rcases bind_eq_ok h_ok with ⟨i14, _, h_ok⟩
          split at h_ok
          · simp at h_ok
          · cases ok_inj h_ok
            simp only [vec_new_len] at h0
            omega
        · cases ok_inj h_ok
          simp only [vec_new_len] at h0
          omega

-- ============================================================================
-- 5. Theorem 9: `link_instructions_preserves_length`
-- ============================================================================

private theorem usize_lt_iff_val (a b : Usize) : (a < b) ↔ a.val < b.val := by
  simp [UScalar.lt_equiv]

private theorem usize_add_one_ok_val {a b : Usize} (h : (a + 1#usize : Result Usize) = .ok b) :
    b.val = a.val + 1 := by
  have h1 : (1#usize).val = 1 := by
    cases System.Platform.numBits_eq <;> simp_all [UScalar.val]
  have heq := UScalar.add_equiv a 1#usize
  rw [h] at heq
  exact heq.2.1.trans (by omega)

private theorem link_instructions_loop0_len
    (v code code' : alloc.vec.Vec ebpf_core.api.EbpfInstruction) (idx : Usize)
    (hinit_eq : code.val.length = idx.val)
    (hinit_le : idx.val ≤ v.val.length)
    (h : ebpf_core.program.link_instructions_loop0 v code idx = .ok code') :
    code'.val.length = v.val.length := by
  unfold ebpf_core.program.link_instructions_loop0 at h
  refine loop_ok_induct
    (fun (c, i) => ebpf_core.program.link_instructions_loop0.body v c i)
    (fun (c, i) => c.val.length = i.val ∧ i.val ≤ v.val.length)
    (fun c => c.val.length = v.val.length)
    ?_ (code, idx) code' ⟨hinit_eq, hinit_le⟩ h
  rintro ⟨c, i⟩ ⟨heq, hle⟩
  unfold ebpf_core.program.link_instructions_loop0.body
  try dsimp (config := { zetaHave := true }) only at heq hle ⊢
  constructor
  · rintro ⟨c', i'⟩ hbody
    try dsimp (config := { zetaHave := true }) only at hbody
    split at hbody
    · rename_i hlt
      rw [usize_lt_iff_val] at hlt
      have hvlen : v.len.val = v.val.length := rfl
      rw [hvlen] at hlt
      rcases bind_eq_ok hbody with ⟨ei, _, hbody⟩
      rcases bind_eq_ok hbody with ⟨c1, hpush, hbody⟩
      rcases bind_eq_ok hbody with ⟨i1, hadd, hok⟩
      cases ok_inj hok
      have hp := vec_push_ok_len hpush
      have ha := usize_add_one_ok_val hadd
      try dsimp only
      omega
    · simp at hbody
  · intro c' hbody
    try dsimp (config := { zetaHave := true }) only at hbody
    split at hbody
    · rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
      rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
      rcases bind_eq_ok hbody with ⟨_, _, hok⟩
      simp at hok
    · rename_i hnlt
      rw [usize_lt_iff_val] at hnlt
      have hvlen : v.len.val = v.val.length := rfl
      rw [hvlen] at hnlt
      cases ok_inj hbody
      omega

private theorem apply_struct_access_mapping_preserves_len
    (sa : ebpf_core.verifier.StructAccess)
    (struct_mappings : Slice ebpf_core.program.StructFieldMapping)
    (code : alloc.vec.Vec ebpf_core.api.EbpfInstruction) :
    ResultOkPost
      (ebpf_core.program.apply_struct_access_mapping sa struct_mappings code)
      (fun (_, code') => code'.val.length = code.val.length) := by
  unfold ebpf_core.program.apply_struct_access_mapping
  prove_vec_post

private theorem link_single_instruction_preserves_len
    (pc : Usize) (vp : ebpf_core.verifier.VerifiedEbpfProgram)
    (linkage : ebpf_core.program.ProgramLinkageInfo) (has_packet_load_fn : Bool)
    (code : alloc.vec.Vec ebpf_core.api.EbpfInstruction) :
    ResultOkPost
      (ebpf_core.program.link_single_instruction pc vp linkage has_packet_load_fn code)
      (fun (_, code') => code'.val.length = code.val.length) := by
  unfold ebpf_core.program.link_single_instruction
  prove_vec_post

private theorem link_instructions_loop1_preserves_len
    (v : alloc.vec.Vec ebpf_core.verifier.StructAccess)
    (v1 : alloc.vec.Vec ebpf_core.program.StructFieldMapping)
    (code code' : alloc.vec.Vec ebpf_core.api.EbpfInstruction)
    (err err' : Option ebpf_core.error.LinkError) (sa_idx : Usize)
    (h : ebpf_core.program.link_instructions_loop1 v v1 code err sa_idx = .ok (code', err')) :
    code'.val.length = code.val.length := by
  unfold ebpf_core.program.link_instructions_loop1 at h
  refine loop_ok_induct
    (fun (c, e, idx) => ebpf_core.program.link_instructions_loop1.body v v1 c e idx)
    (fun (c, _, _) => c.val.length = code.val.length)
    (fun (c, _) => c.val.length = code.val.length)
    ?_ (code, err, sa_idx) (code', err') rfl h
  rintro ⟨c, e, idx⟩ hinv
  unfold ebpf_core.program.link_instructions_loop1.body
  try dsimp (config := { zetaHave := true }) only at hinv ⊢
  constructor
  · rintro ⟨c', e', idx'⟩ hbody
    try dsimp (config := { zetaHave := true }) only at hbody
    split at hbody
    · try dsimp (config := { zetaHave := true }) only at hbody
      split at hbody
      · rcases bind_eq_ok hbody with ⟨sa, _, hbody⟩
        try dsimp (config := { zetaHave := true }) only at hbody
        rcases bind_eq_ok hbody with ⟨⟨r, c1⟩, hmap, hbody⟩
        have hlen := apply_struct_access_mapping_preserves_len sa (alloc.vec.Vec.deref v1) c
          (r, c1) hmap
        cases r with
        | Ok _ =>
          rcases bind_eq_ok hbody with ⟨_, _, hok⟩
          cases ok_inj hok
          try dsimp only at hlen ⊢
          omega
        | Err _ =>
          cases ok_inj hbody
          try dsimp only at hlen ⊢
          omega
      · simp at hbody
    · simp at hbody
  · rintro ⟨c', e'⟩ hbody
    try dsimp (config := { zetaHave := true }) only at hbody
    split at hbody
    · try dsimp (config := { zetaHave := true }) only at hbody
      split at hbody
      · rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
        try dsimp (config := { zetaHave := true }) only at hbody
        rcases bind_eq_ok hbody with ⟨⟨r, _⟩, _, hbody⟩
        cases r with
        | Ok _ =>
          rcases bind_eq_ok hbody with ⟨_, _, hok⟩
          simp at hok
        | Err _ => simp at hbody
      · cases ok_inj hbody; exact hinv
    · cases ok_inj hbody; exact hinv

private theorem link_instructions_loop2_preserves_len
    (v : alloc.vec.Vec ebpf_core.api.EbpfInstruction)
    (v1 : alloc.vec.Vec ebpf_core.verifier.Type)
    (v2 : alloc.vec.Vec ebpf_core.verifier.StructAccess)
    (v3 : alloc.vec.Vec ebpf_core.maps.MapSchema)
    (v4 : alloc.vec.Vec ebpf_core.program.StructFieldMapping)
    (v5 : alloc.vec.Vec ebpf_core.program.HelperLinkage)
    (v6 : alloc.vec.Vec ebpf_core.program.MapLinkage)
    (o : Option U64) (has_packet_load_fn : Bool)
    (code code' : alloc.vec.Vec ebpf_core.api.EbpfInstruction)
    (err err' : Option ebpf_core.error.LinkError) (pc : Usize)
    (h : ebpf_core.program.link_instructions_loop2 v v1 v2 v3 v4 v5 v6 o has_packet_load_fn
      code err pc = .ok (code', err')) :
    code'.val.length = code.val.length := by
  unfold ebpf_core.program.link_instructions_loop2 at h
  refine loop_ok_induct
    (fun (c, e, p) => ebpf_core.program.link_instructions_loop2.body
      v v1 v2 v3 v4 v5 v6 o has_packet_load_fn c e p)
    (fun (c, _, _) => c.val.length = code.val.length)
    (fun (c, _) => c.val.length = code.val.length)
    ?_ (code, err, pc) (code', err') rfl h
  rintro ⟨c, e, p⟩ hinv
  unfold ebpf_core.program.link_instructions_loop2.body
  try dsimp (config := { zetaHave := true }) only at hinv ⊢
  constructor
  · rintro ⟨c', e', p'⟩ hbody
    try dsimp (config := { zetaHave := true }) only at hbody
    split at hbody
    · try dsimp (config := { zetaHave := true }) only at hbody
      split at hbody
      · rcases bind_eq_ok hbody with ⟨⟨r, c1⟩, hsingle, hbody⟩
        have hlen := link_single_instruction_preserves_len p _ _ has_packet_load_fn c
          (r, c1) hsingle
        cases r with
        | Ok _ =>
          rcases bind_eq_ok hbody with ⟨_, _, hok⟩
          cases ok_inj hok
          try dsimp only at hlen ⊢
          omega
        | Err _ =>
          cases ok_inj hbody
          try dsimp only at hlen ⊢
          omega
      · simp at hbody
    · simp at hbody
  · rintro ⟨c', e'⟩ hbody
    try dsimp (config := { zetaHave := true }) only at hbody
    split at hbody
    · try dsimp (config := { zetaHave := true }) only at hbody
      split at hbody
      · rcases bind_eq_ok hbody with ⟨⟨r, _⟩, _, hbody⟩
        cases r with
        | Ok _ =>
          rcases bind_eq_ok hbody with ⟨_, _, hok⟩
          simp at hok
        | Err _ => simp at hbody
      · cases ok_inj hbody; exact hinv
    · cases ok_inj hbody; exact hinv

theorem link_instructions_preserves_length
    (verified : ebpf_core.verifier.VerifiedEbpfProgram)
    (linkage : ebpf_core.program.ProgramLinkageInfo)
    (linkedCode : alloc.vec.Vec ebpf_core.api.EbpfInstruction)
    (h_link : ebpf_core.program.link_instructions verified linkage =
      .ok (.Ok linkedCode)) :
    linkedCode.val.length = verified.code.val.length := by
  unfold ebpf_core.program.link_instructions at h_link
  try dsimp (config := { zetaHave := true }) only at h_link
  by_cases hneq : (linkage.maps.len != verified.maps.len) = true
  · simp [hneq] at h_link
  · simp only [hneq] at h_link
    try dsimp only at h_link
    rcases bind_eq_ok h_link with ⟨⟨o, has_pkt⟩, _, h_link⟩
    rcases bind_eq_ok h_link with ⟨code0, hloop0, h_link⟩
    have hlen0 : code0.val.length = verified.code.val.length :=
      link_instructions_loop0_len verified.code (alloc.vec.Vec.new _) code0 0#usize
        rfl (Nat.zero_le _) hloop0
    rcases bind_eq_ok h_link with ⟨⟨code1, err1⟩, hloop1, h_link⟩
    have hlen1 := link_instructions_loop1_preserves_len
      verified.struct_access_instructions linkage.struct_mappings code0 code1 none err1 0#usize hloop1
    cases err1 with
    | some _ => simp at h_link
    | none =>
      rcases bind_eq_ok h_link with ⟨⟨code2, err2⟩, hloop2, h_link⟩
      have hlen2 := link_instructions_loop2_preserves_len
        verified.code verified.args verified.struct_access_instructions verified.maps
        linkage.struct_mappings linkage.helpers linkage.maps o has_pkt
        code1 code2 none err2 0#usize hloop2
      cases err2 with
      | some _ => simp at h_link
      | none =>
        cases ok_inj h_link
        omega

-- ============================================================================
-- 6. Theorem 10: `execute_core_simulates_run_steps`
-- ============================================================================

private theorem loop_unfold {α β : Type u}
    (body : α → Result (ControlFlow α β)) (x : α) :
    loop body x = (do
      let r ← body x
      match r with
      | .cont x' => loop body x'
      | .done y => Result.ok y) := by
  rw [loop.eq_def]
  rfl

theorem execute_core_simulates_run_steps
    {B : Type} (inst : ebpf_core.executor.EbpfMachineBackend B)
    (code : Slice ebpf_core.api.EbpfInstruction)
    (prog : Program) (env : ExecEnv) (layout : HostLayout) (secret : SecretInit)
    (R : B → MachineState → Prop)
    (fuel : Nat) (b : B) (st : MachineState)
    (istate : ebpf_core.executor.InterpreterState)
    (ret : BitVec 64) (finalWorld : WorldState)
    (h_sim : BackendSimulatesProgram inst code prog env layout secret R)
    (h_rel : R b st)
    (h_res : istate.result = none)
    (h_pc : istate.pc.val = st.pc)
    (h_regs : InterpreterRegsMatch layout st istate.registers)
    (h_run : runSteps prog env layout secret fuel st = some (.inl (ret, finalWorld))) :
    ∃ bFinal,
      ebpf_core.executor.execute_core_loop1 inst code b istate =
        .ok (.Ok ⟨ret⟩, bFinal) := by
  induction fuel generalizing b st istate with
  | zero =>
    simp [runSteps] at h_run
  | succ fuel' ih =>
    simp only [runSteps] at h_run
    rcases h_sim with ⟨h_size, h_len, h_step⟩
    have ⟨h_next, h_halt⟩ := h_step b st h_rel istate h_res h_pc h_regs
    generalize hst : step prog env layout secret st = out at h_run
    cases out with
    | stuck err =>
      simp at h_run
    | halted ret' wrld' =>
      simp only [Option.some.injEq, Sum.inl.injEq, Prod.mk.injEq] at h_run
      rcases h_run with ⟨rfl, rfl⟩
      rcases h_halt ret' wrld' hst with ⟨stateDone, b', hbody1, hdone_res, _⟩
      refine ⟨b', ?_⟩
      unfold ebpf_core.executor.execute_core_loop1
      rw [loop_unfold]
      dsimp only
      rw [hbody1]
      simp only [bind_ok]
      rw [loop_unfold]
      dsimp only
      have hbody2 : ebpf_core.executor.execute_core_loop1.body inst code b' stateDone =
          .ok (.done (.Ok ⟨ret'⟩, b')) := by
        unfold ebpf_core.executor.execute_core_loop1.body
        simp [core.option.Option.is_none, hdone_res]
      rw [hbody2]
      simp only [bind_ok]
    | next st' =>
      rcases h_next st' hst with ⟨stateNext, b', hbody1, hnext_res, hnext_pc, hnext_regs, hnext_rel⟩
      rcases ih b' st' stateNext
        hnext_rel hnext_res hnext_pc hnext_regs h_run with ⟨bFinal, hloop⟩
      refine ⟨bFinal, ?_⟩
      unfold ebpf_core.executor.execute_core_loop1 at hloop ⊢
      rw [loop_unfold]
      dsimp only
      rw [hbody1]
      simpa [bind_ok] using hloop

end StarnixEbpf.ApiConverterLinkerExec
