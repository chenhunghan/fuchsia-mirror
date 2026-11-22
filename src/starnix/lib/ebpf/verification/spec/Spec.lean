-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Assumptions
import Extracted

/-!
# Formal Safety & Functional Specification of the Starnix eBPF Virtual Machine

This security-critical module (`spec/Spec.lean`, classified as High risk in
`RISK.md` and governed by `../REVIEW.md`) defines the reference operational
semantics and safety properties for `//src/starnix/lib/ebpf/core`:
1. Complete catalog of runtime safety violations (`SafetyViolation`) covering
   S1-S8 (control-flow bounds, register discipline, memory/stack/packet bounds
   and alignment, initialized reads, resource linearity, and information-flow
   isolation).
2. Bytecode decoding (`decodeVerifiedCode`) and reference small-step (`step`)
   and bounded-step (`runSteps`) operational semantics of verified eBPF
   programs.
3. Top-level safety predicates (`SafeAndTerminating`, `NonInterfering`).
4. Concretization and 2-safety relational predicates for the abstract scalar
   domain (`ebpf_core.scalar_value`).
5. Bridge predicates relating the bytecode interpreter (`execute_core_loop1`)
   to the reference operational semantics.

Every top-level formal declaration is paired with a mandatory `**Informal**:`
English specification.
-/

namespace StarnixEbpf.Spec

open StarnixEbpf.Assumptions

/--
**Informal**: Complete catalog of runtime safety violations (`SafetyViolation`)
that the eBPF verifier (`verify_program_core`) must statically rule out on all
reachable execution paths:
- Out-of-bounds program counter or jumping into the middle of a 16-byte `LDDW`
  instruction (`invalidPc`, `invalidMidLddw`).
- Accessing a register index outside `r0`..`r10` or writing to the read-only
  frame pointer `r10` (`invalidRegister`, `writeToReadOnlyFramePtr`).
- Out-of-bounds, misaligned, uninitialized, or sub-word pointer stack accesses
  (`outOfBoundsStackAccess`, `misalignedStackAccess`, `uninitializedStackRead`,
  `partialNonScalarStackAccess`).
- Out-of-bounds or read-only memory, packet, or context struct accesses
  (`outOfBoundsMemoryAccess`, `outOfBoundsArrayAccess`,
  `invalidStructFieldAccess`, `invalidDereference`, `writeToReadOnlyMemory`).
- Misaligned or invalid-target atomic operations (`misalignedAtomicAccess`,
  `invalidAtomicTarget`).
- Leaking a capability/pointer into shared memory, byte swaps (when disallowed),
  or the program return value (`pointerLeakToMemoryOrReturn`).
- Using a releasable resource after release, releasing it twice, or exiting
  while a releasable resource remains unreleased (`useAfterRelease`,
  `doubleRelease`, `resourceLeakOnExit`).
- Calling an unregistered helper, violating a helper parameter contract, or
  executing a legacy packet load without a valid packet context in `r6`
  (`unregisteredHelper`, `helperContractViolation`, `invalidPacketContext`).
-/
inductive SafetyViolation where
  | invalidPc (pc : Nat)
  | invalidMidLddw (pc : Nat)
  | invalidRegister (reg : Nat)
  | writeToReadOnlyFramePtr
  | outOfBoundsStackAccess (off : BitVec 64) (width : DataWidth)
  | misalignedStackAccess (off : BitVec 64) (width : DataWidth)
  | uninitializedStackRead (off : BitVec 64) (width : DataWidth)
  | partialNonScalarStackAccess (off : BitVec 64) (width : DataWidth)
  | outOfBoundsMemoryAccess (id : MemoryId) (off : BitVec 64) (width : DataWidth)
  | outOfBoundsArrayAccess (id : MemoryId) (off : BitVec 64) (width : DataWidth)
  | invalidStructFieldAccess (id : MemoryId) (off : BitVec 64) (width : DataWidth)
  | invalidDereference
  | writeToReadOnlyMemory (id : MemoryId) (origin : MemoryOrigin)
  | misalignedAtomicAccess (off : BitVec 64) (width : DataWidth)
  | invalidAtomicTarget
  | pointerLeakToMemoryOrReturn
  | useAfterRelease (resId : MemoryId)
  | doubleRelease (resId : MemoryId)
  | resourceLeakOnExit (remaining : List MemoryId)
  | unregisteredHelper (idx : Nat)
  | helperContractViolation (idx : Nat)
  | invalidPacketContext
  deriving DecidableEq

/--
**Informal**: Binary arithmetic and bitwise operator kinds supported by 32-bit
and 64-bit eBPF ALU instructions in `ebpf_core`.
-/
inductive AluOp where
  | add | sub | mul | div | rem
  | and | or | xor | lsh | rsh | arsh | mov
  deriving DecidableEq, Repr

/--
**Informal**: Comparison operators supported by 32-bit and 64-bit eBPF
conditional jump instructions (`JumpType` in `ebpf_core::verifier`).
-/
inductive JumpOp where
  | eq | ne
  | gt | ge | lt | le
  | sgt | sge | slt | sle
  | set
  deriving DecidableEq, Repr

/--
**Informal**: Source operand of an eBPF ALU, jump, or store instruction: either
an architectural register `r` or a 64-bit sign-extended immediate constant `v`.
-/
inductive Operand where
  | reg (r : Nat)
  | imm (v : BitVec 64)
  deriving DecidableEq, Repr

/--
**Informal**: Atomic read-modify-write operation kind (`add`, `or`, `and`,
`xor` with optional `fetch`, `xchg`, or `cmpxchg`).
-/
inductive AtomicOpKind where
  | add (fetch : Bool)
  | or  (fetch : Bool)
  | and (fetch : Bool)
  | xor (fetch : Bool)
  | xchg
  | cmpxchg
  deriving DecidableEq, Repr

/--
**Informal**: Width (16, 32, or 64 bits) of an eBPF byte-swap / endianness
conversion instruction (`BPF_END`).
-/
inductive EndianWidth where
  | w16 | w32 | w64
  deriving DecidableEq, Repr

/--
**Informal**: Decoded abstract syntax of a single eBPF instruction slot,
matching the instruction dispatch in `ebpf_core::visitor::BpfVisitor` and
`ebpf_core::verifier::CoreCallingContext::resolve_lddw`.
-/
inductive Insn where
  | alu64 (op : AluOp) (dst : Nat) (src : Operand)
  | alu32 (op : AluOp) (dst : Nat) (src : Operand)
  | neg (is64 : Bool) (dst : Nat)
  | endian (doSwap : Bool) (w : EndianWidth) (dst : Nat)
  | lddwImm (dst : Nat) (imm : BitVec 64)
  | lddwMap (dst : Nat) (schemaIdx : Nat)
  | lddwMapValue (dst : Nat) (schemaIdx : Nat) (offset : BitVec 64)
  | lddwCont
  | jump (offset : Int)
  | condJump (is64 : Bool) (op : JumpOp) (dst : Nat) (src : Operand) (offset : Int)
  | load (w : DataWidth) (dst : Nat) (base : Nat) (offset : BitVec 16)
  | store (w : DataWidth) (base : Nat) (offset : BitVec 16) (src : Operand)
  | atomic (is64 : Bool) (op : AtomicOpKind) (base : Nat) (offset : BitVec 16) (src : Nat)
  | loadPacket (w : DataWidth) (immOff : BitVec 32) (indReg : Option Nat)
  | callExternal (helperIdx : Nat)
  | exit
  deriving DecidableEq, Repr

/--
**Informal**: A decoded eBPF program represented as a flat array of instruction
slots.
-/
abbrev Program := Array Insn

/--
**Informal**: Decodes an atomic operation sub-opcode from the low 8 bits of an
`EbpfInstruction` immediate field, returning `none` for unsupported atomic
encodings.
-/
def decodeAtomicOp (op8 : BitVec 8) : Option AtomicOpKind :=
  if op8 == 0x00#8 then some (.add false)
  else if op8 == 0x01#8 then some (.add true)
  else if op8 == 0x40#8 then some (.or false)
  else if op8 == 0x41#8 then some (.or true)
  else if op8 == 0x50#8 then some (.and false)
  else if op8 == 0x51#8 then some (.and true)
  else if op8 == 0xa0#8 then some (.xor false)
  else if op8 == 0xa1#8 then some (.xor true)
  else if op8 == 0xe1#8 then some .xchg
  else if op8 == 0xf1#8 then some .cmpxchg
  else none

/--
**Informal**: Decodes the byte-swap width (`16`, `32`, or `64`) from the 32-bit
immediate field of a `BPF_END` instruction.
-/
def decodeEndianWidth (imm32 : BitVec 32) : Option EndianWidth :=
  if imm32 == 16#32 then some .w16
  else if imm32 == 32#32 then some .w32
  else if imm32 == 64#32 then some .w64
  else none

/--
**Informal**: Decodes a single 64-bit non-`LDDW` `EbpfInstruction` word into an
`Insn`, enforcing the exact immediate-divisor, shift-range, byte-swap-width,
call-source, and atomic-sub-opcode checks performed by
`ebpf_core::visitor::BpfVisitor::visit`.
-/
def decodeSingleInsn (raw : ebpf_core.api.EbpfInstruction) : Option Insn :=
  let w : BitVec 64 := raw.bv
  let opcode : BitVec 8 := w.setWidth 8
  let dst : Nat := ((w >>> 8) &&& 0xF#64).toNat
  let srcReg : Nat := ((w >>> 12) &&& 0xF#64).toNat
  let off16 : BitVec 16 := (w >>> 16).setWidth 16
  let offInt : Int := off16.toInt
  let imm32 : BitVec 32 := (w >>> 32).setWidth 32
  let immSext64 : BitVec 64 := imm32.signExtend 64
  let srcImm : Operand := .imm immSext64
  let srcX : Operand := .reg srcReg
  -- 32-bit ALU
  if opcode == 0x04#8 then some (.alu32 .add dst srcImm)
  else if opcode == 0x0c#8 then some (.alu32 .add dst srcX)
  else if opcode == 0x14#8 then some (.alu32 .sub dst srcImm)
  else if opcode == 0x1c#8 then some (.alu32 .sub dst srcX)
  else if opcode == 0x24#8 then some (.alu32 .mul dst srcImm)
  else if opcode == 0x2c#8 then some (.alu32 .mul dst srcX)
  else if opcode == 0x34#8 then
    if imm32 == 0#32 then none else some (.alu32 .div dst srcImm)
  else if opcode == 0x3c#8 then some (.alu32 .div dst srcX)
  else if opcode == 0x44#8 then some (.alu32 .or dst srcImm)
  else if opcode == 0x4c#8 then some (.alu32 .or dst srcX)
  else if opcode == 0x54#8 then some (.alu32 .and dst srcImm)
  else if opcode == 0x5c#8 then some (.alu32 .and dst srcX)
  else if opcode == 0x64#8 then
    if !imm32.msb && imm32 < 32#32 then some (.alu32 .lsh dst srcImm) else none
  else if opcode == 0x6c#8 then some (.alu32 .lsh dst srcX)
  else if opcode == 0x74#8 then
    if !imm32.msb && imm32 < 32#32 then some (.alu32 .rsh dst srcImm) else none
  else if opcode == 0x7c#8 then some (.alu32 .rsh dst srcX)
  else if opcode == 0x84#8 then some (.neg false dst)
  else if opcode == 0x94#8 then
    if imm32 == 0#32 then none else some (.alu32 .rem dst srcImm)
  else if opcode == 0x9c#8 then some (.alu32 .rem dst srcX)
  else if opcode == 0xa4#8 then some (.alu32 .xor dst srcImm)
  else if opcode == 0xac#8 then some (.alu32 .xor dst srcX)
  else if opcode == 0xb4#8 then some (.alu32 .mov dst srcImm)
  else if opcode == 0xbc#8 then some (.alu32 .mov dst srcX)
  else if opcode == 0xc4#8 then
    if !imm32.msb && imm32 < 32#32 then some (.alu32 .arsh dst srcImm) else none
  else if opcode == 0xcc#8 then some (.alu32 .arsh dst srcX)
  -- Endianness
  else if opcode == 0xd4#8 then
    (decodeEndianWidth imm32).map (fun ew => .endian false ew dst)
  else if opcode == 0xdc#8 then
    (decodeEndianWidth imm32).map (fun ew => .endian true ew dst)
  -- 64-bit ALU
  else if opcode == 0x07#8 then some (.alu64 .add dst srcImm)
  else if opcode == 0x0f#8 then some (.alu64 .add dst srcX)
  else if opcode == 0x17#8 then some (.alu64 .sub dst srcImm)
  else if opcode == 0x1f#8 then some (.alu64 .sub dst srcX)
  else if opcode == 0x27#8 then some (.alu64 .mul dst srcImm)
  else if opcode == 0x2f#8 then some (.alu64 .mul dst srcX)
  else if opcode == 0x37#8 then
    if imm32 == 0#32 then none else some (.alu64 .div dst srcImm)
  else if opcode == 0x3f#8 then some (.alu64 .div dst srcX)
  else if opcode == 0x47#8 then some (.alu64 .or dst srcImm)
  else if opcode == 0x4f#8 then some (.alu64 .or dst srcX)
  else if opcode == 0x57#8 then some (.alu64 .and dst srcImm)
  else if opcode == 0x5f#8 then some (.alu64 .and dst srcX)
  else if opcode == 0x67#8 then
    if !imm32.msb && imm32 < 64#32 then some (.alu64 .lsh dst srcImm) else none
  else if opcode == 0x6f#8 then some (.alu64 .lsh dst srcX)
  else if opcode == 0x77#8 then
    if !imm32.msb && imm32 < 64#32 then some (.alu64 .rsh dst srcImm) else none
  else if opcode == 0x7f#8 then some (.alu64 .rsh dst srcX)
  else if opcode == 0x87#8 then some (.neg true dst)
  else if opcode == 0x97#8 then
    if imm32 == 0#32 then none else some (.alu64 .rem dst srcImm)
  else if opcode == 0x9f#8 then some (.alu64 .rem dst srcX)
  else if opcode == 0xa7#8 then some (.alu64 .xor dst srcImm)
  else if opcode == 0xaf#8 then some (.alu64 .xor dst srcX)
  else if opcode == 0xb7#8 then some (.alu64 .mov dst srcImm)
  else if opcode == 0xbf#8 then some (.alu64 .mov dst srcX)
  else if opcode == 0xc7#8 then
    if !imm32.msb && imm32 < 64#32 then some (.alu64 .arsh dst srcImm) else none
  else if opcode == 0xcf#8 then some (.alu64 .arsh dst srcX)
  -- 32-bit conditional jumps
  else if opcode == 0x16#8 then some (.condJump false .eq dst srcImm offInt)
  else if opcode == 0x1e#8 then some (.condJump false .eq dst srcX offInt)
  else if opcode == 0x26#8 then some (.condJump false .gt dst srcImm offInt)
  else if opcode == 0x2e#8 then some (.condJump false .gt dst srcX offInt)
  else if opcode == 0x36#8 then some (.condJump false .ge dst srcImm offInt)
  else if opcode == 0x3e#8 then some (.condJump false .ge dst srcX offInt)
  else if opcode == 0x46#8 then some (.condJump false .set dst srcImm offInt)
  else if opcode == 0x4e#8 then some (.condJump false .set dst srcX offInt)
  else if opcode == 0x56#8 then some (.condJump false .ne dst srcImm offInt)
  else if opcode == 0x5e#8 then some (.condJump false .ne dst srcX offInt)
  else if opcode == 0x66#8 then some (.condJump false .sgt dst srcImm offInt)
  else if opcode == 0x6e#8 then some (.condJump false .sgt dst srcX offInt)
  else if opcode == 0x76#8 then some (.condJump false .sge dst srcImm offInt)
  else if opcode == 0x7e#8 then some (.condJump false .sge dst srcX offInt)
  else if opcode == 0xa6#8 then some (.condJump false .lt dst srcImm offInt)
  else if opcode == 0xae#8 then some (.condJump false .lt dst srcX offInt)
  else if opcode == 0xb6#8 then some (.condJump false .le dst srcImm offInt)
  else if opcode == 0xbe#8 then some (.condJump false .le dst srcX offInt)
  else if opcode == 0xc6#8 then some (.condJump false .slt dst srcImm offInt)
  else if opcode == 0xce#8 then some (.condJump false .slt dst srcX offInt)
  else if opcode == 0xd6#8 then some (.condJump false .sle dst srcImm offInt)
  else if opcode == 0xde#8 then some (.condJump false .sle dst srcX offInt)
  -- Unconditional jump, 64-bit conditional jumps, external call, exit
  else if opcode == 0x05#8 then some (.jump offInt)
  else if opcode == 0x15#8 then some (.condJump true .eq dst srcImm offInt)
  else if opcode == 0x1d#8 then some (.condJump true .eq dst srcX offInt)
  else if opcode == 0x25#8 then some (.condJump true .gt dst srcImm offInt)
  else if opcode == 0x2d#8 then some (.condJump true .gt dst srcX offInt)
  else if opcode == 0x35#8 then some (.condJump true .ge dst srcImm offInt)
  else if opcode == 0x3d#8 then some (.condJump true .ge dst srcX offInt)
  else if opcode == 0x45#8 then some (.condJump true .set dst srcImm offInt)
  else if opcode == 0x4d#8 then some (.condJump true .set dst srcX offInt)
  else if opcode == 0x55#8 then some (.condJump true .ne dst srcImm offInt)
  else if opcode == 0x5d#8 then some (.condJump true .ne dst srcX offInt)
  else if opcode == 0x65#8 then some (.condJump true .sgt dst srcImm offInt)
  else if opcode == 0x6d#8 then some (.condJump true .sgt dst srcX offInt)
  else if opcode == 0x75#8 then some (.condJump true .sge dst srcImm offInt)
  else if opcode == 0x7d#8 then some (.condJump true .sge dst srcX offInt)
  else if opcode == 0xa5#8 then some (.condJump true .lt dst srcImm offInt)
  else if opcode == 0xad#8 then some (.condJump true .lt dst srcX offInt)
  else if opcode == 0xb5#8 then some (.condJump true .le dst srcImm offInt)
  else if opcode == 0xbd#8 then some (.condJump true .le dst srcX offInt)
  else if opcode == 0xc5#8 then some (.condJump true .slt dst srcImm offInt)
  else if opcode == 0xcd#8 then some (.condJump true .slt dst srcX offInt)
  else if opcode == 0xd5#8 then some (.condJump true .sle dst srcImm offInt)
  else if opcode == 0xdd#8 then some (.condJump true .sle dst srcX offInt)
  else if opcode == 0x85#8 then
    if srcReg == 0 then some (.callExternal imm32.toNat) else none
  else if opcode == 0x95#8 then some .exit
  -- Legacy packet loads
  else if opcode == 0x20#8 then some (.loadPacket .u32 imm32 none)
  else if opcode == 0x28#8 then some (.loadPacket .u16 imm32 none)
  else if opcode == 0x30#8 then some (.loadPacket .u8 imm32 none)
  else if opcode == 0x38#8 then some (.loadPacket .u64 imm32 none)
  else if opcode == 0x40#8 then some (.loadPacket .u32 imm32 (some srcReg))
  else if opcode == 0x48#8 then some (.loadPacket .u16 imm32 (some srcReg))
  else if opcode == 0x50#8 then some (.loadPacket .u8 imm32 (some srcReg))
  else if opcode == 0x58#8 then some (.loadPacket .u64 imm32 (some srcReg))
  -- Memory loads
  else if opcode == 0x61#8 then some (.load .u32 dst srcReg off16)
  else if opcode == 0x69#8 then some (.load .u16 dst srcReg off16)
  else if opcode == 0x71#8 then some (.load .u8 dst srcReg off16)
  else if opcode == 0x79#8 then some (.load .u64 dst srcReg off16)
  -- Memory immediate stores
  else if opcode == 0x62#8 then some (.store .u32 dst off16 srcImm)
  else if opcode == 0x6a#8 then some (.store .u16 dst off16 srcImm)
  else if opcode == 0x72#8 then some (.store .u8 dst off16 srcImm)
  else if opcode == 0x7a#8 then some (.store .u64 dst off16 srcImm)
  -- Memory register stores
  else if opcode == 0x63#8 then some (.store .u32 dst off16 srcX)
  else if opcode == 0x6b#8 then some (.store .u16 dst off16 srcX)
  else if opcode == 0x73#8 then some (.store .u8 dst off16 srcX)
  else if opcode == 0x7b#8 then some (.store .u64 dst off16 srcX)
  -- Atomic operations
  else if opcode == 0xc3#8 then
    (decodeAtomicOp (imm32.setWidth 8)).map (fun aop => .atomic false aop dst off16 srcReg)
  else if opcode == 0xdb#8 then
    (decodeAtomicOp (imm32.setWidth 8)).map (fun aop => .atomic true aop dst off16 srcReg)
  else none

/--
**Informal**: Decodes a raw list of `EbpfInstruction` slots into an equal-length
list of decoded `Insn` slots, translating each 2-slot `BPF_LDDW` (`0x18`) pair
into its primary `lddw*` instruction followed by `Insn.lddwCont`.
-/
def decodeVerifiedCodeList : List ebpf_core.api.EbpfInstruction → Option (List Insn)
  | [] => some []
  | raw1 :: rest =>
      let w1 : BitVec 64 := raw1.bv
      let opcode : BitVec 8 := w1.setWidth 8
      if opcode == 0x18#8 then
        match rest with
        | [] => none
        | raw2 :: tail =>
            let w2 : BitVec 64 := raw2.bv
            if w2.setWidth 32 != 0#32 then none
            else
              let dst : Nat := ((w1 >>> 8) &&& 0xF#64).toNat
              let srcReg : Nat := ((w1 >>> 12) &&& 0xF#64).toNat
              let imm32 : BitVec 32 := (w1 >>> 32).setWidth 32
              let nextImm32 : BitVec 32 := (w2 >>> 32).setWidth 32
              let head? : Option Insn :=
                if srcReg == 0 then
                  let val64 := imm32.zeroExtend 64 ||| ((nextImm32.zeroExtend 64) <<< 32)
                  some (.lddwImm dst val64)
                else if srcReg == 5 then
                  if !imm32.msb then some (.lddwMap dst imm32.toNat) else none
                else if srcReg == 6 then
                  if !imm32.msb && !nextImm32.msb then
                    some (.lddwMapValue dst imm32.toNat (nextImm32.zeroExtend 64))
                  else none
                else none
              match head?, decodeVerifiedCodeList tail with
              | some h, some decodedTail => some (h :: .lddwCont :: decodedTail)
              | _, _ => none
      else
        match decodeSingleInsn raw1, decodeVerifiedCodeList rest with
        | some h, some decodedTail => some (h :: decodedTail)
        | _, _ => none

/--
**Informal**: Decodes the verified `EbpfInstruction` list produced by
`verify_program_core` into a flat `Program` (`Array Insn`).
-/
def decodeVerifiedCode (code : List ebpf_core.api.EbpfInstruction) : Option Program :=
  (decodeVerifiedCodeList code).map List.toArray

/--
**Informal**: 64-bit scalar ALU evaluation following RFC 9669 eBPF semantics:
unsigned division by zero yields `0`, unsigned modulo by zero leaves the
dividend unchanged, and shifts mask the shift amount to the low 6 bits.
-/
def evalAlu64 (op : AluOp) (x y : BitVec 64) : BitVec 64 :=
  match op with
  | .add  => x + y
  | .sub  => x - y
  | .mul  => x * y
  | .div  => if y == 0#64 then 0#64 else x / y
  | .rem  => if y == 0#64 then x else x % y
  | .and  => x &&& y
  | .or   => x ||| y
  | .xor  => x ^^^ y
  | .lsh  => x <<< (y.setWidth 6)
  | .rsh  => x >>> (y.setWidth 6)
  | .arsh => BitVec.sshiftRight' x (y.setWidth 6)
  | .mov  => y

/--
**Informal**: 32-bit scalar ALU evaluation following RFC 9669 eBPF semantics:
truncates both inputs to their low 32 bits, performs the 32-bit operation (with
shifts masked to 5 bits), and zero-extends the 32-bit result to 64 bits.
-/
def evalAlu32 (op : AluOp) (x64 y64 : BitVec 64) : BitVec 64 :=
  let x : BitVec 32 := x64.setWidth 32
  let y : BitVec 32 := y64.setWidth 32
  let res32 : BitVec 32 :=
    match op with
    | .add  => x + y
    | .sub  => x - y
    | .mul  => x * y
    | .div  => if y == 0#32 then 0#32 else x / y
    | .rem  => if y == 0#32 then x else x % y
    | .and  => x &&& y
    | .or   => x ||| y
    | .xor  => x ^^^ y
    | .lsh  => x <<< (y.setWidth 5)
    | .rsh  => x >>> (y.setWidth 5)
    | .arsh => BitVec.sshiftRight' x (y.setWidth 5)
    | .mov  => y
  res32.zeroExtend 64

/--
**Informal**: Two's-complement negation in 64-bit mode (`is64 = true`) or 32-bit
zero-extended mode (`is64 = false`).
-/
def evalNeg (is64 : Bool) (x : BitVec 64) : BitVec 64 :=
  if is64 then -x else (- (x.setWidth 32)).zeroExtend 64

/--
**Informal**: Reverses the two bytes of a 16-bit word.
-/
def bswap16 (x : BitVec 16) : BitVec 16 :=
  ((x &&& 0x00FF#16) <<< 8) ||| ((x >>> 8) &&& 0x00FF#16)

/--
**Informal**: Reverses the four bytes of a 32-bit word.
-/
def bswap32 (x : BitVec 32) : BitVec 32 :=
  ((x &&& 0x0000_00FF#32) <<< 24) |||
  ((x &&& 0x0000_FF00#32) <<< 8)  |||
  ((x >>> 8)  &&& 0x0000_FF00#32) |||
  ((x >>> 24) &&& 0x0000_00FF#32)

/--
**Informal**: Reverses the eight bytes of a 64-bit word.
-/
def bswap64 (x : BitVec 64) : BitVec 64 :=
  let lo := (bswap32 (x.setWidth 32)).zeroExtend 64
  let hi := (bswap32 ((x >>> 32).setWidth 32)).zeroExtend 64
  (lo <<< 32) ||| hi

/--
**Informal**: Evaluates an eBPF endianness conversion (`be16/32/64`,
`le16/32/64`), optionally byte-swapping and zero-extending to 64 bits.
-/
def evalEndian (doSwap : Bool) (w : EndianWidth) (x : BitVec 64) : BitVec 64 :=
  match doSwap, w with
  | false, .w16 => (x.setWidth 16).zeroExtend 64
  | false, .w32 => (x.setWidth 32).zeroExtend 64
  | false, .w64 => x
  | true,  .w16 => (bswap16 (x.setWidth 16)).zeroExtend 64
  | true,  .w32 => (bswap32 (x.setWidth 32)).zeroExtend 64
  | true,  .w64 => bswap64 x

/--
**Informal**: Evaluates a 64-bit conditional jump comparison between two 64-bit
words.
-/
def evalJump64 (op : JumpOp) (x y : BitVec 64) : Bool :=
  match op with
  | .eq  => x == y
  | .ne  => x != y
  | .gt  => x > y
  | .ge  => x ≥ y
  | .lt  => x < y
  | .le  => x ≤ y
  | .sgt => BitVec.slt y x
  | .sge => BitVec.sle y x
  | .slt => BitVec.slt x y
  | .sle => BitVec.sle x y
  | .set => (x &&& y) != 0#64

/--
**Informal**: Evaluates a 32-bit conditional jump comparison on the low 32 bits
of two 64-bit words.
-/
def evalJump32 (op : JumpOp) (x64 y64 : BitVec 64) : Bool :=
  let x := x64.setWidth 32
  let y := y64.setWidth 32
  match op with
  | .eq  => x == y
  | .ne  => x != y
  | .gt  => x > y
  | .ge  => x ≥ y
  | .lt  => x < y
  | .le  => x ≤ y
  | .sgt => BitVec.slt y x
  | .sge => BitVec.sle y x
  | .slt => BitVec.slt x y
  | .sle => BitVec.sle x y
  | .set => (x &&& y) != 0#32

/--
**Informal**: Extracts a narrow sub-word of width `w` starting at byte offset
`subIdx` (`0..7`) within a little-endian 64-bit stack slot word.
-/
def extractSubValue (v : BitVec 64) (subIdx : Nat) (w : DataWidth) : BitVec 64 :=
  let sh := BitVec.ofNat 6 (subIdx * 8)
  (v >>> sh) &&& w.valMask64

/--
**Informal**: Replaces the sub-word of width `w` at byte offset `subIdx` (`0..7`)
within a little-endian 64-bit stack slot word `orig` with `val`.
-/
def insertSubValue (orig val : BitVec 64) (subIdx : Nat) (w : DataWidth) : BitVec 64 :=
  let sh := BitVec.ofNat 6 (subIdx * 8)
  let m := w.valMask64 <<< sh
  (orig &&& ~~~m) ||| ((val &&& w.valMask64) <<< sh)

/--
**Informal**: Reads a little-endian word of width `w` from byte-addressed
memory buffer `id` at byte offset `off`.
-/
def readBufWord (buf : MemoryId → BitVec 64 → BitVec 8) (id : MemoryId)
    (off : BitVec 64) (w : DataWidth) : BitVec 64 :=
  let b (k : BitVec 64) : BitVec 64 := (buf id (off + k)).zeroExtend 64
  match w with
  | .u8  => b 0#64
  | .u16 => b 0#64 ||| (b 1#64 <<< 8)
  | .u32 => b 0#64 ||| (b 1#64 <<< 8) ||| (b 2#64 <<< 16) ||| (b 3#64 <<< 24)
  | .u64 =>
      b 0#64 ||| (b 1#64 <<< 8) ||| (b 2#64 <<< 16) ||| (b 3#64 <<< 24) |||
      (b 4#64 <<< 32) ||| (b 5#64 <<< 40) ||| (b 6#64 <<< 48) ||| (b 7#64 <<< 56)

/--
**Informal**: Writes a single byte `v` into byte-addressed memory buffer `id`
at byte offset `off`.
-/
def writeBufByte (buf : MemoryId → BitVec 64 → BitVec 8) (id : MemoryId)
    (off : BitVec 64) (v : BitVec 8) : MemoryId → BitVec 64 → BitVec 8 :=
  fun id' off' => if id' == id && off' == off then v else buf id' off'

/--
**Informal**: Writes a little-endian word `val` of width `w` into byte-addressed
memory buffer `id` at byte offset `off`.
-/
def writeBufWord (buf : MemoryId → BitVec 64 → BitVec 8) (id : MemoryId)
    (off : BitVec 64) (w : DataWidth) (val : BitVec 64) : MemoryId → BitVec 64 → BitVec 8 :=
  let byteAt (sh : Nat) : BitVec 8 := (val >>> sh).setWidth 8
  match w with
  | .u8  => writeBufByte buf id off (byteAt 0)
  | .u16 =>
      let buf := writeBufByte buf id off (byteAt 0)
      writeBufByte buf id (off + 1#64) (byteAt 8)
  | .u32 =>
      let buf := writeBufByte buf id off (byteAt 0)
      let buf := writeBufByte buf id (off + 1#64) (byteAt 8)
      let buf := writeBufByte buf id (off + 2#64) (byteAt 16)
      writeBufByte buf id (off + 3#64) (byteAt 24)
  | .u64 =>
      let buf := writeBufByte buf id off (byteAt 0)
      let buf := writeBufByte buf id (off + 1#64) (byteAt 8)
      let buf := writeBufByte buf id (off + 2#64) (byteAt 16)
      let buf := writeBufByte buf id (off + 3#64) (byteAt 24)
      let buf := writeBufByte buf id (off + 4#64) (byteAt 32)
      let buf := writeBufByte buf id (off + 5#64) (byteAt 40)
      let buf := writeBufByte buf id (off + 6#64) (byteAt 48)
      writeBufByte buf id (off + 7#64) (byteAt 56)

/--
**Informal**: Writes `sz` bytes produced by `writeByte` into memory buffer `mId`
starting at byte offset `off`.
-/
def writeBufSpanHelper (buf : MemoryId → BitVec 64 → BitVec 8) (mId : MemoryId)
    (off sz : BitVec 64) (writeByte : BitVec 64 → BitVec 8) :
    MemoryId → BitVec 64 → BitVec 8 :=
  fun id' addr =>
    if id' == mId && off ≤ addr && (addr - off) < sz then
      writeByte (addr - off)
    else
      buf id' addr

/--
**Informal**: Sign-extends a 16-bit signed instruction offset to a 64-bit
displacement.
-/
def sextOffset16 (off : BitVec 16) : BitVec 64 :=
  off.signExtend 64

/--
**Informal**: Computes the target instruction index `pc + stepSlots + offset`
of a relative jump, returning `none` if the target falls outside `[0, progLen)`.
-/
def computeTargetPc (progLen pc stepSlots : Nat) (offset : Int) : Option Nat :=
  let target : Int := (Int.ofNat (pc + stepSlots)) + offset
  if target < 0 then none
  else
    let t := target.toNat
    if t < progLen then some t else none

/--
**Informal**: Checks that an access of width `w` at byte offset `off` lies
within `[0, limit]` without unsigned 64-bit addition overflow or `u64::MAX`
offset wraparound.
-/
def inBoundsRange (off : BitVec 64) (w : DataWidth) (limit : BitVec 64) : Bool :=
  let size := w.bvBytes
  (off != ~~~0#64) && (!BitVec.uaddOverflow off size) && ((off + size) ≤ limit)

/--
**Informal**: Checks that a contiguous byte span `[off, off + size]` lies within
`[0, limit]` without unsigned 64-bit addition overflow or `u64::MAX` wraparound.
-/
def inBoundsSpan (off size limit : BitVec 64) : Bool :=
  (off != ~~~0#64) && (size != ~~~0#64) &&
  (!BitVec.uaddOverflow off size) && ((off + size) ≤ limit)

/--
**Informal**: Checks that byte offset `off` is naturally aligned to access
width `w`.
-/
def isAligned (off : BitVec 64) (w : DataWidth) : Bool :=
  (off % w.bvBytes) == 0#64

/--
**Informal**: Verifies that if runtime value `v` carries a releasable resource
identifier `rid`, `rid` is still present in the `live` resource list; otherwise
returns `SafetyViolation.useAfterRelease rid`.
-/
def checkAlive (live : List MemoryId) (v : RuntimeVal) : Option SafetyViolation :=
  match v.resourceId? with
  | none => none
  | some rid => if live.contains rid then none else some (.useAfterRelease rid)

/--
**Informal**: Evaluates a 64-bit ALU operation on `RuntimeVal` operands,
preserving pointer provenance for pointer-plus-scalar and pointer-minus-scalar
arithmetic and yielding a pure scalar difference when subtracting two pointers
into the same memory object.
-/
def evalAlu64Val (layout : HostLayout) (arrayLen : MemoryId → BitVec 64)
    (op : AluOp) (v1 v2 : RuntimeVal) : RuntimeVal :=
  match op, v1, v2 with
  | .mov, _, _ => v2
  | .add, .stackPtr off, .scalar d | .add, .scalar d, .stackPtr off => .stackPtr (off + d)
  | .sub, .stackPtr off, .scalar d => .stackPtr (off - d)
  | .add, .memPtr id orig res sz off, .scalar d | .add, .scalar d, .memPtr id orig res sz off =>
      .memPtr id orig res sz (off + d)
  | .sub, .memPtr id orig res sz off, .scalar d =>
      .memPtr id orig res sz (off - d)
  | .add, .structPtr id res off, .scalar d | .add, .scalar d, .structPtr id res off =>
      .structPtr id res (off + d)
  | .sub, .structPtr id res off, .scalar d =>
      .structPtr id res (off - d)
  | .add, .arrayPtr id off, .scalar d | .add, .scalar d, .arrayPtr id off =>
      if !BitVec.uaddOverflow off d then .arrayPtr id (off + d)
      else .scalar (evalAlu64 .add (v1.toBits layout arrayLen) (v2.toBits layout arrayLen))
  | .sub, .arrayPtr id off, .scalar d =>
      if !BitVec.usubOverflow off d then .arrayPtr id (off - d)
      else .scalar (evalAlu64 .sub (v1.toBits layout arrayLen) (v2.toBits layout arrayLen))
  | .sub, .stackPtr o1, .stackPtr o2 => .scalar (o1 - o2)
  | .sub, .memPtr id1 _ _ _ o1, .memPtr id2 _ _ _ o2 =>
      if id1 == id2 then .scalar (o1 - o2)
      else .scalar (evalAlu64 .sub (v1.toBits layout arrayLen) (v2.toBits layout arrayLen))
  | .sub, .structPtr id1 _ o1, .structPtr id2 _ o2 =>
      if id1 == id2 then .scalar (o1 - o2)
      else .scalar (evalAlu64 .sub (v1.toBits layout arrayLen) (v2.toBits layout arrayLen))
  | .sub, .arrayPtr id1 o1, .arrayPtr id2 o2 =>
      if id1 == id2 then .scalar (o1 - o2)
      else .scalar (evalAlu64 .sub (v1.toBits layout arrayLen) (v2.toBits layout arrayLen))
  | .sub, .arrayPtr id1 o1, .endArrayPtr id2 =>
      if id1 == id2 then .scalar (o1 - arrayLen id2)
      else .scalar (evalAlu64 .sub (v1.toBits layout arrayLen) (v2.toBits layout arrayLen))
  | .sub, .endArrayPtr id1, .arrayPtr id2 o2 =>
      if id1 == id2 then .scalar (arrayLen id1 - o2)
      else .scalar (evalAlu64 .sub (v1.toBits layout arrayLen) (v2.toBits layout arrayLen))
  | _, _, _ =>
      .scalar (evalAlu64 op (v1.toBits layout arrayLen) (v2.toBits layout arrayLen))

/--
**Informal**: Outcome of executing a single instruction in the reference eBPF
state machine: either advances to `next st'`, halts cleanly via `exit` (or
out-of-bounds packet load fast-exit) with return value `retVal` and final world
`finalWorld`, or gets stuck on an explicit `SafetyViolation`.
-/
inductive StepOutcome where
  | next (st' : MachineState)
  | halted (retVal : BitVec 64) (finalWorld : WorldState)
  | stuck (err : SafetyViolation)

/--
**Informal**: Functional update of register file `regs` at register index `dst`
with new value `v`.
-/
def updateReg (regs : Nat → RuntimeVal) (dst : Nat) (v : RuntimeVal) : Nat → RuntimeVal :=
  fun r => if r == dst then v else regs r

/--
**Informal**: Functional update of stack memory `stk` at 8-byte slot index
`slot` with new slot contents `s`.
-/
def updateStack (stk : StackMem) (slot : Nat) (s : StackSlot) : StackMem :=
  fun i => if i == slot then s else stk i

/--
**Informal**: Advances the world step counter by one instruction.
-/
def bumpWorldStep (w : WorldState) : WorldState :=
  { w with stepCount := w.stepCount + 1 }

/--
**Informal**: Checks that reading `bytes` on the 512-byte stack starting at
`startOff` (`Stack::read_data_ptr`) accesses only in-bounds initialized scalar
bytes (zero-byte reads succeed immediately without dereferencing).
-/
def checkStackRangeReadable (stk : StackMem) (startOff bytes : BitVec 64) : Bool :=
  if bytes == 0#64 then
    true
  else if bytes > 512#64 || startOff ≥ 512#64 then
    false
  else if BitVec.uaddOverflow startOff bytes || (startOff + bytes) > 512#64 then
    false
  else
    let startNat := startOff.toNat
    let lenNat := bytes.toNat
    (List.range lenNat).all fun k =>
      let pos := startNat + k
      let slotIdx := pos / 8
      let subIdx := pos % 8
      match stk slotIdx with
      | .spilled _ => false
      | .bytes _ initMask => extractSubValue initMask subIdx .u8 == 0xFF#64

/--
**Informal**: Writes `bytes` of helper output into the stack starting at a valid
stack element offset `startOff < 512` (`Stack::write_data_ptr`), marking all
written byte positions as initialized scalars.
-/
def writeStackRangeHelper (layout : HostLayout) (arrayLen : MemoryId → BitVec 64)
    (stk : StackMem) (startOff bytes : BitVec 64) (writeByte : BitVec 64 → BitVec 8) :
    Option StackMem :=
  if bytes > 512#64 || startOff ≥ 512#64 then none
  else if BitVec.uaddOverflow startOff bytes || (startOff + bytes) > 512#64 then none
  else
    let startNat := startOff.toNat
    let lenNat := bytes.toNat
    let stk' := (List.range lenNat).foldl (fun curStk k =>
      let pos := startNat + k
      let slotIdx := pos / 8
      let subIdx := pos % 8
      let bVal := (writeByte (BitVec.ofNat 64 k)).zeroExtend 64
      let newSlot := match curStk slotIdx with
        | .bytes oldV oldInit =>
            StackSlot.bytes
              (insertSubValue oldV bVal subIdx .u8)
              (insertSubValue oldInit 0xFF#64 subIdx .u8)
        | .spilled cap =>
            let raw := cap.toBits layout arrayLen
            StackSlot.bytes
              (insertSubValue raw bVal subIdx .u8)
              (insertSubValue 0#64 0xFF#64 subIdx .u8)
      updateStack curStk slotIdx newSlot) stk
    some stk'

/--
**Informal**: Resolves a `HelperMemSize` specification (`fixed` or `fromArg`)
against the current helper argument registers `r1`..`r5`.
-/
def resolveHelperMemSize (regs : Nat → RuntimeVal) : HelperMemSize → Option (BitVec 64)
  | .fixed bytes => some bytes
  | .fromArg argIdx =>
      if argIdx < 5 then
        match regs (argIdx + 1) with
        | .scalar sz => some sz
        | _ => none
      else none

/--
**Informal**: Looks up the `MapSpec` pointed to by 0-based helper argument
`mapArgIdx` (`r_{mapArgIdx+1}`).
-/
def lookupHelperMapArg (env : ExecEnv) (regs : Nat → RuntimeVal) (mapArgIdx : Nat) :
    Option MapSpec :=
  if mapArgIdx < 5 then
    match regs (mapArgIdx + 1) with
    | .mapPtr mIdx => env.maps mIdx
    | _ => none
  else none

/--
**Informal**: Validates a single helper argument value `v` (at 0-based argument
index `argIdx`) against `HelperParamSpec`, checking resource liveness, map/key/
value/memory/struct/context/release contracts, and applying any stack write,
buffer write, or resource release side effects.
-/
def matchOneHelperParam (env : ExecEnv) (layout : HostLayout) (regs : Nat → RuntimeVal)
    (preCallStack : StackMem) (helperIdx argIdx : Nat) :
    HelperParamSpec → RuntimeVal → StackMem → WorldState →
    Except SafetyViolation (StackMem × WorldState)
  | .nullOr inner, v, stkAcc, wrldAcc =>
      if v == .scalar 0#64 then .ok (stkAcc, wrldAcc)
      else matchOneHelperParam env layout regs preCallStack helperIdx argIdx inner v stkAcc wrldAcc
  | .scalar, v, stkAcc, wrldAcc =>
      match checkAlive wrldAcc.liveResources v with
      | some e => .error e
      | none =>
          match v with
          | .scalar _ => .ok (stkAcc, wrldAcc)
          | _ => .error (.helperContractViolation helperIdx)
  | .constPtrToMap filter mustBeWritable, v, stkAcc, wrldAcc =>
      match checkAlive wrldAcc.liveResources v with
      | some e => .error e
      | none =>
          match v with
          | .mapPtr mIdx =>
              match env.maps mIdx with
              | none => .error (.helperContractViolation helperIdx)
              | some ms =>
                  if (mustBeWritable && ms.progReadOnly) || !filter.allows ms.mapType then
                    .error (.helperContractViolation helperIdx)
                  else
                    .ok (stkAcc, wrldAcc)
          | _ => .error (.helperContractViolation helperIdx)
  | .mapKey mapArgIdx, v, stkAcc, wrldAcc =>
      match checkAlive wrldAcc.liveResources v, lookupHelperMapArg env regs mapArgIdx with
      | some e, _ => .error e
      | none, none => .error (.helperContractViolation helperIdx)
      | none, some ms =>
          match v with
          | .stackPtr off =>
              if checkStackRangeReadable preCallStack off ms.keySize then .ok (stkAcc, wrldAcc)
              else .error (.helperContractViolation helperIdx)
          | .memPtr _ _ _ bufSz off =>
              if inBoundsSpan off ms.keySize bufSz then .ok (stkAcc, wrldAcc)
              else .error (.helperContractViolation helperIdx)
          | _ => .error (.helperContractViolation helperIdx)
  | .mapValue mapArgIdx, v, stkAcc, wrldAcc =>
      match checkAlive wrldAcc.liveResources v, lookupHelperMapArg env regs mapArgIdx with
      | some e, _ => .error e
      | none, none => .error (.helperContractViolation helperIdx)
      | none, some ms =>
          match v with
          | .stackPtr off =>
              if checkStackRangeReadable preCallStack off ms.valueSize then .ok (stkAcc, wrldAcc)
              else .error (.helperContractViolation helperIdx)
          | .memPtr _ _ _ bufSz off =>
              if inBoundsSpan off ms.valueSize bufSz then .ok (stkAcc, wrldAcc)
              else .error (.helperContractViolation helperIdx)
          | _ => .error (.helperContractViolation helperIdx)
  | .memory sizeSpec isInput isOutput, v, stkAcc, wrldAcc =>
      match checkAlive wrldAcc.liveResources v, resolveHelperMemSize regs sizeSpec with
      | some e, _ => .error e
      | none, none => .error (.helperContractViolation helperIdx)
      | none, some sz =>
          match v with
          | .stackPtr off =>
              if sz > 512#64 || off ≥ 512#64 || BitVec.uaddOverflow off sz || (off + sz) > 512#64 then
                .error (.helperContractViolation helperIdx)
              else if isInput && !checkStackRangeReadable preCallStack off sz then
                .error (.helperContractViolation helperIdx)
              else if isOutput then
                match writeStackRangeHelper layout wrldAcc.arrayLen stkAcc off sz
                  (env.helperBufWriteOracle wrldAcc.stepCount argIdx) with
                | none => .error (.helperContractViolation helperIdx)
                | some stk' => .ok (stk', wrldAcc)
              else
                .ok (stkAcc, wrldAcc)
          | .memPtr mId orig _ bufSz off =>
              if (isOutput && !orig.isWritable) || !inBoundsSpan off sz bufSz then
                .error (.helperContractViolation helperIdx)
              else if isOutput then
                let buf' := writeBufSpanHelper wrldAcc.bufBytes mId off sz
                  (env.helperBufWriteOracle wrldAcc.stepCount argIdx)
                .ok (stkAcc, { wrldAcc with bufBytes := buf' })
              else
                .ok (stkAcc, wrldAcc)
          | _ => .error (.helperContractViolation helperIdx)
  | .structArg schemaId, v, stkAcc, wrldAcc =>
      match checkAlive wrldAcc.liveResources v with
      | some e => .error e
      | none =>
          match v with
          | .structPtr sId _ 0#64 =>
              if memoryIdMatches schemaId sId then .ok (stkAcc, wrldAcc)
              else .error (.helperContractViolation helperIdx)
          | .memPtr mId _ _ _ 0#64 =>
              if memoryIdMatches schemaId mId then .ok (stkAcc, wrldAcc)
              else .error (.helperContractViolation helperIdx)
          | _ => .error (.helperContractViolation helperIdx)
  | .contextArg paramIdx, v, stkAcc, wrldAcc =>
      match checkAlive wrldAcc.liveResources v with
      | some e => .error e
      | none =>
          match env.entryContextArgs paramIdx with
          | some ctxSpec =>
              if ctxSpec.matchesVal v then .ok (stkAcc, wrldAcc)
              else .error (.helperContractViolation helperIdx)
          | none => .error (.helperContractViolation helperIdx)
  | .releasableArg schemaId inner, v, stkAcc, wrldAcc =>
      match v.resourceId? with
      | none => .error (.helperContractViolation helperIdx)
      | some rid =>
          if !memoryIdHasParent rid schemaId then
            .error (.helperContractViolation helperIdx)
          else if !wrldAcc.liveResources.contains rid then
            .error (.useAfterRelease rid)
          else
            matchOneHelperParam env layout regs preCallStack helperIdx argIdx inner
              v.stripResource stkAcc wrldAcc
  | .releaseArg schemaId, v, stkAcc, wrldAcc =>
      let baseAndRid? : Option MemoryId :=
        match v with
        | .memPtr _ _ (some rid) _ 0#64 => some rid
        | .structPtr _ (some rid) 0#64  => some rid
        | _                             => none
      match baseAndRid? with
      | none =>
          match checkAlive wrldAcc.liveResources v with
          | some e => .error e
          | none => .error (.helperContractViolation helperIdx)
      | some rid =>
          if !memoryIdHasParent rid schemaId then
            if !wrldAcc.liveResources.contains rid then .error (.useAfterRelease rid)
            else .error (.helperContractViolation helperIdx)
          else if !wrldAcc.liveResources.contains rid then
            .error (.doubleRelease rid)
          else
            .ok (stkAcc, { wrldAcc with liveResources := wrldAcc.liveResources.erase rid })
  | .anyArg, v, stkAcc, wrldAcc =>
      match checkAlive wrldAcc.liveResources v with
      | some e => .error e
      | none =>
          if env.allowPtrLeaks || v.isScalar then .ok (stkAcc, wrldAcc)
          else .error (.helperContractViolation helperIdx)

/--
**Informal**: Sequentially validates all arguments `r1`..`r_{args.length}` of an
external helper call and applies their stack, memory, and resource-release side
effects.
-/
def checkAndApplyHelperArgs (env : ExecEnv) (layout : HostLayout) (regs : Nat → RuntimeVal)
    (preCallStack : StackMem) (helperIdx : Nat) :
    Nat → List HelperParamSpec → StackMem → WorldState →
    Except SafetyViolation (StackMem × WorldState)
  | _, [], stkAcc, wrldAcc => .ok (stkAcc, wrldAcc)
  | idx, param :: rest, stkAcc, wrldAcc =>
      if idx ≥ 5 then .error (.invalidRegister (idx + 1))
      else
        match matchOneHelperParam env layout regs preCallStack helperIdx idx param
          (regs (idx + 1)) stkAcc wrldAcc with
        | .error e => .error e
        | .ok (stk', wrld') =>
            checkAndApplyHelperArgs env layout regs preCallStack helperIdx (idx + 1) rest stk' wrld'

/--
**Informal**: Resolves the return value (`r0`) and updated `WorldState` of an
external helper call according to `HelperReturnSpec` (including fresh memory and
`Releasable` resource ID allocation, matching `resolve_return_value` in
`ebpf_core::verifier`).
-/
def evalHelperReturn (env : ExecEnv) (layout : HostLayout) (secret : SecretInit)
    (preRegs : Nat → RuntimeVal) (preWorld : WorldState) (helperIdx : Nat)
    (retSpec : HelperReturnSpec) (wrldBounds : WorldState) :
    Option (RuntimeVal × WorldState) :=
  match retSpec with
  | .scalar true =>
      some (.scalar (env.helperScalarOracle wrldBounds.stepCount helperIdx), wrldBounds)
  | .scalar false =>
      let r0Bits :=
        if secret.preserveCallScratch then
          (preRegs 0).toBits layout preWorld.arrayLen
        else
          secret.regBits preWorld.stepCount 0
      some (.scalar r0Bits, wrldBounds)
  | .pointer nullable relSchema? inner =>
      let resolvedInner? : Option (MemoryId × MemoryOrigin × Option (BitVec 64)) :=
        let baseAlloc :=
          freshRuntimeId (wrldBounds.nextAllocId +
            (if nullable then 1 else 0) +
            (if relSchema?.isSome then 1 else 0))
        match inner with
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
      match resolvedInner? with
      | none => none
      | some (ptrId, orig, bufSz?) =>
          let totalAllocs : Nat :=
            (if nullable then 1 else 0) + (if relSchema?.isSome then 1 else 0) + 1
          let isNull := nullable && env.helperNullOracle wrldBounds.stepCount helperIdx
          if isNull then
            some (.scalar 0#64, { wrldBounds with nextAllocId := wrldBounds.nextAllocId + totalAllocs })
          else
            let (resId?, wrld') : Option MemoryId × WorldState :=
              match relSchema? with
              | none =>
                  (none, { wrldBounds with nextAllocId := wrldBounds.nextAllocId + totalAllocs })
              | some relSchema =>
                  let relAllocIdx := wrldBounds.nextAllocId + (if nullable then 1 else 0)
                  let rid := memoryIdPrepend (freshRuntimeId relAllocIdx) relSchema
                  (some rid,
                   { wrldBounds with
                       liveResources := rid :: wrldBounds.liveResources
                       nextAllocId := wrldBounds.nextAllocId + totalAllocs })
            let ptrVal : RuntimeVal :=
              match bufSz? with
              | some sz => .memPtr ptrId orig resId? sz 0#64
              | none    => .structPtr ptrId resId? 0#64
            some (ptrVal, wrld')

/--
**Informal**: Locates the first descriptor in `fields` covering byte offset
`effOff` within `usize::MAX`, matching `StructDescriptor::find_field` in
`ebpf_core::verifier` (skipping any field whose end offset overflows `usize`).
-/
def findStructField (fields : List StructFieldDesc) (effOff : BitVec 64) :
    Option StructFieldDesc :=
  if decide (effOff.toNat ≤ Aeneas.Std.Usize.max) then
    fields.find? fun f =>
      decide (f.offset.toNat + f.kind.byteSize.toNat ≤ Aeneas.Std.Usize.max) &&
      (f.offset ≤ effOff) &&
      (effOff < f.offset + f.kind.byteSize)
  else
    none

/--
**Informal**: Reference small-step operational semantics (`step`) of a single
decoded eBPF instruction at `st.pc`. Checks all register, alignment, bounds,
initialization, helper-contract, and resource-lifetime invariants before
performing the transition, returning `StepOutcome.stuck err` if any safety
invariant is violated.
-/
def step (prog : Program) (env : ExecEnv) (layout : HostLayout) (secret : SecretInit)
    (st : MachineState) : StepOutcome :=
  if hpc : st.pc < prog.size then
    let insn := prog[st.pc]
    let readOp (op : Operand) : Except SafetyViolation RuntimeVal :=
      match op with
      | .imm v => .ok (.scalar v)
      | .reg r =>
          if r < REGISTER_COUNT then .ok (st.regs r)
          else .error (.invalidRegister r)
    let checkDst (dst : Nat) : Except SafetyViolation Unit :=
      if dst < GENERAL_REGISTER_COUNT then .ok ()
      else if dst == GENERAL_REGISTER_COUNT then .error .writeToReadOnlyFramePtr
      else .error (.invalidRegister dst)
    let stepNextPc (nextPc : Nat) (regs' : Nat → RuntimeVal) (stk' : StackMem) (w' : WorldState) :
        StepOutcome :=
      if hnext : nextPc < prog.size then
        if prog[nextPc] == .lddwCont then .stuck (.invalidMidLddw nextPc)
        else .next { pc := nextPc, regs := regs', stack := stk', world := bumpWorldStep w' }
      else
        .stuck (.invalidPc nextPc)
    match insn with
    | .lddwCont => .stuck (.invalidMidLddw st.pc)

    | .lddwImm dst imm =>
        match checkDst dst with
        | .error e => .stuck e
        | .ok () =>
            if h2 : st.pc + 1 < prog.size then
              if prog[st.pc + 1] == .lddwCont then
                stepNextPc (st.pc + 2) (updateReg st.regs dst (.scalar imm)) st.stack st.world
              else .stuck (.invalidMidLddw (st.pc + 1))
            else .stuck (.invalidPc (st.pc + 1))

    | .lddwMap dst schemaIdx =>
        match checkDst dst, env.maps schemaIdx with
        | .error e, _ => .stuck e
        | .ok (), none => .stuck .invalidDereference
        | .ok (), some _ =>
            if h2 : st.pc + 1 < prog.size then
              if prog[st.pc + 1] == .lddwCont then
                stepNextPc (st.pc + 2) (updateReg st.regs dst (.mapPtr schemaIdx)) st.stack st.world
              else .stuck (.invalidMidLddw (st.pc + 1))
            else .stuck (.invalidPc (st.pc + 1))

    | .lddwMapValue dst schemaIdx off =>
        match checkDst dst, env.maps schemaIdx with
        | .error e, _ => .stuck e
        | .ok (), none => .stuck .invalidDereference
        | .ok (), some ms =>
            if ms.mapType != 2#32 || ms.maxEntries == 0#32 || off ≥ ms.valueSize then
              .stuck .invalidDereference
            else if h2 : st.pc + 1 < prog.size then
              if prog[st.pc + 1] == .lddwCont then
                let rid := freshRuntimeId st.world.nextAllocId
                let orig := if ms.progReadOnly then MemoryOrigin.mapReadOnly else .mapWritable
                let wrld' := { st.world with nextAllocId := st.world.nextAllocId + 1 }
                let ptr := RuntimeVal.memPtr rid orig none ms.valueSize off
                stepNextPc (st.pc + 2) (updateReg st.regs dst ptr) st.stack wrld'
              else .stuck (.invalidMidLddw (st.pc + 1))
            else .stuck (.invalidPc (st.pc + 1))

    -- Note: `checkAlive st.world.liveResources` is intentionally omitted on the
    -- operands of `.alu64`, `.alu32`, and `.neg` (unlike `.endian` and
    -- `.condJump` below). `DependencyTracker` in `verifier.rs` only records
    -- `src`/`dst` register dependencies for ALU instructions when `dst` is
    -- subsequently live, so a dead ALU instruction may operate on a released
    -- pointer without the verifier tracking its resource ID. Accordingly,
    -- `useAfterRelease` is enforced only when pointers are dereferenced,
    -- leaked via `.endian`, compared in `.condJump`, passed to helpers, or
    -- returned at `.exit`.
    | .alu64 op dst src =>
        match checkDst dst, readOp src with
        | .error e, _ => .stuck e
        | _, .error e => .stuck e
        | .ok (), .ok vSrc =>
            let vDst := st.regs dst
            if op == .mov then
              stepNextPc (st.pc + 1) (updateReg st.regs dst vSrc) st.stack st.world
            else
              let res := evalAlu64Val layout st.world.arrayLen op vDst vSrc
              stepNextPc (st.pc + 1) (updateReg st.regs dst res) st.stack st.world

    | .alu32 op dst src =>
        match checkDst dst, readOp src with
        | .error e, _ => .stuck e
        | _, .error e => .stuck e
        | .ok (), .ok vSrc =>
            let vDst := st.regs dst
            if op == .mov then
              let res := RuntimeVal.scalar
                (evalAlu32 .mov 0#64 (vSrc.toBits layout st.world.arrayLen))
              stepNextPc (st.pc + 1) (updateReg st.regs dst res) st.stack st.world
            else
              let b1 := vDst.toBits layout st.world.arrayLen
              let b2 := vSrc.toBits layout st.world.arrayLen
              let res := RuntimeVal.scalar (evalAlu32 op b1 b2)
              stepNextPc (st.pc + 1) (updateReg st.regs dst res) st.stack st.world

    | .neg is64 dst =>
        match checkDst dst with
        | .error e => .stuck e
        | .ok () =>
            let vDst := st.regs dst
            let res := RuntimeVal.scalar (evalNeg is64 (vDst.toBits layout st.world.arrayLen))
            stepNextPc (st.pc + 1) (updateReg st.regs dst res) st.stack st.world

    | .endian doSwap w dst =>
        match checkDst dst with
        | .error e => .stuck e
        | .ok () =>
            let vDst := st.regs dst
            match checkAlive st.world.liveResources vDst with
            | some e => .stuck e
            | none =>
                if !vDst.isScalar && !env.allowPtrLeaks then
                  .stuck .pointerLeakToMemoryOrReturn
                else
                  let res := RuntimeVal.scalar
                    (evalEndian doSwap w (vDst.toBits layout st.world.arrayLen))
                  stepNextPc (st.pc + 1) (updateReg st.regs dst res) st.stack st.world

    | .jump offset =>
        match computeTargetPc prog.size st.pc 1 offset with
        | none => .stuck (.invalidPc (st.pc + 1))
        | some target => stepNextPc target st.regs st.stack st.world

    | .condJump is64 op dst src offset =>
        if dst ≥ REGISTER_COUNT then .stuck (.invalidRegister dst)
        else
          match readOp src with
          | .error e => .stuck e
          | .ok vSrc =>
              let vDst := st.regs dst
              match checkAlive st.world.liveResources vDst,
                    checkAlive st.world.liveResources vSrc with
              | some e, _ => .stuck e
              | _, some e => .stuck e
              | none, none =>
                  let b1 := vDst.toBits layout st.world.arrayLen
                  let b2 := vSrc.toBits layout st.world.arrayLen
                  let cond := if is64 then evalJump64 op b1 b2 else evalJump32 op b1 b2
                  let chosenOff : Int := if cond then offset else 0
                  match computeTargetPc prog.size st.pc 1 chosenOff with
                  | none => .stuck (.invalidPc (st.pc + 1))
                  | some target => stepNextPc target st.regs st.stack st.world

    | .load w dst base off16 =>
        match checkDst dst with
        | .error e => .stuck e
        | .ok () =>
            if base ≥ REGISTER_COUNT then .stuck (.invalidRegister base)
            else
              let vBase := st.regs base
              match checkAlive st.world.liveResources vBase with
              | some e => .stuck e
              | none =>
                  let disp := sextOffset16 off16
                  match vBase with
                  | .stackPtr baseOff =>
                      let effOff := baseOff + disp
                      if effOff ≥ 512#64 then .stuck (.outOfBoundsStackAccess effOff w)
                      else if (effOff.toNat % 8) % w.bytes != 0 then
                        .stuck (.misalignedStackAccess effOff w)
                      else
                        let slotIdx := effOff.toNat / 8
                        let subIdx := effOff.toNat % 8
                        match st.stack slotIdx with
                        | .spilled cap =>
                            if w == .u64 then
                              stepNextPc (st.pc + 1) (updateReg st.regs dst cap) st.stack st.world
                            else .stuck (.partialNonScalarStackAccess effOff w)
                        | .bytes val initMask =>
                            if extractSubValue initMask subIdx w != w.valMask64 then
                              .stuck (.uninitializedStackRead effOff w)
                            else
                              let loaded := extractSubValue val subIdx w
                              stepNextPc (st.pc + 1) (updateReg st.regs dst (.scalar loaded)) st.stack st.world

                  | .memPtr id _ _ bufSize baseOff =>
                      let effOff := baseOff + disp
                      if !inBoundsRange effOff w bufSize then
                        .stuck (.outOfBoundsMemoryAccess id effOff w)
                      else
                        let raw := readBufWord st.world.bufBytes id effOff w
                        stepNextPc (st.pc + 1) (updateReg st.regs dst (.scalar raw)) st.stack st.world

                  | .arrayPtr id baseOff =>
                      let effOff := baseOff + disp
                      if !inBoundsRange effOff w (st.world.arrayLen id) then
                        .stuck (.outOfBoundsArrayAccess id effOff w)
                      else
                        let raw := readBufWord st.world.bufBytes id effOff w
                        stepNextPc (st.pc + 1) (updateReg st.regs dst (.scalar raw)) st.stack st.world

                  | .structPtr id _ baseOff =>
                      let effOff := baseOff + disp
                      if !isAligned effOff w then
                        .stuck (.invalidStructFieldAccess id effOff w)
                      else
                        match findStructField (env.structFields id) effOff with
                        | none => .stuck (.invalidStructFieldAccess id effOff w)
                        | some field =>
                            match field.kind with
                            | .scalar sz _ =>
                                if !inBoundsRange (effOff - field.offset) w sz then
                                  .stuck (.invalidStructFieldAccess id effOff w)
                                else
                                  let raw := readBufWord st.world.structBytes id effOff w
                                  stepNextPc (st.pc + 1) (updateReg st.regs dst (.scalar raw)) st.stack st.world
                            | .ptrToMemory is32 fieldId bufSz nullable =>
                                let expW : DataWidth := if is32 then .u32 else .u64
                                if effOff != field.offset || w != expW then
                                  .stuck (.invalidStructFieldAccess id effOff w)
                                else
                                  let targetId := memoryIdPrepend fieldId id
                                  let v :=
                                    if nullable && st.world.structFieldNull targetId then
                                      RuntimeVal.scalar 0#64
                                    else
                                      .memPtr targetId .kernelReadOnly none bufSz 0#64
                                  stepNextPc (st.pc + 1) (updateReg st.regs dst v) st.stack st.world
                            | .ptrToArray is32 arrId =>
                                let expW : DataWidth := if is32 then .u32 else .u64
                                if effOff != field.offset || w != expW then
                                  .stuck (.invalidStructFieldAccess id effOff w)
                                else
                                  let targetId := memoryIdPrepend arrId id
                                  stepNextPc (st.pc + 1)
                                    (updateReg st.regs dst (.arrayPtr targetId 0#64))
                                    st.stack st.world
                            | .ptrToEndArray is32 arrId =>
                                let expW : DataWidth := if is32 then .u32 else .u64
                                if effOff != field.offset || w != expW then
                                  .stuck (.invalidStructFieldAccess id effOff w)
                                else
                                  let targetId := memoryIdPrepend arrId id
                                  stepNextPc (st.pc + 1)
                                    (updateReg st.regs dst (.endArrayPtr targetId))
                                    st.stack st.world

                  | _ => .stuck .invalidDereference

    | .store w base off16 src =>
        if base ≥ REGISTER_COUNT then .stuck (.invalidRegister base)
        else
          match readOp src with
          | .error e => .stuck e
          | .ok vSrc =>
              let vBase := st.regs base
              match checkAlive st.world.liveResources vBase with
              | some e => .stuck e
              | none =>
                  let disp := sextOffset16 off16
                  match vBase with
                  | .stackPtr baseOff =>
                      let effOff := baseOff + disp
                      if effOff ≥ 512#64 then .stuck (.outOfBoundsStackAccess effOff w)
                      else if (effOff.toNat % 8) % w.bytes != 0 then
                        .stuck (.misalignedStackAccess effOff w)
                      else
                        let slotIdx := effOff.toNat / 8
                        let subIdx := effOff.toNat % 8
                        if w == .u64 then
                          let newSlot := match vSrc with
                            | .scalar s => StackSlot.bytes s (~~~0#64)
                            | cap       => StackSlot.spilled cap
                          stepNextPc (st.pc + 1) st.regs (updateStack st.stack slotIdx newSlot) st.world
                        else
                          match vSrc with
                          | .scalar s =>
                              let newSlot := match st.stack slotIdx with
                                | .bytes oldV oldInit =>
                                    StackSlot.bytes
                                      (insertSubValue oldV s subIdx w)
                                      (insertSubValue oldInit (~~~0#64) subIdx w)
                                | .spilled cap =>
                                    StackSlot.bytes
                                      (insertSubValue (cap.toBits layout st.world.arrayLen) s subIdx w)
                                      (insertSubValue 0#64 (~~~0#64) subIdx w)
                              stepNextPc (st.pc + 1) st.regs (updateStack st.stack slotIdx newSlot) st.world
                          | _ => .stuck (.partialNonScalarStackAccess effOff w)

                  | .memPtr id orig _ bufSize baseOff =>
                      if !orig.isWritable then .stuck (.writeToReadOnlyMemory id orig)
                      else
                        match vSrc with
                        | .scalar s =>
                            let effOff := baseOff + disp
                            if !inBoundsRange effOff w bufSize then
                              .stuck (.outOfBoundsMemoryAccess id effOff w)
                            else
                              let buf' := writeBufWord st.world.bufBytes id effOff w s
                              stepNextPc (st.pc + 1) st.regs st.stack { st.world with bufBytes := buf' }
                        | _ => .stuck .pointerLeakToMemoryOrReturn

                  | .arrayPtr id baseOff =>
                      match vSrc with
                      | .scalar s =>
                          let effOff := baseOff + disp
                          if !inBoundsRange effOff w (st.world.arrayLen id) then
                            .stuck (.outOfBoundsArrayAccess id effOff w)
                          else
                            let buf' := writeBufWord st.world.bufBytes id effOff w s
                            stepNextPc (st.pc + 1) st.regs st.stack { st.world with bufBytes := buf' }
                      | _ => .stuck .pointerLeakToMemoryOrReturn

                  | .structPtr id _ baseOff =>
                      match vSrc with
                      | .scalar s =>
                          let effOff := baseOff + disp
                          if !isAligned effOff w then
                            .stuck (.invalidStructFieldAccess id effOff w)
                          else
                            match findStructField (env.structFields id) effOff with
                            | none => .stuck (.invalidStructFieldAccess id effOff w)
                            | some field =>
                                match field.kind with
                                | .scalar sz true =>
                                    if !inBoundsRange (effOff - field.offset) w sz then
                                      .stuck (.invalidStructFieldAccess id effOff w)
                                    else
                                      let sb' := writeBufWord st.world.structBytes id effOff w s
                                      stepNextPc (st.pc + 1) st.regs st.stack
                                        { st.world with structBytes := sb' }
                                | _ => .stuck (.invalidStructFieldAccess id effOff w)
                      | _ => .stuck .pointerLeakToMemoryOrReturn

                  | _ => .stuck .invalidDereference

    | .atomic is64 op base off16 src =>
        let w : DataWidth := if is64 then .u64 else .u32
        if base ≥ REGISTER_COUNT then .stuck (.invalidRegister base)
        else if src ≥ REGISTER_COUNT then .stuck (.invalidRegister src)
        else
          let needsSrcWrite : Bool :=
            match op with
            | .add f | .or f | .and f | .xor f => f
            | .xchg => true
            | .cmpxchg => false
          if needsSrcWrite && src ≥ GENERAL_REGISTER_COUNT then .stuck .writeToReadOnlyFramePtr
          else
            let vBase := st.regs base
            let vSrc := st.regs src
            let vR0 := st.regs 0
            match checkAlive st.world.liveResources vBase,
                  checkAlive st.world.liveResources vSrc,
                  (if op == .cmpxchg then checkAlive st.world.liveResources vR0 else none) with
            | some e, _, _ => .stuck e
            | _, some e, _ => .stuck e
            | _, _, some e => .stuck e
            | none, none, none =>
                let disp := sextOffset16 off16
                let applyRmw (oldVal srcVal r0Val : BitVec 64) : BitVec 64 × Option (Nat × BitVec 64) :=
                  if is64 then
                    match op with
                    | .add f   => (oldVal + srcVal, if f then some (src, oldVal) else none)
                    | .or  f   => (oldVal ||| srcVal, if f then some (src, oldVal) else none)
                    | .and f   => (oldVal &&& srcVal, if f then some (src, oldVal) else none)
                    | .xor f   => (oldVal ^^^ srcVal, if f then some (src, oldVal) else none)
                    | .xchg    => (srcVal, some (src, oldVal))
                    | .cmpxchg => (if oldVal == r0Val then srcVal else oldVal, some (0, oldVal))
                  else
                    let o32 := oldVal.setWidth 32
                    let s32 := srcVal.setWidth 32
                    let r32 := r0Val.setWidth 32
                    let zext (v : BitVec 32) := v.zeroExtend 64
                    match op with
                    | .add f   => (zext (o32 + s32), if f then some (src, zext o32) else none)
                    | .or  f   => (zext (o32 ||| s32), if f then some (src, zext o32) else none)
                    | .and f   => (zext (o32 &&& s32), if f then some (src, zext o32) else none)
                    | .xor f   => (zext (o32 ^^^ s32), if f then some (src, zext o32) else none)
                    | .xchg    => (zext s32, some (src, zext o32))
                    | .cmpxchg => (zext (if o32 == r32 then s32 else o32), some (0, zext o32))
                match vBase with
                | .stackPtr baseOff =>
                    let effOff := baseOff + disp
                    if effOff ≥ 512#64 then .stuck (.outOfBoundsStackAccess effOff w)
                    else if !isAligned effOff w then .stuck (.misalignedAtomicAccess effOff w)
                    else
                      match vSrc, (if op == .cmpxchg then vR0 else .scalar 0#64) with
                      | .scalar sVal, .scalar r0Val =>
                          let slotIdx := effOff.toNat / 8
                          let subIdx := effOff.toNat % 8
                          match st.stack slotIdx with
                          | .spilled _ => .stuck (.partialNonScalarStackAccess effOff w)
                          | .bytes oldSlot initMask =>
                              if extractSubValue initMask subIdx w != w.valMask64 then
                                .stuck (.uninitializedStackRead effOff w)
                              else
                                let oldSlice := extractSubValue oldSlot subIdx w
                                let (newSlice, wb?) := applyRmw oldSlice sVal r0Val
                                let newSlot := StackSlot.bytes
                                  (insertSubValue oldSlot newSlice subIdx w)
                                  (insertSubValue initMask (~~~0#64) subIdx w)
                                let regs' := match wb? with
                                  | none => st.regs
                                  | some (r, v) => updateReg st.regs r (.scalar v)
                                stepNextPc (st.pc + 1) regs' (updateStack st.stack slotIdx newSlot) st.world
                      | _, _ => .stuck .pointerLeakToMemoryOrReturn

                | .memPtr id orig _ bufSize baseOff =>
                    if !orig.isWritable then .stuck (.writeToReadOnlyMemory id orig)
                    else if !orig.isMap then .stuck .invalidAtomicTarget
                    else
                      let effOff := baseOff + disp
                      if !inBoundsRange effOff w bufSize then
                        .stuck (.outOfBoundsMemoryAccess id effOff w)
                      else if !isAligned effOff w then
                        .stuck (.misalignedAtomicAccess effOff w)
                      else
                        match vSrc, (if op == .cmpxchg then vR0 else .scalar 0#64) with
                        | .scalar sVal, .scalar r0Val =>
                            let oldVal := readBufWord st.world.bufBytes id effOff w
                            let (newVal, wb?) := applyRmw oldVal sVal r0Val
                            let buf' := writeBufWord st.world.bufBytes id effOff w newVal
                            let regs' := match wb? with
                              | none => st.regs
                              | some (r, v) => updateReg st.regs r (.scalar v)
                            stepNextPc (st.pc + 1) regs' st.stack { st.world with bufBytes := buf' }
                        | _, _ => .stuck .pointerLeakToMemoryOrReturn

                | _ => .stuck .invalidAtomicTarget

    | .loadPacket w immOff indReg? =>
        match env.packetCtx with
        | none => .stuck .invalidPacketContext
        | some pktSpec =>
            if !pktSpec.matchesVal (st.regs 6) then
              .stuck .invalidPacketContext
            else
              let indVal? : Except SafetyViolation (BitVec 32) :=
                match indReg? with
                | none => .ok 0#32
                | some r =>
                    if r ≥ REGISTER_COUNT then .error (.invalidRegister r)
                    else
                      match st.regs r with
                      | .scalar v => .ok (v.setWidth 32)
                      | _ => .error .invalidPacketContext
              match indVal? with
              | .error e => .stuck e
              | .ok indVal32 =>
                  if BitVec.saddOverflow indVal32 immOff then
                    if st.world.liveResources.isEmpty then
                      .halted 0#64 (bumpWorldStep st.world)
                    else
                      .stuck (.resourceLeakOnExit st.world.liveResources)
                  else
                    let totalOff32 := indVal32 + immOff
                    match env.packetOracle st.world.stepCount totalOff32 w with
                    | none =>
                        if st.world.liveResources.isEmpty then
                          .halted 0#64 (bumpWorldStep st.world)
                        else
                          .stuck (.resourceLeakOnExit st.world.liveResources)
                    | some loadedVal =>
                        let clobber (r : Nat) : RuntimeVal :=
                          if r == 0 then .scalar (loadedVal &&& w.valMask64)
                          else if 1 ≤ r && r ≤ 5 then
                            if secret.preserveCallScratch then
                              .scalar ((st.regs r).toBits layout st.world.arrayLen)
                            else
                              .scalar (secret.regBits st.world.stepCount r)
                          else st.regs r
                        stepNextPc (st.pc + 1) clobber st.stack st.world

    | .callExternal helperIdx =>
        match env.helpers helperIdx with
        | none => .stuck (.unregisteredHelper helperIdx)
        | some spec =>
            match checkAndApplyHelperArgs env layout st.regs st.stack helperIdx 0
              spec.args st.stack st.world with
            | .error e => .stuck e
            | .ok (stkAfterArgs, wrldAfterArgs) =>
                let wrldBounds :=
                  if spec.invalidateArrayBounds then
                    { wrldAfterArgs with arrayLen := env.helperArrayLenOracle wrldAfterArgs.stepCount }
                  else wrldAfterArgs
                match evalHelperReturn env layout secret st.regs st.world helperIdx
                  spec.ret wrldBounds with
                | none => .stuck (.helperContractViolation helperIdx)
                | some (r0Val, wrldFinal) =>
                    let regs' (r : Nat) : RuntimeVal :=
                      if r == 0 then r0Val
                      else if 1 ≤ r && r ≤ 5 then
                        if secret.preserveCallScratch then
                          .scalar ((st.regs r).toBits layout st.world.arrayLen)
                        else
                          .scalar (secret.regBits st.world.stepCount r)
                      else st.regs r
                    stepNextPc (st.pc + 1) regs' stkAfterArgs wrldFinal

    | .exit =>
        if !st.world.liveResources.isEmpty then
          .stuck (.resourceLeakOnExit st.world.liveResources)
        else
          match st.regs 0 with
          | .scalar ret => .halted ret (bumpWorldStep st.world)
          | _           => .stuck .pointerLeakToMemoryOrReturn
  else
    .stuck (.invalidPc st.pc)

/--
**Informal**: Bounded multi-step execution (`runSteps`): iterates `step` for at
most `fuel` transitions, returning `some (.inl (ret, finalWorld))` if the
program halts cleanly, `some (.inr err)` if it triggers a `SafetyViolation`, or
`none` if `fuel` is exhausted before halting.
-/
def runSteps (prog : Program) (env : ExecEnv) (layout : HostLayout) (secret : SecretInit) :
    Nat → MachineState → Option (Sum (BitVec 64 × WorldState) SafetyViolation)
  | 0, _ => none
  | fuel + 1, st =>
      match step prog env layout secret st with
      | .next st'        => runSteps prog env layout secret fuel st'
      | .halted ret wrld => some (.inl (ret, wrld))
      | .stuck err       => some (.inr err)

/--
**Informal**: Single-Execution Safety, Resource Linearity & Bounded Termination
(`SafeAndTerminating`): starting from `mkInit layout secret` under any valid
host address layout `layout` and any initial uninitialized memory/register bits
`secret`, the eBPF program terminates within `fuel` steps without triggering any
`SafetyViolation`, returns a 64-bit scalar `ret`, and leaves zero unreleased
resources (`finalWorld.liveResources = []`).
-/
def SafeAndTerminating (prog : Program) (env : ExecEnv) (fuel : Nat)
    (mkInit : HostLayout → SecretInit → MachineState) : Prop :=
  ∀ (layout : HostLayout) (secret : SecretInit),
    ValidHostLayout layout →
    ∃ ret finalWorld,
      runSteps prog env layout secret fuel (mkInit layout secret) = some (.inl (ret, finalWorld)) ∧
      finalWorld.liveResources = []

/--
**Informal**: Relational 2-Safety Non-Interference (`NonInterfering`): for any
two valid host pointer layouts `layout₁, layout₂` and any two initial
uninitialized stack/register bit assignments `secret₁, secret₂`, both runs of
the eBPF program terminate cleanly within `fuel` steps without safety violations
and produce the **exact same scalar return value `ret`** and **exact same
observable `finalWorld`**. This guarantees that neither KASLR host pointer
addresses nor uninitialized stack/register bytes can leak into program outputs.
-/
def NonInterfering (prog : Program) (env : ExecEnv) (fuel : Nat)
    (mkInit : HostLayout → SecretInit → MachineState) : Prop :=
  ∀ (layout₁ layout₂ : HostLayout) (secret₁ secret₂ : SecretInit),
    ValidHostLayout layout₁ →
    ValidHostLayout layout₂ →
    ∃ ret finalWorld,
      runSteps prog env layout₁ secret₁ fuel (mkInit layout₁ secret₁) = some (.inl (ret, finalWorld)) ∧
      runSteps prog env layout₂ secret₂ fuel (mkInit layout₂ secret₂) = some (.inl (ret, finalWorld)) ∧
      finalWorld.liveResources = []

/--
**Informal**: A 64-bit word `x` belongs to the abstract unsigned range
`r : U64Range` if `r.min ≤ x ≤ r.max`.
-/
def ConcretizesRange64 (r : ebpf_core.scalar_value.U64Range) (x : BitVec 64) : Prop :=
  r.min.bv ≤ x ∧ x ≤ r.max.bv

/--
**Informal**: A 32-bit word `x` belongs to the abstract unsigned range
`r : U32Range` if `r.min ≤ x ≤ r.max`.
-/
def ConcretizesRange32 (r : ebpf_core.scalar_value.U32Range) (x : BitVec 32) : Prop :=
  r.min.bv ≤ x ∧ x ≤ r.max.bv

/--
**Informal**: Structural well-formedness of an abstract 64-bit scalar value
`d : U64ScalarValueData`: unknown bits are zero in `d.value`, unwritten bits are
a subset of unknown bits, and the unsigned interval `[d.urange.min, d.urange.max]`
is non-empty and compatible with the known-bit mask.
-/
def Scalar64Consistent (d : ebpf_core.scalar_value.U64ScalarValueData) : Prop :=
  (d.value.bv &&& d.unknown_mask.bv = 0#64) ∧
  (d.unwritten_mask.bv &&& ~~~d.unknown_mask.bv = 0#64) ∧
  d.urange.min.bv ≤ d.urange.max.bv ∧
  d.value.bv ≤ d.urange.min.bv ∧
  d.urange.max.bv ≤ (d.value.bv ||| d.unknown_mask.bv)

/--
**Informal**: Structural well-formedness of an abstract 32-bit scalar value
`d : U32ScalarValueData`: unknown bits are zero in `d.value`, unwritten bits are
a subset of unknown bits, and `[d.urange.min, d.urange.max]` is non-empty and
compatible with the known-bit mask.
-/
def Scalar32Consistent (d : ebpf_core.scalar_value.U32ScalarValueData) : Prop :=
  (d.value.bv &&& d.unknown_mask.bv = 0#32) ∧
  (d.unwritten_mask.bv &&& ~~~d.unknown_mask.bv = 0#32) ∧
  d.urange.min.bv ≤ d.urange.max.bv ∧
  d.value.bv ≤ d.urange.min.bv ∧
  d.urange.max.bv ≤ (d.value.bv ||| d.unknown_mask.bv)

/--
**Informal**: Concrete 64-bit word `x` is a valid inhabitant of abstract scalar
`d : U64ScalarValueData` (`ConcretizesScalar64`) if `d` is well-formed, `x`
matches `d.value` on all known bits (`~~~d.unknown_mask`), and `x` lies within
`[d.urange.min, d.urange.max]`.
-/
def ConcretizesScalar64 (d : ebpf_core.scalar_value.U64ScalarValueData) (x : BitVec 64) : Prop :=
  Scalar64Consistent d ∧
  (x &&& ~~~d.unknown_mask.bv = d.value.bv) ∧
  d.urange.min.bv ≤ x ∧ x ≤ d.urange.max.bv

/--
**Informal**: Concrete 32-bit word `x` is a valid inhabitant of abstract scalar
`d : U32ScalarValueData` (`ConcretizesScalar32`) if `d` is well-formed, `x`
matches `d.value` on all known bits (`~~~d.unknown_mask`), and `x` lies within
`[d.urange.min, d.urange.max]`.
-/
def ConcretizesScalar32 (d : ebpf_core.scalar_value.U32ScalarValueData) (x : BitVec 32) : Prop :=
  Scalar32Consistent d ∧
  (x &&& ~~~d.unknown_mask.bv = d.value.bv) ∧
  d.urange.min.bv ≤ x ∧ x ≤ d.urange.max.bv

/--
**Informal**: Relational 2-safety concretization (`ConcretizesScalar64Rel2`):
two 64-bit words `x₁, x₂` both concretize `d : U64ScalarValueData` and agree on
all initialized (written) bit positions (`~~~d.unwritten_mask`).
-/
def ConcretizesScalar64Rel2 (d : ebpf_core.scalar_value.U64ScalarValueData) (x₁ x₂ : BitVec 64) : Prop :=
  ConcretizesScalar64 d x₁ ∧
  ConcretizesScalar64 d x₂ ∧
  (x₁ &&& ~~~d.unwritten_mask.bv = x₂ &&& ~~~d.unwritten_mask.bv)

/--
**Informal**: Single-step simulation predicate (`BackendSimulatesProgram`)
connecting the Aeneas-extracted interpreter loop body
(`ebpf_core.executor.execute_core_loop1.body`) over a host `EbpfMachineBackend`
to the reference small-step operational semantics (`step`).
-/
def BackendSimulatesProgram {B : Type}
    (inst : ebpf_core.executor.EbpfMachineBackend B)
    (code : Aeneas.Std.Slice ebpf_core.api.EbpfInstruction)
    (prog : Program) (env : ExecEnv) (layout : HostLayout) (secret : SecretInit)
    (R : B → MachineState → Prop) : Prop :=
  prog.size ≤ Aeneas.Std.Usize.max ∧
  code.val.length = prog.size ∧
  (∀ b st, R b st →
    ∀ (istate : ebpf_core.executor.InterpreterState),
      istate.result = none →
      istate.pc.val = st.pc →
      InterpreterRegsMatch layout st istate.registers →
      (∀ st', step prog env layout secret st = .next st' →
        ∃ stateNext b',
          ebpf_core.executor.execute_core_loop1.body inst code b istate =
            .ok (.cont (b', stateNext)) ∧
          stateNext.result = none ∧
          stateNext.pc.val = st'.pc ∧
          InterpreterRegsMatch layout st' stateNext.registers ∧
          R b' st') ∧
      (∀ ret wrld', step prog env layout secret st = .halted ret wrld' →
        ∃ stateDone b',
          ebpf_core.executor.execute_core_loop1.body inst code b istate =
            .ok (.cont (b', stateDone)) ∧
          stateDone.result = some ⟨ret⟩ ∧
          R b' { st with world := wrld' }))

end StarnixEbpf.Spec
