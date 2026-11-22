// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <gtest/gtest.h>

#include "src/ui/lib/escher/util/image_utils.h"

namespace escher {
namespace {

TEST(GetImageByteSizeTest, TightlyPackedSizes) {
  EXPECT_EQ(image_utils::GetImageByteSize(vk::Format::eR8G8B8A8Unorm, 3, 2), 24U);
  EXPECT_EQ(image_utils::GetImageByteSize(vk::Format::eR8Unorm, 3, 1), 3U);
  EXPECT_EQ(image_utils::GetImageByteSize(vk::Format::eG8B8G8R8422Unorm, 2, 1), 4U);
  // 4:2:0 chroma rounds odd dimensions up: a 9-byte Y plane plus a 2x2 interleaved CbCr plane.
  EXPECT_EQ(image_utils::GetImageByteSize(vk::Format::eG8B8R82Plane420Unorm, 3, 3), 17U);
}

}  // namespace
}  // namespace escher
