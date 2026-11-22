// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "phys/memory/init.h"

#include <lib/boot-options/boot-options.h>
#include <lib/memalloc/pool.h>
#include <lib/memalloc/range.h>
#include <zircon/assert.h>

#include <ktl/algorithm.h>
#include <ktl/limits.h>
#include <ktl/span.h>
#include <ktl/utility.h>
#include <phys/address-space/address-space.h>
#include <phys/allocation/allocation.h>
#include <phys/runtime/early-boot.h>
#include <phys/runtime/main.h>

#include <ktl/enforce.h>

void InitMemoryFromRanges(ktl::span<memalloc::Range> ranges,
                          ktl::span<memalloc::Range> special_ranges, AddressSpace* aspace) {
  // We page-align the special ranges (rounding down start addresses and
  // rounding up sizes) since these ranges in practice are handed off to the
  // kernel as page-aligned VMOs. Aligning them here avoids unintended
  // inclusions in tail and arena selection complications (which also deals in
  // whole pages).
  for (memalloc::Range& range : special_ranges) {
    range = range.AlignTo(AddressSpace::kPageSize);
  }

  // If the data cache is disabled, its re-enabling will occur within
  // ArchSetUpAddressSpace(), which is gated on providing a non-null
  // AddressSpace. In the case where we intend to re-enable it, we must ensure
  // the coherence of allocations made now with the re-enabled state.
  memalloc::Pool::AccessCallback early_access;
  if (aspace) {
    early_access = [](uint64_t addr, uint64_t size) {
      ArchEarlyBootSyncAllocation(reinterpret_cast<uintptr_t>(addr), static_cast<size_t>(size));
    };
  }

  Allocation::Init(ranges, special_ranges, ktl::move(early_access));

  // Now that memory is accounted for, truncate the address range before any
  // further allocations.
  memalloc::Pool& pool = Allocation::GetPool();
  if (BootOptions::Get() && BootOptions::Get()->memory_limit_mb > 0) {
    constexpr uint64_t kBytesPerMib = 0x100'000;
    uint64_t limit_mb = ktl::min(ktl::numeric_limits<uint64_t>::max() / kBytesPerMib,
                                 BootOptions::Get()->memory_limit_mb);
    ZX_ASSERT(pool.TruncateTotalRam(kBytesPerMib * limit_mb).is_ok());
  }

  // Set up our own address space.
  if (aspace) {
    ArchSetUpAddressSpace(*aspace);

    // Data cache is now re-enabled and all allocation access should be kosher
    // again.
    pool.set_access_callback([](uint64_t addr, uint64_t size) {});
  }

  pool.PrintMemoryRanges(ProgramName());
}
