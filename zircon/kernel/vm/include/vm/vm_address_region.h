// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_VM_ADDRESS_REGION_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_VM_ADDRESS_REGION_H_

#include <lib/zircon-internal/thread_annotations.h>
#include <lib/zx/result.h>
#include <stddef.h>
#include <stdint.h>
#include <zircon/types.h>

#include <fbl/recycler.h>
#include <fbl/ref_counted.h>
#include <fbl/ref_ptr.h>
#include <ktl/utility.h>
#include <vm/vm_aspace.h>
#include <vm/vm_constants.h>
#include <vm/vm_object.h>

// Creation flags for VmAddressRegion and VmMappings

// When randomly allocating subregions, reduce sprawl by placing allocations
// near each other.
#define VMAR_FLAG_COMPACT (1 << 0)
// Request that the new region be at the specified offset in its parent region.
#define VMAR_FLAG_SPECIFIC (1 << 1)
// Like VMAR_FLAG_SPECIFIC, but permits overwriting existing mappings.  This
// flag will not overwrite through a subregion.
#define VMAR_FLAG_SPECIFIC_OVERWRITE (1 << 2)
// Allow VmMappings to be created inside the new region with the SPECIFIC or
// OFFSET_IS_UPPER_LIMIT flag.
#define VMAR_FLAG_CAN_MAP_SPECIFIC (1 << 3)
// When on a VmAddressRegion, allow VmMappings to be created inside the region
// with read permissions.  When on a VmMapping, controls whether or not the
// mapping can gain this permission.
#define VMAR_FLAG_CAN_MAP_READ (1 << 4)
// When on a VmAddressRegion, allow VmMappings to be created inside the region
// with write permissions.  When on a VmMapping, controls whether or not the
// mapping can gain this permission.
#define VMAR_FLAG_CAN_MAP_WRITE (1 << 5)
// When on a VmAddressRegion, allow VmMappings to be created inside the region
// with execute permissions.  When on a VmMapping, controls whether or not the
// mapping can gain this permission.
#define VMAR_FLAG_CAN_MAP_EXECUTE (1 << 6)
// Require that VMO backing the mapping is non-resizable.
#define VMAR_FLAG_REQUIRE_NON_RESIZABLE (1 << 7)
// Allow VMO backings that could result in faults.
#define VMAR_FLAG_ALLOW_FAULTS (1 << 8)
// Treat the offset as an upper limit when allocating a VMO or child VMAR.
#define VMAR_FLAG_OFFSET_IS_UPPER_LIMIT (1 << 9)
// Opt this VMAR out of certain debugging checks. This allows for kernel mappings that have a more
// dynamic management strategy, that the regular checks would otherwise spuriously trip on.
#define VMAR_FLAG_DEBUG_DYNAMIC_KERNEL_MAPPING (1 << 10)
// Memory accesses past the stream size rounded up to the page boundary will fault.
#define VMAR_FLAG_FAULT_BEYOND_STREAM_SIZE (1 << 11)

#define VMAR_CAN_RWX_FLAGS \
  (VMAR_FLAG_CAN_MAP_READ | VMAR_FLAG_CAN_MAP_WRITE | VMAR_FLAG_CAN_MAP_EXECUTE)

enum class VmAddressRegionOpChildren : bool {
  Yes,
  No,
};

// forward declarations
class VmAddressRegionOrMapping;
class VmAddressRegion;
class VmMapping;

// A VmAddressRegion represents a contiguous region of the virtual address
// space.  It is partitioned by non-overlapping children of the following types:
// 1) child VmAddressRegion
// 2) child VmMapping (leaves that map VmObjects into the address space)
// 3) gaps (logical, not actually objects).
//
// VmAddressRegionOrMapping represents a tagged union of the two types.
//
// A VmAddressRegion/VmMapping may be in one of two states: ALIVE or DEAD.  If
// it is ALIVE, then the VmAddressRegion is a description of the virtual memory
// mappings of the address range it represents in its parent VmAspace.  If it is
// DEAD, then the VmAddressRegion is invalid and has no meaning.
//
// All VmAddressRegion and VmMapping state is protected by the aspace lock.
class alignas(8) VmAddressRegionOrMapping : public fbl::RefCounted<VmAddressRegionOrMapping>,
                                            public fbl::Recyclable<VmAddressRegionOrMapping> {
 public:
  // accessors
  vaddr_t base() const;
  size_t size() const;

 private:
  VmAddressRegionOrMapping() = delete;
  ~VmAddressRegionOrMapping() = delete;

  DISALLOW_COPY_ASSIGN_AND_MOVE(VmAddressRegionOrMapping);

  friend class fbl::Recyclable<VmAddressRegionOrMapping>;
  void fbl_recycle();
};

extern "C" {
void rust_vm_address_region_or_mapping_recycle(VmAddressRegionOrMapping* region_or_mapping);
vaddr_t rust_vm_address_region_or_mapping_base(const VmAddressRegionOrMapping* region_or_mapping);
size_t rust_vm_address_region_or_mapping_size(const VmAddressRegionOrMapping* region_or_mapping);
}

inline void VmAddressRegionOrMapping::fbl_recycle() {
  rust_vm_address_region_or_mapping_recycle(this);
}

inline vaddr_t VmAddressRegionOrMapping::base() const {
  return rust_vm_address_region_or_mapping_base(this);
}
inline size_t VmAddressRegionOrMapping::size() const {
  return rust_vm_address_region_or_mapping_size(this);
}

// A representation of a contiguous range of virtual address space
class VmAddressRegion final : public fbl::RefCounted<VmAddressRegion>,
                              public fbl::Recyclable<VmAddressRegion> {
 public:
  // Recursively destroy child VMARs and transition to the DEAD state.
  //
  // Returns ZX_OK on success, ZX_ERR_BAD_STATE if already dead, and other
  // values on error (typically unmap failure).
  zx_status_t Destroy();

  // accessors
  vaddr_t base() const;
  size_t size() const;
  uint32_t flags() const;
  const fbl::RefPtr<VmAspace>& aspace() const;

  // Creates a subregion of this region
  zx_status_t CreateSubVmar(size_t offset, size_t size, uint8_t align_pow2, uint32_t vmar_flags,
                            const char* name, fbl::RefPtr<VmAddressRegion>* out);
  // Creates a VmMapping within this region. To avoid leaks, this should be paired with a call to
  // VmMapping::Destroy if desired; dropping `MapResult::mapping` will *not* destroy the mapping.
  struct MapResult {
    // This will never be null
    fbl::RefPtr<VmMapping> mapping;
    // Represents the virtual address of |mapping| at the time of creation, which is equivalent to
    // |mapping->base()|.
    vaddr_t base;
  };
  zx::result<MapResult> CreateVmMapping(size_t mapping_offset, size_t size, uint8_t align_pow2,
                                        uint32_t vmar_flags, fbl::RefPtr<VmObject> vmo,
                                        uint64_t vmo_offset, arch_mmu_flags_t arch_mmu_flags,
                                        const char* name);

  // Change protections on a subset of the region of memory in the containing
  // address space. If the requested range overlaps with a subregion and op_children is No,
  // Protect() will fail, otherwise the mapping permissions in the sub-region may only be reduced.
  zx_status_t Protect(vaddr_t base, size_t size, arch_mmu_flags_t new_arch_mmu_flags,
                      VmAddressRegionOpChildren op_children);

  bool has_parent() const;

 private:
  VmAddressRegion() = delete;
  ~VmAddressRegion() = delete;

  DISALLOW_COPY_ASSIGN_AND_MOVE(VmAddressRegion);

  friend class fbl::Recyclable<VmAddressRegion>;
  void fbl_recycle();
};

extern "C" {
zx_status_t rust_vm_address_region_destroy(VmAddressRegion* vmar);
vaddr_t rust_vm_address_region_base(const VmAddressRegion* vmar);
size_t rust_vm_address_region_size(const VmAddressRegion* vmar);
uint32_t rust_vm_address_region_flags(const VmAddressRegion* vmar);
const fbl::RefPtr<VmAspace>* rust_vm_address_region_aspace(const VmAddressRegion* vmar);
bool rust_vm_address_region_has_parent(const VmAddressRegion* vmar);
zx_status_t rust_vm_address_region_create_sub_vmar(VmAddressRegion* vmar, size_t offset,
                                                   size_t size, uint8_t align_pow2,
                                                   uint32_t vmar_flags, const char* name,
                                                   VmAddressRegion** out);
zx_status_t rust_vm_address_region_create_vm_mapping(VmAddressRegion* vmar, size_t mapping_offset,
                                                     size_t size, uint8_t align_pow2,
                                                     uint32_t vmar_flags, VmObject* vmo,
                                                     uint64_t vmo_offset,
                                                     arch_mmu_flags_t arch_mmu_flags,
                                                     vaddr_t* out_base, VmMapping** out_mapping);
void rust_vm_address_region_recycle(VmAddressRegion* vmar);
zx_status_t rust_vm_address_region_protect(VmAddressRegion* vmar, vaddr_t base, size_t size,
                                           arch_mmu_flags_t new_arch_mmu_flags,
                                           VmAddressRegionOpChildren op_children);
}

inline void VmAddressRegion::fbl_recycle() { rust_vm_address_region_recycle(this); }
inline zx_status_t VmAddressRegion::Destroy() { return rust_vm_address_region_destroy(this); }
inline vaddr_t VmAddressRegion::base() const { return rust_vm_address_region_base(this); }
inline size_t VmAddressRegion::size() const { return rust_vm_address_region_size(this); }
inline uint32_t VmAddressRegion::flags() const { return rust_vm_address_region_flags(this); }
inline const fbl::RefPtr<VmAspace>& VmAddressRegion::aspace() const {
  return *rust_vm_address_region_aspace(this);
}
inline zx_status_t VmAddressRegion::CreateSubVmar(size_t offset, size_t size, uint8_t align_pow2,
                                                  uint32_t vmar_flags, const char* name,
                                                  fbl::RefPtr<VmAddressRegion>* out) {
  VmAddressRegion* vmar;
  zx_status_t status = rust_vm_address_region_create_sub_vmar(this, offset, size, align_pow2,
                                                              vmar_flags, name, &vmar);
  if (status != ZX_OK) {
    return status;
  }
  *out = fbl::ImportFromRawPtr(vmar);
  return ZX_OK;
}
inline zx::result<VmAddressRegion::MapResult> VmAddressRegion::CreateVmMapping(
    size_t mapping_offset, size_t size, uint8_t align_pow2, uint32_t vmar_flags,
    fbl::RefPtr<VmObject> vmo, uint64_t vmo_offset, arch_mmu_flags_t arch_mmu_flags,
    const char* /*name*/) {
  vaddr_t base;
  VmMapping* mapping;
  zx_status_t status = rust_vm_address_region_create_vm_mapping(
      this, mapping_offset, size, align_pow2, vmar_flags, fbl::ExportToRawPtr(&vmo), vmo_offset,
      arch_mmu_flags, &base, &mapping);
  if (status != ZX_OK) {
    return zx::error{status};
  }
  return zx::ok(MapResult{.mapping = fbl::ImportFromRawPtr(mapping), .base = base});
}
inline zx_status_t VmAddressRegion::Protect(vaddr_t base, size_t size,
                                            arch_mmu_flags_t new_arch_mmu_flags,
                                            VmAddressRegionOpChildren op_children) {
  return rust_vm_address_region_protect(this, base, size, new_arch_mmu_flags, op_children);
}
inline bool VmAddressRegion::has_parent() const { return rust_vm_address_region_has_parent(this); }

// A representation of the mapping of a VMO into the address space
class VmMapping final : public fbl::RefCounted<VmMapping>, public fbl::Recyclable<VmMapping> {
 public:
  // Unmap all pages and remove dependency on vm object it has a ref to.
  //
  // Returns ZX_OK on success, ZX_ERR_BAD_STATE if already dead, and other
  // values on error (typically unmap failure).
  zx_status_t Destroy() TA_EXCL(lock());

  // accessors
  vaddr_t base() const;
  size_t size() const;
  const fbl::RefPtr<VmAspace>& aspace() const;

  // Expose our backing lock for annotation purposes.
  Lock<CriticalMutex>* lock() const TA_RET_CAP(aspace()->lock()) { return aspace()->lock(); }

  // Accessors for VMO-mapping state
  // These can be read under either lock (both locks being held for writing).
  uint64_t object_offset() const;

  // Map in pages from the underlying vm object, optionally committing pages as it goes.
  // |ignore_existing| controls whether existing hardware mappings in the specified range should be
  // ignored or treated as an error. |ignore_existing| should only be set to true for user mappings
  // where populating mappings may already be racy with multiple threads, and where we are already
  // tolerant of mappings being arbitrarily created and destroyed.
  zx_status_t MapRange(size_t offset, size_t len, bool commit, bool ignore_existing = false)
      TA_EXCL(lock());

  // Unlocked convenience wrapper of `VmMapping::protect_locked` for testing.
  zx_status_t DebugProtect(vaddr_t base, size_t size, arch_mmu_flags_t new_arch_mmu_flags)
      TA_EXCL(lock());

  // APIs intended for use by VmObject

  // |assert_object_lock| exists to satisfy clang capability analysis since there are circumstances
  // when the object lock is actually being held, but it was not acquired by dereferencing the
  // object. In this scenario we need to explain to the analysis that the lock held is actually the
  // same as the object lock, and even though we otherwise have no intention of using the object,
  // the only way to do this is to notionally dereference it to compare the lock.
  // Since this is asserting that the lock is held, and not just returning a reference to the lock,
  // this method is logically correct since the object pointer itself is only modified if the
  // object lock is held.
  void assert_object_lock() const TA_ASSERT(object_locked()->lock()) TA_NO_THREAD_SAFETY_ANALYSIS {
    AssertHeld(object_locked()->lock_ref());
  }

  enum UnmapOptions : uint8_t {
    kNone = 0u,
    OnlyHasZeroPages = (1u << 0),
    Harvest = (1u << 1),
  };

  // Unmap any pages that map the passed in vmo range from the arch aspace.
  // May not intersect with this range.
  // If |options| has OnlyHasZeroPages then the caller is asserting that it knows that any mappings
  // in the region will only be for the shared zero page.
  void AspaceUnmapLockedObject(uint64_t offset, uint64_t len, UnmapOptions options) const
      TA_REQ(object_locked()->lock());

  // Removes any writeable mappings for the passed in vmo range from the arch aspace.
  // May fall back to unmapping pages from the arch aspace if necessary.
  void AspaceRemoveWriteLockedObject(uint64_t offset, uint64_t len) const
      TA_REQ(object_locked()->lock());

  // Checks if this is a kernel mapping within the given VMO range, which would be an error to be
  // unpinning.
  void AspaceDebugUnpinLockedObject(uint64_t offset, uint64_t len) const
      TA_REQ(object_locked()->lock());

 private:
  VmMapping() = delete;
  ~VmMapping() = delete;

  DISALLOW_COPY_ASSIGN_AND_MOVE(VmMapping);

  friend class fbl::Recyclable<VmMapping>;
  void fbl_recycle();

  const fbl::RefPtr<VmObject>& object_locked() const TA_REQ(lock());
};

extern "C" {
zx_status_t rust_vm_mapping_destroy(VmMapping* mapping);
vaddr_t rust_vm_mapping_base(const VmMapping* mapping);
size_t rust_vm_mapping_size(const VmMapping* mapping);
const fbl::RefPtr<VmAspace>* rust_vm_mapping_aspace(const VmMapping* mapping);
const fbl::RefPtr<VmObject>* rust_vm_mapping_object(const VmMapping* mapping);
uint64_t rust_vm_mapping_object_offset(const VmMapping* mapping);
void rust_vm_mapping_recycle(VmMapping* mapping);
zx_status_t rust_vm_mapping_map_range(VmMapping* mapping, size_t offset, size_t len, bool commit,
                                      bool ignore_existing);
zx_status_t rust_vm_mapping_debug_protect(VmMapping* mapping, vaddr_t base, size_t size,
                                          arch_mmu_flags_t new_arch_mmu_flags);
void rust_vm_mapping_aspace_unmap_locked_object(const VmMapping* mapping, uint64_t offset,
                                                uint64_t len, VmMapping::UnmapOptions options);
void rust_vm_mapping_aspace_remove_write_locked_object(const VmMapping* mapping, uint64_t offset,
                                                       uint64_t len);
void rust_vm_mapping_aspace_debug_unpin_locked_object(const VmMapping* mapping, uint64_t offset,
                                                      uint64_t len);
}

inline void VmMapping::fbl_recycle() { rust_vm_mapping_recycle(this); }
inline zx_status_t VmMapping::Destroy() { return rust_vm_mapping_destroy(this); }
inline vaddr_t VmMapping::base() const { return rust_vm_mapping_base(this); }
inline size_t VmMapping::size() const { return rust_vm_mapping_size(this); }
inline const fbl::RefPtr<VmAspace>& VmMapping::aspace() const {
  return *rust_vm_mapping_aspace(this);
}
inline const fbl::RefPtr<VmObject>& VmMapping::object_locked() const {
  return *rust_vm_mapping_object(this);
}
inline uint64_t VmMapping::object_offset() const { return rust_vm_mapping_object_offset(this); }
inline zx_status_t VmMapping::MapRange(size_t offset, size_t len, bool commit,
                                       bool ignore_existing) {
  return rust_vm_mapping_map_range(this, offset, len, commit, ignore_existing);
}
inline zx_status_t VmMapping::DebugProtect(vaddr_t base, size_t size,
                                           arch_mmu_flags_t new_arch_mmu_flags) {
  return rust_vm_mapping_debug_protect(this, base, size, new_arch_mmu_flags);
}
inline void VmMapping::AspaceUnmapLockedObject(uint64_t offset, uint64_t len,
                                               UnmapOptions options) const {
  rust_vm_mapping_aspace_unmap_locked_object(this, offset, len, options);
}
inline void VmMapping::AspaceRemoveWriteLockedObject(uint64_t offset, uint64_t len) const {
  rust_vm_mapping_aspace_remove_write_locked_object(this, offset, len);
}
inline void VmMapping::AspaceDebugUnpinLockedObject(uint64_t offset, uint64_t len) const {
  rust_vm_mapping_aspace_debug_unpin_locked_object(this, offset, len);
}

// Now that VmAddressRegion is defined, finish declaring inline methods that use it.
inline fbl::RefPtr<VmAddressRegion> VmAspace::RootVmar() {
  return fbl::ImportFromRawPtr(rust_vm_aspace_root_vmar(this));
}

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_VM_ADDRESS_REGION_H_
