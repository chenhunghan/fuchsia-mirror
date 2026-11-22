// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_ZXDUMP_MEMORY_H_
#define SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_ZXDUMP_MEMORY_H_

#include <cstdint>
#include <vector>

#include "src/developer/debug/ipc/records.h"
#include "src/lib/unwinder/memory.h"

namespace zxdump {
class Process;
}  // namespace zxdump

namespace zxdb {

// Reads mappings and dumped bytes of one process. Separated from zxdump::Process so that unit
// tests can build memory layouts without a real core file.
class ProcessMemorySource {
 public:
  virtual ~ProcessMemorySource() = default;

  // Returns the process mappings sorted by base address.
  virtual const std::vector<debug_ipc::AddressRegion>& GetRegions() const = 0;

  // Reads up to |max_size| contiguous dumped bytes starting at |address|. Returns an empty vector
  // when |address| is not present in the dump (unmapped or elided).
  virtual std::vector<uint8_t> ReadDumped(uint64_t address, uint64_t max_size) const = 0;
};

// ProcessMemorySource backed by a zxdump::Process.
class ZxdumpProcessSource : public ProcessMemorySource {
 public:
  // |process| must outlive this object.
  explicit ZxdumpProcessSource(zxdump::Process& process);

  const std::vector<debug_ipc::AddressRegion>& GetRegions() const override { return regions_; }
  std::vector<uint8_t> ReadDumped(uint64_t address, uint64_t max_size) const override;

 private:
  zxdump::Process& process_;
  // Virtual memory mappings from the process's ZX_INFO_PROCESS_MAPS note, sorted by base address.
  // Empty if the dump omitted the ZX_INFO_PROCESS_MAPS note (for example, gcore --no-maps).
  std::vector<debug_ipc::AddressRegion> regions_;
};

// Reads virtual memory of a dumped process, splitting reads into valid and invalid MemoryBlocks
// at mapping and segment boundaries.
class ZxdumpMemory : public unwinder::Memory {
 public:
  // |source| must outlive this object.
  explicit ZxdumpMemory(const ProcessMemorySource& source);

  const std::vector<debug_ipc::AddressRegion>& regions() const { return source_.GetRegions(); }

  // Reads [address, address + size) as contiguous valid and invalid MemoryBlocks.
  // Adjacent valid (and adjacent invalid) ranges are coalesced so callers such as
  // MemoryDump::Read and Disassembler::DisassembleDump do not see artificial block boundaries
  // within readable memory (unless a single block reaches the uint32_t MemoryBlock::size limit).
  std::vector<debug_ipc::MemoryBlock> ReadMemoryBlocks(uint64_t address, uint64_t size) const;

  // unwinder::Memory implementation. Copies |size| bytes at |addr| into |dst|, or returns an error
  // if any byte in the range is not available in the dump.
  unwinder::Error ReadBytes(uint64_t addr, uint64_t size, void* dst) override;

 private:
  const ProcessMemorySource& source_;
};

}  // namespace zxdb

#endif  // SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_ZXDUMP_MEMORY_H_
