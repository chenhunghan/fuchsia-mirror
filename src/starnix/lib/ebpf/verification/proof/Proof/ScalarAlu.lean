-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Proof.ScalarBase

/-!
# Scalar ALU, Bitwise, Shift, Checked Arithmetic, and Width-Conversion Soundness

Proves unconditional `.ok` termination and relational 2-safety soundness
(`ConcretizesScalar64Rel2` / `ConcretizesScalar32Rel2`) for all 64-bit and
32-bit abstract scalar ALU operations in `ebpf_core::scalar_value`, including
Contract Theorems 3, 4, and 5.
-/

open Aeneas Aeneas.Std Result

namespace StarnixEbpf.Proof

open StarnixEbpf.Spec
open ebpf_core.scalar_value

/-! ## 1. Concretization Projection Helpers -/

theorem concretizes64_range {d : U64ScalarValueData} {x : BitVec 64}
    (hc : ConcretizesScalar64 d x) : ConcretizesRange64 d.urange x :=
  ⟨hc.2.2.1, hc.2.2.2⟩

theorem concretizes32_range {d : U32ScalarValueData} {x : BitVec 32}
    (hc : ConcretizesScalar32 d x) : ConcretizesRange32 d.urange x :=
  ⟨hc.2.2.1, hc.2.2.2⟩

theorem base_64_sound_2safety (a b : U64ScalarValueData)
    {x₁ x₂ y₁ y₂ : BitVec 64} (f : BitVec 64 → BitVec 64 → BitVec 64)
    (ha : ConcretizesScalar64Rel2 a x₁ x₂) (hb : ConcretizesScalar64Rel2 b y₁ y₂) :
    ∃ res, U64ScalarValueData.base a b = ok res ∧
      ConcretizesScalar64Rel2 res (f x₁ y₁) (f x₂ y₂) := by
  unfold U64ScalarValueData.base U64ScalarValueData.is_fully_initialized
  rw [bind_ok_eq rfl]
  by_cases ha_init : a.unwritten_mask = 0#u64
  · simp only [ha_init, decide_true, ↓reduceIte]
    rw [bind_ok_eq rfl]
    by_cases hb_init : b.unwritten_mask = 0#u64
    · simp only [hb_init, decide_true, ↓reduceIte]
      have hx := fully_initialized_eq_of_rel2_64 ha_init ha
      have hy := fully_initialized_eq_of_rel2_64 hb_init hb
      subst hx hy
      obtain ⟨d, hd, _, hc⟩ := UNKNOWN_WRITTEN_64_sound
      exact ⟨d, hd, hc (f x₁ y₁)⟩
    · simp only [hb_init, decide_false, Bool.false_eq_true, ↓reduceIte]
      obtain ⟨d, hd, _, hc⟩ := UNINITIALIZED_64_sound
      exact ⟨d, hd, hc _ _⟩
  · simp only [ha_init, decide_false, Bool.false_eq_true, ↓reduceIte]
    obtain ⟨d, hd, _, hc⟩ := UNINITIALIZED_64_sound
    exact ⟨d, hd, hc _ _⟩

theorem base_32_sound_2safety (a b : U32ScalarValueData)
    {x₁ x₂ y₁ y₂ : BitVec 32} (f : BitVec 32 → BitVec 32 → BitVec 32)
    (ha : ConcretizesScalar32Rel2 a x₁ x₂) (hb : ConcretizesScalar32Rel2 b y₁ y₂) :
    ∃ res, U32ScalarValueData.base a b = ok res ∧
      ConcretizesScalar32Rel2 res (f x₁ y₁) (f x₂ y₂) := by
  unfold U32ScalarValueData.base U32ScalarValueData.is_fully_initialized
  rw [bind_ok_eq rfl]
  by_cases ha_init : a.unwritten_mask = 0#u32
  · simp only [ha_init, decide_true, ↓reduceIte]
    rw [bind_ok_eq rfl]
    by_cases hb_init : b.unwritten_mask = 0#u32
    · simp only [hb_init, decide_true, ↓reduceIte]
      have hx := fully_initialized_eq_of_rel2_32 ha_init ha
      have hy := fully_initialized_eq_of_rel2_32 hb_init hb
      subst hx hy
      obtain ⟨d, hd, _, hc⟩ := UNKNOWN_WRITTEN_32_sound
      exact ⟨d, hd, hc (f x₁ y₁)⟩
    · simp only [hb_init, decide_false, Bool.false_eq_true, ↓reduceIte]
      obtain ⟨d, hd, _, hc⟩ := UNINITIALIZED_32_sound
      exact ⟨d, hd, hc _ _⟩
  · simp only [ha_init, decide_false, Bool.false_eq_true, ↓reduceIte]
    obtain ⟨d, hd, _, hc⟩ := UNINITIALIZED_32_sound
    exact ⟨d, hd, hc _ _⟩

/-! ## 2. 64-Bit and 32-Bit Arithmetic (`add`, `sub`, `mul`, `div`, `rem`, `neg`) -/

theorem add_scalar_64_sound_2safety (a b : U64ScalarValueData) {x₁ x₂ y₁ y₂ : BitVec 64}
    (ha : ConcretizesScalar64Rel2 a x₁ x₂) (hb : ConcretizesScalar64Rel2 b y₁ y₂) :
    ∃ res, U64ScalarValueData.add_scalar a b = ok res ∧
      ConcretizesScalar64Rel2 res (evalAlu64 .add x₁ y₁) (evalAlu64 .add x₂ y₂) := by
  unfold U64ScalarValueData.add_scalar U64ScalarValueData.is_known evalAlu64 lift
  rw [bind_ok_eq rfl]
  by_cases hak : a.unknown_mask = 0#u64
  · simp only [hak, decide_true, ↓reduceIte]; rw [bind_ok_eq rfl]
    by_cases hbk : b.unknown_mask = 0#u64
    · simp only [hbk, decide_true, ↓reduceIte]; rw [bind_ok_eq rfl, u64_wrapping_add_eq]
      have hx1 := eq_value_of_known_64 hak ha.1
      have hx2 := eq_value_of_known_64 hak ha.2.1
      have hy1 := eq_value_of_known_64 hbk hb.1
      have hy2 := eq_value_of_known_64 hbk hb.2.1
      subst hx1 hx2 hy1 hy2
      obtain ⟨d, hd, _, hc⟩ := from_known_64_sound ⟨a.value.bv + b.value.bv⟩
      exact ⟨d, hd, hc⟩
    · simp only [hbk, decide_false, Bool.false_eq_true, ↓reduceIte]
      obtain ⟨ur, hur, hurs⟩ := range64_add_ok_and_sound a.urange b.urange
      obtain ⟨usvd, husvd⟩ := base_64_ok a b
      have hbu := base_update_range_64_sound_2safety a b ur (· + ·) ha hb
        (hurs x₁ y₁ (concretizes64_range ha.1) (concretizes64_range hb.1))
        (hurs x₂ y₂ (concretizes64_range ha.2.1) (concretizes64_range hb.2.1))
      rw [bind_ok_eq husvd] at hbu
      rw [bind_ok_eq husvd, bind_ok_eq hur]
      exact hbu
  · simp only [hak, decide_false, Bool.false_eq_true, ↓reduceIte]
    obtain ⟨ur, hur, hurs⟩ := range64_add_ok_and_sound a.urange b.urange
    obtain ⟨usvd, husvd⟩ := base_64_ok a b
    have hbu := base_update_range_64_sound_2safety a b ur (· + ·) ha hb
      (hurs x₁ y₁ (concretizes64_range ha.1) (concretizes64_range hb.1))
      (hurs x₂ y₂ (concretizes64_range ha.2.1) (concretizes64_range hb.2.1))
    rw [bind_ok_eq husvd] at hbu
    rw [bind_ok_eq husvd, bind_ok_eq hur]
    exact hbu

theorem add_scalar_32_sound_2safety (a b : U32ScalarValueData) {x₁ x₂ y₁ y₂ : BitVec 32}
    (ha : ConcretizesScalar32Rel2 a x₁ x₂) (hb : ConcretizesScalar32Rel2 b y₁ y₂) :
    ∃ res, U32ScalarValueData.add_scalar a b = ok res ∧
      ConcretizesScalar32Rel2 res (x₁ + y₁) (x₂ + y₂) := by
  unfold U32ScalarValueData.add_scalar U32ScalarValueData.is_known lift
  rw [bind_ok_eq rfl]
  by_cases hak : a.unknown_mask = 0#u32
  · simp only [hak, decide_true, ↓reduceIte]; rw [bind_ok_eq rfl]
    by_cases hbk : b.unknown_mask = 0#u32
    · simp only [hbk, decide_true, ↓reduceIte]; rw [bind_ok_eq rfl, u32_wrapping_add_eq]
      have hx1 := eq_value_of_known_32 hak ha.1
      have hx2 := eq_value_of_known_32 hak ha.2.1
      have hy1 := eq_value_of_known_32 hbk hb.1
      have hy2 := eq_value_of_known_32 hbk hb.2.1
      subst hx1 hx2 hy1 hy2
      obtain ⟨d, hd, _, hc⟩ := from_known_32_sound ⟨a.value.bv + b.value.bv⟩
      exact ⟨d, hd, hc⟩
    · simp only [hbk, decide_false, Bool.false_eq_true, ↓reduceIte]
      obtain ⟨ur, hur, hurs⟩ := range32_add_ok_and_sound a.urange b.urange
      obtain ⟨usvd, husvd⟩ := base_32_ok a b
      have hbu := base_update_range_32_sound_2safety a b ur (· + ·) ha hb
        (hurs x₁ y₁ (concretizes32_range ha.1) (concretizes32_range hb.1))
        (hurs x₂ y₂ (concretizes32_range ha.2.1) (concretizes32_range hb.2.1))
      rw [bind_ok_eq husvd] at hbu
      rw [bind_ok_eq husvd, bind_ok_eq hur]
      exact hbu
  · simp only [hak, decide_false, Bool.false_eq_true, ↓reduceIte]
    obtain ⟨ur, hur, hurs⟩ := range32_add_ok_and_sound a.urange b.urange
    obtain ⟨usvd, husvd⟩ := base_32_ok a b
    have hbu := base_update_range_32_sound_2safety a b ur (· + ·) ha hb
      (hurs x₁ y₁ (concretizes32_range ha.1) (concretizes32_range hb.1))
      (hurs x₂ y₂ (concretizes32_range ha.2.1) (concretizes32_range hb.2.1))
    rw [bind_ok_eq husvd] at hbu
    rw [bind_ok_eq husvd, bind_ok_eq hur]
    exact hbu

theorem sub_scalar_64_sound_2safety (a b : U64ScalarValueData) {x₁ x₂ y₁ y₂ : BitVec 64}
    (ha : ConcretizesScalar64Rel2 a x₁ x₂) (hb : ConcretizesScalar64Rel2 b y₁ y₂) :
    ∃ res, U64ScalarValueData.sub_scalar a b = ok res ∧
      ConcretizesScalar64Rel2 res (evalAlu64 .sub x₁ y₁) (evalAlu64 .sub x₂ y₂) := by
  unfold U64ScalarValueData.sub_scalar U64ScalarValueData.is_known evalAlu64 lift
  rw [bind_ok_eq rfl]
  by_cases hak : a.unknown_mask = 0#u64
  · simp only [hak, decide_true, ↓reduceIte]; rw [bind_ok_eq rfl]
    by_cases hbk : b.unknown_mask = 0#u64
    · simp only [hbk, decide_true, ↓reduceIte]; rw [bind_ok_eq rfl, u64_wrapping_sub_eq]
      have hx1 := eq_value_of_known_64 hak ha.1
      have hx2 := eq_value_of_known_64 hak ha.2.1
      have hy1 := eq_value_of_known_64 hbk hb.1
      have hy2 := eq_value_of_known_64 hbk hb.2.1
      subst hx1 hx2 hy1 hy2
      obtain ⟨d, hd, _, hc⟩ := from_known_64_sound ⟨a.value.bv - b.value.bv⟩
      exact ⟨d, hd, hc⟩
    · simp only [hbk, decide_false, Bool.false_eq_true, ↓reduceIte]
      obtain ⟨ur, hur, hurs⟩ := range64_sub_ok_and_sound a.urange b.urange
      obtain ⟨usvd, husvd⟩ := base_64_ok a b
      have hbu := base_update_range_64_sound_2safety a b ur (· - ·) ha hb
        (hurs x₁ y₁ (concretizes64_range ha.1) (concretizes64_range hb.1))
        (hurs x₂ y₂ (concretizes64_range ha.2.1) (concretizes64_range hb.2.1))
      rw [bind_ok_eq husvd] at hbu
      rw [bind_ok_eq husvd, bind_ok_eq hur]
      exact hbu
  · simp only [hak, decide_false, Bool.false_eq_true, ↓reduceIte]
    obtain ⟨ur, hur, hurs⟩ := range64_sub_ok_and_sound a.urange b.urange
    obtain ⟨usvd, husvd⟩ := base_64_ok a b
    have hbu := base_update_range_64_sound_2safety a b ur (· - ·) ha hb
      (hurs x₁ y₁ (concretizes64_range ha.1) (concretizes64_range hb.1))
      (hurs x₂ y₂ (concretizes64_range ha.2.1) (concretizes64_range hb.2.1))
    rw [bind_ok_eq husvd] at hbu
    rw [bind_ok_eq husvd, bind_ok_eq hur]
    exact hbu

theorem sub_scalar_32_sound_2safety (a b : U32ScalarValueData) {x₁ x₂ y₁ y₂ : BitVec 32}
    (ha : ConcretizesScalar32Rel2 a x₁ x₂) (hb : ConcretizesScalar32Rel2 b y₁ y₂) :
    ∃ res, U32ScalarValueData.sub_scalar a b = ok res ∧
      ConcretizesScalar32Rel2 res (x₁ - y₁) (x₂ - y₂) := by
  unfold U32ScalarValueData.sub_scalar U32ScalarValueData.is_known lift
  rw [bind_ok_eq rfl]
  by_cases hak : a.unknown_mask = 0#u32
  · simp only [hak, decide_true, ↓reduceIte]; rw [bind_ok_eq rfl]
    by_cases hbk : b.unknown_mask = 0#u32
    · simp only [hbk, decide_true, ↓reduceIte]; rw [bind_ok_eq rfl, u32_wrapping_sub_eq]
      have hx1 := eq_value_of_known_32 hak ha.1
      have hx2 := eq_value_of_known_32 hak ha.2.1
      have hy1 := eq_value_of_known_32 hbk hb.1
      have hy2 := eq_value_of_known_32 hbk hb.2.1
      subst hx1 hx2 hy1 hy2
      obtain ⟨d, hd, _, hc⟩ := from_known_32_sound ⟨a.value.bv - b.value.bv⟩
      exact ⟨d, hd, hc⟩
    · simp only [hbk, decide_false, Bool.false_eq_true, ↓reduceIte]
      obtain ⟨ur, hur, hurs⟩ := range32_sub_ok_and_sound a.urange b.urange
      obtain ⟨usvd, husvd⟩ := base_32_ok a b
      have hbu := base_update_range_32_sound_2safety a b ur (· - ·) ha hb
        (hurs x₁ y₁ (concretizes32_range ha.1) (concretizes32_range hb.1))
        (hurs x₂ y₂ (concretizes32_range ha.2.1) (concretizes32_range hb.2.1))
      rw [bind_ok_eq husvd] at hbu
      rw [bind_ok_eq husvd, bind_ok_eq hur]
      exact hbu
  · simp only [hak, decide_false, Bool.false_eq_true, ↓reduceIte]
    obtain ⟨ur, hur, hurs⟩ := range32_sub_ok_and_sound a.urange b.urange
    obtain ⟨usvd, husvd⟩ := base_32_ok a b
    have hbu := base_update_range_32_sound_2safety a b ur (· - ·) ha hb
      (hurs x₁ y₁ (concretizes32_range ha.1) (concretizes32_range hb.1))
      (hurs x₂ y₂ (concretizes32_range ha.2.1) (concretizes32_range hb.2.1))
    rw [bind_ok_eq husvd] at hbu
    rw [bind_ok_eq husvd, bind_ok_eq hur]
    exact hbu

theorem mul_scalar_64_sound_2safety (a b : U64ScalarValueData) {x₁ x₂ y₁ y₂ : BitVec 64}
    (ha : ConcretizesScalar64Rel2 a x₁ x₂) (hb : ConcretizesScalar64Rel2 b y₁ y₂) :
    ∃ res, U64ScalarValueData.mul_scalar a b = ok res ∧
      ConcretizesScalar64Rel2 res (evalAlu64 .mul x₁ y₁) (evalAlu64 .mul x₂ y₂) := by
  unfold U64ScalarValueData.mul_scalar U64ScalarValueData.is_known evalAlu64 lift
  rw [bind_ok_eq rfl]
  by_cases hak : a.unknown_mask = 0#u64
  · simp only [hak, decide_true, ↓reduceIte]; rw [bind_ok_eq rfl]
    by_cases hbk : b.unknown_mask = 0#u64
    · simp only [hbk, decide_true, ↓reduceIte]; rw [bind_ok_eq rfl, u64_wrapping_mul_eq]
      have hx1 := eq_value_of_known_64 hak ha.1
      have hx2 := eq_value_of_known_64 hak ha.2.1
      have hy1 := eq_value_of_known_64 hbk hb.1
      have hy2 := eq_value_of_known_64 hbk hb.2.1
      subst hx1 hx2 hy1 hy2
      obtain ⟨d, hd, _, hc⟩ := from_known_64_sound ⟨a.value.bv * b.value.bv⟩
      exact ⟨d, hd, hc⟩
    · simp only [hbk, decide_false, Bool.false_eq_true, ↓reduceIte]
      obtain ⟨ur, hur, hurs⟩ := range64_mul_ok_and_sound a.urange b.urange
      obtain ⟨usvd, husvd⟩ := base_64_ok a b
      have hbu := base_update_range_64_sound_2safety a b ur (· * ·) ha hb
        (hurs x₁ y₁ (concretizes64_range ha.1) (concretizes64_range hb.1))
        (hurs x₂ y₂ (concretizes64_range ha.2.1) (concretizes64_range hb.2.1))
      rw [bind_ok_eq husvd] at hbu
      rw [bind_ok_eq husvd, bind_ok_eq hur]
      exact hbu
  · simp only [hak, decide_false, Bool.false_eq_true, ↓reduceIte]
    obtain ⟨ur, hur, hurs⟩ := range64_mul_ok_and_sound a.urange b.urange
    obtain ⟨usvd, husvd⟩ := base_64_ok a b
    have hbu := base_update_range_64_sound_2safety a b ur (· * ·) ha hb
      (hurs x₁ y₁ (concretizes64_range ha.1) (concretizes64_range hb.1))
      (hurs x₂ y₂ (concretizes64_range ha.2.1) (concretizes64_range hb.2.1))
    rw [bind_ok_eq husvd] at hbu
    rw [bind_ok_eq husvd, bind_ok_eq hur]
    exact hbu

theorem mul_scalar_32_sound_2safety (a b : U32ScalarValueData) {x₁ x₂ y₁ y₂ : BitVec 32}
    (ha : ConcretizesScalar32Rel2 a x₁ x₂) (hb : ConcretizesScalar32Rel2 b y₁ y₂) :
    ∃ res, U32ScalarValueData.mul_scalar a b = ok res ∧
      ConcretizesScalar32Rel2 res (x₁ * y₁) (x₂ * y₂) := by
  unfold U32ScalarValueData.mul_scalar U32ScalarValueData.is_known lift
  rw [bind_ok_eq rfl]
  by_cases hak : a.unknown_mask = 0#u32
  · simp only [hak, decide_true, ↓reduceIte]; rw [bind_ok_eq rfl]
    by_cases hbk : b.unknown_mask = 0#u32
    · simp only [hbk, decide_true, ↓reduceIte]; rw [bind_ok_eq rfl, u32_wrapping_mul_eq]
      have hx1 := eq_value_of_known_32 hak ha.1
      have hx2 := eq_value_of_known_32 hak ha.2.1
      have hy1 := eq_value_of_known_32 hbk hb.1
      have hy2 := eq_value_of_known_32 hbk hb.2.1
      subst hx1 hx2 hy1 hy2
      obtain ⟨d, hd, _, hc⟩ := from_known_32_sound ⟨a.value.bv * b.value.bv⟩
      exact ⟨d, hd, hc⟩
    · simp only [hbk, decide_false, Bool.false_eq_true, ↓reduceIte]
      obtain ⟨ur, hur, hurs⟩ := range32_mul_ok_and_sound a.urange b.urange
      obtain ⟨usvd, husvd⟩ := base_32_ok a b
      have hbu := base_update_range_32_sound_2safety a b ur (· * ·) ha hb
        (hurs x₁ y₁ (concretizes32_range ha.1) (concretizes32_range hb.1))
        (hurs x₂ y₂ (concretizes32_range ha.2.1) (concretizes32_range hb.2.1))
      rw [bind_ok_eq husvd] at hbu
      rw [bind_ok_eq husvd, bind_ok_eq hur]
      exact hbu
  · simp only [hak, decide_false, Bool.false_eq_true, ↓reduceIte]
    obtain ⟨ur, hur, hurs⟩ := range32_mul_ok_and_sound a.urange b.urange
    obtain ⟨usvd, husvd⟩ := base_32_ok a b
    have hbu := base_update_range_32_sound_2safety a b ur (· * ·) ha hb
      (hurs x₁ y₁ (concretizes32_range ha.1) (concretizes32_range hb.1))
      (hurs x₂ y₂ (concretizes32_range ha.2.1) (concretizes32_range hb.2.1))
    rw [bind_ok_eq husvd] at hbu
    rw [bind_ok_eq husvd, bind_ok_eq hur]
    exact hbu

theorem div_scalar_64_sound_2safety (a b : U64ScalarValueData) {x₁ x₂ y₁ y₂ : BitVec 64}
    (ha : ConcretizesScalar64Rel2 a x₁ x₂) (hb : ConcretizesScalar64Rel2 b y₁ y₂) :
    ∃ res, U64ScalarValueData.div_scalar a b = ok res ∧
      ConcretizesScalar64Rel2 res (evalAlu64 .div x₁ y₁) (evalAlu64 .div x₂ y₂) := by
  unfold U64ScalarValueData.div_scalar U64ScalarValueData.is_known evalAlu64
  rw [bind_ok_eq rfl]
  by_cases hak : a.unknown_mask = 0#u64
  · simp only [hak, decide_true, ↓reduceIte]; rw [bind_ok_eq rfl]
    by_cases hbk : b.unknown_mask = 0#u64
    · simp only [hbk, decide_true, ↓reduceIte]
      have hx1 := eq_value_of_known_64 hak ha.1
      have hx2 := eq_value_of_known_64 hak ha.2.1
      have hy1 := eq_value_of_known_64 hbk hb.1
      have hy2 := eq_value_of_known_64 hbk hb.2.1
      subst hx1 hx2 hy1 hy2
      by_cases hrv : b.value = 0#u64
      · simp only [hrv, ↓reduceIte, u64_zero_uscalar_bv, beq_self_eq_true]
        obtain ⟨d, hd, _, hc⟩ := from_known_64_sound 0#u64
        exact ⟨d, hd, hc⟩
      · have hrv_bv : (b.value.bv == 0#64) = false :=
          beq_false_of_ne (fun h => hrv ((u64_eq_iff_bv _ _).2 h))
        simp only [hrv, ↓reduceIte, hrv_bv, Bool.false_eq_true]
        rw [bind_ok_eq (u64_div_eq_ok a.value b.value hrv)]
        obtain ⟨d, hd, _, hc⟩ := from_known_64_sound ⟨a.value.bv / b.value.bv⟩
        exact ⟨d, hd, hc⟩
    · simp only [hbk, decide_false, Bool.false_eq_true, ↓reduceIte]
      obtain ⟨ur, hur, hurs⟩ := range64_div_ok_and_sound a.urange b.urange
      obtain ⟨usvd, husvd⟩ := base_64_ok a b
      have hbu := base_update_range_64_sound_2safety a b ur
        (fun u v => if v == 0#64 then 0#64 else u / v) ha hb
        (hurs x₁ y₁ (concretizes64_range ha.1) (concretizes64_range hb.1))
        (hurs x₂ y₂ (concretizes64_range ha.2.1) (concretizes64_range hb.2.1))
      rw [bind_ok_eq husvd] at hbu
      rw [bind_ok_eq husvd, bind_ok_eq hur]
      exact hbu
  · simp only [hak, decide_false, Bool.false_eq_true, ↓reduceIte]
    obtain ⟨ur, hur, hurs⟩ := range64_div_ok_and_sound a.urange b.urange
    obtain ⟨usvd, husvd⟩ := base_64_ok a b
    have hbu := base_update_range_64_sound_2safety a b ur
      (fun u v => if v == 0#64 then 0#64 else u / v) ha hb
      (hurs x₁ y₁ (concretizes64_range ha.1) (concretizes64_range hb.1))
      (hurs x₂ y₂ (concretizes64_range ha.2.1) (concretizes64_range hb.2.1))
    rw [bind_ok_eq husvd] at hbu
    rw [bind_ok_eq husvd, bind_ok_eq hur]
    exact hbu

theorem div_scalar_32_sound_2safety (a b : U32ScalarValueData) {x₁ x₂ y₁ y₂ : BitVec 32}
    (ha : ConcretizesScalar32Rel2 a x₁ x₂) (hb : ConcretizesScalar32Rel2 b y₁ y₂) :
    ∃ res, U32ScalarValueData.div_scalar a b = ok res ∧
      ConcretizesScalar32Rel2 res
        (if y₁ == 0#32 then 0#32 else x₁ / y₁)
        (if y₂ == 0#32 then 0#32 else x₂ / y₂) := by
  unfold U32ScalarValueData.div_scalar U32ScalarValueData.is_known
  rw [bind_ok_eq rfl]
  by_cases hak : a.unknown_mask = 0#u32
  · simp only [hak, decide_true, ↓reduceIte]; rw [bind_ok_eq rfl]
    by_cases hbk : b.unknown_mask = 0#u32
    · simp only [hbk, decide_true, ↓reduceIte]
      have hx1 := eq_value_of_known_32 hak ha.1
      have hx2 := eq_value_of_known_32 hak ha.2.1
      have hy1 := eq_value_of_known_32 hbk hb.1
      have hy2 := eq_value_of_known_32 hbk hb.2.1
      subst hx1 hx2 hy1 hy2
      by_cases hrv : b.value = 0#u32
      · simp only [hrv, ↓reduceIte, u32_zero_uscalar_bv, beq_self_eq_true]
        obtain ⟨d, hd, _, hc⟩ := from_known_32_sound 0#u32
        exact ⟨d, hd, hc⟩
      · have hrv_bv : (b.value.bv == 0#32) = false :=
          beq_false_of_ne (fun h => hrv ((u32_eq_iff_bv _ _).2 h))
        simp only [hrv, ↓reduceIte, hrv_bv, Bool.false_eq_true]
        rw [bind_ok_eq (u32_div_eq_ok a.value b.value hrv)]
        obtain ⟨d, hd, _, hc⟩ := from_known_32_sound ⟨a.value.bv / b.value.bv⟩
        exact ⟨d, hd, hc⟩
    · simp only [hbk, decide_false, Bool.false_eq_true, ↓reduceIte]
      obtain ⟨ur, hur, hurs⟩ := range32_div_ok_and_sound a.urange b.urange
      obtain ⟨usvd, husvd⟩ := base_32_ok a b
      have hbu := base_update_range_32_sound_2safety a b ur
        (fun u v => if v == 0#32 then 0#32 else u / v) ha hb
        (hurs x₁ y₁ (concretizes32_range ha.1) (concretizes32_range hb.1))
        (hurs x₂ y₂ (concretizes32_range ha.2.1) (concretizes32_range hb.2.1))
      rw [bind_ok_eq husvd] at hbu
      rw [bind_ok_eq husvd, bind_ok_eq hur]
      exact hbu
  · simp only [hak, decide_false, Bool.false_eq_true, ↓reduceIte]
    obtain ⟨ur, hur, hurs⟩ := range32_div_ok_and_sound a.urange b.urange
    obtain ⟨usvd, husvd⟩ := base_32_ok a b
    have hbu := base_update_range_32_sound_2safety a b ur
      (fun u v => if v == 0#32 then 0#32 else u / v) ha hb
      (hurs x₁ y₁ (concretizes32_range ha.1) (concretizes32_range hb.1))
      (hurs x₂ y₂ (concretizes32_range ha.2.1) (concretizes32_range hb.2.1))
    rw [bind_ok_eq husvd] at hbu
    rw [bind_ok_eq husvd, bind_ok_eq hur]
    exact hbu

theorem rem_scalar_64_sound_2safety (a b : U64ScalarValueData) {x₁ x₂ y₁ y₂ : BitVec 64}
    (ha : ConcretizesScalar64Rel2 a x₁ x₂) (hb : ConcretizesScalar64Rel2 b y₁ y₂) :
    ∃ res, U64ScalarValueData.rem_scalar a b = ok res ∧
      ConcretizesScalar64Rel2 res (evalAlu64 .rem x₁ y₁) (evalAlu64 .rem x₂ y₂) := by
  unfold U64ScalarValueData.rem_scalar U64ScalarValueData.is_known evalAlu64
  rw [bind_ok_eq rfl]
  by_cases hak : a.unknown_mask = 0#u64
  · simp only [hak, decide_true, ↓reduceIte]; rw [bind_ok_eq rfl]
    by_cases hbk : b.unknown_mask = 0#u64
    · simp only [hbk, decide_true, ↓reduceIte]
      have hx1 := eq_value_of_known_64 hak ha.1
      have hx2 := eq_value_of_known_64 hak ha.2.1
      have hy1 := eq_value_of_known_64 hbk hb.1
      have hy2 := eq_value_of_known_64 hbk hb.2.1
      subst hx1 hx2 hy1 hy2
      by_cases hrv : b.value = 0#u64
      · simp only [hrv, ↓reduceIte, u64_zero_uscalar_bv, beq_self_eq_true]
        exact ⟨a, rfl, ha⟩
      · have hrv_bv : (b.value.bv == 0#64) = false :=
          beq_false_of_ne (fun h => hrv ((u64_eq_iff_bv _ _).2 h))
        simp only [hrv, ↓reduceIte, hrv_bv, Bool.false_eq_true]
        rw [bind_ok_eq (u64_rem_eq_ok a.value b.value hrv)]
        obtain ⟨d, hd, _, hc⟩ := from_known_64_sound ⟨a.value.bv % b.value.bv⟩
        exact ⟨d, hd, hc⟩
    · simp only [hbk, decide_false, Bool.false_eq_true, ↓reduceIte]
      obtain ⟨ur, hur, hurs⟩ := range64_rem_ok_and_sound a.urange b.urange
      obtain ⟨usvd, husvd⟩ := base_64_ok a b
      have hbu := base_update_range_64_sound_2safety a b ur
        (fun u v => if v == 0#64 then u else u % v) ha hb
        (hurs x₁ y₁ (concretizes64_range ha.1) (concretizes64_range hb.1))
        (hurs x₂ y₂ (concretizes64_range ha.2.1) (concretizes64_range hb.2.1))
      rw [bind_ok_eq husvd] at hbu
      rw [bind_ok_eq husvd, bind_ok_eq hur]
      exact hbu
  · simp only [hak, decide_false, Bool.false_eq_true, ↓reduceIte]
    obtain ⟨ur, hur, hurs⟩ := range64_rem_ok_and_sound a.urange b.urange
    obtain ⟨usvd, husvd⟩ := base_64_ok a b
    have hbu := base_update_range_64_sound_2safety a b ur
      (fun u v => if v == 0#64 then u else u % v) ha hb
      (hurs x₁ y₁ (concretizes64_range ha.1) (concretizes64_range hb.1))
      (hurs x₂ y₂ (concretizes64_range ha.2.1) (concretizes64_range hb.2.1))
    rw [bind_ok_eq husvd] at hbu
    rw [bind_ok_eq husvd, bind_ok_eq hur]
    exact hbu

theorem rem_scalar_32_sound_2safety (a b : U32ScalarValueData) {x₁ x₂ y₁ y₂ : BitVec 32}
    (ha : ConcretizesScalar32Rel2 a x₁ x₂) (hb : ConcretizesScalar32Rel2 b y₁ y₂) :
    ∃ res, U32ScalarValueData.rem_scalar a b = ok res ∧
      ConcretizesScalar32Rel2 res
        (if y₁ == 0#32 then x₁ else x₁ % y₁)
        (if y₂ == 0#32 then x₂ else x₂ % y₂) := by
  unfold U32ScalarValueData.rem_scalar U32ScalarValueData.is_known
  rw [bind_ok_eq rfl]
  by_cases hak : a.unknown_mask = 0#u32
  · simp only [hak, decide_true, ↓reduceIte]; rw [bind_ok_eq rfl]
    by_cases hbk : b.unknown_mask = 0#u32
    · simp only [hbk, decide_true, ↓reduceIte]
      have hx1 := eq_value_of_known_32 hak ha.1
      have hx2 := eq_value_of_known_32 hak ha.2.1
      have hy1 := eq_value_of_known_32 hbk hb.1
      have hy2 := eq_value_of_known_32 hbk hb.2.1
      subst hx1 hx2 hy1 hy2
      by_cases hrv : b.value = 0#u32
      · simp only [hrv, ↓reduceIte, u32_zero_uscalar_bv, beq_self_eq_true]
        exact ⟨a, rfl, ha⟩
      · have hrv_bv : (b.value.bv == 0#32) = false :=
          beq_false_of_ne (fun h => hrv ((u32_eq_iff_bv _ _).2 h))
        simp only [hrv, ↓reduceIte, hrv_bv, Bool.false_eq_true]
        rw [bind_ok_eq (u32_rem_eq_ok a.value b.value hrv)]
        obtain ⟨d, hd, _, hc⟩ := from_known_32_sound ⟨a.value.bv % b.value.bv⟩
        exact ⟨d, hd, hc⟩
    · simp only [hbk, decide_false, Bool.false_eq_true, ↓reduceIte]
      obtain ⟨ur, hur, hurs⟩ := range32_rem_ok_and_sound a.urange b.urange
      obtain ⟨usvd, husvd⟩ := base_32_ok a b
      have hbu := base_update_range_32_sound_2safety a b ur
        (fun u v => if v == 0#32 then u else u % v) ha hb
        (hurs x₁ y₁ (concretizes32_range ha.1) (concretizes32_range hb.1))
        (hurs x₂ y₂ (concretizes32_range ha.2.1) (concretizes32_range hb.2.1))
      rw [bind_ok_eq husvd] at hbu
      rw [bind_ok_eq husvd, bind_ok_eq hur]
      exact hbu
  · simp only [hak, decide_false, Bool.false_eq_true, ↓reduceIte]
    obtain ⟨ur, hur, hurs⟩ := range32_rem_ok_and_sound a.urange b.urange
    obtain ⟨usvd, husvd⟩ := base_32_ok a b
    have hbu := base_update_range_32_sound_2safety a b ur
      (fun u v => if v == 0#32 then u else u % v) ha hb
      (hurs x₁ y₁ (concretizes32_range ha.1) (concretizes32_range hb.1))
      (hurs x₂ y₂ (concretizes32_range ha.2.1) (concretizes32_range hb.2.1))
    rw [bind_ok_eq husvd] at hbu
    rw [bind_ok_eq husvd, bind_ok_eq hur]
    exact hbu

theorem neg_scalar_64_sound_2safety (a : U64ScalarValueData) {x₁ x₂ : BitVec 64}
    (ha : ConcretizesScalar64Rel2 a x₁ x₂) :
    ∃ res, U64ScalarValueData.neg_scalar a = ok res ∧
      ConcretizesScalar64Rel2 res (evalNeg true x₁) (evalNeg true x₂) := by
  unfold U64ScalarValueData.neg_scalar U64ScalarValueData.is_known evalNeg lift
  simp only [↓reduceIte]
  rw [bind_ok_eq rfl]
  by_cases hak : a.unknown_mask = 0#u64
  · simp only [hak, decide_true, ↓reduceIte]
    rw [bind_ok_eq rfl, u64_wrapping_sub_eq]
    simp only [u64_zero_bv]
    have hx1 := eq_value_of_known_64 hak ha.1
    have hx2 := eq_value_of_known_64 hak ha.2.1
    subst hx1 hx2
    have hsub : 0#64 - a.value.bv = -a.value.bv := by bv_decide
    obtain ⟨d, hd, _, hc⟩ := from_known_64_sound ⟨0#64 - a.value.bv⟩
    simp only [u64_mk_bv, hsub] at hc
    exact ⟨d, hd, hc⟩
  · simp only [hak, decide_false, Bool.false_eq_true, ↓reduceIte]
    obtain ⟨ur, hur, hurs⟩ := range64_neg_ok_and_sound a.urange
    obtain ⟨usvd, husvd⟩ := base_64_ok a a
    have hbu := base_update_range_64_sound_2safety a a ur (fun u _ => -u) ha ha
      (hurs x₁ (concretizes64_range ha.1))
      (hurs x₂ (concretizes64_range ha.2.1))
    rw [bind_ok_eq husvd] at hbu
    rw [bind_ok_eq husvd, bind_ok_eq hur]
    exact hbu

theorem neg_scalar_32_sound_2safety (a : U32ScalarValueData) {x₁ x₂ : BitVec 32}
    (ha : ConcretizesScalar32Rel2 a x₁ x₂) :
    ∃ res, U32ScalarValueData.neg_scalar a = ok res ∧
      ConcretizesScalar32Rel2 res (-x₁) (-x₂) := by
  unfold U32ScalarValueData.neg_scalar U32ScalarValueData.is_known lift
  rw [bind_ok_eq rfl]
  by_cases hak : a.unknown_mask = 0#u32
  · simp only [hak, decide_true, ↓reduceIte]
    rw [bind_ok_eq rfl, u32_wrapping_sub_eq]
    simp only [u32_zero_bv]
    have hx1 := eq_value_of_known_32 hak ha.1
    have hx2 := eq_value_of_known_32 hak ha.2.1
    subst hx1 hx2
    have hsub : 0#32 - a.value.bv = -a.value.bv := by bv_decide
    obtain ⟨d, hd, _, hc⟩ := from_known_32_sound ⟨0#32 - a.value.bv⟩
    simp only [u32_mk_bv, hsub] at hc
    exact ⟨d, hd, hc⟩
  · simp only [hak, decide_false, Bool.false_eq_true, ↓reduceIte]
    obtain ⟨ur, hur, hurs⟩ := range32_neg_ok_and_sound a.urange
    obtain ⟨usvd, husvd⟩ := base_32_ok a a
    have hbu := base_update_range_32_sound_2safety a a ur (fun u _ => -u) ha ha
      (hurs x₁ (concretizes32_range ha.1))
      (hurs x₂ (concretizes32_range ha.2.1))
    rw [bind_ok_eq husvd] at hbu
    rw [bind_ok_eq husvd, bind_ok_eq hur]
    exact hbu

/-! ## 3. Bitwise Operations (`bitand_scalar`, `bitor_scalar`, `bitxor_scalar`) -/

theorem bitand_scalar_64_sound_2safety (a b : U64ScalarValueData) {x₁ x₂ y₁ y₂ : BitVec 64}
    (ha : ConcretizesScalar64Rel2 a x₁ x₂) (hb : ConcretizesScalar64Rel2 b y₁ y₂) :
    ∃ res, U64ScalarValueData.bitand_scalar a b = ok res ∧
      ConcretizesScalar64Rel2 res (evalAlu64 .and x₁ y₁) (evalAlu64 .and x₂ y₂) := by
  unfold U64ScalarValueData.bitand_scalar U64ScalarValueData.max U64Range.new evalAlu64 lift
  rw [bind_ok_eq rfl, bind_ok_eq rfl, bind_ok_eq (min_u64_eq_ok a.urange.max b.urange.max)]
  simp only [bind_ok]
  rcases ha with ⟨⟨⟨ha_disj, ha_unwr, _, _, _⟩, hat1, _, hamax1⟩, ⟨_, hat2, _, hamax2⟩, hasec⟩
  rcases hb with ⟨⟨⟨hb_disj, hb_unwr, _, _, _⟩, hbt1, _, hbmax1⟩, ⟨_, hbt2, _, hbmax2⟩, hbsec⟩
  refine new64_sound_2safety _ _ _ _ (x₁ &&& y₁) (x₂ &&& y₂) ?_ ?_ ?_ ?_ ?_
  · unfold ConcretizesRange64
    simp only [u64_zero_bv, u64_mk_bv]
    split_ifs with hcmp
    · revert hamax1; bv_decide
    · revert hbmax1; bv_decide
  · simp only [u64_and_bv, u64_or_bv]
    revert hat1 hbt1 ha_disj hb_disj; bv_decide
  · unfold ConcretizesRange64
    simp only [u64_zero_bv, u64_mk_bv]
    split_ifs with hcmp
    · revert hamax2; bv_decide
    · revert hbmax2; bv_decide
  · simp only [u64_and_bv, u64_or_bv]
    revert hat2 hbt2 ha_disj hb_disj; bv_decide
  · simp only [u64_and_bv, u64_or_bv]
    revert hat1 hbt1 hat2 hbt2 ha_disj hb_disj ha_unwr hb_unwr hasec hbsec; bv_decide

theorem bitand_scalar_32_sound_2safety (a b : U32ScalarValueData) {x₁ x₂ y₁ y₂ : BitVec 32}
    (ha : ConcretizesScalar32Rel2 a x₁ x₂) (hb : ConcretizesScalar32Rel2 b y₁ y₂) :
    ∃ res, U32ScalarValueData.bitand_scalar a b = ok res ∧
      ConcretizesScalar32Rel2 res (x₁ &&& y₁) (x₂ &&& y₂) := by
  unfold U32ScalarValueData.bitand_scalar U32ScalarValueData.max U32Range.new lift
  rw [bind_ok_eq rfl, bind_ok_eq rfl, bind_ok_eq (min_u32_eq_ok a.urange.max b.urange.max)]
  simp only [bind_ok]
  rcases ha with ⟨⟨⟨ha_disj, ha_unwr, _, _, _⟩, hat1, _, hamax1⟩, ⟨_, hat2, _, hamax2⟩, hasec⟩
  rcases hb with ⟨⟨⟨hb_disj, hb_unwr, _, _, _⟩, hbt1, _, hbmax1⟩, ⟨_, hbt2, _, hbmax2⟩, hbsec⟩
  refine new32_sound_2safety _ _ _ _ (x₁ &&& y₁) (x₂ &&& y₂) ?_ ?_ ?_ ?_ ?_
  · unfold ConcretizesRange32
    simp only [u32_zero_bv, u32_mk_bv]
    split_ifs with hcmp
    · revert hamax1; bv_decide
    · revert hbmax1; bv_decide
  · simp only [u32_and_bv, u32_or_bv]
    revert hat1 hbt1 ha_disj hb_disj; bv_decide
  · unfold ConcretizesRange32
    simp only [u32_zero_bv, u32_mk_bv]
    split_ifs with hcmp
    · revert hamax2; bv_decide
    · revert hbmax2; bv_decide
  · simp only [u32_and_bv, u32_or_bv]
    revert hat2 hbt2 ha_disj hb_disj; bv_decide
  · simp only [u32_and_bv, u32_or_bv]
    revert hat1 hbt1 hat2 hbt2 ha_disj hb_disj ha_unwr hb_unwr hasec hbsec; bv_decide

theorem bitor_scalar_64_sound_2safety (a b : U64ScalarValueData) {x₁ x₂ y₁ y₂ : BitVec 64}
    (ha : ConcretizesScalar64Rel2 a x₁ x₂) (hb : ConcretizesScalar64Rel2 b y₁ y₂) :
    ∃ res, U64ScalarValueData.bitor_scalar a b = ok res ∧
      ConcretizesScalar64Rel2 res (evalAlu64 .or x₁ y₁) (evalAlu64 .or x₂ y₂) := by
  unfold U64ScalarValueData.bitor_scalar U64ScalarValueData.min U64Range.new evalAlu64 lift
  rw [bind_ok_eq rfl, bind_ok_eq rfl, bind_ok_eq (max_u64_eq_ok a.urange.min b.urange.min)]
  simp only [bind_ok]
  rcases ha with ⟨⟨⟨ha_disj, ha_unwr, _, _, _⟩, hat1, hamin1, _⟩, ⟨_, hat2, hamin2, _⟩, hasec⟩
  rcases hb with ⟨⟨⟨hb_disj, hb_unwr, _, _, _⟩, hbt1, hbmin1, _⟩, ⟨_, hbt2, hbmin2, _⟩, hbsec⟩
  refine new64_sound_2safety _ _ _ _ (x₁ ||| y₁) (x₂ ||| y₂) ?_ ?_ ?_ ?_ ?_
  · unfold ConcretizesRange64
    simp only [u64_max_bv, u64_mk_bv]
    split_ifs with hcmp
    · revert hamin1; bv_decide
    · revert hbmin1; bv_decide
  · simp only [u64_and_bv, u64_or_bv, u64_not_bv]
    revert hat1 hbt1 ha_disj hb_disj; bv_decide
  · unfold ConcretizesRange64
    simp only [u64_max_bv, u64_mk_bv]
    split_ifs with hcmp
    · revert hamin2; bv_decide
    · revert hbmin2; bv_decide
  · simp only [u64_and_bv, u64_or_bv, u64_not_bv]
    revert hat2 hbt2 ha_disj hb_disj; bv_decide
  · simp only [u64_and_bv, u64_or_bv, u64_not_bv]
    revert hat1 hbt1 hat2 hbt2 ha_disj hb_disj ha_unwr hb_unwr hasec hbsec; bv_decide

theorem bitor_scalar_32_sound_2safety (a b : U32ScalarValueData) {x₁ x₂ y₁ y₂ : BitVec 32}
    (ha : ConcretizesScalar32Rel2 a x₁ x₂) (hb : ConcretizesScalar32Rel2 b y₁ y₂) :
    ∃ res, U32ScalarValueData.bitor_scalar a b = ok res ∧
      ConcretizesScalar32Rel2 res (x₁ ||| y₁) (x₂ ||| y₂) := by
  unfold U32ScalarValueData.bitor_scalar U32ScalarValueData.min U32Range.new lift
  rw [bind_ok_eq rfl, bind_ok_eq rfl, bind_ok_eq (max_u32_eq_ok a.urange.min b.urange.min)]
  simp only [bind_ok]
  rcases ha with ⟨⟨⟨ha_disj, ha_unwr, _, _, _⟩, hat1, hamin1, _⟩, ⟨_, hat2, hamin2, _⟩, hasec⟩
  rcases hb with ⟨⟨⟨hb_disj, hb_unwr, _, _, _⟩, hbt1, hbmin1, _⟩, ⟨_, hbt2, hbmin2, _⟩, hbsec⟩
  refine new32_sound_2safety _ _ _ _ (x₁ ||| y₁) (x₂ ||| y₂) ?_ ?_ ?_ ?_ ?_
  · unfold ConcretizesRange32
    simp only [u32_max_bv, u32_mk_bv]
    split_ifs with hcmp
    · revert hamin1; bv_decide
    · revert hbmin1; bv_decide
  · simp only [u32_and_bv, u32_or_bv, u32_not_bv]
    revert hat1 hbt1 ha_disj hb_disj; bv_decide
  · unfold ConcretizesRange32
    simp only [u32_max_bv, u32_mk_bv]
    split_ifs with hcmp
    · revert hamin2; bv_decide
    · revert hbmin2; bv_decide
  · simp only [u32_and_bv, u32_or_bv, u32_not_bv]
    revert hat2 hbt2 ha_disj hb_disj; bv_decide
  · simp only [u32_and_bv, u32_or_bv, u32_not_bv]
    revert hat1 hbt1 hat2 hbt2 ha_disj hb_disj ha_unwr hb_unwr hasec hbsec; bv_decide

theorem bitxor_scalar_64_sound_2safety (a b : U64ScalarValueData) {x₁ x₂ y₁ y₂ : BitVec 64}
    (ha : ConcretizesScalar64Rel2 a x₁ x₂) (hb : ConcretizesScalar64Rel2 b y₁ y₂) :
    ∃ res, U64ScalarValueData.bitxor_scalar a b = ok res ∧
      ConcretizesScalar64Rel2 res (evalAlu64 .xor x₁ y₁) (evalAlu64 .xor x₂ y₂) := by
  unfold U64ScalarValueData.bitxor_scalar U64Range.impl.max U64Range.new evalAlu64 lift
  simp only [bind_ok]
  rcases ha with ⟨⟨⟨ha_disj, _, _, _, _⟩, hat1, _, _⟩, ⟨_, hat2, _, _⟩, hasec⟩
  rcases hb with ⟨⟨⟨hb_disj, _, _, _, _⟩, hbt1, _, _⟩, ⟨_, hbt2, _, _⟩, hbsec⟩
  refine new64_sound_2safety _ _ _ _ (x₁ ^^^ y₁) (x₂ ^^^ y₂)
    (range64_max_sound _) ?_ (range64_max_sound _) ?_ ?_
  · simp only [u64_and_bv, u64_xor_bv, u64_not_bv, u64_or_bv]
    revert hat1 hbt1 ha_disj hb_disj; bv_decide
  · simp only [u64_and_bv, u64_xor_bv, u64_not_bv, u64_or_bv]
    revert hat2 hbt2 ha_disj hb_disj; bv_decide
  · simp only [u64_or_bv]
    revert hasec hbsec; bv_decide

theorem bitxor_scalar_32_sound_2safety (a b : U32ScalarValueData) {x₁ x₂ y₁ y₂ : BitVec 32}
    (ha : ConcretizesScalar32Rel2 a x₁ x₂) (hb : ConcretizesScalar32Rel2 b y₁ y₂) :
    ∃ res, U32ScalarValueData.bitxor_scalar a b = ok res ∧
      ConcretizesScalar32Rel2 res (x₁ ^^^ y₁) (x₂ ^^^ y₂) := by
  unfold U32ScalarValueData.bitxor_scalar U32Range.impl.max U32Range.new lift
  simp only [bind_ok]
  rcases ha with ⟨⟨⟨ha_disj, _, _, _, _⟩, hat1, _, _⟩, ⟨_, hat2, _, _⟩, hasec⟩
  rcases hb with ⟨⟨⟨hb_disj, _, _, _, _⟩, hbt1, _, _⟩, ⟨_, hbt2, _, _⟩, hbsec⟩
  refine new32_sound_2safety _ _ _ _ (x₁ ^^^ y₁) (x₂ ^^^ y₂)
    (range32_max_sound _) ?_ (range32_max_sound _) ?_ ?_
  · simp only [u32_and_bv, u32_xor_bv, u32_not_bv, u32_or_bv]
    revert hat1 hbt1 ha_disj hb_disj; bv_decide
  · simp only [u32_and_bv, u32_xor_bv, u32_not_bv, u32_or_bv]
    revert hat2 hbt2 ha_disj hb_disj; bv_decide
  · simp only [u32_or_bv]
    revert hasec hbsec; bv_decide

/-! ## 4. Shift Operations (`shl_scalar`, `shr_scalar`, `ashr`) -/

theorem shl_scalar_64_sound_2safety (a b : U64ScalarValueData) {x₁ x₂ y₁ y₂ : BitVec 64}
    (ha : ConcretizesScalar64Rel2 a x₁ x₂) (hb : ConcretizesScalar64Rel2 b y₁ y₂) :
    ∃ res, U64ScalarValueData.shl_scalar a b = ok res ∧
      ConcretizesScalar64Rel2 res (evalAlu64 .lsh x₁ y₁) (evalAlu64 .lsh x₂ y₂) := by
  unfold U64ScalarValueData.shl_scalar U64ScalarValueData.is_known evalAlu64 lift
  simp only [u64_guard_eta]
  obtain ⟨ur, hur, hurs⟩ := range64_shl_ok_and_sound a.urange b.urange
  rw [bind_ok_eq hur, bind_ok_eq rfl]
  by_cases hbk : b.unknown_mask = 0#u64
  · simp only [hbk, decide_true, ↓reduceIte, bind_ok]
    rw [u64_wrapping_shl_eq, u64_wrapping_shl_eq, u64_wrapping_shl_eq]
    have hy1 := eq_value_of_known_64 hbk hb.1
    have hy2 := eq_value_of_known_64 hbk hb.2.1
    subst hy1 hy2
    have hc : (UScalar.cast .U32 b.value).bv.setWidth 6 = b.value.bv.setWidth 6 := by
      change (BitVec.setWidth 32 b.value.bv).setWidth 6 = b.value.bv.setWidth 6; bv_decide
    have har1 := concretizes64_range ha.1
    have har2 := concretizes64_range ha.2.1
    have hbr1 := concretizes64_range hb.1
    have hbr2 := concretizes64_range hb.2.1
    have hat1 := ha.1.2.1
    have hat2 := ha.2.1.2.1
    have hasec := ha.2.2
    refine new64_sound_2safety _ _ _ ur
      (x₁ <<< b.value.bv.setWidth 6) (x₂ <<< b.value.bv.setWidth 6)
      (hurs x₁ b.value.bv har1 hbr1) ?_ (hurs x₂ b.value.bv har2 hbr2) ?_ ?_
    · simp only [u64_mk_uscalar_bv, hc]; revert hat1; bv_decide
    · simp only [u64_mk_uscalar_bv, hc]; revert hat2; bv_decide
    · simp only [u64_mk_uscalar_bv, hc]; revert hasec; bv_decide
  · simp only [hbk, decide_false, Bool.false_eq_true, ↓reduceIte]
    exact base_update_range_64_sound_2safety a b ur (fun u v => u <<< v.setWidth 6) ha hb
      (hurs x₁ y₁ (concretizes64_range ha.1) (concretizes64_range hb.1))
      (hurs x₂ y₂ (concretizes64_range ha.2.1) (concretizes64_range hb.2.1))

theorem shl_scalar_32_sound_2safety (a b : U32ScalarValueData) {x₁ x₂ y₁ y₂ : BitVec 32}
    (ha : ConcretizesScalar32Rel2 a x₁ x₂) (hb : ConcretizesScalar32Rel2 b y₁ y₂) :
    ∃ res, U32ScalarValueData.shl_scalar a b = ok res ∧
      ConcretizesScalar32Rel2 res (x₁ <<< y₁.setWidth 5) (x₂ <<< y₂.setWidth 5) := by
  unfold U32ScalarValueData.shl_scalar U32ScalarValueData.is_known lift
  simp only [u32_guard_eta]
  obtain ⟨ur, hur, hurs⟩ := range32_shl_ok_and_sound a.urange b.urange
  rw [bind_ok_eq hur, bind_ok_eq rfl]
  by_cases hbk : b.unknown_mask = 0#u32
  · simp only [hbk, decide_true, ↓reduceIte, bind_ok]
    rw [u32_wrapping_shl_eq, u32_wrapping_shl_eq, u32_wrapping_shl_eq]
    have hy1 := eq_value_of_known_32 hbk hb.1
    have hy2 := eq_value_of_known_32 hbk hb.2.1
    subst hy1 hy2
    have har1 := concretizes32_range ha.1
    have har2 := concretizes32_range ha.2.1
    have hbr1 := concretizes32_range hb.1
    have hbr2 := concretizes32_range hb.2.1
    have hat1 := ha.1.2.1
    have hat2 := ha.2.1.2.1
    have hasec := ha.2.2
    refine new32_sound_2safety _ _ _ ur
      (x₁ <<< b.value.bv.setWidth 5) (x₂ <<< b.value.bv.setWidth 5)
      (hurs x₁ b.value.bv har1 hbr1) ?_ (hurs x₂ b.value.bv har2 hbr2) ?_ ?_
    · simp only [u32_mk_uscalar_bv]; revert hat1; bv_decide
    · simp only [u32_mk_uscalar_bv]; revert hat2; bv_decide
    · simp only [u32_mk_uscalar_bv]; revert hasec; bv_decide
  · simp only [hbk, decide_false, Bool.false_eq_true, ↓reduceIte]
    exact base_update_range_32_sound_2safety a b ur (fun u v => u <<< v.setWidth 5) ha hb
      (hurs x₁ y₁ (concretizes32_range ha.1) (concretizes32_range hb.1))
      (hurs x₂ y₂ (concretizes32_range ha.2.1) (concretizes32_range hb.2.1))

theorem shr_scalar_64_sound_2safety (a b : U64ScalarValueData) {x₁ x₂ y₁ y₂ : BitVec 64}
    (ha : ConcretizesScalar64Rel2 a x₁ x₂) (hb : ConcretizesScalar64Rel2 b y₁ y₂) :
    ∃ res, U64ScalarValueData.shr_scalar a b = ok res ∧
      ConcretizesScalar64Rel2 res (evalAlu64 .rsh x₁ y₁) (evalAlu64 .rsh x₂ y₂) := by
  unfold U64ScalarValueData.shr_scalar U64ScalarValueData.is_known evalAlu64 lift
  simp only [u64_guard_eta]
  obtain ⟨ur, hur, hurs⟩ := range64_shr_ok_and_sound a.urange b.urange
  rw [bind_ok_eq hur, bind_ok_eq rfl]
  by_cases hbk : b.unknown_mask = 0#u64
  · simp only [hbk, decide_true, ↓reduceIte, bind_ok]
    rw [u64_wrapping_shr_eq, u64_wrapping_shr_eq, u64_wrapping_shr_eq]
    have hy1 := eq_value_of_known_64 hbk hb.1
    have hy2 := eq_value_of_known_64 hbk hb.2.1
    subst hy1 hy2
    have hc : (UScalar.cast .U32 b.value).bv.setWidth 6 = b.value.bv.setWidth 6 := by
      change (BitVec.setWidth 32 b.value.bv).setWidth 6 = b.value.bv.setWidth 6; bv_decide
    have har1 := concretizes64_range ha.1
    have har2 := concretizes64_range ha.2.1
    have hbr1 := concretizes64_range hb.1
    have hbr2 := concretizes64_range hb.2.1
    have hat1 := ha.1.2.1
    have hat2 := ha.2.1.2.1
    have hasec := ha.2.2
    refine new64_sound_2safety _ _ _ ur
      (x₁ >>> b.value.bv.setWidth 6) (x₂ >>> b.value.bv.setWidth 6)
      (hurs x₁ b.value.bv har1 hbr1) ?_ (hurs x₂ b.value.bv har2 hbr2) ?_ ?_
    · simp only [u64_mk_uscalar_bv, hc]; revert hat1; bv_decide
    · simp only [u64_mk_uscalar_bv, hc]; revert hat2; bv_decide
    · simp only [u64_mk_uscalar_bv, hc]; revert hasec; bv_decide
  · simp only [hbk, decide_false, Bool.false_eq_true, ↓reduceIte]
    exact base_update_range_64_sound_2safety a b ur (fun u v => u >>> v.setWidth 6) ha hb
      (hurs x₁ y₁ (concretizes64_range ha.1) (concretizes64_range hb.1))
      (hurs x₂ y₂ (concretizes64_range ha.2.1) (concretizes64_range hb.2.1))

theorem shr_scalar_32_sound_2safety (a b : U32ScalarValueData) {x₁ x₂ y₁ y₂ : BitVec 32}
    (ha : ConcretizesScalar32Rel2 a x₁ x₂) (hb : ConcretizesScalar32Rel2 b y₁ y₂) :
    ∃ res, U32ScalarValueData.shr_scalar a b = ok res ∧
      ConcretizesScalar32Rel2 res (x₁ >>> y₁.setWidth 5) (x₂ >>> y₂.setWidth 5) := by
  unfold U32ScalarValueData.shr_scalar U32ScalarValueData.is_known lift
  simp only [u32_guard_eta]
  obtain ⟨ur, hur, hurs⟩ := range32_shr_ok_and_sound a.urange b.urange
  rw [bind_ok_eq hur, bind_ok_eq rfl]
  by_cases hbk : b.unknown_mask = 0#u32
  · simp only [hbk, decide_true, ↓reduceIte, bind_ok]
    rw [u32_wrapping_shr_eq, u32_wrapping_shr_eq, u32_wrapping_shr_eq]
    have hy1 := eq_value_of_known_32 hbk hb.1
    have hy2 := eq_value_of_known_32 hbk hb.2.1
    subst hy1 hy2
    have har1 := concretizes32_range ha.1
    have har2 := concretizes32_range ha.2.1
    have hbr1 := concretizes32_range hb.1
    have hbr2 := concretizes32_range hb.2.1
    have hat1 := ha.1.2.1
    have hat2 := ha.2.1.2.1
    have hasec := ha.2.2
    refine new32_sound_2safety _ _ _ ur
      (x₁ >>> b.value.bv.setWidth 5) (x₂ >>> b.value.bv.setWidth 5)
      (hurs x₁ b.value.bv har1 hbr1) ?_ (hurs x₂ b.value.bv har2 hbr2) ?_ ?_
    · simp only [u32_mk_uscalar_bv]; revert hat1; bv_decide
    · simp only [u32_mk_uscalar_bv]; revert hat2; bv_decide
    · simp only [u32_mk_uscalar_bv]; revert hasec; bv_decide
  · simp only [hbk, decide_false, Bool.false_eq_true, ↓reduceIte]
    exact base_update_range_32_sound_2safety a b ur (fun u v => u >>> v.setWidth 5) ha hb
      (hurs x₁ y₁ (concretizes32_range ha.1) (concretizes32_range hb.1))
      (hurs x₂ y₂ (concretizes32_range ha.2.1) (concretizes32_range hb.2.1))

theorem ashr_64_sound_2safety (a b : U64ScalarValueData) {x₁ x₂ y₁ y₂ : BitVec 64}
    (ha : ConcretizesScalar64Rel2 a x₁ x₂) (hb : ConcretizesScalar64Rel2 b y₁ y₂) :
    ∃ res, U64ScalarValueData.ashr a b = ok res ∧
      ConcretizesScalar64Rel2 res (evalAlu64 .arsh x₁ y₁) (evalAlu64 .arsh x₂ y₂) := by
  unfold U64ScalarValueData.ashr U64ScalarValueData.is_known U64Range.impl.max U64Range.new evalAlu64 lift
  rw [bind_ok_eq rfl]
  by_cases hbk : b.unknown_mask = 0#u64
  · simp only [hbk, decide_true, ↓reduceIte]; rw [bind_ok_eq rfl]
    have hy1 := eq_value_of_known_64 hbk hb.1
    have hy2 := eq_value_of_known_64 hbk hb.2.1
    subst hy1 hy2
    by_cases hak : a.unknown_mask = 0#u64
    · simp only [hak, decide_true, ↓reduceIte]
      rw [bind_ok_eq (ashr_raw_u64_eq_ok a.value b.value)]
      have hx1 := eq_value_of_known_64 hak ha.1
      have hx2 := eq_value_of_known_64 hak ha.2.1
      subst hx1 hx2
      obtain ⟨d, hd, _, hc⟩ := from_known_64_sound ⟨BitVec.sshiftRight' a.value.bv (b.value.bv.setWidth 6)⟩
      exact ⟨d, hd, hc⟩
    · simp only [hak, decide_false, Bool.false_eq_true, ↓reduceIte]
      rw [bind_ok_eq (ashr_raw_u64_eq_ok a.unknown_mask b.value),
          bind_ok_eq (ashr_raw_u64_eq_ok a.unwritten_mask b.value),
          bind_ok_eq (ashr_raw_u64_eq_ok a.value b.value)]
      simp only [bind_ok]
      have hat1 := ha.1.2.1
      have hat2 := ha.2.1.2.1
      have hasec := ha.2.2
      refine new64_sound_2safety _ _ _ _
        (BitVec.sshiftRight' x₁ (b.value.bv.setWidth 6))
        (BitVec.sshiftRight' x₂ (b.value.bv.setWidth 6))
        (range64_max_sound _) ?_ (range64_max_sound _) ?_ ?_
      · simp only [u64_and_bv, u64_not_bv, u64_mk_uscalar_bv]
        revert hat1; bv_decide
      · simp only [u64_and_bv, u64_not_bv, u64_mk_uscalar_bv]
        revert hat2; bv_decide
      · simp only [u64_mk_uscalar_bv]
        revert hasec; bv_decide
  · simp only [hbk, decide_false, Bool.false_eq_true, ↓reduceIte]
    exact base_64_sound_2safety a b (fun u v => BitVec.sshiftRight' u (v.setWidth 6)) ha hb

theorem ashr_32_sound_2safety (a b : U32ScalarValueData) {x₁ x₂ y₁ y₂ : BitVec 32}
    (ha : ConcretizesScalar32Rel2 a x₁ x₂) (hb : ConcretizesScalar32Rel2 b y₁ y₂) :
    ∃ res, U32ScalarValueData.ashr a b = ok res ∧
      ConcretizesScalar32Rel2 res
        (BitVec.sshiftRight' x₁ (y₁.setWidth 5))
        (BitVec.sshiftRight' x₂ (y₂.setWidth 5)) := by
  unfold U32ScalarValueData.ashr U32ScalarValueData.is_known U32Range.impl.max U32Range.new lift
  rw [bind_ok_eq rfl]
  by_cases hbk : b.unknown_mask = 0#u32
  · simp only [hbk, decide_true, ↓reduceIte]; rw [bind_ok_eq rfl]
    have hy1 := eq_value_of_known_32 hbk hb.1
    have hy2 := eq_value_of_known_32 hbk hb.2.1
    subst hy1 hy2
    by_cases hak : a.unknown_mask = 0#u32
    · simp only [hak, decide_true, ↓reduceIte]
      rw [bind_ok_eq (ashr_raw_u32_eq_ok a.value b.value)]
      have hx1 := eq_value_of_known_32 hak ha.1
      have hx2 := eq_value_of_known_32 hak ha.2.1
      subst hx1 hx2
      obtain ⟨d, hd, _, hc⟩ := from_known_32_sound ⟨BitVec.sshiftRight' a.value.bv (b.value.bv.setWidth 5)⟩
      exact ⟨d, hd, hc⟩
    · simp only [hak, decide_false, Bool.false_eq_true, ↓reduceIte]
      rw [bind_ok_eq (ashr_raw_u32_eq_ok a.unknown_mask b.value),
          bind_ok_eq (ashr_raw_u32_eq_ok a.unwritten_mask b.value),
          bind_ok_eq (ashr_raw_u32_eq_ok a.value b.value)]
      simp only [bind_ok]
      have hat1 := ha.1.2.1
      have hat2 := ha.2.1.2.1
      have hasec := ha.2.2
      refine new32_sound_2safety _ _ _ _
        (BitVec.sshiftRight' x₁ (b.value.bv.setWidth 5))
        (BitVec.sshiftRight' x₂ (b.value.bv.setWidth 5))
        (range32_max_sound _) ?_ (range32_max_sound _) ?_ ?_
      · simp only [u32_and_bv, u32_not_bv, u32_mk_uscalar_bv]
        revert hat1; bv_decide
      · simp only [u32_and_bv, u32_not_bv, u32_mk_uscalar_bv]
        revert hat2; bv_decide
      · simp only [u32_mk_uscalar_bv]
        revert hasec; bv_decide
  · simp only [hbk, decide_false, Bool.false_eq_true, ↓reduceIte]
    exact base_32_sound_2safety a b (fun u v => BitVec.sshiftRight' u (v.setWidth 5)) ha hb

/-! ## 5. Checked Arithmetic, Offset Helpers, and Width Conversions -/

theorem checked_add_scalar_64_sound_2safety (a b : U64ScalarValueData) {x₁ x₂ y₁ y₂ : BitVec 64}
    (ha : ConcretizesScalar64Rel2 a x₁ x₂) (hb : ConcretizesScalar64Rel2 b y₁ y₂) :
    ∃ opt, U64ScalarValueData.checked_add_scalar a b = ok opt ∧
      ∀ res, opt = some res →
        ¬BitVec.uaddOverflow x₁ y₁ ∧ ¬BitVec.uaddOverflow x₂ y₂ ∧
        ConcretizesScalar64Rel2 res (x₁ + y₁) (x₂ + y₂) := by
  unfold U64ScalarValueData.checked_add_scalar U64ScalarValueData.is_known lift
  simp only [u64_guard_eta]
  rw [bind_ok_eq rfl]
  by_cases hak : a.unknown_mask = 0#u64
  · simp only [hak, decide_true, ↓reduceIte]; rw [bind_ok_eq rfl]
    by_cases hbk : b.unknown_mask = 0#u64
    · simp only [hbk, decide_true, ↓reduceIte]; rw [bind_ok_eq rfl]
      have hs := U64.checked_add_bv_spec a.value b.value
      cases hck : U64.checked_add a.value b.value with
      | none => exact ⟨none, rfl, fun _ h => by cases h⟩
      | some v =>
        rw [hck] at hs; dsimp only
        have hx1 := eq_value_of_known_64 hak ha.1
        have hx2 := eq_value_of_known_64 hak ha.2.1
        have hy1 := eq_value_of_known_64 hbk hb.1
        have hy2 := eq_value_of_known_64 hbk hb.2.1
        subst hx1 hx2 hy1 hy2
        obtain ⟨d, hd, _, hc⟩ := from_known_64_sound v
        rw [bind_ok_eq hd]
        have hle : a.value.bv ≤ v.bv := by
          rw [BitVec.le_def, UScalar.bv_toNat, UScalar.bv_toNat]; have := hs.2.1; omega
        rw [hs.2.2] at hle hc
        have hno : ¬BitVec.uaddOverflow a.value.bv b.value.bv := by
          revert hle; bv_decide
        exact ⟨some d, rfl, fun _ hr => by cases hr; exact ⟨hno, hno, hc⟩⟩
    · simp only [hbk, decide_false, Bool.false_eq_true, ↓reduceIte]
      obtain ⟨opt, hopt, hsnd⟩ := range64_checked_add_ok_and_sound a.urange b.urange
      rw [bind_ok_eq hopt]
      cases opt with
      | none => exact ⟨none, rfl, fun _ h => by cases h⟩
      | some ur =>
        dsimp only
        have ⟨hno1, hur1⟩ := hsnd ur rfl x₁ y₁ (concretizes64_range ha.1) (concretizes64_range hb.1)
        have ⟨hno2, hur2⟩ := hsnd ur rfl x₂ y₂ (concretizes64_range ha.2.1) (concretizes64_range hb.2.1)
        obtain ⟨usvd, husvd⟩ := base_64_ok a b
        obtain ⟨res, hbu, hc⟩ := base_update_range_64_sound_2safety a b ur (· + ·) ha hb hur1 hur2
        rw [bind_ok_eq husvd] at hbu
        rw [bind_ok_eq husvd, bind_ok_eq hbu]
        exact ⟨some res, rfl, fun _ hr => by cases hr; exact ⟨hno1, hno2, hc⟩⟩
  · simp only [hak, decide_false, Bool.false_eq_true, ↓reduceIte]
    obtain ⟨opt, hopt, hsnd⟩ := range64_checked_add_ok_and_sound a.urange b.urange
    rw [bind_ok_eq hopt]
    cases opt with
    | none => exact ⟨none, rfl, fun _ h => by cases h⟩
    | some ur =>
      dsimp only
      have ⟨hno1, hur1⟩ := hsnd ur rfl x₁ y₁ (concretizes64_range ha.1) (concretizes64_range hb.1)
      have ⟨hno2, hur2⟩ := hsnd ur rfl x₂ y₂ (concretizes64_range ha.2.1) (concretizes64_range hb.2.1)
      obtain ⟨usvd, husvd⟩ := base_64_ok a b
      obtain ⟨res, hbu, hc⟩ := base_update_range_64_sound_2safety a b ur (· + ·) ha hb hur1 hur2
      rw [bind_ok_eq husvd] at hbu
      rw [bind_ok_eq husvd, bind_ok_eq hbu]
      exact ⟨some res, rfl, fun _ hr => by cases hr; exact ⟨hno1, hno2, hc⟩⟩

theorem checked_sub_scalar_64_sound_2safety (a b : U64ScalarValueData) {x₁ x₂ y₁ y₂ : BitVec 64}
    (ha : ConcretizesScalar64Rel2 a x₁ x₂) (hb : ConcretizesScalar64Rel2 b y₁ y₂) :
    ∃ opt, U64ScalarValueData.checked_sub_scalar a b = ok opt ∧
      ∀ res, opt = some res →
        ¬BitVec.usubOverflow x₁ y₁ ∧ ¬BitVec.usubOverflow x₂ y₂ ∧
        ConcretizesScalar64Rel2 res (x₁ - y₁) (x₂ - y₂) := by
  unfold U64ScalarValueData.checked_sub_scalar U64ScalarValueData.is_known lift
  simp only [u64_guard_eta]
  rw [bind_ok_eq rfl]
  by_cases hak : a.unknown_mask = 0#u64
  · simp only [hak, decide_true, ↓reduceIte]; rw [bind_ok_eq rfl]
    by_cases hbk : b.unknown_mask = 0#u64
    · simp only [hbk, decide_true, ↓reduceIte]; rw [bind_ok_eq rfl]
      have hs := U64.checked_sub_bv_spec a.value b.value
      cases hck : U64.checked_sub a.value b.value with
      | none => exact ⟨none, rfl, fun _ h => by cases h⟩
      | some v =>
        rw [hck] at hs; dsimp only
        have hx1 := eq_value_of_known_64 hak ha.1
        have hx2 := eq_value_of_known_64 hak ha.2.1
        have hy1 := eq_value_of_known_64 hbk hb.1
        have hy2 := eq_value_of_known_64 hbk hb.2.1
        subst hx1 hx2 hy1 hy2
        obtain ⟨d, hd, _, hc⟩ := from_known_64_sound v
        rw [bind_ok_eq hd]
        have hle : b.value.bv ≤ a.value.bv := by
          rw [BitVec.le_def, UScalar.bv_toNat, UScalar.bv_toNat]; exact hs.1
        have hno : ¬BitVec.usubOverflow a.value.bv b.value.bv := by
          revert hle; bv_decide
        rw [hs.2.2] at hc
        exact ⟨some d, rfl, fun _ hr => by cases hr; exact ⟨hno, hno, hc⟩⟩
    · simp only [hbk, decide_false, Bool.false_eq_true, ↓reduceIte]
      obtain ⟨opt, hopt, hsnd⟩ := range64_checked_sub_ok_and_sound a.urange b.urange
      rw [bind_ok_eq hopt]
      cases opt with
      | none => exact ⟨none, rfl, fun _ h => by cases h⟩
      | some ur =>
        dsimp only
        have ⟨hno1, hur1⟩ := hsnd ur rfl x₁ y₁ (concretizes64_range ha.1) (concretizes64_range hb.1)
        have ⟨hno2, hur2⟩ := hsnd ur rfl x₂ y₂ (concretizes64_range ha.2.1) (concretizes64_range hb.2.1)
        obtain ⟨usvd, husvd⟩ := base_64_ok a b
        obtain ⟨res, hbu, hc⟩ := base_update_range_64_sound_2safety a b ur (· - ·) ha hb hur1 hur2
        rw [bind_ok_eq husvd] at hbu
        rw [bind_ok_eq husvd, bind_ok_eq hbu]
        exact ⟨some res, rfl, fun _ hr => by cases hr; exact ⟨hno1, hno2, hc⟩⟩
  · simp only [hak, decide_false, Bool.false_eq_true, ↓reduceIte]
    obtain ⟨opt, hopt, hsnd⟩ := range64_checked_sub_ok_and_sound a.urange b.urange
    rw [bind_ok_eq hopt]
    cases opt with
    | none => exact ⟨none, rfl, fun _ h => by cases h⟩
    | some ur =>
      dsimp only
      have ⟨hno1, hur1⟩ := hsnd ur rfl x₁ y₁ (concretizes64_range ha.1) (concretizes64_range hb.1)
      have ⟨hno2, hur2⟩ := hsnd ur rfl x₂ y₂ (concretizes64_range ha.2.1) (concretizes64_range hb.2.1)
      obtain ⟨usvd, husvd⟩ := base_64_ok a b
      obtain ⟨res, hbu, hc⟩ := base_update_range_64_sound_2safety a b ur (· - ·) ha hb hur1 hur2
      rw [bind_ok_eq husvd] at hbu
      rw [bind_ok_eq husvd, bind_ok_eq hbu]
      exact ⟨some res, rfl, fun _ hr => by cases hr; exact ⟨hno1, hno2, hc⟩⟩

theorem checked_add_64_sound_2safety (a b : U64ScalarValueData) {x₁ x₂ y₁ y₂ : BitVec 64}
    (ha : ConcretizesScalar64Rel2 a x₁ x₂) (hb : ConcretizesScalar64Rel2 b y₁ y₂) :
    ∃ opt, U64ScalarValueData.checked_add a b = ok opt ∧
      ∀ res, opt = some res →
        ¬BitVec.uaddOverflow x₁ y₁ ∧ ¬BitVec.uaddOverflow x₂ y₂ ∧
        ConcretizesScalar64Rel2 res (x₁ + y₁) (x₂ + y₂) :=
  checked_add_scalar_64_sound_2safety a b ha hb

theorem checked_sub_64_sound_2safety (a b : U64ScalarValueData) {x₁ x₂ y₁ y₂ : BitVec 64}
    (ha : ConcretizesScalar64Rel2 a x₁ x₂) (hb : ConcretizesScalar64Rel2 b y₁ y₂) :
    ∃ opt, U64ScalarValueData.checked_sub a b = ok opt ∧
      ∀ res, opt = some res →
        ¬BitVec.usubOverflow x₁ y₁ ∧ ¬BitVec.usubOverflow x₂ y₂ ∧
        ConcretizesScalar64Rel2 res (x₁ - y₁) (x₂ - y₂) :=
  checked_sub_scalar_64_sound_2safety a b ha hb

theorem range64_add_i16_ok_and_sound (r : U64Range) (rhs : Std.I16) :
    ∃ rOut, U64Range.add_i16 r rhs = ok rOut ∧
      ∀ x : BitVec 64, ConcretizesRange64 r x → ConcretizesRange64 rOut (x + rhs.bv.signExtend 64) := by
  unfold U64Range.add_i16 U64Range.from_scalar U64Range.new lift
  rw [bind_ok_eq rfl, bind_ok_eq rfl]
  obtain ⟨rOut, hok, hsnd⟩ := range64_add_ok_and_sound r
    { min := IScalar.hcast .U64 rhs, max := IScalar.hcast .U64 rhs }
  refine ⟨rOut, hok, fun x hx => ?_⟩
  have hrhs : ConcretizesRange64
      { min := IScalar.hcast .U64 rhs, max := IScalar.hcast .U64 rhs } (rhs.bv.signExtend 64) :=
    ⟨BitVec.le_refl _, BitVec.le_refl _⟩
  exact hsnd x (rhs.bv.signExtend 64) hx hrhs

theorem add_u64_sound_2safety (a : U64ScalarValueData) (rhs : Std.U64) {x₁ x₂ : BitVec 64}
    (ha : ConcretizesScalar64Rel2 a x₁ x₂) :
    ∃ res, U64ScalarValueData.add_u64 a rhs = ok res ∧
      ConcretizesScalar64Rel2 res (x₁ + rhs.bv) (x₂ + rhs.bv) := by
  unfold U64ScalarValueData.add_u64
  obtain ⟨d, hd, _, hc⟩ := from_known_64_sound rhs
  rw [bind_ok_eq hd]
  exact add_scalar_64_sound_2safety a d ha hc

theorem add_i16_sound_2safety (a : U64ScalarValueData) (rhs : Std.I16) {x₁ x₂ : BitVec 64}
    (ha : ConcretizesScalar64Rel2 a x₁ x₂) :
    ∃ res, U64ScalarValueData.add_i16 a rhs = ok res ∧
      ConcretizesScalar64Rel2 res (x₁ + rhs.bv.signExtend 64) (x₂ + rhs.bv.signExtend 64) := by
  unfold U64ScalarValueData.add_i16 lift
  rw [bind_ok_eq rfl]
  obtain ⟨d, hd, _, hc⟩ := from_known_64_sound (IScalar.hcast .U64 rhs)
  rw [bind_ok_eq hd]
  exact add_scalar_64_sound_2safety a d ha hc

theorem checked_add_u64_sound_2safety (a : U64ScalarValueData) (rhs : Std.U64) {x₁ x₂ : BitVec 64}
    (ha : ConcretizesScalar64Rel2 a x₁ x₂) :
    ∃ opt, U64ScalarValueData.checked_add_u64 a rhs = ok opt ∧
      ∀ res, opt = some res →
        ¬BitVec.uaddOverflow x₁ rhs.bv ∧ ¬BitVec.uaddOverflow x₂ rhs.bv ∧
        ConcretizesScalar64Rel2 res (x₁ + rhs.bv) (x₂ + rhs.bv) := by
  unfold U64ScalarValueData.checked_add_u64
  obtain ⟨d, hd, _, hc⟩ := from_known_64_sound rhs
  rw [bind_ok_eq hd]
  exact checked_add_scalar_64_sound_2safety a d ha hc

theorem add_i32_trait_sound_2safety (a : U64ScalarValueData) (rhs : Std.I32) {x₁ x₂ : BitVec 64}
    (ha : ConcretizesScalar64Rel2 a x₁ x₂) :
    ∃ res, U64ScalarValueData.Insts.CoreOpsArithAddI32U64ScalarValueData.add a rhs = ok res ∧
      ConcretizesScalar64Rel2 res (x₁ + rhs.bv.signExtend 64) (x₂ + rhs.bv.signExtend 64) := by
  unfold U64ScalarValueData.Insts.CoreOpsArithAddI32U64ScalarValueData.add lift
  rw [bind_ok_eq rfl]
  obtain ⟨d, hd, _, hc⟩ := from_known_64_sound (IScalar.hcast .U64 rhs)
  rw [bind_ok_eq hd]
  exact add_scalar_64_sound_2safety a d ha hc

theorem sub_u64_trait_sound_2safety (a : U64ScalarValueData) (rhs : Std.U64) {x₁ x₂ : BitVec 64}
    (ha : ConcretizesScalar64Rel2 a x₁ x₂) :
    ∃ res, U64ScalarValueData.Insts.CoreOpsArithSubU64U64ScalarValueData.sub a rhs = ok res ∧
      ConcretizesScalar64Rel2 res (x₁ - rhs.bv) (x₂ - rhs.bv) := by
  unfold U64ScalarValueData.Insts.CoreOpsArithSubU64U64ScalarValueData.sub
  obtain ⟨d, hd, _, hc⟩ := from_known_64_sound rhs
  rw [bind_ok_eq hd]
  exact sub_scalar_64_sound_2safety a d ha hc

theorem sub_i32_trait_sound_2safety (a : U64ScalarValueData) (rhs : Std.I32) {x₁ x₂ : BitVec 64}
    (ha : ConcretizesScalar64Rel2 a x₁ x₂) :
    ∃ res, U64ScalarValueData.Insts.CoreOpsArithSubI32U64ScalarValueData.sub a rhs = ok res ∧
      ConcretizesScalar64Rel2 res (x₁ - rhs.bv.signExtend 64) (x₂ - rhs.bv.signExtend 64) := by
  unfold U64ScalarValueData.Insts.CoreOpsArithSubI32U64ScalarValueData.sub lift
  rw [bind_ok_eq rfl]
  obtain ⟨d, hd, _, hc⟩ := from_known_64_sound (IScalar.hcast .U64 rhs)
  rw [bind_ok_eq hd]
  exact sub_scalar_64_sound_2safety a d ha hc

theorem sub_i16_trait_sound_2safety (a : U64ScalarValueData) (rhs : Std.I16) {x₁ x₂ : BitVec 64}
    (ha : ConcretizesScalar64Rel2 a x₁ x₂) :
    ∃ res, U64ScalarValueData.Insts.CoreOpsArithSubI16U64ScalarValueData.sub a rhs = ok res ∧
      ConcretizesScalar64Rel2 res (x₁ - rhs.bv.signExtend 64) (x₂ - rhs.bv.signExtend 64) := by
  unfold U64ScalarValueData.Insts.CoreOpsArithSubI16U64ScalarValueData.sub lift
  rw [bind_ok_eq rfl]
  obtain ⟨d, hd, _, hc⟩ := from_known_64_sound (IScalar.hcast .U64 rhs)
  rw [bind_ok_eq hd]
  exact sub_scalar_64_sound_2safety a d ha hc

theorem from_u64_scalar_sound_2safety (v : U64ScalarValueData) {x₁ x₂ : BitVec 64}
    (hv : ConcretizesScalar64Rel2 v x₁ x₂) :
    ∃ res, U32ScalarValueData.from_u64_scalar v = ok res ∧
      ConcretizesScalar32Rel2 res (x₁.setWidth 32) (x₂.setWidth 32) := by
  unfold U32ScalarValueData.from_u64_scalar U64ScalarValueData.max U64ScalarValueData.min
    U32Range.impl.max U32Range.new lift
  rw [bind_ok_eq rfl, bind_ok_eq (u64_shr_32_i32_eq_ok v.urange.max),
      bind_ok_eq rfl, bind_ok_eq (u64_shr_32_i32_eq_ok v.urange.min)]
  rcases hv with ⟨⟨_, ht1, hmin1, hmax1⟩, ⟨_, ht2, hmin2, hmax2⟩, hsec⟩
  by_cases hhi : (⟨v.urange.max.bv >>> 32#6⟩ : Std.U64) = ⟨v.urange.min.bv >>> 32#6⟩
  · have hhi' : v.urange.max.bv >>> 32#6 = v.urange.min.bv >>> 32#6 := (u64_eq_iff_bv _ _).1 hhi
    simp only [hhi, ↓reduceIte, bind_ok]
    refine new32_sound_2safety _ _ _ _ (x₁.setWidth 32) (x₂.setWidth 32) ?_ ?_ ?_ ?_ ?_
    · unfold ConcretizesRange32
      simp only [cast_u64_to_u32_bv]
      revert hhi' hmin1 hmax1; bv_decide
    · simp only [cast_u64_to_u32_bv]; revert ht1; bv_decide
    · unfold ConcretizesRange32
      simp only [cast_u64_to_u32_bv]
      revert hhi' hmin2 hmax2; bv_decide
    · simp only [cast_u64_to_u32_bv]; revert ht2; bv_decide
    · simp only [cast_u64_to_u32_bv]; revert hsec; bv_decide
  · simp only [hhi, ↓reduceIte, bind_ok]
    refine new32_sound_2safety _ _ _ _ (x₁.setWidth 32) (x₂.setWidth 32)
      (range32_max_sound _) ?_ (range32_max_sound _) ?_ ?_
    · simp only [cast_u64_to_u32_bv]; revert ht1; bv_decide
    · simp only [cast_u64_to_u32_bv]; revert ht2; bv_decide
    · simp only [cast_u64_to_u32_bv]; revert hsec; bv_decide

theorem into_u64_scalar_sound_2safety (v : U32ScalarValueData) {x₁ x₂ : BitVec 32}
    (hv : ConcretizesScalar32Rel2 v x₁ x₂) :
    ∃ res, U32ScalarValueData.into_u64_scalar v = ok res ∧
      ConcretizesScalar64Rel2 res (x₁.zeroExtend 64) (x₂.zeroExtend 64) := by
  unfold U32ScalarValueData.into_u64_scalar U32ScalarValueData.min U32ScalarValueData.max
    U64Range.new lift
  simp only [bind_ok]
  rcases hv with ⟨⟨_, ht1, hmin1, hmax1⟩, ⟨_, ht2, hmin2, hmax2⟩, hsec⟩
  refine new64_sound_2safety _ _ _ _ (x₁.zeroExtend 64) (x₂.zeroExtend 64) ?_ ?_ ?_ ?_ ?_
  · unfold ConcretizesRange64
    simp only [cast_u32_to_u64_bv]
    revert hmin1 hmax1; bv_decide
  · simp only [cast_u32_to_u64_bv]; revert ht1; bv_decide
  · unfold ConcretizesRange64
    simp only [cast_u32_to_u64_bv]
    revert hmin2 hmax2; bv_decide
  · simp only [cast_u32_to_u64_bv]; revert ht2; bv_decide
  · simp only [cast_u32_to_u64_bv]; revert hsec; bv_decide

/-! ## 6. Contract Theorems 3, 4, and 5 -/

theorem u64_range_add_sound
    (r1 r2 : ebpf_core.scalar_value.U64Range)
    (x y : BitVec 64)
    (hx : ConcretizesRange64 r1 x)
    (hy : ConcretizesRange64 r2 y) :
    ∃ rOut,
      ebpf_core.scalar_value.U64Range.add_range r1 r2 = .ok rOut ∧
      ConcretizesRange64 rOut (x + y) := by
  obtain ⟨rOut, hok, hsnd⟩ := range64_add_ok_and_sound r1 r2
  exact ⟨rOut, hok, hsnd x y hx hy⟩

theorem u64_scalar_add_sound
    (d1 d2 : ebpf_core.scalar_value.U64ScalarValueData)
    (x1 x2 y1 y2 : BitVec 64)
    (hx : ConcretizesScalar64Rel2 d1 x1 x2)
    (hy : ConcretizesScalar64Rel2 d2 y1 y2) :
    ∃ dOut,
      ebpf_core.scalar_value.U64ScalarValueData.add_scalar d1 d2 = .ok dOut ∧
      ConcretizesScalar64Rel2 dOut (x1 + y1) (x2 + y2) :=
  add_scalar_64_sound_2safety d1 d2 hx hy

theorem u64_scalar_bitand_sound
    (d1 d2 : ebpf_core.scalar_value.U64ScalarValueData)
    (x1 x2 y1 y2 : BitVec 64)
    (hx : ConcretizesScalar64Rel2 d1 x1 x2)
    (hy : ConcretizesScalar64Rel2 d2 y1 y2) :
    ∃ dOut,
      ebpf_core.scalar_value.U64ScalarValueData.bitand_scalar d1 d2 = .ok dOut ∧
      ConcretizesScalar64Rel2 dOut (x1 &&& y1) (x2 &&& y2) :=
  bitand_scalar_64_sound_2safety d1 d2 hx hy

end StarnixEbpf.Proof
