-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Proof.StackAndCtxOps

/-!
# Starnix eBPF Formal Verification - `ComputationContext`, `VerificationContext`, & Entry State

Specifications, structural validity (`CompCtxValid`), and relational 2-safety
(`StateRel2Deps`) bridges for:
- `Array` (`10#usize`), `clone_registers`, `eq_registers`
- `ComputationContext.{default, clone, eq, reg, compute_source, set_reg, update_array_bounds, set_null}`
- Resource removal (`unregister_resource` and `IdSet.remove`) under `StateRel2Deps`
- `checked_add_signed_usize`, `ComputationContext.{jump_with_offset, next, checked_jump_with_offset}`
- `ComputationContext.{check_memory_access, get_map_schema}` and `MemoryParameterSize.size`
- `VerificationContext.{new, emit_log, push_successor, next_id, register_memory_access}`
- `clear_dead_registers`, `verify_program_core_loop3`, and entry-state conformance (`EntryStatesConform`)
-/

set_option linter.unusedTactic false
set_option maxHeartbeats 800000

namespace StarnixEbpf.Proof

open Aeneas Aeneas.Std Aeneas.Std.WP Result
open StarnixEbpf.Assumptions
open StarnixEbpf.Spec
open StarnixEbpf.LoopLemmas
open StarnixEbpf.ApiConverterLinkerExec
open ebpf_core
open ebpf_core.scalar_value

/-! ## 1. `Array` (`10#usize`) & Register File Specifications -/

theorem usize_const_val_10 : (10#usize).val = 10 := by
  cases System.Platform.numBits_eq <;> simp_all [UScalar.val]

theorem register_count_val : api.REGISTER_COUNT.val = 11 := by
  unfold api.REGISTER_COUNT; rfl

theorem general_register_count_val : api.GENERAL_REGISTER_COUNT.val = 10 := by
  unfold api.GENERAL_REGISTER_COUNT; rfl

theorem argument_count_val : api.ARGUMENT_COUNT.val = 5 := by
  unfold api.ARGUMENT_COUNT; rfl

theorem uscalar_cast_usize_of_u8_val (x : Std.U8) :
    (UScalar.cast .Usize x).val = x.val := by
  have hx : x.val < 256 := x.bv.isLt
  cases System.Platform.numBits_eq with
  | inl h32 =>
    dsimp only [UScalar.cast, UScalar.val]
    rw [UScalarTy.Usize_numBits_eq, h32, BitVec.toNat_setWidth, UScalar.bv_toNat]
    exact Nat.mod_eq_of_lt (by omega)
  | inr h64 =>
    dsimp only [UScalar.cast, UScalar.val]
    rw [UScalarTy.Usize_numBits_eq, h64, BitVec.toNat_setWidth, UScalar.bv_toNat]
    exact Nat.mod_eq_of_lt (by omega)

theorem uscalar_cast_u8_of_usize_val {x : Std.Usize} (hx : x.val < 256) :
    (UScalar.cast .U8 x).val = x.val := by
  dsimp only [UScalar.cast, UScalar.val]
  have h8 : UScalarTy.U8.numBits = 8 := rfl
  rw [h8, BitVec.toNat_setWidth, UScalar.bv_toNat]
  exact Nat.mod_eq_of_lt hx

theorem array10_len {α : Type u} (a : Std.Array α 10#usize) : a.val.length = 10 := by
  rw [Std.Array.property, usize_const_val_10]

theorem array_index_usize_10_ok {α : Type u} (a : Std.Array α 10#usize) (i : Std.Usize)
    (hi : i.val < 10) :
    Std.Array.index_usize a i = ok (a.val[i.val]'(by rw [array10_len]; exact hi)) := by
  have hlen : i.val < a.val.length := by rw [array10_len]; exact hi
  unfold Std.Array.index_usize
  rw [Std.Array.getElem?_Usize_eq, List.getElem?_eq_getElem hlen]

theorem array_index_mut_usize_10_ok {α : Type u} (a : Std.Array α 10#usize) (i : Std.Usize)
    (hi : i.val < 10) :
    Std.Array.index_mut_usize a i =
      ok (a.val[i.val]'(by rw [array10_len]; exact hi), Std.Array.set a i) := by
  unfold Std.Array.index_mut_usize
  simp only [array_index_usize_10_ok a i hi, bind_tc_ok]

theorem array_update_10_ok {α : Type u} (a : Std.Array α 10#usize) (i : Std.Usize) (x : α)
    (hi : i.val < 10) :
    Std.Array.update a i x = ok (Std.Array.set a i x) := by
  have hlen : i.val < a.val.length := by rw [array10_len]; exact hi
  unfold Std.Array.update
  rw [Std.Array.getElem?_Usize_eq, List.getElem?_eq_getElem hlen]
  exact congrArg ok (by simp [Std.Array.set])

theorem array_to_slice_10_len {α : Type u} (a : Std.Array α 10#usize) :
    (Slice.len (Std.Array.to_slice a)).val = 10 := by
  simp [Std.Array.to_slice]

theorem clone_registers_spec (regs : Std.Array verifier.Type 10#usize) :
    verifier.clone_registers regs = ok regs := by
  have h0 : (0#usize).val < 10 := by rw [usize_const_val_0]; omega
  have h1 : (1#usize).val < 10 := by rw [usize_one_val]; omega
  have h2 : (2#usize).val < 10 := by rw [usize_two_val]; omega
  have h3 : (3#usize).val < 10 := by cases System.Platform.numBits_eq <;> simp_all [UScalar.val]
  have h4 : (4#usize).val < 10 := by rw [usize_const_val_4]; omega
  have h5 : (5#usize).val < 10 := by cases System.Platform.numBits_eq <;> simp_all [UScalar.val]
  have h6 : (6#usize).val < 10 := by cases System.Platform.numBits_eq <;> simp_all [UScalar.val]
  have h7 : (7#usize).val < 10 := by cases System.Platform.numBits_eq <;> simp_all [UScalar.val]
  have h8 : (8#usize).val < 10 := by rw [usize_const_val_8]; omega
  have h9 : (9#usize).val < 10 := by cases System.Platform.numBits_eq <;> simp_all [UScalar.val]
  unfold verifier.clone_registers
  simp only [array_index_usize_10_ok regs 0#usize h0, array_index_usize_10_ok regs 1#usize h1,
    array_index_usize_10_ok regs 2#usize h2, array_index_usize_10_ok regs 3#usize h3,
    array_index_usize_10_ok regs 4#usize h4, array_index_usize_10_ok regs 5#usize h5,
    array_index_usize_10_ok regs 6#usize h6, array_index_usize_10_ok regs 7#usize h7,
    array_index_usize_10_ok regs 8#usize h8, array_index_usize_10_ok regs 9#usize h9,
    Type_clone_spec, bind_ok]
  congr 1
  apply Std.Array.ext
  rw [Std.Array.make_val]
  have hv0 : (0#usize).val = 0 := usize_const_val_0
  have hv1 : (1#usize).val = 1 := usize_one_val
  have hv2 : (2#usize).val = 2 := usize_two_val
  have hv3 : (3#usize).val = 3 := by cases System.Platform.numBits_eq <;> simp_all [UScalar.val]
  have hv4 : (4#usize).val = 4 := usize_const_val_4
  have hv5 : (5#usize).val = 5 := by cases System.Platform.numBits_eq <;> simp_all [UScalar.val]
  have hv6 : (6#usize).val = 6 := by cases System.Platform.numBits_eq <;> simp_all [UScalar.val]
  have hv7 : (7#usize).val = 7 := by cases System.Platform.numBits_eq <;> simp_all [UScalar.val]
  have hv8 : (8#usize).val = 8 := usize_const_val_8
  have hv9 : (9#usize).val = 9 := by cases System.Platform.numBits_eq <;> simp_all [UScalar.val]
  apply List.ext_getElem
  · simp [array10_len regs]
  · intro j hj1 hj2
    have hj10 : j < 10 := by simp at hj1; exact hj1
    rcases (by omega : j = 0 ∨ j = 1 ∨ j = 2 ∨ j = 3 ∨ j = 4 ∨
        j = 5 ∨ j = 6 ∨ j = 7 ∨ j = 8 ∨ j = 9) with
      rfl | rfl | rfl | rfl | rfl | rfl | rfl | rfl | rfl | rfl <;>
      simp [hv0, hv1, hv2, hv3, hv4, hv5, hv6, hv7, hv8, hv9]

private theorem eq_registers_loop_spec
    (r1 r2 : Std.Array verifier.Type 10#usize) (n : Nat) (i : Std.Usize)
    (hn : 10 - i.val = n) (hle : i.val ≤ 10) :
    ∃ b : Bool,
      verifier.eq_registers_loop r1 r2 i = ok b ∧
      (b = true ↔ ∀ j (_hj : i.val ≤ j) (hj10 : j < 10),
        (r1.val[j]'(by rw [array10_len]; exact hj10)) =
        (r2.val[j]'(by rw [array10_len]; exact hj10))) := by
  induction n generalizing i with
  | zero =>
    have hi10 : i.val = 10 := by omega
    unfold verifier.eq_registers_loop
    rw [loop_unfold_step]
    unfold verifier.eq_registers_loop.body
    simp only [lift]
    have hcast : (UScalar.cast .Usize api.GENERAL_REGISTER_COUNT).val = 10 := by
      rw [uscalar_cast_usize_of_u8_val, general_register_count_val]
    have hnlt : ¬ (i < UScalar.cast .Usize api.GENERAL_REGISTER_COUNT) := by
      intro hc; have : i.val < (UScalar.cast .Usize api.GENERAL_REGISTER_COUNT).val := hc; omega
    simp only [bind_ok, hnlt, ↓reduceIte]
    exact ⟨true, rfl, ⟨(fun _ j hj hj10 => by omega), fun _ => rfl⟩⟩
  | succ k ih =>
    have hilt10 : i.val < 10 := by omega
    unfold verifier.eq_registers_loop
    rw [loop_unfold_step]
    have hcast : (UScalar.cast .Usize api.GENERAL_REGISTER_COUNT).val = 10 := by
      rw [uscalar_cast_usize_of_u8_val, general_register_count_val]
    have hlt : i < UScalar.cast .Usize api.GENERAL_REGISTER_COUNT := by
      show i.val < _; omega
    obtain ⟨bne, hbne, hne_iff⟩ := Type_ne_spec
      (r1.val[i.val]'(by rw [array10_len]; exact hilt10))
      (r2.val[i.val]'(by rw [array10_len]; exact hilt10))
    cases bne with
    | true =>
      have hbody : verifier.eq_registers_loop.body r1 r2 i = ok (ControlFlow.done false) := by
        unfold verifier.eq_registers_loop.body
        simp only [lift, bind_ok, hlt, ↓reduceIte,
          array_index_usize_10_ok r1 i hilt10, array_index_usize_10_ok r2 i hilt10, hbne]
      simp only [hbody, bind_ok]
      have hneq : (r1.val[i.val]'(by rw [array10_len]; exact hilt10)) ≠
          (r2.val[i.val]'(by rw [array10_len]; exact hilt10)) := hne_iff.mp rfl
      refine ⟨false, rfl, ⟨(fun hc => by cases hc), fun hall => ?_⟩⟩
      exact absurd (hall i.val (Nat.le_refl _) hilt10) hneq
    | false =>
      have heq_i : (r1.val[i.val]'(by rw [array10_len]; exact hilt10)) =
          (r2.val[i.val]'(by rw [array10_len]; exact hilt10)) :=
        Classical.not_not.mp (fun hc => by have := hne_iff.mpr hc; cases this)
      have hmax : 10 ≤ Usize.max := by have := usize_65536_le_max; omega
      obtain ⟨i2, hi2, hi2_val⟩ := usize_add_eq_ok i 1#usize (by rw [usize_one_val]; omega)
      rw [usize_one_val] at hi2_val
      have hbody : verifier.eq_registers_loop.body r1 r2 i = ok (ControlFlow.cont i2) := by
        unfold verifier.eq_registers_loop.body
        simp only [lift, bind_ok, hlt, ↓reduceIte, Bool.false_eq_true,
          array_index_usize_10_ok r1 i hilt10, array_index_usize_10_ok r2 i hilt10, hbne, hi2]
      simp only [hbody, bind_ok]
      obtain ⟨b, hb, hb_iff⟩ := ih i2 (by omega) (by omega)
      unfold verifier.eq_registers_loop at hb
      refine ⟨b, hb, ?_⟩
      rw [hb_iff]
      constructor
      · intro hrest j hj hj10
        by_cases hji : j = i.val
        · subst hji; exact heq_i
        · exact hrest j (by omega) hj10
      · intro hall j hj hj10
        exact hall j (by omega) hj10

theorem eq_registers_spec (r1 r2 : Std.Array verifier.Type 10#usize) :
    ∃ b : Bool,
      verifier.eq_registers r1 r2 = ok b ∧
      (b = true ↔ r1 = r2) := by
  unfold verifier.eq_registers
  obtain ⟨b, hb, hiff⟩ := eq_registers_loop_spec r1 r2 10 0#usize
    (by rw [usize_const_val_0]) (by rw [usize_const_val_0]; omega)
  refine ⟨b, hb, ?_⟩
  rw [hiff, Std.Array.eq_iff]
  constructor
  · intro hall
    apply List.ext_getElem
    · rw [array10_len, array10_len]
    · intro j hj1 hj2
      have hj10 : j < 10 := by rw [array10_len] at hj1; exact hj1
      exact hall j (by rw [usize_const_val_0]; omega) hj10
  · intro heq j _ _
    simp [heq]

/-! ## 2. `ComputationContext` Core Methods & `StateRel2Deps` Bridges -/

theorem ComputationContext_default_spec {env : ExecEnv} {vCounter : Nat} :
    ∃ ctx : verifier.ComputationContext,
      verifier.ComputationContext.Insts.CoreDefaultDefault.default = ok ctx ∧
      ctx.pc = 0#usize ∧
      ctx.registers.val = List.replicate 10 (.ScalarValue uninitScalar64) ∧
      ctx.stack = { mask := 0#u64, data := alloc.vec.Vec.new verifier.Type } ∧
      ctx.array_bounds = { entries := alloc.vec.Vec.new (MemoryId × Std.U64) } ∧
      ctx.resources = { entries := alloc.vec.Vec.new MemoryId } ∧
      CompCtxValid env vCounter ctx := by
  unfold verifier.ComputationContext.Insts.CoreDefaultDefault.default
  have hs_def : verifier.Stack.Insts.CoreDefaultDefault.default =
      ok { mask := 0#u64, data := alloc.vec.Vec.new verifier.Type } := rfl
  have hbm_def : verifier.BoundsMap.new =
      ok { entries := alloc.vec.Vec.new (MemoryId × Std.U64) } := rfl
  have his_def : verifier.IdSet.new =
      ok { entries := alloc.vec.Vec.new MemoryId } := rfl
  obtain ⟨s, hs, hsv, _⟩ := Stack_default_spec (env := env) (vCounter := vCounter)
  obtain ⟨bm, hbm, _, hbmv⟩ := BoundsMap_new_spec vCounter
  obtain ⟨is, his, _, hisv⟩ := IdSet_new_spec vCounter
  rw [hs_def] at hs; cases ok_inj hs
  rw [hbm_def] at hbm; cases ok_inj hbm
  rw [his_def] at his; cases ok_inj his
  simp only [Type_UNINITIALIZED_eq, hs_def, hbm_def, his_def, bind_ok]
  refine ⟨_, rfl, rfl, ?_, rfl, rfl, rfl, ?_⟩
  · rw [Std.Array.make_val]; rfl
  · refine ⟨?_, hsv, hbmv, hisv⟩
    intro t ht
    rw [Std.Array.make_val] at ht
    simp only [List.mem_cons, List.not_mem_nil, or_false] at ht
    rcases ht with rfl | rfl | rfl | rfl | rfl | rfl | rfl | rfl | rfl | rfl <;>
      exact uninitScalar64_valid

theorem ComputationContext_clone_spec (ctx : verifier.ComputationContext) :
    verifier.ComputationContext.Insts.CoreCloneClone.clone ctx = ok ctx := by
  unfold verifier.ComputationContext.Insts.CoreCloneClone.clone
  simp only [clone_registers_spec ctx.registers, Stack_clone_spec ctx.stack,
    BoundsMap_clone_spec ctx.array_bounds, IdSet_clone_spec ctx.resources, bind_ok]

theorem ComputationContext_eq_spec (a b : verifier.ComputationContext) :
    ∃ r : Bool,
      verifier.ComputationContext.Insts.CoreCmpPartialEqComputationContext.eq a b = ok r ∧
      (r = true ↔
        a.pc = b.pc ∧
        a.registers = b.registers ∧
        a.stack = b.stack ∧
        a.array_bounds = b.array_bounds ∧
        a.resources = b.resources) := by
  unfold verifier.ComputationContext.Insts.CoreCmpPartialEqComputationContext.eq
  by_cases hpc : a.pc = b.pc
  · simp only [hpc, ↓reduceIte]
    obtain ⟨br, hbr, hr_iff⟩ := eq_registers_spec a.registers b.registers
    simp only [hbr, bind_ok]
    cases br with
    | false =>
      simp only [Bool.false_eq_true, ↓reduceIte]
      exact ⟨false, rfl, ⟨(fun hc => by cases hc), fun h => absurd (hr_iff.mpr h.2.1) (by simp)⟩⟩
    | true =>
      simp only [↓reduceIte]
      have hregs := hr_iff.mp rfl
      obtain ⟨bs, hbs, hs_iff⟩ := Stack_eq_spec a.stack b.stack
      simp only [hbs, bind_ok]
      cases bs with
      | false =>
        simp only [Bool.false_eq_true, ↓reduceIte]
        exact ⟨false, rfl, ⟨(fun hc => by cases hc), fun h => absurd (hs_iff.mpr h.2.2.1) (by simp)⟩⟩
      | true =>
        simp only [↓reduceIte]
        have hstk := hs_iff.mp rfl
        obtain ⟨bbm, hbbm, hbm_iff⟩ := BoundsMap_eq_spec a.array_bounds b.array_bounds
        simp only [hbbm, bind_ok]
        cases bbm with
        | false =>
          simp only [Bool.false_eq_true, ↓reduceIte]
          exact ⟨false, rfl, ⟨(fun hc => by cases hc), fun h => absurd (hbm_iff.mpr h.2.2.2.1) (by simp)⟩⟩
        | true =>
          simp only [↓reduceIte]
          have hbm := hbm_iff.mp rfl
          obtain ⟨bis, hbis, his_iff⟩ := IdSet_eq_spec a.resources b.resources
          refine ⟨bis, hbis, ?_⟩
          rw [his_iff]
          exact ⟨fun his => ⟨trivial, hregs, hstk, hbm, his⟩, fun h => h.2.2.2.2⟩
  · simp only [hpc, ↓reduceIte]
    exact ⟨false, rfl, ⟨(fun hc => by cases hc), fun h => False.elim h.1⟩⟩

theorem ComputationContext_reg_spec (self : verifier.ComputationContext) (index : Std.U8) :
    ∃ r : core.result.Result verifier.Type error.VerifierErrorKind,
      verifier.ComputationContext.reg self index = ok r ∧
      (if index.val ≥ 11 then
         r = .Err (.InvalidRegister index)
       else if h10 : index.val < 10 then
         r = .Ok (self.registers.val[index.val]'(by rw [array10_len]; exact h10))
       else
         r = .Ok (.PtrToStack (knownScalar64 512#u64))) := by
  unfold verifier.ComputationContext.reg
  by_cases h11 : index.val ≥ 11
  · have hge : index >= api.REGISTER_COUNT := by
      show api.REGISTER_COUNT.val ≤ index.val; rw [register_count_val]; exact h11
    simp only [hge, ↓reduceIte, h11]
    exact ⟨_, rfl, rfl⟩
  · have hnge : ¬ (index >= api.REGISTER_COUNT) := by
      intro hc; have : api.REGISTER_COUNT.val ≤ index.val := hc
      rw [register_count_val] at this; omega
    simp only [hnge, ↓reduceIte, h11]
    by_cases h10 : index.val < 10
    · have hlt10 : index < api.GENERAL_REGISTER_COUNT := by
        show index.val < api.GENERAL_REGISTER_COUNT.val; rw [general_register_count_val]; exact h10
      have hi : (UScalar.cast .Usize index).val < 10 := by
        rw [uscalar_cast_usize_of_u8_val]; exact h10
      simp only [hlt10, ↓reduceIte, lift, bind_ok, h10,
        array_index_usize_10_ok self.registers (UScalar.cast .Usize index) hi,
        Type_clone_spec]
      refine ⟨_, rfl, ?_⟩
      congr 1
      have hcast_eq := uscalar_cast_usize_of_u8_val index
      simp only [hcast_eq]
    · have hnlt10 : ¬ (index < api.GENERAL_REGISTER_COUNT) := by
        intro hc; have : index.val < api.GENERAL_REGISTER_COUNT.val := hc
        rw [general_register_count_val] at this; omega
      simp only [hnlt10, ↓reduceIte, h10]
      rw [bind_ok_eq StackOffset_default_spec]
      exact ⟨_, rfl, rfl⟩

theorem ComputationContext_reg_ok_valid_and_rel2
    {env : ExecEnv} {vCounter : Nat} {ctx : verifier.ComputationContext}
    {deps : verifier.DataDependencies} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {st₁ st₂ : MachineState}
    {index : Std.U8} {t : verifier.Type}
    (hvalid : CompCtxValid env vCounter ctx)
    (hrel : StateRel2Deps env ctx deps ρ resActive isNullId st₁ st₂)
    (hlive : index.val = 10 ∨ DataDepsHasReg deps index.val = true)
    (hreg : verifier.ComputationContext.reg ctx index = ok (.Ok t)) :
    index.val < REGISTER_COUNT ∧
    TypeValid env vCounter t ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen t (st₁.regs index.val) (st₂.regs index.val) := by
  obtain ⟨r, hr, hspec⟩ := ComputationContext_reg_spec ctx index
  rw [hreg] at hr
  have hr_eq : r = .Ok t := (ok_inj hr).symm
  subst r
  by_cases h11 : index.val ≥ 11
  · simp [h11] at hspec
  · simp only [ite_eq_right_iff.mpr (fun h => absurd h h11)] at hspec
    have hlt11 : index.val < REGISTER_COUNT := by unfold REGISTER_COUNT; omega
    by_cases h10 : index.val < 10
    · simp only [h10, ↓reduceDIte] at hspec
      cases hspec
      have hlen : index.val < ctx.registers.val.length := by rw [array10_len]; exact h10
      have htv : TypeValid env vCounter (ctx.registers.val[index.val]'hlen) :=
        hvalid.1 _ (List.getElem_mem hlen)
      rcases hlive with h10_eq | hd
      · omega
      · exact ⟨hlt11, htv, hrel.2.2.2.2.2.2.2.2.2.2.1 index.val h10 hlen hd⟩
    · simp only [h10, ↓reduceDIte] at hspec
      cases hspec
      have hi10 : index.val = GENERAL_REGISTER_COUNT := by unfold GENERAL_REGISTER_COUNT; omega
      have hpov : PtrOffsetValid 0xFFFF_FFFF#64 (knownScalar64 512#u64) :=
        ⟨knownScalar64_valid 512#u64, rfl, by decide⟩
      refine ⟨hlt11, hpov, ?_⟩
      rw [hi10, hrel.2.2.2.2.2.2.2.2.1, hrel.2.2.2.2.2.2.2.2.2.1]
      exact ⟨hpov, 512#64, concretizesScalar64Rel2_known 512#u64, rfl, rfl⟩

theorem ComputationContext_compute_source_spec
    (self : verifier.ComputationContext) (src : visitor.Source) :
    ∃ r : core.result.Result verifier.Type error.VerifierErrorKind,
      verifier.ComputationContext.compute_source self src = ok r ∧
      match src with
      | .Reg reg => verifier.ComputationContext.reg self reg = ok r
      | .Value v => r = .Ok (.ScalarValue (knownScalar64 v)) := by
  unfold verifier.ComputationContext.compute_source
  cases src with
  | Reg reg =>
    obtain ⟨r, hr, _⟩ := ComputationContext_reg_spec self reg
    exact ⟨r, hr, hr⟩
  | Value v =>
    dsimp only
    rw [bind_ok_eq (from_known_64_eq v)]
    exact ⟨_, rfl, rfl⟩

theorem ComputationContext_compute_source_ok_valid_and_rel2
    {env : ExecEnv} {vCounter : Nat} {ctx : verifier.ComputationContext}
    {deps : verifier.DataDependencies} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {st₁ st₂ : MachineState}
    {src : visitor.Source} {t : verifier.Type}
    (hvalid : CompCtxValid env vCounter ctx)
    (hrel : StateRel2Deps env ctx deps ρ resActive isNullId st₁ st₂)
    (hlive : match src with
      | .Reg r => r.val = 10 ∨ DataDepsHasReg deps r.val = true
      | .Value _ => True)
    (hsrc : verifier.ComputationContext.compute_source ctx src = ok (.Ok t)) :
    TypeValid env vCounter t ∧
    ∃ v₁ v₂ : RuntimeVal,
      (match operandOfSource src with
       | .imm v => v₁ = .scalar v ∧ v₂ = .scalar v
       | .reg r => r < REGISTER_COUNT ∧ v₁ = st₁.regs r ∧ v₂ = st₂.regs r) ∧
      TypeRel2 env ρ resActive isNullId st₁.world.arrayLen t v₁ v₂ := by
  obtain ⟨r, hr, hspec⟩ := ComputationContext_compute_source_spec ctx src
  rw [hsrc] at hr
  have hr_eq : r = .Ok t := (ok_inj hr).symm
  subst r
  cases src with
  | Reg reg =>
    obtain ⟨hlt, htv, htr⟩ := ComputationContext_reg_ok_valid_and_rel2 hvalid hrel hlive hspec
    exact ⟨htv, st₁.regs reg.val, st₂.regs reg.val, ⟨hlt, rfl, rfl⟩, htr⟩
  | Value v =>
    cases hspec
    refine ⟨knownScalar64_valid v, .scalar v.bv, .scalar v.bv, ⟨rfl, rfl⟩, ?_⟩
    simp only [TypeRel2, knownScalar64_ne_uninit, ↓reduceIte]
    exact ⟨v.bv, v.bv, rfl, rfl, concretizesScalar64Rel2_known v⟩

theorem ComputationContext_set_reg_spec
    (self : verifier.ComputationContext) (index : Std.U8) (reg_type : verifier.Type) :
    ∃ r ctx',
      verifier.ComputationContext.set_reg self index reg_type = ok (r, ctx') ∧
      (if index.val ≥ 11 then
         r = .Err (.InvalidRegister index) ∧ ctx' = self
       else if index.val ≥ 10 then
         r = .Err (.WriteToReadOnlyRegister index) ∧ ctx' = self
       else
         r = .Ok () ∧
         ctx'.pc = self.pc ∧
         ctx'.registers.val = self.registers.val.set index.val reg_type ∧
         ctx'.stack = self.stack ∧
         ctx'.array_bounds = self.array_bounds ∧
         ctx'.resources = self.resources) := by
  unfold verifier.ComputationContext.set_reg
  simp only [lift, bind_ok]
  have hmassert : ((index >= api.REGISTER_COUNT) ||
      ((index >= api.GENERAL_REGISTER_COUNT) ||
        (UScalar.cast .Usize index < 10#usize))) = true := by
    by_cases h10 : index.val < 10
    · have hlt : UScalar.cast .Usize index < 10#usize := by
        show (UScalar.cast .Usize index).val < (10#usize).val
        rw [uscalar_cast_usize_of_u8_val, usize_const_val_10]; exact h10
      simp [hlt]
    · have hge10 : index >= api.GENERAL_REGISTER_COUNT := by
        show api.GENERAL_REGISTER_COUNT.val ≤ index.val
        rw [general_register_count_val]; omega
      simp [hge10]
  have hmassert_ok : massert
      ((decide (index ≥ api.REGISTER_COUNT) ||
        (decide (index ≥ api.GENERAL_REGISTER_COUNT) ||
          decide (UScalar.cast .Usize index < 10#usize))) = true) = ok () := by
    unfold massert
    simp only [hmassert, ↓reduceIte]
  simp only [hmassert_ok, bind_ok]
  by_cases h11 : index.val ≥ 11
  · have hge11 : index >= api.REGISTER_COUNT := by
      show api.REGISTER_COUNT.val ≤ index.val; rw [register_count_val]; exact h11
    simp only [hge11, ↓reduceIte, h11]
    exact ⟨_, _, rfl, rfl, rfl⟩
  · have hnge11 : ¬ (index >= api.REGISTER_COUNT) := by
      intro hc; have : api.REGISTER_COUNT.val ≤ index.val := hc
      rw [register_count_val] at this; omega
    simp only [hnge11, ↓reduceIte, h11]
    by_cases h10 : index.val ≥ 10
    · have hge10 : index >= api.GENERAL_REGISTER_COUNT := by
        show api.GENERAL_REGISTER_COUNT.val ≤ index.val; rw [general_register_count_val]; exact h10
      simp only [hge10, ↓reduceIte, h10]
      exact ⟨_, _, rfl, rfl, rfl⟩
    · have hnge10 : ¬ (index >= api.GENERAL_REGISTER_COUNT) := by
        intro hc; have : api.GENERAL_REGISTER_COUNT.val ≤ index.val := hc
        rw [general_register_count_val] at this; omega
      simp only [hnge10, ↓reduceIte, h10]
      have hilt10 : (UScalar.cast .Usize index).val < 10 := by
        rw [uscalar_cast_usize_of_u8_val]; omega
      simp only [array_index_mut_usize_10_ok self.registers (UScalar.cast .Usize index) hilt10,
        array_update_10_ok _ (UScalar.cast .Usize index) reg_type hilt10, bind_ok]
      refine ⟨_, _, rfl, rfl, rfl, ?_, rfl, rfl, rfl⟩
      have hlen10 : index.val < self.registers.val.length := by rw [array10_len]; omega
      simp only [Std.Array.set_val_eq, uscalar_cast_usize_of_u8_val]
      rw [List.set_set]

theorem ComputationContext_set_reg_ok_spec
    {env : ExecEnv} {vCounter : Nat} {self ctx' : verifier.ComputationContext}
    {index : Std.U8} {reg_type : verifier.Type}
    (hvalid : CompCtxValid env vCounter self)
    (ht : TypeValid env vCounter reg_type)
    (hset : verifier.ComputationContext.set_reg self index reg_type = ok (.Ok (), ctx')) :
    index.val < GENERAL_REGISTER_COUNT ∧
    ctx'.pc = self.pc ∧
    ctx'.registers.val = self.registers.val.set index.val reg_type ∧
    ctx'.stack = self.stack ∧
    ctx'.array_bounds = self.array_bounds ∧
    ctx'.resources = self.resources ∧
    CompCtxValid env vCounter ctx' := by
  obtain ⟨r, c, hr, hspec⟩ := ComputationContext_set_reg_spec self index reg_type
  rw [hset] at hr
  rcases Prod.mk.inj (ok_inj hr) with ⟨rfl, rfl⟩
  by_cases h11 : index.val ≥ 11
  · simp [h11] at hspec
  · simp only [h11, ↓reduceIte] at hspec
    by_cases h10 : index.val ≥ 10
    · simp [h10] at hspec
    · simp only [h10, ↓reduceIte] at hspec
      rcases hspec with ⟨_, hpc, hregs, hstk, hbm, hres⟩
      have hlt10 : index.val < GENERAL_REGISTER_COUNT := by unfold GENERAL_REGISTER_COUNT; omega
      refine ⟨hlt10, hpc, hregs, hstk, hbm, hres, ?_, hstk ▸ hvalid.2.1, hbm ▸ hvalid.2.2.1, hres ▸ hvalid.2.2.2⟩
      intro t ht_in
      rw [hregs] at ht_in
      rcases List.mem_or_eq_of_mem_set ht_in with hm | rfl
      · exact hvalid.1 t hm
      · exact ht

theorem StateRel2Deps_set_reg
    {env : ExecEnv} {ctx ctx' : verifier.ComputationContext}
    {dPre dPost : verifier.DataDependencies} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {st₁ st₂ : MachineState}
    {dst : Nat} {reg_type : verifier.Type} {v₁ v₂ : RuntimeVal}
    (hdst : dst < 10)
    (hregs_eq : ctx'.registers.val = ctx.registers.val.set dst reg_type)
    (hst_eq : ctx'.stack = ctx.stack)
    (hbm_eq : ctx'.array_bounds = ctx.array_bounds)
    (hres_eq : ctx'.resources = ctx.resources)
    (hreg_deps : ∀ r, r < 10 → r ≠ dst → DataDepsHasReg dPost r = true → DataDepsHasReg dPre r = true)
    (hstk_deps : ∀ s, s < 64 → DataDepsHasStack dPost s = true → DataDepsHasStack dPre s = true)
    (hrel : StateRel2Deps env ctx dPre ρ resActive isNullId st₁ st₂)
    (hv_rel : DataDepsHasReg dPost dst = true →
      TypeRel2 env ρ resActive isNullId st₁.world.arrayLen reg_type v₁ v₂) :
    StateRel2Deps env ctx' dPost ρ resActive isNullId
      { st₁ with pc := ctx'.pc.val, regs := updateReg st₁.regs dst v₁, world := bumpWorldStep st₁.world }
      { st₂ with pc := ctx'.pc.val, regs := updateReg st₂.regs dst v₂, world := bumpWorldStep st₂.world } := by
  rcases hrel with ⟨_, _, hw, harr, hnull, hnodup, hlive, hbnds, hr10_1, hr10_2, hregs, hstk⟩
  refine ⟨rfl, rfl, by simp [bumpWorldStep, hw], harr, hnull, hnodup,
    by simpa [bumpWorldStep, hres_eq] using hlive,
    by simpa [bumpWorldStep, hbm_eq] using hbnds,
    ?_, ?_, ?_, ?_⟩
  · have hne10 : (GENERAL_REGISTER_COUNT == dst) = false :=
      beq_false_of_ne (by unfold GENERAL_REGISTER_COUNT; omega)
    simp [updateReg, hne10, hr10_1]
  · have hne10 : (GENERAL_REGISTER_COUNT == dst) = false :=
      beq_false_of_ne (by unfold GENERAL_REGISTER_COUNT; omega)
    simp [updateReg, hne10, hr10_2]
  · intro r hr hlen hd
    by_cases hrd : r = dst
    · subst hrd
      have hbeq : (r == r) = true := beq_self_eq_true r
      simp only [updateReg, hbeq, ↓reduceIte, bumpWorldStep]
      have hget : (ctx'.registers.val[r]'hlen) = reg_type := by
        simp [hregs_eq, List.getElem_set_self]
      rw [hget]
      exact hv_rel hd
    · have hbeq : (r == dst) = false := beq_false_of_ne hrd
      simp only [updateReg, hbeq, Bool.false_eq_true, ↓reduceIte, bumpWorldStep]
      have hlen_pre : r < ctx.registers.val.length := by rw [array10_len]; exact hr
      have hget : (ctx'.registers.val[r]'hlen) = (ctx.registers.val[r]'hlen_pre) := by
        simp [hregs_eq, List.getElem_set_ne (Ne.symm hrd)]
      rw [hget]
      exact hregs r hr hlen_pre (hreg_deps r hr hrd hd)
  · intro slot hslot hd
    simpa [bumpWorldStep, hst_eq] using hstk slot hslot (hstk_deps slot hslot hd)

theorem ComputationContext_update_array_bounds_ok_spec
    {env : ExecEnv} {vCounter : Nat} {self ctx' : verifier.ComputationContext}
    {mid : MemoryId} {new_bound : U64ScalarValueData}
    (hvalid : CompCtxValid env vCounter self)
    (hid : memIdBounded vCounter mid = true)
    (hupd : verifier.ComputationContext.update_array_bounds self mid new_bound = ok ctx') :
    ctx'.pc = self.pc ∧
    ctx'.registers = self.registers ∧
    ctx'.stack = self.stack ∧
    ctx'.resources = self.resources ∧
    CompCtxValid env vCounter ctx' ∧
    (∀ (m : MemoryId) (b : Std.U64), (m, b) ∈ ctx'.array_bounds.entries.val →
      (m = mid ∧ b = new_bound.urange.min) ∨ (m, b) ∈ self.array_bounds.entries.val) ∧
    (∃ b : Std.U64, (mid, b) ∈ ctx'.array_bounds.entries.val ∧ new_bound.urange.min.bv ≤ b.bv) := by
  unfold verifier.ComputationContext.update_array_bounds at hupd
  have hmin : U64ScalarValueData.min new_bound = ok new_bound.urange.min := by
    unfold U64ScalarValueData.min U64Range.min; rfl
  rw [bind_ok_eq hmin] at hupd
  rcases bind_eq_ok hupd with ⟨bm', hbm_upd, hok⟩
  cases ok_inj hok
  obtain ⟨hbm_valid, hmem, hex⟩ := BoundsMap_update_max_ok_spec hvalid.2.2.1 hid hbm_upd
  exact ⟨rfl, rfl, rfl, rfl, ⟨hvalid.1, hvalid.2.1, hbm_valid, hvalid.2.2.2⟩, hmem, hex⟩

theorem StateRel2Deps_update_array_bounds
    {env : ExecEnv} {vCounter : Nat} {ctx ctx' : verifier.ComputationContext}
    {deps : verifier.DataDependencies} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {st₁ st₂ : MachineState}
    {mid : MemoryId} {new_bound : U64ScalarValueData}
    (hvalid : CompCtxValid env vCounter ctx)
    (hid : memIdBounded vCounter mid = true)
    (hbound_le : new_bound.urange.min.bv ≤ st₁.world.arrayLen (renameMemId ρ mid))
    (hupd : verifier.ComputationContext.update_array_bounds ctx mid new_bound = ok ctx')
    (hrel : StateRel2Deps env ctx deps ρ resActive isNullId st₁ st₂) :
    CompCtxValid env vCounter ctx' ∧
    StateRel2Deps env ctx' deps ρ resActive isNullId st₁ st₂ := by
  obtain ⟨hpc, hregs, hstk, hres, hcvalid, hmem, _⟩ :=
    ComputationContext_update_array_bounds_ok_spec hvalid hid hupd
  rcases hrel with ⟨hpc1, hpc2, hw, harr, hnull, hnodup, hlive, hbnds, hr10_1, hr10_2, hreg_rel, hstk_rel⟩
  refine ⟨hcvalid, ⟨hpc ▸ hpc1, hpc ▸ hpc2, hw, harr, hnull, hnodup, hres ▸ hlive, ?_,
    hr10_1, hr10_2, hregs ▸ hreg_rel, hstk ▸ hstk_rel⟩⟩
  intro m b hmb
  rcases hmem m b hmb with ⟨rfl, rfl⟩ | hold
  · exact hbound_le
  · exact hbnds m b hold

private theorem ComputationContext_set_null_loop_spec
    (null_id : MemoryId) (is_null : Bool) (n : Nat)
    (self : verifier.ComputationContext) (i : Std.Usize)
    (hn : 10 - i.val = n) (hle : i.val ≤ 10) :
    ∃ aOut : Std.Array verifier.Type 10#usize,
      verifier.ComputationContext.set_null_loop self null_id is_null i =
        ok (self.pc, aOut, self.stack, self.array_bounds, self.resources,
            self.path_depth, self.jmp_seq) ∧
      (∀ j (hj10 : j < 10),
        (aOut.val[j]'(by rw [array10_len]; exact hj10)) =
          if i.val ≤ j then
            pureTypeSetNull (self.registers.val[j]'(by rw [array10_len]; exact hj10)) null_id is_null
          else
            self.registers.val[j]'(by rw [array10_len]; exact hj10)) := by
  induction n generalizing self i with
  | zero =>
    have hi10 : i.val = 10 := by omega
    unfold verifier.ComputationContext.set_null_loop
    rw [loop_unfold_step]
    unfold verifier.ComputationContext.set_null_loop.body
    simp only [lift, bind_ok]
    have hslen : (Slice.len (Std.Array.to_slice self.registers)).val = 10 :=
      array_to_slice_10_len self.registers
    have hnlt : ¬ (i < Slice.len (Std.Array.to_slice self.registers)) := by
      intro hc; have : i.val < (Slice.len (Std.Array.to_slice self.registers)).val := hc; omega
    simp only [hnlt, ↓reduceIte, bind_ok]
    refine ⟨self.registers, rfl, fun j hj10 => ?_⟩
    have hnj : ¬ (i.val ≤ j) := by omega
    simp [hnj]
  | succ k ih =>
    have hilt10 : i.val < 10 := by omega
    unfold verifier.ComputationContext.set_null_loop
    rw [loop_unfold_step]
    have hslen : (Slice.len (Std.Array.to_slice self.registers)).val = 10 :=
      array_to_slice_10_len self.registers
    have hlt : i < Slice.len (Std.Array.to_slice self.registers) := by
      show i.val < _; omega
    have hmax : 10 ≤ Usize.max := by have := usize_65536_le_max; omega
    obtain ⟨i2, hi2, hi2_val⟩ := usize_add_eq_ok i 1#usize (by rw [usize_one_val]; omega)
    rw [usize_one_val] at hi2_val
    let newReg := pureTypeSetNull
      (self.registers.val[i.val]'(by rw [array10_len]; exact hilt10)) null_id is_null
    set nextCtx : verifier.ComputationContext :=
      { self with registers := Std.Array.set self.registers i newReg }
    have hbody : verifier.ComputationContext.set_null_loop.body null_id is_null self i =
        ok (ControlFlow.cont (nextCtx, i2)) := by
      unfold verifier.ComputationContext.set_null_loop.body
      simp only [lift, bind_ok, hlt, ↓reduceIte,
        array_index_mut_usize_10_ok self.registers i hilt10,
        Type_set_null_spec, hi2]
      rfl
    simp only [hbody, bind_ok]
    obtain ⟨aOut, hloop, haOut⟩ := ih nextCtx i2 (by omega) (by omega)
    unfold verifier.ComputationContext.set_null_loop at hloop
    refine ⟨aOut, hloop, fun j hj10 => ?_⟩
    rw [haOut j hj10]
    by_cases hji : j = i.val
    · subst hji
      have hle_self : i.val ≤ i.val := Nat.le_refl _
      have hnle2 : ¬ (i2.val ≤ i.val) := by omega
      simp only [hnle2, hle_self, ↓reduceIte, nextCtx, newReg, Std.Array.set_val_eq, List.getElem_set_self]
    · by_cases hij : i.val ≤ j
      · have hij2 : i2.val ≤ j := by omega
        simp only [hij2, hij, ↓reduceIte, nextCtx, Std.Array.set_val_eq, List.getElem_set_ne (Ne.symm hji)]
      · have hnij2 : ¬ (i2.val ≤ j) := by omega
        simp only [hnij2, hij, ↓reduceIte, nextCtx, Std.Array.set_val_eq, List.getElem_set_ne (Ne.symm hji)]

theorem ComputationContext_set_null_spec
    {env : ExecEnv} {vCounter : Nat} {self : verifier.ComputationContext}
    (hvalid : CompCtxValid env vCounter self)
    (null_id : MemoryId) (is_null : Bool) :
    ∃ ctx' : verifier.ComputationContext,
      verifier.ComputationContext.set_null self null_id is_null = ok ctx' ∧
      ctx'.pc = self.pc ∧
      ctx'.registers.val = self.registers.val.map (fun t => pureTypeSetNull t null_id is_null) ∧
      ctx'.array_bounds = self.array_bounds ∧
      ctx'.resources = self.resources ∧
      (∀ slot, slot < 64 →
        stackSlotType ctx'.stack slot =
          pureTypeSetNull (stackSlotType self.stack slot) null_id is_null) ∧
      CompCtxValid env vCounter ctx' := by
  unfold verifier.ComputationContext.set_null
  obtain ⟨aOut, hloop, haOut⟩ := ComputationContext_set_null_loop_spec null_id is_null 10
    self 0#usize (by rw [usize_const_val_0]) (by rw [usize_const_val_0]; omega)
  obtain ⟨sOut, hsOut, hstk_valid, hslot_eq⟩ :=
    Stack_set_null_spec hvalid.2.1 null_id is_null
  let ctx' : verifier.ComputationContext :=
    { pc := self.pc, registers := aOut, stack := sOut,
      array_bounds := self.array_bounds, resources := self.resources,
      path_depth := self.path_depth, jmp_seq := self.jmp_seq }
  have hset : verifier.ComputationContext.set_null self null_id is_null = ok ctx' := by
    unfold verifier.ComputationContext.set_null
    simp only [hloop, bind_ok]
    change (do
      let s1 ← verifier.Stack.set_null self.stack null_id is_null
      ok { pc := self.pc, registers := aOut, stack := s1,
           array_bounds := self.array_bounds, resources := self.resources,
           path_depth := self.path_depth, jmp_seq := self.jmp_seq }) = ok ctx'
    simp only [hsOut, bind_ok]
    rfl
  have hregs_map : aOut.val = self.registers.val.map (fun t => pureTypeSetNull t null_id is_null) := by
    apply List.ext_getElem
    · rw [array10_len, List.length_map, array10_len]
    · intro j hj1 hj2
      have hj10 : j < 10 := by rw [array10_len] at hj1; exact hj1
      rw [haOut j hj10]
      have h0le : (0#usize).val ≤ j := by rw [usize_const_val_0]; exact Nat.zero_le j
      simp only [h0le, ↓reduceIte, List.getElem_map]
  refine ⟨ctx', hset, rfl, hregs_map, rfl, rfl, hslot_eq, ?_, hstk_valid, hvalid.2.2.1, hvalid.2.2.2⟩
  intro t ht
  rw [hregs_map, List.mem_map] at ht
  rcases ht with ⟨t0, ht0, rfl⟩
  exact Type_set_null_valid (hvalid.1 t0 ht0)

theorem StateRel2Deps_set_null
    {env : ExecEnv} {vCounter : Nat} {ctx : verifier.ComputationContext}
    {deps : verifier.DataDependencies} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {st₁ st₂ : MachineState}
    {null_id : MemoryId} {is_null : Bool}
    (hvalid : CompCtxValid env vCounter ctx)
    (hnullEq : isNullId (renameMemId ρ null_id) = is_null)
    (hrel : StateRel2Deps env ctx deps ρ resActive isNullId st₁ st₂) :
    ∃ ctx' : verifier.ComputationContext,
      verifier.ComputationContext.set_null ctx null_id is_null = ok ctx' ∧
      ctx'.pc = ctx.pc ∧
      ctx'.resources = ctx.resources ∧
      CompCtxValid env vCounter ctx' ∧
      StateRel2Deps env ctx' deps ρ resActive isNullId st₁ st₂ := by
  obtain ⟨ctx', hsn, hpc, hregs_map, hbm, hres, hslot_eq, hcvalid⟩ :=
    ComputationContext_set_null_spec hvalid null_id is_null
  rcases hrel with ⟨hpc1, hpc2, hw, harr, hnull, hnodup, hlive, hbnds, hr10_1, hr10_2, hregs, hstk⟩
  refine ⟨ctx', hsn, hpc, hres, hcvalid,
    ⟨hpc ▸ hpc1, hpc ▸ hpc2, hw, harr, hnull, hnodup, hres ▸ hlive, hbm ▸ hbnds,
     hr10_1, hr10_2, ?_, ?_⟩⟩
  · intro r hr hlen hd
    have hlen0 : r < ctx.registers.val.length := by rw [array10_len]; exact hr
    have hget : (ctx'.registers.val[r]'hlen) =
        pureTypeSetNull (ctx.registers.val[r]'hlen0) null_id is_null := by
      simp [hregs_map]
    rw [hget]
    exact Type_set_null_rel2 hnullEq (hregs r hr hlen0 hd)
  · intro slot hslot hd
    rw [hslot_eq slot hslot]
    exact Type_set_null_stack_rel2 hnullEq (hstk slot hslot hd)

theorem StateRel2Deps_unregister_resource_inactive
    {env : ExecEnv} {vCounter : Nat} {ctx : verifier.ComputationContext}
    {deps : verifier.DataDependencies} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {st₁ st₂ : MachineState}
    {inner : verifier.Type}
    (hvalid : CompCtxValid env vCounter ctx)
    (hinact : nullInnerInactive ρ resActive inner)
    (hrel : StateRel2Deps env ctx deps ρ resActive isNullId st₁ st₂) :
    ∃ ctx' : verifier.ComputationContext,
      verifier.Type.unregister_resource inner ctx = ok ctx' ∧
      ctx'.pc = ctx.pc ∧
      ctx'.registers = ctx.registers ∧
      ctx'.stack = ctx.stack ∧
      ctx'.array_bounds = ctx.array_bounds ∧
      CompCtxValid env vCounter ctx' ∧
      StateRel2Deps env ctx' deps ρ resActive isNullId st₁ st₂ := by
  obtain ⟨ctx', hunreg, hpc, hregs, hstk, hbm, hresV, hmem⟩ :=
    Type_unregister_resource_spec hvalid.2.2.2 inner
  rcases hrel with ⟨hpc1, hpc2, hw, harr, hnull, hnodup, hlive, hbnds, hr10_1, hr10_2, hreg_rel, hstk_rel⟩
  refine ⟨ctx', hunreg, hpc, hregs, hstk, hbm,
    ⟨hregs ▸ hvalid.1, hstk ▸ hvalid.2.1, hbm ▸ hvalid.2.2.1, hresV⟩,
    ⟨hpc ▸ hpc1, hpc ▸ hpc2, hw, harr, hnull, hnodup, ?_, hbm ▸ hbnds,
     hr10_1, hr10_2, hregs ▸ hreg_rel, hstk ▸ hstk_rel⟩⟩
  cases inner with
  | Releasable mid _ =>
    dsimp only [nullInnerInactive] at hinact
    intro m
    rw [hlive m]
    constructor
    · rintro ⟨r, hr_in, hact, rfl⟩
      refine ⟨r, (hmem r).mpr ⟨hr_in, ?_⟩, hact, rfl⟩
      intro hrm; subst hrm; rw [hinact] at hact; cases hact
    · rintro ⟨r, hr_in, hact, rfl⟩
      exact ⟨r, ((hmem r).mp hr_in).1, hact, rfl⟩
  | _ =>
    simpa [hmem] using hlive

theorem StateRel2Deps_remove_active_resource
    {vCounter rtCounter : Nat} {ρ : Std.U64 → Std.U64}
    {resActive : MemoryId → Bool}
    {resources resources' : verifier.IdSet}
    {liveResources : List MemoryId}
    {mid : MemoryId}
    (hρ : RhoValid vCounter rtCounter ρ)
    (hresV : IdSetValid vCounter resources)
    (hmidB : memIdBounded vCounter mid = true)
    (hnodup : liveResources.Nodup)
    (hlive : ∀ m : MemoryId,
      m ∈ liveResources ↔
        ∃ r ∈ resources.entries.val,
          resActive (renameMemId ρ r) = true ∧ renameMemId ρ r = m)
    (hrem : ∀ x, x ∈ resources'.entries.val ↔ x ∈ resources.entries.val ∧ x ≠ mid) :
    (liveResources.erase (renameMemId ρ mid)).Nodup ∧
    (∀ m : MemoryId,
      m ∈ liveResources.erase (renameMemId ρ mid) ↔
        ∃ r ∈ resources'.entries.val,
          resActive (renameMemId ρ r) = true ∧ renameMemId ρ r = m) := by
  refine ⟨hnodup.erase _, fun m => ?_⟩
  rw [hnodup.mem_erase_iff, hlive m]
  constructor
  · rintro ⟨hne, r, hr_in, hact, rfl⟩
    refine ⟨r, (hrem r).mpr ⟨hr_in, ?_⟩, hact, rfl⟩
    intro hrm; subst hrm; exact hne rfl
  · rintro ⟨r, hr_in', hact, rfl⟩
    obtain ⟨hr_in, hr_ne⟩ := (hrem r).mp hr_in'
    have hrB : memIdBounded vCounter r = true := hresV.2 r hr_in
    refine ⟨?_, r, hr_in, hact, rfl⟩
    intro heq
    exact hr_ne (renameMemId_inj_on_bounded hρ hrB hmidB heq)

/-! ## 3. Signed PC Addition, Jumps, Memory Bounds, & Helper Metadata -/

theorem checked_add_signed_usize_ok_spec
    {base out : Std.Usize} {delta : Std.Isize}
    (h : verifier.checked_add_signed_usize base delta = ok (some out)) :
    0 ≤ (Int.ofNat base.val) + delta.val ∧
    (out.val : Int) = (Int.ofNat base.val) + delta.val := by
  unfold verifier.checked_add_signed_usize at h
  by_cases hge : delta >= 0#isize
  · simp only [hge, ↓reduceIte, lift, bind_ok] at h
    have hge_val : 0 ≤ delta.val := by
      have : (0#isize).val ≤ delta.val := hge
      simpa using this
    have hd_max : delta.val ≤ UScalar.max .Usize := by
      have hmax_i := delta.hmax
      cases System.Platform.numBits_eq with
      | inl h32 =>
        simp [UScalar.max, IScalarTy.Isize_numBits_eq, UScalarTy.Usize_numBits_eq, h32] at hmax_i ⊢
        omega
      | inr h64 =>
        simp [UScalar.max, IScalarTy.Isize_numBits_eq, UScalarTy.Usize_numBits_eq, h64] at hmax_i ⊢
        omega
    have hcast := IScalar.hcast_inBounds_spec .Usize delta ⟨hge_val, hd_max⟩
    simp only [lift, spec_ok] at hcast
    have hc := ok_inj h
    have hs := Usize.checked_add_bv_spec base (IScalar.hcast .Usize delta)
    rw [hc] at hs
    simp only [Int.ofNat_eq_natCast] at *
    omega
  · simp only [hge, ↓reduceIte, lift, bind_ok] at h
    have hlt_val : delta.val < 0 := by
      have hn : ¬ ((0#isize).val ≤ delta.val) := hge
      simpa using hn
    have hcast_abs : ((IScalar.hcast .Usize (core.num.Isize.wrapping_sub 0#isize delta)).val : Int) = -delta.val := by
      have heq_w : UScalarTy.Usize.numBits = IScalarTy.Isize.numBits := by
        rw [UScalarTy.Usize_numBits_eq, IScalarTy.Isize_numBits_eq]
      dsimp only [IScalar.hcast, UScalar.val, core.num.Isize.wrapping_sub, IScalar.wrapping_sub]
      have h0bv : (0#isize).bv = 0#IScalarTy.Isize.numBits := rfl
      rw [h0bv, BitVec.zero_sub, heq_w, BitVec.signExtend_eq]
      rcases delta with ⟨bv⟩
      change bv.toInt < 0 at hlt_val
      have hneg : (-bv).toNat = (-bv.toInt).toNat := by
        rw [BitVec.toNat_neg, BitVec.toInt_eq_toNat_cond]
        rw [BitVec.toInt_eq_toNat_cond] at hlt_val
        have hlt2 : bv.toNat < 2 ^ IScalarTy.Isize.numBits := bv.isLt
        set N := 2 ^ IScalarTy.Isize.numBits
        split_ifs at hlt_val ⊢
        · omega
        · have hpos : 0 < bv.toNat := by omega
          have hmod : (N - bv.toNat) % N = N - bv.toNat := Nat.mod_eq_of_lt (by omega)
          rw [hmod]
          omega
      rw [hneg]
      change (((-bv.toInt).toNat : Nat) : Int) = -bv.toInt
      omega
    have hc := ok_inj h
    have hs := Usize.checked_sub_bv_spec base (IScalar.hcast .Usize (core.num.Isize.wrapping_sub 0#isize delta))
    rw [hc] at hs
    simp only [Int.ofNat_eq_natCast] at *
    omega

theorem ComputationContext_jump_with_offset_ok_spec
    {self ctx' : verifier.ComputationContext} {offset : Std.I16}
    (hjmp : verifier.ComputationContext.jump_with_offset self offset = ok (.Ok ctx')) :
    0 ≤ (Int.ofNat (self.pc.val + 1)) + offset.val ∧
    (ctx'.pc.val : Int) = (Int.ofNat (self.pc.val + 1)) + offset.val ∧
    ctx' = { self with pc := ctx'.pc, path_depth := ctx'.path_depth, jmp_seq := ctx'.jmp_seq } := by
  unfold verifier.ComputationContext.jump_with_offset at hjmp
  simp only [lift, bind_ok] at hjmp
  have hcast_i16 : (IScalar.cast .Isize offset).val = offset.val := by
    have hmin := offset.hmin
    have hmax := offset.hmax
    have hb : IScalar.min .Isize ≤ offset.val ∧ offset.val ≤ IScalar.max .Isize := by
      cases System.Platform.numBits_eq with
      | inl h32 =>
        simp [IScalar.min, IScalar.max, IScalarTy.Isize_numBits_eq, h32] at hmin hmax ⊢
        omega
      | inr h64 =>
        simp [IScalar.min, IScalar.max, IScalarTy.Isize_numBits_eq, h64] at hmin hmax ⊢
        omega
    have hs := IScalar.cast_inBounds_spec .Isize offset hb
    simp only [lift, spec_ok] at hs
    exact hs
  rcases bind_eq_ok hjmp with ⟨o, hchk, hjmp⟩
  cases o with
  | none => cases ok_inj hjmp
  | some step1 =>
    dsimp only at hjmp
    obtain ⟨hge0, hstep1_val⟩ := checked_add_signed_usize_ok_spec hchk
    rw [hcast_i16] at hge0 hstep1_val
    cases hc1 : Usize.checked_add step1 1#usize with
    | none => simp [hc1] at hjmp
    | some pcOut =>
      simp only [hc1, clone_registers_spec self.registers, Stack_clone_spec self.stack,
        BoundsMap_clone_spec self.array_bounds, IdSet_clone_spec self.resources, bind_ok] at hjmp
      rcases bind_eq_ok hjmp with ⟨i1, _, hjmp⟩
      cases ok_inj hjmp
      have hs1 := Usize.checked_add_bv_spec step1 1#usize
      rw [hc1] at hs1
      rw [usize_one_val] at hs1
      simp only [Int.ofNat_eq_natCast, Nat.cast_add, Nat.cast_one] at hge0 hstep1_val ⊢
      refine ⟨by omega, by omega, trivial⟩

theorem ComputationContext_next_ok_spec
    {self ctx' : verifier.ComputationContext}
    (hnext : verifier.ComputationContext.next self = ok (.Ok ctx')) :
    ctx'.pc.val = self.pc.val + 1 ∧
    ctx' = { self with pc := ctx'.pc, path_depth := ctx'.path_depth, jmp_seq := ctx'.jmp_seq } := by
  unfold verifier.ComputationContext.next at hnext
  obtain ⟨_, hpc_val, heq⟩ := ComputationContext_jump_with_offset_ok_spec hnext
  have h0 : (0#i16).val = 0 := rfl
  rw [h0, Int.add_zero] at hpc_val
  exact ⟨Int.ofNat.inj hpc_val, heq⟩

theorem ComputationContext_checked_jump_with_offset_ok_spec
    {self ctx' : verifier.ComputationContext}
    {vctx : verifier.VerificationContext} {offset : Std.I16}
    (hjmp : verifier.ComputationContext.checked_jump_with_offset self vctx offset = ok (.Ok ctx')) :
    computeTargetPc vctx.code.val.length self.pc.val 1 offset.val = some ctx'.pc.val ∧
    ctx' = { self with pc := ctx'.pc, path_depth := ctx'.path_depth, jmp_seq := ctx'.jmp_seq } := by
  unfold verifier.ComputationContext.checked_jump_with_offset at hjmp
  rcases bind_eq_ok hjmp with ⟨r, hj, hjmp⟩
  cases r with
  | Err _ => cases ok_inj hjmp
  | Ok r1 =>
    dsimp only at hjmp
    by_cases hge : r1.pc >= alloc.vec.Vec.len vctx.code
    · simp [hge] at hjmp
    · simp only [hge, ↓reduceIte] at hjmp
      rcases bind_eq_ok hjmp with ⟨i1, _, hjmp⟩
      cases ok_inj hjmp
      have hlt : r1.pc.val < vctx.code.val.length :=
        Nat.lt_of_not_ge (fun hc => hge hc)
      obtain ⟨hge0, hpc_int, heq⟩ := ComputationContext_jump_with_offset_ok_spec hj
      refine ⟨?_, ?_⟩
      · unfold computeTargetPc
        dsimp only
        simp only [Int.ofNat_eq_natCast, Nat.cast_add, Nat.cast_one] at hge0 hpc_int
        have hnlt0 : ¬ (((self.pc.val : Int) + 1 + offset.val) < 0) := by omega
        have htoNat : ((self.pc.val : Int) + 1 + offset.val).toNat = r1.pc.val := by omega
        simp [hnlt0, htoNat, hlt]
      · cases r1; cases self; dsimp only at heq ⊢
        injection heq with _ h1 h2 h3 h4 _ _
        subst_vars
        rfl

theorem ComputationContext_check_memory_access_ok_spec
    {self : verifier.ComputationContext}
    {dst_offset : U64ScalarValueData} {dst_buffer_size : Std.U64}
    {instruction_offset : Std.I16} {w : api.DataWidth} {bw : Std.Usize}
    {o : BitVec 64}
    (hbw : api.DataWidth.bytes w = ok bw)
    (hconc : ConcretizesScalar64 dst_offset o)
    (hchk : verifier.ComputationContext.check_memory_access
      self dst_offset dst_buffer_size instruction_offset bw = ok (.Ok ())) :
    inBoundsRange (o + sextOffset16 instruction_offset.bv)
      (DataWidth.ofCore w) dst_buffer_size.bv = true := by
  unfold verifier.ComputationContext.check_memory_access at hchk
  rcases bind_eq_ok hchk with ⟨base_range, hadd16, hchk⟩
  simp only [lift, bind_ok] at hchk
  have hconc_sum : ConcretizesRange64 base_range (o + sextOffset16 instruction_offset.bv) := by
    obtain ⟨rOut, hrOut, hcr⟩ := range64_add_i16_ok_and_sound dst_offset.urange instruction_offset
    rw [hadd16] at hrOut
    cases ok_inj hrOut
    simpa [sextOffset16] using hcr o ⟨hconc.2.2.1, hconc.2.2.2⟩
  obtain ⟨bw', hbw', hbw_val⟩ := DataWidth_bytes_spec w
  rw [hbw] at hbw'
  cases ok_inj hbw'
  have hcast_bw : (UScalar.cast .U64 bw).bv = (DataWidth.ofCore w).bvBytes := by
    rw [uscalar_cast_u64_of_usize_bv, hbw_val]
    cases w <;> rfl
  cases hc_add : U64.checked_add base_range.max (UScalar.cast .U64 bw) with
  | none => simp [hc_add] at hchk
  | some endOff =>
    simp only [hc_add] at hchk
    by_cases hgt : endOff > dst_buffer_size
    · simp [hgt] at hchk
    · simp only [hgt, ↓reduceIte] at hchk
      by_cases hmax_eq : base_range.max = core.num.U64.MAX
      · simp [hmax_eq] at hchk
      · have hs := U64.checked_add_bv_spec base_range.max (UScalar.cast .U64 bw)
        rw [hc_add] at hs
        dsimp only at hs
        have hle_buf : endOff.bv ≤ dst_buffer_size.bv := by
          have hnlt : ¬ (dst_buffer_size.val < endOff.val) := hgt
          rw [BitVec.le_def, UScalar.bv_toNat, UScalar.bv_toNat]
          omega
        have hmax_ne : base_range.max.bv ≠ ~~~0#64 := by
          intro hc
          apply hmax_eq
          apply (u64_eq_iff_bv _ _).mpr
          rwa [u64_max_bv]
        have heff_le_max : o + sextOffset16 instruction_offset.bv ≤ base_range.max.bv :=
          hconc_sum.2
        have hbw_toNat : (UScalar.cast .U64 bw).val = (DataWidth.ofCore w).bvBytes.toNat := by
          rw [← UScalar.bv_toNat, hcast_bw]
        have hmax_toNat : base_range.max.val = base_range.max.bv.toNat := rfl
        have hno_ovf_max : base_range.max.bv.toNat + (DataWidth.ofCore w).bvBytes.toNat < 2 ^ 64 := by
          have h1 := hs.1
          have hm64 : U64.max = 2 ^ 64 - 1 := by simp [U64.max, U64.numBits]
          rw [hmax_toNat, hbw_toNat] at h1
          omega
        have hend_bv : endOff.bv = base_range.max.bv + (DataWidth.ofCore w).bvBytes := by
          have h3 : endOff.bv = base_range.max.bv + U64.bv (UScalar.cast .U64 bw) := hs.2.2
          have hc4 : U64.bv (UScalar.cast .U64 bw) = (DataWidth.ofCore w).bvBytes := hcast_bw
          rw [h3, hc4]
        rw [hend_bv] at hle_buf
        unfold inBoundsRange
        dsimp only
        revert heff_le_max hmax_ne hle_buf
        have hw_cases : (DataWidth.ofCore w).bvBytes = 1#64 ∨
            (DataWidth.ofCore w).bvBytes = 2#64 ∨
            (DataWidth.ofCore w).bvBytes = 4#64 ∨
            (DataWidth.ofCore w).bvBytes = 8#64 := by
          cases w <;> simp [DataWidth.ofCore, DataWidth.bvBytes, DataWidth.bytes]
        have hno_uovf : ¬ BitVec.uaddOverflow base_range.max.bv (DataWidth.ofCore w).bvBytes := by
          simp only [BitVec.uaddOverflow, ge_iff_le, decide_eq_true_eq, not_le]
          exact hno_ovf_max
        revert hno_uovf
        rcases hw_cases with hw | hw | hw | hw <;> (rw [hw]; bv_decide)


theorem uscalar_cast_usize_of_u32_val (x : Std.U32) :
    (UScalar.cast .Usize x).val = x.val := by
  have hx : x.val < 2 ^ 32 := x.bv.isLt
  cases System.Platform.numBits_eq with
  | inl h32 =>
    dsimp only [UScalar.cast, UScalar.val]
    rw [UScalarTy.Usize_numBits_eq, h32, BitVec.toNat_setWidth, UScalar.bv_toNat]
    exact Nat.mod_eq_of_lt hx
  | inr h64 =>
    dsimp only [UScalar.cast, UScalar.val]
    rw [UScalarTy.Usize_numBits_eq, h64, BitVec.toNat_setWidth, UScalar.bv_toNat]
    exact Nat.mod_eq_of_lt (Nat.lt_trans hx (by decide))

theorem uscalar_cast_u64_of_usize_of_u32_bv (x : Std.U32) :
    (UScalar.cast .U64 (UScalar.cast .Usize x)).bv = x.bv.zeroExtend 64 := by
  rw [uscalar_cast_u64_of_usize_bv, uscalar_cast_usize_of_u32_val]
  apply BitVec.eq_of_toNat_eq
  rw [BitVec.toNat_ofNat, BitVec.toNat_setWidth, UScalar.bv_toNat]

theorem ComputationContext_check_memory_access_u32_span_ok_spec
    {self : verifier.ComputationContext}
    {dst_offset : scalar_value.U64ScalarValueData}
    {dst_buffer_size : Std.U64} {sz32 : Std.U32} {o : BitVec 64}
    (hconc : ConcretizesScalar64 dst_offset o)
    (hchk : verifier.ComputationContext.check_memory_access
      self dst_offset dst_buffer_size 0#i16 (UScalar.cast .Usize sz32) = ok (.Ok ())) :
    inBoundsSpan o (sz32.bv.zeroExtend 64) dst_buffer_size.bv = true := by
  unfold verifier.ComputationContext.check_memory_access at hchk
  rcases bind_eq_ok hchk with ⟨base_range, hadd16, hchk⟩
  simp only [lift, bind_ok] at hchk
  obtain ⟨rOut, hrOut, hcr0⟩ := range64_add_i16_ok_and_sound dst_offset.urange 0#i16
  have hcr := hcr0 o ⟨hconc.2.2.1, hconc.2.2.2⟩
  rw [hadd16] at hrOut
  cases ok_inj hrOut
  have h0_sext : (0#i16).bv.signExtend 64 = 0#64 := by decide
  have ho_le_max : o ≤ base_range.max.bv := by
    have := hcr.2
    rwa [h0_sext, BitVec.add_zero] at this
  have hcast_sz := uscalar_cast_u64_of_usize_of_u32_bv sz32
  cases hc_add : U64.checked_add base_range.max (UScalar.cast .U64 (UScalar.cast .Usize sz32)) with
  | none => simp [hc_add] at hchk
  | some endOff =>
    simp only [hc_add] at hchk
    by_cases hgt : endOff > dst_buffer_size
    · simp [hgt] at hchk
    · simp only [hgt, ↓reduceIte] at hchk
      by_cases hmax_eq : base_range.max = core.num.U64.MAX
      · simp [hmax_eq] at hchk
      · have hs := U64.checked_add_bv_spec base_range.max
          (UScalar.cast .U64 (UScalar.cast .Usize sz32))
        rw [hc_add] at hs
        dsimp only at hs
        have hle_buf : endOff.bv ≤ dst_buffer_size.bv := by
          have hnlt : ¬ (dst_buffer_size.val < endOff.val) := hgt
          rw [BitVec.le_def, UScalar.bv_toNat, UScalar.bv_toNat]
          omega
        have hmax_ne : base_range.max.bv ≠ ~~~0#64 := by
          intro hc
          apply hmax_eq
          apply (u64_eq_iff_bv _ _).mpr
          rwa [u64_max_bv]
        have hsz_toNat : (UScalar.cast .U64 (UScalar.cast .Usize sz32)).val =
            (sz32.bv.zeroExtend 64).toNat := by
          rw [← UScalar.bv_toNat, hcast_sz]
        have hmax_toNat : base_range.max.val = base_range.max.bv.toNat := rfl
        have hno_ovf : ¬ BitVec.uaddOverflow base_range.max.bv (sz32.bv.zeroExtend 64) := by
          have h1 := hs.1
          have hm64 : U64.max = 2 ^ 64 - 1 := by simp [U64.max, U64.numBits]
          rw [hmax_toNat, hsz_toNat] at h1
          simp only [BitVec.uaddOverflow, ge_iff_le, decide_eq_true_eq, not_le]
          omega
        have hend_bv : endOff.bv = base_range.max.bv + sz32.bv.zeroExtend 64 := by
          have h3 : endOff.bv = base_range.max.bv + U64.bv (UScalar.cast .U64 (UScalar.cast .Usize sz32)) := hs.2.2
          have hc4 : U64.bv (UScalar.cast .U64 (UScalar.cast .Usize sz32)) = sz32.bv.zeroExtend 64 := hcast_sz
          rw [h3, hc4]
        rw [hend_bv] at hle_buf
        have hsz_le : sz32.bv.zeroExtend 64 ≤ 0xFFFF_FFFF#64 := by
          rw [BitVec.le_def, BitVec.toNat_setWidth]
          have hlt := sz32.bv.isLt
          have hlt64 : sz32.bv.toNat < 2 ^ 64 := Nat.lt_trans hlt (by decide)
          rw [Nat.mod_eq_of_lt hlt64]
          simp only [BitVec.toNat_ofNat] at *
          omega
        unfold inBoundsSpan
        revert ho_le_max hmax_ne hle_buf hno_ovf hsz_le
        bv_decide

theorem ComputationContext_get_map_schema_ok_spec
    {env : ExecEnv} {vCounter : Nat} {self : verifier.ComputationContext}
    {deps : verifier.DataDependencies} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {st₁ st₂ : MachineState}
    {argument : Std.U8} {schema : maps.MapSchema}
    (hvalid : CompCtxValid env vCounter self)
    (hrel : StateRel2Deps env self deps ρ resActive isNullId st₁ st₂)
    (hlive : DataDepsHasReg deps (argument.val + 1) = true)
    (hget : verifier.ComputationContext.get_map_schema self argument = ok (.Ok schema)) :
    argument.val < 5 ∧
    lookupHelperMapArg env st₁.regs argument.val = some (MapSpec.ofCore schema) ∧
    lookupHelperMapArg env st₂.regs argument.val = some (MapSpec.ofCore schema) := by
  unfold verifier.ComputationContext.get_map_schema at hget
  simp only [lift, bind_ok] at hget
  cases hc : U8.checked_add argument 1#u8 with
  | none => simp [hc] at hget
  | some reg =>
    simp only [hc] at hget
    have hs := U8.checked_add_bv_spec argument 1#u8
    rw [hc] at hs
    have hreg_val : reg.val = argument.val + 1 := hs.2.1
    by_cases hgt : reg > api.ARGUMENT_COUNT
    · simp [hgt] at hget
    · simp only [hgt, ↓reduceIte] at hget
      have harg_lt5 : argument.val < 5 := by
        have hn : ¬ (api.ARGUMENT_COUNT.val < reg.val) := hgt
        rw [argument_count_val, hreg_val] at hn
        omega
      rcases bind_eq_ok hget with ⟨r, hreg_call, hget⟩
      cases r with
      | Err _ => simp at hget
      | Ok r1 =>
        cases r1 <;> simp at hget
        subst hget
        obtain ⟨_, _, htr⟩ := ComputationContext_reg_ok_valid_and_rel2 hvalid hrel
          (Or.inr (by rwa [hreg_val])) hreg_call
        rw [hreg_val] at htr
        rcases htr with ⟨hmap, hv1, hv2⟩
        rw [UScalar.bv_toNat] at hmap
        refine ⟨harg_lt5, ?_, ?_⟩
        · simp [lookupHelperMapArg, harg_lt5, hv1, hmap]
        · simp [lookupHelperMapArg, harg_lt5, hv2, hmap]

theorem MemoryParameterSize_size_ok_spec
    {env : ExecEnv} {vCounter : Nat} {ctx : verifier.ComputationContext}
    {deps : verifier.DataDependencies} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {st₁ st₂ : MachineState}
    {mps : verifier.MemoryParameterSize} {sizeSpec : HelperMemSize} {sz : Std.U64}
    (hmatch : (∃ v, mps = .Value v ∧ sizeSpec = .fixed v.bv) ∨
              (∃ idx, mps = .Reference idx ∧ sizeSpec = .fromArg idx.val ∧
                DataDepsHasReg deps (idx.val + 1) = true))
    (hvalid : CompCtxValid env vCounter ctx)
    (hrel : StateRel2Deps env ctx deps ρ resActive isNullId st₁ st₂)
    (hsz : verifier.MemoryParameterSize.size mps ctx = ok (.Ok sz)) :
    resolveHelperMemSize st₁.regs sizeSpec = some sz.bv ∧
    resolveHelperMemSize st₂.regs sizeSpec = some sz.bv := by
  rcases hmatch with ⟨v, rfl, rfl⟩ | ⟨idx, rfl, rfl, hlive⟩
  · unfold verifier.MemoryParameterSize.size at hsz
    cases ok_inj hsz
    exact ⟨rfl, rfl⟩
  · unfold verifier.MemoryParameterSize.size at hsz
    simp only [lift, bind_ok] at hsz
    cases hc : U8.checked_add idx 1#u8 with
    | none => simp [hc] at hsz
    | some reg =>
      simp only [hc] at hsz
      have hs := U8.checked_add_bv_spec idx 1#u8
      rw [hc] at hs
      have hreg_val : reg.val = idx.val + 1 := hs.2.1
      by_cases hgt : reg > api.ARGUMENT_COUNT
      · simp [hgt] at hsz
      · simp only [hgt, ↓reduceIte] at hsz
        have hidx_lt5 : idx.val < 5 := by
          have hn : ¬ (api.ARGUMENT_COUNT.val < reg.val) := hgt
          rw [argument_count_val, hreg_val] at hn
          omega
        rcases bind_eq_ok hsz with ⟨r, hreg_call, hsz⟩
        cases r with
        | Err _ => simp at hsz
        | Ok t =>
          cases t <;> try (simp at hsz)
          rename_i data
          rcases bind_eq_ok hsz with ⟨b, hb_known, hsz⟩
          cases b with
          | false => simp at hsz
          | true =>
            simp only [↓reduceIte] at hsz
            cases ok_inj hsz
            have hknown_mask : data.unknown_mask = 0#u64 := by
              unfold scalar_value.U64ScalarValueData.is_known at hb_known
              simpa using ok_inj hb_known
            have hne_uninit : data ≠ uninitScalar64 := by
              intro hc_eq
              subst hc_eq
              have : (core.num.U64.MAX : Std.U64).bv = (0#u64).bv :=
                congrArg UScalar.bv hknown_mask
              rw [u64_max_bv] at this
              exact absurd this (by decide)
            obtain ⟨_, _, htr⟩ := ComputationContext_reg_ok_valid_and_rel2 hvalid hrel
              (Or.inr (by rwa [hreg_val])) hreg_call
            rw [hreg_val] at htr
            simp only [TypeRel2, hne_uninit, ↓reduceIte] at htr
            rcases htr with ⟨x₁, x₂, hv1, hv2, hc1, hc2, _⟩
            have hx1 := eq_value_of_known_64 hknown_mask hc1
            have hx2 := eq_value_of_known_64 hknown_mask hc2
            subst hx1; subst hx2
            exact ⟨by simp [resolveHelperMemSize, hidx_lt5, hv1],
                   by simp [resolveHelperMemSize, hidx_lt5, hv2]⟩

/-! ## 5. `VerificationContext`, `clear_dead_registers`, `Stack.retain`, & Entry State -/

theorem VerificationContext_new_ok_spec
    {cctx : verifier.CoreCallingContext} {log_enabled : Bool}
    {code : alloc.vec.Vec api.EbpfInstruction}
    {tcs : alloc.vec.Vec (Option verifier.TerminatingContexts)}
    {vctx : verifier.VerificationContext}
    (h : verifier.VerificationContext.new cctx log_enabled code tcs = ok vctx) :
    vctx.calling_context = cctx ∧
    vctx.code = code ∧
    vctx.counter = 0#u64 ∧
    vctx.iteration = 0#usize ∧
    vctx.terminating_contexts = tcs ∧
    vctx.pending_successors = alloc.vec.Vec.new (Option verifier.ComputationContext) ∧
    vctx.pending_log = none := by
  unfold verifier.VerificationContext.new at h
  rcases bind_eq_ok h with ⟨mai, _, h⟩
  cases ok_inj h
  exact ⟨rfl, rfl, rfl, rfl, rfl, rfl, rfl⟩

theorem VerificationContext_emit_log_ok_spec
    {self vctx' : verifier.VerificationContext} {pc : Std.Usize}
    {ev : verifier.VerifierLogEvent}
    (h : verifier.VerificationContext.emit_log self pc ev = ok vctx') :
    vctx'.calling_context = self.calling_context ∧
    vctx'.code = self.code ∧
    vctx'.counter = self.counter ∧
    vctx'.iteration = self.iteration ∧
    vctx'.terminating_contexts = self.terminating_contexts ∧
    vctx'.pending_successors = self.pending_successors := by
  unfold verifier.VerificationContext.emit_log at h
  rcases bind_eq_ok h with ⟨_, _, h⟩
  by_cases hlog : self.log_enabled
  · simp only [hlog, ↓reduceIte] at h
    rcases bind_eq_ok h with ⟨ei, _, h⟩
    rcases bind_eq_ok h with ⟨opcode, _, h⟩
    cases ok_inj h
    exact ⟨rfl, rfl, rfl, rfl, rfl, rfl⟩
  · simp only [hlog, Bool.false_eq_true, ↓reduceIte] at h
    cases ok_inj h
    exact ⟨rfl, rfl, rfl, rfl, rfl, rfl⟩

theorem VerificationContext_push_successor_ok_spec
    {self vctx' : verifier.VerificationContext} {ctx : verifier.ComputationContext}
    (h : verifier.VerificationContext.push_successor self ctx = ok vctx') :
    vctx'.calling_context = self.calling_context ∧
    vctx'.code = self.code ∧
    vctx'.counter = self.counter ∧
    vctx'.iteration = self.iteration ∧
    vctx'.terminating_contexts = self.terminating_contexts ∧
    vctx'.pending_log = self.pending_log ∧
    vctx'.pending_successors.val = self.pending_successors.val ++ [some ctx] := by
  unfold verifier.VerificationContext.push_successor at h
  rcases bind_eq_ok h with ⟨v, hpush, h⟩
  cases ok_inj h
  exact ⟨rfl, rfl, rfl, rfl, rfl, rfl, vec_push_ok_val hpush⟩

theorem VerificationContext_next_id_ok_spec
    {self vctx' : verifier.VerificationContext} {mid : MemoryId}
    (h : verifier.VerificationContext.next_id self = ok (mid, vctx')) :
    mid = verifier.MemoryId.mk .Verification self.counter none ∧
    vctx' = { self with counter := vctx'.counter } ∧
    vctx'.counter.bv.toNat = self.counter.bv.toNat + 1 := by
  unfold verifier.VerificationContext.next_id at h
  rcases bind_eq_ok h with ⟨i, hadd, h⟩
  have hval := (uscalar_add_ok_val hadd).1
  cases ok_inj h
  refine ⟨rfl, rfl, ?_⟩
  simpa [UScalar.bv_toNat] using hval

theorem VerificationContext_register_memory_access_ok_spec
    {self vctx' : verifier.VerificationContext} {pc : Std.Usize}
    {sa : Option verifier.StructAccess}
    (h : verifier.VerificationContext.register_memory_access self pc sa = ok (.Ok (), vctx')) :
    vctx'.calling_context = self.calling_context ∧
    vctx'.code = self.code ∧
    vctx'.counter = self.counter ∧
    vctx'.iteration = self.iteration ∧
    vctx'.terminating_contexts = self.terminating_contexts ∧
    vctx'.pending_successors = self.pending_successors ∧
    vctx'.pending_log = self.pending_log := by
  unfold verifier.VerificationContext.register_memory_access at h
  rcases bind_eq_ok h with ⟨ma, _, h⟩
  cases ma with
  | Unaccessed =>
    cases sa with
    | none =>
      rcases bind_eq_ok h with ⟨⟨_, back⟩, _, h⟩
      cases ok_inj h
      exact ⟨rfl, rfl, rfl, rfl, rfl, rfl, rfl⟩
    | some access =>
      rcases bind_eq_ok h with ⟨⟨_, back⟩, _, h⟩
      cases ok_inj h
      exact ⟨rfl, rfl, rfl, rfl, rfl, rfl, rfl⟩
  | NonStruct =>
    cases sa with
    | none =>
      cases ok_inj h
      exact ⟨rfl, rfl, rfl, rfl, rfl, rfl, rfl⟩
    | some _ => cases ok_inj h
  | Struct existing =>
    cases sa with
    | none => cases ok_inj h
    | some access =>
      dsimp only at h
      rcases bind_eq_ok h with ⟨b, _, h⟩
      cases b with
      | true => cases ok_inj h
      | false =>
        simp only [Bool.false_eq_true, ↓reduceIte] at h
        cases ok_inj h
        exact ⟨rfl, rfl, rfl, rfl, rfl, rfl, rfl⟩

theorem clear_dead_registers_ok_spec
    {deps : verifier.DataDependencies} {ctx ctx' : verifier.ComputationContext}
    (h : verifier.clear_dead_registers ctx deps = ok ctx') :
    ctx'.pc = ctx.pc ∧
    ctx'.stack = ctx.stack ∧
    ctx'.array_bounds = ctx.array_bounds ∧
    ctx'.resources = ctx.resources ∧
    (∀ r (_hr : r < 10) (hlen' : r < ctx'.registers.val.length) (hlen : r < ctx.registers.val.length),
      ctx'.registers.val[r]'hlen' =
        if DataDepsHasReg deps r then ctx.registers.val[r]'hlen else .ScalarValue uninitScalar64) := by
  unfold verifier.clear_dead_registers verifier.clear_dead_registers_loop at h
  let Pre : verifier.ComputationContext × Std.Usize → Prop :=
    fun s => s.2.val ≤ 10
  let Rel : verifier.ComputationContext × Std.Usize → verifier.ComputationContext → Prop :=
    fun s out =>
      out.pc = s.1.pc ∧
      out.stack = s.1.stack ∧
      out.array_bounds = s.1.array_bounds ∧
      out.resources = s.1.resources ∧
      (∀ r (_hr : r < 10) (ho : r < out.registers.val.length) (hi : r < s.1.registers.val.length),
        out.registers.val[r]'ho =
          if s.2.val ≤ r then
            (if DataDepsHasReg deps r then s.1.registers.val[r]'hi else .ScalarValue uninitScalar64)
          else s.1.registers.val[r]'hi)
  have hmain := loop_ok_pre_post
    (fun (c1, r1) => verifier.clear_dead_registers_loop.body deps c1 r1)
    Pre Rel
    (by
      intro ⟨cur, reg⟩ ⟨cur1, reg1⟩ out hpre hbody
      unfold verifier.clear_dead_registers_loop.body at hbody
      simp only [lift, bind_ok] at hbody
      have hcast10 : (UScalar.cast .Usize api.GENERAL_REGISTER_COUNT).val = 10 := by
        rw [uscalar_cast_usize_of_u8_val, general_register_count_val]
      by_cases hlt : reg < UScalar.cast .Usize api.GENERAL_REGISTER_COUNT
      · simp only [hlt, ↓reduceIte] at hbody
        have hreg_lt10 : reg.val < 10 := by
          have : reg.val < (UScalar.cast .Usize api.GENERAL_REGISTER_COUNT).val := hlt
          omega
        rcases bind_eq_ok hbody with ⟨b, hb, hbody⟩
        rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
        rcases bind_eq_ok hbody with ⟨curM, hcurM, hbody⟩
        rcases bind_eq_ok hbody with ⟨regN, hadd, hbody⟩
        cases ok_inj hbody
        have hreg1_val : reg1.val = reg.val + 1 := by
          have := (uscalar_add_ok_val hadd).1
          have h1 := usize_one_val
          omega
        have hu8 : (UScalar.cast .U8 reg).val = reg.val :=
          uscalar_cast_u8_of_usize_val (by omega)
        have hb_eq : b = DataDepsHasReg deps reg.val := by
          have hspec := (DataDependencies_has_reg_ok_spec hb).2
          rwa [hu8] at hspec
        refine ⟨by dsimp [Pre]; omega, fun hrel_next => ?_⟩
        rcases hrel_next with ⟨hpc, hstk, hbnds, hres, hregs⟩
        dsimp only at hpc hstk hbnds hres hregs
        cases b with
        | true =>
          simp only [↓reduceIte] at hcurM
          cases ok_inj hcurM
          refine ⟨hpc, hstk, hbnds, hres, fun r hr ho hi => ?_⟩
          rw [hregs r hr ho hi]
          by_cases hrr : r = reg.val
          · subst hrr
            have hd : DataDepsHasReg deps reg.val = true := hb_eq.symm
            have hnle1 : ¬ (reg1.val ≤ reg.val) := by omega
            simp [hnle1, hd]
          · by_cases hle : reg.val ≤ r
            · have hle1 : reg1.val ≤ r := by omega
              simp [hle, hle1]
            · have hnle1 : ¬ (reg1.val ≤ r) := by omega
              simp [hle, hnle1]
        | false =>
          simp only [Bool.false_eq_true, ↓reduceIte] at hcurM
          rcases bind_eq_ok hcurM with ⟨⟨t0, back0⟩, hmut0, hcurM⟩
          rcases bind_eq_ok hcurM with ⟨tU, htU, hcurM⟩
          rcases bind_eq_ok hcurM with ⟨a1, hupd0, hcurM⟩
          cases ok_inj hcurM
          simp only [array_index_mut_usize_10_ok cur.registers reg hreg_lt10] at hmut0
          cases ok_inj hmut0
          rw [Type_UNINITIALIZED_eq] at htU
          cases ok_inj htU
          have hself_set : Std.Array.set cur.registers reg
              (cur.registers.val[reg.val]'(by rw [array10_len]; exact hreg_lt10)) = cur.registers := by
            apply Std.Array.ext; simp [Std.Array.set_val_eq]
          simp only [hself_set, array_update_10_ok cur.registers reg (.ScalarValue uninitScalar64) hreg_lt10] at hupd0
          cases ok_inj hupd0
          refine ⟨hpc, hstk, hbnds, hres, fun r hr ho hi => ?_⟩
          have hm : r < (Std.Array.set cur.registers reg (.ScalarValue uninitScalar64)).val.length := by
            rw [array10_len]; exact hr
          rw [hregs r hr ho hm]
          by_cases hrr : r = reg.val
          · subst hrr
            have hd : DataDepsHasReg deps reg.val = false := hb_eq.symm
            have hnle1 : ¬ (reg1.val ≤ reg.val) := by omega
            simp [hnle1, hd, Std.Array.set_val_eq]
          · have hget_set : (Std.Array.set cur.registers reg (.ScalarValue uninitScalar64)).val[r]'hm =
                cur.registers.val[r]'hi := by
              simp [Std.Array.set_val_eq, Ne.symm hrr]
            rw [hget_set]
            by_cases hle : reg.val ≤ r
            · have hle1 : reg1.val ≤ r := by omega
              simp [hle, hle1]
            · have hnle1 : ¬ (reg1.val ≤ r) := by omega
              simp [hle, hnle1]
      · simp only [hlt, ↓reduceIte] at hbody
        cases ok_inj hbody)
    (by
      intro ⟨cur, reg⟩ out hpre hbody
      unfold verifier.clear_dead_registers_loop.body at hbody
      simp only [lift, bind_ok] at hbody
      have hcast10 : (UScalar.cast .Usize api.GENERAL_REGISTER_COUNT).val = 10 := by
        rw [uscalar_cast_usize_of_u8_val, general_register_count_val]
      by_cases hlt : reg < UScalar.cast .Usize api.GENERAL_REGISTER_COUNT
      · simp only [hlt, ↓reduceIte] at hbody
        rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
        rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
        rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
        rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
        cases ok_inj hbody
      · simp only [hlt, ↓reduceIte] at hbody
        cases ok_inj hbody
        have hreg10 : reg.val = 10 := by
          have hn : ¬ (reg.val < (UScalar.cast .Usize api.GENERAL_REGISTER_COUNT).val) := hlt
          dsimp [Pre] at hpre
          omega
        refine ⟨rfl, rfl, rfl, rfl, fun r hr ho hi => ?_⟩
        have hnle : ¬ (reg.val ≤ r) := by omega
        simp [hnle])
    (ctx, 0#usize) ctx' (by dsimp [Pre]; rw [usize_const_val_0]; omega) h
  rcases hmain with ⟨hpc, hstk, hbnds, hres, hregs⟩
  refine ⟨hpc, hstk, hbnds, hres, fun r hr ho hi => ?_⟩
  have h0 : (0#usize).val ≤ r := by rw [usize_const_val_0]; exact Nat.zero_le r
  simpa [h0] using hregs r hr ho hi

theorem clear_dead_registers_valid_and_rel2
    {env : ExecEnv} {vCounter : Nat} {deps : verifier.DataDependencies}
    {ctx ctx' : verifier.ComputationContext}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool} {st₁ st₂ : MachineState}
    (hclr : verifier.clear_dead_registers ctx deps = ok ctx')
    (hvalid : CompCtxValid env vCounter ctx) :
    CompCtxValid env vCounter ctx' ∧
    (StateRel2Deps env ctx deps ρ resActive isNullId st₁ st₂ ↔
     StateRel2Deps env ctx' deps ρ resActive isNullId st₁ st₂) := by
  rcases clear_dead_registers_ok_spec hclr with ⟨hpc, hstk, hbnds, hres, hregs⟩
  refine ⟨⟨?_, hstk ▸ hvalid.2.1, hbnds ▸ hvalid.2.2.1, hres ▸ hvalid.2.2.2⟩, ?_⟩
  · intro t ht
    rcases List.mem_iff_getElem.mp ht with ⟨r, hlen', rfl⟩
    have hr : r < 10 := by rw [array10_len] at hlen'; exact hlen'
    have hlen : r < ctx.registers.val.length := by rw [array10_len]; exact hr
    rw [hregs r hr hlen' hlen]
    split_ifs
    · exact hvalid.1 _ (List.getElem_mem hlen)
    · exact uninitScalar64_valid
  · constructor
    · intro ⟨h1, h2, h3, h4, h5, h6, h7, h8, h9, h10, hrg, hsk⟩
      refine ⟨hpc ▸ h1, hpc ▸ h2, h3, h4, h5, h6, hres ▸ h7, hbnds ▸ h8, h9, h10, ?_, hstk ▸ hsk⟩
      intro r hr hlen' hd
      have hlen : r < ctx.registers.val.length := by rw [array10_len]; exact hr
      rw [hregs r hr hlen' hlen]
      simp only [hd, ↓reduceIte]
      exact hrg r hr hlen hd
    · intro ⟨h1, h2, h3, h4, h5, h6, h7, h8, h9, h10, hrg, hsk⟩
      refine ⟨hpc.symm ▸ h1, hpc.symm ▸ h2, h3, h4, h5, h6, hres.symm ▸ h7, hbnds.symm ▸ h8, h9, h10, ?_, hstk.symm ▸ hsk⟩
      intro r hr hlen hd
      have hlen' : r < ctx'.registers.val.length := by rw [array10_len]; exact hr
      have htr := hrg r hr hlen' hd
      rw [hregs r hr hlen' hlen] at htr
      simpa only [hd, ↓reduceIte] using htr

theorem StateRel2Deps_stack_retain
    {env : ExecEnv} {vCounter : Nat} {deps : verifier.DataDependencies}
    {ctx : verifier.ComputationContext} {s' : verifier.Stack}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool} {st₁ st₂ : MachineState}
    (hvalid : CompCtxValid env vCounter ctx)
    (hret : verifier.Stack.retain ctx.stack deps.stack = ok s') :
    CompCtxValid env vCounter { ctx with stack := s' } ∧
    (StateRel2Deps env ctx deps ρ resActive isNullId st₁ st₂ ↔
     StateRel2Deps env { ctx with stack := s' } deps ρ resActive isNullId st₁ st₂) := by
  obtain ⟨sOut, hsOut, hsv, hslots⟩ := Stack_retain_spec hvalid.2.1 deps.stack
  rw [hret] at hsOut
  cases ok_inj hsOut
  refine ⟨⟨hvalid.1, hsv, hvalid.2.2.1, hvalid.2.2.2⟩, ?_⟩
  constructor
  · intro ⟨h1, h2, h3, h4, h5, h6, h7, h8, h9, h10, hrg, hsk⟩
    refine ⟨h1, h2, h3, h4, h5, h6, h7, h8, h9, h10, hrg, ?_⟩
    intro slot hslot hd
    rw [hslots slot hslot]
    have hbit : deps.stack.bv.getLsbD slot = true := hd
    simp only [hbit, ↓reduceIte]
    exact hsk slot hslot hd
  · intro ⟨h1, h2, h3, h4, h5, h6, h7, h8, h9, h10, hrg, hsk⟩
    refine ⟨h1, h2, h3, h4, h5, h6, h7, h8, h9, h10, hrg, ?_⟩
    intro slot hslot hd
    have htr := hsk slot hslot hd
    have hbit : deps.stack.bv.getLsbD slot = true := hd
    rw [hslots slot hslot] at htr
    simpa only [hbit, ↓reduceIte] using htr

theorem verify_program_core_loop3_ok_spec
    {args : alloc.vec.Vec verifier.Type} {ctx0 ctxInit : verifier.ComputationContext}
    (hlen : args.val.length ≤ 5)
    (hloop : verifier.verify_program_core_loop3 args ctx0 0#usize = ok ctxInit) :
    ctxInit.pc = ctx0.pc ∧
    ctxInit.stack = ctx0.stack ∧
    ctxInit.array_bounds = ctx0.array_bounds ∧
    ctxInit.resources = ctx0.resources ∧
    (∀ r (_hr : r < 10) (ho : r < ctxInit.registers.val.length) (hi : r < ctx0.registers.val.length),
      ctxInit.registers.val[r]'ho =
        if harg : 1 ≤ r ∧ r - 1 < args.val.length then
          args.val[r - 1]'harg.2
        else
          ctx0.registers.val[r]'hi) := by
  unfold verifier.verify_program_core_loop3 at hloop
  let Pre : verifier.ComputationContext × Std.Usize → Prop :=
    fun s => s.2.val ≤ args.val.length
  let Rel : verifier.ComputationContext × Std.Usize → verifier.ComputationContext → Prop :=
    fun s out =>
      out.pc = s.1.pc ∧
      out.stack = s.1.stack ∧
      out.array_bounds = s.1.array_bounds ∧
      out.resources = s.1.resources ∧
      (∀ r (_hr : r < 10) (ho : r < out.registers.val.length) (hi : r < s.1.registers.val.length),
        out.registers.val[r]'ho =
          if harg : s.2.val + 1 ≤ r ∧ r - 1 < args.val.length then
            args.val[r - 1]'harg.2
          else
            s.1.registers.val[r]'hi)
  have hmain := loop_ok_pre_post
    (fun (c1, i1) => verifier.verify_program_core_loop3.body args c1 i1)
    Pre Rel
    (by
      intro ⟨cur, idx⟩ ⟨cur1, idx1⟩ out hpre hbody
      unfold verifier.verify_program_core_loop3.body at hbody
      by_cases hlt : idx < alloc.vec.Vec.len args
      · simp only [hlt, ↓reduceIte] at hbody
        have hidx_lt : idx.val < args.val.length := hlt
        rcases bind_eq_ok hbody with ⟨t, hvec, hbody⟩
        rcases bind_eq_ok hbody with ⟨t1, hcl, hbody⟩
        rcases bind_eq_ok hbody with ⟨i1, hadd, hbody⟩
        rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
        rcases bind_eq_ok hbody with ⟨⟨t2, back⟩, hmut, hbody⟩
        rcases bind_eq_ok hbody with ⟨a1, hupd, hbody⟩
        cases ok_inj hbody
        have ht_eq : t = args.val[idx.val]'hidx_lt := by
          have := (vec_index_usize_ok hvec).2
          rw [List.getElem?_eq_getElem hidx_lt] at this
          exact Option.some.inj this.symm
        have ht1_eq : t1 = t := (ok_inj ((Type_clone_spec t).symm.trans hcl)).symm
        have hidx1_val : idx1.val = idx.val + 1 := by
          have := (uscalar_add_ok_val hadd).1
          have h1 := usize_one_val
          omega
        have hi1_lt10 : idx1.val < 10 := by omega
        simp only [array_index_mut_usize_10_ok cur.registers idx1 hi1_lt10] at hmut
        cases ok_inj hmut
        have hself_set : Std.Array.set cur.registers idx1
            (cur.registers.val[idx1.val]'(by rw [array10_len]; exact hi1_lt10)) = cur.registers := by
          apply Std.Array.ext; simp [Std.Array.set_val_eq]
        simp only [hself_set, array_update_10_ok cur.registers idx1 t1 hi1_lt10] at hupd
        cases ok_inj hupd
        refine ⟨by dsimp [Pre]; omega, fun ⟨hpc, hstk, hbnds, hres, hregs⟩ => ?_⟩
        dsimp only at hpc hstk hbnds hres hregs
        refine ⟨hpc, hstk, hbnds, hres, fun r hr ho hi => ?_⟩
        have hm : r < (Std.Array.set cur.registers idx1 t1).val.length := by
          rw [array10_len]; exact hr
        have hstep_reg := hregs r hr ho hm
        dsimp only at hstep_reg ⊢
        rw [hstep_reg]
        by_cases hri : r = idx1.val
        · subst hri
          split_ifs with h1 h2 h3
          · omega
          · omega
          · simp only [Std.Array.set_val_eq, List.getElem_set_self, ht1_eq, ht_eq]
            have hsub : idx1.val - 1 = idx.val := by omega
            simp only [hsub]
          · exfalso; exact h3 ⟨by omega, by omega⟩
        · have hget_set : (Std.Array.set cur.registers idx1 t1).val[r]'hm = cur.registers.val[r]'hi := by
            simp [Std.Array.set_val_eq, Ne.symm hri]
          rw [hget_set]
          split_ifs with h1 h2 h3
          · rfl
          · exfalso; exact h2 ⟨by omega, h1.2⟩
          · exfalso; omega
          · rfl
      · simp only [hlt, ↓reduceIte] at hbody
        cases ok_inj hbody)
    (by
      intro ⟨cur, idx⟩ out hpre hbody
      unfold verifier.verify_program_core_loop3.body at hbody
      by_cases hlt : idx < alloc.vec.Vec.len args
      · simp only [hlt, ↓reduceIte] at hbody
        rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
        rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
        rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
        rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
        rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
        rcases bind_eq_ok hbody with ⟨_, _, hbody⟩
        cases ok_inj hbody
      · simp only [hlt, ↓reduceIte] at hbody
        cases ok_inj hbody
        have hidx_eq : idx.val = args.val.length := by
          have hn : ¬ (idx.val < args.val.length) := hlt
          dsimp [Pre] at hpre
          omega
        refine ⟨rfl, rfl, rfl, rfl, fun r hr ho hi => ?_⟩
        have hn : ¬ (idx.val + 1 ≤ r ∧ r - 1 < args.val.length) := by omega
        simp only [hn, ↓reduceDIte])
    (ctx0, 0#usize) ctxInit (by dsimp [Pre]; rw [usize_const_val_0]; exact Nat.zero_le _) hloop
  rcases hmain with ⟨hpc, hstk, hbnds, hres, hregs⟩
  refine ⟨hpc, hstk, hbnds, hres, fun r hr ho hi => ?_⟩
  have h0 : (0#usize).val + 1 = 1 := by rw [usize_const_val_0]
  simpa [h0] using hregs r hr ho hi

private theorem scalar_zero_eq_known0 {d : scalar_value.U64ScalarValueData}
    (hv : d.value.bv = 0#64) (hu : d.unknown_mask.bv = 0#64) (hw : d.unwritten_mask.bv = 0#64)
    (hmin : d.urange.min.bv = 0#64) (hmax : d.urange.max.bv = 0#64) :
    d = knownScalar64 0#u64 := by
  cases d with
  | mk val unk unwr ur =>
    cases ur with
    | mk umin umax =>
      dsimp only at hv hu hw hmin hmax
      have e1 : val = 0#u64 := (u64_eq_iff_bv _ _).mpr hv
      have e2 : unk = 0#u64 := (u64_eq_iff_bv _ _).mpr hu
      have e3 : unwr = 0#u64 := (u64_eq_iff_bv _ _).mpr hw
      have e4 : umin = 0#u64 := (u64_eq_iff_bv _ _).mpr hmin
      have e5 : umax = 0#u64 := (u64_eq_iff_bv _ _).mpr hmax
      subst e1; subst e2; subst e3; subst e4; subst e5
      rfl

theorem EntryArgConforms_valid_and_rel2
    {env : ExecEnv} {argType : verifier.Type} {ctxObj? : Option ContextObjSpec}
    {v₁ v₂ : RuntimeVal} {arrayLen : MemoryId → BitVec 64} {nullPred : MemoryId → Bool}
    (harr : ∀ id, arrayLen id ≤ 0xFFFF_FFFF#64)
    (harg : EntryArgConforms env argType ctxObj? v₁ v₂) :
    TypeValid env 0 argType ∧
    TypeRel2 env (fun x => x) (fun _ => false) nullPred arrayLen argType v₁ v₂ := by
  cases argType with
  | ScalarValue d =>
    rcases harg with ⟨_, hif⟩
    by_cases hw0 : (d.unwritten_mask.bv == 0#64) = true
    · simp only [hw0, ↓reduceIte] at hif
      rcases hif with ⟨hv, hunk, hmin, hmax, x, rfl, rfl⟩
      have hd_eq : d = unknownWrittenScalar64 := by
        have hw : d.unwritten_mask.bv = 0#64 := beq_iff_eq.mp hw0
        cases d with
        | mk val unk unwr ur =>
          cases ur with
          | mk umin umax =>
            dsimp only at hv hunk hw hmin hmax
            have e1 : val = 0#u64 := (u64_eq_iff_bv _ _).mpr hv
            have e2 : unk = core.num.U64.MAX := (u64_eq_iff_bv _ _).mpr (by rwa [u64_max_bv])
            have e3 : unwr = 0#u64 := (u64_eq_iff_bv _ _).mpr hw
            have e4 : umin = 0#u64 := (u64_eq_iff_bv _ _).mpr hmin
            have e5 : umax = core.num.U64.MAX := (u64_eq_iff_bv _ _).mpr (by rwa [u64_max_bv])
            subst e1; subst e2; subst e3; subst e4; subst e5
            rfl
      subst hd_eq
      refine ⟨unknownWrittenScalar64_valid, ?_⟩
      have hne : unknownWrittenScalar64 ≠ uninitScalar64 := by
        intro hc
        have : (0#u64 : Std.U64).bv = core.num.U64.MAX.bv :=
          congrArg (fun s => s.unwritten_mask.bv) hc
        rw [u64_max_bv] at this
        exact absurd this (by decide)
      simp only [TypeRel2, hne, ↓reduceIte]
      exact ⟨x, x, rfl, rfl, concretizesScalar64Rel2_unknownWritten x⟩
    · simp only [hw0] at hif
      rcases hif with ⟨hv, hunk, hw, hmin, hmax, _⟩
      have hd_eq : d = uninitScalar64 := by
        cases d with
        | mk val unk unwr ur =>
          cases ur with
          | mk umin umax =>
            dsimp only at hv hunk hw hmin hmax
            have e1 : val = 0#u64 := (u64_eq_iff_bv _ _).mpr hv
            have e2 : unk = core.num.U64.MAX := (u64_eq_iff_bv _ _).mpr (by rwa [u64_max_bv])
            have e3 : unwr = core.num.U64.MAX := (u64_eq_iff_bv _ _).mpr (by rwa [u64_max_bv])
            have e4 : umin = 0#u64 := (u64_eq_iff_bv _ _).mpr hmin
            have e5 : umax = core.num.U64.MAX := (u64_eq_iff_bv _ _).mpr (by rwa [u64_max_bv])
            subst e1; subst e2; subst e3; subst e4; subst e5
            rfl
      subst hd_eq
      exact ⟨uninitScalar64_valid, by simp [TypeRel2]⟩
  | PtrToStruct sid off sd =>
    rcases harg with ⟨hsd, hv, hu, hw, hmin, hmax, _, rfl, rfl⟩
    have hoff := scalar_zero_eq_known0 hv hu hw hmin hmax
    subst hoff
    have hpov : PtrOffsetValid 0xFFFF_FFFF#64 (knownScalar64 0#u64) :=
      ⟨knownScalar64_valid 0#u64, rfl, by decide⟩
    have hsd_wit : ∃ schemaId, StructDescConforms env schemaId sd ∧ memoryIdMatches schemaId sid = true :=
      ⟨sid, hsd, memoryIdMatches_refl sid⟩
    refine ⟨⟨memIdBounded_of_noVerif 0 hsd.1, hpov, hsd_wit⟩, ?_⟩
    refine ⟨hpov, hsd_wit, 0#64, concretizesScalar64Rel2_known 0#u64, ?_, ?_⟩ <;> simp [renameMemId_id]
  | PtrToMemory mid off sz orig =>
    rcases harg with ⟨hnv, hv, hu, hw, hmin, hmax, _, rfl, rfl⟩
    have hoff := scalar_zero_eq_known0 hv hu hw hmin hmax
    subst hoff
    have hpov : PtrOffsetValid 0xFFFF_FFFF#64 (knownScalar64 0#u64) :=
      ⟨knownScalar64_valid 0#u64, rfl, by decide⟩
    refine ⟨⟨memIdBounded_of_noVerif 0 hnv, hpov⟩, ?_⟩
    refine ⟨hpov, 0#64, concretizesScalar64Rel2_known 0#u64, ?_, ?_⟩ <;> simp [renameMemId_id]
  | PtrToArray aid off =>
    rcases harg with ⟨hnv, hv, hu, hw, hmin, hmax, _, rfl, rfl⟩
    have hoff := scalar_zero_eq_known0 hv hu hw hmin hmax
    subst hoff
    have hpov : PtrOffsetValid MAX_PACKET_OFFSET (knownScalar64 0#u64) :=
      ⟨knownScalar64_valid 0#u64, rfl, by decide⟩
    refine ⟨⟨memIdBounded_of_noVerif 0 hnv, hpov⟩, ?_⟩
    refine ⟨hpov, harr _, 0#64, concretizesScalar64Rel2_known 0#u64, ?_, ?_⟩ <;> simp [renameMemId_id]
  | PtrToEndArray aid =>
    rcases harg with ⟨hnv, _, rfl, rfl⟩
    exact ⟨memIdBounded_of_noVerif 0 hnv, harr _, by simp [renameMemId_id], by simp [renameMemId_id]⟩
  | ConstPtrToMap mapIdx schema =>
    rcases harg with ⟨hmap, _, rfl, rfl⟩
    exact ⟨hmap, hmap, rfl, rfl⟩
  | _ => exact False.elim harg

theorem entry_context_valid_and_rel2
    {cctx : verifier.CoreCallingContext} {env : ExecEnv}
    {mkInit : HostLayout → SecretInit → MachineState}
    {ctx0 ctxInit : verifier.ComputationContext}
    (hentry : EntryStatesConform cctx env mkInit)
    (hlen : cctx.args.val.length ≤ 5)
    (hdef : verifier.ComputationContext.Insts.CoreDefaultDefault.default = ok ctx0)
    (hloop3 : verifier.verify_program_core_loop3 cctx.args ctx0 0#usize = ok ctxInit) :
    CompCtxValid env 0 ctxInit ∧
    (∀ layout₁ layout₂ secret₁ secret₂,
      ValidHostLayout layout₁ →
      ValidHostLayout layout₂ →
      let st₁ := mkInit layout₁ secret₁
      let st₂ := mkInit layout₂ secret₂
      st₁.world.nextAllocId = 0 ∧
      RhoValid 0 st₁.world.nextAllocId (fun x => x) ∧
      StateRel2Deps env ctxInit fullDataDeps (fun x => x) (fun _ => false)
        st₁.world.structFieldNull st₁ st₂) := by
  obtain ⟨ctxD, hdefD, hpcD, hregsD, hstkD, hbndsD, hresD, hvalidD⟩ :=
    ComputationContext_default_spec (env := env) (vCounter := 0)
  rw [hdef] at hdefD
  cases ok_inj hdefD
  rcases verify_program_core_loop3_ok_spec hlen hloop3 with
    ⟨hpcI, hstkI, hbndsI, hresI, hregsI⟩
  have hdum := hentry.2.2.2.2.2.2 dummyHostLayout dummyHostLayout dummySecretInit dummySecretInit
    dummyHostLayout_valid dummyHostLayout_valid
  refine ⟨⟨?_, hstkI ▸ hvalidD.2.1, hbndsI ▸ hvalidD.2.2.1, hresI ▸ hvalidD.2.2.2⟩, ?_⟩
  · intro t ht
    rcases List.mem_iff_getElem.mp ht with ⟨r, ho, rfl⟩
    have hr : r < 10 := by rw [array10_len] at ho; exact ho
    have hi : r < ctx0.registers.val.length := by rw [array10_len]; exact hr
    rw [hregsI r hr ho hi]
    split_ifs with harg
    · have hc_arg := hdum.2.2.2.2.2.2.2.2.2.1 (r - 1) harg.2
      exact (EntryArgConforms_valid_and_rel2 (nullPred := fun _ => false) hdum.2.2.2.2.2.1 hc_arg).1
    · exact hvalidD.1 _ (List.getElem_mem hi)
  · intro layout₁ layout₂ secret₁ secret₂ hl₁ hl₂
    dsimp only
    set st₁ := mkInit layout₁ secret₁
    set st₂ := mkInit layout₂ secret₂
    rcases hentry.2.2.2.2.2.2 layout₁ layout₂ secret₁ secret₂ hl₁ hl₂ with
      ⟨hpc1, hpc2, hwrld, hlive0, halloc0, harr, hr10_1, hr10_2, _, hargs, _, _⟩
    refine ⟨halloc0, RhoValid_zero _, ?_⟩
    have hpc0 : ctxInit.pc.val = 0 := by rw [hpcI, hpcD]; rfl
    refine ⟨by rw [hpc1, hpc0], by rw [hpc2, hpc0], hwrld, harr, fun _ _ => rfl, ?_, ?_, ?_,
      hr10_1, hr10_2, ?_, ?_⟩
    · rw [hlive0]; exact List.nodup_nil
    · intro m
      rw [hlive0, hresI, hresD]
      simp [alloc.vec.Vec.new]
    · intro m bound hmem
      rw [hbndsI, hbndsD] at hmem
      simp [alloc.vec.Vec.new] at hmem
    · intro r hr ho _
      have hi : r < ctx0.registers.val.length := by rw [array10_len]; exact hr
      rw [hregsI r hr ho hi]
      split_ifs with harg
      · have hc := hargs (r - 1) harg.2
        have hr_idx : r - 1 + 1 = r := by omega
        rw [hr_idx] at hc
        exact (EntryArgConforms_valid_and_rel2 harr hc).2
      · have huninit : ctx0.registers.val[r]'hi = .ScalarValue uninitScalar64 := by
          simp only [hregsD, List.getElem_replicate]
        rw [huninit]
        simp [TypeRel2]
    · intro slot _ _
      have hslot_uninit : stackSlotType ctxInit.stack slot = .ScalarValue uninitScalar64 := by
        rw [hstkI, hstkD]
        simp [stackSlotType, BitVec.getLsbD_zero]
      rw [hslot_uninit]
      simp [StackSlotRel2]

/-- Canonical `Type::unknown_written_scalar(w)` scalar data constrained to width `w`. -/
def unknownWrittenScalarWidth (w : DataWidth) : U64ScalarValueData :=
  { value := 0#u64
    unknown_mask := ⟨w.valMask64⟩
    unwritten_mask := 0#u64
    urange := { min := 0#u64, max := ⟨w.valMask64⟩ }
    _guard := () }

theorem unknownWrittenScalarWidth_eq_pureNew64 (w : DataWidth) :
    pureNew64 0#64 w.valMask64 0#64 0#64 w.valMask64 = unknownWrittenScalarWidth w := by
  cases w <;> {
    unfold pureNew64 unknownWrittenScalarWidth DataWidth.valMask64
    simp only [U64ScalarValueData.mk.injEq, U64Range.mk.injEq, u64_eq_iff_bv,
      u64_mk_bv, u64_zero_bv]
    refine ⟨?_, ?_, ?_, ?_, ?_⟩ <;> bv_decide
  }

theorem Type_unknown_written_scalar_eq (w : api.DataWidth) :
    verifier.Type.unknown_written_scalar w =
      ok (.ScalarValue (unknownWrittenScalarWidth (DataWidth.ofCore w))) := by
  unfold verifier.Type.unknown_written_scalar U64Range.new
  rw [bind_ok_eq (Type_mask_spec w)]
  simp only [bind_ok]
  rw [new64_eq_ok]
  dsimp only [u64_zero_bv, u64_mk_bv]
  rw [unknownWrittenScalarWidth_eq_pureNew64 (DataWidth.ofCore w)]
  simp only [bind_ok]

theorem unknownWrittenScalarWidth_valid (w : DataWidth) :
    Scalar64Valid (unknownWrittenScalarWidth w) := by
  rw [← unknownWrittenScalarWidth_eq_pureNew64 w]
  exact pureNew64_valid _ _ _ _ _

theorem concretizesScalar64Rel2_unknownWrittenWidth (w : DataWidth) (x : BitVec 64)
    (hx : x ≤ w.valMask64) :
    ConcretizesScalar64Rel2 (unknownWrittenScalarWidth w) x x := by
  have hc : ConcretizesScalar64 (unknownWrittenScalarWidth w) x := by
    cases w <;> {
      unfold ConcretizesScalar64 Scalar64Consistent unknownWrittenScalarWidth DataWidth.valMask64 at *
      simp only [u64_zero_bv, u64_mk_bv] at *
      refine ⟨⟨?_, ?_, ?_, ?_, ?_⟩, ?_, ?_, ?_⟩ <;> bv_decide
    }
  exact ⟨hc, hc, rfl⟩

theorem readBufWord_le_valMask64
    (buf : MemoryId → BitVec 64 → BitVec 8) (id : MemoryId) (off : BitVec 64) (w : DataWidth) :
    readBufWord buf id off w ≤ w.valMask64 := by
  unfold readBufWord
  generalize buf id (off + 0#64) = b0
  generalize buf id (off + 1#64) = b1
  generalize buf id (off + 2#64) = b2
  generalize buf id (off + 3#64) = b3
  generalize buf id (off + 4#64) = b4
  generalize buf id (off + 5#64) = b5
  generalize buf id (off + 6#64) = b6
  generalize buf id (off + 7#64) = b7
  cases w <;> (unfold DataWidth.valMask64; dsimp only; bv_decide)

theorem concretizesScalar64Rel2_readBufWord
    (buf : MemoryId → BitVec 64 → BitVec 8) (id : MemoryId) (off : BitVec 64) (w : DataWidth) :
    ConcretizesScalar64Rel2 (unknownWrittenScalarWidth w)
      (readBufWord buf id off w) (readBufWord buf id off w) :=
  concretizesScalar64Rel2_unknownWrittenWidth w _ (readBufWord_le_valMask64 buf id off w)

end StarnixEbpf.Proof
