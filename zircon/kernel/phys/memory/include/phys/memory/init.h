// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_PHYS_MEMORY_INCLUDE_PHYS_MEMORY_INIT_H_
#define ZIRCON_KERNEL_PHYS_MEMORY_INCLUDE_PHYS_MEMORY_INIT_H_

#include <lib/memalloc/range.h>

#include <ktl/optional.h>
#include <ktl/span.h>
#include <phys/runtime/early-boot.h>

// Defined in <phys/address-space/address-space.h>.
class AddressSpace;

// Read the boot loader data to initialize memory for "allocation.h" APIs.
// The argument is the pointer to the ZBI, Multiboot info, Device Tree, etc.
// depending on the particular phys environment.  This panics if no memory is
// found for the allocator.
//
// In the case of ZBI booting, an EarlyBootZbi should additionally be provided
// - and consumed - as well for safe iteration if caches were left disabled on
// boot. If caches were left disabled, they are re-enabled in this routine.
//
// Further, if an address space object is provided, it will be used to install
// an identity-mapped virtual address space via ArchSetUpAddressSpace().
void InitMemory(const void* bootloader_data, ktl::optional<EarlyBootZbi> zbi,
                AddressSpace* aspace = nullptr);

// Performs the protocol-agnostic core of physical memory and address space
// bring-up from discovered memory and special ranges:
// * Page-aligns `special_ranges` in place to `AddressSpace::kPageSize`;
// * Initializes `Allocation::GetPool()` (with early-boot cache synchronization
//   when `aspace` is provided), applies `kernel.memory-limit-mb`, and logs the
//   resulting memory ranges;
// * When `aspace` is non-null, sets up the identity-mapped address space via
//   `ArchSetUpAddressSpace()`.
void InitMemoryFromRanges(ktl::span<memalloc::Range> ranges,
                          ktl::span<memalloc::Range> special_ranges,
                          AddressSpace* aspace = nullptr);

#endif  // ZIRCON_KERNEL_PHYS_MEMORY_INCLUDE_PHYS_MEMORY_INIT_H_
