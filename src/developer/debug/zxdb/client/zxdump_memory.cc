// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/client/zxdump_memory.h"

#include <lib/zxdump/task.h>
#include <zircon/syscalls/object.h>

#include <algorithm>
#include <cinttypes>
#include <cstddef>
#include <cstring>
#include <limits>
#include <span>

#include <safemath/checked_math.h>

namespace zxdb {

namespace {

constexpr uint64_t kMaxBlockSize = std::numeric_limits<uint32_t>::max();

}  // namespace

ZxdumpProcessSource::ZxdumpProcessSource(zxdump::Process& process) : process_(process) {
  auto maps = process_.get_info<ZX_INFO_PROCESS_MAPS>();  // NOLINT(bugprone-signed-bitwise)
  if (maps.is_error()) {
    return;
  }
  for (const zx_info_maps_t& map : *maps) {
    if (map.type != ZX_INFO_MAPS_TYPE_MAPPING || map.size == 0) {
      continue;
    }
    auto& region = regions_.emplace_back();
    region.name = std::string(map.name, strnlen(map.name, sizeof(map.name)));
    region.base = map.base;
    region.size = map.size;
    region.depth = map.depth;
    region.vmo_koid = map.u.mapping.vmo_koid;
    region.vmo_offset = map.u.mapping.vmo_offset;
    region.committed_bytes = map.u.mapping.committed_bytes;
    region.read =
        (map.u.mapping.mmu_flags & ZX_VM_PERM_READ) != 0;  // NOLINT(bugprone-signed-bitwise)
    region.write =
        (map.u.mapping.mmu_flags & ZX_VM_PERM_WRITE) != 0;  // NOLINT(bugprone-signed-bitwise)
    region.execute =
        (map.u.mapping.mmu_flags & ZX_VM_PERM_EXECUTE) != 0;  // NOLINT(bugprone-signed-bitwise)
  }
  std::ranges::sort(regions_, [](const debug_ipc::AddressRegion& a,
                                 const debug_ipc::AddressRegion& b) { return a.base < b.base; });
}

std::vector<uint8_t> ZxdumpProcessSource::ReadDumped(uint64_t address, uint64_t max_size) const {
  if (max_size == 0) {
    return {};
  }
  auto result = process_.read_memory<std::byte>(address, 1, zxdump::ReadMemorySize::kMore);
  if (result.is_error() || result->empty()) {
    return {};
  }
  std::span<const std::byte> view = **result;
  const size_t count = static_cast<size_t>(std::min<uint64_t>(max_size, view.size()));
  std::vector<uint8_t> out(count);
  memcpy(out.data(), view.data(), count);
  return out;
}

ZxdumpMemory::ZxdumpMemory(const ProcessMemorySource& source) : source_(source) {}

std::vector<debug_ipc::MemoryBlock> ZxdumpMemory::ReadMemoryBlocks(uint64_t address,
                                                                   uint64_t size) const {
  if (size == 0) {
    return {};
  }

  std::vector<debug_ipc::MemoryBlock> blocks;
  // Coalesces contiguous invalid or valid slices into the previous block of the same kind so
  // adjacent regions, gaps, or PT_LOAD segments do not produce back-to-back blocks with the same
  // validity. Without the 32-bit kMaxBlockSize cap on MemoryBlock::size, each helper would be a
  // single if/else check.
  auto append_invalid = [&blocks](uint64_t start, uint64_t len) {
    while (len > 0) {
      if (!blocks.empty() && !blocks.back().valid &&
          blocks.back().address + blocks.back().size == start &&
          blocks.back().size < kMaxBlockSize) {
        const uint64_t add = std::min(len, kMaxBlockSize - blocks.back().size);
        blocks.back().size += static_cast<uint32_t>(add);
        start += add;
        len -= add;
        continue;
      }
      const uint64_t chunk = std::min(len, kMaxBlockSize);
      auto& block = blocks.emplace_back();
      block.address = start;
      block.size = static_cast<uint32_t>(chunk);
      block.valid = false;
      start += chunk;
      len -= chunk;
    }
  };
  auto append_valid = [&blocks](uint64_t start, std::span<const uint8_t> data) {
    while (!data.empty()) {
      if (!blocks.empty() && blocks.back().valid &&
          blocks.back().address + blocks.back().size == start &&
          blocks.back().size < kMaxBlockSize) {
        const size_t add = static_cast<size_t>(
            std::min<uint64_t>(data.size(), kMaxBlockSize - blocks.back().size));
        blocks.back().data.insert(blocks.back().data.end(), data.begin(),
                                  data.begin() + static_cast<ptrdiff_t>(add));
        blocks.back().size += static_cast<uint32_t>(add);
        start += add;
        data = data.subspan(add);
        continue;
      }
      const size_t chunk = static_cast<size_t>(std::min<uint64_t>(data.size(), kMaxBlockSize));
      auto& block = blocks.emplace_back();
      block.address = start;
      block.size = static_cast<uint32_t>(chunk);
      block.valid = true;
      block.data.assign(data.begin(), data.begin() + static_cast<ptrdiff_t>(chunk));
      start += chunk;
      data = data.subspan(chunk);
    }
  };

  const uint64_t end =
      safemath::CheckAdd(address, size).ValueOrDefault(std::numeric_limits<uint64_t>::max());
  uint64_t cursor = address;
  // Reads dumped bytes across [cursor, range_end). Even inside a mapped region, the dump may
  // only contain a prefix of the mapping (or span multiple PT_LOAD segments) and elide the tail
  // (filesz < memsz), so ReadDumped can return fewer bytes than requested or an empty vector.
  auto read_range = [&](uint64_t range_end) {
    while (cursor < range_end) {
      const uint64_t want = std::min(range_end - cursor, kMaxBlockSize);
      std::vector<uint8_t> bytes = source_.ReadDumped(cursor, want);
      if (bytes.empty()) {
        append_invalid(cursor, range_end - cursor);
        cursor = range_end;
        break;
      }
      const uint64_t got = bytes.size();
      append_valid(cursor, bytes);
      cursor += got;
    }
  };

  const auto& regions = source_.GetRegions();
  if (regions.empty()) {
    read_range(end);
    return blocks;
  }

  auto region = std::lower_bound(  // NOLINT(modernize-use-ranges)
      regions.begin(), regions.end(), cursor,
      [](const debug_ipc::AddressRegion& r, uint64_t addr) { return r.base + r.size <= addr; });

  while (cursor < end) {
    if (region == regions.end() || region->base > cursor) {
      const uint64_t gap_end = region == regions.end() ? end : std::min(end, region->base);
      append_invalid(cursor, gap_end - cursor);
      cursor = gap_end;
      continue;
    }

    const uint64_t region_limits = region->base + region->size;
    const uint64_t region_end = std::min(end, region_limits);
    read_range(region_end);
    if (cursor >= region_limits) {
      ++region;
    }
  }

  return blocks;
}

unwinder::Error ZxdumpMemory::ReadBytes(uint64_t addr, uint64_t size, void* dst) {
  uint8_t* out = static_cast<uint8_t*>(dst);
  uint64_t copied = 0;
  while (copied < size) {
    uint64_t read_addr = 0;
    if (!safemath::CheckAdd(addr, copied).AssignIfValid(&read_addr)) {
      return unwinder::Error("Dump memory read address overflow at 0x%" PRIx64, addr);
    }
    std::vector<uint8_t> bytes = source_.ReadDumped(read_addr, size - copied);
    if (bytes.empty()) {
      return unwinder::Error("Dump memory is unavailable at 0x%" PRIx64, read_addr);
    }
    memcpy(out + copied, bytes.data(), bytes.size());
    copied += bytes.size();
  }
  return unwinder::Success();
}

}  // namespace zxdb
