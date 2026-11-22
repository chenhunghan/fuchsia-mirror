-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Proof.ApiConverterLinkerExec
import Proof.ScalarAlu
import Proof.SubsumptionAndTopLevel

/-!
# Correctness Proofs for `ebpf_core` (`proof/Proof.lean`)

Re-exports all machine-checked proof modules discharging every theorem in
`spec/Contract.lean`:

- `Proof.ApiConverterLinkerExec`: `EbpfInstruction.set_code`/`code` roundtrip,
  big-endian `apply_endianness` involutivity, `cbpf_to_ebpf_core` non-emptiness,
  `link_instructions` length preservation, and `execute_core_loop1` simulation.
- `Proof.ScalarAlu`: soundness and 2-safety relational concretization for
  `U64Range.add_range`, `U64ScalarValueData.add_scalar`, and
  `U64ScalarValueData.bitand_scalar`.
- `Proof.SubsumptionAndTopLevel` (and upstream verifier modules): end-to-end
  single-execution safety & termination (`verify_program_core_implies_safe_and_terminating`)
  and relational 2-safety non-interference (`verify_program_core_implies_non_interfering`).
-/

namespace StarnixEbpf.Proof

export StarnixEbpf.ApiConverterLinkerExec (
  ebpf_instruction_set_code_roundtrip
  apply_endianness_be_involutive
  cbpf_to_ebpf_core_nonempty_on_ok
  link_instructions_preserves_length
  execute_core_simulates_run_steps
)

end StarnixEbpf.Proof
