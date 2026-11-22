// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BOARD_LIB_ACPI_PCI_ROOT_WINDOWS_H_
#define SRC_DEVICES_BOARD_LIB_ACPI_PCI_ROOT_WINDOWS_H_

#include <lib/zx/eventpair.h>
#include <lib/zx/resource.h>
#include <lib/zx/result.h>
#include <zircon/types.h>

#include <utility>
#include <vector>

#include <region-alloc/region-alloc.h>

namespace acpi {

// The MMIO and IO address space windows a single PCI root makes available to the devices beneath
// it, and the allocations handed out from them.
//
// There is one instance per root. Allocations are returned to the windows when the bus driver
// closes their token. It is only used from the board driver's synchronized dispatcher.
class PciRootWindows {
 public:
  enum class Space {
    kMemory,
    kIo,
  };

  struct Allocation {
    zx_paddr_t base;
    zx::resource resource;
    // Handed to the bus driver alongside the resource. The root holds the peer for as long as the
    // allocation exists.
    zx::eventpair token;
  };

  PciRootWindows(zx::unowned_resource mmio_resource, zx::unowned_resource ioport_resource)
      : mmio_resource_(std::move(mmio_resource)), ioport_resource_(std::move(ioport_resource)) {}

  PciRootWindows(const PciRootWindows&) = delete;
  PciRootWindows& operator=(const PciRootWindows&) = delete;

  // Adds a range this root produces. Memory ranges below 4GiB go in the low window, ranges at or
  // above 4GiB go in the high window, and ranges straddling 4GiB are split across both. Overlapping
  // ranges are merged.
  zx_status_t AddWindow(Space space, uint64_t base, uint64_t size);

  // Removes a range consumed by something else in the platform. Parts of the range that are not in
  // a window are ignored.
  zx_status_t SubtractWindow(Space space, uint64_t base, uint64_t size);

  // Allocates |size| bytes of |space|, at |base| if it is non-zero. Memory allocations come from
  // the low window if |low| is set, otherwise from the high window, falling back to the low window
  // when no base was requested. Memory allocations are page aligned and rounded up to a page; when
  // |base| is zero, the resulting size must be a power of two.
  zx::result<Allocation> Allocate(Space space, zx_paddr_t base, size_t size, bool low);

 private:
  zx::result<Allocation> AllocateFrom(RegionAllocator& allocator, const char* name,
                                      uint32_t resource_kind, zx_paddr_t base, size_t size);

  const zx::unowned_resource mmio_resource_;
  const zx::unowned_resource ioport_resource_;

  RegionAllocator mmio32_;
  RegionAllocator mmio64_;
  RegionAllocator io_;
  // Declared after the RegionAllocators so allocated Region::UPtrs return their regions before the
  // allocators are destroyed.
  std::vector<std::pair<RegionAllocator::Region::UPtr, zx::eventpair>> allocations_;
};

}  // namespace acpi

#endif  // SRC_DEVICES_BOARD_LIB_ACPI_PCI_ROOT_WINDOWS_H_
