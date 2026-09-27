// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <zircon/assert.h>
#include <zircon/errors.h>

#include <cstdint>
#include <memory>
#include <utility>
#include <vector>

#include <fbl/ref_ptr.h>
#include <gtest/gtest.h>

#include "src/lib/testing/predicates/status.h"
#include "src/storage/blobfs/blob.h"
#include "src/storage/blobfs/blobfs.h"
#include "src/storage/blobfs/format.h"
#include "src/storage/blobfs/test/blob_utils.h"
#include "src/storage/blobfs/test/blobfs_test_setup.h"
#include "src/storage/lib/block_client/cpp/block_device.h"
#include "src/storage/lib/block_protocol/block-fifo.h"
#include "src/storage/lib/buffer/vmo_buffer.h"

namespace blobfs {
namespace {

constexpr uint32_t kBlockSize = 512;
constexpr uint32_t kNumBlocks = 400 * kBlobfsBlockSize / kBlockSize;

class BlobfsVerifyHealthTest : public testing::Test {
 protected:
  void SetUp() override { EXPECT_EQ(ZX_OK, setup_.CreateFormatMount(kNumBlocks, kBlockSize)); }

  fbl::RefPtr<Blob> InstallBlob(const TestDeliveryBlob& delivery_blob) {
    auto blob = CreateBlob(*setup_.blobfs(), delivery_blob);
    ZX_ASSERT(blob.is_ok());
    return std::move(blob).value();
  }

  void CorruptBlob(const Digest& digest) {
    uint64_t block;
    {
      auto blob = GetBlob(*setup_.blobfs(), digest);
      ASSERT_OK(blob);
      block = setup_.blobfs()->GetNode(blob->Ino())->extents[0].Start() +
              DataStartBlock(setup_.blobfs()->Info());
    }

    // Unmount.
    std::unique_ptr<block_client::BlockDevice> device = setup_.Unmount();

    // Read the block that contains the blob.
    storage::VmoBuffer buffer;
    ASSERT_EQ(buffer.Initialize(device.get(), 1, kBlobfsBlockSize, "test_buffer"), ZX_OK);
    BlockFifoRequest request = {
        .command = {.opcode = BLOCK_OPCODE_READ, .flags = 0},
        .vmoid = buffer.vmoid(),
        .length = kBlobfsBlockSize / kBlockSize,
        .dev_offset = block * kBlobfsBlockSize / kBlockSize,
    };
    ASSERT_EQ(device->FifoTransaction(&request, 1), ZX_OK);

    // Flip a byte.
    uint8_t* target = static_cast<uint8_t*>(buffer.Data(0));
    *target ^= 0xff;

    // Write the block back.
    request.command = {.opcode = BLOCK_OPCODE_WRITE, .flags = 0};
    ASSERT_EQ(device->FifoTransaction(&request, 1), ZX_OK);

    // Remount.
    EXPECT_EQ(ZX_OK, setup_.Mount(std::move(device)));
  }

  BlobfsTestSetupWithThread setup_;
};

TEST_F(BlobfsVerifyHealthTest, EmptyFilesystemPassesChecks) {
  EXPECT_OK(setup_.blobfs()->VerifyHealth());
}

TEST_F(BlobfsVerifyHealthTest, PopulatedFilesystemPassesChecks) {
  // Since only open files are validated, open a bunch of valid files.
  std::vector<fbl::RefPtr<Blob>> files;
  for (uint8_t i = 0; i < 10; ++i) {
    auto delivery_blob = TestDeliveryBlob::CreateUncompressed(65536, i);
    files.push_back(InstallBlob(delivery_blob));
  }

  EXPECT_OK(setup_.blobfs()->VerifyHealth());
}

TEST_F(BlobfsVerifyHealthTest, NullBlobPassesChecks) {
  auto delivery_blob = TestDeliveryBlob::CreateUncompressed(0);
  auto blob = InstallBlob(delivery_blob);

  EXPECT_OK(setup_.blobfs()->VerifyHealth());
}

TEST_F(BlobfsVerifyHealthTest, InvalidFileFailsChecks) {
  auto delivery_blob = TestDeliveryBlob::CreateUncompressed(65536);
  InstallBlob(delivery_blob);
  CorruptBlob(delivery_blob.digest());

  auto blob = GetBlob(*setup_.blobfs(), delivery_blob.digest());
  ASSERT_OK(blob);

  EXPECT_STATUS(setup_.blobfs()->VerifyHealth(), ZX_ERR_IO_DATA_INTEGRITY);
}

TEST_F(BlobfsVerifyHealthTest, InvalidButClosedFilePassesChecks) {
  auto delivery_blob = TestDeliveryBlob::CreateUncompressed(65536);
  InstallBlob(delivery_blob);
  CorruptBlob(delivery_blob.digest());

  EXPECT_OK(setup_.blobfs()->VerifyHealth());
}

}  // namespace
}  // namespace blobfs
