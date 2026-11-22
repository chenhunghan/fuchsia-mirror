-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Proof.DecodeAndPrescan
import Proof.ScalarProofs

/-!
# Relational 2-Safety Invariant & `MemoryId` Order / Renaming Infrastructure

Defines the core invariants connecting the Aeneas-extracted abstract interpreter
(`ebpf_core::verifier`) to the small-step operational semantics (`StarnixEbpf.Spec`):
1. Canonical abstract scalars (`uninitScalar64`, `unknownWrittenScalar64`, `knownScalar64`)
   and unfolding equations for `@[irreducible]` verifier constants.
2. Pure `Namespace` and `MemoryId` specifications, strict total order (`MemoryIdLt`),
   and injective runtime ID renaming (`renameMemId`, `RhoValid`).
3. Structural validity predicates (`IdSetValid`, `BoundsMapValid`, `TypeValid`,
   `StackValid`, `CompCtxValid`).
4. Dependency-indexed relational 2-safety predicates (`TypeRel2`, `StackSlotRel2`,
   `StateRel2Deps`) and bounded 2-safety termination (`SemTerminatesIn`).
-/

open Aeneas Aeneas.Std Result

namespace StarnixEbpf.Proof

open StarnixEbpf.Assumptions
open StarnixEbpf.Spec
open StarnixEbpf.LoopLemmas
open ebpf_core
open ebpf_core.scalar_value

/-! ## 1. Canonical Abstract Scalars, `DecidableEq`, & `@[irreducible]` Constants -/

instance : DecidableEq U64Range := fun a b =>
  if h : a.min = b.min ∧ a.max = b.max then
    isTrue (by cases a; cases b; simp_all)
  else
    isFalse (by intro heq; subst heq; exact h ⟨rfl, rfl⟩)

instance : DecidableEq U64ScalarValueData := fun a b =>
  if h : a.value = b.value ∧ a.unknown_mask = b.unknown_mask ∧
         a.unwritten_mask = b.unwritten_mask ∧ a.urange = b.urange then
    isTrue (by cases a; cases b; simp_all)
  else
    isFalse (by intro heq; subst heq; exact h ⟨rfl, rfl, rfl, rfl⟩)

/-- Canonical `U64ScalarValueData::UNINITIALIZED` value. -/
def uninitScalar64 : U64ScalarValueData :=
  { value := 0#u64
    unknown_mask := core.num.U64.MAX
    unwritten_mask := core.num.U64.MAX
    urange := { min := 0#u64, max := core.num.U64.MAX }
    _guard := () }

/-- Canonical `U64ScalarValueData::UNKNOWN_WRITTEN` (`Type::UNKNOWN_SCALAR`) value. -/
def unknownWrittenScalar64 : U64ScalarValueData :=
  { value := 0#u64
    unknown_mask := core.num.U64.MAX
    unwritten_mask := 0#u64
    urange := { min := 0#u64, max := core.num.U64.MAX }
    _guard := () }

/-- Canonical `U64ScalarValueData::from_known(v)` value. -/
def knownScalar64 (v : Std.U64) : U64ScalarValueData :=
  { value := v
    unknown_mask := 0#u64
    unwritten_mask := 0#u64
    urange := { min := v, max := v }
    _guard := () }

theorem UNINITIALIZED_64_eq :
    U64ScalarValueData.UNINITIALIZED = ok uninitScalar64 := by
  unfold U64ScalarValueData.UNINITIALIZED U64Range.impl.max U64Range.new
  simp only [bind_ok]
  rw [new64_eq_ok]
  congr 1

theorem UNKNOWN_WRITTEN_64_eq :
    U64ScalarValueData.UNKNOWN_WRITTEN = ok unknownWrittenScalar64 := by
  unfold U64ScalarValueData.UNKNOWN_WRITTEN U64Range.impl.max U64Range.new
  simp only [bind_ok]
  rw [new64_eq_ok]
  congr 1

theorem from_known_64_eq (v : Std.U64) :
    U64ScalarValueData.from_known v = ok (knownScalar64 v) := by
  unfold U64ScalarValueData.from_known U64Range.from_scalar U64Range.new
  simp only [bind_ok]
  rw [new64_eq_ok]
  congr 1
  unfold pureNew64 knownScalar64
  simp only [U64ScalarValueData.mk.injEq, U64Range.mk.injEq, u64_eq_iff_bv,
    u64_mk_bv, u64_zero_bv]
  refine ⟨?_, ?_, ?_, ?_, ?_⟩ <;> bv_decide

theorem Type_UNINITIALIZED_eq :
    verifier.Type.UNINITIALIZED = ok (.ScalarValue uninitScalar64) := by
  unfold verifier.Type.UNINITIALIZED
  rw [UNINITIALIZED_64_eq]
  simp only [bind_ok]

theorem Type_UNKNOWN_SCALAR_eq :
    verifier.Type.UNKNOWN_SCALAR = ok (.ScalarValue unknownWrittenScalar64) := by
  unfold verifier.Type.UNKNOWN_SCALAR
  rw [UNKNOWN_WRITTEN_64_eq]
  simp only [bind_ok]

theorem uninitScalar64_valid : Scalar64Valid uninitScalar64 :=
  UNINITIALIZED_64_valid UNINITIALIZED_64_eq

theorem unknownWrittenScalar64_valid : Scalar64Valid unknownWrittenScalar64 :=
  UNKNOWN_WRITTEN_64_valid UNKNOWN_WRITTEN_64_eq

theorem knownScalar64_valid (v : Std.U64) : Scalar64Valid (knownScalar64 v) :=
  from_known_64_valid (from_known_64_eq v)

theorem concretizesScalar64Rel2_uninit (x₁ x₂ : BitVec 64) :
    ConcretizesScalar64Rel2 uninitScalar64 x₁ x₂ := by
  unfold ConcretizesScalar64Rel2 ConcretizesScalar64 Scalar64Consistent uninitScalar64
  simp only [u64_max_bv, u64_zero_bv]
  refine ⟨⟨⟨?_, ?_, ?_, ?_, ?_⟩, ?_, ?_, ?_⟩, ⟨⟨?_, ?_, ?_, ?_, ?_⟩, ?_, ?_, ?_⟩, ?_⟩ <;> bv_decide

theorem concretizesScalar64Rel2_unknownWritten (x : BitVec 64) :
    ConcretizesScalar64Rel2 unknownWrittenScalar64 x x := by
  unfold ConcretizesScalar64Rel2 ConcretizesScalar64 Scalar64Consistent unknownWrittenScalar64
  simp only [u64_max_bv, u64_zero_bv]
  refine ⟨⟨⟨?_, ?_, ?_, ?_, ?_⟩, ?_, ?_, ?_⟩, ⟨⟨?_, ?_, ?_, ?_, ?_⟩, ?_, ?_, ?_⟩, ?_⟩ <;> bv_decide

theorem concretizesScalar64_known (v : Std.U64) :
    ConcretizesScalar64 (knownScalar64 v) v.bv := by
  unfold ConcretizesScalar64 Scalar64Consistent knownScalar64
  simp only [u64_zero_bv]
  refine ⟨⟨?_, ?_, ?_, ?_, ?_⟩, ?_, ?_, ?_⟩ <;> bv_decide

theorem concretizesScalar64Rel2_known (v : Std.U64) :
    ConcretizesScalar64Rel2 (knownScalar64 v) v.bv v.bv :=
  ⟨concretizesScalar64_known v, concretizesScalar64_known v, rfl⟩

theorem eq_of_concretizesScalar64_known {v : Std.U64} {x : BitVec 64}
    (h : ConcretizesScalar64 (knownScalar64 v) x) : x = v.bv := by
  have hmin := h.2.2.1
  have hmax := h.2.2.2
  simp only [knownScalar64] at hmin hmax
  exact BitVec.le_antisymm hmax hmin

theorem verifier_U32_MAX_eq : verifier.U32_MAX = ok 0xFFFF_FFFF#u64 := by
  unfold verifier.U32_MAX core.num.U32.MAX
  rfl

theorem verifier_MAX_PACKET_OFFSET_eq : verifier.MAX_PACKET_OFFSET = 65535#u64 := by
  unfold verifier.MAX_PACKET_OFFSET
  rfl

theorem verifier_STACK_ELEMENT_SIZE_eq : verifier.STACK_ELEMENT_SIZE = 8#usize := by
  unfold verifier.STACK_ELEMENT_SIZE
  rfl

theorem BPF_MAX_VAR_OFF_eq :
    verifier.ComputationContext.apply_computation.BPF_MAX_VAR_OFF = ok 536870912#u64 := by
  unfold verifier.ComputationContext.apply_computation.BPF_MAX_VAR_OFF
  have hge : (29#i32).val ≥ 0 := by decide
  have hlt : (29#i32).toNat < 64 := by decide
  change UScalar.shiftLeft_IScalar 1#u64 29#i32 = _
  simp only [UScalar.shiftLeft_IScalar, hge, ↓reduceIte, UScalar.shiftLeft,
    UScalarTy.numBits, hlt]
  rfl

theorem BPF_MAX_VAR_OFF_1_eq :
    verifier.ComputationContext.apply_computation.BPF_MAX_VAR_OFF_1 = ok 536870912#u64 := by
  unfold verifier.ComputationContext.apply_computation.BPF_MAX_VAR_OFF_1
  have hge : (29#i32).val ≥ 0 := by decide
  have hlt : (29#i32).toNat < 64 := by decide
  change UScalar.shiftLeft_IScalar 1#u64 29#i32 = _
  simp only [UScalar.shiftLeft_IScalar, hge, ↓reduceIte, UScalar.shiftLeft,
    UScalarTy.numBits, hlt]
  rfl

theorem api_BPF_STACK_SIZE_eq : api.BPF_STACK_SIZE = 512#usize := by
  unfold api.BPF_STACK_SIZE; rfl

theorem api_BPF_MAX_INSTS_eq : api.BPF_MAX_INSTS = 65536#usize := by
  unfold api.BPF_MAX_INSTS; rfl

theorem api_GENERAL_REGISTER_COUNT_eq : api.GENERAL_REGISTER_COUNT = 10#u8 := by
  unfold api.GENERAL_REGISTER_COUNT; rfl

theorem api_REGISTER_COUNT_eq : api.REGISTER_COUNT = 11#u8 := by
  unfold api.REGISTER_COUNT; rfl

theorem api_ARGUMENT_COUNT_eq : api.ARGUMENT_COUNT = 5#u8 := by
  unfold api.ARGUMENT_COUNT; rfl

/-- Canonical valid host layout witness used to extract layout-independent facts
    from `EntryStatesConform`. -/
def dummyHostLayout : HostLayout :=
  { stackBase := 1#64
    bufBase := fun _ => 1#64
    structBase := fun _ => 1#64
    arrayBase := fun _ => 1#64
    mapAddr := fun _ => 1#64 }

theorem dummyHostLayout_valid : ValidHostLayout dummyHostLayout := by
  unfold ValidHostLayout dummyHostLayout CANONICAL_USER_MAX
  dsimp only
  refine ⟨by decide, fun _ => by decide, fun _ => by decide, fun _ => by decide, fun _ => by decide⟩

def dummySecretInit : SecretInit :=
  { stackBits := fun _ => 0#64
    regBits := fun _ _ => 0#64
    preserveCallScratch := false }

/-! ## 2. `Namespace` and `MemoryId` Specifications, Strict Order, & Renaming -/

def pureNsTag : verifier.Namespace → Nat
  | .Hardcoded => 0
  | .Generated => 1
  | .Verification => 2

theorem pureNsTag_inj {a b : verifier.Namespace} (h : pureNsTag a = pureNsTag b) : a = b := by
  cases a <;> cases b <;> simp [pureNsTag] at h <;> rfl

def pureCmpNat (a b : Nat) : Ordering :=
  if a < b then .lt else if b < a then .gt else .eq

theorem pureCmpNat_eq_iff (a b : Nat) : pureCmpNat a b = .eq ↔ a = b := by
  unfold pureCmpNat
  split_ifs <;> simp <;> omega

theorem pureCmpNat_lt_iff (a b : Nat) : pureCmpNat a b = .lt ↔ a < b := by
  unfold pureCmpNat
  split_ifs <;> simp <;> omega

theorem pureCmpNat_gt_iff (a b : Nat) : pureCmpNat a b = .gt ↔ b < a := by
  unfold pureCmpNat
  split_ifs <;> simp <;> omega

theorem pureCmpNat_swap (a b : Nat) : pureCmpNat b a = (pureCmpNat a b).swap := by
  unfold pureCmpNat
  by_cases h1 : b < a
  · have h2 : ¬ a < b := by omega
    simp [h1, h2, Ordering.swap]
  · by_cases h2 : a < b
    · simp [h1, h2, Ordering.swap]
    · simp [h1, h2, Ordering.swap]

/-- Pure lexicographic comparison on `MemoryId` matching `MemoryId::cmp_id`. -/
def pureMemoryIdCmp : MemoryId → MemoryId → Ordering
  | .mk ns1 id1 p1, .mk ns2 id2 p2 =>
      match pureCmpNat (pureNsTag ns1) (pureNsTag ns2) with
      | .lt => .lt
      | .gt => .gt
      | .eq =>
          match pureCmpNat id1.bv.toNat id2.bv.toNat with
          | .lt => .lt
          | .gt => .gt
          | .eq =>
              match p1, p2 with
              | none, none => .eq
              | none, some _ => .lt
              | some _, none => .gt
              | some q1, some q2 => pureMemoryIdCmp q1 q2

/-- Strict total order on `MemoryId` induced by `pureMemoryIdCmp`. -/
def MemoryIdLt (a b : MemoryId) : Prop :=
  pureMemoryIdCmp a b = .lt

theorem pureMemoryIdCmp_refl : ∀ a : MemoryId, pureMemoryIdCmp a a = .eq
  | .mk ns idx none => by
      simp [pureMemoryIdCmp, (pureCmpNat_eq_iff _ _).2 rfl]
  | .mk ns idx (some q) => by
      simp [pureMemoryIdCmp, (pureCmpNat_eq_iff _ _).2 rfl, pureMemoryIdCmp_refl q]

private theorem pureMemoryIdCmp_eq_imp :
    ∀ (a b : MemoryId), pureMemoryIdCmp a b = .eq → a = b
  | .mk ns1 id1 none, .mk ns2 id2 p2, h => by
      unfold pureMemoryIdCmp at h
      cases hns : pureCmpNat (pureNsTag ns1) (pureNsTag ns2) <;> rw [hns] at h
      · cases h
      · cases hid : pureCmpNat id1.bv.toNat id2.bv.toNat <;> rw [hid] at h
        · cases h
        · have hnsEq : ns1 = ns2 := pureNsTag_inj ((pureCmpNat_eq_iff _ _).1 hns)
          have hidEq : id1 = id2 := by
            have hbveq := BitVec.eq_of_toNat_eq ((pureCmpNat_eq_iff _ _).1 hid)
            exact (u64_eq_iff_bv _ _).2 hbveq
          subst hnsEq; subst hidEq
          cases p2 with
          | none => rfl
          | some _ => cases h
        · cases h
      · cases h
  | .mk ns1 id1 (some q1), .mk ns2 id2 p2, h => by
      unfold pureMemoryIdCmp at h
      cases hns : pureCmpNat (pureNsTag ns1) (pureNsTag ns2) <;> rw [hns] at h
      · cases h
      · cases hid : pureCmpNat id1.bv.toNat id2.bv.toNat <;> rw [hid] at h
        · cases h
        · have hnsEq : ns1 = ns2 := pureNsTag_inj ((pureCmpNat_eq_iff _ _).1 hns)
          have hidEq : id1 = id2 := by
            have hbveq := BitVec.eq_of_toNat_eq ((pureCmpNat_eq_iff _ _).1 hid)
            exact (u64_eq_iff_bv _ _).2 hbveq
          subst hnsEq; subst hidEq
          cases p2 with
          | none => cases h
          | some q2 =>
            congr 2
            exact pureMemoryIdCmp_eq_imp q1 q2 h
        · cases h
      · cases h

theorem pureMemoryIdCmp_eq_iff (a b : MemoryId) :
    pureMemoryIdCmp a b = .eq ↔ a = b :=
  ⟨pureMemoryIdCmp_eq_imp a b, fun h => by subst h; exact pureMemoryIdCmp_refl a⟩

theorem pureMemoryIdCmp_swap :
    ∀ (a b : MemoryId), pureMemoryIdCmp b a = (pureMemoryIdCmp a b).swap
  | .mk ns1 id1 none, .mk ns2 id2 p2 => by
      unfold pureMemoryIdCmp
      rw [pureCmpNat_swap (pureNsTag ns1) (pureNsTag ns2)]
      cases pureCmpNat (pureNsTag ns1) (pureNsTag ns2) <;> simp only [Ordering.swap]
      rw [pureCmpNat_swap id1.bv.toNat id2.bv.toNat]
      cases pureCmpNat id1.bv.toNat id2.bv.toNat <;> simp only [Ordering.swap]
      cases p2 <;> rfl
  | .mk ns1 id1 (some q1), .mk ns2 id2 p2 => by
      unfold pureMemoryIdCmp
      rw [pureCmpNat_swap (pureNsTag ns1) (pureNsTag ns2)]
      cases pureCmpNat (pureNsTag ns1) (pureNsTag ns2) <;> simp only [Ordering.swap]
      rw [pureCmpNat_swap id1.bv.toNat id2.bv.toNat]
      cases pureCmpNat id1.bv.toNat id2.bv.toNat <;> simp only [Ordering.swap]
      cases p2 with
      | none => rfl
      | some q2 => exact pureMemoryIdCmp_swap q1 q2

theorem MemoryIdLt_irrefl (a : MemoryId) : ¬ MemoryIdLt a a := by
  unfold MemoryIdLt
  rw [pureMemoryIdCmp_refl]
  decide

theorem MemoryIdLt_asymm {a b : MemoryId} (h : MemoryIdLt a b) : ¬ MemoryIdLt b a := by
  unfold MemoryIdLt at *
  rw [pureMemoryIdCmp_swap, h]
  decide

theorem MemoryIdLt_trans :
    ∀ {a b c : MemoryId}, MemoryIdLt a b → MemoryIdLt b c → MemoryIdLt a c
  | .mk ns1 id1 none, .mk ns2 id2 p2, .mk ns3 id3 p3, hab, hbc => by
      unfold MemoryIdLt pureMemoryIdCmp at hab hbc ⊢
      cases hns12 : pureCmpNat (pureNsTag ns1) (pureNsTag ns2) <;> rw [hns12] at hab
      · cases hns23 : pureCmpNat (pureNsTag ns2) (pureNsTag ns3) <;> rw [hns23] at hbc
        · have hlt12 := (pureCmpNat_lt_iff _ _).1 hns12
          have hlt23 := (pureCmpNat_lt_iff _ _).1 hns23
          rw [(pureCmpNat_lt_iff _ _).2 (Nat.lt_trans hlt12 hlt23)]
        · have hlt12 := (pureCmpNat_lt_iff _ _).1 hns12
          have heq23 := (pureCmpNat_eq_iff _ _).1 hns23
          rw [(pureCmpNat_lt_iff _ _).2 (by omega)]
        · cases hbc
      · have heqns12 := (pureCmpNat_eq_iff _ _).1 hns12
        cases hns23 : pureCmpNat (pureNsTag ns2) (pureNsTag ns3) <;> rw [hns23] at hbc
        · have hlt23 := (pureCmpNat_lt_iff _ _).1 hns23
          rw [(pureCmpNat_lt_iff _ _).2 (by omega)]
        · have heqns23 := (pureCmpNat_eq_iff _ _).1 hns23
          rw [(pureCmpNat_eq_iff _ _).2 (heqns12.trans heqns23)]
          cases hid12 : pureCmpNat id1.bv.toNat id2.bv.toNat <;> rw [hid12] at hab
          · cases hid23 : pureCmpNat id2.bv.toNat id3.bv.toNat <;> rw [hid23] at hbc
            · have h12 := (pureCmpNat_lt_iff _ _).1 hid12
              have h23 := (pureCmpNat_lt_iff _ _).1 hid23
              rw [(pureCmpNat_lt_iff _ _).2 (Nat.lt_trans h12 h23)]
            · have h12 := (pureCmpNat_lt_iff _ _).1 hid12
              have h23 := (pureCmpNat_eq_iff _ _).1 hid23
              rw [(pureCmpNat_lt_iff _ _).2 (by omega)]
            · cases hbc
          · have heqid12 := (pureCmpNat_eq_iff _ _).1 hid12
            cases hid23 : pureCmpNat id2.bv.toNat id3.bv.toNat <;> rw [hid23] at hbc
            · have h23 := (pureCmpNat_lt_iff _ _).1 hid23
              rw [(pureCmpNat_lt_iff _ _).2 (by omega)]
            · have heqid23 := (pureCmpNat_eq_iff _ _).1 hid23
              rw [(pureCmpNat_eq_iff _ _).2 (heqid12.trans heqid23)]
              cases p2 with
              | none => cases hab
              | some _ =>
                cases p3 with
                | none => cases hbc
                | some _ => rfl
            · cases hbc
          · cases hab
        · cases hbc
      · cases hab
  | .mk ns1 id1 (some q1), .mk ns2 id2 p2, .mk ns3 id3 p3, hab, hbc => by
      unfold MemoryIdLt pureMemoryIdCmp at hab hbc ⊢
      cases hns12 : pureCmpNat (pureNsTag ns1) (pureNsTag ns2) <;> rw [hns12] at hab
      · cases hns23 : pureCmpNat (pureNsTag ns2) (pureNsTag ns3) <;> rw [hns23] at hbc
        · have hlt12 := (pureCmpNat_lt_iff _ _).1 hns12
          have hlt23 := (pureCmpNat_lt_iff _ _).1 hns23
          rw [(pureCmpNat_lt_iff _ _).2 (Nat.lt_trans hlt12 hlt23)]
        · have hlt12 := (pureCmpNat_lt_iff _ _).1 hns12
          have heq23 := (pureCmpNat_eq_iff _ _).1 hns23
          rw [(pureCmpNat_lt_iff _ _).2 (by omega)]
        · cases hbc
      · have heqns12 := (pureCmpNat_eq_iff _ _).1 hns12
        cases hns23 : pureCmpNat (pureNsTag ns2) (pureNsTag ns3) <;> rw [hns23] at hbc
        · have hlt23 := (pureCmpNat_lt_iff _ _).1 hns23
          rw [(pureCmpNat_lt_iff _ _).2 (by omega)]
        · have heqns23 := (pureCmpNat_eq_iff _ _).1 hns23
          rw [(pureCmpNat_eq_iff _ _).2 (heqns12.trans heqns23)]
          cases hid12 : pureCmpNat id1.bv.toNat id2.bv.toNat <;> rw [hid12] at hab
          · cases hid23 : pureCmpNat id2.bv.toNat id3.bv.toNat <;> rw [hid23] at hbc
            · have h12 := (pureCmpNat_lt_iff _ _).1 hid12
              have h23 := (pureCmpNat_lt_iff _ _).1 hid23
              rw [(pureCmpNat_lt_iff _ _).2 (Nat.lt_trans h12 h23)]
            · have h12 := (pureCmpNat_lt_iff _ _).1 hid12
              have h23 := (pureCmpNat_eq_iff _ _).1 hid23
              rw [(pureCmpNat_lt_iff _ _).2 (by omega)]
            · cases hbc
          · have heqid12 := (pureCmpNat_eq_iff _ _).1 hid12
            cases hid23 : pureCmpNat id2.bv.toNat id3.bv.toNat <;> rw [hid23] at hbc
            · have h23 := (pureCmpNat_lt_iff _ _).1 hid23
              rw [(pureCmpNat_lt_iff _ _).2 (by omega)]
            · have heqid23 := (pureCmpNat_eq_iff _ _).1 hid23
              rw [(pureCmpNat_eq_iff _ _).2 (heqid12.trans heqid23)]
              cases p2 with
              | none => cases hab
              | some q2 =>
                cases p3 with
                | none => cases hbc
                | some q3 => exact MemoryIdLt_trans hab hbc
            · cases hbc
          · cases hab
        · cases hbc
      · cases hab

theorem MemoryIdLt_nodup {l : List MemoryId} (h : List.Pairwise MemoryIdLt l) : l.Nodup :=
  h.imp (fun {a b} hab heq => by subst heq; exact MemoryIdLt_irrefl a hab)

theorem verifier_cmp_u8_spec (a b : Std.U8) :
    verifier.cmp_u8 a b = ok (pureCmpNat a.val b.val) := by
  unfold verifier.cmp_u8 pureCmpNat
  by_cases hlt : a < b
  · have hv : a.val < b.val := hlt
    simp [hlt, hv]
  · have hnv : ¬ a.val < b.val := hlt
    simp only [hlt, hnv, ↓reduceIte]
    by_cases hgt : a > b
    · have hgv : b.val < a.val := hgt
      simp [hgt, hgv]
    · have hngv : ¬ b.val < a.val := hgt
      simp [hgt, hngv]

theorem verifier_cmp_u64_spec (a b : Std.U64) :
    verifier.cmp_u64 a b = ok (pureCmpNat a.bv.toNat b.bv.toNat) := by
  unfold verifier.cmp_u64 pureCmpNat
  simp only [UScalar.bv_toNat]
  by_cases hlt : a < b
  · have hv : a.val < b.val := hlt
    simp [hlt, hv]
  · have hnv : ¬ a.val < b.val := hlt
    simp only [hlt, hnv, ↓reduceIte]
    by_cases hgt : a > b
    · have hgv : b.val < a.val := hgt
      simp [hgt, hgv]
    · have hngv : ¬ b.val < a.val := hgt
      simp [hgt, hngv]

theorem Namespace_eq_spec (a b : verifier.Namespace) :
    verifier.Namespace.Insts.CoreCmpPartialEqNamespace.eq a b = ok (decide (a = b)) := by
  unfold verifier.Namespace.Insts.CoreCmpPartialEqNamespace.eq verifier.Namespace.tag
  cases a <;> cases b <;> simp

theorem Namespace_ne_spec (a b : verifier.Namespace) :
    core.cmp.PartialEq.ne.trait_default
      verifier.Namespace.Insts.CoreCmpPartialEqNamespace a b = ok (decide (a ≠ b)) := by
  unfold core.cmp.PartialEq.ne.trait_default core.cmp.PartialEq.ne.default
    verifier.Namespace.Insts.CoreCmpPartialEqNamespace
  simp only [Namespace_eq_spec]
  cases a <;> cases b <;> simp

theorem Namespace_cmp_ns_spec (a b : verifier.Namespace) :
    verifier.Namespace.cmp_ns a b = ok (pureCmpNat (pureNsTag a) (pureNsTag b)) := by
  unfold verifier.Namespace.cmp_ns verifier.Namespace.tag
  cases a <;> cases b <;> simp [verifier_cmp_u8_spec, pureCmpNat, pureNsTag]

theorem MemoryId_clone_spec :
    ∀ m : MemoryId, verifier.MemoryId.Insts.CoreCloneClone.clone m = ok m
  | .mk ns idx none => by
      rw [verifier.MemoryId.Insts.CoreCloneClone.clone.eq_def]
      simp
  | .mk ns idx (some q) => by
      rw [verifier.MemoryId.Insts.CoreCloneClone.clone.eq_def]
      simp [MemoryId_clone_spec q]

theorem MemoryId_eq_spec :
    ∀ (a b : MemoryId),
      verifier.MemoryId.Insts.CoreCmpPartialEqMemoryId.eq a b = ok (decide (a = b))
  | .mk ns1 id1 none, .mk ns2 id2 p2 => by
      rw [verifier.MemoryId.Insts.CoreCmpPartialEqMemoryId.eq.eq_def]
      simp only [verifier.MemoryId.namespace._simpLemma_,
        verifier.MemoryId.id._simpLemma_, verifier.MemoryId.parent._simpLemma_,
        Namespace_ne_spec, bind_ok]
      by_cases hns : ns1 = ns2
      · subst hns
        simp only [ne_eq, not_true_eq_false, decide_false, Bool.false_eq_true, ↓reduceIte]
        by_cases hid : id1 = id2
        · subst hid
          simp only [bne_self_eq_false, Bool.false_eq_true, ↓reduceIte]
          cases p2 <;> simp
        · have hbne : (id1 != id2) = true := bne_iff_ne.2 hid
          simp [hbne, hid]
      · simp [hns]
  | .mk ns1 id1 (some q1), .mk ns2 id2 p2 => by
      rw [verifier.MemoryId.Insts.CoreCmpPartialEqMemoryId.eq.eq_def]
      simp only [verifier.MemoryId.namespace._simpLemma_,
        verifier.MemoryId.id._simpLemma_, verifier.MemoryId.parent._simpLemma_,
        Namespace_ne_spec, bind_ok]
      by_cases hns : ns1 = ns2
      · subst hns
        simp only [ne_eq, not_true_eq_false, decide_false, Bool.false_eq_true, ↓reduceIte]
        by_cases hid : id1 = id2
        · subst hid
          simp only [bne_self_eq_false, Bool.false_eq_true, ↓reduceIte]
          cases p2 with
          | none => simp
          | some q2 =>
            dsimp only
            rw [MemoryId_eq_spec q1 q2]
            congr 1
            simp
        · have hbne : (id1 != id2) = true := bne_iff_ne.2 hid
          simp [hbne, hid]
      · simp [hns]

theorem MemoryId_ne_spec (a b : MemoryId) :
    core.cmp.PartialEq.ne.trait_default
      verifier.MemoryId.Insts.CoreCmpPartialEqMemoryId a b = ok (decide (a ≠ b)) := by
  unfold core.cmp.PartialEq.ne.trait_default core.cmp.PartialEq.ne.default
    verifier.MemoryId.Insts.CoreCmpPartialEqMemoryId
  simp only [MemoryId_eq_spec]
  by_cases h : a = b <;> simp [h]

theorem MemoryId_cmp_id_spec :
    ∀ (a b : MemoryId), verifier.MemoryId.cmp_id a b = ok (pureMemoryIdCmp a b)
  | .mk ns1 id1 none, .mk ns2 id2 p2 => by
      rw [verifier.MemoryId.cmp_id.eq_def]
      unfold pureMemoryIdCmp
      simp only [verifier.MemoryId.namespace._simpLemma_,
        verifier.MemoryId.id._simpLemma_, verifier.MemoryId.parent._simpLemma_,
        Namespace_cmp_ns_spec, bind_ok]
      cases hns : pureCmpNat (pureNsTag ns1) (pureNsTag ns2) with
      | lt => simp
      | gt => simp
      | eq =>
        simp only [↓reduceIte, verifier_cmp_u64_spec, bind_ok]
        cases hid : pureCmpNat id1.bv.toNat id2.bv.toNat with
        | lt => simp
        | gt => simp
        | eq => cases p2 <;> simp
  | .mk ns1 id1 (some q1), .mk ns2 id2 p2 => by
      rw [verifier.MemoryId.cmp_id.eq_def]
      unfold pureMemoryIdCmp
      simp only [verifier.MemoryId.namespace._simpLemma_,
        verifier.MemoryId.id._simpLemma_, verifier.MemoryId.parent._simpLemma_,
        Namespace_cmp_ns_spec, bind_ok]
      cases hns : pureCmpNat (pureNsTag ns1) (pureNsTag ns2) with
      | lt => simp
      | gt => simp
      | eq =>
        simp only [↓reduceIte, verifier_cmp_u64_spec, bind_ok]
        cases hid : pureCmpNat id1.bv.toNat id2.bv.toNat with
        | lt => simp
        | gt => simp
        | eq =>
          cases p2 with
          | none => simp
          | some q2 =>
            simp only [↓reduceIte, bind_ok]
            exact MemoryId_cmp_id_spec q1 q2

theorem MemoryId_prepended_spec :
    ∀ (a b : MemoryId), verifier.MemoryId.prepended a b = ok (memoryIdPrepend a b)
  | .mk ns idx none, b => by
      rw [verifier.MemoryId.prepended.eq_def]
      simp [memoryIdPrepend]
  | .mk ns idx (some q), b => by
      rw [verifier.MemoryId.prepended.eq_def]
      simp [memoryIdPrepend, MemoryId_prepended_spec q b]

theorem MemoryId_has_parent_spec (a b : MemoryId) :
    verifier.MemoryId.has_parent a b = ok (memoryIdHasParent a b) := by
  unfold verifier.MemoryId.has_parent memoryIdHasParent
  cases a with
  | mk ns idx p =>
    cases p with
    | none => rfl
    | some q =>
      simp only [verifier.MemoryId.parent._simpLemma_, MemoryId_eq_spec]
      by_cases h : q = b <;> simp [h]

theorem MemoryId_matches_spec :
    ∀ (a b : MemoryId), verifier.MemoryId.matches a b = ok (memoryIdMatches a b)
  | .mk ns1 id1 none, .mk ns2 id2 p2 => by
      rw [verifier.MemoryId.matches.eq_def]
      unfold memoryIdMatches
      simp only [verifier.MemoryId.namespace._simpLemma_,
        verifier.MemoryId.id._simpLemma_, verifier.MemoryId.parent._simpLemma_,
        Namespace_ne_spec, bind_ok]
      by_cases hns : ns1 = ns2
      · subst hns
        simp only [ne_eq, not_true_eq_false, decide_false, Bool.false_eq_true, ↓reduceIte]
        by_cases hid : id1 = id2
        · subst hid
          simp [bne_self_eq_false]
        · have hbne : (id1 != id2) = true := bne_iff_ne.2 hid
          simp [hbne]
      · have hbne : (ns1 != ns2) = true := bne_iff_ne.2 hns
        simp [hns, hbne]
  | .mk ns1 id1 (some q1), .mk ns2 id2 p2 => by
      rw [verifier.MemoryId.matches.eq_def]
      unfold memoryIdMatches
      simp only [verifier.MemoryId.namespace._simpLemma_,
        verifier.MemoryId.id._simpLemma_, verifier.MemoryId.parent._simpLemma_,
        Namespace_ne_spec, bind_ok]
      by_cases hns : ns1 = ns2
      · subst hns
        simp only [ne_eq, not_true_eq_false, decide_false, Bool.false_eq_true, ↓reduceIte]
        by_cases hid : id1 = id2
        · subst hid
          simp only [bne_self_eq_false, Bool.false_eq_true, ↓reduceIte]
          cases p2 with
          | none => simp
          | some q2 =>
            simp only [Bool.false_or]
            exact MemoryId_matches_spec q1 q2
        · have hbne : (id1 != id2) = true := bne_iff_ne.2 hid
          simp [hbne]
      · have hbne : (ns1 != ns2) = true := bne_iff_ne.2 hns
        simp [hns, hbne]

theorem memoryIdMatches_refl : ∀ m : MemoryId, memoryIdMatches m m = true
  | .mk ns idx none => by simp [memoryIdMatches]
  | .mk ns idx (some q) => by simp [memoryIdMatches, memoryIdMatches_refl q]

theorem memoryIdMatches_prepend_right :
    ∀ (a b : MemoryId), memoryIdMatches a (memoryIdPrepend a b) = true
  | .mk ns idx none, b => by simp [memoryIdPrepend, memoryIdMatches]
  | .mk ns idx (some q), b => by
      simp [memoryIdPrepend, memoryIdMatches, memoryIdMatches_prepend_right q b]

/-! ### 2b. `MemoryId` Counter Boundedness & Injective Runtime ID Renaming -/

/-- Every `.Verification` node anywhere in `m`'s parent chain has `id < bound`. -/
def memIdBounded (bound : Nat) : MemoryId → Bool
  | .mk .Verification idx none => decide (idx.bv.toNat < bound)
  | .mk .Verification idx (some p) => decide (idx.bv.toNat < bound) && memIdBounded bound p
  | .mk .Hardcoded _ none | .mk .Generated _ none => true
  | .mk .Hardcoded _ (some p) | .mk .Generated _ (some p) => memIdBounded bound p

theorem memIdBounded_mono {b₁ b₂ : Nat} (hle : b₁ ≤ b₂) :
    ∀ {m : MemoryId}, memIdBounded b₁ m = true → memIdBounded b₂ m = true
  | .mk ns idx none, h => by
      cases ns <;> simp only [memIdBounded, decide_eq_true_eq] at h ⊢
      omega
  | .mk ns idx (some q), h => by
      cases ns <;> simp only [memIdBounded, Bool.and_eq_true, decide_eq_true_eq] at h ⊢
      · exact memIdBounded_mono hle h
      · exact memIdBounded_mono hle h
      · exact ⟨by omega, memIdBounded_mono hle h.2⟩

theorem memIdBounded_of_noVerif (bound : Nat) :
    ∀ {m : MemoryId}, memoryIdNoVerif m = true → memIdBounded bound m = true
  | .mk ns idx none, h => by
      cases ns <;> simp [memoryIdNoVerif, memIdBounded] at h ⊢
  | .mk ns idx (some q), h => by
      cases ns <;> simp [memoryIdNoVerif, memIdBounded] at h ⊢
      · exact memIdBounded_of_noVerif bound h
      · exact memIdBounded_of_noVerif bound h

theorem memIdBounded_prepend {bound : Nat} :
    ∀ {a b : MemoryId},
      memIdBounded bound a = true → memIdBounded bound b = true →
      memIdBounded bound (memoryIdPrepend a b) = true
  | .mk ns idx none, b, ha, hb => by
      cases ns <;> simp only [memoryIdPrepend, memIdBounded,
        Bool.and_eq_true, decide_eq_true_eq] at ha ⊢
      · exact hb
      · exact hb
      · exact ⟨ha, hb⟩
  | .mk ns idx (some q), b, ha, hb => by
      cases ns <;> simp only [memoryIdPrepend, memIdBounded,
        Bool.and_eq_true, decide_eq_true_eq] at ha ⊢
      · exact memIdBounded_prepend ha hb
      · exact memIdBounded_prepend ha hb
      · exact ⟨ha.1, memIdBounded_prepend ha.2 hb⟩

theorem prepend_root_ns (a b : MemoryId) :
    (memoryIdPrepend a b).namespace = a.namespace := by
  cases a with
  | mk ns idx p =>
    cases p <;> rfl

theorem noVerif_root_ne_verif {m : MemoryId} (h : memoryIdNoVerif m = true) :
    m.namespace ≠ .Verification := by
  cases m with
  | mk ns idx p =>
    cases ns <;> simp [memoryIdNoVerif] at h ⊢

def renameNamespaceId (ρ : Std.U64 → Std.U64) (ns : verifier.Namespace) (idx : Std.U64) : Std.U64 :=
  match ns with
  | .Verification => ρ idx
  | .Hardcoded | .Generated => idx

/-- Applies `ρ` to all `.Verification` counter nodes in a `MemoryId` chain while
    leaving external `.Hardcoded`/`.Generated` nodes untouched. -/
def renameMemId (ρ : Std.U64 → Std.U64) : MemoryId → MemoryId
  | .mk ns idx none => .mk ns (renameNamespaceId ρ ns idx) none
  | .mk ns idx (some p) => .mk ns (renameNamespaceId ρ ns idx) (some (renameMemId ρ p))

theorem renameMemId_root_ns (ρ : Std.U64 → Std.U64) (m : MemoryId) :
    (renameMemId ρ m).namespace = m.namespace := by
  cases m with
  | mk ns idx p =>
    cases p <;> rfl

theorem renameMemId_id : ∀ m : MemoryId, renameMemId (fun x => x) m = m
  | .mk ns idx none => by
      cases ns <;> simp [renameMemId, renameNamespaceId]
  | .mk ns idx (some q) => by
      cases ns <;> simp [renameMemId, renameNamespaceId, renameMemId_id q]

theorem renameMemId_noVerif_id (ρ : Std.U64 → Std.U64) :
    ∀ {m : MemoryId}, memoryIdNoVerif m = true → renameMemId ρ m = m
  | .mk ns idx none, h => by
      cases ns <;> simp [memoryIdNoVerif, renameMemId, renameNamespaceId] at h ⊢
  | .mk ns idx (some q), h => by
      cases ns <;> simp [memoryIdNoVerif, renameMemId, renameNamespaceId] at h ⊢
      · exact renameMemId_noVerif_id ρ h
      · exact renameMemId_noVerif_id ρ h

theorem renameMemId_prepend (ρ : Std.U64 → Std.U64) :
    ∀ (a b : MemoryId),
      renameMemId ρ (memoryIdPrepend a b) = memoryIdPrepend (renameMemId ρ a) (renameMemId ρ b)
  | .mk ns idx none, b => by
      simp [memoryIdPrepend, renameMemId]
  | .mk ns idx (some q), b => by
      simp [memoryIdPrepend, renameMemId, renameMemId_prepend ρ q b]

theorem renameMemId_matches_noVerif (ρ : Std.U64 → Std.U64) :
    ∀ {schemaId : MemoryId} (m : MemoryId),
      memoryIdNoVerif schemaId = true →
      memoryIdMatches schemaId (renameMemId ρ m) = memoryIdMatches schemaId m
  | .mk ns1 id1 none, .mk ns2 id2 none, h => by
      have hns1 : ns1 ≠ .Verification := noVerif_root_ne_verif h
      cases ns2 <;> simp [renameMemId, renameNamespaceId, memoryIdMatches]
      by_cases heq : ns1 = .Verification
      · exact False.elim (hns1 heq)
      · simp [heq]
  | .mk ns1 id1 none, .mk ns2 id2 (some q2), h => by
      have hns1 : ns1 ≠ .Verification := noVerif_root_ne_verif h
      cases ns2 with
      | Hardcoded | Generated =>
        simp [renameMemId, renameNamespaceId, memoryIdMatches]
      | Verification =>
        have hne : (ns1 != .Verification) = true := bne_iff_ne.2 hns1
        simp [renameMemId, renameNamespaceId, memoryIdMatches, hne]
  | .mk ns1 id1 (some q1), .mk ns2 id2 none, h => by
      have hns1 : ns1 ≠ .Verification := noVerif_root_ne_verif h
      cases ns2 <;> simp [renameMemId, renameNamespaceId, memoryIdMatches]
      by_cases heq : ns1 = .Verification
      · exact False.elim (hns1 heq)
      · simp [heq]
  | .mk ns1 id1 (some q1), .mk ns2 id2 (some q2), h => by
      have hns1 : ns1 ≠ .Verification := noVerif_root_ne_verif h
      cases ns2 with
      | Hardcoded =>
        cases ns1 <;> simp [memoryIdNoVerif] at h
        · simp [renameMemId, renameNamespaceId, memoryIdMatches,
            renameMemId_matches_noVerif ρ q2 h]
        · simp [renameMemId, renameNamespaceId, memoryIdMatches]
      | Generated =>
        cases ns1 <;> simp [memoryIdNoVerif] at h
        · simp [renameMemId, renameNamespaceId, memoryIdMatches]
        · simp [renameMemId, renameNamespaceId, memoryIdMatches,
            renameMemId_matches_noVerif ρ q2 h]
      | Verification =>
        have hne : (ns1 != .Verification) = true := bne_iff_ne.2 hns1
        simp [renameMemId, renameNamespaceId, memoryIdMatches, hne]

theorem renameMemId_congr {ρ₁ ρ₂ : Std.U64 → Std.U64} {bound : Nat}
    (hagree : ∀ x : Std.U64, x.bv.toNat < bound → ρ₁ x = ρ₂ x) :
    ∀ {m : MemoryId}, memIdBounded bound m = true → renameMemId ρ₁ m = renameMemId ρ₂ m
  | .mk ns idx none, hb => by
      cases ns <;> simp only [memIdBounded, decide_eq_true_eq, renameMemId, renameNamespaceId] at hb ⊢
      rw [hagree idx hb]
  | .mk ns idx (some q), hb => by
      cases ns <;> simp only [memIdBounded, Bool.and_eq_true,
        decide_eq_true_eq, renameMemId, renameNamespaceId] at hb ⊢
      · congr 2; exact renameMemId_congr hagree hb
      · congr 2; exact renameMemId_congr hagree hb
      · rw [hagree idx hb.1, renameMemId_congr hagree hb.2]

/-- `ρ` injectively maps verifier counters `< vCounter` into either runtime allocation IDs
    `< rtCounter` or reserved unused-branch IDs `2 ^ 63 + x.bv.toNat`. -/
def RhoValid (vCounter rtCounter : Nat) (ρ : Std.U64 → Std.U64) : Prop :=
  (∀ x y : Std.U64, x.bv.toNat < vCounter → y.bv.toNat < vCounter → ρ x = ρ y → x = y) ∧
  (∀ x : Std.U64, x.bv.toNat < vCounter →
    (ρ x).bv.toNat < rtCounter ∨ (x.bv.toNat < 2 ^ 63 ∧ (ρ x).bv.toNat = 2 ^ 63 + x.bv.toNat))

theorem RhoValid_zero (rtCounter : Nat) : RhoValid 0 rtCounter (fun x => x) :=
  ⟨fun _ _ hx => False.elim (Nat.not_lt_zero _ hx),
   fun _ hx => False.elim (Nat.not_lt_zero _ hx)⟩

theorem RhoValid_mono_left {v₁ v₂ rt : Nat} {ρ : Std.U64 → Std.U64}
    (hle : v₁ ≤ v₂) (h : RhoValid v₂ rt ρ) : RhoValid v₁ rt ρ :=
  ⟨fun x y hx hy heq => h.1 x y (Nat.lt_of_lt_of_le hx hle) (Nat.lt_of_lt_of_le hy hle) heq,
   fun x hx => h.2 x (Nat.lt_of_lt_of_le hx hle)⟩

theorem RhoValid_mono_right {v rt₁ rt₂ : Nat} {ρ : Std.U64 → Std.U64}
    (hle : rt₁ ≤ rt₂) (h : RhoValid v rt₁ ρ) : RhoValid v rt₂ ρ :=
  ⟨h.1, fun x hx => (h.2 x hx).imp (fun hlt => Nat.lt_of_lt_of_le hlt hle) id⟩

theorem RhoValid_lift {v₁ v₂ rt : Nat} {ρ : Std.U64 → Std.U64}
    (_hle : v₁ ≤ v₂) (hv₂ : v₂ < 2 ^ 63) (hrt : rt ≤ 2 ^ 63)
    (hρ : RhoValid v₁ rt ρ) :
    ∃ ρ' : Std.U64 → Std.U64,
      RhoValid v₂ rt ρ' ∧
      (∀ x : Std.U64, x.bv.toNat < v₁ → ρ x = ρ' x) := by
  let ρ' : Std.U64 → Std.U64 := fun x =>
    if x.bv.toNat < v₁ then ρ x else ⟨BitVec.ofNat 64 (2 ^ 63 + x.bv.toNat)⟩
  have hval : ∀ z : Std.U64, z.bv.toNat < v₂ →
      (⟨BitVec.ofNat 64 (2 ^ 63 + z.bv.toNat)⟩ : Std.U64).bv.toNat = 2 ^ 63 + z.bv.toNat := by
    intro z hz
    change (BitVec.ofNat 64 (2 ^ 63 + z.bv.toNat)).toNat = 2 ^ 63 + z.bv.toNat
    rw [BitVec.toNat_ofNat]
    exact Nat.mod_eq_of_lt (by omega)
  refine ⟨ρ', ⟨?_, ?_⟩, fun x hx => by simp only [ρ', hx, ↓reduceIte]⟩
  · intro x y hx hy heq
    by_cases hx₁ : x.bv.toNat < v₁ <;> by_cases hy₁ : y.bv.toNat < v₁
    · simp only [ρ', hx₁, hy₁, ↓reduceIte] at heq
      exact hρ.1 x y hx₁ hy₁ heq
    · simp only [ρ', hx₁, hy₁, ↓reduceIte] at heq
      have h_eq : (ρ x).bv.toNat = 2 ^ 63 + y.bv.toNat := by
        rw [heq]; exact hval y hy
      rcases hρ.2 x hx₁ with hlt | ⟨_, hhi⟩ <;> omega
    · simp only [ρ', hx₁, hy₁, ↓reduceIte] at heq
      have h_eq : 2 ^ 63 + x.bv.toNat = (ρ y).bv.toNat := by
        rw [← hval x hx, heq]
      rcases hρ.2 y hy₁ with hlt | ⟨_, hhi⟩ <;> omega
    · simp only [ρ', hx₁, hy₁, ↓reduceIte] at heq
      have h_eq : 2 ^ 63 + x.bv.toNat = 2 ^ 63 + y.bv.toNat := by
        rw [← hval x hx, heq, hval y hy]
      exact (u64_eq_iff_bv _ _).2 (BitVec.eq_of_toNat_eq (by omega))
  · intro x hx
    by_cases hx₁ : x.bv.toNat < v₁
    · simp only [ρ', hx₁, ↓reduceIte]
      exact hρ.2 x hx₁
    · simp only [ρ', hx₁, ↓reduceIte]
      exact Or.inr ⟨by omega, hval x hx⟩

theorem RhoValid_extend {v rt rt' : Nat} {ρ : Std.U64 → Std.U64}
    {oldId newId : Std.U64}
    (hρ : RhoValid v rt ρ)
    (hold : oldId.bv.toNat = v)
    (hge : rt ≤ newId.bv.toNat)
    (hlt : newId.bv.toNat < rt')
    (hlt63 : newId.bv.toNat < 2 ^ 63) :
    let ρ' := fun x => if x = oldId then newId else ρ x
    RhoValid (v + 1) rt' ρ' ∧
    (∀ x : Std.U64, x.bv.toNat < v → ρ' x = ρ x) ∧
    ρ' oldId = newId := by
  dsimp only
  have hag : ∀ x : Std.U64, x.bv.toNat < v → (if x = oldId then newId else ρ x) = ρ x := by
    intro x hx
    have hne : x ≠ oldId := by intro heq; subst heq; omega
    simp [hne]
  refine ⟨⟨?_, ?_⟩, hag, by simp⟩
  · intro x y hx hy heq
    by_cases hxo : x = oldId
    · by_cases hyo : y = oldId
      · exact hxo.trans hyo.symm
      · subst hxo
        simp only [↓reduceIte, hyo] at heq
        have hy_lt : y.bv.toNat < v := by
          have : y.bv.toNat ≠ v := by
            intro hc
            apply hyo
            exact (u64_eq_iff_bv _ _).2 (BitVec.eq_of_toNat_eq (hc.trans hold.symm))
          omega
        have h_eq : newId.bv.toNat = (ρ y).bv.toNat := by rw [heq]
        rcases hρ.2 y hy_lt with hlt_rt | ⟨_, hhi⟩ <;> omega
    · by_cases hyo : y = oldId
      · subst hyo
        simp only [hxo, ↓reduceIte] at heq
        have hx_lt : x.bv.toNat < v := by
          have : x.bv.toNat ≠ v := by
            intro hc
            apply hxo
            exact (u64_eq_iff_bv _ _).2 (BitVec.eq_of_toNat_eq (hc.trans hold.symm))
          omega
        have h_eq : (ρ x).bv.toNat = newId.bv.toNat := by rw [heq]
        rcases hρ.2 x hx_lt with hlt_rt | ⟨_, hhi⟩ <;> omega
      · simp only [hxo, hyo, ↓reduceIte] at heq
        have hx_lt : x.bv.toNat < v := by
          have : x.bv.toNat ≠ v := by
            intro hc; apply hxo
            exact (u64_eq_iff_bv _ _).2 (BitVec.eq_of_toNat_eq (hc.trans hold.symm))
          omega
        have hy_lt : y.bv.toNat < v := by
          have : y.bv.toNat ≠ v := by
            intro hc; apply hyo
            exact (u64_eq_iff_bv _ _).2 (BitVec.eq_of_toNat_eq (hc.trans hold.symm))
          omega
        exact hρ.1 x y hx_lt hy_lt heq
  · intro x hx
    by_cases hxo : x = oldId
    · subst hxo; simp only [↓reduceIte]; exact Or.inl hlt
    · simp only [hxo, ↓reduceIte]
      have hx_lt : x.bv.toNat < v := by
        have : x.bv.toNat ≠ v := by
          intro hc; apply hxo
          exact (u64_eq_iff_bv _ _).2 (BitVec.eq_of_toNat_eq (hc.trans hold.symm))
        omega
      exact (hρ.2 x hx_lt).imp
        (fun hlt_rt => Nat.lt_of_lt_of_le hlt_rt (Nat.le_trans hge (Nat.le_of_lt hlt))) id

theorem renameMemId_inj_on_bounded {v rt : Nat} {ρ : Std.U64 → Std.U64}
    (hρ : RhoValid v rt ρ) :
    ∀ {a b : MemoryId},
      memIdBounded v a = true → memIdBounded v b = true →
      renameMemId ρ a = renameMemId ρ b → a = b
  | .mk ns1 id1 none, .mk ns2 id2 none, ha, hb, heq => by
      simp only [renameMemId] at heq
      injection heq with hns hid _
      subst hns
      cases ns1
      · simp only [renameNamespaceId] at hid; subst hid; rfl
      · simp only [renameNamespaceId] at hid; subst hid; rfl
      · simp only [renameNamespaceId] at hid
        simp only [memIdBounded, decide_eq_true_eq] at ha hb
        rw [hρ.1 id1 id2 ha hb hid]
  | .mk ns1 id1 none, .mk ns2 id2 (some q2), _, _, heq => by
      simp only [renameMemId] at heq
      injection heq with _ _ hp
      cases hp
  | .mk ns1 id1 (some q1), .mk ns2 id2 none, _, _, heq => by
      simp only [renameMemId] at heq
      injection heq with _ _ hp
      cases hp
  | .mk ns1 id1 (some q1), .mk ns2 id2 (some q2), ha, hb, heq => by
      simp only [renameMemId] at heq
      injection heq with hns hid hp
      injection hp with hq
      subst hns
      cases ns1 <;> simp only [memIdBounded, Bool.and_eq_true, decide_eq_true_eq] at ha hb <;>
        simp only [renameNamespaceId] at hid
      · rw [hid, renameMemId_inj_on_bounded hρ ha hb hq]
      · rw [hid, renameMemId_inj_on_bounded hρ ha hb hq]
      · rw [hρ.1 id1 id2 ha.1 hb.1 hid, renameMemId_inj_on_bounded hρ ha.2 hb.2 hq]

theorem renameMemId_hasParent {v rt : Nat} {ρ : Std.U64 → Std.U64}
    (hρ : RhoValid v rt ρ) {a b : MemoryId}
    (ha : memIdBounded v a = true) (hb : memIdBounded v b = true) :
    memoryIdHasParent (renameMemId ρ a) (renameMemId ρ b) = memoryIdHasParent a b := by
  cases a with
  | mk ns idx p =>
    cases p with
    | none => rfl
    | some q =>
      have hq : memIdBounded v q = true := by
        cases ns <;> simp only [memIdBounded, Bool.and_eq_true] at ha
        · exact ha
        · exact ha
        · exact ha.2
      simp only [renameMemId, memoryIdHasParent]
      by_cases heq : q = b
      · subst heq; simp
      · have hne : renameMemId ρ q ≠ renameMemId ρ b := by
          intro hc; exact heq (renameMemId_inj_on_bounded hρ hq hb hc)
        simp [heq, hne]

theorem renameMemId_matches {v rt : Nat} {ρ : Std.U64 → Std.U64}
    (hρ : RhoValid v rt ρ) :
    ∀ {a b : MemoryId},
      memIdBounded v a = true → memIdBounded v b = true →
      memoryIdMatches (renameMemId ρ a) (renameMemId ρ b) = memoryIdMatches a b
  | .mk ns1 id1 none, .mk ns2 id2 p2, ha, hb => by
      by_cases hns : ns1 = ns2
      · subst hns
        have hid_bne : (renameNamespaceId ρ ns1 id1 != renameNamespaceId ρ ns1 id2) = (id1 != id2) := by
          cases ns1 with
          | Hardcoded | Generated => rfl
          | Verification =>
            have h1 : id1.bv.toNat < v := by
              simp only [memIdBounded, decide_eq_true_eq] at ha; exact ha
            have h2 : id2.bv.toNat < v := by
              cases p2 <;> simp only [memIdBounded, Bool.and_eq_true, decide_eq_true_eq] at hb
              · exact hb
              · exact hb.1
            by_cases hi : id1 = id2
            · subst hi; simp
            · have hne : ρ id1 ≠ ρ id2 := fun hc => hi (hρ.1 id1 id2 h1 h2 hc)
              simp [renameNamespaceId, bne_iff_ne.2 hi, bne_iff_ne.2 hne]
        cases p2 <;> simp [renameMemId, memoryIdMatches, hid_bne]
      · have hbne : (ns1 != ns2) = true := bne_iff_ne.2 hns
        cases p2 <;> simp [renameMemId, memoryIdMatches, hbne]
  | .mk ns1 id1 (some q1), .mk ns2 id2 p2, ha, hb => by
      by_cases hns : ns1 = ns2
      · subst hns
        have hid_bne : (renameNamespaceId ρ ns1 id1 != renameNamespaceId ρ ns1 id2) = (id1 != id2) := by
          cases ns1 with
          | Hardcoded | Generated => rfl
          | Verification =>
            have h1 : id1.bv.toNat < v := by
              simp only [memIdBounded, Bool.and_eq_true, decide_eq_true_eq] at ha; exact ha.1
            have h2 : id2.bv.toNat < v := by
              cases p2 <;> simp only [memIdBounded, Bool.and_eq_true, decide_eq_true_eq] at hb
              · exact hb
              · exact hb.1
            by_cases hi : id1 = id2
            · subst hi; simp
            · have hne : ρ id1 ≠ ρ id2 := fun hc => hi (hρ.1 id1 id2 h1 h2 hc)
              simp [renameNamespaceId, bne_iff_ne.2 hi, bne_iff_ne.2 hne]
        cases p2 with
        | none => simp [renameMemId, memoryIdMatches, hid_bne]
        | some q2 =>
          have hq1 : memIdBounded v q1 = true := by
            cases ns1 <;> simp only [memIdBounded, Bool.and_eq_true] at ha
            · exact ha
            · exact ha
            · exact ha.2
          have hq2 : memIdBounded v q2 = true := by
            cases ns1 <;> simp only [memIdBounded, Bool.and_eq_true] at hb
            · exact hb
            · exact hb
            · exact hb.2
          simp [renameMemId, memoryIdMatches, hid_bne, renameMemId_matches hρ hq1 hq2]
      · have hbne : (ns1 != ns2) = true := bne_iff_ne.2 hns
        cases p2 <;> simp [renameMemId, memoryIdMatches, hbne]

theorem freshRuntimeId_ne_renamed {v rt allocIdx : Nat} {ρ : Std.U64 → Std.U64}
    (hρ : RhoValid v rt ρ) (hge : rt ≤ allocIdx) (hlt : allocIdx < 2 ^ 63)
    {m : MemoryId} (hm : memIdBounded v m = true) :
    renameMemId ρ m ≠ freshRuntimeId allocIdx := by
  intro heq
  cases m with
  | mk ns idx p =>
    cases p with
    | none =>
      cases ns <;> simp [renameMemId, renameNamespaceId, freshRuntimeId] at heq
      simp only [memIdBounded, decide_eq_true_eq] at hm
      have h2 : (ρ idx).bv.toNat = allocIdx := by
        rw [heq]
        change (BitVec.ofNat 64 allocIdx).toNat = allocIdx
        rw [BitVec.toNat_ofNat]
        exact Nat.mod_eq_of_lt (by omega)
      rcases hρ.2 idx hm with h1 | ⟨_, h1⟩ <;> omega
    | some _ =>
      simp [renameMemId, freshRuntimeId] at heq

/-! ## 3. Structural Validity Predicates (`IdSetValid`, `BoundsMapValid`, `TypeValid`, `CompCtxValid`) -/

def popcountRange (b : BitVec 64) : Nat → Nat
  | 0 => 0
  | n + 1 => popcountRange b n + (if b.getLsbD n then 1 else 0)

abbrev popcount64 (b : BitVec 64) : Nat := popcountRange b 64

abbrev stackPos (b : BitVec 64) (idx : Nat) : Nat := popcountRange b idx

theorem popcountRange_le (b : BitVec 64) (n : Nat) : popcountRange b n ≤ n := by
  induction n with
  | zero => exact Nat.le_refl 0
  | succ k ih =>
    simp only [popcountRange]
    split_ifs <;> omega

theorem popcountRange_mono (b : BitVec 64) {m n : Nat} (hle : m ≤ n) :
    popcountRange b m ≤ popcountRange b n := by
  induction n with
  | zero =>
    have : m = 0 := by omega
    subst this
    exact Nat.le_refl _
  | succ k ih =>
    by_cases hm : m ≤ k
    · have h1 := ih hm
      simp only [popcountRange]
      split_ifs <;> omega
    · have : m = k + 1 := by omega
      subst this
      exact Nat.le_refl _

theorem stackPos_lt_popcountRange (b : BitVec 64) {idx n : Nat}
    (hlt : idx < n) (hbit : b.getLsbD idx = true) :
    popcountRange b idx < popcountRange b n := by
  have h1 : popcountRange b idx < popcountRange b (idx + 1) := by
    simp [popcountRange, hbit]
  exact Nat.lt_of_lt_of_le h1 (popcountRange_mono b hlt)

theorem stackPos_lt_popcount64 (b : BitVec 64) {idx : Nat}
    (hlt : idx < 64) (hbit : b.getLsbD idx = true) :
    stackPos b idx < popcount64 b :=
  stackPos_lt_popcountRange b hlt hbit

/-- Well-formedness of an `IdSet`: strictly sorted by `MemoryIdLt` and counter-bounded. -/
def IdSetValid (vCounter : Nat) (s : verifier.IdSet) : Prop :=
  List.Pairwise MemoryIdLt s.entries.val ∧
  (∀ m ∈ s.entries.val, memIdBounded vCounter m = true)

theorem IdSetValid_mono {v₁ v₂ : Nat} (hle : v₁ ≤ v₂) {s : verifier.IdSet}
    (h : IdSetValid v₁ s) : IdSetValid v₂ s :=
  ⟨h.1, fun m hm => memIdBounded_mono hle (h.2 m hm)⟩

/-- Well-formedness of a `BoundsMap`: keys strictly sorted by `MemoryIdLt` and counter-bounded. -/
def BoundsMapValid (vCounter : Nat) (bm : verifier.BoundsMap) : Prop :=
  List.Pairwise MemoryIdLt (bm.entries.val.map Prod.fst) ∧
  (∀ p ∈ bm.entries.val, memIdBounded vCounter p.1 = true)

theorem BoundsMapValid_mono {v₁ v₂ : Nat} (hle : v₁ ≤ v₂) {bm : verifier.BoundsMap}
    (h : BoundsMapValid v₁ bm) : BoundsMapValid v₂ bm :=
  ⟨h.1, fun p hp => memIdBounded_mono hle (h.2 p hp)⟩

/-- Well-formedness of a pointer offset abstract scalar: valid tnum/range, fully initialized,
    and bounded above by `limit`. -/
def PtrOffsetValid (limit : BitVec 64) (off : U64ScalarValueData) : Prop :=
  Scalar64Valid off ∧ off.unwritten_mask.bv = 0#64 ∧ off.urange.max.bv ≤ limit

theorem PtrOffsetValid_mono {l₁ l₂ : BitVec 64} (hle : l₁ ≤ l₂) {off : U64ScalarValueData}
    (h : PtrOffsetValid l₁ off) : PtrOffsetValid l₂ off :=
  ⟨h.1, h.2.1, BitVec.le_trans h.2.2 hle⟩

/-- Shapes permitted inside `Type.NullOr`. -/
def NullOrInnerValid : verifier.Type → Prop
  | .PtrToMemory _ _ _ _ => True
  | .PtrToStruct _ _ _ => True
  | .Releasable _ (.PtrToMemory _ _ _ _) => True
  | .Releasable _ (.PtrToStruct _ _ _) => True
  | _ => False

/-- Structural well-formedness of an abstract runtime `verifier.Type` in registers or stack. -/
def TypeValid (env : ExecEnv) (vCounter : Nat) : verifier.Type → Prop
  | .ScalarValue d => Scalar64Valid d
  | .ConstPtrToMap mapIdx schema =>
      env.maps mapIdx.bv.toNat = some (MapSpec.ofCore schema)
  | .PtrToStack off =>
      PtrOffsetValid 0xFFFF_FFFF#64 off
  | .PtrToMemory id off _ _ =>
      memIdBounded vCounter id = true ∧ PtrOffsetValid 0xFFFF_FFFF#64 off
  | .PtrToStruct id off sd =>
      memIdBounded vCounter id = true ∧ PtrOffsetValid 0xFFFF_FFFF#64 off ∧
      (∃ schemaId, StructDescConforms env schemaId sd ∧ memoryIdMatches schemaId id = true)
  | .PtrToArray id off =>
      memIdBounded vCounter id = true ∧ PtrOffsetValid MAX_PACKET_OFFSET off
  | .PtrToEndArray id =>
      memIdBounded vCounter id = true
  | .Releasable relId (.PtrToMemory id off sz orig) =>
      memIdBounded vCounter relId = true ∧
      TypeValid env vCounter (.PtrToMemory id off sz orig)
  | .Releasable relId (.PtrToStruct id off sd) =>
      memIdBounded vCounter relId = true ∧
      TypeValid env vCounter (.PtrToStruct id off sd)
  | .NullOr nullId inner =>
      memIdBounded vCounter nullId = true ∧ NullOrInnerValid inner ∧
      TypeValid env vCounter inner
  | _ => False

theorem TypeValid_mono {env : ExecEnv} {v₁ v₂ : Nat} (hle : v₁ ≤ v₂) {t : verifier.Type}
    (h : TypeValid env v₁ t) : TypeValid env v₂ t := by
  induction t with
  | ScalarValue _ | ConstPtrToMap _ _ | PtrToStack _ => exact h
  | PtrToMemory id off _ _ => exact ⟨memIdBounded_mono hle h.1, h.2⟩
  | PtrToStruct id off sd => exact ⟨memIdBounded_mono hle h.1, h.2.1, h.2.2⟩
  | PtrToArray id off => exact ⟨memIdBounded_mono hle h.1, h.2⟩
  | PtrToEndArray id => exact memIdBounded_mono hle h
  | NullOr nullId inner ih => exact ⟨memIdBounded_mono hle h.1, h.2.1, ih h.2.2⟩
  | Releasable relId inner ih =>
    cases inner <;> try exact False.elim h
    · exact ⟨memIdBounded_mono hle h.1, ih h.2⟩
    · exact ⟨memIdBounded_mono hle h.1, ih h.2⟩
  | _ => exact False.elim h

/-- Structural well-formedness of `verifier.Stack`. -/
def StackValid (env : ExecEnv) (vCounter : Nat) (s : verifier.Stack) : Prop :=
  s.data.val.length = popcount64 s.mask.bv ∧
  (∀ t ∈ s.data.val, TypeValid env vCounter t)

theorem StackValid_mono {env : ExecEnv} {v₁ v₂ : Nat} (hle : v₁ ≤ v₂) {s : verifier.Stack}
    (h : StackValid env v₁ s) : StackValid env v₂ s :=
  ⟨h.1, fun t ht => TypeValid_mono hle (h.2 t ht)⟩

/-- Pure lookup of stack slot `slot` (`0..63`) in `verifier.Stack`. -/
def stackSlotType (s : verifier.Stack) (slot : Nat) : verifier.Type :=
  if s.mask.bv.getLsbD slot then
    s.data.val.getD (stackPos s.mask.bv slot) (.ScalarValue uninitScalar64)
  else
    .ScalarValue uninitScalar64

theorem stackSlotType_valid {env : ExecEnv} {vCounter : Nat} {s : verifier.Stack}
    (hs : StackValid env vCounter s) (slot : Nat) (hslot : slot < 64) :
    TypeValid env vCounter (stackSlotType s slot) := by
  unfold stackSlotType
  by_cases hbit : s.mask.bv.getLsbD slot = true
  · simp only [hbit, ↓reduceIte]
    have hpos : stackPos s.mask.bv slot < s.data.val.length := by
      rw [hs.1]; exact stackPos_lt_popcount64 s.mask.bv hslot hbit
    rw [List.getD_eq_getElem _ _ hpos]
    exact hs.2 _ (List.getElem_mem hpos)
  · simp only [hbit, Bool.false_eq_true, ↓reduceIte, TypeValid]
    exact uninitScalar64_valid

/-- Structural well-formedness of `verifier.ComputationContext`. -/
def CompCtxValid (env : ExecEnv) (vCounter : Nat) (ctx : verifier.ComputationContext) : Prop :=
  (∀ t ∈ ctx.registers.val, TypeValid env vCounter t) ∧
  StackValid env vCounter ctx.stack ∧
  BoundsMapValid vCounter ctx.array_bounds ∧
  IdSetValid vCounter ctx.resources

theorem CompCtxValid_mono {env : ExecEnv} {v₁ v₂ : Nat} (hle : v₁ ≤ v₂)
    {ctx : verifier.ComputationContext} (h : CompCtxValid env v₁ ctx) :
    CompCtxValid env v₂ ctx :=
  ⟨fun t ht => TypeValid_mono hle (h.1 t ht),
   StackValid_mono hle h.2.1,
   BoundsMapValid_mono hle h.2.2.1,
   IdSetValid_mono hle h.2.2.2⟩

/-! ## 4. Relational 2-Safety Predicates (`TypeRel2`, `StackSlotRel2`, `StateRel2Deps`) -/

/-- When a `NullOr nullId inner` pointer is null at runtime, any `Releasable relId _`
    wrapped inside `inner` was never allocated (`resActive (renameMemId ρ relId) = false`). -/
def nullInnerInactive (ρ : Std.U64 → Std.U64) (resActive : MemoryId → Bool) :
    verifier.Type → Prop
  | .Releasable relId _ => resActive (renameMemId ρ relId) = false
  | _ => True

/-- Relational 2-safety correspondence between an abstract `verifier.Type` and concrete
    runtime values `v₁, v₂` in two executions. -/
def TypeRel2 (env : ExecEnv) (ρ : Std.U64 → Std.U64)
    (resActive isNullId : MemoryId → Bool)
    (arrayLen : MemoryId → BitVec 64) :
    verifier.Type → RuntimeVal → RuntimeVal → Prop
  | .ScalarValue d, v₁, v₂ =>
      if d = uninitScalar64 then
        True
      else
        ∃ x₁ x₂ : BitVec 64,
          v₁ = .scalar x₁ ∧ v₂ = .scalar x₂ ∧ ConcretizesScalar64Rel2 d x₁ x₂
  | .ConstPtrToMap mapIdx schema, v₁, v₂ =>
      env.maps mapIdx.bv.toNat = some (MapSpec.ofCore schema) ∧
      v₁ = .mapPtr mapIdx.bv.toNat ∧ v₂ = .mapPtr mapIdx.bv.toNat
  | .PtrToStack off, v₁, v₂ =>
      PtrOffsetValid 0xFFFF_FFFF#64 off ∧
      ∃ o : BitVec 64,
        ConcretizesScalar64Rel2 off o o ∧
        v₁ = .stackPtr o ∧ v₂ = .stackPtr o
  | .PtrToMemory id off sz orig, v₁, v₂ =>
      PtrOffsetValid 0xFFFF_FFFF#64 off ∧
      ∃ o : BitVec 64,
        ConcretizesScalar64Rel2 off o o ∧
        v₁ = .memPtr (renameMemId ρ id) (MemoryOrigin.ofCore orig) none sz.bv o ∧
        v₂ = .memPtr (renameMemId ρ id) (MemoryOrigin.ofCore orig) none sz.bv o
  | .PtrToStruct id off sd, v₁, v₂ =>
      PtrOffsetValid 0xFFFF_FFFF#64 off ∧
      (∃ schemaId, StructDescConforms env schemaId sd ∧ memoryIdMatches schemaId id = true) ∧
      ∃ o : BitVec 64,
        ConcretizesScalar64Rel2 off o o ∧
        v₁ = .structPtr (renameMemId ρ id) none o ∧
        v₂ = .structPtr (renameMemId ρ id) none o
  | .PtrToArray id off, v₁, v₂ =>
      PtrOffsetValid MAX_PACKET_OFFSET off ∧
      arrayLen (renameMemId ρ id) ≤ 0xFFFF_FFFF#64 ∧
      ∃ o : BitVec 64,
        ConcretizesScalar64Rel2 off o o ∧
        v₁ = .arrayPtr (renameMemId ρ id) o ∧
        v₂ = .arrayPtr (renameMemId ρ id) o
  | .PtrToEndArray id, v₁, v₂ =>
      arrayLen (renameMemId ρ id) ≤ 0xFFFF_FFFF#64 ∧
      v₁ = .endArrayPtr (renameMemId ρ id) ∧
      v₂ = .endArrayPtr (renameMemId ρ id)
  | .Releasable relId (.PtrToMemory id off sz orig), v₁, v₂ =>
      resActive (renameMemId ρ relId) = true ∧
      PtrOffsetValid 0xFFFF_FFFF#64 off ∧
      ∃ o : BitVec 64,
        ConcretizesScalar64Rel2 off o o ∧
        v₁ = .memPtr (renameMemId ρ id) (MemoryOrigin.ofCore orig) (some (renameMemId ρ relId)) sz.bv o ∧
        v₂ = .memPtr (renameMemId ρ id) (MemoryOrigin.ofCore orig) (some (renameMemId ρ relId)) sz.bv o
  | .Releasable relId (.PtrToStruct id off sd), v₁, v₂ =>
      resActive (renameMemId ρ relId) = true ∧
      PtrOffsetValid 0xFFFF_FFFF#64 off ∧
      (∃ schemaId, StructDescConforms env schemaId sd ∧ memoryIdMatches schemaId id = true) ∧
      ∃ o : BitVec 64,
        ConcretizesScalar64Rel2 off o o ∧
        v₁ = .structPtr (renameMemId ρ id) (some (renameMemId ρ relId)) o ∧
        v₂ = .structPtr (renameMemId ρ id) (some (renameMemId ρ relId)) o
  | .NullOr nullId inner, v₁, v₂ =>
      if isNullId (renameMemId ρ nullId) then
        nullInnerInactive ρ resActive inner ∧ v₁ = .scalar 0#64 ∧ v₂ = .scalar 0#64
      else
        TypeRel2 env ρ resActive isNullId arrayLen inner v₁ v₂
  | _, _, _ => False

/-- Relational 2-safety correspondence between an abstract stack slot `verifier.Type`
    and concrete `StackSlot` values `slot₁, slot₂` in two executions. -/
def StackSlotRel2 (env : ExecEnv) (ρ : Std.U64 → Std.U64)
    (resActive isNullId : MemoryId → Bool)
    (arrayLen : MemoryId → BitVec 64) :
    verifier.Type → StackSlot → StackSlot → Prop
  | .ScalarValue d, slot₁, slot₂ =>
      if d = uninitScalar64 then
        True
      else
        ∃ val₁ init₁ val₂ init₂ : BitVec 64,
          slot₁ = .bytes val₁ init₁ ∧
          slot₂ = .bytes val₂ init₂ ∧
          ConcretizesScalar64Rel2 d val₁ val₂ ∧
          ConcretizesScalar64Rel2 d (val₁ &&& init₁) (val₁ ||| ~~~init₁) ∧
          ConcretizesScalar64Rel2 d (val₂ &&& init₂) (val₂ ||| ~~~init₂)
  | .NullOr nullId inner, slot₁, slot₂ =>
      if isNullId (renameMemId ρ nullId) then
        nullInnerInactive ρ resActive inner ∧
        slot₁ = .bytes 0#64 (~~~0#64) ∧ slot₂ = .bytes 0#64 (~~~0#64)
      else
        StackSlotRel2 env ρ resActive isNullId arrayLen inner slot₁ slot₂
  | t, slot₁, slot₂ =>
      ∃ cap : RuntimeVal,
        cap.isScalar = false ∧
        slot₁ = .spilled cap ∧
        slot₂ = .spilled cap ∧
        TypeRel2 env ρ resActive isNullId arrayLen t cap cap

theorem StackSlotRel2_scalar_init_mask {d : scalar_value.U64ScalarValueData}
    {val init : BitVec 64}
    (h : ConcretizesScalar64Rel2 d (val &&& init) (val ||| ~~~init)) :
    ~~~d.unwritten_mask.bv &&& init = ~~~d.unwritten_mask.bv := by
  have hsec := h.2.2
  revert hsec
  bv_decide

theorem canonical_ptr_add_nonzero {base off : BitVec 64}
    (hbase_lo : 0#64 < base) (hbase_hi : base ≤ CANONICAL_USER_MAX)
    (hoff : off ≤ 0xFFFF_FFFF#64) :
    base + off ≠ 0#64 ∧ ¬ BitVec.uaddOverflow base off := by
  unfold CANONICAL_USER_MAX at hbase_hi
  constructor <;> bv_decide

theorem TypeRel2_ptr_nonscalar_and_nonzero
    {env : ExecEnv} {vCounter : Nat} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool}
    {arrayLen : MemoryId → BitVec 64} {layout : HostLayout}
    (hlayout : ValidHostLayout layout)
    {t : verifier.Type} {v₁ v₂ : RuntimeVal}
    (ht : TypeValid env vCounter t)
    (hrel : TypeRel2 env ρ resActive isNullId arrayLen t v₁ v₂)
    (hnonnull : match t with
      | .ScalarValue _ => False
      | .NullOr nullId _ => isNullId (renameMemId ρ nullId) = false
      | _ => True) :
    v₁ = v₂ ∧ v₁.isScalar = false ∧ v₁.toBits layout arrayLen ≠ 0#64 := by
  induction t with
  | ScalarValue _ => exact False.elim hnonnull
  | ConstPtrToMap mapIdx schema =>
    rcases hrel with ⟨_, rfl, rfl⟩
    exact ⟨rfl, rfl, hlayout.2.2.2.2 _⟩
  | PtrToStack off =>
    rcases hrel with ⟨hp, o, hconc, rfl, rfl⟩
    have ho : o ≤ 0xFFFF_FFFF#64 := BitVec.le_trans hconc.1.2.2.2 hp.2.2
    exact ⟨rfl, rfl, (canonical_ptr_add_nonzero hlayout.1.1 hlayout.1.2 ho).1⟩
  | PtrToMemory id off sz orig =>
    rcases hrel with ⟨hp, o, hconc, rfl, rfl⟩
    have ho : o ≤ 0xFFFF_FFFF#64 := BitVec.le_trans hconc.1.2.2.2 hp.2.2
    have hb := hlayout.2.1 (renameMemId ρ id)
    exact ⟨rfl, rfl, (canonical_ptr_add_nonzero hb.1 hb.2 ho).1⟩
  | PtrToStruct id off sd =>
    rcases hrel with ⟨hp, _, o, hconc, rfl, rfl⟩
    have ho : o ≤ 0xFFFF_FFFF#64 := BitVec.le_trans hconc.1.2.2.2 hp.2.2
    have hb := hlayout.2.2.1 (renameMemId ρ id)
    exact ⟨rfl, rfl, (canonical_ptr_add_nonzero hb.1 hb.2 ho).1⟩
  | PtrToArray id off =>
    rcases hrel with ⟨hp, _, o, hconc, rfl, rfl⟩
    have ho : o ≤ 0xFFFF_FFFF#64 :=
      BitVec.le_trans hconc.1.2.2.2 (BitVec.le_trans hp.2.2 (by decide))
    have hb := hlayout.2.2.2.1 (renameMemId ρ id)
    exact ⟨rfl, rfl, (canonical_ptr_add_nonzero hb.1 hb.2 ho).1⟩
  | PtrToEndArray id =>
    rcases hrel with ⟨hlen, rfl, rfl⟩
    have hb := hlayout.2.2.2.1 (renameMemId ρ id)
    exact ⟨rfl, rfl, (canonical_ptr_add_nonzero hb.1 hb.2 hlen).1⟩
  | Releasable relId inner =>
    cases inner with
    | PtrToMemory id off sz orig =>
      rcases hrel with ⟨_, hp, o, hconc, rfl, rfl⟩
      have ho : o ≤ 0xFFFF_FFFF#64 := BitVec.le_trans hconc.1.2.2.2 hp.2.2
      have hb := hlayout.2.1 (renameMemId ρ id)
      exact ⟨rfl, rfl, (canonical_ptr_add_nonzero hb.1 hb.2 ho).1⟩
    | PtrToStruct id off sd =>
      rcases hrel with ⟨_, hp, _, o, hconc, rfl, rfl⟩
      have ho : o ≤ 0xFFFF_FFFF#64 := BitVec.le_trans hconc.1.2.2.2 hp.2.2
      have hb := hlayout.2.2.1 (renameMemId ρ id)
      exact ⟨rfl, rfl, (canonical_ptr_add_nonzero hb.1 hb.2 ho).1⟩
    | _ => exact False.elim hrel
  | NullOr nullId inner ih =>
    rcases ht with ⟨_, hinnerShape, hinnerValid⟩
    simp only [TypeRel2, hnonnull, Bool.false_eq_true, ↓reduceIte] at hrel
    cases inner <;> try exact False.elim hinnerShape
    all_goals exact ih hinnerValid hrel trivial
  | _ => exact False.elim hrel

theorem TypeRel2_congr {env : ExecEnv} {vCounter : Nat}
    {ρ₁ ρ₂ : Std.U64 → Std.U64} {res₁ res₂ null₁ null₂ : MemoryId → Bool}
    {arr₁ arr₂ : MemoryId → BitVec 64}
    {t : verifier.Type} {v₁ v₂ : RuntimeVal}
    (ht : TypeValid env vCounter t)
    (hρ : ∀ x : Std.U64, x.bv.toNat < vCounter → ρ₁ x = ρ₂ x)
    (hres : ∀ m : MemoryId, memIdBounded vCounter m = true →
      res₁ (renameMemId ρ₁ m) = res₂ (renameMemId ρ₂ m))
    (hnull : ∀ m : MemoryId, memIdBounded vCounter m = true →
      null₁ (renameMemId ρ₁ m) = null₂ (renameMemId ρ₂ m))
    (harr : ∀ m : MemoryId, memIdBounded vCounter m = true →
      arr₁ (renameMemId ρ₁ m) ≤ 0xFFFF_FFFF#64 →
      arr₂ (renameMemId ρ₂ m) ≤ 0xFFFF_FFFF#64)
    (hrel : TypeRel2 env ρ₁ res₁ null₁ arr₁ t v₁ v₂) :
    TypeRel2 env ρ₂ res₂ null₂ arr₂ t v₁ v₂ := by
  induction t with
  | ScalarValue _ | ConstPtrToMap _ _ | PtrToStack _ => exact hrel
  | PtrToMemory id off sz orig =>
    unfold TypeRel2 at hrel ⊢
    rw [← renameMemId_congr hρ ht.1]
    exact hrel
  | PtrToStruct id off sd =>
    unfold TypeRel2 at hrel ⊢
    rw [← renameMemId_congr hρ ht.1]
    exact hrel
  | PtrToArray id off =>
    rcases hrel with ⟨hp, hlen, o, hconc, hv1, hv2⟩
    refine ⟨hp, harr id ht.1 hlen, o, hconc, ?_, ?_⟩
    · rwa [← renameMemId_congr hρ ht.1]
    · rwa [← renameMemId_congr hρ ht.1]
  | PtrToEndArray id =>
    rcases hrel with ⟨hlen, hv1, hv2⟩
    refine ⟨harr id ht hlen, ?_, ?_⟩
    · rwa [← renameMemId_congr hρ ht]
    · rwa [← renameMemId_congr hρ ht]
  | Releasable relId inner =>
    cases inner <;> try exact False.elim ht
    · rcases ht with ⟨hrelB, hidB, _⟩
      rcases hrel with ⟨hact, hp, o, hconc, hv1, hv2⟩
      refine ⟨?_, hp, o, hconc, ?_, ?_⟩
      · rwa [← hres relId hrelB]
      · rwa [← renameMemId_congr hρ hidB, ← renameMemId_congr hρ hrelB]
      · rwa [← renameMemId_congr hρ hidB, ← renameMemId_congr hρ hrelB]
    · rcases ht with ⟨hrelB, hidB, _⟩
      rcases hrel with ⟨hact, hp, hsd, o, hconc, hv1, hv2⟩
      refine ⟨?_, hp, hsd, o, hconc, ?_, ?_⟩
      · rwa [← hres relId hrelB]
      · rwa [← renameMemId_congr hρ hidB, ← renameMemId_congr hρ hrelB]
      · rwa [← renameMemId_congr hρ hidB, ← renameMemId_congr hρ hrelB]
  | NullOr nullId inner ih =>
    rcases ht with ⟨hnullB, _, hinnerV⟩
    unfold TypeRel2 at hrel ⊢
    rw [← hnull nullId hnullB]
    by_cases hn : null₁ (renameMemId ρ₁ nullId)
    · simp only [hn, ↓reduceIte] at hrel ⊢
      refine ⟨?_, hrel.2⟩
      cases inner with
      | Releasable relId sub =>
        cases sub <;> try exact False.elim hinnerV
        · rcases hinnerV with ⟨hrelB, _⟩
          simpa [nullInnerInactive, ← hres relId hrelB] using hrel.1
        · rcases hinnerV with ⟨hrelB, _⟩
          simpa [nullInnerInactive, ← hres relId hrelB] using hrel.1
      | _ => trivial
    · simp [hn] at hrel ⊢; exact ih hinnerV hrel
  | _ => exact False.elim ht

theorem StackSlotRel2_congr {env : ExecEnv} {vCounter : Nat}
    {ρ₁ ρ₂ : Std.U64 → Std.U64} {res₁ res₂ null₁ null₂ : MemoryId → Bool}
    {arr₁ arr₂ : MemoryId → BitVec 64}
    {t : verifier.Type} {slot₁ slot₂ : StackSlot}
    (ht : TypeValid env vCounter t)
    (hρ : ∀ x : Std.U64, x.bv.toNat < vCounter → ρ₁ x = ρ₂ x)
    (hres : ∀ m : MemoryId, memIdBounded vCounter m = true →
      res₁ (renameMemId ρ₁ m) = res₂ (renameMemId ρ₂ m))
    (hnull : ∀ m : MemoryId, memIdBounded vCounter m = true →
      null₁ (renameMemId ρ₁ m) = null₂ (renameMemId ρ₂ m))
    (harr : ∀ m : MemoryId, memIdBounded vCounter m = true →
      arr₁ (renameMemId ρ₁ m) ≤ 0xFFFF_FFFF#64 →
      arr₂ (renameMemId ρ₂ m) ≤ 0xFFFF_FFFF#64)
    (hrel : StackSlotRel2 env ρ₁ res₁ null₁ arr₁ t slot₁ slot₂) :
    StackSlotRel2 env ρ₂ res₂ null₂ arr₂ t slot₁ slot₂ := by
  induction t with
  | ScalarValue _ => exact hrel
  | NullOr nullId inner ih =>
    rcases ht with ⟨hnullB, _, hinnerV⟩
    unfold StackSlotRel2 at hrel ⊢
    rw [← hnull nullId hnullB]
    by_cases hn : null₁ (renameMemId ρ₁ nullId)
    · simp only [hn, ↓reduceIte] at hrel ⊢
      refine ⟨?_, hrel.2⟩
      cases inner with
      | Releasable relId sub =>
        cases sub <;> try exact False.elim hinnerV
        · rcases hinnerV with ⟨hrelB, _⟩
          simpa [nullInnerInactive, ← hres relId hrelB] using hrel.1
        · rcases hinnerV with ⟨hrelB, _⟩
          simpa [nullInnerInactive, ← hres relId hrelB] using hrel.1
      | _ => trivial
    · simp [hn] at hrel ⊢; exact ih hinnerV hrel
  | _ =>
    rcases hrel with ⟨cap, hsc, hs1, hs2, htr⟩
    exact ⟨cap, hsc, hs1, hs2, TypeRel2_congr ht hρ hres hnull harr htr⟩

/-- Bitset membership for register `r` in `DataDependencies`. -/
def DataDepsHasReg (deps : verifier.DataDependencies) (r : Nat) : Bool :=
  deps.registers.bv.getLsbD r

/-- Bitset membership for stack slot `slot` in `DataDependencies`. -/
def DataDepsHasStack (deps : verifier.DataDependencies) (slot : Nat) : Bool :=
  deps.stack.bv.getLsbD slot

/-- Full dependency mask marking all 10 general registers and all 64 stack slots live. -/
def fullDataDeps : verifier.DataDependencies :=
  { registers := 0x03FF#u16, stack := core.num.U64.MAX }

theorem fullDataDeps_hasReg {r : Nat} (hr : r < 10) :
    DataDepsHasReg fullDataDeps r = true := by
  unfold DataDepsHasReg fullDataDeps
  have : r = 0 ∨ r = 1 ∨ r = 2 ∨ r = 3 ∨ r = 4 ∨ r = 5 ∨ r = 6 ∨ r = 7 ∨ r = 8 ∨ r = 9 := by omega
  rcases this with rfl | rfl | rfl | rfl | rfl | rfl | rfl | rfl | rfl | rfl <;> decide

theorem fullDataDeps_hasStack {slot : Nat} (hs : slot < 64) :
    DataDepsHasStack fullDataDeps slot = true := by
  unfold DataDepsHasStack fullDataDeps
  rw [u64_max_bv]
  simp only [BitVec.getLsbD_not, BitVec.getLsbD_zero, hs, decide_true, Bool.not_false, Bool.and_self]

/-- Pointwise inclusion of `DataDependencies` on valid register (`0..9`) and stack (`0..63`) indices. -/
def DepsSubset (d₁ d₂ : verifier.DataDependencies) : Prop :=
  (∀ r, r < 10 → DataDepsHasReg d₁ r = true → DataDepsHasReg d₂ r = true) ∧
  (∀ s, s < 64 → DataDepsHasStack d₁ s = true → DataDepsHasStack d₂ s = true)

theorem DepsSubset_refl (d : verifier.DataDependencies) : DepsSubset d d :=
  ⟨fun _ _ h => h, fun _ _ h => h⟩

theorem DepsSubset_trans {d₁ d₂ d₃ : verifier.DataDependencies}
    (h12 : DepsSubset d₁ d₂) (h23 : DepsSubset d₂ d₃) : DepsSubset d₁ d₃ :=
  ⟨fun r hr h => h23.1 r hr (h12.1 r hr h),
   fun s hs h => h23.2 s hs (h12.2 s hs h)⟩

theorem DepsSubset_full (d : verifier.DataDependencies) : DepsSubset d fullDataDeps :=
  ⟨fun _ hr _ => fullDataDeps_hasReg hr, fun _ hs _ => fullDataDeps_hasStack hs⟩

/-- Dependency-parameterized relational 2-safety invariant (`StateRel2Deps`) linking
    an abstract `ComputationContext` + `DataDependencies` to two concrete `MachineState`s. -/
def StateRel2Deps (env : ExecEnv) (ctx : verifier.ComputationContext)
    (deps : verifier.DataDependencies) (ρ : Std.U64 → Std.U64)
    (resActive isNullId : MemoryId → Bool)
    (st₁ st₂ : MachineState) : Prop :=
  st₁.pc = ctx.pc.val ∧
  st₂.pc = ctx.pc.val ∧
  st₁.world = st₂.world ∧
  (∀ id, st₁.world.arrayLen id ≤ 0xFFFF_FFFF#64) ∧
  (∀ m : MemoryId, m.namespace ≠ .Verification → isNullId m = st₁.world.structFieldNull m) ∧
  st₁.world.liveResources.Nodup ∧
  (∀ m : MemoryId,
    m ∈ st₁.world.liveResources ↔
      ∃ r ∈ ctx.resources.entries.val,
        resActive (renameMemId ρ r) = true ∧ renameMemId ρ r = m) ∧
  (∀ (m : MemoryId) (bound : Std.U64),
    (m, bound) ∈ ctx.array_bounds.entries.val →
    bound.bv ≤ st₁.world.arrayLen (renameMemId ρ m)) ∧
  st₁.regs GENERAL_REGISTER_COUNT = .stackPtr 512#64 ∧
  st₂.regs GENERAL_REGISTER_COUNT = .stackPtr 512#64 ∧
  (∀ r (_hr : r < 10) (hlen : r < ctx.registers.val.length),
    DataDepsHasReg deps r = true →
    TypeRel2 env ρ resActive isNullId st₁.world.arrayLen
      (ctx.registers.val[r]'hlen) (st₁.regs r) (st₂.regs r)) ∧
  (∀ slot, slot < 64 →
    DataDepsHasStack deps slot = true →
    StackSlotRel2 env ρ resActive isNullId st₁.world.arrayLen
      (stackSlotType ctx.stack slot) (st₁.stack slot) (st₂.stack slot))

theorem StateRel2Deps_anti_deps {env : ExecEnv} {ctx : verifier.ComputationContext}
    {d₁ d₂ : verifier.DataDependencies} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {st₁ st₂ : MachineState}
    (hsub : DepsSubset d₁ d₂)
    (hrel : StateRel2Deps env ctx d₂ ρ resActive isNullId st₁ st₂) :
    StateRel2Deps env ctx d₁ ρ resActive isNullId st₁ st₂ := by
  rcases hrel with ⟨hpc1, hpc2, hw, harr, hnull, hnodup, hres, hbnds, hr10_1, hr10_2, hregs, hstk⟩
  refine ⟨hpc1, hpc2, hw, harr, hnull, hnodup, hres, hbnds, hr10_1, hr10_2, ?_, ?_⟩
  · intro r hr hlen hd
    exact hregs r hr hlen (hsub.1 r hr hd)
  · intro s hs hd
    exact hstk s hs (hsub.2 s hs hd)

/-! ## 5. Bounded 2-Safety Execution & Termination (`SemTerminatesIn`) -/

theorem runSteps_mono {prog : Program} {env : ExecEnv}
    {layout : HostLayout} {secret : SecretInit}
    {f₁ f₂ : Nat} {st : MachineState} {res : BitVec 64 × WorldState}
    (hle : f₁ ≤ f₂)
    (hrun : runSteps prog env layout secret f₁ st = some (.inl res)) :
    runSteps prog env layout secret f₂ st = some (.inl res) := by
  induction f₁ generalizing f₂ st with
  | zero => cases hrun
  | succ k ih =>
    cases f₂ with
    | zero => omega
    | succ k₂ =>
      unfold runSteps at hrun ⊢
      cases hstep : step prog env layout secret st <;> rw [hstep] at hrun
      · exact ih (by omega) hrun
      · exact hrun
      · cases hrun

theorem StateRel2Deps_congr_rho {env : ExecEnv} {vCounter : Nat}
    {ctx : verifier.ComputationContext} {deps : verifier.DataDependencies}
    {ρ₁ ρ₂ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {st₁ st₂ : MachineState}
    (hvalid : CompCtxValid env vCounter ctx)
    (hρ : ∀ x : Std.U64, x.bv.toNat < vCounter → ρ₁ x = ρ₂ x)
    (hrel : StateRel2Deps env ctx deps ρ₁ resActive isNullId st₁ st₂) :
    StateRel2Deps env ctx deps ρ₂ resActive isNullId st₁ st₂ := by
  rcases hrel with ⟨hpc1, hpc2, hw, harr, hnull, hnodup, hres, hbnds, hr10_1, hr10_2, hregs, hstk⟩
  refine ⟨hpc1, hpc2, hw, harr, hnull, hnodup, ?_, ?_, hr10_1, hr10_2, ?_, ?_⟩
  · intro m
    rw [hres m]
    constructor
    · rintro ⟨r, hr, hact, rfl⟩
      have hrB := hvalid.2.2.2.2 r hr
      rw [renameMemId_congr hρ hrB] at hact ⊢
      exact ⟨r, hr, hact, rfl⟩
    · rintro ⟨r, hr, hact, rfl⟩
      have hrB := hvalid.2.2.2.2 r hr
      rw [← renameMemId_congr hρ hrB] at hact ⊢
      exact ⟨r, hr, hact, rfl⟩
  · intro m bound hm
    have hmB := hvalid.2.2.1.2 (m, bound) hm
    rw [← renameMemId_congr hρ hmB]
    exact hbnds m bound hm
  · intro r hr hlen hd
    have ht := hvalid.1 (ctx.registers.val[r]'hlen) (List.getElem_mem hlen)
    exact TypeRel2_congr ht hρ
      (fun m hm => by rw [renameMemId_congr hρ hm])
      (fun m hm => by rw [renameMemId_congr hρ hm])
      (fun m hm h => by rwa [← renameMemId_congr hρ hm])
      (hregs r hr hlen hd)
  · intro s hs hd
    have ht := stackSlotType_valid hvalid.2.1 s hs
    exact StackSlotRel2_congr ht hρ
      (fun m hm => by rw [renameMemId_congr hρ hm])
      (fun m hm => by rw [renameMemId_congr hρ hm])
      (fun m hm h => by rwa [← renameMemId_congr hρ hm])
      (hstk s hs hd)

/-- Relational 2-safety termination predicate: any pair of concrete executions
    satisfying `StateRel2Deps` at `(vCounter, ctx, deps)` terminates cleanly within
    `fuel` steps without safety violations and produces identical `(ret, finalWorld)`
    with zero leaked resources. -/
def SemTerminatesIn (prog : Program) (env : ExecEnv)
    (vCounter : Nat) (ctx : verifier.ComputationContext)
    (deps : verifier.DataDependencies) (fuel : Nat) : Prop :=
  ∀ (layout₁ layout₂ : HostLayout) (secret₁ secret₂ : SecretInit)
    (ρ : Std.U64 → Std.U64) (resActive isNullId : MemoryId → Bool)
    (st₁ st₂ : MachineState),
    ValidHostLayout layout₁ →
    ValidHostLayout layout₂ →
    (!env.allowPtrLeaks ∨ (layout₁ = layout₂ ∧ secret₁ = secret₂)) →
    st₁.world.nextAllocId + 3 * fuel < 2 ^ 63 →
    RhoValid vCounter st₁.world.nextAllocId ρ →
    StateRel2Deps env ctx deps ρ resActive isNullId st₁ st₂ →
    ∃ ret finalWorld,
      runSteps prog env layout₁ secret₁ fuel st₁ = some (.inl (ret, finalWorld)) ∧
      runSteps prog env layout₂ secret₂ fuel st₂ = some (.inl (ret, finalWorld)) ∧
      finalWorld.liveResources = []

theorem SemTerminatesIn_mono_fuel {prog : Program} {env : ExecEnv}
    {vCounter : Nat} {ctx : verifier.ComputationContext}
    {deps : verifier.DataDependencies} {f₁ f₂ : Nat}
    (hle : f₁ ≤ f₂)
    (h : SemTerminatesIn prog env vCounter ctx deps f₁) :
    SemTerminatesIn prog env vCounter ctx deps f₂ := by
  intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
  have halloc₁ : st₁.world.nextAllocId + 3 * f₁ < 2 ^ 63 := by omega
  rcases h l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc₁ hρ hrel with
    ⟨ret, wrld, hr₁, hr₂, hlive⟩
  exact ⟨ret, wrld, runSteps_mono hle hr₁, runSteps_mono hle hr₂, hlive⟩

theorem SemTerminatesIn_mono_vCounter {prog : Program} {env : ExecEnv}
    {v₁ v₂ : Nat} {ctx : verifier.ComputationContext}
    {deps : verifier.DataDependencies} {fuel : Nat}
    (hle : v₁ ≤ v₂)
    (h : SemTerminatesIn prog env v₁ ctx deps fuel) :
    SemTerminatesIn prog env v₂ ctx deps fuel := by
  intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
  exact h l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc
    (RhoValid_mono_left hle hρ) hrel

theorem SemTerminatesIn_shrink_vCounter {prog : Program} {env : ExecEnv}
    {v₁ v₂ : Nat} {ctx : verifier.ComputationContext}
    {deps : verifier.DataDependencies} {fuel : Nat}
    (hle : v₁ ≤ v₂) (hv₂ : v₂ < 2 ^ 63)
    (hvalid : CompCtxValid env v₁ ctx)
    (h : SemTerminatesIn prog env v₂ ctx deps fuel) :
    SemTerminatesIn prog env v₁ ctx deps fuel := by
  intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
  obtain ⟨ρ', hρ', hag⟩ := RhoValid_lift hle hv₂ (by omega) hρ
  exact h l₁ l₂ s₁ s₂ ρ' resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ'
    (StateRel2Deps_congr_rho hvalid hag hrel)

theorem SemTerminatesIn_change_vCounter {prog : Program} {env : ExecEnv}
    {v₁ v₂ : Nat} {ctx : verifier.ComputationContext}
    {deps : verifier.DataDependencies} {fuel : Nat}
    (hv₁ : v₁ < 2 ^ 63)
    (hvalid : CompCtxValid env v₂ ctx)
    (h : SemTerminatesIn prog env v₁ ctx deps fuel) :
    SemTerminatesIn prog env v₂ ctx deps fuel := by
  by_cases hle : v₁ ≤ v₂
  · exact SemTerminatesIn_mono_vCounter hle h
  · exact SemTerminatesIn_shrink_vCounter (by omega) hv₁ hvalid h

theorem SemTerminatesIn_mono_deps {prog : Program} {env : ExecEnv}
    {vCounter : Nat} {ctx : verifier.ComputationContext}
    {d₁ d₂ : verifier.DataDependencies} {fuel : Nat}
    (hsub : DepsSubset d₁ d₂)
    (h : SemTerminatesIn prog env vCounter ctx d₁ fuel) :
    SemTerminatesIn prog env vCounter ctx d₂ fuel := by
  intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
  exact h l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ
    (StateRel2Deps_anti_deps hsub hrel)

end StarnixEbpf.Proof
