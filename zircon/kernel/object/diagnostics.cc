// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/diagnostics.h"

#include <inttypes.h>
#include <stdio.h>
#include <string.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <object/handle.h>
#include <object/job_dispatcher.h>
#include <object/process_dispatcher.h>
#include <object/vm_object_dispatcher.h>
#include <pretty/cpp/sizes.h>
#include <vm/vm_object_paged.h>

#include <ktl/enforce.h>

namespace {

using pretty::FormattedBytes;

// Returns a string representation of VMO-related rights.
constexpr size_t kRightsStrLen = 8;
const char* VmoRightsToString(uint32_t rights, char str[kRightsStrLen]) {
  char* c = str;
  *c++ = (rights & ZX_RIGHT_READ) ? 'r' : '-';
  *c++ = (rights & ZX_RIGHT_WRITE) ? 'w' : '-';
  *c++ = (rights & ZX_RIGHT_EXECUTE) ? 'x' : '-';
  *c++ = (rights & ZX_RIGHT_MAP) ? 'm' : '-';
  *c++ = (rights & ZX_RIGHT_DUPLICATE) ? 'd' : '-';
  *c++ = (rights & ZX_RIGHT_TRANSFER) ? 't' : '-';
  *c = '\0';
  return str;
}

// Prints a header for the columns printed by DumpVmObject.
// If |handles| is true, the dumped objects are expected to have handle info.
void PrintVmoDumpHeader(bool handles) {
  printf("%s koid obj                parent #depth #chld #map #shr    size   uncomp   comp name\n",
         handles ? "      handle rights " : "           -      - ");
}

void DumpVmObject(const VmObject& vmo, pretty::SizeUnit format_unit, zx_handle_t handle,
                  uint32_t rights, zx_koid_t koid) {
  char handle_str[11];
  if (handle != ZX_HANDLE_INVALID) {
    snprintf(handle_str, sizeof(handle_str), "%u", static_cast<uint32_t>(handle));
  } else {
    handle_str[0] = '-';
    handle_str[1] = '\0';
  }

  char rights_str[kRightsStrLen];
  if (rights != 0) {
    VmoRightsToString(rights, rights_str);
  } else {
    rights_str[0] = '-';
    rights_str[1] = '\0';
  }

  FormattedBytes size_str(vmo.size(), format_unit);

  FormattedBytes uncomp_size;
  FormattedBytes comp_size;
  const char* uncomp_str = "phys";
  const char* comp_str = "phys";
  if (vmo.is_paged()) {
    VmObject::AttributionCounts counts = vmo.GetAttributedMemory();
    uncomp_size.SetSize(counts.uncompressed_bytes, format_unit);
    comp_size.SetSize(counts.compressed_bytes, format_unit);
    uncomp_str = uncomp_size.c_str();
    comp_str = comp_size.c_str();
  }

  char child_str[21];
  if (vmo.child_type() != VmObject::kNotChild) {
    snprintf(child_str, sizeof(child_str), "%" PRIu64, vmo.parent_user_id());
  } else {
    child_str[0] = '-';
    child_str[1] = '\0';
  }

  char name[ZX_MAX_NAME_LEN];
  vmo.get_name(name, sizeof(name));
  if (name[0] == '\0') {
    name[0] = '-';
    name[1] = '\0';
  }

  printf(
      "  %10s "  // handle
      "%6s "     // rights
      "%5" PRIu64
      " "     // koid
      "%p "   // vm_object
      "%6s "  // child parent koid
      "%6" PRIu32
      " "  // lookup depth
      "%5" PRIu32
      " "  // number of children
      "%4" PRIu32
      " "  // map count
      "%4" PRIu32
      " "      // share count
      "%7s "   // size in bytes
      "%7s "   // uncompressed bytes
      "%7s "   // compressed bytes
      "%s\n",  // name
      handle_str, rights_str, koid, &vmo, child_str, vmo.DebugLookupDepth(), vmo.num_children(),
      vmo.num_mappings(), vmo.share_count(), size_str.c_str(), uncomp_str, comp_str, name);
}

// If |hidden_only| is set, will only dump VMOs that are not mapped
// into any process:
// - VMOs that userspace has handles to but does not map
// - VMOs that are mapped only into kernel space
// - Kernel-only, unmapped VMOs that have no handles
void DumpAllVmObjects(bool hidden_only, pretty::SizeUnit format_unit) {
  if (hidden_only) {
    printf("\"Hidden\" VMOs, oldest to newest:\n");
  } else {
    printf("All VMOs, oldest to newest:\n");
  }
  PrintVmoDumpHeader(/* handles */ false);
  VmObject::ForEach([=](const VmObject& vmo) {
    if (hidden_only && vmo.IsMappedByUser()) {
      return ZX_OK;
    }
    DumpVmObject(vmo, format_unit, ZX_HANDLE_INVALID,
                 /* rights */ 0u,
                 /* koid */ vmo.user_id());
    // TODO(dbort): Dump the VmAspaces (processes) that map the VMO.
    // TODO(dbort): Dump the processes that hold handles to the VMO.
    //     This will be a lot harder to gather.
    return ZX_OK;
  });
  PrintVmoDumpHeader(/* handles */ false);
}

// Dump memory held by slices whose parent VMOs have gone away.
void DumpOrphanedSlices(pretty::SizeUnit format_unit) {
  size_t orphaned = 0;
  size_t total = 0;
  size_t contig = 0, pager = 0, anon = 0;
  size_t orphaned_bytes = 0;
  VmObject::ForEachRef([&](VmObject* vmo) {
    if (auto paged = DownCastVmObject<VmObjectPaged>(vmo)) {
      if (paged->is_slice()) {
        total++;
        if (paged->is_contiguous()) {
          contig++;
        } else if (paged->is_user_pager_backed()) {
          pager++;
        } else {
          anon++;
        }
        // Orphaned slices will not have a parent ID.
        if (paged->parent_user_id() == 0) {
          orphaned++;
          // Note that it's possible to accumulate some pages multiple times if some slices point to
          // the same parent. At the time of writing this, there were no orphaned slices found. If
          // that changes, it would make sense to deduplicate.
          size_t owner_total =
              paged->GetAttributedMemoryRangeInReferenceOwner(0, UINT64_MAX).total_bytes();
          size_t owner_visible = paged->GetAttributedMemoryInReferenceOwner().total_bytes();
          DEBUG_ASSERT(owner_total >= owner_visible);
          orphaned_bytes += owner_total - owner_visible;
        }
      }
    }
    return ZX_OK;
  });
  FormattedBytes orphaned_bytes_str(orphaned_bytes, format_unit);
  printf(
      "%zu orphaned slice vmos using %s. total slices: %zu (contig %zu, pager %zu, anon %zu)\n\n",
      orphaned, orphaned_bytes_str.c_str(), total, contig, pager, anon);
}

// Dump memory held on by VMOs that are only referenced by mappings but no VMO handles. Try to find
// pages that lie outside of any mappings, and ideally should have been reclaimed.
void DumpOrphanedMappedVmos(pretty::SizeUnit format_unit) {
  enum types : uint8_t { Root, CowClone, Slice, Reference, NumTypes };
  char type_str[][6] = {"root", "cow", "slice", "ref"};

  // Track orphaned memory vs total memory held by the VMO.
  size_t orphaned[NumTypes] = {0, 0, 0, 0};
  size_t orphaned_total = 0;
  size_t orphaned_private_bytes[NumTypes] = {0, 0, 0, 0};
  size_t orphaned_total_bytes[NumTypes] = {0, 0, 0, 0};

  // Track some additional info about the distribution of VMO types, mapping ranges, etc., that can
  // help inform any heuristics we might use to reclaim the unmapped memory.
  size_t childless[NumTypes] = {0, 0, 0, 0};
  size_t pager[NumTypes] = {0, 0, 0, 0};
  size_t non_zero_lower_bound = 0;

  VmObject::ForEachRef([&](VmObject* vmo) {
    if (auto paged = DownCastVmObject<VmObjectPaged>(vmo)) {
      // If the VMO has handles, it will have a dispatcher, which is also the child observer. So
      // check for a null child observer.
      if (paged->IsMappedByUser() && !vmo->is_child_observer_valid()) {
        // - If this is a reference or slice, then check if the parent is alive, because only then
        // could the reference be the only one keeping the parent's VmCowPages alive. (Note that we
        // do not account for sibling references of the same parent, so there is some duplication
        // here. At the time of writing this, there was no orphaned memory found in references so
        // this was not relevant.)
        // - If not a reference, then we need to make sure no child can see into us. See
        // children_cannot_access_pages() for details on how this is determined.
        bool maybe_has_orphaned_memory = paged->is_reference()
                                             ? paged->parent_user_id() == 0
                                             : paged->children_cannot_access_pages();
        if (maybe_has_orphaned_memory) {
          orphaned_total++;
          // Find the maximal range in the VMO that is mapped. There can be races and the mapping(s)
          // could go away by the time we query this range. This is fine since we're not aiming to
          // take a snapshot of the system, just an estimate.
          auto range_opt = vmo->GetMaximalMappedRange();
          if (!range_opt.has_value()) {
            return ZX_OK;
          }
          auto range = range_opt.value();
          if (range.first > 0) {
            non_zero_lower_bound++;
          }

          uint8_t type;
          switch (paged->child_type()) {
            case VmObject::kNotChild:
              type = Root;
              break;
            case VmObject::kCowClone:
              type = CowClone;
              break;
            case VmObject::kSlice:
              type = Slice;
              break;
            case VmObject::kReference:
              type = Reference;
              break;
          }

          orphaned[type]++;
          if (paged->num_children() == 0) {
            childless[type]++;
          }
          if (paged->is_user_pager_backed()) {
            pager[type]++;
          }

          // How much memory is orphaned below the lower bound of the maximal mapped range.
          auto lower_bytes = paged->is_reference()
                                 ? paged->GetAttributedMemoryRangeInReferenceOwner(0, range.first)
                                 : paged->GetAttributedMemoryInRange(0, range.first);
          // How much memory is orphaned above the upper bound of the maximal mapped range.
          auto upper_bytes =
              paged->is_reference()
                  ? paged->GetAttributedMemoryRangeInReferenceOwner(range.second, paged->size())
                  : paged->GetAttributedMemoryInRange(range.second, paged->size());
          if (type == Root) {
            DEBUG_ASSERT(lower_bytes.total_private_bytes() == lower_bytes.total_bytes());
            DEBUG_ASSERT(upper_bytes.total_private_bytes() == upper_bytes.total_bytes());
          }
          orphaned_private_bytes[type] +=
              lower_bytes.total_private_bytes() + upper_bytes.total_private_bytes();
          orphaned_total_bytes[type] += lower_bytes.total_bytes() + upper_bytes.total_bytes();
          // If we found any orphaned memory, dump some info about the VMO.
          if (lower_bytes.total_private_bytes() + upper_bytes.total_private_bytes() > 0) {
            char name[ZX_MAX_NAME_LEN];
            vmo->get_name(name, sizeof(name));
            FormattedBytes private_str(
                lower_bytes.total_private_bytes() + upper_bytes.total_private_bytes(), format_unit);
            FormattedBytes total_str(lower_bytes.total_bytes() + upper_bytes.total_bytes(),
                                     format_unit);
            FormattedBytes lower_str(lower_bytes.total_private_bytes(), format_unit);
            FormattedBytes upper_str(upper_bytes.total_private_bytes(), format_unit);
            printf(
                "orphaned %s: %s in %u mapping(s) [range: %zu, %zu],"
                " unmapped private %s (total %s) l:%s u:%s\n",
                type_str[type], name, vmo->num_mappings(), range.first, range.second,
                private_str.c_str(), total_str.c_str(), lower_str.c_str(), upper_str.c_str());
          }
        }
      }
    }
    return ZX_OK;
  });

  size_t private_bytes = 0;
  for (auto bytes : orphaned_private_bytes) {
    private_bytes += bytes;
  }
  FormattedBytes private_str(private_bytes, format_unit);
  size_t total_bytes = 0;
  for (auto bytes : orphaned_total_bytes) {
    total_bytes += bytes;
  }
  FormattedBytes total_str(total_bytes, format_unit);
  printf("%zu vmos with mappings only. unmapped private %s (total %s). non-zero lower bound: %zu\n",
         orphaned_total, private_str.c_str(), total_str.c_str(), non_zero_lower_bound);
  printf("DISTRIBUTION\n");
  for (uint8_t type = 0; type < NumTypes; type++) {
    private_str = FormattedBytes(orphaned_private_bytes[type], format_unit);
    total_str = FormattedBytes(orphaned_total_bytes[type], format_unit);
    printf("%s vmos: %zu (pager %zu, childless %zu). private %s, total %s\n", type_str[type],
           orphaned[type], pager[type], childless[type], private_str.c_str(), total_str.c_str());
  }
}

// Dumps all VMOs associated with a process.
void DumpProcessVmObjects(zx_koid_t id, pretty::SizeUnit format_unit) {
  auto pd = ProcessDispatcher::LookupProcessById(id);
  if (!pd) {
    printf("process not found!\n");
    return;
  }

  printf("process [%" PRIu64 "]:\n", id);
  printf("Handles to VMOs:\n");
  PrintVmoDumpHeader(/* handles */ true);
  int count = 0;
  uint64_t total_size = 0;
  uint64_t total_alloc = 0;
  uint64_t total_compressed = 0;
  pd->handle_table().ForEachHandle(
      [&](zx_handle_t handle, zx_rights_t rights, const Dispatcher* disp) {
        auto vmod = DownCastDispatcher<const VmObjectDispatcher>(disp);
        if (vmod == nullptr) {
          return ZX_OK;
        }
        auto vmo = vmod->vmo();
        DumpVmObject(*vmo, format_unit, handle, rights, vmod->get_koid());

        // TODO: Doesn't handle the case where a process has multiple
        // handles to the same VMO; will double-count all of these totals.
        count++;
        total_size += vmo->size();
        // TODO: Doing this twice (here and in DumpVmObject) is a waste of
        // work, and can get out of sync.
        VmObject::AttributionCounts counts = vmo->GetAttributedMemory();
        total_alloc += counts.uncompressed_bytes;
        total_compressed += counts.compressed_bytes;
        return ZX_OK;
      });
  printf("  total: %d VMOs, size %s, alloc %s compressed %s\n", count,
         FormattedBytes(total_size, format_unit).c_str(),
         FormattedBytes(total_alloc, format_unit).c_str(),
         FormattedBytes(total_compressed, format_unit).c_str());

  // Call DumpVmObject() on all VMOs under the process's VmAspace.
  printf("Mapped VMOs:\n");
  PrintVmoDumpHeader(/* handles */ false);
  rust_diagnostics_dump_aspace_vm_objects(*pd, static_cast<char>(format_unit));
  PrintVmoDumpHeader(/* handles */ false);
}

void DumpVmObjectCowTree(zx_koid_t id) {
  fbl::RefPtr<VmCowPages> cow_pages;
  zx_status_t status = VmObject::ForEach([id, &cow_pages = cow_pages](const VmObject& vmo) {
    if (vmo.user_id() == id) {
      if (!vmo.is_paged()) {
        printf("vmo %" PRIu64 " is not paged\n", id);
        return ZX_ERR_STOP;
      }
      const auto& paged_vmo = static_cast<const VmObjectPaged&>(vmo);
      cow_pages = paged_vmo.DebugGetCowPages();
      if (!cow_pages) {
        printf("vmo %" PRIu64 " is not fully initialized\n", id);
      }
      return ZX_ERR_STOP;
    }
    return ZX_OK;
  });
  if (status == ZX_OK) {
    printf("vmo %" PRIu64 " not found\n", id);
    return;
  }
  if (!cow_pages) {
    return;
  }
  // Walk up to the root of the tree.
  while (auto parent = cow_pages->DebugGetParent()) {
    cow_pages = parent;
  }
  cow_pages->DebugForEachDescendant([](VmCowPages* cur, uint depth) {
    AssertHeld(cur->lock_ref());
    cur->DumpLocked(depth, false);
    return ZX_OK;
  });
}

}  // namespace

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_dump_all_vm_objects(bool hidden_only, char format_unit) {
  DumpAllVmObjects(hidden_only, static_cast<pretty::SizeUnit>(format_unit));
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_dump_orphaned_slices(char format_unit) {
  DumpOrphanedSlices(static_cast<pretty::SizeUnit>(format_unit));
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_dump_orphaned_mapped_vmos(char format_unit) {
  DumpOrphanedMappedVmos(static_cast<pretty::SizeUnit>(format_unit));
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_dump_process_vm_objects(zx_koid_t id, char format_unit) {
  DumpProcessVmObjects(id, static_cast<pretty::SizeUnit>(format_unit));
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_dump_vm_object_cow_tree(zx_koid_t id) { DumpVmObjectCowTree(id); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_dump_vm_object(const VmObject& vmo, char format_unit, zx_handle_t handle,
                                          zx_rights_t rights, zx_koid_t koid) {
  DumpVmObject(vmo, static_cast<pretty::SizeUnit>(format_unit), handle, rights, koid);
}

}  // extern "C"
