// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <gtest/gtest.h>

#include "src/media/drivers/amlogic_decoder/vp9_utils.h"

namespace amlogic_decoder {
namespace test {
namespace {

TEST(Vp9UtilsTest, TryParseSuperframeHeaderNormal) {
  // VP9 superframe has the index at the end of the packet.
  // Superframe header format:
  // 1 Byte header: | 1 | 1 | 0 | bytes_per_framesize - 1 (2 bits) | superframe_count - 1 (3 bits) |
  // Let's say we have 2 frames, each size 10.
  // bytes_per_framesize = 1 (bits: 00)
  // superframe_count = 2 (bits: 001)
  // header byte = 0b11000001 = 0xc1
  // Layout of index:
  // [0] = 10 (size of frame 0)
  // [1] = 10 (size of frame 1)
  // [2] = 0xc1
  //
  // Code checks:
  // if (data[frame_size - superframe_index_size] != superframe_header)
  // so data[frame_size - 4] must be 0xc1.
  // Therefore the index format is:
  // [frame_size - 4] = 0xc1
  // [frame_size - 3] = 10
  // [frame_size - 2] = 10
  // [frame_size - 1] = 0xc1
  // Total size = frame 0 size + frame 1 size = 20. The superframe itself must contain the frames.
  // Frame 0: 10 bytes.
  // Frame 1: 10 bytes.
  // Index: 4 bytes.
  // Total packet size: 24 bytes.
  std::vector<uint8_t> packet(24, 0);
  packet[20] = 0xc1;
  packet[21] = 10;
  packet[22] = 10;
  packet[23] = 0xc1;

  auto frame_sizes = TryParseSuperframeHeader(packet.data(), static_cast<uint32_t>(packet.size()));
  ASSERT_EQ(2u, frame_sizes.size());
  EXPECT_EQ(10u, frame_sizes[0]);
  EXPECT_EQ(10u, frame_sizes[1]);
}

TEST(Vp9UtilsTest, TryParseSuperframeHeaderOverflow) {
  // Let's construct a packet that triggers a uint32_t overflow condition.
  // bytes_per_framesize = 4
  // superframe_count = 2
  // header byte: 0b11011001 = 0xd9
  //
  // Let's create a packet of size 100.
  // Index format:
  // [100 - 10] = 0xd9
  // [100 - 9 .. 100 - 6] = size 0 (uint32_t)
  // [100 - 5 .. 100 - 2] = size 1 (uint32_t)
  // [100 - 1] = 0xd9
  //
  // Let's make:
  // size 0 = 50
  // size 1 = 4294967256 (which is 2^32 - 40)
  // When parsed, size 0 + size 1 = 50 + 2^32 - 40 = 2^32 + 10, which overflows to 10.
  // 10 <= 100 (frame_size), so it would pass the check if overflow happens.
  // If we fixed the uint32_t overflow, this check should fail and return empty.
  //
  // This test was confirmed to fail without the fix and pass with the fix.

  std::vector<uint8_t> packet(100, 0);
  packet[90] = 0xd9;

  // Set size 0 to 50 (uint32_t, little endian)
  uint32_t size0 = 50;
  memcpy(&packet[91], &size0, 4);

  // Set size 1 to 2^32 - 40 = 4294967256 (uint32_t, little endian)
  uint32_t size1 = 4294967256u;
  memcpy(&packet[95], &size1, 4);

  packet[99] = 0xd9;

  auto frame_sizes = TryParseSuperframeHeader(packet.data(), static_cast<uint32_t>(packet.size()));
  EXPECT_TRUE(frame_sizes.empty());
}

}  // namespace
}  // namespace test
}  // namespace amlogic_decoder
