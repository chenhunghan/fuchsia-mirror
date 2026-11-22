-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Aeneas

namespace StarnixEbpf.LoopLemmas

open Aeneas.Std
open Aeneas.Data.Coinductive
open Lean.Order

theorem ok_inj {α : Type u} {x y : α} (h : (Result.ok x : Result α) = Result.ok y) : x = y :=
  Result.ok_injective h

theorem bind_eq_ok {α : Type u} {β : Type v} {m : Result α} {f : α → Result β} {y : β}
    (h : bind m f = Result.ok y) :
    ∃ x, m = Result.ok x ∧ f x = Result.ok y := by
  cases m using Result.cases with
  | ret x =>
      refine ⟨x, rfl, ?_⟩
      simpa using h
  | vis eff k =>
      simp at h
  | div =>
      simp at h

section UnsealedResult

unseal Result

theorem eq_ok_admissible {β : Type v} (P : β → Prop) :
    admissible (fun m : Result β => ∀ y, m = Result.ok y → P y) := by
  intro c hc hAll y hSup
  have hSup' : CCPO.csup hc = ITree.ret y := by
    simpa [Result.ok] using hSup
  have hMem : c (ITree.ret y) := ITree.csup_ret_mem hc hSup'
  exact hAll (ITree.ret y) hMem y rfl

end UnsealedResult

theorem loop_ok_induct {α : Type u} {β : Type v}
    (body : α → Result (ControlFlow α β))
    (Inv : α → Prop)
    (Post : β → Prop)
    (h_step : ∀ x, Inv x →
      (∀ x', body x = Result.ok (.cont x') → Inv x') ∧
      (∀ y, body x = Result.ok (.done y) → Post y)) :
    ∀ x y, Inv x → loop body x = Result.ok y → Post y := by
  have h_adm : admissible (fun (f : α → Result β) => ∀ x, Inv x → ∀ y, f x = Result.ok y → Post y) := by
    apply admissible_pi
    intro x
    apply admissible_pi
    intro _
    exact admissible_apply (fun _ m => ∀ y, m = Result.ok y → Post y) x
      (eq_ok_admissible Post)
  have h_ind : ∀ x, Inv x → ∀ y, loop body x = Result.ok y → Post y := by
    refine loop.fixpoint_induct body
      (motive := fun f => ∀ x, Inv x → ∀ y, f x = Result.ok y → Post y)
      h_adm ?_
    intro f ih x hx y hstep
    rcases bind_eq_ok hstep with ⟨r, hr, hcont⟩
    cases r with
    | cont x' =>
        exact ih x' ((h_step x hx).1 x' hr) y hcont
    | done y' =>
        have hy : y' = y := ok_inj hcont
        subst hy
        exact (h_step x hx).2 y' hr
  intro x y hx hloop
  exact h_ind x hx y hloop

theorem loop_ok_rel_induct {α : Type u} {β : Type v}
    (body : α → Result (ControlFlow α β))
    (Post : α → β → Prop)
    (h_step : ∀ x,
      (∀ x' y, body x = Result.ok (.cont x') → Post x' y → Post x y) ∧
      (∀ y, body x = Result.ok (.done y) → Post x y)) :
    ∀ x y, loop body x = Result.ok y → Post x y := by
  have h_adm : admissible (fun (f : α → Result β) => ∀ x y, f x = Result.ok y → Post x y) := by
    apply admissible_pi
    intro x
    exact admissible_apply (fun _ m => ∀ y, m = Result.ok y → Post x y) x
      (eq_ok_admissible (Post x))
  refine loop.fixpoint_induct body
    (motive := fun f => ∀ x y, f x = Result.ok y → Post x y)
    h_adm ?_
  intro f ih x y hstep
  rcases bind_eq_ok hstep with ⟨r, hr, hcont⟩
  cases r with
  | cont x' =>
      exact (h_step x).1 x' y hr (ih x' y hcont)
  | done y' =>
      have hy : y' = y := ok_inj hcont
      subst hy
      exact (h_step x).2 y' hr

end StarnixEbpf.LoopLemmas
