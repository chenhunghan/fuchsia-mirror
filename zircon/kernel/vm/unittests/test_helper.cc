// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "test_helper.h"

#include <lib/fit/defer.h>
#include <lib/page/size.h>

namespace vm_unittest {

zx_status_t make_partially_committed_pager_vmo(size_t num_pages, size_t committed_pages,
                                               bool trap_dirty, bool resizable,
                                               bool ignore_requests, vm_page_t** out_pages,
                                               fbl::RefPtr<VmObjectPaged>* out_vmo) {
  // Disable the scanner so we can safely submit our aux vmo and query pages without eviction
  // happening.
  AutoVmScannerDisable scanner_disable;

  // Create a pager backed VMO and jump through some hoops to pre-fill pages for it so we do not
  // actually take any page faults.
  fbl::AllocChecker ac;
  fbl::RefPtr<StubPageProvider> pager =
      fbl::MakeRefCountedChecked<StubPageProvider>(&ac, trap_dirty, ignore_requests);
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }

  fbl::RefPtr<PageSource> src = fbl::MakeRefCountedChecked<PageSource>(&ac, ktl::move(pager));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }

  fbl::RefPtr<VmObjectPaged> vmo;
  zx_status_t status = VmObjectPaged::CreateExternal(
      ktl::move(src), resizable ? VmObjectPaged::kResizable : 0, num_pages * kPageSize, &vmo);
  if (status != ZX_OK) {
    return status;
  }

  if (committed_pages > 0) {
    fbl::RefPtr<VmObjectPaged> aux_vmo;
    status = VmObjectPaged::Create(0, 0, committed_pages * kPageSize, &aux_vmo);
    if (status != ZX_OK) {
      return status;
    }

    status = aux_vmo->CommitRange(0, committed_pages * kPageSize);
    if (status != ZX_OK) {
      return status;
    }

    VmPageSpliceList splice_list;
    status = aux_vmo->TakePages(0, committed_pages * kPageSize, &splice_list);
    if (status != ZX_OK) {
      return status;
    }

    status =
        vmo->SupplyPages(0, committed_pages * kPageSize, &splice_list, SupplyOptions::PagerSupply);
    if (status != ZX_OK) {
      return status;
    }

    if (out_pages) {
      for (uint64_t i = 0; i < committed_pages; i++) {
        status = vmo->GetPage(i * kPageSize, 0, nullptr, &out_pages[i], nullptr);
        if (status != ZX_OK) {
          return status;
        }
      }
    }
  }

  *out_vmo = ktl::move(vmo);
  return ZX_OK;
}

zx_status_t make_committed_pager_vmo(size_t num_pages, bool trap_dirty, bool resizable,
                                     vm_page_t** out_pages, fbl::RefPtr<VmObjectPaged>* out_vmo) {
  return make_partially_committed_pager_vmo(num_pages, num_pages, trap_dirty, resizable, false,
                                            out_pages, out_vmo);
}

zx_status_t supply_pager_vmo_pages(VmObjectPaged* vmo, uint64_t page_offset, uint64_t num_pages,
                                   vm_page_t** out_pages) {
  // Disable the scanner so we can safely submit our aux vmo and query pages without eviction
  // happening.
  AutoVmScannerDisable scanner_disable;

  fbl::RefPtr<VmObjectPaged> aux_vmo;
  zx_status_t status = VmObjectPaged::Create(0, 0, num_pages * kPageSize, &aux_vmo);
  if (status != ZX_OK) {
    return status;
  }

  status = aux_vmo->CommitRange(0, num_pages * kPageSize);
  if (status != ZX_OK) {
    return status;
  }

  VmPageSpliceList splice_list;
  status = aux_vmo->TakePages(0, num_pages * kPageSize, &splice_list);
  if (status != ZX_OK) {
    return status;
  }

  status = vmo->SupplyPages(page_offset * kPageSize, num_pages * kPageSize, &splice_list,
                            SupplyOptions::PagerSupply);
  if (status != ZX_OK) {
    return status;
  }

  if (out_pages) {
    for (uint64_t i = 0; i < num_pages; i++) {
      status = vmo->GetPage((page_offset + i) * kPageSize, 0, nullptr, &out_pages[i], nullptr);
      if (status != ZX_OK) {
        return status;
      }
    }
  }

  return ZX_OK;
}

uint32_t test_rand(uint32_t seed) { return (seed = seed * 1664525 + 1013904223); }

// fill a region of memory with a pattern based on the address of the region
void fill_region(uintptr_t seed, void* _ptr, size_t len) {
  uint32_t* ptr = (uint32_t*)_ptr;

  ASSERT(IS_ROUNDED((uintptr_t)ptr, 4));

  uint32_t val = (uint32_t)seed;
  val ^= (uint32_t)(seed >> 32);
  for (size_t i = 0; i < len / 4; i++) {
    ptr[i] = val;

    val = test_rand(val);
  }
}

// test a region of memory against a known pattern
bool test_region(uintptr_t seed, void* _ptr, size_t len) {
  uint32_t* ptr = (uint32_t*)_ptr;

  ASSERT(IS_ROUNDED((uintptr_t)ptr, 4));

  uint32_t val = (uint32_t)seed;
  val ^= (uint32_t)(seed >> 32);
  for (size_t i = 0; i < len / 4; i++) {
    if (ptr[i] != val) {
      unittest_printf("value at %p (%zu) is incorrect: 0x%x vs 0x%x\n", &ptr[i], i, ptr[i], val);
      return false;
    }

    val = test_rand(val);
  }

  return true;
}

// Helper function used by vm_mapping_page_fault_optimisation_test.
// Given a mapping, check that a run of consecutive pages are mapped (indicated by
// expected_mapped_page_count) and that remaining pages are unmapped.
bool verify_mapped_page_range(vaddr_t base, size_t mapping_size,
                              size_t expected_mapped_page_count) {
  BEGIN_TEST;

  // All pages up to expected_mapped_page_count should have been faulted.
  paddr_t paddr_writable;
  arch_mmu_flags_t mmu_flags;
  uint64_t offset = 0;
  for (; offset < expected_mapped_page_count * kPageSize; offset += kPageSize) {
    ASSERT_OK(Thread::Current::Get()->active_aspace()->arch_aspace().Query(
        base + offset, &paddr_writable, &mmu_flags));
    EXPECT_TRUE(mmu_flags & ARCH_MMU_FLAG_PERM_READ);
  }

  // Remaining pages should not have been faulted in.
  for (; offset < mapping_size; offset += kPageSize) {
    ASSERT_EQ(Thread::Current::Get()->active_aspace()->arch_aspace().Query(
                  base + offset, &paddr_writable, &mmu_flags),
              ZX_ERR_NOT_FOUND);
  }

  END_TEST;
}

bool verify_continuous_attribution_bytes(VmObject& vmo, uint64_t expected_bytes) {
  BEGIN_TEST;

  VmObjectPaged* paged = DownCastVmObject<VmObjectPaged>(&vmo);
  ASSERT_NONNULL(paged, "only paged VMOs have a continuously tracked populated bytes count");
  EXPECT_NE(VmObject::ChildType::kSlice, paged->child_type(),
            "slice VMOs do not have a continuously tracked populated bytes count");

  if constexpr (EXPERIMENTAL_CONTINUOUS_PER_VMO_ATTRIBUTION_ENABLED) {
    const uint64_t tracked_slots = paged->DebugGetCowPages()->DebugGetPopulatedSlotsCount();
    const uint64_t tracked_bytes = kPageSize * tracked_slots;
    EXPECT_EQ(expected_bytes, tracked_bytes);
  }

  END_TEST;
}

VmObject::AttributionCounts make_private_attribution_counts(uint64_t uncompressed,
                                                            uint64_t compressed) {
  return vm::AttributionCounts{
      .uncompressed_bytes = uncompressed,
      .compressed_bytes = compressed,
      .private_uncompressed_bytes = uncompressed,
      .private_compressed_bytes = compressed,
      .scaled_uncompressed_bytes = vm::FractionalBytes(uncompressed),
      .scaled_compressed_bytes = vm::FractionalBytes(compressed),
  };
}

void change_vmo_high_priority_count(VmObjectPaged& vmo, int64_t delta) {
  PriorityChanger pc = vmo.MakePriorityChanger(delta);
  if (delta > 0) {
    pc.PrepareMayNotAlreadyBeHighPriority();
  }
  Guard<CriticalMutex> guard{AliasedLock, vmo.lock(), pc.lock()};
  pc.ChangeHighPriorityCountLocked();
}

}  // namespace vm_unittest
