# Formal Verification of `ebpf_core` (`//src/starnix/lib/ebpf`)

This package formally specifies and verifies the Starnix eBPF core crate
([`//src/starnix/lib/ebpf/core`](../core)) using Charon, Aeneas, and Lean 4.

See [`//build/testing/formal_verification/README.md`](../../../../build/testing/formal_verification/README.md)
for the general verification architecture, `spec/` vs. `proof/` separation,
[`spec/RISK.md`](spec/RISK.md) / [`proof/RISK.md`](proof/RISK.md) /
[`REVIEW.md`](REVIEW.md) governance, and local toolchain setup.

## eBPF Specification Overview

- **[`spec/Assumptions.lean`](spec/Assumptions.lean) - External Host & Callback Assumptions**:
  - `HostLayout` and `ValidHostLayout`: non-null, non-overlapping 64-bit host
    virtual address assignment for the 512-byte eBPF stack, context/packet
    buffers, map objects, and array map data buffers.
  - `CoreCallingContext` and `EntryStatesConform`: entry-state preconditions on
    `%r1`..`%r5`, `%r10` (frame pointer at stack offset 512), and uninitialized
    stack/register bits.
  - `ValidVerifierLogger`: totality of the external `CoreVerifierLogger` trait
    (`is_enabled`, `log_instruction`, `log_error`).
- **[`spec/Spec.lean`](spec/Spec.lean) - eBPF VM Operational Semantics & Safety Properties**:
  - `SafetyViolation`: complete catalog of stuck states covering invalid PCs/jumps,
    invalid registers, writes to read-only `%r10`, out-of-bounds or misaligned
    stack/memory/packet accesses, uninitialized stack reads, partial overwrite
    of spilled capabilities, pointer leaks to memory or return values,
    use-after-release, double-release, resource leaks on exit, and helper
    contract violations.
  - `step` and `runSteps`: reference small-step and bounded-step operational
    semantics of decoded eBPF programs.
  - `SafeAndTerminating` and `NonInterfering`: single-run memory/control-flow/resource
    safety and relational 2-safety non-interference (guaranteeing that neither
    KASLR pointer layouts nor uninitialized stack/register bytes leak into the
    return value or observable world state).
  - `ConcretizesRange64`, `ConcretizesScalar64`, and `ConcretizesScalar64Rel2`:
    concretization predicates for the 64-bit and 32-bit abstract scalar domains.
  - `BackendSimulatesProgram`: simulation contract assumed of the host
    `EbpfMachineBackend` implementation (`load_memory`, `store_memory`,
    `atomic_rmw32`/`64`, `atomic_cmpxchg32`/`64`, `call_helper`,
    `load_from_packet`).
- **[`spec/Contract.lean`](spec/Contract.lean) - Top-Level Verified Theorems**:
  - Soundness of `verify_program_core` (`verify_program_core_implies_safe_and_terminating`,
    `verify_program_core_implies_non_interfering`).
  - Simulation of `execute_core_loop1` (`execute_core_simulates_run_steps`).
  - Structural invariants of `link_instructions` (`link_instructions_preserves_length`)
    and `cbpf_to_ebpf_core` (`cbpf_to_ebpf_core_nonempty_on_ok`).
  - Abstract domain and instruction helper soundness (`u64_range_add_sound`,
    `u64_scalar_add_sound`, `u64_scalar_bitand_sound`,
    `ebpf_instruction_set_code_roundtrip`, `apply_endianness_be_involutive`).
- **[`proof/Proof.lean`](proof/Proof.lean) & [`proof/Proof/`](proof/Proof/) - Engineer-Editable Proofs**:
  - Machine-checked Lean 4 proof modules discharging every theorem in
    `spec/Contract.lean` with zero `sorry`s.

## Running `:ebpf_verification_test`

```bash
fx test --host ebpf_verification_test
```
