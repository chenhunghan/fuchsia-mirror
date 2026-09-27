// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/zx/vmo.h>
#include <zircon/syscalls.h>

#include <gtest/gtest.h>

#include "lib/media/codec_impl/codec_buffer.h"
#include "lib/media/codec_impl/codec_vmo_range.h"

class CodecBufferForTest : public CodecBuffer {
 public:
  CodecBufferForTest(CodecBuffer::Info buffer_info, CodecVmoRange vmo_range)
      : CodecBuffer(nullptr, buffer_info, std::move(vmo_range)) {}

  using CodecBuffer::Info;
  using CodecBuffer::Map;
};

namespace {

CodecBufferForTest::Info MakeDefaultBufferInfo(CodecPort port = kOutputPort) {
  return CodecBufferForTest::Info{
      .port = port,
      .lifetime_ordinal = 1,
      .index = 0,
      .is_secure = false,
  };
}

TEST(CodecBufferTest, SizeVsRawVmoSize) {
  const size_t page_size = zx_system_get_page_size();
  const size_t content_size = page_size;
  const size_t raw_vmo_size = 4 * page_size;

  zx::vmo vmo;
  ASSERT_EQ(ZX_OK, zx::vmo::create(raw_vmo_size, 0, &vmo));

  CodecVmoRange vmo_range(std::move(vmo), 0, content_size);
  CodecBufferForTest buffer(MakeDefaultBufferInfo(), std::move(vmo_range));

  EXPECT_EQ(0u, buffer.vmo_offset());
  EXPECT_EQ(content_size, buffer.size());
  EXPECT_EQ(raw_vmo_size, buffer.raw_vmo_size());
}

TEST(CodecBufferTest, MapCoversTrailingPaddingPagesUpToRawVmoSize) {
  const size_t page_size = zx_system_get_page_size();
  const size_t content_size = page_size;
  const size_t raw_vmo_size = 4 * page_size;

  zx::vmo vmo;
  ASSERT_EQ(ZX_OK, zx::vmo::create(raw_vmo_size, 0, &vmo));

  zx::vmo dup_vmo;
  ASSERT_EQ(ZX_OK, vmo.duplicate(ZX_RIGHT_SAME_RIGHTS, &dup_vmo));

  {
    CodecVmoRange vmo_range(std::move(vmo), 0, content_size);
    CodecBufferForTest buffer(MakeDefaultBufferInfo(), std::move(vmo_range));

    ASSERT_TRUE(buffer.Map());
    ASSERT_NE(nullptr, buffer.base());
    EXPECT_EQ(content_size, buffer.size());
    EXPECT_EQ(raw_vmo_size, buffer.raw_vmo_size());

    // Write to the last byte of valid content and the last byte of trailing padding pages
    // (in page 3, beyond round_up(vmo_offset + size(), page_size)).
    constexpr uint8_t kContentLastByte = 0x5a;
    constexpr uint8_t kPaddingLastByte = 0xa5;
    buffer.base()[content_size - 1] = kContentLastByte;
    buffer.base()[raw_vmo_size - 1] = kPaddingLastByte;

    uint8_t read_content_byte = 0;
    uint8_t read_padding_byte = 0;
    ASSERT_EQ(ZX_OK, dup_vmo.read(&read_content_byte, content_size - 1, 1));
    ASSERT_EQ(ZX_OK, dup_vmo.read(&read_padding_byte, raw_vmo_size - 1, 1));
    EXPECT_EQ(kContentLastByte, read_content_byte);
    EXPECT_EQ(kPaddingLastByte, read_padding_byte);
  }
}

TEST(CodecBufferTest, MapWithNonZeroVmoOffsetAndTrailingPadding) {
  const size_t page_size = zx_system_get_page_size();
  constexpr uint64_t kVmoOffset = 128;
  const size_t content_size = page_size;
  const size_t raw_vmo_size = 4 * page_size;

  zx::vmo vmo;
  ASSERT_EQ(ZX_OK, zx::vmo::create(raw_vmo_size, 0, &vmo));

  zx::vmo dup_vmo;
  ASSERT_EQ(ZX_OK, vmo.duplicate(ZX_RIGHT_SAME_RIGHTS, &dup_vmo));

  {
    CodecVmoRange vmo_range(std::move(vmo), kVmoOffset, content_size);
    CodecBufferForTest buffer(MakeDefaultBufferInfo(), std::move(vmo_range));

    ASSERT_TRUE(buffer.Map());
    ASSERT_NE(nullptr, buffer.base());
    EXPECT_EQ(kVmoOffset, buffer.vmo_offset());
    EXPECT_EQ(content_size, buffer.size());
    EXPECT_EQ(raw_vmo_size, buffer.raw_vmo_size());
    EXPECT_EQ(kVmoOffset, reinterpret_cast<uintptr_t>(buffer.base()) % page_size);

    constexpr uint8_t kFirstByte = 0x11;
    constexpr uint8_t kLastVmoByte = 0x22;
    buffer.base()[0] = kFirstByte;
    buffer.base()[raw_vmo_size - kVmoOffset - 1] = kLastVmoByte;

    uint8_t read_first_byte = 0;
    uint8_t read_last_vmo_byte = 0;
    ASSERT_EQ(ZX_OK, dup_vmo.read(&read_first_byte, kVmoOffset, 1));
    ASSERT_EQ(ZX_OK, dup_vmo.read(&read_last_vmo_byte, raw_vmo_size - 1, 1));
    EXPECT_EQ(kFirstByte, read_first_byte);
    EXPECT_EQ(kLastVmoByte, read_last_vmo_byte);
  }
}

}  // namespace
