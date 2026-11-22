-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Proof.AluAndJumpSoundness

/-!
# Instruction-Level Forward & Backward Soundness: ALU, Move, Neg, Endian, Jump, Exit

Connects `VisitorDispatchNonLddw` on `ComputationContext` (forward abstract
interpretation) and `DependencyTracker` (backward liveness transfer) to the
operational reference semantics (`Spec.step` / `Spec.runSteps` / `SemTerminatesIn`)
for `.alu64`, `.alu32`, `.neg`, `.endian`, `.jump`, and `.exit`.
-/

set_option linter.unusedTactic false
set_option linter.unnecessarySimpa false
set_option linter.unnecessarySeqFocus false
set_option maxHeartbeats 800000

open Aeneas Aeneas.Std Result

namespace StarnixEbpf.Proof

open StarnixEbpf.Assumptions
open StarnixEbpf.Spec
open StarnixEbpf.LoopLemmas
open ebpf_core
open ebpf_core.scalar_value

/-! ## 1. Common Visitor Abbreviations, Environment Conformance, & Step Combinators -/

abbrev CompCtxVisitorInst :
    visitor.BpfVisitor verifier.ComputationContext verifier.VerificationContext :=
  verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext

abbrev DepTrackerVisitorInst :
    visitor.BpfVisitor verifier.DependencyTracker verifier.VerificationContext :=
  verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext

/-- Environment-level portion of `EntryStatesConform`, invariant throughout verification. -/
def CallingContextEnvMatches (cctx : verifier.CoreCallingContext) (env : ExecEnv) : Prop :=
  env.allowPtrLeaks = cctx.allow_ptr_leaks ∧
  PacketTypeConforms cctx.packet_type env.packetCtx ∧
  (∀ idx, env.maps idx = cctx.maps.val[idx]?.map MapSpec.ofCore) ∧
  (∀ helperIdx sig,
    cctx.helpers.val.find? (fun p => p.1.val == helperIdx) =
      some (⟨BitVec.ofNat 32 helperIdx⟩, sig) ∨
    (∃ k, cctx.helpers.val.find? (fun p => p.1.val == helperIdx) = some (k, sig)) →
    ∃ spec, env.helpers helperIdx = some spec ∧ HelperSigMatches env sig spec) ∧
  (∀ step id, env.helperArrayLenOracle step id ≤ 0xFFFF_FFFF#64)

theorem CallingContextEnvMatches.of_entryStatesConform
    {cctx : verifier.CoreCallingContext} {env : ExecEnv}
    {mkInit : HostLayout → SecretInit → MachineState}
    (h : EntryStatesConform cctx env mkInit) :
    CallingContextEnvMatches cctx env :=
  ⟨h.1, h.2.1, h.2.2.1,
    fun helperIdx sig hor =>
      h.2.2.2.2.1 helperIdx sig (hor.elim (fun h1 => ⟨_, h1⟩) id),
    h.2.2.2.2.2.1⟩

/-- Standard forward-visit postcondition for `BpfVisitor.visit` on `ComputationContext`. -/
def ForwardVisitPost (env : ExecEnv)
    (vctx0 vctx1 : verifier.VerificationContext)
    (ctx_out : verifier.ComputationContext) : Prop :=
  vctx1.calling_context = vctx0.calling_context ∧
  vctx1.code = vctx0.code ∧
  vctx1.iteration = vctx0.iteration ∧
  vctx1.terminating_contexts = vctx0.terminating_contexts ∧
  vctx0.counter.bv.toNat ≤ vctx1.counter.bv.toNat ∧
  CompCtxValid env vctx1.counter.bv.toNat ctx_out ∧
  (∀ opt ∈ vctx1.pending_successors.val,
    ∃ succ, opt = some succ ∧ CompCtxValid env vctx1.counter.bv.toNat succ)

theorem stepNextIte_eq_next {prog : Program} {env : ExecEnv}
    {layout : HostLayout} {secret : SecretInit}
    {nextPc : Nat} {regs' : Nat → RuntimeVal} {stk' : StackMem} {w' : WorldState}
    {st' : MachineState} {F : Nat} {res : BitVec 64 × WorldState}
    (hst' : st' = { pc := nextPc, regs := regs', stack := stk', world := bumpWorldStep w' })
    (hrun : runSteps prog env layout secret F st' = some (.inl res)) :
    (if hnext : nextPc < prog.size then
       if prog[nextPc] == .lddwCont then StepOutcome.stuck (.invalidMidLddw nextPc)
       else StepOutcome.next { pc := nextPc, regs := regs', stack := stk', world := bumpWorldStep w' }
     else StepOutcome.stuck (.invalidPc nextPc)) =
    StepOutcome.next st' := by
  subst hst'
  obtain ⟨_, hnext, hnot_lddw⟩ := runSteps_pc_valid hrun
  simp [hnext, hnot_lddw]

def evalOperand (st : MachineState) : Operand → RuntimeVal
  | .imm imm => .scalar imm
  | .reg r   => st.regs r

theorem checkDst_eq_ok {dst : Nat} (hdst : dst < GENERAL_REGISTER_COUNT) :
    (if dst < GENERAL_REGISTER_COUNT then Except.ok ()
     else if dst == GENERAL_REGISTER_COUNT then Except.error SafetyViolation.writeToReadOnlyFramePtr
     else Except.error (SafetyViolation.invalidRegister dst)) =
    Except.ok () := by
  simp [hdst]

theorem readOp_eq_ok (st : MachineState) (op : Operand)
    (hvalid : ∀ r, op = .reg r → r < REGISTER_COUNT) :
    (match op with
     | .imm imm => Except.ok (RuntimeVal.scalar imm)
     | .reg r =>
         if r < REGISTER_COUNT then Except.ok (st.regs r)
         else Except.error (SafetyViolation.invalidRegister r)) =
    Except.ok (evalOperand st op) := by
  cases op with
  | imm imm => rfl
  | reg r => simp [evalOperand, hvalid r rfl]

/-! ## 2. Unary Register / Source Lookup & `Type.inner` Helpers -/

theorem ComputationContext_reg_ok_valid
    {env : ExecEnv} {vCounter : Nat} {ctx : verifier.ComputationContext}
    {index : Std.U8} {t : verifier.Type}
    (hvalid : CompCtxValid env vCounter ctx)
    (hreg : verifier.ComputationContext.reg ctx index = ok (.Ok t)) :
    index.val < REGISTER_COUNT ∧ TypeValid env vCounter t := by
  obtain ⟨r, hr, hspec⟩ := ComputationContext_reg_spec ctx index
  rw [hreg] at hr
  have hr_eq : r = .Ok t := (ok_inj hr).symm
  subst hr_eq
  by_cases h11 : index.val ≥ 11
  · simp [h11] at hspec
  · simp only [h11, ↓reduceIte] at hspec
    have hlt11 : index.val < REGISTER_COUNT := by unfold REGISTER_COUNT; omega
    by_cases h10 : index.val < 10
    · simp only [h10, ↓reduceDIte] at hspec
      cases core.result.Result.Ok.inj hspec.symm
      have hlen : index.val < ctx.registers.val.length := by rw [array10_len]; exact h10
      exact ⟨hlt11, hvalid.1 _ (List.getElem_mem hlen)⟩
    · simp only [h10, ↓reduceDIte] at hspec
      cases core.result.Result.Ok.inj hspec.symm
      exact ⟨hlt11, knownScalar64_valid 512#u64, rfl, by decide⟩

theorem ComputationContext_compute_source_ok_valid
    {env : ExecEnv} {vCounter : Nat} {ctx : verifier.ComputationContext}
    {src : visitor.Source} {t : verifier.Type}
    (hvalid : CompCtxValid env vCounter ctx)
    (hsrc : verifier.ComputationContext.compute_source ctx src = ok (.Ok t)) :
    (∀ r, operandOfSource src = .reg r → r < REGISTER_COUNT) ∧
    TypeValid env vCounter t := by
  cases src with
  | Reg reg =>
    unfold verifier.ComputationContext.compute_source at hsrc
    obtain ⟨hlt, htv⟩ := ComputationContext_reg_ok_valid hvalid hsrc
    refine ⟨fun r hr => ?_, htv⟩
    cases hr; exact hlt
  | Value v =>
    unfold verifier.ComputationContext.compute_source at hsrc
    dsimp only at hsrc
    rw [bind_ok_eq (from_known_64_eq v)] at hsrc
    cases core.result.Result.Ok.inj (ok_inj hsrc)
    exact ⟨(fun _ hr => by cases hr), knownScalar64_valid v⟩

theorem TypeRel2_scalar_rel2_of_concretizes
    {env : ExecEnv} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {arrayLen : MemoryId → BitVec 64}
    {d : U64ScalarValueData} {x₁ x₂ : BitVec 64}
    (hc : ConcretizesScalar64Rel2 d x₁ x₂) :
    TypeRel2 env ρ resActive isNullId arrayLen (.ScalarValue d) (.scalar x₁) (.scalar x₂) := by
  by_cases hd : d = uninitScalar64
  · simp [TypeRel2, hd]
  · simp only [TypeRel2, hd, ↓reduceIte]
    exact ⟨x₁, x₂, rfl, rfl, hc⟩

theorem ComputationContext_compute_source_evalOperand_rel2
    {env : ExecEnv} {vCounter : Nat} {ctx : verifier.ComputationContext}
    {deps : verifier.DataDependencies} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {st₁ st₂ : MachineState}
    {src : visitor.Source} {t : verifier.Type}
    (hvalid : CompCtxValid env vCounter ctx)
    (hrel : StateRel2Deps env ctx deps ρ resActive isNullId st₁ st₂)
    (hlive : ∀ r, src = .Reg r → r.val = 10 ∨ DataDepsHasReg deps r.val = true)
    (hsrc : verifier.ComputationContext.compute_source ctx src = ok (.Ok t)) :
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen t
      (evalOperand st₁ (operandOfSource src))
      (evalOperand st₂ (operandOfSource src)) := by
  cases src with
  | Reg reg =>
    unfold verifier.ComputationContext.compute_source at hsrc
    exact (ComputationContext_reg_ok_valid_and_rel2 hvalid hrel (hlive reg rfl) hsrc).2.2
  | Value v =>
    unfold verifier.ComputationContext.compute_source at hsrc
    dsimp only at hsrc
    rw [bind_ok_eq (from_known_64_eq v)] at hsrc
    cases core.result.Result.Ok.inj (ok_inj hsrc)
    exact TypeRel2_scalar_rel2_of_concretizes (concretizesScalar64Rel2_known v)

theorem Type_inner_ok_valid_and_checkAlive_none
    {env : ExecEnv} {vCounter : Nat} {ctx : verifier.ComputationContext}
    {deps : verifier.DataDependencies} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {st₁ st₂ : MachineState}
    {t u : verifier.Type} {v₁ v₂ : RuntimeVal}
    (hctx : CompCtxValid env vCounter ctx)
    (hrel_ctx : StateRel2Deps env ctx deps ρ resActive isNullId st₁ st₂)
    (ht : TypeValid env vCounter t)
    (hne : t ≠ .ScalarValue uninitScalar64)
    (hinner : verifier.Type.inner t ctx = ok (.Ok u))
    (hrel : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen t v₁ v₂) :
    u = pureUnwrapReleasable t ∧
    TypeValid env vCounter u ∧
    checkAlive st₁.world.liveResources v₁ = none ∧
    checkAlive st₂.world.liveResources v₂ = none := by
  unfold verifier.Type.inner at hinner
  obtain ⟨rc, hrc, _⟩ := Type_check_not_released_spec hctx.2.2.2 t
  rw [bind_ok_eq hrc] at hinner
  cases rc with
  | Err _ => cases ok_inj hinner
  | Ok u0 =>
    cases u0
    dsimp only at hinner
    rw [bind_ok_eq (Type_unwrap_releasable_spec t)] at hinner
    have hu_eq : u = pureUnwrapReleasable t :=
      (core.result.Result.Ok.inj (ok_inj hinner)).symm
    obtain ⟨hc1, hc2⟩ := Type_check_not_released_checkAlive_none
      hctx.2.2.2 hrel_ctx.2.2.2.2.2.2.1 ht hne hrc hrel
    have hc2' : checkAlive st₂.world.liveResources v₂ = none := by
      rwa [← hrel_ctx.2.2.1]
    exact ⟨hu_eq, hu_eq ▸ Type_unwrap_releasable_valid ht, hc1, hc2'⟩

/-! ## 3. `DependencyTracker.mov`, 32-Bit `mov` Scalar Truncation, & `endianness` Helpers -/

theorem DependencyTracker_mov_ok_spec
    {self tracker : verifier.DependencyTracker}
    {vctx vctx' : verifier.VerificationContext}
    {dst : Std.U8} {src : visitor.Source}
    {res : core.result.Result Unit error.VerifierErrorKind}
    (h : DepTrackerVisitorInst.mov self vctx dst src = ok (res, tracker, vctx')) :
    res = .Ok () ∧
    tracker.context = self.context ∧
    vctx' = vctx ∧
    (∀ r, r < 10 → r ≠ dst.val →
      DataDepsHasReg self.deps r = true → DataDepsHasReg tracker.deps r = true) ∧
    (∀ s, s < 64 →
      DataDepsHasStack self.deps s = true → DataDepsHasStack tracker.deps s = true) ∧
    (DataDepsHasReg self.deps dst.val = true →
      ∀ s, src = .Reg s → DataDepsHasReg tracker.deps s.val = true) := by
  change verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mov
    self vctx dst src = ok (res, tracker, vctx') at h
  unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mov at h
  obtain ⟨beq, hbeq, hbeq_iff⟩ := Source_eq_spec src (.Reg dst)
  rw [bind_ok_eq hbeq] at h
  cases beq with
  | true =>
    simp only [↓reduceIte] at h
    rcases Prod.mk.inj (ok_inj h) with ⟨hres, hpair⟩
    rcases Prod.mk.inj hpair with ⟨htrk, hv⟩
    subst res; subst tracker; subst vctx'
    have hsrc_eq : src = .Reg dst := hbeq_iff.mp rfl
    subst src
    refine ⟨rfl, rfl, rfl, fun _ _ _ hr => hr, fun _ _ hs => hs, fun hd s hs => ?_⟩
    cases hs; exact hd
  | false =>
    simp only [Bool.false_eq_true, ↓reduceIte] at h
    have hsrc_ne : src ≠ .Reg dst := fun hc => by
      have := hbeq_iff.mpr hc; cases this
    rcases bind_eq_ok h with ⟨b1, hb1, h⟩
    obtain ⟨hdst_lt16, rfl⟩ := DataDependencies_has_reg_ok_spec hb1
    by_cases hlive : DataDepsHasReg self.deps dst.val = true
    · simp only [hlive, ↓reduceIte] at h
      rcases hsrc_cases : src with src1 | v
      · rw [hsrc_cases] at h
        dsimp only at h
        rcases bind_eq_ok h with ⟨dd, hadd, h⟩
        rcases bind_eq_ok h with ⟨⟨_, dd1⟩, hrem, h⟩
        rcases Prod.mk.inj (ok_inj h) with ⟨hres, hpair⟩
        rcases Prod.mk.inj hpair with ⟨htrk, hv⟩
        subst res; subst tracker; subst vctx'
        obtain ⟨hsrc_lt16, _, hadd_reg, hadd_stk, _⟩ := DataDependencies_add_reg_ok_spec hadd
        obtain ⟨_, _, _, hrem_reg, hrem_stk⟩ := DataDependencies_remove_reg_ok_spec hrem
        have hne_val : src1.val ≠ dst.val := by
          intro hc
          apply hsrc_ne
          rw [hsrc_cases]
          congr 1
          exact UScalar.eq_of_val_eq hc
        refine ⟨rfl, rfl, rfl, ?_, ?_, ?_⟩
        · intro r hr hne hd
          dsimp only
          rw [hrem_reg r (by omega), hadd_reg r (by omega), hd, Bool.true_or]
          simp [hne]
        · intro s _ hd
          dsimp only
          rw [hrem_stk s, hadd_stk s]
          exact hd
        · intro _ s hs
          cases hs
          dsimp only
          rw [hrem_reg src1.val hsrc_lt16, hadd_reg src1.val hsrc_lt16]
          simp [hne_val]
      · rw [hsrc_cases] at h
        simp only [bind_ok] at h
        rcases bind_eq_ok h with ⟨⟨_, dd1⟩, hrem, h⟩
        rcases Prod.mk.inj (ok_inj h) with ⟨hres, hpair⟩
        rcases Prod.mk.inj hpair with ⟨htrk, hv⟩
        subst res; subst tracker; subst vctx'
        obtain ⟨_, _, _, hrem_reg, hrem_stk⟩ := DataDependencies_remove_reg_ok_spec hrem
        refine ⟨rfl, rfl, rfl, ?_, ?_, ?_⟩
        · intro r hr hne hd
          dsimp only
          rw [hrem_reg r (by omega), hd]
          simp [hne]
        · intro s _ hd
          dsimp only
          rw [hrem_stk s]
          exact hd
        · intro _ s hs
          cases hs
    · simp only [hlive, Bool.false_eq_true, ↓reduceIte] at h
      rcases Prod.mk.inj (ok_inj h) with ⟨hres, hpair⟩
      rcases Prod.mk.inj hpair with ⟨htrk, hv⟩
      subst res; subst tracker; subst vctx'
      exact ⟨rfl, rfl, rfl, fun _ _ _ hr => hr, fun _ _ hs => hs,
        fun hc => absurd hc hlive⟩

private theorem extractSubValue_u32_zero_eq_mov32 (x : BitVec 64) :
    extractSubValue x 0 .u32 = evalAlu32 .mov 0#64 x := by
  unfold extractSubValue evalAlu32 DataWidth.valMask64
  bv_decide

theorem mov32_scalar_branch_valid
    {data : U64ScalarValueData} {value : verifier.Type}
    (_hd : Scalar64Valid data)
    (h : (do
      let i ← lift (UScalar.cast .U32 data.value)
      let v1 ← lift (UScalar.cast .U64 i)
      let i1 ← lift (UScalar.cast .U32 data.unknown_mask)
      let unknown_mask ← lift (UScalar.cast .U64 i1)
      let i2 ← lift (UScalar.cast .U32 data.unwritten_mask)
      let unwritten_mask ← lift (UScalar.cast .U64 i2)
      let ur ← U64Range.from_scalar 0#u64
      let urange ← U64Range.compute_range_for_bytes_swap ur data.urange 0#usize 0#usize 4#usize
      let usvd ← U64ScalarValueData.new v1 unknown_mask unwritten_mask urange
      ok (verifier.Type.ScalarValue usvd)) = ok value) :
    TypeValid env vCounter value := by
  have hur0 : U64Range.from_scalar 0#u64 = ok { min := 0#u64, max := 0#u64 } := rfl
  simp only [lift_eq_ok, bind_ok, hur0] at h
  rcases bind_eq_ok h with ⟨urange, _, h⟩
  rcases bind_eq_ok h with ⟨usvd, hnew, h⟩
  cases ok_inj h
  exact new64_valid hnew

theorem mov32_scalar_branch_rel2
    {env : ExecEnv} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {arrayLen : MemoryId → BitVec 64}
    {layout₁ layout₂ : HostLayout}
    {data : U64ScalarValueData} {value : verifier.Type}
    {v₁ v₂ : RuntimeVal}
    (hrel : TypeRel2 env ρ resActive isNullId arrayLen (.ScalarValue data) v₁ v₂)
    (h : (do
      let i ← lift (UScalar.cast .U32 data.value)
      let v1 ← lift (UScalar.cast .U64 i)
      let i1 ← lift (UScalar.cast .U32 data.unknown_mask)
      let unknown_mask ← lift (UScalar.cast .U64 i1)
      let i2 ← lift (UScalar.cast .U32 data.unwritten_mask)
      let unwritten_mask ← lift (UScalar.cast .U64 i2)
      let ur ← U64Range.from_scalar 0#u64
      let urange ← U64Range.compute_range_for_bytes_swap ur data.urange 0#usize 0#usize 4#usize
      let usvd ← U64ScalarValueData.new v1 unknown_mask unwritten_mask urange
      ok (verifier.Type.ScalarValue usvd)) = ok value) :
    TypeRel2 env ρ resActive isNullId arrayLen value
      (.scalar (evalAlu32 .mov 0#64 (v₁.toBits layout₁ arrayLen)))
      (.scalar (evalAlu32 .mov 0#64 (v₂.toBits layout₂ arrayLen))) := by
  have hur0 : U64Range.from_scalar 0#u64 = ok { min := 0#u64, max := 0#u64 } := rfl
  simp only [lift_eq_ok, bind_ok, hur0] at h
  have hconc := TypeRel2_scalar_toBits_rel2
    (layout₁ := layout₁) (layout₂ := layout₂) hrel
  have hw_u32 : DataWidth.u32 = .u8 ∨ DataWidth.u32 = .u16 ∨ DataWidth.u32 = .u32 :=
    Or.inr (Or.inr rfl)
  have hsub0 : (0#usize).val + DataWidth.u32.bytes ≤ 8 := by decide
  obtain ⟨rOut, hrOut, hcr1⟩ := compute_range_for_bytes_swap_load_sound
    data.urange 0#usize 4#usize .u32 (v₁.toBits layout₁ arrayLen) hw_u32 usize_const_val_4 hsub0
    ⟨hconc.1.2.2.1, hconc.1.2.2.2⟩
  obtain ⟨rOut2, hrOut2, hcr2⟩ := compute_range_for_bytes_swap_load_sound
    data.urange 0#usize 4#usize .u32 (v₂.toBits layout₂ arrayLen) hw_u32 usize_const_val_4 hsub0
    ⟨hconc.2.1.2.2.1, hconc.2.1.2.2.2⟩
  rw [hrOut] at hrOut2
  cases ok_inj hrOut2
  rw [bind_ok_eq hrOut] at h
  have h0_val : (0#usize).val = 0 := rfl
  rw [h0_val, extractSubValue_u32_zero_eq_mov32] at hcr1
  rw [h0_val, extractSubValue_u32_zero_eq_mov32] at hcr2
  rcases hconc with ⟨⟨_, ht1, _⟩, ⟨_, ht2, _⟩, hsec⟩
  obtain ⟨res, hnew, hrel_res⟩ := new64_sound_2safety
    (UScalar.cast .U64 (UScalar.cast .U32 data.value))
    (UScalar.cast .U64 (UScalar.cast .U32 data.unknown_mask))
    (UScalar.cast .U64 (UScalar.cast .U32 data.unwritten_mask))
    rOut
    (evalAlu32 .mov 0#64 (v₁.toBits layout₁ arrayLen))
    (evalAlu32 .mov 0#64 (v₂.toBits layout₂ arrayLen))
    hcr1
    (by
      simp only [cast_u32_to_u64_bv, cast_u64_to_u32_bv]
      unfold evalAlu32
      generalize v₁.toBits layout₁ arrayLen = b₁ at ht1 ⊢
      generalize data.value.bv = vbv at ht1 ⊢
      generalize data.unknown_mask.bv = ubv at ht1 ⊢
      revert ht1; bv_decide)
    hcr2
    (by
      simp only [cast_u32_to_u64_bv, cast_u64_to_u32_bv]
      unfold evalAlu32
      generalize v₂.toBits layout₂ arrayLen = b₂ at ht2 ⊢
      generalize data.value.bv = vbv at ht2 ⊢
      generalize data.unknown_mask.bv = ubv at ht2 ⊢
      revert ht2; bv_decide)
    (by
      simp only [cast_u32_to_u64_bv, cast_u64_to_u32_bv]
      unfold evalAlu32
      generalize v₁.toBits layout₁ arrayLen = b₁ at hsec ⊢
      generalize v₂.toBits layout₂ arrayLen = b₂ at hsec ⊢
      generalize data.unwritten_mask.bv = wbv at hsec ⊢
      revert hsec; bv_decide)
  rw [bind_ok_eq hnew] at h
  cases ok_inj h
  exact TypeRel2_scalar_rel2_of_concretizes hrel_res

private theorem u64_shl_i32_ok' (x : Std.U64) (s : Std.I32)
    (hge : s.val ≥ 0) (hlt : s.toNat < 64) :
    (x <<< s : Result Std.U64) = ok ⟨x.bv <<< s.toNat⟩ := by
  change UScalar.shiftLeft_IScalar x s = _
  simp only [UScalar.shiftLeft_IScalar, hge, ↓reduceIte, UScalar.shiftLeft,
    UScalarTy.numBits, hlt]
  rfl

private theorem u64_shr_i32_ok' (x : Std.U64) (s : Std.I32)
    (hge : s.val ≥ 0) (hlt : s.toNat < 64) :
    (x >>> s : Result Std.U64) = ok ⟨x.bv >>> s.toNat⟩ := by
  change UScalar.shiftRight_IScalar x s = _
  simp only [UScalar.shiftRight_IScalar, hge, ↓reduceIte, UScalar.shiftRight,
    UScalarTy.numBits, hlt]
  rfl

private theorem u64_shl_8_ok  (x : Std.U64) : (x <<< 8#i32  : Result Std.U64) = ok ⟨x.bv <<< 8⟩  :=
  u64_shl_i32_ok' x 8#i32  (by decide) (by decide)
private theorem u64_shl_24_ok (x : Std.U64) : (x <<< 24#i32 : Result Std.U64) = ok ⟨x.bv <<< 24⟩ :=
  u64_shl_i32_ok' x 24#i32 (by decide) (by decide)
private theorem u64_shl_40_ok (x : Std.U64) : (x <<< 40#i32 : Result Std.U64) = ok ⟨x.bv <<< 40⟩ :=
  u64_shl_i32_ok' x 40#i32 (by decide) (by decide)
private theorem u64_shl_56_ok (x : Std.U64) : (x <<< 56#i32 : Result Std.U64) = ok ⟨x.bv <<< 56⟩ :=
  u64_shl_i32_ok' x 56#i32 (by decide) (by decide)

private theorem u64_shr_8_ok  (x : Std.U64) : (x >>> 8#i32  : Result Std.U64) = ok ⟨x.bv >>> 8⟩  :=
  u64_shr_i32_ok' x 8#i32  (by decide) (by decide)
private theorem u64_shr_24_ok (x : Std.U64) : (x >>> 24#i32 : Result Std.U64) = ok ⟨x.bv >>> 24⟩ :=
  u64_shr_i32_ok' x 24#i32 (by decide) (by decide)
private theorem u64_shr_40_ok (x : Std.U64) : (x >>> 40#i32 : Result Std.U64) = ok ⟨x.bv >>> 40⟩ :=
  u64_shr_i32_ok' x 40#i32 (by decide) (by decide)
private theorem u64_shr_56_ok (x : Std.U64) : (x >>> 56#i32 : Result Std.U64) = ok ⟨x.bv >>> 56⟩ :=
  u64_shr_i32_ok' x 56#i32 (by decide) (by decide)

theorem apply_endianness_sound (doSwap : Bool) (ew : EndianWidth) (v : Std.U64) :
    ∃ out : Std.U64,
      api.apply_endianness doSwap (coreWidthOfEndian ew) v = ok out ∧
      out.bv = evalEndian doSwap ew v.bv := by
  cases doSwap <;> cases ew
  · -- false, w16
    refine ⟨v &&& 65535#u64, rfl, ?_⟩
    unfold evalEndian
    simp only [u64_and_bv, UScalar.ofNatCore_bv, UScalarTy.numBits]
    generalize v.bv = vbv; bv_decide
  · -- false, w32
    refine ⟨v &&& 4294967295#u64, rfl, ?_⟩
    unfold evalEndian
    simp only [u64_and_bv, UScalar.ofNatCore_bv, UScalarTy.numBits]
    generalize v.bv = vbv; bv_decide
  · -- false, w64
    exact ⟨v, rfl, rfl⟩
  · -- true, w16
    unfold api.apply_endianness coreWidthOfEndian evalEndian bswap16
    simp only [↓reduceIte, lift, bind_ok, u64_shl_8_ok, u64_shr_8_ok]
    refine ⟨_, rfl, ?_⟩
    simp only [u64_and_bv, u64_or_bv, u64_mk_bv, UScalar.ofNatCore_bv, UScalarTy.numBits]
    generalize v.bv = vbv; bv_decide
  · -- true, w32
    unfold api.apply_endianness coreWidthOfEndian evalEndian bswap32
    simp only [↓reduceIte, lift, bind_ok,
      u64_shl_8_ok, u64_shl_24_ok, u64_shr_8_ok, u64_shr_24_ok]
    refine ⟨_, rfl, ?_⟩
    simp only [u64_and_bv, u64_or_bv, u64_mk_bv, UScalar.ofNatCore_bv, UScalarTy.numBits]
    generalize v.bv = vbv; bv_decide
  · -- true, w64
    unfold api.apply_endianness coreWidthOfEndian evalEndian bswap64 bswap32
    simp only [↓reduceIte, lift, bind_ok,
      u64_shl_8_ok, u64_shl_24_ok, u64_shl_40_ok, u64_shl_56_ok,
      u64_shr_8_ok, u64_shr_24_ok, u64_shr_40_ok, u64_shr_56_ok]
    refine ⟨_, rfl, ?_⟩
    simp only [u64_and_bv, u64_or_bv, u64_mk_bv, UScalar.ofNatCore_bv, UScalarTy.numBits]
    generalize v.bv = vbv; bv_decide

theorem endian_scalar_valid_and_rel2
    {env : ExecEnv} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {arrayLen : MemoryId → BitVec 64}
    {doSwap : Bool} {ew : EndianWidth}
    {data usvd : U64ScalarValueData} {x : BitVec 64}
    (hconc : ConcretizesScalar64 data x)
    (h : (do
      let i ← api.apply_endianness doSwap (coreWidthOfEndian ew) data.value
      let i1 ← api.apply_endianness doSwap (coreWidthOfEndian ew) data.unknown_mask
      let ur ← U64Range.impl.max
      U64ScalarValueData.new i i1 0#u64 ur) = ok usvd) :
    Scalar64Valid usvd ∧
    TypeRel2 env ρ resActive isNullId arrayLen (.ScalarValue usvd)
      (.scalar (evalEndian doSwap ew x))
      (.scalar (evalEndian doSwap ew x)) := by
  obtain ⟨i, hi, hi_bv⟩ := apply_endianness_sound doSwap ew data.value
  obtain ⟨i1, hi1, hi1_bv⟩ := apply_endianness_sound doSwap ew data.unknown_mask
  have hur : U64Range.impl.max = ok { min := 0#u64, max := core.num.U64.MAX } := rfl
  rw [bind_ok_eq hi, bind_ok_eq hi1, bind_ok_eq hur] at h
  have hvalid : Scalar64Valid usvd := new64_valid h
  rcases hconc with ⟨_, ht, _⟩
  have hrange : ConcretizesRange64 { min := 0#u64, max := core.num.U64.MAX } (evalEndian doSwap ew x) := by
    unfold ConcretizesRange64
    simp only [u64_zero_bv, u64_max_bv]
    generalize evalEndian doSwap ew x = y
    bv_decide
  have htnum : evalEndian doSwap ew x &&& ~~~i1.bv = i.bv &&& ~~~i1.bv := by
    rw [hi_bv, hi1_bv]
    generalize data.value.bv = vbv at ht ⊢
    generalize data.unknown_mask.bv = ubv at ht ⊢
    cases doSwap <;> cases ew <;> (
      unfold evalEndian bswap64 bswap32 bswap16
      revert ht; bv_decide)
  obtain ⟨res, hnew, hrel_res⟩ := new64_sound_2safety
    i i1 0#u64 { min := 0#u64, max := core.num.U64.MAX }
    (evalEndian doSwap ew x) (evalEndian doSwap ew x)
    hrange htnum hrange htnum rfl
  rw [hnew] at h
  cases ok_inj h
  exact ⟨hvalid, TypeRel2_scalar_rel2_of_concretizes hrel_res⟩

/-! ## 4. Unified `ComputationContext.alu` Forward & Backward Soundness -/

private theorem alu_log_ok_spec
    {ctx : verifier.ComputationContext} {vctx0 : verifier.VerificationContext}
    {log_op : Option verifier.AluLogOp} {dst : Std.U8} {src : visitor.Source}
    {A : Type} {k : verifier.ComputationContext × verifier.VerificationContext → Result A} {res : A}
    (h : (do
      let p ← match log_op with
        | none => ok (ctx, vctx0)
        | some op => do
          let vc ← verifier.VerificationContext.emit_log vctx0 ctx.pc (.Alu op dst src)
          ok (ctx, vc)
      k p) = ok res) :
    ∃ vctx_log,
      k (ctx, vctx_log) = ok res ∧
      vctx_log.calling_context = vctx0.calling_context ∧
      vctx_log.code = vctx0.code ∧
      vctx_log.counter = vctx0.counter ∧
      vctx_log.iteration = vctx0.iteration ∧
      vctx_log.terminating_contexts = vctx0.terminating_contexts ∧
      vctx_log.pending_successors = vctx0.pending_successors := by
  cases log_op with
  | none =>
    simp only [bind_ok] at h
    exact ⟨vctx0, h, rfl, rfl, rfl, rfl, rfl, rfl⟩
  | some op =>
    dsimp only at h
    rcases bind_eq_ok h with ⟨⟨c1, vc1⟩, hemit_pair, hk⟩
    rcases bind_eq_ok hemit_pair with ⟨vc2, hemit, heq⟩
    rcases Prod.mk.inj (ok_inj heq) with ⟨hc1, hvc1⟩
    subst c1; subst vc1
    obtain ⟨h1, h2, h3, h4, h5, h6⟩ := VerificationContext_emit_log_ok_spec hemit
    exact ⟨vc2, hk, h1, h2, h3, h4, h5, h6⟩

theorem ComputationContext_alu_forward
    {env : ExecEnv} {ctx : verifier.ComputationContext}
    {vctx0 vctx1 : verifier.VerificationContext}
    {log_op : Option verifier.AluLogOp} {dst : Std.U8} {src : visitor.Source}
    {alu_type : verifier.AluType} {alu_op : verifier.ScalarAluOp}
    (hctx : CompCtxValid env vctx0.counter.bv.toNat ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hcompat : (alu_type = .Add ∨ alu_type = .Sub) → (alu_op = .Add64 ∨ alu_op = .Sub64))
    (hok : verifier.ComputationContext.alu ctx log_op vctx0 dst src alu_type alu_op =
      ok (.Ok (), vctx1)) :
    ForwardVisitPost env vctx0 vctx1 ctx := by
  unfold verifier.ComputationContext.alu at hok
  obtain ⟨vctx_log, hok, hl_cctx, hl_code, hl_cnt, hl_iter, hl_tc, hl_ps⟩ := alu_log_ok_spec hok
  rcases bind_eq_ok hok with ⟨r, hreg, hok⟩
  cases r with
  | Err _ => cases Prod.mk.inj (ok_inj hok) |>.1
  | Ok v =>
    dsimp only at hok
    rcases bind_eq_ok hok with ⟨r1, hsrc, hok⟩
    cases r1 with
    | Err _ => cases Prod.mk.inj (ok_inj hok) |>.1
    | Ok v1 =>
      dsimp only at hok
      rcases bind_eq_ok hok with ⟨r2, happ, hok⟩
      cases r2 with
      | Err _ => cases Prod.mk.inj (ok_inj hok) |>.1
      | Ok v2 =>
        dsimp only at hok
        rcases bind_eq_ok hok with ⟨r3, hnext, hok⟩
        cases r3 with
        | Err _ => cases Prod.mk.inj (ok_inj hok) |>.1
        | Ok n =>
          dsimp only at hok
          rcases bind_eq_ok hok with ⟨⟨r4, n1⟩, hset, hok⟩
          cases r4 with
          | Err _ => cases Prod.mk.inj (ok_inj hok) |>.1
          | Ok u0 =>
            cases u0
            rcases bind_eq_ok hok with ⟨vc2, hpush, hok⟩
            rcases Prod.mk.inj (ok_inj hok) with ⟨_, rfl⟩
            obtain ⟨_, hv_valid⟩ := ComputationContext_reg_ok_valid hctx hreg
            obtain ⟨_, hv1_valid⟩ := ComputationContext_compute_source_ok_valid hctx hsrc
            have hv2_valid := apply_computation_ok_valid hctx hv_valid hv1_valid hcompat happ
            obtain ⟨_, hn_eq⟩ := ComputationContext_next_ok_spec hnext
            have hn_valid : CompCtxValid env vctx0.counter.bv.toNat n := by
              rw [hn_eq]; exact hctx
            obtain ⟨_, _, _, _, _, _, hn1_valid⟩ :=
              ComputationContext_set_reg_ok_spec hn_valid hv2_valid hset
            obtain ⟨hp1, hp2, hp3, hp4, hp5, _, hp7⟩ := VerificationContext_push_successor_ok_spec hpush
            have hcnt_eq : vc2.counter = vctx0.counter := hp3.trans hl_cnt
            refine ⟨hp1.trans hl_cctx, hp2.trans hl_code, hp4.trans hl_iter, hp5.trans hl_tc,
              by rw [hcnt_eq], by rwa [hcnt_eq], ?_⟩
            intro opt hopt
            rw [hp7, hl_ps, hpending, List.nil_append, List.mem_singleton] at hopt
            subst hopt
            exact ⟨n1, rfl, by rwa [hcnt_eq]⟩

theorem ComputationContext_alu_backward
    {prog : Program} {env : ExecEnv} {vCounter F : Nat}
    {ctx : verifier.ComputationContext}
    {vctx0 vctx1 : verifier.VerificationContext}
    {dPost dPre : verifier.DataDependencies}
    {is64 : Bool} {op : AluOp} {log_op : Option verifier.AluLogOp}
    {dst : Std.U8} {src : visitor.Source}
    {alu_type : verifier.AluType} {alu_op : verifier.ScalarAluOp}
    (hop_ne_mov : op ≠ .mov)
    (hm : AluOpMatches is64 op alu_type alu_op)
    (hpc : ∃ hlt : ctx.pc.val < prog.size,
      prog[ctx.pc.val]'hlt =
        (if is64 then Insn.alu64 op dst.val (operandOfSource src)
         else Insn.alu32 op dst.val (operandOfSource src)))
    (hctx : CompCtxValid env vCounter ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hfwd : verifier.ComputationContext.alu ctx log_op vctx0 dst src alu_type alu_op =
      ok (.Ok (), vctx1))
    (hchildren : ∀ succ, some succ ∈ vctx1.pending_successors.val →
      SemTerminatesIn prog env vCounter succ dPost F)
    (hback : verifier.DataDependencies.alu dPost dst src = ok (.Ok (), dPre)) :
    SemTerminatesIn prog env vCounter ctx dPre (F + 1) := by
  unfold verifier.ComputationContext.alu at hfwd
  obtain ⟨vctx_log, hfwd, _, _, _, _, _, hl_ps⟩ := alu_log_ok_spec hfwd
  rcases bind_eq_ok hfwd with ⟨r, hreg, hfwd⟩
  cases r with
  | Err _ => cases Prod.mk.inj (ok_inj hfwd) |>.1
  | Ok v =>
    dsimp only at hfwd
    rcases bind_eq_ok hfwd with ⟨r1, hsrc, hfwd⟩
    cases r1 with
    | Err _ => cases Prod.mk.inj (ok_inj hfwd) |>.1
    | Ok v1 =>
      dsimp only at hfwd
      rcases bind_eq_ok hfwd with ⟨r2, happ, hfwd⟩
      cases r2 with
      | Err _ => cases Prod.mk.inj (ok_inj hfwd) |>.1
      | Ok v2 =>
        dsimp only at hfwd
        rcases bind_eq_ok hfwd with ⟨r3, hnext, hfwd⟩
        cases r3 with
        | Err _ => cases Prod.mk.inj (ok_inj hfwd) |>.1
        | Ok n =>
          dsimp only at hfwd
          rcases bind_eq_ok hfwd with ⟨⟨r4, n1⟩, hset, hfwd⟩
          cases r4 with
          | Err _ => cases Prod.mk.inj (ok_inj hfwd) |>.1
          | Ok u0 =>
            cases u0
            rcases bind_eq_ok hfwd with ⟨vc2, hpush, hfwd⟩
            rcases Prod.mk.inj (ok_inj hfwd) with ⟨_, rfl⟩
            obtain ⟨_, hv_valid⟩ := ComputationContext_reg_ok_valid hctx hreg
            obtain ⟨hsrc_reg, hv1_valid⟩ := ComputationContext_compute_source_ok_valid hctx hsrc
            obtain ⟨hpc_next, hn_eq⟩ := ComputationContext_next_ok_spec hnext
            have hn_valid : CompCtxValid env vCounter n := by rw [hn_eq]; exact hctx
            have hcompat : (alu_type = .Add ∨ alu_type = .Sub) → (alu_op = .Add64 ∨ alu_op = .Sub64) := by
              cases hm <;> simp
            have hv2_valid := apply_computation_ok_valid hctx hv_valid hv1_valid hcompat happ
            obtain ⟨hdst_lt10, hpc_n1, hregs_n1, hstk_n1, hbm_n1, hres_n1, _⟩ :=
              ComputationContext_set_reg_ok_spec hn_valid hv2_valid hset
            obtain ⟨_, _, _, _, _, _, hp7⟩ := VerificationContext_push_successor_ok_spec hpush
            have hn1_mem : some n1 ∈ vc2.pending_successors.val := by
              rw [hp7, hl_ps, hpending]; simp
            have hsem := hchildren n1 hn1_mem
            obtain ⟨_, _, _, hsub_deps, hlive_src⟩ := DataDependencies_alu_ok_spec hback
            obtain ⟨hlt_pc, hinsn_at⟩ := hpc
            intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
            let resVal (l : HostLayout) (st : MachineState) : RuntimeVal :=
              if is64 then
                evalAlu64Val l st.world.arrayLen op (st.regs dst.val)
                  (evalOperand st (operandOfSource src))
              else
                .scalar (evalAlu32 op
                  ((st.regs dst.val).toBits l st.world.arrayLen)
                  ((evalOperand st (operandOfSource src)).toBits l st.world.arrayLen))
            let st₁' : MachineState :=
              { st₁ with pc := n1.pc.val, regs := updateReg st₁.regs dst.val (resVal l₁ st₁),
                         world := bumpWorldStep st₁.world }
            let st₂' : MachineState :=
              { st₂ with pc := n1.pc.val, regs := updateReg st₂.regs dst.val (resVal l₂ st₂),
                         world := bumpWorldStep st₂.world }
            have hrel_post : StateRel2Deps env n1 dPost ρ resActive isNullId st₁' st₂' := by
              refine StateRel2Deps_set_reg (by unfold GENERAL_REGISTER_COUNT at hdst_lt10; exact hdst_lt10)
                (hregs_n1.trans (by rw [hn_eq]))
                (hstk_n1.trans (by rw [hn_eq]))
                (hbm_n1.trans (by rw [hn_eq]))
                (hres_n1.trans (by rw [hn_eq]))
                (fun r hr _ hd => hsub_deps.1 r hr hd)
                (fun s hs hd => hsub_deps.2 s hs hd)
                hrel
                (fun hdst_live => ?_)
              have hdst_pre : DataDepsHasReg dPre dst.val = true :=
                hsub_deps.1 dst.val (by unfold GENERAL_REGISTER_COUNT at hdst_lt10; exact hdst_lt10) hdst_live
              obtain ⟨_, _, hrel_dst⟩ :=
                ComputationContext_reg_ok_valid_and_rel2 hctx hrel (Or.inr hdst_pre) hreg
              have hsrc_live_pre : ∀ r, src = .Reg r → r.val = 10 ∨ DataDepsHasReg dPre r.val = true := by
                intro r hr; subst src; exact Or.inr (hlive_src hdst_live).2
              have hrel_src :=
                ComputationContext_compute_source_evalOperand_rel2 hctx hrel hsrc_live_pre hsrc
              have hrel_res := (apply_computation_ok_valid_and_rel2
                (layout₁ := l₁) (layout₂ := l₂)
                hctx hv_valid hv1_valid hm hrel_dst hrel_src happ).2
              simpa only [resVal, hrel.2.2.1] using hrel_res
            have halloc' : (bumpWorldStep st₁.world).nextAllocId + 3 * F < 2 ^ 63 := by
              simp only [bumpWorldStep]; omega
            obtain ⟨ret, finalWorld, hr₁, hr₂, hlive_fin⟩ :=
              hsem l₁ l₂ s₁ s₂ ρ resActive isNullId st₁' st₂' hl₁ hl₂ hleak halloc' hρ hrel_post
            have hpc1 := hrel.1
            have hpc2 := hrel.2.1
            have hn1_pc : n1.pc.val = ctx.pc.val + 1 := by rw [hpc_n1, hpc_next]
            have hmov_false : (op == .mov) = false := beq_false_of_ne hop_ne_mov
            refine ⟨ret, finalWorld, ?_, ?_, hlive_fin⟩
            · unfold runSteps step
              cases is64 <;> rcases hop : operandOfSource src with r | imm
              · simp only [Bool.false_eq_true, ↓reduceIte] at hinsn_at
                simp only [hpc1, hlt_pc, ↓reduceDIte, hinsn_at,
                  checkDst_eq_ok hdst_lt10, hop, hsrc_reg r hop, hmov_false, Bool.false_eq_true, ↓reduceIte]
                rw [stepNextIte_eq_next (by dsimp [st₁', resVal]; rw [hn1_pc, hop]; rfl) hr₁]
                exact hr₁
              · simp only [Bool.false_eq_true, ↓reduceIte] at hinsn_at
                simp only [hpc1, hlt_pc, ↓reduceDIte, hinsn_at,
                  checkDst_eq_ok hdst_lt10, hop, hmov_false, Bool.false_eq_true, ↓reduceIte]
                rw [stepNextIte_eq_next (by dsimp [st₁', resVal]; rw [hn1_pc, hop]; rfl) hr₁]
                exact hr₁
              · simp only [↓reduceIte] at hinsn_at
                simp only [hpc1, hlt_pc, ↓reduceDIte, hinsn_at,
                  checkDst_eq_ok hdst_lt10, hop, hsrc_reg r hop, hmov_false, Bool.false_eq_true, ↓reduceIte]
                rw [stepNextIte_eq_next (by dsimp [st₁', resVal]; rw [hn1_pc, hop]; rfl) hr₁]
                exact hr₁
              · simp only [↓reduceIte] at hinsn_at
                simp only [hpc1, hlt_pc, ↓reduceDIte, hinsn_at,
                  checkDst_eq_ok hdst_lt10, hop, hmov_false, Bool.false_eq_true, ↓reduceIte]
                rw [stepNextIte_eq_next (by dsimp [st₁', resVal]; rw [hn1_pc, hop]; rfl) hr₁]
                exact hr₁
            · unfold runSteps step
              cases is64 <;> rcases hop : operandOfSource src with r | imm
              · simp only [Bool.false_eq_true, ↓reduceIte] at hinsn_at
                simp only [hpc2, hlt_pc, ↓reduceDIte, hinsn_at,
                  checkDst_eq_ok hdst_lt10, hop, hsrc_reg r hop, hmov_false, Bool.false_eq_true, ↓reduceIte]
                rw [stepNextIte_eq_next (by dsimp [st₂', resVal]; rw [hn1_pc, hop]; rfl) hr₂]
                exact hr₂
              · simp only [Bool.false_eq_true, ↓reduceIte] at hinsn_at
                simp only [hpc2, hlt_pc, ↓reduceDIte, hinsn_at,
                  checkDst_eq_ok hdst_lt10, hop, hmov_false, Bool.false_eq_true, ↓reduceIte]
                rw [stepNextIte_eq_next (by dsimp [st₂', resVal]; rw [hn1_pc, hop]; rfl) hr₂]
                exact hr₂
              · simp only [↓reduceIte] at hinsn_at
                simp only [hpc2, hlt_pc, ↓reduceDIte, hinsn_at,
                  checkDst_eq_ok hdst_lt10, hop, hsrc_reg r hop, hmov_false, Bool.false_eq_true, ↓reduceIte]
                rw [stepNextIte_eq_next (by dsimp [st₂', resVal]; rw [hn1_pc, hop]; rfl) hr₂]
                exact hr₂
              · simp only [↓reduceIte] at hinsn_at
                simp only [hpc2, hlt_pc, ↓reduceDIte, hinsn_at,
                  checkDst_eq_ok hdst_lt10, hop, hmov_false, Bool.false_eq_true, ↓reduceIte]
                rw [stepNextIte_eq_next (by dsimp [st₂', resVal]; rw [hn1_pc, hop]; rfl) hr₂]
                exact hr₂

/-! ## 5. Forward & Backward Soundness for `.alu64`, `.alu32`, `.neg`, `.endian`, `.jump`, `.exit` -/

theorem visit_alu64_forward
    {env : ExecEnv} {ctx ctx_out : verifier.ComputationContext}
    {vctx0 vctx1 : verifier.VerificationContext}
    {op : AluOp} {dst : Nat} {src : Operand}
    (hctx : CompCtxValid env vctx0.counter.bv.toNat ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hfwd : VisitorDispatchNonLddw CompCtxVisitorInst ctx vctx0
      (.ok (.Ok (), ctx_out, vctx1)) (.alu64 op dst src)) :
    ctx_out = ctx ∧ ForwardVisitPost env vctx0 vctx1 ctx_out := by
  rcases hfwd with ⟨dst8, vsrc, rfl, rfl, _, hcall⟩
  have hcall' := hcall.symm
  cases op with
  | mov =>
    dsimp [alu64Method, CompCtxVisitorInst] at hcall'
    unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mov64 at hcall'
    rcases bind_eq_ok hcall' with ⟨vc1, hemit, hcall'⟩
    rcases bind_eq_ok hcall' with ⟨r, hsrc, hcall'⟩
    cases r with
    | Err _ => cases Prod.mk.inj (ok_inj hcall') |>.1
    | Ok v =>
      dsimp only at hcall'
      rcases bind_eq_ok hcall' with ⟨r1, hnext, hcall'⟩
      cases r1 with
      | Err _ => cases Prod.mk.inj (ok_inj hcall') |>.1
      | Ok n =>
        dsimp only at hcall'
        rcases bind_eq_ok hcall' with ⟨⟨r2, n1⟩, hset, hcall'⟩
        cases r2 with
        | Err _ => cases Prod.mk.inj (ok_inj hcall') |>.1
        | Ok u0 =>
          cases u0
          rcases bind_eq_ok hcall' with ⟨vc2, hpush, hcall'⟩
          rcases Prod.mk.inj (ok_inj hcall') with ⟨_, hpair⟩
          rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
          obtain ⟨he1, he2, he3, he4, he5, he6⟩ := VerificationContext_emit_log_ok_spec hemit
          obtain ⟨_, hv_valid⟩ := ComputationContext_compute_source_ok_valid hctx hsrc
          obtain ⟨_, hn_eq⟩ := ComputationContext_next_ok_spec hnext
          have hn_valid : CompCtxValid env vctx0.counter.bv.toNat n := by rw [hn_eq]; exact hctx
          obtain ⟨_, _, _, _, _, _, hn1_valid⟩ :=
            ComputationContext_set_reg_ok_spec hn_valid hv_valid hset
          obtain ⟨hp1, hp2, hp3, hp4, hp5, _, hp7⟩ := VerificationContext_push_successor_ok_spec hpush
          have hcnt_eq : vc2.counter = vctx0.counter := hp3.trans he3
          refine ⟨rfl, hp1.trans he1, hp2.trans he2, hp4.trans he4, hp5.trans he5,
            by rw [hcnt_eq], by rwa [hcnt_eq], ?_⟩
          intro opt hopt
          rw [hp7, he6, hpending, List.nil_append, List.mem_singleton] at hopt
          subst hopt
          exact ⟨n1, rfl, by rwa [hcnt_eq]⟩
  | add | sub | mul | div | rem | and | or | xor | lsh | rsh | arsh =>
    dsimp [alu64Method, CompCtxVisitorInst] at hcall'
    first
    | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.add64 at hcall'
    | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.sub64 at hcall'
    | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mul64 at hcall'
    | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.div64 at hcall'
    | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mod64 at hcall'
    | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.and64 at hcall'
    | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.or64 at hcall'
    | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.xor64 at hcall'
    | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.lsh64 at hcall'
    | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.rsh64 at hcall'
    | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.arsh64 at hcall'
    rcases bind_eq_ok hcall' with ⟨⟨r, vc1⟩, halu, hcall'⟩
    rcases Prod.mk.inj (ok_inj hcall') with ⟨rfl, hpair⟩
    rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
    exact ⟨rfl, ComputationContext_alu_forward hctx hpending (by simp) halu⟩

theorem visit_alu64_backward
    {prog : Program} {env : ExecEnv} {vCounter F : Nat}
    {ctx ctx_out : verifier.ComputationContext}
    {vctx0 vctx1 vctx_back vctx_back1 : verifier.VerificationContext}
    {dPost : verifier.DataDependencies} {tracker : verifier.DependencyTracker}
    {op : AluOp} {dst : Nat} {src : Operand}
    (hpc : ∃ hlt : ctx.pc.val < prog.size, prog[ctx.pc.val]'hlt = .alu64 op dst src)
    (hctx : CompCtxValid env vCounter ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hfwd : VisitorDispatchNonLddw CompCtxVisitorInst ctx vctx0
      (.ok (.Ok (), ctx_out, vctx1)) (.alu64 op dst src))
    (hchildren : ∀ succ, some succ ∈ vctx1.pending_successors.val →
      SemTerminatesIn prog env vCounter succ dPost F)
    (hback : VisitorDispatchNonLddw DepTrackerVisitorInst
      { context := ctx_out, deps := dPost } vctx_back
      (.ok (.Ok (), tracker, vctx_back1)) (.alu64 op dst src)) :
    ctx_out = ctx ∧
    tracker.context = ctx ∧
    vctx_back1 = vctx_back ∧
    SemTerminatesIn prog env vCounter ctx tracker.deps (F + 1) := by
  rcases hfwd with ⟨dst8, vsrc, hdst, hsrc_eq, _, hcall⟩
  have hctx_out : ctx_out = ctx := by
    have hc := hcall.symm
    cases op with
    | mov =>
      dsimp [alu64Method, CompCtxVisitorInst] at hc
      unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mov64 at hc
      rcases bind_eq_ok hc with ⟨_, _, hc⟩
      rcases bind_eq_ok hc with ⟨r, _, hc⟩
      cases r with
      | Err _ => cases Prod.mk.inj (ok_inj hc) |>.1
      | Ok _ =>
        dsimp only at hc
        rcases bind_eq_ok hc with ⟨r1, _, hc⟩
        cases r1 with
        | Err _ => cases Prod.mk.inj (ok_inj hc) |>.1
        | Ok _ =>
          dsimp only at hc
          rcases bind_eq_ok hc with ⟨⟨r2, _⟩, _, hc⟩
          cases r2 with
          | Err _ => cases Prod.mk.inj (ok_inj hc) |>.1
          | Ok u0 =>
            cases u0
            rcases bind_eq_ok hc with ⟨_, _, hc⟩
            exact (Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2).1.symm
    | add | sub | mul | div | rem | and | or | xor | lsh | rsh | arsh =>
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
      rcases bind_eq_ok hc with ⟨_, _, hc⟩
      exact (Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2).1.symm
  subst ctx_out
  rcases hback with ⟨dst8_b, vsrc_b, hdst_b, hsrc_b, _, hback_call⟩
  have hdst_u8 : dst8_b = dst8 := UScalar.eq_of_val_eq (hdst_b.symm.trans hdst)
  subst dst8_b
  have hvsrc_eq : vsrc_b = vsrc := by
    have heq := hsrc_b.symm.trans hsrc_eq
    cases vsrc_b <;> cases vsrc <;> simp [operandOfSource] at heq
    · exact congrArg visitor.Source.Reg (UScalar.eq_of_val_eq heq)
    · exact congrArg visitor.Source.Value ((u64_eq_iff_bv _ _).mpr heq)
  subst vsrc_b
  subst dst; subst src
  have hcall' := hcall.symm
  have hback' := hback_call.symm
  cases op with
  | mov =>
    dsimp [alu64Method, CompCtxVisitorInst] at hcall'
    dsimp [alu64Method, DepTrackerVisitorInst] at hback'
    unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mov64 at hcall'
    unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mov64 at hback'
    obtain ⟨_, htrk_ctx, hv_back, hreg_deps, hstk_deps, hsrc_live⟩ :=
      DependencyTracker_mov_ok_spec hback'
    rcases bind_eq_ok hcall' with ⟨vc1, hemit, hcall'⟩
    rcases bind_eq_ok hcall' with ⟨r, hsrc, hcall'⟩
    cases r with
    | Err _ => cases Prod.mk.inj (ok_inj hcall') |>.1
    | Ok v =>
      dsimp only at hcall'
      rcases bind_eq_ok hcall' with ⟨r1, hnext, hcall'⟩
      cases r1 with
      | Err _ => cases Prod.mk.inj (ok_inj hcall') |>.1
      | Ok n =>
        dsimp only at hcall'
        rcases bind_eq_ok hcall' with ⟨⟨r2, n1⟩, hset, hcall'⟩
        cases r2 with
        | Err _ => cases Prod.mk.inj (ok_inj hcall') |>.1
        | Ok u0 =>
          cases u0
          rcases bind_eq_ok hcall' with ⟨vc2, hpush, hcall'⟩
          rcases Prod.mk.inj (ok_inj hcall') with ⟨_, hpair⟩
          rcases Prod.mk.inj hpair with ⟨_, rfl⟩
          obtain ⟨_, _, _, _, _, he6⟩ := VerificationContext_emit_log_ok_spec hemit
          obtain ⟨hsrc_reg, hv_valid⟩ := ComputationContext_compute_source_ok_valid hctx hsrc
          obtain ⟨hpc_next, hn_eq⟩ := ComputationContext_next_ok_spec hnext
          have hn_valid : CompCtxValid env vCounter n := by rw [hn_eq]; exact hctx
          obtain ⟨hdst_lt10, hpc_n1, hregs_n1, hstk_n1, hbm_n1, hres_n1, _⟩ :=
            ComputationContext_set_reg_ok_spec hn_valid hv_valid hset
          obtain ⟨_, _, _, _, _, _, hp7⟩ := VerificationContext_push_successor_ok_spec hpush
          have hn1_mem : some n1 ∈ vc2.pending_successors.val := by
            rw [hp7, he6, hpending]; simp
          have hsem := hchildren n1 hn1_mem
          obtain ⟨hlt_pc, hinsn_at⟩ := hpc
          refine ⟨rfl, htrk_ctx, hv_back, ?_⟩
          intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
          let st₁' : MachineState :=
            { st₁ with pc := n1.pc.val,
                       regs := updateReg st₁.regs dst8.val (evalOperand st₁ (operandOfSource vsrc)),
                       world := bumpWorldStep st₁.world }
          let st₂' : MachineState :=
            { st₂ with pc := n1.pc.val,
                       regs := updateReg st₂.regs dst8.val (evalOperand st₂ (operandOfSource vsrc)),
                       world := bumpWorldStep st₂.world }
          have hrel_post : StateRel2Deps env n1 dPost ρ resActive isNullId st₁' st₂' := by
            refine StateRel2Deps_set_reg (by unfold GENERAL_REGISTER_COUNT at hdst_lt10; exact hdst_lt10)
              (hregs_n1.trans (by rw [hn_eq]))
              (hstk_n1.trans (by rw [hn_eq]))
              (hbm_n1.trans (by rw [hn_eq]))
              (hres_n1.trans (by rw [hn_eq]))
              hreg_deps hstk_deps hrel
              (fun hdst_live => ?_)
            have hsrc_live_pre : ∀ r, vsrc = .Reg r → r.val = 10 ∨ DataDepsHasReg tracker.deps r.val = true :=
              fun r hr => Or.inr (hsrc_live hdst_live r hr)
            exact ComputationContext_compute_source_evalOperand_rel2 hctx hrel hsrc_live_pre hsrc
          have halloc' : (bumpWorldStep st₁.world).nextAllocId + 3 * F < 2 ^ 63 := by
            simp only [bumpWorldStep]; omega
          obtain ⟨ret, finalWorld, hr₁, hr₂, hlive_fin⟩ :=
            hsem l₁ l₂ s₁ s₂ ρ resActive isNullId st₁' st₂' hl₁ hl₂ hleak halloc' hρ hrel_post
          have hpc1 := hrel.1
          have hpc2 := hrel.2.1
          have hn1_pc : n1.pc.val = ctx.pc.val + 1 := by rw [hpc_n1, hpc_next]
          refine ⟨ret, finalWorld, ?_, ?_, hlive_fin⟩
          · unfold runSteps step
            rcases hop : operandOfSource vsrc with r | imm
            · simp only [hpc1, hlt_pc, ↓reduceDIte, hinsn_at,
                checkDst_eq_ok hdst_lt10, hop, hsrc_reg r hop, beq_self_eq_true, ↓reduceIte]
              rw [stepNextIte_eq_next (by dsimp [st₁']; rw [hn1_pc, hop]; rfl) hr₁]
              exact hr₁
            · simp only [hpc1, hlt_pc, ↓reduceDIte, hinsn_at,
                checkDst_eq_ok hdst_lt10, hop, beq_self_eq_true, ↓reduceIte]
              rw [stepNextIte_eq_next (by dsimp [st₁']; rw [hn1_pc, hop]; rfl) hr₁]
              exact hr₁
          · unfold runSteps step
            rcases hop : operandOfSource vsrc with r | imm
            · simp only [hpc2, hlt_pc, ↓reduceDIte, hinsn_at,
                checkDst_eq_ok hdst_lt10, hop, hsrc_reg r hop, beq_self_eq_true, ↓reduceIte]
              rw [stepNextIte_eq_next (by dsimp [st₂']; rw [hn1_pc, hop]; rfl) hr₂]
              exact hr₂
            · simp only [hpc2, hlt_pc, ↓reduceDIte, hinsn_at,
                checkDst_eq_ok hdst_lt10, hop, beq_self_eq_true, ↓reduceIte]
              rw [stepNextIte_eq_next (by dsimp [st₂']; rw [hn1_pc, hop]; rfl) hr₂]
              exact hr₂
  | add | sub | mul | div | rem | and | or | xor | lsh | rsh | arsh =>
    dsimp [alu64Method, CompCtxVisitorInst] at hcall'
    dsimp [alu64Method, DepTrackerVisitorInst] at hback'
    first
    | (unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.add64 at hcall';
       unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.add64 at hback')
    | (unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.sub64 at hcall';
       unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.sub64 at hback')
    | (unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mul64 at hcall';
       unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mul64 at hback')
    | (unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.div64 at hcall';
       unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.div64 at hback')
    | (unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mod64 at hcall';
       unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mod64 at hback')
    | (unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.and64 at hcall';
       unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.and64 at hback')
    | (unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.or64 at hcall';
       unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.or64 at hback')
    | (unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.xor64 at hcall';
       unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.xor64 at hback')
    | (unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.lsh64 at hcall';
       unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.lsh64 at hback')
    | (unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.rsh64 at hcall';
       unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.rsh64 at hback')
    | (unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.arsh64 at hcall';
       unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.arsh64 at hback')
    rcases bind_eq_ok hcall' with ⟨⟨rf, vc1⟩, halu, hcall'⟩
    rcases Prod.mk.inj (ok_inj hcall') with ⟨rfl, hpair⟩
    rcases Prod.mk.inj hpair with ⟨_, rfl⟩
    rcases bind_eq_ok hback' with ⟨⟨rb, dd⟩, hd_alu, hback'⟩
    rcases Prod.mk.inj (ok_inj hback') with ⟨rfl, hpair_b⟩
    rcases Prod.mk.inj hpair_b with ⟨rfl, rfl⟩
    refine ⟨rfl, rfl, rfl, ?_⟩
    exact ComputationContext_alu_backward (is64 := true) (by decide)
      (by constructor) hpc hctx hpending halu hchildren hd_alu

theorem visit_alu32_forward
    {env : ExecEnv} {ctx ctx_out : verifier.ComputationContext}
    {vctx0 vctx1 : verifier.VerificationContext}
    {op : AluOp} {dst : Nat} {src : Operand}
    (hctx : CompCtxValid env vctx0.counter.bv.toNat ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hfwd : VisitorDispatchNonLddw CompCtxVisitorInst ctx vctx0
      (.ok (.Ok (), ctx_out, vctx1)) (.alu32 op dst src)) :
    ctx_out = ctx ∧ ForwardVisitPost env vctx0 vctx1 ctx_out := by
  rcases hfwd with ⟨dst8, vsrc, rfl, rfl, _, hcall⟩
  have hcall' := hcall.symm
  cases op with
  | mov =>
    dsimp [alu32Method, CompCtxVisitorInst] at hcall'
    unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mov at hcall'
    rcases bind_eq_ok hcall' with ⟨vc1, hemit, hcall'⟩
    rcases bind_eq_ok hcall' with ⟨r, hsrc, hcall'⟩
    cases r with
    | Err _ => cases Prod.mk.inj (ok_inj hcall') |>.1
    | Ok v =>
      dsimp only at hcall'
      rcases bind_eq_ok hcall' with ⟨value, hval, hcall'⟩
      rcases bind_eq_ok hcall' with ⟨r1, hnext, hcall'⟩
      cases r1 with
      | Err _ => cases Prod.mk.inj (ok_inj hcall') |>.1
      | Ok n =>
        dsimp only at hcall'
        rcases bind_eq_ok hcall' with ⟨⟨r2, n1⟩, hset, hcall'⟩
        cases r2 with
        | Err _ => cases Prod.mk.inj (ok_inj hcall') |>.1
        | Ok u0 =>
          cases u0
          rcases bind_eq_ok hcall' with ⟨vc2, hpush, hcall'⟩
          rcases Prod.mk.inj (ok_inj hcall') with ⟨_, hpair⟩
          rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
          obtain ⟨he1, he2, he3, he4, he5, he6⟩ := VerificationContext_emit_log_ok_spec hemit
          obtain ⟨_, hv_valid⟩ := ComputationContext_compute_source_ok_valid hctx hsrc
          have hvalue_valid : TypeValid env vctx0.counter.bv.toNat value := by
            cases v with
            | ScalarValue data => exact mov32_scalar_branch_valid hv_valid hval
            | _ =>
              rw [Type_UNINITIALIZED_eq] at hval
              cases ok_inj hval
              exact uninitScalar64_valid
          obtain ⟨_, hn_eq⟩ := ComputationContext_next_ok_spec hnext
          have hn_valid : CompCtxValid env vctx0.counter.bv.toNat n := by rw [hn_eq]; exact hctx
          obtain ⟨_, _, _, _, _, _, hn1_valid⟩ :=
            ComputationContext_set_reg_ok_spec hn_valid hvalue_valid hset
          obtain ⟨hp1, hp2, hp3, hp4, hp5, _, hp7⟩ := VerificationContext_push_successor_ok_spec hpush
          have hcnt_eq : vc2.counter = vctx0.counter := hp3.trans he3
          refine ⟨rfl, hp1.trans he1, hp2.trans he2, hp4.trans he4, hp5.trans he5,
            by rw [hcnt_eq], by rwa [hcnt_eq], ?_⟩
          intro opt hopt
          rw [hp7, he6, hpending, List.nil_append, List.mem_singleton] at hopt
          subst hopt
          exact ⟨n1, rfl, by rwa [hcnt_eq]⟩
  | add | sub | mul | div | rem | and | or | xor | lsh | rsh | arsh =>
    dsimp [alu32Method, CompCtxVisitorInst] at hcall'
    first
    | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.add at hcall'
    | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.sub at hcall'
    | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mul at hcall'
    | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.div at hcall'
    | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mod at hcall'
    | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.and at hcall'
    | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.or at hcall'
    | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.xor at hcall'
    | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.lsh at hcall'
    | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.rsh at hcall'
    | unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.arsh at hcall'
    rcases bind_eq_ok hcall' with ⟨⟨r, vc1⟩, halu, hcall'⟩
    rcases Prod.mk.inj (ok_inj hcall') with ⟨rfl, hpair⟩
    rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
    exact ⟨rfl, ComputationContext_alu_forward hctx hpending (by simp) halu⟩

theorem visit_alu32_backward
    {prog : Program} {env : ExecEnv} {vCounter F : Nat}
    {ctx ctx_out : verifier.ComputationContext}
    {vctx0 vctx1 vctx_back vctx_back1 : verifier.VerificationContext}
    {dPost : verifier.DataDependencies} {tracker : verifier.DependencyTracker}
    {op : AluOp} {dst : Nat} {src : Operand}
    (hpc : ∃ hlt : ctx.pc.val < prog.size, prog[ctx.pc.val]'hlt = .alu32 op dst src)
    (hctx : CompCtxValid env vCounter ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hfwd : VisitorDispatchNonLddw CompCtxVisitorInst ctx vctx0
      (.ok (.Ok (), ctx_out, vctx1)) (.alu32 op dst src))
    (hchildren : ∀ succ, some succ ∈ vctx1.pending_successors.val →
      SemTerminatesIn prog env vCounter succ dPost F)
    (hback : VisitorDispatchNonLddw DepTrackerVisitorInst
      { context := ctx_out, deps := dPost } vctx_back
      (.ok (.Ok (), tracker, vctx_back1)) (.alu32 op dst src)) :
    ctx_out = ctx ∧
    tracker.context = ctx ∧
    vctx_back1 = vctx_back ∧
    SemTerminatesIn prog env vCounter ctx tracker.deps (F + 1) := by
  rcases hfwd with ⟨dst8, vsrc, hdst, hsrc_eq, _, hcall⟩
  have hctx_out : ctx_out = ctx := by
    have hc := hcall.symm
    cases op with
    | mov =>
      dsimp [alu32Method, CompCtxVisitorInst] at hc
      unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mov at hc
      rcases bind_eq_ok hc with ⟨_, _, hc⟩
      rcases bind_eq_ok hc with ⟨r, _, hc⟩
      cases r with
      | Err _ => cases Prod.mk.inj (ok_inj hc) |>.1
      | Ok _ =>
        dsimp only at hc
        rcases bind_eq_ok hc with ⟨_, _, hc⟩
        rcases bind_eq_ok hc with ⟨r1, _, hc⟩
        cases r1 with
        | Err _ => cases Prod.mk.inj (ok_inj hc) |>.1
        | Ok _ =>
          dsimp only at hc
          rcases bind_eq_ok hc with ⟨⟨r2, _⟩, _, hc⟩
          cases r2 with
          | Err _ => cases Prod.mk.inj (ok_inj hc) |>.1
          | Ok u0 =>
            cases u0
            rcases bind_eq_ok hc with ⟨_, _, hc⟩
            exact (Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2).1.symm
    | add | sub | mul | div | rem | and | or | xor | lsh | rsh | arsh =>
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
      rcases bind_eq_ok hc with ⟨_, _, hc⟩
      exact (Prod.mk.inj (Prod.mk.inj (ok_inj hc)).2).1.symm
  subst ctx_out
  rcases hback with ⟨dst8_b, vsrc_b, hdst_b, hsrc_b, _, hback_call⟩
  have hdst_u8 : dst8_b = dst8 := UScalar.eq_of_val_eq (hdst_b.symm.trans hdst)
  subst dst8_b
  have hvsrc_eq : vsrc_b = vsrc := by
    have heq := hsrc_b.symm.trans hsrc_eq
    cases vsrc_b <;> cases vsrc <;> simp [operandOfSource] at heq
    · exact congrArg visitor.Source.Reg (UScalar.eq_of_val_eq heq)
    · exact congrArg visitor.Source.Value ((u64_eq_iff_bv _ _).mpr heq)
  subst vsrc_b
  subst dst; subst src
  have hcall' := hcall.symm
  have hback' := hback_call.symm
  cases op with
  | mov =>
    dsimp [alu32Method, CompCtxVisitorInst] at hcall'
    dsimp [alu32Method, DepTrackerVisitorInst] at hback'
    unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mov at hcall'
    obtain ⟨_, htrk_ctx, hv_back, hreg_deps, hstk_deps, hsrc_live⟩ :=
      DependencyTracker_mov_ok_spec hback'
    rcases bind_eq_ok hcall' with ⟨vc1, hemit, hcall'⟩
    rcases bind_eq_ok hcall' with ⟨r, hsrc, hcall'⟩
    cases r with
    | Err _ => cases Prod.mk.inj (ok_inj hcall') |>.1
    | Ok v =>
      dsimp only at hcall'
      rcases bind_eq_ok hcall' with ⟨value, hval, hcall'⟩
      rcases bind_eq_ok hcall' with ⟨r1, hnext, hcall'⟩
      cases r1 with
      | Err _ => cases Prod.mk.inj (ok_inj hcall') |>.1
      | Ok n =>
        dsimp only at hcall'
        rcases bind_eq_ok hcall' with ⟨⟨r2, n1⟩, hset, hcall'⟩
        cases r2 with
        | Err _ => cases Prod.mk.inj (ok_inj hcall') |>.1
        | Ok u0 =>
          cases u0
          rcases bind_eq_ok hcall' with ⟨vc2, hpush, hcall'⟩
          rcases Prod.mk.inj (ok_inj hcall') with ⟨_, hpair⟩
          rcases Prod.mk.inj hpair with ⟨_, rfl⟩
          obtain ⟨_, _, _, _, _, he6⟩ := VerificationContext_emit_log_ok_spec hemit
          obtain ⟨hsrc_reg, hv_valid⟩ := ComputationContext_compute_source_ok_valid hctx hsrc
          have hvalue_valid : TypeValid env vCounter value := by
            cases v with
            | ScalarValue data => exact mov32_scalar_branch_valid hv_valid hval
            | _ =>
              rw [Type_UNINITIALIZED_eq] at hval
              cases ok_inj hval
              exact uninitScalar64_valid
          obtain ⟨hpc_next, hn_eq⟩ := ComputationContext_next_ok_spec hnext
          have hn_valid : CompCtxValid env vCounter n := by rw [hn_eq]; exact hctx
          obtain ⟨hdst_lt10, hpc_n1, hregs_n1, hstk_n1, hbm_n1, hres_n1, _⟩ :=
            ComputationContext_set_reg_ok_spec hn_valid hvalue_valid hset
          obtain ⟨_, _, _, _, _, _, hp7⟩ := VerificationContext_push_successor_ok_spec hpush
          have hn1_mem : some n1 ∈ vc2.pending_successors.val := by
            rw [hp7, he6, hpending]; simp
          have hsem := hchildren n1 hn1_mem
          obtain ⟨hlt_pc, hinsn_at⟩ := hpc
          refine ⟨rfl, htrk_ctx, hv_back, ?_⟩
          intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
          let resVal (l : HostLayout) (st : MachineState) : RuntimeVal :=
            .scalar (evalAlu32 .mov 0#64
              ((evalOperand st (operandOfSource vsrc)).toBits l st.world.arrayLen))
          let st₁' : MachineState :=
            { st₁ with pc := n1.pc.val, regs := updateReg st₁.regs dst8.val (resVal l₁ st₁),
                       world := bumpWorldStep st₁.world }
          let st₂' : MachineState :=
            { st₂ with pc := n1.pc.val, regs := updateReg st₂.regs dst8.val (resVal l₂ st₂),
                       world := bumpWorldStep st₂.world }
          have hrel_post : StateRel2Deps env n1 dPost ρ resActive isNullId st₁' st₂' := by
            refine StateRel2Deps_set_reg (by unfold GENERAL_REGISTER_COUNT at hdst_lt10; exact hdst_lt10)
              (hregs_n1.trans (by rw [hn_eq]))
              (hstk_n1.trans (by rw [hn_eq]))
              (hbm_n1.trans (by rw [hn_eq]))
              (hres_n1.trans (by rw [hn_eq]))
              hreg_deps hstk_deps hrel
              (fun hdst_live => ?_)
            have hsrc_live_pre : ∀ r, vsrc = .Reg r → r.val = 10 ∨ DataDepsHasReg tracker.deps r.val = true :=
              fun r hr => Or.inr (hsrc_live hdst_live r hr)
            have hrel_src :=
              ComputationContext_compute_source_evalOperand_rel2 hctx hrel hsrc_live_pre hsrc
            cases v with
            | ScalarValue data =>
              simpa only [resVal, hrel.2.2.1] using
                mov32_scalar_branch_rel2 (layout₁ := l₁) (layout₂ := l₂) hrel_src hval
            | _ =>
              rw [Type_UNINITIALIZED_eq] at hval
              cases ok_inj hval
              simp [TypeRel2]
          have halloc' : (bumpWorldStep st₁.world).nextAllocId + 3 * F < 2 ^ 63 := by
            simp only [bumpWorldStep]; omega
          obtain ⟨ret, finalWorld, hr₁, hr₂, hlive_fin⟩ :=
            hsem l₁ l₂ s₁ s₂ ρ resActive isNullId st₁' st₂' hl₁ hl₂ hleak halloc' hρ hrel_post
          have hpc1 := hrel.1
          have hpc2 := hrel.2.1
          have hn1_pc : n1.pc.val = ctx.pc.val + 1 := by rw [hpc_n1, hpc_next]
          refine ⟨ret, finalWorld, ?_, ?_, hlive_fin⟩
          · unfold runSteps step
            rcases hop : operandOfSource vsrc with r | imm
            · simp only [hpc1, hlt_pc, ↓reduceDIte, hinsn_at,
                checkDst_eq_ok hdst_lt10, hop, hsrc_reg r hop, beq_self_eq_true, ↓reduceIte]
              rw [stepNextIte_eq_next (by dsimp [st₁', resVal]; rw [hn1_pc, hop]; rfl) hr₁]
              exact hr₁
            · simp only [hpc1, hlt_pc, ↓reduceDIte, hinsn_at,
                checkDst_eq_ok hdst_lt10, hop, beq_self_eq_true, ↓reduceIte]
              rw [stepNextIte_eq_next (by dsimp [st₁', resVal]; rw [hn1_pc, hop]; rfl) hr₁]
              exact hr₁
          · unfold runSteps step
            rcases hop : operandOfSource vsrc with r | imm
            · simp only [hpc2, hlt_pc, ↓reduceDIte, hinsn_at,
                checkDst_eq_ok hdst_lt10, hop, hsrc_reg r hop, beq_self_eq_true, ↓reduceIte]
              rw [stepNextIte_eq_next (by dsimp [st₂', resVal]; rw [hn1_pc, hop]; rfl) hr₂]
              exact hr₂
            · simp only [hpc2, hlt_pc, ↓reduceDIte, hinsn_at,
                checkDst_eq_ok hdst_lt10, hop, beq_self_eq_true, ↓reduceIte]
              rw [stepNextIte_eq_next (by dsimp [st₂', resVal]; rw [hn1_pc, hop]; rfl) hr₂]
              exact hr₂
  | add | sub | mul | div | rem | and | or | xor | lsh | rsh | arsh =>
    dsimp [alu32Method, CompCtxVisitorInst] at hcall'
    dsimp [alu32Method, DepTrackerVisitorInst] at hback'
    first
    | (unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.add at hcall';
       unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.add at hback')
    | (unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.sub at hcall';
       unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.sub at hback')
    | (unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mul at hcall';
       unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mul at hback')
    | (unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.div at hcall';
       unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.div at hback')
    | (unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mod at hcall';
       unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.mod at hback')
    | (unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.and at hcall';
       unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.and at hback')
    | (unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.or at hcall';
       unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.or at hback')
    | (unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.xor at hcall';
       unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.xor at hback')
    | (unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.lsh at hcall';
       unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.lsh at hback')
    | (unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.rsh at hcall';
       unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.rsh at hback')
    | (unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.arsh at hcall';
       unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.arsh at hback')
    rcases bind_eq_ok hcall' with ⟨⟨rf, vc1⟩, halu, hcall'⟩
    rcases Prod.mk.inj (ok_inj hcall') with ⟨rfl, hpair⟩
    rcases Prod.mk.inj hpair with ⟨_, rfl⟩
    rcases bind_eq_ok hback' with ⟨⟨rb, dd⟩, hd_alu, hback'⟩
    rcases Prod.mk.inj (ok_inj hback') with ⟨rfl, hpair_b⟩
    rcases Prod.mk.inj hpair_b with ⟨rfl, rfl⟩
    refine ⟨rfl, rfl, rfl, ?_⟩
    exact ComputationContext_alu_backward (is64 := false) (by decide)
      (by constructor) hpc hctx hpending halu hchildren hd_alu

theorem visit_neg_forward
    {env : ExecEnv} {ctx ctx_out : verifier.ComputationContext}
    {vctx0 vctx1 : verifier.VerificationContext}
    {is64 : Bool} {dst : Nat}
    (hctx : CompCtxValid env vctx0.counter.bv.toNat ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hfwd : VisitorDispatchNonLddw CompCtxVisitorInst ctx vctx0
      (.ok (.Ok (), ctx_out, vctx1)) (.neg is64 dst)) :
    ctx_out = ctx ∧ ForwardVisitPost env vctx0 vctx1 ctx_out := by
  rcases hfwd with ⟨dst8, rfl, hcall⟩
  have hcall' := hcall.symm
  cases is64 <;> {
    dsimp [CompCtxVisitorInst] at hcall'
    try unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.neg64 at hcall'
    try unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.neg at hcall'
    rcases bind_eq_ok hcall' with ⟨vc1, hemit, hcall'⟩
    rcases bind_eq_ok hcall' with ⟨⟨r, vc2⟩, halu, hcall'⟩
    rcases Prod.mk.inj (ok_inj hcall') with ⟨rfl, hpair⟩
    rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
    obtain ⟨he1, he2, he3, he4, he5, he6⟩ := VerificationContext_emit_log_ok_spec hemit
    have hctx1 : CompCtxValid env vc1.counter.bv.toNat ctx := by rwa [he3]
    have hpend1 : vc1.pending_successors.val = [] := by rwa [he6]
    have hpost1 := ComputationContext_alu_forward hctx1 hpend1 (by simp) halu
    refine ⟨rfl, hpost1.1.trans he1, hpost1.2.1.trans he2, hpost1.2.2.1.trans he4,
      hpost1.2.2.2.1.trans he5, ?_, hpost1.2.2.2.2.2.1, hpost1.2.2.2.2.2.2⟩
    rw [← he3]
    exact hpost1.2.2.2.2.1
  }

theorem visit_neg_backward
    {prog : Program} {env : ExecEnv} {vCounter F : Nat}
    {ctx ctx_out : verifier.ComputationContext}
    {vctx0 vctx1 vctx_back vctx_back1 : verifier.VerificationContext}
    {dPost : verifier.DataDependencies} {tracker : verifier.DependencyTracker}
    {is64 : Bool} {dst : Nat}
    (hpc : ∃ hlt : ctx.pc.val < prog.size, prog[ctx.pc.val]'hlt = .neg is64 dst)
    (hctx : CompCtxValid env vCounter ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hfwd : VisitorDispatchNonLddw CompCtxVisitorInst ctx vctx0
      (.ok (.Ok (), ctx_out, vctx1)) (.neg is64 dst))
    (hchildren : ∀ succ, some succ ∈ vctx1.pending_successors.val →
      SemTerminatesIn prog env vCounter succ dPost F)
    (hback : VisitorDispatchNonLddw DepTrackerVisitorInst
      { context := ctx_out, deps := dPost } vctx_back
      (.ok (.Ok (), tracker, vctx_back1)) (.neg is64 dst)) :
    ctx_out = ctx ∧
    tracker.context = ctx ∧
    vctx_back1 = vctx_back ∧
    SemTerminatesIn prog env vCounter ctx tracker.deps (F + 1) := by
  rcases hfwd with ⟨dst8, hdst, hcall⟩
  have hcall' := hcall.symm
  have hneg_unpack :
      ctx_out = ctx ∧
      ∃ vc1,
        verifier.VerificationContext.emit_log vctx0 ctx.pc
          (.Neg (!is64) dst8) = ok vc1 ∧
        verifier.ComputationContext.alu ctx none vc1 dst8
          (.Value 0#u64) .Plain (if is64 then .Neg64 else .Neg32) = ok (.Ok (), vctx1) := by
    cases is64
    · dsimp [CompCtxVisitorInst] at hcall' ⊢
      unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.neg at hcall'
      rcases bind_eq_ok hcall' with ⟨vc1, hemit, hcall'⟩
      rcases bind_eq_ok hcall' with ⟨⟨r, vc2⟩, halu, hcall'⟩
      rcases Prod.mk.inj (ok_inj hcall') with ⟨rfl, hpair⟩
      rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
      exact ⟨rfl, vc1, hemit, halu⟩
    · dsimp [CompCtxVisitorInst] at hcall' ⊢
      unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.neg64 at hcall'
      rcases bind_eq_ok hcall' with ⟨vc1, hemit, hcall'⟩
      rcases bind_eq_ok hcall' with ⟨⟨r, vc2⟩, halu, hcall'⟩
      rcases Prod.mk.inj (ok_inj hcall') with ⟨rfl, hpair⟩
      rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
      exact ⟨rfl, vc1, hemit, halu⟩
  rcases hneg_unpack with ⟨hctx_eq, vc1, hemit, halu⟩
  subst ctx_out
  rcases hback with ⟨dst8_b, hdst_b, hback_call⟩
  have hback' := hback_call.symm
  have hback_eq : tracker = { context := ctx, deps := dPost } ∧ vctx_back1 = vctx_back := by
    cases is64 <;> {
      dsimp [DepTrackerVisitorInst] at hback'
      try unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.neg64 at hback'
      try unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.neg at hback'
      rcases Prod.mk.inj (Prod.mk.inj (ok_inj hback')).2 with ⟨htrk, hvb⟩
      exact ⟨htrk.symm, hvb.symm⟩
    }
  rcases hback_eq with ⟨htrk_eq, hvb_eq⟩
  subst tracker; subst vctx_back1
  subst dst
  unfold verifier.ComputationContext.alu at halu
  simp only [bind_ok] at halu
  rcases bind_eq_ok halu with ⟨r, hreg, halu⟩
  cases r with
  | Err _ => cases Prod.mk.inj (ok_inj halu) |>.1
  | Ok v =>
    dsimp only at halu
    rcases bind_eq_ok halu with ⟨r1, hsrc, halu⟩
    cases r1 with
    | Err _ => cases Prod.mk.inj (ok_inj halu) |>.1
    | Ok v1 =>
      dsimp only at halu
      rcases bind_eq_ok halu with ⟨r2, happ, halu⟩
      cases r2 with
      | Err _ => cases Prod.mk.inj (ok_inj halu) |>.1
      | Ok v2 =>
        dsimp only at halu
        rcases bind_eq_ok halu with ⟨r3, hnext, halu⟩
        cases r3 with
        | Err _ => cases Prod.mk.inj (ok_inj halu) |>.1
        | Ok n =>
          dsimp only at halu
          rcases bind_eq_ok halu with ⟨⟨r4, n1⟩, hset, halu⟩
          cases r4 with
          | Err _ => cases Prod.mk.inj (ok_inj halu) |>.1
          | Ok u0 =>
            cases u0
            rcases bind_eq_ok halu with ⟨vc2, hpush, halu⟩
            rcases Prod.mk.inj (ok_inj halu) with ⟨_, rfl⟩
            obtain ⟨_, _, _, _, _, he6⟩ := VerificationContext_emit_log_ok_spec hemit
            obtain ⟨_, hv_valid⟩ := ComputationContext_reg_ok_valid hctx hreg
            obtain ⟨_, hv1_valid⟩ := ComputationContext_compute_source_ok_valid hctx hsrc
            have hv2_valid := apply_computation_ok_valid hctx hv_valid hv1_valid (by simp) happ
            obtain ⟨hpc_next, hn_eq⟩ := ComputationContext_next_ok_spec hnext
            have hn_valid : CompCtxValid env vCounter n := by rw [hn_eq]; exact hctx
            obtain ⟨hdst_lt10, hpc_n1, hregs_n1, hstk_n1, hbm_n1, hres_n1, _⟩ :=
              ComputationContext_set_reg_ok_spec hn_valid hv2_valid hset
            obtain ⟨_, _, _, _, _, _, hp7⟩ := VerificationContext_push_successor_ok_spec hpush
            have hn1_mem : some n1 ∈ vc2.pending_successors.val := by
              rw [hp7, he6, hpending]; simp
            have hsem := hchildren n1 hn1_mem
            obtain ⟨hlt_pc, hinsn_at⟩ := hpc
            refine ⟨rfl, rfl, rfl, ?_⟩
            intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
            let resVal (l : HostLayout) (st : MachineState) : RuntimeVal :=
              .scalar (evalNeg is64 ((st.regs dst8.val).toBits l st.world.arrayLen))
            let st₁' : MachineState :=
              { st₁ with pc := n1.pc.val, regs := updateReg st₁.regs dst8.val (resVal l₁ st₁),
                         world := bumpWorldStep st₁.world }
            let st₂' : MachineState :=
              { st₂ with pc := n1.pc.val, regs := updateReg st₂.regs dst8.val (resVal l₂ st₂),
                         world := bumpWorldStep st₂.world }
            have hrel_post : StateRel2Deps env n1 dPost ρ resActive isNullId st₁' st₂' := by
              refine StateRel2Deps_set_reg (by unfold GENERAL_REGISTER_COUNT at hdst_lt10; exact hdst_lt10)
                (hregs_n1.trans (by rw [hn_eq]))
                (hstk_n1.trans (by rw [hn_eq]))
                (hbm_n1.trans (by rw [hn_eq]))
                (hres_n1.trans (by rw [hn_eq]))
                (fun _ _ _ hd => hd)
                (fun _ _ hd => hd)
                hrel
                (fun hdst_live => ?_)
              obtain ⟨_, _, hrel_dst⟩ :=
                ComputationContext_reg_ok_valid_and_rel2 hctx hrel (Or.inr hdst_live) hreg
              have hrel_res := (apply_computation_neg_ok_valid_and_rel2
                (layout₁ := l₁) (layout₂ := l₂)
                hctx hv_valid hv1_valid hrel_dst happ).2
              simpa only [resVal, hrel.2.2.1] using hrel_res
            have halloc' : (bumpWorldStep st₁.world).nextAllocId + 3 * F < 2 ^ 63 := by
              simp only [bumpWorldStep]; omega
            obtain ⟨ret, finalWorld, hr₁, hr₂, hlive_fin⟩ :=
              hsem l₁ l₂ s₁ s₂ ρ resActive isNullId st₁' st₂' hl₁ hl₂ hleak halloc' hρ hrel_post
            have hn1_pc : n1.pc.val = ctx.pc.val + 1 := by rw [hpc_n1, hpc_next]
            refine ⟨ret, finalWorld, ?_, ?_, hlive_fin⟩
            · unfold runSteps step
              simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, checkDst_eq_ok hdst_lt10]
              rw [stepNextIte_eq_next (by dsimp [st₁', resVal]; rw [hn1_pc]) hr₁]
              exact hr₁
            · unfold runSteps step
              simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, checkDst_eq_ok hdst_lt10]
              rw [stepNextIte_eq_next (by dsimp [st₂', resVal]; rw [hn1_pc]) hr₂]
              exact hr₂

private theorem endianness_ptr_leak_arm_unpack
    {self : verifier.ComputationContext}
    {vc1 vctx_out : verifier.VerificationContext} {dst : Std.U8}
    (h : (if vc1.calling_context.allow_ptr_leaks then
            do
              let new_value ← verifier.Type.UNKNOWN_SCALAR
              let r2 ← verifier.ComputationContext.next self
              match r2 with
              | core.result.Result.Ok n =>
                let (r3, n1) ← verifier.ComputationContext.set_reg n dst new_value
                match r3 with
                | core.result.Result.Ok _ =>
                  let vc2 ← verifier.VerificationContext.push_successor vc1 n1
                  ok (core.result.Result.Ok (), vc2)
                | core.result.Result.Err _ => ok (r3, vc1)
              | core.result.Result.Err err => ok (core.result.Result.Err err, vc1)
          else
            ok (core.result.Result.Err error.VerifierErrorKind.InvalidByteSwapTarget, vc1)) =
        ok (.Ok (), vctx_out)) :
    vc1.calling_context.allow_ptr_leaks = true ∧
    ∃ n n1,
      verifier.ComputationContext.next self = ok (.Ok n) ∧
      verifier.ComputationContext.set_reg n dst (.ScalarValue unknownWrittenScalar64) =
        ok (.Ok (), n1) ∧
      verifier.VerificationContext.push_successor vc1 n1 = ok vctx_out := by
  by_cases hleak : vc1.calling_context.allow_ptr_leaks = true
  · simp only [hleak, ↓reduceIte, Type_UNKNOWN_SCALAR_eq, bind_ok] at h
    rcases bind_eq_ok h with ⟨r2, hnext, h⟩
    cases r2 with
    | Err _ => cases Prod.mk.inj (ok_inj h) |>.1
    | Ok n =>
      dsimp only at h
      rcases bind_eq_ok h with ⟨⟨r3, n1⟩, hset, h⟩
      cases r3 with
      | Err _ => cases Prod.mk.inj (ok_inj h) |>.1
      | Ok u0 =>
        cases u0
        rcases bind_eq_ok h with ⟨vc2, hpush, h⟩
        rcases Prod.mk.inj (ok_inj h) with ⟨_, rfl⟩
        exact ⟨hleak, n, n1, hnext, hset, hpush⟩
  · simp only [hleak, Bool.false_eq_true, ↓reduceIte] at h
    cases Prod.mk.inj (ok_inj h) |>.1

private theorem endianness_ok_unpack
    {self : verifier.ComputationContext}
    {doSwap : Bool} {ew : EndianWidth}
    {vctx0 vctx1 : verifier.VerificationContext} {dst : Std.U8}
    (h : verifier.ComputationContext.endianness self doSwap vctx0 dst (coreWidthOfEndian ew) =
      ok (.Ok (), vctx1)) :
    ∃ vc1 v v1 n n1 newTy,
      verifier.VerificationContext.emit_log vctx0 self.pc
        (.Endian doSwap (coreWidthOfEndian ew) dst) = ok vc1 ∧
      verifier.ComputationContext.reg self dst = ok (.Ok v) ∧
      verifier.Type.inner v self = ok (.Ok v1) ∧
      verifier.ComputationContext.next self = ok (.Ok n) ∧
      verifier.ComputationContext.set_reg n dst newTy = ok (.Ok (), n1) ∧
      verifier.VerificationContext.push_successor vc1 n1 = ok vctx1 ∧
      ((∃ data usvd,
          v1 = .ScalarValue data ∧
          data.unwritten_mask = 0#u64 ∧
          newTy = .ScalarValue usvd ∧
          (do
            let i ← api.apply_endianness doSwap (coreWidthOfEndian ew) data.value
            let i1 ← api.apply_endianness doSwap (coreWidthOfEndian ew) data.unknown_mask
            let ur ← U64Range.impl.max
            U64ScalarValueData.new i i1 0#u64 ur) = ok usvd) ∨
       ((∀ d, v1 ≠ .ScalarValue d) ∧
        vc1.calling_context.allow_ptr_leaks = true ∧
        newTy = .ScalarValue unknownWrittenScalar64)) := by
  unfold verifier.ComputationContext.endianness at h
  rcases bind_eq_ok h with ⟨vc1, hemit, h⟩
  rcases bind_eq_ok h with ⟨r, hreg, h⟩
  cases r with
  | Err _ => cases Prod.mk.inj (ok_inj h) |>.1
  | Ok v =>
    dsimp only at h
    rcases bind_eq_ok h with ⟨r1, hinner, h⟩
    cases r1 with
    | Err _ => cases Prod.mk.inj (ok_inj h) |>.1
    | Ok v1 =>
      dsimp only at h
      cases v1 with
      | ScalarValue data =>
        dsimp only at h
        rcases bind_eq_ok h with ⟨b, hb, h⟩
        cases b with
        | false =>
          simp only [Bool.false_eq_true, ↓reduceIte] at h
          cases Prod.mk.inj (ok_inj h) |>.1
        | true =>
          simp only [↓reduceIte] at h
          have hunw : data.unwritten_mask = 0#u64 := by
            unfold U64ScalarValueData.is_fully_initialized at hb
            exact of_decide_eq_true (ok_inj hb)
          rcases bind_eq_ok h with ⟨i, hi, h⟩
          rcases bind_eq_ok h with ⟨i1, hi1, h⟩
          rcases bind_eq_ok h with ⟨ur, hur, h⟩
          rcases bind_eq_ok h with ⟨usvd, hnew, h⟩
          rcases bind_eq_ok h with ⟨r2, hnext, h⟩
          cases r2 with
          | Err _ => cases Prod.mk.inj (ok_inj h) |>.1
          | Ok n =>
            dsimp only at h
            rcases bind_eq_ok h with ⟨⟨r3, n1⟩, hset, h⟩
            cases r3 with
            | Err _ => cases Prod.mk.inj (ok_inj h) |>.1
            | Ok u0 =>
              cases u0
              rcases bind_eq_ok h with ⟨vc2, hpush, h⟩
              rcases Prod.mk.inj (ok_inj h) with ⟨_, rfl⟩
              refine ⟨vc1, v, .ScalarValue data, n, n1, .ScalarValue usvd,
                hemit, hreg, hinner, hnext, hset, hpush, Or.inl ⟨data, usvd, rfl, hunw, rfl, ?_⟩⟩
              rw [bind_ok_eq hi, bind_ok_eq hi1, bind_ok_eq hur]
              exact hnew
      | _ =>
        dsimp only at h
        obtain ⟨hleak, n, n1, hnext, hset, hpush⟩ := endianness_ptr_leak_arm_unpack h
        exact ⟨vc1, v, _, n, n1, .ScalarValue unknownWrittenScalar64,
          hemit, hreg, hinner, hnext, hset, hpush,
          Or.inr ⟨(fun d hc => by cases hc), hleak, rfl⟩⟩

theorem visit_endian_forward
    {env : ExecEnv} {ctx ctx_out : verifier.ComputationContext}
    {vctx0 vctx1 : verifier.VerificationContext}
    {doSwap : Bool} {ew : EndianWidth} {dst : Nat}
    (hctx : CompCtxValid env vctx0.counter.bv.toNat ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hfwd : VisitorDispatchNonLddw CompCtxVisitorInst ctx vctx0
      (.ok (.Ok (), ctx_out, vctx1)) (.endian doSwap ew dst)) :
    ctx_out = ctx ∧ ForwardVisitPost env vctx0 vctx1 ctx_out := by
  rcases hfwd with ⟨dst8, rfl, hcall⟩
  have hcall' := hcall.symm
  have hend : ctx_out = ctx ∧
      verifier.ComputationContext.endianness ctx doSwap vctx0 dst8 (coreWidthOfEndian ew) =
        ok (.Ok (), vctx1) := by
    cases doSwap
    · dsimp [CompCtxVisitorInst] at hcall'
      unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.le at hcall'
      rcases bind_eq_ok hcall' with ⟨⟨r, vc1⟩, he, hcall'⟩
      rcases Prod.mk.inj (ok_inj hcall') with ⟨rfl, hpair⟩
      rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
      exact ⟨rfl, he⟩
    · dsimp [CompCtxVisitorInst] at hcall'
      unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.be at hcall'
      rcases bind_eq_ok hcall' with ⟨⟨r, vc1⟩, he, hcall'⟩
      rcases Prod.mk.inj (ok_inj hcall') with ⟨rfl, hpair⟩
      rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
      exact ⟨rfl, he⟩
  rcases hend with ⟨hctx_eq, hend⟩
  subst ctx_out
  obtain ⟨vc1, v, v1, n, n1, newTy, hemit, hreg, _, hnext, hset, hpush, hcases⟩ :=
    endianness_ok_unpack hend
  obtain ⟨he1, he2, he3, he4, he5, he6⟩ := VerificationContext_emit_log_ok_spec hemit
  have hnew_valid : TypeValid env vctx0.counter.bv.toNat newTy := by
    rcases hcases with ⟨data, usvd, _, _, rfl, hblk⟩ | ⟨_, _, rfl⟩
    · obtain ⟨i, hi, _⟩ := apply_endianness_sound doSwap ew data.value
      obtain ⟨i1, hi1, _⟩ := apply_endianness_sound doSwap ew data.unknown_mask
      have hur : U64Range.impl.max = ok { min := 0#u64, max := core.num.U64.MAX } := rfl
      rw [bind_ok_eq hi, bind_ok_eq hi1, bind_ok_eq hur] at hblk
      exact new64_valid hblk
    · exact unknownWrittenScalar64_valid
  obtain ⟨_, hn_eq⟩ := ComputationContext_next_ok_spec hnext
  have hn_valid : CompCtxValid env vctx0.counter.bv.toNat n := by rw [hn_eq]; exact hctx
  obtain ⟨_, _, _, _, _, _, hn1_valid⟩ :=
    ComputationContext_set_reg_ok_spec hn_valid hnew_valid hset
  obtain ⟨hp1, hp2, hp3, hp4, hp5, _, hp7⟩ := VerificationContext_push_successor_ok_spec hpush
  have hcnt_eq : vctx1.counter = vctx0.counter := hp3.trans he3
  refine ⟨rfl, hp1.trans he1, hp2.trans he2, hp4.trans he4, hp5.trans he5,
    by rw [hcnt_eq], by rwa [hcnt_eq], ?_⟩
  intro opt hopt
  rw [hp7, he6, hpending, List.nil_append, List.mem_singleton] at hopt
  subst hopt
  exact ⟨n1, rfl, by rwa [hcnt_eq]⟩

theorem visit_endian_backward
    {prog : Program} {env : ExecEnv} {vCounter F : Nat}
    {ctx ctx_out : verifier.ComputationContext}
    {vctx0 vctx1 vctx_back vctx_back1 : verifier.VerificationContext}
    {dPost : verifier.DataDependencies} {tracker : verifier.DependencyTracker}
    {doSwap : Bool} {ew : EndianWidth} {dst : Nat}
    (hcctx : CallingContextEnvMatches vctx0.calling_context env)
    (hpc : ∃ hlt : ctx.pc.val < prog.size, prog[ctx.pc.val]'hlt = .endian doSwap ew dst)
    (hctx : CompCtxValid env vCounter ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hfwd : VisitorDispatchNonLddw CompCtxVisitorInst ctx vctx0
      (.ok (.Ok (), ctx_out, vctx1)) (.endian doSwap ew dst))
    (hchildren : ∀ succ, some succ ∈ vctx1.pending_successors.val →
      SemTerminatesIn prog env vCounter succ dPost F)
    (hback : VisitorDispatchNonLddw DepTrackerVisitorInst
      { context := ctx_out, deps := dPost } vctx_back
      (.ok (.Ok (), tracker, vctx_back1)) (.endian doSwap ew dst)) :
    ctx_out = ctx ∧
    tracker.context = ctx ∧
    vctx_back1 = vctx_back ∧
    SemTerminatesIn prog env vCounter ctx tracker.deps (F + 1) := by
  rcases hfwd with ⟨dst8, hdst, hcall⟩
  have hcall' := hcall.symm
  have hend : ctx_out = ctx ∧
      verifier.ComputationContext.endianness ctx doSwap vctx0 dst8 (coreWidthOfEndian ew) =
        ok (.Ok (), vctx1) := by
    cases doSwap
    · dsimp [CompCtxVisitorInst] at hcall'
      unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.le at hcall'
      rcases bind_eq_ok hcall' with ⟨⟨r, vc1⟩, he, hcall'⟩
      rcases Prod.mk.inj (ok_inj hcall') with ⟨rfl, hpair⟩
      rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
      exact ⟨rfl, he⟩
    · dsimp [CompCtxVisitorInst] at hcall'
      unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.be at hcall'
      rcases bind_eq_ok hcall' with ⟨⟨r, vc1⟩, he, hcall'⟩
      rcases Prod.mk.inj (ok_inj hcall') with ⟨rfl, hpair⟩
      rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
      exact ⟨rfl, he⟩
  rcases hend with ⟨hctx_eq, hend⟩
  subst ctx_out
  rcases hback with ⟨dst8_b, hdst_b, hback_call⟩
  have hdst_u8 : dst8_b = dst8 := UScalar.eq_of_val_eq (hdst_b.symm.trans hdst)
  subst dst8_b; subst dst
  have hback' := hback_call.symm
  have hback_unpack :
      tracker.context = ctx ∧
      vctx_back1 = vctx_back ∧
      verifier.DataDependencies.add_reg dPost dst8 = ok tracker.deps := by
    cases doSwap
    · dsimp [DepTrackerVisitorInst] at hback'
      unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.le at hback'
      rcases bind_eq_ok hback' with ⟨dd, hadd, hback'⟩
      rcases Prod.mk.inj (Prod.mk.inj (ok_inj hback')).2 with ⟨rfl, rfl⟩
      exact ⟨rfl, rfl, hadd⟩
    · dsimp [DepTrackerVisitorInst] at hback'
      unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.be at hback'
      rcases bind_eq_ok hback' with ⟨dd, hadd, hback'⟩
      rcases Prod.mk.inj (Prod.mk.inj (ok_inj hback')).2 with ⟨rfl, rfl⟩
      exact ⟨rfl, rfl, hadd⟩
  obtain ⟨htrk_ctx, hv_back, hadd_reg⟩ := hback_unpack
  obtain ⟨hdst_lt16, _, hadd_r, _, hsub_deps⟩ := DataDependencies_add_reg_ok_spec hadd_reg
  have hdst_live_pre : DataDepsHasReg tracker.deps dst8.val = true := by
    rw [hadd_r dst8.val hdst_lt16]; simp
  obtain ⟨vc1, v, v1, n, n1, newTy, hemit, hreg, hinner, hnext, hset, hpush, hcases⟩ :=
    endianness_ok_unpack hend
  obtain ⟨he1, _, _, _, _, he6⟩ := VerificationContext_emit_log_ok_spec hemit
  have hnew_valid : TypeValid env vCounter newTy := by
    rcases hcases with ⟨data, usvd, _, _, rfl, hblk⟩ | ⟨_, _, rfl⟩
    · obtain ⟨i, hi, _⟩ := apply_endianness_sound doSwap ew data.value
      obtain ⟨i1, hi1, _⟩ := apply_endianness_sound doSwap ew data.unknown_mask
      have hur : U64Range.impl.max = ok { min := 0#u64, max := core.num.U64.MAX } := rfl
      rw [bind_ok_eq hi, bind_ok_eq hi1, bind_ok_eq hur] at hblk
      exact new64_valid hblk
    · exact unknownWrittenScalar64_valid
  obtain ⟨hpc_next, hn_eq⟩ := ComputationContext_next_ok_spec hnext
  have hn_valid : CompCtxValid env vCounter n := by rw [hn_eq]; exact hctx
  obtain ⟨hdst_lt10, hpc_n1, hregs_n1, hstk_n1, hbm_n1, hres_n1, _⟩ :=
    ComputationContext_set_reg_ok_spec hn_valid hnew_valid hset
  obtain ⟨_, _, _, _, _, _, hp7⟩ := VerificationContext_push_successor_ok_spec hpush
  have hn1_mem : some n1 ∈ vctx1.pending_successors.val := by
    rw [hp7, he6, hpending]; simp
  have hsem := hchildren n1 hn1_mem
  obtain ⟨hlt_pc, hinsn_at⟩ := hpc
  refine ⟨rfl, htrk_ctx, hv_back, ?_⟩
  intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
  obtain ⟨_, hv_valid, hrel_dst⟩ :=
    ComputationContext_reg_ok_valid_and_rel2 hctx hrel (Or.inr hdst_live_pre) hreg
  have hv_ne_uninit : v ≠ .ScalarValue uninitScalar64 := by
    intro hc; subst hc
    obtain ⟨r_in, hr_in, hr_iff⟩ := Type_inner_spec hctx.2.2.2 (.ScalarValue uninitScalar64)
    rw [hinner] at hr_in
    have hv1_eq : v1 = .ScalarValue uninitScalar64 :=
      ((hr_iff v1).mp (ok_inj hr_in).symm).1
    subst hv1_eq
    rcases hcases with ⟨data, _, hdata, hunw, _, _⟩ | ⟨hne, _, _⟩
    · cases hdata
      exact uninitScalar64_unwritten_ne_zero hunw
    · exact hne uninitScalar64 rfl
  obtain ⟨hv1_eq, _, hc_alive1, hc_alive2⟩ :=
    Type_inner_ok_valid_and_checkAlive_none hctx hrel hv_valid hv_ne_uninit hinner hrel_dst
  let resVal (l : HostLayout) (st : MachineState) : RuntimeVal :=
    .scalar (evalEndian doSwap ew ((st.regs dst8.val).toBits l st.world.arrayLen))
  have hstep_And_rel :
      ((!(st₁.regs dst8.val).isScalar && !env.allowPtrLeaks) = false) ∧
      ((!(st₂.regs dst8.val).isScalar && !env.allowPtrLeaks) = false) ∧
      TypeRel2 env ρ resActive isNullId st₁.world.arrayLen newTy
        (resVal l₁ st₁) (resVal l₂ st₂) := by
    rcases hcases with ⟨data, usvd, hv1_sv, hunw, rfl, hblk⟩ | ⟨hne_sv, hleak_vc1, rfl⟩
    · have hv_sv : v = .ScalarValue data := by
        cases v with
        | Releasable _ inner =>
          cases inner <;> try exact False.elim hv_valid
          all_goals (rw [hv1_sv] at hv1_eq; cases hv1_eq)
        | _ =>
          dsimp [pureUnwrapReleasable] at hv1_eq
          exact hv1_eq.symm.trans hv1_sv
      subst hv_sv
      have hd_ne : data ≠ uninitScalar64 := fun hc => uninitScalar64_unwritten_ne_zero (hc ▸ hunw)
      simp only [TypeRel2, hd_ne, ↓reduceIte] at hrel_dst
      rcases hrel_dst with ⟨x₁, x₂, hx1, hx2, hconc⟩
      have hx_eq := fully_initialized_eq_of_rel2_64 hunw hconc
      subst hx_eq
      obtain ⟨_, hrel_usvd⟩ := endian_scalar_valid_and_rel2 (env := env) (ρ := ρ)
        (resActive := resActive) (isNullId := isNullId) (arrayLen := st₁.world.arrayLen)
        hconc.1 hblk
      refine ⟨by rw [hx1]; rfl, by rw [hx2]; rfl, ?_⟩
      simpa only [resVal, hx1, hx2, RuntimeVal.toBits] using hrel_usvd
    · have hallow_env : env.allowPtrLeaks = true := by
        rw [hcctx.1, ← he1]; exact hleak_vc1
      have hl_eq : l₁ = l₂ := by
        rcases hleak with hn | ⟨hl, _⟩
        · simp [hallow_env] at hn
        · exact hl
      have hv_init : verifier.Type.is_initialized v = ok true := by
        rw [Type_is_initialized_spec]
        cases v with
        | ScalarValue d =>
          dsimp [pureUnwrapReleasable] at hv1_eq
          exact False.elim (hne_sv d hv1_eq)
        | _ => rfl
      have hbits_eq := Type_is_initialized_toBits_eq hv_valid hv_init hrel_dst hl_eq
      refine ⟨by simp [hallow_env], by simp [hallow_env], ?_⟩
      dsimp only [resVal]
      rw [← hrel.2.2.1, ← hbits_eq]
      exact TypeRel2_scalar_rel2_of_concretizes (concretizesScalar64Rel2_unknownWritten _)
  rcases hstep_And_rel with ⟨hguard1, hguard2, hrel_new⟩
  let st₁' : MachineState :=
    { st₁ with pc := n1.pc.val, regs := updateReg st₁.regs dst8.val (resVal l₁ st₁),
               world := bumpWorldStep st₁.world }
  let st₂' : MachineState :=
    { st₂ with pc := n1.pc.val, regs := updateReg st₂.regs dst8.val (resVal l₂ st₂),
               world := bumpWorldStep st₂.world }
  have hrel_post : StateRel2Deps env n1 dPost ρ resActive isNullId st₁' st₂' :=
    StateRel2Deps_set_reg (by unfold GENERAL_REGISTER_COUNT at hdst_lt10; exact hdst_lt10)
      (hregs_n1.trans (by rw [hn_eq]))
      (hstk_n1.trans (by rw [hn_eq]))
      (hbm_n1.trans (by rw [hn_eq]))
      (hres_n1.trans (by rw [hn_eq]))
      (fun r hr _ hd => hsub_deps.1 r hr hd)
      (fun s hs hd => hsub_deps.2 s hs hd)
      hrel
      (fun _ => hrel_new)
  have halloc' : (bumpWorldStep st₁.world).nextAllocId + 3 * F < 2 ^ 63 := by
    simp only [bumpWorldStep]; omega
  obtain ⟨ret, finalWorld, hr₁, hr₂, hlive_fin⟩ :=
    hsem l₁ l₂ s₁ s₂ ρ resActive isNullId st₁' st₂' hl₁ hl₂ hleak halloc' hρ hrel_post
  have hn1_pc : n1.pc.val = ctx.pc.val + 1 := by rw [hpc_n1, hpc_next]
  refine ⟨ret, finalWorld, ?_, ?_, hlive_fin⟩
  · unfold runSteps step
    simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, checkDst_eq_ok hdst_lt10,
      hc_alive1, hguard1, Bool.false_eq_true, ↓reduceIte]
    rw [stepNextIte_eq_next (by dsimp [st₁', resVal]; rw [hn1_pc]) hr₁]
    exact hr₁
  · unfold runSteps step
    simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, checkDst_eq_ok hdst_lt10,
      hc_alive2, hguard2, Bool.false_eq_true, ↓reduceIte]
    rw [stepNextIte_eq_next (by dsimp [st₂', resVal]; rw [hn1_pc]) hr₂]
    exact hr₂

theorem visit_jump_forward
    {env : ExecEnv} {ctx ctx_out : verifier.ComputationContext}
    {vctx0 vctx1 : verifier.VerificationContext}
    {offInt : Int}
    (hctx : CompCtxValid env vctx0.counter.bv.toNat ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hfwd : VisitorDispatchNonLddw CompCtxVisitorInst ctx vctx0
      (.ok (.Ok (), ctx_out, vctx1)) (.jump offInt)) :
    ctx_out = ctx ∧ ForwardVisitPost env vctx0 vctx1 ctx_out := by
  rcases hfwd with ⟨off, rfl, hcall⟩
  have hcall' := hcall.symm
  dsimp [CompCtxVisitorInst] at hcall'
  unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.jump at hcall'
  rcases bind_eq_ok hcall' with ⟨vc1, hemit, hcall'⟩
  rcases bind_eq_ok hcall' with ⟨r, hjmp, hcall'⟩
  cases r with
  | Err _ => cases Prod.mk.inj (ok_inj hcall') |>.1
  | Ok n =>
    dsimp only at hcall'
    rcases bind_eq_ok hcall' with ⟨vc2, hpush, hcall'⟩
    rcases Prod.mk.inj (ok_inj hcall') with ⟨_, hpair⟩
    rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
    obtain ⟨he1, he2, he3, he4, he5, he6⟩ := VerificationContext_emit_log_ok_spec hemit
    obtain ⟨_, hn_eq⟩ := ComputationContext_checked_jump_with_offset_ok_spec hjmp
    have hn_valid : CompCtxValid env vctx0.counter.bv.toNat n := by rw [hn_eq]; exact hctx
    obtain ⟨hp1, hp2, hp3, hp4, hp5, _, hp7⟩ := VerificationContext_push_successor_ok_spec hpush
    have hcnt_eq : vc2.counter = vctx0.counter := hp3.trans he3
    refine ⟨rfl, hp1.trans he1, hp2.trans he2, hp4.trans he4, hp5.trans he5,
      by rw [hcnt_eq], by rwa [hcnt_eq], ?_⟩
    intro opt hopt
    rw [hp7, he6, hpending, List.nil_append, List.mem_singleton] at hopt
    subst hopt
    exact ⟨n, rfl, by rwa [hcnt_eq]⟩

theorem visit_jump_backward
    {prog : Program} {env : ExecEnv} {vCounter F : Nat}
    {ctx ctx_out : verifier.ComputationContext}
    {vctx0 vctx1 vctx_back vctx_back1 : verifier.VerificationContext}
    {dPost : verifier.DataDependencies} {tracker : verifier.DependencyTracker}
    {offInt : Int}
    (hcode_len : vctx0.code.val.length = prog.size)
    (hpc : ∃ hlt : ctx.pc.val < prog.size, prog[ctx.pc.val]'hlt = .jump offInt)
    (hpending : vctx0.pending_successors.val = [])
    (hfwd : VisitorDispatchNonLddw CompCtxVisitorInst ctx vctx0
      (.ok (.Ok (), ctx_out, vctx1)) (.jump offInt))
    (hchildren : ∀ succ, some succ ∈ vctx1.pending_successors.val →
      SemTerminatesIn prog env vCounter succ dPost F)
    (hback : VisitorDispatchNonLddw DepTrackerVisitorInst
      { context := ctx_out, deps := dPost } vctx_back
      (.ok (.Ok (), tracker, vctx_back1)) (.jump offInt)) :
    ctx_out = ctx ∧
    tracker.context = ctx ∧
    vctx_back1 = vctx_back ∧
    SemTerminatesIn prog env vCounter ctx tracker.deps (F + 1) := by
  rcases hfwd with ⟨off, hoff, hcall⟩
  have hcall' := hcall.symm
  dsimp [CompCtxVisitorInst] at hcall'
  unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.jump at hcall'
  rcases bind_eq_ok hcall' with ⟨vc1, hemit, hcall'⟩
  rcases bind_eq_ok hcall' with ⟨r, hjmp, hcall'⟩
  cases r with
  | Err _ => cases Prod.mk.inj (ok_inj hcall') |>.1
  | Ok n =>
    dsimp only at hcall'
    rcases bind_eq_ok hcall' with ⟨vc2, hpush, hcall'⟩
    rcases Prod.mk.inj (ok_inj hcall') with ⟨_, hpair⟩
    rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
    rcases hback with ⟨off_b, _, hback_call⟩
    have hback' := hback_call.symm
    dsimp [DepTrackerVisitorInst] at hback'
    unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.jump at hback'
    rcases Prod.mk.inj (Prod.mk.inj (ok_inj hback')).2 with ⟨rfl, rfl⟩
    subst hoff
    obtain ⟨_, he2, _, _, _, he6⟩ := VerificationContext_emit_log_ok_spec hemit
    obtain ⟨htgt, hn_eq⟩ := ComputationContext_checked_jump_with_offset_ok_spec hjmp
    rw [he2, hcode_len] at htgt
    obtain ⟨_, _, _, _, _, _, hp7⟩ := VerificationContext_push_successor_ok_spec hpush
    have hn_mem : some n ∈ vc2.pending_successors.val := by
      rw [hp7, he6, hpending]; simp
    have hsem := hchildren n hn_mem
    obtain ⟨hlt_pc, hinsn_at⟩ := hpc
    refine ⟨rfl, rfl, rfl, ?_⟩
    intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
    let st₁' : MachineState :=
      { st₁ with pc := n.pc.val, world := bumpWorldStep st₁.world }
    let st₂' : MachineState :=
      { st₂ with pc := n.pc.val, world := bumpWorldStep st₂.world }
    have hrel_post : StateRel2Deps env n dPost ρ resActive isNullId st₁' st₂' := by
      rcases hrel with ⟨_, _, hw, harr, hnull, hnodup, hlive, hbnds, hr10_1, hr10_2, hregs, hstk⟩
      dsimp only [st₁', st₂']
      rw [hn_eq]
      exact ⟨rfl, rfl, by simp [bumpWorldStep, hw], harr, hnull, hnodup,
        by simpa [bumpWorldStep] using hlive,
        by simpa [bumpWorldStep] using hbnds,
        hr10_1, hr10_2,
        fun r hr hlen hd => hregs r hr hlen hd,
        fun s hs hd => hstk s hs hd⟩
    have halloc' : (bumpWorldStep st₁.world).nextAllocId + 3 * F < 2 ^ 63 := by
      simp only [bumpWorldStep]; omega
    obtain ⟨ret, finalWorld, hr₁, hr₂, hlive_fin⟩ :=
      hsem l₁ l₂ s₁ s₂ ρ resActive isNullId st₁' st₂' hl₁ hl₂ hleak halloc' hρ hrel_post
    refine ⟨ret, finalWorld, ?_, ?_, hlive_fin⟩
    · unfold runSteps step
      simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, htgt]
      rw [stepNextIte_eq_next rfl hr₁]
      exact hr₁
    · unfold runSteps step
      simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, htgt]
      rw [stepNextIte_eq_next rfl hr₂]
      exact hr₂

theorem visit_exit_forward
    {env : ExecEnv} {ctx ctx_out : verifier.ComputationContext}
    {vctx0 vctx1 : verifier.VerificationContext}
    (hctx : CompCtxValid env vctx0.counter.bv.toNat ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hfwd : VisitorDispatchNonLddw CompCtxVisitorInst ctx vctx0
      (.ok (.Ok (), ctx_out, vctx1)) .exit) :
    ctx_out = ctx ∧ ForwardVisitPost env vctx0 vctx1 ctx_out := by
  have hcall' := hfwd.symm
  dsimp [VisitorDispatchNonLddw, CompCtxVisitorInst] at hcall'
  unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.exit at hcall'
  rcases bind_eq_ok hcall' with ⟨vc1, hemit, hcall'⟩
  rcases bind_eq_ok hcall' with ⟨r, _, hcall'⟩
  cases r with
  | Err _ => cases Prod.mk.inj (ok_inj hcall') |>.1
  | Ok v =>
    dsimp only at hcall'
    rcases bind_eq_ok hcall' with ⟨b, _, hcall'⟩
    cases b with
    | false =>
      simp only [Bool.false_eq_true, ↓reduceIte] at hcall'
      cases Prod.mk.inj (ok_inj hcall') |>.1
    | true =>
      simp only [↓reduceIte] at hcall'
      rcases bind_eq_ok hcall' with ⟨i, _, hcall'⟩
      by_cases hne0 : (i != 0#usize) = true
      · simp only [hne0, ↓reduceIte] at hcall'
        cases Prod.mk.inj (ok_inj hcall') |>.1
      · simp only [hne0, Bool.false_eq_true, ↓reduceIte] at hcall'
        rcases Prod.mk.inj (Prod.mk.inj (ok_inj hcall')).2 with ⟨rfl, rfl⟩
        obtain ⟨he1, he2, he3, he4, he5, he6⟩ := VerificationContext_emit_log_ok_spec hemit
        refine ⟨rfl, he1, he2, he4, he5, by rw [he3], by rwa [he3], ?_⟩
        intro opt hopt
        rw [he6, hpending] at hopt
        cases hopt

theorem visit_exit_backward
    {prog : Program} {env : ExecEnv} {vCounter F : Nat}
    {ctx ctx_out : verifier.ComputationContext}
    {vctx0 vctx1 vctx_back vctx_back1 : verifier.VerificationContext}
    {dPost : verifier.DataDependencies} {tracker : verifier.DependencyTracker}
    (hpc : ∃ hlt : ctx.pc.val < prog.size, prog[ctx.pc.val]'hlt = .exit)
    (hctx : CompCtxValid env vCounter ctx)
    (hfwd : VisitorDispatchNonLddw CompCtxVisitorInst ctx vctx0
      (.ok (.Ok (), ctx_out, vctx1)) .exit)
    (hback : VisitorDispatchNonLddw DepTrackerVisitorInst
      { context := ctx_out, deps := dPost } vctx_back
      (.ok (.Ok (), tracker, vctx_back1)) .exit) :
    ctx_out = ctx ∧
    tracker.context = ctx ∧
    vctx_back1 = vctx_back ∧
    SemTerminatesIn prog env vCounter ctx tracker.deps (F + 1) := by
  have hcall' := hfwd.symm
  dsimp [VisitorDispatchNonLddw, CompCtxVisitorInst] at hcall'
  unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.exit at hcall'
  rcases bind_eq_ok hcall' with ⟨vc1, _, hcall'⟩
  rcases bind_eq_ok hcall' with ⟨r, hreg0, hcall'⟩
  cases r with
  | Err _ => cases Prod.mk.inj (ok_inj hcall') |>.1
  | Ok v =>
    dsimp only at hcall'
    rcases bind_eq_ok hcall' with ⟨b, hws, hcall'⟩
    cases b with
    | false =>
      simp only [Bool.false_eq_true, ↓reduceIte] at hcall'
      cases Prod.mk.inj (ok_inj hcall') |>.1
    | true =>
      simp only [↓reduceIte] at hcall'
      rcases bind_eq_ok hcall' with ⟨i, hlen, hcall'⟩
      by_cases hne0 : (i != 0#usize) = true
      · simp only [hne0, ↓reduceIte] at hcall'
        cases Prod.mk.inj (ok_inj hcall') |>.1
      · simp only [hne0, Bool.false_eq_true, ↓reduceIte] at hcall'
        rcases Prod.mk.inj (Prod.mk.inj (ok_inj hcall')).2 with ⟨hctx_eq, hv1_eq⟩
        subst ctx_out; subst vctx1
        have hback' := hback.symm
        dsimp [VisitorDispatchNonLddw, DepTrackerVisitorInst] at hback'
        unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.exit at hback'
        rcases bind_eq_ok hback' with ⟨dd, hadd0, hback'⟩
        rcases Prod.mk.inj (Prod.mk.inj (ok_inj hback')).2 with ⟨htrk_eq, hvb_eq⟩
        subst tracker; subst vctx_back1
        obtain ⟨h0_lt16, _, hadd_r, _, _⟩ := DataDependencies_add_reg_ok_spec hadd0
        have hr0_live :
            DataDepsHasReg ({ context := ctx, deps := dd } : verifier.DependencyTracker).deps (0#u8).val = true := by
          dsimp only
          rw [hadd_r (0#u8).val h0_lt16]; simp
        have hi_eq0 : i = 0#usize :=
          Classical.byContradiction fun hc => hne0 (bne_iff_ne.mpr hc)
        rw [IdSet_len_spec] at hlen
        have hres_empty : ctx.resources.entries.val = [] := by
          have hl : ctx.resources.entries.val.length = 0 := by
            have := congrArg UScalar.val ((ok_inj hlen).trans hi_eq0)
            simpa using this
          exact List.eq_nil_of_length_eq_zero hl
        have hws_some : ∃ d, verifier.Type.written_scalar v = ok (some d) := by
          rw [Type_is_written_scalar_spec] at hws
          cases v with
          | ScalarValue d =>
            have hunw : d.unwritten_mask = 0#u64 := of_decide_eq_true (ok_inj hws)
            refine ⟨d, ?_⟩
            rw [Type_written_scalar_spec]
            simp [hunw]
          | _ => cases ok_inj hws
        obtain ⟨d_ret, hws_d⟩ := hws_some
        obtain ⟨hlt_pc, hinsn_at⟩ := hpc
        refine ⟨rfl, rfl, rfl, ?_⟩
        intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ _ _ _ _ _ hrel
        obtain ⟨_, _, hrel0⟩ :=
          ComputationContext_reg_ok_valid_and_rel2 hctx hrel (Or.inr hr0_live) hreg0
        obtain ⟨_, _, ret, hr0_1, hr0_2, _⟩ := Type_written_scalar_some_rel2 hws_d hrel0
        have h0_val : (0#u8).val = 0 := rfl
        rw [h0_val] at hr0_1 hr0_2
        have hlive_empty1 : st₁.world.liveResources = [] := by
          apply List.eq_nil_iff_forall_not_mem.mpr
          intro m hm
          rcases (hrel.2.2.2.2.2.2.1 m).mp hm with ⟨r_id, hr_in, _⟩
          rw [hres_empty] at hr_in
          cases hr_in
        have hlive_empty2 : st₂.world.liveResources = [] := by
          rw [← hrel.2.2.1]; exact hlive_empty1
        refine ⟨ret, bumpWorldStep st₁.world, ?_, ?_, ?_⟩
        · unfold runSteps step
          simp [hrel.1, hlt_pc, hinsn_at, hlive_empty1, hr0_1]
        · unfold runSteps step
          simp [hrel.2.1, hlt_pc, hinsn_at, hlive_empty2, hr0_2, hrel.2.2.1]
        · simp [bumpWorldStep, hlive_empty1]

/-! ## 6. Shared Written-Scalar & Scratch-Clobbering Helpers -/

theorem Type_written_scalar_ok_some {t : verifier.Type} {d : U64ScalarValueData}
    (h : verifier.Type.written_scalar t = ok (some d)) :
    t = .ScalarValue d ∧ d.unwritten_mask = 0#u64 := by
  rw [Type_written_scalar_spec] at h
  cases t with
  | ScalarValue data =>
    by_cases hu : data.unwritten_mask = 0#u64
    · simp only [hu, ↓reduceIte] at h
      cases Option.some.inj (ok_inj h)
      exact ⟨rfl, hu⟩
    · simp only [hu, ↓reduceIte] at h
      cases ok_inj h
  | _ => cases ok_inj h

theorem Type_is_written_scalar_ok_true {t : verifier.Type}
    (h : verifier.Type.is_written_scalar t = ok true) :
    ∃ d : U64ScalarValueData, t = .ScalarValue d ∧ d.unwritten_mask = 0#u64 := by
  unfold verifier.Type.is_written_scalar at h
  rcases bind_eq_ok h with ⟨o, ho, h⟩
  cases o with
  | none => cases ok_inj h
  | some d =>
    obtain ⟨ht, hunw⟩ := Type_written_scalar_ok_some ho
    exact ⟨d, ht, hunw⟩

private theorem clobber_one_reg_step
    {regs aOut : Std.Array verifier.Type 10#usize} {idx : Std.Usize} {v : verifier.Type}
    {t : verifier.Type} {im : verifier.Type → Std.Array verifier.Type 10#usize}
    (hidx : idx.val < 10)
    (hi : Std.Array.index_mut_usize regs idx = ok (t, im))
    (hu : Std.Array.update (im t) idx v = ok aOut) :
    aOut.val = regs.val.set idx.val v := by
  have h1 := ok_inj (hi.symm.trans (array_index_mut_usize_10_ok regs idx hidx))
  rcases Prod.mk.inj h1 with ⟨rfl, rfl⟩
  have h2 := ok_inj (hu.symm.trans (array_update_10_ok _ idx v hidx))
  subst h2
  simp [Std.Array.set_val_eq]

theorem clobber_scratch_tail_spec
    {env : ExecEnv} {vCounter : Nat}
    {self ctx_out n1 : verifier.ComputationContext}
    {vc1 vctx1 : verifier.VerificationContext}
    (hn1_valid : CompCtxValid env vCounter n1)
    (h : (do
      massert (1#usize < 10#usize)
      let (t1, index_mut_back) ← Std.Array.index_mut_usize n1.registers 1#usize
      let a := index_mut_back t1
      let t2 ← verifier.Type.UNINITIALIZED
      let a1 ← Std.Array.update a 1#usize t2
      massert (2#usize < 10#usize)
      let (t3, index_mut_back1) ← Std.Array.index_mut_usize a1 2#usize
      let a2 := index_mut_back1 t3
      let a3 ← Std.Array.update a2 2#usize t2
      massert (3#usize < 10#usize)
      let (t4, index_mut_back2) ← Std.Array.index_mut_usize a3 3#usize
      let a4 := index_mut_back2 t4
      let a5 ← Std.Array.update a4 3#usize t2
      massert (4#usize < 10#usize)
      let (t5, index_mut_back3) ← Std.Array.index_mut_usize a5 4#usize
      let a6 := index_mut_back3 t5
      let a7 ← Std.Array.update a6 4#usize t2
      massert (5#usize < 10#usize)
      let (t6, index_mut_back4) ← Std.Array.index_mut_usize a7 5#usize
      let a8 := index_mut_back4 t6
      let a9 ← Std.Array.update a8 5#usize t2
      let context2 ← verifier.VerificationContext.push_successor vc1 { n1 with registers := a9 }
      ok ((core.result.Result.Ok () : core.result.Result Unit error.VerifierErrorKind), self, context2)) =
      ok ((core.result.Result.Ok () : core.result.Result Unit error.VerifierErrorKind), ctx_out, vctx1)) :
    ∃ a9 : Std.Array verifier.Type 10#usize,
      ctx_out = self ∧
      CompCtxValid env vCounter { n1 with registers := a9 } ∧
      (∀ (r : Nat) (hr : r < 10), r < a9.val.length →
        if 1 ≤ r ∧ r ≤ 5 then
          (a9.val[r]'(by rw [array10_len]; exact hr)) = .ScalarValue uninitScalar64
        else
          (a9.val[r]'(by rw [array10_len]; exact hr)) =
            (n1.registers.val[r]'(by rw [array10_len]; exact hr))) ∧
      vctx1.calling_context = vc1.calling_context ∧
      vctx1.code = vc1.code ∧
      vctx1.counter = vc1.counter ∧
      vctx1.iteration = vc1.iteration ∧
      vctx1.terminating_contexts = vc1.terminating_contexts ∧
      vctx1.pending_successors.val = vc1.pending_successors.val ++ [some { n1 with registers := a9 }] := by
  have h1 : (1#usize).val < 10 := by rw [usize_one_val]; omega
  have h2 : (2#usize).val < 10 := by rw [usize_two_val]; omega
  have h3 : (3#usize).val < 10 := by cases System.Platform.numBits_eq <;> simp_all [UScalar.val]
  have h4 : (4#usize).val < 10 := by rw [usize_const_val_4]; omega
  have h5 : (5#usize).val < 10 := by cases System.Platform.numBits_eq <;> simp_all [UScalar.val]
  have hv1 : (1#usize).val = 1 := usize_one_val
  have hv2 : (2#usize).val = 2 := usize_two_val
  have hv3 : (3#usize).val = 3 := by cases System.Platform.numBits_eq <;> simp_all [UScalar.val]
  have hv4 : (4#usize).val = 4 := usize_const_val_4
  have hv5 : (5#usize).val = 5 := by cases System.Platform.numBits_eq <;> simp_all [UScalar.val]
  rcases bind_eq_ok h with ⟨_, _, h⟩
  rcases bind_eq_ok h with ⟨p1, hi1, _⟩; rcases p1 with ⟨t1, im1⟩
  rw [bind_pair_ok_eq hi1] at h
  rcases bind_eq_ok h with ⟨t2, ht2, h⟩
  have ht2_eq : t2 = .ScalarValue uninitScalar64 := ok_inj (ht2.symm.trans Type_UNINITIALIZED_eq)
  rcases bind_eq_ok h with ⟨a1, hu1, h⟩
  have ha1 := clobber_one_reg_step h1 hi1 hu1
  rcases bind_eq_ok h with ⟨_, _, h⟩
  rcases bind_eq_ok h with ⟨p2, hi2, _⟩; rcases p2 with ⟨t3, im2⟩
  rw [bind_pair_ok_eq hi2] at h
  rcases bind_eq_ok h with ⟨a3, hu2, h⟩
  have ha3 := clobber_one_reg_step h2 hi2 hu2
  rcases bind_eq_ok h with ⟨_, _, h⟩
  rcases bind_eq_ok h with ⟨p3, hi3, _⟩; rcases p3 with ⟨t4, im3⟩
  rw [bind_pair_ok_eq hi3] at h
  rcases bind_eq_ok h with ⟨a5, hu3, h⟩
  have ha5 := clobber_one_reg_step h3 hi3 hu3
  rcases bind_eq_ok h with ⟨_, _, h⟩
  rcases bind_eq_ok h with ⟨p4, hi4, _⟩; rcases p4 with ⟨t5, im4⟩
  rw [bind_pair_ok_eq hi4] at h
  rcases bind_eq_ok h with ⟨a7, hu4, h⟩
  have ha7 := clobber_one_reg_step h4 hi4 hu4
  rcases bind_eq_ok h with ⟨_, _, h⟩
  rcases bind_eq_ok h with ⟨p5, hi5, _⟩; rcases p5 with ⟨t6, im5⟩
  rw [bind_pair_ok_eq hi5] at h
  rcases bind_eq_ok h with ⟨a9, hu5, h⟩
  have ha9 := clobber_one_reg_step h5 hi5 hu5
  rcases bind_eq_ok h with ⟨context2, hpush, h⟩
  rcases Prod.mk.inj (Prod.mk.inj (ok_inj h)).2 with ⟨rfl, rfl⟩
  obtain ⟨hp1, hp2, hp3, hp4, hp5, _, hp7⟩ := VerificationContext_push_successor_ok_spec hpush
  have ha9_val : a9.val = ((((n1.registers.val.set 1 (.ScalarValue uninitScalar64)).set 2
      (.ScalarValue uninitScalar64)).set 3 (.ScalarValue uninitScalar64)).set 4
      (.ScalarValue uninitScalar64)).set 5 (.ScalarValue uninitScalar64) := by
    rw [ha9, ha7, ha5, ha3, ha1, hv1, hv2, hv3, hv4, hv5, ht2_eq]
  refine ⟨a9, rfl, ?_, ?_, hp1, hp2, hp3, hp4, hp5, hp7⟩
  · refine ⟨?_, hn1_valid.2⟩
    intro t ht
    rw [ha9_val] at ht
    repeat (rcases List.mem_or_eq_of_mem_set ht with ht | rfl; swap; exact uninitScalar64_valid)
    exact hn1_valid.1 t ht
  · intro r hr _
    have hget_eq : (a9.val[r]'(by rw [array10_len]; exact hr)) =
        (((((n1.registers.val.set 1 (.ScalarValue uninitScalar64)).set 2
          (.ScalarValue uninitScalar64)).set 3 (.ScalarValue uninitScalar64)).set 4
          (.ScalarValue uninitScalar64)).set 5 (.ScalarValue uninitScalar64))[r]'(by
            simp only [List.length_set, array10_len]; exact hr) := by
      simp only [ha9_val]
    rw [hget_eq]
    rcases (by omega : r = 0 ∨ r = 1 ∨ r = 2 ∨ r = 3 ∨ r = 4 ∨
        r = 5 ∨ r = 6 ∨ r = 7 ∨ r = 8 ∨ r = 9) with
      rfl | rfl | rfl | rfl | rfl | rfl | rfl | rfl | rfl | rfl <;>
      simp

end StarnixEbpf.Proof
