-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Proof.InsnAluEndExit

/-!
# Instruction-Level Forward & Backward Soundness: Memory Load & Store (`LDX`, `ST`, `STX`)

Proves forward validity (`visit_load_forward`, `visit_store_forward`) and backward
2-safety/termination (`visit_load_backward`, `visit_store_backward`) for
`Insn.load` and `Insn.store` across all four pointer regions (`PtrToStack`,
`PtrToMemory`, `PtrToArray`, and `PtrToStruct`), including sub-word stack
reads/writes, dead 64-bit stack stores, immediate masking, and all six context
struct field descriptor kinds.
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

/-! ## 1. Width, Bounds, Pointer-Unwrapping, & State-Update Helpers -/

@[simp]
theorem DataWidth_ofCore_coreWidthOfSpec (w : DataWidth) :
    DataWidth.ofCore (coreWidthOfSpec w) = w := by
  cases w <;> rfl

private theorem i16_eq_of_bv_eq {a b : Std.I16} (h : a.bv = b.bv) : a = b := by
  cases a; cases b; exact congrArg IScalar.mk h

theorem inBoundsRange_mono_right {effOff b₁ b₂ : BitVec 64} {w : DataWidth}
    (hin : inBoundsRange effOff w b₁ = true) (hle : b₁ ≤ b₂) :
    inBoundsRange effOff w b₂ = true := by
  unfold inBoundsRange at hin ⊢
  cases w <;> dsimp [DataWidth.bvBytes] at hin ⊢ <;> revert hin hle <;> bv_decide

theorem insertSubValue_and_valMask64 (orig val : BitVec 64) (subIdx : Nat) (w : DataWidth) :
    insertSubValue orig (val &&& w.valMask64) subIdx w = insertSubValue orig val subIdx w := by
  unfold insertSubValue
  dsimp only
  rw [BitVec.and_assoc, BitVec.and_self]

theorem valMask64_u64_and (val : BitVec 64) :
    val &&& DataWidth.u64.valMask64 = val := by
  unfold DataWidth.valMask64
  bv_decide

theorem is_fully_initialized_64_ok_true {d : U64ScalarValueData}
    (h : U64ScalarValueData.is_fully_initialized d = ok true) :
    d.unwritten_mask = 0#u64 ∧ d ≠ uninitScalar64 := by
  unfold U64ScalarValueData.is_fully_initialized at h
  have hunw : d.unwritten_mask = 0#u64 := of_decide_eq_true (ok_inj h)
  refine ⟨hunw, fun hc => ?_⟩
  subst hc
  exact uninitScalar64_unwritten_ne_zero hunw

theorem extractSubValue_uninit_ne_zero (w : DataWidth) :
    extractSubValue uninitScalar64.unwritten_mask.bv 0 w ≠ 0#64 := by
  have hunw : uninitScalar64.unwritten_mask.bv = ~~~0#64 := u64_max_uscalar_bv
  rw [hunw]
  cases w <;> unfold extractSubValue DataWidth.valMask64 <;> decide

theorem unwrap_stack_ptr_rel2
    {env : ExecEnv} {vCounter : Nat} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {arrayLen : MemoryId → BitVec 64}
    {t : verifier.Type} {off : U64ScalarValueData} {v₁ v₂ : RuntimeVal}
    (ht : TypeValid env vCounter t)
    (hu : pureUnwrapReleasable t = .PtrToStack off)
    (hrel : TypeRel2 env ρ resActive isNullId arrayLen t v₁ v₂) :
    t = .PtrToStack off ∧
    PtrOffsetValid 0xFFFF_FFFF#64 off ∧
    ∃ baseOff : BitVec 64,
      ConcretizesScalar64Rel2 off baseOff baseOff ∧
      v₁ = .stackPtr baseOff ∧ v₂ = .stackPtr baseOff := by
  cases t with
  | PtrToStack off0 =>
    cases hu
    rcases hrel with ⟨hp, baseOff, hconc, hv1, hv2⟩
    exact ⟨rfl, hp, baseOff, hconc, hv1, hv2⟩
  | Releasable relId inner =>
    cases inner <;> try exact False.elim ht
    all_goals cases hu
  | _ => cases hu

theorem unwrap_mem_ptr_rel2
    {env : ExecEnv} {vCounter : Nat} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {arrayLen : MemoryId → BitVec 64}
    {t : verifier.Type} {id : MemoryId} {off : U64ScalarValueData}
    {sz : Std.U64} {orig : verifier.PointerOrigin} {v₁ v₂ : RuntimeVal}
    (ht : TypeValid env vCounter t)
    (hu : pureUnwrapReleasable t = .PtrToMemory id off sz orig)
    (hrel : TypeRel2 env ρ resActive isNullId arrayLen t v₁ v₂) :
    memIdBounded vCounter id = true ∧
    PtrOffsetValid 0xFFFF_FFFF#64 off ∧
    ∃ (relOpt : Option MemoryId) (baseOff : BitVec 64),
      ConcretizesScalar64Rel2 off baseOff baseOff ∧
      v₁ = .memPtr (renameMemId ρ id) (MemoryOrigin.ofCore orig) relOpt sz.bv baseOff ∧
      v₂ = .memPtr (renameMemId ρ id) (MemoryOrigin.ofCore orig) relOpt sz.bv baseOff := by
  cases t with
  | PtrToMemory id0 off0 sz0 orig0 =>
    cases hu
    rcases hrel with ⟨hp, baseOff, hconc, hv1, hv2⟩
    exact ⟨ht.1, hp, none, baseOff, hconc, hv1, hv2⟩
  | Releasable relId inner =>
    cases inner <;> try exact False.elim ht
    · cases hu
      rcases hrel with ⟨_, hp, baseOff, hconc, hv1, hv2⟩
      exact ⟨ht.2.1, hp, some (renameMemId ρ relId), baseOff, hconc, hv1, hv2⟩
    · cases hu
  | _ => cases hu

theorem unwrap_array_ptr_rel2
    {env : ExecEnv} {vCounter : Nat} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {arrayLen : MemoryId → BitVec 64}
    {t : verifier.Type} {id : MemoryId} {off : U64ScalarValueData} {v₁ v₂ : RuntimeVal}
    (ht : TypeValid env vCounter t)
    (hu : pureUnwrapReleasable t = .PtrToArray id off)
    (hrel : TypeRel2 env ρ resActive isNullId arrayLen t v₁ v₂) :
    t = .PtrToArray id off ∧
    memIdBounded vCounter id = true ∧
    PtrOffsetValid MAX_PACKET_OFFSET off ∧
    ∃ baseOff : BitVec 64,
      ConcretizesScalar64Rel2 off baseOff baseOff ∧
      v₁ = .arrayPtr (renameMemId ρ id) baseOff ∧
      v₂ = .arrayPtr (renameMemId ρ id) baseOff := by
  cases t with
  | PtrToArray id0 off0 =>
    cases hu
    rcases hrel with ⟨hp, _, baseOff, hconc, hv1, hv2⟩
    exact ⟨rfl, ht.1, hp, baseOff, hconc, hv1, hv2⟩
  | Releasable relId inner =>
    cases inner <;> try exact False.elim ht
    all_goals cases hu
  | _ => cases hu

theorem unwrap_struct_ptr_rel2
    {env : ExecEnv} {vCounter : Nat} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {arrayLen : MemoryId → BitVec 64}
    {t : verifier.Type} {id : MemoryId} {off : U64ScalarValueData}
    {sd : verifier.StructDescriptor} {v₁ v₂ : RuntimeVal}
    (ht : TypeValid env vCounter t)
    (hu : pureUnwrapReleasable t = .PtrToStruct id off sd)
    (hrel : TypeRel2 env ρ resActive isNullId arrayLen t v₁ v₂) :
    memIdBounded vCounter id = true ∧
    PtrOffsetValid 0xFFFF_FFFF#64 off ∧
    (∃ schemaId, StructDescConforms env schemaId sd ∧ memoryIdMatches schemaId id = true) ∧
    ∃ (relOpt : Option MemoryId) (baseOff : BitVec 64),
      ConcretizesScalar64Rel2 off baseOff baseOff ∧
      v₁ = .structPtr (renameMemId ρ id) relOpt baseOff ∧
      v₂ = .structPtr (renameMemId ρ id) relOpt baseOff := by
  cases t with
  | PtrToStruct id0 off0 sd0 =>
    cases hu
    rcases hrel with ⟨hp, hconf, baseOff, hconc, hv1, hv2⟩
    exact ⟨ht.1, hp, hconf, none, baseOff, hconc, hv1, hv2⟩
  | Releasable relId inner =>
    cases inner <;> try exact False.elim ht
    · cases hu
    · cases hu
      rcases hrel with ⟨_, hp, hconf, baseOff, hconc, hv1, hv2⟩
      exact ⟨ht.2.1, hp, hconf, some (renameMemId ρ relId), baseOff, hconc, hv1, hv2⟩
  | _ => cases hu

theorem StateRel2Deps_update_stack
    {env : ExecEnv} {ctx ctx' : verifier.ComputationContext}
    {dPre dPost : verifier.DataDependencies} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {st₁ st₂ : MachineState}
    {slotIdx : Nat} {s' : verifier.Stack} {newSlot₁ newSlot₂ : StackSlot}
    (hslot_lt : slotIdx < 64)
    (hregs_eq : ctx'.registers = ctx.registers)
    (hstk_eq : ctx'.stack = s')
    (hbm_eq : ctx'.array_bounds = ctx.array_bounds)
    (hres_eq : ctx'.resources = ctx.resources)
    (hslots_other : ∀ s < 64, s ≠ slotIdx → stackSlotType s' s = stackSlotType ctx.stack s)
    (hreg_deps : ∀ r, r < 10 → DataDepsHasReg dPost r = true → DataDepsHasReg dPre r = true)
    (hstk_deps : ∀ s, s < 64 → s ≠ slotIdx → DataDepsHasStack dPost s = true → DataDepsHasStack dPre s = true)
    (hrel : StateRel2Deps env ctx dPre ρ resActive isNullId st₁ st₂)
    (hnew_rel : DataDepsHasStack dPost slotIdx = true →
      StackSlotRel2 env ρ resActive isNullId st₁.world.arrayLen
        (stackSlotType s' slotIdx) newSlot₁ newSlot₂) :
    StateRel2Deps env ctx' dPost ρ resActive isNullId
      { st₁ with pc := ctx'.pc.val, stack := updateStack st₁.stack slotIdx newSlot₁, world := bumpWorldStep st₁.world }
      { st₂ with pc := ctx'.pc.val, stack := updateStack st₂.stack slotIdx newSlot₂, world := bumpWorldStep st₂.world } := by
  rcases hrel with ⟨_, _, hw, harr, hnull, hnodup, hlive, hbnds, hr10_1, hr10_2, hregs, hstk⟩
  refine ⟨rfl, rfl, by simp [bumpWorldStep, hw], harr, hnull, hnodup,
    by simpa [bumpWorldStep, hres_eq] using hlive,
    by simpa [bumpWorldStep, hbm_eq] using hbnds,
    hr10_1, hr10_2, ?_, ?_⟩
  · intro r hr hlen hd
    have hlen_pre : r < ctx.registers.val.length := by rwa [← hregs_eq]
    have hget : (ctx'.registers.val[r]'hlen) = (ctx.registers.val[r]'hlen_pre) := by
      simp [hregs_eq]
    rw [hget]
    exact hregs r hr hlen_pre (hreg_deps r hr hd)
  · intro s hs hd
    rw [hstk_eq]
    by_cases heq : s = slotIdx
    · subst heq
      simp only [updateStack, beq_self_eq_true, ↓reduceIte, bumpWorldStep]
      exact hnew_rel hd
    · have hbeq : (s == slotIdx) = false := beq_false_of_ne heq
      simp only [updateStack, hbeq, Bool.false_eq_true, ↓reduceIte, bumpWorldStep]
      rw [hslots_other s hs heq]
      exact hstk s hs (hstk_deps s hs heq hd)

theorem StateRel2Deps_update_world_bytes
    {env : ExecEnv} {ctx ctx' : verifier.ComputationContext}
    {dPre dPost : verifier.DataDependencies} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {st₁ st₂ : MachineState}
    {w₁' w₂' : WorldState}
    (hnext_eq : ctx' = { ctx with pc := ctx'.pc, path_depth := ctx'.path_depth, jmp_seq := ctx'.jmp_seq })
    (hw_eq : w₁' = w₂')
    (harr_eq : w₁'.arrayLen = st₁.world.arrayLen)
    (hnull_eq : w₁'.structFieldNull = st₁.world.structFieldNull)
    (hlive_eq : w₁'.liveResources = st₁.world.liveResources)
    (hsub : DepsSubset dPost dPre)
    (hrel : StateRel2Deps env ctx dPre ρ resActive isNullId st₁ st₂) :
    StateRel2Deps env ctx' dPost ρ resActive isNullId
      { st₁ with pc := ctx'.pc.val, world := bumpWorldStep w₁' }
      { st₂ with pc := ctx'.pc.val, world := bumpWorldStep w₂' } := by
  rcases hrel with ⟨_, _, _, harr, hnull, hnodup, hlive, hbnds, hr10_1, hr10_2, hregs, hstk⟩
  rw [hnext_eq]
  refine ⟨rfl, rfl, by simp [bumpWorldStep, hw_eq],
    by simpa [bumpWorldStep, harr_eq] using harr,
    by simpa [bumpWorldStep, hnull_eq] using hnull,
    by simpa [bumpWorldStep, hlive_eq] using hnodup,
    by simpa [bumpWorldStep, hlive_eq] using hlive,
    by simpa [bumpWorldStep, harr_eq] using hbnds,
    hr10_1, hr10_2,
    fun r hr hlen hd => by
      simpa [bumpWorldStep, harr_eq] using hregs r hr hlen (hsub.1 r hr hd),
    fun s hs hd => by
      simpa [bumpWorldStep, harr_eq] using hstk s hs (hsub.2 s hs hd)⟩

/-! ## 2. Memory Load (`load_memory`, `visit_load_forward`, `visit_load_backward`) -/

private theorem struct_load_field_valid
    {env : ExecEnv} {vCounter : Nat} {id : MemoryId}
    {sd : verifier.StructDescriptor} {fd : verifier.FieldDescriptor}
    {w : api.DataWidth} {retTy : verifier.Type} {is32 : Bool}
    (hid : memIdBounded vCounter id = true)
    (hconf : ∃ schemaId, StructDescConforms env schemaId sd ∧ memoryIdMatches schemaId id = true)
    (hfd_mem : fd ∈ sd.fields.val)
    (hbranch : (match fd.field_type with
      | .Scalar _ => do
        let t ← verifier.Type.unknown_written_scalar w
        ok (t, false)
      | .MutableScalar _ => do
        let t ← verifier.Type.unknown_written_scalar w
        ok (t, false)
      | .PtrToMemory is_32_bit memory_id buffer_size => do
        let mi ← verifier.MemoryId.Insts.CoreCloneClone.clone id
        let mi1 ← verifier.MemoryId.prepended memory_id mi
        let usvd ← U64ScalarValueData.from_known 0#u64
        let i ← lift (UScalar.cast .U64 buffer_size)
        ok (verifier.Type.PtrToMemory mi1 usvd i .KernelReadOnly, is_32_bit)
      | .NullablePtrToMemory is_32_bit memory_id buffer_size => do
        let mi ← verifier.MemoryId.Insts.CoreCloneClone.clone id
        let prep ← verifier.MemoryId.prepended memory_id mi
        let mi1 ← verifier.MemoryId.Insts.CoreCloneClone.clone prep
        let usvd ← U64ScalarValueData.from_known 0#u64
        let i ← lift (UScalar.cast .U64 buffer_size)
        ok (verifier.Type.NullOr mi1
          (verifier.Type.PtrToMemory prep usvd i .KernelReadOnly), is_32_bit)
      | .PtrToArray is_32_bit array_id => do
        let mi ← verifier.MemoryId.Insts.CoreCloneClone.clone id
        let mi1 ← verifier.MemoryId.prepended array_id mi
        let usvd ← U64ScalarValueData.from_known 0#u64
        ok (verifier.Type.PtrToArray mi1 usvd, is_32_bit)
      | .PtrToEndArray is_32_bit array_id => do
        let mi ← verifier.MemoryId.Insts.CoreCloneClone.clone id
        let mi1 ← verifier.MemoryId.prepended array_id mi
        ok (verifier.Type.PtrToEndArray mi1, is_32_bit)) = ok (retTy, is32)) :
    TypeValid env vCounter retTy := by
  rcases hconf with ⟨schemaId, ⟨_, hfields_nv, _⟩, _⟩
  have hfd_nv := hfields_nv fd hfd_mem
  have hzero_ptr32 : PtrOffsetValid 0xFFFF_FFFF#64 (knownScalar64 0#u64) :=
    ⟨knownScalar64_valid 0#u64, rfl, by decide⟩
  have hzero_pkt : PtrOffsetValid MAX_PACKET_OFFSET (knownScalar64 0#u64) :=
    ⟨knownScalar64_valid 0#u64, rfl, by decide⟩
  cases hft : fd.field_type <;> rw [hft] at hbranch hfd_nv
  · simp only [Type_unknown_written_scalar_eq, bind_ok] at hbranch
    rcases Prod.mk.inj (ok_inj hbranch) with ⟨rfl, _⟩
    exact unknownWrittenScalarWidth_valid _
  · simp only [Type_unknown_written_scalar_eq, bind_ok] at hbranch
    rcases Prod.mk.inj (ok_inj hbranch) with ⟨rfl, _⟩
    exact unknownWrittenScalarWidth_valid _
  · rename_i is_32_bit memory_id buffer_size
    dsimp only at hbranch
    rw [bind_ok_eq (MemoryId_clone_spec id),
        bind_ok_eq (MemoryId_prepended_spec memory_id id),
        bind_ok_eq (from_known_64_eq 0#u64)] at hbranch
    simp only [lift_eq_ok, bind_ok] at hbranch
    rcases Prod.mk.inj (ok_inj hbranch) with ⟨rfl, _⟩
    exact ⟨memIdBounded_prepend (memIdBounded_of_noVerif vCounter hfd_nv) hid, hzero_ptr32⟩
  · rename_i is_32_bit memory_id buffer_size
    dsimp only at hbranch
    rw [bind_ok_eq (MemoryId_clone_spec id),
        bind_ok_eq (MemoryId_prepended_spec memory_id id),
        bind_ok_eq (MemoryId_clone_spec (memoryIdPrepend memory_id id)),
        bind_ok_eq (from_known_64_eq 0#u64)] at hbranch
    simp only [lift_eq_ok, bind_ok] at hbranch
    rcases Prod.mk.inj (ok_inj hbranch) with ⟨rfl, _⟩
    have hb_prep := memIdBounded_prepend (memIdBounded_of_noVerif vCounter hfd_nv) hid
    exact ⟨hb_prep, trivial, hb_prep, hzero_ptr32⟩
  · rename_i is_32_bit array_id
    dsimp only at hbranch
    rw [bind_ok_eq (MemoryId_clone_spec id),
        bind_ok_eq (MemoryId_prepended_spec array_id id),
        bind_ok_eq (from_known_64_eq 0#u64)] at hbranch
    rcases Prod.mk.inj (ok_inj hbranch) with ⟨rfl, _⟩
    exact ⟨memIdBounded_prepend (memIdBounded_of_noVerif vCounter hfd_nv) hid, hzero_pkt⟩
  · rename_i is_32_bit array_id
    dsimp only at hbranch
    rw [bind_ok_eq (MemoryId_clone_spec id),
        bind_ok_eq (MemoryId_prepended_spec array_id id)] at hbranch
    rcases Prod.mk.inj (ok_inj hbranch) with ⟨rfl, _⟩
    exact memIdBounded_prepend (memIdBounded_of_noVerif vCounter hfd_nv) hid

private theorem StructDescriptor_find_field_ok_some_mem
    {sd : verifier.StructDescriptor} {base_offset : U64ScalarValueData}
    {field : verifier.Field} {fd : verifier.FieldDescriptor}
    (hfind : verifier.StructDescriptor.find_field sd base_offset field = ok (some fd)) :
    fd ∈ sd.fields.val := by
  unfold verifier.StructDescriptor.find_field at hfind
  rcases bind_eq_ok hfind with ⟨off_sc, _, hfind⟩
  rcases bind_eq_ok hfind with ⟨b_kn, _, hfind⟩
  cases b_kn with
  | false => cases ok_inj hfind
  | true =>
    rcases bind_eq_ok hfind with ⟨u_max, _, hfind⟩
    by_cases hgt : off_sc.value > u_max
    · simp only [hgt, ↓reduceIte] at hfind; cases ok_inj hfind
    · simp only [hgt, ↓reduceIte, lift_eq_ok, bind_ok] at hfind
      let offset : Std.Usize := UScalar.cast .Usize off_sc.value
      rcases bind_eq_ok hfind with ⟨wb, _, hfind⟩
      by_cases hwb0 : wb = 0#usize
      · simp only [hwb0, ↓reduceIte] at hfind; cases ok_inj hfind
      · simp only [hwb0, ↓reduceIte] at hfind
        rcases bind_eq_ok hfind with ⟨rem_u, _, hfind⟩
        cases hrem : (rem_u != 0#usize) <;>
          simp only [hrem, Bool.false_eq_true, ↓reduceIte] at hfind
        · obtain ⟨opt, hloop, _, hloop_mem⟩ :=
            StructDescriptor_find_field_loop_spec sd offset
          rw [bind_ok_eq hloop] at hfind
          cases opt with
          | none => cases ok_inj hfind
          | some fd0 =>
            dsimp only at hfind
            have hmem0 := (hloop_mem fd0 rfl).1
            have checkSc : ∀ (sz : Std.Usize),
                (match Usize.checked_add offset wb with
                 | none => ok none
                 | some aend =>
                   match Usize.checked_add fd0.offset sz with
                   | none => ok none
                   | some fend =>
                     if aend <= fend then ok (some fd0) else ok none) = ok (some fd) →
                fd0 = fd := by
              intro sz hsc
              cases ho1 : Usize.checked_add offset wb <;> rw [ho1] at hsc
              · cases ok_inj hsc
              · rename_i aend
                cases ho2 : Usize.checked_add fd0.offset sz <;> rw [ho2] at hsc
                · cases ok_inj hsc
                · rename_i fend
                  dsimp only at hsc
                  by_cases hle : aend ≤ fend
                  · simp only [hle, ↓reduceIte] at hsc; exact Option.some.inj (ok_inj hsc)
                  · simp only [hle, ↓reduceIte] at hsc; cases ok_inj hsc
            have checkPt : ∀ (is32 : Bool),
                (do
                  let exp ← if is32 then ok api.DataWidth.U32 else ok api.DataWidth.U64
                  let valid ←
                    if offset = fd0.offset then
                      api.DataWidth.Insts.CoreCmpPartialEqDataWidth.eq field.width exp
                    else ok false
                  if valid then ok (some fd0) else ok none) = ok (some fd) →
                fd0 = fd := by
              intro is32 hpt
              rcases bind_eq_ok hpt with ⟨exp, _, hpt⟩
              rcases bind_eq_ok hpt with ⟨valid, _, hpt⟩
              cases valid with
              | false => cases ok_inj hpt
              | true => exact Option.some.inj (ok_inj hpt)
            cases hft : fd0.field_type <;> rw [hft] at hfind <;> dsimp only at hfind
            · exact checkSc _ hfind ▸ hmem0
            · exact checkSc _ hfind ▸ hmem0
            · exact checkPt _ hfind ▸ hmem0
            · exact checkPt _ hfind ▸ hmem0
            · exact checkPt _ hfind ▸ hmem0
            · exact checkPt _ hfind ▸ hmem0
        · cases ok_inj hfind

private def stepNextPcAux (prog : Program) (nextPc : Nat)
    (regs' : Nat → RuntimeVal) (stk' : StackMem) (w' : WorldState) : StepOutcome :=
  if hnext : nextPc < prog.size then
    if prog[nextPc] == Insn.lddwCont then
      .stuck (.invalidMidLddw nextPc)
    else
      .next { pc := nextPc, regs := regs', stack := stk', world := bumpWorldStep w' }
  else
    .stuck (.invalidPc nextPc)

private def structFieldStepOut (prog : Program) (nextPc dst : Nat)
    (regs : Nat → RuntimeVal) (stk : StackMem) (world : WorldState)
    (id : MemoryId) (effOff : BitVec 64) (w : DataWidth) (field : StructFieldDesc) : StepOutcome :=
  match field.kind with
  | .scalar sz _ =>
      if !inBoundsRange (effOff - field.offset) w sz then
        .stuck (.invalidStructFieldAccess id effOff w)
      else
        let raw := readBufWord world.structBytes id effOff w
        stepNextPcAux prog nextPc (updateReg regs dst (.scalar raw)) stk world
  | .ptrToMemory is32 fieldId bufSz nullable =>
      let expW : DataWidth := if is32 then .u32 else .u64
      if effOff != field.offset || w != expW then
        .stuck (.invalidStructFieldAccess id effOff w)
      else
        let targetId := memoryIdPrepend fieldId id
        let v :=
          if nullable && world.structFieldNull targetId then
            RuntimeVal.scalar 0#64
          else
            .memPtr targetId .kernelReadOnly none bufSz 0#64
        stepNextPcAux prog nextPc (updateReg regs dst v) stk world
  | .ptrToArray is32 arrId =>
      let expW : DataWidth := if is32 then .u32 else .u64
      if effOff != field.offset || w != expW then
        .stuck (.invalidStructFieldAccess id effOff w)
      else
        let targetId := memoryIdPrepend arrId id
        stepNextPcAux prog nextPc (updateReg regs dst (.arrayPtr targetId 0#64)) stk world
  | .ptrToEndArray is32 arrId =>
      let expW : DataWidth := if is32 then .u32 else .u64
      if effOff != field.offset || w != expW then
        .stuck (.invalidStructFieldAccess id effOff w)
      else
        let targetId := memoryIdPrepend arrId id
        stepNextPcAux prog nextPc (updateReg regs dst (.endArrayPtr targetId)) stk world

theorem ComputationContext_load_memory_ok_valid
    {env : ExecEnv} {vCounter : Nat}
    {self : verifier.ComputationContext} {vctx0 vctx1 : verifier.VerificationContext}
    {addr resTy : verifier.Type} {field : verifier.Field}
    (hctx : CompCtxValid env vCounter self)
    (haddr : TypeValid env vCounter addr)
    (hload : verifier.ComputationContext.load_memory self vctx0 addr field =
      ok (.Ok resTy, vctx1)) :
    TypeValid env vCounter resTy ∧
    vctx1.calling_context = vctx0.calling_context ∧
    vctx1.code = vctx0.code ∧
    vctx1.counter = vctx0.counter ∧
    vctx1.iteration = vctx0.iteration ∧
    vctx1.terminating_contexts = vctx0.terminating_contexts ∧
    vctx1.pending_successors = vctx0.pending_successors := by
  unfold verifier.ComputationContext.load_memory at hload
  rcases bind_eq_ok hload with ⟨r, hinner, hload⟩
  cases r with
  | Err _ => cases (Prod.mk.inj (ok_inj hload)).1
  | Ok a =>
    dsimp only at hload
    have ha_eq : a = pureUnwrapReleasable addr := by
      unfold verifier.Type.inner at hinner
      rcases bind_eq_ok hinner with ⟨rc, _, hinner⟩
      cases rc with
      | Err _ => cases ok_inj hinner
      | Ok _ =>
        dsimp only at hinner
        rw [bind_ok_eq (Type_unwrap_releasable_spec addr)] at hinner
        exact (core.result.Result.Ok.inj (ok_inj hinner)).symm
    have ha_valid : TypeValid env vCounter a := ha_eq ▸ Type_unwrap_releasable_valid haddr
    cases a with
    | PtrToStack offset =>
      dsimp only at hload
      rcases bind_eq_ok hload with ⟨offset_sum, _, hload⟩
      rcases bind_eq_ok hload with ⟨r1, hsl, hload⟩
      cases r1 with
      | Err _ => cases (Prod.mk.inj (ok_inj hload)).1
      | Ok tOut =>
        dsimp only at hload
        rcases bind_eq_ok hload with ⟨⟨r2, context1⟩, hreg_ma, hload⟩
        cases r2 with
        | Err _ => cases (Prod.mk.inj (ok_inj hload)).1
        | Ok u0 =>
          cases u0
          rcases Prod.mk.inj (ok_inj hload) with ⟨hok, rfl⟩
          cases core.result.Result.Ok.inj hok
          obtain ⟨r_sl, hr_sl, hpost_sl⟩ := Stack_load_spec hctx.2.1 offset_sum field.width
          rw [hsl] at hr_sl
          have ht_valid := hpost_sl resTy (ok_inj hr_sl).symm
          obtain ⟨h1, h2, h3, h4, h5, h6, _⟩ :=
            VerificationContext_register_memory_access_ok_spec hreg_ma
          exact ⟨ht_valid, h1, h2, h3, h4, h5, h6⟩
    | PtrToMemory id offset buffer_size orig =>
      dsimp only at hload
      rcases bind_eq_ok hload with ⟨bw, _, hload⟩
      rcases bind_eq_ok hload with ⟨r1, _, hload⟩
      cases r1 with
      | Err _ => cases (Prod.mk.inj (ok_inj hload)).1
      | Ok _ =>
        dsimp only at hload
        rw [bind_ok_eq (Type_unknown_written_scalar_eq field.width)] at hload
        rcases bind_eq_ok hload with ⟨⟨r2, context1⟩, hreg_ma, hload⟩
        cases r2 with
        | Err _ => cases (Prod.mk.inj (ok_inj hload)).1
        | Ok u0 =>
          cases u0
          rcases Prod.mk.inj (ok_inj hload) with ⟨hok, rfl⟩
          cases core.result.Result.Ok.inj hok
          obtain ⟨h1, h2, h3, h4, h5, h6, _⟩ :=
            VerificationContext_register_memory_access_ok_spec hreg_ma
          exact ⟨unknownWrittenScalarWidth_valid _, h1, h2, h3, h4, h5, h6⟩
    | PtrToStruct id offset descriptor =>
      dsimp only at hload
      rcases bind_eq_ok hload with ⟨o, hfind, hload⟩
      cases o with
      | none => cases (Prod.mk.inj (ok_inj hload)).1
      | some field_desc =>
        dsimp only at hload
        rcases bind_eq_ok hload with ⟨⟨return_type, _⟩, hft_br, hload⟩
        rcases bind_eq_ok hload with ⟨_, _, hload⟩
        rcases bind_eq_ok hload with ⟨⟨r1, context1⟩, hreg_ma, hload⟩
        cases r1 with
        | Err _ => cases (Prod.mk.inj (ok_inj hload)).1
        | Ok u0 =>
          cases u0
          rcases Prod.mk.inj (ok_inj hload) with ⟨hok, rfl⟩
          cases core.result.Result.Ok.inj hok
          have hfd_mem := StructDescriptor_find_field_ok_some_mem hfind
          have ht_valid := struct_load_field_valid ha_valid.1 ha_valid.2.2 hfd_mem hft_br
          obtain ⟨h1, h2, h3, h4, h5, h6, _⟩ :=
            VerificationContext_register_memory_access_ok_spec hreg_ma
          exact ⟨ht_valid, h1, h2, h3, h4, h5, h6⟩
    | PtrToArray id offset =>
      dsimp only at hload
      rcases bind_eq_ok hload with ⟨bound, _, hload⟩
      rcases bind_eq_ok hload with ⟨bw, _, hload⟩
      rcases bind_eq_ok hload with ⟨r1, _, hload⟩
      cases r1 with
      | Err _ => cases (Prod.mk.inj (ok_inj hload)).1
      | Ok _ =>
        dsimp only at hload
        rw [bind_ok_eq (Type_unknown_written_scalar_eq field.width)] at hload
        rcases bind_eq_ok hload with ⟨⟨r2, context1⟩, hreg_ma, hload⟩
        cases r2 with
        | Err _ => cases (Prod.mk.inj (ok_inj hload)).1
        | Ok u0 =>
          cases u0
          rcases Prod.mk.inj (ok_inj hload) with ⟨hok, rfl⟩
          cases core.result.Result.Ok.inj hok
          obtain ⟨h1, h2, h3, h4, h5, h6, _⟩ :=
            VerificationContext_register_memory_access_ok_spec hreg_ma
          exact ⟨unknownWrittenScalarWidth_valid _, h1, h2, h3, h4, h5, h6⟩
    | _ => cases (Prod.mk.inj (ok_inj hload)).1

theorem visit_load_forward
    {env : ExecEnv} {ctx ctx_out : verifier.ComputationContext}
    {vctx0 vctx1 : verifier.VerificationContext}
    {w : DataWidth} {dst base : Nat} {off16 : BitVec 16}
    (hctx : CompCtxValid env vctx0.counter.bv.toNat ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hfwd : VisitorDispatchNonLddw CompCtxVisitorInst ctx vctx0
      (.ok (.Ok (), ctx_out, vctx1)) (.load w dst base off16)) :
    ctx_out = ctx ∧ ForwardVisitPost env vctx0 vctx1 ctx_out := by
  rcases hfwd with ⟨dst8, src8, off, _, _, _, hcall⟩
  have hcall' := hcall.symm
  dsimp [CompCtxVisitorInst] at hcall'
  unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.load at hcall'
  rcases bind_eq_ok hcall' with ⟨vc1, hemit, hcall'⟩
  obtain ⟨he1, he2, he3, he4, he5, he6⟩ := VerificationContext_emit_log_ok_spec hemit
  rcases bind_eq_ok hcall' with ⟨r, hreg, hcall'⟩
  cases r with
  | Err _ => cases (Prod.mk.inj (ok_inj hcall')).1
  | Ok v =>
    dsimp only at hcall'
    obtain ⟨_, hv_valid⟩ := ComputationContext_reg_ok_valid hctx hreg
    rw [bind_ok_eq (Field_new_spec off (coreWidthOfSpec w))] at hcall'
    rcases bind_eq_ok hcall' with ⟨⟨r1, vc2⟩, hlm, hcall'⟩
    cases r1 with
    | Err _ => cases (Prod.mk.inj (ok_inj hcall')).1
    | Ok v1 =>
      obtain ⟨hv1_valid, hm1, hm2, hm3, hm4, hm5, hm6⟩ :=
        ComputationContext_load_memory_ok_valid hctx hv_valid hlm
      rcases bind_eq_ok hcall' with ⟨r2, hnext, hcall'⟩
      cases r2 with
      | Err _ => cases (Prod.mk.inj (ok_inj hcall')).1
      | Ok n =>
        dsimp only at hcall'
        obtain ⟨_, hn_eq⟩ := ComputationContext_next_ok_spec hnext
        have hn_valid : CompCtxValid env vctx0.counter.bv.toNat n := by rw [hn_eq]; exact hctx
        rcases bind_eq_ok hcall' with ⟨⟨r3, n1⟩, hset, hcall'⟩
        cases r3 with
        | Err _ => cases (Prod.mk.inj (ok_inj hcall')).1
        | Ok u0 =>
          cases u0
          obtain ⟨_, _, _, _, _, _, hn1_valid⟩ :=
            ComputationContext_set_reg_ok_spec hn_valid hv1_valid hset
          rcases bind_eq_ok hcall' with ⟨vc3, hpush, hcall'⟩
          rcases Prod.mk.inj (ok_inj hcall') with ⟨_, hpair⟩
          rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
          obtain ⟨hp1, hp2, hp3, hp4, hp5, _, hp7⟩ :=
            VerificationContext_push_successor_ok_spec hpush
          have hcnt : vc3.counter = vctx0.counter := hp3.trans (hm3.trans he3)
          refine ⟨rfl, hp1.trans (hm1.trans he1), hp2.trans (hm2.trans he2),
            hp4.trans (hm4.trans he4), hp5.trans (hm5.trans he5),
            by rw [hcnt], by rwa [hcnt], ?_⟩
          intro opt hopt
          rw [hp7, hm6, he6, hpending, List.nil_append, List.mem_singleton] at hopt
          subst hopt
          exact ⟨n1, rfl, by rwa [hcnt]⟩

theorem visit_load_backward
    {prog : Program} {env : ExecEnv} {vCounter F : Nat}
    {ctx ctx_out : verifier.ComputationContext}
    {vctx0 vctx1 vctx_back vctx_back1 : verifier.VerificationContext}
    {dPost : verifier.DataDependencies} {tracker : verifier.DependencyTracker}
    {w : DataWidth} {dst base : Nat} {off16 : BitVec 16}
    (hpc : ∃ hlt : ctx.pc.val < prog.size,
      prog[ctx.pc.val]'hlt = .load w dst base off16)
    (hctx : CompCtxValid env vCounter ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hfwd : VisitorDispatchNonLddw CompCtxVisitorInst ctx vctx0
      (.ok (.Ok (), ctx_out, vctx1)) (.load w dst base off16))
    (hchildren : ∀ succ, some succ ∈ vctx1.pending_successors.val →
      SemTerminatesIn prog env vCounter succ dPost F)
    (hback : VisitorDispatchNonLddw DepTrackerVisitorInst
      { context := ctx_out, deps := dPost } vctx_back
      (.ok (.Ok (), tracker, vctx_back1)) (.load w dst base off16)) :
    ctx_out = ctx ∧
    tracker.context = ctx ∧
    vctx_back1 = vctx_back ∧
    SemTerminatesIn prog env vCounter ctx tracker.deps (F + 1) := by
  rcases hfwd with ⟨dst8, src8, off, hdst, hbase, hoff, hcall⟩
  rcases hback with ⟨dst8_b, src8_b, off_b, hdst_b, hbase_b, hoff_b, hback_call⟩
  have hdst_u8 : dst8_b = dst8 := UScalar.eq_of_val_eq (hdst_b.symm.trans hdst)
  have hsrc_u8 : src8_b = src8 := UScalar.eq_of_val_eq (hbase_b.symm.trans hbase)
  have hoff_i16 : off_b = off := i16_eq_of_bv_eq (hoff_b.symm.trans hoff)
  subst dst8_b; subst src8_b; subst off_b
  subst dst; subst base; subst off16
  have hcall' := hcall.symm
  dsimp [CompCtxVisitorInst] at hcall'
  unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.load at hcall'
  rcases bind_eq_ok hcall' with ⟨vc1, hemit, hcall'⟩
  obtain ⟨_, _, _, _, _, he6⟩ := VerificationContext_emit_log_ok_spec hemit
  rcases bind_eq_ok hcall' with ⟨r_reg, hreg, hcall'⟩
  cases r_reg with
  | Err _ => cases (Prod.mk.inj (ok_inj hcall')).1
  | Ok v =>
    dsimp only at hcall'
    obtain ⟨hsrc_lt, hv_valid⟩ := ComputationContext_reg_ok_valid hctx hreg
    rw [bind_ok_eq (Field_new_spec off (coreWidthOfSpec w))] at hcall'
    rcases bind_eq_ok hcall' with ⟨⟨r1, vc2⟩, hlm, hcall'⟩
    cases r1 with
    | Err _ => cases (Prod.mk.inj (ok_inj hcall')).1
    | Ok v1 =>
      obtain ⟨hv1_valid, _, _, _, _, _, hm6⟩ :=
        ComputationContext_load_memory_ok_valid hctx hv_valid hlm
      rcases bind_eq_ok hcall' with ⟨r2, hnext, hcall'⟩
      cases r2 with
      | Err _ => cases (Prod.mk.inj (ok_inj hcall')).1
      | Ok n =>
        dsimp only at hcall'
        obtain ⟨hpc_next, hn_eq⟩ := ComputationContext_next_ok_spec hnext
        have hn_valid : CompCtxValid env vCounter n := by rw [hn_eq]; exact hctx
        rcases bind_eq_ok hcall' with ⟨⟨r3, n1⟩, hset, hcall'⟩
        cases r3 with
        | Err _ => cases (Prod.mk.inj (ok_inj hcall')).1
        | Ok u0 =>
          cases u0
          obtain ⟨hdst_lt10, hpc_n1, hregs_n1, hstk_n1, hbm_n1, hres_n1, _⟩ :=
            ComputationContext_set_reg_ok_spec hn_valid hv1_valid hset
          rcases bind_eq_ok hcall' with ⟨vc3, hpush, hcall'⟩
          rcases Prod.mk.inj (ok_inj hcall') with ⟨_, hpair⟩
          rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
          obtain ⟨_, _, _, _, _, _, hp7⟩ := VerificationContext_push_successor_ok_spec hpush
          have hn1_mem : some n1 ∈ vc3.pending_successors.val := by
            rw [hp7, hm6, he6, hpending]; simp
          have hsem := hchildren n1 hn1_mem
          have hn1_pc : n1.pc.val = ctx.pc.val + 1 := by rw [hpc_n1, hpc_next]
          have hback' := hback_call.symm
          dsimp [DepTrackerVisitorInst] at hback'
          unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.load at hback'
          rcases bind_eq_ok hback' with ⟨⟨_, dd⟩, hrem_dst, hback'⟩
          obtain ⟨_, _, _, hrem_reg, hrem_stk⟩ := DataDependencies_remove_reg_ok_spec hrem_dst
          rcases bind_eq_ok hback' with ⟨r_reg_b, hreg_b, hback'⟩
          rw [hreg] at hreg_b; cases ok_inj hreg_b
          dsimp only at hback'
          unfold verifier.ComputationContext.load_memory at hlm
          rcases bind_eq_ok hlm with ⟨r_in, hinner, hlm⟩
          cases r_in with
          | Err _ => cases (Prod.mk.inj (ok_inj hlm)).1
          | Ok a =>
            dsimp only at hlm
            rw [bind_ok_eq hinner] at hback'
            dsimp only at hback'
            have hv_ne_uninit : v ≠ .ScalarValue uninitScalar64 := by
              rintro rfl
              unfold verifier.Type.inner at hinner
              rcases bind_eq_ok hinner with ⟨rc, _, hinner⟩
              cases rc with
              | Err _ => cases ok_inj hinner
              | Ok _ =>
                dsimp only at hinner
                rw [bind_ok_eq (Type_unwrap_releasable_spec (.ScalarValue uninitScalar64))] at hinner
                cases core.result.Result.Ok.inj (ok_inj hinner)
                cases (Prod.mk.inj (ok_inj hlm)).1
            obtain ⟨hlt_pc, hinsn_at⟩ := hpc
            have hn_src_ge : ¬ (src8.val ≥ REGISTER_COUNT) := by omega
            cases a with
            | PtrToStack stack_offset =>
              dsimp only at hlm hback'
              rcases bind_eq_ok hlm with ⟨offset_sum, hadd_off, hlm⟩
              rw [bind_ok_eq hadd_off] at hback'
              rcases bind_eq_ok hback' with ⟨r_chk, hchk_off, hback'⟩
              cases r_chk with
              | Err _ => cases (Prod.mk.inj (ok_inj hback')).1
              | Ok u_chk =>
                cases u_chk
                dsimp only at hback'
                rcases bind_eq_ok hback' with ⟨idx, hidx, hback'⟩
                rcases bind_eq_ok hback' with ⟨dd1, hadd_stk, hback'⟩
                rcases bind_eq_ok hback' with ⟨dd2, hadd_src, hback'⟩
                rcases Prod.mk.inj (ok_inj hback') with ⟨_, hp_b⟩
                rcases Prod.mk.inj hp_b with ⟨rfl, rfl⟩
                obtain ⟨hidx_lt64, _, hstk_reg, hstk_stk, _⟩ := DataDependencies_add_stack_ok_spec hadd_stk
                obtain ⟨hsrc_lt16, _, hsrc_reg_eq, hsrc_stk_eq, _⟩ := DataDependencies_add_reg_ok_spec hadd_src
                have hsrc_live : DataDepsHasReg dd2 src8.val = true := by
                  rw [hsrc_reg_eq src8.val hsrc_lt16]; simp
                have hslot_live_idx : DataDepsHasStack dd2 idx.val = true := by
                  rw [hsrc_stk_eq idx.val, hstk_stk idx.val hidx_lt64]; simp
                rcases bind_eq_ok hlm with ⟨r_sl, hsl, hlm⟩
                cases r_sl with
                | Err _ => cases (Prod.mk.inj (ok_inj hlm)).1
                | Ok tOut =>
                  dsimp only at hlm
                  rcases bind_eq_ok hlm with ⟨⟨r_ma, vc_ma⟩, _, hlm⟩
                  cases r_ma with
                  | Err _ => cases (Prod.mk.inj (ok_inj hlm)).1
                  | Ok _ =>
                    cases core.result.Result.Ok.inj (Prod.mk.inj (ok_inj hlm)).1
                    refine ⟨rfl, rfl, rfl, ?_⟩
                    intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
                    obtain ⟨_, _, hrel_src⟩ :=
                      ComputationContext_reg_ok_valid_and_rel2 hctx hrel (Or.inr hsrc_live) hreg
                    obtain ⟨hu_eq, _, hca1, hca2⟩ :=
                      Type_inner_ok_valid_and_checkAlive_none hctx hrel hv_valid hv_ne_uninit hinner hrel_src
                    obtain ⟨_, _, baseOff, hconc_base, hv_base1, hv_base2⟩ :=
                      unwrap_stack_ptr_rel2 hv_valid hu_eq.symm hrel_src
                    obtain ⟨off_s, hadd_s, hconc_sum⟩ :=
                      StackOffset_add_i16_sound_2safety stack_offset off hconc_base
                    rw [hadd_off] at hadd_s; cases ok_inj hadd_s
                    obtain ⟨rchk0, hrchk0, hchk_iff⟩ := StackOffset_check_valid_offset_spec offset_sum
                    rw [hchk_off] at hrchk0; cases ok_inj hrchk0
                    obtain ⟨hoff_kn, hoff_lt⟩ := (hchk_iff ()).mp rfl
                    have heff_eq : baseOff + sextOffset16 off.bv = offset_sum.value.bv :=
                      eq_value_of_known_64 hoff_kn hconc_sum.1
                    obtain ⟨idx0, hidx0, hidx_val⟩ := StackOffset_array_index_spec (BitVec.le_of_lt hoff_lt)
                    rw [hidx] at hidx0; cases ok_inj hidx0
                    have hslot_idx_eq : (baseOff + sextOffset16 off.bv).toNat / 8 = idx.val := by
                      rw [heff_eq, hidx_val]
                    have hslot_rel := hrel.2.2.2.2.2.2.2.2.2.2.2
                      ((baseOff + sextOffset16 off.bv).toNat / 8)
                      (by rw [hslot_idx_eq]; exact hidx_lt64)
                      (by rw [hslot_idx_eq]; exact hslot_live_idx)
                    obtain ⟨heff_lt512, hal, _, vOut₁, vOut₂, hmatch1, hmatch2, hrel_out⟩ :=
                      Stack_load_rel2 hctx.2.1 hconc_sum hsl hslot_rel
                    rw [DataWidth_ofCore_coreWidthOfSpec] at hal hmatch1 hmatch2
                    let st₁' : MachineState :=
                      { st₁ with pc := n1.pc.val, regs := updateReg st₁.regs dst8.val vOut₁,
                                 world := bumpWorldStep st₁.world }
                    let st₂' : MachineState :=
                      { st₂ with pc := n1.pc.val, regs := updateReg st₂.regs dst8.val vOut₂,
                                 world := bumpWorldStep st₂.world }
                    have hrel_post : StateRel2Deps env n1 dPost ρ resActive isNullId st₁' st₂' :=
                      StateRel2Deps_set_reg (by unfold GENERAL_REGISTER_COUNT at hdst_lt10; exact hdst_lt10)
                        (hregs_n1.trans (by rw [hn_eq]))
                        (hstk_n1.trans (by rw [hn_eq]))
                        (hbm_n1.trans (by rw [hn_eq]))
                        (hres_n1.trans (by rw [hn_eq]))
                        (fun r hr hne hd => by
                          rw [hsrc_reg_eq r (by omega), hstk_reg r, hrem_reg r (by omega)]
                          simp [hd, hne])
                        (fun s hs hd => by
                          rw [hsrc_stk_eq s, hstk_stk s hs, hrem_stk s, hd, Bool.true_or])
                        hrel
                        (fun _ => hrel_out)
                    have halloc' : (bumpWorldStep st₁.world).nextAllocId + 3 * F < 2 ^ 63 := by
                      simp only [bumpWorldStep]; omega
                    obtain ⟨ret, finalWorld, hr₁, hr₂, hlive_fin⟩ :=
                      hsem l₁ l₂ s₁ s₂ ρ resActive isNullId st₁' st₂' hl₁ hl₂ hleak halloc' hρ hrel_post
                    have hn_ge512 : ¬ (baseOff + sextOffset16 off.bv ≥ 512#64) :=
                      BitVec.not_le.mpr heff_lt512
                    have hmis_false : (((baseOff + sextOffset16 off.bv).toNat % 8) % w.bytes != 0) = false :=
                      bne_eq_false_iff_eq.mpr hal
                    refine ⟨ret, finalWorld, ?_, ?_, hlive_fin⟩
                    · unfold runSteps step
                      simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, checkDst_eq_ok hdst_lt10,
                        hn_src_ge, ↓reduceIte, hv_base1]
                      rw [hv_base1] at hca1
                      simp only [hca1, hn_ge512, ↓reduceIte, hmis_false, Bool.false_eq_true]
                      cases hslot1 : st₁.stack ((baseOff + sextOffset16 off.bv).toNat / 8) with
                      | spilled cap =>
                        rw [hslot1] at hmatch1
                        rcases hmatch1 with ⟨rfl, rfl⟩
                        simp only [beq_self_eq_true, ↓reduceIte]
                        rw [stepNextIte_eq_next (by dsimp [st₁']; rw [hn1_pc]) hr₁]
                        exact hr₁
                      | bytes val initMask =>
                        rw [hslot1] at hmatch1
                        rcases hmatch1 with ⟨hinit_eq, rfl⟩
                        have hbne_init : (extractSubValue initMask ((baseOff + sextOffset16 off.bv).toNat % 8) w != w.valMask64) = false :=
                          bne_eq_false_iff_eq.mpr hinit_eq
                        simp only [hbne_init, Bool.false_eq_true, ↓reduceIte]
                        rw [stepNextIte_eq_next (by dsimp [st₁']; rw [hn1_pc]) hr₁]
                        exact hr₁
                    · unfold runSteps step
                      simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, checkDst_eq_ok hdst_lt10,
                        hn_src_ge, ↓reduceIte, hv_base2]
                      rw [hv_base2] at hca2
                      simp only [hca2, hn_ge512, ↓reduceIte, hmis_false, Bool.false_eq_true]
                      cases hslot2 : st₂.stack ((baseOff + sextOffset16 off.bv).toNat / 8) with
                      | spilled cap =>
                        rw [hslot2] at hmatch2
                        rcases hmatch2 with ⟨rfl, rfl⟩
                        simp only [beq_self_eq_true, ↓reduceIte]
                        rw [stepNextIte_eq_next (by dsimp [st₂']; rw [hn1_pc]) hr₂]
                        exact hr₂
                      | bytes val initMask =>
                        rw [hslot2] at hmatch2
                        rcases hmatch2 with ⟨hinit_eq, rfl⟩
                        have hbne_init : (extractSubValue initMask ((baseOff + sextOffset16 off.bv).toNat % 8) w != w.valMask64) = false :=
                          bne_eq_false_iff_eq.mpr hinit_eq
                        simp only [hbne_init, Bool.false_eq_true, ↓reduceIte]
                        rw [stepNextIte_eq_next (by dsimp [st₂']; rw [hn1_pc]) hr₂]
                        exact hr₂
            | PtrToMemory id offset buffer_size orig =>
              dsimp only at hlm hback'
              rcases bind_eq_ok hback' with ⟨dd1, hadd_src, hback'⟩
              rcases Prod.mk.inj (ok_inj hback') with ⟨_, hp_b⟩
              rcases Prod.mk.inj hp_b with ⟨rfl, rfl⟩
              obtain ⟨hsrc_lt16, _, hsrc_reg_eq, hsrc_stk_eq, _⟩ := DataDependencies_add_reg_ok_spec hadd_src
              have hsrc_live : DataDepsHasReg dd1 src8.val = true := by
                rw [hsrc_reg_eq src8.val hsrc_lt16]; simp
              rcases bind_eq_ok hlm with ⟨bw, hbw, hlm⟩
              rcases bind_eq_ok hlm with ⟨r_chk, hchk, hlm⟩
              cases r_chk with
              | Err _ => cases (Prod.mk.inj (ok_inj hlm)).1
              | Ok u_chk =>
                cases u_chk
                dsimp only at hlm
                rw [bind_ok_eq (Type_unknown_written_scalar_eq (coreWidthOfSpec w))] at hlm
                rcases bind_eq_ok hlm with ⟨⟨r_ma, vc_ma⟩, _, hlm⟩
                cases r_ma with
                | Err _ => cases (Prod.mk.inj (ok_inj hlm)).1
                | Ok _ =>
                  cases core.result.Result.Ok.inj (Prod.mk.inj (ok_inj hlm)).1
                  refine ⟨rfl, rfl, rfl, ?_⟩
                  intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
                  obtain ⟨_, _, hrel_src⟩ :=
                    ComputationContext_reg_ok_valid_and_rel2 hctx hrel (Or.inr hsrc_live) hreg
                  obtain ⟨hu_eq, _, hca1, hca2⟩ :=
                    Type_inner_ok_valid_and_checkAlive_none hctx hrel hv_valid hv_ne_uninit hinner hrel_src
                  obtain ⟨_, _, relOpt, baseOff, hconc_base, hv_base1, hv_base2⟩ :=
                    unwrap_mem_ptr_rel2 hv_valid hu_eq.symm hrel_src
                  have hin_bnds : inBoundsRange (baseOff + sextOffset16 off.bv) w buffer_size.bv = true := by
                    simpa only [DataWidth_ofCore_coreWidthOfSpec] using
                      ComputationContext_check_memory_access_ok_spec hbw hconc_base.1 hchk
                  let raw := readBufWord st₁.world.bufBytes (renameMemId ρ id) (baseOff + sextOffset16 off.bv) w
                  let st₁' : MachineState :=
                    { st₁ with pc := n1.pc.val, regs := updateReg st₁.regs dst8.val (.scalar raw),
                               world := bumpWorldStep st₁.world }
                  let st₂' : MachineState :=
                    { st₂ with pc := n1.pc.val, regs := updateReg st₂.regs dst8.val (.scalar raw),
                               world := bumpWorldStep st₂.world }
                  have hrel_post : StateRel2Deps env n1 dPost ρ resActive isNullId st₁' st₂' :=
                    StateRel2Deps_set_reg (by unfold GENERAL_REGISTER_COUNT at hdst_lt10; exact hdst_lt10)
                      (hregs_n1.trans (by rw [hn_eq]))
                      (hstk_n1.trans (by rw [hn_eq]))
                      (hbm_n1.trans (by rw [hn_eq]))
                      (hres_n1.trans (by rw [hn_eq]))
                      (fun r hr hne hd => by
                        rw [hsrc_reg_eq r (by omega), hrem_reg r (by omega)]
                        simp [hd, hne])
                      (fun s _ hd => by rw [hsrc_stk_eq s, hrem_stk s]; exact hd)
                      hrel
                      (fun _ => by
                        rw [DataWidth_ofCore_coreWidthOfSpec]
                        exact TypeRel2_scalar_rel2_of_concretizes
                          (concretizesScalar64Rel2_readBufWord _ _ _ w))
                  have halloc' : (bumpWorldStep st₁.world).nextAllocId + 3 * F < 2 ^ 63 := by
                    simp only [bumpWorldStep]; omega
                  obtain ⟨ret, finalWorld, hr₁, hr₂, hlive_fin⟩ :=
                    hsem l₁ l₂ s₁ s₂ ρ resActive isNullId st₁' st₂' hl₁ hl₂ hleak halloc' hρ hrel_post
                  refine ⟨ret, finalWorld, ?_, ?_, hlive_fin⟩
                  · unfold runSteps step
                    rw [hv_base1] at hca1
                    simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, checkDst_eq_ok hdst_lt10,
                      hn_src_ge, ↓reduceIte, hv_base1, hca1, hin_bnds, Bool.not_true, Bool.false_eq_true]
                    rw [stepNextIte_eq_next (by dsimp [st₁', raw]; rw [hn1_pc]) hr₁]
                    exact hr₁
                  · unfold runSteps step
                    rw [hv_base2] at hca2
                    simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, checkDst_eq_ok hdst_lt10,
                      hn_src_ge, ↓reduceIte, hv_base2, hca2, hin_bnds, Bool.not_true, Bool.false_eq_true]
                    rw [stepNextIte_eq_next (by dsimp [st₂', raw]; rw [hn1_pc, ← hrel.2.2.1]) hr₂]
                    exact hr₂
            | PtrToArray id offset =>
              dsimp only at hlm hback'
              rcases bind_eq_ok hback' with ⟨dd1, hadd_src, hback'⟩
              rcases Prod.mk.inj (ok_inj hback') with ⟨_, hp_b⟩
              rcases Prod.mk.inj hp_b with ⟨rfl, rfl⟩
              obtain ⟨hsrc_lt16, _, hsrc_reg_eq, hsrc_stk_eq, _⟩ := DataDependencies_add_reg_ok_spec hadd_src
              have hsrc_live : DataDepsHasReg dd1 src8.val = true := by
                rw [hsrc_reg_eq src8.val hsrc_lt16]; simp
              rcases bind_eq_ok hlm with ⟨bound, hbm_get, hlm⟩
              rcases bind_eq_ok hlm with ⟨bw, hbw, hlm⟩
              rcases bind_eq_ok hlm with ⟨r_chk, hchk, hlm⟩
              cases r_chk with
              | Err _ => cases (Prod.mk.inj (ok_inj hlm)).1
              | Ok u_chk =>
                cases u_chk
                dsimp only at hlm
                rw [bind_ok_eq (Type_unknown_written_scalar_eq (coreWidthOfSpec w))] at hlm
                rcases bind_eq_ok hlm with ⟨⟨r_ma, vc_ma⟩, _, hlm⟩
                cases r_ma with
                | Err _ => cases (Prod.mk.inj (ok_inj hlm)).1
                | Ok _ =>
                  cases core.result.Result.Ok.inj (Prod.mk.inj (ok_inj hlm)).1
                  refine ⟨rfl, rfl, rfl, ?_⟩
                  intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
                  obtain ⟨_, _, hrel_src⟩ :=
                    ComputationContext_reg_ok_valid_and_rel2 hctx hrel (Or.inr hsrc_live) hreg
                  obtain ⟨hu_eq, _, hca1, hca2⟩ :=
                    Type_inner_ok_valid_and_checkAlive_none hctx hrel hv_valid hv_ne_uninit hinner hrel_src
                  obtain ⟨_, _, _, baseOff, hconc_base, hv_base1, hv_base2⟩ :=
                    unwrap_array_ptr_rel2 hv_valid hu_eq.symm hrel_src
                  obtain ⟨b0, hb0, hb_cases⟩ := BoundsMap_get_or_zero_spec hctx.2.2.1 id
                  rw [hbm_get] at hb0; cases ok_inj hb0
                  have hb_le_arr : bound.bv ≤ st₁.world.arrayLen (renameMemId ρ id) := by
                    rcases hb_cases with ⟨rfl, _⟩ | hmem_bm
                    · exact BitVec.le_def.mpr (Nat.zero_le _)
                    · exact hrel.2.2.2.2.2.2.2.1 id bound hmem_bm
                  have hin_bound : inBoundsRange (baseOff + sextOffset16 off.bv) w bound.bv = true := by
                    simpa only [DataWidth_ofCore_coreWidthOfSpec] using
                      ComputationContext_check_memory_access_ok_spec hbw hconc_base.1 hchk
                  have hin_arr1 := inBoundsRange_mono_right hin_bound hb_le_arr
                  have hin_arr2 : inBoundsRange (baseOff + sextOffset16 off.bv) w
                      (st₂.world.arrayLen (renameMemId ρ id)) = true := by
                    rwa [← hrel.2.2.1]
                  let raw := readBufWord st₁.world.bufBytes (renameMemId ρ id) (baseOff + sextOffset16 off.bv) w
                  let st₁' : MachineState :=
                    { st₁ with pc := n1.pc.val, regs := updateReg st₁.regs dst8.val (.scalar raw),
                               world := bumpWorldStep st₁.world }
                  let st₂' : MachineState :=
                    { st₂ with pc := n1.pc.val, regs := updateReg st₂.regs dst8.val (.scalar raw),
                               world := bumpWorldStep st₂.world }
                  have hrel_post : StateRel2Deps env n1 dPost ρ resActive isNullId st₁' st₂' :=
                    StateRel2Deps_set_reg (by unfold GENERAL_REGISTER_COUNT at hdst_lt10; exact hdst_lt10)
                      (hregs_n1.trans (by rw [hn_eq]))
                      (hstk_n1.trans (by rw [hn_eq]))
                      (hbm_n1.trans (by rw [hn_eq]))
                      (hres_n1.trans (by rw [hn_eq]))
                      (fun r hr hne hd => by
                        rw [hsrc_reg_eq r (by omega), hrem_reg r (by omega)]
                        simp [hd, hne])
                      (fun s _ hd => by rw [hsrc_stk_eq s, hrem_stk s]; exact hd)
                      hrel
                      (fun _ => by
                        rw [DataWidth_ofCore_coreWidthOfSpec]
                        exact TypeRel2_scalar_rel2_of_concretizes
                          (concretizesScalar64Rel2_readBufWord _ _ _ w))
                  have halloc' : (bumpWorldStep st₁.world).nextAllocId + 3 * F < 2 ^ 63 := by
                    simp only [bumpWorldStep]; omega
                  obtain ⟨ret, finalWorld, hr₁, hr₂, hlive_fin⟩ :=
                    hsem l₁ l₂ s₁ s₂ ρ resActive isNullId st₁' st₂' hl₁ hl₂ hleak halloc' hρ hrel_post
                  refine ⟨ret, finalWorld, ?_, ?_, hlive_fin⟩
                  · unfold runSteps step
                    rw [hv_base1] at hca1
                    simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, checkDst_eq_ok hdst_lt10,
                      hn_src_ge, ↓reduceIte, hv_base1, hca1, hin_arr1, Bool.not_true, Bool.false_eq_true]
                    rw [stepNextIte_eq_next (by dsimp [st₁', raw]; rw [hn1_pc]) hr₁]
                    exact hr₁
                  · unfold runSteps step
                    rw [hv_base2] at hca2
                    simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, checkDst_eq_ok hdst_lt10,
                      hn_src_ge, ↓reduceIte, hv_base2, hca2, hin_arr2, Bool.not_true, Bool.false_eq_true]
                    rw [stepNextIte_eq_next (by dsimp [st₂', raw]; rw [hn1_pc, ← hrel.2.2.1]) hr₂]
                    exact hr₂
            | PtrToStruct id offset descriptor =>
              dsimp only at hlm hback'
              rcases bind_eq_ok hback' with ⟨dd1, hadd_src, hback'⟩
              rcases Prod.mk.inj (ok_inj hback') with ⟨_, hp_b⟩
              rcases Prod.mk.inj hp_b with ⟨rfl, rfl⟩
              obtain ⟨hsrc_lt16, _, hsrc_reg_eq, hsrc_stk_eq, _⟩ := DataDependencies_add_reg_ok_spec hadd_src
              have hsrc_live : DataDepsHasReg dd1 src8.val = true := by
                rw [hsrc_reg_eq src8.val hsrc_lt16]; simp
              rcases bind_eq_ok hlm with ⟨o_fd, hfind, hlm⟩
              cases o_fd with
              | none => cases (Prod.mk.inj (ok_inj hlm)).1
              | some fd =>
                dsimp only at hlm
                rcases bind_eq_ok hlm with ⟨⟨return_type, _⟩, hft_br, hlm⟩
                rcases bind_eq_ok hlm with ⟨_, _, hlm⟩
                rcases bind_eq_ok hlm with ⟨⟨r_ma, vc_ma⟩, _, hlm⟩
                cases r_ma with
                | Err _ => cases (Prod.mk.inj (ok_inj hlm)).1
                | Ok _ =>
                  have hret_eq : return_type = v1 :=
                    core.result.Result.Ok.inj (Prod.mk.inj (ok_inj hlm)).1
                  subst hret_eq
                  refine ⟨rfl, rfl, rfl, ?_⟩
                  intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
                  obtain ⟨_, _, hrel_src⟩ :=
                    ComputationContext_reg_ok_valid_and_rel2 hctx hrel (Or.inr hsrc_live) hreg
                  obtain ⟨hu_eq, _, hca1, hca2⟩ :=
                    Type_inner_ok_valid_and_checkAlive_none hctx hrel hv_valid hv_ne_uninit hinner hrel_src
                  obtain ⟨_, _, ⟨schemaId, ⟨hschema_nv, hfields_nv, henv_fields⟩, hmatch_id⟩,
                      relOpt, baseOff, hconc_base, hv_base1, hv_base2⟩ :=
                    unwrap_struct_ptr_rel2 hv_valid hu_eq.symm hrel_src
                  have hmatch_ren : memoryIdMatches schemaId (renameMemId ρ id) = true := by
                    rw [renameMemId_matches_noVerif ρ id hschema_nv]; exact hmatch_id
                  have henv_eq := henv_fields (renameMemId ρ id) hmatch_ren
                  obtain ⟨_, _, hal_eff, hfind_spec, hfd_mem, hkind_spec⟩ :=
                    StructDescriptor_find_field_ok_some_spec hconc_base hfind
                  dsimp only at hkind_spec
                  rw [DataWidth_ofCore_coreWidthOfSpec] at hal_eff hkind_spec
                  rw [← henv_eq] at hfind_spec
                  have hfd_nv := hfields_nv fd hfd_mem
                  let effOff := baseOff + sextOffset16 off.bv
                  have hzero_ptr32 : PtrOffsetValid 0xFFFF_FFFF#64 (knownScalar64 0#u64) :=
                    ⟨knownScalar64_valid 0#u64, rfl, by decide⟩
                  have hzero_pkt : PtrOffsetValid MAX_PACKET_OFFSET (knownScalar64 0#u64) :=
                    ⟨knownScalar64_valid 0#u64, rfl, by decide⟩
                  have hstep_load : ∃ vLoad : RuntimeVal,
                      TypeRel2 env ρ resActive isNullId st₁.world.arrayLen return_type vLoad vLoad ∧
                      structFieldStepOut prog (ctx.pc.val + 1) dst8.val st₁.regs st₁.stack st₁.world
                        (renameMemId ρ id) effOff w (StructFieldDesc.ofCore fd) =
                        stepNextPcAux prog (ctx.pc.val + 1) (updateReg st₁.regs dst8.val vLoad) st₁.stack st₁.world ∧
                      structFieldStepOut prog (ctx.pc.val + 1) dst8.val st₂.regs st₂.stack st₂.world
                        (renameMemId ρ id) effOff w (StructFieldDesc.ofCore fd) =
                        stepNextPcAux prog (ctx.pc.val + 1) (updateReg st₂.regs dst8.val vLoad) st₂.stack st₂.world := by
                    have hsb_eq : st₂.world.structBytes = st₁.world.structBytes := by rw [hrel.2.2.1]
                    have hsfn_eq : st₂.world.structFieldNull = st₁.world.structFieldNull := by rw [hrel.2.2.1]
                    cases hft : fd.field_type <;> rw [hft] at hft_br hkind_spec hfd_nv
                    · -- .Scalar sz
                      simp only [Type_unknown_written_scalar_eq, DataWidth_ofCore_coreWidthOfSpec, bind_ok] at hft_br
                      rcases Prod.mk.inj (ok_inj hft_br) with ⟨rfl, _⟩
                      dsimp only [structFieldStepOut, StructFieldDesc.ofCore, effOff]
                      rw [hft]
                      dsimp only
                      dsimp only [StructFieldDesc.ofCore] at hkind_spec
                      refine ⟨.scalar (readBufWord st₁.world.structBytes (renameMemId ρ id) effOff w),
                        TypeRel2_scalar_rel2_of_concretizes (concretizesScalar64Rel2_readBufWord _ _ _ w), ?_, ?_⟩
                      · simp only [hkind_spec, Bool.not_true, Bool.false_eq_true, ↓reduceIte]; rfl
                      · simp only [hkind_spec, Bool.not_true, Bool.false_eq_true, ↓reduceIte, hsb_eq]; rfl
                    · -- .MutableScalar sz
                      simp only [Type_unknown_written_scalar_eq, DataWidth_ofCore_coreWidthOfSpec, bind_ok] at hft_br
                      rcases Prod.mk.inj (ok_inj hft_br) with ⟨rfl, _⟩
                      dsimp only [structFieldStepOut, StructFieldDesc.ofCore, effOff]
                      rw [hft]
                      dsimp only
                      dsimp only [StructFieldDesc.ofCore] at hkind_spec
                      refine ⟨.scalar (readBufWord st₁.world.structBytes (renameMemId ρ id) effOff w),
                        TypeRel2_scalar_rel2_of_concretizes (concretizesScalar64Rel2_readBufWord _ _ _ w), ?_, ?_⟩
                      · simp only [hkind_spec, Bool.not_true, Bool.false_eq_true, ↓reduceIte]; rfl
                      · simp only [hkind_spec, Bool.not_true, Bool.false_eq_true, ↓reduceIte, hsb_eq]; rfl
                    · -- .PtrToMemory is_32_bit memory_id buffer_size
                      rename_i is_32_bit memory_id buffer_size
                      dsimp only at hft_br
                      rw [bind_ok_eq (MemoryId_clone_spec id),
                          bind_ok_eq (MemoryId_prepended_spec memory_id id),
                          bind_ok_eq (from_known_64_eq 0#u64)] at hft_br
                      simp only [lift_eq_ok, bind_ok] at hft_br
                      rcases Prod.mk.inj (ok_inj hft_br) with ⟨rfl, _⟩
                      rcases hkind_spec with ⟨heff_off, hw_exp⟩
                      have hren_prep : renameMemId ρ (memoryIdPrepend memory_id id) =
                          memoryIdPrepend memory_id (renameMemId ρ id) := by
                        rw [renameMemId_prepend, renameMemId_noVerif_id ρ hfd_nv]
                      have hcast_sz : U64.bv (UScalar.cast .U64 buffer_size) = BitVec.ofNat 64 buffer_size.val :=
                        uscalar_cast_u64_of_usize_bv buffer_size
                      dsimp only [structFieldStepOut, StructFieldDesc.ofCore]
                      rw [hft]
                      dsimp only
                      have hbne1 : (effOff != (StructFieldDesc.ofCore fd).offset) = false :=
                        bne_eq_false_iff_eq.mpr heff_off
                      have hbne2 : (w != (if is_32_bit then DataWidth.u32 else .u64)) = false :=
                        bne_eq_false_iff_eq.mpr hw_exp
                      refine ⟨.memPtr (memoryIdPrepend memory_id (renameMemId ρ id)) .kernelReadOnly none
                          (BitVec.ofNat 64 buffer_size.val) 0#64, ?_, ?_, ?_⟩
                      · refine ⟨hzero_ptr32, 0#64, concretizesScalar64Rel2_known 0#u64, ?_, ?_⟩
                        · rw [hren_prep, hcast_sz]; rfl
                        · rw [hren_prep, hcast_sz]; rfl
                      · simp only [StructFieldDesc.ofCore, hft] at hbne1
                        simp only [hbne1, hbne2, Bool.false_or, Bool.false_and, Bool.false_eq_true, ↓reduceIte]
                      · simp only [StructFieldDesc.ofCore, hft] at hbne1
                        simp only [hbne1, hbne2, Bool.false_or, Bool.false_and, Bool.false_eq_true, ↓reduceIte]
                    · -- .NullablePtrToMemory is_32_bit memory_id buffer_size
                      rename_i is_32_bit memory_id buffer_size
                      dsimp only at hft_br
                      rw [bind_ok_eq (MemoryId_clone_spec id),
                          bind_ok_eq (MemoryId_prepended_spec memory_id id),
                          bind_ok_eq (MemoryId_clone_spec (memoryIdPrepend memory_id id)),
                          bind_ok_eq (from_known_64_eq 0#u64)] at hft_br
                      simp only [lift_eq_ok, bind_ok] at hft_br
                      rcases Prod.mk.inj (ok_inj hft_br) with ⟨rfl, _⟩
                      rcases hkind_spec with ⟨heff_off, hw_exp⟩
                      have hren_prep : renameMemId ρ (memoryIdPrepend memory_id id) =
                          memoryIdPrepend memory_id (renameMemId ρ id) := by
                        rw [renameMemId_prepend, renameMemId_noVerif_id ρ hfd_nv]
                      have hns_ne : (memoryIdPrepend memory_id (renameMemId ρ id)).namespace ≠ .Verification := by
                        rw [prepend_root_ns]; exact noVerif_root_ne_verif hfd_nv
                      have hnull_eq := hrel.2.2.2.2.1 _ hns_ne
                      have hcast_sz : U64.bv (UScalar.cast .U64 buffer_size) = BitVec.ofNat 64 buffer_size.val :=
                        uscalar_cast_u64_of_usize_bv buffer_size
                      dsimp only [structFieldStepOut, StructFieldDesc.ofCore]
                      rw [hft]
                      dsimp only
                      have hbne1 : (effOff != (StructFieldDesc.ofCore fd).offset) = false :=
                        bne_eq_false_iff_eq.mpr heff_off
                      have hbne2 : (w != (if is_32_bit then DataWidth.u32 else .u64)) = false :=
                        bne_eq_false_iff_eq.mpr hw_exp
                      let vRes := if st₁.world.structFieldNull (memoryIdPrepend memory_id (renameMemId ρ id)) then
                          RuntimeVal.scalar 0#64
                        else .memPtr (memoryIdPrepend memory_id (renameMemId ρ id)) .kernelReadOnly none
                          (BitVec.ofNat 64 buffer_size.val) 0#64
                      refine ⟨vRes, ?_, ?_, ?_⟩
                      · dsimp only [TypeRel2, vRes]
                        rw [hren_prep, hnull_eq]
                        cases hn_sf : st₁.world.structFieldNull (memoryIdPrepend memory_id (renameMemId ρ id))
                        · simp only [Bool.false_eq_true, ↓reduceIte]
                          refine ⟨hzero_ptr32, 0#64, concretizesScalar64Rel2_known 0#u64, ?_, ?_⟩
                          · rw [hcast_sz]; rfl
                          · rw [hcast_sz]; rfl
                        · simp only [↓reduceIte, nullInnerInactive, and_self]
                      · simp only [StructFieldDesc.ofCore, hft] at hbne1
                        simp only [hbne1, hbne2, Bool.false_or, Bool.true_and, Bool.false_eq_true, ↓reduceIte]
                        rfl
                      · simp only [StructFieldDesc.ofCore, hft] at hbne1
                        simp only [hbne1, hbne2, Bool.false_or, Bool.true_and, Bool.false_eq_true, ↓reduceIte, hsfn_eq]
                        rfl
                    · -- .PtrToArray is_32_bit array_id
                      rename_i is_32_bit array_id
                      dsimp only at hft_br
                      rw [bind_ok_eq (MemoryId_clone_spec id),
                          bind_ok_eq (MemoryId_prepended_spec array_id id),
                          bind_ok_eq (from_known_64_eq 0#u64)] at hft_br
                      rcases Prod.mk.inj (ok_inj hft_br) with ⟨rfl, _⟩
                      rcases hkind_spec with ⟨heff_off, hw_exp⟩
                      have hren_prep : renameMemId ρ (memoryIdPrepend array_id id) =
                          memoryIdPrepend array_id (renameMemId ρ id) := by
                        rw [renameMemId_prepend, renameMemId_noVerif_id ρ hfd_nv]
                      dsimp only [structFieldStepOut, StructFieldDesc.ofCore]
                      rw [hft]
                      dsimp only
                      have hbne1 : (effOff != (StructFieldDesc.ofCore fd).offset) = false :=
                        bne_eq_false_iff_eq.mpr heff_off
                      have hbne2 : (w != (if is_32_bit then DataWidth.u32 else .u64)) = false :=
                        bne_eq_false_iff_eq.mpr hw_exp
                      refine ⟨.arrayPtr (memoryIdPrepend array_id (renameMemId ρ id)) 0#64, ?_, ?_, ?_⟩
                      · refine ⟨hzero_pkt, hrel.2.2.2.1 _, 0#64, concretizesScalar64Rel2_known 0#u64, ?_, ?_⟩
                        · rw [hren_prep]
                        · rw [hren_prep]
                      · simp only [StructFieldDesc.ofCore, hft] at hbne1
                        simp only [hbne1, hbne2, Bool.false_or, Bool.false_eq_true, ↓reduceIte]
                      · simp only [StructFieldDesc.ofCore, hft] at hbne1
                        simp only [hbne1, hbne2, Bool.false_or, Bool.false_eq_true, ↓reduceIte]
                    · -- .PtrToEndArray is_32_bit array_id
                      rename_i is_32_bit array_id
                      dsimp only at hft_br
                      rw [bind_ok_eq (MemoryId_clone_spec id),
                          bind_ok_eq (MemoryId_prepended_spec array_id id)] at hft_br
                      rcases Prod.mk.inj (ok_inj hft_br) with ⟨rfl, _⟩
                      rcases hkind_spec with ⟨heff_off, hw_exp⟩
                      have hren_prep : renameMemId ρ (memoryIdPrepend array_id id) =
                          memoryIdPrepend array_id (renameMemId ρ id) := by
                        rw [renameMemId_prepend, renameMemId_noVerif_id ρ hfd_nv]
                      dsimp only [structFieldStepOut, StructFieldDesc.ofCore]
                      rw [hft]
                      dsimp only
                      have hbne1 : (effOff != (StructFieldDesc.ofCore fd).offset) = false :=
                        bne_eq_false_iff_eq.mpr heff_off
                      have hbne2 : (w != (if is_32_bit then DataWidth.u32 else .u64)) = false :=
                        bne_eq_false_iff_eq.mpr hw_exp
                      refine ⟨.endArrayPtr (memoryIdPrepend array_id (renameMemId ρ id)), ?_, ?_, ?_⟩
                      · refine ⟨hrel.2.2.2.1 _, ?_, ?_⟩
                        · rw [hren_prep]
                        · rw [hren_prep]
                      · simp only [StructFieldDesc.ofCore, hft] at hbne1
                        simp only [hbne1, hbne2, Bool.false_or, Bool.false_eq_true, ↓reduceIte]
                      · simp only [StructFieldDesc.ofCore, hft] at hbne1
                        simp only [hbne1, hbne2, Bool.false_or, Bool.false_eq_true, ↓reduceIte]
                  rcases hstep_load with ⟨vLoad, hrel_load, hst1_eq, hst2_eq⟩
                  let st₁' : MachineState :=
                    { st₁ with pc := n1.pc.val, regs := updateReg st₁.regs dst8.val vLoad,
                               world := bumpWorldStep st₁.world }
                  let st₂' : MachineState :=
                    { st₂ with pc := n1.pc.val, regs := updateReg st₂.regs dst8.val vLoad,
                               world := bumpWorldStep st₂.world }
                  have hrel_post : StateRel2Deps env n1 dPost ρ resActive isNullId st₁' st₂' :=
                    StateRel2Deps_set_reg (by unfold GENERAL_REGISTER_COUNT at hdst_lt10; exact hdst_lt10)
                      (hregs_n1.trans (by rw [hn_eq]))
                      (hstk_n1.trans (by rw [hn_eq]))
                      (hbm_n1.trans (by rw [hn_eq]))
                      (hres_n1.trans (by rw [hn_eq]))
                      (fun r hr hne hd => by
                        rw [hsrc_reg_eq r (by omega), hrem_reg r (by omega)]
                        simp [hd, hne])
                      (fun s _ hd => by rw [hsrc_stk_eq s, hrem_stk s]; exact hd)
                      hrel
                      (fun _ => hrel_load)
                  have halloc' : (bumpWorldStep st₁.world).nextAllocId + 3 * F < 2 ^ 63 := by
                    simp only [bumpWorldStep]; omega
                  obtain ⟨ret, finalWorld, hr₁, hr₂, hlive_fin⟩ :=
                    hsem l₁ l₂ s₁ s₂ ρ resActive isNullId st₁' st₂' hl₁ hl₂ hleak halloc' hρ hrel_post
                  refine ⟨ret, finalWorld, ?_, ?_, hlive_fin⟩
                  · unfold runSteps step
                    rw [hv_base1] at hca1
                    simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, checkDst_eq_ok hdst_lt10,
                      hn_src_ge, ↓reduceIte, hv_base1, hca1, hal_eff, Bool.not_true, Bool.false_eq_true,
                      hfind_spec]
                    change (match structFieldStepOut prog (ctx.pc.val + 1) dst8.val st₁.regs st₁.stack st₁.world
                        (renameMemId ρ id) effOff w (StructFieldDesc.ofCore fd) with
                      | .next st' => runSteps prog env l₁ s₁ F st'
                      | .halted ret w => some (.inl (ret, w))
                      | .stuck err => some (.inr err)) = _
                    rw [hst1_eq]
                    dsimp only [stepNextPcAux]
                    rw [stepNextIte_eq_next (by dsimp [st₁']; rw [hn1_pc]) hr₁]
                    exact hr₁
                  · unfold runSteps step
                    rw [hv_base2] at hca2
                    simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, checkDst_eq_ok hdst_lt10,
                      hn_src_ge, ↓reduceIte, hv_base2, hca2, hal_eff, Bool.not_true, Bool.false_eq_true,
                      hfind_spec]
                    change (match structFieldStepOut prog (ctx.pc.val + 1) dst8.val st₂.regs st₂.stack st₂.world
                        (renameMemId ρ id) effOff w (StructFieldDesc.ofCore fd) with
                      | .next st' => runSteps prog env l₂ s₂ F st'
                      | .halted ret w => some (.inl (ret, w))
                      | .stuck err => some (.inr err)) = _
                    rw [hst2_eq]
                    dsimp only [stepNextPcAux]
                    rw [stepNextIte_eq_next (by dsimp [st₂']; rw [hn1_pc]) hr₂]
                    exact hr₂
            | _ => cases (Prod.mk.inj (ok_inj hlm)).1

/-! ## 3. Memory Store (`store_memory`, `visit_store_forward`, `visit_store_backward`) -/

theorem ComputationContext_store_memory_ok_valid
    {env : ExecEnv} {vCounter : Nat}
    {self n1 : verifier.ComputationContext}
    {vctx0 vctx1 : verifier.VerificationContext}
    {pc : Std.Usize} {addr value : verifier.Type} {field : verifier.Field}
    (hctx : CompCtxValid env vCounter self)
    (hval : TypeValid env vCounter value)
    (hstore : verifier.ComputationContext.store_memory self vctx0 pc addr field value =
      ok (.Ok (), n1, vctx1)) :
    CompCtxValid env vCounter n1 ∧
    vctx1.calling_context = vctx0.calling_context ∧
    vctx1.code = vctx0.code ∧
    vctx1.counter = vctx0.counter ∧
    vctx1.iteration = vctx0.iteration ∧
    vctx1.terminating_contexts = vctx0.terminating_contexts ∧
    vctx1.pending_successors = vctx0.pending_successors := by
  unfold verifier.ComputationContext.store_memory at hstore
  rcases bind_eq_ok hstore with ⟨r, _, hstore⟩
  cases r with
  | Err _ => cases (Prod.mk.inj (ok_inj hstore)).1
  | Ok _ =>
    dsimp only at hstore
    rw [bind_ok_eq (Type_unwrap_releasable_spec addr)] at hstore
    rcases bind_eq_ok hstore with ⟨is_init, _, hstore⟩
    cases hu : pureUnwrapReleasable addr <;> rw [hu] at hstore
    all_goals try (cases (Prod.mk.inj (ok_inj hstore)).1)
    · -- PtrToStack
      rename_i offset
      dsimp only at hstore
      rcases bind_eq_ok hstore with ⟨offset_sum, _, hstore⟩
      rcases bind_eq_ok hstore with ⟨⟨r1, s'⟩, hst_op, hstore⟩
      cases r1 with
      | Err _ => cases (Prod.mk.inj (ok_inj hstore)).1
      | Ok _ =>
        rcases bind_eq_ok hstore with ⟨⟨r2, context1⟩, hreg_ma, hstore⟩
        rcases Prod.mk.inj (ok_inj hstore) with ⟨rfl, hp⟩
        rcases Prod.mk.inj hp with ⟨rfl, rfl⟩
        obtain ⟨r0, s0, h0, hs0_valid, _, _⟩ :=
          Stack_store_spec hctx.2.1 offset_sum hval field.width
        rw [hst_op] at h0
        rcases Prod.mk.inj (ok_inj h0) with ⟨_, rfl⟩
        obtain ⟨h1, h2, h3, h4, h5, h6, _⟩ :=
          VerificationContext_register_memory_access_ok_spec hreg_ma
        exact ⟨⟨hctx.1, hs0_valid, hctx.2.2.1, hctx.2.2.2⟩, h1, h2, h3, h4, h5, h6⟩
    · -- PtrToMemory
      rename_i id offset buffer_size origin
      dsimp only at hstore
      rw [bind_ok_eq (PointerOrigin_is_writable_spec origin)] at hstore
      cases hw : (MemoryOrigin.ofCore origin).isWritable <;>
        simp only [hw, Bool.false_eq_true, ↓reduceIte] at hstore
      · cases (Prod.mk.inj (ok_inj hstore)).1
      · rcases bind_eq_ok hstore with ⟨bw, _, hstore⟩
        rcases bind_eq_ok hstore with ⟨r1, _, hstore⟩
        cases r1 with
        | Err _ => cases (Prod.mk.inj (ok_inj hstore)).1
        | Ok _ =>
          cases is_init <;> simp only [Bool.false_eq_true, ↓reduceIte] at hstore
          · cases (Prod.mk.inj (ok_inj hstore)).1
          · rcases bind_eq_ok hstore with ⟨⟨r2, context1⟩, hreg_ma, hstore⟩
            rcases Prod.mk.inj (ok_inj hstore) with ⟨rfl, hp⟩
            rcases Prod.mk.inj hp with ⟨rfl, rfl⟩
            obtain ⟨h1, h2, h3, h4, h5, h6, _⟩ :=
              VerificationContext_register_memory_access_ok_spec hreg_ma
            exact ⟨hctx, h1, h2, h3, h4, h5, h6⟩
    · -- PtrToStruct
      rename_i id offset descriptor
      dsimp only at hstore
      rcases bind_eq_ok hstore with ⟨o, _, hstore⟩
      cases o with
      | none => cases (Prod.mk.inj (ok_inj hstore)).1
      | some fd =>
        dsimp only at hstore
        rcases bind_eq_ok hstore with ⟨b_mut, _, hstore⟩
        cases b_mut with
        | false => cases (Prod.mk.inj (ok_inj hstore)).1
        | true =>
          cases is_init <;>
            simp only [Bool.false_eq_true, ↓reduceIte, bind_ok_eq (MemoryId_clone_spec id)] at hstore
          · cases (Prod.mk.inj (ok_inj hstore)).1
          · rcases bind_eq_ok hstore with ⟨⟨r1, context1⟩, hreg_ma, hstore⟩
            rcases Prod.mk.inj (ok_inj hstore) with ⟨rfl, hp⟩
            rcases Prod.mk.inj hp with ⟨rfl, rfl⟩
            obtain ⟨h1, h2, h3, h4, h5, h6, _⟩ :=
              VerificationContext_register_memory_access_ok_spec hreg_ma
            exact ⟨hctx, h1, h2, h3, h4, h5, h6⟩
    · -- PtrToArray
      rename_i id offset
      dsimp only at hstore
      rcases bind_eq_ok hstore with ⟨bound, _, hstore⟩
      rcases bind_eq_ok hstore with ⟨bw, _, hstore⟩
      rcases bind_eq_ok hstore with ⟨r1, _, hstore⟩
      cases r1 with
      | Err _ => cases (Prod.mk.inj (ok_inj hstore)).1
      | Ok _ =>
        cases is_init <;> simp only [Bool.false_eq_true, ↓reduceIte] at hstore
        · cases (Prod.mk.inj (ok_inj hstore)).1
        · rcases bind_eq_ok hstore with ⟨⟨r2, context1⟩, hreg_ma, hstore⟩
          rcases Prod.mk.inj (ok_inj hstore) with ⟨rfl, hp⟩
          rcases Prod.mk.inj hp with ⟨rfl, rfl⟩
          obtain ⟨h1, h2, h3, h4, h5, h6, _⟩ :=
            VerificationContext_register_memory_access_ok_spec hreg_ma
          exact ⟨hctx, h1, h2, h3, h4, h5, h6⟩

theorem visit_store_forward
    {env : ExecEnv} {ctx ctx_out : verifier.ComputationContext}
    {vctx0 vctx1 : verifier.VerificationContext}
    {w : DataWidth} {base : Nat} {off16 : BitVec 16} {src : Operand}
    (hctx : CompCtxValid env vctx0.counter.bv.toNat ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hfwd : VisitorDispatchNonLddw CompCtxVisitorInst ctx vctx0
      (.ok (.Ok (), ctx_out, vctx1)) (.store w base off16 src)) :
    ctx_out = ctx ∧ ForwardVisitPost env vctx0 vctx1 ctx_out := by
  rcases hfwd with ⟨dst8, off, vsrc, _, _, _, hcall⟩
  have hcall' := hcall.symm
  dsimp [CompCtxVisitorInst] at hcall'
  unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.store at hcall'
  cases vsrc with
  | Reg r =>
    dsimp only at hcall'
    rcases bind_eq_ok hcall' with ⟨vc1, hemit, hcall'⟩
    obtain ⟨he1, he2, he3, he4, he5, he6⟩ := VerificationContext_emit_log_ok_spec hemit
    rcases bind_eq_ok hcall' with ⟨r1, hreg_src, hcall'⟩
    cases r1 with
    | Err _ => cases (Prod.mk.inj (ok_inj hcall')).1
    | Ok v =>
      dsimp only at hcall'
      obtain ⟨_, hv_valid⟩ := ComputationContext_reg_ok_valid hctx hreg_src
      rcases bind_eq_ok hcall' with ⟨r2, hnext, hcall'⟩
      cases r2 with
      | Err _ => cases (Prod.mk.inj (ok_inj hcall')).1
      | Ok n =>
        dsimp only at hcall'
        obtain ⟨_, hn_eq⟩ := ComputationContext_next_ok_spec hnext
        have hn_valid : CompCtxValid env vctx0.counter.bv.toNat n := by rw [hn_eq]; exact hctx
        rcases bind_eq_ok hcall' with ⟨r3, _, hcall'⟩
        cases r3 with
        | Err _ => cases (Prod.mk.inj (ok_inj hcall')).1
        | Ok a =>
          dsimp only at hcall'
          rw [bind_ok_eq (Field_new_spec off (coreWidthOfSpec w))] at hcall'
          rcases bind_eq_ok hcall' with ⟨⟨r4, n1, vc2⟩, hsm, hcall'⟩
          cases r4 with
          | Err _ => cases (Prod.mk.inj (ok_inj hcall')).1
          | Ok u0 =>
            cases u0
            obtain ⟨hn1_valid, hm1, hm2, hm3, hm4, hm5, hm6⟩ :=
              ComputationContext_store_memory_ok_valid hn_valid hv_valid hsm
            rcases bind_eq_ok hcall' with ⟨vc3, hpush, hcall'⟩
            rcases Prod.mk.inj (ok_inj hcall') with ⟨_, hpair⟩
            rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
            obtain ⟨hp1, hp2, hp3, hp4, hp5, _, hp7⟩ :=
              VerificationContext_push_successor_ok_spec hpush
            have hcnt : vc3.counter = vctx0.counter := hp3.trans (hm3.trans he3)
            refine ⟨rfl, hp1.trans (hm1.trans he1), hp2.trans (hm2.trans he2),
              hp4.trans (hm4.trans he4), hp5.trans (hm5.trans he5),
              by rw [hcnt], by rwa [hcnt], ?_⟩
            intro opt hopt
            rw [hp7, hm6, he6, hpending, List.nil_append, List.mem_singleton] at hopt
            subst hopt
            exact ⟨n1, rfl, by rwa [hcnt]⟩
  | Value imm64 =>
    dsimp only at hcall'
    rcases bind_eq_ok hcall' with ⟨vc1, hemit, hcall'⟩
    obtain ⟨he1, he2, he3, he4, he5, he6⟩ := VerificationContext_emit_log_ok_spec hemit
    rw [bind_ok_eq (Type_mask_spec (coreWidthOfSpec w))] at hcall'
    simp only [lift_eq_ok, bind_ok] at hcall'
    rw [bind_ok_eq (from_known_64_eq _)] at hcall'
    rcases bind_eq_ok hcall' with ⟨r, hnext, hcall'⟩
    cases r with
    | Err _ => cases (Prod.mk.inj (ok_inj hcall')).1
    | Ok n =>
      dsimp only at hcall'
      obtain ⟨_, hn_eq⟩ := ComputationContext_next_ok_spec hnext
      have hn_valid : CompCtxValid env vctx0.counter.bv.toNat n := by rw [hn_eq]; exact hctx
      rcases bind_eq_ok hcall' with ⟨r1, _, hcall'⟩
      cases r1 with
      | Err _ => cases (Prod.mk.inj (ok_inj hcall')).1
      | Ok a =>
        dsimp only at hcall'
        rw [bind_ok_eq (Field_new_spec off (coreWidthOfSpec w))] at hcall'
        rcases bind_eq_ok hcall' with ⟨⟨r2, n1, vc2⟩, hsm, hcall'⟩
        cases r2 with
        | Err _ => cases (Prod.mk.inj (ok_inj hcall')).1
        | Ok u0 =>
          cases u0
          have hk_valid : TypeValid env vctx0.counter.bv.toNat
              (.ScalarValue (knownScalar64 (imm64 &&& ⟨(DataWidth.ofCore (coreWidthOfSpec w)).valMask64⟩))) :=
            knownScalar64_valid _
          obtain ⟨hn1_valid, hm1, hm2, hm3, hm4, hm5, hm6⟩ :=
            ComputationContext_store_memory_ok_valid hn_valid hk_valid hsm
          rcases bind_eq_ok hcall' with ⟨vc3, hpush, hcall'⟩
          rcases Prod.mk.inj (ok_inj hcall') with ⟨_, hpair⟩
          rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
          obtain ⟨hp1, hp2, hp3, hp4, hp5, _, hp7⟩ :=
            VerificationContext_push_successor_ok_spec hpush
          have hcnt : vc3.counter = vctx0.counter := hp3.trans (hm3.trans he3)
          refine ⟨rfl, hp1.trans (hm1.trans he1), hp2.trans (hm2.trans he2),
            hp4.trans (hm4.trans he4), hp5.trans (hm5.trans he5),
            by rw [hcnt], by rwa [hcnt], ?_⟩
          intro opt hopt
          rw [hp7, hm6, he6, hpending, List.nil_append, List.mem_singleton] at hopt
          subst hopt
          exact ⟨n1, rfl, by rwa [hcnt]⟩

private def storeEffSrcVal (w : DataWidth) (st : MachineState) : visitor.Source → RuntimeVal
  | .Reg r => st.regs r.val
  | .Value imm => .scalar (imm.bv &&& w.valMask64)

private theorem depTracker_store_non_stack_spec
    {ctx : verifier.ComputationContext}
    {dPost dd : verifier.DataDependencies} {vsrc : visitor.Source}
    {tracker : verifier.DependencyTracker}
    {vctx vctx' : verifier.VerificationContext}
    {res : core.result.Result Unit error.VerifierErrorKind}
    (hdst_live_dd : DataDepsHasReg dd dstVal = true)
    (hsub_dd : DepsSubset dPost dd)
    (h : (match vsrc with
          | visitor.Source.Reg src1 => do
            let dd1 ← verifier.DataDependencies.add_reg dd src1
            ok (core.result.Result.Ok (), { context := ctx, deps := dd1 }, vctx)
          | visitor.Source.Value _ =>
            ok (core.result.Result.Ok (), { context := ctx, deps := dd }, vctx) :
            Result ((core.result.Result Unit error.VerifierErrorKind) ×
              verifier.DependencyTracker × verifier.VerificationContext)) =
        ok (res, tracker, vctx')) :
    tracker.context = ctx ∧
    vctx' = vctx ∧
    DataDepsHasReg tracker.deps dstVal = true ∧
    DepsSubset dPost tracker.deps ∧
    (∀ r, vsrc = .Reg r → DataDepsHasReg tracker.deps r.val = true) := by
  cases vsrc with
  | Reg src1 =>
    dsimp only at h
    rcases bind_eq_ok h with ⟨dd1, hadd, h⟩
    rcases Prod.mk.inj (ok_inj h) with ⟨_, hp⟩
    rcases Prod.mk.inj hp with ⟨rfl, rfl⟩
    obtain ⟨hsrc_lt16, _, hadd_r, _, hsub1⟩ := DataDependencies_add_reg_ok_spec hadd
    refine ⟨rfl, rfl, ?_, DepsSubset_trans hsub_dd hsub1, ?_⟩
    · by_cases hd16 : dstVal < 16
      · rw [hadd_r dstVal hd16, hdst_live_dd, Bool.true_or]
      · unfold DataDepsHasReg at hdst_live_dd
        exact False.elim (by
          have : (dd.registers.bv.getLsbD dstVal) = false :=
            BitVec.getLsbD_of_ge dd.registers.bv dstVal (by omega)
          rw [this] at hdst_live_dd; cases hdst_live_dd)
    · intro r hr; cases hr
      rw [hadd_r src1.val hsrc_lt16]; simp
  | Value _ =>
    rcases Prod.mk.inj (ok_inj h) with ⟨_, hp⟩
    rcases Prod.mk.inj hp with ⟨rfl, rfl⟩
    exact ⟨rfl, rfl, hdst_live_dd, hsub_dd, fun _ hr => by cases hr⟩

private theorem store_memory_step_backward
    {prog : Program} {env : ExecEnv} {vCounter F : Nat}
    {ctx n n1 : verifier.ComputationContext}
    {vc1 vc2 vctx_back vctx_back1 : verifier.VerificationContext}
    {dPost : verifier.DataDependencies} {tracker : verifier.DependencyTracker}
    {w : DataWidth} {dst8 : Std.U8} {off : Std.I16} {vsrc : visitor.Source}
    {a vVal : verifier.Type} {res_b : core.result.Result Unit error.VerifierErrorKind}
    (hlt_pc : ctx.pc.val < prog.size)
    (hinsn_at : prog[ctx.pc.val]'hlt_pc = .store w dst8.val off.bv (operandOfSource vsrc))
    (hctx : CompCtxValid env vCounter ctx)
    (hdst_lt : dst8.val < REGISTER_COUNT)
    (hsrc_reg : ∀ r, operandOfSource vsrc = .reg r → r < REGISTER_COUNT)
    (ha_valid : TypeValid env vCounter a)
    (hv_valid : TypeValid env vCounter vVal)
    (hreg_dst : verifier.ComputationContext.reg ctx dst8 = ok (.Ok a))
    (hpc_next : n.pc.val = ctx.pc.val + 1)
    (hn_eq : n = { ctx with pc := n.pc, path_depth := n.path_depth, jmp_seq := n.jmp_seq })
    (hsm : verifier.ComputationContext.store_memory n vc1 ctx.pc a
      { offset := off, width := coreWidthOfSpec w } vVal = ok (.Ok (), n1, vc2))
    (hsem : SemTerminatesIn prog env vCounter n1 dPost F)
    (hsrc_rel_gen : ∀ (ρ : Std.U64 → Std.U64) (resActive isNullId : MemoryId → Bool)
      (deps : verifier.DataDependencies) (st₁ st₂ : MachineState),
      StateRel2Deps env ctx deps ρ resActive isNullId st₁ st₂ →
      (∀ r, vsrc = .Reg r → DataDepsHasReg deps r.val = true) →
      TypeRel2 env ρ resActive isNullId st₁.world.arrayLen vVal
        (storeEffSrcVal w st₁ vsrc) (storeEffSrcVal w st₂ vsrc))
    (hsrc_scalar_eq : ∀ (ρ : Std.U64 → Std.U64) (resActive isNullId : MemoryId → Bool)
      (deps : verifier.DataDependencies) (st₁ st₂ : MachineState) (data : U64ScalarValueData),
      vVal = .ScalarValue data →
      data.unwritten_mask = 0#u64 →
      StateRel2Deps env ctx deps ρ resActive isNullId st₁ st₂ →
      (∀ r, vsrc = .Reg r → DataDepsHasReg deps r.val = true) →
      ∃ s : BitVec 64,
        evalOperand st₁ (operandOfSource vsrc) = .scalar s ∧
        evalOperand st₂ (operandOfSource vsrc) = .scalar s)
    (hback_call : DepTrackerVisitorInst.store { context := ctx, deps := dPost } vctx_back
      dst8 off vsrc (coreWidthOfSpec w) = ok (res_b, tracker, vctx_back1)) :
    tracker.context = ctx ∧
    vctx_back1 = vctx_back ∧
    SemTerminatesIn prog env vCounter ctx tracker.deps (F + 1) := by
  dsimp [DepTrackerVisitorInst] at hback_call
  unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.store at hback_call
  dsimp only at hback_call
  rw [bind_ok_eq hreg_dst] at hback_call
  dsimp only at hback_call
  unfold verifier.ComputationContext.store_memory at hsm
  have hn_res_eq : n.resources = ctx.resources := by rw [hn_eq]
  have hn_stk_eq : n.stack = ctx.stack := by rw [hn_eq]
  have hn_bm_eq : n.array_bounds = ctx.array_bounds := by rw [hn_eq]
  rcases bind_eq_ok hsm with ⟨r_cnr, hcnr, hsm⟩
  cases r_cnr with
  | Err _ => cases (Prod.mk.inj (ok_inj hsm)).1
  | Ok u_cnr =>
    cases u_cnr
    dsimp only at hsm
    rw [hn_res_eq] at hcnr
    have hinner_eq : verifier.Type.inner a ctx = ok (.Ok (pureUnwrapReleasable a)) := by
      unfold verifier.Type.inner
      rw [bind_ok_eq hcnr]
      dsimp only
      rw [bind_ok_eq (Type_unwrap_releasable_spec a)]
    rw [bind_ok_eq hinner_eq] at hback_call
    dsimp only at hback_call
    rcases bind_eq_ok hback_call with ⟨dd, hadd_dst, hback_call⟩
    obtain ⟨hdst_lt16, _, hdst_reg_eq, _, hsub_dd⟩ := DataDependencies_add_reg_ok_spec hadd_dst
    have hdst_live_dd : DataDepsHasReg dd dst8.val = true := by
      rw [hdst_reg_eq dst8.val hdst_lt16]; simp
    rw [bind_ok_eq (Type_unwrap_releasable_spec a)] at hsm
    rcases bind_eq_ok hsm with ⟨is_init, his_init, hsm⟩
    have ha_ne_uninit : a ≠ .ScalarValue uninitScalar64 := by
      rintro rfl
      cases (Prod.mk.inj (ok_inj hsm)).1
    have hn_dst_ge : ¬ (dst8.val ≥ REGISTER_COUNT) := by omega
    cases hu : pureUnwrapReleasable a <;> rw [hu] at hsm hback_call
    all_goals try (cases (Prod.mk.inj (ok_inj hsm)).1)
    · -- PtrToStack
      rename_i stack_offset
      dsimp only at hsm hback_call
      rcases bind_eq_ok hsm with ⟨offset_sum, hadd_off, hsm⟩
      rw [bind_ok_eq hadd_off] at hback_call
      rcases bind_eq_ok hsm with ⟨⟨r_st, s'⟩, hst_op, hsm⟩
      cases r_st with
      | Err _ => cases (Prod.mk.inj (ok_inj hsm)).1
      | Ok u_st =>
        cases u_st
        rcases bind_eq_ok hsm with ⟨⟨r_ma, vc_ma⟩, _, hsm⟩
        rcases Prod.mk.inj (ok_inj hsm) with ⟨rfl, hp_sm⟩
        rcases Prod.mk.inj hp_sm with ⟨rfl, rfl⟩
        rw [hn_stk_eq] at hst_op
        obtain ⟨r0, s0, h0, _, _, hpost_st⟩ :=
          Stack_store_spec hctx.2.1 offset_sum hv_valid (coreWidthOfSpec w)
        rw [hst_op] at h0
        rcases Prod.mk.inj (ok_inj h0) with ⟨r0_eq, hs0_eq⟩
        subst hs0_eq
        obtain ⟨hoff_kn, hoff_lt, hal, hslots_other, _, hnar_val⟩ := hpost_st () r0_eq.symm
        rw [DataWidth_ofCore_coreWidthOfSpec] at hal
        obtain ⟨rchk0, hrchk0, hchk_iff⟩ := StackOffset_check_valid_offset_spec offset_sum
        have hrchk_ok : rchk0 = .Ok () := (hchk_iff ()).mpr ⟨hoff_kn, hoff_lt⟩
        subst hrchk_ok
        rw [bind_ok_eq hrchk0] at hback_call
        dsimp only at hback_call
        obtain ⟨bne_w, hbne_w, hbne_iff⟩ := DataWidth_ne_spec (coreWidthOfSpec w) .U64
        rw [bind_ok_eq hbne_w] at hback_call
        obtain ⟨idx, hidx, hidx_val⟩ := StackOffset_array_index_spec (BitVec.le_of_lt hoff_lt)
        have hoff_512 : offset_sum.value.bv.toNat < 512 := by
          rw [BitVec.lt_def] at hoff_lt; simpa using hoff_lt
        have hidx_lt64 : idx.val < 64 := by omega
        have hdep_stk :
            tracker.context = ctx ∧
            vctx_back1 = vctx_back ∧
            DataDepsHasReg tracker.deps dst8.val = true ∧
            (∀ r, r < 10 → DataDepsHasReg dPost r = true → DataDepsHasReg tracker.deps r = true) ∧
            (∀ s, s < 64 → s ≠ idx.val → DataDepsHasStack dPost s = true →
              DataDepsHasStack tracker.deps s = true) ∧
            (coreWidthOfSpec w ≠ .U64 →
              (∀ r, vsrc = .Reg r → DataDepsHasReg tracker.deps r.val = true) ∧
              (DataDepsHasStack dPost idx.val = true → DataDepsHasStack tracker.deps idx.val = true)) ∧
            (DataDepsHasStack dPost idx.val = true →
              ∀ r, vsrc = .Reg r → DataDepsHasReg tracker.deps r.val = true) := by
          cases bne_w with
          | true =>
            have hw_ne_core : coreWidthOfSpec w ≠ .U64 := hbne_iff.mp rfl
            simp only [↓reduceIte, bind_ok] at hback_call
            obtain ⟨htrk_c, hv_b, hdst_l, hsub_t, hsrc_l⟩ :=
              depTracker_store_non_stack_spec hdst_live_dd hsub_dd hback_call
            exact ⟨htrk_c, hv_b, hdst_l, hsub_t.1,
              fun s hs _ hd => hsub_t.2 s hs hd,
              fun _ => ⟨hsrc_l, fun hd => hsub_t.2 idx.val hidx_lt64 hd⟩,
              fun _ => hsrc_l⟩
          | false =>
            have hw_eq_core : coreWidthOfSpec w = .U64 := by
              by_contra hne; exact absurd (hbne_iff.mpr hne) (by decide)
            simp only [Bool.false_eq_true, ↓reduceIte] at hback_call
            have hpair_ex :
                ∃ dd2 src_live1,
                  (do
                    let i ← verifier.StackOffset.array_index offset_sum
                    let (s, d) ← verifier.DataDependencies.remove_stack dd i
                    ok (d, s)) = ok (dd2, src_live1) ∧
                  verifier.DataDependencies.remove_stack dd idx = ok (src_live1, dd2) := by
              rcases bind_eq_ok hback_call with ⟨⟨dd1_b, sl_b⟩, hp0, _⟩
              have hp1 := hp0
              rw [bind_ok_eq hidx] at hp1
              rcases bind_eq_ok hp1 with ⟨⟨sl1, dd2⟩, hr, hp1⟩
              rcases Prod.mk.inj (ok_inj hp1) with ⟨rfl, rfl⟩
              exact ⟨dd2, sl1, hp0, hr⟩
            rcases hpair_ex with ⟨dd2, src_live1, hpair_eq, hrem_stk⟩
            rw [bind_pair_ok_eq hpair_eq] at hback_call
            obtain ⟨_, hlive_eq, _, hrem_s_reg, hrem_s_stk⟩ :=
              DataDependencies_remove_stack_ok_spec hrem_stk
            have hlive_dpost : src_live1 = DataDepsHasStack dPost idx.val := by
              rw [hlive_eq, (DataDependencies_add_reg_ok_spec hadd_dst).2.2.2.1 idx.val]
            cases src_live1 with
            | true =>
              simp only [↓reduceIte] at hback_call
              have hsub_dd2_regs : ∀ r, r < 10 → DataDepsHasReg dPost r = true → DataDepsHasReg dd2 r = true :=
                fun r hr hd => by rw [hrem_s_reg r]; exact hsub_dd.1 r hr hd
              cases vsrc with
              | Reg src1 =>
                dsimp only at hback_call
                rcases bind_eq_ok hback_call with ⟨dd3, hadd_src1, hback_call⟩
                rcases Prod.mk.inj (ok_inj hback_call) with ⟨_, hp⟩
                rcases Prod.mk.inj hp with ⟨rfl, rfl⟩
                obtain ⟨hsrc1_lt16, _, hadd1_r, hadd1_s, hsub3⟩ := DataDependencies_add_reg_ok_spec hadd_src1
                refine ⟨rfl, rfl, ?_, ?_, ?_, fun hc => absurd hw_eq_core hc, ?_⟩
                · rw [hadd1_r dst8.val hdst_lt16, hrem_s_reg dst8.val, hdst_live_dd, Bool.true_or]
                · intro r hr hd; exact hsub3.1 r hr (hsub_dd2_regs r hr hd)
                · intro s hs hne hd
                  rw [hadd1_s s, hrem_s_stk s hs]
                  simp [hsub_dd.2 s hs hd, hne]
                · intro _ r hr; cases hr
                  rw [hadd1_r src1.val hsrc1_lt16]; simp
              | Value _ =>
                rcases Prod.mk.inj (ok_inj hback_call) with ⟨_, hp⟩
                rcases Prod.mk.inj hp with ⟨rfl, rfl⟩
                refine ⟨rfl, rfl, ?_, hsub_dd2_regs, ?_, fun hc => absurd hw_eq_core hc,
                  fun _ _ hr => by cases hr⟩
                · rw [hrem_s_reg dst8.val]; exact hdst_live_dd
                · intro s hs hne hd
                  rw [hrem_s_stk s hs]; simp [hsub_dd.2 s hs hd, hne]
            | false =>
              simp only [Bool.false_eq_true, ↓reduceIte] at hback_call
              rcases Prod.mk.inj (ok_inj hback_call) with ⟨_, hp⟩
              rcases Prod.mk.inj hp with ⟨rfl, rfl⟩
              refine ⟨rfl, rfl, ?_, ?_, ?_, fun hc => absurd hw_eq_core hc, ?_⟩
              · rw [hrem_s_reg dst8.val]; exact hdst_live_dd
              · intro r hr hd; rw [hrem_s_reg r]; exact hsub_dd.1 r hr hd
              · intro s hs hne hd
                rw [hrem_s_stk s hs]; simp [hsub_dd.2 s hs hd, hne]
              · intro hd; rw [← hlive_dpost] at hd; cases hd
        rcases hdep_stk with ⟨htrk_c, hv_b, hdst_l, hreg_deps, hstk_other_deps, hnar_deps, hlive_src_deps⟩
        refine ⟨htrk_c, hv_b, ?_⟩
        intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
        obtain ⟨_, _, hrel_dst⟩ :=
          ComputationContext_reg_ok_valid_and_rel2 hctx hrel (Or.inr hdst_l) hreg_dst
        obtain ⟨hu_eq, _, hca1, hca2⟩ :=
          Type_inner_ok_valid_and_checkAlive_none hctx hrel ha_valid ha_ne_uninit hinner_eq hrel_dst
        rw [hu] at hu_eq
        obtain ⟨_, _, baseOff, hconc_base, hv_base1, hv_base2⟩ :=
          unwrap_stack_ptr_rel2 ha_valid hu hrel_dst
        obtain ⟨off_s, hadd_s, hconc_sum⟩ :=
          StackOffset_add_i16_sound_2safety stack_offset off hconc_base
        rw [hadd_off] at hadd_s; cases ok_inj hadd_s
        have heff_eq : baseOff + sextOffset16 off.bv = offset_sum.value.bv :=
          eq_value_of_known_64 hoff_kn hconc_sum.1
        have heff_lt512 : baseOff + sextOffset16 off.bv < 512#64 := by rwa [heff_eq]
        have hal_eff : ((baseOff + sextOffset16 off.bv).toNat % 8) % w.bytes = 0 := by
          rwa [heff_eq]
        have hslot_idx_eq : (baseOff + sextOffset16 off.bv).toNat / 8 = idx.val := by
          rw [heff_eq, hidx_val]
        let effOff := baseOff + sextOffset16 off.bv
        let slotIdx := effOff.toNat / 8
        let subIdx := effOff.toNat % 8
        let newSlot₁ : StackSlot :=
          if w == .u64 then
            match evalOperand st₁ (operandOfSource vsrc) with
            | .scalar s => StackSlot.bytes s (~~~0#64)
            | cap => StackSlot.spilled cap
          else
            match evalOperand st₁ (operandOfSource vsrc) with
            | .scalar s =>
                match st₁.stack slotIdx with
                | .bytes oldV oldInit =>
                    StackSlot.bytes (insertSubValue oldV s subIdx w) (insertSubValue oldInit (~~~0#64) subIdx w)
                | .spilled cap =>
                    StackSlot.bytes (insertSubValue (cap.toBits l₁ st₁.world.arrayLen) s subIdx w)
                      (insertSubValue 0#64 (~~~0#64) subIdx w)
            | cap => StackSlot.spilled cap
        let newSlot₂ : StackSlot :=
          if w == .u64 then
            match evalOperand st₂ (operandOfSource vsrc) with
            | .scalar s => StackSlot.bytes s (~~~0#64)
            | cap => StackSlot.spilled cap
          else
            match evalOperand st₂ (operandOfSource vsrc) with
            | .scalar s =>
                match st₂.stack slotIdx with
                | .bytes oldV oldInit =>
                    StackSlot.bytes (insertSubValue oldV s subIdx w) (insertSubValue oldInit (~~~0#64) subIdx w)
                | .spilled cap =>
                    StackSlot.bytes (insertSubValue (cap.toBits l₂ st₁.world.arrayLen) s subIdx w)
                      (insertSubValue 0#64 (~~~0#64) subIdx w)
            | cap => StackSlot.spilled cap
        have hnar_scalars : w ≠ .u64 →
            ∃ sc₁ sc₂ : BitVec 64,
              evalOperand st₁ (operandOfSource vsrc) = .scalar sc₁ ∧
              evalOperand st₂ (operandOfSource vsrc) = .scalar sc₂ := by
          intro hw_ne
          have hw_ne_core : coreWidthOfSpec w ≠ .U64 := by
            intro hc; apply hw_ne
            have h0 := congrArg DataWidth.ofCore hc
            rw [DataWidth_ofCore_coreWidthOfSpec] at h0
            exact h0
          obtain ⟨data, hv_eq, hunw_sub⟩ := hnar_val hw_ne_core
          rw [DataWidth_ofCore_coreWidthOfSpec] at hunw_sub
          have hd_ne : data ≠ uninitScalar64 := by
            rintro rfl; exact extractSubValue_uninit_ne_zero w hunw_sub
          have hrel_eff := hsrc_rel_gen ρ resActive isNullId tracker.deps st₁ st₂ hrel
            (hnar_deps hw_ne_core).1
          rw [hv_eq] at hrel_eff
          simp only [TypeRel2, hd_ne, ↓reduceIte] at hrel_eff
          rcases hrel_eff with ⟨sc₁, sc₂, he1, he2, _⟩
          cases vsrc with
          | Reg r => exact ⟨sc₁, sc₂, he1, he2⟩
          | Value imm => exact ⟨imm.bv, imm.bv, rfl, rfl⟩
        have hnew_slot_rel : DataDepsHasStack dPost slotIdx = true →
            StackSlotRel2 env ρ resActive isNullId st₁.world.arrayLen
              (stackSlotType s' slotIdx) newSlot₁ newSlot₂ := by
          intro hd_slot
          have hd_idx : DataDepsHasStack dPost idx.val = true := by
            simpa only [slotIdx, effOff, hslot_idx_eq] using hd_slot
          have hrel_eff := hsrc_rel_gen ρ resActive isNullId tracker.deps st₁ st₂ hrel
            (hlive_src_deps hd_idx)
          have hslot_old : coreWidthOfSpec w ≠ .U64 →
              StackSlotRel2 env ρ resActive isNullId st₁.world.arrayLen
                (stackSlotType ctx.stack slotIdx) (st₁.stack slotIdx) (st₂.stack slotIdx) := by
            intro hw_ne_core
            have hlive_pre := (hnar_deps hw_ne_core).2 hd_idx
            exact hrel.2.2.2.2.2.2.2.2.2.2.2 slotIdx
              (by dsimp only [slotIdx, effOff]; rw [hslot_idx_eq]; exact hidx_lt64)
              (by dsimp only [slotIdx, effOff]; rw [hslot_idx_eq]; exact hlive_pre)
          have hrel_ss := (Stack_store_rel2 (layout₁ := l₁) (layout₂ := l₂)
            hctx.2.1 hv_valid hconc_sum hst_op hrel_eff hslot_old).2
          dsimp only at hrel_ss
          rw [DataWidth_ofCore_coreWidthOfSpec] at hrel_ss
          dsimp only [newSlot₁, newSlot₂, slotIdx, subIdx, effOff]
          cases vsrc with
          | Reg r =>
            dsimp only [storeEffSrcVal, operandOfSource, evalOperand] at hrel_eff hrel_ss ⊢
            revert hrel_eff hrel_ss
            generalize st₁.regs r.val = v1
            generalize st₂.regs r.val = v2
            generalize st₁.stack ((baseOff + sextOffset16 off.bv).toNat / 8) = sk1
            generalize st₂.stack ((baseOff + sextOffset16 off.bv).toNat / 8) = sk2
            cases v1 <;> cases v2 <;> cases sk1 <;> cases sk2 <;>
              (intro _ hss; exact hss)
          | Value imm =>
            dsimp only [storeEffSrcVal, operandOfSource, evalOperand] at hrel_ss ⊢
            by_cases hw : w = .u64
            · subst hw
              simpa only [beq_self_eq_true, ↓reduceIte, valMask64_u64_and] using hrel_ss
            · have hbeq : (w == .u64) = false := beq_false_of_ne hw
              simp only [hbeq, Bool.false_eq_true, ↓reduceIte] at hrel_ss ⊢
              revert hrel_ss
              generalize st₁.stack ((baseOff + sextOffset16 off.bv).toNat / 8) = sk1
              generalize st₂.stack ((baseOff + sextOffset16 off.bv).toNat / 8) = sk2
              cases sk1 <;> cases sk2 <;>
                (intro hss; simpa only [insertSubValue_and_valMask64] using hss)
        let st₁' : MachineState :=
          { st₁ with pc := n.pc.val, stack := updateStack st₁.stack slotIdx newSlot₁,
                     world := bumpWorldStep st₁.world }
        let st₂' : MachineState :=
          { st₂ with pc := n.pc.val, stack := updateStack st₂.stack slotIdx newSlot₂,
                     world := bumpWorldStep st₂.world }
        have hrel_post : StateRel2Deps env { n with stack := s' } dPost ρ resActive isNullId st₁' st₂' :=
          StateRel2Deps_update_stack
            (by dsimp only [slotIdx, effOff]; rw [hslot_idx_eq]; exact hidx_lt64)
            (by rw [hn_eq])
            rfl
            (by rw [hn_eq])
            (by rw [hn_eq])
            (fun s hs hne => by
              apply hslots_other s hs
              rwa [← heff_eq])
            hreg_deps
            (fun s hs hne hd => hstk_other_deps s hs (by rwa [← hslot_idx_eq]) hd)
            hrel
            hnew_slot_rel
        have halloc' : (bumpWorldStep st₁.world).nextAllocId + 3 * F < 2 ^ 63 := by
          simp only [bumpWorldStep]; omega
        obtain ⟨ret, finalWorld, hr₁, hr₂, hlive_fin⟩ :=
          hsem l₁ l₂ s₁ s₂ ρ resActive isNullId st₁' st₂' hl₁ hl₂ hleak halloc' hρ hrel_post
        have hn_ge512 : ¬ (baseOff + sextOffset16 off.bv ≥ 512#64) := BitVec.not_le.mpr heff_lt512
        have hmis_false : ((baseOff + sextOffset16 off.bv).toNat % 8 % w.bytes != 0) = false :=
          bne_eq_false_iff_eq.mpr hal_eff
        refine ⟨ret, finalWorld, ?_, ?_, hlive_fin⟩
        · unfold runSteps step
          rw [hv_base1] at hca1
          rcases hop : operandOfSource vsrc with r | imm
          · simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_dst_ge, ↓reduceIte,
              hop, hsrc_reg r hop, hv_base1, hca1, hn_ge512, hmis_false, Bool.false_eq_true]
            by_cases hw64 : w = .u64
            · subst hw64
              simp only [beq_self_eq_true, ↓reduceIte]
              rw [stepNextIte_eq_next (by
                dsimp only [st₁', newSlot₁, slotIdx, subIdx, effOff, evalOperand]
                rw [hpc_next, hop]
                simp only [beq_self_eq_true, ↓reduceIte]
                cases st₁.regs r <;> rfl) hr₁]
              exact hr₁
            · have hbeq64 : (w == .u64) = false := beq_false_of_ne hw64
              obtain ⟨sc₁, _, he1, _⟩ := hnar_scalars hw64
              rw [hop] at he1; dsimp only [evalOperand] at he1
              simp only [hbeq64, Bool.false_eq_true, ↓reduceIte, he1]
              rw [stepNextIte_eq_next (by
                dsimp only [st₁', newSlot₁, slotIdx, subIdx, effOff]
                rw [hpc_next, hop]
                simp only [evalOperand, hbeq64, Bool.false_eq_true, ↓reduceIte, he1]
                cases st₁.stack ((baseOff + sextOffset16 off.bv).toNat / 8) <;> rfl) hr₁]
              exact hr₁
          · simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_dst_ge, ↓reduceIte,
              hop, hv_base1, hca1, hn_ge512, hmis_false, Bool.false_eq_true]
            by_cases hw64 : w = .u64
            · subst hw64
              simp only [beq_self_eq_true, ↓reduceIte]
              rw [stepNextIte_eq_next (by
                dsimp only [st₁', newSlot₁, slotIdx, subIdx, effOff]
                rw [hpc_next, hop]
                simp only [evalOperand, beq_self_eq_true, ↓reduceIte]) hr₁]
              exact hr₁
            · have hbeq64 : (w == .u64) = false := beq_false_of_ne hw64
              simp only [hbeq64, Bool.false_eq_true, ↓reduceIte]
              rw [stepNextIte_eq_next (by
                dsimp only [st₁', newSlot₁, slotIdx, subIdx, effOff]
                rw [hpc_next, hop]
                simp only [evalOperand, hbeq64, Bool.false_eq_true, ↓reduceIte]
                cases st₁.stack ((baseOff + sextOffset16 off.bv).toNat / 8) <;> rfl) hr₁]
              exact hr₁
        · unfold runSteps step
          rw [hv_base2] at hca2
          rcases hop : operandOfSource vsrc with r | imm
          · simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_dst_ge, ↓reduceIte,
              hop, hsrc_reg r hop, hv_base2, hca2, hn_ge512, hmis_false, Bool.false_eq_true]
            by_cases hw64 : w = .u64
            · subst hw64
              simp only [beq_self_eq_true, ↓reduceIte]
              rw [stepNextIte_eq_next (by
                dsimp only [st₂', newSlot₂, slotIdx, subIdx, effOff, evalOperand]
                rw [hpc_next, hop]
                simp only [beq_self_eq_true, ↓reduceIte]
                cases st₂.regs r <;> rfl) hr₂]
              exact hr₂
            · have hbeq64 : (w == .u64) = false := beq_false_of_ne hw64
              obtain ⟨_, sc₂, _, he2⟩ := hnar_scalars hw64
              rw [hop] at he2; dsimp only [evalOperand] at he2
              simp only [hbeq64, Bool.false_eq_true, ↓reduceIte, he2]
              rw [stepNextIte_eq_next (by
                dsimp only [st₂', newSlot₂, slotIdx, subIdx, effOff]
                rw [hpc_next, hop, ← hrel.2.2.1]
                simp only [evalOperand, hbeq64, Bool.false_eq_true, ↓reduceIte, he2]
                cases st₂.stack ((baseOff + sextOffset16 off.bv).toNat / 8) <;> rfl) hr₂]
              exact hr₂
          · simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_dst_ge, ↓reduceIte,
              hop, hv_base2, hca2, hn_ge512, hmis_false, Bool.false_eq_true]
            by_cases hw64 : w = .u64
            · subst hw64
              simp only [beq_self_eq_true, ↓reduceIte]
              rw [stepNextIte_eq_next (by
                dsimp only [st₂', newSlot₂, slotIdx, subIdx, effOff]
                rw [hpc_next, hop]
                simp only [evalOperand, beq_self_eq_true, ↓reduceIte]) hr₂]
              exact hr₂
            · have hbeq64 : (w == .u64) = false := beq_false_of_ne hw64
              simp only [hbeq64, Bool.false_eq_true, ↓reduceIte]
              rw [stepNextIte_eq_next (by
                dsimp only [st₂', newSlot₂, slotIdx, subIdx, effOff]
                rw [hpc_next, hop, ← hrel.2.2.1]
                simp only [evalOperand, hbeq64, Bool.false_eq_true, ↓reduceIte]
                cases st₂.stack ((baseOff + sextOffset16 off.bv).toNat / 8) <;> rfl) hr₂]
              exact hr₂
    · -- PtrToMemory
      rename_i id offset buffer_size origin
      dsimp only at hsm hback_call
      obtain ⟨htrk_c, hv_b, hdst_l, hsub_t, hsrc_l⟩ :=
        depTracker_store_non_stack_spec hdst_live_dd hsub_dd hback_call
      rw [bind_ok_eq (PointerOrigin_is_writable_spec origin)] at hsm
      cases hw_orig : (MemoryOrigin.ofCore origin).isWritable <;>
        simp only [hw_orig, Bool.false_eq_true, ↓reduceIte] at hsm
      · cases (Prod.mk.inj (ok_inj hsm)).1
      · rcases bind_eq_ok hsm with ⟨bw, hbw, hsm⟩
        rcases bind_eq_ok hsm with ⟨r_chk, hchk, hsm⟩
        cases r_chk with
        | Err _ => cases (Prod.mk.inj (ok_inj hsm)).1
        | Ok u_chk =>
          cases u_chk
          cases hinit : is_init <;>
            simp only [hinit, Bool.false_eq_true, ↓reduceIte] at hsm
          · cases (Prod.mk.inj (ok_inj hsm)).1
          · rcases bind_eq_ok hsm with ⟨⟨r_ma, vc_ma⟩, _, hsm⟩
            rcases Prod.mk.inj (ok_inj hsm) with ⟨rfl, hp_sm⟩
            rcases Prod.mk.inj hp_sm with ⟨rfl, rfl⟩
            have hval_init : ∃ data, vVal = .ScalarValue data ∧ data.unwritten_mask = 0#u64 := by
              rw [hinit] at his_init
              cases vVal <;> try (cases ok_inj his_init)
              exact ⟨_, rfl, (is_fully_initialized_64_ok_true his_init).1⟩
            rcases hval_init with ⟨data, hv_eq, hunw⟩
            refine ⟨htrk_c, hv_b, ?_⟩
            intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
            obtain ⟨_, _, hrel_dst⟩ :=
              ComputationContext_reg_ok_valid_and_rel2 hctx hrel (Or.inr hdst_l) hreg_dst
            obtain ⟨_, _, hca1, hca2⟩ :=
              Type_inner_ok_valid_and_checkAlive_none hctx hrel ha_valid ha_ne_uninit hinner_eq hrel_dst
            obtain ⟨_, _, relOpt, baseOff, hconc_base, hv_base1, hv_base2⟩ :=
              unwrap_mem_ptr_rel2 ha_valid hu hrel_dst
            obtain ⟨sc, he1, he2⟩ :=
              hsrc_scalar_eq ρ resActive isNullId tracker.deps st₁ st₂ data hv_eq hunw hrel hsrc_l
            have hin_bnds : inBoundsRange (baseOff + sextOffset16 off.bv) w buffer_size.bv = true := by
              have hchk_ctx : verifier.ComputationContext.check_memory_access ctx offset buffer_size off bw = ok (.Ok ()) := by
                rw [hn_eq] at hchk; exact hchk
              simpa only [DataWidth_ofCore_coreWidthOfSpec] using
                ComputationContext_check_memory_access_ok_spec hbw hconc_base.1 hchk_ctx
            let buf' := writeBufWord st₁.world.bufBytes (renameMemId ρ id) (baseOff + sextOffset16 off.bv) w sc
            let w₁' : WorldState := { st₁.world with bufBytes := buf' }
            let w₂' : WorldState := { st₂.world with bufBytes := writeBufWord st₂.world.bufBytes (renameMemId ρ id) (baseOff + sextOffset16 off.bv) w sc }
            have hw'_eq : w₁' = w₂' := by dsimp [w₁', w₂', buf']; rw [hrel.2.2.1]
            let st₁' : MachineState := { st₁ with pc := n.pc.val, world := bumpWorldStep w₁' }
            let st₂' : MachineState := { st₂ with pc := n.pc.val, world := bumpWorldStep w₂' }
            have hrel_post : StateRel2Deps env n dPost ρ resActive isNullId st₁' st₂' :=
              StateRel2Deps_update_world_bytes hn_eq hw'_eq rfl rfl rfl hsub_t hrel
            have halloc' : (bumpWorldStep st₁.world).nextAllocId + 3 * F < 2 ^ 63 := by
              simp only [bumpWorldStep]; omega
            obtain ⟨ret, finalWorld, hr₁, hr₂, hlive_fin⟩ :=
              hsem l₁ l₂ s₁ s₂ ρ resActive isNullId st₁' st₂' hl₁ hl₂ hleak halloc' hρ hrel_post
            refine ⟨ret, finalWorld, ?_, ?_, hlive_fin⟩
            · unfold runSteps step
              rw [hv_base1] at hca1
              rcases hop : operandOfSource vsrc with r | imm
              · rw [hop] at he1; dsimp [evalOperand] at he1
                simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_dst_ge, ↓reduceIte,
                  hop, hsrc_reg r hop, hv_base1, hca1, hw_orig, Bool.not_true, Bool.false_eq_true,
                  he1, hin_bnds]
                rw [stepNextIte_eq_next (by dsimp [st₁', w₁', buf']; rw [hpc_next]) hr₁]
                exact hr₁
              · rw [hop] at he1; cases RuntimeVal.scalar.inj he1
                simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_dst_ge, ↓reduceIte,
                  hop, hv_base1, hca1, hw_orig, Bool.not_true, Bool.false_eq_true, hin_bnds]
                rw [stepNextIte_eq_next (by dsimp [st₁', w₁', buf']; rw [hpc_next]) hr₁]
                exact hr₁
            · unfold runSteps step
              rw [hv_base2] at hca2
              rcases hop : operandOfSource vsrc with r | imm
              · rw [hop] at he2; dsimp [evalOperand] at he2
                simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_dst_ge, ↓reduceIte,
                  hop, hsrc_reg r hop, hv_base2, hca2, hw_orig, Bool.not_true, Bool.false_eq_true,
                  he2, hin_bnds]
                rw [stepNextIte_eq_next (by dsimp [st₂', w₂']; rw [hpc_next]) hr₂]
                exact hr₂
              · rw [hop] at he2; cases RuntimeVal.scalar.inj he2
                simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_dst_ge, ↓reduceIte,
                  hop, hv_base2, hca2, hw_orig, Bool.not_true, Bool.false_eq_true, hin_bnds]
                rw [stepNextIte_eq_next (by dsimp [st₂', w₂']; rw [hpc_next]) hr₂]
                exact hr₂
    · -- PtrToStruct
      rename_i id offset descriptor
      dsimp only at hsm hback_call
      obtain ⟨htrk_c, hv_b, hdst_l, hsub_t, hsrc_l⟩ :=
        depTracker_store_non_stack_spec hdst_live_dd hsub_dd hback_call
      rcases bind_eq_ok hsm with ⟨o_fd, hfind, hsm⟩
      cases o_fd with
      | none => cases (Prod.mk.inj (ok_inj hsm)).1
      | some fd =>
        dsimp only at hsm
        rcases bind_eq_ok hsm with ⟨b_mut, hmut, hsm⟩
        cases b_mut with
        | false => cases (Prod.mk.inj (ok_inj hsm)).1
        | true =>
          cases hinit : is_init <;>
            simp only [hinit, Bool.false_eq_true, ↓reduceIte, bind_ok_eq (MemoryId_clone_spec id)] at hsm
          · cases (Prod.mk.inj (ok_inj hsm)).1
          · rcases bind_eq_ok hsm with ⟨⟨r_ma, vc_ma⟩, _, hsm⟩
            rcases Prod.mk.inj (ok_inj hsm) with ⟨rfl, hp_sm⟩
            rcases Prod.mk.inj hp_sm with ⟨rfl, rfl⟩
            have hfd_mut : ∃ sz, fd.field_type = .MutableScalar sz := by
              cases hft : fd.field_type <;> rw [hft] at hmut <;> try (cases ok_inj hmut)
              exact ⟨_, rfl⟩
            rcases hfd_mut with ⟨sz_fd, hft_mut⟩
            have hval_init : ∃ data, vVal = .ScalarValue data ∧ data.unwritten_mask = 0#u64 := by
              rw [hinit] at his_init
              cases vVal <;> try (cases ok_inj his_init)
              exact ⟨_, rfl, (is_fully_initialized_64_ok_true his_init).1⟩
            rcases hval_init with ⟨data, hv_eq, hunw⟩
            refine ⟨htrk_c, hv_b, ?_⟩
            intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
            obtain ⟨_, _, hrel_dst⟩ :=
              ComputationContext_reg_ok_valid_and_rel2 hctx hrel (Or.inr hdst_l) hreg_dst
            obtain ⟨_, _, hca1, hca2⟩ :=
              Type_inner_ok_valid_and_checkAlive_none hctx hrel ha_valid ha_ne_uninit hinner_eq hrel_dst
            obtain ⟨_, _, ⟨schemaId, ⟨hschema_nv, _, henv_fields⟩, hmatch_id⟩,
                relOpt, baseOff, hconc_base, hv_base1, hv_base2⟩ :=
              unwrap_struct_ptr_rel2 ha_valid hu hrel_dst
            obtain ⟨sc, he1, he2⟩ :=
              hsrc_scalar_eq ρ resActive isNullId tracker.deps st₁ st₂ data hv_eq hunw hrel hsrc_l
            have hmatch_ren : memoryIdMatches schemaId (renameMemId ρ id) = true := by
              rw [renameMemId_matches_noVerif ρ id hschema_nv]; exact hmatch_id
            have henv_eq := henv_fields (renameMemId ρ id) hmatch_ren
            obtain ⟨_, _, hal_eff, hfind_spec, _, hkind_spec⟩ :=
              StructDescriptor_find_field_ok_some_spec hconc_base hfind
            rw [DataWidth_ofCore_coreWidthOfSpec] at hal_eff hkind_spec
            rw [← henv_eq] at hfind_spec
            rw [hft_mut] at hkind_spec
            have hkind_ofCore : (StructFieldDesc.ofCore fd).kind = .scalar (BitVec.ofNat 64 sz_fd.val) true := by
              dsimp [StructFieldDesc.ofCore]; rw [hft_mut]
            let effOff := baseOff + sextOffset16 off.bv
            let w₁' : WorldState :=
              { st₁.world with structBytes := writeBufWord st₁.world.structBytes (renameMemId ρ id) effOff w sc }
            let w₂' : WorldState :=
              { st₂.world with structBytes := writeBufWord st₂.world.structBytes (renameMemId ρ id) effOff w sc }
            have hw'_eq : w₁' = w₂' := by dsimp [w₁', w₂']; rw [hrel.2.2.1]
            let st₁' : MachineState := { st₁ with pc := n.pc.val, world := bumpWorldStep w₁' }
            let st₂' : MachineState := { st₂ with pc := n.pc.val, world := bumpWorldStep w₂' }
            have hrel_post : StateRel2Deps env n dPost ρ resActive isNullId st₁' st₂' :=
              StateRel2Deps_update_world_bytes hn_eq hw'_eq rfl rfl rfl hsub_t hrel
            have halloc' : (bumpWorldStep st₁.world).nextAllocId + 3 * F < 2 ^ 63 := by
              simp only [bumpWorldStep]; omega
            obtain ⟨ret, finalWorld, hr₁, hr₂, hlive_fin⟩ :=
              hsem l₁ l₂ s₁ s₂ ρ resActive isNullId st₁' st₂' hl₁ hl₂ hleak halloc' hρ hrel_post
            refine ⟨ret, finalWorld, ?_, ?_, hlive_fin⟩
            · unfold runSteps step
              rw [hv_base1] at hca1
              rcases hop : operandOfSource vsrc with r | imm
              · rw [hop] at he1; dsimp [evalOperand] at he1
                simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_dst_ge, ↓reduceIte,
                  hop, hsrc_reg r hop, hv_base1, hca1, he1, hal_eff, Bool.not_true, Bool.false_eq_true,
                  hfind_spec, hkind_ofCore, hkind_spec]
                rw [stepNextIte_eq_next (by dsimp [st₁', w₁', effOff]; rw [hpc_next]) hr₁]
                exact hr₁
              · rw [hop] at he1; cases RuntimeVal.scalar.inj he1
                simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_dst_ge, ↓reduceIte,
                  hop, hv_base1, hca1, hal_eff, Bool.not_true, Bool.false_eq_true,
                  hfind_spec, hkind_ofCore, hkind_spec]
                rw [stepNextIte_eq_next (by dsimp [st₁', w₁', effOff]; rw [hpc_next]) hr₁]
                exact hr₁
            · unfold runSteps step
              rw [hv_base2] at hca2
              rcases hop : operandOfSource vsrc with r | imm
              · rw [hop] at he2; dsimp [evalOperand] at he2
                simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_dst_ge, ↓reduceIte,
                  hop, hsrc_reg r hop, hv_base2, hca2, he2, hal_eff, Bool.not_true, Bool.false_eq_true,
                  hfind_spec, hkind_ofCore, hkind_spec]
                rw [stepNextIte_eq_next (by dsimp [st₂', w₂', effOff]; rw [hpc_next]) hr₂]
                exact hr₂
              · rw [hop] at he2; cases RuntimeVal.scalar.inj he2
                simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_dst_ge, ↓reduceIte,
                  hop, hv_base2, hca2, hal_eff, Bool.not_true, Bool.false_eq_true,
                  hfind_spec, hkind_ofCore, hkind_spec]
                rw [stepNextIte_eq_next (by dsimp [st₂', w₂', effOff]; rw [hpc_next]) hr₂]
                exact hr₂
    · -- PtrToArray
      rename_i id offset
      dsimp only at hsm hback_call
      obtain ⟨htrk_c, hv_b, hdst_l, hsub_t, hsrc_l⟩ :=
        depTracker_store_non_stack_spec hdst_live_dd hsub_dd hback_call
      rcases bind_eq_ok hsm with ⟨bound, hbm_get, hsm⟩
      rcases bind_eq_ok hsm with ⟨bw, hbw, hsm⟩
      rcases bind_eq_ok hsm with ⟨r_chk, hchk, hsm⟩
      cases r_chk with
      | Err _ => cases (Prod.mk.inj (ok_inj hsm)).1
      | Ok u_chk =>
        cases u_chk
        cases hinit : is_init <;>
          simp only [hinit, Bool.false_eq_true, ↓reduceIte] at hsm
        · cases (Prod.mk.inj (ok_inj hsm)).1
        · rcases bind_eq_ok hsm with ⟨⟨r_ma, vc_ma⟩, _, hsm⟩
          rcases Prod.mk.inj (ok_inj hsm) with ⟨rfl, hp_sm⟩
          rcases Prod.mk.inj hp_sm with ⟨rfl, rfl⟩
          have hval_init : ∃ data, vVal = .ScalarValue data ∧ data.unwritten_mask = 0#u64 := by
            rw [hinit] at his_init
            cases vVal <;> try (cases ok_inj his_init)
            exact ⟨_, rfl, (is_fully_initialized_64_ok_true his_init).1⟩
          rcases hval_init with ⟨data, hv_eq, hunw⟩
          refine ⟨htrk_c, hv_b, ?_⟩
          intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
          obtain ⟨_, _, hrel_dst⟩ :=
            ComputationContext_reg_ok_valid_and_rel2 hctx hrel (Or.inr hdst_l) hreg_dst
          obtain ⟨_, _, hca1, hca2⟩ :=
            Type_inner_ok_valid_and_checkAlive_none hctx hrel ha_valid ha_ne_uninit hinner_eq hrel_dst
          obtain ⟨_, _, _, baseOff, hconc_base, hv_base1, hv_base2⟩ :=
            unwrap_array_ptr_rel2 ha_valid hu hrel_dst
          obtain ⟨sc, he1, he2⟩ :=
            hsrc_scalar_eq ρ resActive isNullId tracker.deps st₁ st₂ data hv_eq hunw hrel hsrc_l
          rw [hn_bm_eq] at hbm_get
          obtain ⟨b0, hb0, hb_cases⟩ := BoundsMap_get_or_zero_spec hctx.2.2.1 id
          rw [hbm_get] at hb0; cases ok_inj hb0
          have hb_le_arr : bound.bv ≤ st₁.world.arrayLen (renameMemId ρ id) := by
            rcases hb_cases with ⟨rfl, _⟩ | hmem_bm
            · exact BitVec.le_def.mpr (Nat.zero_le _)
            · exact hrel.2.2.2.2.2.2.2.1 id bound hmem_bm
          have hin_bound : inBoundsRange (baseOff + sextOffset16 off.bv) w bound.bv = true := by
            have hchk_ctx : verifier.ComputationContext.check_memory_access ctx offset bound off bw = ok (.Ok ()) := by
              rw [hn_eq] at hchk; exact hchk
            simpa only [DataWidth_ofCore_coreWidthOfSpec] using
              ComputationContext_check_memory_access_ok_spec hbw hconc_base.1 hchk_ctx
          have hin_arr1 := inBoundsRange_mono_right hin_bound hb_le_arr
          have hin_arr2 : inBoundsRange (baseOff + sextOffset16 off.bv) w
              (st₂.world.arrayLen (renameMemId ρ id)) = true := by
            rwa [← hrel.2.2.1]
          let effOff := baseOff + sextOffset16 off.bv
          let w₁' : WorldState :=
            { st₁.world with bufBytes := writeBufWord st₁.world.bufBytes (renameMemId ρ id) effOff w sc }
          let w₂' : WorldState :=
            { st₂.world with bufBytes := writeBufWord st₂.world.bufBytes (renameMemId ρ id) effOff w sc }
          have hw'_eq : w₁' = w₂' := by dsimp [w₁', w₂']; rw [hrel.2.2.1]
          let st₁' : MachineState := { st₁ with pc := n.pc.val, world := bumpWorldStep w₁' }
          let st₂' : MachineState := { st₂ with pc := n.pc.val, world := bumpWorldStep w₂' }
          have hrel_post : StateRel2Deps env n dPost ρ resActive isNullId st₁' st₂' :=
            StateRel2Deps_update_world_bytes hn_eq hw'_eq rfl rfl rfl hsub_t hrel
          have halloc' : (bumpWorldStep st₁.world).nextAllocId + 3 * F < 2 ^ 63 := by
            simp only [bumpWorldStep]; omega
          obtain ⟨ret, finalWorld, hr₁, hr₂, hlive_fin⟩ :=
            hsem l₁ l₂ s₁ s₂ ρ resActive isNullId st₁' st₂' hl₁ hl₂ hleak halloc' hρ hrel_post
          refine ⟨ret, finalWorld, ?_, ?_, hlive_fin⟩
          · unfold runSteps step
            rw [hv_base1] at hca1
            rcases hop : operandOfSource vsrc with r | imm
            · rw [hop] at he1; dsimp [evalOperand] at he1
              simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_dst_ge, ↓reduceIte,
                hop, hsrc_reg r hop, hv_base1, hca1, he1, hin_arr1, Bool.not_true, Bool.false_eq_true]
              rw [stepNextIte_eq_next (by dsimp [st₁', w₁', effOff]; rw [hpc_next]) hr₁]
              exact hr₁
            · rw [hop] at he1; cases RuntimeVal.scalar.inj he1
              simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_dst_ge, ↓reduceIte,
                hop, hv_base1, hca1, hin_arr1, Bool.not_true, Bool.false_eq_true]
              rw [stepNextIte_eq_next (by dsimp [st₁', w₁', effOff]; rw [hpc_next]) hr₁]
              exact hr₁
          · unfold runSteps step
            rw [hv_base2] at hca2
            rcases hop : operandOfSource vsrc with r | imm
            · rw [hop] at he2; dsimp [evalOperand] at he2
              simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_dst_ge, ↓reduceIte,
                hop, hsrc_reg r hop, hv_base2, hca2, he2, hin_arr2, Bool.not_true, Bool.false_eq_true]
              rw [stepNextIte_eq_next (by dsimp [st₂', w₂', effOff]; rw [hpc_next]) hr₂]
              exact hr₂
            · rw [hop] at he2; cases RuntimeVal.scalar.inj he2
              simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_dst_ge, ↓reduceIte,
                hop, hv_base2, hca2, hin_arr2, Bool.not_true, Bool.false_eq_true]
              rw [stepNextIte_eq_next (by dsimp [st₂', w₂', effOff]; rw [hpc_next]) hr₂]
              exact hr₂

theorem visit_store_backward
    {prog : Program} {env : ExecEnv} {vCounter F : Nat}
    {ctx ctx_out : verifier.ComputationContext}
    {vctx0 vctx1 vctx_back vctx_back1 : verifier.VerificationContext}
    {dPost : verifier.DataDependencies} {tracker : verifier.DependencyTracker}
    {w : DataWidth} {base : Nat} {off16 : BitVec 16} {src : Operand}
    (hpc : ∃ hlt : ctx.pc.val < prog.size,
      prog[ctx.pc.val]'hlt = .store w base off16 src)
    (hctx : CompCtxValid env vCounter ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hfwd : VisitorDispatchNonLddw CompCtxVisitorInst ctx vctx0
      (.ok (.Ok (), ctx_out, vctx1)) (.store w base off16 src))
    (hchildren : ∀ succ, some succ ∈ vctx1.pending_successors.val →
      SemTerminatesIn prog env vCounter succ dPost F)
    (hback : VisitorDispatchNonLddw DepTrackerVisitorInst
      { context := ctx_out, deps := dPost } vctx_back
      (.ok (.Ok (), tracker, vctx_back1)) (.store w base off16 src)) :
    ctx_out = ctx ∧
    tracker.context = ctx ∧
    vctx_back1 = vctx_back ∧
    SemTerminatesIn prog env vCounter ctx tracker.deps (F + 1) := by
  rcases hfwd with ⟨dst8, off, vsrc, hbase, hoff, hsrc_eq, hcall⟩
  rcases hback with ⟨dst8_b, off_b, vsrc_b, hbase_b, hoff_b, hsrc_b, hback_call⟩
  have hdst_u8 : dst8_b = dst8 := UScalar.eq_of_val_eq (hbase_b.symm.trans hbase)
  have hoff_i16 : off_b = off := i16_eq_of_bv_eq (hoff_b.symm.trans hoff)
  subst dst8_b; subst off_b
  have hvsrc_eq : vsrc_b = vsrc := by
    have heq := hsrc_b.symm.trans hsrc_eq
    cases vsrc_b <;> cases vsrc <;> simp [operandOfSource] at heq
    · exact congrArg visitor.Source.Reg (UScalar.eq_of_val_eq heq)
    · exact congrArg visitor.Source.Value ((u64_eq_iff_bv _ _).mpr heq)
  subst vsrc_b
  subst base; subst off16; subst src
  obtain ⟨hlt_pc, hinsn_at⟩ := hpc
  have hcall' := hcall.symm
  dsimp [CompCtxVisitorInst] at hcall'
  unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.store at hcall'
  cases vsrc with
  | Reg r_src =>
    dsimp only at hcall'
    rcases bind_eq_ok hcall' with ⟨vc1, hemit, hcall'⟩
    obtain ⟨_, _, _, _, _, he6⟩ := VerificationContext_emit_log_ok_spec hemit
    rcases bind_eq_ok hcall' with ⟨r1, hreg_src, hcall'⟩
    cases r1 with
    | Err _ => cases (Prod.mk.inj (ok_inj hcall')).1
    | Ok v =>
      dsimp only at hcall'
      obtain ⟨hr_src_lt, hv_valid⟩ := ComputationContext_reg_ok_valid hctx hreg_src
      rcases bind_eq_ok hcall' with ⟨r2, hnext, hcall'⟩
      cases r2 with
      | Err _ => cases (Prod.mk.inj (ok_inj hcall')).1
      | Ok n =>
        dsimp only at hcall'
        obtain ⟨hpc_next, hn_eq⟩ := ComputationContext_next_ok_spec hnext
        have hn_valid : CompCtxValid env vCounter n := by rw [hn_eq]; exact hctx
        rcases bind_eq_ok hcall' with ⟨r3, hreg_dst, hcall'⟩
        cases r3 with
        | Err _ => cases (Prod.mk.inj (ok_inj hcall')).1
        | Ok a =>
          dsimp only at hcall'
          obtain ⟨hdst_lt, ha_valid⟩ := ComputationContext_reg_ok_valid hctx hreg_dst
          rw [bind_ok_eq (Field_new_spec off (coreWidthOfSpec w))] at hcall'
          rcases bind_eq_ok hcall' with ⟨⟨r4, n1, vc2⟩, hsm, hcall'⟩
          cases r4 with
          | Err _ => cases (Prod.mk.inj (ok_inj hcall')).1
          | Ok u0 =>
            cases u0
            obtain ⟨_, _, _, _, _, _, hm6⟩ :=
              ComputationContext_store_memory_ok_valid hn_valid hv_valid hsm
            rcases bind_eq_ok hcall' with ⟨vc3, hpush, hcall'⟩
            rcases Prod.mk.inj (ok_inj hcall') with ⟨_, hpair⟩
            rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
            obtain ⟨_, _, _, _, _, _, hp7⟩ := VerificationContext_push_successor_ok_spec hpush
            have hn1_mem : some n1 ∈ vc3.pending_successors.val := by
              rw [hp7, hm6, he6, hpending]; simp
            have hsem := hchildren n1 hn1_mem
            obtain ⟨htrk_c, hv_b, hsem_pre⟩ :=
              store_memory_step_backward hlt_pc hinsn_at hctx hdst_lt
                (fun r hr => by cases hr; exact hr_src_lt)
                ha_valid hv_valid hreg_dst hpc_next hn_eq hsm hsem
                (fun ρ resActive isNullId deps st₁ st₂ hrel hlive =>
                  (ComputationContext_reg_ok_valid_and_rel2 hctx hrel
                    (Or.inr (hlive r_src rfl)) hreg_src).2.2)
                (fun ρ resActive isNullId deps st₁ st₂ data hv_eq hunw hrel hlive => by
                  have hrel_s := (ComputationContext_reg_ok_valid_and_rel2 hctx hrel
                    (Or.inr (hlive r_src rfl)) hreg_src).2.2
                  rw [hv_eq] at hrel_s
                  have hd_ne : data ≠ uninitScalar64 := fun hc => by
                    subst hc; exact uninitScalar64_unwritten_ne_zero hunw
                  simp only [TypeRel2, hd_ne, ↓reduceIte] at hrel_s
                  rcases hrel_s with ⟨x₁, x₂, hx1, hx2, hconc⟩
                  have hx_eq := fully_initialized_eq_of_rel2_64 hunw hconc
                  subst hx_eq
                  exact ⟨x₁, hx1, hx2⟩)
                hback_call.symm
            exact ⟨rfl, htrk_c, hv_b, hsem_pre⟩
  | Value imm64 =>
    dsimp only at hcall'
    rcases bind_eq_ok hcall' with ⟨vc1, hemit, hcall'⟩
    obtain ⟨_, _, _, _, _, he6⟩ := VerificationContext_emit_log_ok_spec hemit
    rw [bind_ok_eq (Type_mask_spec (coreWidthOfSpec w))] at hcall'
    simp only [lift_eq_ok, bind_ok] at hcall'
    rw [bind_ok_eq (from_known_64_eq _)] at hcall'
    rcases bind_eq_ok hcall' with ⟨r, hnext, hcall'⟩
    cases r with
    | Err _ => cases (Prod.mk.inj (ok_inj hcall')).1
    | Ok n =>
      dsimp only at hcall'
      obtain ⟨hpc_next, hn_eq⟩ := ComputationContext_next_ok_spec hnext
      have hn_valid : CompCtxValid env vCounter n := by rw [hn_eq]; exact hctx
      rcases bind_eq_ok hcall' with ⟨r1, hreg_dst, hcall'⟩
      cases r1 with
      | Err _ => cases (Prod.mk.inj (ok_inj hcall')).1
      | Ok a =>
        dsimp only at hcall'
        obtain ⟨hdst_lt, ha_valid⟩ := ComputationContext_reg_ok_valid hctx hreg_dst
        rw [bind_ok_eq (Field_new_spec off (coreWidthOfSpec w))] at hcall'
        rcases bind_eq_ok hcall' with ⟨⟨r2, n1, vc2⟩, hsm, hcall'⟩
        cases r2 with
        | Err _ => cases (Prod.mk.inj (ok_inj hcall')).1
        | Ok u0 =>
          cases u0
          have hk_valid : TypeValid env vCounter
              (.ScalarValue (knownScalar64 (imm64 &&& ⟨(DataWidth.ofCore (coreWidthOfSpec w)).valMask64⟩))) :=
            knownScalar64_valid _
          obtain ⟨_, _, _, _, _, _, hm6⟩ :=
            ComputationContext_store_memory_ok_valid hn_valid hk_valid hsm
          rcases bind_eq_ok hcall' with ⟨vc3, hpush, hcall'⟩
          rcases Prod.mk.inj (ok_inj hcall') with ⟨_, hpair⟩
          rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
          obtain ⟨_, _, _, _, _, _, hp7⟩ := VerificationContext_push_successor_ok_spec hpush
          have hn1_mem : some n1 ∈ vc3.pending_successors.val := by
            rw [hp7, hm6, he6, hpending]; simp
          have hsem := hchildren n1 hn1_mem
          obtain ⟨htrk_c, hv_b, hsem_pre⟩ :=
            store_memory_step_backward hlt_pc hinsn_at hctx hdst_lt
              (fun _ hr => by cases hr)
              ha_valid hk_valid hreg_dst hpc_next hn_eq hsm hsem
              (fun _ _ _ _ _ _ _ _ => by
                dsimp only [storeEffSrcVal]
                rw [DataWidth_ofCore_coreWidthOfSpec]
                have hc := concretizesScalar64Rel2_known (imm64 &&& ⟨w.valMask64⟩)
                simp only [u64_and_bv, u64_mk_bv] at hc
                exact TypeRel2_scalar_rel2_of_concretizes hc)
              (fun _ _ _ _ _ _ _ _ _ _ _ => ⟨imm64.bv, rfl, rfl⟩)
              hback_call.symm
          exact ⟨rfl, htrk_c, hv_b, hsem_pre⟩

end StarnixEbpf.Proof
