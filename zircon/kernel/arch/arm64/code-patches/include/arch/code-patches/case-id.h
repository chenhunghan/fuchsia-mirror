// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_ARM64_CODE_PATCHES_INCLUDE_ARCH_CODE_PATCHES_CASE_ID_H_
#define ZIRCON_KERNEL_ARCH_ARM64_CODE_PATCHES_INCLUDE_ARCH_CODE_PATCHES_CASE_ID_H_

// Each macro calls M(Enumerator, name) for one axis of variation among the
// __aarch64_{op}{size}_{order} functions that the compiler calls for
// -moutline-atomics.  Every op has every order with sizes 1, 2, 4, and 8; only
// cas also has size 16.
#define ARM64_OUTLINE_ATOMIC_OPS(M) \
  M(Cas, cas)                       \
  M(Swp, swp)                       \
  M(Ldadd, ldadd)                   \
  M(Ldclr, ldclr)                   \
  M(Ldeor, ldeor)                   \
  M(Ldset, ldset)

#define ARM64_OUTLINE_ATOMIC_SIZES(M) \
  M(1, 1)                             \
  M(2, 2)                             \
  M(4, 4)                             \
  M(8, 8)                             \
  M(16, 16)

#define ARM64_OUTLINE_ATOMIC_ORDERS(M) \
  M(Relax, relax)                      \
  M(Acq, acq)                          \
  M(Rel, rel)                          \
  M(AcqRel, acq_rel)

// Preprocessor counterparts of the Arm64OutlineAtomic* enum values below, for
// ARM64_OUTLINE_ATOMIC_CASE_ID() in assembly.  static_asserts below check that
// these match the enums.
#define ARM64_OUTLINE_ATOMIC_DETAIL_COUNT_ONE(Name, name) +1

// NOLINTBEGIN(modernize-macro-to-enum)
#define ARM64_OUTLINE_ATOMIC_DETAIL_OP_cas 0
#define ARM64_OUTLINE_ATOMIC_DETAIL_OP_swp 1
#define ARM64_OUTLINE_ATOMIC_DETAIL_OP_ldadd 2
#define ARM64_OUTLINE_ATOMIC_DETAIL_OP_ldclr 3
#define ARM64_OUTLINE_ATOMIC_DETAIL_OP_ldeor 4
#define ARM64_OUTLINE_ATOMIC_DETAIL_OP_ldset 5
#define ARM64_OUTLINE_ATOMIC_DETAIL_OPS \
  (0 ARM64_OUTLINE_ATOMIC_OPS(ARM64_OUTLINE_ATOMIC_DETAIL_COUNT_ONE))

#define ARM64_OUTLINE_ATOMIC_DETAIL_SIZE_1 0
#define ARM64_OUTLINE_ATOMIC_DETAIL_SIZE_2 1
#define ARM64_OUTLINE_ATOMIC_DETAIL_SIZE_4 2
#define ARM64_OUTLINE_ATOMIC_DETAIL_SIZE_8 3
#define ARM64_OUTLINE_ATOMIC_DETAIL_SIZE_16 4
#define ARM64_OUTLINE_ATOMIC_DETAIL_SIZES \
  (0 ARM64_OUTLINE_ATOMIC_SIZES(ARM64_OUTLINE_ATOMIC_DETAIL_COUNT_ONE))

#define ARM64_OUTLINE_ATOMIC_DETAIL_ORDER_relax 0
#define ARM64_OUTLINE_ATOMIC_DETAIL_ORDER_acq 1
#define ARM64_OUTLINE_ATOMIC_DETAIL_ORDER_rel 2
#define ARM64_OUTLINE_ATOMIC_DETAIL_ORDER_acq_rel 3
#define ARM64_OUTLINE_ATOMIC_DETAIL_ORDERS \
  (0 ARM64_OUTLINE_ATOMIC_ORDERS(ARM64_OUTLINE_ATOMIC_DETAIL_COUNT_ONE))
// NOLINTEND(modernize-macro-to-enum)

// Since cas is the first op and 16 is the last size, this numbers the
// functions densely with the four cas16 functions last.
#define ARM64_OUTLINE_ATOMIC_DETAIL_INDEX(op, size, order) \
  ((order) +                                               \
   (ARM64_OUTLINE_ATOMIC_DETAIL_ORDERS * ((op) + (ARM64_OUTLINE_ATOMIC_DETAIL_OPS * (size)))))

// Given `op, size, order` (or a single macro expanding to that, such as
// OUTLINE_ATOMIC in //zircon/kernel/arch/arm64/outline-atomics/BUILD.gn),
// yield the CodePatchId / CASE_ID_* value for __aarch64_{op}{size}_{order}.
#define ARM64_OUTLINE_ATOMIC_DETAIL_CASE_ID(op, size, order)                  \
  (CASE_ID_OUTLINE_ATOMICS_FIRST +                                            \
   ARM64_OUTLINE_ATOMIC_DETAIL_INDEX(ARM64_OUTLINE_ATOMIC_DETAIL_OP_##op,     \
                                     ARM64_OUTLINE_ATOMIC_DETAIL_SIZE_##size, \
                                     ARM64_OUTLINE_ATOMIC_DETAIL_ORDER_##order))
#define ARM64_OUTLINE_ATOMIC_CASE_ID(...) ARM64_OUTLINE_ATOMIC_DETAIL_CASE_ID(__VA_ARGS__)

#ifdef __ASSEMBLER__

#include <arch/code-patches/case-id-asm.h>

#else  // !__ASSEMBLER__

// Note: //zircon/kernel/lib/code-patching/gen-case-id-asm.cc includes this
// header inside a namespace, so do not #include C++ headers here.
#include <stdint.h>

#define ARM64_OUTLINE_ATOMIC_DETAIL_ENUM(Name, name) k##Name,

enum class Arm64OutlineAtomicOp : uint8_t {
  ARM64_OUTLINE_ATOMIC_OPS(ARM64_OUTLINE_ATOMIC_DETAIL_ENUM)
};
enum class Arm64OutlineAtomicSize : uint8_t {
  ARM64_OUTLINE_ATOMIC_SIZES(ARM64_OUTLINE_ATOMIC_DETAIL_ENUM)
};
enum class Arm64OutlineAtomicOrder : uint8_t {
  ARM64_OUTLINE_ATOMIC_ORDERS(ARM64_OUTLINE_ATOMIC_DETAIL_ENUM)
};

#undef ARM64_OUTLINE_ATOMIC_DETAIL_ENUM

#define ARM64_OUTLINE_ATOMIC_DETAIL_CHECK_OP(Name, name)                \
  static_assert(static_cast<uint32_t>(Arm64OutlineAtomicOp::k##Name) == \
                ARM64_OUTLINE_ATOMIC_DETAIL_OP_##name);
#define ARM64_OUTLINE_ATOMIC_DETAIL_CHECK_SIZE(Name, name)                \
  static_assert(static_cast<uint32_t>(Arm64OutlineAtomicSize::k##Name) == \
                ARM64_OUTLINE_ATOMIC_DETAIL_SIZE_##name);
#define ARM64_OUTLINE_ATOMIC_DETAIL_CHECK_ORDER(Name, name)                \
  static_assert(static_cast<uint32_t>(Arm64OutlineAtomicOrder::k##Name) == \
                ARM64_OUTLINE_ATOMIC_DETAIL_ORDER_##name);

ARM64_OUTLINE_ATOMIC_OPS(ARM64_OUTLINE_ATOMIC_DETAIL_CHECK_OP)
ARM64_OUTLINE_ATOMIC_SIZES(ARM64_OUTLINE_ATOMIC_DETAIL_CHECK_SIZE)
ARM64_OUTLINE_ATOMIC_ORDERS(ARM64_OUTLINE_ATOMIC_DETAIL_CHECK_ORDER)

#undef ARM64_OUTLINE_ATOMIC_DETAIL_CHECK_OP
#undef ARM64_OUTLINE_ATOMIC_DETAIL_CHECK_SIZE
#undef ARM64_OUTLINE_ATOMIC_DETAIL_CHECK_ORDER

inline constexpr uint32_t kArm64OutlineAtomicOps = ARM64_OUTLINE_ATOMIC_DETAIL_OPS;
inline constexpr uint32_t kArm64OutlineAtomicSizes = ARM64_OUTLINE_ATOMIC_DETAIL_SIZES;
inline constexpr uint32_t kArm64OutlineAtomicOrders = ARM64_OUTLINE_ATOMIC_DETAIL_ORDERS;

constexpr uint32_t Arm64OutlineAtomicIndex(Arm64OutlineAtomicOp op, Arm64OutlineAtomicSize size,
                                           Arm64OutlineAtomicOrder order) {
  return ARM64_OUTLINE_ATOMIC_DETAIL_INDEX(static_cast<uint32_t>(op), static_cast<uint32_t>(size),
                                           static_cast<uint32_t>(order));
}

inline constexpr uint32_t kArm64OutlineAtomicCount =
    Arm64OutlineAtomicIndex(Arm64OutlineAtomicOp::kCas, Arm64OutlineAtomicSize::k16,
                            Arm64OutlineAtomicOrder::kAcqRel) +
    1;
static_assert(kArm64OutlineAtomicCount ==
              ((((kArm64OutlineAtomicSizes - 1) * kArm64OutlineAtomicOps) + 1) *
               kArm64OutlineAtomicOrders));

// Defines known code-patching case IDs for the kernel.
// Each should be listed below in CodePatchNames as well.
enum class CodePatchId : uint32_t {
  // This case serves as a verification that code-patching was performed before
  // the kernel was booted, `nop`ing out a trap among the kernel's earliest
  // instructions.
  kSelfTest,

  // The patched area is the one instruction that acts as the SMCCC conduit.
  // It is initially `smc #0` but may be replaced with `hvc #0`.
  kSmcccConduit,

  // The patched area is a single `mov w0, #...` instruction.  It gets patched
  // with the SMCCC function number used for SMCCC_ARCH_WORKAROUND_3.
  kSmcccWorkaroundFunction,

  // Each __aarch64_* function for -moutline-atomics is a stub patched with
  // its "lse/...", "lse-a55/...", or "llsc/..." alternative.  See above.
  kOutlineAtomicsFirst,
  kOutlineAtomicsLast = kOutlineAtomicsFirst + kArm64OutlineAtomicCount - 1,
};

// The callback accepts an initializer-list of something constructible with
// {CodePatchId, std::string_view} and gets a list mapping kFooBar -> "FOO_BAR"
// name strings.  The names should be the kFooBar -> FOO_BAR transliteration of
// the enum names. In assembly code, these will be used as "CASE_ID_FOO_BAR".
inline constexpr auto WithCodePatchNames = [](auto&& callback) {
  return callback({
      {CodePatchId::kSelfTest, "SELF_TEST"},
      {CodePatchId::kSmcccConduit, "SMCCC_CONDUIT"},
      {CodePatchId::kSmcccWorkaroundFunction, "SMCCC_WORKAROUND_FUNCTION"},
      {CodePatchId::kOutlineAtomicsFirst, "OUTLINE_ATOMICS_FIRST"},
      {CodePatchId::kOutlineAtomicsLast, "OUTLINE_ATOMICS_LAST"},
  });
};

#endif  // __ASSEMBLER__

#endif  // ZIRCON_KERNEL_ARCH_ARM64_CODE_PATCHES_INCLUDE_ARCH_CODE_PATCHES_CASE_ID_H_
