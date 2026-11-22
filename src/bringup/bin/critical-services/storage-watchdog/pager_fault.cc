// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/bringup/bin/critical-services/storage-watchdog/pager_fault.h"

#include <string.h>

#include <string_view>
#include <unordered_set>
#include <utility>
#include <vector>

#ifdef __Fuchsia__
#include <lib/tbi/tbi.h>
#include <zircon/syscalls.h>
#include <zircon/syscalls/object.h>

#include <task-utils/walker.h>

#include "src/lib/fsl/handles/object_info.h"
#endif

namespace storage_watchdog {
namespace {

// ZX_INFO_PROCESS_MAPS can emit nested regions before inner mappings; find the matching mapping
// with a valid VMO koid containing |fault_vaddr|.
zx_koid_t FindMappedVmoKoid(uint64_t fault_vaddr, std::span<const MappingEntry> fault_mappings) {
  for (const MappingEntry& map : fault_mappings) {
    if (map.vmo_koid != ZX_KOID_INVALID && fault_vaddr >= map.base &&
        fault_vaddr - map.base < map.size) {
      return map.vmo_koid;
    }
  }
  return ZX_KOID_INVALID;
}

}  // namespace

std::optional<PagerOwner> ResolveTrustedPagerOwner(
    uint64_t fault_vaddr, std::span<const MappingEntry> fault_mappings, const VmoTable& vmo_table,
    const TrustedRootPagerVmos& trusted_root_pager_vmos, zx_koid_t* missing_koid_out) {
  if (missing_koid_out != nullptr) {
    *missing_koid_out = ZX_KOID_INVALID;
  }
  if (trusted_root_pager_vmos.empty()) {
    return std::nullopt;
  }

  zx_koid_t cur_koid = FindMappedVmoKoid(fault_vaddr, fault_mappings);
  if (cur_koid == ZX_KOID_INVALID) {
    return std::nullopt;
  }

  std::unordered_set<zx_koid_t> visited;
  for (size_t hop = 0; hop <= kMaxVmoParentHops && cur_koid != ZX_KOID_INVALID; ++hop) {
    if (!visited.insert(cur_koid).second) {
      return std::nullopt;
    }
    if (auto owner_it = trusted_root_pager_vmos.find(cur_koid);
        owner_it != trusted_root_pager_vmos.end()) {
      return owner_it->second;
    }
    auto vmo_it = vmo_table.find(cur_koid);
    if (vmo_it == vmo_table.end()) {
      if (missing_koid_out != nullptr) {
        *missing_koid_out = cur_koid;
      }
      return std::nullopt;
    }
    if (!vmo_it->second.pager_backed) {
      return std::nullopt;
    }
    cur_koid = vmo_it->second.parent_koid;
  }
  return std::nullopt;
}

std::optional<PagerOwner> ResolveTrustedPagerOwnerAcrossProcesses(
    uint64_t fault_vaddr, std::span<const MappingEntry> fault_mappings, VmoTable& vmo_table,
    const TrustedRootPagerVmos& trusted_root_pager_vmos,
    const std::function<bool(VmoTable& vmo_table)>& scan_next_process) {
  zx_koid_t missing_koid = ZX_KOID_INVALID;
  std::optional<PagerOwner> owner = ResolveTrustedPagerOwner(
      fault_vaddr, fault_mappings, vmo_table, trusted_root_pager_vmos, &missing_koid);
  while (!owner.has_value() && missing_koid != ZX_KOID_INVALID) {
    if (!scan_next_process(vmo_table)) {
      return std::nullopt;
    }
    if (!vmo_table.contains(missing_koid)) {
      continue;
    }
    owner = ResolveTrustedPagerOwner(fault_vaddr, fault_mappings, vmo_table,
                                     trusted_root_pager_vmos, &missing_koid);
  }
  return owner;
}

#ifdef __Fuchsia__
namespace {

void RecordPagerBackedVmos(std::span<const zx_info_vmo_t> vmos, VmoTable& vmo_table) {
  for (const zx_info_vmo_t& vmo : vmos) {
    if ((vmo.flags & ZX_INFO_VMO_PAGER_BACKED) != 0) {
      vmo_table.try_emplace(vmo.koid, VmoParentEntry{
                                          .koid = vmo.koid,
                                          .parent_koid = vmo.parent_koid,
                                          .pager_backed = true,
                                      });
    }
  }
}

void RecordPagerBackedVmos(const zx::process& process, VmoTable& vmo_table) {
  RecordPagerBackedVmos(GetProcessVmos(process), vmo_table);
}

// Checks whether |process| runs exclusively bootfs code. If |known_paged_exec_vmos| is provided,
// executable mappings whose VMO koid is already known to be pager-backed (such as the shared
// starnix_kernel executable across 100+ Starnix processes) fail fast from ZX_INFO_PROCESS_MAPS
// without querying ZX_INFO_PROCESS_VMOS. If |vmos_out| is non-null, stores the queried VMOs on
// success so callers do not re-query ZX_INFO_PROCESS_VMOS.
bool HasBootfsCodeOnly(const zx::process& process,
                       std::unordered_set<zx_koid_t>* known_paged_exec_vmos = nullptr,
                       std::vector<zx_info_vmo_t>* vmos_out = nullptr) {
  const std::vector<zx_info_maps_t> maps = GetProcessMaps(process);
  if (maps.empty()) {
    return false;
  }

  bool has_non_vdso_bootfs_exec = false;
  std::vector<zx_koid_t> exec_vmo_koids;
  for (const zx_info_maps_t& map : maps) {
    if (map.type != ZX_INFO_MAPS_TYPE_MAPPING ||
        (map.u.mapping.mmu_flags & ZX_VM_PERM_EXECUTE) == 0) {
      continue;
    }
    const zx_koid_t vmo_koid = map.u.mapping.vmo_koid;
    if (known_paged_exec_vmos != nullptr && known_paged_exec_vmos->contains(vmo_koid)) {
      return false;
    }
    exec_vmo_koids.push_back(vmo_koid);
    const std::string_view map_name(map.name, strnlen(map.name, sizeof(map.name)));
    if ((map.u.mapping.mmu_flags & ZX_VM_PERM_WRITE) == 0 && !map_name.starts_with("vdso/")) {
      has_non_vdso_bootfs_exec = true;
    }
  }
  if (!has_non_vdso_bootfs_exec) {
    return false;
  }

  // ZX_INFO_PROCESS_VMOS fails once the process is dying and can be truncated, so require every
  // executable mapping's VMO to appear in the non-pager-backed set.
  std::vector<zx_info_vmo_t> vmos = GetProcessVmos(process);
  std::unordered_set<zx_koid_t> non_paged_vmos;
  for (const zx_info_vmo_t& vmo : vmos) {
    if ((vmo.flags & ZX_INFO_VMO_PAGER_BACKED) == 0) {
      non_paged_vmos.insert(vmo.koid);
    } else if (known_paged_exec_vmos != nullptr) {
      known_paged_exec_vmos->insert(vmo.koid);
    }
  }
  for (const zx_koid_t vmo_koid : exec_vmo_koids) {
    if (!non_paged_vmos.contains(vmo_koid)) {
      return false;
    }
  }
  if (vmos_out != nullptr) {
    *vmos_out = std::move(vmos);
  }
  return true;
}

bool HasPagerHandle(const zx::process& process) {
  zx_info_process_handle_stats_t stats = {};
  return process.get_info(ZX_INFO_PROCESS_HANDLE_STATS, &stats, sizeof(stats), nullptr, nullptr) ==
             ZX_OK &&
         stats.handle_count[ZX_OBJ_TYPE_PAGER] > 0;
}

class PagerProcessWalker final : public TaskEnumerator {
 public:
  std::vector<zx::process> TakeProcesses() { return std::move(processes_); }

  zx_status_t OnProcess(int, zx_handle_t process, zx_koid_t, zx_koid_t) override {
    zx::process dup;
    if (zx_handle_duplicate(process, ZX_RIGHT_SAME_RIGHTS, dup.reset_and_get_address()) == ZX_OK) {
      processes_.push_back(std::move(dup));
    }
    return ZX_OK;
  }

 protected:
  bool has_on_process() const override { return true; }

 private:
  std::vector<zx::process> processes_;
};

}  // namespace

uint64_t ExtractFaultingAddress(const zx_exception_report_t& report) {
  if (report.header.type != ZX_EXCP_FATAL_PAGE_FAULT) {
    return 0;
  }
#ifdef __aarch64__
  // FAR_EL1 preserves AArch64 TBI tags, while ZX_INFO_PROCESS_MAPS reports untagged addresses.
  return tbi::RemoveTag(report.context.arch.u.arm_64.far);
#elifdef __x86_64__
  return report.context.arch.u.x86_64.cr2;
#elifdef __riscv
  return report.context.arch.u.riscv_64.tval;
#else
#error Unsupported architecture
#endif
}

bool IsBootfsPagerProcess(const zx::process& process) {
  return HasPagerHandle(process) && HasBootfsCodeOnly(process);
}

std::optional<PagerOwner> FindTrustedPagerFaultOwner(const zx::process& faulting_process,
                                                     uint64_t fault_vaddr,
                                                     const zx::job& root_job) {
  if (fault_vaddr == 0 || !faulting_process.is_valid() || !root_job.is_valid()) {
    return std::nullopt;
  }

  const std::vector<zx_info_maps_t> raw_maps = GetProcessMaps(faulting_process);
  std::vector<MappingEntry> fault_mappings;
  fault_mappings.reserve(raw_maps.size());
  for (const zx_info_maps_t& map : raw_maps) {
    if (map.type == ZX_INFO_MAPS_TYPE_MAPPING) {
      fault_mappings.push_back(MappingEntry{
          .base = map.base,
          .size = map.size,
          .vmo_koid = map.u.mapping.vmo_koid,
      });
    }
  }

  VmoTable vmo_table;
  RecordPagerBackedVmos(faulting_process, vmo_table);

  // Bail out before walking the job tree if |fault_vaddr| is not in a pager-backed mapping.
  if (!vmo_table.contains(FindMappedVmoKoid(fault_vaddr, fault_mappings))) {
    return std::nullopt;
  }

  PagerProcessWalker walker;
  if (walker.WalkJobTree(root_job.get()) != ZX_OK) {
    return std::nullopt;
  }
  const std::vector<zx::process> processes = walker.TakeProcesses();

  TrustedRootPagerVmos trusted_root_pager_vmos;
  std::vector<const zx::process*> other_processes;
  other_processes.reserve(processes.size());
  std::unordered_set<zx_koid_t> known_paged_exec_vmos;
  for (const zx::process& proc : processes) {
    std::vector<zx_info_vmo_t> proc_vmos;
    if (!HasPagerHandle(proc) || !HasBootfsCodeOnly(proc, &known_paged_exec_vmos, &proc_vmos)) {
      other_processes.push_back(&proc);
      continue;
    }
    const PagerOwner owner = {
        .name = fsl::GetObjectName(proc.get()),
        .process_koid = fsl::GetKoid(proc.get()),
    };
    RecordPagerBackedVmos(proc_vmos, vmo_table);
    for (const zx_info_vmo_t& vmo : proc_vmos) {
      // Only root pager VMOs (created directly via zx_pager_create_vmo, where
      // parent_koid == ZX_KOID_INVALID) held via handle in a bootfs pager process qualify as
      // trusted roots.
      if ((vmo.flags & ZX_INFO_VMO_PAGER_BACKED) != 0 &&
          (vmo.flags & ZX_INFO_VMO_VIA_HANDLE) != 0 && vmo.parent_koid == ZX_KOID_INVALID) {
        trusted_root_pager_vmos.try_emplace(vmo.koid, owner);
      }
    }
  }

  if (trusted_root_pager_vmos.empty()) {
    return std::nullopt;
  }

  auto next = other_processes.begin();
  return ResolveTrustedPagerOwnerAcrossProcesses(fault_vaddr, fault_mappings, vmo_table,
                                                 trusted_root_pager_vmos, [&](VmoTable& table) {
                                                   if (next == other_processes.end()) {
                                                     return false;
                                                   }
                                                   RecordPagerBackedVmos(**next++, table);
                                                   return true;
                                                 });
}

#endif  // __Fuchsia__

}  // namespace storage_watchdog
