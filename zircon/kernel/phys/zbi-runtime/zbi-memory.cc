// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/memalloc/range.h>
#include <lib/zbi-format/memory.h>
#include <lib/zbi-format/zbi.h>
#include <lib/zbitl/error-stdio.h>
#include <lib/zbitl/view.h>
#include <zircon/assert.h>

#include <ktl/optional.h>
#include <ktl/span.h>
#include <phys/memory/init.h>
#include <phys/runtime/early-boot.h>
#include <phys/runtime/self.h>

#include <ktl/enforce.h>

void InitMemory(const void* zbi_ptr, ktl::optional<EarlyBootZbi> zbi, AddressSpace* aspace) {
  ktl::span<zbi_mem_range_t> zbi_ranges;
  ktl::optional<memalloc::Range> nvram_range;

  ZX_DEBUG_ASSERT(zbi);

  for (auto [header, wrapped_payload] : *zbi) {
    switch (header->type) {
      case ZBI_TYPE_MEM_CONFIG: {
        ktl::span payload = wrapped_payload.get();
        zbi_ranges = {
            const_cast<zbi_mem_range_t*>(reinterpret_cast<const zbi_mem_range_t*>(payload.data())),
            payload.size_bytes() / sizeof(zbi_mem_range_t)};
        break;
      }

      case ZBI_TYPE_NVRAM: {
        ktl::span payload = wrapped_payload.get();
        ZX_ASSERT(payload.size_bytes() >= sizeof(zbi_nvram_t));
        const zbi_nvram_t* nvram = reinterpret_cast<const zbi_nvram_t*>(payload.data());
        nvram_range = {
            .addr = nvram->base,
            .size = nvram->length,
            .type = memalloc::Type::kNvram,
        };
        break;
      }
    }
  }
  if (auto result = zbi->take_error(); result.is_error()) {
    zbitl::PrintViewError(result.error_value());
    ZX_PANIC("error occurred while parsing the data ZBI");
  }

  ZX_ASSERT_MSG(!zbi_ranges.empty(), "no MEM_CONFIG item found in the data ZBI");

  uint64_t phys_start = reinterpret_cast<uint64_t>(PHYS_LOAD_ADDRESS);
  uint64_t phys_end = reinterpret_cast<uint64_t>(_end);
  memalloc::Range special_memory_ranges[3] = {
      {
          .addr = phys_start,
          .size = phys_end - phys_start,
          .type = memalloc::Type::kPhysKernel,
      },
      {
          .addr = reinterpret_cast<uint64_t>(zbi_ptr),
          .size = zbi->size_bytes(),
          .type = memalloc::Type::kDataZbi,
      },
  };

  ktl::span<memalloc::Range> special_ranges(special_memory_ranges);
  if (nvram_range) {
    special_ranges.back() = *nvram_range;
  } else {
    special_ranges = special_ranges.subspan(0, special_ranges.size() - 1);
  }

  InitMemoryFromRanges(memalloc::AsRanges(zbi_ranges), special_ranges, aspace);
}
