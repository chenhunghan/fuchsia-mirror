// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <bits.h>
#include <lib/unittest/unittest.h>
#include <lib/zircon-internal/macros.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <arch/arm64/mmu.h>
#include <arch/aspace.h>
#include <vm/arch_vm_aspace.h>
#include <vm/physmap.h>
#include <vm/pmm.h>
#include <vm/vm_aspace.h>

namespace {

using ArchUnmapOptions = ArchVmAspaceInterface::ArchUnmapOptions;

constexpr size_t kTestVirtualAddress = (1UL << 30);  // arbitrary address

bool arm64_test_perms() {
  BEGIN_TEST;

  ArmArchVmAspace aspace(USER_ASPACE_BASE, USER_ASPACE_SIZE, ArmAspaceType::kUser);
  EXPECT_EQ(ZX_OK, aspace.Init());

  auto map_query_test = [&](arch_mmu_flags_t mmu_perms) -> bool {
    paddr_t pa = 0;
    vm_page_t* vm_page;
    pmm_alloc_page(/*alloc_flags=*/0, &vm_page, &pa);
    EXPECT_EQ(ZX_OK, aspace.Map(kTestVirtualAddress, &pa, 1, mmu_perms,
                                ArchVmAspaceInterface::ExistingEntryAction::Error));

    paddr_t query_pa;
    arch_mmu_flags_t query_flags;
    EXPECT_EQ(ZX_OK, aspace.Query(kTestVirtualAddress, &query_pa, &query_flags));
    EXPECT_EQ(pa, query_pa);
    EXPECT_EQ(mmu_perms, query_flags);

    // FUTURE ENHANCEMENT: use a private api to read the terminal page table entry
    // and validate the bits that were set.

    EXPECT_EQ(ZX_OK, aspace.Unmap(kTestVirtualAddress, 1, ArchUnmapOptions::None));

    return all_ok;
  };

  // map nox page, query to see that X bit isn't set
  map_query_test(ARCH_MMU_FLAG_PERM_READ);
  map_query_test(ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE);
  map_query_test(ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_EXECUTE);
  map_query_test(ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE | ARCH_MMU_FLAG_PERM_EXECUTE);

  // map X page, query to see that X bit is set
  map_query_test(ARCH_MMU_FLAG_PERM_USER | ARCH_MMU_FLAG_PERM_READ);
  map_query_test(ARCH_MMU_FLAG_PERM_USER | ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE);
  map_query_test(ARCH_MMU_FLAG_PERM_USER | ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_EXECUTE);
  map_query_test(ARCH_MMU_FLAG_PERM_USER | ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE |
                 ARCH_MMU_FLAG_PERM_EXECUTE);

  // TODO: https://fxbug.dev/42169684 Add a more comprehensive test that reads back the page table
  // entries and all the translation tables leading up to it to make sure the permission bits are
  // set properly. Requires plumbing through some code to the ArmArchVmAspace to return a copy of
  // all the levels of the translation tables and terminal entry.

  EXPECT_EQ(ZX_OK, aspace.Destroy());

  END_TEST;
}

bool arm64_test_destroy_without_init() {
  BEGIN_TEST;

  // See that it's OK to Destroy even if Init was never called.
  ArmArchVmAspace aspace(USER_ASPACE_BASE, USER_ASPACE_SIZE, ArmAspaceType::kUser);
  ASSERT_OK(aspace.Destroy());

  // See that double Destroy is also OK.
  ASSERT_OK(aspace.Destroy());

  END_TEST;
}

bool arm64_test_non_physmap_exec() {
  BEGIN_TEST;

  ArmArchVmAspace aspace(USER_ASPACE_BASE, USER_ASPACE_SIZE, ArmAspaceType::kUser);
  ASSERT_OK(aspace.Init());

  // Choose a physical address outside the physmap range to verify that cache maintenance during
  // executable mappings does not attempt to dereference an unmapped physmap virtual address.
  paddr_t pa = RoundUpPageSize(gPhysmapSize);
  ASSERT_FALSE(is_physmap_phys_addr(pa));

  constexpr arch_mmu_flags_t kRxFlags =
      ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_EXECUTE | ARCH_MMU_FLAG_CACHED;
  constexpr arch_mmu_flags_t kRoFlags = ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_CACHED;

  EXPECT_OK(aspace.Map(kTestVirtualAddress, &pa, 1, kRxFlags,
                       ArchVmAspaceInterface::ExistingEntryAction::Error));
  paddr_t query_pa = 0;
  arch_mmu_flags_t query_flags = 0;
  EXPECT_OK(aspace.Query(kTestVirtualAddress, &query_pa, &query_flags));
  EXPECT_EQ(pa, query_pa);
  EXPECT_EQ(kRxFlags, query_flags);
  EXPECT_OK(aspace.Unmap(kTestVirtualAddress, 1, ArchUnmapOptions::None));

  EXPECT_OK(aspace.MapContiguous(kTestVirtualAddress, pa, 1, kRxFlags));
  EXPECT_OK(aspace.Unmap(kTestVirtualAddress, 1, ArchUnmapOptions::None));

  EXPECT_OK(aspace.Map(kTestVirtualAddress, &pa, 1, kRoFlags,
                       ArchVmAspaceInterface::ExistingEntryAction::Error));
  EXPECT_OK(aspace.Protect(kTestVirtualAddress, 1, kRxFlags, ArchUnmapOptions::None));
  EXPECT_OK(aspace.Unmap(kTestVirtualAddress, 1, ArchUnmapOptions::None));

  EXPECT_OK(aspace.Destroy());

  END_TEST;
}

}  // anonymous namespace

UNITTEST_START_TESTCASE(arm64_mmu_tests)
UNITTEST("perms", arm64_test_perms)
UNITTEST("destroy-without-init", arm64_test_destroy_without_init)
UNITTEST("non-physmap-exec", arm64_test_non_physmap_exec)
UNITTEST_END_TESTCASE(arm64_mmu_tests, "arm64_mmu", "arm64 mmu tests")
