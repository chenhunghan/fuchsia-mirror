-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Assumptions
import Spec
import Proof
import Extracted

/-!
# Top-Level Verified Contract for `ebpf_core` (`spec/Contract.lean`)

This security-critical module (`spec/Contract.lean`, classified as High risk in
`RISK.md` and governed by `../REVIEW.md`) states the top-level theorems for
`//src/starnix/lib/ebpf/core`, delegates each proof body to `StarnixEbpf.Proof`,
and audits the axioms of every theorem using `#print axioms`.

Every theorem in this file must be preceded by a `/-- **Informal**: ... -/`
docstring explaining the theorem in clear, precise English.
-/

namespace StarnixEbpf.Contract

open Aeneas.Std
open StarnixEbpf.Assumptions
open StarnixEbpf.Spec

/--
**Informal**: Writing an 8-bit opcode into an `EbpfInstruction` with `set_code`
and reading it back with `code` always succeeds and returns the exact opcode
that was written.
-/
theorem ebpf_instruction_set_code_roundtrip
    (insn : ebpf_core.api.EbpfInstruction) (opcode : U8) :
    ∃ insn',
      ebpf_core.api.EbpfInstruction.set_code insn opcode = .ok insn' ∧
      ebpf_core.api.EbpfInstruction.code insn' = .ok opcode := by
  exact StarnixEbpf.Proof.ebpf_instruction_set_code_roundtrip insn opcode

#print axioms ebpf_instruction_set_code_roundtrip

/--
**Informal**: Applying big-endian byte swapping (`apply_endianness` with
`to_be = true`) twice at the same data width `w` always succeeds and returns
the original 64-bit value masked to width `w`.
-/
theorem apply_endianness_be_involutive
    (w : ebpf_core.api.DataWidth) (v : U64) :
    ∃ v1 v2,
      ebpf_core.api.apply_endianness true w v = .ok v1 ∧
      ebpf_core.api.apply_endianness true w v1 = .ok v2 ∧
      v2.bv = v.bv &&& (DataWidth.ofCore w).valMask64 := by
  exact StarnixEbpf.Proof.apply_endianness_be_involutive w v

#print axioms apply_endianness_be_involutive

/--
**Informal**: Interval addition on 64-bit unsigned ranges (`U64Range.add_range`)
is sound: for any concrete 64-bit words `x` in `r1` and `y` in `r2`, their
modular 64-bit sum `x + y` lies within the resulting `U64Range`.
-/
theorem u64_range_add_sound
    (r1 r2 : ebpf_core.scalar_value.U64Range) (x y : BitVec 64)
    (hx : ConcretizesRange64 r1 x)
    (hy : ConcretizesRange64 r2 y) :
    ∃ rOut,
      ebpf_core.scalar_value.U64Range.add_range r1 r2 = .ok rOut ∧
      ConcretizesRange64 rOut (x + y) := by
  exact StarnixEbpf.Proof.u64_range_add_sound r1 r2 x y hx hy

#print axioms u64_range_add_sound

/--
**Informal**: Abstract 64-bit scalar addition (`U64ScalarValueData.add_scalar`)
is sound for both single-run bounds/tnum tracking and 2-safety initialization
tracking: if `(x1, x2)` concretize `d1` and `(y1, y2)` concretize `d2` while
agreeing on all initialized bits, then `(x1 + y1, x2 + y2)` concretize the
result of `add_scalar d1 d2` and agree on all bits marked initialized in the
result.
-/
theorem u64_scalar_add_sound
    (d1 d2 : ebpf_core.scalar_value.U64ScalarValueData)
    (x1 x2 y1 y2 : BitVec 64)
    (hx : ConcretizesScalar64Rel2 d1 x1 x2)
    (hy : ConcretizesScalar64Rel2 d2 y1 y2) :
    ∃ dOut,
      ebpf_core.scalar_value.U64ScalarValueData.add_scalar d1 d2 = .ok dOut ∧
      ConcretizesScalar64Rel2 dOut (x1 + y1) (x2 + y2) := by
  exact StarnixEbpf.Proof.u64_scalar_add_sound d1 d2 x1 x2 y1 y2 hx hy

#print axioms u64_scalar_add_sound

/--
**Informal**: Abstract 64-bit bitwise AND (`U64ScalarValueData.bitand_scalar`)
is sound for both single-run bounds/tnum tracking and 2-safety initialization
tracking, including zero-masking of uninitialized bits when ANDed with a known
zero bit.
-/
theorem u64_scalar_bitand_sound
    (d1 d2 : ebpf_core.scalar_value.U64ScalarValueData)
    (x1 x2 y1 y2 : BitVec 64)
    (hx : ConcretizesScalar64Rel2 d1 x1 x2)
    (hy : ConcretizesScalar64Rel2 d2 y1 y2) :
    ∃ dOut,
      ebpf_core.scalar_value.U64ScalarValueData.bitand_scalar d1 d2 = .ok dOut ∧
      ConcretizesScalar64Rel2 dOut (x1 &&& y1) (x2 &&& y2) := by
  exact StarnixEbpf.Proof.u64_scalar_bitand_sound d1 d2 x1 x2 y1 y2 hx hy

#print axioms u64_scalar_bitand_sound

/--
**Informal**: Classic BPF (cBPF) to eBPF translation (`cbpf_to_ebpf_core`)
never produces an empty eBPF program: whenever `cbpf_to_ebpf_core` returns
`Ok(ebpf_code)`, the resulting instruction vector has at least one instruction.
-/
theorem cbpf_to_ebpf_core_nonempty_on_ok
    (bpf_code : Slice ebpf_core.api.SockFilter)
    (config : ebpf_core.converter.CbpfConfig)
    (ebpf_code : alloc.vec.Vec ebpf_core.api.EbpfInstruction)
    (h_ok : ebpf_core.converter.cbpf_to_ebpf_core bpf_code config =
      .ok (.Ok ebpf_code)) :
    0 < ebpf_code.val.length := by
  exact StarnixEbpf.Proof.cbpf_to_ebpf_core_nonempty_on_ok bpf_code config ebpf_code h_ok

#print axioms cbpf_to_ebpf_core_nonempty_on_ok

/--
**Informal**: Single-Execution Safety & Termination of `verify_program_core`:
if `verify_program_core` accepts `rawCode` under calling context `cctx` and a
valid logger, then `verified.code` decodes to a well-formed `Program` of the
same length and executing `prog` from any conforming initial machine state
terminates in at most `VERIFIER_MAX_STEPS` (`655,361`) steps without triggering
any `SafetyViolation` (no out-of-bounds or misaligned stack/memory/packet
accesses, no uninitialized stack reads, no invalid register accesses or writes
to `r10`, no helper contract violations, and zero leaked resources on exit).
-/
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
  exact StarnixEbpf.Proof.verify_program_core_implies_safe_and_terminating
    loggerInst rawCode cctx logger logger' verified env mkInit h_logger h_entry h_verif

#print axioms verify_program_core_implies_safe_and_terminating

/--
**Informal**: Relational 2-Safety Non-Interference of `verify_program_core`:
if `verify_program_core` accepts `rawCode` with pointer leaks disabled
(`cctx.allow_ptr_leaks = false`), then `verified.code` decodes to a well-formed
`Program` `prog` of the same length, and for any two valid host pointer layouts
`layout₁, layout₂` and any two initial uninitialized stack/register bit
assignments `secret₁, secret₂`, both executions terminate in at most
`VERIFIER_MAX_STEPS` (`655,361`) steps without safety violations and produce the
exact same scalar return value and observable world state. Neither KASLR pointer
values nor uninitialized host stack/register bits can leak.
-/
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
  exact StarnixEbpf.Proof.verify_program_core_implies_non_interfering
    loggerInst rawCode cctx logger logger' verified env mkInit
    h_logger h_entry h_no_ptr_leaks h_verif

#print axioms verify_program_core_implies_non_interfering

/--
**Informal**: Bytecode linking (`link_instructions`) preserves the instruction
stream length of a `VerifiedEbpfProgram` whenever linking succeeds.
-/
theorem link_instructions_preserves_length
    (verified : ebpf_core.verifier.VerifiedEbpfProgram)
    (linkage : ebpf_core.program.ProgramLinkageInfo)
    (linkedCode : alloc.vec.Vec ebpf_core.api.EbpfInstruction)
    (h_link : ebpf_core.program.link_instructions verified linkage =
      .ok (.Ok linkedCode)) :
    linkedCode.val.length = verified.code.val.length := by
  exact StarnixEbpf.Proof.link_instructions_preserves_length verified linkage linkedCode h_link

#print axioms link_instructions_preserves_length

/--
**Informal**: Interpreter simulation (`execute_core_simulates_run_steps`): for
any host `EbpfMachineBackend` that refines single-step transitions
(`BackendSimulatesProgram`), whenever the reference semantics `runSteps` halts
with return value `ret`, the extracted Rust interpreter loop `execute_core_loop1`
terminates without runtime error and returns `Ok(ret)`.
-/
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
  exact StarnixEbpf.Proof.execute_core_simulates_run_steps
    inst code prog env layout secret R fuel b st istate ret finalWorld
    h_sim h_rel h_res h_pc h_regs h_run

#print axioms execute_core_simulates_run_steps

end StarnixEbpf.Contract
