// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_VM_ASPACE_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_VM_ASPACE_H_

#include <zircon/types.h>

#include <arch/aspace.h>
#include <fbl/macros.h>
#include <fbl/recycler.h>
#include <fbl/ref_counted.h>
#include <fbl/ref_ptr.h>
#include <kernel/lockdep.h>
#include <kernel/mutex.h>
#include <ktl/type_traits.h>
#include <vm/arch_vm_aspace.h>

class VmAddressRegion;

// Size and alignment of the aspace lock of the Rust `VmAspace` (vm_aspace.rs), which C++ uses
// as `Lock<CriticalMutex>`. Rust asserts both values.
constexpr size_t kVmAspaceLockSize = sizeof(Lock<CriticalMutex>);
constexpr size_t kVmAspaceLockAlign = alignof(Lock<CriticalMutex>);

// Size and alignment of the reference count at the start of the Rust `VmAspace` (vm_aspace.rs),
// which C++ uses as `fbl::RefCounted<VmAspace>`. Rust asserts both values.
constexpr size_t kVmAspaceRefCountedSize = sizeof(fbl::RefCounted<VmAspace>);
constexpr size_t kVmAspaceRefCountedAlign = alignof(fbl::RefCounted<VmAspace>);

class VmAspace : public fbl::RefCounted<VmAspace>, public fbl::Recyclable<VmAspace> {
 public:
  enum class Type : uint8_t {
    User = 0,
    Kernel,
    // You probably do not want to use LOW_KERNEL. It is primarily used for SMP bootstrap or EFI
    // to allow mappings of very low memory using the standard VMM subsystem.
    LowKernel,
    // Used to construct an address space representing hypervisor guest memory.
    GuestPhysical,
  };

  // Create an address space of the type specified in |type| with name |name|.
  //
  // The returned aspace will start at |base| and span |size|.
  //
  // If |share_opt| is ShareOpt::Shared, we're creating a shared address space, and the underlying
  // ArchVmAspace will be initialized using the `InitShared` method instead of the normal
  // `Init` method.
  //
  // If |share_opt| is ShareOpt::Restricted, we're creating a restricted address space, and the
  // underlying ArchVmAspace will be initialized using the `InitRestricted` method.
  //
  // Although reference counted, the returned VmAspace must be explicitly destroyed via Destroy.
  //
  // Returns null on failure (e.g. due to resource starvation).
  enum class ShareOpt : uint8_t {
    None,
    Restricted,
    Shared,
  };
  static fbl::RefPtr<VmAspace> Create(vaddr_t base, size_t size, Type type, const char* name,
                                      ShareOpt share_opt);

  // Destroy this address space.
  //
  // Destroy does not free this object, but rather allows it to be freed when the last retaining
  // RefPtr is destroyed.
  zx_status_t Destroy();

  // simple accessors
  ArchVmAspace& arch_aspace();
  bool is_user() const;

  // Get the root VMAR (briefly acquires the aspace lock)
  // May return nullptr if the aspace has been destroyed or is not yet initialized.
  fbl::RefPtr<VmAddressRegion> RootVmar();

  // Returns true if the address space has been destroyed.
  bool is_destroyed() const;

  // accessor for singleton kernel address space
  static VmAspace* kernel_aspace();

  // legacy functions to assist in the transition to VMARs
  // These all assume a flat VMAR structure in which all VMOs are mapped
  // as children of the root.  They will all assert if used on user aspaces
  // TODO(teisenbe): remove uses of these in favor of new VMAR interfaces
  zx_status_t AllocPhysical(const char* name, size_t size, void** ptr, uint8_t align_pow2,
                            paddr_t paddr, uint vmm_flags, arch_mmu_flags_t arch_mmu_flags);

 protected:
  // Share the aspace lock with VmMapping so it can serialize
  // changes to the aspace.
  friend class VmMapping;
  Lock<CriticalMutex>* lock() const;

 private:
  VmAspace() = delete;
  ~VmAspace() = delete;

  DISALLOW_COPY_ASSIGN_AND_MOVE(VmAspace);

  friend class fbl::Recyclable<VmAspace>;
  void fbl_recycle();
};

static_assert(ktl::is_standard_layout_v<VmAspace>);

extern "C" {
void rust_vm_aspace_recycle(VmAspace* aspace);
VmAspace* rust_vm_aspace_create_with_opts(vaddr_t base, size_t size, VmAspace::Type type,
                                          const char* name, VmAspace::ShareOpt share_opt);
zx_status_t rust_vm_aspace_destroy(VmAspace* aspace);
VmAspace* rust_vm_aspace_kernel_aspace();
bool rust_vm_aspace_is_user(const VmAspace* aspace);
ArchVmAspace* rust_vm_aspace_arch_aspace(VmAspace* aspace);
Lock<CriticalMutex>* rust_vm_aspace_lock(const VmAspace* aspace);
zx_status_t rust_vm_aspace_alloc_physical(VmAspace* aspace, const char* name, size_t size,
                                          void** ptr, uint8_t align_pow2, paddr_t paddr,
                                          uint vmm_flags, arch_mmu_flags_t arch_mmu_flags);
bool rust_vm_aspace_is_destroyed(const VmAspace* aspace);
VmAddressRegion* rust_vm_aspace_root_vmar(VmAspace* aspace);
}

inline ArchVmAspace& VmAspace::arch_aspace() { return *rust_vm_aspace_arch_aspace(this); }
inline bool VmAspace::is_user() const { return rust_vm_aspace_is_user(this); }
inline Lock<CriticalMutex>* VmAspace::lock() const { return rust_vm_aspace_lock(this); }
inline VmAspace* VmAspace::kernel_aspace() { return rust_vm_aspace_kernel_aspace(); }

inline fbl::RefPtr<VmAspace> VmAspace::Create(vaddr_t base, size_t size, Type type,
                                              const char* name, ShareOpt share_opt) {
  return fbl::ImportFromRawPtr(rust_vm_aspace_create_with_opts(base, size, type, name, share_opt));
}

inline void VmAspace::fbl_recycle() { rust_vm_aspace_recycle(this); }

inline zx_status_t VmAspace::Destroy() { return rust_vm_aspace_destroy(this); }

inline bool VmAspace::is_destroyed() const { return rust_vm_aspace_is_destroyed(this); }

inline zx_status_t VmAspace::AllocPhysical(const char* name, size_t size, void** ptr,
                                           uint8_t align_pow2, paddr_t paddr, uint vmm_flags,
                                           arch_mmu_flags_t arch_mmu_flags) {
  return rust_vm_aspace_alloc_physical(this, name, size, ptr, align_pow2, paddr, vmm_flags,
                                       arch_mmu_flags);
}

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_VM_ASPACE_H_
