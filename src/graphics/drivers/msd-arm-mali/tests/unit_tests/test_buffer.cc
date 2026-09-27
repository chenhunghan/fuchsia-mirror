// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <gtest/gtest.h>

#include "src/graphics/drivers/msd-arm-mali/src/msd_arm_buffer.h"

class TestMsdArmBuffer {
 public:
  static void TestFlush() {
    auto buffer = MsdArmBuffer::Create(1024, "test-buffer");
    ASSERT_NE(nullptr, buffer);
    EXPECT_TRUE(buffer->EnsureRegionFlushed(100, 200));
    EXPECT_EQ(100u, buffer->flushed_region_.start());
    EXPECT_EQ(200u, buffer->flushed_region_.end());
    EXPECT_TRUE(buffer->EnsureRegionFlushed(0, 300));
    EXPECT_EQ(0u, buffer->flushed_region_.start());
    EXPECT_EQ(300u, buffer->flushed_region_.end());
    EXPECT_TRUE(buffer->EnsureRegionFlushed(0, 0));
    EXPECT_EQ(0u, buffer->flushed_region_.start());
    EXPECT_EQ(300u, buffer->flushed_region_.end());
  }

  static void TestDecommit() {
    const size_t page_size = zx_system_get_page_size();
    auto buffer = MsdArmBuffer::Create(page_size * 4, "test-buffer");
    ASSERT_NE(nullptr, buffer);

    // Commit all pages.
    EXPECT_TRUE(buffer->CommitPageRange(0, 4));
    EXPECT_EQ(0u, buffer->committed_region_.start());
    EXPECT_EQ(4u, buffer->committed_region_.end());

    // Flush page 2 and 3.
    EXPECT_TRUE(buffer->EnsureRegionFlushed(page_size * 2, page_size * 4));
    EXPECT_EQ(page_size * 2, buffer->flushed_region_.start());
    EXPECT_EQ(page_size * 4, buffer->flushed_region_.end());

    // Decommit page 3.
    EXPECT_TRUE(buffer->DecommitPageRange(3, 1));
    EXPECT_EQ(0u, buffer->committed_region_.start());
    EXPECT_EQ(3u, buffer->committed_region_.end());
    EXPECT_EQ(page_size * 2, buffer->flushed_region_.start());
    EXPECT_EQ(page_size * 3, buffer->flushed_region_.end());

    // Decommit page 0 and 1.
    EXPECT_TRUE(buffer->DecommitPageRange(0, 2));
    EXPECT_EQ(2u, buffer->committed_region_.start());
    EXPECT_EQ(3u, buffer->committed_region_.end());
    EXPECT_EQ(page_size * 2, buffer->flushed_region_.start());
    EXPECT_EQ(page_size * 3, buffer->flushed_region_.end());

    // Decommit page 2.
    EXPECT_TRUE(buffer->DecommitPageRange(2, 1));
    EXPECT_TRUE(buffer->committed_region_.empty());
    EXPECT_TRUE(buffer->flushed_region_.empty());
  }
};

TEST(MsdArmBuffer, Flush) {
  TestMsdArmBuffer test;
  test.TestFlush();
}

TEST(MsdArmBuffer, Decommit) {
  TestMsdArmBuffer test;
  test.TestDecommit();
}
