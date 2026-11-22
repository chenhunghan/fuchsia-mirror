// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/display/util.h"

#include <fidl/fuchsia.images2/cpp/wire.h>

#include <gtest/gtest.h>

namespace scenic_impl::display::test {

TEST(DisplayUtilTest, BufferCollectionPixelFormatToImageTilingType) {
  EXPECT_EQ(::display::BufferCollectionPixelFormatToImageTilingType(
                fuchsia_images2::PixelFormatModifier::kIntelI915XTiled),
            1u);

  EXPECT_EQ(::display::BufferCollectionPixelFormatToImageTilingType(
                fuchsia_images2::PixelFormatModifier::kIntelI915YTiled),
            2u);

  EXPECT_EQ(::display::BufferCollectionPixelFormatToImageTilingType(
                fuchsia_images2::PixelFormatModifier::kIntelI915YfTiled),
            3u);

  EXPECT_EQ(::display::BufferCollectionPixelFormatToImageTilingType(
                fuchsia_images2::PixelFormatModifier::kLinear),
            fuchsia_hardware_display_types::wire::kImageTilingTypeLinear);

  EXPECT_EQ(::display::BufferCollectionPixelFormatToImageTilingType(
                fuchsia_images2::PixelFormatModifier::kArmAfbc16X16),
            fuchsia_hardware_display_types::wire::kImageTilingTypeLinear);
}

}  // namespace scenic_impl::display::test
