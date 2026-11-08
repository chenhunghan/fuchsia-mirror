// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <zircon/errors.h>
#include <zircon/types.h>

#include <dev/iommu/bti.h>
#include <dev/iommu/pmt.h>
#include <fbl/ref_ptr.h>
#include <kernel/ffi.h>
#include <ktl/utility.h>
#include <vm/pinned_vm_object.h>
#include <vm/vm_object.h>

// LINT.IfChange(BtiPageLeakReason)
static_assert(sizeof(iommu::Bti::BtiPageLeakReason) == sizeof(uint32_t));
static_assert(alignof(iommu::Bti::BtiPageLeakReason) == alignof(uint32_t));
static_assert(
    static_cast<uint32_t>(iommu::Bti::BtiPageLeakReason::BtiOrphanedWithQuarantinedPmts) == 0);
static_assert(static_cast<uint32_t>(iommu::Bti::BtiPageLeakReason::PmtQuarantinedWhenBtiOrphaned) ==
              1);
static_assert(static_cast<uint32_t>(iommu::Bti::BtiPageLeakReason::PmtQuarantined) == 2);
// LINT.ThenChange(
//   //zircon/kernel/dev/iommu/arm_smmu/common/constants.rs:BtiPageLeakReason,
//   //zircon/kernel/dev/iommu/bti.rs:BtiPageLeakReason,
//   //zircon/kernel/dev/iommu/include/dev/iommu/bti.h:BtiPageLeakReason,
// )

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_bti_recycle(iommu::Bti* bti) { delete bti; }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void* cpp_bti_get_ref_counted(const iommu::Bti* bti) {
  return const_cast<fbl::RefCounted<iommu::Bti>*>(
      static_cast<const fbl::RefCounted<iommu::Bti>*>(bti));
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_bti_release_quarantine(iommu::Bti* bti) { bti->ReleaseQuarantine(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_bti_on_dispatcher_zero_handles(iommu::Bti* bti) {
  bti->OnDispatcherZeroHandles();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint64_t cpp_bti_minimum_contiguity(const iommu::Bti* bti) {
  return bti->minimum_contiguity();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint64_t cpp_bti_aspace_size(const iommu::Bti* bti) { return bti->aspace_size(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint64_t cpp_bti_pmo_count(const iommu::Bti* bti) { return bti->pmo_count(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint64_t cpp_bti_quarantine_count(const iommu::Bti* bti) {
  return bti->quarantine_count();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_bti_in_fault_state(const iommu::Bti* bti) {
  return bti->in_fault_state();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_bti_map(iommu::Bti* bti, PinnedVmObject* pinned_vmo,
                                          uint32_t perms, bool require_contiguous,
                                          ffi::Uninitialized<fbl::RefPtr<iommu::Pmt>>* pmt_out) {
  zx::result<fbl::RefPtr<iommu::Pmt>> maybe_pmt =
      bti->Map(ktl::move(*pinned_vmo), perms,
               require_contiguous ? iommu::RequireContiguousMapping::Yes
                                  : iommu::RequireContiguousMapping::No);
  if (!maybe_pmt.is_ok()) {
    return maybe_pmt.error_value();
  }
  pmt_out->Initialize(ktl::move(maybe_pmt.value()));
  return ZX_OK;
}

void rust_bti_print_quarantine_warning(uint64_t bti_id, const char (*bti_name)[ZX_MAX_NAME_LEN],
                                       iommu::Bti::BtiPageLeakReason reason,
                                       uint64_t total_leaked_pages, size_t total_leaked_vmos);

}  // extern "C"

namespace iommu {

void Bti::PrintQuarantineWarning(BtiPageLeakReason reason, uint64_t total_leaked_pages,
                                 size_t total_leaked_vmos) const {
  char bti_name[ZX_MAX_NAME_LEN]{};
  [[maybe_unused]] zx_status_t status = this->get_name(bti_name);
  DEBUG_ASSERT(status == ZX_OK);
  rust_bti_print_quarantine_warning(bti_id(), &bti_name, reason, total_leaked_pages,
                                    total_leaked_vmos);
}

}  // namespace iommu
