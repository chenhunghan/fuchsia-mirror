-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Extracted

/-!
# Formal Assumptions on External Dependencies, Calling Context & Host Layout

This security-critical module (`spec/Assumptions.lean`, classified as High risk
in `RISK.md` and governed by `../REVIEW.md`) isolates all assumptions made about
code and state *outside* `//src/starnix/lib/ebpf/core`:
1. Host virtual memory layout (`HostLayout`, `ValidHostLayout`).
2. Uninitialized host stack/register bits (`SecretInit`) and concrete machine
   values (`RuntimeVal`, `StackSlot`, `ExecEnv`, `WorldState`, `MachineState`).
3. Caller preconditions when invoking an eBPF program (`CoreCallingContext`,
   `EntryArgConforms`, `EntryStatesConform`).
4. Behavioral contracts assumed of external traits (`CoreVerifierLogger`,
   `EbpfMachineBackend`, `InterpreterRegsMatch`).

Every top-level formal declaration is paired with a mandatory `**Informal**:`
English specification.
-/

namespace StarnixEbpf.Assumptions

/--
**Informal**: The eBPF instruction set provides 10 general-purpose read/write
registers (`r0` through `r9`).
-/
abbrev GENERAL_REGISTER_COUNT : Nat := 10

/--
**Informal**: An eBPF virtual machine has 11 architectural registers in total:
10 general-purpose registers (`r0`..`r9`) plus the read-only stack frame pointer
`r10`.
-/
abbrev REGISTER_COUNT : Nat := 11

/--
**Informal**: Each eBPF program invocation has a fixed 512-byte stack frame,
indexed downward from the frame pointer `r10`.
-/
abbrev BPF_STACK_SIZE : Nat := 512

/--
**Informal**: The 512-byte eBPF stack frame is partitioned into 64 aligned
8-byte (64-bit) stack slots.
-/
abbrev BPF_STACK_SLOTS : Nat := 64

/--
**Informal**: Maximum number of single-step verifier visits permitted by
`verify_program_core` (`10 * BPF_MAX_INSTS + 1 = 655,361`), bounding the
worst-case instruction execution count of every accepted program.
-/
abbrev VERIFIER_MAX_STEPS : Nat := 655361

/--
**Informal**: Packet offsets validated by the verifier are bounded above by
65,535 (`0xFFFF`) bytes to prevent 64-bit address wraparound.
-/
abbrev MAX_PACKET_OFFSET : BitVec 64 := 0xFFFF#64

/--
**Informal**: Variable pointer offsets in the verifier are bounded in magnitude
by $2^{29}$ bytes (`BPF_MAX_VAR_OFF`) so pointer arithmetic cannot overflow.
-/
def BPF_MAX_VAR_OFF : BitVec 64 := 1#64 <<< 29

/--
**Informal**: Canonical upper bound ($2^{56}$) on userspace virtual object base
addresses in Starnix on Zircon (`x86_64`, `arm64`, `riscv64`) and Linux
(`LA48`/`LA57`). Since all verifier-admitted pointer offsets are at most
$2^{32}$, `base + off < 2^{57} < 2^{63}` never wraps modulo $2^{64}$ or flips
the 64-bit sign bit.
-/
def CANONICAL_USER_MAX : BitVec 64 := 0x0100_0000_0000_0000#64

/--
**Informal**: Namespace tag (`Hardcoded`, `Generated`, or `Verification`) for
memory object identifiers in `ebpf_core::verifier::Namespace`.
-/
abbrev MemoryNamespace := ebpf_core.verifier.Namespace

/--
**Informal**: Hierarchical identifier (`ebpf_core::verifier::MemoryId`)
distinguishing distinct host memory regions, context structures, packet arrays,
and releasable kernel resources.
-/
abbrev MemoryId := ebpf_core.verifier.MemoryId

/--
**Informal**: Decidable equality on `MemoryNamespace`.
-/
instance : DecidableEq ebpf_core.verifier.Namespace := fun a b =>
  match a, b with
  | .Hardcoded, .Hardcoded => isTrue rfl
  | .Generated, .Generated => isTrue rfl
  | .Verification, .Verification => isTrue rfl
  | .Hardcoded, .Generated => isFalse (by intro h; cases h)
  | .Hardcoded, .Verification => isFalse (by intro h; cases h)
  | .Generated, .Hardcoded => isFalse (by intro h; cases h)
  | .Generated, .Verification => isFalse (by intro h; cases h)
  | .Verification, .Hardcoded => isFalse (by intro h; cases h)
  | .Verification, .Generated => isFalse (by intro h; cases h)

/--
**Informal**: Default `MemoryNamespace` inhabitant (`Hardcoded`).
-/
instance : Inhabited ebpf_core.verifier.Namespace := ⟨.Hardcoded⟩

/--
**Informal**: Structural decision procedure for equality of two hierarchical
`MemoryId` values.
-/
def decEqMemoryId : (a b : ebpf_core.verifier.MemoryId) → Decidable (a = b)
  | .mk ns1 id1 none, .mk ns2 id2 none =>
      if h : ns1 = ns2 ∧ id1 = id2 then
        isTrue (by cases h; subst_vars; rfl)
      else
        isFalse (by intro heq; cases heq; exact h ⟨rfl, rfl⟩)
  | .mk _ _ none, .mk _ _ (some _) =>
      isFalse (by intro heq; cases heq)
  | .mk _ _ (some _), .mk _ _ none =>
      isFalse (by intro heq; cases heq)
  | .mk ns1 id1 (some p1), .mk ns2 id2 (some p2) =>
      match decEqMemoryId p1 p2 with
      | isTrue hp =>
          if h : ns1 = ns2 ∧ id1 = id2 then
            isTrue (by cases h; subst_vars; rfl)
          else
            isFalse (by intro heq; cases heq; exact h ⟨rfl, rfl⟩)
      | isFalse hp =>
          isFalse (by intro heq; cases heq; exact hp rfl)

/--
**Informal**: Decidable equality instance for `MemoryId`.
-/
instance : DecidableEq ebpf_core.verifier.MemoryId := decEqMemoryId

/--
**Informal**: Default `MemoryId` inhabitant.
-/
instance : Inhabited ebpf_core.verifier.MemoryId :=
  ⟨.mk .Hardcoded ⟨0#64⟩ none⟩

/--
**Informal**: Appends `other` at the root of `self`'s parent chain, matching
`ebpf_core::verifier::MemoryId::prepended`.
-/
def memoryIdPrepend : MemoryId → MemoryId → MemoryId
  | .mk ns i none, other => .mk ns i (some other)
  | .mk ns i (some p), other => .mk ns i (some (memoryIdPrepend p other))

/--
**Informal**: Returns `true` if and only if the immediate parent of `self` is
`expectedParent`, matching `ebpf_core::verifier::MemoryId::has_parent`.
-/
def memoryIdHasParent (self expectedParent : MemoryId) : Bool :=
  match self.parent with
  | none => false
  | some p => p == expectedParent

/--
**Informal**: Prefix-compatibility check on two `MemoryId` chains, matching
`ebpf_core::verifier::MemoryId::matches`.
-/
def memoryIdMatches : MemoryId → MemoryId → Bool
  | .mk ns1 id1 p1, .mk ns2 id2 p2 =>
      if ns1 != ns2 || id1 != id2 then false
      else
        match p1, p2 with
        | none, _ => true
        | some _, none => true
        | some q1, some q2 => memoryIdMatches q1 q2

/--
**Informal**: Returns `true` if `MemoryId` contains only external caller
namespaces (`Hardcoded` or `Generated`) and no internal `Verification` counter
nodes.
-/
def memoryIdNoVerif : MemoryId → Bool
  | .mk .Verification _ _ => false
  | .mk _ _ none => true
  | .mk _ _ (some p) => memoryIdNoVerif p

/--
**Informal**: Allocates a fresh runtime `MemoryId` in the `Verification`
namespace from monotonically increasing allocation counter `n`.
-/
def freshRuntimeId (n : Nat) : MemoryId :=
  .mk .Verification ⟨BitVec.ofNat 64 n⟩ none

/--
**Informal**: Width of an eBPF memory or packet access (1, 2, 4, or 8 bytes),
corresponding to `ebpf_core::api::DataWidth`.
-/
inductive DataWidth where
  | u8
  | u16
  | u32
  | u64
  deriving DecidableEq, Repr, Inhabited

namespace DataWidth

/--
**Informal**: Number of bytes (`1`, `2`, `4`, or `8`) accessed by a `DataWidth`.
-/
def bytes : DataWidth → Nat
  | .u8  => 1
  | .u16 => 2
  | .u32 => 4
  | .u64 => 8

/--
**Informal**: Number of bits (`8`, `16`, `32`, or `64`) accessed by a `DataWidth`.
-/
def bits : DataWidth → Nat
  | .u8  => 8
  | .u16 => 16
  | .u32 => 32
  | .u64 => 64

/--
**Informal**: Byte width of a `DataWidth` represented as a 64-bit bitvector.
-/
def bvBytes (w : DataWidth) : BitVec 64 :=
  BitVec.ofNat 64 w.bytes

/--
**Informal**: Bitmask selecting the low `w.bits` bits of a 64-bit word.
-/
def valMask64 : DataWidth → BitVec 64
  | .u8  => 0xFF#64
  | .u16 => 0xFFFF#64
  | .u32 => 0xFFFF_FFFF#64
  | .u64 => ~~~0#64

/--
**Informal**: Converts an Aeneas-extracted `ebpf_core.api.DataWidth` value into
the specification's `DataWidth`.
-/
def ofCore : ebpf_core.api.DataWidth → DataWidth
  | .U8  => .u8
  | .U16 => .u16
  | .U32 => .u32
  | .U64 => .u64

end DataWidth

/--
**Informal**: Provenance and mutability classification of a host buffer pointer
(read-only vs. writable eBPF map value or kernel buffer), corresponding to
`ebpf_core::verifier::PointerOrigin`.
-/
inductive MemoryOrigin where
  | mapReadOnly
  | mapWritable
  | kernelReadOnly
  | kernelWritable
  deriving DecidableEq, Repr, Inhabited

namespace MemoryOrigin

/--
**Informal**: Returns `true` if and only if the memory region permits store and
helper output operations.
-/
def isWritable : MemoryOrigin → Bool
  | .mapWritable | .kernelWritable => true
  | .mapReadOnly | .kernelReadOnly => false

/--
**Informal**: Returns `true` if and only if the memory region is backed by an
eBPF map value (required for atomic operations on shared memory).
-/
def isMap : MemoryOrigin → Bool
  | .mapReadOnly | .mapWritable => true
  | .kernelReadOnly | .kernelWritable => false

/--
**Informal**: Converts an Aeneas-extracted `ebpf_core.verifier.PointerOrigin`
value into the specification's `MemoryOrigin`.
-/
def ofCore : ebpf_core.verifier.PointerOrigin → MemoryOrigin
  | .MapReadOnly    => .mapReadOnly
  | .MapWritable    => .mapWritable
  | .KernelReadOnly => .kernelReadOnly
  | .KernelWritable => .kernelWritable

end MemoryOrigin

/--
**Informal**: Concrete 64-bit host virtual address assignment (`HostLayout`)
mapping the eBPF stack, sized buffers, context structs, packet arrays, and map
descriptors to 64-bit host addresses. Quantifying over all valid layouts proves
that program behavior never depends on or leaks raw host/KASLR addresses.
-/
structure HostLayout where
  /-- Base address of the 512-byte stack (`r10` holds `stackBase + 512`). -/
  stackBase : BitVec 64
  /-- Base address of sized memory buffer `id`. -/
  bufBase : MemoryId → BitVec 64
  /-- Base address of host context struct `id`. -/
  structBase : MemoryId → BitVec 64
  /-- Base address of packet/data array `id`. -/
  arrayBase : MemoryId → BitVec 64
  /-- Non-null opaque address of pinned map descriptor `schemaIdx`. -/
  mapAddr : Nat → BitVec 64

/--
**Informal**: Host virtual address layout assumption (`ValidHostLayout`): every
live userspace/Starnix object base (the 512-byte stack, sized buffers, context
structs, and packet arrays) resides at a non-null canonical virtual address in
`(0, CANONICAL_USER_MAX]`, and every map descriptor handle is non-null.
-/
def ValidHostLayout (layout : HostLayout) : Prop :=
  (0#64 < layout.stackBase ∧ layout.stackBase ≤ CANONICAL_USER_MAX) ∧
  (∀ id, 0#64 < layout.bufBase id ∧ layout.bufBase id ≤ CANONICAL_USER_MAX) ∧
  (∀ id, 0#64 < layout.structBase id ∧ layout.structBase id ≤ CANONICAL_USER_MAX) ∧
  (∀ id, 0#64 < layout.arrayBase id ∧ layout.arrayBase id ≤ CANONICAL_USER_MAX) ∧
  (∀ idx, layout.mapAddr idx ≠ 0#64)

/--
**Informal**: Adversarial bit-pattern assignment (`SecretInit`) for
uninitialized stack slots and caller-saved scratch registers (`r0`..`r5`).
Quantifying over arbitrary `SecretInit` instances proves that verified eBPF
programs never read or leak uninitialized host memory or register contents.
-/
structure SecretInit where
  stackBits : Nat → BitVec 64
  regBits : Nat → Nat → BitVec 64
  preserveCallScratch : Bool := false

/--
**Informal**: Runtime value (`RuntimeVal`) held in an eBPF register or spilled
stack slot. Distinguishes pure 64-bit scalars (including null `0`) from
capability-bearing pointers (stack pointers, sized buffer pointers, context
struct pointers, packet start/end pointers, and map descriptors) while
projecting to a concrete 64-bit word under any `HostLayout`.
-/
inductive RuntimeVal where
  | scalar (val : BitVec 64)
  | stackPtr (off : BitVec 64)
  | memPtr (id : MemoryId) (origin : MemoryOrigin) (resId : Option MemoryId)
      (bufSize : BitVec 64) (off : BitVec 64)
  | structPtr (id : MemoryId) (resId : Option MemoryId) (off : BitVec 64)
  | arrayPtr (id : MemoryId) (off : BitVec 64)
  | endArrayPtr (id : MemoryId)
  | mapPtr (schemaIdx : Nat)
  deriving DecidableEq, Inhabited

namespace RuntimeVal

/--
**Informal**: Extracts the releasable resource identifier associated with a
runtime value, if it is a releasable buffer or struct pointer.
-/
def resourceId? : RuntimeVal → Option MemoryId
  | .memPtr _ _ resId _ _ => resId
  | .structPtr _ resId _  => resId
  | _                     => none

/--
**Informal**: Strips any releasable resource wrapper from a runtime pointer
value, matching `Type::Releasable` unwrapping in `Type::match_parameter_type`.
-/
def stripResource : RuntimeVal → RuntimeVal
  | .memPtr id orig _ sz off => .memPtr id orig none sz off
  | .structPtr id _ off      => .structPtr id none off
  | v                        => v

/--
**Informal**: Projects an abstract `RuntimeVal` to its concrete 64-bit register
representation under host address assignment `layout` and packet lengths
`arrayLen`.
-/
def toBits (layout : HostLayout) (arrayLen : MemoryId → BitVec 64) : RuntimeVal → BitVec 64
  | .scalar v               => v
  | .stackPtr off           => layout.stackBase + off
  | .memPtr id _ _ _ off    => layout.bufBase id + off
  | .structPtr id _ off     => layout.structBase id + off
  | .arrayPtr id off        => layout.arrayBase id + off
  | .endArrayPtr id         => layout.arrayBase id + arrayLen id
  | .mapPtr idx             => layout.mapAddr idx

/--
**Informal**: Returns `true` if and only if the runtime value is a pure 64-bit
scalar (and not a capability-bearing pointer).
-/
def isScalar : RuntimeVal → Bool
  | .scalar _ => true
  | _         => false

end RuntimeVal

/--
**Informal**: State of a single 8-byte stack slot (`0..64`), which either holds
raw scalar bytes together with a bitmask `initMask` tracking which bits have
been initialized, or holds a full 64-bit spilled capability (`RuntimeVal`).
-/
inductive StackSlot where
  | bytes (val : BitVec 64) (initMask : BitVec 64)
  | spilled (v : RuntimeVal)
  deriving DecidableEq, Inhabited

/--
**Informal**: Complete 64-slot (512-byte) stack memory mapping slot index
`0..64` to its `StackSlot`.
-/
abbrev StackMem := Nat → StackSlot

/--
**Informal**: Initial stack memory before program execution begins: every slot
contains arbitrary secret bits `secret.stackBits slot` and has `initMask = 0`
(completely uninitialized).
-/
def initStackMem (secret : SecretInit) : StackMem :=
  fun slot => StackSlot.bytes (secret.stackBits slot) 0#64

/--
**Informal**: Specification of a pinned eBPF map (`MapSpec`), recording its map
type, key size, value size, maximum entry count, and whether eBPF programs have
read-only access (`MapFlags::ProgReadOnly`).
-/
structure MapSpec where
  mapType : BitVec 32
  keySize : BitVec 64
  valueSize : BitVec 64
  maxEntries : BitVec 32
  progReadOnly : Bool
  deriving DecidableEq, Repr

/--
**Informal**: Extracts a `MapSpec` from an Aeneas-extracted
`ebpf_core.maps.MapSchema` (`MapFlags::ProgReadOnly = 1 << 7 = 0x80`).
-/
def MapSpec.ofCore (s : ebpf_core.maps.MapSchema) : MapSpec :=
  { mapType := s.map_type.bv
    keySize := s.key_size.bv.zeroExtend 64
    valueSize := s.value_size.bv.zeroExtend 64
    maxEntries := s.max_entries.bv
    progReadOnly := (s.flags.bv &&& 0x80#32) != 0#32 }

/--
**Informal**: Semantic classification of a host context struct field: a
read-only or mutable scalar region of `size` bytes, a non-null or nullable
read-only buffer pointer, a packet/array start pointer, or a packet/array end
pointer.
-/
inductive StructFieldKind where
  | scalar (size : BitVec 64) (mutable : Bool)
  | ptrToMemory (is32Bit : Bool) (fieldId : MemoryId) (bufSize : BitVec 64) (nullable : Bool)
  | ptrToArray (is32Bit : Bool) (arrayId : MemoryId)
  | ptrToEndArray (is32Bit : Bool) (arrayId : MemoryId)
  deriving DecidableEq

/--
**Informal**: Byte span occupied by a `StructFieldKind` in the context struct
view exposed to eBPF bytecode.
-/
def StructFieldKind.byteSize : StructFieldKind → BitVec 64
  | .scalar sz _ => sz
  | .ptrToMemory is32 _ _ _ | .ptrToArray is32 _ | .ptrToEndArray is32 _ =>
      if is32 then 4#64 else 8#64

/--
**Informal**: Descriptor for a single field of a context struct at byte offset
`offset`.
-/
structure StructFieldDesc where
  offset : BitVec 64
  kind : StructFieldKind
  deriving DecidableEq

/--
**Informal**: Converts an Aeneas-extracted `ebpf_core.verifier.FieldDescriptor`
into a `StructFieldDesc`.
-/
def StructFieldDesc.ofCore (fd : ebpf_core.verifier.FieldDescriptor) : StructFieldDesc :=
  let kind := match fd.field_type with
    | .Scalar sz => .scalar (BitVec.ofNat 64 sz.val) false
    | .MutableScalar sz => .scalar (BitVec.ofNat 64 sz.val) true
    | .PtrToMemory is32 id sz => .ptrToMemory is32 id (BitVec.ofNat 64 sz.val) false
    | .NullablePtrToMemory is32 id sz => .ptrToMemory is32 id (BitVec.ofNat 64 sz.val) true
    | .PtrToArray is32 id => .ptrToArray is32 id
    | .PtrToEndArray is32 id => .ptrToEndArray is32 id
  { offset := BitVec.ofNat 64 fd.offset.val, kind }

/--
**Informal**: Entry pointer capability identity (`ContextObjSpec`) for matching
`ContextParameter` helper arguments and `packet_type` packet-load contexts.
-/
inductive ContextObjSpec where
  | structCtx (id : MemoryId)
  | memCtx (id : MemoryId) (origin : MemoryOrigin) (bufSize : BitVec 64)
  | arrayCtx (id : MemoryId)
  | endArrayCtx (id : MemoryId)
  | mapCtx (schemaIdx : Nat)
  deriving DecidableEq

/--
**Informal**: Checks whether runtime value `v` is the unmodified non-releasable
entry capability described by `ContextObjSpec`.
-/
def ContextObjSpec.matchesVal : ContextObjSpec → RuntimeVal → Bool
  | .structCtx id, .structPtr id' none 0#64 => id' == id
  | .memCtx id orig sz, .memPtr id' orig' none sz' 0#64 =>
      id' == id && orig' == orig && sz' == sz
  | .arrayCtx id, .arrayPtr id' 0#64 => id' == id
  | .endArrayCtx id, .endArrayPtr id' => id' == id
  | .mapCtx idx, .mapPtr idx' => idx' == idx
  | _, _ => false

/--
**Informal**: Map-type allow/deny filter for `ConstPtrToMapParameter` helper
arguments.
-/
inductive HelperMapFilter where
  | allowList (types : List (BitVec 32))
  | denyList (types : List (BitVec 32))
  deriving DecidableEq, Repr

/--
**Informal**: Evaluates whether `mapType` is accepted by `HelperMapFilter`.
-/
def HelperMapFilter.allows : HelperMapFilter → BitVec 32 → Bool
  | .allowList ts, t => ts.contains t
  | .denyList ts, t  => !ts.contains t

/--
**Informal**: Buffer byte-length specification for `MemoryParameter`: either a
constant byte count or the scalar value passed in 0-based helper argument
`argIdx`.
-/
inductive HelperMemSize where
  | fixed (bytes : BitVec 64)
  | fromArg (argIdx : Nat)
  deriving DecidableEq, Repr

/--
**Informal**: Precondition contract for a single parameter of an external eBPF
helper function, covering every parameter variant supported by the Starnix eBPF
verifier (`ScalarValueParameter`, `ConstPtrToMapParameter`, `MapKeyParameter`,
`MapValueParameter`, `MemoryParameter`, `StructParameter`, `ContextParameter`,
`ReleasableParameter`, `ReleaseParameter`, `NullOrParameter`, and
`AnyParameter`).
-/
inductive HelperParamSpec where
  | scalar
  | constPtrToMap (filter : HelperMapFilter) (mustBeWritable : Bool)
  | mapKey (mapArgIdx : Nat)
  | mapValue (mapArgIdx : Nat)
  | memory (size : HelperMemSize) (input : Bool) (output : Bool)
  | structArg (schemaId : MemoryId)
  | contextArg (argIdx : Nat)
  | releasableArg (schemaId : MemoryId) (inner : HelperParamSpec)
  | releaseArg (schemaId : MemoryId)
  | nullOr (inner : HelperParamSpec)
  | anyArg
  deriving DecidableEq

/--
**Informal**: Underlying pointer kind returned in `r0` by a pointer-returning
eBPF helper function (map value lookup, dynamically or statically sized memory
allocation, or context/socket struct pointer).
-/
inductive HelperPtrReturnKind where
  | mapValue (mapArgIdx : Nat)
  | allocMemory (size : HelperMemSize)
  | staticMemory (schemaId : MemoryId) (bufSize : BitVec 64) (origin : MemoryOrigin)
  | staticStruct (schemaId : MemoryId)
  deriving DecidableEq

/--
**Informal**: Postcondition contract for the return value (`r0`) of an external
eBPF helper function: either an initialized/uninitialized scalar (`void` helpers
leave `r0` uninitialized) or a (possibly nullable, possibly releasable) pointer.
-/
inductive HelperReturnSpec where
  | scalar (initialized : Bool)
  | pointer (nullable : Bool) (releasableSchema : Option MemoryId) (inner : HelperPtrReturnKind)
  deriving DecidableEq

/--
**Informal**: Complete signature and side-effect specification of a registered
external eBPF helper function.
-/
structure HelperSpec where
  args : List HelperParamSpec
  ret : HelperReturnSpec
  invalidateArrayBounds : Bool
  deriving DecidableEq

/--
**Informal**: Host execution environment (`ExecEnv`) providing map schemas,
context struct layouts, entry context capabilities, registered helper
specifications, pointer-leak policy, and deterministic step-indexed oracles for
helper outputs and legacy packet loads.
-/
structure ExecEnv where
  maps : Nat → Option MapSpec
  structFields : MemoryId → List StructFieldDesc
  entryContextArgs : Nat → Option ContextObjSpec
  packetCtx : Option ContextObjSpec
  allowPtrLeaks : Bool
  helpers : Nat → Option HelperSpec
  helperScalarOracle : Nat → Nat → BitVec 64
  helperNullOracle : Nat → Nat → Bool
  helperBufWriteOracle : Nat → Nat → BitVec 64 → BitVec 8
  helperArrayLenOracle : Nat → MemoryId → BitVec 64
  packetOracle : Nat → BitVec 32 → DataWidth → Option (BitVec 64)

/--
**Informal**: Observable external state (`WorldState`) tracked during eBPF
execution: contents of sized memory buffers and context structs, read-only
nullable struct-field nullness bits, dynamic packet/array lengths, step counter,
currently live unreleased `Releasable` resources, and fresh runtime allocation
counter.
-/
structure WorldState where
  bufBytes : MemoryId → BitVec 64 → BitVec 8
  structBytes : MemoryId → BitVec 64 → BitVec 8
  structFieldNull : MemoryId → Bool
  arrayLen : MemoryId → BitVec 64
  stepCount : Nat
  liveResources : List MemoryId
  nextAllocId : Nat

/--
**Informal**: Complete instantaneous state (`MachineState`) of the eBPF virtual
machine: program counter `pc`, architectural registers `regs`, 512-byte stack
`stack`, and observable external `world` state.
-/
structure MachineState where
  pc : Nat
  regs : Nat → RuntimeVal
  stack : StackMem
  world : WorldState

/--
**Informal**: Well-formedness of a `StructDescriptor` supplied by Starnix: no
field carries an internal `Verification` `MemoryId` and `env.structFields`
exposes the exact field list of `sd` for every instance matching `schemaId`.
-/
def StructDescConforms (env : ExecEnv) (schemaId : MemoryId)
    (sd : ebpf_core.verifier.StructDescriptor) : Prop :=
  memoryIdNoVerif schemaId = true ∧
  (∀ fd ∈ sd.fields.val,
    match fd.field_type with
    | .Scalar _ | .MutableScalar _ => True
    | .PtrToMemory _ id _ | .NullablePtrToMemory _ id _
    | .PtrToArray _ id | .PtrToEndArray _ id => memoryIdNoVerif id = true) ∧
  (∀ actualId, memoryIdMatches schemaId actualId = true →
    env.structFields actualId = sd.fields.val.map StructFieldDesc.ofCore)

/--
**Informal**: Exact correspondence between an Aeneas-extracted verifier
parameter `Type` and its specification `HelperParamSpec` in execution
environment `env` (including that `ContextParameter` targets a pointer context
argument in `env.entryContextArgs`).
-/
inductive ParamTypeMatches (env : ExecEnv) : ebpf_core.verifier.Type → HelperParamSpec → Prop where
  | scalar :
      ParamTypeMatches env .ScalarValueParameter .scalar
  | constMapAllow (ts : Aeneas.Std.alloc.vec.Vec Aeneas.Std.U32) (w : Bool) :
      ParamTypeMatches env
        (.ConstPtrToMapParameter (.AllowList ts) w)
        (.constPtrToMap (.allowList (ts.val.map (·.bv))) w)
  | constMapDeny (ts : Aeneas.Std.alloc.vec.Vec Aeneas.Std.U32) (w : Bool) :
      ParamTypeMatches env
        (.ConstPtrToMapParameter (.DenyList ts) w)
        (.constPtrToMap (.denyList (ts.val.map (·.bv))) w)
  | mapKey (idx : Aeneas.Std.U8) :
      ParamTypeMatches env (.MapKeyParameter idx) (.mapKey idx.val)
  | mapValue (idx : Aeneas.Std.U8) :
      ParamTypeMatches env (.MapValueParameter idx) (.mapValue idx.val)
  | memFixed (sz : Aeneas.Std.U64) (inp out : Bool) :
      ParamTypeMatches env
        (.MemoryParameter (.Value sz) inp out)
        (.memory (.fixed sz.bv) inp out)
  | memRef (idx : Aeneas.Std.U8) (inp out : Bool) :
      ParamTypeMatches env
        (.MemoryParameter (.Reference idx) inp out)
        (.memory (.fromArg idx.val) inp out)
  | structArg (id : MemoryId) (h : memoryIdNoVerif id = true) :
      ParamTypeMatches env (.StructParameter id) (.structArg id)
  | contextArg (idx : Aeneas.Std.U8) (h : (env.entryContextArgs idx.val).isSome = true) :
      ParamTypeMatches env (.ContextParameter idx) (.contextArg idx.val)
  | releasableArg (id : MemoryId) (innerT : ebpf_core.verifier.Type) (innerS : HelperParamSpec)
      (hid : memoryIdNoVerif id = true)
      (hinner : ParamTypeMatches env innerT innerS) :
      ParamTypeMatches env (.ReleasableParameter id innerT) (.releasableArg id innerS)
  | releaseArg (id : MemoryId) (hid : memoryIdNoVerif id = true) :
      ParamTypeMatches env (.ReleaseParameter id) (.releaseArg id)
  | nullOr (innerT : ebpf_core.verifier.Type) (innerS : HelperParamSpec)
      (hinner : ParamTypeMatches env innerT innerS) :
      ParamTypeMatches env (.NullOrParameter innerT) (.nullOr innerS)
  | anyArg :
      ParamTypeMatches env .AnyParameter .anyArg

/--
**Informal**: Exact correspondence between a non-null non-releasable pointer
return `Type` in `FunctionSignature.return_value` and `HelperPtrReturnKind`.
-/
def PtrReturnTypeMatches (env : ExecEnv) :
    ebpf_core.verifier.Type → HelperPtrReturnKind → Prop
  | .MapValueParameter idx, .mapValue idx' =>
      idx.val = idx'
  | .MemoryParameter (.Value sz) _ _, .allocMemory (.fixed sz') =>
      sz.bv = sz'
  | .MemoryParameter (.Reference idx) _ _, .allocMemory (.fromArg idx') =>
      idx.val = idx'
  | .PtrToMemory id off sz orig, .staticMemory id' sz' orig' =>
      id = id' ∧ memoryIdNoVerif id = true ∧
      off.value.bv = 0#64 ∧ off.unknown_mask.bv = 0#64 ∧ off.unwritten_mask.bv = 0#64 ∧
      off.urange.min.bv = 0#64 ∧ off.urange.max.bv = 0#64 ∧
      sz.bv = sz' ∧ MemoryOrigin.ofCore orig = orig'
  | .PtrToStruct id off sd, .staticStruct id' =>
      id = id' ∧ StructDescConforms env id sd ∧
      off.value.bv = 0#64 ∧ off.unknown_mask.bv = 0#64 ∧ off.unwritten_mask.bv = 0#64 ∧
      off.urange.min.bv = 0#64 ∧ off.urange.max.bv = 0#64
  | _, _ => False

/--
**Informal**: Exact correspondence between an Aeneas-extracted
`FunctionSignature.return_value` and `HelperReturnSpec` (accounting for all
combinations permitted by `FunctionSignatureBuilder::build`).
-/
def ReturnTypeMatches (env : ExecEnv) :
    ebpf_core.verifier.Type → HelperReturnSpec → Prop
  | .ScalarValue d, .scalar true =>
      d.value.bv = 0#64 ∧ d.unknown_mask.bv = ~~~0#64 ∧ d.unwritten_mask.bv = 0#64 ∧
      d.urange.min.bv = 0#64 ∧ d.urange.max.bv = ~~~0#64
  | .ScalarValue d, .scalar false =>
      d.value.bv = 0#64 ∧ d.unknown_mask.bv = ~~~0#64 ∧ d.unwritten_mask.bv = ~~~0#64 ∧
      d.urange.min.bv = 0#64 ∧ d.urange.max.bv = ~~~0#64
  | .NullOrParameter (.ReleasableParameter relId innerT),
    .pointer true (some relId') innerS =>
      relId = relId' ∧ memoryIdNoVerif relId = true ∧
      PtrReturnTypeMatches env innerT innerS
  | .NullOrParameter innerT, .pointer true none innerS =>
      PtrReturnTypeMatches env innerT innerS
  | .ReleasableParameter relId innerT, .pointer false (some relId') innerS =>
      relId = relId' ∧ memoryIdNoVerif relId = true ∧
      PtrReturnTypeMatches env innerT innerS
  | innerT, .pointer false none innerS =>
      PtrReturnTypeMatches env innerT innerS
  | _, _ => False

/--
**Informal**: Structural well-formedness of a `HelperParamSpec` in a helper
signature of `numArgs` arguments (matching `Type::assert_valid_signature_type`
enforced by `FunctionSignatureBuilder::build`): any cross-argument map or
buffer-size reference targets an in-range parameter index `< numArgs`.
-/
def HelperParamIndexValid (numArgs : Nat) : HelperParamSpec → Prop
  | .mapKey idx | .mapValue idx => idx < numArgs
  | .memory (.fromArg idx) _ _ => idx < numArgs
  | .releasableArg _ inner | .nullOr inner => HelperParamIndexValid numArgs inner
  | _ => True

/--
**Informal**: Structural well-formedness of a `HelperReturnSpec` in a helper
signature of `numArgs` arguments (matching `Type::assert_valid_signature_type`
enforced by `FunctionSignatureBuilder::build`): any cross-argument map or
buffer-size reference targets an in-range parameter index `< numArgs`.
-/
def HelperReturnIndexValid (numArgs : Nat) : HelperReturnSpec → Prop
  | .pointer _ _ (.mapValue idx) => idx < numArgs
  | .pointer _ _ (.allocMemory (.fromArg idx)) => idx < numArgs
  | _ => True

/--
**Informal**: Correspondence between a verifier `FunctionSignature` and its
runtime `HelperSpec` in `ExecEnv` (including the parameter-index bounds enforced
by `FunctionSignatureBuilder::build`).
-/
def HelperSigMatches (env : ExecEnv)
    (sig : ebpf_core.verifier.FunctionSignature) (spec : HelperSpec) : Prop :=
  sig.args.val.length ≤ 5 ∧
  List.Forall₂ (ParamTypeMatches env) sig.args.val spec.args ∧
  (∀ p ∈ spec.args, HelperParamIndexValid spec.args.length p) ∧
  ReturnTypeMatches env sig.return_value spec.ret ∧
  HelperReturnIndexValid spec.args.length spec.ret ∧
  sig.invalidate_array_bounds = spec.invalidateArrayBounds

/--
**Informal**: Conformance between a verifier entry argument `Type` in
`cctx.args[i]`, the optional `ContextObjSpec` recorded at `env.entryContextArgs i`,
and the concrete entry register pair `(v₁, v₂)` in two executions.

Note: the initialized `.ScalarValue` branch requires `Type::UNKNOWN_SCALAR`
(`unknown_mask = ~~~0#64`, `urange = 0..=u64::MAX`), which corresponds to
`StaticTypeChecker` (`<usize as ProgramArgument>::get_type()`) rather than
`DynamicTypeChecker` (`ScalarValueData::from_known`).
-/
def EntryArgConforms (env : ExecEnv) (argType : ebpf_core.verifier.Type)
    (ctxObj? : Option ContextObjSpec) (v₁ v₂ : RuntimeVal) : Prop :=
  match argType with
  | .ScalarValue d =>
      ctxObj? = none ∧
      if d.unwritten_mask.bv == 0#64 then
        d.value.bv = 0#64 ∧ d.unknown_mask.bv = ~~~0#64 ∧
        d.urange.min.bv = 0#64 ∧ d.urange.max.bv = ~~~0#64 ∧
        ∃ x : BitVec 64, v₁ = .scalar x ∧ v₂ = .scalar x
      else
        d.value.bv = 0#64 ∧ d.unknown_mask.bv = ~~~0#64 ∧ d.unwritten_mask.bv = ~~~0#64 ∧
        d.urange.min.bv = 0#64 ∧ d.urange.max.bv = ~~~0#64 ∧
        ∃ x₁ x₂ : BitVec 64, v₁ = .scalar x₁ ∧ v₂ = .scalar x₂
  | .PtrToStruct id off sd =>
      StructDescConforms env id sd ∧
      off.value.bv = 0#64 ∧ off.unknown_mask.bv = 0#64 ∧ off.unwritten_mask.bv = 0#64 ∧
      off.urange.min.bv = 0#64 ∧ off.urange.max.bv = 0#64 ∧
      ctxObj? = some (.structCtx id) ∧
      v₁ = .structPtr id none 0#64 ∧ v₂ = .structPtr id none 0#64
  | .PtrToMemory id off sz orig =>
      memoryIdNoVerif id = true ∧
      off.value.bv = 0#64 ∧ off.unknown_mask.bv = 0#64 ∧ off.unwritten_mask.bv = 0#64 ∧
      off.urange.min.bv = 0#64 ∧ off.urange.max.bv = 0#64 ∧
      ctxObj? = some (.memCtx id (MemoryOrigin.ofCore orig) sz.bv) ∧
      v₁ = .memPtr id (MemoryOrigin.ofCore orig) none sz.bv 0#64 ∧
      v₂ = .memPtr id (MemoryOrigin.ofCore orig) none sz.bv 0#64
  | .PtrToArray id off =>
      memoryIdNoVerif id = true ∧
      off.value.bv = 0#64 ∧ off.unknown_mask.bv = 0#64 ∧ off.unwritten_mask.bv = 0#64 ∧
      off.urange.min.bv = 0#64 ∧ off.urange.max.bv = 0#64 ∧
      ctxObj? = some (.arrayCtx id) ∧
      v₁ = .arrayPtr id 0#64 ∧ v₂ = .arrayPtr id 0#64
  | .PtrToEndArray id =>
      memoryIdNoVerif id = true ∧
      ctxObj? = some (.endArrayCtx id) ∧
      v₁ = .endArrayPtr id ∧ v₂ = .endArrayPtr id
  | .ConstPtrToMap mapIdx schema =>
      env.maps mapIdx.bv.toNat = some (MapSpec.ofCore schema) ∧
      ctxObj? = some (.mapCtx mapIdx.bv.toNat) ∧
      v₁ = .mapPtr mapIdx.bv.toNat ∧ v₂ = .mapPtr mapIdx.bv.toNat
  | _ => False

/--
**Informal**: Correspondence between `cctx.packet_type` and `env.packetCtx`.
-/
def PacketTypeConforms :
    Option ebpf_core.verifier.Type → Option ContextObjSpec → Prop
  | none, none => True
  | some (.PtrToStruct id off _), some (.structCtx id') =>
      id = id' ∧ memoryIdNoVerif id = true ∧
      off.value.bv = 0#64 ∧ off.unknown_mask.bv = 0#64 ∧ off.unwritten_mask.bv = 0#64 ∧
      off.urange.min.bv = 0#64 ∧ off.urange.max.bv = 0#64
  | some (.PtrToMemory id off sz orig), some (.memCtx id' orig' sz') =>
      id = id' ∧ memoryIdNoVerif id = true ∧
      off.value.bv = 0#64 ∧ off.unknown_mask.bv = 0#64 ∧ off.unwritten_mask.bv = 0#64 ∧
      off.urange.min.bv = 0#64 ∧ off.urange.max.bv = 0#64 ∧
      MemoryOrigin.ofCore orig = orig' ∧ sz.bv = sz'
  | _, _ => False

/--
**Informal**: Caller entry-state assumption (`EntryStatesConform`): directly
connects `cctx : CoreCallingContext` to the runtime environment `env : ExecEnv`
and initial state constructor `mkInit`:
- `env.maps`, `env.helpers`, `env.packetCtx`, and `env.allowPtrLeaks` match the
  schemas, helper signatures, packet context type, and pointer-leak policy in
  `cctx`.
- Packet/array lengths are bounded by $2^{32} - 1$ (`0xFFFF_FFFF`).
- For any valid host layouts `layout₁, layout₂` and secrets `secret₁, secret₂`,
  `pc` starts at `0`, `r10` holds `stackPtr 512`, `r1`..`r5` conform to
  `cctx.args`, unused registers (`r0`, `r(args.len+1)..r9`) and all 64 stack
  slots start as uninitialized scalars, `liveResources` is empty, and
  `nextAllocId` starts at `0`.
-/
def EntryStatesConform (cctx : ebpf_core.verifier.CoreCallingContext) (env : ExecEnv)
    (mkInit : HostLayout → SecretInit → MachineState) : Prop :=
  env.allowPtrLeaks = cctx.allow_ptr_leaks ∧
  PacketTypeConforms cctx.packet_type env.packetCtx ∧
  (∀ idx, env.maps idx = cctx.maps.val[idx]?.map MapSpec.ofCore) ∧
  (∀ i, cctx.args.val.length ≤ i → env.entryContextArgs i = none) ∧
  (∀ helperIdx sig,
    (∃ k, cctx.helpers.val.find? (fun p => p.1.val == helperIdx) = some (k, sig)) →
    ∃ spec, env.helpers helperIdx = some spec ∧ HelperSigMatches env sig spec) ∧
  (∀ step id, env.helperArrayLenOracle step id ≤ 0xFFFF_FFFF#64) ∧
  (∀ layout₁ layout₂ secret₁ secret₂,
    ValidHostLayout layout₁ →
    ValidHostLayout layout₂ →
    let st₁ := mkInit layout₁ secret₁
    let st₂ := mkInit layout₂ secret₂
    st₁.pc = 0 ∧
    st₂.pc = 0 ∧
    st₁.world = st₂.world ∧
    st₁.world.liveResources = [] ∧
    st₁.world.nextAllocId = 0 ∧
    (∀ id, st₁.world.arrayLen id ≤ 0xFFFF_FFFF#64) ∧
    st₁.regs GENERAL_REGISTER_COUNT = .stackPtr 512#64 ∧
    st₂.regs GENERAL_REGISTER_COUNT = .stackPtr 512#64 ∧
    (∃ b₁ b₂, st₁.regs 0 = .scalar b₁ ∧ st₂.regs 0 = .scalar b₂) ∧
    (∀ i (hi : i < cctx.args.val.length),
      EntryArgConforms env cctx.args.val[i] (env.entryContextArgs i)
        (st₁.regs (i + 1)) (st₂.regs (i + 1))) ∧
    (∀ r, cctx.args.val.length < r → r < GENERAL_REGISTER_COUNT →
      ∃ b₁ b₂, st₁.regs r = .scalar b₁ ∧ st₂.regs r = .scalar b₂) ∧
    (∀ s, s < BPF_STACK_SLOTS →
      ∃ b₁ b₂, st₁.stack s = .bytes b₁ 0#64 ∧ st₂.stack s = .bytes b₂ 0#64))

/--
**Informal**: External logger assumption (`ValidVerifierLogger`): any host
implementation of `CoreVerifierLogger` returns without panicking on `is_enabled`,
`log_instruction`, and `log_error` calls.
-/
def ValidVerifierLogger {L : Type} (inst : ebpf_core.verifier.CoreVerifierLogger L) : Prop :=
  (∀ l, ∃ b, inst.is_enabled l = Aeneas.Std.Result.ok b) ∧
  (∀ l pc depth event, ∃ l', inst.log_instruction l pc depth event = Aeneas.Std.Result.ok l') ∧
  (∀ l err, ∃ l', inst.log_error l err = Aeneas.Std.Result.ok l')

/--
**Informal**: Register correspondence (`InterpreterRegsMatch`): the first 11
slots (`r0`..`r10`) of `InterpreterState.registers` hold the 64-bit concrete
representations of `st.regs 0`..`st.regs 10` under host address layout `layout`.
-/
def InterpreterRegsMatch (layout : HostLayout) (st : MachineState)
    (regs : Aeneas.Std.Array Aeneas.Std.U64 16#usize) : Prop :=
  ∀ r (_hr : r < REGISTER_COUNT) (h16 : r < regs.val.length),
    (regs.val[r]'h16).bv = (st.regs r).toBits layout st.world.arrayLen

end StarnixEbpf.Assumptions
