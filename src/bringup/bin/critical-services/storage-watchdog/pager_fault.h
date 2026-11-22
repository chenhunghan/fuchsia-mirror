// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_BRINGUP_BIN_CRITICAL_SERVICES_STORAGE_WATCHDOG_PAGER_FAULT_H_
#define SRC_BRINGUP_BIN_CRITICAL_SERVICES_STORAGE_WATCHDOG_PAGER_FAULT_H_

#include <stddef.h>
#include <stdint.h>
#include <zircon/types.h>

#include <functional>
#include <optional>
#include <span>
#include <string>
#include <string_view>
#include <unordered_map>
#include <vector>

#ifdef __Fuchsia__
#include <lib/zx/job.h>
#include <lib/zx/process.h>
#include <zircon/syscalls/exception.h>
#include <zircon/syscalls/object.h>
#endif

namespace storage_watchdog {

inline constexpr size_t kMaxVmoParentHops = 32;

struct MappingEntry {
  uint64_t base = 0;
  uint64_t size = 0;
  zx_koid_t vmo_koid = ZX_KOID_INVALID;
};

struct VmoParentEntry {
  zx_koid_t koid = ZX_KOID_INVALID;
  zx_koid_t parent_koid = ZX_KOID_INVALID;
  bool pager_backed = false;
};

// A trusted bootfs pager process, keyed in |TrustedRootPagerVmos| by the root pager VMOs it holds.
struct PagerOwner {
  std::string name;
  zx_koid_t process_koid = ZX_KOID_INVALID;

  bool operator==(const PagerOwner&) const = default;
};

// The processes a stall was traced to, which the dump covers first and in full, on top of its
// standard selection. Every field is optional (empty or ZX_KOID_INVALID means unknown).
struct WedgeDumpFocus {
  // The process serving the pager that a timed-out page fault waited on, e.g. pkg-cache.cm or a
  // storage driver host. Its threads can be stuck anywhere (a FIFO, channel, or futex), so all of
  // them are unwound.
  std::string pager_process_name;
  zx_koid_t pager_process_koid = ZX_KOID_INVALID;
  // The process whose page fault timed out. crashsvc holds that exception, so the faulting thread
  // is stopped in ZX_THREAD_STATE_BLOCKED_EXCEPTION rather than BLOCKED_PAGER; every thread of
  // this process in that state is unwound.
  zx_koid_t faulting_process_koid = ZX_KOID_INVALID;

  // Formats the focus header line (without trailing newline), or std::nullopt if no focus koid is
  // set.
  std::optional<std::string> FormatLine(std::string_view tag) const {
    if (pager_process_koid != ZX_KOID_INVALID && faulting_process_koid != ZX_KOID_INVALID) {
      return std::string(tag) + ": pager process " + pager_process_name + " (" +
             std::to_string(pager_process_koid) + "), faulting process " +
             std::to_string(faulting_process_koid);
    }
    if (pager_process_koid != ZX_KOID_INVALID) {
      return std::string(tag) + ": pager process " + pager_process_name + " (" +
             std::to_string(pager_process_koid) + ")";
    }
    if (faulting_process_koid != ZX_KOID_INVALID) {
      return std::string(tag) + ": faulting process " + std::to_string(faulting_process_koid);
    }
    return std::nullopt;
  }
};

using VmoTable = std::unordered_map<zx_koid_t, VmoParentEntry>;
using TrustedRootPagerVmos = std::unordered_map<zx_koid_t, PagerOwner>;

// Resolves |fault_vaddr| in |fault_mappings| to its backing VMO and walks |parent_koid| links in
// |vmo_table| (up to |kMaxVmoParentHops|) looking for a root pager VMO in
// |trusted_root_pager_vmos|.
//
// If |missing_koid_out| is non-null, sets it to the ancestor VMO koid that stopped the walk
// because it was absent from |vmo_table| (held only by another process), or ZX_KOID_INVALID.
std::optional<PagerOwner> ResolveTrustedPagerOwner(
    uint64_t fault_vaddr, std::span<const MappingEntry> fault_mappings, const VmoTable& vmo_table,
    const TrustedRootPagerVmos& trusted_root_pager_vmos, zx_koid_t* missing_koid_out = nullptr);

// Like ResolveTrustedPagerOwner(), but when the walk stops at an ancestor VMO absent from
// |vmo_table| (e.g. an intermediate clone in a multi-process COW chain, or an untrusted pager's
// root), calls |scan_next_process| one process at a time and re-evaluates as soon as the missing
// VMO appears. Stops once the chain resolves or terminates in a known VMO, avoiding a full scan
// across all processes (such as 100+ Starnix processes sharing a handle table).
std::optional<PagerOwner> ResolveTrustedPagerOwnerAcrossProcesses(
    uint64_t fault_vaddr, std::span<const MappingEntry> fault_mappings, VmoTable& vmo_table,
    const TrustedRootPagerVmos& trusted_root_pager_vmos,
    const std::function<bool(VmoTable& vmo_table)>& scan_next_process);

// Compiled on host as well for pager_fault_unittest, which exercises the pure VMO-ancestry
// resolution helpers above without Zircon handles.
#ifdef __Fuchsia__
// ZX_INFO_PROCESS_MAPS and ZX_INFO_PROCESS_VMOS can grow between calls, and Zircon emits
// higher-address and restricted-aspace entries last, so a truncated result would omit the
// pager-backed mappings checked by callers. Retries with proportional slack and returns {} on
// truncation or error so callers fail closed.
template <typename T>
std::vector<T> GetProcessInfo(const zx::process& process, zx_object_info_topic_t topic) {
  constexpr int kMaxAttempts = 4;
  std::vector<T> items;
  size_t actual = 0;
  size_t avail = 0;
  for (int attempt = 0; attempt < kMaxAttempts; ++attempt) {
    if (process.get_info(topic, items.data(), items.size() * sizeof(T), &actual, &avail) != ZX_OK) {
      return {};
    }
    if (actual == avail) {
      items.resize(actual);
      return items;
    }
    if (attempt + 1 < kMaxAttempts) {
      items.resize(avail + (avail / 4) + 16);
    }
  }
  return {};
}

// Targets that transitively add -I//zircon/system/public (via //src/lib/debug:backtrace-request)
// shadow the sysroot headers as non-system includes, causing clang-tidy to flag the signed int
// shift operands in __ZX_INFO_TOPIC.
inline std::vector<zx_info_maps_t> GetProcessMaps(const zx::process& process) {
  return GetProcessInfo<zx_info_maps_t>(process,
                                        ZX_INFO_PROCESS_MAPS);  // NOLINT(bugprone-signed-bitwise)
}

inline std::vector<zx_info_vmo_t> GetProcessVmos(const zx::process& process) {
  return GetProcessInfo<zx_info_vmo_t>(process,
                                       ZX_INFO_PROCESS_VMOS);  // NOLINT(bugprone-signed-bitwise)
}

// Returns the untagged faulting address for ZX_EXCP_FATAL_PAGE_FAULT, or 0 otherwise.
uint64_t ExtractFaultingAddress(const zx_exception_report_t& report);

// Returns true iff |process| holds at least one ZX_OBJ_TYPE_PAGER handle and runs exclusively
// bootfs code (such as fxfs.cm, pkg-cache, or the storage driver_host).
bool IsBootfsPagerProcess(const zx::process& process);

// Returns the trusted bootfs pager process under |root_job| that holds a handle to the root pager
// VMO (parent_koid == ZX_KOID_INVALID) backing |fault_vaddr| in |faulting_process|, or
// std::nullopt if the fault is not backed by one. Note that Zircon does not expose which
// zx::pager object serves a VMO, so a root pager VMO held via handle in a bootfs pager process
// (including a client VMO registered with block_server via AttachVmo) is attributed to that
// process. If the pager has already closed the root VMO, its children report ZX_KOID_INVALID as
// parent_koid and the fault is not attributed to any pager.
std::optional<PagerOwner> FindTrustedPagerFaultOwner(const zx::process& faulting_process,
                                                     uint64_t fault_vaddr, const zx::job& root_job);
#endif

}  // namespace storage_watchdog

#endif  // SRC_BRINGUP_BIN_CRITICAL_SERVICES_STORAGE_WATCHDOG_PAGER_FAULT_H_
