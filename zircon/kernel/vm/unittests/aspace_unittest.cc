// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/fit/defer.h>
#include <lib/page/size.h>
#include <pow2.h>
#include <zircon/errors.h>

#include <arch/kernel_aspace.h>
#include <vm/vm.h>

#include "test_helper.h"

#include <ktl/enforce.h>

namespace vm_unittest {

struct KernelRegion {
  const char* name;
  vaddr_t base;
  size_t size;
  arch_mmu_flags_t arch_mmu_flags;
};

const ktl::array kernel_regions = {
    KernelRegion{
        .name = "kernel_code",
        .base = (vaddr_t)__code_start,
        .size = RoundUpPageSize((uintptr_t)__code_end - (uintptr_t)__code_start),
        .arch_mmu_flags = ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_EXECUTE,
    },
    KernelRegion{
        .name = "kernel_rodata",
        .base = (vaddr_t)__rodata_start,
        .size = RoundUpPageSize((uintptr_t)__rodata_end - (uintptr_t)__rodata_start),
        .arch_mmu_flags = ARCH_MMU_FLAG_PERM_READ,
    },
    KernelRegion{
        .name = "kernel_relro",
        .base = (vaddr_t)__relro_start,
        .size = RoundUpPageSize((uintptr_t)__relro_end - (uintptr_t)__relro_start),
        .arch_mmu_flags = ARCH_MMU_FLAG_PERM_READ,
    },
    KernelRegion{
        .name = "kernel_data_bss",
        .base = (vaddr_t)__data_start,
        .size = RoundUpPageSize((uintptr_t)_end - (uintptr_t)__data_start),
        .arch_mmu_flags = ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
    },
};

// Tests that memory attribution works as expected in a nested aspace hierarchy.
static bool vmaspace_nested_attribution_test() {
  BEGIN_TEST;

  AutoVmScannerDisable scanner_disable;

  fbl::RefPtr<VmAspace> aspace = VmAspace::Create(VmAspace::Type::User, "test-aspace");
  ASSERT_NONNULL(aspace);

  // 8 page vmar.
  fbl::RefPtr<VmAddressRegion> vmar;
  ASSERT_OK(aspace->RootVmar()->CreateSubVmar(
      0, kPageSize * 8, 0,
      VMAR_FLAG_CAN_MAP_SPECIFIC | VMAR_FLAG_CAN_MAP_READ | VMAR_FLAG_CAN_MAP_WRITE, "test vmar",
      &vmar));

  // Child vmar that covers the first 4 pages of the previous vmar.
  fbl::RefPtr<VmAddressRegion> subvmar1;
  ASSERT_OK(vmar->CreateSubVmar(
      0, kPageSize * 4, 0,
      VMAR_FLAG_CAN_MAP_SPECIFIC | VMAR_FLAG_CAN_MAP_READ | VMAR_FLAG_CAN_MAP_WRITE, "test vmar",
      &subvmar1));

  // Grandchild vmar that covers the first 2 pages of the child.
  fbl::RefPtr<VmAddressRegion> subvmar2;
  ASSERT_OK(subvmar1->CreateSubVmar(
      0, kPageSize * 2, 0,
      VMAR_FLAG_CAN_MAP_SPECIFIC | VMAR_FLAG_CAN_MAP_READ | VMAR_FLAG_CAN_MAP_WRITE, "test vmar",
      &subvmar2));

  // Make 2 page vmo.
  fbl::RefPtr<VmObjectPaged> vmo;
  zx_status_t status =
      VmObjectPaged::Create(PMM_ALLOC_FLAG_ANY, VmObjectPaged::kResizable, 2 * kPageSize, &vmo);
  ASSERT_EQ(ZX_OK, status);

  // Map VMO to grandchild.
  EXPECT_EQ(aspace->is_user(), true);
  auto mapping_result =
      subvmar2->CreateVmMapping(0, 2 * kPageSize, 0, 0, vmo, 0, kArchRwUserFlags, "test-mapping");
  EXPECT_EQ(ZX_OK, mapping_result.status_value());
  fbl::RefPtr<VmMapping> mapping = ktl::move(mapping_result->mapping);

  // Commit 2 pages into mapping.
  status = vmo->CommitRange(0, 2 * kPageSize);
  ASSERT_EQ(ZX_OK, status);

  // Verify that the two pages are counted for the parent vmar chain.
  ASSERT_TRUE(make_private_attribution_counts(2ul * kPageSize, 0) ==
              mapping->GetAttributedMemory());
  ASSERT_TRUE(make_private_attribution_counts(2ul * kPageSize, 0) ==
              subvmar2->GetAttributedMemory());
  ASSERT_TRUE(make_private_attribution_counts(2ul * kPageSize, 0) ==
              subvmar1->GetAttributedMemory());
  ASSERT_TRUE(make_private_attribution_counts(2ul * kPageSize, 0) == vmar->GetAttributedMemory());

  END_TEST;
}

// Tests that memory attribution at the VmMapping layer behaves as expected under commits and
// decommits on the vmo range.
static bool vm_mapping_attribution_commit_decommit_test() {
  BEGIN_TEST;
  AutoVmScannerDisable scanner_disable;

  using AttributionCounts = VmObject::AttributionCounts;
  // Create a test VmAspace to temporarily switch to for creating test mappings.
  fbl::RefPtr<VmAspace> aspace = VmAspace::Create(VmAspace::Type::User, "test-aspace");
  ASSERT_NONNULL(aspace);

  // Create a VMO to map.
  fbl::RefPtr<VmObjectPaged> vmo;
  zx_status_t status =
      VmObjectPaged::Create(PMM_ALLOC_FLAG_ANY, VmObjectPaged::kResizable, 16 * kPageSize, &vmo);
  ASSERT_EQ(ZX_OK, status);

  EXPECT_TRUE(vmo->GetAttributedMemory() == AttributionCounts{});
  EXPECT_TRUE(verify_continuous_attribution_bytes(*vmo, 0));

  // Map the left half of the VMO.
  EXPECT_EQ(aspace->is_user(), true);
  auto mapping_result = aspace->RootVmar()->CreateVmMapping(0, 8 * kPageSize, 0, 0, vmo, 0,
                                                            kArchRwUserFlags, "test-mapping");
  EXPECT_EQ(ZX_OK, mapping_result.status_value());
  fbl::RefPtr<VmMapping> mapping = ktl::move(mapping_result->mapping);

  EXPECT_TRUE(vmo->GetAttributedMemory() == AttributionCounts{});
  EXPECT_TRUE(verify_continuous_attribution_bytes(*vmo, 0));
  EXPECT_TRUE(mapping->GetAttributedMemory() == AttributionCounts{});

  // Commit pages a little into the mapping, and past it.
  status = vmo->CommitRange(4 * kPageSize, 8 * kPageSize);
  ASSERT_EQ(ZX_OK, status);
  EXPECT_TRUE(vmo->GetAttributedMemory() == make_private_attribution_counts(8ul * kPageSize, 0));
  EXPECT_TRUE(mapping->GetAttributedMemory() ==
              make_private_attribution_counts(4ul * kPageSize, 0));

  // Decommit the pages committed above, returning the VMO to zero committed pages.
  status = vmo->DecommitRange(4 * kPageSize, 8 * kPageSize);
  ASSERT_EQ(ZX_OK, status);
  EXPECT_TRUE(vmo->GetAttributedMemory() == AttributionCounts{});
  EXPECT_TRUE(verify_continuous_attribution_bytes(*vmo, 0));
  EXPECT_TRUE(mapping->GetAttributedMemory() == AttributionCounts{});

  // Commit some pages in the VMO again.
  status = vmo->CommitRange(0, 10 * kPageSize);
  ASSERT_EQ(ZX_OK, status);
  EXPECT_TRUE(vmo->GetAttributedMemory() == make_private_attribution_counts(10ul * kPageSize, 0));
  EXPECT_TRUE(verify_continuous_attribution_bytes(*vmo, 10ul * kPageSize));
  EXPECT_TRUE(mapping->GetAttributedMemory() ==
              make_private_attribution_counts(8ul * kPageSize, 0));

  // Decommit pages in the vmo via the mapping.
  status = mapping->DecommitRange(0, mapping->size());
  ASSERT_EQ(ZX_OK, status);
  EXPECT_TRUE(vmo->GetAttributedMemory() == make_private_attribution_counts(2ul * kPageSize, 0));
  EXPECT_TRUE(verify_continuous_attribution_bytes(*vmo, 2ul * kPageSize));
  EXPECT_TRUE(mapping->GetAttributedMemory() == AttributionCounts{});

  // Destroy the mapping.
  status = mapping->Destroy();
  ASSERT_EQ(ZX_OK, status);
  EXPECT_TRUE(vmo->GetAttributedMemory() == make_private_attribution_counts(2ul * kPageSize, 0));
  EXPECT_TRUE(verify_continuous_attribution_bytes(*vmo, 2ul * kPageSize));
  EXPECT_TRUE((vm::AttributionCounts{}) == mapping->GetAttributedMemory());

  // Free the test address space.
  status = aspace->Destroy();
  EXPECT_EQ(ZX_OK, status);

  END_TEST;
}

// Tests that memory attribution at the VmMapping layer behaves as expected under map and unmap
// operations on the mapping.
static bool vm_mapping_attribution_map_unmap_test() {
  BEGIN_TEST;
  AutoVmScannerDisable scanner_disable;

  using AttributionCounts = VmObject::AttributionCounts;
  // Create a test VmAspace to temporarily switch to for creating test mappings.
  fbl::RefPtr<VmAspace> aspace = VmAspace::Create(VmAspace::Type::User, "test-aspace");
  ASSERT_NONNULL(aspace);

  // Create a VMO to map.
  fbl::RefPtr<VmObjectPaged> vmo;
  zx_status_t status =
      VmObjectPaged::Create(PMM_ALLOC_FLAG_ANY, VmObjectPaged::kResizable, 16 * kPageSize, &vmo);
  ASSERT_EQ(ZX_OK, status);

  EXPECT_TRUE(vmo->GetAttributedMemory() == AttributionCounts{});
  EXPECT_TRUE(verify_continuous_attribution_bytes(*vmo, 0));

  // Map the left half of the VMO.
  EXPECT_EQ(aspace->is_user(), true);
  auto mapping_result = aspace->RootVmar()->CreateVmMapping(0, 8 * kPageSize, 0, 0, vmo, 0,
                                                            kArchRwUserFlags, "test-mapping");
  EXPECT_EQ(ZX_OK, mapping_result.status_value());
  fbl::RefPtr<VmMapping> mapping = ktl::move(mapping_result->mapping);

  EXPECT_TRUE(vmo->GetAttributedMemory() == AttributionCounts{});
  EXPECT_TRUE(verify_continuous_attribution_bytes(*vmo, 0));
  EXPECT_TRUE(mapping->GetAttributedMemory() == AttributionCounts{});

  // Commit pages in the vmo via the mapping.
  status = mapping->MapRange(0, mapping->size(), true);
  ASSERT_EQ(ZX_OK, status);
  EXPECT_TRUE(vmo->GetAttributedMemory() == make_private_attribution_counts(8ul * kPageSize, 0));
  EXPECT_TRUE(verify_continuous_attribution_bytes(*vmo, 8ul * kPageSize));
  EXPECT_TRUE(mapping->GetAttributedMemory() ==
              make_private_attribution_counts(8ul * kPageSize, 0));

  // Unmap from the right end of the mapping.
  auto old_base = mapping->base();
  status = mapping->DebugUnmap(mapping->base() + mapping->size() - kPageSize, kPageSize);
  ASSERT_EQ(ZX_OK, status);
  mapping = aspace->FindRegion(old_base)->as_vm_mapping();
  ASSERT_TRUE(mapping);
  EXPECT_EQ(old_base, mapping->base());
  EXPECT_EQ(7ul * kPageSize, mapping->size());
  EXPECT_TRUE(vmo->GetAttributedMemory() == make_private_attribution_counts(8ul * kPageSize, 0));
  EXPECT_TRUE(verify_continuous_attribution_bytes(*vmo, 8ul * kPageSize));
  EXPECT_TRUE(mapping->GetAttributedMemory() ==
              make_private_attribution_counts(7ul * kPageSize, 0));

  // Unmap from the center of the mapping.
  status = mapping->DebugUnmap(mapping->base() + 4 * kPageSize, kPageSize);
  ASSERT_EQ(ZX_OK, status);
  mapping = aspace->FindRegion(old_base)->as_vm_mapping();
  ASSERT_TRUE(mapping);
  EXPECT_EQ(old_base, mapping->base());
  EXPECT_EQ(4ul * kPageSize, mapping->size());
  EXPECT_TRUE(vmo->GetAttributedMemory() == make_private_attribution_counts(8ul * kPageSize, 0));
  EXPECT_TRUE(verify_continuous_attribution_bytes(*vmo, 8ul * kPageSize));
  EXPECT_TRUE(mapping->GetAttributedMemory() ==
              make_private_attribution_counts(4ul * kPageSize, 0));

  // Unmap from the left end of the mapping.
  status = mapping->DebugUnmap(mapping->base(), kPageSize);
  ASSERT_EQ(ZX_OK, status);
  mapping = aspace->FindRegion(old_base + kPageSize)->as_vm_mapping();
  ASSERT_TRUE(mapping);
  EXPECT_NE(old_base, mapping->base());
  EXPECT_EQ(3ul * kPageSize, mapping->size());
  EXPECT_TRUE(vmo->GetAttributedMemory() == make_private_attribution_counts(8ul * kPageSize, 0));
  EXPECT_TRUE(verify_continuous_attribution_bytes(*vmo, 8ul * kPageSize));
  EXPECT_TRUE(mapping->GetAttributedMemory() ==
              make_private_attribution_counts(3ul * kPageSize, 0));

  // Free the test address space.
  status = aspace->Destroy();
  EXPECT_EQ(ZX_OK, status);

  END_TEST;
}

// Tests that memory attribution at the VmMapping layer behaves as expected when adjacent mappings
// are merged.
static bool vm_mapping_attribution_merge_test() {
  BEGIN_TEST;
  AutoVmScannerDisable scanner_disable;

  using AttributionCounts = VmObject::AttributionCounts;
  // Create a test VmAspace to temporarily switch to for creating test mappings.
  fbl::RefPtr<VmAspace> aspace = VmAspace::Create(VmAspace::Type::User, "test-aspace");
  ASSERT_NONNULL(aspace);
  EXPECT_EQ(aspace->is_user(), true);

  // Create a VMO to map.
  fbl::RefPtr<VmObjectPaged> vmo;
  zx_status_t status =
      VmObjectPaged::Create(PMM_ALLOC_FLAG_ANY, VmObjectPaged::kResizable, 16 * kPageSize, &vmo);
  ASSERT_EQ(ZX_OK, status);

  EXPECT_TRUE(vmo->GetAttributedMemory() == AttributionCounts{});
  EXPECT_TRUE(verify_continuous_attribution_bytes(*vmo, 0));

  // Create some contiguous mappings, marked unmergeable (default behavior) to begin with.
  struct {
    fbl::RefPtr<VmMapping> ref = nullptr;
    AttributionCounts expected_attribution_counts;
  } mappings[4];

  uint64_t offset = 0;
  static constexpr uint64_t kSize = 4 * kPageSize;
  for (int i = 0; i < 4; i++) {
    auto mapping_result = aspace->RootVmar()->CreateVmMapping(
        offset, kSize, 0, VMAR_FLAG_SPECIFIC, vmo, offset, kArchRwUserFlags, "test-mapping");
    ASSERT_EQ(ZX_OK, mapping_result.status_value());
    mappings[i].ref = ktl::move(mapping_result->mapping);
    EXPECT_TRUE(vmo->GetAttributedMemory() == AttributionCounts{});
    EXPECT_TRUE(verify_continuous_attribution_bytes(*vmo, 0));
    EXPECT_TRUE(mappings[i].ref->GetAttributedMemory() == mappings[i].expected_attribution_counts);
    offset += kSize;
  }
  EXPECT_EQ(offset, 16ul * kPageSize);

  // Commit pages in the VMO.
  status = vmo->CommitRange(0, 16 * kPageSize);
  ASSERT_EQ(ZX_OK, status);
  for (int i = 0; i < 4; i++) {
    mappings[i].expected_attribution_counts = make_private_attribution_counts(4ul * kPageSize, 0);
    EXPECT_TRUE(vmo->GetAttributedMemory() == make_private_attribution_counts(16ul * kPageSize, 0));
    EXPECT_TRUE(mappings[i].ref->GetAttributedMemory() == mappings[i].expected_attribution_counts);
  }

  // Mark mappings 0 and 2 mergeable. This should not change anything since they're separated by an
  // unmergeable mapping.
  VmMapping::MarkMergeable(ktl::move(mappings[0].ref));
  VmMapping::MarkMergeable(ktl::move(mappings[2].ref));
  for (int i = 0; i < 4; i++) {
    EXPECT_TRUE(vmo->GetAttributedMemory() == make_private_attribution_counts(16ul * kPageSize, 0));
    fbl::RefPtr<VmMapping> map =
        aspace->FindRegion(aspace->RootVmar()->base() + kSize * i)->as_vm_mapping();
    ASSERT_TRUE(map);
    EXPECT_TRUE(map->GetAttributedMemory() == mappings[i].expected_attribution_counts);
  }

  // Mark mapping 3 mergeable. This will merge mappings 2 and 3, destroying mapping 3 and moving all
  // of its pages into mapping 2.
  VmMapping::MarkMergeable(ktl::move(mappings[3].ref));
  mappings[2].expected_attribution_counts += mappings[3].expected_attribution_counts;
  for (int i = 0; i < 3; i++) {
    EXPECT_TRUE(vmo->GetAttributedMemory() == make_private_attribution_counts(16ul * kPageSize, 0));
    fbl::RefPtr<VmMapping> map =
        aspace->FindRegion(aspace->RootVmar()->base() + kSize * i)->as_vm_mapping();
    ASSERT_TRUE(map);
    EXPECT_TRUE(map->GetAttributedMemory() == mappings[i].expected_attribution_counts);
  }

  // Mark mapping 1 mergeable. This will merge mappings 0, 1 and 2, with only mapping 0 surviving
  // the merge. All the VMO's pages will have been moved to mapping 0.
  VmMapping::MarkMergeable(ktl::move(mappings[1].ref));
  mappings[0].expected_attribution_counts += mappings[1].expected_attribution_counts;
  mappings[0].expected_attribution_counts += mappings[2].expected_attribution_counts;
  EXPECT_TRUE(vmo->GetAttributedMemory() == make_private_attribution_counts(16ul * kPageSize, 0));
  fbl::RefPtr<VmMapping> map = aspace->FindRegion(aspace->RootVmar()->base())->as_vm_mapping();
  ASSERT_TRUE(map);
  EXPECT_TRUE(map->GetAttributedMemory() == mappings[0].expected_attribution_counts);

  // Free the test address space.
  status = aspace->Destroy();
  EXPECT_EQ(ZX_OK, status);

  END_TEST;
}

static bool vm_mapping_page_fault_optimisation_test() {
  BEGIN_TEST;

  AutoVmScannerDisable scanner_disable;

  constexpr uint64_t kMaxOptPages = VmMapping::kPageFaultMaxOptimisticPages;

  // Size the allocation of the VMO / mapping to be double the optimistic extension so we can
  // validate that it is limited by the optimistic cap, not the size of the VMO.
  constexpr size_t alloc_size = kMaxOptPages * 2 * kPageSize;
  static const uint8_t align_pow2 = log2_floor(alloc_size);

  // Mapped & fully committed VMO.
  fbl::RefPtr<VmObjectPaged> committed_vmo;
  ASSERT_OK(VmObjectPaged::Create(PMM_ALLOC_FLAG_ANY, 0, alloc_size, &committed_vmo));

  ktl::unique_ptr<testing::UserMemory> mapping =
      testing::UserMemory::Create(committed_vmo, 0, align_pow2);
  ASSERT_NONNULL(mapping);

  committed_vmo->CommitRange(0, alloc_size);

  // Trigger a page fault on the first page in the VMO/Mapping.
  mapping->put(42);

  // Optimisation will fault the minimum of kMaxOptPages pages and the end of the VMO, protection
  // range, mapping or page table. We have ensured that all of these will be > kMaxOptPages in this
  // case.
  ASSERT_TRUE(verify_mapped_page_range(mapping->base(), alloc_size, kMaxOptPages));

  // Mapped but not committed VMO.
  fbl::RefPtr<VmObjectPaged> uncommitted_vmo;
  ASSERT_OK(VmObjectPaged::Create(PMM_ALLOC_FLAG_ANY, 0, alloc_size, &uncommitted_vmo));

  ktl::unique_ptr<testing::UserMemory> mapping2 =
      testing::UserMemory::Create(uncommitted_vmo, 0, align_pow2);
  ASSERT_NONNULL(mapping2);

  // Trigger a page fault on the first page in the VMO/Mapping.
  mapping2->put(42);

  // As the VMO is uncommitted, only the requested page should have been faulted.
  ASSERT_TRUE(verify_mapped_page_range(mapping2->base(), alloc_size, 1));

  // Single committed page.
  fbl::RefPtr<VmObjectPaged> onepage_committed_vmo;
  ASSERT_OK(VmObjectPaged::Create(PMM_ALLOC_FLAG_ANY, 0, alloc_size, &onepage_committed_vmo));

  ktl::unique_ptr<testing::UserMemory> mapping3 =
      testing::UserMemory::Create(onepage_committed_vmo, 0, align_pow2);
  ASSERT_NONNULL(mapping3);

  onepage_committed_vmo->CommitRange(0, kPageSize);

  // Trigger a page fault on the first page in the VMO/Mapping.
  mapping3->put(42);

  // Only the requested page should have been faulted.
  ASSERT_TRUE(verify_mapped_page_range(mapping3->base(), alloc_size, 1));

  // 4 committed pages.
  static_assert(4 <= kMaxOptPages);
  fbl::RefPtr<VmObjectPaged> partially_committed_vmo;
  ASSERT_OK(VmObjectPaged::Create(PMM_ALLOC_FLAG_ANY, 0, alloc_size, &partially_committed_vmo));

  ktl::unique_ptr<testing::UserMemory> mapping4 =
      testing::UserMemory::Create(partially_committed_vmo, 0, align_pow2);
  ASSERT_NONNULL(mapping4);

  partially_committed_vmo->CommitRange(0, 4 * kPageSize);

  // Trigger a page fault on the first page in the VMO/Mapping.
  mapping4->put(42);

  // Only the already committed pages should be committed.
  ASSERT_TRUE(verify_mapped_page_range(mapping4->base(), alloc_size, 4));

  END_TEST;
}

// Validate that the page fault optimisation correctly respects page table boundaries.
static bool vm_mapping_page_fault_optimization_pt_limit_test() {
  BEGIN_TEST;

  AutoVmScannerDisable scanner_disable;

  constexpr size_t kMaxOptPages = VmMapping::kPageFaultMaxOptimisticPages;
  // Size our top level vmar allocation to be two page tables in size, ensuring that we will both
  // have a page table boundary crossing in the allocation, as well as some amount of allocation on
  // either side of it.
  constexpr size_t kPageTableSize = ArchVmAspace::NextUserPageTableOffset(0);
  constexpr size_t kVmarSize = kPageTableSize * 2;
  // Align our allocation on a page table boundary, ensuring we have 1 page table worth of space
  // before and after our PT crossing point.
  constexpr size_t kVmarAlign = log2_floor(kPageTableSize);
  // Size the allocation of the VMO / mapping to be double the optimistic extension so we can
  // validate that it is limited by the optimistic cap, not the size of the VMO.
  constexpr size_t kMapSize = kMaxOptPages * 2 * kPageSize;

  // Allocate our large top level vmar in root vmar of the current aspace.
  fbl::RefPtr<VmAddressRegion> root_vmar = Thread::Current::active_aspace()->RootVmar();
  fbl::RefPtr<VmAddressRegion> vmar;
  ASSERT_OK(root_vmar->CreateSubVmar(0, kVmarSize, kVmarAlign,
                                     VMAR_FLAG_CAN_MAP_READ | VMAR_FLAG_CAN_MAP_WRITE |
                                         VMAR_FLAG_CAN_MAP_EXECUTE | VMAR_FLAG_CAN_MAP_SPECIFIC,
                                     "unittest", &vmar));
  auto cleanup_vmar = fit::defer([&] { vmar->Destroy(); });

  const vaddr_t next_pt_base = ArchVmAspace::NextUserPageTableOffset(vmar->base());
  // If our alignment was specified correctly the next pt should be exactly one pt from our base.
  ASSERT_EQ(vmar->base() + kPageTableSize, next_pt_base);

  // Try touching at different distances from the start of the next page table and validate that
  // mappings are not added beyond it.
  for (size_t page_offset = 0; page_offset <= kMaxOptPages + 1; page_offset++) {
    // Create a subvmar at the correct offset that will precisely hold our mapping.
    fbl::RefPtr<VmAddressRegion> sub_vmar;
    const size_t offset = kPageTableSize - kPageSize * page_offset;
    ASSERT_OK(vmar->CreateSubVmar(offset, kMapSize, 0,
                                  VMAR_FLAG_CAN_MAP_READ | VMAR_FLAG_CAN_MAP_WRITE |
                                      VMAR_FLAG_CAN_MAP_EXECUTE | VMAR_FLAG_SPECIFIC,
                                  "unittest", &sub_vmar));
    auto cleanup_sub_vmar = fit::defer([&] { sub_vmar->Destroy(); });

    fbl::RefPtr<VmObjectPaged> committed_vmo;
    ASSERT_OK(VmObjectPaged::Create(PMM_ALLOC_FLAG_ANY, 0, kMapSize, &committed_vmo));

    ktl::unique_ptr<testing::UserMemory> mapping =
        testing::UserMemory::CreateInVmar(committed_vmo, sub_vmar);
    ASSERT_NONNULL(mapping);

    committed_vmo->CommitRange(0, kMapSize);

    // Trigger a page fault on the first page of the mapping.
    mapping->put(42);

    // We expect the number of pages that are mapped in to be clipped at the page table boundary,
    // which would be |page_offset|. The two exceptions to this are if page_offset is greater than
    // kMaxOptPages, in which case that becomes the cap, or if the page_offset is 0, in which case
    // we are actually at the *start* of the next page table, and so kMaxOptPages should get mapped.
    const size_t kExpectedPages =
        page_offset == 0 ? kMaxOptPages : ktl::min(kMaxOptPages, page_offset);

    ASSERT_TRUE(verify_mapped_page_range(mapping->base(), kMapSize, kExpectedPages));
  }

  END_TEST;
}

static bool vm_mapping_page_fault_range_test() {
  BEGIN_TEST;

  AutoVmScannerDisable scanner_disable;

  constexpr size_t kTestPages = VmMapping::kPageFaultMaxOptimisticPages * 2;
  constexpr size_t kAllocSize = kTestPages * kPageSize;
  constexpr uint kReadFlags = VMM_PF_FLAG_USER;
  constexpr uint kWriteFlags = VMM_PF_FLAG_USER | VMM_PF_FLAG_WRITE;
  // Aligning the mapping is for when testing the optimistic fault handler to ensure that there are
  // no spurious failures due to crossing a page table boundary.
  static const uint8_t align_pow2 = log2_floor(kAllocSize);

  fbl::RefPtr<VmObjectPaged> vmo;
  ASSERT_OK(VmObjectPaged::Create(PMM_ALLOC_FLAG_ANY, VmObjectPaged::kResizable, kAllocSize, &vmo));

  ktl::unique_ptr<testing::UserMemory> mapping = testing::UserMemory::Create(vmo, 0, align_pow2);
  ASSERT_NONNULL(mapping);

  // Faulting even 1 additional page should prevent optimistic faulting.
  {
    // Decommit and recommit the VMO to ensure no page table mappings.
    EXPECT_OK(vmo->DecommitRange(0, kAllocSize));
    EXPECT_OK(vmo->CommitRange(0, kAllocSize));
    EXPECT_TRUE(verify_mapped_page_range(mapping->base(), kAllocSize, 0));

    // Fault a two page range should only give two pages.
    EXPECT_OK(Thread::Current::SoftFaultInRange(mapping->base(), kReadFlags, kPageSize * 2));
    EXPECT_TRUE(verify_mapped_page_range(mapping->base(), kAllocSize, 2));

    // Reset and fault a single page to validate optimistic faulting would otherwise have happened.
    EXPECT_OK(vmo->DecommitRange(0, kAllocSize));
    EXPECT_OK(vmo->CommitRange(0, kAllocSize));
    EXPECT_TRUE(verify_mapped_page_range(mapping->base(), kAllocSize, 0));
    EXPECT_OK(Thread::Current::SoftFaultInRange(mapping->base(), kReadFlags, kPageSize));
    EXPECT_TRUE(verify_mapped_page_range(mapping->base(), kAllocSize,
                                         VmMapping::kPageFaultMaxOptimisticPages));
  }

  // Will map in pages that are not committed on read without allocating.
  {
    // Start with one page committed.
    EXPECT_OK(vmo->DecommitRange(0, kAllocSize));
    EXPECT_OK(vmo->CommitRange(0, kPageSize));
    EXPECT_TRUE(verify_mapped_page_range(mapping->base(), kAllocSize, 0));
    EXPECT_TRUE(vmo->GetAttributedMemory() == make_private_attribution_counts(kPageSize, 0));

    // Read faulting the range should map without allocating.
    EXPECT_OK(Thread::Current::SoftFaultInRange(mapping->base(), kReadFlags, kAllocSize));
    EXPECT_TRUE(verify_mapped_page_range(mapping->base(), kAllocSize, kTestPages));
    EXPECT_TRUE(vmo->GetAttributedMemory() == make_private_attribution_counts(kPageSize, 0));
  }

  // Write faulting should cause allocations
  {
    // Start with one page committed.
    EXPECT_OK(vmo->DecommitRange(0, kAllocSize));
    EXPECT_OK(vmo->CommitRange(0, kPageSize));
    EXPECT_TRUE(verify_mapped_page_range(mapping->base(), kAllocSize, 0));
    EXPECT_TRUE(vmo->GetAttributedMemory() == make_private_attribution_counts(kPageSize, 0));

    // Write faulting the range should both map and allocate the pages.
    EXPECT_OK(Thread::Current::SoftFaultInRange(mapping->base(), kWriteFlags, kAllocSize));
    EXPECT_TRUE(verify_mapped_page_range(mapping->base(), kAllocSize, kTestPages));
    EXPECT_TRUE(vmo->GetAttributedMemory() == make_private_attribution_counts(kAllocSize, 0));
  }

  // Faulting a partial range should not overrun
  {
    EXPECT_OK(vmo->DecommitRange(0, kAllocSize));
    EXPECT_TRUE(verify_mapped_page_range(mapping->base(), kAllocSize, 0));
    EXPECT_TRUE(vmo->GetAttributedMemory() == make_private_attribution_counts(0, 0));

    // Write faulting the range should both map and allocate the requested pages, but no more.
    EXPECT_OK(Thread::Current::SoftFaultInRange(mapping->base(), kWriteFlags, kAllocSize / 2));
    EXPECT_TRUE(verify_mapped_page_range(mapping->base(), kAllocSize, kTestPages / 2));
    EXPECT_TRUE(vmo->GetAttributedMemory() == make_private_attribution_counts(kAllocSize / 2, 0));
  }

  // Should not error if > VMO length.
  {
    EXPECT_OK(vmo->DecommitRange(0, kAllocSize));
    // Shrink the VMO so that it is smaller than the mapping.
    EXPECT_OK(vmo->Resize(kAllocSize / 2));
    EXPECT_TRUE(verify_mapped_page_range(mapping->base(), kAllocSize, 0));

    // Attempt to fault the entire mapping range, which is now larger than the VMO.
    EXPECT_OK(Thread::Current::SoftFaultInRange(mapping->base(), kWriteFlags, kAllocSize));
    // Only half should have been mapped and what is now the whole VMO should be committed.
    EXPECT_TRUE(verify_mapped_page_range(mapping->base(), kAllocSize, kTestPages / 2));
    EXPECT_TRUE(vmo->GetAttributedMemory() == make_private_attribution_counts(kAllocSize / 2, 0));

    // Restore the VMO size.
    EXPECT_OK(vmo->Resize(kAllocSize));
  }

  // Will respect protection boundaries.
  {
    EXPECT_OK(vmo->DecommitRange(0, kAllocSize));
    // Remove write permissions from half the mapping.
    EXPECT_OK(mapping->Protect(ARCH_MMU_FLAG_PERM_READ, kAllocSize / 2));

    // Attempt to write fault the entire mapping.
    EXPECT_OK(Thread::Current::SoftFaultInRange(mapping->base(), kWriteFlags, kAllocSize));
    // Only the writable half should have been mapped and committed.
    EXPECT_TRUE(verify_mapped_page_range(mapping->base(), kAllocSize, kTestPages / 2));
    EXPECT_TRUE(vmo->GetAttributedMemory() == make_private_attribution_counts(kAllocSize / 2, 0));

    // Reset protections.
    EXPECT_OK(mapping->Protect(ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE));
  }

  // Will mark modified if even one writable page is mapped even if mapping aborts early due to an
  // error.
  {
    // Create a pager backed VMO with one page committed and map it.
    fbl::RefPtr<VmObjectPaged> paged_vmo;
    ASSERT_OK(
        make_partially_committed_pager_vmo(kTestPages, 1, false, false, true, nullptr, &paged_vmo));
    ktl::unique_ptr<testing::UserMemory> paged_mapping = testing::UserMemory::Create(paged_vmo);

    // Consume any existing modified flag.
    zx_pager_vmo_stats_t stats;
    EXPECT_OK(paged_vmo->QueryPagerVmoStats(true, &stats));
    EXPECT_OK(paged_vmo->QueryPagerVmoStats(true, &stats));
    EXPECT_EQ(stats.modified, 0u);

    // Perform a fault that will have to generate a page request. To avoid blocking on the page
    // request we must directly call the PageFaultLocked method instead of the VmAspace fault.
    {
      const vaddr_t base = paged_mapping->base();
      ktl::pair<zx_status_t, uint32_t> result;
      size_t retry_count = 0;
      do {
        Guard<CriticalMutex> guard{paged_mapping->mapping()->lock()};
        MultiPageRequest page_request;
        // Although the first page is supplied to paged_vmo, attempting to map it could still fail
        // due to either it being deduped to a marker, or it being a loaned page and needing to be
        // swapped. Both of these cases require an allocation, which could need to wait. This wait
        // request should only be due to the pmm random delayed allocations, and so we can just
        // ignore it and try again.
        result = paged_mapping->mapping()->PageFaultLocked(base, kWriteFlags, kTestPages - 1,
                                                           &page_request);
        page_request.CancelRequests();
        retry_count++;
      } while (result.first == ZX_ERR_SHOULD_WAIT && result.second == 0 && retry_count < 100);
      EXPECT_EQ(result.first, ZX_ERR_SHOULD_WAIT);
      EXPECT_EQ(result.second, 1u);
    }

    // The one previously committed page should have been mapped in and the VMO marked modified.
    EXPECT_TRUE(verify_mapped_page_range(paged_mapping->base(), kAllocSize, 1));

    EXPECT_OK(paged_vmo->QueryPagerVmoStats(true, &stats));
    EXPECT_EQ(stats.modified, ZX_PAGER_VMO_STATS_MODIFIED);
  }

  // Read fault on copy-on-write hierarchy with some leaf pages will map both parent and child
  // pages without committing extra pages into the child.
  {
    EXPECT_OK(vmo->CommitRange(0, kAllocSize));
    // Create a snapshot with some committed pages and map it in.
    fbl::RefPtr<VmObject> child_vmo;
    ASSERT_OK(vmo->CreateClone(Resizability::NonResizable, SnapshotType::Full, 0, kAllocSize, true,
                               &child_vmo));
    EXPECT_OK(child_vmo->CommitRange(0, kPageSize));
    EXPECT_OK(child_vmo->CommitRange(kAllocSize / 2, kPageSize));
    ktl::unique_ptr<testing::UserMemory> child_mapping = testing::UserMemory::Create(child_vmo);

    // Read fault the entire range. Everything should get mapped with the child's memory attribution
    // being unchanged.
    VmObject::AttributionCounts original_counts = child_vmo->GetAttributedMemory();
    EXPECT_OK(Thread::Current::SoftFaultInRange(child_mapping->base(), kReadFlags, kAllocSize));
    EXPECT_TRUE(verify_mapped_page_range(child_mapping->base(), kAllocSize, kTestPages));
    EXPECT_TRUE(original_counts == child_vmo->GetAttributedMemory());
    EXPECT_TRUE(verify_continuous_attribution_bytes(
        *child_vmo, original_counts.uncompressed_bytes + original_counts.compressed_bytes));
  }

  // Calling ReadUser will fault the requested range.
  {
    EXPECT_OK(vmo->DecommitRange(0, kAllocSize));
    vmo->CommitRange(0, kAllocSize);

    ASSERT_TRUE(verify_mapped_page_range(mapping->base(), kAllocSize, 0));

    auto [status, read_actual] = vmo->ReadUser(
        mapping->user_out<char>(), 0, sizeof(char[kPageSize * 2]), VmObjectReadWriteOptions::None);
    ASSERT_EQ(status, ZX_OK);
    ASSERT_EQ(read_actual, sizeof(char[kPageSize * 2]));

    // The page fault optimisation should not have been triggered so the exact range is mapped.
    ASSERT_TRUE(verify_mapped_page_range(mapping->base(), kAllocSize, 2));
  }

  END_TEST;
}

using ArchUnmapOptions = ArchVmAspaceInterface::ArchUnmapOptions;

static bool arch_noncontiguous_map() {
  BEGIN_TEST;

  // Get some phys pages to test on
  paddr_t phys[3];
  VmPageDoublyLinkedList phys_list;
  zx_status_t status = pmm_alloc_pages(ktl::size(phys), 0, &phys_list);
  ASSERT_EQ(ZX_OK, status, "non contig map alloc");
  {
    size_t i = 0;
    for (auto& p : phys_list) {
      phys[i] = p.paddr();
      ++i;
    }
  }

  {
    constexpr vaddr_t base = USER_ASPACE_BASE + 10 * kPageSize;

    ArchVmAspace aspace(USER_ASPACE_BASE, USER_ASPACE_SIZE, 0);
    status = aspace.Init();
    ASSERT_EQ(ZX_OK, status, "failed to init aspace\n");

    // Attempt to map a set of vm_page_t
    status = aspace.Map(base, phys, ktl::size(phys), ARCH_MMU_FLAG_PERM_READ,
                        ArchVmAspace::ExistingEntryAction::Error);
    ASSERT_EQ(ZX_OK, status, "failed first map\n");

    // Expect that the map succeeded
    for (size_t i = 0; i < ktl::size(phys); ++i) {
      paddr_t paddr;
      arch_mmu_flags_t mmu_flags;
      status = aspace.Query(base + i * kPageSize, &paddr, &mmu_flags);
      EXPECT_EQ(ZX_OK, status, "bad first map\n");
      EXPECT_EQ(phys[i], paddr, "bad first map\n");
      EXPECT_EQ(ARCH_MMU_FLAG_PERM_READ, mmu_flags, "bad first map\n");
    }

    // Attempt to map again, should fail
    status = aspace.Map(base, phys, ktl::size(phys), ARCH_MMU_FLAG_PERM_READ,
                        ArchVmAspace::ExistingEntryAction::Error);
    EXPECT_EQ(ZX_ERR_ALREADY_EXISTS, status, "double map\n");

    // Attempt to map partially overlapping, should fail
    status = aspace.Map(base + 2 * kPageSize, phys, ktl::size(phys), ARCH_MMU_FLAG_PERM_READ,
                        ArchVmAspace::ExistingEntryAction::Error);
    EXPECT_EQ(ZX_ERR_ALREADY_EXISTS, status, "double map\n");
    status = aspace.Map(base - 2 * kPageSize, phys, ktl::size(phys), ARCH_MMU_FLAG_PERM_READ,
                        ArchVmAspace::ExistingEntryAction::Error);
    EXPECT_EQ(ZX_ERR_ALREADY_EXISTS, status, "double map\n");

    // No entries should have been created by the partial failures
    status = aspace.Query(base - 2 * kPageSize, nullptr, nullptr);
    EXPECT_EQ(ZX_ERR_NOT_FOUND, status, "bad first map\n");
    status = aspace.Query(base - kPageSize, nullptr, nullptr);
    EXPECT_EQ(ZX_ERR_NOT_FOUND, status, "bad first map\n");
    status = aspace.Query(base + 3 * kPageSize, nullptr, nullptr);
    EXPECT_EQ(ZX_ERR_NOT_FOUND, status, "bad first map\n");
    status = aspace.Query(base + 4 * kPageSize, nullptr, nullptr);
    EXPECT_EQ(ZX_ERR_NOT_FOUND, status, "bad first map\n");

    // Unmap all remaining entries
    // The partial failures did not create any new entries, so only entries
    // created by the first map should be unmapped.
    status = aspace.Unmap(base, ktl::size(phys), ArchUnmapOptions::Enlarge);
    ASSERT_EQ(ZX_OK, status, "failed unmap\n");

    status = aspace.Destroy();
    EXPECT_EQ(ZX_OK, status, "failed to destroy aspace\n");
  }

  pmm_free(&phys_list);

  END_TEST;
}

static bool arch_noncontiguous_map_with_upgrade() {
  BEGIN_TEST;

  // Get some phys pages to test on
  paddr_t phys[3];
  VmPageDoublyLinkedList phys_list;
  zx_status_t status = pmm_alloc_pages(ktl::size(phys), 0, &phys_list);
  ASSERT_EQ(ZX_OK, status, "non contig map alloc");
  {
    size_t i = 0;
    for (auto& p : phys_list) {
      phys[i] = p.paddr();
      ++i;
    }
  }

  {
    constexpr vaddr_t base = USER_ASPACE_BASE + 10 * kPageSize;
    constexpr vaddr_t window_base = base - 2 * kPageSize;

    ArchVmAspace aspace(USER_ASPACE_BASE, USER_ASPACE_SIZE, 0);
    status = aspace.Init();
    ASSERT_EQ(ZX_OK, status, "failed to init aspace\n");

    // Attempt to map a set of vm_page_t
    status = aspace.Map(base, phys, ktl::size(phys), ARCH_MMU_FLAG_PERM_READ,
                        ArchVmAspace::ExistingEntryAction::Error);
    ASSERT_EQ(ZX_OK, status, "failed first map\n");

    // Attempt to map with upgrades allowed, should succeed
    status =
        aspace.Map(base, phys, ktl::size(phys), ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
                   ArchVmAspace::ExistingEntryAction::Upgrade);
    EXPECT_EQ(ZX_OK, status, "map upgrade failed\n");

    // Attempt to map with upgrades allowed, should succeed but not remap anything
    // b/c downgrade to read not allowed
    status = aspace.Map(base, phys, ktl::size(phys), ARCH_MMU_FLAG_PERM_READ,
                        ArchVmAspace::ExistingEntryAction::Upgrade);
    EXPECT_EQ(ZX_OK, status, "map upgrade failed\n");

    // Expect that the upgrade maps succeeded
    for (size_t i = 0; i < ktl::size(phys); ++i) {
      const uint MAP_WINDOW_PADDR_INDEX[ktl::size(phys)] = {0, 1, 2};
      constexpr uint MAP_WINDOW_MMU_FLAGS[ktl::size(phys)] = {
          ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
          ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
          ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE};

      paddr_t paddr;
      arch_mmu_flags_t mmu_flags;
      status = aspace.Query(base + i * kPageSize, &paddr, &mmu_flags);
      EXPECT_EQ(ZX_OK, status, "bad map upgrade\n");
      EXPECT_EQ(phys[MAP_WINDOW_PADDR_INDEX[i]], paddr, "bad map upgrade\n");
      EXPECT_EQ(MAP_WINDOW_MMU_FLAGS[i], mmu_flags, "bad map upgrade\n");
    }

    // Attempt to map partially overlapping with upgrades allowed, should succeed
    status = aspace.Map(base + 2 * kPageSize, phys, ktl::size(phys),
                        ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
                        ArchVmAspace::ExistingEntryAction::Upgrade);
    EXPECT_EQ(ZX_OK, status, "map upgrade failed\n");
    status = aspace.Map(base - 2 * kPageSize, phys, ktl::size(phys), ARCH_MMU_FLAG_PERM_READ,
                        ArchVmAspace::ExistingEntryAction::Upgrade);
    EXPECT_EQ(ZX_OK, status, "map upgrade failed\n");

    // Expect that the `Upgrade` maps succeeded
    // We check the entire [base - 2, base + 4] "window" covered by the partial maps
    constexpr size_t MAP_WINDOW_SIZE = 7;
    for (size_t i = 0; i < MAP_WINDOW_SIZE; ++i) {
      const uint MAP_WINDOW_PADDR_INDEX[MAP_WINDOW_SIZE] = {0, 1, 2, 1, 0, 1, 2};
      constexpr uint MAP_WINDOW_MMU_FLAGS[MAP_WINDOW_SIZE] = {
          ARCH_MMU_FLAG_PERM_READ,
          ARCH_MMU_FLAG_PERM_READ,
          ARCH_MMU_FLAG_PERM_READ,
          ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
          ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
          ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
          ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE};

      paddr_t paddr;
      arch_mmu_flags_t mmu_flags;
      status = aspace.Query(window_base + i * kPageSize, &paddr, &mmu_flags);
      EXPECT_EQ(ZX_OK, status, "bad map upgrade\n");
      EXPECT_EQ(phys[MAP_WINDOW_PADDR_INDEX[i]], paddr, "bad map upgrade\n");
      EXPECT_EQ(MAP_WINDOW_MMU_FLAGS[i], mmu_flags, "bad map upgrade\n");
    }

    // Unmap any remaining entries
    status = aspace.Unmap(window_base, MAP_WINDOW_SIZE, ArchUnmapOptions::Enlarge);
    ASSERT_EQ(ZX_OK, status, "failed unmap\n");

    status = aspace.Destroy();
    EXPECT_EQ(ZX_OK, status, "failed to destroy aspace\n");
  }

  pmm_free(&phys_list);

  END_TEST;
}

// Get the mmu_flags of the given vaddr of the given aspace.
//
// Return 0 if the page is unmapped or on error.
static arch_mmu_flags_t get_vaddr_flags(ArchVmAspace* aspace, vaddr_t vaddr) {
  paddr_t unused_paddr;
  arch_mmu_flags_t mmu_flags;
  if (aspace->Query(vaddr, &unused_paddr, &mmu_flags) != ZX_OK) {
    return 0;
  }
  return mmu_flags;
}

// Determine if the given page is mapped in.
static bool is_vaddr_mapped(ArchVmAspace* aspace, vaddr_t vaddr) {
  return get_vaddr_flags(aspace, vaddr) != 0;
}

static bool arch_vm_aspace_protect_split_pages() {
  BEGIN_TEST;

  constexpr uint kReadOnly = ARCH_MMU_FLAG_PERM_READ;
  constexpr uint kReadWrite = ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE;

  // Create a basic address space, starting from vaddr 0.
  ArchVmAspace aspace(0, USER_ASPACE_SIZE, 0);
  ASSERT_OK(aspace.Init());
  auto cleanup = fit::defer([&]() {
    aspace.Unmap(0, USER_ASPACE_SIZE / kPageSize, ArchUnmapOptions::Enlarge);
    aspace.Destroy();
  });

  // Map in a large contiguous area, which should be mapped by two large pages.
  static_assert(ZX_MAX_PAGE_SIZE > kPageSize);
  constexpr size_t kRegionSize = 16ul * 1024 * 1024 * 1024;  // 16 GiB.
  ASSERT_OK(
      aspace.MapContiguous(/*vaddr=*/0, /*paddr=*/0, /*count=*/kRegionSize / kPageSize, kReadOnly));

  // Attempt to protect a subrange in the middle of the region, which will require splitting
  // pages.
  constexpr vaddr_t kProtectedRange = kRegionSize / 2 - kPageSize;
  constexpr size_t kProtectedPages = 2;
  ASSERT_OK(aspace.Protect(kProtectedRange, /*count=*/kProtectedPages, kReadWrite,
                           ArchUnmapOptions::Enlarge));

  // Ensure the pages inside the range changed.
  EXPECT_EQ(get_vaddr_flags(&aspace, kProtectedRange), kReadWrite);
  EXPECT_EQ(get_vaddr_flags(&aspace, kProtectedRange + kPageSize), kReadWrite);

  // Ensure the pages surrounding the range did not change.
  EXPECT_EQ(get_vaddr_flags(&aspace, kProtectedRange - kPageSize), kReadOnly);
  EXPECT_EQ(get_vaddr_flags(&aspace, kProtectedRange + kProtectedPages * kPageSize), kReadOnly);

  END_TEST;
}

static bool arch_vm_aspace_protect_split_pages_out_of_memory() {
  BEGIN_TEST;

  constexpr uint kReadOnly = ARCH_MMU_FLAG_PERM_READ;
  constexpr uint kReadWrite = ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE;

  // Create a custom allocator that we can cause to stop returning allocations.
  //
  // ArchVmAspace doesn't allow us to send state to the allocator, so we use a
  // global static here to control the allocator.
  static bool allow_allocations;
  auto allocator = +[](uint alloc_flags, vm_page** p, paddr_t* pa) -> zx_status_t {
    if (!allow_allocations) {
      return ZX_ERR_NO_MEMORY;
    }
    return pmm_alloc_page(0, p, pa);
  };
  allow_allocations = true;

  // Create a basic address space, starting from vaddr 0.
  ArchVmAspace aspace(0, USER_ASPACE_SIZE, 0, allocator);
  ASSERT_OK(aspace.Init());
  auto cleanup = fit::defer([&]() {
    aspace.Unmap(0, USER_ASPACE_SIZE / kPageSize, ArchUnmapOptions::Enlarge);
    aspace.Destroy();
  });

  // Map in a large contiguous area, large enough to use large pages to fill.
  constexpr size_t kRegionSize = 16ul * 1024 * 1024 * 1024;  // 16 GiB.
  ASSERT_OK(
      aspace.MapContiguous(/*vaddr=*/0, /*paddr=*/0, /*count=*/kRegionSize / kPageSize, kReadOnly));

  // Prevent further allocations.
  allow_allocations = false;

  // Attempt to protect a subrange in the middle of the region, which will require splitting
  // pages. Expect this to fail.
  constexpr vaddr_t kProtectedRange = kRegionSize / 2 - kPageSize;
  constexpr size_t kProtectedSize = 2 * kPageSize;
  zx_status_t status =
      aspace.Protect(kProtectedRange, /*count=*/2, kReadWrite, ArchUnmapOptions::Enlarge);
  EXPECT_EQ(status, ZX_ERR_NO_MEMORY);

  // The pages surrounding our protect range should still be mapped.
  EXPECT_EQ(get_vaddr_flags(&aspace, kProtectedRange - kPageSize), kReadOnly);
  EXPECT_EQ(get_vaddr_flags(&aspace, kProtectedRange + kProtectedSize), kReadOnly);

  // The pages we tried to protect should still be mapped, albeit permissions might
  // be changed.
  EXPECT_TRUE(is_vaddr_mapped(&aspace, kProtectedRange));
  EXPECT_TRUE(is_vaddr_mapped(&aspace, kProtectedRange + kPageSize));

  END_TEST;
}

// Test to make sure all the vm kernel regions (code, rodata, data, bss, etc.) are correctly mapped
// in vm and have the correct arch_mmu_flags. This test also check that all gaps are contained
// within a VMAR.
static bool vm_kernel_region_test() {
  BEGIN_TEST;

  fbl::RefPtr<VmAddressRegionOrMapping> kernel_vmar =
      VmAspace::kernel_aspace()->RootVmar()->FindRegion(
          reinterpret_cast<vaddr_t>(__executable_start));
  EXPECT_NE(kernel_vmar.get(), nullptr);
  EXPECT_FALSE(kernel_vmar->is_mapping());
  for (vaddr_t base = reinterpret_cast<vaddr_t>(__executable_start);
       base < reinterpret_cast<vaddr_t>(_end); base += kPageSize) {
    bool within_region = false;
    for (const auto& kernel_region : kernel_regions) {
      // This would not overflow because the region base and size are hard-coded.
      if (kernel_region.size != 0 && base >= kernel_region.base &&
          base + kPageSize <= kernel_region.base + kernel_region.size) {
        // If this page exists within a kernel region, then it should be within a VmMapping with
        // the correct arch MMU flags.
        within_region = true;
        fbl::RefPtr<VmAddressRegionOrMapping> region =
            kernel_vmar->as_vm_address_region()->FindRegion(base);
        // Every page from __code_start to _end should either be a VmMapping or a VMAR.
        EXPECT_NE(region.get(), nullptr);
        EXPECT_TRUE(region->is_mapping());
        Guard<CriticalMutex> guard{region->as_vm_mapping()->lock()};
        EXPECT_EQ(kernel_region.arch_mmu_flags,
                  region->as_vm_mapping()->arch_mmu_flags_locked(base));
        break;
      }
    }
    if (!within_region) {
      auto region = VmAspace::kernel_aspace()->RootVmar()->FindRegion(base);
      EXPECT_EQ(region.get(), kernel_vmar.get());
    }
  }

  END_TEST;
}

class TestRegionList;

class TestRegion : public fbl::RefCounted<TestRegion> {
 public:
  TestRegion(vaddr_t base, size_t size, TestRegionList const& list)
      : list_(list), base_(base), size_(size) {}
  ~TestRegion() = default;

  vaddr_t base() const { return base_; }
  size_t size() const { return size_; }

  vaddr_t base_locked() const { return base_; }
  size_t size_locked() const { return size_; }

  Lock<CriticalMutex>* lock() const;
  Lock<CriticalMutex>& lock_ref() const;

 private:
  friend class TestRegionList;
  // Simulates aspace for templated code
  TestRegionList const& list_;
  vaddr_t base_;
  size_t size_;
};

class TestRegionList : public fbl::RefCounted<TestRegionList> {
 public:
  TestRegionList() : guard_(&lock_) {}
  Lock<CriticalMutex>* lock() const TA_RET_CAP(lock_) { return &lock_; }
  Lock<CriticalMutex>& lock_ref() const TA_RET_CAP(lock_) { return lock_; }

  RegionList<TestRegion>* get_regions() { return &regions_; }

  void insert_region(vaddr_t base, size_t size) {
    fbl::AllocChecker ac;
    auto test_region = fbl::AdoptRef(new (&ac) TestRegion(base, size, *this));
    ASSERT(ac.check());
    zx_status_t status = regions_.InsertRegion(ktl::move(test_region));
    ASSERT(status == ZX_OK);
  }

  bool remove_region(vaddr_t base) {
    auto region = regions_.FindRegion(base);
    if (region == nullptr) {
      return false;
    }
    regions_.RemoveRegion(region);
    return true;
  }

 private:
  friend class TestRegion;
  mutable DECLARE_CRITICAL_MUTEX(TestRegionList) lock_;
  Guard<CriticalMutex> guard_;
  RegionList<TestRegion> regions_;
};

Lock<CriticalMutex>* TestRegion::lock() const TA_RET_CAP(list_.lock()) { return list_.lock(); }
Lock<CriticalMutex>& TestRegion::lock_ref() const TA_RET_CAP(list_.lock()) {
  return list_.lock_ref();
}

static bool region_list_get_alloc_spot_test() {
  BEGIN_TEST;

  TestRegionList test_list;
  auto regions = test_list.get_regions();
  vaddr_t base = 0xFFFF000000000000;
  vaddr_t size = 0x0001000000000000;
  vaddr_t alloc_spot = 0;
  // Set the align to be 0x1000.
  uint8_t align_pow2 = 12;
  // Allocate 1 page, should be allocated at [+0, +0x1000].
  size_t alloc_size = 0x1000;
  zx_status_t status = regions->GetAllocSpot(&alloc_spot, align_pow2, /*entropy=*/0, alloc_size,
                                             base, size, /*prng=*/nullptr);
  EXPECT_EQ(ZX_OK, status);
  EXPECT_EQ(base, alloc_spot);

  test_list.insert_region(alloc_spot, alloc_size);

  // Manually insert a sub region at [+0x2000, 0x3000].
  test_list.insert_region(base + 0x2000, alloc_size);

  // Try to allocate 2 page, since the gap is too small, we would allocate at [0x3000, 0x5000].
  alloc_size = 0x2000;
  status = regions->GetAllocSpot(&alloc_spot, align_pow2, /*entropy=*/0, alloc_size, base, size,
                                 /*prng=*/nullptr);
  EXPECT_EQ(ZX_OK, status);
  EXPECT_EQ(base + 0x3000, alloc_spot);
  test_list.insert_region(alloc_spot, alloc_size);

  EXPECT_TRUE(test_list.remove_region(base + 0x2000));

  // After we remove the region, we now have a gap at [0x1000, 0x3000].
  alloc_size = 0x2000;
  status = regions->GetAllocSpot(&alloc_spot, align_pow2, /*entropy=*/0, alloc_size, base, size,
                                 /*prng=*/nullptr);
  EXPECT_EQ(ZX_OK, status);
  EXPECT_EQ(base + 0x1000, alloc_spot);
  test_list.insert_region(alloc_spot, alloc_size);

  // Now we have fill all the gaps, next region should start at 0x5000.
  alloc_size = 0x1000;
  status = regions->GetAllocSpot(&alloc_spot, align_pow2, /*entropy=*/0, alloc_size, base, size,
                                 /*prng=*/nullptr);
  EXPECT_EQ(ZX_OK, status);
  EXPECT_EQ(base + 0x5000, alloc_spot);
  test_list.insert_region(alloc_spot, alloc_size);

  // Test for possible overflow cases. We try to allocate all the rest of the spaces. The last
  // region should be from [0x6000, base + size - 1], we should be able to find this region and
  // allocate all the size from it.
  alloc_size = size - 0x6000;
  status = regions->GetAllocSpot(&alloc_spot, align_pow2, /*entropy=*/0, alloc_size, base, size,
                                 /*prng=*/nullptr);
  EXPECT_EQ(ZX_OK, status);
  EXPECT_EQ(base + 0x6000, alloc_spot);

  END_TEST;
}

static bool region_list_get_alloc_spot_no_memory_test() {
  BEGIN_TEST;

  TestRegionList test_list;
  auto regions = test_list.get_regions();
  vaddr_t base = 0xFFFF000000000000;
  vaddr_t size = 0x0001000000000000;
  // Set the align to be 0x1000.
  uint8_t align_pow2 = 12;

  test_list.insert_region(base, size - 0x1000);

  size_t alloc_size = 0x2000;
  vaddr_t alloc_spot = 0;
  // There is only a 1 page gap, and we are asking for two pages, so ZX_ERR_NO_RESOURCES should be
  // returned.
  zx_status_t status =
      regions->GetAllocSpot(&alloc_spot, align_pow2, /*entropy=*/0, alloc_size, base, size,
                            /*prng=*/nullptr);
  EXPECT_EQ(ZX_ERR_NO_RESOURCES, status);

  END_TEST;
}

static bool region_list_find_region_test() {
  BEGIN_TEST;

  TestRegionList test_list;
  auto regions = test_list.get_regions();
  vaddr_t base = 0xFFFF000000000000;

  auto region = regions->FindRegion(base);
  EXPECT_EQ(region, nullptr);

  test_list.insert_region(base + 0x1000, 0x1000);

  region = regions->FindRegion(base + 1);
  EXPECT_EQ(region, nullptr);

  region = regions->FindRegion(base + 0x1001);
  EXPECT_NE(region, nullptr);
  EXPECT_EQ(base + 0x1000, region->base());
  EXPECT_EQ((size_t)0x1000, region->size());

  END_TEST;
}

static bool region_list_include_or_higher_test() {
  BEGIN_TEST;

  TestRegionList test_list;
  auto regions = test_list.get_regions();
  vaddr_t base = 0xFFFF000000000000;

  test_list.insert_region(base + 0x1000, 0x1000);

  auto itr = regions->IncludeOrHigher(base + 1);
  EXPECT_TRUE(itr.IsValid());
  EXPECT_EQ(base + 0x1000, (*itr).second->base());
  EXPECT_EQ((size_t)0x1000, (*itr).second->size());

  itr = regions->IncludeOrHigher(base + 0x1001);
  EXPECT_TRUE(itr.IsValid());
  EXPECT_EQ(base + 0x1000, (*itr).second->base());
  EXPECT_EQ((size_t)0x1000, (*itr).second->size());

  itr = regions->IncludeOrHigher(base + 0x2000);
  EXPECT_FALSE(itr.IsValid());

  END_TEST;
}

static bool region_list_upper_bound_test() {
  BEGIN_TEST;

  TestRegionList test_list;
  auto regions = test_list.get_regions();
  vaddr_t base = 0xFFFF000000000000;

  test_list.insert_region(base + 0x1000, 0x1000);

  auto itr = regions->UpperBound(base + 0xFFF);
  EXPECT_TRUE(itr.IsValid());
  EXPECT_EQ(base + 0x1000, (*itr).second->base());
  EXPECT_EQ((size_t)0x1000, (*itr).second->size());

  itr = regions->UpperBound(base + 0x1000);
  EXPECT_FALSE(itr.IsValid());

  END_TEST;
}

static bool region_list_is_range_available_test() {
  BEGIN_TEST;

  TestRegionList test_list;
  auto regions = test_list.get_regions();
  vaddr_t base = 0xFFFF000000000000;

  test_list.insert_region(base + 0x1000, 0x1000);
  test_list.insert_region(base + 0x3000, 0x1000);

  EXPECT_TRUE(regions->IsRangeAvailable(base, 0x1000));
  EXPECT_FALSE(regions->IsRangeAvailable(base, 0x1001));
  EXPECT_FALSE(regions->IsRangeAvailable(base + 1, 0x1000));
  EXPECT_TRUE(regions->IsRangeAvailable(base + 0x2000, 1));
  EXPECT_FALSE(regions->IsRangeAvailable(base + 0x1FFF, 0x2000));

  EXPECT_TRUE(regions->IsRangeAvailable(0xFFFFFFFFFFFFFFFF, 1));
  EXPECT_FALSE(regions->IsRangeAvailable(base, 0x0001000000000000));

  END_TEST;
}

UNITTEST_START_TESTCASE(aspace_tests)
VM_UNITTEST(vmaspace_nested_attribution_test)
VM_UNITTEST(vm_mapping_attribution_commit_decommit_test)
VM_UNITTEST(vm_mapping_attribution_map_unmap_test)
VM_UNITTEST(vm_mapping_attribution_merge_test)
VM_UNITTEST(vm_mapping_page_fault_optimisation_test)
VM_UNITTEST(vm_mapping_page_fault_optimization_pt_limit_test)
VM_UNITTEST(vm_mapping_page_fault_range_test)
VM_UNITTEST(arch_noncontiguous_map)
VM_UNITTEST(arch_noncontiguous_map_with_upgrade)
VM_UNITTEST(arch_vm_aspace_protect_split_pages)
VM_UNITTEST(arch_vm_aspace_protect_split_pages_out_of_memory)
VM_UNITTEST(vm_kernel_region_test)
VM_UNITTEST(region_list_get_alloc_spot_test)
VM_UNITTEST(region_list_get_alloc_spot_no_memory_test)
VM_UNITTEST(region_list_find_region_test)
VM_UNITTEST(region_list_include_or_higher_test)
VM_UNITTEST(region_list_upper_bound_test)
VM_UNITTEST(region_list_is_range_available_test)
UNITTEST_END_TESTCASE(aspace_tests, "aspace", "VmAspace / ArchVmAspace / VMAR tests")

}  // namespace vm_unittest
