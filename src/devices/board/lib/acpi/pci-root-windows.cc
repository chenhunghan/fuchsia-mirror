// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/board/lib/acpi/pci-root-windows.h"

#include <zircon/compiler.h>
#include <zircon/syscalls.h>
#include <zircon/syscalls/resource.h>

#include <cstring>

namespace acpi {

namespace {

constexpr uint64_t k4GiB = uint64_t{1} << 32u;

}  // namespace

zx_status_t PciRootWindows::AddWindow(Space space, uint64_t base, uint64_t size) {
  uint64_t end = 0;
  if (add_overflow(base, size, &end)) {
    return ZX_ERR_INVALID_ARGS;
  }
  if (space == Space::kIo) {
    return io_.AddRegion({.base = base, .size = size}, RegionAllocator::AllowOverlap::Yes);
  }

  if (end <= k4GiB) {
    return mmio32_.AddRegion({.base = base, .size = size}, RegionAllocator::AllowOverlap::Yes);
  }
  if (base >= k4GiB) {
    return mmio64_.AddRegion({.base = base, .size = size}, RegionAllocator::AllowOverlap::Yes);
  }

  zx_status_t status =
      mmio32_.AddRegion({.base = base, .size = k4GiB - base}, RegionAllocator::AllowOverlap::Yes);
  if (status != ZX_OK) {
    return status;
  }
  return mmio64_.AddRegion({.base = k4GiB, .size = end - k4GiB},
                           RegionAllocator::AllowOverlap::Yes);
}

zx_status_t PciRootWindows::SubtractWindow(Space space, uint64_t base, uint64_t size) {
  if (space == Space::kIo) {
    return io_.SubtractRegion({.base = base, .size = size}, RegionAllocator::AllowIncomplete::Yes);
  }

  zx_status_t status =
      mmio32_.SubtractRegion({.base = base, .size = size}, RegionAllocator::AllowIncomplete::Yes);
  if (status != ZX_OK) {
    return status;
  }
  return mmio64_.SubtractRegion({.base = base, .size = size},
                                RegionAllocator::AllowIncomplete::Yes);
}

zx::result<PciRootWindows::Allocation> PciRootWindows::Allocate(Space space, zx_paddr_t base,
                                                                size_t size, bool low) {
  std::erase_if(allocations_, [](const auto& allocation) {
    return allocation.second.wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time::infinite_past(),
                                      nullptr) == ZX_OK;
  });

  if (space == Space::kIo) {
    return AllocateFrom(io_, "PCI Io", ZX_RSRC_KIND_IOPORT, base, size);
  }

  if (low) {
    return AllocateFrom(mmio32_, "PCI Mmio32", ZX_RSRC_KIND_MMIO, base, size);
  }

  zx::result allocation = AllocateFrom(mmio64_, "PCI Mmio64", ZX_RSRC_KIND_MMIO, base, size);
  // Some systems, such as some Intel NUCs, have devices with 64 bit BARs but describe no address
  // space above 4GiB in ACPI. If no particular base was requested, try the low window.
  if (allocation.is_error() && base == 0) {
    return AllocateFrom(mmio32_, "PCI Mmio32", ZX_RSRC_KIND_MMIO, base, size);
  }
  return allocation;
}

zx::result<PciRootWindows::Allocation> PciRootWindows::AllocateFrom(RegionAllocator& allocator,
                                                                    const char* name,
                                                                    uint32_t resource_kind,
                                                                    zx_paddr_t base, size_t size) {
  if (size == 0) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  if (resource_kind == ZX_RSRC_KIND_MMIO) {
    const uint64_t page_size = zx_system_get_page_size();
    // Return an error if an existing base isn't page aligned so the bus driver will reallocate.
    if (base % page_size != 0) {
      return zx::error(ZX_ERR_INVALID_ARGS);
    }
    size_t rounded_size = 0;
    if (add_overflow(size, page_size - 1, &rounded_size)) {
      return zx::error(ZX_ERR_INVALID_ARGS);
    }
    size = (rounded_size / page_size) * page_size;
  }

  // A non-zero |base| is a request for a specific range, usually for a device the firmware already
  // configured. Otherwise any naturally aligned range of the right size will do.
  RegionAllocator::Region::UPtr region;
  zx_status_t status = (base) ? allocator.GetRegion({.base = base, .size = size}, region)
                              : allocator.GetRegion(size, size, region);
  if (status != ZX_OK) {
    return zx::error(status);
  }

  const zx::unowned_resource& parent_resource =
      (resource_kind == ZX_RSRC_KIND_IOPORT) ? ioport_resource_ : mmio_resource_;
  Allocation allocation{.base = region->base};
  status =
      zx::resource::create(*parent_resource, resource_kind | ZX_RSRC_FLAG_EXCLUSIVE, region->base,
                           region->size, name, strlen(name), &allocation.resource);
  if (status != ZX_OK) {
    return zx::error(status);
  }

  zx::eventpair peer;
  status = zx::eventpair::create(0, &peer, &allocation.token);
  if (status != ZX_OK) {
    return zx::error(status);
  }

  allocations_.emplace_back(std::move(region), std::move(peer));
  return zx::ok(std::move(allocation));
}

}  // namespace acpi
