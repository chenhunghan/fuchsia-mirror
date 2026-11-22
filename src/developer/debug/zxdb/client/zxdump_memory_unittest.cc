// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/client/zxdump_memory.h"

#include <fcntl.h>
#include <lib/elfldltl/layout.h>
#include <lib/zxdump/task.h>
#include <zircon/syscalls/object.h>

#include <algorithm>
#include <cstring>
#include <string>
#include <string_view>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>

#include "src/developer/debug/zxdb/common/scoped_temp_file.h"
#include "src/lib/files/file.h"

namespace zxdb {

namespace {

// A fake process memory source. Each region holds a pattern of bytes, and the dump holds the
// first |dumped| bytes of each region.
class FakeSource : public ProcessMemorySource {
 public:
  struct FakeRegion {
    debug_ipc::AddressRegion region;
    uint64_t dumped = 0;
    uint8_t seed = 0;
  };

  // Adds a region whose first |dumped| bytes the dump holds.
  void Add(uint64_t base, uint64_t size, uint64_t dumped, uint8_t seed, bool add_region = true) {
    FakeRegion fake;
    fake.region.base = base;
    fake.region.size = size;
    fake.dumped = dumped;
    fake.seed = seed;
    fakes_.push_back(fake);
    if (add_region) {
      regions_.push_back(fake.region);
    }
  }

  static uint8_t ByteAt(const FakeRegion& fake, uint64_t address) {
    return static_cast<uint8_t>(fake.seed + (address - fake.region.base));
  }

  const std::vector<debug_ipc::AddressRegion>& GetRegions() const override { return regions_; }

  std::vector<uint8_t> ReadDumped(uint64_t address, uint64_t max_size) const override {
    for (const FakeRegion& fake : fakes_) {
      if (address < fake.region.base || address >= fake.region.base + fake.region.size) {
        continue;
      }
      const uint64_t dumped_end = fake.region.base + fake.dumped;
      if (address >= dumped_end) {
        return {};
      }
      const uint64_t count = std::min(max_size, dumped_end - address);
      std::vector<uint8_t> bytes(count);
      for (uint64_t i = 0; i < count; i++) {
        bytes[i] = ByteAt(fake, address + i);
      }
      return bytes;
    }
    return {};
  }

  const FakeRegion& fake(size_t i) const { return fakes_[i]; }

 private:
  std::vector<FakeRegion> fakes_;
  std::vector<debug_ipc::AddressRegion> regions_;
};

void ExpectBlock(const debug_ipc::MemoryBlock& block, uint64_t address, uint32_t size, bool valid) {
  EXPECT_EQ(address, block.address);
  EXPECT_EQ(size, block.size);
  EXPECT_EQ(valid, block.valid);
  EXPECT_EQ(valid ? size : 0u, block.data.size());
}

template <typename T>
std::string Bytes(const T& value) {
  return std::string(reinterpret_cast<const char*>(&value), sizeof(value));
}

void PadNote(std::string& data) { data.resize((data.size() + 3) / 4 * 4, '\0'); }

std::string MakeNote(std::string_view name, uint32_t type, std::string_view desc) {
  using Nhdr = elfldltl::Elf64<>::Nhdr;
  const std::string name_z = std::string(name) + '\0';
  Nhdr nhdr = {};
  nhdr.namesz = static_cast<uint32_t>(name_z.size());
  nhdr.descsz = static_cast<uint32_t>(desc.size());
  nhdr.type = type;
  std::string note = Bytes(nhdr) + name_z;
  PadNote(note);
  note += desc;
  PadNote(note);
  return note;
}

TEST(ZxdumpMemory, ZxdumpProcessSource) {
  using Ehdr = elfldltl::Elf64<>::Ehdr;
  using Phdr = elfldltl::Elf64<>::Phdr;

  zx_info_handle_basic_t basic = {};
  basic.koid = 4242;
  basic.type = ZX_OBJ_TYPE_PROCESS;

  zx_info_maps_t maps[2] = {};
  maps[0].base = 0x2000;
  maps[0].size = 0x2000;
  maps[0].depth = 3;
  maps[0].type = ZX_INFO_MAPS_TYPE_MAPPING;
  strncpy(maps[0].name, "stack", sizeof(maps[0].name) - 1);
  maps[0].u.mapping.mmu_flags =
      ZX_VM_PERM_READ | ZX_VM_PERM_WRITE;  // NOLINT(bugprone-signed-bitwise)
  maps[0].u.mapping.vmo_koid = 99;
  maps[0].u.mapping.vmo_offset = 0x100;
  maps[0].u.mapping.committed_bytes = 0x1000;

  maps[1].base = 0x1000;
  maps[1].size = 0x1000;
  maps[1].depth = 2;
  maps[1].type = ZX_INFO_MAPS_TYPE_MAPPING;
  strncpy(maps[1].name, "code", sizeof(maps[1].name) - 1);
  maps[1].u.mapping.mmu_flags =
      ZX_VM_PERM_READ | ZX_VM_PERM_EXECUTE;  // NOLINT(bugprone-signed-bitwise)

  const std::string notes =
      MakeNote("ZirconProcessInfo", ZX_INFO_HANDLE_BASIC, Bytes(basic)) +
      MakeNote("ZirconProcessInfo", ZX_INFO_PROCESS_MAPS,  // NOLINT(bugprone-signed-bitwise)
               std::string_view(reinterpret_cast<const char*>(maps), sizeof(maps)));
  const std::string payload = "0123456789abcdef";

  Ehdr ehdr = {};  // NOLINT(bugprone-invalid-enum-default-initialization)
  ehdr.magic = Ehdr::kMagic;
  ehdr.elfclass = elfldltl::ElfClass::k64;
  ehdr.elfdata = elfldltl::ElfData::k2Lsb;
  ehdr.ident_version = elfldltl::ElfVersion::kCurrent;
  ehdr.type = elfldltl::ElfType::kCore;
  ehdr.machine = elfldltl::ElfMachine::kAarch64;
  ehdr.version = elfldltl::ElfVersion::kCurrent;
  ehdr.phoff = sizeof(Ehdr);
  ehdr.ehsize = sizeof(Ehdr);
  ehdr.phentsize = sizeof(Phdr);
  ehdr.phnum = 2;

  std::string core = Bytes(ehdr) + std::string(2 * sizeof(Phdr), '\0');
  const uint64_t note_offset = core.size();
  core += notes;
  core.resize((core.size() + 0xfff) & ~size_t{0xfff}, '\0');
  const uint64_t load_offset = core.size();
  core += payload;
  core.resize((core.size() + 0xfff) & ~size_t{0xfff}, '\0');

  Phdr phdrs[2] = {};
  phdrs[0].type = elfldltl::ElfPhdrType::kNote;
  phdrs[0].offset = note_offset;
  phdrs[0].filesz = notes.size();
  phdrs[0].align = 4;

  phdrs[1].type = elfldltl::ElfPhdrType::kLoad;
  phdrs[1].flags = Phdr::kRead | Phdr::kWrite;
  phdrs[1].offset = load_offset;
  phdrs[1].vaddr = 0x2000;
  phdrs[1].filesz = payload.size();
  phdrs[1].memsz = 0x2000;
  phdrs[1].align = 0x1000;
  memcpy(core.data() + sizeof(Ehdr), phdrs, sizeof(phdrs));

  ScopedTempFile temp;
  ASSERT_TRUE(files::WriteFile(temp.name(), core));
  fbl::unique_fd fd(
      open(temp.name().c_str(), O_RDONLY | O_CLOEXEC));  // NOLINT(bugprone-signed-bitwise)
  ASSERT_TRUE(fd);

  zxdump::TaskHolder holder;
  auto inserted = holder.Insert(std::move(fd));
  ASSERT_TRUE(inserted.is_ok()) << inserted.error_value().op_;
  auto found = holder.root_job().find(4242);
  ASSERT_TRUE(found.is_ok());
  ASSERT_EQ(ZX_OBJ_TYPE_PROCESS, found->get().type());
  auto& process = static_cast<zxdump::Process&>(found->get());

  ZxdumpProcessSource source(process);
  const auto& regions = source.GetRegions();
  ASSERT_EQ(2u, regions.size());
  EXPECT_EQ("code", regions[0].name);
  EXPECT_EQ(0x1000u, regions[0].base);
  EXPECT_EQ(2u, regions[0].depth);
  EXPECT_TRUE(regions[0].read);
  EXPECT_FALSE(regions[0].write);
  EXPECT_TRUE(regions[0].execute);

  EXPECT_EQ("stack", regions[1].name);
  EXPECT_EQ(0x2000u, regions[1].base);
  EXPECT_EQ(0x2000u, regions[1].size);
  EXPECT_EQ(3u, regions[1].depth);
  EXPECT_EQ(99u, regions[1].vmo_koid);
  EXPECT_EQ(0x100u, regions[1].vmo_offset);
  EXPECT_EQ(0x1000u, regions[1].committed_bytes);
  EXPECT_TRUE(regions[1].read);
  EXPECT_TRUE(regions[1].write);
  EXPECT_FALSE(regions[1].execute);

  auto dumped = source.ReadDumped(0x2000, 0x100);
  EXPECT_EQ(std::string(dumped.begin(), dumped.end()), payload);
  EXPECT_TRUE(source.ReadDumped(0x2000 + payload.size(), 0x10).empty());
}

TEST(ZxdumpMemory, ReadSplitsAtGapAndCoalescesAdjacentValidRegions) {
  FakeSource source;
  source.Add(0x1000, 0x100, 0x100, 1);
  source.Add(0x1100, 0x20000, 0x20000, 2);  // Adjacent region > 64 KiB.
  source.Add(0x21300, 0x100, 0x100, 3);     // After a gap.

  ZxdumpMemory memory(source);
  auto blocks = memory.ReadMemoryBlocks(0xf80, 0x20400);
  ASSERT_EQ(4u, blocks.size());
  ExpectBlock(blocks[0], 0xf80, 0x80, false);
  ExpectBlock(blocks[1], 0x1000, 0x20100, true);
  ExpectBlock(blocks[2], 0x21100, 0x200, false);
  ExpectBlock(blocks[3], 0x21300, 0x80, true);

  EXPECT_EQ(FakeSource::ByteAt(source.fake(0), 0x1000), blocks[1].data[0]);
  EXPECT_EQ(FakeSource::ByteAt(source.fake(1), 0x1100), blocks[1].data[0x100]);
  EXPECT_EQ(FakeSource::ByteAt(source.fake(2), 0x21300), blocks[3].data[0]);
}

TEST(ZxdumpMemory, FallbackWithoutRegions) {
  FakeSource source;
  source.Add(0x1000, 0x100, 0x80, 7, /*add_region=*/false);
  ASSERT_TRUE(source.GetRegions().empty());

  ZxdumpMemory memory(source);
  auto blocks = memory.ReadMemoryBlocks(0x1000, 0x100);
  ASSERT_EQ(2u, blocks.size());
  ExpectBlock(blocks[0], 0x1000, 0x80, true);
  ExpectBlock(blocks[1], 0x1080, 0x80, false);

  std::vector<uint8_t> buf(0x80);
  ASSERT_TRUE(memory.ReadBytes(0x1000, 0x80, buf.data()).ok());
  EXPECT_EQ(FakeSource::ByteAt(source.fake(0), 0x1000), buf[0]);
}

// debug_ipc::MemoryBlock::size has 32 bits. A run longer than that splits into several blocks
// instead of a truncated size.
TEST(ZxdumpMemory, ReadSplitsRunsLongerThanBlockSize) {
  constexpr uint64_t kGiB = uint64_t{1} << 30u;
  constexpr uint64_t kBase = uint64_t{1} << 40u;
  constexpr uint64_t kSize = 5 * kGiB;
  FakeSource source;
  source.Add(kBase, kSize, 0, 1);  // Skipped, so no bytes are read.

  ZxdumpMemory memory(source);
  auto blocks = memory.ReadMemoryBlocks(kBase - kGiB, kSize + (2 * kGiB));
  ASSERT_FALSE(blocks.empty());
  uint64_t next = kBase - kGiB;
  for (const debug_ipc::MemoryBlock& block : blocks) {
    EXPECT_FALSE(block.valid);
    EXPECT_EQ(next, block.address);
    EXPECT_GT(block.size, 0u);
    next = block.address + block.size;
  }
  EXPECT_EQ(kBase + kSize + kGiB, next);
}

TEST(ZxdumpMemory, SkippedMemoryIsInvalid) {
  FakeSource source;
  source.Add(0x1000, 0x1000, 0, 1);
  source.Add(0x2000, 0x1000, 0x10, 2);

  ZxdumpMemory memory(source);
  auto blocks = memory.ReadMemoryBlocks(0x1ff0, 0x40);
  ASSERT_EQ(3u, blocks.size());
  ExpectBlock(blocks[0], 0x1ff0, 0x10, false);
  ExpectBlock(blocks[1], 0x2000, 0x10, true);
  ExpectBlock(blocks[2], 0x2010, 0x20, false);
}

TEST(ZxdumpMemory, ZeroAndUnmapped) {
  FakeSource source;
  source.Add(0x1000, 0x1000, 0x1000, 1);
  ZxdumpMemory memory(source);

  EXPECT_TRUE(memory.ReadMemoryBlocks(0x1000, 0).empty());

  auto blocks = memory.ReadMemoryBlocks(0, 0x10);
  ASSERT_EQ(1u, blocks.size());
  ExpectBlock(blocks[0], 0, 0x10, false);

  blocks = memory.ReadMemoryBlocks(0x9000, 0x10);
  ASSERT_EQ(1u, blocks.size());
  ExpectBlock(blocks[0], 0x9000, 0x10, false);
}

TEST(ZxdumpMemory, ReadBytes) {
  FakeSource source;
  source.Add(0x1000, 0x100, 0x100, 1);
  source.Add(0x1100, 0x100, 0x80, 2);
  ZxdumpMemory memory(source);

  // Across two regions.
  std::vector<uint8_t> buffer(0x100);
  ASSERT_TRUE(memory.ReadBytes(0x1080, 0x100, buffer.data()).ok());
  EXPECT_EQ(FakeSource::ByteAt(source.fake(0), 0x1080), buffer[0]);
  EXPECT_EQ(FakeSource::ByteAt(source.fake(1), 0x1100), buffer[0x80]);

  // Typed unwinder::Memory::Read helper.
  uint8_t byte = 0;
  EXPECT_TRUE(memory.Read(0x1010, byte).ok());
  EXPECT_EQ(FakeSource::ByteAt(source.fake(0), 0x1010), byte);

  // Into the skipped tail of the second region.
  EXPECT_TRUE(memory.ReadBytes(0x1170, 0x20, buffer.data()).has_err());
  // Unmapped.
  EXPECT_TRUE(memory.ReadBytes(0x5000, 1, buffer.data()).has_err());
  // Zero size.
  EXPECT_TRUE(memory.ReadBytes(0x5000, 0, buffer.data()).ok());
}

}  // namespace
}  // namespace zxdb
