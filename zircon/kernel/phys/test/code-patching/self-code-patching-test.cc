// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <inttypes.h>
#include <lib/code-patching/code-patching.h>
#include <lib/memalloc/range.h>
#include <lib/zbitl/error-stdio.h>
#include <lib/zbitl/items/bootfs.h>
#include <lib/zbitl/view.h>
#include <stdio.h>

#include <fbl/alloc_checker.h>
#include <ktl/byte.h>
#include <ktl/span.h>
#include <ktl/string_view.h>
#include <ktl/utility.h>
#include <phys/address-space/address-space.h>
#include <phys/allocation/zbitl.h>
#include <phys/memory/init.h>
#include <phys/runtime/self.h>
#include <phys/runtime/symbolize.h>

#include "../test-main.h"
#include "test.h"

#include <ktl/enforce.h>

// The BOOTFS namespace under which code patching blobs live.
constexpr ktl::string_view kNamespace = "code-patches-test";

// Defined by //zircon/kernel/phys/test/code-patching:multiply_by_factor.
//
// Marked with cfi_unchecked_callee because this isn't really a C function.
// It's a block of memory that's patched with other ASM functions, so it won't
// have it's own CFI metadata.
extern "C" uint64_t multiply_by_factor(uint64_t x) [[clang::cfi_unchecked_callee]];

namespace {

using Bootfs = zbitl::Bootfs<ktl::span<ktl::byte>>;

constexpr size_t kExpectedNumPatches = 4;

// Returns the address range within this executable associated with a given
// link-time, virtual range.
ktl::span<ktl::byte> GetInstructionRange(uint64_t range_start, size_t range_size) {
  const size_t loaded_size = static_cast<size_t>(_end - PHYS_LOAD_ADDRESS);
  ktl::span<ktl::byte> loaded_range{PHYS_LOAD_ADDRESS, loaded_size};
  ZX_ASSERT(range_size <= loaded_range.size());
  ZX_ASSERT(range_start <= loaded_range.size() - range_size);
  return loaded_range.subspan(range_start, range_size);
}

int TestAddOnePatching(code_patching::PatcherWithGlobalCacheConsistency& patcher,
                       const code_patching::Directive& patch) {
  ZX_ASSERT_MSG(patch.range_size == kAddOnePatchSize,
                "Expected patch case #%u to cover %zu bytes; got %u", kAddOneCaseId,
                kAddOnePatchSize, patch.range_size);

  {
    uint64_t result = AddOne(583);
    ZX_ASSERT_MSG(result == 584, "AddOne(583) returned %lu; expected 584.\n", result);
  }

  // After patching (and synchronizing the instruction and data caches), we
  // expect AddOne() to be the identity function.
  auto insns = GetInstructionRange(patch.range_start, patch.range_size);
  patcher.NopFill(insns);
  patcher.Commit();

  {
    uint64_t result = AddOne(583);
    ZX_ASSERT_MSG(result == 583, "Patched AddOne(583) returned %lu; expected 583.\n", result);
  }
  return 0;
}

int TestMultiplyByFactorPatching(code_patching::PatcherWithGlobalCacheConsistency& patcher,
                                 const code_patching::Directive& patch) {
  ZX_ASSERT_MSG(patch.range_size == kMultiplyByFactorPatchSize,
                "Expected patch case #%u to cover %zu bytes; got %u", kMultiplyByFactorCaseId,
                kMultiplyByFactorPatchSize, patch.range_size);

  auto insns = GetInstructionRange(patch.range_start, patch.range_size);

  // After patching and synchronizing, we expect multiply_by_factor() to
  // multiply by 2.
  if (auto result = patcher.PatchWithAlternative(insns, "multiply_by_two"sv); result.is_error()) {
    printf("FAILED: ");
    zbitl::PrintBootfsError(result.error_value());
    return 1;
  }
  patcher.Commit();

  {
    uint64_t result = multiply_by_factor(583);
    ZX_ASSERT_MSG(result == 2 * 583, "multiply_by_factor(583) returned %lu; expected %d.\n", result,
                  2 * 583);
  }

  // After patching and synchronizing, we expect multiply_by_factor() to
  // multiply by ten.
  if (auto result = patcher.PatchWithAlternative(insns, "multiply_by_ten"sv); result.is_error()) {
    printf("FAILED: ");
    zbitl::PrintBootfsError(result.error_value());
    return 1;
  }
  patcher.Commit();

  {
    uint64_t result = multiply_by_factor(583);
    ZX_ASSERT_MSG(result == 10 * 583, "multiply_by_factor(583) returned %lu; expected %d.\n",
                  result, 10 * 583);
  }

  return 0;
}

int TestAddValuePatching(code_patching::PatcherWithGlobalCacheConsistency& patcher,
                         const code_patching::Directive& patch) {
  auto insns = GetInstructionRange(patch.range_start, patch.range_size);

  auto test_case = [&](ktl::string_view alternative, uint64_t addend) -> bool {
    if (auto result = patcher.PatchWithAlternative(insns, alternative); result.is_error()) {
      printf("FAILED (%.*s): ", static_cast<int>(alternative.size()), alternative.data());
      zbitl::PrintBootfsError(result.error_value());
      return false;
    }
    patcher.Commit();

    uint64_t result = add_value(583);
    if (result != 583 + addend) {
      printf("FAILED: add_value(583) [%.*s] returned %" PRIu64 "; expected %" PRIu64 ".\n",
             static_cast<int>(alternative.size()), alternative.data(), result, 583 + addend);
      return false;
    }
    return true;
  };

  bool ok = test_case("by-config/add_value"sv, 2);
  ok = test_case("add_value"sv, 3) && ok;
  ok = test_case("by-deps/add_ten"sv, 10) && ok;
  return ok ? 0 : 1;
}

// multiply_by_factor and multiply_by_factor_twin are identical except for
// their patch case IDs, so identical code folding could merge them into one.
// Then patching either would patch both.
int TestTwinPatching(code_patching::PatcherWithGlobalCacheConsistency& patcher,
                     const code_patching::Directive& patch,
                     const code_patching::Directive& twin_patch) {
  int failed = 0;
  if (patch.range_start == twin_patch.range_start) {
    printf("FAILED: multiply_by_factor and multiply_by_factor_twin both at %#x\n",
           patch.range_start);
    failed = 1;
  }

  auto insns = GetInstructionRange(patch.range_start, patch.range_size);
  auto twin_insns = GetInstructionRange(twin_patch.range_start, twin_patch.range_size);
  for (auto [range, alternative] : {
           ktl::pair{insns, "multiply_by_ten"sv},
           ktl::pair{twin_insns, "multiply_by_two"sv},
       }) {
    if (auto result = patcher.PatchWithAlternative(range, alternative); result.is_error()) {
      printf("FAILED: ");
      zbitl::PrintBootfsError(result.error_value());
      return 1;
    }
  }
  patcher.Commit();

  if (uint64_t result = multiply_by_factor(583); result != 10 * 583) {
    printf("FAILED: multiply_by_factor(583) returned %" PRIu64 "; expected %d.\n", result,
           10 * 583);
    failed = 1;
  }
  if (uint64_t result = multiply_by_factor_twin(583); result != 2 * 583) {
    printf("FAILED: multiply_by_factor_twin(583) returned %" PRIu64 "; expected %d.\n", result,
           2 * 583);
    failed = 1;
  }
  return failed;
}

}  // namespace

int TestMain(void* zbi_ptr, ktl::optional<EarlyBootZbi> early_zbi, arch::EarlyTicks) {
  Symbolize symbolize("code-patching-test");

  // Initialize memory for allocation/free.
  AddressSpace aspace;
  InitMemory(zbi_ptr, ktl::move(early_zbi), &aspace);

  zbitl::View zbi{zbitl::StorageFromRawHeader(static_cast<const zbi_header_t*>(zbi_ptr))};

  // Search for a payload of type ZBI_TYPE_STORAGE_KERNEL
  auto zbi_it = zbi.find(ZBI_TYPE_STORAGE_KERNEL);

  // Ensure there was no error during iteration.
  if (auto result = zbi.take_error(); result.is_error()) {
    printf("FAILED: Error while enumerating ZBI: ");
    zbitl::PrintViewError(result.error_value());
    return 1;
  }

  // Fail if we didn't find anything.
  if (zbi_it == zbi.end()) {
    printf("FAILED: No STORAGE_KERNEL item found.\n");
    return 1;
  }

  fbl::AllocChecker ac;
  const uint32_t bootfs_size = zbitl::UncompressedLength(*(zbi_it->header));
  auto bootfs_buffer = Allocation::New(ac, memalloc::Type::kKernelStorage, bootfs_size);
  if (!ac.check()) {
    printf("FAILED: Cannot allocate %#x bytes for decompressed STORAGE_KERNEL item!\n",
           bootfs_size);
    abort();
  }

  if (auto result = zbi.CopyStorageItem(bootfs_buffer.data(), zbi_it, ZbitlScratchAllocator);
      result.is_error()) {
    printf("FAILED: Cannot load STORAGE_KERNEL item (uncompressed size %#x): ", bootfs_size);
    zbitl::PrintViewCopyError(result.error_value());
    abort();
  }

  Bootfs bootfs_reader;
  if (auto result = Bootfs::Create(bootfs_buffer.data()); result.is_error()) {
    zbitl::PrintBootfsError(result.error_value());
    return 1;
  } else {
    bootfs_reader = ktl::move(result.value());
  }

  Bootfs::View bootfs;
  if (auto result = bootfs_reader.root().subdir(kNamespace); result.is_error()) {
    zbitl::PrintBootfsError(result.error_value());
    return 1;
  } else {
    bootfs = ktl::move(result).value();
  }

  code_patching::PatcherWithGlobalCacheConsistency patcher;
  if (auto result = patcher.Init(bootfs); result.is_error()) {
    printf("FAILED: Could not initialize code_patching::Patcher: ");
    zbitl::PrintBootfsError(result.error_value());
    return 1;
  }

  ktl::span<const code_patching::Directive> patches = patcher.patches();

  printf("Patches found:\n");
  printf("| %-4s | %-8s | %-8s | %-4s |\n", "ID", "Start", "End", "Size");
  for (const auto& patch : patches) {
    printf("| %-4u | %#-8x | %#-8x | %-4u |\n", patch.id, patch.range_start,
           patch.range_start + patch.range_size, patch.range_size);
  }

  if (patches.size() != kExpectedNumPatches) {
    printf("Expected %zu code patch directive: got %zu", kExpectedNumPatches, patches.size());
    return 1;
  }

  const code_patching::Directive* multiply_by_factor_patch = nullptr;
  const code_patching::Directive* multiply_by_factor_twin_patch = nullptr;
  for (const auto& patch : patches) {
    switch (patch.id) {
      case kAddOneCaseId:
        if (int result = TestAddOnePatching(patcher, patch); result != 0) {
          return result;
        }
        break;
      case kMultiplyByFactorCaseId:
        if (int result = TestMultiplyByFactorPatching(patcher, patch); result != 0) {
          return result;
        }
        multiply_by_factor_patch = &patch;
        break;
      case kMultiplyByFactorTwinCaseId:
        multiply_by_factor_twin_patch = &patch;
        break;
      case kAddValueCaseId:
        if (int result = TestAddValuePatching(patcher, patch); result != 0) {
          return result;
        }
        break;
      default:
        printf("Unexpected patch case ID: %u\n", patch.id);
        return 1;
    }
  }

  ZX_ASSERT(multiply_by_factor_patch);
  ZX_ASSERT(multiply_by_factor_twin_patch);
  return TestTwinPatching(patcher, *multiply_by_factor_patch, *multiply_by_factor_twin_patch);
}
