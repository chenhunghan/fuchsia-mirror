-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Proof.InsnAluEndExit

/-!
# Soundness of 64-Bit `LDDW` Loads and External Helper Calls

Proves forward invariant preservation (`CompCtxValid`, `ForwardVisitPost`) and
backward relational 2-safety termination (`SemTerminatesIn`) for:
1. 2-slot `BPF_LDDW` (`0x18`) instructions (`BpfVisitor.load64`: `.lddwImm`,
   `.lddwMap`, `.lddwMapValue`), including fresh `MemoryId` allocation via
   `VerificationContext.next_id`.
2. External helper calls (`BpfVisitor.call_external`: `CoreCallingContext.find_helper`,
   `DependencyTracker.call_external`, `Type.stack_input_size`,
   `Type.match_parameter_type` across all 13 `ParamTypeMatches` constructors,
   `BoundsMap.clear` on `invalidate_array_bounds`, and
   `ComputationContext.resolve_return_value` across all 6 `ReturnTypeMatches`
   cases).
-/

namespace StarnixEbpf.Proof

open Aeneas Aeneas.Std Result ControlFlow Error
open StarnixEbpf.Assumptions
open StarnixEbpf.Spec
open StarnixEbpf.LoopLemmas
open ebpf_core
open ebpf_core.scalar_value

/-! ## 1. Full Calling-Context Conformance, `MapFlags`, & `LDDW` Soundness -/

/-- Extends `CallingContextEnvMatches` with entry-argument conformance for `cctx.args`,
    needed when validating `Type.ContextParameter` helper arguments. -/
def CallingContextFullMatches (cctx : verifier.CoreCallingContext) (env : ExecEnv) : Prop :=
  CallingContextEnvMatches cctx env ∧
  (∀ i (hi : i < cctx.args.val.length),
    ∃ v₁ v₂, EntryArgConforms env cctx.args.val[i] (env.entryContextArgs i) v₁ v₂)

theorem CallingContextFullMatches.of_entryStatesConform
    {cctx : verifier.CoreCallingContext} {env : ExecEnv}
    {mkInit : HostLayout → SecretInit → MachineState}
    (h : EntryStatesConform cctx env mkInit) :
    CallingContextFullMatches cctx env := by
  refine ⟨CallingContextEnvMatches.of_entryStatesConform h, fun i hi => ?_⟩
  have hdum := h.2.2.2.2.2.2 dummyHostLayout dummyHostLayout dummySecretInit dummySecretInit
    dummyHostLayout_valid dummyHostLayout_valid
  exact ⟨_, _, hdum.2.2.2.2.2.2.2.2.2.1 i hi⟩

private theorem progReadOnly_bv_eq (fl : BitVec 32) :
    decide (fl &&& 0x80#32 = 0x80#32) = (fl &&& 0x80#32 != 0#32) := by
  bv_decide

theorem MapFlags_ProgReadOnly_contains_spec (s : maps.MapSchema) :
    ∃ mf b,
      maps.MapFlags.ProgReadOnly = ok mf ∧
      maps.MapFlags.contains s.flags mf = ok b ∧
      b = (MapSpec.ofCore s).progReadOnly := by
  have hmf : maps.MapFlags.ProgReadOnly = ok 128#u32 := by
    simp only [global_simps]
    change (do let i ← UScalar.shiftLeft_IScalar 1#u32 7#i32; ok i) = ok 128#u32
    simp only [UScalar.shiftLeft_IScalar, UScalar.shiftLeft, IScalar.val,
      UScalarTy.numBits, show (7#i32).bv.toInt ≥ 0 by decide,
      show (7#i32).toNat < 32 by decide, ↓reduceIte, bind_ok]
    rfl
  refine ⟨128#u32, (MapSpec.ofCore s).progReadOnly, hmf, ?_, rfl⟩
  unfold maps.MapFlags.contains MapSpec.ofCore
  simp only [lift_eq_ok, bind_ok]
  congr 1
  have heq : (s.flags &&& 128#u32 = 128#u32) ↔ (s.flags.bv &&& 0x80#32 = 0x80#32) :=
    u32_eq_iff_bv (s.flags &&& 128#u32) 128#u32
  rw [decide_eq_decide.mpr heq]
  exact progReadOnly_bv_eq s.flags.bv

def isLddwPrimary : Insn → Bool
  | .lddwImm _ _ | .lddwMap _ _ | .lddwMapValue _ _ _ => true
  | _ => false

private def optNotLddw : Option Insn → Bool
  | some (.lddwImm _ _) | some (.lddwMap _ _) | some (.lddwMapValue _ _ _) => false
  | _ => true

private theorem optNotLddw_decodeSingleInsn (raw : api.EbpfInstruction) :
    optNotLddw (decodeSingleInsn raw) = true := by
  unfold decodeSingleInsn
  dsimp only
  cases decodeAtomicOp (((raw.bv >>> 32).setWidth 32).setWidth 8) <;>
    cases decodeEndianWidth ((raw.bv >>> 32).setWidth 32) <;> {
    simp only [Option.map]
    simp only [apply_ite optNotLddw]
    simp only [optNotLddw, ite_self]
  }

private theorem decodeSingleInsn_not_lddwPrimary {raw : api.EbpfInstruction} {insn : Insn}
    (h : decodeSingleInsn raw = some insn) :
    isLddwPrimary insn = false := by
  have hopt := optNotLddw_decodeSingleInsn raw
  rw [h] at hopt
  cases insn <;> first | rfl | cases hopt

private theorem decodeVerifiedCodeList_lddw_has_cont :
    ∀ (raws : List api.EbpfInstruction) (insns : List Insn) (pc : Nat),
      decodeVerifiedCodeList raws = some insns →
      (hlt : pc < insns.length) →
      isLddwPrimary (insns[pc]'hlt) = true →
      pc + 1 < insns.length ∧ insns[pc + 1]? = some .lddwCont
  | [], insns, pc, hdec, hlt, _ => by
      simp [decodeVerifiedCodeList] at hdec
      subst hdec; simp at hlt
  | r1 :: rest, insns, pc, hdec, hlt, hlddw => by
      unfold decodeVerifiedCodeList at hdec
      dsimp only at hdec
      by_cases hop : (r1.bv.setWidth 8 == 0x18#8) = true
      · simp only [hop, ↓reduceIte] at hdec
        cases rest with
        | nil => simp at hdec
        | cons r2 tail =>
          dsimp only at hdec
          by_cases hcz : (r2.bv.setWidth 32 != 0#32) = true
          · simp [hcz] at hdec
          · simp only [hcz, Bool.false_eq_true, ↓reduceIte] at hdec
            split at hdec
            · rename_i h_ins decodedTail _ hTail
              cases Option.some.inj hdec
              cases pc with
              | zero =>
                exact ⟨by simp, rfl⟩
              | succ pc1 =>
                cases pc1 with
                | zero =>
                  simp [isLddwPrimary] at hlddw
                | succ pc2 =>
                  have hlt2 : pc2 < decodedTail.length := by
                    simp only [List.length_cons] at hlt; omega
                  obtain ⟨h1, h2⟩ := decodeVerifiedCodeList_lddw_has_cont tail decodedTail pc2
                    hTail hlt2 (by simpa using hlddw)
                  exact ⟨by simp only [List.length_cons]; omega, by simpa using h2⟩
            · simp at hdec
      · simp only [hop, Bool.false_eq_true, ↓reduceIte] at hdec
        split at hdec
        · rename_i h_ins decodedTail hSingle hTail
          cases Option.some.inj hdec
          cases pc with
          | zero =>
            have hf := decodeSingleInsn_not_lddwPrimary hSingle
            simp [hf] at hlddw
          | succ pc1 =>
            have hlt1 : pc1 < decodedTail.length := by
              simp only [List.length_cons] at hlt; omega
            obtain ⟨h1, h2⟩ := decodeVerifiedCodeList_lddw_has_cont rest decodedTail pc1
              hTail hlt1 (by simpa using hlddw)
            exact ⟨by simp only [List.length_cons]; omega, by simpa using h2⟩
        · simp at hdec

theorem decodeVerifiedCode_lddw_has_cont
    {raws : List api.EbpfInstruction} {prog : Program} {pc : Nat}
    (hdec : decodeVerifiedCode raws = some prog)
    (hlt : pc < prog.size)
    (hlddw : isLddwPrimary (prog[pc]'hlt) = true) :
    pc + 1 < prog.size ∧ prog[pc + 1]? = some .lddwCont := by
  unfold decodeVerifiedCode at hdec
  cases hlist : decodeVerifiedCodeList raws <;> rw [hlist] at hdec
  · simp at hdec
  · rename_i insns
    simp only [Option.map, Option.some.injEq] at hdec
    subst hdec
    have hlt_l : pc < insns.length := by simpa using hlt
    obtain ⟨h1, h2⟩ := decodeVerifiedCodeList_lddw_has_cont raws insns pc hlist hlt_l
      (by simpa using hlddw)
    exact ⟨by simpa using h1, by simpa using h2⟩

theorem lddwInsnOfResolved_isLddwPrimary (dst : Nat) (r : verifier.ResolvedLddw) :
    isLddwPrimary (lddwInsnOfResolved dst r) = true := by
  cases r <;> rfl

private theorem u64_bv_cast_of_u32 (x : Std.U32) :
    U64.bv (UScalar.cast .U64 x) = x.bv.zeroExtend 64 := by
  ext i hi
  dsimp only [UScalar.cast, UScalar.bv, U64.bv, UScalarTy.U64_numBits_eq]

private theorem uscalar_cast_u64_of_u32_bv (x : Std.U32) :
    (UScalar.cast .U64 x).bv = x.bv.zeroExtend 64 :=
  u64_bv_cast_of_u32 x

private theorem uscalar_cast_u64_of_u32_toNat (x : Std.U32) :
    (UScalar.cast .U64 x).bv.toNat = x.val := by
  change (U64.bv (UScalar.cast .U64 x)).toNat = x.val
  rw [u64_bv_cast_of_u32]
  have hlt32 : x.bv.toNat < 2 ^ 64 := Nat.lt_trans x.bv.isLt (by decide)
  change (BitVec.setWidth 64 (U32.bv x)).toNat = x.val
  rw [BitVec.toNat_setWidth, Nat.mod_eq_of_lt hlt32]
  rfl

private theorem u64_cast_of_u32_le_u32max (x : Std.U32) :
    U64.bv (UScalar.cast .U64 x) ≤ 0xFFFF_FFFF#64 := by
  rw [u64_bv_cast_of_u32, BitVec.le_def]
  have hlt32 := x.bv.isLt
  have hlt64 : x.bv.toNat < 2 ^ 64 := Nat.lt_trans hlt32 (by decide)
  change (BitVec.setWidth 64 (U32.bv x)).toNat ≤ (0xFFFF_FFFF#64).toNat
  rw [BitVec.toNat_setWidth, Nat.mod_eq_of_lt hlt64]
  simp only [BitVec.toNat_ofNat] at *
  omega

theorem visit_load64_forward
    {env : ExecEnv} {ctx ctx_out : verifier.ComputationContext}
    {vctx0 vctx1 : verifier.VerificationContext}
    {dst src : Std.U8} {lower : Std.U32}
    (hcctx : CallingContextEnvMatches vctx0.calling_context env)
    (hctx : CompCtxValid env vctx0.counter.bv.toNat ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hfwd : CompCtxVisitorInst.load64 ctx vctx0 dst src lower = ok (.Ok (), ctx_out, vctx1)) :
    ctx_out = ctx ∧ ForwardVisitPost env vctx0 vctx1 ctx_out := by
  dsimp [CompCtxVisitorInst] at hfwd
  unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.load64 at hfwd
  simp only [lift_eq_ok, bind_ok] at hfwd
  cases ho : Usize.checked_add ctx.pc 1#usize <;> rw [ho] at hfwd
  · cases (Prod.mk.inj (ok_inj hfwd)).1
  · rename_i next_pc
    dsimp only at hfwd
    by_cases hge : next_pc >= alloc.vec.Vec.len vctx0.code
    · simp only [hge, ↓reduceIte] at hfwd
      cases (Prod.mk.inj (ok_inj hfwd)).1
    · simp only [hge, ↓reduceIte] at hfwd
      rcases bind_eq_ok hfwd with ⟨ei, hei, hfwd⟩
      rw [bind_ok_eq (ebpf_imm_eq_ok ei)] at hfwd
      rcases bind_eq_ok hfwd with ⟨r_res, hres, hfwd⟩
      cases r_res with
      | Err _ => cases (Prod.mk.inj (ok_inj hfwd)).1
      | Ok r1 =>
        dsimp only at hfwd
        have hres_spec := resolve_lddw_ok_spec vctx0.calling_context src lower.bv
          ((ei.bv >>> 32).setWidth 32) r1 hres
        rcases bind_eq_ok hfwd with ⟨p_cv, hcv, _⟩
        rcases p_cv with ⟨context1, value⟩
        rw [bind_pair_ok_eq hcv] at hfwd
        -- Prove uniform validity of `(context1, value)`:
        have hcv_valid :
            context1.calling_context = vctx0.calling_context ∧
            context1.code = vctx0.code ∧
            context1.iteration = vctx0.iteration ∧
            context1.terminating_contexts = vctx0.terminating_contexts ∧
            context1.pending_successors = vctx0.pending_successors ∧
            vctx0.counter.bv.toNat ≤ context1.counter.bv.toNat ∧
            TypeValid env context1.counter.bv.toNat value := by
          rcases hres_spec with ⟨_, rfl⟩ | ⟨schema, _, _, _, hmap_get, rfl⟩ |
              ⟨schema, _, _, _, _, _, _, _, hoff_lt, rfl⟩
          · dsimp only at hcv
            rcases bind_eq_ok hcv with ⟨context2, hemit, hcv⟩
            rw [bind_ok_eq (from_known_64_eq _)] at hcv
            rcases Prod.mk.inj (ok_inj hcv) with ⟨rfl, rfl⟩
            obtain ⟨he1, he2, he3, he4, he5, he6⟩ := VerificationContext_emit_log_ok_spec hemit
            exact ⟨he1, he2, he4, he5, he6, by rw [he3], knownScalar64_valid _⟩
          · dsimp only at hcv
            rcases bind_eq_ok hcv with ⟨context2, hemit, hcv⟩
            rcases Prod.mk.inj (ok_inj hcv) with ⟨rfl, rfl⟩
            obtain ⟨he1, he2, he3, he4, he5, he6⟩ := VerificationContext_emit_log_ok_spec hemit
            refine ⟨he1, he2, he4, he5, he6, by rw [he3], ?_⟩
            dsimp only [TypeValid]
            rw [uscalar_cast_u64_of_u32_toNat]
            have hval_l : (⟨lower.bv⟩ : Std.U32).val = lower.bv.toNat := rfl
            rw [hval_l, hcctx.2.2.1 lower.bv.toNat, hmap_get]
            rfl
          · dsimp only at hcv
            rcases bind_eq_ok hcv with ⟨context2, hemit, hcv⟩
            obtain ⟨he1, he2, he3, he4, he5, he6⟩ := VerificationContext_emit_log_ok_spec hemit
            rcases bind_eq_ok hcv with ⟨p_nid, hnid, _⟩
            rcases p_nid with ⟨id, context3⟩
            rw [bind_pair_ok_eq hnid] at hcv
            obtain ⟨hid_eq, hc3_eq, hc3_cnt⟩ := VerificationContext_next_id_ok_spec hnid
            obtain ⟨mf, b_ro, hmf, hcont, _⟩ := MapFlags_ProgReadOnly_contains_spec schema
            rw [bind_ok_eq hmf, bind_ok_eq hcont] at hcv
            rcases bind_eq_ok hcv with ⟨origin, _, hcv⟩
            simp only [from_known_64_eq, bind_ok] at hcv
            rcases Prod.mk.inj (ok_inj hcv) with ⟨hc1_eq, rfl⟩
            subst context3
            have hcnt_le : vctx0.counter.bv.toNat ≤ context1.counter.bv.toNat := by
              rw [hc3_cnt, he3]; omega
            refine ⟨by rw [hc3_eq]; exact he1, by rw [hc3_eq]; exact he2,
              by rw [hc3_eq]; exact he4, by rw [hc3_eq]; exact he5,
              by rw [hc3_eq]; exact he6, hcnt_le, ?_⟩
            refine ⟨?_, knownScalar64_valid _, rfl, ?_⟩
            · rw [hid_eq]
              simp only [memIdBounded, decide_eq_true_eq]
              omega
            · exact u64_cast_of_u32_le_u32max ⟨(ei.bv >>> 32).setWidth 32⟩
        rcases hcv_valid with ⟨hc1, hc2, hc4, hc5, hc6, hcnt_le, hval_valid⟩
        rcases bind_eq_ok hfwd with ⟨r2, hjmp, hfwd⟩
        cases r2 with
        | Err _ => cases (Prod.mk.inj (ok_inj hfwd)).1
        | Ok n =>
          dsimp only at hfwd
          obtain ⟨_, _, hn_eq⟩ := ComputationContext_jump_with_offset_ok_spec hjmp
          have hctx_c1 : CompCtxValid env context1.counter.bv.toNat ctx :=
            CompCtxValid_mono hcnt_le hctx
          have hn_valid : CompCtxValid env context1.counter.bv.toNat n := by
            rw [hn_eq]; exact hctx_c1
          rcases bind_eq_ok hfwd with ⟨⟨r3, n1⟩, hset, hfwd⟩
          cases r3 with
          | Err _ => cases (Prod.mk.inj (ok_inj hfwd)).1
          | Ok _ =>
            obtain ⟨_, _, _, _, _, _, hn1_valid⟩ :=
              ComputationContext_set_reg_ok_spec hn_valid hval_valid hset
            rcases bind_eq_ok hfwd with ⟨context2, hpush, hfwd⟩
            rcases Prod.mk.inj (Prod.mk.inj (ok_inj hfwd)).2 with ⟨rfl, rfl⟩
            obtain ⟨hp1, hp2, hp3, hp4, hp5, _, hp7⟩ :=
              VerificationContext_push_successor_ok_spec hpush
            refine ⟨rfl, hp1.trans hc1, hp2.trans hc2, hp4.trans hc4, hp5.trans hc5,
              by rw [hp3]; exact hcnt_le, by rw [hp3]; exact hctx_c1, ?_⟩
            intro opt hopt
            rw [hp7, hc6, hpending, List.nil_append, List.mem_singleton] at hopt
            subst hopt
            exact ⟨n1, rfl, by rwa [hp3]⟩

theorem visit_load64_backward
    {prog : Program} {env : ExecEnv} {F : Nat}
    {ctx ctx_out : verifier.ComputationContext}
    {vctx0 vctx1 vctx_back vctx_back1 : verifier.VerificationContext}
    {dPost : verifier.DataDependencies} {tracker : verifier.DependencyTracker}
    {dst src : Std.U8} {lower : Std.U32}
    {raw2 : api.EbpfInstruction} {rlddw : verifier.ResolvedLddw}
    (hcctx : CallingContextEnvMatches vctx0.calling_context env)
    (hlt_pc : ctx.pc.val < prog.size)
    (hinsn_at : prog[ctx.pc.val]'hlt_pc = lddwInsnOfResolved dst.val rlddw)
    (hcont : ctx.pc.val + 1 < prog.size ∧ prog[ctx.pc.val + 1]? = some .lddwCont)
    (hcode2 : vctx0.code.val[ctx.pc.val + 1]? = some raw2)
    (hres_pre : verifier.CoreCallingContext.resolve_lddw vctx0.calling_context src
      (UScalar.hcast .I32 lower)
      (UScalar.hcast .I32 (⟨(raw2.bv >>> 32).setWidth 32⟩ : Std.U32)) = ok (.Ok rlddw))
    (hctx : CompCtxValid env vctx0.counter.bv.toNat ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hfwd : CompCtxVisitorInst.load64 ctx vctx0 dst src lower = ok (.Ok (), ctx_out, vctx1))
    (hchildren : ∀ succ, some succ ∈ vctx1.pending_successors.val →
      SemTerminatesIn prog env vctx1.counter.bv.toNat succ dPost F)
    (hback : DepTrackerVisitorInst.load64 { context := ctx_out, deps := dPost } vctx_back
      dst src lower = ok (.Ok (), tracker, vctx_back1)) :
    ctx_out = ctx ∧
    tracker.context = ctx ∧
    vctx_back1 = vctx_back ∧
    SemTerminatesIn prog env vctx0.counter.bv.toNat ctx tracker.deps (F + 1) := by
  dsimp [CompCtxVisitorInst] at hfwd
  unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.load64 at hfwd
  simp only [lift_eq_ok, bind_ok] at hfwd
  cases ho : Usize.checked_add ctx.pc 1#usize <;> rw [ho] at hfwd
  · cases (Prod.mk.inj (ok_inj hfwd)).1
  · rename_i next_pc
    dsimp only at hfwd
    have hnpc_val : next_pc.val = ctx.pc.val + 1 := by
      have hs := Usize.checked_add_bv_spec ctx.pc 1#usize
      rw [ho] at hs
      simpa [usize_one_val] using hs.2.1
    by_cases hge : next_pc >= alloc.vec.Vec.len vctx0.code
    · simp only [hge, ↓reduceIte] at hfwd
      cases (Prod.mk.inj (ok_inj hfwd)).1
    · simp only [hge, ↓reduceIte] at hfwd
      rcases bind_eq_ok hfwd with ⟨ei, hei, hfwd⟩
      have hei_get := (vec_index_usize_ok hei).2
      rw [hnpc_val, hcode2] at hei_get
      cases Option.some.inj hei_get
      rw [bind_ok_eq (ebpf_imm_eq_ok raw2)] at hfwd
      rw [bind_ok_eq hres_pre] at hfwd
      dsimp only at hfwd
      have hres_spec := resolve_lddw_ok_spec vctx0.calling_context src lower.bv
        ((raw2.bv >>> 32).setWidth 32) rlddw hres_pre
      rcases bind_eq_ok hfwd with ⟨p_cv, hcv, _⟩
      rcases p_cv with ⟨context1, value⟩
      rw [bind_pair_ok_eq hcv] at hfwd
      rcases bind_eq_ok hfwd with ⟨r2, hjmp, hfwd⟩
      cases r2 with
      | Err _ => cases (Prod.mk.inj (ok_inj hfwd)).1
      | Ok n =>
        dsimp only at hfwd
        obtain ⟨_, hpc_int, hn_eq⟩ := ComputationContext_jump_with_offset_ok_spec hjmp
        have hi16_one : (1#i16 : Std.I16).val = 1 := by decide
        have hpc_n_val : n.pc.val = ctx.pc.val + 2 := by
          simp only [Int.ofNat_eq_natCast, hi16_one] at hpc_int; omega
        rcases bind_eq_ok hfwd with ⟨⟨r3, n1⟩, hset, hfwd⟩
        cases r3 with
        | Err _ => cases (Prod.mk.inj (ok_inj hfwd)).1
        | Ok _ =>
          rcases bind_eq_ok hfwd with ⟨context2, hpush, hfwd⟩
          rcases Prod.mk.inj (Prod.mk.inj (ok_inj hfwd)).2 with ⟨hctx_out, rfl⟩
          subst ctx_out
          obtain ⟨_, _, hp3, _, _, _, hp7⟩ := VerificationContext_push_successor_ok_spec hpush
          -- Unpack backward `DependencyTracker.load64`:
          dsimp [DepTrackerVisitorInst] at hback
          unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.load64 at hback
          rcases bind_eq_ok hback with ⟨⟨_, dd⟩, hrem_dst, hback⟩
          rcases Prod.mk.inj (Prod.mk.inj (ok_inj hback)).2 with ⟨rfl, rfl⟩
          obtain ⟨_, _, _, hrem_r, hrem_s⟩ := DataDependencies_remove_reg_ok_spec hrem_dst
          refine ⟨rfl, rfl, rfl, ?_⟩
          have hlt1 : ctx.pc.val + 1 < prog.size := hcont.1
          have hcont_at : (prog[ctx.pc.val + 1]'hlt1 == .lddwCont) = true := by
            have h2 := hcont.2
            rw [Array.getElem?_eq_getElem hlt1] at h2
            rw [Option.some.inj h2]
            rfl
          have hlow_eta : (⟨lower.bv⟩ : Std.U32) = lower := rfl
          -- Split on the 3 `ResolvedLddw` cases:
          rcases hres_spec with ⟨_, rfl⟩ | ⟨schema, _, hm1, _, hmap_get, rfl⟩ |
              ⟨schema, _, hm1, hm2, _, hmap_get, hmap_arr, hmax_ne0, hoff_lt, rfl⟩
          · -- Case 1: `ResolvedLddw.Imm val`
            dsimp only at hcv
            rcases bind_eq_ok hcv with ⟨context2_l, hemit, hcv⟩
            rw [bind_ok_eq (from_known_64_eq _)] at hcv
            rcases Prod.mk.inj (ok_inj hcv) with ⟨rfl, rfl⟩
            obtain ⟨_, _, he3, _, _, he6⟩ := VerificationContext_emit_log_ok_spec hemit
            have hn_valid : CompCtxValid env vctx0.counter.bv.toNat n := by rw [hn_eq]; exact hctx
            obtain ⟨hdst_lt, hpc_n1, hregs_n1, hstk_n1, hbm_n1, hres_n1, _⟩ :=
              ComputationContext_set_reg_ok_spec hn_valid
                (show TypeValid env vctx0.counter.bv.toNat (.ScalarValue _) from knownScalar64_valid _) hset
            have hmem_n1 : some n1 ∈ context2.pending_successors.val := by
              rw [hp7, he6, hpending]; simp
            have hcnt_eq : context2.counter.bv.toNat = vctx0.counter.bv.toNat := by
              rw [hp3, he3]
            have hsem_n1 : SemTerminatesIn prog env vctx0.counter.bv.toNat n1 dPost F := by
              rw [← hcnt_eq]; exact hchildren n1 hmem_n1
            intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
            let immU64 : Std.U64 := ⟨lower.bv.zeroExtend 64 ||| (((raw2.bv >>> 32).setWidth 32).zeroExtend 64 <<< 32)⟩
            have hrel_val : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen
                (.ScalarValue (knownScalar64 immU64)) (.scalar immU64.bv) (.scalar immU64.bv) :=
              TypeRel2_scalar_rel2_of_concretizes (concretizesScalar64Rel2_known immU64)
            have hrel_post : StateRel2Deps env n1 dPost ρ resActive isNullId
                { pc := n1.pc.val, regs := updateReg st₁.regs dst.val (.scalar immU64.bv),
                  stack := st₁.stack, world := bumpWorldStep st₁.world }
                { pc := n1.pc.val, regs := updateReg st₂.regs dst.val (.scalar immU64.bv),
                  stack := st₂.stack, world := bumpWorldStep st₂.world } := by
              rcases hrel with ⟨_, _, hw, harr, hnull, hnd, hlv, hbnd, hr10_1, hr10_2, hrg, hsk⟩
              have hres_eq : n1.resources = ctx.resources := by rw [hres_n1, hn_eq]
              have hbm_eq : n1.array_bounds = ctx.array_bounds := by rw [hbm_n1, hn_eq]
              have hstk_eq : n1.stack = ctx.stack := by rw [hstk_n1, hn_eq]
              refine ⟨rfl, rfl, by simp [bumpWorldStep, hw], harr, hnull, hnd,
                by rw [hres_eq]; exact hlv, by rw [hbm_eq]; exact hbnd,
                by simp [updateReg, show GENERAL_REGISTER_COUNT ≠ dst.val by omega, hr10_1],
                by simp [updateReg, show GENERAL_REGISTER_COUNT ≠ dst.val by omega, hr10_2],
                ?_, ?_⟩
              · intro r hr hlen hd
                rw [hn_eq] at hregs_n1
                have hget : n1.registers.val[r]'hlen =
                    (ctx.registers.val.set dst.val (.ScalarValue (knownScalar64 immU64)))[r]'(by
                      rw [List.length_set, array10_len]; exact hr) := by
                  simp only [hregs_n1, immU64]
                rw [hget]
                by_cases hrd : r = dst.val
                · subst hrd
                  dsimp only [bumpWorldStep]
                  simpa [updateReg] using hrel_val
                · simp only [List.getElem_set_ne (Ne.symm hrd), updateReg, beq_eq_false_iff_ne.mpr hrd,
                    Bool.false_eq_true, ↓reduceIte, bumpWorldStep]
                  exact hrg r hr (by rw [array10_len]; exact hr)
                    (by rw [hrem_r r (by omega), hd]; simp [hrd])
              · intro s hs hd
                rw [hstk_eq]
                exact hsk s hs (by rwa [hrem_s s])
            have halloc' : (bumpWorldStep st₁.world).nextAllocId + 3 * F < 2 ^ 63 := by
              simp only [bumpWorldStep]; omega
            obtain ⟨ret, finalWorld, hr₁, hr₂, hlive_fin⟩ :=
              hsem_n1 l₁ l₂ s₁ s₂ ρ resActive isNullId
                { pc := n1.pc.val, regs := updateReg st₁.regs dst.val (.scalar immU64.bv),
                  stack := st₁.stack, world := bumpWorldStep st₁.world }
                { pc := n1.pc.val, regs := updateReg st₂.regs dst.val (.scalar immU64.bv),
                  stack := st₂.stack, world := bumpWorldStep st₂.world }
                hl₁ hl₂ hleak halloc' hρ hrel_post
            have hpc_n1_val : n1.pc.val = ctx.pc.val + 2 := by rw [hpc_n1, hpc_n_val]
            refine ⟨ret, finalWorld, ?_, ?_, hlive_fin⟩
            · unfold runSteps step
              dsimp only [lddwInsnOfResolved] at hinsn_at
              simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, checkDst_eq_ok hdst_lt,
                hlt1, hcont_at, ↓reduceIte]
              rw [stepNextIte_eq_next (by dsimp [immU64]; rw [hpc_n1_val]) hr₁]
              exact hr₁
            · unfold runSteps step
              dsimp only [lddwInsnOfResolved] at hinsn_at
              simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, checkDst_eq_ok hdst_lt,
                hlt1, hcont_at, ↓reduceIte]
              rw [stepNextIte_eq_next (by dsimp [immU64]; rw [hpc_n1_val]) hr₂]
              exact hr₂
          · -- Case 2: `ResolvedLddw.MapPtr ⟨lower.bv⟩ schema`
            rw [hlow_eta] at hcv hinsn_at
            dsimp only at hcv
            rcases bind_eq_ok hcv with ⟨context2_l, hemit, hcv⟩
            rcases Prod.mk.inj (ok_inj hcv) with ⟨rfl, rfl⟩
            obtain ⟨_, _, he3, _, _, he6⟩ := VerificationContext_emit_log_ok_spec hemit
            have hmap_env : env.maps lower.val = some (MapSpec.ofCore schema) := by
              have hm := hcctx.2.2.1 lower.bv.toNat
              rw [hmap_get] at hm
              exact hm
            have hval_valid : TypeValid env vctx0.counter.bv.toNat
                (.ConstPtrToMap (UScalar.cast .U64 lower) schema) := by
              dsimp only [TypeValid]
              rwa [uscalar_cast_u64_of_u32_toNat]
            have hn_valid : CompCtxValid env vctx0.counter.bv.toNat n := by rw [hn_eq]; exact hctx
            obtain ⟨hdst_lt, hpc_n1, hregs_n1, hstk_n1, hbm_n1, hres_n1, _⟩ :=
              ComputationContext_set_reg_ok_spec hn_valid hval_valid hset
            have hmem_n1 : some n1 ∈ context2.pending_successors.val := by
              rw [hp7, he6, hpending]; simp
            have hcnt_eq : context2.counter.bv.toNat = vctx0.counter.bv.toNat := by
              rw [hp3, he3]
            have hsem_n1 : SemTerminatesIn prog env vctx0.counter.bv.toNat n1 dPost F := by
              rw [← hcnt_eq]; exact hchildren n1 hmem_n1
            intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
            have hrel_val : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen
                (.ConstPtrToMap (UScalar.cast .U64 lower) schema)
                (.mapPtr lower.val) (.mapPtr lower.val) := by
              dsimp only [TypeRel2]
              rw [uscalar_cast_u64_of_u32_toNat]
              exact ⟨hmap_env, rfl, rfl⟩
            have hrel_post : StateRel2Deps env n1 dPost ρ resActive isNullId
                { pc := n1.pc.val, regs := updateReg st₁.regs dst.val (.mapPtr lower.val),
                  stack := st₁.stack, world := bumpWorldStep st₁.world }
                { pc := n1.pc.val, regs := updateReg st₂.regs dst.val (.mapPtr lower.val),
                  stack := st₂.stack, world := bumpWorldStep st₂.world } := by
              rcases hrel with ⟨_, _, hw, harr, hnull, hnd, hlv, hbnd, hr10_1, hr10_2, hrg, hsk⟩
              have hres_eq : n1.resources = ctx.resources := by rw [hres_n1, hn_eq]
              have hbm_eq : n1.array_bounds = ctx.array_bounds := by rw [hbm_n1, hn_eq]
              have hstk_eq : n1.stack = ctx.stack := by rw [hstk_n1, hn_eq]
              refine ⟨rfl, rfl, by simp [bumpWorldStep, hw], harr, hnull, hnd,
                by rw [hres_eq]; exact hlv, by rw [hbm_eq]; exact hbnd,
                by simp [updateReg, show GENERAL_REGISTER_COUNT ≠ dst.val by omega, hr10_1],
                by simp [updateReg, show GENERAL_REGISTER_COUNT ≠ dst.val by omega, hr10_2],
                ?_, ?_⟩
              · intro r hr hlen hd
                rw [hn_eq] at hregs_n1
                have hget : n1.registers.val[r]'hlen =
                    (ctx.registers.val.set dst.val
                      (.ConstPtrToMap (UScalar.cast .U64 lower) schema))[r]'(by
                        rw [List.length_set, array10_len]; exact hr) := by
                  simp only [hregs_n1]
                rw [hget]
                by_cases hrd : r = dst.val
                · subst hrd
                  dsimp only [bumpWorldStep]
                  simpa [updateReg] using hrel_val
                · simp only [List.getElem_set_ne (Ne.symm hrd), updateReg, beq_eq_false_iff_ne.mpr hrd,
                    Bool.false_eq_true, ↓reduceIte, bumpWorldStep]
                  exact hrg r hr (by rw [array10_len]; exact hr)
                    (by rw [hrem_r r (by omega), hd]; simp [hrd])
              · intro s hs hd
                rw [hstk_eq]
                exact hsk s hs (by rwa [hrem_s s])
            have halloc' : (bumpWorldStep st₁.world).nextAllocId + 3 * F < 2 ^ 63 := by
              simp only [bumpWorldStep]; omega
            obtain ⟨ret, finalWorld, hr₁, hr₂, hlive_fin⟩ :=
              hsem_n1 l₁ l₂ s₁ s₂ ρ resActive isNullId
                { pc := n1.pc.val, regs := updateReg st₁.regs dst.val (.mapPtr lower.val),
                  stack := st₁.stack, world := bumpWorldStep st₁.world }
                { pc := n1.pc.val, regs := updateReg st₂.regs dst.val (.mapPtr lower.val),
                  stack := st₂.stack, world := bumpWorldStep st₂.world }
                hl₁ hl₂ hleak halloc' hρ hrel_post
            have hpc_n1_val : n1.pc.val = ctx.pc.val + 2 := by rw [hpc_n1, hpc_n_val]
            refine ⟨ret, finalWorld, ?_, ?_, hlive_fin⟩
            · unfold runSteps step
              dsimp only [lddwInsnOfResolved] at hinsn_at
              simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, checkDst_eq_ok hdst_lt,
                hmap_env, hlt1, hcont_at, ↓reduceIte]
              rw [stepNextIte_eq_next (by rw [hpc_n1_val]) hr₁]
              exact hr₁
            · unfold runSteps step
              dsimp only [lddwInsnOfResolved] at hinsn_at
              simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, checkDst_eq_ok hdst_lt,
                hmap_env, hlt1, hcont_at, ↓reduceIte]
              rw [stepNextIte_eq_next (by rw [hpc_n1_val]) hr₂]
              exact hr₂
          · -- Case 3: `ResolvedLddw.MapValuePtr ⟨lower.bv⟩ schema ⟨off32⟩`
            rw [hlow_eta] at hcv hinsn_at
            dsimp only at hcv
            let offU32 : Std.U32 := ⟨(raw2.bv >>> 32).setWidth 32⟩
            rcases bind_eq_ok hcv with ⟨context2_l, hemit, hcv⟩
            obtain ⟨_, _, he3, _, _, he6⟩ := VerificationContext_emit_log_ok_spec hemit
            rcases bind_eq_ok hcv with ⟨p_nid, hnid, _⟩
            rcases p_nid with ⟨id, context3⟩
            rw [bind_pair_ok_eq hnid] at hcv
            obtain ⟨hid_eq, hc3_eq, hc3_cnt⟩ := VerificationContext_next_id_ok_spec hnid
            obtain ⟨mf, b_ro, hmf, hcont_mf, hbro_eq⟩ := MapFlags_ProgReadOnly_contains_spec schema
            rw [bind_ok_eq hmf, bind_ok_eq hcont_mf] at hcv
            rcases bind_eq_ok hcv with ⟨origin, horig, hcv⟩
            simp only [from_known_64_eq, bind_ok] at hcv
            rcases Prod.mk.inj (ok_inj hcv) with ⟨hc1_eq, rfl⟩
            subst context3
            have horig_core : MemoryOrigin.ofCore origin =
                (if (MapSpec.ofCore schema).progReadOnly then MemoryOrigin.mapReadOnly
                 else .mapWritable) := by
              rw [← hbro_eq]
              cases b_ro <;> simp only [Bool.false_eq_true, ↓reduceIte] at horig ⊢ <;>
                cases ok_inj horig <;> rfl
            have hcnt_c3 : context1.counter.bv.toNat = vctx0.counter.bv.toNat + 1 := by
              rw [hc3_cnt, he3]
            have hctx_c1 : CompCtxValid env context1.counter.bv.toNat ctx :=
              CompCtxValid_mono (by omega) hctx
            have hn_valid : CompCtxValid env context1.counter.bv.toNat n := by
              rw [hn_eq]; exact hctx_c1
            have hpov : PtrOffsetValid 0xFFFF_FFFF#64 (knownScalar64 (UScalar.cast .U64 offU32)) :=
              ⟨knownScalar64_valid _, rfl, u64_cast_of_u32_le_u32max offU32⟩
            have hval_valid : TypeValid env context1.counter.bv.toNat
                (.PtrToMemory id (knownScalar64 (UScalar.cast .U64 offU32))
                  (UScalar.cast .U64 schema.value_size) origin) := by
              refine ⟨?_, hpov⟩
              rw [hid_eq, he3]
              simp only [memIdBounded, decide_eq_true_eq]
              omega
            obtain ⟨hdst_lt, hpc_n1, hregs_n1, hstk_n1, hbm_n1, hres_n1, _⟩ :=
              ComputationContext_set_reg_ok_spec hn_valid hval_valid hset
            have hmem_n1 : some n1 ∈ context2.pending_successors.val := by
              rw [hp7, hc3_eq, he6, hpending]; simp
            have hcnt_c2 : context2.counter.bv.toNat = context1.counter.bv.toNat := by
              rw [hp3]
            have hsem_n1 : SemTerminatesIn prog env context1.counter.bv.toNat n1 dPost F := by
              rw [← hcnt_c2]; exact hchildren n1 hmem_n1
            have hmap_env : env.maps lower.val = some (MapSpec.ofCore schema) := by
              have hm := hcctx.2.2.1 lower.bv.toNat
              rw [hmap_get] at hm
              exact hm
            intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
            -- Extend `ρ` with `vctx0.counter ↦ ⟨BitVec.ofNat 64 st₁.world.nextAllocId⟩`:
            let newU64 : Std.U64 := ⟨BitVec.ofNat 64 st₁.world.nextAllocId⟩
            have hnew_toNat : newU64.bv.toNat = st₁.world.nextAllocId := by
              dsimp only [newU64]
              rw [BitVec.toNat_ofNat, Nat.mod_eq_of_lt (by omega)]
            obtain ⟨hρ', hag, hρ_old⟩ :=
              RhoValid_extend (rt' := st₁.world.nextAllocId + 1) (oldId := vctx0.counter)
                (newId := newU64) hρ rfl (by rw [hnew_toNat]) (by rw [hnew_toNat]; omega)
                (by rw [hnew_toNat]; omega)
            rw [← hcnt_c3] at hρ'
            let ρ' := fun x => if x = vctx0.counter then newU64 else ρ x
            let rid := freshRuntimeId st₁.world.nextAllocId
            have hrename_id : renameMemId ρ' id = rid := by
              rw [hid_eq, he3]
              simp only [renameMemId, renameNamespaceId, ρ', ↓reduceIte, rid, freshRuntimeId, newU64]
            let ptrVal := RuntimeVal.memPtr rid (MemoryOrigin.ofCore origin) none
              (MapSpec.ofCore schema).valueSize (offU32.bv.zeroExtend 64)
            have hrel_val : TypeRel2 env ρ' resActive isNullId st₁.world.arrayLen
                (.PtrToMemory id (knownScalar64 (UScalar.cast .U64 offU32))
                  (UScalar.cast .U64 schema.value_size) origin) ptrVal ptrVal := by
              refine ⟨hpov, offU32.bv.zeroExtend 64, ?_, ?_, ?_⟩
              · have hc := concretizesScalar64Rel2_known (UScalar.cast .U64 offU32)
                rwa [u64_bv_cast_of_u32] at hc
              · dsimp only [ptrVal, MapSpec.ofCore]
                rw [hrename_id, u64_bv_cast_of_u32]
              · dsimp only [ptrVal, MapSpec.ofCore]
                rw [hrename_id, u64_bv_cast_of_u32]
            let wrld₁' := { st₁.world with nextAllocId := st₁.world.nextAllocId + 1 }
            let wrld₂' := { st₂.world with nextAllocId := st₂.world.nextAllocId + 1 }
            have hw'_eq : wrld₁' = wrld₂' := by dsimp only [wrld₁', wrld₂']; rw [hrel.2.2.1]
            have hrel_post : StateRel2Deps env n1 dPost ρ' resActive isNullId
                { pc := n1.pc.val, regs := updateReg st₁.regs dst.val ptrVal,
                  stack := st₁.stack, world := bumpWorldStep wrld₁' }
                { pc := n1.pc.val, regs := updateReg st₂.regs dst.val ptrVal,
                  stack := st₂.stack, world := bumpWorldStep wrld₂' } := by
              rcases hrel with ⟨_, _, _, harr, hnull, hnd, hlv, hbnd, hr10_1, hr10_2, hrg, hsk⟩
              have hres_eq : n1.resources = ctx.resources := by rw [hres_n1, hn_eq]
              have hbm_eq : n1.array_bounds = ctx.array_bounds := by rw [hbm_n1, hn_eq]
              have hstk_eq : n1.stack = ctx.stack := by rw [hstk_n1, hn_eq]
              have hag_sym : ∀ x : Std.U64, x.bv.toNat < vctx0.counter.bv.toNat → ρ x = ρ' x :=
                fun x hx => (hag x hx).symm
              refine ⟨rfl, rfl, by simp [bumpWorldStep, hw'_eq], harr, hnull, hnd, ?_, ?_,
                by simp [updateReg, show GENERAL_REGISTER_COUNT ≠ dst.val by omega, hr10_1],
                by simp [updateReg, show GENERAL_REGISTER_COUNT ≠ dst.val by omega, hr10_2],
                ?_, ?_⟩
              · intro m
                rw [hres_eq]; dsimp only [bumpWorldStep, wrld₁']; rw [hlv m]
                constructor
                · rintro ⟨r_m, hr_in, hact, rfl⟩
                  have hrmB := hctx.2.2.2.2 r_m hr_in
                  rw [renameMemId_congr hag_sym hrmB] at hact ⊢
                  exact ⟨r_m, hr_in, hact, rfl⟩
                · rintro ⟨r_m, hr_in, hact, rfl⟩
                  have hrmB := hctx.2.2.2.2 r_m hr_in
                  rw [← renameMemId_congr hag_sym hrmB] at hact ⊢
                  exact ⟨r_m, hr_in, hact, rfl⟩
              · intro m bound hmem_b
                rw [hbm_eq] at hmem_b
                have hmB : memIdBounded vctx0.counter.bv.toNat m = true :=
                  hctx.2.2.1.2 (m, bound) hmem_b
                dsimp only [bumpWorldStep, wrld₁']
                rw [← renameMemId_congr hag_sym hmB]
                exact hbnd m bound hmem_b
              · intro r hr hlen hd
                rw [hn_eq] at hregs_n1
                have hlen_set : r < (ctx.registers.val.set dst.val
                    (.PtrToMemory id (knownScalar64 (UScalar.cast .U64 offU32))
                      (UScalar.cast .U64 schema.value_size) origin)).length :=
                  hregs_n1 ▸ hlen
                have hget : n1.registers.val[r]'hlen =
                    (ctx.registers.val.set dst.val
                      (.PtrToMemory id (knownScalar64 (UScalar.cast .U64 offU32))
                        (UScalar.cast .U64 schema.value_size) origin))[r]'hlen_set := by
                  simp only [hregs_n1]
                rw [hget]
                by_cases hrd : r = dst.val
                · subst hrd
                  simp only [List.getElem_set_self, updateReg, beq_self_eq_true, ↓reduceIte]
                  exact hrel_val
                · have hlen0 : r < ctx.registers.val.length := by rw [array10_len]; exact hr
                  simp only [List.getElem_set_ne (Ne.symm hrd), updateReg, beq_eq_false_iff_ne.mpr hrd,
                    Bool.false_eq_true, ↓reduceIte]
                  have ht_valid := hctx.1 _ (List.getElem_mem hlen0)
                  exact TypeRel2_congr ht_valid hag_sym
                    (fun m hm => by rw [renameMemId_congr hag_sym hm])
                    (fun m hm => by rw [renameMemId_congr hag_sym hm])
                    (fun m hm h => by change st₁.world.arrayLen _ ≤ _; rwa [← renameMemId_congr hag_sym hm])
                    (hrg r hr hlen0 (by rw [hrem_r r (by omega), hd]; simp [hrd]))
              · intro s hs hd
                rw [hstk_eq]
                dsimp only [bumpWorldStep, wrld₁']
                have hst_valid := StackValid_slot_valid hctx.2.1 s
                exact StackSlotRel2_congr hst_valid hag_sym
                  (fun m hm => by rw [renameMemId_congr hag_sym hm])
                  (fun m hm => by rw [renameMemId_congr hag_sym hm])
                  (fun m hm h => by rwa [← renameMemId_congr hag_sym hm])
                  (hsk s hs (by rwa [hrem_s s]))
            have halloc' : (bumpWorldStep wrld₁').nextAllocId + 3 * F < 2 ^ 63 := by
              simp only [bumpWorldStep, wrld₁']; omega
            obtain ⟨ret, finalWorld, hr₁, hr₂, hlive_fin⟩ :=
              hsem_n1 l₁ l₂ s₁ s₂ ρ' resActive isNullId
                { pc := n1.pc.val, regs := updateReg st₁.regs dst.val ptrVal,
                  stack := st₁.stack, world := bumpWorldStep wrld₁' }
                { pc := n1.pc.val, regs := updateReg st₂.regs dst.val ptrVal,
                  stack := st₂.stack, world := bumpWorldStep wrld₂' }
                hl₁ hl₂ hleak halloc' hρ' hrel_post
            have hpc_n1_val : n1.pc.val = ctx.pc.val + 2 := by rw [hpc_n1, hpc_n_val]
            have hmt_arr : ((MapSpec.ofCore schema).mapType != 2#32) = false := by
              dsimp only [MapSpec.ofCore]
              rw [hmap_arr, bpf_map_type_array_eq]
              rfl
            have hme_ne0 : ((MapSpec.ofCore schema).maxEntries == 0#32) = false := by
              dsimp only [MapSpec.ofCore]
              apply beq_eq_false_iff_ne.mpr
              intro hc
              exact hmax_ne0 ((u32_eq_iff_bv _ 0#u32).mpr hc)
            have hn_off_ge : ¬ (offU32.bv.zeroExtend 64 ≥ (MapSpec.ofCore schema).valueSize) := by
              dsimp only [MapSpec.ofCore]
              intro hc
              rw [ge_iff_le, BitVec.le_def] at hc
              have hlt_a : schema.value_size.bv.toNat < 2 ^ 64 :=
                Nat.lt_trans schema.value_size.bv.isLt (by decide)
              have hlt_b : offU32.bv.toNat < 2 ^ 64 :=
                Nat.lt_trans offU32.bv.isLt (by decide)
              change (BitVec.setWidth 64 (U32.bv schema.value_size)).toNat ≤
                (BitVec.setWidth 64 (U32.bv offU32)).toNat at hc
              rw [BitVec.toNat_setWidth, BitVec.toNat_setWidth,
                  Nat.mod_eq_of_lt hlt_a, Nat.mod_eq_of_lt hlt_b] at hc
              have hv_eq : schema.value_size.val = schema.value_size.bv.toNat := rfl
              have hoff_eq_raw : offU32.bv.toNat = ((raw2.bv >>> 32).setWidth 32).toNat := rfl
              omega
            have hn_off_ge_raw : ¬ (((⟨(raw2.bv >>> 32).setWidth 32⟩ : Std.U32).bv).zeroExtend 64 ≥
                (MapSpec.ofCore schema).valueSize) := hn_off_ge
            refine ⟨ret, finalWorld, ?_, ?_, hlive_fin⟩
            · unfold runSteps step
              dsimp only [lddwInsnOfResolved] at hinsn_at
              simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, checkDst_eq_ok hdst_lt,
                hmap_env, hmt_arr, hme_ne0, hn_off_ge_raw, decide_false, Bool.false_or,
                Bool.false_eq_true, hlt1, hcont_at, ↓reduceIte]
              rw [stepNextIte_eq_next (by
                dsimp only [ptrVal, rid, wrld₁']
                rw [hpc_n1_val, horig_core]) hr₁]
              exact hr₁
            · unfold runSteps step
              dsimp only [lddwInsnOfResolved] at hinsn_at
              have hnext_alloc_eq : st₂.world.nextAllocId = st₁.world.nextAllocId := by
                rw [hrel.2.2.1]
              simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, checkDst_eq_ok hdst_lt,
                hmap_env, hmt_arr, hme_ne0, hn_off_ge_raw, decide_false, Bool.false_or,
                Bool.false_eq_true, hlt1, hcont_at, ↓reduceIte]
              rw [stepNextIte_eq_next (by
                dsimp only [ptrVal, rid, wrld₂']
                rw [hpc_n1_val, horig_core, hnext_alloc_eq]) hr₂]
              exact hr₂

/-! ## 2. `CoreCallingContext.find_helper`, `Type.stack_input_size`, & `DependencyTracker.call_external` -/

theorem CoreCallingContext_find_helper_ok_some_spec
    {cctx : verifier.CoreCallingContext} {env : ExecEnv}
    {index : Std.U32} {sig : verifier.FunctionSignature}
    (hcctx : CallingContextEnvMatches cctx env)
    (hfind : verifier.CoreCallingContext.find_helper cctx index = ok (some sig)) :
    ∃ spec, env.helpers index.val = some spec ∧ HelperSigMatches env sig spec := by
  unfold verifier.CoreCallingContext.find_helper verifier.CoreCallingContext.find_helper_loop at hfind
  suffices ∀ n (i : Std.Usize),
      cctx.helpers.val.length - i.val = n →
      i.val ≤ cctx.helpers.val.length →
      loop (fun i1 => verifier.CoreCallingContext.find_helper_loop.body cctx index i1) i =
        ok (some sig) →
      ∃ k, (cctx.helpers.val.drop i.val).find? (fun p => p.1.val == index.val) = some (k, sig) by
    obtain ⟨k, hk⟩ := this cctx.helpers.val.length 0#usize (by simp) (Nat.zero_le _) hfind
    rw [usize_const_val_0, List.drop_zero] at hk
    exact hcctx.2.2.2.1 index.val sig (Or.inr ⟨k, hk⟩)
  intro n
  induction n with
  | zero =>
    intro i hrem hle hl
    have hnlt : ¬ (i < alloc.vec.Vec.len cctx.helpers) := by
      intro hc; have : i.val < cctx.helpers.val.length := hc; omega
    rw [loop_unfold_step] at hl
    unfold verifier.CoreCallingContext.find_helper_loop.body at hl
    simp [hnlt] at hl
  | succ m ih =>
    intro i hrem hle hl
    have hlt_l : i.val < cctx.helpers.val.length := by omega
    have hlt : i < alloc.vec.Vec.len cctx.helpers := hlt_l
    rw [loop_unfold_step] at hl
    unfold verifier.CoreCallingContext.find_helper_loop.body at hl
    simp only [hlt, ↓reduceIte] at hl
    rw [bind_pair_ok_eq (vec_index_usize_eq_ok cctx.helpers i hlt_l)] at hl
    rw [List.drop_eq_getElem_cons hlt_l]
    rcases hp : cctx.helpers.val[i.val]'hlt_l with ⟨k0, fs0⟩
    rw [List.get_eq_getElem, hp] at hl
    by_cases heq : k0 = index
    · subst heq
      simp only [↓reduceIte, bind_ok] at hl
      cases Option.some.inj (ok_inj hl)
      exact ⟨k0, by simp [List.find?]⟩
    · simp only [heq, ↓reduceIte] at hl
      have hne_val : (k0.val == index.val) = false :=
        beq_eq_false_iff_ne.mpr (fun hc => heq (UScalar.eq_of_val_eq hc))
      rcases bind_eq_ok hl with ⟨r, hbody, hl⟩
      rcases bind_eq_ok hbody with ⟨i3, hi3, hbody⟩
      cases ok_inj hbody
      dsimp only at hl
      have hi3_val : i3.val = i.val + 1 := by
        simpa [usize_one_val] using (uscalar_add_ok_val hi3).1
      simp only [List.find?, hne_val]
      rw [← hi3_val]
      exact ih i3 (by omega) (by omega) hl

/-- Exact stack-input-size extraction for parameter types when a stack pointer is passed:
    whenever `ParamTypeMatches env paramTy paramSpec` holds, `Type.stack_input_size paramTy ctx`
    returns the exact `Option U64` size needed for any stack read on `paramSpec`. -/
theorem Type_stack_input_size_mapKey
    {ctx : verifier.ComputationContext} {idx : Std.U8} {s : maps.MapSchema}
    (hget : verifier.ComputationContext.get_map_schema ctx idx = ok (.Ok s)) :
    verifier.Type.stack_input_size (.MapKeyParameter idx) ctx =
      ok (.Ok (some (UScalar.cast .U64 s.key_size))) := by
  rw [verifier.Type.stack_input_size.eq_def]
  simp [hget, lift_eq_ok]

theorem Type_stack_input_size_mapValue
    {ctx : verifier.ComputationContext} {idx : Std.U8} {s : maps.MapSchema}
    (hget : verifier.ComputationContext.get_map_schema ctx idx = ok (.Ok s)) :
    verifier.Type.stack_input_size (.MapValueParameter idx) ctx =
      ok (.Ok (some (UScalar.cast .U64 s.value_size))) := by
  rw [verifier.Type.stack_input_size.eq_def]
  simp [hget, lift_eq_ok]

theorem Type_stack_input_size_memory_input
    {ctx : verifier.ComputationContext} {mps : verifier.MemoryParameterSize}
    {out : Bool} {sz : Std.U64}
    (hsz : verifier.MemoryParameterSize.size mps ctx = ok (.Ok sz)) :
    verifier.Type.stack_input_size (.MemoryParameter mps true out) ctx =
      ok (.Ok (some sz)) := by
  rw [verifier.Type.stack_input_size.eq_def]
  simp [hsz]

theorem Type_stack_input_size_nullOr (inner : verifier.Type) (ctx : verifier.ComputationContext) :
    verifier.Type.stack_input_size (.NullOrParameter inner) ctx =
      verifier.Type.stack_input_size inner ctx := by
  rw [verifier.Type.stack_input_size.eq_def]

theorem Type_stack_input_size_releasable (id : MemoryId) (inner : verifier.Type)
    (ctx : verifier.ComputationContext) :
    verifier.Type.stack_input_size (.ReleasableParameter id inner) ctx =
      verifier.Type.stack_input_size inner ctx := by
  rw [verifier.Type.stack_input_size.eq_def]

private theorem cast_512_usize_eq_u64 : UScalar.cast .U64 512#usize = 512#u64 := by
  apply (u64_eq_iff_bv _ _).mpr
  change (UScalar.cast .U64 512#usize).bv = (512#u64).bv
  rw [uscalar_cast_u64_of_usize_bv, usize_const_val_512]
  rfl

/-- Helper connecting `DependencyTracker.track_call_arg`'s stack-range bitmask logic to
    byte-level slot coverage `(offset.value.bv.toNat + k) / 8` for `k < input_size.bv.toNat`. -/
private theorem depTracker_stack_range_covers
    {cur curOut : verifier.DependencyTracker}
    {offset : verifier.StackOffset} {input_size : Std.U64}
    (hbranch : (do
      let r4 ← verifier.StackOffset.check_valid_offset offset
      match r4 with
      | core.result.Result.Ok _ =>
        let «end» ← verifier.StackOffset.add_u64 offset input_size
        let i3 ← lift (UScalar.cast .U64 api.BPF_STACK_SIZE)
        if input_size > i3 then
          ok (core.result.Result.Err error.VerifierErrorKind.StackAccessOutOfBounds, cur)
        else
          let b ← verifier.StackOffset.is_within_stack «end»
          if b then
            let start_idx ← verifier.StackOffset.array_index offset
            let end_idx ← verifier.StackOffset.array_index «end»
            let dd ← verifier.DataDependencies.add_stack_range cur.deps start_idx end_idx
            let i4 ← verifier.StackOffset.sub_index «end»
            if i4 != 0#usize then
              let dd1 ← verifier.DataDependencies.add_stack dd end_idx
              ok (core.result.Result.Ok (), { cur with deps := dd1 })
            else
              ok (core.result.Result.Ok (), { cur with deps := dd })
          else
            ok (core.result.Result.Err error.VerifierErrorKind.StackAccessOutOfBounds, cur)
      | core.result.Result.Err _ => ok (r4, cur)) = ok (.Ok (), curOut)) :
    curOut.context = cur.context ∧
    DepsSubset cur.deps curOut.deps ∧
    (∀ slot < 64,
      (∃ k < input_size.bv.toNat, (offset.value.bv.toNat + k) / 8 = slot) →
      DataDepsHasStack curOut.deps slot = true) := by
  obtain ⟨rchk, hrchk, hchk_iff⟩ := StackOffset_check_valid_offset_spec offset
  rw [bind_ok_eq hrchk] at hbranch
  cases rchk with
  | Err _ => cases (Prod.mk.inj (ok_inj hbranch)).1
  | Ok u0 =>
    dsimp only at hbranch
    obtain ⟨hoff_kn, hoff_lt⟩ := (hchk_iff u0).mp rfl
    have hend_eq : verifier.StackOffset.add_u64 offset input_size =
        ok (knownScalar64 ⟨offset.value.bv + input_size.bv⟩) := by
      unfold verifier.StackOffset.add_u64 U64ScalarValueData.add_u64
        U64ScalarValueData.add_scalar U64ScalarValueData.is_known lift
      rw [bind_ok_eq (from_known_64_eq input_size)]
      have hkn_b : (knownScalar64 input_size).unknown_mask = 0#u64 := rfl
      simp only [hoff_kn, hkn_b, decide_true, ↓reduceIte, bind_ok,
        u64_wrapping_add_eq, from_known_64_eq]
      rfl
    rw [bind_ok_eq hend_eq] at hbranch
    simp only [lift_eq_ok, bind_ok, api_BPF_STACK_SIZE_eq, cast_512_usize_eq_u64] at hbranch
    by_cases hbgt : input_size > 512#u64
    · simp only [hbgt, ↓reduceIte] at hbranch
      cases (Prod.mk.inj (ok_inj hbranch)).1
    · simp only [hbgt, ↓reduceIte] at hbranch
      have hble : input_size.bv ≤ 512#64 := by
        have hnot : ¬ (512#64 < input_size.bv) :=
          fun hc => hbgt ((u64_gt_iff_bv input_size 512#u64).mpr hc)
        rw [BitVec.lt_def] at hnot; rw [BitVec.le_def]; omega
      rw [bind_ok_eq (StackOffset_is_within_stack_spec _)] at hbranch
      have h1_512 : offset.value.bv.toNat < 512 := by rw [BitVec.lt_def] at hoff_lt; simpa using hoff_lt
      have h2_512 : input_size.bv.toNat ≤ 512 := by rw [BitVec.le_def] at hble; simpa using hble
      have hkn_end : (knownScalar64 ⟨offset.value.bv + input_size.bv⟩).unknown_mask = 0#u64 := rfl
      have hval_end : (knownScalar64 ⟨offset.value.bv + input_size.bv⟩).value.bv =
          offset.value.bv + input_size.bv := rfl
      by_cases hwithin : offset.value.bv + input_size.bv ≤ 512#64
      · have hw_dec : decide ((knownScalar64 ⟨offset.value.bv + input_size.bv⟩).unknown_mask = 0#u64 ∧
            (knownScalar64 ⟨offset.value.bv + input_size.bv⟩).value.bv ≤ 512#64) = true :=
          decide_eq_true ⟨hkn_end, hwithin⟩
        simp only [hw_dec, ↓reduceIte] at hbranch
        have hsum_toNat : (offset.value.bv + input_size.bv).toNat =
            offset.value.bv.toNat + input_size.bv.toNat := by
          rw [BitVec.toNat_add]; exact Nat.mod_eq_of_lt (by omega)
        have hsum_le512 : offset.value.bv.toNat + input_size.bv.toNat ≤ 512 := by
          rw [← hsum_toNat]; rw [BitVec.le_def] at hwithin; simpa using hwithin
        obtain ⟨start_idx, hstart, hstart_val⟩ := StackOffset_array_index_spec (BitVec.le_of_lt hoff_lt)
        obtain ⟨end_idx, hend_i, hend_val⟩ :=
          StackOffset_array_index_spec (s := knownScalar64 ⟨offset.value.bv + input_size.bv⟩) hwithin
        rw [hval_end, hsum_toNat] at hend_val
        obtain ⟨sub_i, hsub_i, hsub_val, _⟩ :=
          StackOffset_sub_index_spec (s := knownScalar64 ⟨offset.value.bv + input_size.bv⟩) hwithin
        rw [hval_end, hsum_toNat] at hsub_val
        rw [bind_ok_eq hstart, bind_ok_eq hend_i] at hbranch
        rcases bind_eq_ok hbranch with ⟨dd1, hadd_rng, hbranch⟩
        obtain ⟨_, _, hsub1, hrng⟩ := DataDependencies_add_stack_range_ok_spec hadd_rng
        rw [bind_ok_eq hsub_i] at hbranch
        by_cases hrem_nz : (sub_i != 0#usize) = true
        · simp only [hrem_nz, ↓reduceIte] at hbranch
          rcases bind_eq_ok hbranch with ⟨dd2, hadd_end, hbranch⟩
          rcases Prod.mk.inj (ok_inj hbranch) with ⟨_, rfl⟩
          obtain ⟨_, _, _, hstk2, hsub2⟩ := DataDependencies_add_stack_ok_spec hadd_end
          refine ⟨rfl, DepsSubset_trans hsub1 hsub2, fun slot hslot ⟨k, hk, hdiv⟩ => ?_⟩
          have hpos_lt : offset.value.bv.toNat + k < offset.value.bv.toNat + input_size.bv.toNat := by omega
          by_cases hlt_end : slot < end_idx.val
          · exact hsub2.2 slot hslot (hrng slot hslot (by omega) hlt_end)
          · have heq_end : slot = end_idx.val := by omega
            subst heq_end
            rw [hstk2 end_idx.val hslot]; simp
        · simp only [hrem_nz, Bool.false_eq_true, ↓reduceIte] at hbranch
          rcases Prod.mk.inj (ok_inj hbranch) with ⟨_, rfl⟩
          have hsub0 : sub_i.val = 0 := by
            have hz : sub_i = 0#usize := bne_eq_false_iff_eq.mp (Bool.eq_false_iff.mpr hrem_nz)
            rw [hz, usize_const_val_0]
          refine ⟨rfl, hsub1, fun slot hslot ⟨k, hk, hdiv⟩ => ?_⟩
          have hmod0 : (offset.value.bv.toNat + input_size.bv.toNat) % 8 = 0 := by omega
          exact hrng slot hslot (by omega) (by omega)
      · have hw_dec : decide ((knownScalar64 ⟨offset.value.bv + input_size.bv⟩).unknown_mask = 0#u64 ∧
            (knownScalar64 ⟨offset.value.bv + input_size.bv⟩).value.bv ≤ 512#64) = false :=
          decide_eq_false (fun h => hwithin h.2)
        simp only [hw_dec, Bool.false_eq_true, ↓reduceIte] at hbranch
        cases (Prod.mk.inj (ok_inj hbranch)).1

private theorem helper_arg_reg_mask_bv_high {r_in : BitVec 16} {c r : Nat}
    (hc : c = 0 ∨ c = 1 ∨ c = 2 ∨ c = 3 ∨ c = 4 ∨ c = 5)
    (hr : r = 6 ∨ r = 7 ∨ r = 8 ∨ r = 9)
    (hbit : r_in.getLsbD r = true) :
    ((r_in &&& ~~~63#16) ||| ((((1#16) <<< c) - 1#16) <<< 1)).getLsbD r = true := by
  rcases hc with rfl | rfl | rfl | rfl | rfl | rfl <;>
    rcases hr with rfl | rfl | rfl | rfl <;>
    (revert hbit; bv_decide)

private theorem helper_arg_reg_mask_bv_low {r_in : BitVec 16} {c k : Nat}
    (hc : c = 0 ∨ c = 1 ∨ c = 2 ∨ c = 3 ∨ c = 4 ∨ c = 5)
    (hk : k < c) :
    ((r_in &&& ~~~63#16) ||| ((((1#16) <<< c) - 1#16) <<< 1)).getLsbD (k + 1) = true := by
  rcases hc with rfl | rfl | rfl | rfl | rfl | rfl
  · omega
  · have : k = 0 := by omega
    subst this; bv_decide
  · rcases (by omega : k = 0 ∨ k = 1) with rfl | rfl <;> bv_decide
  · rcases (by omega : k = 0 ∨ k = 1 ∨ k = 2) with rfl | rfl | rfl <;> bv_decide
  · rcases (by omega : k = 0 ∨ k = 1 ∨ k = 2 ∨ k = 3) with rfl | rfl | rfl | rfl <;> bv_decide
  · rcases (by omega : k = 0 ∨ k = 1 ∨ k = 2 ∨ k = 3 ∨ k = 4) with rfl | rfl | rfl | rfl | rfl <;>
      bv_decide

private theorem helper_arg_reg_mask_spec {num_args : Std.Usize} {r_in r_out : Std.U16}
    (hle5 : num_args.val ≤ 5)
    (hmask : (do
      let i3 ← 1#u16 <<< num_args
      let i4 ← i3 - 1#u16
      let i5 ← i4 <<< 1#i32
      ok ((r_in &&& ~~~63#u16) ||| i5)) = ok r_out) :
    (∀ k < num_args.val, r_out.bv.getLsbD (k + 1) = true) ∧
    (∀ r < 10, 6 ≤ r → r_in.bv.getLsbD r = true → r_out.bv.getLsbD r = true) := by
  have hshl1 : 1#u16 <<< num_args = ok ⟨(1#16) <<< num_args.val⟩ := by
    have hlt16 : num_args.val < 16 := by omega
    change UScalar.shiftLeft_UScalar 1#u16 num_args = _
    simp only [UScalar.shiftLeft_UScalar, UScalar.shiftLeft, UScalarTy.numBits, hlt16, ↓reduceIte]
    rfl
  rw [bind_ok_eq hshl1] at hmask
  have hc : num_args.val = 0 ∨ num_args.val = 1 ∨ num_args.val = 2 ∨
      num_args.val = 3 ∨ num_args.val = 4 ∨ num_args.val = 5 := by omega
  have hval : r_out.bv = (r_in.bv &&& ~~~63#16) ||| ((((1#16) <<< num_args.val) - 1#16) <<< 1) := by
    rcases hc with hn | hn | hn | hn | hn | hn <;> {
      rw [hn] at hmask ⊢
      rw [bind_ok_eq (uscalar_sub_eq_ok _ 1#u16 (by decide))] at hmask
      change (do let i5 ← UScalar.shiftLeft_IScalar _ 1#i32; ok _) = ok r_out at hmask
      simp only [UScalar.shiftLeft_IScalar, UScalar.shiftLeft, IScalar.val, UScalarTy.numBits,
        show (1#i32).bv.toInt ≥ 0 by decide, show (1#i32).toNat < 16 by decide, ↓reduceIte,
        bind_ok] at hmask
      rw [← ok_inj hmask]
      rfl
    }
  rw [hval]
  exact ⟨fun k hk => helper_arg_reg_mask_bv_low hc hk,
    fun r hr hge6 hbit => helper_arg_reg_mask_bv_high hc (by omega) hbit⟩

theorem depTracker_call_external_spec
    {ctx : verifier.ComputationContext} {dPost : verifier.DataDependencies}
    {vctx vctx' : verifier.VerificationContext} {index : Std.U32}
    {sig : verifier.FunctionSignature} {tracker : verifier.DependencyTracker}
    (hfind : verifier.CoreCallingContext.find_helper vctx.calling_context index = ok (some sig))
    (hback : DepTrackerVisitorInst.call_external { context := ctx, deps := dPost } vctx index =
      ok (.Ok (), tracker, vctx')) :
    tracker.context = ctx ∧
    vctx' = vctx ∧
    sig.args.val.length ≤ 5 ∧
    (∀ k < sig.args.val.length, DataDepsHasReg tracker.deps (k + 1) = true) ∧
    (∀ r < 10, 6 ≤ r → DataDepsHasReg dPost r = true → DataDepsHasReg tracker.deps r = true) ∧
    (∀ s < 64, DataDepsHasStack dPost s = true → DataDepsHasStack tracker.deps s = true) ∧
    (∀ (k : Nat) (hk : k < sig.args.val.length) (regTy : verifier.Type)
       (offset : verifier.StackOffset) (input_size : Std.U64),
      verifier.ComputationContext.reg ctx ⟨BitVec.ofNat 8 (k + 1)⟩ = ok (.Ok regTy) →
      verifier.Type.inner regTy ctx = ok (.Ok (.PtrToStack offset)) →
      verifier.Type.stack_input_size (sig.args.val[k]'hk) ctx = ok (.Ok (some input_size)) →
      input_size.bv ≠ 0#64 →
      ∀ slot < 64,
        (∃ b < input_size.bv.toNat, (offset.value.bv.toNat + b) / 8 = slot) →
        DataDepsHasStack tracker.deps slot = true) := by
  dsimp [DepTrackerVisitorInst] at hback
  unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.call_external at hback
  rw [bind_ok_eq hfind] at hback
  dsimp only at hback
  simp only [lift_eq_ok, bind_ok] at hback
  have harg5 := bpf_argument_count_val
  by_cases hgt : alloc.vec.Vec.len sig.args > UScalar.cast .Usize api.ARGUMENT_COUNT
  · simp only [hgt, ↓reduceIte] at hback
    rcases bind_eq_ok hback with ⟨_, _, hback⟩
    cases (Prod.mk.inj (ok_inj hback)).1
  · simp only [hgt, ↓reduceIte] at hback
    have hlen_le5 : sig.args.val.length ≤ 5 := by
      have hn : ¬ ((UScalar.cast .Usize api.ARGUMENT_COUNT).val < sig.args.val.length) := hgt
      omega
    rcases bind_eq_ok hback with ⟨⟨self1, loop_err⟩, hloop, hback⟩
    cases loop_err with
    | some _ => cases (Prod.mk.inj (ok_inj hback)).1
    | none =>
      -- Prove loop invariant for `DependencyTracker.call_external_loop`:
      have hloop_spec :
          ∀ n (cur : verifier.DependencyTracker) (arg_idx : Std.Usize),
            sig.args.val.length - arg_idx.val = n →
            arg_idx.val ≤ sig.args.val.length →
            verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.call_external_loop
              cur sig.args (alloc.vec.Vec.len sig.args) none arg_idx = ok (self1, none) →
            self1.context = cur.context ∧
            DepsSubset cur.deps self1.deps ∧
            (∀ (k : Nat) (hk : k < sig.args.val.length), arg_idx.val ≤ k →
              ∀ (regTy : verifier.Type) (offset : verifier.StackOffset) (input_size : Std.U64),
                verifier.ComputationContext.reg cur.context ⟨BitVec.ofNat 8 (k + 1)⟩ = ok (.Ok regTy) →
                verifier.Type.inner regTy cur.context = ok (.Ok (.PtrToStack offset)) →
                verifier.Type.stack_input_size (sig.args.val[k]'hk) cur.context =
                  ok (.Ok (some input_size)) →
                input_size.bv ≠ 0#64 →
                ∀ slot < 64,
                  (∃ b < input_size.bv.toNat, (offset.value.bv.toNat + b) / 8 = slot) →
                  DataDepsHasStack self1.deps slot = true) := by
        intro n
        induction n with
        | zero =>
          intro cur arg_idx hrem hle hl
          have hnlt : ¬ (arg_idx < alloc.vec.Vec.len sig.args) := by
            intro hc; have : arg_idx.val < sig.args.val.length := hc; omega
          unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.call_external_loop at hl
          rw [loop_unfold_step] at hl
          unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.call_external_loop.body at hl
          simp only [hnlt, ↓reduceIte, bind_ok] at hl
          rcases Prod.mk.inj (ok_inj hl) with ⟨rfl, _⟩
          exact ⟨rfl, DepsSubset_refl _, fun k hk hge => absurd hk (by omega)⟩
        | succ m ih =>
          intro cur arg_idx hrem hle hl
          have hlt_l : arg_idx.val < sig.args.val.length := by omega
          have hlt : arg_idx < alloc.vec.Vec.len sig.args := hlt_l
          unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.call_external_loop at hl
          rw [loop_unfold_step] at hl
          unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.call_external_loop.body at hl
          rcases bind_eq_ok hl with ⟨cf, hbody, hl⟩
          simp only [hlt, ↓reduceIte, core.option.Option.is_none, Option.isNone] at hbody
          rw [bind_ok_eq (vec_index_usize_eq_ok sig.args arg_idx hlt_l)] at hbody
          obtain ⟨i_next, hi_next, hi_val⟩ := usize_add_eq_ok arg_idx 1#usize
            (by rw [usize_one_val]; have := sig.args.property; omega)
          rw [usize_one_val] at hi_val
          rcases bind_eq_ok hbody with ⟨⟨step_res, cur_next⟩, hstep_dd, _⟩
          rw [bind_pair_ok_eq hstep_dd] at hbody
          cases step_res with
          | Err e =>
            cases ok_inj hbody
            dsimp only at hl
            -- Next iteration has `loop_err = some e`, which exits immediately with `some e ≠ none`:
            rw [loop_unfold_step] at hl
            dsimp only at hl
            by_cases hlt2 : arg_idx < alloc.vec.Vec.len sig.args
            · simp [hlt2, core.option.Option.is_none] at hl
            · simp [hlt2] at hl
          | Ok u_s =>
            cases u_s
            dsimp only at hbody
            rw [bind_ok_eq hi_next] at hbody
            cases ok_inj hbody
            dsimp only at hl
            -- Prove `cur_next.context = cur.context`, `DepsSubset cur.deps cur_next.deps`, and step coverage at `arg_idx`:
            have hstep_info :
                cur_next.context = cur.context ∧
                DepsSubset cur.deps cur_next.deps ∧
                (∀ (regTy : verifier.Type) (offset : verifier.StackOffset) (input_size : Std.U64),
                  verifier.ComputationContext.reg cur.context ⟨BitVec.ofNat 8 (arg_idx.val + 1)⟩ = ok (.Ok regTy) →
                  verifier.Type.inner regTy cur.context = ok (.Ok (.PtrToStack offset)) →
                  verifier.Type.stack_input_size (sig.args.val[arg_idx.val]'hlt_l) cur.context =
                    ok (.Ok (some input_size)) →
                  input_size.bv ≠ 0#64 →
                  ∀ slot < 64,
                    (∃ b < input_size.bv.toNat, (offset.value.bv.toNat + b) / 8 = slot) →
                    DataDepsHasStack cur_next.deps slot = true) := by
              unfold verifier.DependencyTracker.track_call_arg at hstep_dd
              rw [bind_ok_eq hi_next, bind_ok_eq (lift_eq_ok (UScalar.cast .U8 i_next))] at hstep_dd
              rcases bind_eq_ok hstep_dd with ⟨r_reg, hreg_c, hstep_dd⟩
              have hu8_eq : UScalar.cast .U8 i_next = (⟨BitVec.ofNat 8 (arg_idx.val + 1)⟩ : Std.U8) := by
                apply UScalar.eq_of_val_eq
                change (BitVec.setWidth 8 i_next.bv).toNat = (BitVec.ofNat 8 (arg_idx.val + 1)).toNat
                rw [BitVec.toNat_setWidth, BitVec.toNat_ofNat, UScalar.bv_toNat, hi_val]
              rw [hu8_eq] at hreg_c
              cases r_reg with
              | Err _ => cases (Prod.mk.inj (ok_inj hstep_dd)).1
              | Ok reg0 =>
                dsimp only at hstep_dd
                rcases bind_eq_ok hstep_dd with ⟨r1, hinner0, hstep_dd⟩
                cases r1 with
                | Err _ => cases (Prod.mk.inj (ok_inj hstep_dd)).1
                | Ok reg_inner =>
                  cases reg_inner with
                  | PtrToStack off0 =>
                    dsimp only at hstep_dd
                    rcases bind_eq_ok hstep_dd with ⟨r2, hsis0, hstep_dd⟩
                    cases r2 with
                    | Err _ => cases (Prod.mk.inj (ok_inj hstep_dd)).1
                    | Ok o_sz =>
                      cases o_sz with
                      | none =>
                        rcases Prod.mk.inj (ok_inj hstep_dd) with ⟨_, rfl⟩
                        refine ⟨rfl, DepsSubset_refl _, fun regTy offset input_size hr1 hr2 hr3 _ => ?_⟩
                        rw [hsis0] at hr3; cases core.result.Result.Ok.inj (ok_inj hr3)
                      | some in_sz =>
                        dsimp only at hstep_dd
                        by_cases heq0 : in_sz = 0#u64
                        · simp only [heq0, ↓reduceIte] at hstep_dd
                          rcases Prod.mk.inj (ok_inj hstep_dd) with ⟨_, rfl⟩
                          refine ⟨rfl, DepsSubset_refl _, fun regTy offset input_size _ _ hr3 hnz => ?_⟩
                          rw [hsis0] at hr3
                          cases Option.some.inj (core.result.Result.Ok.inj (ok_inj hr3))
                          subst heq0
                          exact absurd rfl hnz
                        · simp only [heq0, ↓reduceIte] at hstep_dd
                          obtain ⟨hctx_eq, hsub_d, hcov_d⟩ := depTracker_stack_range_covers hstep_dd
                          refine ⟨hctx_eq, hsub_d, fun regTy offset input_size hr1 hr2 hr3 _ => ?_⟩
                          rw [hreg_c] at hr1; cases core.result.Result.Ok.inj (ok_inj hr1)
                          rw [hinner0] at hr2; cases core.result.Result.Ok.inj (ok_inj hr2)
                          rw [hsis0] at hr3
                          cases Option.some.inj (core.result.Result.Ok.inj (ok_inj hr3))
                          exact hcov_d
                  | _ =>
                    rcases Prod.mk.inj (ok_inj hstep_dd) with ⟨_, rfl⟩
                    refine ⟨rfl, DepsSubset_refl _, fun regTy offset _ hr1 hr2 => ?_⟩
                    rw [hreg_c] at hr1; cases core.result.Result.Ok.inj (ok_inj hr1)
                    rw [hinner0] at hr2; cases core.result.Result.Ok.inj (ok_inj hr2)
            obtain ⟨hctx_ih, hsub_ih, hcov_ih⟩ :=
              ih cur_next i_next (by omega) (by omega) hl
            refine ⟨hctx_ih.trans hstep_info.1, DepsSubset_trans hstep_info.2.1 hsub_ih,
              fun k hk hge regTy offset input_size hr1 hr2 hr3 hnz slot hslot hex => ?_⟩
            by_cases hkeq : k = arg_idx.val
            · subst hkeq
              exact hsub_ih.2 slot hslot (hstep_info.2.2 regTy offset input_size hr1 hr2 hr3 hnz slot hslot hex)
            · rw [← hstep_info.1] at hr1 hr2 hr3
              exact hcov_ih k hk (by omega) regTy offset input_size hr1 hr2 hr3 hnz slot hslot hex
      obtain ⟨hctx_s1, hsub_s1, hcov_s1⟩ :=
        hloop_spec sig.args.val.length { context := ctx, deps := dPost } 0#usize
          (by simp) (Nat.zero_le _) hloop
      rcases bind_eq_ok hback with ⟨i3, hi3, hback⟩
      rcases bind_eq_ok hback with ⟨i4, hi4, hback⟩
      rcases bind_eq_ok hback with ⟨i5, hi5, hback⟩
      rcases Prod.mk.inj (Prod.mk.inj (ok_inj hback)).2 with ⟨rfl, rfl⟩
      have hmask_op : (do
        let i3' ← 1#u16 <<< alloc.vec.Vec.len sig.args
        let i4' ← i3' - 1#u16
        let i5' ← i4' <<< 1#i32
        ok ((self1.deps.registers &&& ~~~63#u16) ||| i5')) =
        ok ((self1.deps.registers &&& ~~~63#u16) ||| i5) := by
        rw [bind_ok_eq hi3, bind_ok_eq hi4, bind_ok_eq hi5]
      obtain ⟨hargs_live, h6_9_live⟩ :=
        helper_arg_reg_mask_spec (num_args := alloc.vec.Vec.len sig.args)
          (r_in := self1.deps.registers) hlen_le5 hmask_op
      exact ⟨hctx_s1, rfl, hlen_le5,
        fun k hk => hargs_live k hk,
        fun r hr hge6 hd => h6_9_live r hr hge6 (hsub_s1.1 r hr hd),
        fun s hs hd => hsub_s1.2 s hs hd,
        fun k hk => hcov_s1 k hk (by rw [usize_const_val_0]; exact Nat.zero_le _)⟩

/-! ## 3. `Type.match_parameter_type` & `call_external_loop` Soundness -/

theorem matchOneHelperParam_scalar_zero_ok
    {env : ExecEnv} {layout : HostLayout} {regs : Nat → RuntimeVal}
    {preCallStack : StackMem} {helperIdx argIdx : Nat}
    {ps : HelperParamSpec} {stkAcc : StackMem} {wrldAcc : WorldState}
    {out : StackMem × WorldState}
    (h : matchOneHelperParam env layout regs preCallStack helperIdx argIdx ps
      (.scalar 0#64) stkAcc wrldAcc = .ok out) :
    out = (stkAcc, wrldAcc) := by
  cases ps with
  | scalar | nullOr _ | anyArg =>
    simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, RuntimeVal.isScalar] at h
    exact h.symm
  | constPtrToMap _ _ | structArg _ | releasableArg _ _ | releaseArg _ =>
    simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?] at h
  | mapKey idx | mapValue idx =>
    unfold matchOneHelperParam at h
    dsimp only [checkAlive, RuntimeVal.resourceId?] at h
    cases hc : lookupHelperMapArg env regs idx <;> rw [hc] at h <;> cases h
  | memory sz _ _ =>
    unfold matchOneHelperParam at h
    dsimp only [checkAlive, RuntimeVal.resourceId?] at h
    cases hc : resolveHelperMemSize regs sz <;> rw [hc] at h <;> cases h
  | contextArg idx =>
    unfold matchOneHelperParam at h
    dsimp only [checkAlive, RuntimeVal.resourceId?, ContextObjSpec.matchesVal] at h
    cases hc : env.entryContextArgs idx <;> rw [hc] at h
    · cases h
    · rename_i cs; cases cs <;> cases h

theorem matchOneHelperParam_nullOr_of_inner_ok
    {env : ExecEnv} {layout : HostLayout} {regs : Nat → RuntimeVal}
    {preCallStack : StackMem} {helperIdx argIdx : Nat}
    {innerS : HelperParamSpec} {v : RuntimeVal}
    {stkAcc stk' : StackMem} {wrldAcc wrld' : WorldState}
    (h : matchOneHelperParam env layout regs preCallStack helperIdx argIdx innerS
      v stkAcc wrldAcc = .ok (stk', wrld')) :
    matchOneHelperParam env layout regs preCallStack helperIdx argIdx (.nullOr innerS)
      v stkAcc wrldAcc = .ok (stk', wrld') := by
  unfold matchOneHelperParam
  by_cases hv : v = .scalar 0#64
  · subst hv
    rcases Prod.mk.inj (matchOneHelperParam_scalar_zero_ok h) with ⟨rfl, rfl⟩
    simp
  · have hbf : (v == .scalar 0#64) = false := beq_eq_false_iff_ne.mpr hv
    simp [hbf, h]

private theorem inBoundsSpan_of_max_sub {o omax s buf : BitVec 64}
    (ho : o ≤ omax) (hmax_le : omax ≤ buf) (hmax_ne : omax ≠ ~~~0#64)
    (hs_le : s ≤ buf - omax) (hs_ne : s ≠ ~~~0#64) :
    inBoundsSpan o s buf = true := by
  unfold inBoundsSpan; bv_decide

/-- Extracts exact arithmetic and writability facts from the `PtrToMemory` branch of
    `Type.match_parameter_type` on `MemoryParameter size inp out`. -/
private theorem mem_param_ptr_to_mem_ok_spec
    {size : verifier.MemoryParameterSize} {ctx next next' : verifier.ComputationContext}
    {offset : U64ScalarValueData} {buf : Std.U64} {orig : verifier.PointerOrigin}
    {out : Bool}
    (h : (if out then do
            let b ← verifier.PointerOrigin.is_writable orig
            if b then do
              let r ← verifier.MemoryParameterSize.size size ctx
              match r with
              | core.result.Result.Ok s => do
                let offset_max ← scalar_value.U64ScalarValueData.max offset
                if offset_max > buf then
                  ok (core.result.Result.Err error.VerifierErrorKind.OutOfBoundsMemoryAccess, next)
                else if offset_max = core.num.U64.MAX then
                  ok (core.result.Result.Err error.VerifierErrorKind.OutOfBoundsMemoryAccess, next)
                else do
                  let size_left ← buf - offset_max
                  if s > size_left then
                    ok (core.result.Result.Err error.VerifierErrorKind.OutOfBoundsMemoryAccess, next)
                  else if s = core.num.U64.MAX then
                    ok (core.result.Result.Err error.VerifierErrorKind.OutOfBoundsMemoryAccess, next)
                  else ok (core.result.Result.Ok (), next)
              | core.result.Result.Err err => ok (core.result.Result.Err err, next)
            else ok (core.result.Result.Err error.VerifierErrorKind.WriteToReadOnlyMemory, next)
          else do
            let r ← verifier.MemoryParameterSize.size size ctx
            match r with
            | core.result.Result.Ok s => do
              let offset_max ← scalar_value.U64ScalarValueData.max offset
              if offset_max > buf then
                ok (core.result.Result.Err error.VerifierErrorKind.OutOfBoundsMemoryAccess, next)
              else if offset_max = core.num.U64.MAX then
                ok (core.result.Result.Err error.VerifierErrorKind.OutOfBoundsMemoryAccess, next)
              else do
                let size_left ← buf - offset_max
                if s > size_left then
                  ok (core.result.Result.Err error.VerifierErrorKind.OutOfBoundsMemoryAccess, next)
                else if s = core.num.U64.MAX then
                  ok (core.result.Result.Err error.VerifierErrorKind.OutOfBoundsMemoryAccess, next)
                else ok (core.result.Result.Ok (), next)
            | core.result.Result.Err err => ok (core.result.Result.Err err, next)) =
      ok (.Ok (), next')) :
    next' = next ∧
    (out = true → (MemoryOrigin.ofCore orig).isWritable = true) ∧
    ∃ s : Std.U64,
      verifier.MemoryParameterSize.size size ctx = ok (.Ok s) ∧
      offset.urange.max.bv ≤ buf.bv ∧
      offset.urange.max.bv ≠ ~~~0#64 ∧
      s.bv ≤ buf.bv - offset.urange.max.bv ∧
      s.bv ≠ ~~~0#64 := by
  have hcore :
      (do
        let r ← verifier.MemoryParameterSize.size size ctx
        match r with
        | core.result.Result.Ok s => do
          let offset_max ← scalar_value.U64ScalarValueData.max offset
          if offset_max > buf then
            ok (core.result.Result.Err error.VerifierErrorKind.OutOfBoundsMemoryAccess, next)
          else if offset_max = core.num.U64.MAX then
            ok (core.result.Result.Err error.VerifierErrorKind.OutOfBoundsMemoryAccess, next)
          else do
            let size_left ← buf - offset_max
            if s > size_left then
              ok (core.result.Result.Err error.VerifierErrorKind.OutOfBoundsMemoryAccess, next)
            else if s = core.num.U64.MAX then
              ok (core.result.Result.Err error.VerifierErrorKind.OutOfBoundsMemoryAccess, next)
            else ok (core.result.Result.Ok (), next)
        | core.result.Result.Err err => ok (core.result.Result.Err err, next)) =
        ok (.Ok (), next') →
      next' = next ∧
      ∃ s : Std.U64,
        verifier.MemoryParameterSize.size size ctx = ok (.Ok s) ∧
        offset.urange.max.bv ≤ buf.bv ∧
        offset.urange.max.bv ≠ ~~~0#64 ∧
        s.bv ≤ buf.bv - offset.urange.max.bv ∧
        s.bv ≠ ~~~0#64 := by
    intro hc
    rcases bind_eq_ok hc with ⟨r, hsz, hc⟩
    cases r with
    | Err _ => cases (Prod.mk.inj (ok_inj hc)).1
    | Ok s =>
      dsimp only at hc
      have hmax : U64ScalarValueData.max offset = ok offset.urange.max := rfl
      rw [bind_ok_eq hmax] at hc
      by_cases hgt1 : offset.urange.max > buf
      · simp only [hgt1, ↓reduceIte] at hc; cases (Prod.mk.inj (ok_inj hc)).1
      · simp only [hgt1, ↓reduceIte] at hc
        have hle_val : offset.urange.max.val ≤ buf.val := by
          have hn : ¬ (buf.val < offset.urange.max.val) := hgt1; omega
        have hle_bv : offset.urange.max.bv ≤ buf.bv := by
          rw [BitVec.le_def, UScalar.bv_toNat, UScalar.bv_toNat]; exact hle_val
        by_cases heq1 : offset.urange.max = core.num.U64.MAX
        · simp only [heq1, ↓reduceIte] at hc; cases (Prod.mk.inj (ok_inj hc)).1
        · simp only [heq1, ↓reduceIte] at hc
          have hne1 : offset.urange.max.bv ≠ ~~~0#64 := fun h0 =>
            heq1 ((u64_eq_iff_bv _ _).mpr (by rwa [u64_max_bv]))
          rw [bind_ok_eq (uscalar_sub_eq_ok buf offset.urange.max hle_val)] at hc
          by_cases hgt2 : s > ⟨buf.bv - offset.urange.max.bv⟩
          · simp only [hgt2, ↓reduceIte] at hc; cases (Prod.mk.inj (ok_inj hc)).1
          · simp only [hgt2, ↓reduceIte] at hc
            have hs_le : s.bv ≤ buf.bv - offset.urange.max.bv := by
              have hn : ¬ ((buf.bv - offset.urange.max.bv).toNat < s.val) := hgt2
              rw [BitVec.le_def, UScalar.bv_toNat]; omega
            by_cases heq2 : s = core.num.U64.MAX
            · simp only [heq2, ↓reduceIte] at hc; cases (Prod.mk.inj (ok_inj hc)).1
            · simp only [heq2, ↓reduceIte] at hc
              have hne2 : s.bv ≠ ~~~0#64 := fun h0 =>
                heq2 ((u64_eq_iff_bv _ _).mpr (by rwa [u64_max_bv]))
              exact ⟨(Prod.mk.inj (ok_inj hc)).2.symm, s, hsz, hle_bv, hne1, hs_le, hne2⟩
  cases out with
  | false =>
    simp only [Bool.false_eq_true, ↓reduceIte] at h
    obtain ⟨hn, hex⟩ := hcore h
    exact ⟨hn, (fun hc => by cases hc), hex⟩
  | true =>
    simp only [↓reduceIte] at h
    rw [bind_ok_eq (PointerOrigin_is_writable_spec orig)] at h
    cases hw : (MemoryOrigin.ofCore orig).isWritable <;>
      simp only [hw, Bool.false_eq_true, ↓reduceIte] at h
    · cases (Prod.mk.inj (ok_inj h)).1
    · obtain ⟨hn, hex⟩ := hcore h
      exact ⟨hn, fun _ => rfl, hex⟩

/-- Extracts exact arithmetic and updated-stack facts from the `PtrToStack` branch of
    `Type.match_parameter_type` on `MemoryParameter size inp out`. -/
private theorem mem_param_ptr_to_stack_ok_valid
    {env : ExecEnv} {vCounter : Nat}
    {size : verifier.MemoryParameterSize} {ctx next next' : verifier.ComputationContext}
    {offset : verifier.StackOffset} {inp out : Bool}
    (hnext : CompCtxValid env vCounter next)
    (h : (do
            let r ← verifier.MemoryParameterSize.size size ctx
            match r with
            | core.result.Result.Ok s =>
              let i ← lift (UScalar.cast .U64 api.BPF_STACK_SIZE)
              if s > i then
                ok (core.result.Result.Err error.VerifierErrorKind.OutOfBoundsMemoryAccess, next)
              else
                let r1 ← verifier.StackOffset.check_valid_offset offset
                match r1 with
                | core.result.Result.Ok _ =>
                  let buffer_end ← verifier.StackOffset.add_u64 offset s
                  let b ← verifier.StackOffset.is_within_stack buffer_end
                  if b then
                    if out then
                      let (r2, s1) ← verifier.Stack.write_data_ptr next.stack offset s
                      match r2 with
                      | core.result.Result.Ok _ =>
                        if inp then
                          let r3 ← verifier.Stack.read_data_ptr ctx.stack offset s
                          match r3 with
                          | core.result.Result.Ok _ => ok (core.result.Result.Ok (), { next with stack := s1 })
                          | core.result.Result.Err _ => ok (r3, { next with stack := s1 })
                        else ok (core.result.Result.Ok (), { next with stack := s1 })
                      | core.result.Result.Err _ => ok (r2, { next with stack := s1 })
                    else if inp then
                      let r2 ← verifier.Stack.read_data_ptr ctx.stack offset s
                      match r2 with
                      | core.result.Result.Ok _ => ok (core.result.Result.Ok (), next)
                      | core.result.Result.Err _ => ok (r2, next)
                    else ok (core.result.Result.Ok (), next)
                  else ok (core.result.Result.Err error.VerifierErrorKind.OutOfBoundsMemoryAccess, next)
                | core.result.Result.Err _ => ok (r1, next)
            | core.result.Result.Err err => ok (core.result.Result.Err err, next)) =
      ok (.Ok (), next')) :
    next'.pc = next.pc ∧
    next'.registers = next.registers ∧
    next'.array_bounds = next.array_bounds ∧
    CompCtxValid env vCounter next' := by
  rcases bind_eq_ok h with ⟨r, _, h⟩
  cases r with
  | Err _ => cases (Prod.mk.inj (ok_inj h)).1
  | Ok s =>
    simp only [lift_eq_ok, bind_ok] at h
    by_cases hgt : s > UScalar.cast .U64 api.BPF_STACK_SIZE
    · simp only [hgt, ↓reduceIte] at h; cases (Prod.mk.inj (ok_inj h)).1
    · simp only [hgt, ↓reduceIte] at h
      rcases bind_eq_ok h with ⟨r1, _, h⟩
      cases r1 with
      | Err _ => cases (Prod.mk.inj (ok_inj h)).1
      | Ok _ =>
        rcases bind_eq_ok h with ⟨buffer_end, _, h⟩
        rcases bind_eq_ok h with ⟨bw, _, h⟩
        cases bw <;> simp only [Bool.false_eq_true, ↓reduceIte] at h
        · cases (Prod.mk.inj (ok_inj h)).1
        · cases out <;> simp only [Bool.false_eq_true, ↓reduceIte] at h
          · cases inp <;> simp only [Bool.false_eq_true, ↓reduceIte] at h
            · rcases Prod.mk.inj (ok_inj h) with ⟨_, rfl⟩; exact ⟨rfl, rfl, rfl, hnext⟩
            · rcases bind_eq_ok h with ⟨r2, _, h⟩
              cases r2 <;> simp at h
              subst h; exact ⟨rfl, rfl, rfl, hnext⟩
          · rcases bind_eq_ok h with ⟨⟨r2, s1⟩, hwr, h⟩
            cases r2 with
            | Err _ => cases (Prod.mk.inj (ok_inj h)).1
            | Ok _ =>
              obtain ⟨_, s_tot, hw_tot, hs1_valid⟩ := Stack_write_data_ptr_total hnext.2.1 offset s
              rw [hwr] at hw_tot
              rcases Prod.mk.inj (ok_inj hw_tot) with ⟨_, rfl⟩
              cases inp <;> simp only [Bool.false_eq_true, ↓reduceIte] at h
              · rcases Prod.mk.inj (ok_inj h) with ⟨_, rfl⟩
                exact ⟨rfl, rfl, rfl, hnext.1, hs1_valid, hnext.2.2⟩
              · rcases bind_eq_ok h with ⟨r3, _, h⟩
                cases r3 <;> simp at h
                subst h
                exact ⟨rfl, rfl, rfl, hnext.1, hs1_valid, hnext.2.2⟩

/-- Forward validity preservation for `Type.match_parameter_type`. -/
theorem Type_match_parameter_type_ok_valid
    {env : ExecEnv} {vCounter : Nat}
    {argVal paramTy : verifier.Type} {paramSpec : HelperParamSpec}
    {vctx : verifier.VerificationContext}
    {ctx next next' : verifier.ComputationContext}
    {index : Std.Usize}
    (hparam : ParamTypeMatches env paramTy paramSpec)
    (hargV : TypeValid env vCounter argVal)
    (hnext : CompCtxValid env vCounter next)
    (hm : verifier.Type.match_parameter_type argVal vctx ctx paramTy index next =
      ok (.Ok (), next')) :
    next'.pc = next.pc ∧
    next'.registers = next.registers ∧
    next'.array_bounds = next.array_bounds ∧
    CompCtxValid env vCounter next' := by
  induction hparam generalizing argVal next next' with
  | scalar =>
    rw [verifier.Type.match_parameter_type.eq_def] at hm
    cases argVal <;> try (cases (Prod.mk.inj (ok_inj hm)).1)
    · rcases bind_eq_ok hm with ⟨b, _, hm⟩
      cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · cases (Prod.mk.inj (ok_inj hm)).1
      · rcases Prod.mk.inj (ok_inj hm) with ⟨_, rfl⟩; exact ⟨rfl, rfl, rfl, hnext⟩
    · rename_i id inner
      rcases bind_eq_ok hm with ⟨b, _, hm⟩
      cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · cases (Prod.mk.inj (ok_inj hm)).1
      · cases inner <;> try exact False.elim hargV
        · rw [verifier.Type.match_parameter_type.eq_def] at hm
          cases (Prod.mk.inj (ok_inj hm)).1
        · rw [verifier.Type.match_parameter_type.eq_def] at hm
          cases (Prod.mk.inj (ok_inj hm)).1
  | constMapAllow ts w =>
    rw [verifier.Type.match_parameter_type.eq_def] at hm
    cases argVal <;> try (cases (Prod.mk.inj (ok_inj hm)).1)
    · cases w <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · rcases bind_eq_ok hm with ⟨b, _, hm⟩
        cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
        · cases (Prod.mk.inj (ok_inj hm)).1
        · rcases Prod.mk.inj (ok_inj hm) with ⟨_, rfl⟩; exact ⟨rfl, rfl, rfl, hnext⟩
      · rcases bind_eq_ok hm with ⟨_, _, hm⟩
        rcases bind_eq_ok hm with ⟨b, _, hm⟩
        cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
        · rcases bind_eq_ok hm with ⟨b1, _, hm⟩
          cases b1 <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
          · cases (Prod.mk.inj (ok_inj hm)).1
          · rcases Prod.mk.inj (ok_inj hm) with ⟨_, rfl⟩; exact ⟨rfl, rfl, rfl, hnext⟩
        · cases (Prod.mk.inj (ok_inj hm)).1
    · rename_i id inner
      rcases bind_eq_ok hm with ⟨b, _, hm⟩
      cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · cases (Prod.mk.inj (ok_inj hm)).1
      · cases inner <;> try exact False.elim hargV
        · rw [verifier.Type.match_parameter_type.eq_def] at hm
          cases (Prod.mk.inj (ok_inj hm)).1
        · rw [verifier.Type.match_parameter_type.eq_def] at hm
          cases (Prod.mk.inj (ok_inj hm)).1
  | constMapDeny ts w =>
    rw [verifier.Type.match_parameter_type.eq_def] at hm
    cases argVal <;> try (cases (Prod.mk.inj (ok_inj hm)).1)
    · cases w <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · rcases bind_eq_ok hm with ⟨b, _, hm⟩
        cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
        · cases (Prod.mk.inj (ok_inj hm)).1
        · rcases Prod.mk.inj (ok_inj hm) with ⟨_, rfl⟩; exact ⟨rfl, rfl, rfl, hnext⟩
      · rcases bind_eq_ok hm with ⟨_, _, hm⟩
        rcases bind_eq_ok hm with ⟨b, _, hm⟩
        cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
        · rcases bind_eq_ok hm with ⟨b1, _, hm⟩
          cases b1 <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
          · cases (Prod.mk.inj (ok_inj hm)).1
          · rcases Prod.mk.inj (ok_inj hm) with ⟨_, rfl⟩; exact ⟨rfl, rfl, rfl, hnext⟩
        · cases (Prod.mk.inj (ok_inj hm)).1
    · rename_i id inner
      rcases bind_eq_ok hm with ⟨b, _, hm⟩
      cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · cases (Prod.mk.inj (ok_inj hm)).1
      · cases inner <;> try exact False.elim hargV
        · rw [verifier.Type.match_parameter_type.eq_def] at hm
          cases (Prod.mk.inj (ok_inj hm)).1
        · rw [verifier.Type.match_parameter_type.eq_def] at hm
          cases (Prod.mk.inj (ok_inj hm)).1
  | mapKey idx =>
    rw [verifier.Type.match_parameter_type.eq_def] at hm
    cases argVal <;> try (cases (Prod.mk.inj (ok_inj hm)).1)
    · rcases bind_eq_ok hm with ⟨r, _, hm⟩
      cases r <;> dsimp only at hm
      · simp only [lift_eq_ok, bind_ok] at hm
        rcases bind_eq_ok hm with ⟨_, _, hm⟩
        rcases Prod.mk.inj (ok_inj hm) with ⟨_, rfl⟩; exact ⟨rfl, rfl, rfl, hnext⟩
      · cases (Prod.mk.inj (ok_inj hm)).1
    · rcases bind_eq_ok hm with ⟨r, _, hm⟩
      cases r <;> dsimp only at hm
      · simp only [lift_eq_ok, bind_ok] at hm
        rcases bind_eq_ok hm with ⟨_, _, hm⟩
        rcases Prod.mk.inj (ok_inj hm) with ⟨_, rfl⟩; exact ⟨rfl, rfl, rfl, hnext⟩
      · cases (Prod.mk.inj (ok_inj hm)).1
    · rename_i id inner
      rcases bind_eq_ok hm with ⟨b, _, hm⟩
      cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · cases (Prod.mk.inj (ok_inj hm)).1
      · cases inner <;> try exact False.elim hargV
        · rw [verifier.Type.match_parameter_type.eq_def] at hm
          dsimp only at hm
          rcases bind_eq_ok hm with ⟨r, _, hm⟩
          cases r <;> dsimp only at hm
          · simp only [lift_eq_ok, bind_ok] at hm
            rcases bind_eq_ok hm with ⟨_, _, hm⟩
            rcases Prod.mk.inj (ok_inj hm) with ⟨_, rfl⟩; exact ⟨rfl, rfl, rfl, hnext⟩
          · cases (Prod.mk.inj (ok_inj hm)).1
        · rw [verifier.Type.match_parameter_type.eq_def] at hm
          cases (Prod.mk.inj (ok_inj hm)).1
  | mapValue idx =>
    rw [verifier.Type.match_parameter_type.eq_def] at hm
    cases argVal <;> try (cases (Prod.mk.inj (ok_inj hm)).1)
    · rcases bind_eq_ok hm with ⟨r, _, hm⟩
      cases r <;> dsimp only at hm
      · simp only [lift_eq_ok, bind_ok] at hm
        rcases bind_eq_ok hm with ⟨_, _, hm⟩
        rcases Prod.mk.inj (ok_inj hm) with ⟨_, rfl⟩; exact ⟨rfl, rfl, rfl, hnext⟩
      · cases (Prod.mk.inj (ok_inj hm)).1
    · rcases bind_eq_ok hm with ⟨r, _, hm⟩
      cases r <;> dsimp only at hm
      · simp only [lift_eq_ok, bind_ok] at hm
        rcases bind_eq_ok hm with ⟨_, _, hm⟩
        rcases Prod.mk.inj (ok_inj hm) with ⟨_, rfl⟩; exact ⟨rfl, rfl, rfl, hnext⟩
      · cases (Prod.mk.inj (ok_inj hm)).1
    · rename_i id inner
      rcases bind_eq_ok hm with ⟨b, _, hm⟩
      cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · cases (Prod.mk.inj (ok_inj hm)).1
      · cases inner <;> try exact False.elim hargV
        · rw [verifier.Type.match_parameter_type.eq_def] at hm
          dsimp only at hm
          rcases bind_eq_ok hm with ⟨r, _, hm⟩
          cases r <;> dsimp only at hm
          · simp only [lift_eq_ok, bind_ok] at hm
            rcases bind_eq_ok hm with ⟨_, _, hm⟩
            rcases Prod.mk.inj (ok_inj hm) with ⟨_, rfl⟩; exact ⟨rfl, rfl, rfl, hnext⟩
          · cases (Prod.mk.inj (ok_inj hm)).1
        · rw [verifier.Type.match_parameter_type.eq_def] at hm
          cases (Prod.mk.inj (ok_inj hm)).1
  | memFixed sz inp out =>
    rw [verifier.Type.match_parameter_type.eq_def] at hm
    cases argVal <;> try (cases (Prod.mk.inj (ok_inj hm)).1)
    · exact mem_param_ptr_to_stack_ok_valid hnext hm
    · obtain ⟨rfl, _⟩ := mem_param_ptr_to_mem_ok_spec hm
      exact ⟨rfl, rfl, rfl, hnext⟩
    · rename_i id inner
      rcases bind_eq_ok hm with ⟨b, _, hm⟩
      cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · cases (Prod.mk.inj (ok_inj hm)).1
      · cases inner <;> try exact False.elim hargV
        · rw [verifier.Type.match_parameter_type.eq_def] at hm
          obtain ⟨rfl, _⟩ := mem_param_ptr_to_mem_ok_spec hm
          exact ⟨rfl, rfl, rfl, hnext⟩
        · rw [verifier.Type.match_parameter_type.eq_def] at hm
          cases (Prod.mk.inj (ok_inj hm)).1
  | memRef idx inp out =>
    rw [verifier.Type.match_parameter_type.eq_def] at hm
    cases argVal <;> try (cases (Prod.mk.inj (ok_inj hm)).1)
    · exact mem_param_ptr_to_stack_ok_valid hnext hm
    · obtain ⟨rfl, _⟩ := mem_param_ptr_to_mem_ok_spec hm
      exact ⟨rfl, rfl, rfl, hnext⟩
    · rename_i id inner
      rcases bind_eq_ok hm with ⟨b, _, hm⟩
      cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · cases (Prod.mk.inj (ok_inj hm)).1
      · cases inner <;> try exact False.elim hargV
        · rw [verifier.Type.match_parameter_type.eq_def] at hm
          obtain ⟨rfl, _⟩ := mem_param_ptr_to_mem_ok_spec hm
          exact ⟨rfl, rfl, rfl, hnext⟩
        · rw [verifier.Type.match_parameter_type.eq_def] at hm
          cases (Prod.mk.inj (ok_inj hm)).1
  | structArg id _ =>
    rw [verifier.Type.match_parameter_type.eq_def] at hm
    cases argVal <;> try (cases (Prod.mk.inj (ok_inj hm)).1)
    · rcases bind_eq_ok hm with ⟨b, _, hm⟩
      cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · cases (Prod.mk.inj (ok_inj hm)).1
      · rcases bind_eq_ok hm with ⟨b1, _, hm⟩
        cases b1 <;> simp at hm; subst hm; exact ⟨rfl, rfl, rfl, hnext⟩
    · rcases bind_eq_ok hm with ⟨b, _, hm⟩
      cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · cases (Prod.mk.inj (ok_inj hm)).1
      · rcases bind_eq_ok hm with ⟨b1, _, hm⟩
        cases b1 <;> simp at hm; subst hm; exact ⟨rfl, rfl, rfl, hnext⟩
    · rename_i id2 inner
      rcases bind_eq_ok hm with ⟨b, _, hm⟩
      cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · cases (Prod.mk.inj (ok_inj hm)).1
      · cases inner <;> try exact False.elim hargV
        · rw [verifier.Type.match_parameter_type.eq_def] at hm
          dsimp only at hm
          rcases bind_eq_ok hm with ⟨b2, _, hm⟩
          cases b2 <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
          · cases (Prod.mk.inj (ok_inj hm)).1
          · rcases bind_eq_ok hm with ⟨b1, _, hm⟩
            cases b1 <;> simp at hm; subst hm; exact ⟨rfl, rfl, rfl, hnext⟩
        · rw [verifier.Type.match_parameter_type.eq_def] at hm
          dsimp only at hm
          rcases bind_eq_ok hm with ⟨b2, _, hm⟩
          cases b2 <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
          · cases (Prod.mk.inj (ok_inj hm)).1
          · rcases bind_eq_ok hm with ⟨b1, _, hm⟩
            cases b1 <;> simp at hm; subst hm; exact ⟨rfl, rfl, rfl, hnext⟩
  | contextArg idx _ =>
    rw [verifier.Type.match_parameter_type.eq_def] at hm
    rcases bind_eq_ok hm with ⟨r, _, hm⟩
    cases r with
    | Err _ => cases (Prod.mk.inj (ok_inj hm)).1
    | Ok _ =>
      simp only [lift_eq_ok, bind_ok] at hm
      split_ifs at hm
      · rcases bind_eq_ok hm with ⟨t, _, hm⟩
        rcases bind_eq_ok hm with ⟨b, _, hm⟩
        cases b <;> simp at hm
        subst hm; exact ⟨rfl, rfl, rfl, hnext⟩
      · cases (Prod.mk.inj (ok_inj hm)).1
  | releasableArg id innerT innerS _ _ ih =>
    rw [verifier.Type.match_parameter_type.eq_def] at hm
    cases argVal <;> try (cases (Prod.mk.inj (ok_inj hm)).1)
    rename_i id2 inner2
    have hinnerV : TypeValid env vCounter inner2 := by
      cases inner2 <;> try exact False.elim hargV
      · exact ⟨hargV.2.1, hargV.2.2⟩
      · exact ⟨hargV.2.1, hargV.2.2⟩
    rcases bind_eq_ok hm with ⟨b, _, hm⟩
    cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
    · rcases bind_eq_ok hm with ⟨b1, _, hm⟩
      cases b1 <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · cases (Prod.mk.inj (ok_inj hm)).1
      · cases inner2 <;> try exact False.elim hargV
        · rw [verifier.Type.match_parameter_type.eq_def] at hm
          cases (Prod.mk.inj (ok_inj hm)).1
        · rw [verifier.Type.match_parameter_type.eq_def] at hm
          cases (Prod.mk.inj (ok_inj hm)).1
    · rcases bind_eq_ok hm with ⟨b1, _, hm⟩
      cases b1 <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · cases (Prod.mk.inj (ok_inj hm)).1
      · exact ih hinnerV hnext hm
  | releaseArg id _ =>
    rw [verifier.Type.match_parameter_type.eq_def] at hm
    cases argVal <;> try (cases (Prod.mk.inj (ok_inj hm)).1)
    rename_i id2 inner
    rcases bind_eq_ok hm with ⟨b, _, hm⟩
    cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
    · rcases bind_eq_ok hm with ⟨b1, _, hm⟩
      cases b1 <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · cases (Prod.mk.inj (ok_inj hm)).1
      · cases inner <;> try exact False.elim hargV
        · rw [verifier.Type.match_parameter_type.eq_def] at hm
          cases (Prod.mk.inj (ok_inj hm)).1
        · rw [verifier.Type.match_parameter_type.eq_def] at hm
          cases (Prod.mk.inj (ok_inj hm)).1
    · rcases bind_eq_ok hm with ⟨b1, _, hm⟩
      cases b1 <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · cases (Prod.mk.inj (ok_inj hm)).1
      · obtain ⟨_, is_new, hrem_eq, his_valid, _, _⟩ := IdSet_remove_spec hnext.2.2.2 id2
        cases inner <;> try exact False.elim hargV
        · rcases bind_eq_ok hm with ⟨b2, _, hm⟩
          cases b2 <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
          · cases (Prod.mk.inj (ok_inj hm)).1
          · rw [bind_pair_ok_eq hrem_eq] at hm
            rcases Prod.mk.inj (ok_inj hm) with ⟨_, rfl⟩
            exact ⟨rfl, rfl, rfl, hnext.1, hnext.2.1, hnext.2.2.1, his_valid⟩
        · rcases bind_eq_ok hm with ⟨b2, _, hm⟩
          cases b2 <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
          · cases (Prod.mk.inj (ok_inj hm)).1
          · rw [bind_pair_ok_eq hrem_eq] at hm
            rcases Prod.mk.inj (ok_inj hm) with ⟨_, rfl⟩
            exact ⟨rfl, rfl, rfl, hnext.1, hnext.2.1, hnext.2.2.1, his_valid⟩
  | nullOr innerT innerS _ ih =>
    rw [verifier.Type.match_parameter_type.eq_def] at hm
    cases argVal with
    | ScalarValue data =>
      dsimp only at hm
      rcases bind_eq_ok hm with ⟨b, _, hm⟩
      cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · exact ih hargV hnext hm
      · split_ifs at hm
        · rcases Prod.mk.inj (ok_inj hm) with ⟨_, rfl⟩; exact ⟨rfl, rfl, rfl, hnext⟩
        · exact ih hargV hnext hm
    | _ => exact ih hargV hnext hm
  | anyArg =>
    have hleaf : ∀ x,
        (do
          let r ← verifier.Type.check_not_released x next.resources
          match r with
          | core.result.Result.Ok _ =>
            let allowed ←
              if vctx.calling_context.allow_ptr_leaks then verifier.Type.is_initialized x
              else verifier.Type.is_written_scalar x
            if allowed then ok (core.result.Result.Ok (), next)
            else ok (core.result.Result.Err (error.VerifierErrorKind.HelperArgumentTypeMismatch index), next)
          | core.result.Result.Err _ => ok (r, next)) = ok (.Ok (), next') → next' = next := by
      intro x hx
      rcases bind_eq_ok hx with ⟨r, _, hx⟩
      cases r <;> dsimp only at hx
      · rcases bind_eq_ok hx with ⟨al, _, hx⟩
        cases al <;> simp at hx; exact hx.symm
      · cases (Prod.mk.inj (ok_inj hx)).1
    have heq : next' = next := by
      rw [verifier.Type.match_parameter_type.eq_def] at hm
      cases argVal with
      | Releasable id inner =>
        dsimp only at hm
        rcases bind_eq_ok hm with ⟨b, _, hm⟩
        cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
        · cases (Prod.mk.inj (ok_inj hm)).1
        · cases inner <;> try exact False.elim hargV
          · rw [verifier.Type.match_parameter_type.eq_def] at hm
            exact hleaf _ hm
          · rw [verifier.Type.match_parameter_type.eq_def] at hm
            exact hleaf _ hm
      | _ => exact hleaf _ hm
    subst heq; exact ⟨rfl, rfl, rfl, hnext⟩

/-! ### 3b. Relational 2-Safety of `Type.match_parameter_type` & `call_external_loop` -/

/-- Accumulator invariant threaded across `call_external_loop` (`arg_index = 0 ..< num_args`). -/
structure HelperCallAccRel
    (env : ExecEnv) (vCounter : Nat)
    (ctx : verifier.ComputationContext) (dPost : verifier.DataDependencies)
    (ρ : Std.U64 → Std.U64) (resActive isNullId : MemoryId → Bool)
    (st₁ st₂ : MachineState)
    (next : verifier.ComputationContext)
    (stk₁ stk₂ : StackMem) (wrld₁ wrld₂ : WorldState) : Prop where
  pc_val : next.pc.val = ctx.pc.val + 1
  regs_eq : next.registers = ctx.registers
  bounds_eq : next.array_bounds = ctx.array_bounds
  stk_valid : StackValid env vCounter next.stack
  res_valid : IdSetValid vCounter next.resources
  wrld_eq : wrld₁ = wrld₂
  structBytes_eq : wrld₁.structBytes = st₁.world.structBytes
  structFieldNull_eq : wrld₁.structFieldNull = st₁.world.structFieldNull
  arrayLen_eq : wrld₁.arrayLen = st₁.world.arrayLen
  stepCount_eq : wrld₁.stepCount = st₁.world.stepCount
  nextAllocId_eq : wrld₁.nextAllocId = st₁.world.nextAllocId
  live_nodup : wrld₁.liveResources.Nodup
  live_iff : ∀ m : MemoryId,
    m ∈ wrld₁.liveResources ↔
      ∃ r ∈ next.resources.entries.val,
        resActive (renameMemId ρ r) = true ∧ renameMemId ρ r = m
  stk_rel2 : ∀ slot < 64, DataDepsHasStack dPost slot = true →
    StackSlotRel2 env ρ resActive isNullId st₁.world.arrayLen
      (stackSlotType next.stack slot) (stk₁ slot) (stk₂ slot)

private theorem releasable_arg_unwrap_spec
    {env : ExecEnv} {vCounter : Nat}
    {ctx next : verifier.ComputationContext} {dPost : verifier.DataDependencies}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {st₁ st₂ : MachineState} {stk₁ stk₂ : StackMem} {wrld₁ wrld₂ : WorldState}
    {id2 : MemoryId} {inner : verifier.Type} {v₁ v₂ : RuntimeVal}
    (hacc : HelperCallAccRel env vCounter ctx dPost ρ resActive isNullId
      st₁ st₂ next stk₁ stk₂ wrld₁ wrld₂)
    (hargV : TypeValid env vCounter (.Releasable id2 inner))
    (hargRel : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen
      (.Releasable id2 inner) v₁ v₂)
    (hcont : verifier.IdSet.contains next.resources id2 = ok true) :
    memIdBounded vCounter id2 = true ∧
    TypeValid env vCounter inner ∧
    (∀ off, inner ≠ .PtrToStack off) ∧
    (renameMemId ρ id2 ∈ wrld₁.liveResources) ∧
    (wrld₁.liveResources.contains (renameMemId ρ id2) = true) ∧
    (wrld₂.liveResources.contains (renameMemId ρ id2) = true) ∧
    checkAlive wrld₁.liveResources v₁ = none ∧
    checkAlive wrld₂.liveResources v₂ = none ∧
    v₁.resourceId? = some (renameMemId ρ id2) ∧
    v₂.resourceId? = some (renameMemId ρ id2) ∧
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen inner
      v₁.stripResource v₂.stripResource ∧
    (∀ (l : HostLayout) (regs : Nat → RuntimeVal) (preStk : StackMem)
       (hIdx aIdx : Nat) (ps : HelperParamSpec) (stk : StackMem),
      (match ps with
       | .mapKey _ | .mapValue _ | .memory _ _ _ | .structArg _ | .anyArg => true
       | _ => false) = true →
      matchOneHelperParam env l regs preStk hIdx aIdx ps v₁ stk wrld₁ =
        matchOneHelperParam env l regs preStk hIdx aIdx ps v₁.stripResource stk wrld₁ ∧
      matchOneHelperParam env l regs preStk hIdx aIdx ps v₂ stk wrld₂ =
        matchOneHelperParam env l regs preStk hIdx aIdx ps v₂.stripResource stk wrld₂) := by
  have hc_eq := IdSet_contains_spec hacc.res_valid id2
  rw [hcont] at hc_eq
  have hid2_in : id2 ∈ next.resources.entries.val := of_decide_eq_true (ok_inj hc_eq).symm
  cases inner <;> try exact False.elim hargV
  · -- inner = PtrToMemory mid off sz orig
    rcases hargV with ⟨hid2B, hmidB, hpov⟩
    rcases hargRel with ⟨hact, _, o, hconc, hv1, hv2⟩
    have hmem1 : renameMemId ρ id2 ∈ wrld₁.liveResources :=
      (hacc.live_iff _).mpr ⟨id2, hid2_in, hact, rfl⟩
    have hmem2 : renameMemId ρ id2 ∈ wrld₂.liveResources := by
      rw [← hacc.wrld_eq]; exact hmem1
    have hc1 : wrld₁.liveResources.contains (renameMemId ρ id2) = true :=
      List.contains_iff_mem.mpr hmem1
    have hc2 : wrld₂.liveResources.contains (renameMemId ρ id2) = true :=
      List.contains_iff_mem.mpr hmem2
    subst hv1; subst hv2
    refine ⟨hid2B, ⟨hmidB, hpov⟩, (fun _ hc => by cases hc), hmem1, hc1, hc2,
      by simp [checkAlive, RuntimeVal.resourceId?, hmem1],
      by simp [checkAlive, RuntimeVal.resourceId?, hmem2],
      rfl, rfl, ⟨hpov, o, hconc, rfl, rfl⟩, ?_⟩
    intro l regs preStk hIdx aIdx ps stk hps
    by_cases ho0 : o = 0#64
    · subst ho0
      (cases ps <;> try cases hps) <;>
        exact ⟨by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?,
                    RuntimeVal.stripResource, RuntimeVal.isScalar, hmem1],
               by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?,
                    RuntimeVal.stripResource, RuntimeVal.isScalar, hmem2]⟩
    · (cases ps <;> try cases hps) <;>
        exact ⟨by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?,
                    RuntimeVal.stripResource, RuntimeVal.isScalar, hmem1, ho0],
               by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?,
                    RuntimeVal.stripResource, RuntimeVal.isScalar, hmem2, ho0]⟩
  · -- inner = PtrToStruct mid off sd
    rcases hargV with ⟨hid2B, hmidB, hpov, hsd⟩
    rcases hargRel with ⟨hact, _, _, o, hconc, hv1, hv2⟩
    have hmem1 : renameMemId ρ id2 ∈ wrld₁.liveResources :=
      (hacc.live_iff _).mpr ⟨id2, hid2_in, hact, rfl⟩
    have hmem2 : renameMemId ρ id2 ∈ wrld₂.liveResources := by
      rw [← hacc.wrld_eq]; exact hmem1
    have hc1 : wrld₁.liveResources.contains (renameMemId ρ id2) = true :=
      List.contains_iff_mem.mpr hmem1
    have hc2 : wrld₂.liveResources.contains (renameMemId ρ id2) = true :=
      List.contains_iff_mem.mpr hmem2
    subst hv1; subst hv2
    refine ⟨hid2B, ⟨hmidB, hpov, hsd⟩, (fun _ hc => by cases hc), hmem1, hc1, hc2,
      by simp [checkAlive, RuntimeVal.resourceId?, hmem1],
      by simp [checkAlive, RuntimeVal.resourceId?, hmem2],
      rfl, rfl, ⟨hpov, hsd, o, hconc, rfl, rfl⟩, ?_⟩
    intro l regs preStk hIdx aIdx ps stk hps
    by_cases ho0 : o = 0#64
    · subst ho0
      (cases ps <;> try cases hps) <;>
        exact ⟨by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?,
                    RuntimeVal.stripResource, RuntimeVal.isScalar, hmem1],
               by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?,
                    RuntimeVal.stripResource, RuntimeVal.isScalar, hmem2]⟩
    · (cases ps <;> try cases hps) <;>
        exact ⟨by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?,
                    RuntimeVal.stripResource, RuntimeVal.isScalar, hmem1, ho0],
               by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?,
                    RuntimeVal.stripResource, RuntimeVal.isScalar, hmem2, ho0]⟩

private theorem mem_param_non_rel_rel2
    {env : ExecEnv} {vCounter numArgs helperIdx : Nat}
    {l₁ l₂ : HostLayout}
    {ctx next next' : verifier.ComputationContext}
    {dPre dPost : verifier.DataDependencies}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {st₁ st₂ : MachineState} {stk₁ stk₂ : StackMem} {wrld₁ wrld₂ : WorldState}
    {mps : verifier.MemoryParameterSize} {sizeSpec : HelperMemSize}
    {inp out : Bool} {index : Std.Usize}
    {argVal : verifier.Type} {v₁ v₂ : RuntimeVal}
    (hctx : CompCtxValid env vCounter ctx)
    (hrel : StateRel2Deps env ctx dPre ρ resActive isNullId st₁ st₂)
    (hacc : HelperCallAccRel env vCounter ctx dPost ρ resActive isNullId
      st₁ st₂ next stk₁ stk₂ wrld₁ wrld₂)
    (hmps : (∃ v, mps = .Value v ∧ sizeSpec = .fixed v.bv) ∨
            (∃ idx, mps = .Reference idx ∧ sizeSpec = .fromArg idx.val ∧ idx.val < numArgs))
    (hargs_live : ∀ j < numArgs, DataDepsHasReg dPre (j + 1) = true)
    (hstk_cov : ∀ (offset : verifier.StackOffset) (input_size : Std.U64),
      argVal = .PtrToStack offset →
      verifier.Type.stack_input_size (.MemoryParameter mps inp out) ctx =
        ok (.Ok (some input_size)) →
      input_size.bv ≠ 0#64 →
      ∀ slot < 64,
        (∃ b < input_size.bv.toNat, (offset.value.bv.toNat + b) / 8 = slot) →
        DataDepsHasStack dPre slot = true)
    (hnot_rel : ∀ id0 in0, argVal ≠ .Releasable id0 in0)
    (hargV : TypeValid env vCounter argVal)
    (hargRel : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen argVal v₁ v₂)
    (hm : verifier.Type.match_parameter_type argVal vctx ctx
      (.MemoryParameter mps inp out) index next = ok (.Ok (), next')) :
    ∃ stk₁' stk₂' wrld₁' wrld₂',
      matchOneHelperParam env l₁ st₁.regs st₁.stack helperIdx index.val
        (.memory sizeSpec inp out) v₁ stk₁ wrld₁ = .ok (stk₁', wrld₁') ∧
      matchOneHelperParam env l₂ st₂.regs st₂.stack helperIdx index.val
        (.memory sizeSpec inp out) v₂ stk₂ wrld₂ = .ok (stk₂', wrld₂') ∧
      HelperCallAccRel env vCounter ctx dPost ρ resActive isNullId
        st₁ st₂ next' stk₁' stk₂' wrld₁' wrld₂' := by
  have hmps_live :
      (∃ v, mps = .Value v ∧ sizeSpec = .fixed v.bv) ∨
      (∃ idx, mps = .Reference idx ∧ sizeSpec = .fromArg idx.val ∧
        DataDepsHasReg dPre (idx.val + 1) = true) := by
    rcases hmps with ⟨v, rfl, rfl⟩ | ⟨idx, rfl, rfl, hlt⟩
    · exact Or.inl ⟨v, rfl, rfl⟩
    · exact Or.inr ⟨idx, rfl, rfl, hargs_live idx.val hlt⟩
  rw [verifier.Type.match_parameter_type.eq_def] at hm
  cases argVal <;> try (cases (Prod.mk.inj (ok_inj hm)).1)
  · -- PtrToStack offset
    rename_i offset
    dsimp only at hm
    rcases bind_eq_ok hm with ⟨r, hsz, hm⟩
    cases r with
    | Err _ => cases (Prod.mk.inj (ok_inj hm)).1
    | Ok s =>
      obtain ⟨hrsz1, hrsz2⟩ := MemoryParameterSize_size_ok_spec hmps_live hctx hrel hsz
      simp only [lift_eq_ok, bind_ok] at hm
      rw [api_BPF_STACK_SIZE_eq, cast_512_usize_eq_u64] at hm
      by_cases hgt : s > 512#u64
      · simp only [hgt, ↓reduceIte] at hm; cases (Prod.mk.inj (ok_inj hm)).1
      · simp only [hgt, ↓reduceIte] at hm
        have hble : s.bv ≤ 512#64 := by
          have hn : ¬ (512#64 < s.bv) := fun hc => hgt ((u64_gt_iff_bv s 512#u64).mpr hc)
          rw [BitVec.lt_def] at hn; rw [BitVec.le_def]; omega
        obtain ⟨rchk, hrchk, hchk_iff⟩ := StackOffset_check_valid_offset_spec offset
        rw [bind_ok_eq hrchk] at hm
        cases rchk with
        | Err _ => cases (Prod.mk.inj (ok_inj hm)).1
        | Ok u0 =>
          dsimp only at hm
          obtain ⟨hoff_kn, hoff_lt⟩ := (hchk_iff u0).mp rfl
          have hend_eq : verifier.StackOffset.add_u64 offset s =
              ok (knownScalar64 ⟨offset.value.bv + s.bv⟩) := by
            unfold verifier.StackOffset.add_u64 U64ScalarValueData.add_u64
              U64ScalarValueData.add_scalar U64ScalarValueData.is_known lift
            rw [bind_ok_eq (from_known_64_eq s)]
            have hkn_b : (knownScalar64 s).unknown_mask = 0#u64 := rfl
            simp only [hoff_kn, hkn_b, decide_true, ↓reduceIte, bind_ok,
              u64_wrapping_add_eq, from_known_64_eq]
            rfl
          rw [bind_ok_eq hend_eq, bind_ok_eq (StackOffset_is_within_stack_spec _)] at hm
          have h1_512 : offset.value.bv.toNat < 512 := by rw [BitVec.lt_def] at hoff_lt; simpa using hoff_lt
          have h2_512 : s.bv.toNat ≤ 512 := by rw [BitVec.le_def] at hble; simpa using hble
          have hno_ovf : ¬ BitVec.uaddOverflow offset.value.bv s.bv := by
            simp only [BitVec.uaddOverflow, decide_eq_true_eq]; omega
          have hkn_end : (knownScalar64 ⟨offset.value.bv + s.bv⟩).unknown_mask = 0#u64 := rfl
          have hval_end : (knownScalar64 ⟨offset.value.bv + s.bv⟩).value.bv = offset.value.bv + s.bv := rfl
          by_cases hwithin : offset.value.bv + s.bv ≤ 512#64
          · have hw_dec : decide ((knownScalar64 ⟨offset.value.bv + s.bv⟩).unknown_mask = 0#u64 ∧
                (knownScalar64 ⟨offset.value.bv + s.bv⟩).value.bv ≤ 512#64) = true :=
              decide_eq_true ⟨hkn_end, hwithin⟩
            simp only [hw_dec, ↓reduceIte] at hm
            rcases hargRel with ⟨_, startOff, hconc_off, hv1, hv2⟩
            have hstart_eq : startOff = offset.value.bv := eq_value_of_known_64 hoff_kn hconc_off.1
            subst hv1; subst hv2; subst hstart_eq
            have hn_gt : ¬ (s.bv > 512#64) := BitVec.not_lt.mpr hble
            have hn_ge : ¬ (offset.value.bv ≥ 512#64) := BitVec.not_le.mpr hoff_lt
            have hn_ovf : BitVec.uaddOverflow offset.value.bv s.bv = false := Bool.eq_false_iff.mpr hno_ovf
            have hn_sum_gt : ¬ ((offset.value.bv + s.bv) > 512#64) := BitVec.not_lt.mpr hwithin
            -- Helper for input readability when `inp = true`:
            have hinp_ok :
                ∀ (hrd : verifier.Stack.read_data_ptr ctx.stack offset s = ok (.Ok ())),
                  inp = true →
                  checkStackRangeReadable st₁.stack offset.value.bv s.bv = true ∧
                  checkStackRangeReadable st₂.stack offset.value.bv s.bv = true := by
              intro hrd hinp_t
              subst hinp_t
              have hsis := Type_stack_input_size_memory_input (out := out) hsz
              exact Stack_read_data_ptr_rel2 hctx.2.1 hconc_off hrd (fun hnz slot hsl hex =>
                hrel.2.2.2.2.2.2.2.2.2.2.2 slot hsl (hstk_cov offset s rfl hsis hnz slot hsl hex))
            cases out with
            | false =>
              simp only [Bool.false_eq_true, ↓reduceIte] at hm
              cases inp with
              | false =>
                simp only [Bool.false_eq_true, ↓reduceIte] at hm
                rcases Prod.mk.inj (ok_inj hm) with ⟨_, rfl⟩
                refine ⟨stk₁, stk₂, wrld₁, wrld₂, ?_, ?_, hacc⟩
                · simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hrsz1,
                    hn_gt, hn_ge, hn_ovf, hn_sum_gt]
                · simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hrsz2,
                    hn_gt, hn_ge, hn_ovf, hn_sum_gt]
              | true =>
                simp only [↓reduceIte] at hm
                rcases bind_eq_ok hm with ⟨r2, hrd, hm⟩
                cases r2 <;> simp at hm
                subst hm
                obtain ⟨hrd1, hrd2⟩ := hinp_ok hrd rfl
                refine ⟨stk₁, stk₂, wrld₁, wrld₂, ?_, ?_, hacc⟩
                · simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hrsz1,
                    hn_gt, hn_ge, hn_ovf, hn_sum_gt, hrd1]
                · simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hrsz2,
                    hn_gt, hn_ge, hn_ovf, hn_sum_gt, hrd2]
            | true =>
              simp only [↓reduceIte] at hm
              rcases bind_eq_ok hm with ⟨⟨r2, s1⟩, hwr, hm⟩
              cases r2 with
              | Err _ => cases (Prod.mk.inj (ok_inj hm)).1
              | Ok _ =>
                obtain ⟨s_w, stk₁', stk₂', hwr_eq, hw1, hw2, hs1_valid, hstk_rel'⟩ :=
                  Stack_write_data_ptr_rel2 (deps := dPost) (layout₁ := l₁) (layout₂ := l₂)
                    (env.helperBufWriteOracle wrld₁.stepCount index.val)
                    hacc.stk_valid hconc_off hoff_kn hoff_lt hble hno_ovf hwithin hacc.stk_rel2
                rw [hwr] at hwr_eq
                rcases Prod.mk.inj (ok_inj hwr_eq) with ⟨_, rfl⟩
                have hw1' : writeStackRangeHelper l₁ wrld₁.arrayLen stk₁ offset.value.bv s.bv
                    (env.helperBufWriteOracle wrld₁.stepCount index.val) = some stk₁' := by
                  rwa [hacc.arrayLen_eq]
                have hw2' : writeStackRangeHelper l₂ wrld₂.arrayLen stk₂ offset.value.bv s.bv
                    (env.helperBufWriteOracle wrld₂.stepCount index.val) = some stk₂' := by
                  rw [← hacc.wrld_eq, hacc.arrayLen_eq]; exact hw2
                cases inp with
                | false =>
                  simp only [Bool.false_eq_true, ↓reduceIte] at hm
                  rcases Prod.mk.inj (ok_inj hm) with ⟨_, rfl⟩
                  refine ⟨stk₁', stk₂', wrld₁, wrld₂, ?_, ?_,
                    { hacc with stk_valid := hs1_valid, stk_rel2 := hstk_rel' }⟩
                  · simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hrsz1,
                      hn_gt, hn_ge, hn_ovf, hn_sum_gt, hw1']
                  · simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hrsz2,
                      hn_gt, hn_ge, hn_ovf, hn_sum_gt, hw2']
                | true =>
                  simp only [↓reduceIte] at hm
                  rcases bind_eq_ok hm with ⟨r3, hrd, hm⟩
                  cases r3 <;> simp at hm
                  subst hm
                  obtain ⟨hrd1, hrd2⟩ := hinp_ok hrd rfl
                  refine ⟨stk₁', stk₂', wrld₁, wrld₂, ?_, ?_,
                    { hacc with stk_valid := hs1_valid, stk_rel2 := hstk_rel' }⟩
                  · simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hrsz1,
                      hn_gt, hn_ge, hn_ovf, hn_sum_gt, hrd1, hw1']
                  · simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hrsz2,
                      hn_gt, hn_ge, hn_ovf, hn_sum_gt, hrd2, hw2']
          · have hw_dec : decide ((knownScalar64 ⟨offset.value.bv + s.bv⟩).unknown_mask = 0#u64 ∧
                (knownScalar64 ⟨offset.value.bv + s.bv⟩).value.bv ≤ 512#64) = false :=
              decide_eq_false (fun h => hwithin h.2)
            simp only [hw_dec, Bool.false_eq_true, ↓reduceIte] at hm
            cases (Prod.mk.inj (ok_inj hm)).1
  · -- PtrToMemory mid offset buf orig
    rename_i mid offset buf orig
    obtain ⟨rfl, hw_orig, s, hsz, hmax_le, hmax_ne, hs_le, hs_ne⟩ :=
      mem_param_ptr_to_mem_ok_spec hm
    obtain ⟨hrsz1, hrsz2⟩ := MemoryParameterSize_size_ok_spec hmps_live hctx hrel hsz
    rcases hargRel with ⟨_, o, hconc_o, hv1, hv2⟩
    subst hv1; subst hv2
    have hin_span : inBoundsSpan o s.bv buf.bv = true :=
      inBoundsSpan_of_max_sub hconc_o.1.2.2.2 hmax_le hmax_ne hs_le hs_ne
    cases out with
    | false =>
      refine ⟨stk₁, stk₂, wrld₁, wrld₂, ?_, ?_, hacc⟩
      · simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hrsz1, hin_span]
      · simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hrsz2, hin_span]
    | true =>
      have hwo := hw_orig rfl
      let buf' := writeBufSpanHelper wrld₁.bufBytes (renameMemId ρ mid) o s.bv
        (env.helperBufWriteOracle wrld₁.stepCount index.val)
      refine ⟨stk₁, stk₂, { wrld₁ with bufBytes := buf' }, { wrld₂ with bufBytes := buf' },
        ?_, ?_, ?_⟩
      · simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hrsz1, hwo, hin_span, buf']
      · rw [← hacc.wrld_eq]
        simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hrsz2, hwo, hin_span, buf']
      · exact { hacc with
          wrld_eq := by rw [hacc.wrld_eq]
          structBytes_eq := hacc.structBytes_eq
          structFieldNull_eq := hacc.structFieldNull_eq
          arrayLen_eq := hacc.arrayLen_eq
          stepCount_eq := hacc.stepCount_eq
          nextAllocId_eq := hacc.nextAllocId_eq
          live_nodup := hacc.live_nodup
          live_iff := hacc.live_iff }
  · -- Releasable -> excluded by `hnot_rel`
    rename_i id0 in0
    exact False.elim (hnot_rel id0 in0 rfl)

/-- Complete 2-safety soundness of `Type.match_parameter_type` against `Spec.matchOneHelperParam`
    across all 13 `ParamTypeMatches` constructors. -/
theorem Type_match_parameter_type_rel2
    {env : ExecEnv} {vCounter numArgs helperIdx : Nat}
    {l₁ l₂ : HostLayout}
    {vctx : verifier.VerificationContext}
    {ctx next next' : verifier.ComputationContext}
    {dPre dPost : verifier.DataDependencies}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {st₁ st₂ : MachineState} {stk₁ stk₂ : StackMem} {wrld₁ wrld₂ : WorldState}
    {argVal paramTy : verifier.Type} {paramSpec : HelperParamSpec}
    {index : Std.Usize} {v₁ v₂ : RuntimeVal}
    (hcctx : CallingContextFullMatches vctx.calling_context env)
    (hctx : CompCtxValid env vCounter ctx)
    (hρ : RhoValid vCounter st₁.world.nextAllocId ρ)
    (hrel : StateRel2Deps env ctx dPre ρ resActive isNullId st₁ st₂)
    (hargs_live : ∀ j < numArgs, DataDepsHasReg dPre (j + 1) = true)
    (hparam : ParamTypeMatches env paramTy paramSpec)
    (hidx_valid : HelperParamIndexValid numArgs paramSpec)
    (hstk_cov : ∀ (offset : verifier.StackOffset) (input_size : Std.U64),
      argVal = .PtrToStack offset →
      verifier.Type.stack_input_size paramTy ctx = ok (.Ok (some input_size)) →
      input_size.bv ≠ 0#64 →
      ∀ slot < 64,
        (∃ b < input_size.bv.toNat, (offset.value.bv.toNat + b) / 8 = slot) →
        DataDepsHasStack dPre slot = true)
    (hacc : HelperCallAccRel env vCounter ctx dPost ρ resActive isNullId
      st₁ st₂ next stk₁ stk₂ wrld₁ wrld₂)
    (hargV : TypeValid env vCounter argVal)
    (hargRel : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen argVal v₁ v₂)
    (hm : verifier.Type.match_parameter_type argVal vctx ctx paramTy index next =
      ok (.Ok (), next')) :
    ∃ stk₁' stk₂' wrld₁' wrld₂',
      matchOneHelperParam env l₁ st₁.regs st₁.stack helperIdx index.val
        paramSpec v₁ stk₁ wrld₁ = .ok (stk₁', wrld₁') ∧
      matchOneHelperParam env l₂ st₂.regs st₂.stack helperIdx index.val
        paramSpec v₂ stk₂ wrld₂ = .ok (stk₂', wrld₂') ∧
      HelperCallAccRel env vCounter ctx dPost ρ resActive isNullId
        st₁ st₂ next' stk₁' stk₂' wrld₁' wrld₂' := by
  induction hparam generalizing argVal v₁ v₂ next next' stk₁ stk₂ wrld₁ wrld₂ with
  | scalar =>
    rw [verifier.Type.match_parameter_type.eq_def] at hm
    cases argVal <;> try (cases (Prod.mk.inj (ok_inj hm)).1)
    · rename_i data
      dsimp only at hm
      rcases bind_eq_ok hm with ⟨b, hfi, hm⟩
      cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · cases (Prod.mk.inj (ok_inj hm)).1
      · rcases Prod.mk.inj (ok_inj hm) with ⟨_, rfl⟩
        have hunw : data.unwritten_mask = 0#u64 := by
          unfold U64ScalarValueData.is_fully_initialized at hfi
          exact of_decide_eq_true (ok_inj hfi)
        have hne : data ≠ uninitScalar64 := fun hc => by
          subst hc; exact uninitScalar64_unwritten_ne_zero hunw
        simp only [TypeRel2, hne, ↓reduceIte] at hargRel
        rcases hargRel with ⟨x₁, x₂, rfl, rfl, _⟩
        exact ⟨stk₁, stk₂, wrld₁, wrld₂,
          by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?],
          by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?], hacc⟩
    · rename_i id2 inner
      rcases bind_eq_ok hm with ⟨b, _, hm⟩
      cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · cases (Prod.mk.inj (ok_inj hm)).1
      · cases inner <;> try exact False.elim hargV
        · rw [verifier.Type.match_parameter_type.eq_def] at hm; cases (Prod.mk.inj (ok_inj hm)).1
        · rw [verifier.Type.match_parameter_type.eq_def] at hm; cases (Prod.mk.inj (ok_inj hm)).1
  | constMapAllow ts w =>
    rw [verifier.Type.match_parameter_type.eq_def] at hm
    cases argVal <;> try (cases (Prod.mk.inj (ok_inj hm)).1)
    · rename_i mapIdx schema
      rcases hargRel with ⟨hmap_env, rfl, rfl⟩
      rw [UScalar.bv_toNat] at hmap_env
      cases w with
      | false =>
        simp only [Bool.false_eq_true, ↓reduceIte] at hm
        rw [bind_ok_eq (MapTypeFilter_is_allowed_allow_spec ts schema.map_type)] at hm
        cases hal : (HelperMapFilter.allowList (ts.val.map (·.bv))).allows schema.map_type.bv <;>
          simp only [hal, Bool.false_eq_true, ↓reduceIte] at hm
        · cases (Prod.mk.inj (ok_inj hm)).1
        · rcases Prod.mk.inj (ok_inj hm) with ⟨_, rfl⟩
          exact ⟨stk₁, stk₂, wrld₁, wrld₂,
            by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hmap_env, MapSpec.ofCore, hal],
            by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hmap_env, MapSpec.ofCore, hal],
            hacc⟩
      | true =>
        simp only [↓reduceIte] at hm
        obtain ⟨mf, b_ro, hmf, hcont_mf, hbro_eq⟩ := MapFlags_ProgReadOnly_contains_spec schema
        rw [bind_ok_eq hmf, bind_ok_eq hcont_mf] at hm
        cases b_ro <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
        · rw [bind_ok_eq (MapTypeFilter_is_allowed_allow_spec ts schema.map_type)] at hm
          cases hal : (HelperMapFilter.allowList (ts.val.map (·.bv))).allows schema.map_type.bv <;>
            simp only [hal, Bool.false_eq_true, ↓reduceIte] at hm
          · cases (Prod.mk.inj (ok_inj hm)).1
          · rcases Prod.mk.inj (ok_inj hm) with ⟨_, rfl⟩
            have hro_f : (MapSpec.ofCore schema).progReadOnly = false := hbro_eq.symm
            exact ⟨stk₁, stk₂, wrld₁, wrld₂,
              by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hmap_env, hro_f,
                  show (MapSpec.ofCore schema).mapType = schema.map_type.bv from rfl, hal],
              by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hmap_env, hro_f,
                  show (MapSpec.ofCore schema).mapType = schema.map_type.bv from rfl, hal],
              hacc⟩
        · cases (Prod.mk.inj (ok_inj hm)).1
    · rename_i id2 inner
      rcases bind_eq_ok hm with ⟨b, _, hm⟩
      cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · cases (Prod.mk.inj (ok_inj hm)).1
      · cases inner <;> try exact False.elim hargV
        · rw [verifier.Type.match_parameter_type.eq_def] at hm; cases (Prod.mk.inj (ok_inj hm)).1
        · rw [verifier.Type.match_parameter_type.eq_def] at hm; cases (Prod.mk.inj (ok_inj hm)).1
  | constMapDeny ts w =>
    rw [verifier.Type.match_parameter_type.eq_def] at hm
    cases argVal <;> try (cases (Prod.mk.inj (ok_inj hm)).1)
    · rename_i mapIdx schema
      rcases hargRel with ⟨hmap_env, rfl, rfl⟩
      rw [UScalar.bv_toNat] at hmap_env
      cases w with
      | false =>
        simp only [Bool.false_eq_true, ↓reduceIte] at hm
        rw [bind_ok_eq (MapTypeFilter_is_allowed_deny_spec ts schema.map_type)] at hm
        cases hal : (HelperMapFilter.denyList (ts.val.map (·.bv))).allows schema.map_type.bv <;>
          simp only [hal, Bool.false_eq_true, ↓reduceIte] at hm
        · cases (Prod.mk.inj (ok_inj hm)).1
        · rcases Prod.mk.inj (ok_inj hm) with ⟨_, rfl⟩
          exact ⟨stk₁, stk₂, wrld₁, wrld₂,
            by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hmap_env, MapSpec.ofCore, hal],
            by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hmap_env, MapSpec.ofCore, hal],
            hacc⟩
      | true =>
        simp only [↓reduceIte] at hm
        obtain ⟨mf, b_ro, hmf, hcont_mf, hbro_eq⟩ := MapFlags_ProgReadOnly_contains_spec schema
        rw [bind_ok_eq hmf, bind_ok_eq hcont_mf] at hm
        cases b_ro <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
        · rw [bind_ok_eq (MapTypeFilter_is_allowed_deny_spec ts schema.map_type)] at hm
          cases hal : (HelperMapFilter.denyList (ts.val.map (·.bv))).allows schema.map_type.bv <;>
            simp only [hal, Bool.false_eq_true, ↓reduceIte] at hm
          · cases (Prod.mk.inj (ok_inj hm)).1
          · rcases Prod.mk.inj (ok_inj hm) with ⟨_, rfl⟩
            have hro_f : (MapSpec.ofCore schema).progReadOnly = false := hbro_eq.symm
            exact ⟨stk₁, stk₂, wrld₁, wrld₂,
              by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hmap_env, hro_f,
                  show (MapSpec.ofCore schema).mapType = schema.map_type.bv from rfl, hal],
              by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hmap_env, hro_f,
                  show (MapSpec.ofCore schema).mapType = schema.map_type.bv from rfl, hal],
              hacc⟩
        · cases (Prod.mk.inj (ok_inj hm)).1
    · rename_i id2 inner
      rcases bind_eq_ok hm with ⟨b, _, hm⟩
      cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · cases (Prod.mk.inj (ok_inj hm)).1
      · cases inner <;> try exact False.elim hargV
        · rw [verifier.Type.match_parameter_type.eq_def] at hm; cases (Prod.mk.inj (ok_inj hm)).1
        · rw [verifier.Type.match_parameter_type.eq_def] at hm; cases (Prod.mk.inj (ok_inj hm)).1
  | mapKey idx =>
    have hidx_live : DataDepsHasReg dPre (idx.val + 1) = true := hargs_live idx.val hidx_valid
    have hmem_case : ∀ (mid : MemoryId) (offset : U64ScalarValueData) (buf : Std.U64)
        (orig : verifier.PointerOrigin) (u₁ u₂ : RuntimeVal),
        TypeRel2 env ρ resActive isNullId st₁.world.arrayLen
          (.PtrToMemory mid offset buf orig) u₁ u₂ →
        verifier.Type.match_parameter_type (.PtrToMemory mid offset buf orig) vctx ctx
          (.MapKeyParameter idx) index next = ok (.Ok (), next') →
        next' = next ∧
        matchOneHelperParam env l₁ st₁.regs st₁.stack helperIdx index.val
          (.mapKey idx.val) u₁ stk₁ wrld₁ = .ok (stk₁, wrld₁) ∧
        matchOneHelperParam env l₂ st₂.regs st₂.stack helperIdx index.val
          (.mapKey idx.val) u₂ stk₂ wrld₂ = .ok (stk₂, wrld₂) := by
      intro mid offset buf orig u₁ u₂ hrel_u hm_u
      rw [verifier.Type.match_parameter_type.eq_def] at hm_u
      dsimp only at hm_u
      rcases bind_eq_ok hm_u with ⟨r, hget, hm_u⟩
      cases r with
      | Err _ => cases (Prod.mk.inj (ok_inj hm_u)).1
      | Ok s =>
        simp only [lift_eq_ok, bind_ok] at hm_u
        rcases bind_eq_ok hm_u with ⟨r1, hchk, hm_u⟩
        cases r1 <;> simp at hm_u
        subst hm_u
        obtain ⟨_, hlup1, hlup2⟩ :=
          ComputationContext_get_map_schema_ok_spec hctx hrel hidx_live hget
        rcases hrel_u with ⟨_, o, hconc_o, rfl, rfl⟩
        have hin := ComputationContext_check_memory_access_u32_span_ok_spec hconc_o.1 hchk
        exact ⟨rfl,
          by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hlup1, MapSpec.ofCore, hin],
          by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hlup2, MapSpec.ofCore, hin]⟩
    cases argVal with
    | PtrToStack offset =>
      rw [verifier.Type.match_parameter_type.eq_def] at hm
      dsimp only at hm
      rcases bind_eq_ok hm with ⟨r, hget, hm⟩
      cases r with
      | Err _ => cases (Prod.mk.inj (ok_inj hm)).1
      | Ok s =>
        simp only [lift_eq_ok, bind_ok] at hm
        rcases bind_eq_ok hm with ⟨r1, hrd, hm⟩
        cases r1 <;> simp at hm
        subst hm
        obtain ⟨_, hlup1, hlup2⟩ :=
          ComputationContext_get_map_schema_ok_spec hctx hrel hidx_live hget
        rcases hargRel with ⟨_, o, hconc_o, rfl, rfl⟩
        have hsis := Type_stack_input_size_mapKey hget
        have hcast_eq : (⟨s.key_size.bv.zeroExtend 64⟩ : Std.U64) = UScalar.cast .U64 s.key_size :=
          (u64_eq_iff_bv _ _).mpr (uscalar_cast_u64_of_u32_bv s.key_size).symm
        have hrd' : verifier.Stack.read_data_ptr ctx.stack offset ⟨s.key_size.bv.zeroExtend 64⟩ = ok (.Ok ()) := by
          rwa [hcast_eq]
        obtain ⟨hrd1, hrd2⟩ := Stack_read_data_ptr_rel2 hctx.2.1 hconc_o hrd'
          (fun hnz slot hsl hex => by
            have hstart_eq : o = offset.value.bv := by
              obtain ⟨r_p, hr_p, hp_p⟩ := Stack_read_data_ptr_spec hctx.2.1 offset ⟨s.key_size.bv.zeroExtend 64⟩
              rw [hrd'] at hr_p
              rcases hp_p () (ok_inj hr_p).symm with hb0 | ⟨_, hkn, _⟩
              · exact False.elim (hnz ((u64_eq_iff_bv _ 0#u64).mp hb0))
              · exact eq_value_of_known_64 hkn hconc_o.1
            subst hstart_eq
            have hnz' : (UScalar.cast .U64 s.key_size).bv ≠ 0#64 := by rwa [uscalar_cast_u64_of_u32_bv]
            have hex' : ∃ b < (UScalar.cast .U64 s.key_size).bv.toNat,
                (offset.value.bv.toNat + b) / 8 = slot := by rwa [uscalar_cast_u64_of_u32_bv]
            exact hrel.2.2.2.2.2.2.2.2.2.2.2 slot hsl
              (hstk_cov offset (UScalar.cast .U64 s.key_size) rfl hsis hnz' slot hsl hex'))
        exact ⟨stk₁, stk₂, wrld₁, wrld₂,
          by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hlup1, MapSpec.ofCore, hrd1],
          by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hlup2, MapSpec.ofCore, hrd2],
          hacc⟩
    | PtrToMemory mid offset buf orig =>
      obtain ⟨rfl, h1, h2⟩ := hmem_case mid offset buf orig v₁ v₂ hargRel hm
      exact ⟨stk₁, stk₂, wrld₁, wrld₂, h1, h2, hacc⟩
    | Releasable id2 inner =>
      rw [verifier.Type.match_parameter_type.eq_def] at hm
      dsimp only at hm
      rcases bind_eq_ok hm with ⟨b, hcont, hm⟩
      cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · cases (Prod.mk.inj (ok_inj hm)).1
      · obtain ⟨_, _, _, _, _, _, _, _, _, _, hstripRel, hstripEq⟩ :=
          releasable_arg_unwrap_spec hacc hargV hargRel hcont
        obtain ⟨heq1, heq2⟩ := hstripEq l₁ st₁.regs st₁.stack helperIdx index.val (.mapKey idx.val) stk₁ rfl
        have heq2' := (hstripEq l₂ st₂.regs st₂.stack helperIdx index.val (.mapKey idx.val) stk₂ rfl).2
        cases inner <;> try exact False.elim hargV
        · obtain ⟨rfl, h1, h2⟩ := hmem_case _ _ _ _ _ _ hstripRel hm
          exact ⟨stk₁, stk₂, wrld₁, wrld₂, heq1.trans h1, heq2'.trans h2, hacc⟩
        · rw [verifier.Type.match_parameter_type.eq_def] at hm; cases (Prod.mk.inj (ok_inj hm)).1
    | _ =>
      rw [verifier.Type.match_parameter_type.eq_def] at hm; cases (Prod.mk.inj (ok_inj hm)).1
  | mapValue idx =>
    have hidx_live : DataDepsHasReg dPre (idx.val + 1) = true := hargs_live idx.val hidx_valid
    have hmem_case : ∀ (mid : MemoryId) (offset : U64ScalarValueData) (buf : Std.U64)
        (orig : verifier.PointerOrigin) (u₁ u₂ : RuntimeVal),
        TypeRel2 env ρ resActive isNullId st₁.world.arrayLen
          (.PtrToMemory mid offset buf orig) u₁ u₂ →
        verifier.Type.match_parameter_type (.PtrToMemory mid offset buf orig) vctx ctx
          (.MapValueParameter idx) index next = ok (.Ok (), next') →
        next' = next ∧
        matchOneHelperParam env l₁ st₁.regs st₁.stack helperIdx index.val
          (.mapValue idx.val) u₁ stk₁ wrld₁ = .ok (stk₁, wrld₁) ∧
        matchOneHelperParam env l₂ st₂.regs st₂.stack helperIdx index.val
          (.mapValue idx.val) u₂ stk₂ wrld₂ = .ok (stk₂, wrld₂) := by
      intro mid offset buf orig u₁ u₂ hrel_u hm_u
      rw [verifier.Type.match_parameter_type.eq_def] at hm_u
      dsimp only at hm_u
      rcases bind_eq_ok hm_u with ⟨r, hget, hm_u⟩
      cases r with
      | Err _ => cases (Prod.mk.inj (ok_inj hm_u)).1
      | Ok s =>
        simp only [lift_eq_ok, bind_ok] at hm_u
        rcases bind_eq_ok hm_u with ⟨r1, hchk, hm_u⟩
        cases r1 <;> simp at hm_u
        subst hm_u
        obtain ⟨_, hlup1, hlup2⟩ :=
          ComputationContext_get_map_schema_ok_spec hctx hrel hidx_live hget
        rcases hrel_u with ⟨_, o, hconc_o, rfl, rfl⟩
        have hin := ComputationContext_check_memory_access_u32_span_ok_spec hconc_o.1 hchk
        exact ⟨rfl,
          by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hlup1, MapSpec.ofCore, hin],
          by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hlup2, MapSpec.ofCore, hin]⟩
    cases argVal with
    | PtrToStack offset =>
      rw [verifier.Type.match_parameter_type.eq_def] at hm
      dsimp only at hm
      rcases bind_eq_ok hm with ⟨r, hget, hm⟩
      cases r with
      | Err _ => cases (Prod.mk.inj (ok_inj hm)).1
      | Ok s =>
        simp only [lift_eq_ok, bind_ok] at hm
        rcases bind_eq_ok hm with ⟨r1, hrd, hm⟩
        cases r1 <;> simp at hm
        subst hm
        obtain ⟨_, hlup1, hlup2⟩ :=
          ComputationContext_get_map_schema_ok_spec hctx hrel hidx_live hget
        rcases hargRel with ⟨_, o, hconc_o, rfl, rfl⟩
        have hsis := Type_stack_input_size_mapValue hget
        have hcast_eq : (⟨s.value_size.bv.zeroExtend 64⟩ : Std.U64) = UScalar.cast .U64 s.value_size :=
          (u64_eq_iff_bv _ _).mpr (uscalar_cast_u64_of_u32_bv s.value_size).symm
        have hrd' : verifier.Stack.read_data_ptr ctx.stack offset ⟨s.value_size.bv.zeroExtend 64⟩ = ok (.Ok ()) := by
          rwa [hcast_eq]
        obtain ⟨hrd1, hrd2⟩ := Stack_read_data_ptr_rel2 hctx.2.1 hconc_o hrd'
          (fun hnz slot hsl hex => by
            have hstart_eq : o = offset.value.bv := by
              obtain ⟨r_p, hr_p, hp_p⟩ := Stack_read_data_ptr_spec hctx.2.1 offset ⟨s.value_size.bv.zeroExtend 64⟩
              rw [hrd'] at hr_p
              rcases hp_p () (ok_inj hr_p).symm with hb0 | ⟨_, hkn, _⟩
              · exact False.elim (hnz ((u64_eq_iff_bv _ 0#u64).mp hb0))
              · exact eq_value_of_known_64 hkn hconc_o.1
            subst hstart_eq
            have hnz' : (UScalar.cast .U64 s.value_size).bv ≠ 0#64 := by rwa [uscalar_cast_u64_of_u32_bv]
            have hex' : ∃ b < (UScalar.cast .U64 s.value_size).bv.toNat,
                (offset.value.bv.toNat + b) / 8 = slot := by rwa [uscalar_cast_u64_of_u32_bv]
            exact hrel.2.2.2.2.2.2.2.2.2.2.2 slot hsl
              (hstk_cov offset (UScalar.cast .U64 s.value_size) rfl hsis hnz' slot hsl hex'))
        exact ⟨stk₁, stk₂, wrld₁, wrld₂,
          by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hlup1, MapSpec.ofCore, hrd1],
          by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hlup2, MapSpec.ofCore, hrd2],
          hacc⟩
    | PtrToMemory mid offset buf orig =>
      obtain ⟨rfl, h1, h2⟩ := hmem_case mid offset buf orig v₁ v₂ hargRel hm
      exact ⟨stk₁, stk₂, wrld₁, wrld₂, h1, h2, hacc⟩
    | Releasable id2 inner =>
      rw [verifier.Type.match_parameter_type.eq_def] at hm
      dsimp only at hm
      rcases bind_eq_ok hm with ⟨b, hcont, hm⟩
      cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · cases (Prod.mk.inj (ok_inj hm)).1
      · obtain ⟨_, _, _, _, _, _, _, _, _, _, hstripRel, hstripEq⟩ :=
          releasable_arg_unwrap_spec hacc hargV hargRel hcont
        obtain ⟨heq1, _⟩ := hstripEq l₁ st₁.regs st₁.stack helperIdx index.val (.mapValue idx.val) stk₁ rfl
        have heq2' := (hstripEq l₂ st₂.regs st₂.stack helperIdx index.val (.mapValue idx.val) stk₂ rfl).2
        cases inner <;> try exact False.elim hargV
        · obtain ⟨rfl, h1, h2⟩ := hmem_case _ _ _ _ _ _ hstripRel hm
          exact ⟨stk₁, stk₂, wrld₁, wrld₂, heq1.trans h1, heq2'.trans h2, hacc⟩
        · rw [verifier.Type.match_parameter_type.eq_def] at hm; cases (Prod.mk.inj (ok_inj hm)).1
    | _ =>
      rw [verifier.Type.match_parameter_type.eq_def] at hm; cases (Prod.mk.inj (ok_inj hm)).1
  | memFixed sz inp out =>
    cases argVal with
    | Releasable id2 inner =>
      rw [verifier.Type.match_parameter_type.eq_def] at hm
      dsimp only at hm
      rcases bind_eq_ok hm with ⟨b, hcont, hm⟩
      cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · cases (Prod.mk.inj (ok_inj hm)).1
      · obtain ⟨_, hinnerV, hnot_stk, _, _, _, _, _, _, _, hstripRel, hstripEq⟩ :=
          releasable_arg_unwrap_spec hacc hargV hargRel hcont
        obtain ⟨heq1, _⟩ :=
          hstripEq l₁ st₁.regs st₁.stack helperIdx index.val (.memory (.fixed sz.bv) inp out) stk₁ rfl
        have heq2' :=
          (hstripEq l₂ st₂.regs st₂.stack helperIdx index.val (.memory (.fixed sz.bv) inp out) stk₂ rfl).2
        have hnot_rel_in : ∀ id0 in0, inner ≠ .Releasable id0 in0 := by
          (cases inner <;> try exact False.elim hargV) <;> exact fun _ _ hc => by cases hc
        obtain ⟨s1, s2, w1, w2, h1, h2, hacc'⟩ :=
          mem_param_non_rel_rel2 hctx hrel hacc (Or.inl ⟨sz, rfl, rfl⟩) hargs_live
            (fun off _ hc => False.elim (hnot_stk off hc)) hnot_rel_in hinnerV hstripRel hm
        exact ⟨s1, s2, w1, w2, heq1.trans h1, heq2'.trans h2, hacc'⟩
    | _ =>
      exact mem_param_non_rel_rel2 hctx hrel hacc (Or.inl ⟨sz, rfl, rfl⟩) hargs_live
        hstk_cov (fun _ _ hc => by cases hc) hargV hargRel hm
  | memRef idx inp out =>
    cases argVal with
    | Releasable id2 inner =>
      rw [verifier.Type.match_parameter_type.eq_def] at hm
      dsimp only at hm
      rcases bind_eq_ok hm with ⟨b, hcont, hm⟩
      cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · cases (Prod.mk.inj (ok_inj hm)).1
      · obtain ⟨_, hinnerV, hnot_stk, _, _, _, _, _, _, _, hstripRel, hstripEq⟩ :=
          releasable_arg_unwrap_spec hacc hargV hargRel hcont
        obtain ⟨heq1, _⟩ :=
          hstripEq l₁ st₁.regs st₁.stack helperIdx index.val (.memory (.fromArg idx.val) inp out) stk₁ rfl
        have heq2' :=
          (hstripEq l₂ st₂.regs st₂.stack helperIdx index.val (.memory (.fromArg idx.val) inp out) stk₂ rfl).2
        have hnot_rel_in : ∀ id0 in0, inner ≠ .Releasable id0 in0 := by
          (cases inner <;> try exact False.elim hargV) <;> exact fun _ _ hc => by cases hc
        obtain ⟨s1, s2, w1, w2, h1, h2, hacc'⟩ :=
          mem_param_non_rel_rel2 hctx hrel hacc (Or.inr ⟨idx, rfl, rfl, hidx_valid⟩) hargs_live
            (fun off _ hc => False.elim (hnot_stk off hc)) hnot_rel_in hinnerV hstripRel hm
        exact ⟨s1, s2, w1, w2, heq1.trans h1, heq2'.trans h2, hacc'⟩
    | _ =>
      exact mem_param_non_rel_rel2 hctx hrel hacc (Or.inr ⟨idx, rfl, rfl, hidx_valid⟩) hargs_live
        hstk_cov (fun _ _ hc => by cases hc) hargV hargRel hm
  | structArg schemaId hid_nv =>
    have hbase : ∀ (av : verifier.Type) (u₁ u₂ : RuntimeVal),
        (∀ id0 in0, av ≠ .Releasable id0 in0) →
        TypeValid env vCounter av →
        TypeRel2 env ρ resActive isNullId st₁.world.arrayLen av u₁ u₂ →
        verifier.Type.match_parameter_type av vctx ctx (.StructParameter schemaId) index next =
          ok (.Ok (), next') →
        next' = next ∧
        matchOneHelperParam env l₁ st₁.regs st₁.stack helperIdx index.val
          (.structArg schemaId) u₁ stk₁ wrld₁ = .ok (stk₁, wrld₁) ∧
        matchOneHelperParam env l₂ st₂.regs st₂.stack helperIdx index.val
          (.structArg schemaId) u₂ stk₂ wrld₂ = .ok (stk₂, wrld₂) := by
      intro av u₁ u₂ hnr hav hru hmu
      rw [verifier.Type.match_parameter_type.eq_def] at hmu
      cases av <;> try (cases (Prod.mk.inj (ok_inj hmu)).1)
      · -- PtrToMemory id2 offset _ _
        rename_i id2 offset _ _
        dsimp only at hmu
        rcases bind_eq_ok hmu with ⟨b, hz, hmu⟩
        cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hmu
        · cases (Prod.mk.inj (ok_inj hmu)).1
        · rw [bind_ok_eq (MemoryId_matches_spec schemaId id2)] at hmu
          cases hm_id : memoryIdMatches schemaId id2 <;>
            simp only [hm_id, Bool.false_eq_true, ↓reduceIte] at hmu
          · cases (Prod.mk.inj (ok_inj hmu)).1
          · rcases Prod.mk.inj (ok_inj hmu) with ⟨_, rfl⟩
            rcases hru with ⟨_, o, hconc_o, rfl, rfl⟩
            have ho0 : o = 0#64 := is_zero_64_sound hz hconc_o.1
            subst ho0
            have hm_ren : memoryIdMatches schemaId (renameMemId ρ id2) = true := by
              rw [renameMemId_matches_noVerif ρ id2 hid_nv]; exact hm_id
            exact ⟨rfl,
              by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hm_ren],
              by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hm_ren]⟩
      · -- PtrToStruct id2 offset _
        rename_i id2 offset _
        dsimp only at hmu
        rcases bind_eq_ok hmu with ⟨b, hz, hmu⟩
        cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hmu
        · cases (Prod.mk.inj (ok_inj hmu)).1
        · rw [bind_ok_eq (MemoryId_matches_spec schemaId id2)] at hmu
          cases hm_id : memoryIdMatches schemaId id2 <;>
            simp only [hm_id, Bool.false_eq_true, ↓reduceIte] at hmu
          · cases (Prod.mk.inj (ok_inj hmu)).1
          · rcases Prod.mk.inj (ok_inj hmu) with ⟨_, rfl⟩
            rcases hru with ⟨_, _, o, hconc_o, rfl, rfl⟩
            have ho0 : o = 0#64 := is_zero_64_sound hz hconc_o.1
            subst ho0
            have hm_ren : memoryIdMatches schemaId (renameMemId ρ id2) = true := by
              rw [renameMemId_matches_noVerif ρ id2 hid_nv]; exact hm_id
            exact ⟨rfl,
              by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hm_ren],
              by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hm_ren]⟩
      · rename_i id0 in0; exact False.elim (hnr id0 in0 rfl)
    cases argVal with
    | Releasable id2 inner =>
      rw [verifier.Type.match_parameter_type.eq_def] at hm
      dsimp only at hm
      rcases bind_eq_ok hm with ⟨b, hcont, hm⟩
      cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · cases (Prod.mk.inj (ok_inj hm)).1
      · obtain ⟨_, hinnerV, _, _, _, _, _, _, _, _, hstripRel, hstripEq⟩ :=
          releasable_arg_unwrap_spec hacc hargV hargRel hcont
        obtain ⟨heq1, _⟩ := hstripEq l₁ st₁.regs st₁.stack helperIdx index.val (.structArg schemaId) stk₁ rfl
        have heq2' := (hstripEq l₂ st₂.regs st₂.stack helperIdx index.val (.structArg schemaId) stk₂ rfl).2
        have hnr : ∀ id0 in0, inner ≠ .Releasable id0 in0 := by
          (cases inner <;> try exact False.elim hargV) <;> exact fun _ _ hc => by cases hc
        obtain ⟨rfl, h1, h2⟩ := hbase inner _ _ hnr hinnerV hstripRel hm
        exact ⟨stk₁, stk₂, wrld₁, wrld₂, heq1.trans h1, heq2'.trans h2, hacc⟩
    | _ =>
      obtain ⟨rfl, h1, h2⟩ := hbase _ v₁ v₂ (fun _ _ hc => by cases hc) hargV hargRel hm
      exact ⟨stk₁, stk₂, wrld₁, wrld₂, h1, h2, hacc⟩
  | contextArg idx hisSome =>
    rw [verifier.Type.match_parameter_type.eq_def] at hm
    rcases bind_eq_ok hm with ⟨r, hchk_nr, hm⟩
    cases r with
    | Err _ => cases (Prod.mk.inj (ok_inj hm)).1
    | Ok u0 =>
      cases u0
      simp only [lift_eq_ok, bind_ok] at hm
      have hidx_val : (UScalar.cast .Usize idx).val = idx.val :=
        uscalar_cast_usize_of_u8_val idx
      by_cases hlt_c : UScalar.cast .Usize idx < alloc.vec.Vec.len vctx.calling_context.args
      · simp only [hlt_c, ↓reduceIte] at hm
        have hlt_args : idx.val < vctx.calling_context.args.val.length := by
          have h1 : (UScalar.cast .Usize idx).val < vctx.calling_context.args.val.length := hlt_c
          rwa [hidx_val] at h1
        rcases bind_eq_ok hm with ⟨t, ht_idx, hm⟩
        have ht_get := (vec_index_usize_ok ht_idx).2
        rw [hidx_val, List.getElem?_eq_getElem hlt_args] at ht_get
        have ht_eq_arg : vctx.calling_context.args.val[idx.val]'hlt_args = t :=
          Option.some.inj ht_get
        obtain ⟨b_eq, hb_eq, hb_iff⟩ := Type_eq_spec t argVal
        rw [bind_ok_eq hb_eq] at hm
        cases b_eq <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
        · cases (Prod.mk.inj (ok_inj hm)).1
        · rcases Prod.mk.inj (ok_inj hm) with ⟨_, rfl⟩
          have ht_self : t = argVal := hb_iff.mp rfl
          subst ht_self
          obtain ⟨vi₁, vi₂, hconf⟩ := hcctx.2 idx.val hlt_args
          rw [ht_eq_arg] at hconf
          have hne_uninit : t ≠ .ScalarValue uninitScalar64 := by
            intro hc; subst hc
            rw [hconf.1] at hisSome; simp at hisSome
          obtain ⟨hca1, hca2_w1⟩ := Type_check_not_released_checkAlive_none
            hacc.res_valid hacc.live_iff hargV hne_uninit hchk_nr hargRel
          have hca2 : checkAlive wrld₂.liveResources v₂ = none := by
            rwa [← hacc.wrld_eq]
          cases t with
          | ScalarValue _ =>
            rw [hconf.1] at hisSome; simp at hisSome
          | PtrToStruct sid off sd =>
            rcases hconf with ⟨hsd, hv, hu, _, _, _, hctx_opt, _, _⟩
            rcases hargRel with ⟨_, _, o, hconc_o, rfl, rfl⟩
            have ho_kn : off.unknown_mask = 0#u64 := (u64_eq_iff_bv _ _).mpr hu
            have ho0 : o = 0#64 := by rw [eq_value_of_known_64 ho_kn hconc_o.1, hv]
            subst ho0
            rw [renameMemId_noVerif_id ρ hsd.1] at hca1 hca2 ⊢
            exact ⟨stk₁, stk₂, wrld₁, wrld₂,
              by simp [matchOneHelperParam, hca1, hctx_opt, ContextObjSpec.matchesVal],
              by simp [matchOneHelperParam, hca2, hctx_opt, ContextObjSpec.matchesVal], hacc⟩
          | PtrToMemory mid off sz orig =>
            rcases hconf with ⟨hnv, hv, hu, _, _, _, hctx_opt, _, _⟩
            rcases hargRel with ⟨_, o, hconc_o, rfl, rfl⟩
            have ho_kn : off.unknown_mask = 0#u64 := (u64_eq_iff_bv _ _).mpr hu
            have ho0 : o = 0#64 := by rw [eq_value_of_known_64 ho_kn hconc_o.1, hv]
            subst ho0
            rw [renameMemId_noVerif_id ρ hnv] at hca1 hca2 ⊢
            exact ⟨stk₁, stk₂, wrld₁, wrld₂,
              by simp [matchOneHelperParam, hca1, hctx_opt, ContextObjSpec.matchesVal],
              by simp [matchOneHelperParam, hca2, hctx_opt, ContextObjSpec.matchesVal], hacc⟩
          | PtrToArray aid off =>
            rcases hconf with ⟨hnv, hv, hu, _, _, _, hctx_opt, _, _⟩
            rcases hargRel with ⟨_, _, o, hconc_o, rfl, rfl⟩
            have ho_kn : off.unknown_mask = 0#u64 := (u64_eq_iff_bv _ _).mpr hu
            have ho0 : o = 0#64 := by rw [eq_value_of_known_64 ho_kn hconc_o.1, hv]
            subst ho0
            rw [renameMemId_noVerif_id ρ hnv] at hca1 hca2 ⊢
            exact ⟨stk₁, stk₂, wrld₁, wrld₂,
              by simp [matchOneHelperParam, hca1, hctx_opt, ContextObjSpec.matchesVal],
              by simp [matchOneHelperParam, hca2, hctx_opt, ContextObjSpec.matchesVal], hacc⟩
          | PtrToEndArray aid =>
            rcases hconf with ⟨hnv, hctx_opt, _, _⟩
            rcases hargRel with ⟨_, rfl, rfl⟩
            rw [renameMemId_noVerif_id ρ hnv] at hca1 hca2 ⊢
            exact ⟨stk₁, stk₂, wrld₁, wrld₂,
              by simp [matchOneHelperParam, hca1, hctx_opt, ContextObjSpec.matchesVal],
              by simp [matchOneHelperParam, hca2, hctx_opt, ContextObjSpec.matchesVal], hacc⟩
          | ConstPtrToMap mapIdx schema =>
            rcases hconf with ⟨_, hctx_opt, _, _⟩
            rcases hargRel with ⟨_, rfl, rfl⟩
            exact ⟨stk₁, stk₂, wrld₁, wrld₂,
              by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hctx_opt, ContextObjSpec.matchesVal],
              by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, hctx_opt, ContextObjSpec.matchesVal], hacc⟩
          | _ => exact False.elim hconf
      · simp only [hlt_c, ↓reduceIte] at hm
        cases (Prod.mk.inj (ok_inj hm)).1
  | releasableArg schemaId innerT innerS hid_nv _ ih =>
    rw [verifier.Type.match_parameter_type.eq_def] at hm
    cases argVal <;> try (cases (Prod.mk.inj (ok_inj hm)).1)
    rename_i id2 inner2
    dsimp only at hm
    rw [bind_ok_eq (MemoryId_has_parent_spec id2 schemaId)] at hm
    cases hhp : memoryIdHasParent id2 schemaId <;>
      simp only [hhp, Bool.false_eq_true, ↓reduceIte] at hm
    · rcases bind_eq_ok hm with ⟨b1, _, hm⟩
      cases b1 <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · cases (Prod.mk.inj (ok_inj hm)).1
      · cases inner2 <;> try exact False.elim hargV
        · rw [verifier.Type.match_parameter_type.eq_def] at hm; cases (Prod.mk.inj (ok_inj hm)).1
        · rw [verifier.Type.match_parameter_type.eq_def] at hm; cases (Prod.mk.inj (ok_inj hm)).1
    · rcases bind_eq_ok hm with ⟨b1, hcont, hm⟩
      cases b1 <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · cases (Prod.mk.inj (ok_inj hm)).1
      · obtain ⟨hid2B, hinnerV, hnot_stk, hmem1, _, _, _, _, hrid1, hrid2, hstripRel, _⟩ :=
          releasable_arg_unwrap_spec hacc hargV hargRel hcont
        have hmem2 : renameMemId ρ id2 ∈ wrld₂.liveResources := hacc.wrld_eq ▸ hmem1
        have hhp_ren : memoryIdHasParent (renameMemId ρ id2) schemaId = true := by
          have hschB := memIdBounded_of_noVerif vCounter hid_nv
          have h := renameMemId_hasParent hρ hid2B hschB
          rwa [renameMemId_noVerif_id ρ hid_nv, hhp] at h
        obtain ⟨s1, s2, w1, w2, h1, h2, hacc'⟩ :=
          ih hidx_valid (fun off _ hc => False.elim (hnot_stk off hc))
            hacc hinnerV hstripRel hm
        refine ⟨s1, s2, w1, w2, ?_, ?_, hacc'⟩
        · simp [matchOneHelperParam, hrid1, hhp_ren, hmem1, h1]
        · simp [matchOneHelperParam, hrid2, hhp_ren, hmem2, h2]
  | releaseArg schemaId hid_nv =>
    rw [verifier.Type.match_parameter_type.eq_def] at hm
    cases argVal <;> try (cases (Prod.mk.inj (ok_inj hm)).1)
    rename_i id2 inner
    dsimp only at hm
    rw [bind_ok_eq (MemoryId_has_parent_spec id2 schemaId)] at hm
    cases hhp : memoryIdHasParent id2 schemaId <;>
      simp only [hhp, Bool.false_eq_true, ↓reduceIte] at hm
    · rcases bind_eq_ok hm with ⟨b1, _, hm⟩
      cases b1 <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · cases (Prod.mk.inj (ok_inj hm)).1
      · cases inner <;> try exact False.elim hargV
        · rw [verifier.Type.match_parameter_type.eq_def] at hm; cases (Prod.mk.inj (ok_inj hm)).1
        · rw [verifier.Type.match_parameter_type.eq_def] at hm; cases (Prod.mk.inj (ok_inj hm)).1
    · rcases bind_eq_ok hm with ⟨b1, hcont, hm⟩
      cases b1 <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · cases (Prod.mk.inj (ok_inj hm)).1
      · obtain ⟨hid2B, _, _, hmem1, _, _, _, _, _, _, _, _⟩ :=
          releasable_arg_unwrap_spec hacc hargV hargRel hcont
        have hmem2 : renameMemId ρ id2 ∈ wrld₂.liveResources := hacc.wrld_eq ▸ hmem1
        have hhp_ren : memoryIdHasParent (renameMemId ρ id2) schemaId = true := by
          have hschB := memIdBounded_of_noVerif vCounter hid_nv
          have h := renameMemId_hasParent hρ hid2B hschB
          rwa [renameMemId_noVerif_id ρ hid_nv, hhp] at h
        obtain ⟨_, is_new, hrem_eq, his_valid, hrem_mem, _⟩ :=
          IdSet_remove_spec hacc.res_valid id2
        obtain ⟨hnodup', hlive'⟩ := StateRel2Deps_remove_active_resource
          hρ hacc.res_valid hid2B hacc.live_nodup hacc.live_iff hrem_mem
        let rid := renameMemId ρ id2
        let w₁' := { wrld₁ with liveResources := wrld₁.liveResources.erase rid }
        let w₂' := { wrld₂ with liveResources := wrld₂.liveResources.erase rid }
        have hw'_eq : w₁' = w₂' := by dsimp only [w₁', w₂']; rw [hacc.wrld_eq]
        cases inner <;> try exact False.elim hargV
        · -- inner = PtrToMemory
          rcases bind_eq_ok hm with ⟨b2, hz, hm⟩
          cases b2 <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
          · cases (Prod.mk.inj (ok_inj hm)).1
          · rw [bind_pair_ok_eq hrem_eq] at hm
            rcases Prod.mk.inj (ok_inj hm) with ⟨_, rfl⟩
            rcases hargRel with ⟨_, _, o, hconc_o, rfl, rfl⟩
            have ho0 : o = 0#64 := is_zero_64_sound hz hconc_o.1
            subst ho0
            exact ⟨stk₁, stk₂, w₁', w₂',
              by simp [matchOneHelperParam, hhp_ren, hmem1, w₁', rid],
              by simp [matchOneHelperParam, hhp_ren, hmem2, w₂', rid],
              { hacc with
                res_valid := his_valid
                wrld_eq := hw'_eq
                live_nodup := hnodup'
                live_iff := hlive' }⟩
        · -- inner = PtrToStruct
          rcases bind_eq_ok hm with ⟨b2, hz, hm⟩
          cases b2 <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
          · cases (Prod.mk.inj (ok_inj hm)).1
          · rw [bind_pair_ok_eq hrem_eq] at hm
            rcases Prod.mk.inj (ok_inj hm) with ⟨_, rfl⟩
            rcases hargRel with ⟨_, _, _, o, hconc_o, rfl, rfl⟩
            have ho0 : o = 0#64 := is_zero_64_sound hz hconc_o.1
            subst ho0
            exact ⟨stk₁, stk₂, w₁', w₂',
              by simp [matchOneHelperParam, hhp_ren, hmem1, w₁', rid],
              by simp [matchOneHelperParam, hhp_ren, hmem2, w₂', rid],
              { hacc with
                res_valid := his_valid
                wrld_eq := hw'_eq
                live_nodup := hnodup'
                live_iff := hlive' }⟩
  | nullOr innerT innerS _ ih =>
    have hstk_cov' : ∀ (offset : verifier.StackOffset) (input_size : Std.U64),
        argVal = .PtrToStack offset →
        verifier.Type.stack_input_size innerT ctx = ok (.Ok (some input_size)) →
        input_size.bv ≠ 0#64 →
        ∀ slot < 64,
          (∃ b < input_size.bv.toNat, (offset.value.bv.toNat + b) / 8 = slot) →
          DataDepsHasStack dPre slot = true := by
      intro offset input_size hps hsis
      rw [← Type_stack_input_size_nullOr] at hsis
      exact hstk_cov offset input_size hps hsis
    rw [verifier.Type.match_parameter_type.eq_def] at hm
    cases argVal with
    | ScalarValue data =>
      dsimp only at hm
      rcases bind_eq_ok hm with ⟨b, hkn_op, hm⟩
      cases b with
      | false =>
        simp only [Bool.false_eq_true, ↓reduceIte] at hm
        obtain ⟨s1, s2, w1, w2, h1, h2, hacc'⟩ :=
          ih hidx_valid hstk_cov' hacc hargV hargRel hm
        exact ⟨s1, s2, w1, w2,
          matchOneHelperParam_nullOr_of_inner_ok h1,
          matchOneHelperParam_nullOr_of_inner_ok h2, hacc'⟩
      | true =>
        simp only [↓reduceIte] at hm
        by_cases hv0 : data.value = 0#u64
        · simp only [hv0, ↓reduceIte] at hm
          rcases Prod.mk.inj (ok_inj hm) with ⟨_, rfl⟩
          have hkn : data.unknown_mask = 0#u64 := by
            unfold U64ScalarValueData.is_known at hkn_op
            exact of_decide_eq_true (ok_inj hkn_op)
          have hne : data ≠ uninitScalar64 := fun hc => by
            subst hc
            have : core.num.U64.MAX.bv = (0#u64 : Std.U64).bv := congrArg UScalar.bv hkn
            rw [u64_max_bv] at this; exact absurd this (by decide)
          simp only [TypeRel2, hne, ↓reduceIte] at hargRel
          rcases hargRel with ⟨x₁, x₂, rfl, rfl, hc1, hc2, _⟩
          have hx1 : x₁ = 0#64 := by rw [eq_value_of_known_64 hkn hc1, hv0]; rfl
          have hx2 : x₂ = 0#64 := by rw [eq_value_of_known_64 hkn hc2, hv0]; rfl
          subst hx1; subst hx2
          exact ⟨stk₁, stk₂, wrld₁, wrld₂,
            by simp [matchOneHelperParam],
            by simp [matchOneHelperParam], hacc⟩
        · simp only [hv0, ↓reduceIte] at hm
          obtain ⟨s1, s2, w1, w2, h1, h2, hacc'⟩ :=
            ih hidx_valid hstk_cov' hacc hargV hargRel hm
          exact ⟨s1, s2, w1, w2,
            matchOneHelperParam_nullOr_of_inner_ok h1,
            matchOneHelperParam_nullOr_of_inner_ok h2, hacc'⟩
    | _ =>
      obtain ⟨s1, s2, w1, w2, h1, h2, hacc'⟩ :=
        ih hidx_valid hstk_cov' hacc hargV hargRel hm
      exact ⟨s1, s2, w1, w2,
        matchOneHelperParam_nullOr_of_inner_ok h1,
        matchOneHelperParam_nullOr_of_inner_ok h2, hacc'⟩
  | anyArg =>
    have hleaf : ∀ (av : verifier.Type) (u₁ u₂ : RuntimeVal),
        TypeValid env vCounter av →
        TypeRel2 env ρ resActive isNullId st₁.world.arrayLen av u₁ u₂ →
        (do
          let r ← verifier.Type.check_not_released av next.resources
          match r with
          | core.result.Result.Ok _ =>
            let allowed ←
              if vctx.calling_context.allow_ptr_leaks then verifier.Type.is_initialized av
              else verifier.Type.is_written_scalar av
            if allowed then ok (core.result.Result.Ok (), next)
            else ok (core.result.Result.Err (error.VerifierErrorKind.HelperArgumentTypeMismatch index), next)
          | core.result.Result.Err _ => ok (r, next)) = ok (.Ok (), next') →
        next' = next ∧
        matchOneHelperParam env l₁ st₁.regs st₁.stack helperIdx index.val
          .anyArg u₁ stk₁ wrld₁ = .ok (stk₁, wrld₁) ∧
        matchOneHelperParam env l₂ st₂.regs st₂.stack helperIdx index.val
          .anyArg u₂ stk₂ wrld₂ = .ok (stk₂, wrld₂) := by
      intro av u₁ u₂ hav hru hx
      rcases bind_eq_ok hx with ⟨r, hchk_nr, hx⟩
      cases r with
      | Err _ => cases (Prod.mk.inj (ok_inj hx)).1
      | Ok _ =>
        dsimp only at hx
        rcases bind_eq_ok hx with ⟨al, hal_op, hx⟩
        cases al <;> simp only [Bool.false_eq_true, ↓reduceIte] at hx
        · cases (Prod.mk.inj (ok_inj hx)).1
        · rcases Prod.mk.inj (ok_inj hx) with ⟨_, rfl⟩
          have hleak_eq : env.allowPtrLeaks = vctx.calling_context.allow_ptr_leaks := hcctx.1.1
          cases hpl : vctx.calling_context.allow_ptr_leaks <;>
            simp only [hpl, Bool.false_eq_true, ↓reduceIte] at hal_op
          · -- `allow_ptr_leaks = false` -> `is_written_scalar av = ok true`
            obtain ⟨d, rfl, hunw⟩ := Type_is_written_scalar_ok_true hal_op
            have hne : d ≠ uninitScalar64 := fun hc => by
              subst hc; exact uninitScalar64_unwritten_ne_zero hunw
            simp only [TypeRel2, hne, ↓reduceIte] at hru
            rcases hru with ⟨x₁, x₂, rfl, rfl, _⟩
            exact ⟨rfl,
              by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, RuntimeVal.isScalar],
              by simp [matchOneHelperParam, checkAlive, RuntimeVal.resourceId?, RuntimeVal.isScalar]⟩
          · -- `allow_ptr_leaks = true` -> `is_initialized av = ok true`
            have hne_u : av ≠ .ScalarValue uninitScalar64 := by
              intro hc; subst hc
              rw [Type_is_initialized_spec] at hal_op
              dsimp only at hal_op
              have hunw : uninitScalar64.unwritten_mask = 0#u64 :=
                of_decide_eq_true (ok_inj hal_op)
              exact uninitScalar64_unwritten_ne_zero hunw
            obtain ⟨hca1, hca2_w1⟩ := Type_check_not_released_checkAlive_none
              hacc.res_valid hacc.live_iff hav hne_u hchk_nr hru
            have hca2 : checkAlive wrld₂.liveResources u₂ = none := by rwa [← hacc.wrld_eq]
            have henv_pl : env.allowPtrLeaks = true := hleak_eq.trans hpl
            exact ⟨rfl,
              by simp [matchOneHelperParam, hca1, henv_pl],
              by simp [matchOneHelperParam, hca2, henv_pl]⟩
    rw [verifier.Type.match_parameter_type.eq_def] at hm
    cases argVal with
    | Releasable id2 inner =>
      dsimp only at hm
      rcases bind_eq_ok hm with ⟨b, hcont, hm⟩
      cases b <;> simp only [Bool.false_eq_true, ↓reduceIte] at hm
      · cases (Prod.mk.inj (ok_inj hm)).1
      · obtain ⟨_, hinnerV, _, _, _, _, _, _, _, _, hstripRel, hstripEq⟩ :=
          releasable_arg_unwrap_spec hacc hargV hargRel hcont
        obtain ⟨heq1, _⟩ := hstripEq l₁ st₁.regs st₁.stack helperIdx index.val .anyArg stk₁ rfl
        have heq2' := (hstripEq l₂ st₂.regs st₂.stack helperIdx index.val .anyArg stk₂ rfl).2
        cases inner <;> try exact False.elim hargV
        · rw [verifier.Type.match_parameter_type.eq_def] at hm
          obtain ⟨rfl, h1, h2⟩ := hleaf _ _ _ hinnerV hstripRel hm
          exact ⟨stk₁, stk₂, wrld₁, wrld₂, heq1.trans h1, heq2'.trans h2, hacc⟩
        · rw [verifier.Type.match_parameter_type.eq_def] at hm
          obtain ⟨rfl, h1, h2⟩ := hleaf _ _ _ hinnerV hstripRel hm
          exact ⟨stk₁, stk₂, wrld₁, wrld₂, heq1.trans h1, heq2'.trans h2, hacc⟩
    | _ =>
      obtain ⟨rfl, h1, h2⟩ := hleaf _ v₁ v₂ hargV hargRel hm
      exact ⟨stk₁, stk₂, wrld₁, wrld₂, h1, h2, hacc⟩

/-- Forward validity preservation across `ComputationContext.call_external_loop`. -/
theorem call_external_loop_ok_valid
    {env : ExecEnv} {vCounter : Nat}
    {ctx next nextOut : verifier.ComputationContext}
    {vctx : verifier.VerificationContext}
    {sigArgs : alloc.vec.Vec verifier.Type} {specArgs : List HelperParamSpec}
    (hctx : CompCtxValid env vCounter ctx)
    (hforall : List.Forall₂ (ParamTypeMatches env) sigArgs.val specArgs)
    (hnext : CompCtxValid env vCounter next)
    (hloop : verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.call_external_loop
      ctx.pc ctx.registers ctx.stack ctx.array_bounds ctx.resources
      ctx.path_depth ctx.jmp_seq
      vctx.calling_context vctx.log_enabled vctx.pending_log
      vctx.pending_successors vctx.code vctx.counter
      vctx.iteration vctx.terminating_contexts
      vctx.memory_access_instructions sigArgs next none 0#usize = ok (nextOut, none)) :
    nextOut.pc = next.pc ∧
    nextOut.registers = next.registers ∧
    nextOut.array_bounds = next.array_bounds ∧
    CompCtxValid env vCounter nextOut := by
  suffices ∀ n (cur : verifier.ComputationContext) (arg_idx : Std.Usize),
      sigArgs.val.length - arg_idx.val = n →
      arg_idx.val ≤ sigArgs.val.length →
      CompCtxValid env vCounter cur →
      verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.call_external_loop
        ctx.pc ctx.registers ctx.stack ctx.array_bounds ctx.resources
        ctx.path_depth ctx.jmp_seq
        vctx.calling_context vctx.log_enabled vctx.pending_log
        vctx.pending_successors vctx.code vctx.counter
        vctx.iteration vctx.terminating_contexts
        vctx.memory_access_instructions sigArgs cur none arg_idx = ok (nextOut, none) →
      nextOut.pc = cur.pc ∧
      nextOut.registers = cur.registers ∧
      nextOut.array_bounds = cur.array_bounds ∧
      CompCtxValid env vCounter nextOut by
    exact this sigArgs.val.length next 0#usize (by simp) (Nat.zero_le _) hnext hloop
  intro n
  induction n with
  | zero =>
    intro cur arg_idx hrem hle hcur hl
    have hnlt : ¬ (arg_idx < alloc.vec.Vec.len sigArgs) := by
      intro hc; have : arg_idx.val < sigArgs.val.length := hc; omega
    unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.call_external_loop at hl
    rw [loop_unfold_step] at hl
    unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.call_external_loop.body at hl
    simp only [hnlt, ↓reduceIte, bind_ok] at hl
    rcases Prod.mk.inj (ok_inj hl) with ⟨rfl, _⟩
    exact ⟨rfl, rfl, rfl, hcur⟩
  | succ m ih =>
    intro cur arg_idx hrem hle hcur hl
    have hlt_l : arg_idx.val < sigArgs.val.length := by omega
    have hlt : arg_idx < alloc.vec.Vec.len sigArgs := hlt_l
    unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.call_external_loop at hl
    rw [loop_unfold_step] at hl
    unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.call_external_loop.body at hl
    rcases bind_eq_ok hl with ⟨cf, hbody, hl⟩
    simp only [hlt, ↓reduceIte, core.option.Option.is_none, Option.isNone] at hbody
    obtain ⟨i4, hi4, hi4_val⟩ := usize_add_eq_ok arg_idx 1#usize
      (by rw [usize_one_val]; have := sigArgs.property; omega)
    rw [usize_one_val] at hi4_val
    rw [bind_ok_eq (vec_index_usize_eq_ok sigArgs arg_idx hlt_l)] at hbody
    rcases bind_eq_ok hbody with ⟨⟨r1, next1⟩, hm_call, _⟩
    rw [bind_pair_ok_eq hm_call] at hbody
    cases r1 with
    | Err e =>
      cases ok_inj hbody
      dsimp only at hl
      rw [loop_unfold_step] at hl
      dsimp only at hl
      by_cases hlt2 : arg_idx < alloc.vec.Vec.len sigArgs
      · simp [hlt2, core.option.Option.is_none] at hl
      · simp [hlt2] at hl
    | Ok u0 =>
      cases u0
      dsimp only at hbody
      rw [bind_ok_eq hi4] at hbody
      cases ok_inj hbody
      dsimp only at hl
      unfold verifier.ComputationContext.match_call_arg at hm_call
      rw [bind_ok_eq hi4] at hm_call
      simp only [lift_eq_ok, bind_ok] at hm_call
      rcases bind_eq_ok hm_call with ⟨r_reg, hreg, hm⟩
      cases r_reg with
      | Err _ => cases (Prod.mk.inj (ok_inj hm)).1
      | Ok arg_val =>
        dsimp only at hm
        obtain ⟨_, hargV⟩ := ComputationContext_reg_ok_valid hctx hreg
        have hlen_eq : sigArgs.val.length = specArgs.length := hforall.length_eq
        have hlt_sp : arg_idx.val < specArgs.length := hlen_eq ▸ hlt_l
        have hpm : ParamTypeMatches env (sigArgs.val[arg_idx.val]'hlt_l) (specArgs[arg_idx.val]'hlt_sp) :=
          (List.forall₂_iff_get.mp hforall).2 arg_idx.val hlt_l hlt_sp
        obtain ⟨hpc1, hrg1, hbm1, hn1_valid⟩ :=
          Type_match_parameter_type_ok_valid (vctx := vctx) (ctx := ctx) hpm hargV hcur hm
        obtain ⟨hpc2, hrg2, hbm2, hout_valid⟩ :=
          ih next1 i4 (by omega) (by omega) hn1_valid hl
        exact ⟨hpc2.trans hpc1, hrg2.trans hrg1, hbm2.trans hbm1, hout_valid⟩

/-- Relational 2-safety soundness of `ComputationContext.call_external_loop` against
    `Spec.checkAndApplyHelperArgs`. -/
theorem call_external_loop_sound
    {env : ExecEnv} {vCounter helperIdx : Nat}
    {l₁ l₂ : HostLayout}
    {ctx next nextOut : verifier.ComputationContext}
    {vctx : verifier.VerificationContext}
    {dPre dPost : verifier.DataDependencies}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {st₁ st₂ : MachineState}
    {sigArgs : alloc.vec.Vec verifier.Type} {specArgs : List HelperParamSpec}
    (hcctx : CallingContextFullMatches vctx.calling_context env)
    (hctx : CompCtxValid env vCounter ctx)
    (hρ : RhoValid vCounter st₁.world.nextAllocId ρ)
    (hrel : StateRel2Deps env ctx dPre ρ resActive isNullId st₁ st₂)
    (hle5 : sigArgs.val.length ≤ 5)
    (hforall : List.Forall₂ (ParamTypeMatches env) sigArgs.val specArgs)
    (hidx_all : ∀ p ∈ specArgs, HelperParamIndexValid specArgs.length p)
    (hargs_live : ∀ k < sigArgs.val.length, DataDepsHasReg dPre (k + 1) = true)
    (hcov : ∀ (k : Nat) (hk : k < sigArgs.val.length) (regTy : verifier.Type)
       (offset : verifier.StackOffset) (input_size : Std.U64),
      verifier.ComputationContext.reg ctx ⟨BitVec.ofNat 8 (k + 1)⟩ = ok (.Ok regTy) →
      verifier.Type.inner regTy ctx = ok (.Ok (.PtrToStack offset)) →
      verifier.Type.stack_input_size (sigArgs.val[k]'hk) ctx = ok (.Ok (some input_size)) →
      input_size.bv ≠ 0#64 →
      ∀ slot < 64,
        (∃ b < input_size.bv.toNat, (offset.value.bv.toNat + b) / 8 = slot) →
        DataDepsHasStack dPre slot = true)
    (hacc0 : HelperCallAccRel env vCounter ctx dPost ρ resActive isNullId
      st₁ st₂ next st₁.stack st₂.stack st₁.world st₂.world)
    (hloop : verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.call_external_loop
      ctx.pc ctx.registers ctx.stack ctx.array_bounds ctx.resources
      ctx.path_depth ctx.jmp_seq
      vctx.calling_context vctx.log_enabled vctx.pending_log
      vctx.pending_successors vctx.code vctx.counter
      vctx.iteration vctx.terminating_contexts
      vctx.memory_access_instructions sigArgs next none 0#usize = ok (nextOut, none)) :
    ∃ stk₁' stk₂' wrld₁' wrld₂',
      checkAndApplyHelperArgs env l₁ st₁.regs st₁.stack helperIdx 0
        specArgs st₁.stack st₁.world = .ok (stk₁', wrld₁') ∧
      checkAndApplyHelperArgs env l₂ st₂.regs st₂.stack helperIdx 0
        specArgs st₂.stack st₂.world = .ok (stk₂', wrld₂') ∧
      HelperCallAccRel env vCounter ctx dPost ρ resActive isNullId
        st₁ st₂ nextOut stk₁' stk₂' wrld₁' wrld₂' := by
  have hlen_eq : sigArgs.val.length = specArgs.length := hforall.length_eq
  have hargs_live' : ∀ j < specArgs.length, DataDepsHasReg dPre (j + 1) = true := by
    rw [← hlen_eq]; exact hargs_live
  suffices ∀ n (cur : verifier.ComputationContext) (arg_idx : Std.Usize)
      (stk₁ stk₂ : StackMem) (wrld₁ wrld₂ : WorldState),
      sigArgs.val.length - arg_idx.val = n →
      arg_idx.val ≤ sigArgs.val.length →
      HelperCallAccRel env vCounter ctx dPost ρ resActive isNullId
        st₁ st₂ cur stk₁ stk₂ wrld₁ wrld₂ →
      verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.call_external_loop
        ctx.pc ctx.registers ctx.stack ctx.array_bounds ctx.resources
        ctx.path_depth ctx.jmp_seq
        vctx.calling_context vctx.log_enabled vctx.pending_log
        vctx.pending_successors vctx.code vctx.counter
        vctx.iteration vctx.terminating_contexts
        vctx.memory_access_instructions sigArgs cur none arg_idx = ok (nextOut, none) →
      ∃ stk₁' stk₂' wrld₁' wrld₂',
        checkAndApplyHelperArgs env l₁ st₁.regs st₁.stack helperIdx arg_idx.val
          (specArgs.drop arg_idx.val) stk₁ wrld₁ = .ok (stk₁', wrld₁') ∧
        checkAndApplyHelperArgs env l₂ st₂.regs st₂.stack helperIdx arg_idx.val
          (specArgs.drop arg_idx.val) stk₂ wrld₂ = .ok (stk₂', wrld₂') ∧
        HelperCallAccRel env vCounter ctx dPost ρ resActive isNullId
          st₁ st₂ nextOut stk₁' stk₂' wrld₁' wrld₂' by
    simpa [usize_const_val_0] using
      this sigArgs.val.length next 0#usize st₁.stack st₂.stack st₁.world st₂.world
        (by simp) (Nat.zero_le _) hacc0 hloop
  intro n
  induction n with
  | zero =>
    intro cur arg_idx stk₁ stk₂ wrld₁ wrld₂ hrem hle hacc hl
    have hi : arg_idx.val = sigArgs.val.length := by omega
    have hnlt : ¬ (arg_idx < alloc.vec.Vec.len sigArgs) := by
      intro hc; have : arg_idx.val < sigArgs.val.length := hc; omega
    unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.call_external_loop at hl
    rw [loop_unfold_step] at hl
    unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.call_external_loop.body at hl
    simp only [hnlt, ↓reduceIte, bind_ok] at hl
    rcases Prod.mk.inj (ok_inj hl) with ⟨rfl, _⟩
    have hdrop : specArgs.drop arg_idx.val = [] := by
      rw [hi, hlen_eq, List.drop_length]
    rw [hdrop]
    exact ⟨stk₁, stk₂, wrld₁, wrld₂, rfl, rfl, hacc⟩
  | succ m ih =>
    intro cur arg_idx stk₁ stk₂ wrld₁ wrld₂ hrem hle hacc hl
    have hlt_l : arg_idx.val < sigArgs.val.length := by omega
    have hlt : arg_idx < alloc.vec.Vec.len sigArgs := hlt_l
    unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.call_external_loop at hl
    rw [loop_unfold_step] at hl
    unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.call_external_loop.body at hl
    rcases bind_eq_ok hl with ⟨cf, hbody, hl⟩
    simp only [hlt, ↓reduceIte, core.option.Option.is_none, Option.isNone] at hbody
    obtain ⟨i4, hi4, hi4_val⟩ := usize_add_eq_ok arg_idx 1#usize
      (by rw [usize_one_val]; have := sigArgs.property; omega)
    rw [usize_one_val] at hi4_val
    rw [bind_ok_eq (vec_index_usize_eq_ok sigArgs arg_idx hlt_l)] at hbody
    rcases bind_eq_ok hbody with ⟨⟨r1, next1⟩, hm_call, _⟩
    rw [bind_pair_ok_eq hm_call] at hbody
    cases r1 with
    | Err _ =>
      cases ok_inj hbody; dsimp only at hl; rw [loop_unfold_step] at hl; dsimp only at hl
      by_cases hlt2 : arg_idx < alloc.vec.Vec.len sigArgs
      · simp [hlt2, core.option.Option.is_none] at hl
      · simp [hlt2] at hl
    | Ok u0 =>
      cases u0
      dsimp only at hbody
      rw [bind_ok_eq hi4] at hbody
      cases ok_inj hbody
      dsimp only at hl
      unfold verifier.ComputationContext.match_call_arg at hm_call
      rw [bind_ok_eq hi4] at hm_call
      simp only [lift_eq_ok, bind_ok] at hm_call
      rcases bind_eq_ok hm_call with ⟨r_reg, hreg, hm⟩
      cases r_reg with
      | Err _ => cases (Prod.mk.inj (ok_inj hm)).1
      | Ok arg_val =>
        dsimp only at hm
        have hu8_val : (UScalar.cast .U8 i4).val = arg_idx.val + 1 := by
          change (BitVec.setWidth 8 i4.bv).toNat = arg_idx.val + 1
          rw [BitVec.toNat_setWidth, UScalar.bv_toNat, hi4_val]
          exact Nat.mod_eq_of_lt (by omega)
        have hu8_eq : UScalar.cast .U8 i4 = (⟨BitVec.ofNat 8 (arg_idx.val + 1)⟩ : Std.U8) :=
          UScalar.eq_of_val_eq (by rw [hu8_val]; change _ = (BitVec.ofNat 8 (arg_idx.val + 1)).toNat;
                                   rw [BitVec.toNat_ofNat, Nat.mod_eq_of_lt (by omega)])
        obtain ⟨_, hargV, hargRel⟩ := ComputationContext_reg_ok_valid_and_rel2 hctx hrel
          (Or.inr (by rw [hu8_val]; exact hargs_live arg_idx.val hlt_l)) hreg
        rw [hu8_val] at hargRel
        have hlt_sp : arg_idx.val < specArgs.length := hlen_eq ▸ hlt_l
        have hpm : ParamTypeMatches env (sigArgs.val[arg_idx.val]'hlt_l) (specArgs[arg_idx.val]'hlt_sp) :=
          (List.forall₂_iff_get.mp hforall).2 arg_idx.val hlt_l hlt_sp
        have hidx_p : HelperParamIndexValid specArgs.length (specArgs[arg_idx.val]'hlt_sp) :=
          hidx_all _ (List.getElem_mem hlt_sp)
        have hstk_cov_k : ∀ (offset : verifier.StackOffset) (input_size : Std.U64),
            arg_val = .PtrToStack offset →
            verifier.Type.stack_input_size (sigArgs.val[arg_idx.val]'hlt_l) ctx =
              ok (.Ok (some input_size)) →
            input_size.bv ≠ 0#64 →
            ∀ slot < 64,
              (∃ b < input_size.bv.toNat, (offset.value.bv.toNat + b) / 8 = slot) →
              DataDepsHasStack dPre slot = true := by
          intro offset input_size hav_eq hsis hnz
          subst hav_eq
          have hreg' : verifier.ComputationContext.reg ctx ⟨BitVec.ofNat 8 (arg_idx.val + 1)⟩ =
              ok (.Ok (.PtrToStack offset)) := by rwa [← hu8_eq]
          have hinner_stk : verifier.Type.inner (.PtrToStack offset) ctx =
              ok (.Ok (.PtrToStack offset)) := by
            rw [verifier.Type.inner.eq_def]
            unfold verifier.Type.check_not_released verifier.Type.unwrap_releasable
            simp only [bind_ok]
          exact hcov arg_idx.val hlt_l (.PtrToStack offset) offset input_size hreg' hinner_stk hsis hnz
        obtain ⟨stk₁_m, stk₂_m, wrld₁_m, wrld₂_m, hm1, hm2, hacc1⟩ :=
          Type_match_parameter_type_rel2 (vctx := vctx) (l₁ := l₁) (l₂ := l₂) (helperIdx := helperIdx)
            hcctx hctx hρ hrel hargs_live' hpm hidx_p hstk_cov_k hacc hargV hargRel hm
        obtain ⟨stk₁', stk₂', wrld₁', wrld₂', hrest1, hrest2, hacc_fin⟩ :=
          ih next1 i4 stk₁_m stk₂_m wrld₁_m wrld₂_m (by omega) (by omega) hacc1 hl
        rw [hi4_val] at hrest1 hrest2
        have hn_ge5 : ¬ (arg_idx.val ≥ 5) := by omega
        rw [List.drop_eq_getElem_cons hlt_sp]
        exact ⟨stk₁', stk₂', wrld₁', wrld₂',
          by simp [checkAndApplyHelperArgs, hn_ge5, hm1, hrest1],
          by simp [checkAndApplyHelperArgs, hn_ge5, hm2, hrest2],
          hacc_fin⟩

/-! ## 4. `resolve_return_value` & `invalidate_array_bounds` Soundness -/

private theorem eq_knownScalar64_zero {off : U64ScalarValueData}
    (h1 : off.value.bv = 0#64) (h2 : off.unknown_mask.bv = 0#64)
    (h3 : off.unwritten_mask.bv = 0#64)
    (h4 : off.urange.min.bv = 0#64) (h5 : off.urange.max.bv = 0#64) :
    off = knownScalar64 0#u64 := by
  cases off with
  | mk v um uw r g =>
    cases r with
    | mk lo hi =>
      cases g
      unfold knownScalar64
      simp only [U64ScalarValueData.mk.injEq, U64Range.mk.injEq, u64_eq_iff_bv]
      exact ⟨h1, h2, h3, ⟨h4, h5⟩, trivial⟩

private theorem eq_unknownWrittenScalar64 {d : U64ScalarValueData}
    (h1 : d.value.bv = 0#64) (h2 : d.unknown_mask.bv = ~~~0#64)
    (h3 : d.unwritten_mask.bv = 0#64)
    (h4 : d.urange.min.bv = 0#64) (h5 : d.urange.max.bv = ~~~0#64) :
    d = unknownWrittenScalar64 := by
  cases d with
  | mk v um uw r g =>
    cases r with
    | mk lo hi =>
      cases g
      unfold unknownWrittenScalar64
      simp only [U64ScalarValueData.mk.injEq, U64Range.mk.injEq, u64_eq_iff_bv, u64_max_bv]
      exact ⟨h1, h2, h3, ⟨h4, h5⟩, trivial⟩

private theorem eq_uninitScalar64 {d : U64ScalarValueData}
    (h1 : d.value.bv = 0#64) (h2 : d.unknown_mask.bv = ~~~0#64)
    (h3 : d.unwritten_mask.bv = ~~~0#64)
    (h4 : d.urange.min.bv = 0#64) (h5 : d.urange.max.bv = ~~~0#64) :
    d = uninitScalar64 := by
  cases d with
  | mk v um uw r g =>
    cases r with
    | mk lo hi =>
      cases g
      unfold uninitScalar64
      simp only [U64ScalarValueData.mk.injEq, U64Range.mk.injEq, u64_eq_iff_bv, u64_max_bv]
      exact ⟨h1, h2, h3, ⟨h4, h5⟩, trivial⟩

private theorem ptrOffsetValid_knownZero :
    PtrOffsetValid 0xFFFF_FFFF#64 (knownScalar64 0#u64) :=
  ⟨knownScalar64_valid 0#u64, rfl, by decide⟩

private theorem ReturnTypeMatches_inv {env : ExecEnv}
    {retTy : verifier.Type} {retSpec : HelperReturnSpec}
    (h : ReturnTypeMatches env retTy retSpec) :
    (retTy = .ScalarValue unknownWrittenScalar64 ∧ retSpec = .scalar true) ∨
    (retTy = .ScalarValue uninitScalar64 ∧ retSpec = .scalar false) ∨
    (∃ innerS, retSpec = .pointer false none innerS ∧ PtrReturnTypeMatches env retTy innerS) ∨
    (∃ relId innerT innerS,
      retTy = .ReleasableParameter relId innerT ∧
      retSpec = .pointer false (some relId) innerS ∧
      memoryIdNoVerif relId = true ∧
      PtrReturnTypeMatches env innerT innerS) ∨
    (∃ innerT innerS,
      retTy = .NullOrParameter innerT ∧
      retSpec = .pointer true none innerS ∧
      PtrReturnTypeMatches env innerT innerS) ∨
    (∃ relId innerT innerS,
      retTy = .NullOrParameter (.ReleasableParameter relId innerT) ∧
      retSpec = .pointer true (some relId) innerS ∧
      memoryIdNoVerif relId = true ∧
      PtrReturnTypeMatches env innerT innerS) := by
  cases retSpec with
  | scalar b =>
    cases b <;> cases retTy <;> try exact False.elim h
    · rename_i d
      rcases h with ⟨h1, h2, h3, h4, h5⟩
      exact Or.inr (Or.inl ⟨congrArg _ (eq_uninitScalar64 h1 h2 h3 h4 h5), rfl⟩)
    · rename_i subT
      cases subT <;> exact False.elim h
    · rename_i d
      rcases h with ⟨h1, h2, h3, h4, h5⟩
      exact Or.inl ⟨congrArg _ (eq_unknownWrittenScalar64 h1 h2 h3 h4 h5), rfl⟩
    · rename_i subT
      cases subT <;> exact False.elim h
  | pointer nullable relSchema? innerS =>
    cases nullable <;> cases relSchema?
    · -- false, none
      cases retTy <;> try exact False.elim h
      case NullOrParameter subT =>
        cases subT <;> exact Or.inr (Or.inr (Or.inl ⟨innerS, rfl, h⟩))
      all_goals exact Or.inr (Or.inr (Or.inl ⟨innerS, rfl, h⟩))
    · -- false, some relId'
      rename_i relId'
      cases retTy <;> try exact False.elim h
      case NullOrParameter subT =>
        cases subT <;> exact False.elim h
      case ReleasableParameter relId innerT =>
        rcases h with ⟨rfl, hnoV, hpm⟩
        exact Or.inr (Or.inr (Or.inr (Or.inl ⟨relId, innerT, innerS, rfl, rfl, hnoV, hpm⟩)))
    · -- true, none
      cases retTy <;> try exact False.elim h
      rename_i innerT
      cases innerT <;>
        exact Or.inr (Or.inr (Or.inr (Or.inr (Or.inl ⟨_, innerS, rfl, rfl, h⟩))))
    · -- true, some relId'
      rename_i relId'
      cases retTy <;> try exact False.elim h
      rename_i subT
      cases subT <;> try exact False.elim h
      rename_i relId innerT
      rcases h with ⟨rfl, hnoV, hpm⟩
      exact Or.inr (Or.inr (Or.inr (Or.inr (Or.inr ⟨relId, innerT, innerS, rfl, rfl, hnoV, hpm⟩))))

private theorem resolve_ptr_return_ok_valid
    {env : ExecEnv} {ctx next next' : verifier.ComputationContext}
    {vctx vctx' : verifier.VerificationContext}
    {innerT t_ptr : verifier.Type} {innerS : HelperPtrReturnKind}
    (hpm : PtrReturnTypeMatches env innerT innerS)
    (hres : verifier.ComputationContext.resolve_return_value ctx vctx innerT next =
      ok (.Ok t_ptr, vctx', next')) :
    vctx' = { vctx with counter := vctx'.counter } ∧
    vctx'.counter.bv.toNat = vctx.counter.bv.toNat + 1 ∧
    next' = next ∧
    TypeValid env vctx'.counter.bv.toNat t_ptr ∧
    NullOrInnerValid t_ptr ∧
    ((∃ id off sz orig, t_ptr = .PtrToMemory id off sz orig) ∨
     (∃ id off sd, t_ptr = .PtrToStruct id off sd)) := by
  cases innerT <;> try (cases innerS <;> exact False.elim hpm)
  case PtrToMemory id off sz orig =>
    cases innerS <;> try exact False.elim hpm
    rename_i id' sz' orig'
    rcases hpm with ⟨rfl, hnoV, h1, h2, h3, h4, h5, _, _⟩
    have hoff_eq := eq_knownScalar64_zero h1 h2 h3 h4 h5
    rw [verifier.ComputationContext.resolve_return_value.eq_def] at hres
    dsimp only at hres
    rcases bind_eq_ok hres with ⟨p_nid, hnid, _⟩
    rcases p_nid with ⟨mi, vc1⟩
    rw [bind_pair_ok_eq hnid, bind_ok_eq (MemoryId_prepended_spec id mi)] at hres
    obtain ⟨hmi_eq, hvc1_eq, hvc1_cnt⟩ := VerificationContext_next_id_ok_spec hnid
    rcases Prod.mk.inj (ok_inj hres) with ⟨hr, hpair⟩
    rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
    cases core.result.Result.Ok.inj hr
    have hmiB : memIdBounded vc1.counter.bv.toNat mi = true := by
      rw [hmi_eq]; simp only [memIdBounded, decide_eq_true_eq]; omega
    have hnewB : memIdBounded vc1.counter.bv.toNat (memoryIdPrepend id mi) = true :=
      memIdBounded_prepend (memIdBounded_of_noVerif _ hnoV) hmiB
    exact ⟨hvc1_eq, hvc1_cnt, rfl,
      ⟨hnewB, hoff_eq ▸ ptrOffsetValid_knownZero⟩, trivial, Or.inl ⟨_, _, _, _, rfl⟩⟩
  case PtrToStruct id off sd =>
    cases innerS <;> try exact False.elim hpm
    rename_i id'
    rcases hpm with ⟨rfl, hsd, h1, h2, h3, h4, h5⟩
    have hoff_eq := eq_knownScalar64_zero h1 h2 h3 h4 h5
    rw [verifier.ComputationContext.resolve_return_value.eq_def] at hres
    dsimp only at hres
    rcases bind_eq_ok hres with ⟨p_nid, hnid, _⟩
    rcases p_nid with ⟨mi, vc1⟩
    rw [bind_pair_ok_eq hnid,
        bind_ok_eq (MemoryId_prepended_spec id mi),
        bind_ok_eq (StructDescriptor_clone_spec sd)] at hres
    obtain ⟨hmi_eq, hvc1_eq, hvc1_cnt⟩ := VerificationContext_next_id_ok_spec hnid
    rcases Prod.mk.inj (ok_inj hres) with ⟨hr, hpair⟩
    rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
    cases core.result.Result.Ok.inj hr
    have hmiB : memIdBounded vc1.counter.bv.toNat mi = true := by
      rw [hmi_eq]; simp only [memIdBounded, decide_eq_true_eq]; omega
    have hnewB : memIdBounded vc1.counter.bv.toNat (memoryIdPrepend id mi) = true :=
      memIdBounded_prepend (memIdBounded_of_noVerif _ hsd.1) hmiB
    exact ⟨hvc1_eq, hvc1_cnt, rfl,
      ⟨hnewB, hoff_eq ▸ ptrOffsetValid_knownZero, ⟨id, hsd, memoryIdMatches_prepend_right id mi⟩⟩,
      trivial, Or.inr ⟨_, _, _, rfl⟩⟩
  case MapValueParameter mapIdx =>
    cases innerS <;> try exact False.elim hpm
    rw [verifier.ComputationContext.resolve_return_value.eq_def] at hres
    dsimp only at hres
    rcases bind_eq_ok hres with ⟨r_ms, _, hres⟩
    cases r_ms with
    | Err _ => cases (Prod.mk.inj (ok_inj hres)).1
    | Ok s =>
      dsimp only at hres
      rcases bind_eq_ok hres with ⟨p_nid, hnid, _⟩
      rcases p_nid with ⟨id, vc1⟩
      rw [bind_pair_ok_eq hnid] at hres
      obtain ⟨hid_eq, hvc1_eq, hvc1_cnt⟩ := VerificationContext_next_id_ok_spec hnid
      obtain ⟨mf, b_ro, hmf, hcont, _⟩ := MapFlags_ProgReadOnly_contains_spec s
      rw [bind_ok_eq hmf, bind_ok_eq hcont] at hres
      rcases bind_eq_ok hres with ⟨origin, _, hres⟩
      simp only [from_known_64_eq, lift_eq_ok, bind_ok] at hres
      rcases Prod.mk.inj (ok_inj hres) with ⟨hr, hpair⟩
      rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
      cases core.result.Result.Ok.inj hr
      have hidB : memIdBounded vc1.counter.bv.toNat id = true := by
        rw [hid_eq]; simp only [memIdBounded, decide_eq_true_eq]; omega
      exact ⟨hvc1_eq, hvc1_cnt, rfl,
        ⟨hidB, ptrOffsetValid_knownZero⟩, trivial, Or.inl ⟨_, _, _, _, rfl⟩⟩
  case MemoryParameter mps b1 b2 =>
    rw [verifier.ComputationContext.resolve_return_value.eq_def] at hres
    dsimp only at hres
    rcases bind_eq_ok hres with ⟨r_sz, _, hres⟩
    cases r_sz with
    | Err _ => cases (Prod.mk.inj (ok_inj hres)).1
    | Ok sz =>
      dsimp only at hres
      rcases bind_eq_ok hres with ⟨p_nid, hnid, _⟩
      rcases p_nid with ⟨id, vc1⟩
      rw [bind_pair_ok_eq hnid] at hres
      obtain ⟨hid_eq, hvc1_eq, hvc1_cnt⟩ := VerificationContext_next_id_ok_spec hnid
      simp only [from_known_64_eq, bind_ok] at hres
      rcases Prod.mk.inj (ok_inj hres) with ⟨hr, hpair⟩
      rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
      cases core.result.Result.Ok.inj hr
      have hidB : memIdBounded vc1.counter.bv.toNat id = true := by
        rw [hid_eq]; simp only [memIdBounded, decide_eq_true_eq]; omega
      exact ⟨hvc1_eq, hvc1_cnt, rfl,
        ⟨hidB, ptrOffsetValid_knownZero⟩, trivial, Or.inl ⟨_, _, _, _, rfl⟩⟩

theorem resolve_return_value_ok_valid
    {env : ExecEnv} {ctx next next' : verifier.ComputationContext}
    {vctx vctx' : verifier.VerificationContext}
    {retTy retVal : verifier.Type} {retSpec : HelperReturnSpec}
    (hret : ReturnTypeMatches env retTy retSpec)
    (hnext : CompCtxValid env vctx.counter.bv.toNat next)
    (hres : verifier.ComputationContext.resolve_return_value ctx vctx retTy next =
      ok (.Ok retVal, vctx', next')) :
    vctx'.calling_context = vctx.calling_context ∧
    vctx'.code = vctx.code ∧
    vctx'.iteration = vctx.iteration ∧
    vctx'.terminating_contexts = vctx.terminating_contexts ∧
    vctx'.pending_successors = vctx.pending_successors ∧
    vctx.counter.bv.toNat ≤ vctx'.counter.bv.toNat ∧
    next'.pc = next.pc ∧
    next'.registers = next.registers ∧
    next'.stack = next.stack ∧
    next'.array_bounds = next.array_bounds ∧
    CompCtxValid env vctx'.counter.bv.toNat next' ∧
    TypeValid env vctx'.counter.bv.toNat retVal := by
  rcases ReturnTypeMatches_inv hret with
    ⟨rfl, rfl⟩ | ⟨rfl, rfl⟩ | ⟨innerS, rfl, hpm⟩ |
    ⟨relId, innerT, innerS, rfl, rfl, hnoV, hpm⟩ |
    ⟨innerT, innerS, rfl, rfl, hpm⟩ |
    ⟨relId, innerT, innerS, rfl, rfl, hnoV, hpm⟩
  · -- ScalarValue unknownWrittenScalar64
    rw [verifier.ComputationContext.resolve_return_value.eq_def] at hres
    simp only [Type_clone_spec, bind_ok] at hres
    rcases Prod.mk.inj (ok_inj hres) with ⟨hr, hpair⟩
    rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
    cases core.result.Result.Ok.inj hr
    exact ⟨rfl, rfl, rfl, rfl, rfl, Nat.le_refl _, rfl, rfl, rfl, rfl,
      hnext, unknownWrittenScalar64_valid⟩
  · -- ScalarValue uninitScalar64
    rw [verifier.ComputationContext.resolve_return_value.eq_def] at hres
    simp only [Type_clone_spec, bind_ok] at hres
    rcases Prod.mk.inj (ok_inj hres) with ⟨hr, hpair⟩
    rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
    cases core.result.Result.Ok.inj hr
    exact ⟨rfl, rfl, rfl, rfl, rfl, Nat.le_refl _, rfl, rfl, rfl, rfl,
      hnext, uninitScalar64_valid⟩
  · -- pointer false none
    obtain ⟨hvc_eq, hvc_cnt, rfl, htV, _, _⟩ := resolve_ptr_return_ok_valid hpm hres
    refine ⟨by rw [hvc_eq], by rw [hvc_eq], by rw [hvc_eq], by rw [hvc_eq], by rw [hvc_eq],
      by omega, rfl, rfl, rfl, rfl, CompCtxValid_mono (by omega) hnext, htV⟩
  · -- pointer false (some relId)
    rw [verifier.ComputationContext.resolve_return_value.eq_def] at hres
    dsimp only at hres
    rcases bind_eq_ok hres with ⟨p_nid, hnid, _⟩
    rcases p_nid with ⟨mi, vc1⟩
    rw [bind_pair_ok_eq hnid] at hres
    obtain ⟨hmi_eq, hvc1_eq, hvc1_cnt⟩ := VerificationContext_next_id_ok_spec hnid
    rw [bind_ok_eq (MemoryId_clone_spec relId),
        bind_ok_eq (MemoryId_prepended_spec mi relId),
        bind_ok_eq (MemoryId_clone_spec (memoryIdPrepend mi relId))] at hres
    rcases bind_eq_ok hres with ⟨⟨_, is⟩, hins, hres⟩
    rcases bind_eq_ok hres with ⟨⟨r_in, vc2, next1⟩, hres_in, hres⟩
    cases r_in with
    | Err _ => cases (Prod.mk.inj (ok_inj hres)).1
    | Ok t_ptr =>
      rcases Prod.mk.inj (ok_inj hres) with ⟨hr, hpair⟩
      rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
      cases core.result.Result.Ok.inj hr
      have hmiB : memIdBounded vc1.counter.bv.toNat mi = true := by
        rw [hmi_eq]; simp only [memIdBounded, decide_eq_true_eq]; omega
      have hrelB1 : memIdBounded vc1.counter.bv.toNat (memoryIdPrepend mi relId) = true :=
        memIdBounded_prepend hmiB (memIdBounded_of_noVerif _ hnoV)
      have hnext_vc1 : CompCtxValid env vc1.counter.bv.toNat next :=
        CompCtxValid_mono (by omega) hnext
      obtain ⟨hisV, _, _⟩ := IdSet_insert_ok_spec hnext_vc1.2.2.2 hrelB1 hins
      have hnext_is : CompCtxValid env vc1.counter.bv.toNat { next with resources := is } :=
        ⟨hnext_vc1.1, hnext_vc1.2.1, hnext_vc1.2.2.1, hisV⟩
      obtain ⟨hvc2_eq, hvc2_cnt, rfl, htV, _, hshape⟩ :=
        resolve_ptr_return_ok_valid hpm hres_in
      have hrelB2 : memIdBounded vc2.counter.bv.toNat (memoryIdPrepend mi relId) = true :=
        memIdBounded_mono (by omega) hrelB1
      have hretV : TypeValid env vc2.counter.bv.toNat (.Releasable (memoryIdPrepend mi relId) t_ptr) := by
        rcases hshape with ⟨_, _, _, _, rfl⟩ | ⟨_, _, _, rfl⟩
        · exact ⟨hrelB2, htV⟩
        · exact ⟨hrelB2, htV⟩
      refine ⟨by rw [hvc2_eq, hvc1_eq], by rw [hvc2_eq, hvc1_eq],
        by rw [hvc2_eq, hvc1_eq], by rw [hvc2_eq, hvc1_eq], by rw [hvc2_eq, hvc1_eq],
        by omega, rfl, rfl, rfl, rfl, CompCtxValid_mono (by omega) hnext_is, hretV⟩
  · -- pointer true none
    rw [verifier.ComputationContext.resolve_return_value.eq_def] at hres
    dsimp only at hres
    rcases bind_eq_ok hres with ⟨⟨nullId, vc1⟩, hnid, hres⟩
    obtain ⟨hnull_eq, hvc1_eq, hvc1_cnt⟩ := VerificationContext_next_id_ok_spec hnid
    rcases bind_eq_ok hres with ⟨⟨r_in, vc2, next1⟩, hres_in, hres⟩
    cases r_in with
    | Err _ => cases (Prod.mk.inj (ok_inj hres)).1
    | Ok t_ptr =>
      rcases Prod.mk.inj (ok_inj hres) with ⟨hr, hpair⟩
      rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
      cases core.result.Result.Ok.inj hr
      obtain ⟨hvc2_eq, hvc2_cnt, rfl, htV, hshape_null, _⟩ :=
        resolve_ptr_return_ok_valid hpm hres_in
      have hnullB : memIdBounded vc2.counter.bv.toNat nullId = true := by
        rw [hnull_eq]; simp only [memIdBounded, decide_eq_true_eq]; omega
      refine ⟨by rw [hvc2_eq, hvc1_eq], by rw [hvc2_eq, hvc1_eq],
        by rw [hvc2_eq, hvc1_eq], by rw [hvc2_eq, hvc1_eq], by rw [hvc2_eq, hvc1_eq],
        by omega, rfl, rfl, rfl, rfl, CompCtxValid_mono (by omega) hnext,
        ⟨hnullB, hshape_null, htV⟩⟩
  · -- pointer true (some relId)
    rw [verifier.ComputationContext.resolve_return_value.eq_def] at hres
    dsimp only at hres
    rcases bind_eq_ok hres with ⟨⟨nullId, vc1⟩, hnid0, hres⟩
    obtain ⟨hnull_eq, hvc1_eq, hvc1_cnt⟩ := VerificationContext_next_id_ok_spec hnid0
    rcases bind_eq_ok hres with ⟨⟨r_rel, vc3, next2⟩, hres_rel, hres⟩
    cases r_rel with
    | Err _ => cases (Prod.mk.inj (ok_inj hres)).1
    | Ok t_rel =>
      rcases Prod.mk.inj (ok_inj hres) with ⟨hr, hpair⟩
      rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
      cases core.result.Result.Ok.inj hr
      rw [verifier.ComputationContext.resolve_return_value.eq_def] at hres_rel
      dsimp only at hres_rel
      rcases bind_eq_ok hres_rel with ⟨p_nid1, hnid1, _⟩
      rcases p_nid1 with ⟨mi, vc2⟩
      rw [bind_pair_ok_eq hnid1] at hres_rel
      obtain ⟨hmi_eq, hvc2_eq, hvc2_cnt⟩ := VerificationContext_next_id_ok_spec hnid1
      rw [bind_ok_eq (MemoryId_clone_spec relId),
          bind_ok_eq (MemoryId_prepended_spec mi relId),
          bind_ok_eq (MemoryId_clone_spec (memoryIdPrepend mi relId))] at hres_rel
      rcases bind_eq_ok hres_rel with ⟨⟨_, is⟩, hins, hres_rel⟩
      rcases bind_eq_ok hres_rel with ⟨⟨r_in, vc3', next1⟩, hres_in, hres_rel⟩
      cases r_in with
      | Err _ => cases (Prod.mk.inj (ok_inj hres_rel)).1
      | Ok t_ptr =>
        rcases Prod.mk.inj (ok_inj hres_rel) with ⟨hr_rel, hpair_rel⟩
        rcases Prod.mk.inj hpair_rel with ⟨rfl, rfl⟩
        cases core.result.Result.Ok.inj hr_rel
        have hmiB : memIdBounded vc2.counter.bv.toNat mi = true := by
          rw [hmi_eq]; simp only [memIdBounded, decide_eq_true_eq]; omega
        have hrelB2 : memIdBounded vc2.counter.bv.toNat (memoryIdPrepend mi relId) = true :=
          memIdBounded_prepend hmiB (memIdBounded_of_noVerif _ hnoV)
        have hnext_vc2 : CompCtxValid env vc2.counter.bv.toNat next :=
          CompCtxValid_mono (by omega) hnext
        obtain ⟨hisV, _, _⟩ := IdSet_insert_ok_spec hnext_vc2.2.2.2 hrelB2 hins
        have hnext_is : CompCtxValid env vc2.counter.bv.toNat { next with resources := is } :=
          ⟨hnext_vc2.1, hnext_vc2.2.1, hnext_vc2.2.2.1, hisV⟩
        obtain ⟨hvc3_eq, hvc3_cnt, rfl, htV, _, hshape⟩ :=
          resolve_ptr_return_ok_valid hpm hres_in
        have hrelB3 : memIdBounded vc3'.counter.bv.toNat (memoryIdPrepend mi relId) = true :=
          memIdBounded_mono (by omega) hrelB2
        have hnullB3 : memIdBounded vc3'.counter.bv.toNat nullId = true := by
          rw [hnull_eq]; simp only [memIdBounded, decide_eq_true_eq]; omega
        have hretV : TypeValid env vc3'.counter.bv.toNat
            (.NullOr nullId (.Releasable (memoryIdPrepend mi relId) t_ptr)) := by
          rcases hshape with ⟨_, _, _, _, rfl⟩ | ⟨_, _, _, rfl⟩
          · exact ⟨hnullB3, trivial, hrelB3, htV⟩
          · exact ⟨hnullB3, trivial, hrelB3, htV⟩
        refine ⟨by rw [hvc3_eq, hvc2_eq, hvc1_eq], by rw [hvc3_eq, hvc2_eq, hvc1_eq],
          by rw [hvc3_eq, hvc2_eq, hvc1_eq], by rw [hvc3_eq, hvc2_eq, hvc1_eq],
          by rw [hvc3_eq, hvc2_eq, hvc1_eq], by omega, rfl, rfl, rfl, rfl,
          CompCtxValid_mono (by omega) hnext_is, hretV⟩

/-! ### 4b. Relational 2-Safety of `resolve_return_value` against `Spec.evalHelperReturn` -/

@[reducible] private def evalPtrReturnInner
    (env : ExecEnv) (preRegs : Nat → RuntimeVal) (baseAlloc : MemoryId) :
    HelperPtrReturnKind → Option (MemoryId × MemoryOrigin × Option (BitVec 64))
  | .mapValue mapArgIdx =>
      (lookupHelperMapArg env preRegs mapArgIdx).map fun ms =>
        let orig := if ms.progReadOnly then MemoryOrigin.mapReadOnly else .mapWritable
        (baseAlloc, orig, some ms.valueSize)
  | .allocMemory sizeSpec =>
      (resolveHelperMemSize preRegs sizeSpec).map fun sz =>
        (baseAlloc, .mapWritable, some sz)
  | .staticMemory schemaId sz orig =>
      some (memoryIdPrepend schemaId baseAlloc, orig, some sz)
  | .staticStruct schemaId =>
      some (memoryIdPrepend schemaId baseAlloc, .kernelReadOnly, none)

private theorem evalHelperReturn_pointer_eq
    (env : ExecEnv) (l : HostLayout) (s : SecretInit)
    (preRegs : Nat → RuntimeVal) (preWorld : WorldState) (helperIdx : Nat)
    (nullable : Bool) (relSchema? : Option MemoryId) (inner : HelperPtrReturnKind)
    (wrldBounds : WorldState) :
    evalHelperReturn env l s preRegs preWorld helperIdx (.pointer nullable relSchema? inner) wrldBounds =
      match evalPtrReturnInner env preRegs
          (freshRuntimeId (wrldBounds.nextAllocId + (if nullable then 1 else 0) +
            (if relSchema?.isSome then 1 else 0)))
          inner with
      | none => none
      | some (ptrId, orig, bufSz?) =>
          let totalAllocs : Nat :=
            (if nullable then 1 else 0) + (if relSchema?.isSome then 1 else 0) + 1
          if (nullable && env.helperNullOracle wrldBounds.stepCount helperIdx) then
            some (.scalar 0#64, { wrldBounds with nextAllocId := wrldBounds.nextAllocId + totalAllocs })
          else
            let (resId?, wrld') : Option MemoryId × WorldState :=
              match relSchema? with
              | none =>
                  (none, { wrldBounds with nextAllocId := wrldBounds.nextAllocId + totalAllocs })
              | some relSchema =>
                  let rid := memoryIdPrepend
                    (freshRuntimeId (wrldBounds.nextAllocId + (if nullable then 1 else 0))) relSchema
                  (some rid,
                   { wrldBounds with
                       liveResources := rid :: wrldBounds.liveResources
                       nextAllocId := wrldBounds.nextAllocId + totalAllocs })
            some (match bufSz? with
              | some sz => .memPtr ptrId orig resId? sz 0#64
              | none    => .structPtr ptrId resId? 0#64, wrld') := by
  cases inner <;> rfl

private theorem freshMemId_root_ne_renamed
    {v rt allocIdx : Nat} {ρ : Std.U64 → Std.U64} {parent? : Option MemoryId} {m : MemoryId}
    (hρ : RhoValid v rt ρ) (hge : rt ≤ allocIdx) (hlt63 : allocIdx < 2 ^ 63)
    (hb : memIdBounded v m = true) :
    verifier.MemoryId.mk .Verification ⟨BitVec.ofNat 64 allocIdx⟩ parent? ≠ renameMemId ρ m := by
  intro heq
  cases m with
  | mk ns idx p =>
    cases ns with
    | Hardcoded | Generated =>
      cases p <;> cases heq
    | Verification =>
      have hidx_lt : idx.bv.toNat < v := by
        cases p <;> simp only [memIdBounded, Bool.and_eq_true, decide_eq_true_eq] at hb
        · exact hb
        · exact hb.1
      have hid_eq : (⟨BitVec.ofNat 64 allocIdx⟩ : Std.U64) = ρ idx := by
        cases p <;> simp only [renameMemId, renameNamespaceId, verifier.MemoryId.mk.injEq] at heq <;>
          exact heq.2.1
      have htoNat : allocIdx = (ρ idx).bv.toNat := by
        have h1 : (BitVec.ofNat 64 allocIdx).toNat = (ρ idx).bv.toNat :=
          congrArg (fun u : Std.U64 => u.bv.toNat) hid_eq
        rwa [BitVec.toNat_ofNat, Nat.mod_eq_of_lt (by omega)] at h1
      rcases hρ.2 idx hidx_lt with hρ_lt | ⟨_, hρ_hi⟩ <;> omega

private theorem resolve_ptr_return_sound
    {env : ExecEnv} {v0 rt_in baseAllocIdx : Nat}
    {ctx next next' : verifier.ComputationContext}
    {vctx vctx' : verifier.VerificationContext}
    {dPre : verifier.DataDependencies}
    {ρ0 ρ_in : Std.U64 → Std.U64} {resActive0 isNullId0 : MemoryId → Bool}
    {st₁ st₂ : MachineState}
    {innerT t_ptr : verifier.Type} {innerS : HelperPtrReturnKind}
    (hpm : PtrReturnTypeMatches env innerT innerS)
    (hidx_map : ∀ idx, innerS = .mapValue idx → DataDepsHasReg dPre (idx + 1) = true)
    (hidx_mem : ∀ idx, innerS = .allocMemory (.fromArg idx) → DataDepsHasReg dPre (idx + 1) = true)
    (hctx : CompCtxValid env v0 ctx)
    (hrel : StateRel2Deps env ctx dPre ρ0 resActive0 isNullId0 st₁ st₂)
    (hρ_in : RhoValid vctx.counter.bv.toNat rt_in ρ_in)
    (hge_alloc : rt_in ≤ baseAllocIdx)
    (hlt_alloc : baseAllocIdx < 2 ^ 63)
    (hres : verifier.ComputationContext.resolve_return_value ctx vctx innerT next =
      ok (.Ok t_ptr, vctx', next')) :
    ∃ (ρ_out : Std.U64 → Std.U64) (ptrId : MemoryId) (orig : MemoryOrigin)
      (bufSz? : Option (BitVec 64)),
      vctx' = { vctx with counter := vctx'.counter } ∧
      vctx'.counter.bv.toNat = vctx.counter.bv.toNat + 1 ∧
      next' = next ∧
      RhoValid vctx'.counter.bv.toNat (baseAllocIdx + 1) ρ_out ∧
      (∀ x : Std.U64, x.bv.toNat < vctx.counter.bv.toNat → ρ_in x = ρ_out x) ∧
      evalPtrReturnInner env st₁.regs (freshRuntimeId baseAllocIdx) innerS =
        some (ptrId, orig, bufSz?) ∧
      evalPtrReturnInner env st₂.regs (freshRuntimeId baseAllocIdx) innerS =
        some (ptrId, orig, bufSz?) ∧
      ((∃ sz rawId rawSz rawOrig,
          bufSz? = some sz ∧
          t_ptr = .PtrToMemory rawId (knownScalar64 0#u64) rawSz rawOrig ∧
          memIdBounded vctx'.counter.bv.toNat rawId = true ∧
          renameMemId ρ_out rawId = ptrId ∧
          MemoryOrigin.ofCore rawOrig = orig ∧
          rawSz.bv = sz) ∨
       (bufSz? = none ∧
        orig = .kernelReadOnly ∧
        ∃ rawId sd,
          t_ptr = .PtrToStruct rawId (knownScalar64 0#u64) sd ∧
          memIdBounded vctx'.counter.bv.toNat rawId = true ∧
          renameMemId ρ_out rawId = ptrId ∧
          (∃ schemaId, StructDescConforms env schemaId sd ∧
            memoryIdMatches schemaId rawId = true))) := by
  let allocU64 : Std.U64 := ⟨BitVec.ofNat 64 baseAllocIdx⟩
  have halloc_toNat : allocU64.bv.toNat = baseAllocIdx := by
    dsimp only [allocU64]
    rw [BitVec.toNat_ofNat, Nat.mod_eq_of_lt (by omega)]
  obtain ⟨hρ_ext, hag, hρ_at⟩ :=
    RhoValid_extend (rt' := baseAllocIdx + 1) (oldId := vctx.counter) (newId := allocU64)
      hρ_in rfl (by omega) (by omega) (by omega)
  let ρ_out := fun x => if x = vctx.counter then allocU64 else ρ_in x
  have hag_sym : ∀ x : Std.U64, x.bv.toNat < vctx.counter.bv.toNat → ρ_in x = ρ_out x :=
    fun x hx => (hag x hx).symm
  cases innerT <;> try (cases innerS <;> exact False.elim hpm)
  case PtrToMemory id off sz orig =>
    cases innerS <;> try exact False.elim hpm
    rename_i id' sz' orig'
    rcases hpm with ⟨rfl, hnoV, h1, h2, h3, h4, h5, hsz_eq, horig_eq⟩
    have hoff_eq := eq_knownScalar64_zero h1 h2 h3 h4 h5
    rw [verifier.ComputationContext.resolve_return_value.eq_def] at hres
    dsimp only at hres
    rcases bind_eq_ok hres with ⟨p_nid, hnid, _⟩
    rcases p_nid with ⟨mi, vc1⟩
    rw [bind_pair_ok_eq hnid, bind_ok_eq (MemoryId_prepended_spec id mi)] at hres
    obtain ⟨hmi_eq, hvc1_eq, hvc1_cnt⟩ := VerificationContext_next_id_ok_spec hnid
    rcases Prod.mk.inj (ok_inj hres) with ⟨hr, hpair⟩
    rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
    cases core.result.Result.Ok.inj hr
    rw [← hvc1_cnt] at hρ_ext
    have hmiB : memIdBounded vc1.counter.bv.toNat mi = true := by
      rw [hmi_eq]; simp only [memIdBounded, decide_eq_true_eq]; omega
    have hnewB : memIdBounded vc1.counter.bv.toNat (memoryIdPrepend id mi) = true :=
      memIdBounded_prepend (memIdBounded_of_noVerif _ hnoV) hmiB
    have hren : renameMemId ρ_out (memoryIdPrepend id mi) =
        memoryIdPrepend id (freshRuntimeId baseAllocIdx) := by
      rw [renameMemId_prepend, renameMemId_noVerif_id ρ_out hnoV, hmi_eq]
      simp only [renameMemId, renameNamespaceId, ρ_out, ↓reduceIte, freshRuntimeId, allocU64]
    exact ⟨ρ_out, memoryIdPrepend id (freshRuntimeId baseAllocIdx), orig', some sz',
      hvc1_eq, hvc1_cnt, rfl, hρ_ext, hag_sym, rfl, rfl,
      Or.inl ⟨sz', memoryIdPrepend id mi, sz, orig, rfl,
        by rw [hoff_eq], hnewB, hren, horig_eq, hsz_eq⟩⟩
  case PtrToStruct id off sd =>
    cases innerS <;> try exact False.elim hpm
    rename_i id'
    rcases hpm with ⟨rfl, hsd, h1, h2, h3, h4, h5⟩
    have hoff_eq := eq_knownScalar64_zero h1 h2 h3 h4 h5
    rw [verifier.ComputationContext.resolve_return_value.eq_def] at hres
    dsimp only at hres
    rcases bind_eq_ok hres with ⟨p_nid, hnid, _⟩
    rcases p_nid with ⟨mi, vc1⟩
    rw [bind_pair_ok_eq hnid,
        bind_ok_eq (MemoryId_prepended_spec id mi),
        bind_ok_eq (StructDescriptor_clone_spec sd)] at hres
    obtain ⟨hmi_eq, hvc1_eq, hvc1_cnt⟩ := VerificationContext_next_id_ok_spec hnid
    rcases Prod.mk.inj (ok_inj hres) with ⟨hr, hpair⟩
    rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
    cases core.result.Result.Ok.inj hr
    rw [← hvc1_cnt] at hρ_ext
    have hmiB : memIdBounded vc1.counter.bv.toNat mi = true := by
      rw [hmi_eq]; simp only [memIdBounded, decide_eq_true_eq]; omega
    have hnewB : memIdBounded vc1.counter.bv.toNat (memoryIdPrepend id mi) = true :=
      memIdBounded_prepend (memIdBounded_of_noVerif _ hsd.1) hmiB
    have hren : renameMemId ρ_out (memoryIdPrepend id mi) =
        memoryIdPrepend id (freshRuntimeId baseAllocIdx) := by
      rw [renameMemId_prepend, renameMemId_noVerif_id ρ_out hsd.1, hmi_eq]
      simp only [renameMemId, renameNamespaceId, ρ_out, ↓reduceIte, freshRuntimeId, allocU64]
    exact ⟨ρ_out, memoryIdPrepend id (freshRuntimeId baseAllocIdx), .kernelReadOnly, none,
      hvc1_eq, hvc1_cnt, rfl, hρ_ext, hag_sym, rfl, rfl,
      Or.inr ⟨rfl, rfl, memoryIdPrepend id mi, sd, by rw [hoff_eq], hnewB, hren,
        ⟨id, hsd, memoryIdMatches_prepend_right id mi⟩⟩⟩
  case MapValueParameter mapIdx =>
    cases innerS <;> try exact False.elim hpm
    rename_i mapIdx'
    have hidx_eq : mapIdx.val = mapIdx' := hpm
    subst mapIdx'
    rw [verifier.ComputationContext.resolve_return_value.eq_def] at hres
    dsimp only at hres
    rcases bind_eq_ok hres with ⟨r_ms, hget, hres⟩
    cases r_ms with
    | Err _ => cases (Prod.mk.inj (ok_inj hres)).1
    | Ok s =>
      dsimp only at hres
      obtain ⟨_, hlk1, hlk2⟩ := ComputationContext_get_map_schema_ok_spec hctx hrel
        (hidx_map mapIdx.val rfl) hget
      rcases bind_eq_ok hres with ⟨p_nid, hnid, _⟩
      rcases p_nid with ⟨id, vc1⟩
      rw [bind_pair_ok_eq hnid] at hres
      obtain ⟨hid_eq, hvc1_eq, hvc1_cnt⟩ := VerificationContext_next_id_ok_spec hnid
      obtain ⟨mf, b_ro, hmf, hcont, hbro_eq⟩ := MapFlags_ProgReadOnly_contains_spec s
      rw [bind_ok_eq hmf, bind_ok_eq hcont] at hres
      rcases bind_eq_ok hres with ⟨origin, horig, hres⟩
      simp only [from_known_64_eq, lift_eq_ok, bind_ok] at hres
      rcases Prod.mk.inj (ok_inj hres) with ⟨hr, hpair⟩
      rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
      cases core.result.Result.Ok.inj hr
      rw [← hvc1_cnt] at hρ_ext
      have horig_core : MemoryOrigin.ofCore origin =
          (if (MapSpec.ofCore s).progReadOnly then MemoryOrigin.mapReadOnly else .mapWritable) := by
        rw [← hbro_eq]
        cases b_ro <;> simp only [Bool.false_eq_true, ↓reduceIte] at horig ⊢ <;>
          cases ok_inj horig <;> rfl
      have hidB : memIdBounded vc1.counter.bv.toNat id = true := by
        rw [hid_eq]; simp only [memIdBounded, decide_eq_true_eq]; omega
      have hren : renameMemId ρ_out id = freshRuntimeId baseAllocIdx := by
        rw [hid_eq]
        simp only [renameMemId, renameNamespaceId, ρ_out, ↓reduceIte, freshRuntimeId, allocU64]
      have hval_sz : (UScalar.cast .U64 s.value_size).bv = (MapSpec.ofCore s).valueSize :=
        uscalar_cast_u64_of_u32_bv s.value_size
      exact ⟨ρ_out, freshRuntimeId baseAllocIdx,
        (if (MapSpec.ofCore s).progReadOnly then MemoryOrigin.mapReadOnly else .mapWritable),
        some (MapSpec.ofCore s).valueSize,
        hvc1_eq, hvc1_cnt, rfl, hρ_ext, hag_sym,
        by simp [evalPtrReturnInner, hlk1],
        by simp [evalPtrReturnInner, hlk2],
        Or.inl ⟨(MapSpec.ofCore s).valueSize, id, UScalar.cast .U64 s.value_size, origin,
          rfl, rfl, hidB, hren, horig_core, hval_sz⟩⟩
  case MemoryParameter mps b1 b2 =>
    have hinnerS : ∃ sizeSpec, innerS = .allocMemory sizeSpec := by
      cases mps <;> cases innerS <;> try exact False.elim hpm
      all_goals exact ⟨_, rfl⟩
    rcases hinnerS with ⟨sizeSpec, rfl⟩
    have hmatch_mps :
        (∃ v, mps = .Value v ∧ sizeSpec = .fixed v.bv) ∨
        (∃ idx, mps = .Reference idx ∧ sizeSpec = .fromArg idx.val ∧
          DataDepsHasReg dPre (idx.val + 1) = true) := by
      cases mps <;> cases sizeSpec <;> try exact False.elim hpm
      · rename_i v _
        exact Or.inl ⟨v, rfl, congrArg _ hpm.symm⟩
      · rename_i idx idx'
        have hidx_eq : idx.val = idx' := hpm
        subst idx'
        exact Or.inr ⟨idx, rfl, rfl, hidx_mem idx.val rfl⟩
    rw [verifier.ComputationContext.resolve_return_value.eq_def] at hres
    dsimp only at hres
    rcases bind_eq_ok hres with ⟨r_sz, hsz_call, hres⟩
    cases r_sz with
    | Err _ => cases (Prod.mk.inj (ok_inj hres)).1
    | Ok sz =>
      dsimp only at hres
      obtain ⟨hsz1, hsz2⟩ := MemoryParameterSize_size_ok_spec hmatch_mps hctx hrel hsz_call
      rcases bind_eq_ok hres with ⟨p_nid, hnid, _⟩
      rcases p_nid with ⟨id, vc1⟩
      rw [bind_pair_ok_eq hnid] at hres
      obtain ⟨hid_eq, hvc1_eq, hvc1_cnt⟩ := VerificationContext_next_id_ok_spec hnid
      simp only [from_known_64_eq, bind_ok] at hres
      rcases Prod.mk.inj (ok_inj hres) with ⟨hr, hpair⟩
      rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
      cases core.result.Result.Ok.inj hr
      rw [← hvc1_cnt] at hρ_ext
      have hidB : memIdBounded vc1.counter.bv.toNat id = true := by
        rw [hid_eq]; simp only [memIdBounded, decide_eq_true_eq]; omega
      have hren : renameMemId ρ_out id = freshRuntimeId baseAllocIdx := by
        rw [hid_eq]
        simp only [renameMemId, renameNamespaceId, ρ_out, ↓reduceIte, freshRuntimeId, allocU64]
      exact ⟨ρ_out, freshRuntimeId baseAllocIdx, .mapWritable, some sz.bv,
        hvc1_eq, hvc1_cnt, rfl, hρ_ext, hag_sym,
        by simp [evalPtrReturnInner, hsz1],
        by simp [evalPtrReturnInner, hsz2],
        Or.inl ⟨sz.bv, id, sz, .MapWritable, rfl, rfl, hidB, hren, rfl, rfl⟩⟩

theorem resolve_return_value_sound
    {env : ExecEnv} {helperIdx numArgs : Nat}
    {l₁ l₂ : HostLayout} {s₁ s₂ : SecretInit}
    {ctx next1 next2 : verifier.ComputationContext}
    {vctx1 vctx2 : verifier.VerificationContext}
    {dPre : verifier.DataDependencies}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {st₁ st₂ : MachineState} {wrldBounds₁ wrldBounds₂ : WorldState}
    {retTy retVal : verifier.Type} {retSpec : HelperReturnSpec}
    (hret : ReturnTypeMatches env retTy retSpec)
    (hret_idx : HelperReturnIndexValid numArgs retSpec)
    (hargs_live : ∀ j < numArgs, DataDepsHasReg dPre (j + 1) = true)
    (hctx : CompCtxValid env vctx1.counter.bv.toNat ctx)
    (hnext1 : CompCtxValid env vctx1.counter.bv.toNat next1)
    (hrel : StateRel2Deps env ctx dPre ρ resActive isNullId st₁ st₂)
    (hρ : RhoValid vctx1.counter.bv.toNat wrldBounds₁.nextAllocId ρ)
    (halloc3 : wrldBounds₁.nextAllocId + 3 < 2 ^ 63)
    (hwb_eq : wrldBounds₁ = wrldBounds₂)
    (_hwb_arr : ∀ id, wrldBounds₁.arrayLen id ≤ 0xFFFF_FFFF#64)
    (hwb_null : ∀ m : MemoryId, m.namespace ≠ .Verification →
      isNullId m = wrldBounds₁.structFieldNull m)
    (hwb_nodup : wrldBounds₁.liveResources.Nodup)
    (hwb_live : ∀ m : MemoryId,
      m ∈ wrldBounds₁.liveResources ↔
        ∃ r ∈ next1.resources.entries.val,
          resActive (renameMemId ρ r) = true ∧ renameMemId ρ r = m)
    (hres : verifier.ComputationContext.resolve_return_value ctx vctx1 retTy next1 =
      ok (.Ok retVal, vctx2, next2)) :
    ∃ (r0Val₁ r0Val₂ : RuntimeVal) (wrldFinal₁ wrldFinal₂ : WorldState)
      (ρ' : Std.U64 → Std.U64) (resActive' isNullId' : MemoryId → Bool),
      evalHelperReturn env l₁ s₁ st₁.regs st₁.world helperIdx retSpec wrldBounds₁ =
        some (r0Val₁, wrldFinal₁) ∧
      evalHelperReturn env l₂ s₂ st₂.regs st₂.world helperIdx retSpec wrldBounds₂ =
        some (r0Val₂, wrldFinal₂) ∧
      wrldFinal₁ = wrldFinal₂ ∧
      wrldFinal₁.arrayLen = wrldBounds₁.arrayLen ∧
      wrldFinal₁.structFieldNull = wrldBounds₁.structFieldNull ∧
      wrldFinal₁.nextAllocId ≤ wrldBounds₁.nextAllocId + 3 ∧
      RhoValid vctx2.counter.bv.toNat wrldFinal₁.nextAllocId ρ' ∧
      (∀ x : Std.U64, x.bv.toNat < vctx1.counter.bv.toNat → ρ x = ρ' x) ∧
      (∀ m : MemoryId, memIdBounded vctx1.counter.bv.toNat m = true →
        resActive (renameMemId ρ m) = resActive' (renameMemId ρ' m)) ∧
      (∀ m : MemoryId, memIdBounded vctx1.counter.bv.toNat m = true →
        isNullId (renameMemId ρ m) = isNullId' (renameMemId ρ' m)) ∧
      (∀ m : MemoryId, m.namespace ≠ .Verification →
        isNullId' m = wrldFinal₁.structFieldNull m) ∧
      wrldFinal₁.liveResources.Nodup ∧
      (∀ m : MemoryId,
        m ∈ wrldFinal₁.liveResources ↔
          ∃ r ∈ next2.resources.entries.val,
            resActive' (renameMemId ρ' r) = true ∧ renameMemId ρ' r = m) ∧
      TypeRel2 env ρ' resActive' isNullId' wrldFinal₁.arrayLen retVal r0Val₁ r0Val₂ := by
  subst hwb_eq
  rcases ReturnTypeMatches_inv hret with
    ⟨rfl, rfl⟩ | ⟨rfl, rfl⟩ | ⟨innerS, rfl, hpm⟩ |
    ⟨relId, innerT, innerS, rfl, rfl, hnoV, hpm⟩ |
    ⟨innerT, innerS, rfl, rfl, hpm⟩ |
    ⟨relId, innerT, innerS, rfl, rfl, hnoV, hpm⟩
  · -- Case 1: `.ScalarValue unknownWrittenScalar64`, `.scalar true`
    rw [verifier.ComputationContext.resolve_return_value.eq_def] at hres
    simp only [Type_clone_spec, bind_ok] at hres
    rcases Prod.mk.inj (ok_inj hres) with ⟨hr, hpair⟩
    rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
    cases core.result.Result.Ok.inj hr
    let v := env.helperScalarOracle wrldBounds₁.stepCount helperIdx
    exact ⟨.scalar v, .scalar v, wrldBounds₁, wrldBounds₁, ρ, resActive, isNullId,
      rfl, rfl, rfl, rfl, rfl, by omega, hρ, (fun _ _ => rfl), (fun _ _ => rfl), (fun _ _ => rfl),
      hwb_null, hwb_nodup, hwb_live,
      TypeRel2_scalar_rel2_of_concretizes (concretizesScalar64Rel2_unknownWritten v)⟩
  · -- Case 2: `.ScalarValue uninitScalar64`, `.scalar false`
    rw [verifier.ComputationContext.resolve_return_value.eq_def] at hres
    simp only [Type_clone_spec, bind_ok] at hres
    rcases Prod.mk.inj (ok_inj hres) with ⟨hr, hpair⟩
    rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
    cases core.result.Result.Ok.inj hr
    exact ⟨_, _, wrldBounds₁, wrldBounds₁, ρ, resActive, isNullId,
      rfl, rfl, rfl, rfl, rfl, by omega, hρ, (fun _ _ => rfl), (fun _ _ => rfl), (fun _ _ => rfl),
      hwb_null, hwb_nodup, hwb_live, by simp [TypeRel2]⟩
  · -- Case 3: `.pointer false none innerS`
    have hidx_map : ∀ idx, innerS = .mapValue idx → DataDepsHasReg dPre (idx + 1) = true := by
      intro idx heq; subst heq; exact hargs_live idx hret_idx
    have hidx_mem : ∀ idx, innerS = .allocMemory (.fromArg idx) → DataDepsHasReg dPre (idx + 1) = true := by
      intro idx heq; subst heq; exact hargs_live idx hret_idx
    obtain ⟨ρ_out, ptrId, orig, bufSz?, _, _, hnext2_eq, hρ_out, hag_out, hev1, hev2, hshape⟩ :=
      resolve_ptr_return_sound hpm hidx_map hidx_mem hctx hrel hρ (Nat.le_refl _)
        (by omega) hres
    rw [hnext2_eq]
    let wrldF := { wrldBounds₁ with nextAllocId := wrldBounds₁.nextAllocId + 1 }
    have hlive_out : ∀ m : MemoryId,
        m ∈ wrldF.liveResources ↔
          ∃ r ∈ next1.resources.entries.val,
            resActive (renameMemId ρ_out r) = true ∧ renameMemId ρ_out r = m := by
      intro m
      dsimp only [wrldF]
      rw [hwb_live m]
      constructor
      · rintro ⟨r, hr_in, hact, rfl⟩
        have hrB := hnext1.2.2.2.2 r hr_in
        rw [renameMemId_congr hag_out hrB] at hact ⊢
        exact ⟨r, hr_in, hact, rfl⟩
      · rintro ⟨r, hr_in, hact, rfl⟩
        have hrB := hnext1.2.2.2.2 r hr_in
        rw [← renameMemId_congr hag_out hrB] at hact ⊢
        exact ⟨r, hr_in, hact, rfl⟩
    rcases hshape with
      ⟨sz, rawId, rawSz, rawOrig, rfl, rfl, _, hren, horig, hsz⟩ |
      ⟨rfl, rfl, rawId, sd, rfl, _, hren, hsd⟩
    · refine ⟨.memPtr ptrId orig none sz 0#64, .memPtr ptrId orig none sz 0#64,
        wrldF, wrldF, ρ_out, resActive, isNullId,
        ?_, ?_, rfl, rfl, rfl, by dsimp [wrldF]; omega, hρ_out, hag_out,
        (fun m hm => by rw [renameMemId_congr hag_out hm]),
        (fun m hm => by rw [renameMemId_congr hag_out hm]),
        hwb_null, hwb_nodup, hlive_out,
        ⟨ptrOffsetValid_knownZero, 0#64, concretizesScalar64Rel2_known 0#u64,
          by rw [hren, horig, hsz], by rw [hren, horig, hsz]⟩⟩
      · simp only [evalHelperReturn_pointer_eq, Option.isSome, Bool.false_eq_true, ↓reduceIte, Nat.add_zero]
        rw [hev1]; simp [wrldF]
      · simp only [evalHelperReturn_pointer_eq, Option.isSome, Bool.false_eq_true, ↓reduceIte, Nat.add_zero]
        rw [hev2]; simp [wrldF]
    · refine ⟨.structPtr ptrId none 0#64, .structPtr ptrId none 0#64,
        wrldF, wrldF, ρ_out, resActive, isNullId,
        ?_, ?_, rfl, rfl, rfl, by dsimp [wrldF]; omega, hρ_out, hag_out,
        (fun m hm => by rw [renameMemId_congr hag_out hm]),
        (fun m hm => by rw [renameMemId_congr hag_out hm]),
        hwb_null, hwb_nodup, hlive_out,
        ⟨ptrOffsetValid_knownZero, hsd, 0#64, concretizesScalar64Rel2_known 0#u64,
          by rw [hren], by rw [hren]⟩⟩
      · simp only [evalHelperReturn_pointer_eq, Option.isSome, Bool.false_eq_true, ↓reduceIte, Nat.add_zero]
        rw [hev1]; simp [wrldF]
      · simp only [evalHelperReturn_pointer_eq, Option.isSome, Bool.false_eq_true, ↓reduceIte, Nat.add_zero]
        rw [hev2]; simp [wrldF]
  · -- Case 4: `.pointer false (some relId) innerS`
    have hidx_map : ∀ idx, innerS = .mapValue idx → DataDepsHasReg dPre (idx + 1) = true := by
      intro idx heq; subst heq; exact hargs_live idx hret_idx
    have hidx_mem : ∀ idx, innerS = .allocMemory (.fromArg idx) → DataDepsHasReg dPre (idx + 1) = true := by
      intro idx heq; subst heq; exact hargs_live idx hret_idx
    rw [verifier.ComputationContext.resolve_return_value.eq_def] at hres
    dsimp only at hres
    rcases bind_eq_ok hres with ⟨p_nid, hnid, _⟩
    rcases p_nid with ⟨mi, vc1⟩
    rw [bind_pair_ok_eq hnid] at hres
    obtain ⟨hmi_eq, hvc1_eq, hvc1_cnt⟩ := VerificationContext_next_id_ok_spec hnid
    rw [bind_ok_eq (MemoryId_clone_spec relId),
        bind_ok_eq (MemoryId_prepended_spec mi relId),
        bind_ok_eq (MemoryId_clone_spec (memoryIdPrepend mi relId))] at hres
    rcases bind_eq_ok hres with ⟨⟨_, is⟩, hins, hres⟩
    rcases bind_eq_ok hres with ⟨⟨r_in, vc2, next_out⟩, hres_in, hres⟩
    cases r_in with
    | Err _ => cases (Prod.mk.inj (ok_inj hres)).1
    | Ok t_ptr =>
      rcases Prod.mk.inj (ok_inj hres) with ⟨hr, hpair⟩
      rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
      cases core.result.Result.Ok.inj hr
      let N := wrldBounds₁.nextAllocId
      let relU64 : Std.U64 := ⟨BitVec.ofNat 64 N⟩
      have hrelU64_nat : relU64.bv.toNat = N := by
        dsimp only [relU64]; rw [BitVec.toNat_ofNat, Nat.mod_eq_of_lt (by omega)]
      obtain ⟨hρ1, hag1, _⟩ :=
        RhoValid_extend (rt' := N + 1) (oldId := vctx1.counter) (newId := relU64)
          hρ rfl (by omega) (by omega) (by omega)
      rw [← hvc1_cnt] at hρ1
      let ρ1 := fun x => if x = vctx1.counter then relU64 else ρ x
      have hag1_sym : ∀ x : Std.U64, x.bv.toNat < vctx1.counter.bv.toNat → ρ x = ρ1 x :=
        fun x hx => (hag1 x hx).symm
      have hmiB : memIdBounded vc1.counter.bv.toNat mi = true := by
        rw [hmi_eq]; simp only [memIdBounded, decide_eq_true_eq]; omega
      have hrelB1 : memIdBounded vc1.counter.bv.toNat (memoryIdPrepend mi relId) = true :=
        memIdBounded_prepend hmiB (memIdBounded_of_noVerif _ hnoV)
      obtain ⟨_, his_iff, _⟩ :=
        IdSet_insert_ok_spec (IdSetValid_mono (by omega) hnext1.2.2.2) hrelB1 hins
      obtain ⟨ρ_out, ptrId, orig, bufSz?, _, _, rfl, hρ_out, hag_out, hev1, hev2, hshape⟩ :=
        resolve_ptr_return_sound (baseAllocIdx := N + 1) hpm hidx_map hidx_mem hctx hrel hρ1
          (Nat.le_refl _) (by omega) hres_in
      have hag_all : ∀ x : Std.U64, x.bv.toNat < vctx1.counter.bv.toNat → ρ x = ρ_out x :=
        fun x hx => (hag1_sym x hx).trans (hag_out x (by omega))
      let rid := memoryIdPrepend (freshRuntimeId N) relId
      have hrid_def : rid = verifier.MemoryId.mk .Verification relU64 (some relId) := rfl
      have hren_rel : renameMemId ρ_out (memoryIdPrepend mi relId) = rid := by
        rw [← renameMemId_congr hag_out hrelB1, renameMemId_prepend,
            renameMemId_noVerif_id ρ1 hnoV, hmi_eq]
        simp only [renameMemId, renameNamespaceId, ↓reduceIte, rid, freshRuntimeId, relU64]
      have hrid_ne_old : ∀ m : MemoryId, memIdBounded vctx1.counter.bv.toNat m = true →
          rid ≠ renameMemId ρ m :=
        fun m hm => freshMemId_root_ne_renamed hρ (Nat.le_refl _) (by omega) hm
      let resActive' : MemoryId → Bool := fun m => if m = rid then true else resActive m
      have hres_agree : ∀ m : MemoryId, memIdBounded vctx1.counter.bv.toNat m = true →
          resActive (renameMemId ρ m) = resActive' (renameMemId ρ_out m) := by
        intro m hm
        rw [← renameMemId_congr hag_all hm]
        have hne : renameMemId ρ m ≠ rid := (hrid_ne_old m hm).symm
        simp [resActive', hne]
      have hrid_notin : rid ∉ wrldBounds₁.liveResources := by
        intro hmem
        rcases (hwb_live rid).mp hmem with ⟨r, hr_in, _, heq⟩
        exact hrid_ne_old r (hnext1.2.2.2.2 r hr_in) heq.symm
      let wrldF := { wrldBounds₁ with
        liveResources := rid :: wrldBounds₁.liveResources
        nextAllocId := N + 2 }
      have hnodupF : wrldF.liveResources.Nodup :=
        List.nodup_cons.mpr ⟨hrid_notin, hwb_nodup⟩
      have hliveF : ∀ m : MemoryId,
          m ∈ wrldF.liveResources ↔
            ∃ r ∈ is.entries.val,
              resActive' (renameMemId ρ_out r) = true ∧ renameMemId ρ_out r = m := by
        intro m
        dsimp only [wrldF]
        rw [List.mem_cons, hwb_live m]
        constructor
        · rintro (rfl | ⟨r, hr_in, hact, rfl⟩)
          · exact ⟨memoryIdPrepend mi relId, (his_iff _).mpr (Or.inl rfl),
              by simp [hren_rel, resActive'], hren_rel⟩
          · have hrB := hnext1.2.2.2.2 r hr_in
            refine ⟨r, (his_iff _).mpr (Or.inr hr_in), ?_, (renameMemId_congr hag_all hrB).symm⟩
            rwa [← hres_agree r hrB]
        · rintro ⟨r, hr_is, hact, rfl⟩
          rcases (his_iff r).mp hr_is with rfl | hr_in
          · exact Or.inl hren_rel
          · have hrB := hnext1.2.2.2.2 r hr_in
            rw [← hres_agree r hrB] at hact
            exact Or.inr ⟨r, hr_in, hact, renameMemId_congr hag_all hrB⟩
      rcases hshape with
        ⟨sz, rawId, rawSz, rawOrig, rfl, rfl, _, hren, horig, hsz⟩ |
        ⟨rfl, rfl, rawId, sd, rfl, _, hren, hsd⟩
      · refine ⟨.memPtr ptrId orig (some rid) sz 0#64, .memPtr ptrId orig (some rid) sz 0#64,
          wrldF, wrldF, ρ_out, resActive', isNullId,
          ?_, ?_, rfl, rfl, rfl, by dsimp [wrldF, N]; omega, hρ_out, hag_all,
          hres_agree, (fun m hm => by rw [renameMemId_congr hag_all hm]),
          hwb_null, hnodupF, hliveF,
          ⟨by simp [hren_rel, resActive'], ptrOffsetValid_knownZero, 0#64,
            concretizesScalar64Rel2_known 0#u64,
            by rw [hren, hren_rel, horig, hsz], by rw [hren, hren_rel, horig, hsz]⟩⟩
        · dsimp only [N] at hev1
          simp only [evalHelperReturn_pointer_eq, Option.isSome, Bool.false_eq_true, ↓reduceIte, Nat.add_zero]
          rw [hev1]; simp [wrldF, rid, N]
        · dsimp only [N] at hev2
          simp only [evalHelperReturn_pointer_eq, Option.isSome, Bool.false_eq_true, ↓reduceIte, Nat.add_zero]
          rw [hev2]; simp [wrldF, rid, N]
      · refine ⟨.structPtr ptrId (some rid) 0#64, .structPtr ptrId (some rid) 0#64,
          wrldF, wrldF, ρ_out, resActive', isNullId,
          ?_, ?_, rfl, rfl, rfl, by dsimp [wrldF, N]; omega, hρ_out, hag_all,
          hres_agree, (fun m hm => by rw [renameMemId_congr hag_all hm]),
          hwb_null, hnodupF, hliveF,
          ⟨by simp [hren_rel, resActive'], ptrOffsetValid_knownZero, hsd, 0#64,
            concretizesScalar64Rel2_known 0#u64,
            by rw [hren, hren_rel], by rw [hren, hren_rel]⟩⟩
        · dsimp only [N] at hev1
          simp only [evalHelperReturn_pointer_eq, Option.isSome, Bool.false_eq_true, ↓reduceIte, Nat.add_zero]
          rw [hev1]; simp [wrldF, rid, N]
        · dsimp only [N] at hev2
          simp only [evalHelperReturn_pointer_eq, Option.isSome, Bool.false_eq_true, ↓reduceIte, Nat.add_zero]
          rw [hev2]; simp [wrldF, rid, N]
  · -- Case 5: `.pointer true none innerS`
    have hidx_map : ∀ idx, innerS = .mapValue idx → DataDepsHasReg dPre (idx + 1) = true := by
      intro idx heq; subst heq; exact hargs_live idx hret_idx
    have hidx_mem : ∀ idx, innerS = .allocMemory (.fromArg idx) → DataDepsHasReg dPre (idx + 1) = true := by
      intro idx heq; subst heq; exact hargs_live idx hret_idx
    rw [verifier.ComputationContext.resolve_return_value.eq_def] at hres
    dsimp only at hres
    rcases bind_eq_ok hres with ⟨⟨nullId, vc1⟩, hnid, hres⟩
    obtain ⟨hnull_eq, hvc1_eq, hvc1_cnt⟩ := VerificationContext_next_id_ok_spec hnid
    rcases bind_eq_ok hres with ⟨⟨r_in, vc2, next_out⟩, hres_in, hres⟩
    cases r_in with
    | Err _ => cases (Prod.mk.inj (ok_inj hres)).1
    | Ok t_ptr =>
      rcases Prod.mk.inj (ok_inj hres) with ⟨hr, hpair⟩
      rcases Prod.mk.inj hpair with ⟨rfl, hn2_eq⟩
      subst next2
      cases core.result.Result.Ok.inj hr
      let N := wrldBounds₁.nextAllocId
      let nullU64 : Std.U64 := ⟨BitVec.ofNat 64 N⟩
      have hnullU64_nat : nullU64.bv.toNat = N := by
        dsimp only [nullU64]; rw [BitVec.toNat_ofNat, Nat.mod_eq_of_lt (by omega)]
      obtain ⟨hρ1, hag1, _⟩ :=
        RhoValid_extend (rt' := N + 1) (oldId := vctx1.counter) (newId := nullU64)
          hρ rfl (by omega) (by omega) (by omega)
      rw [← hvc1_cnt] at hρ1
      let ρ1 := fun x => if x = vctx1.counter then nullU64 else ρ x
      have hag1_sym : ∀ x : Std.U64, x.bv.toNat < vctx1.counter.bv.toNat → ρ x = ρ1 x :=
        fun x hx => (hag1 x hx).symm
      have hnullB1 : memIdBounded vc1.counter.bv.toNat nullId = true := by
        rw [hnull_eq]; simp only [memIdBounded, decide_eq_true_eq]; omega
      obtain ⟨ρ_out, ptrId, orig, bufSz?, _, _, hn_out_eq, hρ_out, hag_out, hev1, hev2, hshape⟩ :=
        resolve_ptr_return_sound (baseAllocIdx := N + 1) hpm hidx_map hidx_mem hctx hrel hρ1
          (Nat.le_refl _) (by omega) hres_in
      subst next_out
      have hag_all : ∀ x : Std.U64, x.bv.toNat < vctx1.counter.bv.toNat → ρ x = ρ_out x :=
        fun x hx => (hag1_sym x hx).trans (hag_out x (by omega))
      let nullRt := freshRuntimeId N
      have hren_null : renameMemId ρ_out nullId = nullRt := by
        rw [← renameMemId_congr hag_out hnullB1, hnull_eq]
        simp only [renameMemId, renameNamespaceId, ↓reduceIte, nullRt, freshRuntimeId, nullU64]
      have hnull_ne_old : ∀ m : MemoryId, memIdBounded vctx1.counter.bv.toNat m = true →
          nullRt ≠ renameMemId ρ m :=
        fun m hm => freshMemId_root_ne_renamed hρ (Nat.le_refl _) (by omega) hm
      let isNull := env.helperNullOracle wrldBounds₁.stepCount helperIdx
      let isNullId' : MemoryId → Bool := fun m => if m = nullRt then isNull else isNullId m
      have hnull_agree : ∀ m : MemoryId, memIdBounded vctx1.counter.bv.toNat m = true →
          isNullId (renameMemId ρ m) = isNullId' (renameMemId ρ_out m) := by
        intro m hm
        rw [← renameMemId_congr hag_all hm]
        have hne : renameMemId ρ m ≠ nullRt := (hnull_ne_old m hm).symm
        simp [isNullId', hne]
      have hnull_nonverif : ∀ m : MemoryId, m.namespace ≠ .Verification →
          isNullId' m = wrldBounds₁.structFieldNull m := by
        intro m hns
        have hne : m ≠ nullRt := fun heq => hns (heq ▸ rfl)
        simp [isNullId', hne, hwb_null m hns]
      let wrldF := { wrldBounds₁ with nextAllocId := N + 2 }
      have hliveF : ∀ m : MemoryId,
          m ∈ wrldF.liveResources ↔
            ∃ r ∈ next1.resources.entries.val,
              resActive (renameMemId ρ_out r) = true ∧ renameMemId ρ_out r = m := by
        intro m; dsimp only [wrldF]; rw [hwb_live m]
        constructor
        · rintro ⟨r, hr_in, hact, rfl⟩
          have hrB := hnext1.2.2.2.2 r hr_in
          rw [renameMemId_congr hag_all hrB] at hact ⊢
          exact ⟨r, hr_in, hact, rfl⟩
        · rintro ⟨r, hr_in, hact, rfl⟩
          have hrB := hnext1.2.2.2.2 r hr_in
          rw [← renameMemId_congr hag_all hrB] at hact ⊢
          exact ⟨r, hr_in, hact, rfl⟩
      rcases hshape with
        ⟨sz, rawId, rawSz, rawOrig, rfl, rfl, _, hren, horig, hsz⟩ |
        ⟨rfl, rfl, rawId, sd, rfl, _, hren, hsd⟩
      · cases hisNull : isNull
        · refine ⟨.memPtr ptrId orig none sz 0#64, .memPtr ptrId orig none sz 0#64,
            wrldF, wrldF, ρ_out, resActive, isNullId',
            ?_, ?_, rfl, rfl, rfl, by dsimp [wrldF, N]; omega, hρ_out, hag_all,
            (fun m hm => by rw [renameMemId_congr hag_all hm]), hnull_agree,
            hnull_nonverif, hwb_nodup, hliveF, ?_⟩
          · dsimp only [isNull, N] at hisNull hev1
            simp only [evalHelperReturn_pointer_eq, Option.isSome, Bool.false_eq_true, ↓reduceIte, Nat.add_zero]
            rw [hev1]; simp [hisNull, wrldF, N]
          · dsimp only [isNull, N] at hisNull hev2
            simp only [evalHelperReturn_pointer_eq, Option.isSome, Bool.false_eq_true, ↓reduceIte, Nat.add_zero]
            rw [hev2]; simp [hisNull, wrldF, N]
          · simp only [TypeRel2, hren_null, isNullId', ↓reduceIte, hisNull, Bool.false_eq_true]
            exact ⟨ptrOffsetValid_knownZero, 0#64, concretizesScalar64Rel2_known 0#u64,
              by rw [hren, horig, hsz], by rw [hren, horig, hsz]⟩
        · refine ⟨.scalar 0#64, .scalar 0#64, wrldF, wrldF, ρ_out, resActive, isNullId',
            ?_, ?_, rfl, rfl, rfl, by dsimp [wrldF, N]; omega, hρ_out, hag_all,
            (fun m hm => by rw [renameMemId_congr hag_all hm]), hnull_agree,
            hnull_nonverif, hwb_nodup, hliveF, ?_⟩
          · dsimp only [isNull, N] at hisNull hev1
            simp only [evalHelperReturn_pointer_eq, Option.isSome, Bool.false_eq_true, ↓reduceIte, Nat.add_zero]
            rw [hev1]; simp [hisNull, wrldF, N]
          · dsimp only [isNull, N] at hisNull hev2
            simp only [evalHelperReturn_pointer_eq, Option.isSome, Bool.false_eq_true, ↓reduceIte, Nat.add_zero]
            rw [hev2]; simp [hisNull, wrldF, N]
          · simp [TypeRel2, hren_null, isNullId', hisNull, nullInnerInactive]
      · cases hisNull : isNull
        · refine ⟨.structPtr ptrId none 0#64, .structPtr ptrId none 0#64,
            wrldF, wrldF, ρ_out, resActive, isNullId',
            ?_, ?_, rfl, rfl, rfl, by dsimp [wrldF, N]; omega, hρ_out, hag_all,
            (fun m hm => by rw [renameMemId_congr hag_all hm]), hnull_agree,
            hnull_nonverif, hwb_nodup, hliveF, ?_⟩
          · dsimp only [isNull, N] at hisNull hev1
            simp only [evalHelperReturn_pointer_eq, Option.isSome, Bool.false_eq_true, ↓reduceIte, Nat.add_zero]
            rw [hev1]; simp [hisNull, wrldF, N]
          · dsimp only [isNull, N] at hisNull hev2
            simp only [evalHelperReturn_pointer_eq, Option.isSome, Bool.false_eq_true, ↓reduceIte, Nat.add_zero]
            rw [hev2]; simp [hisNull, wrldF, N]
          · simp only [TypeRel2, hren_null, isNullId', ↓reduceIte, hisNull, Bool.false_eq_true]
            exact ⟨ptrOffsetValid_knownZero, hsd, 0#64, concretizesScalar64Rel2_known 0#u64,
              by rw [hren], by rw [hren]⟩
        · refine ⟨.scalar 0#64, .scalar 0#64, wrldF, wrldF, ρ_out, resActive, isNullId',
            ?_, ?_, rfl, rfl, rfl, by dsimp [wrldF, N]; omega, hρ_out, hag_all,
            (fun m hm => by rw [renameMemId_congr hag_all hm]), hnull_agree,
            hnull_nonverif, hwb_nodup, hliveF, ?_⟩
          · dsimp only [isNull, N] at hisNull hev1
            simp only [evalHelperReturn_pointer_eq, Option.isSome, Bool.false_eq_true, ↓reduceIte, Nat.add_zero]
            rw [hev1]; simp [hisNull, wrldF, N]
          · dsimp only [isNull, N] at hisNull hev2
            simp only [evalHelperReturn_pointer_eq, Option.isSome, Bool.false_eq_true, ↓reduceIte, Nat.add_zero]
            rw [hev2]; simp [hisNull, wrldF, N]
          · simp [TypeRel2, hren_null, isNullId', hisNull, nullInnerInactive]
  · -- Case 6: `.pointer true (some relId) innerS`
    have hidx_map : ∀ idx, innerS = .mapValue idx → DataDepsHasReg dPre (idx + 1) = true := by
      intro idx heq; subst heq; exact hargs_live idx hret_idx
    have hidx_mem : ∀ idx, innerS = .allocMemory (.fromArg idx) → DataDepsHasReg dPre (idx + 1) = true := by
      intro idx heq; subst heq; exact hargs_live idx hret_idx
    rw [verifier.ComputationContext.resolve_return_value.eq_def] at hres
    dsimp only at hres
    rcases bind_eq_ok hres with ⟨⟨nullId, vc1⟩, hnid0, hres⟩
    obtain ⟨hnull_eq, hvc1_eq, hvc1_cnt⟩ := VerificationContext_next_id_ok_spec hnid0
    rcases bind_eq_ok hres with ⟨⟨r_rel, vc3, next_out⟩, hres_rel, hres⟩
    cases r_rel with
    | Err _ => cases (Prod.mk.inj (ok_inj hres)).1
    | Ok t_rel =>
      rcases Prod.mk.inj (ok_inj hres) with ⟨hr, hpair⟩
      rcases Prod.mk.inj hpair with ⟨rfl, rfl⟩
      cases core.result.Result.Ok.inj hr
      rw [verifier.ComputationContext.resolve_return_value.eq_def] at hres_rel
      dsimp only at hres_rel
      rcases bind_eq_ok hres_rel with ⟨p_nid1, hnid1, _⟩
      rcases p_nid1 with ⟨mi, vc2⟩
      rw [bind_pair_ok_eq hnid1] at hres_rel
      obtain ⟨hmi_eq, hvc2_eq, hvc2_cnt⟩ := VerificationContext_next_id_ok_spec hnid1
      rw [bind_ok_eq (MemoryId_clone_spec relId),
          bind_ok_eq (MemoryId_prepended_spec mi relId),
          bind_ok_eq (MemoryId_clone_spec (memoryIdPrepend mi relId))] at hres_rel
      rcases bind_eq_ok hres_rel with ⟨⟨_, is⟩, hins, hres_rel⟩
      rcases bind_eq_ok hres_rel with ⟨⟨r_in, vc3', next_in⟩, hres_in, hres_rel⟩
      cases r_in with
      | Err _ => cases (Prod.mk.inj (ok_inj hres_rel)).1
      | Ok t_ptr =>
        rcases Prod.mk.inj (ok_inj hres_rel) with ⟨hr_rel, hpair_rel⟩
        rcases Prod.mk.inj hpair_rel with ⟨rfl, rfl⟩
        cases core.result.Result.Ok.inj hr_rel
        let N := wrldBounds₁.nextAllocId
        let nullU64 : Std.U64 := ⟨BitVec.ofNat 64 N⟩
        have hnullU64_nat : nullU64.bv.toNat = N := by
          dsimp only [nullU64]; rw [BitVec.toNat_ofNat, Nat.mod_eq_of_lt (by omega)]
        obtain ⟨hρ1, hag1, _⟩ :=
          RhoValid_extend (rt' := N + 1) (oldId := vctx1.counter) (newId := nullU64)
            hρ rfl (by omega) (by omega) (by omega)
        rw [← hvc1_cnt] at hρ1
        let ρ1 := fun x => if x = vctx1.counter then nullU64 else ρ x
        have hag1_sym : ∀ x : Std.U64, x.bv.toNat < vctx1.counter.bv.toNat → ρ x = ρ1 x :=
          fun x hx => (hag1 x hx).symm
        let relU64 : Std.U64 := ⟨BitVec.ofNat 64 (N + 1)⟩
        have hrelU64_nat : relU64.bv.toNat = N + 1 := by
          dsimp only [relU64]; rw [BitVec.toNat_ofNat, Nat.mod_eq_of_lt (by omega)]
        obtain ⟨hρ2, hag2, _⟩ :=
          RhoValid_extend (rt' := N + 2) (oldId := vc1.counter) (newId := relU64)
            hρ1 rfl (by omega) (by omega) (by omega)
        rw [← hvc2_cnt] at hρ2
        let ρ2 := fun x => if x = vc1.counter then relU64 else ρ1 x
        have hag2_sym : ∀ x : Std.U64, x.bv.toNat < vc1.counter.bv.toNat → ρ1 x = ρ2 x :=
          fun x hx => (hag2 x hx).symm
        have hnullB1 : memIdBounded vc1.counter.bv.toNat nullId = true := by
          rw [hnull_eq]; simp only [memIdBounded, decide_eq_true_eq]; omega
        have hmiB2 : memIdBounded vc2.counter.bv.toNat mi = true := by
          rw [hmi_eq]; simp only [memIdBounded, decide_eq_true_eq]; omega
        have hrelB2 : memIdBounded vc2.counter.bv.toNat (memoryIdPrepend mi relId) = true :=
          memIdBounded_prepend hmiB2 (memIdBounded_of_noVerif _ hnoV)
        obtain ⟨_, his_iff, _⟩ :=
          IdSet_insert_ok_spec (IdSetValid_mono (by omega) hnext1.2.2.2) hrelB2 hins
        obtain ⟨ρ_out, ptrId, orig, bufSz?, _, _, rfl, hρ_out, hag_out, hev1, hev2, hshape⟩ :=
          resolve_ptr_return_sound (baseAllocIdx := N + 2) hpm hidx_map hidx_mem hctx hrel hρ2
            (Nat.le_refl _) (by omega) hres_in
        have hag_from_1 : ∀ x : Std.U64, x.bv.toNat < vc1.counter.bv.toNat → ρ1 x = ρ_out x :=
          fun x hx => (hag2_sym x hx).trans (hag_out x (by omega))
        have hag_all : ∀ x : Std.U64, x.bv.toNat < vctx1.counter.bv.toNat → ρ x = ρ_out x :=
          fun x hx => (hag1_sym x hx).trans (hag_from_1 x (by omega))
        let nullRt := freshRuntimeId N
        have hren_null : renameMemId ρ_out nullId = nullRt := by
          rw [← renameMemId_congr hag_from_1 hnullB1, hnull_eq]
          dsimp only [ρ1, nullRt, freshRuntimeId, nullU64]
          simp only [renameMemId, renameNamespaceId, ↓reduceIte]
        let rid := memoryIdPrepend (freshRuntimeId (N + 1)) relId
        have hren_rel : renameMemId ρ_out (memoryIdPrepend mi relId) = rid := by
          rw [← renameMemId_congr hag_out hrelB2, renameMemId_prepend,
              renameMemId_noVerif_id ρ2 hnoV, hmi_eq]
          simp only [renameMemId, renameNamespaceId, ↓reduceIte, rid, freshRuntimeId, relU64]
        have hnull_ne_old : ∀ m : MemoryId, memIdBounded vctx1.counter.bv.toNat m = true →
            nullRt ≠ renameMemId ρ m :=
          fun m hm => freshMemId_root_ne_renamed hρ (Nat.le_refl _) (by omega) hm
        have hrid_ne_old : ∀ m : MemoryId, memIdBounded vctx1.counter.bv.toNat m = true →
            rid ≠ renameMemId ρ m :=
          fun m hm => freshMemId_root_ne_renamed hρ (by omega) (by omega) hm
        let isNull := env.helperNullOracle wrldBounds₁.stepCount helperIdx
        let isNullId' : MemoryId → Bool := fun m => if m = nullRt then isNull else isNullId m
        let resActive' : MemoryId → Bool := fun m => if m = rid then !isNull else resActive m
        have hnull_agree : ∀ m : MemoryId, memIdBounded vctx1.counter.bv.toNat m = true →
            isNullId (renameMemId ρ m) = isNullId' (renameMemId ρ_out m) := by
          intro m hm
          rw [← renameMemId_congr hag_all hm]
          have hne : renameMemId ρ m ≠ nullRt := (hnull_ne_old m hm).symm
          simp [isNullId', hne]
        have hres_agree : ∀ m : MemoryId, memIdBounded vctx1.counter.bv.toNat m = true →
            resActive (renameMemId ρ m) = resActive' (renameMemId ρ_out m) := by
          intro m hm
          rw [← renameMemId_congr hag_all hm]
          have hne : renameMemId ρ m ≠ rid := (hrid_ne_old m hm).symm
          simp [resActive', hne]
        have hnull_nonverif : ∀ m : MemoryId, m.namespace ≠ .Verification →
            isNullId' m = wrldBounds₁.structFieldNull m := by
          intro m hns
          have hne : m ≠ nullRt := fun heq => hns (heq ▸ rfl)
          simp [isNullId', hne, hwb_null m hns]
        have hrid_notin : rid ∉ wrldBounds₁.liveResources := by
          intro hmem
          rcases (hwb_live rid).mp hmem with ⟨r, hr_in, _, heq⟩
          exact hrid_ne_old r (hnext1.2.2.2.2 r hr_in) heq.symm
        cases hisNull : isNull
        · -- `isNull = false`: resource IS allocated and live
          let wrldF := { wrldBounds₁ with
            liveResources := rid :: wrldBounds₁.liveResources
            nextAllocId := N + 3 }
          have hnodupF : wrldF.liveResources.Nodup :=
            List.nodup_cons.mpr ⟨hrid_notin, hwb_nodup⟩
          have hliveF : ∀ m : MemoryId,
              m ∈ wrldF.liveResources ↔
                ∃ r ∈ is.entries.val,
                  resActive' (renameMemId ρ_out r) = true ∧ renameMemId ρ_out r = m := by
            intro m; dsimp only [wrldF]; rw [List.mem_cons, hwb_live m]
            constructor
            · rintro (rfl | ⟨r, hr_in, hact, rfl⟩)
              · exact ⟨memoryIdPrepend mi relId, (his_iff _).mpr (Or.inl rfl),
                  by simp [hren_rel, resActive', hisNull], hren_rel⟩
              · have hrB := hnext1.2.2.2.2 r hr_in
                refine ⟨r, (his_iff _).mpr (Or.inr hr_in), ?_, (renameMemId_congr hag_all hrB).symm⟩
                rwa [← hres_agree r hrB]
            · rintro ⟨r, hr_is, hact, rfl⟩
              rcases (his_iff r).mp hr_is with rfl | hr_in
              · exact Or.inl hren_rel
              · have hrB := hnext1.2.2.2.2 r hr_in
                rw [← hres_agree r hrB] at hact
                exact Or.inr ⟨r, hr_in, hact, renameMemId_congr hag_all hrB⟩
          rcases hshape with
            ⟨sz, rawId, rawSz, rawOrig, rfl, rfl, _, hren, horig, hsz⟩ |
            ⟨rfl, rfl, rawId, sd, rfl, _, hren, hsd⟩
          · refine ⟨.memPtr ptrId orig (some rid) sz 0#64, .memPtr ptrId orig (some rid) sz 0#64,
              wrldF, wrldF, ρ_out, resActive', isNullId',
              ?_, ?_, rfl, rfl, rfl, by dsimp [wrldF, N]; omega, hρ_out, hag_all,
              hres_agree, hnull_agree, hnull_nonverif, hnodupF, hliveF, ?_⟩
            · dsimp only [isNull, N] at hisNull hev1
              simp only [evalHelperReturn_pointer_eq, Option.isSome, ↓reduceIte]
              rw [Nat.add_assoc, show 1 + 1 = 2 from rfl, hev1]
              simp [hisNull, wrldF, rid, N]
            · dsimp only [isNull, N] at hisNull hev2
              simp only [evalHelperReturn_pointer_eq, Option.isSome, ↓reduceIte]
              rw [Nat.add_assoc, show 1 + 1 = 2 from rfl, hev2]
              simp [hisNull, wrldF, rid, N]
            · simp only [TypeRel2, hren_null, isNullId', ↓reduceIte, hisNull, Bool.false_eq_true]
              exact ⟨by simp [hren_rel, resActive', hisNull], ptrOffsetValid_knownZero, 0#64,
                concretizesScalar64Rel2_known 0#u64,
                by rw [hren, hren_rel, horig, hsz], by rw [hren, hren_rel, horig, hsz]⟩
          · refine ⟨.structPtr ptrId (some rid) 0#64, .structPtr ptrId (some rid) 0#64,
              wrldF, wrldF, ρ_out, resActive', isNullId',
              ?_, ?_, rfl, rfl, rfl, by dsimp [wrldF, N]; omega, hρ_out, hag_all,
              hres_agree, hnull_agree, hnull_nonverif, hnodupF, hliveF, ?_⟩
            · dsimp only [isNull, N] at hisNull hev1
              simp only [evalHelperReturn_pointer_eq, Option.isSome, ↓reduceIte]
              rw [Nat.add_assoc, show 1 + 1 = 2 from rfl, hev1]
              simp [hisNull, wrldF, rid, N]
            · dsimp only [isNull, N] at hisNull hev2
              simp only [evalHelperReturn_pointer_eq, Option.isSome, ↓reduceIte]
              rw [Nat.add_assoc, show 1 + 1 = 2 from rfl, hev2]
              simp [hisNull, wrldF, rid, N]
            · simp only [TypeRel2, hren_null, isNullId', ↓reduceIte, hisNull, Bool.false_eq_true]
              exact ⟨by simp [hren_rel, resActive', hisNull], ptrOffsetValid_knownZero, hsd, 0#64,
                concretizesScalar64Rel2_known 0#u64,
                by rw [hren, hren_rel], by rw [hren, hren_rel]⟩
        · -- `isNull = true`: return is null scalar 0, ghost resource `rid` is inactive!
          let wrldF := { wrldBounds₁ with nextAllocId := N + 3 }
          have hliveF : ∀ m : MemoryId,
              m ∈ wrldF.liveResources ↔
                ∃ r ∈ is.entries.val,
                  resActive' (renameMemId ρ_out r) = true ∧ renameMemId ρ_out r = m := by
            intro m; dsimp only [wrldF]; rw [hwb_live m]
            constructor
            · rintro ⟨r, hr_in, hact, rfl⟩
              have hrB := hnext1.2.2.2.2 r hr_in
              refine ⟨r, (his_iff _).mpr (Or.inr hr_in), ?_, (renameMemId_congr hag_all hrB).symm⟩
              rwa [← hres_agree r hrB]
            · rintro ⟨r, hr_is, hact, rfl⟩
              rcases (his_iff r).mp hr_is with rfl | hr_in
              · simp [hren_rel, resActive', hisNull] at hact
              · have hrB := hnext1.2.2.2.2 r hr_in
                rw [← hres_agree r hrB] at hact
                exact ⟨r, hr_in, hact, renameMemId_congr hag_all hrB⟩
          rcases hshape with
            ⟨sz, rawId, rawSz, rawOrig, rfl, rfl, _, _, _, _⟩ |
            ⟨rfl, rfl, rawId, sd, rfl, _, _, _⟩
          · refine ⟨.scalar 0#64, .scalar 0#64, wrldF, wrldF, ρ_out, resActive', isNullId',
              ?_, ?_, rfl, rfl, rfl, by dsimp [wrldF, N]; omega, hρ_out, hag_all,
              hres_agree, hnull_agree, hnull_nonverif, hwb_nodup, hliveF, ?_⟩
            · dsimp only [isNull, N] at hisNull hev1
              simp only [evalHelperReturn_pointer_eq, Option.isSome, ↓reduceIte]
              rw [Nat.add_assoc, show 1 + 1 = 2 from rfl, hev1]
              simp [hisNull, wrldF, N]
            · dsimp only [isNull, N] at hisNull hev2
              simp only [evalHelperReturn_pointer_eq, Option.isSome, ↓reduceIte]
              rw [Nat.add_assoc, show 1 + 1 = 2 from rfl, hev2]
              simp [hisNull, wrldF, N]
            · simp [TypeRel2, hren_null, isNullId', hisNull, nullInnerInactive, hren_rel, resActive']
          · refine ⟨.scalar 0#64, .scalar 0#64, wrldF, wrldF, ρ_out, resActive', isNullId',
              ?_, ?_, rfl, rfl, rfl, by dsimp [wrldF, N]; omega, hρ_out, hag_all,
              hres_agree, hnull_agree, hnull_nonverif, hwb_nodup, hliveF, ?_⟩
            · dsimp only [isNull, N] at hisNull hev1
              simp only [evalHelperReturn_pointer_eq, Option.isSome, ↓reduceIte]
              rw [Nat.add_assoc, show 1 + 1 = 2 from rfl, hev1]
              simp [hisNull, wrldF, N]
            · dsimp only [isNull, N] at hisNull hev2
              simp only [evalHelperReturn_pointer_eq, Option.isSome, ↓reduceIte]
              rw [Nat.add_assoc, show 1 + 1 = 2 from rfl, hev2]
              simp [hisNull, wrldF, N]
            · simp [TypeRel2, hren_null, isNullId', hisNull, nullInnerInactive, hren_rel, resActive']

/-! ## 5. Forward & Backward Soundness of `BpfVisitor.call_external` -/

theorem visit_callExternal_forward
    {env : ExecEnv} {ctx ctx_out : verifier.ComputationContext}
    {vctx0 vctx1 : verifier.VerificationContext} {helperIdx : Nat}
    (hcctx : CallingContextFullMatches vctx0.calling_context env)
    (hctx : CompCtxValid env vctx0.counter.bv.toNat ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hfwd : VisitorDispatchNonLddw CompCtxVisitorInst ctx vctx0
      (.ok (.Ok (), ctx_out, vctx1)) (.callExternal helperIdx)) :
    ctx_out = ctx ∧
    ForwardVisitPost env vctx0 vctx1 ctx_out := by
  rcases hfwd with ⟨idx32, hidx_eq, hcall⟩
  subst helperIdx
  have hcall' := hcall.symm
  dsimp [CompCtxVisitorInst] at hcall'
  unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.call_external at hcall'
  rcases bind_eq_ok hcall' with ⟨vc1, hemit, hcall'⟩
  obtain ⟨he1, he2, he3, he4, he5, he6⟩ := VerificationContext_emit_log_ok_spec hemit
  rcases bind_eq_ok hcall' with ⟨o_sig, hfind, hcall'⟩
  cases o_sig with
  | none => cases (Prod.mk.inj (ok_inj hcall')).1
  | some sig =>
    dsimp only at hcall'
    simp only [lift_eq_ok, bind_ok] at hcall'
    by_cases hgt : alloc.vec.Vec.len sig.args > UScalar.cast .Usize api.ARGUMENT_COUNT
    · simp only [hgt, ↓reduceIte] at hcall'
      rcases bind_eq_ok hcall' with ⟨_, _, hcall'⟩
      cases (Prod.mk.inj (ok_inj hcall')).1
    · simp only [hgt, ↓reduceIte] at hcall'
      rcases bind_eq_ok hcall' with ⟨r_next, hnext, hcall'⟩
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
          rw [hn_eq]
          exact ⟨hctx_vc1.1, hctx_vc1.2.1, hctx_vc1.2.2.1, hctx_vc1.2.2.2⟩
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
            obtain ⟨hvc2_1, hvc2_2, hvc2_4, hvc2_5, hvc2_6, hvc2_cnt,
              _, _, _, _, hnext2_valid, hv_valid⟩ :=
              resolve_return_value_ok_valid hret_match hnext1_valid hres_ret
            rcases bind_eq_ok hcall' with ⟨p_set, hset, _⟩
            rcases p_set with ⟨r2, next3⟩
            rw [bind_pair_ok_eq hset] at hcall'
            cases r2 with
            | Err _ => cases (Prod.mk.inj (ok_inj hcall')).1
            | Ok u_set =>
              cases u_set
              dsimp only at hcall'
              obtain ⟨_, _, _, _, _, _, hnext3_valid⟩ :=
                ComputationContext_set_reg_ok_spec hnext2_valid hv_valid hset
              obtain ⟨a9, hctx_out, ha9_valid, _, ht_cc, ht_cd, ht_cnt, ht_it, ht_tc, ht_ps⟩ :=
                clobber_scratch_tail_spec hnext3_valid hcall'
              subst hctx_out
              have hcnt_le : vctx0.counter.bv.toNat ≤ vctx1.counter.bv.toNat := by
                rw [ht_cnt, ← he3]; exact hvc2_cnt
              refine ⟨rfl, ?_, ?_, ?_, ?_, hcnt_le, ?_, ?_⟩
              · rw [ht_cc, hvc2_1, he1]
              · rw [ht_cd, hvc2_2, he2]
              · rw [ht_it, hvc2_4, he4]
              · rw [ht_tc, hvc2_5, he5]
              · exact CompCtxValid_mono hcnt_le hctx
              · intro opt hopt
                rw [ht_ps, hvc2_6, he6, hpending, List.nil_append, List.mem_singleton] at hopt
                subst hopt
                exact ⟨{ next3 with registers := a9 }, rfl, ht_cnt ▸ ha9_valid⟩

theorem visit_callExternal_backward
    {prog : Program} {env : ExecEnv} {F : Nat}
    {ctx ctx_out : verifier.ComputationContext}
    {vctx0 vctx1 vctx_back vctx_back1 : verifier.VerificationContext}
    {dPost : verifier.DataDependencies} {tracker : verifier.DependencyTracker}
    {helperIdx : Nat}
    (hcctx : CallingContextFullMatches vctx0.calling_context env)
    (hcctx_back : vctx_back.calling_context = vctx0.calling_context)
    (hpc : ∃ hlt : ctx.pc.val < prog.size,
      prog[ctx.pc.val]'hlt = .callExternal helperIdx)
    (hctx : CompCtxValid env vctx0.counter.bv.toNat ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hfwd : VisitorDispatchNonLddw CompCtxVisitorInst ctx vctx0
      (.ok (.Ok (), ctx_out, vctx1)) (.callExternal helperIdx))
    (hchildren : ∀ succ, some succ ∈ vctx1.pending_successors.val →
      SemTerminatesIn prog env vctx1.counter.bv.toNat succ dPost F)
    (hback : VisitorDispatchNonLddw DepTrackerVisitorInst
      { context := ctx_out, deps := dPost } vctx_back
      (.ok (.Ok (), tracker, vctx_back1)) (.callExternal helperIdx)) :
    ctx_out = ctx ∧
    tracker.context = ctx ∧
    vctx_back1 = vctx_back ∧
    SemTerminatesIn prog env vctx0.counter.bv.toNat ctx tracker.deps (F + 1) := by
  obtain ⟨hlt_pc, hinsn_at⟩ := hpc
  rcases hfwd with ⟨idx32, hidx_eq, hcall⟩
  rcases hback with ⟨idx32_b, hidx_b_eq, hback_call⟩
  have hidx32_eq : idx32_b = idx32 := UScalar.eq_of_val_eq (hidx_b_eq.symm.trans hidx_eq)
  subst idx32_b; subst helperIdx
  have hcall' := hcall.symm
  have hback' := hback_call.symm
  dsimp [CompCtxVisitorInst] at hcall'
  unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.call_external at hcall'
  rcases bind_eq_ok hcall' with ⟨vc1, hemit, hcall'⟩
  obtain ⟨he1, _, he3, _, _, he6⟩ := VerificationContext_emit_log_ok_spec hemit
  rcases bind_eq_ok hcall' with ⟨o_sig, hfind, hcall'⟩
  cases o_sig with
  | none => cases (Prod.mk.inj (ok_inj hcall')).1
  | some sig =>
    dsimp only at hcall'
    simp only [lift_eq_ok, bind_ok] at hcall'
    by_cases hgt : alloc.vec.Vec.len sig.args > UScalar.cast .Usize api.ARGUMENT_COUNT
    · simp only [hgt, ↓reduceIte] at hcall'
      rcases bind_eq_ok hcall' with ⟨_, _, hcall'⟩
      cases (Prod.mk.inj (ok_inj hcall')).1
    · simp only [hgt, ↓reduceIte] at hcall'
      rcases bind_eq_ok hcall' with ⟨r_next, hnext, hcall'⟩
      cases r_next with
      | Err _ => cases (Prod.mk.inj (ok_inj hcall')).1
      | Ok n =>
        dsimp only at hcall'
        obtain ⟨hpc_n, hn_eq⟩ := ComputationContext_next_ok_spec hnext
        have hcctx_vc1 : CallingContextFullMatches vc1.calling_context env := by
          rw [he1]; exact hcctx
        obtain ⟨spec, henv_hlp, hsig_match⟩ :=
          CoreCallingContext_find_helper_ok_some_spec hcctx_vc1.1 hfind
        rcases hsig_match with ⟨hle5, hforall, hidx_params, hret_match, hret_idx, hinv_eq⟩
        have hctx_vc1 : CompCtxValid env vc1.counter.bv.toNat ctx := by
          rw [he3]; exact hctx
        have hn_valid : CompCtxValid env vc1.counter.bv.toNat n := by
          rw [hn_eq]; exact ⟨hctx_vc1.1, hctx_vc1.2.1, hctx_vc1.2.2.1, hctx_vc1.2.2.2⟩
        rcases bind_eq_ok hcall' with ⟨p_loop, hloop, _⟩
        rcases p_loop with ⟨next, loop_err⟩
        rw [bind_pair_ok_eq hloop] at hcall'
        cases loop_err with
        | some _ => cases (Prod.mk.inj (ok_inj hcall')).1
        | none =>
          obtain ⟨hpc_next, hregs_next, hbm_next, hnext_valid⟩ :=
            call_external_loop_ok_valid (vctx := vc1) hctx_vc1 hforall hn_valid hloop
          rw [bind_ok_eq (Type_clone_spec sig.return_value)] at hcall'
          rcases bind_eq_ok hcall' with ⟨next1, hinv_bm, hcall'⟩
          have hnext1_info :
              next1.pc = next.pc ∧
              next1.registers = next.registers ∧
              next1.stack = next.stack ∧
              next1.resources = next.resources ∧
              CompCtxValid env vc1.counter.bv.toNat next1 ∧
              (if sig.invalidate_array_bounds then next1.array_bounds.entries.val = []
               else next1.array_bounds = next.array_bounds) := by
            by_cases hinv : sig.invalidate_array_bounds
            · simp only [hinv, ↓reduceIte] at hinv_bm ⊢
              obtain ⟨bm, hbm_cl, hbm_empty, hbm_valid⟩ :=
                BoundsMap_clear_spec vc1.counter.bv.toNat next.array_bounds
              rw [bind_ok_eq hbm_cl] at hinv_bm
              cases ok_inj hinv_bm
              exact ⟨rfl, rfl, rfl, rfl,
                ⟨hnext_valid.1, hnext_valid.2.1, hbm_valid, hnext_valid.2.2.2⟩, hbm_empty⟩
            · simp only [hinv, Bool.false_eq_true, ↓reduceIte] at hinv_bm ⊢
              cases ok_inj hinv_bm
              exact ⟨rfl, rfl, rfl, rfl, hnext_valid, rfl⟩
          rcases hnext1_info with ⟨hpc_n1, hregs_n1, hstk_n1, hres_n1, hnext1_valid, hbm_n1_cond⟩
          rcases bind_eq_ok hcall' with ⟨p_ret, hres_ret, _⟩
          rcases p_ret with ⟨r1, p_ret2⟩
          rcases p_ret2 with ⟨vc2, next2⟩
          rw [bind_triple_ok_eq hres_ret] at hcall'
          cases r1 with
          | Err _ => cases (Prod.mk.inj (ok_inj hcall')).1
          | Ok v =>
            dsimp only at hcall'
            obtain ⟨_, _, _, _, hvc2_6, hvc2_cnt,
              hpc_n2, hregs_n2, hstk_n2, hbm_n2, hnext2_valid, hv_valid⟩ :=
              resolve_return_value_ok_valid hret_match hnext1_valid hres_ret
            rcases bind_eq_ok hcall' with ⟨p_set, hset, _⟩
            rcases p_set with ⟨r2, next3⟩
            rw [bind_pair_ok_eq hset] at hcall'
            cases r2 with
            | Err _ => cases (Prod.mk.inj (ok_inj hcall')).1
            | Ok u_set =>
              cases u_set
              dsimp only at hcall'
              obtain ⟨_, hpc_n3, hregs_n3, hstk_n3, hbm_n3, hres_n3, hnext3_valid⟩ :=
                ComputationContext_set_reg_ok_spec hnext2_valid hv_valid hset
              obtain ⟨a9, hctx_out, _, ha9_get, _, _, ht_cnt, _, _, ht_ps⟩ :=
                clobber_scratch_tail_spec hnext3_valid hcall'
              subst ctx_out
              -- Unpack backward `DependencyTracker.call_external`:
              have hfind_back : verifier.CoreCallingContext.find_helper vctx_back.calling_context idx32 =
                  ok (some sig) := by
                rw [hcctx_back, ← he1]; exact hfind
              obtain ⟨htrk_ctx, hvback_eq, _, hargs_live, h6_9_live, hstk_deps, hcov⟩ :=
                depTracker_call_external_spec hfind_back hback'
              let succ : verifier.ComputationContext := { next3 with registers := a9 }
              have hmem_succ : some succ ∈ vctx1.pending_successors.val := by
                rw [ht_ps, hvc2_6, he6, hpending]; simp [succ]
              have hsem_succ : SemTerminatesIn prog env vc2.counter.bv.toNat succ dPost F := by
                rw [← ht_cnt]; exact hchildren succ hmem_succ
              refine ⟨rfl, htrk_ctx, hvback_eq, ?_⟩
              intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
              have hρ_vc1 : RhoValid vc1.counter.bv.toNat st₁.world.nextAllocId ρ := by
                rw [he3]; exact hρ
              -- Initial accumulator at `n`:
              have hacc0 : HelperCallAccRel env vc1.counter.bv.toNat ctx dPost ρ resActive isNullId
                  st₁ st₂ n st₁.stack st₂.stack st₁.world st₂.world := by
                rcases hrel with ⟨_, _, hw, _, _, hnd, hlv, _, _, _, _, hsk⟩
                exact {
                  pc_val := hpc_n
                  regs_eq := by rw [hn_eq]
                  bounds_eq := by rw [hn_eq]
                  stk_valid := hn_valid.2.1
                  res_valid := hn_valid.2.2.2
                  wrld_eq := hw
                  structBytes_eq := rfl
                  structFieldNull_eq := rfl
                  arrayLen_eq := rfl
                  stepCount_eq := rfl
                  nextAllocId_eq := rfl
                  live_nodup := hnd
                  live_iff := by rw [hn_eq]; exact hlv
                  stk_rel2 := fun slot hs hd => by
                    rw [hn_eq]; exact hsk slot hs (hstk_deps slot hs hd)
                }
              obtain ⟨stk₁', stk₂', wrld₁', wrld₂', hchk1, hchk2, hacc_out⟩ :=
                call_external_loop_sound (vctx := vc1) (l₁ := l₁) (l₂ := l₂) (helperIdx := idx32.val)
                  hcctx_vc1 hctx_vc1 hρ_vc1 hrel hle5 hforall hidx_params
                  hargs_live hcov hacc0 hloop
              -- Apply `invalidate_array_bounds`:
              let wrldBounds₁ : WorldState :=
                if spec.invalidateArrayBounds then
                  { wrld₁' with arrayLen := env.helperArrayLenOracle wrld₁'.stepCount }
                else wrld₁'
              let wrldBounds₂ : WorldState :=
                if spec.invalidateArrayBounds then
                  { wrld₂' with arrayLen := env.helperArrayLenOracle wrld₂'.stepCount }
                else wrld₂'
              have hwb_eq : wrldBounds₁ = wrldBounds₂ := by
                dsimp only [wrldBounds₁, wrldBounds₂]; rw [hacc_out.wrld_eq]
              have hwb_arr : ∀ id, wrldBounds₁.arrayLen id ≤ 0xFFFF_FFFF#64 := by
                intro id
                dsimp only [wrldBounds₁]
                by_cases hinv_sp : spec.invalidateArrayBounds
                · simp only [hinv_sp, ↓reduceIte]
                  exact hcctx.1.2.2.2.2 wrld₁'.stepCount id
                · simp only [hinv_sp, Bool.false_eq_true, ↓reduceIte]
                  rw [hacc_out.arrayLen_eq]
                  exact hrel.2.2.2.1 id
              have hwb_null : ∀ m : MemoryId, m.namespace ≠ .Verification →
                  isNullId m = wrldBounds₁.structFieldNull m := by
                intro m hm
                have hsfn : wrldBounds₁.structFieldNull = st₁.world.structFieldNull := by
                  dsimp only [wrldBounds₁]
                  cases spec.invalidateArrayBounds <;>
                    simp [hacc_out.structFieldNull_eq]
                rw [hsfn]; exact hrel.2.2.2.2.1 m hm
              have hwb_alloc : wrldBounds₁.nextAllocId = st₁.world.nextAllocId := by
                dsimp only [wrldBounds₁]
                cases spec.invalidateArrayBounds <;> simp [hacc_out.nextAllocId_eq]
              have hwb_nodup : wrldBounds₁.liveResources.Nodup := by
                dsimp only [wrldBounds₁]
                cases spec.invalidateArrayBounds <;> simpa using hacc_out.live_nodup
              have hwb_live : ∀ m : MemoryId,
                  m ∈ wrldBounds₁.liveResources ↔
                    ∃ r ∈ next1.resources.entries.val,
                      resActive (renameMemId ρ r) = true ∧ renameMemId ρ r = m := by
                intro m
                rw [hres_n1]
                dsimp only [wrldBounds₁]
                cases spec.invalidateArrayBounds <;> simpa using hacc_out.live_iff m
              have hargs_live_sp : ∀ j < spec.args.length, DataDepsHasReg tracker.deps (j + 1) = true := by
                rw [← hforall.length_eq]; exact hargs_live
              obtain ⟨r0Val₁, r0Val₂, wrldFinal₁, wrldFinal₂, ρ', resActive', isNullId',
                hev_ret1, hev_ret2, hwf_eq, hwf_arr_eq, hwf_sfn_eq, hwf_alloc_le,
                hρ', hag', hres_ag', hnull_ag', hnull_nv', hwf_nodup, hwf_live, hr0_rel⟩ :=
                resolve_return_value_sound (l₁ := l₁) (l₂ := l₂) (s₁ := s₁) (s₂ := s₂)
                  (helperIdx := idx32.val) hret_match hret_idx hargs_live_sp
                  hctx_vc1 hnext1_valid hrel
                  (by rw [hwb_alloc]; exact hρ_vc1)
                  (by rw [hwb_alloc]; omega)
                  hwb_eq hwb_arr hwb_null hwb_nodup hwb_live hres_ret
              -- Construct post-call `MachineState`s:
              let regs₁' (r : Nat) : RuntimeVal :=
                if r == 0 then r0Val₁
                else if 1 ≤ r && r ≤ 5 then
                  if s₁.preserveCallScratch then
                    .scalar ((st₁.regs r).toBits l₁ st₁.world.arrayLen)
                  else
                    .scalar (s₁.regBits st₁.world.stepCount r)
                else st₁.regs r
              let regs₂' (r : Nat) : RuntimeVal :=
                if r == 0 then r0Val₂
                else if 1 ≤ r && r ≤ 5 then
                  if s₂.preserveCallScratch then
                    .scalar ((st₂.regs r).toBits l₂ st₂.world.arrayLen)
                  else
                    .scalar (s₂.regBits st₂.world.stepCount r)
                else st₂.regs r
              have hsucc_pc : succ.pc.val = ctx.pc.val + 1 := by
                dsimp only [succ]
                rw [hpc_n3, hpc_n2, hpc_n1, hpc_next, hpc_n]
              let st₁' : MachineState :=
                { pc := succ.pc.val, regs := regs₁', stack := stk₁', world := bumpWorldStep wrldFinal₁ }
              let st₂' : MachineState :=
                { pc := succ.pc.val, regs := regs₂', stack := stk₂', world := bumpWorldStep wrldFinal₂ }
              have hwf_arr_bound : ∀ id, wrldFinal₁.arrayLen id ≤ 0xFFFF_FFFF#64 := by
                intro id; rw [hwf_arr_eq]; exact hwb_arr id
              have hrel_post : StateRel2Deps env succ dPost ρ' resActive' isNullId' st₁' st₂' := by
                rcases hrel with ⟨_, _, _, _, _, _, _, hbnds0, hr10_1, hr10_2, hrg0, _⟩
                refine ⟨rfl, rfl, by simp [st₁', st₂', bumpWorldStep, hwf_eq],
                  by simpa [st₁', bumpWorldStep] using hwf_arr_bound,
                  by simpa [st₁', bumpWorldStep] using hnull_nv',
                  by simpa [st₁', bumpWorldStep] using hwf_nodup,
                  by simpa [st₁', succ, hres_n3, bumpWorldStep] using hwf_live,
                  ?_,
                  by simpa [st₁', regs₁', GENERAL_REGISTER_COUNT] using hr10_1,
                  by simpa [st₂', regs₂', GENERAL_REGISTER_COUNT] using hr10_2,
                  ?_, ?_⟩
                · intro m bound hmem_b
                  dsimp only [succ, st₁', bumpWorldStep] at hmem_b ⊢
                  rw [hbm_n3, hbm_n2] at hmem_b
                  by_cases hinv : sig.invalidate_array_bounds
                  · simp only [hinv, ↓reduceIte] at hbm_n1_cond
                    rw [hbm_n1_cond] at hmem_b; cases hmem_b
                  · simp only [hinv, Bool.false_eq_true, ↓reduceIte] at hbm_n1_cond
                    have hinv_sp_f : spec.invalidateArrayBounds = false := by
                      rw [← hinv_eq]; exact Bool.eq_false_of_ne_true hinv
                    rw [hbm_n1_cond, hbm_next, hn_eq] at hmem_b
                    have hmB := hctx_vc1.2.2.1.2 (m, bound) hmem_b
                    rw [← renameMemId_congr hag' hmB, hwf_arr_eq]
                    dsimp only [wrldBounds₁]
                    simp only [hinv_sp_f, Bool.false_eq_true, ↓reduceIte, hacc_out.arrayLen_eq]
                    exact hbnds0 m bound hmem_b
                · intro r hr hlen hd
                  have ha9 := ha9_get r hr hlen
                  by_cases h15 : 1 ≤ r ∧ r ≤ 5
                  · simp only [h15, and_self, ↓reduceIte] at ha9
                    change TypeRel2 env ρ' resActive' isNullId' _ (a9.val[r]'hlen) _ _
                    rw [ha9]
                    simp [TypeRel2]
                  · simp only [h15, ↓reduceIte] at ha9
                    change TypeRel2 env ρ' resActive' isNullId' _ (a9.val[r]'hlen) _ _
                    rw [ha9]
                    by_cases hr0 : r = 0
                    · subst hr0
                      have hn3_0 : (next3.registers.val[0]'(by rw [array10_len]; omega)) = v := by
                        simp [hregs_n3]
                      rw [hn3_0]
                      dsimp only [st₁', st₂', regs₁', regs₂', bumpWorldStep]
                      simp only [beq_self_eq_true, ↓reduceIte]
                      exact hr0_rel
                    · have hge6 : 6 ≤ r := by omega
                      have hlen_ctx : r < ctx.registers.val.length := by rw [array10_len]; exact hr
                      have hn3_r : (next3.registers.val[r]'(by rw [array10_len]; exact hr)) =
                          (ctx.registers.val[r]'hlen_ctx) := by
                        simp only [hregs_n3, show (0#u8).val = 0 from rfl,
                          List.getElem_set_ne (Ne.symm hr0),
                          hregs_n2, hregs_n1, hregs_next, hacc0.regs_eq]
                      rw [hn3_r]
                      have hb0 : (r == 0) = false := beq_false_of_ne hr0
                      have hb15 : (decide (1 ≤ r) && decide (r ≤ 5)) = false := by
                        simp only [Bool.and_eq_false_iff, decide_eq_false_iff_not]; omega
                      dsimp only [st₁', st₂', regs₁', regs₂', bumpWorldStep]
                      simp only [hb0, hb15, Bool.false_eq_true, ↓reduceIte]
                      have ht_valid := hctx_vc1.1 _ (List.getElem_mem hlen_ctx)
                      exact TypeRel2_congr ht_valid hag' hres_ag' hnull_ag'
                        (fun _ _ _ => hwf_arr_bound _)
                        (hrg0 r hr hlen_ctx (h6_9_live r hr hge6 hd))
                · intro s hs hd
                  dsimp only [st₁', st₂', succ, bumpWorldStep]
                  rw [hstk_n3, hstk_n2, hstk_n1]
                  have hst_valid := stackSlotType_valid hacc_out.stk_valid s hs
                  exact StackSlotRel2_congr hst_valid hag' hres_ag' hnull_ag'
                    (fun _ _ _ => hwf_arr_bound _)
                    (hacc_out.stk_rel2 s hs hd)
              have halloc' : (bumpWorldStep wrldFinal₁).nextAllocId + 3 * F < 2 ^ 63 := by
                simp only [bumpWorldStep]
                rw [hwb_alloc] at hwf_alloc_le
                omega
              obtain ⟨ret, finalWorld, hr₁, hr₂, hlive_fin⟩ :=
                hsem_succ l₁ l₂ s₁ s₂ ρ' resActive' isNullId' st₁' st₂'
                  hl₁ hl₂ hleak halloc' hρ' hrel_post
              have hstep1 : step prog env l₁ s₁ st₁ = .next st₁' := by
                unfold step
                have hlt1 : st₁.pc < prog.size := hrel.1 ▸ hlt_pc
                have hins1 : prog[st₁.pc]'hlt1 = .callExternal idx32.val := by
                  simp only [hrel.1]; exact hinsn_at
                simp only [hlt1, ↓reduceDIte, hins1, henv_hlp, hchk1]
                dsimp only [wrldBounds₁] at hev_ret1
                rw [hev_ret1]
                exact stepNextIte_eq_next (by dsimp only [st₁', regs₁']; rw [hrel.1, hsucc_pc]) hr₁
              have hstep2 : step prog env l₂ s₂ st₂ = .next st₂' := by
                unfold step
                have hlt2 : st₂.pc < prog.size := hrel.2.1 ▸ hlt_pc
                have hins2 : prog[st₂.pc]'hlt2 = .callExternal idx32.val := by
                  simp only [hrel.2.1]; exact hinsn_at
                simp only [hlt2, ↓reduceDIte, hins2, henv_hlp, hchk2]
                dsimp only [wrldBounds₂] at hev_ret2
                rw [hev_ret2]
                exact stepNextIte_eq_next (by dsimp only [st₂', regs₂']; rw [hrel.2.1, hsucc_pc]) hr₂
              refine ⟨ret, finalWorld, ?_, ?_, hlive_fin⟩
              · unfold runSteps; rw [hstep1]; exact hr₁
              · unfold runSteps; rw [hstep2]; exact hr₂

end StarnixEbpf.Proof
