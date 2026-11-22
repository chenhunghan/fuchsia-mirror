// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_HYPERVISOR_H_
#define ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_HYPERVISOR_H_

#include <lib/object-constants.h>
#include <lib/zx/result.h>
#include <stddef.h>
#include <stdint.h>
#include <zircon/types.h>

#include <arch/x86/hypervisor/vmx_state.h>
#include <fbl/ref_ptr.h>
#include <hypervisor/aspace.h>
#include <hypervisor/page.h>
#include <hypervisor/trap_map.h>
#include <kernel/ffi.h>
#include <kernel/mutex.h>

class VpidAllocator;

extern "C" {
void rust_vpid_allocator_init(VpidAllocator* allocator);
void rust_vpid_allocator_destroy(VpidAllocator* allocator);
zx_status_t rust_vpid_allocator_try_alloc(VpidAllocator* allocator, uint16_t* out);
zx_status_t rust_vpid_allocator_free(VpidAllocator* allocator, uint16_t vpid);
}  // extern "C"

// This is layout-compatible with the Rust `VmxPage` in arch/x86/hypervisor/vmx_cpu_state.rs.
class VmxPage : public hypervisor::Page {
 private:
  using hypervisor::Page::Alloc;
};

// Allocates the VPIDs of a guest's VCPUs.
//
// This is a facade over the Rust `VpidAllocator` in arch/x86/hypervisor/guest.rs, which is stored
// in place in `storage_`.
class VpidAllocator {
 public:
  VpidAllocator() { rust_vpid_allocator_init(this); }
  ~VpidAllocator() { rust_vpid_allocator_destroy(this); }

  // The Rust `VpidAllocator` is pinned in place in `storage_`, so it must never be copied or moved.
  VpidAllocator(const VpidAllocator&) = delete;
  VpidAllocator(VpidAllocator&&) = delete;
  VpidAllocator& operator=(const VpidAllocator&) = delete;
  VpidAllocator& operator=(VpidAllocator&&) = delete;

  zx::result<uint16_t> TryAlloc() {
    uint16_t vpid = 0;
    if (zx_status_t status = rust_vpid_allocator_try_alloc(this, &vpid); status != ZX_OK) {
      return zx::error(status);
    }
    return zx::ok(vpid);
  }
  zx::result<> Free(uint16_t vpid) { return zx::make_result(rust_vpid_allocator_free(this, vpid)); }

 private:
  // The number of `usize` words in the Rust bitmap, which holds a bit for each VPID in
  // [0, Guest::kMaxGuestVcpus]. The size and alignment of `VpidAllocator` are asserted against the
  // Rust type in arch/x86/hypervisor/guest.rs.
  static constexpr size_t kBitmapWords = 2;

  // The Rust `VpidAllocator` holds a lockdep-tracked `Mutex` (prefixed by a lock class ID when
  // lockdep is enabled), a `usize` search hint, and a bitmap of a `usize` size and `kBitmapWords`
  // `usize` words.
  static constexpr size_t kStorageSize =
      kLockClassIdSize + sizeof(Mutex) + sizeof(size_t) + ((1 + kBitmapWords) * sizeof(size_t));

  [[maybe_unused]] OpaqueStorage<kStorageSize, 8> storage_{};
};

static_assert(alignof(VpidAllocator) == 8);

// Represents a guest within the hypervisor.
//
// This is a layout-compatible view of the Rust `Guest` in arch/x86/hypervisor/guest.rs, which
// creates and destroys guests.
class Guest {
 public:
  // Maximum VCPUs per guest.
  static constexpr size_t kMaxGuestVcpus = 64;

  Guest() = delete;
  Guest(const Guest&) = delete;
  Guest(Guest&&) = delete;
  Guest& operator=(const Guest&) = delete;
  Guest& operator=(Guest&&) = delete;

  ~Guest() = delete;

  hypervisor::GuestPhysicalAspace& PhysicalAspace() { return gpa_; }
  fbl::RefPtr<VmAddressRegion> RootVmar() const { return gpa_.RootVmar(); }
  hypervisor::TrapMap& Traps() { return traps_; }
  zx_paddr_t MsrBitmapsAddress() const { return msr_bitmaps_page_.PhysicalAddress(); }

  zx::result<uint16_t> TryAllocVpid() { return vpid_allocator_.TryAlloc(); }
  zx::result<> FreeVpid(uint16_t vpid) { return vpid_allocator_.Free(vpid); }

 private:
  hypervisor::GuestPhysicalAspace gpa_;
  hypervisor::TrapMap traps_;
  VpidAllocator vpid_allocator_;
  VmxPage msr_bitmaps_page_;
};

#endif  // ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_HYPERVISOR_H_
