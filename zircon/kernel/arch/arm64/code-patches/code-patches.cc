// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/arch/arm64/smccc.h>
#include <lib/code-patching/code-patches.h>
#include <zircon/assert.h>

#include <cstdint>

#include <arch/code-patches/case-id.h>
#include <ktl/algorithm.h>
#include <ktl/array.h>
#include <ktl/span.h>
#include <ktl/string_view.h>
#include <phys/handoff/handoff.h>
#include <phys/runtime/arm64/info.h>
#include <phys/runtime/symbolize.h>

#include "../smccc.h"

namespace {

// This yields the pieces of the alternative name for an -moutline-atomics
// function, e.g. "lse/__aarch64_cas1_relax" for CodePatchId::kOutlineAtomicsFirst.
// See <arch/code-patches/case-id.h> for how the case IDs are assigned.
constexpr ktl::array<ktl::string_view, 6> OutlineAtomicNamePieces(CodePatchId case_id,
                                                                  Arm64Atomics atomics) {
#define TABLE_ENTRY(Name, name) #name##sv,
  constexpr ktl::array kOps = {ARM64_OUTLINE_ATOMIC_OPS(TABLE_ENTRY)};
  constexpr ktl::array kSizes = {ARM64_OUTLINE_ATOMIC_SIZES(TABLE_ENTRY)};
  constexpr ktl::array kOrders = {ARM64_OUTLINE_ATOMIC_ORDERS(TABLE_ENTRY)};
#undef TABLE_ENTRY
  static_assert(kOps.size() == kArm64OutlineAtomicOps);
  static_assert(kSizes.size() == kArm64OutlineAtomicSizes);
  static_assert(kOrders.size() == kArm64OutlineAtomicOrders);

  uint32_t index =
      static_cast<uint32_t>(case_id) - static_cast<uint32_t>(CodePatchId::kOutlineAtomicsFirst);
  ZX_DEBUG_ASSERT(index < kArm64OutlineAtomicCount);
  const ktl::string_view order = kOrders[index % kOrders.size()];
  index /= kOrders.size();
  const ktl::string_view op = kOps[index % kOps.size()];
  const ktl::string_view size = kSizes[index / kOps.size()];
  return {Arm64AtomicsName(atomics), "/__aarch64_"sv, op, size, "_"sv, order};
}

consteval size_t MaxOutlineAtomicAlternativeSize() {
  size_t max_size = 0;
  for (Arm64Atomics atomics : {Arm64Atomics::kLlsc, Arm64Atomics::kLse, Arm64Atomics::kLseA55}) {
    for (uint32_t i = 0; i < kArm64OutlineAtomicCount; ++i) {
      const auto id =
          static_cast<CodePatchId>(static_cast<uint32_t>(CodePatchId::kOutlineAtomicsFirst) + i);
      size_t size = 0;
      for (ktl::string_view piece : OutlineAtomicNamePieces(id, atomics)) {
        size += piece.size();
      }
      max_size = ktl::max(max_size, size);
    }
  }
  return max_size;
}

using OutlineAtomicNameBuffer = ktl::array<char, MaxOutlineAtomicAlternativeSize()>;

ktl::string_view OutlineAtomicAlternative(CodePatchId case_id, Arm64Atomics atomics,
                                          OutlineAtomicNameBuffer& buffer) {
  auto out = buffer.begin();
  for (ktl::string_view piece : OutlineAtomicNamePieces(case_id, atomics)) {
    ZX_DEBUG_ASSERT(piece.size() <= static_cast<size_t>(buffer.end() - out));
    out = ktl::copy(piece.begin(), piece.end(), out);
  }
  return {buffer.data(), static_cast<size_t>(out - buffer.begin())};
}

}  // namespace

// Declared in <lib/code-patching/code-patches.h>.
bool ArchPatchCode(code_patching::Patcher& patcher, const ArchPatchInfo& info,
                   ktl::span<ktl::byte> insns, CodePatchId case_id,
                   fit::inline_function<void(ktl::initializer_list<ktl::string_view>)> print) {
  switch (case_id) {
    case CodePatchId::kSelfTest:
      patcher.NopFill(insns);
      print({"'smoke test' trap patched"});
      return true;

    case CodePatchId::kSmcccConduit: {
      // This patches in the smccc_conduit macro defined in ../smccc.h.
      ZX_ASSERT(insns.size_bytes() == sizeof(arch::ArmSmcccConduit));
      auto& insn = *reinterpret_cast<arch::ArmSmcccConduit*>(insns.data());
      ZX_ASSERT(insn == arch::ArmSmcccConduit::kSmc);
      if (gArchPhysInfo->smccc_use_hvc) {
        insn = arch::ArmSmcccConduit::kHvc;
      }
      print({
          "SMCCC conduit patched to use "sv,
          gArchPhysInfo->smccc_use_hvc ? "HVC"sv : "SMC"sv,
      });
      return true;
    }

    case CodePatchId::kSmcccWorkaroundFunction: {
      // This patches in the smccc_workaround_function_w0 macro defined in
      // ../smccc.h, used in in the alternate vector table in ../exceptions.S.
      // The same SMCCC functions are available on all CPUs and the best one
      // available is determined by ArchPreparePatchInfo.  arm64_select_vbar in
      // ../src/arch.rs will determine whether a specific CPU needs to use the SMCCC
      // workaround at all.
      ZX_ASSERT(insns.size_bytes() == sizeof(uint32_t));
      uint32_t& insn = *reinterpret_cast<uint32_t*>(insns.data());
      ktl::string_view choice = "unused";
      switch (info.alternate_vbar) {
        case Arm64AlternateVbar::kArchWorkaround3:
          choice = "SMCCC_ARCH_WORKAROUND_3"sv;
          insn = kMovW0SmcccArchWorkaround3;
          break;
        case Arm64AlternateVbar::kArchWorkaround1:
          choice = "SMCCC_ARCH_WORKAROUND_1"sv;
          insn = kMovW0SmcccArchWorkaround1;
          break;
        case Arm64AlternateVbar::kPsciVersion:
          choice = "SMCCC 1.1 PSCI_VERSION"sv;
          insn = kMovW0PsciVersion;
          break;
        case Arm64AlternateVbar::kSmccc10:
          choice = "SMCCC 1.0 PSCI_VERSION"sv;
          insn = kMovW0PsciVersion;
          break;
        case Arm64AlternateVbar::kNone:
          // The code won't actually be used.  To ensure that, the unpatched
          // instruction is a fatal trap.
          break;
        case Arm64AlternateVbar::kAuto:
          ZX_PANIC("should have been decided already");
          break;
      }
      print({"CPU workaround SMCCC function is "sv, choice});
      return true;
    }

    case CodePatchId::kOutlineAtomicsFirst... CodePatchId::kOutlineAtomicsLast: {
      // See ArchPreparePatchInfo for how the choice is made.
      ZX_ASSERT(info.atomics != Arm64Atomics::kAuto);
      OutlineAtomicNameBuffer buffer;
      ktl::string_view alternative = OutlineAtomicAlternative(case_id, info.atomics, buffer);
      patcher.MandatoryPatchWithAlternative(insns, alternative);
      print({"using alternative \"", alternative, "\""});
      return true;
    }
  }
  return false;
}
