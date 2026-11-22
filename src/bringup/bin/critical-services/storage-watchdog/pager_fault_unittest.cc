// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/bringup/bin/critical-services/storage-watchdog/pager_fault.h"

#include <algorithm>
#include <cstring>
#include <functional>
#include <optional>
#include <string>
#include <utility>
#include <vector>

#include <gtest/gtest.h>

#ifdef __Fuchsia__
#include <lib/fit/defer.h>
#include <lib/zx/job.h>
#include <lib/zx/pager.h>
#include <lib/zx/port.h>
#include <lib/zx/process.h>
#include <lib/zx/vmar.h>
#include <lib/zx/vmo.h>
#include <zircon/syscalls.h>
#include <zircon/syscalls/object.h>

#include <unordered_set>

#include "src/lib/fsl/handles/object_info.h"
#endif

namespace storage_watchdog {
namespace {

constexpr uint64_t kPage = 4096;

VmoTable Table(const std::vector<VmoParentEntry>& entries) {
  VmoTable table;
  for (const VmoParentEntry& entry : entries) {
    table[entry.koid] = entry;
  }
  return table;
}

PagerOwner Owner(std::string name) { return {.name = std::move(name), .process_koid = 1}; }

std::optional<std::string> Name(const std::optional<PagerOwner>& owner) {
  if (!owner.has_value()) {
    return std::nullopt;
  }
  return owner->name;
}

TEST(PagerFaultTest, DirectChildOfTrustedRoot) {
  const std::vector<MappingEntry> maps = {{.base = 0x1000, .size = 4 * kPage, .vmo_koid = 11}};
  const auto table = Table({
      {.koid = 11, .parent_koid = 10, .pager_backed = true},
      {.koid = 10, .parent_koid = ZX_KOID_INVALID, .pager_backed = true},
  });
  const TrustedRootPagerVmos trusted = {{10, Owner("driver_host")}};
  EXPECT_EQ(Name(ResolveTrustedPagerOwner(0x1000 + kPage, maps, table, trusted)), "driver_host");
}

TEST(PagerFaultTest, MappingOfTrustedRootItself) {
  const std::vector<MappingEntry> maps = {{.base = 0x1000, .size = kPage, .vmo_koid = 10}};
  const auto table = Table({{.koid = 10, .parent_koid = ZX_KOID_INVALID, .pager_backed = true}});
  const TrustedRootPagerVmos trusted = {{10, Owner("fxfs.cm")}};
  EXPECT_EQ(Name(ResolveTrustedPagerOwner(0x1000, maps, table, trusted)), "fxfs.cm");
}

TEST(PagerFaultTest, MultiLevelCowGrandchild) {
  const std::vector<MappingEntry> maps = {
      {.base = 0x10000, .size = kPage, .vmo_koid = ZX_KOID_INVALID},  // VMAR region, no VMO.
      {.base = 0x10000, .size = kPage, .vmo_koid = 33},
  };
  const auto table = Table({
      {.koid = 33, .parent_koid = 32, .pager_backed = true},
      {.koid = 32, .parent_koid = 31, .pager_backed = true},
      {.koid = 31, .parent_koid = 30, .pager_backed = true},
  });
  const TrustedRootPagerVmos trusted = {{30, Owner("driver_host")}};
  EXPECT_EQ(Name(ResolveTrustedPagerOwner(0x10010, maps, table, trusted)), "driver_host");
}

TEST(PagerFaultTest, IntermediateVmoMissingIsReported) {
  const std::vector<MappingEntry> maps = {{.base = 0x1000, .size = kPage, .vmo_koid = 43}};
  // 42 (the parent of 43) is held only by a third process and is absent from the table.
  auto table = Table({{.koid = 43, .parent_koid = 42, .pager_backed = true}});
  const TrustedRootPagerVmos trusted = {{40, Owner("driver_host")}};
  zx_koid_t missing = 7;
  EXPECT_EQ(ResolveTrustedPagerOwner(0x1000, maps, table, trusted, &missing), std::nullopt);
  EXPECT_EQ(missing, 42u);

  // Once the third process's VMOs are added, the chain resolves.
  table[42] = {.koid = 42, .parent_koid = 40, .pager_backed = true};
  EXPECT_EQ(Name(ResolveTrustedPagerOwner(0x1000, maps, table, trusted, &missing)), "driver_host");
  EXPECT_EQ(missing, ZX_KOID_INVALID);
}

TEST(PagerFaultTest, MissingKoidIsInvalidWhenChainEndsInTable) {
  const std::vector<MappingEntry> maps = {{.base = 0x1000, .size = kPage, .vmo_koid = 61}};
  // An untrusted root held by the faulting process itself: nothing is missing.
  const auto table = Table({
      {.koid = 61, .parent_koid = 60, .pager_backed = true},
      {.koid = 60, .parent_koid = ZX_KOID_INVALID, .pager_backed = true},
  });
  const TrustedRootPagerVmos trusted = {{99, Owner("driver_host")}};
  zx_koid_t missing = 7;
  EXPECT_EQ(ResolveTrustedPagerOwner(0x1000, maps, table, trusted, &missing), std::nullopt);
  EXPECT_EQ(missing, ZX_KOID_INVALID);
  missing = 7;
  EXPECT_EQ(ResolveTrustedPagerOwner(0x9000, maps, table, trusted, &missing), std::nullopt);
  EXPECT_EQ(missing, ZX_KOID_INVALID);
}

class FakeProcessScan {
 public:
  explicit FakeProcessScan(std::vector<std::vector<VmoParentEntry>> processes)
      : processes_(std::move(processes)) {}

  std::function<bool(VmoTable&)> Scanner() {
    return [this](VmoTable& table) {
      if (scanned_ == processes_.size()) {
        return false;
      }
      for (const VmoParentEntry& entry : processes_[scanned_]) {
        table.try_emplace(entry.koid, entry);
      }
      scanned_++;
      return true;
    };
  }

  size_t scanned() const { return scanned_; }

 private:
  std::vector<std::vector<VmoParentEntry>> processes_;
  size_t scanned_ = 0;
};

TEST(PagerFaultTest, ScanAcrossProcessesStopsOnceChainResolves) {
  const std::vector<MappingEntry> maps = {{.base = 0x1000, .size = kPage, .vmo_koid = 43}};
  VmoTable table = Table({{.koid = 43, .parent_koid = 42, .pager_backed = true}});
  const TrustedRootPagerVmos trusted = {{40, Owner("driver_host")}};
  FakeProcessScan scan({
      {{.koid = 100, .parent_koid = ZX_KOID_INVALID, .pager_backed = true}},
      {{.koid = 42, .parent_koid = 40, .pager_backed = true}},
      {{.koid = 200, .parent_koid = ZX_KOID_INVALID, .pager_backed = true}},
      {{.koid = 300, .parent_koid = ZX_KOID_INVALID, .pager_backed = true}},
  });
  EXPECT_EQ(
      Name(ResolveTrustedPagerOwnerAcrossProcesses(0x1000, maps, table, trusted, scan.Scanner())),
      "driver_host");
  EXPECT_EQ(scan.scanned(), 2u);
}

TEST(PagerFaultTest, ScanAcrossProcessesStopsAtUntrustedRoot) {
  const std::vector<MappingEntry> maps = {{.base = 0x1000, .size = kPage, .vmo_koid = 43}};
  VmoTable table = Table({{.koid = 43, .parent_koid = 42, .pager_backed = true}});
  const TrustedRootPagerVmos trusted = {{40, Owner("driver_host")}};
  // Missing parent 42 is an untrusted pager's root (parent_koid == ZX_KOID_INVALID), which
  // terminates the walk.
  FakeProcessScan scan({
      {{.koid = 42, .parent_koid = ZX_KOID_INVALID, .pager_backed = true}},
      {{.koid = 200, .parent_koid = ZX_KOID_INVALID, .pager_backed = true}},
      {{.koid = 300, .parent_koid = ZX_KOID_INVALID, .pager_backed = true}},
  });
  EXPECT_EQ(ResolveTrustedPagerOwnerAcrossProcesses(0x1000, maps, table, trusted, scan.Scanner()),
            std::nullopt);
  EXPECT_EQ(scan.scanned(), 1u);
}

TEST(PagerFaultTest, ScanAcrossProcessesFollowsMultipleMissingHops) {
  const std::vector<MappingEntry> maps = {{.base = 0x1000, .size = kPage, .vmo_koid = 43}};
  VmoTable table = Table({{.koid = 43, .parent_koid = 42, .pager_backed = true}});
  const TrustedRootPagerVmos trusted = {{40, Owner("driver_host")}};
  FakeProcessScan scan({
      {{.koid = 42, .parent_koid = 41, .pager_backed = true}},
      {{.koid = 200, .parent_koid = ZX_KOID_INVALID, .pager_backed = true}},
      {{.koid = 41, .parent_koid = 40, .pager_backed = true}},
      {{.koid = 300, .parent_koid = ZX_KOID_INVALID, .pager_backed = true}},
  });
  EXPECT_EQ(
      Name(ResolveTrustedPagerOwnerAcrossProcesses(0x1000, maps, table, trusted, scan.Scanner())),
      "driver_host");
  EXPECT_EQ(scan.scanned(), 3u);
}

TEST(PagerFaultTest, ScanAcrossProcessesNotStartedWhenNothingIsMissing) {
  const std::vector<MappingEntry> maps = {{.base = 0x1000, .size = kPage, .vmo_koid = 61}};
  VmoTable table = Table({
      {.koid = 61, .parent_koid = 60, .pager_backed = true},
      {.koid = 60, .parent_koid = ZX_KOID_INVALID, .pager_backed = true},
  });
  const TrustedRootPagerVmos trusted = {{99, Owner("driver_host")}};
  FakeProcessScan scan({{{.koid = 200, .parent_koid = ZX_KOID_INVALID, .pager_backed = true}}});
  EXPECT_EQ(ResolveTrustedPagerOwnerAcrossProcesses(0x1000, maps, table, trusted, scan.Scanner()),
            std::nullopt);
  EXPECT_EQ(scan.scanned(), 0u);
}

TEST(PagerFaultTest, ScanAcrossProcessesGivesUpWhenNeverFound) {
  const std::vector<MappingEntry> maps = {{.base = 0x1000, .size = kPage, .vmo_koid = 43}};
  VmoTable table = Table({{.koid = 43, .parent_koid = 42, .pager_backed = true}});
  const TrustedRootPagerVmos trusted = {{40, Owner("driver_host")}};
  FakeProcessScan scan({
      {{.koid = 200, .parent_koid = ZX_KOID_INVALID, .pager_backed = true}},
      {{.koid = 300, .parent_koid = ZX_KOID_INVALID, .pager_backed = true}},
  });
  EXPECT_EQ(ResolveTrustedPagerOwnerAcrossProcesses(0x1000, maps, table, trusted, scan.Scanner()),
            std::nullopt);
  EXPECT_EQ(scan.scanned(), 2u);
}

TEST(PagerFaultTest, CycleIsRejected) {
  const std::vector<MappingEntry> maps = {{.base = 0x1000, .size = kPage, .vmo_koid = 51}};
  const auto table = Table({
      {.koid = 51, .parent_koid = 52, .pager_backed = true},
      {.koid = 52, .parent_koid = 51, .pager_backed = true},
  });
  const TrustedRootPagerVmos trusted = {{50, Owner("driver_host")}};
  EXPECT_EQ(ResolveTrustedPagerOwner(0x1000, maps, table, trusted), std::nullopt);
}

TEST(PagerFaultTest, ChainLongerThanHopLimitIsRejected) {
  const std::vector<MappingEntry> maps = {{.base = 0x1000, .size = kPage, .vmo_koid = 1000}};
  std::vector<VmoParentEntry> entries;
  for (zx_koid_t koid = 1000; koid < 1000 + kMaxVmoParentHops + 1; ++koid) {
    entries.push_back({.koid = koid, .parent_koid = koid + 1, .pager_backed = true});
  }
  const auto table = Table(entries);
  EXPECT_EQ(Name(ResolveTrustedPagerOwner(0x1000, maps, table,
                                          {{1000 + kMaxVmoParentHops, Owner("driver_host")}})),
            "driver_host");
  EXPECT_EQ(ResolveTrustedPagerOwner(0x1000, maps, table,
                                     {{1000 + kMaxVmoParentHops + 1, Owner("driver_host")}}),
            std::nullopt);
}

TEST(PagerFaultTest, UntrustedPagerRootIsRejected) {
  // A pager-backed VMO whose root belongs to an untrusted pager (e.g. a Starnix or test pager).
  const std::vector<MappingEntry> maps = {{.base = 0x1000, .size = kPage, .vmo_koid = 61}};
  const auto table = Table({
      {.koid = 61, .parent_koid = 60, .pager_backed = true},
      {.koid = 60, .parent_koid = ZX_KOID_INVALID, .pager_backed = true},
  });
  const TrustedRootPagerVmos trusted = {{99, Owner("driver_host")}};
  EXPECT_EQ(ResolveTrustedPagerOwner(0x1000, maps, table, trusted), std::nullopt);
}

TEST(PagerFaultTest, NonPagerBackedVmoIsRejected) {
  const std::vector<MappingEntry> maps = {{.base = 0x1000, .size = kPage, .vmo_koid = 71}};
  const auto table = Table({{.koid = 71, .parent_koid = 70, .pager_backed = false}});
  const TrustedRootPagerVmos trusted = {{70, Owner("driver_host")}};
  EXPECT_EQ(ResolveTrustedPagerOwner(0x1000, maps, table, trusted), std::nullopt);
}

TEST(PagerFaultTest, UnmappedAddressIsRejected) {
  const std::vector<MappingEntry> maps = {{.base = 0x1000, .size = kPage, .vmo_koid = 81}};
  const auto table = Table({{.koid = 81, .parent_koid = 80, .pager_backed = true}});
  const TrustedRootPagerVmos trusted = {{80, Owner("driver_host")}};
  EXPECT_EQ(ResolveTrustedPagerOwner(0x1000 + kPage, maps, table, trusted), std::nullopt);
  EXPECT_EQ(ResolveTrustedPagerOwner(0x0fff, maps, table, trusted), std::nullopt);
}

TEST(PagerFaultTest, NoTrustedRootsIsRejected) {
  const std::vector<MappingEntry> maps = {{.base = 0x1000, .size = kPage, .vmo_koid = 91}};
  const auto table = Table({{.koid = 91, .parent_koid = ZX_KOID_INVALID, .pager_backed = true}});
  EXPECT_EQ(ResolveTrustedPagerOwner(0x1000, maps, table, {}), std::nullopt);
}

TEST(PagerFaultTest, WedgeDumpFocusFormattingOmitsInvalidKoids) {
  EXPECT_EQ((WedgeDumpFocus{}.FormatLine("STORAGE WATCHDOG")), std::nullopt);
  EXPECT_EQ((WedgeDumpFocus{.faulting_process_koid = 42}.FormatLine("STORAGE WATCHDOG")),
            "STORAGE WATCHDOG: faulting process 42");
  EXPECT_EQ((WedgeDumpFocus{
                .pager_process_name = "pkg-cache.cm",
                .pager_process_koid = 17,
            }
                 .FormatLine("STORAGE WATCHDOG")),
            "STORAGE WATCHDOG: pager process pkg-cache.cm (17)");
  EXPECT_EQ((WedgeDumpFocus{
                .pager_process_name = "pkg-cache.cm",
                .pager_process_koid = 17,
                .faulting_process_koid = 42,
            }
                 .FormatLine("STORAGE WATCHDOG")),
            "STORAGE WATCHDOG: pager process pkg-cache.cm (17), faulting process 42");
}

#ifdef __Fuchsia__

struct PagedVmos {
  zx::pager pager;
  zx::port port;
  zx::vmo root;
  zx::vmo slice;
  zx::vmo snapshot;
};

PagedVmos CreatePagedVmoHierarchy() {
  const uint64_t page_size = zx_system_get_page_size();
  PagedVmos vmos;
  EXPECT_EQ(zx::pager::create(0, &vmos.pager), ZX_OK);
  EXPECT_EQ(zx::port::create(0, &vmos.port), ZX_OK);
  EXPECT_EQ(vmos.pager.create_vmo(0, vmos.port, 0, 4 * page_size, &vmos.root), ZX_OK);
  EXPECT_EQ(vmos.root.create_child(ZX_VMO_CHILD_SLICE, 0, 4 * page_size, &vmos.slice), ZX_OK);
  EXPECT_EQ(vmos.slice.create_child(ZX_VMO_CHILD_SNAPSHOT_AT_LEAST_ON_WRITE, 0, 2 * page_size,
                                    &vmos.snapshot),
            ZX_OK);
  return vmos;
}

bool HasPagedExecutableMapping(const zx::process& process) {
  std::unordered_set<zx_koid_t> paged_vmos;
  for (const zx_info_vmo_t& vmo : GetProcessVmos(process)) {
    if ((vmo.flags & ZX_INFO_VMO_PAGER_BACKED) != 0) {
      paged_vmos.insert(vmo.koid);
    }
  }
  const std::vector<zx_info_maps_t> maps = GetProcessMaps(process);
  return std::ranges::any_of(maps, [&paged_vmos](const zx_info_maps_t& map) {
    return map.type == ZX_INFO_MAPS_TYPE_MAPPING &&
           (map.u.mapping.mmu_flags & ZX_VM_PERM_EXECUTE) != 0 &&
           paged_vmos.contains(map.u.mapping.vmo_koid);
  });
}

// Validates the kernel invariants ResolveTrustedPagerOwner relies on: every clone of a pager VMO
// is pager-backed, links to its parent through parent_koid, and the root has parent_koid
// ZX_KOID_INVALID.
TEST(PagerFaultKernelTest, PagedCloneChainResolvesToRoot) {
  const uint64_t page_size = zx_system_get_page_size();
  PagedVmos vmos = CreatePagedVmoHierarchy();
  ASSERT_TRUE(vmos.snapshot.is_valid());

  zx_vaddr_t addr = 0;
  ASSERT_EQ(zx::vmar::root_self()->map(ZX_VM_PERM_READ, 0, vmos.snapshot, 0, 2 * page_size, &addr),
            ZX_OK);
  auto unmap = fit::defer(
      [addr, page_size] { EXPECT_EQ(zx::vmar::root_self()->unmap(addr, 2 * page_size), ZX_OK); });

  const std::vector<zx_info_vmo_t> infos = GetProcessVmos(*zx::process::self());
  ASSERT_FALSE(infos.empty());

  VmoTable table;
  for (const zx_info_vmo_t& info : infos) {
    table.try_emplace(info.koid, VmoParentEntry{
                                     .koid = info.koid,
                                     .parent_koid = info.parent_koid,
                                     .pager_backed = (info.flags & ZX_INFO_VMO_PAGER_BACKED) != 0,
                                 });
  }

  const zx_koid_t root_koid = fsl::GetKoid(vmos.root.get());
  const zx_koid_t snapshot_koid = fsl::GetKoid(vmos.snapshot.get());
  ASSERT_TRUE(table.contains(root_koid));
  EXPECT_EQ(table[root_koid].parent_koid, ZX_KOID_INVALID);
  EXPECT_TRUE(table[root_koid].pager_backed);
  ASSERT_TRUE(table.contains(snapshot_koid));
  EXPECT_TRUE(table[snapshot_koid].pager_backed);

  const std::vector<MappingEntry> maps = {
      {.base = addr, .size = 2 * page_size, .vmo_koid = snapshot_koid},
  };
  const TrustedRootPagerVmos trusted = {{root_koid, Owner("trusted-pager")}};
  EXPECT_EQ(Name(ResolveTrustedPagerOwner(addr + page_size, maps, table, trusted)),
            "trusted-pager");
}

// A process running package (pager-backed) code is never a trusted pager, even if it holds a
// pager object and renames itself to look like fxfs.
TEST(PagerFaultKernelTest, PackageProcessWithPagerIsNotTrusted) {
  const uint64_t page_size = zx_system_get_page_size();
  PagedVmos vmos = CreatePagedVmoHierarchy();
  ASSERT_TRUE(vmos.pager.is_valid());

  const std::string old_name = fsl::GetCurrentProcessName();
  ASSERT_EQ(fsl::SetObjectName(zx::process::self()->get(), "fxfs.cm"), ZX_OK);
  auto restore_name = fit::defer(
      [&old_name] { EXPECT_EQ(fsl::SetObjectName(zx::process::self()->get(), old_name), ZX_OK); });

  zx_info_process_handle_stats_t stats = {};
  ASSERT_EQ(zx::process::self()->get_info(ZX_INFO_PROCESS_HANDLE_STATS, &stats, sizeof(stats),
                                          nullptr, nullptr),
            ZX_OK);
  EXPECT_GT(stats.handle_count[ZX_OBJ_TYPE_PAGER], 0u);

  const bool paged_exec = HasPagedExecutableMapping(*zx::process::self());
  const bool trusted = IsBootfsPagerProcess(*zx::process::self());

  zx_vaddr_t addr = 0;
  ASSERT_EQ(zx::vmar::root_self()->map(ZX_VM_PERM_READ, 0, vmos.snapshot, 0, 2 * page_size, &addr),
            ZX_OK);
  auto unmap = fit::defer(
      [addr, page_size] { EXPECT_EQ(zx::vmar::root_self()->unmap(addr, 2 * page_size), ZX_OK); });
  const std::optional<PagerOwner> owner =
      FindTrustedPagerFaultOwner(*zx::process::self(), addr, *zx::job::default_job());

  if (!paged_exec) {
    GTEST_SKIP() << "test binary is not running from a package blob";
  }
  EXPECT_FALSE(trusted);
  EXPECT_EQ(owner, std::nullopt);
}

TEST(PagerFaultKernelTest, NonPagerMappingIsRejectedEarly) {
  const uint64_t page_size = zx_system_get_page_size();
  zx::vmo vmo;
  ASSERT_EQ(zx::vmo::create(page_size, 0, &vmo), ZX_OK);
  zx_vaddr_t addr = 0;
  ASSERT_EQ(zx::vmar::root_self()->map(ZX_VM_PERM_READ, 0, vmo, 0, page_size, &addr), ZX_OK);
  auto unmap = fit::defer(
      [addr, page_size] { EXPECT_EQ(zx::vmar::root_self()->unmap(addr, page_size), ZX_OK); });
  EXPECT_EQ(FindTrustedPagerFaultOwner(*zx::process::self(), 0, *zx::job::default_job()),
            std::nullopt);
  EXPECT_EQ(FindTrustedPagerFaultOwner(*zx::process::self(), addr, *zx::job::default_job()),
            std::nullopt);
}

TEST(PagerFaultKernelTest, ExtractFaultingAddressIgnoresOtherExceptions) {
  zx_exception_report_t report = {};
  report.header.type = ZX_EXCP_SW_BREAKPOINT;
  EXPECT_EQ(ExtractFaultingAddress(report), 0u);

  report.header.type = ZX_EXCP_FATAL_PAGE_FAULT;
#ifdef __aarch64__
  report.context.arch.u.arm_64.far = 0xb400'0071'2345'6000ULL;
  EXPECT_EQ(ExtractFaultingAddress(report), 0x0000'0071'2345'6000ULL);
#elifdef __x86_64__
  report.context.arch.u.x86_64.cr2 = 0x7123'4560'00ULL;
  EXPECT_EQ(ExtractFaultingAddress(report), 0x7123'4560'00ULL);
#elifdef __riscv
  report.context.arch.u.riscv_64.tval = 0x7123'4560'00ULL;
  EXPECT_EQ(ExtractFaultingAddress(report), 0x7123'4560'00ULL);
#endif
}

#endif  // __Fuchsia__

}  // namespace
}  // namespace storage_watchdog
