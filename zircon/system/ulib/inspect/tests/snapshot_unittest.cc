// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/fzl/owned-vmo-mapper.h>
#include <lib/inspect/cpp/vmo/block.h>
#include <lib/inspect/cpp/vmo/limits.h>
#include <lib/inspect/cpp/vmo/snapshot.h>

#include <zxtest/zxtest.h>

namespace {

using inspect::BackingBuffer;
using inspect::Snapshot;
using inspect::internal::Block;
using inspect::internal::BlockType;
using inspect::internal::FreeBlockFields;
using inspect::internal::GetBlock;
using inspect::internal::HeaderBlockFields;
using inspect::internal::kMagicNumber;
using inspect::internal::kMaxVmoSize;
using inspect::internal::kMinOrderSize;
using inspect::internal::kMinVmoSize;
using inspect::internal::kVmoHeaderBlockSize;
using inspect::internal::kVmoHeaderOrder;
using inspect::internal::SetHeaderVmoSize;

TEST(Snapshot, ValidRead) {
  fzl::OwnedVmoMapper vmo;
  ASSERT_OK(vmo.CreateAndMap(4096, "test"));
  memset(vmo.start(), 'a', 4096);
  Block* header = reinterpret_cast<Block*>(vmo.start());
  header->header = HeaderBlockFields::Order::Make(kVmoHeaderOrder) |
                   HeaderBlockFields::Type::Make(BlockType::kHeader) |
                   HeaderBlockFields::Version::Make(0);
  memcpy(&header->header_data[4], kMagicNumber, 4);
  header->payload.u64 = 0;
  SetHeaderVmoSize(header, vmo.size());

  Snapshot snapshot;
  zx_status_t status = Snapshot::Create(vmo.vmo(), &snapshot);

  ASSERT_OK(status);
  ASSERT_EQ(4096u, snapshot.size());

  // Make sure that the data was actually fully copied to the snapshot.
  std::vector<uint8_t> buf;
  buf.resize(4096u - kVmoHeaderBlockSize);
  memset(buf.data(), 'a', buf.size());
  EXPECT_EQ(0, memcmp(snapshot.data() + kVmoHeaderBlockSize, buf.data(), buf.size()));
}

TEST(Snapshot, ReadFailsWithBadVersion) {
  fzl::OwnedVmoMapper vmo;
  ASSERT_OK(vmo.CreateAndMap(4096, "test"));
  Block* header = reinterpret_cast<Block*>(vmo.start());
  header->header = HeaderBlockFields::Order::Make(kVmoHeaderOrder) |
                   HeaderBlockFields::Type::Make(BlockType::kHeader) |
                   HeaderBlockFields::Version::Make(1ul << 15);
  memcpy(&header->header_data[4], kMagicNumber, 4);
  header->payload.u64 = 0;
  Snapshot snapshot;
  zx_status_t status = Snapshot::Create(vmo.vmo(), &snapshot);

  EXPECT_EQ(ZX_ERR_INTERNAL, status);
}

TEST(Snapshot, InvalidBufferSize) {
  for (size_t i = 0; i < kMinOrderSize; i++) {
    Snapshot snapshot;
    std::vector<uint8_t> buffer;
    buffer.resize(i);
    EXPECT_EQ(ZX_ERR_INVALID_ARGS, Snapshot::Create(BackingBuffer(std::move(buffer)), &snapshot));
  }
}

TEST(Snapshot, GetBlock) {
  fzl::OwnedVmoMapper vmo;
  ASSERT_OK(vmo.CreateAndMap(4096, "test"));
  memset(vmo.start(), 'a', 4096);
  Block* header = reinterpret_cast<Block*>(vmo.start());
  header->header = HeaderBlockFields::Order::Make(kVmoHeaderOrder) |
                   HeaderBlockFields::Type::Make(BlockType::kHeader) |
                   HeaderBlockFields::Version::Make(0);
  memcpy(&header->header_data[4], kMagicNumber, 4);
  header->payload.u64 = 0;
  SetHeaderVmoSize(header, vmo.size());

  {
    Snapshot snapshot;
    zx_status_t status = Snapshot::Create(vmo.vmo(), &snapshot);
    EXPECT_OK(status);
    // Can get block 0.
    EXPECT_NE(nullptr, GetBlock(&snapshot, 0));
    // Cannot get block past the end of the snapshot.
    EXPECT_EQ(nullptr, GetBlock(&snapshot, 4096 / kMinOrderSize));
  }

  Block* tester =
      reinterpret_cast<Block*>(reinterpret_cast<uint8_t*>(vmo.start()) + 4096 - kMinOrderSize * 2);
  size_t tester_index = 4096 / kMinOrderSize - 2;

  {
    tester->header =
        FreeBlockFields::Order::Make(1) | FreeBlockFields::Type::Make(BlockType::kFree);

    Snapshot snapshot;
    zx_status_t status = Snapshot::Create(vmo.vmo(), &snapshot);
    EXPECT_OK(status);
    // Can get tester, since it's adjacent to the end of the snapshot.
    EXPECT_NE(nullptr, GetBlock(&snapshot, tester_index));
  }
  {
    // Set the order to be too large for the buffer
    tester->header =
        FreeBlockFields::Order::Make(2) | FreeBlockFields::Type::Make(BlockType::kFree);

    Snapshot snapshot;
    zx_status_t status = Snapshot::Create(vmo.vmo(), &snapshot);
    EXPECT_OK(status);
    // Cannot get block, since its order is too large for the remaining space.
    EXPECT_EQ(nullptr, GetBlock(&snapshot, tester_index));
  }
}

TEST(Snapshot, InvalidWritePending) {
  fzl::OwnedVmoMapper vmo;
  ASSERT_OK(vmo.CreateAndMap(4096, "test"));
  Block* header = reinterpret_cast<Block*>(vmo.start());
  header->header = HeaderBlockFields::Order::Make(kVmoHeaderOrder) |
                   HeaderBlockFields::Type::Make(BlockType::kHeader) |
                   HeaderBlockFields::Version::Make(0);
  memcpy(&header->header_data[4], kMagicNumber, 4);
  header->payload.u64 = 1;

  Snapshot snapshot;
  zx_status_t status = Snapshot::Create(vmo.vmo(), &snapshot);

  EXPECT_EQ(ZX_ERR_INTERNAL, status);
  EXPECT_FALSE(!!snapshot);
}

TEST(Snapshot, ValidPendingSkipCheck) {
  fzl::OwnedVmoMapper vmo;
  ASSERT_OK(vmo.CreateAndMap(4096, "test"));
  Block* header = reinterpret_cast<Block*>(vmo.start());
  header->header = HeaderBlockFields::Order::Make(kVmoHeaderOrder) |
                   HeaderBlockFields::Type::Make(BlockType::kHeader) |
                   HeaderBlockFields::Version::Make(0);
  memcpy(&header->header_data[4], kMagicNumber, 4);
  header->payload.u64 = 1;
  SetHeaderVmoSize(header, vmo.size());

  Snapshot snapshot;
  zx_status_t status = Snapshot::Create(
      vmo.vmo(), {.read_attempts = 100, .skip_consistency_check = true}, &snapshot);
  EXPECT_OK(status);
  EXPECT_EQ(4096u, snapshot.size());
}

TEST(Snapshot, InvalidGenerationChange) {
  fzl::OwnedVmoMapper vmo;
  ASSERT_OK(vmo.CreateAndMap(4096, "test"));
  Block* header = reinterpret_cast<Block*>(vmo.start());
  header->header = HeaderBlockFields::Order::Make(kVmoHeaderOrder) |
                   HeaderBlockFields::Type::Make(BlockType::kHeader) |
                   HeaderBlockFields::Version::Make(0);
  memcpy(&header->header_data[4], kMagicNumber, 4);
  header->payload.u64 = 0;

  Snapshot snapshot;
  zx_status_t status = Snapshot::Create(
      vmo.vmo(), Snapshot::kDefaultOptions,
      [header](const uint8_t* buffer, size_t buffer_size) { header->payload.u64 += 2; }, &snapshot);

  EXPECT_EQ(ZX_ERR_INTERNAL, status);
}

TEST(Snapshot, InvalidGenerationChangeFinalStep) {
  fzl::OwnedVmoMapper vmo;
  ASSERT_OK(vmo.CreateAndMap(4096, "test"));
  Block* header = reinterpret_cast<Block*>(vmo.start());
  header->header = HeaderBlockFields::Order::Make(kVmoHeaderOrder) |
                   HeaderBlockFields::Type::Make(BlockType::kHeader) |
                   HeaderBlockFields::Version::Make(0);
  memcpy(&header->header_data[4], kMagicNumber, 4);
  header->payload.u64 = 0;

  int times_called = 0;
  Snapshot snapshot;
  zx_status_t status = Snapshot::Create(
      vmo.vmo(), Snapshot::Options{.read_attempts = 1, .skip_consistency_check = false},
      [&](const uint8_t* buffer, size_t buffer_size) {
        // Only change the generation count after the second read obtaining the whole buffer.
        if (++times_called == 2) {
          header->payload.u64 += 2;
        }
      },
      &snapshot);

  EXPECT_EQ(ZX_ERR_INTERNAL, status);
}

TEST(Snapshot, ValidGenerationChangeSkipCheck) {
  fzl::OwnedVmoMapper vmo;
  ASSERT_OK(vmo.CreateAndMap(4096, "test"));
  Block* header = reinterpret_cast<Block*>(vmo.start());
  header->header = HeaderBlockFields::Order::Make(kVmoHeaderOrder) |
                   HeaderBlockFields::Type::Make(BlockType::kHeader) |
                   HeaderBlockFields::Version::Make(0);
  memcpy(&header->header_data[4], kMagicNumber, 4);
  header->payload.u64 = 0;
  SetHeaderVmoSize(header, vmo.size());

  Snapshot snapshot;
  zx_status_t status = Snapshot::Create(
      vmo.vmo(), {.read_attempts = 100, .skip_consistency_check = true},
      [header](const uint8_t* buffer, size_t buffer_size) { header->payload.u64 += 2; }, &snapshot);

  EXPECT_OK(status);
  EXPECT_EQ(4096u, snapshot.size());
}

TEST(Snapshot, InvalidBadMagicNumber) {
  fzl::OwnedVmoMapper vmo;
  ASSERT_OK(vmo.CreateAndMap(4096, "test"));
  Block* header = reinterpret_cast<Block*>(vmo.start());
  header->header = HeaderBlockFields::Order::Make(kVmoHeaderOrder) |
                   HeaderBlockFields::Type::Make(BlockType::kHeader) |
                   HeaderBlockFields::Version::Make(0);
  header->payload.u64 = 0;

  Snapshot snapshot;
  zx_status_t status = Snapshot::Create(vmo.vmo(), &snapshot);

  EXPECT_EQ(ZX_ERR_INTERNAL, status);
}

TEST(Snapshot, InvalidBadMagicNumberSkipCheck) {
  fzl::OwnedVmoMapper vmo;
  ASSERT_OK(vmo.CreateAndMap(4096, "test"));
  Block* header = reinterpret_cast<Block*>(vmo.start());
  header->header = HeaderBlockFields::Order::Make(kVmoHeaderOrder) |
                   HeaderBlockFields::Type::Make(BlockType::kHeader) |
                   HeaderBlockFields::Version::Make(0);
  header->payload.u64 = 0;

  Snapshot snapshot;
  zx_status_t status = Snapshot::Create(
      vmo.vmo(), {.read_attempts = 100, .skip_consistency_check = true}, &snapshot);

  EXPECT_EQ(ZX_ERR_INTERNAL, status);
}

TEST(Snapshot, FrozenVmo) {
  fzl::OwnedVmoMapper vmo;
  ASSERT_OK(vmo.CreateAndMap(4096, "test"));
  Block* header = reinterpret_cast<Block*>(vmo.start());
  header->header = HeaderBlockFields::Order::Make(kVmoHeaderOrder) |
                   HeaderBlockFields::Type::Make(BlockType::kHeader) |
                   HeaderBlockFields::Version::Make(0);
  memcpy(&header->header_data[4], kMagicNumber, 4);
  header->payload.u64 = inspect::internal::kVmoFrozen;

  zx::vmo child;
  ASSERT_OK(vmo.vmo().create_child(ZX_VMO_CHILD_SNAPSHOT | ZX_VMO_CHILD_NO_WRITE, 0, 4096, &child));

  Snapshot snapshot;
  zx_status_t status =
      Snapshot::Create(child, {.read_attempts = 100, .skip_consistency_check = true}, &snapshot);

  EXPECT_EQ(ZX_OK, status);

  const auto* header_block = reinterpret_cast<Block const*>(snapshot.data());
  EXPECT_EQ(inspect::internal::kVmoFrozen, header_block->payload.u64);
}

TEST(Snapshot, VmoWithUnusedSpace) {
  fzl::OwnedVmoMapper vmo;
  size_t size = 4 * kMinVmoSize;
  ASSERT_OK(vmo.CreateAndMap(size, "test"));
  memset(vmo.start(), 'a', size);
  Block* header = reinterpret_cast<Block*>(vmo.start());
  header->header = HeaderBlockFields::Order::Make(kVmoHeaderOrder) |
                   HeaderBlockFields::Type::Make(BlockType::kHeader) |
                   HeaderBlockFields::Version::Make(0);
  memcpy(&header->header_data[4], kMagicNumber, 4);
  header->payload.u64 = 0;
  SetHeaderVmoSize(header, kMinVmoSize);
  Snapshot snapshot;
  zx_status_t status = Snapshot::Create(vmo.vmo(), &snapshot);

  ASSERT_OK(status);
  ASSERT_EQ(kMinVmoSize, snapshot.size());

  // Make sure that the data was actually fully copied to the snapshot.
  std::vector<uint8_t> buf;
  buf.resize(kMinVmoSize - kVmoHeaderBlockSize);
  memset(buf.data(), 'a', buf.size());
  EXPECT_EQ(0, memcmp(snapshot.data() + kVmoHeaderBlockSize, buf.data(), buf.size()));
}

TEST(Snapshot, TruncatedSnapshotOfVeryLargeVmo) {
  fzl::OwnedVmoMapper vmo;
  size_t size = 2 * kMaxVmoSize;
  ASSERT_OK(vmo.CreateAndMap(size, "test"));
  memset(vmo.start(), 'a', size);
  Block* header = reinterpret_cast<Block*>(vmo.start());
  header->header = HeaderBlockFields::Order::Make(kVmoHeaderOrder) |
                   HeaderBlockFields::Type::Make(BlockType::kHeader) |
                   HeaderBlockFields::Version::Make(0);
  memcpy(&header->header_data[4], kMagicNumber, 4);
  header->payload.u64 = 0;
  SetHeaderVmoSize(header, vmo.size());

  Snapshot snapshot;
  zx_status_t status = Snapshot::Create(vmo.vmo(), &snapshot);

  ASSERT_OK(status);
  ASSERT_EQ(kMaxVmoSize, snapshot.size());

  // Make sure that the data was actually fully copied to the snapshot.
  std::vector<uint8_t> buf;
  buf.resize(kMaxVmoSize - kVmoHeaderBlockSize);
  memset(buf.data(), 'a', buf.size());
  EXPECT_EQ(0, memcmp(snapshot.data() + kVmoHeaderBlockSize, buf.data(), buf.size()));
}

TEST(Snapshot, HeaderWithoutSizeInfo) {
  fzl::OwnedVmoMapper vmo;
  size_t size = 2 * kMinVmoSize;
  ASSERT_OK(vmo.CreateAndMap(size, "test"));
  memset(vmo.start(), 'a', size);
  Block* header = reinterpret_cast<Block*>(vmo.start());
  header->header = HeaderBlockFields::Order::Make(0) |
                   HeaderBlockFields::Type::Make(BlockType::kHeader) |
                   HeaderBlockFields::Version::Make(0);
  memcpy(&header->header_data[4], kMagicNumber, 4);
  header->payload.u64 = 0;

  Snapshot snapshot;
  zx_status_t status = Snapshot::Create(vmo.vmo(), &snapshot);

  ASSERT_OK(status);
  ASSERT_EQ(size, snapshot.size());

  // Make sure that the data was actually fully copied to the snapshot.
  std::vector<uint8_t> buf;
  buf.resize(size - kVmoHeaderBlockSize);
  memset(buf.data(), 'a', buf.size());
  EXPECT_EQ(0, memcmp(snapshot.data() + kVmoHeaderBlockSize, buf.data(), buf.size()));
}

TEST(Snapshot, FrozenMutableVmoFails) {
  fzl::OwnedVmoMapper vmo;
  ASSERT_OK(vmo.CreateAndMap(4096, "test"));
  Block* header = reinterpret_cast<Block*>(vmo.start());
  header->header = HeaderBlockFields::Order::Make(kVmoHeaderOrder) |
                   HeaderBlockFields::Type::Make(BlockType::kHeader) |
                   HeaderBlockFields::Version::Make(0);
  memcpy(&header->header_data[4], kMagicNumber, 4);
  header->payload.u64 = inspect::internal::kVmoFrozen;

  Snapshot snapshot;
  zx_status_t status = Snapshot::Create(vmo.vmo(), Snapshot::Options{}, &snapshot);

  EXPECT_EQ(ZX_ERR_BAD_STATE, status);
}

TEST(Snapshot, GetArraySlotBounds) {
  Block block;
  block.header = inspect::internal::BlockFields::Type::Make(BlockType::kArrayValue) |
                 inspect::internal::BlockFields::Order::Make(1);
  block.payload.u64 = inspect::internal::ArrayBlockPayload::EntryType::Make(BlockType::kIntValue);

  // Capacity is (32 - 8 - 8) / 8 = 2.
  // Index 0 and 1 are valid.
  EXPECT_NE(nullptr, inspect::internal::GetArraySlot<int64_t>(&block, 0));
  EXPECT_NE(nullptr, inspect::internal::GetArraySlot<int64_t>(&block, 1));

  // Index 2 is equal to capacity and must be rejected for both mutable and const blocks.
  EXPECT_EQ(nullptr, inspect::internal::GetArraySlot<int64_t>(&block, 2));
  EXPECT_EQ(nullptr,
            inspect::internal::GetArraySlot<const int64_t>(static_cast<const Block*>(&block), 2));

  // Index 3 is greater than capacity and must be rejected.
  EXPECT_EQ(nullptr, inspect::internal::GetArraySlot<int64_t>(&block, 3));
}

TEST(Snapshot, GetArraySlotForStringBounds) {
  Block block;
  block.header = inspect::internal::BlockFields::Type::Make(BlockType::kArrayValue) |
                 inspect::internal::BlockFields::Order::Make(1);
  block.payload.u64 =
      inspect::internal::ArrayBlockPayload::EntryType::Make(BlockType::kStringReference);

  // Capacity is (32 - 8 - 8) / 4 = 4.
  // Index 0, 1, 2, 3 are valid.
  EXPECT_TRUE(inspect::internal::GetArraySlotForString(&block, 0).has_value());
  EXPECT_TRUE(inspect::internal::GetArraySlotForString(&block, 3).has_value());

  // Index 4 is equal to capacity and must be rejected.
  EXPECT_FALSE(inspect::internal::GetArraySlotForString(&block, 4).has_value());
}

}  // namespace
