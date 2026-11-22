// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/image-format/image_format.h>
#include <lib/sysmem-version/sysmem-version.h>
#include <lib/zbi-format/graphics.h>

#include <string>

#include <zxtest/zxtest.h>

namespace sysmem_v1 = fuchsia_sysmem;
namespace sysmem_v2 = fuchsia_sysmem2;

TEST(ImageFormat, LinearComparison_V2) {
  PixelFormatAndModifier plain;
  plain.pixel_format = fuchsia_images2::PixelFormat::kB8G8R8A8;

  PixelFormatAndModifier linear;
  linear.pixel_format = fuchsia_images2::PixelFormat::kB8G8R8A8;
  linear.pixel_format_modifier = fuchsia_images2::PixelFormatModifier::kLinear;

  PixelFormatAndModifier x_tiled;
  x_tiled.pixel_format = fuchsia_images2::PixelFormat::kB8G8R8A8;
  x_tiled.pixel_format_modifier = fuchsia_images2::PixelFormatModifier::kIntelI915XTiled;

  EXPECT_TRUE(ImageFormatIsPixelFormatEqual(plain, plain));
  EXPECT_TRUE(ImageFormatIsPixelFormatEqual(linear, linear));

  EXPECT_TRUE(ImageFormatIsPixelFormatEqual(plain, linear));
  EXPECT_TRUE(ImageFormatIsPixelFormatEqual(linear, plain));

  EXPECT_FALSE(ImageFormatIsPixelFormatEqual(linear, x_tiled));
  EXPECT_FALSE(ImageFormatIsPixelFormatEqual(plain, x_tiled));
}

// This is fairly redundant with LinearComparison_V2 given that wire enums are the same underlying
// type as natural enums, but we keep this test in case that changes.
TEST(ImageFormat, LinearComparison_V2_wire) {
  fidl::Arena allocator;
  PixelFormatAndModifier plain;
  plain.pixel_format = fuchsia_images2::wire::PixelFormat::kB8G8R8A8;

  PixelFormatAndModifier linear;
  linear.pixel_format = fuchsia_images2::PixelFormat::kB8G8R8A8;
  linear.pixel_format_modifier = fuchsia_images2::wire::PixelFormatModifier::kLinear;

  PixelFormatAndModifier x_tiled;
  x_tiled.pixel_format = fuchsia_images2::PixelFormat::kB8G8R8A8;
  x_tiled.pixel_format_modifier = fuchsia_images2::wire::PixelFormatModifier::kIntelI915XTiled;

  EXPECT_TRUE(ImageFormatIsPixelFormatEqual(plain, plain));
  EXPECT_TRUE(ImageFormatIsPixelFormatEqual(linear, linear));

  EXPECT_TRUE(ImageFormatIsPixelFormatEqual(plain, linear));
  EXPECT_TRUE(ImageFormatIsPixelFormatEqual(linear, plain));

  EXPECT_FALSE(ImageFormatIsPixelFormatEqual(linear, x_tiled));
  EXPECT_FALSE(ImageFormatIsPixelFormatEqual(plain, x_tiled));
}

TEST(ImageFormat, LinearComparison_V1_wire) {
  sysmem_v1::wire::PixelFormat plain = {
      .type = sysmem_v1::wire::PixelFormatType::kBgra32,
      .has_format_modifier = false,
  };

  sysmem_v1::wire::PixelFormat linear = {
      .type = sysmem_v1::wire::PixelFormatType::kBgra32,
      .has_format_modifier = true,
      .format_modifier =
          {
              .value = sysmem_v1::wire::kFormatModifierLinear,
          },
  };

  sysmem_v1::wire::PixelFormat x_tiled = {
      .type = sysmem_v1::wire::PixelFormatType::kBgra32,
      .has_format_modifier = true,
      .format_modifier =
          {
              .value = sysmem_v1::wire::kFormatModifierIntelI915XTiled,
          },
  };

  EXPECT_TRUE(ImageFormatIsPixelFormatEqual(plain, plain));
  EXPECT_TRUE(ImageFormatIsPixelFormatEqual(linear, linear));

  EXPECT_TRUE(ImageFormatIsPixelFormatEqual(plain, linear));
  EXPECT_TRUE(ImageFormatIsPixelFormatEqual(linear, plain));

  EXPECT_FALSE(ImageFormatIsPixelFormatEqual(linear, x_tiled));
  EXPECT_FALSE(ImageFormatIsPixelFormatEqual(plain, x_tiled));
}

TEST(ImageFormat, LinearRowBytes_V2) {
  sysmem_v2::ImageFormatConstraints constraints;
  constraints.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  constraints.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kLinear;
  constraints.min_size() = {12u, 1u};
  constraints.max_size() = {100u, 0xFFFFFFFF};
  constraints.bytes_per_row_divisor().emplace(4u * 8u);
  constraints.max_bytes_per_row().emplace(100000u);

  uint32_t row_bytes;
  EXPECT_TRUE(ImageFormatMinimumRowBytes(constraints, 17, &row_bytes));
  EXPECT_EQ(row_bytes, 4 * 24);

  // too narrow in pixels
  EXPECT_FALSE(ImageFormatMinimumRowBytes(constraints, 11, &row_bytes));
  // too wide in pixels
  EXPECT_FALSE(ImageFormatMinimumRowBytes(constraints, 101, &row_bytes));
}

#if FUCHSIA_API_LEVEL_AT_LEAST(32)
TEST(ImageFormat, LinearRowBytesChecked_V2) {
  sysmem_v2::ImageFormatConstraints constraints;
  constraints.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  constraints.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kLinear;
  constraints.min_size() = {12u, 1u};
  constraints.max_size() = {100u, 0xFFFFFFFF};
  constraints.bytes_per_row_divisor().emplace(4u * 8u);
  constraints.max_bytes_per_row().emplace(100000u);

  auto row_bytes_checked = ImageFormatMinimumRowBytesChecked(constraints, 17);
  EXPECT_TRUE(row_bytes_checked.IsValid());
  EXPECT_EQ(row_bytes_checked.ValueOrDie(), 4 * 24);

  // too narrow in pixels
  EXPECT_FALSE(ImageFormatMinimumRowBytesChecked(constraints, 11).IsValid());
  // too wide in pixels
  EXPECT_FALSE(ImageFormatMinimumRowBytesChecked(constraints, 101).IsValid());
}
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)

TEST(ImageFormat, LinearRowBytes_V2_wire) {
  fidl::Arena allocator;
  auto constraints = sysmem_v2::wire::ImageFormatConstraints::Builder(allocator)
                         .pixel_format(fuchsia_images2::wire::PixelFormat::kB8G8R8A8)
                         .pixel_format_modifier(fuchsia_images2::wire::PixelFormatModifier::kLinear)
                         .min_size(fuchsia_math::wire::SizeU{12u, 1u})
                         .max_size(fuchsia_math::wire::SizeU{100u, 0xFFFFFFFF})
                         .bytes_per_row_divisor(4u * 8u)
                         .max_bytes_per_row(100000u)
                         .Build();

  uint32_t row_bytes;
  EXPECT_TRUE(ImageFormatMinimumRowBytes(constraints, 17, &row_bytes));
  EXPECT_EQ(row_bytes, 4 * 24);

  // too narrow in pixels
  EXPECT_FALSE(ImageFormatMinimumRowBytes(constraints, 11, &row_bytes));
  // too wide in pixels
  EXPECT_FALSE(ImageFormatMinimumRowBytes(constraints, 101, &row_bytes));
}

#if FUCHSIA_API_LEVEL_AT_LEAST(32)
TEST(ImageFormat, LinearRowBytesChecked_V2_wire) {
  fidl::Arena allocator;
  auto constraints = sysmem_v2::wire::ImageFormatConstraints::Builder(allocator)
                         .pixel_format(fuchsia_images2::wire::PixelFormat::kB8G8R8A8)
                         .pixel_format_modifier(fuchsia_images2::wire::PixelFormatModifier::kLinear)
                         .min_size(fuchsia_math::wire::SizeU{12u, 1u})
                         .max_size(fuchsia_math::wire::SizeU{100u, 0xFFFFFFFF})
                         .bytes_per_row_divisor(4u * 8u)
                         .max_bytes_per_row(100000u)
                         .Build();

  auto row_bytes_checked = ImageFormatMinimumRowBytesChecked(constraints, 17);
  EXPECT_TRUE(row_bytes_checked.IsValid());
  EXPECT_EQ(row_bytes_checked.ValueOrDie(), 4 * 24);

  // too narrow in pixels
  EXPECT_FALSE(ImageFormatMinimumRowBytesChecked(constraints, 11).IsValid());
  // too wide in pixels
  EXPECT_FALSE(ImageFormatMinimumRowBytesChecked(constraints, 101).IsValid());
}
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)

TEST(ImageFormat, LinearRowBytes_V1_wire) {
  sysmem_v1::wire::PixelFormat linear = {
      .type = sysmem_v1::wire::PixelFormatType::kBgra32,
      .has_format_modifier = true,
      .format_modifier =
          {
              .value = sysmem_v1::wire::kFormatModifierLinear,
          },
  };
  sysmem_v1::wire::ImageFormatConstraints constraints = {
      .pixel_format = linear,
      .min_coded_width = 12,
      .max_coded_width = 100,
      .max_bytes_per_row = 100000,
      .bytes_per_row_divisor = 4 * 8,
  };

  uint32_t row_bytes;
  EXPECT_TRUE(ImageFormatMinimumRowBytes(constraints, 17, &row_bytes));
  EXPECT_EQ(row_bytes, 4 * 24);

  EXPECT_FALSE(ImageFormatMinimumRowBytes(constraints, 11, &row_bytes));
  EXPECT_FALSE(ImageFormatMinimumRowBytes(constraints, 101, &row_bytes));
}

#if FUCHSIA_API_LEVEL_AT_LEAST(32)
TEST(ImageFormat, LinearRowBytesChecked_V1_wire) {
  sysmem_v1::wire::PixelFormat linear = {
      .type = sysmem_v1::wire::PixelFormatType::kBgra32,
      .has_format_modifier = true,
      .format_modifier =
          {
              .value = sysmem_v1::wire::kFormatModifierLinear,
          },
  };
  sysmem_v1::wire::ImageFormatConstraints constraints = {
      .pixel_format = linear,
      .min_coded_width = 12,
      .max_coded_width = 100,
      .max_bytes_per_row = 100000,
      .bytes_per_row_divisor = 4 * 8,
  };

  auto row_bytes_checked = ImageFormatMinimumRowBytesChecked(constraints, 17);
  EXPECT_TRUE(row_bytes_checked.IsValid());
  EXPECT_EQ(row_bytes_checked.ValueOrDie(), 4 * 24);

  EXPECT_FALSE(ImageFormatMinimumRowBytesChecked(constraints, 11).IsValid());
  EXPECT_FALSE(ImageFormatMinimumRowBytesChecked(constraints, 101).IsValid());
}
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)

TEST(ImageFormat, InvalidColorSpace_V1_wire) {
  fuchsia_sysmem::wire::PixelFormat kPixelFormat = {
      .type = fuchsia_sysmem::PixelFormatType::kRgb565,
      .has_format_modifier = true,
      .format_modifier = {
          .value = fuchsia_sysmem::wire::kFormatModifierLinear,
      }};

  sysmem_v1::wire::ColorSpace color_space{sysmem_v1::wire::ColorSpaceType::kInvalid};
  // Shouldn't crash.
  EXPECT_FALSE(ImageFormatIsSupportedColorSpaceForPixelFormat(color_space, kPixelFormat));
}

TEST(ImageFormat, PassThroughColorSpace_V1_wire) {
  fidl::Arena allocator;
  fuchsia_sysmem::wire::PixelFormat linear_bgra = {
      .type = fuchsia_sysmem::wire::PixelFormatType::kBgra32,
      .has_format_modifier = true,
      .format_modifier =
          {
              .value = fuchsia_sysmem::kFormatModifierLinear,
          },
  };

  sysmem_v1::wire::ColorSpace color_space{sysmem_v1::wire::ColorSpaceType::kPassThrough};
  EXPECT_TRUE(ImageFormatIsSupportedColorSpaceForPixelFormat(color_space, linear_bgra));

  fuchsia_sysmem::wire::PixelFormat linear_nv12 = {
      .type = fuchsia_sysmem::wire::PixelFormatType::kNv12,
      .has_format_modifier = true,
      .format_modifier =
          {
              .value = fuchsia_sysmem::kFormatModifierLinear,
          },
  };

  EXPECT_TRUE(ImageFormatIsSupportedColorSpaceForPixelFormat(color_space, linear_nv12));
}

TEST(ImageFormat, L8AndY8PermitRgbAndYuv) {
  fuchsia_images2::ColorSpace color_space_rgb = fuchsia_images2::ColorSpace::kSrgb;
  fuchsia_images2::ColorSpace color_space_yuv = fuchsia_images2::ColorSpace::kRec709;

  auto pixel_format_L8 = PixelFormatAndModifier(fuchsia_images2::PixelFormat::kL8,
                                                fuchsia_images2::PixelFormatModifier::kLinear);
  EXPECT_TRUE(ImageFormatIsSupportedColorSpaceForPixelFormat(color_space_rgb, pixel_format_L8));
  EXPECT_TRUE(ImageFormatIsSupportedColorSpaceForPixelFormat(color_space_yuv, pixel_format_L8));

  auto pixel_format_R8 = PixelFormatAndModifier(fuchsia_images2::PixelFormat::kR8,
                                                fuchsia_images2::PixelFormatModifier::kLinear);
  EXPECT_TRUE(ImageFormatIsSupportedColorSpaceForPixelFormat(color_space_rgb, pixel_format_R8));
  EXPECT_TRUE(ImageFormatIsSupportedColorSpaceForPixelFormat(color_space_yuv, pixel_format_R8));
}

TEST(ImageFormat, ZbiPixelFormatV2) {
  {
    fpromise::result<fuchsia_images2::wire::PixelFormat> convert_result =
        ImageFormatConvertZbiToSysmemPixelFormat_v2(ZBI_PIXEL_FORMAT_RGB_565);
    ASSERT_TRUE(convert_result.is_ok());
    EXPECT_EQ(convert_result.value(), fuchsia_images2::wire::PixelFormat::kR5G6B5);
  }
  {
    fpromise::result<fuchsia_images2::wire::PixelFormat> convert_result =
        ImageFormatConvertZbiToSysmemPixelFormat_v2(ZBI_PIXEL_FORMAT_RGB_332);
    ASSERT_TRUE(convert_result.is_ok());
    EXPECT_EQ(convert_result.value(), fuchsia_images2::wire::PixelFormat::kR3G3B2);
  }
  {
    fpromise::result<fuchsia_images2::wire::PixelFormat> convert_result =
        ImageFormatConvertZbiToSysmemPixelFormat_v2(ZBI_PIXEL_FORMAT_RGB_2220);
    ASSERT_TRUE(convert_result.is_ok());
    EXPECT_EQ(convert_result.value(), fuchsia_images2::wire::PixelFormat::kR2G2B2X2);
  }
  {
    fpromise::result<fuchsia_images2::wire::PixelFormat> convert_result =
        ImageFormatConvertZbiToSysmemPixelFormat_v2(ZBI_PIXEL_FORMAT_ARGB_8888);
    ASSERT_TRUE(convert_result.is_ok());
    EXPECT_EQ(convert_result.value(), fuchsia_images2::wire::PixelFormat::kB8G8R8A8);
  }
  {
    fpromise::result<fuchsia_images2::wire::PixelFormat> convert_result =
        ImageFormatConvertZbiToSysmemPixelFormat_v2(ZBI_PIXEL_FORMAT_RGB_X888);
    ASSERT_TRUE(convert_result.is_ok());
    EXPECT_EQ(convert_result.value(), fuchsia_images2::wire::PixelFormat::kB8G8R8A8);
  }
  {
    fpromise::result<fuchsia_images2::wire::PixelFormat> convert_result =
        ImageFormatConvertZbiToSysmemPixelFormat_v2(ZBI_PIXEL_FORMAT_MONO_8);
    ASSERT_TRUE(convert_result.is_ok());
    EXPECT_EQ(convert_result.value(), fuchsia_images2::wire::PixelFormat::kL8);
  }
  {
    fpromise::result<fuchsia_images2::wire::PixelFormat> convert_result =
        ImageFormatConvertZbiToSysmemPixelFormat_v2(ZBI_PIXEL_FORMAT_I420);
    ASSERT_TRUE(convert_result.is_ok());
    EXPECT_EQ(convert_result.value(), fuchsia_images2::wire::PixelFormat::kI420);
  }
  {
    fpromise::result<fuchsia_images2::wire::PixelFormat> convert_result =
        ImageFormatConvertZbiToSysmemPixelFormat_v2(ZBI_PIXEL_FORMAT_NV12);
    ASSERT_TRUE(convert_result.is_ok());
    EXPECT_EQ(convert_result.value(), fuchsia_images2::wire::PixelFormat::kNv12);
  }
  {
    fpromise::result<fuchsia_images2::wire::PixelFormat> convert_result =
        ImageFormatConvertZbiToSysmemPixelFormat_v2(ZBI_PIXEL_FORMAT_RGB_888);
    ASSERT_TRUE(convert_result.is_ok());
    EXPECT_EQ(convert_result.value(), fuchsia_images2::wire::PixelFormat::kB8G8R8);
  }
  {
    fpromise::result<fuchsia_images2::wire::PixelFormat> convert_result =
        ImageFormatConvertZbiToSysmemPixelFormat_v2(ZBI_PIXEL_FORMAT_ABGR_8888);
    ASSERT_TRUE(convert_result.is_ok());
    EXPECT_EQ(convert_result.value(), fuchsia_images2::wire::PixelFormat::kR8G8B8A8);
  }
  {
    fpromise::result<fuchsia_images2::wire::PixelFormat> convert_result =
        ImageFormatConvertZbiToSysmemPixelFormat_v2(ZBI_PIXEL_FORMAT_BGR_888_X);
    ASSERT_TRUE(convert_result.is_ok());
    EXPECT_EQ(convert_result.value(), fuchsia_images2::wire::PixelFormat::kR8G8B8A8);
  }
  {
    fpromise::result<fuchsia_images2::wire::PixelFormat> convert_result =
        ImageFormatConvertZbiToSysmemPixelFormat_v2(ZBI_PIXEL_FORMAT_ARGB_2_10_10_10);
    ASSERT_TRUE(convert_result.is_ok());
    EXPECT_EQ(convert_result.value(), fuchsia_images2::wire::PixelFormat::kA2R10G10B10);
  }
  {
    fpromise::result<fuchsia_images2::wire::PixelFormat> convert_result =
        ImageFormatConvertZbiToSysmemPixelFormat_v2(ZBI_PIXEL_FORMAT_ABGR_2_10_10_10);
    ASSERT_TRUE(convert_result.is_ok());
    EXPECT_EQ(convert_result.value(), fuchsia_images2::wire::PixelFormat::kA2B10G10R10);
  }
}

TEST(ImageFormat, PlaneByteOffset_V2) {
  sysmem_v2::ImageFormatConstraints constraints;
  constraints.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  constraints.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kLinear;
  constraints.min_size() = {12u, 12u};
  constraints.max_size() = {100u, 100u};
  constraints.bytes_per_row_divisor().emplace(4u * 8u);
  constraints.max_bytes_per_row().emplace(100000u);

  auto image_format_result = ImageConstraintsToFormat(constraints, 18, 17);
  EXPECT_TRUE(image_format_result.is_ok());
  auto image_format = image_format_result.take_value();
  // The raw size would be 72 without bytes_per_row_divisor of 32.
  EXPECT_EQ(96u, image_format.bytes_per_row());

  uint64_t byte_offset;
  EXPECT_TRUE(ImageFormatPlaneByteOffset(image_format, 0, &byte_offset));
  EXPECT_EQ(0u, byte_offset);
  EXPECT_FALSE(ImageFormatPlaneByteOffset(image_format, 1, &byte_offset));

  // clone via generated code
  auto constraints2 = constraints;
  constraints2.pixel_format() = fuchsia_images2::PixelFormat::kI420;

  constexpr uint32_t kBytesPerRow = 32;
  image_format_result = ImageConstraintsToFormat(constraints2, 18, 20);
  EXPECT_TRUE(image_format_result.is_ok());
  image_format = image_format_result.take_value();
  EXPECT_EQ(kBytesPerRow, image_format.bytes_per_row());
  EXPECT_TRUE(ImageFormatPlaneByteOffset(image_format, 0, &byte_offset));
  EXPECT_EQ(0u, byte_offset);
  EXPECT_TRUE(ImageFormatPlaneByteOffset(image_format, 1, &byte_offset));
  EXPECT_EQ(kBytesPerRow * 20, byte_offset);
  EXPECT_TRUE(ImageFormatPlaneByteOffset(image_format, 2, &byte_offset));
  EXPECT_EQ(kBytesPerRow * 20 + kBytesPerRow / 2 * 20 / 2, byte_offset);
  EXPECT_FALSE(ImageFormatPlaneByteOffset(image_format, 3, &byte_offset));

  uint32_t row_bytes;
  EXPECT_TRUE(ImageFormatPlaneRowBytes(image_format, 0, &row_bytes));
  EXPECT_EQ(kBytesPerRow, row_bytes);
  EXPECT_TRUE(ImageFormatPlaneRowBytes(image_format, 1, &row_bytes));
  EXPECT_EQ(kBytesPerRow / 2, row_bytes);
  EXPECT_TRUE(ImageFormatPlaneRowBytes(image_format, 2, &row_bytes));
  EXPECT_EQ(kBytesPerRow / 2, row_bytes);
  EXPECT_FALSE(ImageFormatPlaneRowBytes(image_format, 3, &row_bytes));
}

#if FUCHSIA_API_LEVEL_AT_LEAST(32)
TEST(ImageFormat, PlaneByteOffsetChecked_V2) {
  sysmem_v2::ImageFormatConstraints constraints;
  constraints.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  constraints.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kLinear;
  constraints.min_size() = {12u, 12u};
  constraints.max_size() = {100u, 100u};
  constraints.bytes_per_row_divisor().emplace(4u * 8u);
  constraints.max_bytes_per_row().emplace(100000u);

  auto image_format_result = ImageConstraintsToFormat(constraints, 18, 17);
  EXPECT_TRUE(image_format_result.is_ok());
  auto image_format = image_format_result.take_value();
  // The raw size would be 72 without bytes_per_row_divisor of 32.
  EXPECT_EQ(96u, image_format.bytes_per_row());

  auto byte_offset_checked = ImageFormatPlaneByteOffsetChecked(image_format, 0);
  EXPECT_TRUE(byte_offset_checked.IsValid());
  EXPECT_EQ(0u, byte_offset_checked.ValueOrDie());
  EXPECT_FALSE(ImageFormatPlaneByteOffsetChecked(image_format, 1).IsValid());

  // clone via generated code
  auto constraints2 = constraints;
  constraints2.pixel_format() = fuchsia_images2::PixelFormat::kI420;

  constexpr uint32_t kBytesPerRow = 32;
  image_format_result = ImageConstraintsToFormat(constraints2, 18, 20);
  EXPECT_TRUE(image_format_result.is_ok());
  image_format = image_format_result.take_value();
  EXPECT_EQ(kBytesPerRow, image_format.bytes_per_row());

  byte_offset_checked = ImageFormatPlaneByteOffsetChecked(image_format, 0);
  EXPECT_TRUE(byte_offset_checked.IsValid());
  EXPECT_EQ(0u, byte_offset_checked.ValueOrDie());

  byte_offset_checked = ImageFormatPlaneByteOffsetChecked(image_format, 1);
  EXPECT_TRUE(byte_offset_checked.IsValid());
  EXPECT_EQ(kBytesPerRow * 20, byte_offset_checked.ValueOrDie());

  byte_offset_checked = ImageFormatPlaneByteOffsetChecked(image_format, 2);
  EXPECT_TRUE(byte_offset_checked.IsValid());
  EXPECT_EQ(kBytesPerRow * 20 + kBytesPerRow / 2 * 20 / 2, byte_offset_checked.ValueOrDie());

  EXPECT_FALSE(ImageFormatPlaneByteOffsetChecked(image_format, 3).IsValid());

  auto row_bytes_checked = ImageFormatPlaneRowBytesChecked(image_format, 0);
  EXPECT_TRUE(row_bytes_checked.IsValid());
  EXPECT_EQ(kBytesPerRow, row_bytes_checked.ValueOrDie());

  row_bytes_checked = ImageFormatPlaneRowBytesChecked(image_format, 1);
  EXPECT_TRUE(row_bytes_checked.IsValid());
  EXPECT_EQ(kBytesPerRow / 2, row_bytes_checked.ValueOrDie());

  row_bytes_checked = ImageFormatPlaneRowBytesChecked(image_format, 2);
  EXPECT_TRUE(row_bytes_checked.IsValid());
  EXPECT_EQ(kBytesPerRow / 2, row_bytes_checked.ValueOrDie());

  EXPECT_FALSE(ImageFormatPlaneRowBytesChecked(image_format, 3).IsValid());
}
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)

TEST(ImageFormat, PlaneByteOffset_V2_wire) {
  fidl::Arena allocator;
  auto constraints = sysmem_v2::wire::ImageFormatConstraints::Builder(allocator)
                         .pixel_format(fuchsia_images2::wire::PixelFormat::kB8G8R8A8)
                         .pixel_format_modifier(fuchsia_images2::wire::PixelFormatModifier::kLinear)
                         .min_size(fuchsia_math::wire::SizeU{12u, 12u})
                         .max_size(fuchsia_math::wire::SizeU{100u, 100u})
                         .bytes_per_row_divisor(4u * 8u)
                         .max_bytes_per_row(100000u)
                         .Build();

  auto image_format_result = ImageConstraintsToFormat(allocator, constraints, 18, 17);
  EXPECT_TRUE(image_format_result.is_ok());
  auto image_format = image_format_result.take_value();
  // The raw size would be 72 without bytes_per_row_divisor of 32.
  EXPECT_EQ(96u, image_format.bytes_per_row());

  uint64_t byte_offset;
  EXPECT_TRUE(ImageFormatPlaneByteOffset(image_format, 0, &byte_offset));
  EXPECT_EQ(0u, byte_offset);
  EXPECT_FALSE(ImageFormatPlaneByteOffset(image_format, 1, &byte_offset));

  auto constraints2 = sysmem::V2CloneImageFormatConstraints(allocator, constraints);
  constraints2.pixel_format() = fuchsia_images2::PixelFormat::kI420;

  constexpr uint32_t kBytesPerRow = 32;
  image_format_result = ImageConstraintsToFormat(allocator, constraints2, 18, 20);
  EXPECT_TRUE(image_format_result.is_ok());
  image_format = image_format_result.take_value();
  EXPECT_EQ(kBytesPerRow, image_format.bytes_per_row());
  EXPECT_TRUE(ImageFormatPlaneByteOffset(image_format, 0, &byte_offset));
  EXPECT_EQ(0u, byte_offset);
  EXPECT_TRUE(ImageFormatPlaneByteOffset(image_format, 1, &byte_offset));
  EXPECT_EQ(kBytesPerRow * 20, byte_offset);
  EXPECT_TRUE(ImageFormatPlaneByteOffset(image_format, 2, &byte_offset));
  EXPECT_EQ(kBytesPerRow * 20 + kBytesPerRow / 2 * 20 / 2, byte_offset);
  EXPECT_FALSE(ImageFormatPlaneByteOffset(image_format, 3, &byte_offset));

  uint32_t row_bytes;
  EXPECT_TRUE(ImageFormatPlaneRowBytes(image_format, 0, &row_bytes));
  EXPECT_EQ(kBytesPerRow, row_bytes);
  EXPECT_TRUE(ImageFormatPlaneRowBytes(image_format, 1, &row_bytes));
  EXPECT_EQ(kBytesPerRow / 2, row_bytes);
  EXPECT_TRUE(ImageFormatPlaneRowBytes(image_format, 2, &row_bytes));
  EXPECT_EQ(kBytesPerRow / 2, row_bytes);
  EXPECT_FALSE(ImageFormatPlaneRowBytes(image_format, 3, &row_bytes));
}

#if FUCHSIA_API_LEVEL_AT_LEAST(32)
TEST(ImageFormat, PlaneByteOffsetChecked_V2_wire) {
  fidl::Arena allocator;
  auto constraints = sysmem_v2::wire::ImageFormatConstraints::Builder(allocator)
                         .pixel_format(fuchsia_images2::wire::PixelFormat::kB8G8R8A8)
                         .pixel_format_modifier(fuchsia_images2::wire::PixelFormatModifier::kLinear)
                         .min_size(fuchsia_math::wire::SizeU{12u, 12u})
                         .max_size(fuchsia_math::wire::SizeU{100u, 100u})
                         .bytes_per_row_divisor(4u * 8u)
                         .max_bytes_per_row(100000u)
                         .Build();

  auto image_format_result = ImageConstraintsToFormat(allocator, constraints, 18, 17);
  EXPECT_TRUE(image_format_result.is_ok());
  auto image_format = image_format_result.take_value();
  // The raw size would be 72 without bytes_per_row_divisor of 32.
  EXPECT_EQ(96u, image_format.bytes_per_row());

  auto byte_offset_checked = ImageFormatPlaneByteOffsetChecked(image_format, 0);
  EXPECT_TRUE(byte_offset_checked.IsValid());
  EXPECT_EQ(0u, byte_offset_checked.ValueOrDie());
  EXPECT_FALSE(ImageFormatPlaneByteOffsetChecked(image_format, 1).IsValid());

  auto constraints2 = sysmem::V2CloneImageFormatConstraints(allocator, constraints);
  constraints2.pixel_format() = fuchsia_images2::PixelFormat::kI420;

  constexpr uint32_t kBytesPerRow = 32;
  image_format_result = ImageConstraintsToFormat(allocator, constraints2, 18, 20);
  EXPECT_TRUE(image_format_result.is_ok());
  image_format = image_format_result.take_value();
  EXPECT_EQ(kBytesPerRow, image_format.bytes_per_row());

  byte_offset_checked = ImageFormatPlaneByteOffsetChecked(image_format, 0);
  EXPECT_TRUE(byte_offset_checked.IsValid());
  EXPECT_EQ(0u, byte_offset_checked.ValueOrDie());

  byte_offset_checked = ImageFormatPlaneByteOffsetChecked(image_format, 1);
  EXPECT_TRUE(byte_offset_checked.IsValid());
  EXPECT_EQ(kBytesPerRow * 20, byte_offset_checked.ValueOrDie());

  byte_offset_checked = ImageFormatPlaneByteOffsetChecked(image_format, 2);
  EXPECT_TRUE(byte_offset_checked.IsValid());
  EXPECT_EQ(kBytesPerRow * 20 + kBytesPerRow / 2 * 20 / 2, byte_offset_checked.ValueOrDie());

  EXPECT_FALSE(ImageFormatPlaneByteOffsetChecked(image_format, 3).IsValid());

  auto row_bytes_checked = ImageFormatPlaneRowBytesChecked(image_format, 0);
  EXPECT_TRUE(row_bytes_checked.IsValid());
  EXPECT_EQ(kBytesPerRow, row_bytes_checked.ValueOrDie());

  row_bytes_checked = ImageFormatPlaneRowBytesChecked(image_format, 1);
  EXPECT_TRUE(row_bytes_checked.IsValid());
  EXPECT_EQ(kBytesPerRow / 2, row_bytes_checked.ValueOrDie());

  row_bytes_checked = ImageFormatPlaneRowBytesChecked(image_format, 2);
  EXPECT_TRUE(row_bytes_checked.IsValid());
  EXPECT_EQ(kBytesPerRow / 2, row_bytes_checked.ValueOrDie());

  EXPECT_FALSE(ImageFormatPlaneRowBytesChecked(image_format, 3).IsValid());
}
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)

TEST(ImageFormat, PlaneByteOffset_V1_wire) {
  sysmem_v1::wire::PixelFormat linear = {
      .type = sysmem_v1::wire::PixelFormatType::kBgra32,
      .has_format_modifier = true,
      .format_modifier =
          {
              .value = sysmem_v1::wire::kFormatModifierLinear,
          },
  };
  sysmem_v1::wire::ImageFormatConstraints constraints = {
      .pixel_format = linear,
      .min_coded_width = 12,
      .max_coded_width = 100,
      .min_coded_height = 12,
      .max_coded_height = 100,
      .max_bytes_per_row = 100000,
      .bytes_per_row_divisor = 4 * 8,
  };

  auto image_format_result = ImageConstraintsToFormat(constraints, 18, 17);
  EXPECT_TRUE(image_format_result.is_ok());
  auto image_format = image_format_result.take_value();
  // The raw size would be 72 without bytes_per_row_divisor of 32.
  EXPECT_EQ(96u, image_format.bytes_per_row);

  uint64_t byte_offset;
  EXPECT_TRUE(ImageFormatPlaneByteOffset(image_format, 0, &byte_offset));
  EXPECT_EQ(0u, byte_offset);
  EXPECT_FALSE(ImageFormatPlaneByteOffset(image_format, 1, &byte_offset));

  constraints.pixel_format.type = sysmem_v1::wire::PixelFormatType::kI420;

  constexpr uint32_t kBytesPerRow = 32;
  image_format_result = ImageConstraintsToFormat(constraints, 18, 20);
  EXPECT_TRUE(image_format_result.is_ok());
  image_format = image_format_result.take_value();
  EXPECT_EQ(kBytesPerRow, image_format.bytes_per_row);
  EXPECT_TRUE(ImageFormatPlaneByteOffset(image_format, 0, &byte_offset));
  EXPECT_EQ(0u, byte_offset);
  EXPECT_TRUE(ImageFormatPlaneByteOffset(image_format, 1, &byte_offset));
  EXPECT_EQ(kBytesPerRow * 20, byte_offset);
  EXPECT_TRUE(ImageFormatPlaneByteOffset(image_format, 2, &byte_offset));
  EXPECT_EQ(kBytesPerRow * 20 + kBytesPerRow / 2 * 20 / 2, byte_offset);
  EXPECT_FALSE(ImageFormatPlaneByteOffset(image_format, 3, &byte_offset));

  uint32_t row_bytes;
  EXPECT_TRUE(ImageFormatPlaneRowBytes(image_format, 0, &row_bytes));
  EXPECT_EQ(kBytesPerRow, row_bytes);
  EXPECT_TRUE(ImageFormatPlaneRowBytes(image_format, 1, &row_bytes));
  EXPECT_EQ(kBytesPerRow / 2, row_bytes);
  EXPECT_TRUE(ImageFormatPlaneRowBytes(image_format, 2, &row_bytes));
  EXPECT_EQ(kBytesPerRow / 2, row_bytes);
  EXPECT_FALSE(ImageFormatPlaneRowBytes(image_format, 3, &row_bytes));
}

#if FUCHSIA_API_LEVEL_AT_LEAST(32)
TEST(ImageFormat, PlaneByteOffsetChecked_V1_wire) {
  sysmem_v1::wire::PixelFormat linear = {
      .type = sysmem_v1::wire::PixelFormatType::kBgra32,
      .has_format_modifier = true,
      .format_modifier =
          {
              .value = sysmem_v1::wire::kFormatModifierLinear,
          },
  };
  sysmem_v1::wire::ImageFormatConstraints constraints = {
      .pixel_format = linear,
      .min_coded_width = 12,
      .max_coded_width = 100,
      .min_coded_height = 12,
      .max_coded_height = 100,
      .max_bytes_per_row = 100000,
      .bytes_per_row_divisor = 4 * 8,
  };

  auto image_format_result = ImageConstraintsToFormat(constraints, 18, 17);
  EXPECT_TRUE(image_format_result.is_ok());
  auto image_format = image_format_result.take_value();
  // The raw size would be 72 without bytes_per_row_divisor of 32.
  EXPECT_EQ(96u, image_format.bytes_per_row);

  auto byte_offset_checked = ImageFormatPlaneByteOffsetChecked(image_format, 0);
  EXPECT_TRUE(byte_offset_checked.IsValid());
  EXPECT_EQ(0u, byte_offset_checked.ValueOrDie());
  EXPECT_FALSE(ImageFormatPlaneByteOffsetChecked(image_format, 1).IsValid());

  constraints.pixel_format.type = sysmem_v1::wire::PixelFormatType::kI420;

  constexpr uint32_t kBytesPerRow = 32;
  image_format_result = ImageConstraintsToFormat(constraints, 18, 20);
  EXPECT_TRUE(image_format_result.is_ok());
  image_format = image_format_result.take_value();
  EXPECT_EQ(kBytesPerRow, image_format.bytes_per_row);

  byte_offset_checked = ImageFormatPlaneByteOffsetChecked(image_format, 0);
  EXPECT_TRUE(byte_offset_checked.IsValid());
  EXPECT_EQ(0u, byte_offset_checked.ValueOrDie());

  byte_offset_checked = ImageFormatPlaneByteOffsetChecked(image_format, 1);
  EXPECT_TRUE(byte_offset_checked.IsValid());
  EXPECT_EQ(kBytesPerRow * 20, byte_offset_checked.ValueOrDie());

  byte_offset_checked = ImageFormatPlaneByteOffsetChecked(image_format, 2);
  EXPECT_TRUE(byte_offset_checked.IsValid());
  EXPECT_EQ(kBytesPerRow * 20 + kBytesPerRow / 2 * 20 / 2, byte_offset_checked.ValueOrDie());

  EXPECT_FALSE(ImageFormatPlaneByteOffsetChecked(image_format, 3).IsValid());

  auto row_bytes_checked = ImageFormatPlaneRowBytesChecked(image_format, 0);
  EXPECT_TRUE(row_bytes_checked.IsValid());
  EXPECT_EQ(kBytesPerRow, row_bytes_checked.ValueOrDie());

  row_bytes_checked = ImageFormatPlaneRowBytesChecked(image_format, 1);
  EXPECT_TRUE(row_bytes_checked.IsValid());
  EXPECT_EQ(kBytesPerRow / 2, row_bytes_checked.ValueOrDie());

  row_bytes_checked = ImageFormatPlaneRowBytesChecked(image_format, 2);
  EXPECT_TRUE(row_bytes_checked.IsValid());
  EXPECT_EQ(kBytesPerRow / 2, row_bytes_checked.ValueOrDie());

  EXPECT_FALSE(ImageFormatPlaneRowBytesChecked(image_format, 3).IsValid());
}
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)

TEST(ImageFormat, TransactionEliminationFormats_V2) {
  PixelFormatAndModifier format;
  format.pixel_format = fuchsia_images2::PixelFormat::kB8G8R8A8;
  format.pixel_format_modifier = fuchsia_images2::PixelFormatModifier::kLinear;

  EXPECT_TRUE(ImageFormatCompatibleWithProtectedMemory(format));

  auto format2 = format;
  format2.pixel_format_modifier = fuchsia_images2::PixelFormatModifier::kArmLinearTe;

  EXPECT_FALSE(ImageFormatCompatibleWithProtectedMemory(format2));

  sysmem_v2::ImageFormatConstraints constraints;
  constraints.pixel_format() = format2.pixel_format;
  constraints.pixel_format_modifier() = format2.pixel_format_modifier;
  constraints.min_size() = {12u, 12u};
  constraints.max_size() = {100u, 100u};
  constraints.bytes_per_row_divisor().emplace(4u * 8u);
  constraints.max_bytes_per_row().emplace(100000u);

  auto image_format_result = ImageConstraintsToFormat(constraints, 18, 17);
  EXPECT_TRUE(image_format_result.is_ok());
  auto image_format = image_format_result.take_value();
  // The raw size would be 72 without bytes_per_row_divisor of 32.
  EXPECT_EQ(96u, image_format.bytes_per_row());

  // Check the color plane data.
  uint32_t row_bytes;
  uint64_t plane_offset;
  EXPECT_TRUE(ImageFormatPlaneByteOffset(image_format, 0, &plane_offset));
  EXPECT_EQ(0u, plane_offset);
  EXPECT_TRUE(ImageFormatPlaneRowBytes(image_format, 0, &row_bytes));
  EXPECT_EQ(image_format.bytes_per_row(), row_bytes);

  constexpr uint32_t kTePlane = 3;
  // Check the TE plane data.
  EXPECT_TRUE(ImageFormatPlaneByteOffset(image_format, kTePlane, &plane_offset));
  EXPECT_LE(image_format.bytes_per_row().value() * 17, plane_offset);
  EXPECT_TRUE(ImageFormatPlaneRowBytes(image_format, kTePlane, &row_bytes));

  // Row size should be rounded up to 64 bytes.
  EXPECT_EQ(64, row_bytes);
}

#if FUCHSIA_API_LEVEL_AT_LEAST(32)
TEST(ImageFormat, TransactionEliminationBufferSize_ArmV12) {
  // Test that for 1280x800, the TE CRC buffer size requires 52 rows of 640 bytes
  // plus the 64-byte header, totaling 33,344 bytes (to satisfy Arm Mali v12+
  // 64x64 region fetch requirements).
  sysmem_v2::ImageFormatConstraints constraints;
  constraints.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  constraints.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kArmLinearTe;
  constraints.min_size() = {1280u, 800u};
  constraints.max_size() = {1280u, 800u};
  constraints.bytes_per_row_divisor().emplace(1u);
  constraints.max_bytes_per_row().emplace(100000u);

  auto image_format_result = ImageConstraintsToFormat(constraints, 1280, 800);
  EXPECT_TRUE(image_format_result.is_ok());
  auto image_format = image_format_result.take_value();

  constexpr uint32_t kTePlane = 3;
  auto plane_offset_checked = ImageFormatPlaneByteOffsetChecked(image_format, kTePlane);
  EXPECT_TRUE(plane_offset_checked.IsValid());
  auto total_size_checked = ImageFormatImageSizeChecked(image_format);
  EXPECT_TRUE(total_size_checked.IsValid());
  EXPECT_EQ(33344u, (total_size_checked - plane_offset_checked).ValueOrDie());
}
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)

#if FUCHSIA_API_LEVEL_AT_LEAST(32)
TEST(ImageFormat, TransactionEliminationFormatsChecked_V2) {
  PixelFormatAndModifier format;
  format.pixel_format = fuchsia_images2::PixelFormat::kB8G8R8A8;
  format.pixel_format_modifier = fuchsia_images2::PixelFormatModifier::kLinear;

  EXPECT_TRUE(ImageFormatCompatibleWithProtectedMemory(format));

  auto format2 = format;
  format2.pixel_format_modifier = fuchsia_images2::PixelFormatModifier::kArmLinearTe;

  EXPECT_FALSE(ImageFormatCompatibleWithProtectedMemory(format2));

  sysmem_v2::ImageFormatConstraints constraints;
  constraints.pixel_format() = format2.pixel_format;
  constraints.pixel_format_modifier() = format2.pixel_format_modifier;
  constraints.min_size() = {12u, 12u};
  constraints.max_size() = {100u, 100u};
  constraints.bytes_per_row_divisor().emplace(4u * 8u);
  constraints.max_bytes_per_row().emplace(100000u);

  auto image_format_result = ImageConstraintsToFormat(constraints, 18, 17);
  EXPECT_TRUE(image_format_result.is_ok());
  auto image_format = image_format_result.take_value();
  // The raw size would be 72 without bytes_per_row_divisor of 32.
  EXPECT_EQ(96u, image_format.bytes_per_row());

  // Check the color plane data.
  auto plane_offset_checked = ImageFormatPlaneByteOffsetChecked(image_format, 0);
  EXPECT_TRUE(plane_offset_checked.IsValid());
  EXPECT_EQ(0u, plane_offset_checked.ValueOrDie());
  auto row_bytes_checked = ImageFormatPlaneRowBytesChecked(image_format, 0);
  EXPECT_TRUE(row_bytes_checked.IsValid());
  EXPECT_EQ(image_format.bytes_per_row(), row_bytes_checked.ValueOrDie());

  constexpr uint32_t kTePlane = 3;
  // Check the TE plane data.
  plane_offset_checked = ImageFormatPlaneByteOffsetChecked(image_format, kTePlane);
  EXPECT_TRUE(plane_offset_checked.IsValid());
  EXPECT_LE(image_format.bytes_per_row().value() * 17, plane_offset_checked.ValueOrDie());
  row_bytes_checked = ImageFormatPlaneRowBytesChecked(image_format, kTePlane);
  EXPECT_TRUE(row_bytes_checked.IsValid());

  // Row size should be rounded up to 64 bytes.
  EXPECT_EQ(64, row_bytes_checked.ValueOrDie());
}
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)

TEST(ImageFormat, TransactionEliminationFormats_V2_wire) {
  fidl::Arena allocator;
  PixelFormatAndModifier format;
  format.pixel_format = fuchsia_images2::wire::PixelFormat::kB8G8R8A8;
  format.pixel_format_modifier = fuchsia_images2::wire::PixelFormatModifier::kLinear;

  EXPECT_TRUE(ImageFormatCompatibleWithProtectedMemory(format));

  auto format2 = format;
  format2.pixel_format_modifier = fuchsia_images2::wire::PixelFormatModifier::kArmLinearTe;

  EXPECT_FALSE(ImageFormatCompatibleWithProtectedMemory(format2));

  auto constraints = sysmem_v2::wire::ImageFormatConstraints::Builder(allocator)
                         .pixel_format(format2.pixel_format)
                         .pixel_format_modifier(format2.pixel_format_modifier)
                         .min_size(fuchsia_math::wire::SizeU{12u, 12u})
                         .max_size(fuchsia_math::wire::SizeU{100u, 100u})
                         .bytes_per_row_divisor(4u * 8u)
                         .max_bytes_per_row(100000u)
                         .Build();

  auto image_format_result = ImageConstraintsToFormat(allocator, constraints, 18, 17);
  EXPECT_TRUE(image_format_result.is_ok());
  auto image_format = image_format_result.take_value();
  // The raw size would be 72 without bytes_per_row_divisor of 32.
  EXPECT_EQ(96u, image_format.bytes_per_row());

  // Check the color plane data.
  uint32_t row_bytes;
  uint64_t plane_offset;
  EXPECT_TRUE(ImageFormatPlaneByteOffset(image_format, 0, &plane_offset));
  EXPECT_EQ(0u, plane_offset);
  EXPECT_TRUE(ImageFormatPlaneRowBytes(image_format, 0, &row_bytes));
  EXPECT_EQ(image_format.bytes_per_row(), row_bytes);

  constexpr uint32_t kTePlane = 3;
  // Check the TE plane data.
  EXPECT_TRUE(ImageFormatPlaneByteOffset(image_format, kTePlane, &plane_offset));
  EXPECT_LE(image_format.bytes_per_row() * 17, plane_offset);
  EXPECT_TRUE(ImageFormatPlaneRowBytes(image_format, kTePlane, &row_bytes));

  // Row size should be rounded up to 64 bytes.
  EXPECT_EQ(64, row_bytes);
}

#if FUCHSIA_API_LEVEL_AT_LEAST(32)
TEST(ImageFormat, TransactionEliminationFormatsChecked_V2_wire) {
  fidl::Arena allocator;
  PixelFormatAndModifier format;
  format.pixel_format = fuchsia_images2::wire::PixelFormat::kB8G8R8A8;
  format.pixel_format_modifier = fuchsia_images2::wire::PixelFormatModifier::kLinear;

  EXPECT_TRUE(ImageFormatCompatibleWithProtectedMemory(format));

  auto format2 = format;
  format2.pixel_format_modifier = fuchsia_images2::wire::PixelFormatModifier::kArmLinearTe;

  EXPECT_FALSE(ImageFormatCompatibleWithProtectedMemory(format2));

  auto constraints = sysmem_v2::wire::ImageFormatConstraints::Builder(allocator)
                         .pixel_format(format2.pixel_format)
                         .pixel_format_modifier(format2.pixel_format_modifier)
                         .min_size(fuchsia_math::wire::SizeU{12u, 12u})
                         .max_size(fuchsia_math::wire::SizeU{100u, 100u})
                         .bytes_per_row_divisor(4u * 8u)
                         .max_bytes_per_row(100000u)
                         .Build();

  auto image_format_result = ImageConstraintsToFormat(allocator, constraints, 18, 17);
  EXPECT_TRUE(image_format_result.is_ok());
  auto image_format = image_format_result.take_value();
  // The raw size would be 72 without bytes_per_row_divisor of 32.
  EXPECT_EQ(96u, image_format.bytes_per_row());

  // Check the color plane data.
  auto plane_offset_checked = ImageFormatPlaneByteOffsetChecked(image_format, 0);
  EXPECT_TRUE(plane_offset_checked.IsValid());
  EXPECT_EQ(0u, plane_offset_checked.ValueOrDie());
  auto row_bytes_checked = ImageFormatPlaneRowBytesChecked(image_format, 0);
  EXPECT_TRUE(row_bytes_checked.IsValid());
  EXPECT_EQ(image_format.bytes_per_row(), row_bytes_checked.ValueOrDie());

  constexpr uint32_t kTePlane = 3;
  // Check the TE plane data.
  plane_offset_checked = ImageFormatPlaneByteOffsetChecked(image_format, kTePlane);
  EXPECT_TRUE(plane_offset_checked.IsValid());
  EXPECT_LE(image_format.bytes_per_row() * 17, plane_offset_checked.ValueOrDie());
  row_bytes_checked = ImageFormatPlaneRowBytesChecked(image_format, kTePlane);
  EXPECT_TRUE(row_bytes_checked.IsValid());

  // Row size should be rounded up to 64 bytes.
  EXPECT_EQ(64, row_bytes_checked.ValueOrDie());
}
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)

TEST(ImageFormat, TransactionEliminationFormats_V1_wire) {
  sysmem_v1::wire::PixelFormat format = {
      .type = sysmem_v1::wire::PixelFormatType::kBgra32,
      .has_format_modifier = true,
      .format_modifier =
          {
              .value = sysmem_v1::wire::kFormatModifierLinear,
          },
  };
  EXPECT_TRUE(ImageFormatCompatibleWithProtectedMemory(format));

  format.format_modifier.value = sysmem_v1::wire::kFormatModifierArmLinearTe;
  EXPECT_FALSE(ImageFormatCompatibleWithProtectedMemory(format));

  sysmem_v1::wire::ImageFormatConstraints constraints = {
      .pixel_format = format,
      .min_coded_width = 12,
      .max_coded_width = 100,
      .min_coded_height = 12,
      .max_coded_height = 100,
      .max_bytes_per_row = 100000,
      .bytes_per_row_divisor = 4 * 8,
  };

  auto optional_format = ImageConstraintsToFormat(constraints, 18, 17);
  EXPECT_TRUE(optional_format);
  auto& image_format = optional_format.value();
  // The raw size would be 72 without bytes_per_row_divisor of 32.
  EXPECT_EQ(96u, image_format.bytes_per_row);

  // Check the color plane data.
  uint32_t row_bytes;
  uint64_t plane_offset;
  EXPECT_TRUE(ImageFormatPlaneByteOffset(image_format, 0, &plane_offset));
  EXPECT_EQ(0u, plane_offset);
  EXPECT_TRUE(ImageFormatPlaneRowBytes(image_format, 0, &row_bytes));
  EXPECT_EQ(image_format.bytes_per_row, row_bytes);

  constexpr uint32_t kTePlane = 3;
  // Check the TE plane data.
  EXPECT_TRUE(ImageFormatPlaneByteOffset(image_format, kTePlane, &plane_offset));
  EXPECT_LE(image_format.bytes_per_row * 17, plane_offset);
  EXPECT_TRUE(ImageFormatPlaneRowBytes(image_format, kTePlane, &row_bytes));

  // Row size should be rounded up to 64 bytes.
  EXPECT_EQ(64, row_bytes);
}

#if FUCHSIA_API_LEVEL_AT_LEAST(32)
TEST(ImageFormat, TransactionEliminationFormatsChecked_V1_wire) {
  sysmem_v1::wire::PixelFormat format = {
      .type = sysmem_v1::wire::PixelFormatType::kBgra32,
      .has_format_modifier = true,
      .format_modifier =
          {
              .value = sysmem_v1::wire::kFormatModifierLinear,
          },
  };
  EXPECT_TRUE(ImageFormatCompatibleWithProtectedMemory(format));

  format.format_modifier.value = sysmem_v1::wire::kFormatModifierArmLinearTe;
  EXPECT_FALSE(ImageFormatCompatibleWithProtectedMemory(format));

  sysmem_v1::wire::ImageFormatConstraints constraints = {
      .pixel_format = format,
      .min_coded_width = 12,
      .max_coded_width = 100,
      .min_coded_height = 12,
      .max_coded_height = 100,
      .max_bytes_per_row = 100000,
      .bytes_per_row_divisor = 4 * 8,
  };

  auto optional_format = ImageConstraintsToFormat(constraints, 18, 17);
  EXPECT_TRUE(optional_format);
  auto& image_format = optional_format.value();
  // The raw size would be 72 without bytes_per_row_divisor of 32.
  EXPECT_EQ(96u, image_format.bytes_per_row);

  // Check the color plane data.
  auto plane_offset_checked = ImageFormatPlaneByteOffsetChecked(image_format, 0);
  EXPECT_TRUE(plane_offset_checked.IsValid());
  EXPECT_EQ(0u, plane_offset_checked.ValueOrDie());
  auto row_bytes_checked = ImageFormatPlaneRowBytesChecked(image_format, 0);
  EXPECT_TRUE(row_bytes_checked.IsValid());
  EXPECT_EQ(image_format.bytes_per_row, row_bytes_checked.ValueOrDie());

  constexpr uint32_t kTePlane = 3;
  // Check the TE plane data.
  plane_offset_checked = ImageFormatPlaneByteOffsetChecked(image_format, kTePlane);
  EXPECT_TRUE(plane_offset_checked.IsValid());
  EXPECT_LE(image_format.bytes_per_row * 17, plane_offset_checked.ValueOrDie());
  row_bytes_checked = ImageFormatPlaneRowBytesChecked(image_format, kTePlane);
  EXPECT_TRUE(row_bytes_checked.IsValid());

  // Row size should be rounded up to 64 bytes.
  EXPECT_EQ(64, row_bytes_checked.ValueOrDie());
}
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)

TEST(ImageFormat, BasicSizes_V2) {
  constexpr uint32_t kWidth = 64;
  constexpr uint32_t kHeight = 128;
  constexpr uint32_t kStride = kWidth * 6;

  fuchsia_images2::ImageFormat image_format_bgra32;
  image_format_bgra32.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  image_format_bgra32.size() = {kWidth, kHeight};
  image_format_bgra32.bytes_per_row().emplace(kStride);
  EXPECT_EQ(kHeight * kStride, ImageFormatImageSize(image_format_bgra32));
  auto pixel_format_and_modifier_bgra32 =
      PixelFormatAndModifierFromImageFormat(image_format_bgra32);
  EXPECT_EQ(1, ImageFormatSurfaceWidthMinDivisor(pixel_format_and_modifier_bgra32));
  EXPECT_EQ(1, ImageFormatSurfaceHeightMinDivisor(pixel_format_and_modifier_bgra32));
  EXPECT_EQ(4, ImageFormatSampleAlignment(pixel_format_and_modifier_bgra32));

  fuchsia_images2::ImageFormat image_format_nv12;
  image_format_nv12.pixel_format() = fuchsia_images2::PixelFormat::kNv12;
  image_format_nv12.size() = {kWidth, kHeight};
  image_format_nv12.bytes_per_row() = kStride;
  EXPECT_EQ(kHeight * kStride * 3 / 2, ImageFormatImageSize(image_format_nv12));
  auto pixel_format_and_modifier_nv12 = PixelFormatAndModifierFromImageFormat(image_format_nv12);
  EXPECT_EQ(2, ImageFormatSurfaceWidthMinDivisor(pixel_format_and_modifier_nv12));
  EXPECT_EQ(2, ImageFormatSurfaceHeightMinDivisor(pixel_format_and_modifier_nv12));
  EXPECT_EQ(2, ImageFormatSampleAlignment(pixel_format_and_modifier_nv12));

  fuchsia_images2::ImageFormat image_format_p010;
  image_format_p010.pixel_format() = fuchsia_images2::PixelFormat::kP010;
  image_format_p010.size() = {kWidth, kHeight};
  image_format_p010.bytes_per_row() = kStride;
  EXPECT_EQ(kHeight * kStride * 3 / 2, ImageFormatImageSize(image_format_p010));
  auto pixel_format_and_modifier_p010 = PixelFormatAndModifierFromImageFormat(image_format_p010);
  EXPECT_EQ(2, ImageFormatSurfaceWidthMinDivisor(pixel_format_and_modifier_p010));
  EXPECT_EQ(2, ImageFormatSurfaceHeightMinDivisor(pixel_format_and_modifier_p010));
  EXPECT_EQ(4, ImageFormatSampleAlignment(pixel_format_and_modifier_p010));

  fuchsia_images2::ImageFormat image_format_yuy2;
  image_format_yuy2.pixel_format() = fuchsia_images2::PixelFormat::kYuy2;
  image_format_yuy2.size() = {kWidth, kHeight};
  image_format_yuy2.bytes_per_row() = kStride;
  EXPECT_EQ(kHeight * kStride, ImageFormatImageSize(image_format_yuy2));
  auto pixel_format_and_modifier_yuy2 = PixelFormatAndModifierFromImageFormat(image_format_yuy2);
  EXPECT_EQ(2, ImageFormatSurfaceWidthMinDivisor(pixel_format_and_modifier_yuy2));
  EXPECT_EQ(2, ImageFormatSurfaceHeightMinDivisor(pixel_format_and_modifier_yuy2));
  EXPECT_EQ(2, ImageFormatSampleAlignment(pixel_format_and_modifier_yuy2));
}

#if FUCHSIA_API_LEVEL_AT_LEAST(32)
TEST(ImageFormat, BasicSizesChecked_V2) {
  constexpr uint32_t kWidth = 64;
  constexpr uint32_t kHeight = 128;
  constexpr uint32_t kStride = kWidth * 6;

  fuchsia_images2::ImageFormat image_format_bgra32;
  image_format_bgra32.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  image_format_bgra32.size() = {kWidth, kHeight};
  image_format_bgra32.bytes_per_row().emplace(kStride);
  auto size_checked_bgra32 = ImageFormatImageSizeChecked(image_format_bgra32);
  EXPECT_TRUE(size_checked_bgra32.IsValid());
  EXPECT_EQ(kHeight * kStride, size_checked_bgra32.ValueOrDie());
  auto pixel_format_and_modifier_bgra32 =
      PixelFormatAndModifierFromImageFormat(image_format_bgra32);
  EXPECT_EQ(1, ImageFormatSurfaceWidthMinDivisor(pixel_format_and_modifier_bgra32));
  EXPECT_EQ(1, ImageFormatSurfaceHeightMinDivisor(pixel_format_and_modifier_bgra32));
  EXPECT_EQ(4, ImageFormatSampleAlignment(pixel_format_and_modifier_bgra32));

  fuchsia_images2::ImageFormat image_format_nv12;
  image_format_nv12.pixel_format() = fuchsia_images2::PixelFormat::kNv12;
  image_format_nv12.size() = {kWidth, kHeight};
  image_format_nv12.bytes_per_row() = kStride;
  auto size_checked_nv12 = ImageFormatImageSizeChecked(image_format_nv12);
  EXPECT_TRUE(size_checked_nv12.IsValid());
  EXPECT_EQ(kHeight * kStride * 3 / 2, size_checked_nv12.ValueOrDie());
  auto pixel_format_and_modifier_nv12 = PixelFormatAndModifierFromImageFormat(image_format_nv12);
  EXPECT_EQ(2, ImageFormatSurfaceWidthMinDivisor(pixel_format_and_modifier_nv12));
  EXPECT_EQ(2, ImageFormatSurfaceHeightMinDivisor(pixel_format_and_modifier_nv12));
  EXPECT_EQ(2, ImageFormatSampleAlignment(pixel_format_and_modifier_nv12));

  fuchsia_images2::ImageFormat image_format_p010;
  image_format_p010.pixel_format() = fuchsia_images2::PixelFormat::kP010;
  image_format_p010.size() = {kWidth, kHeight};
  image_format_p010.bytes_per_row() = kStride;
  auto size_checked_p010 = ImageFormatImageSizeChecked(image_format_p010);
  EXPECT_TRUE(size_checked_p010.IsValid());
  EXPECT_EQ(kHeight * kStride * 3 / 2, size_checked_p010.ValueOrDie());
  auto pixel_format_and_modifier_p010 = PixelFormatAndModifierFromImageFormat(image_format_p010);
  EXPECT_EQ(2, ImageFormatSurfaceWidthMinDivisor(pixel_format_and_modifier_p010));
  EXPECT_EQ(2, ImageFormatSurfaceHeightMinDivisor(pixel_format_and_modifier_p010));
  EXPECT_EQ(4, ImageFormatSampleAlignment(pixel_format_and_modifier_p010));

  fuchsia_images2::ImageFormat image_format_yuy2;
  image_format_yuy2.pixel_format() = fuchsia_images2::PixelFormat::kYuy2;
  image_format_yuy2.size() = {kWidth, kHeight};
  image_format_yuy2.bytes_per_row() = kStride;
  auto size_checked_yuy2 = ImageFormatImageSizeChecked(image_format_yuy2);
  EXPECT_TRUE(size_checked_yuy2.IsValid());
  EXPECT_EQ(kHeight * kStride, size_checked_yuy2.ValueOrDie());
  auto pixel_format_and_modifier_yuy2 = PixelFormatAndModifierFromImageFormat(image_format_yuy2);
  EXPECT_EQ(2, ImageFormatSurfaceWidthMinDivisor(pixel_format_and_modifier_yuy2));
  EXPECT_EQ(2, ImageFormatSurfaceHeightMinDivisor(pixel_format_and_modifier_yuy2));
  EXPECT_EQ(2, ImageFormatSampleAlignment(pixel_format_and_modifier_yuy2));
}
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)

TEST(ImageFormat, BasicSizes_V2_wire) {
  fidl::Arena allocator;
  constexpr uint32_t kWidth = 64;
  constexpr uint32_t kHeight = 128;
  constexpr uint32_t kStride = kWidth * 6;

  auto image_format_bgra32 = fuchsia_images2::wire::ImageFormat::Builder(allocator)
                                 .pixel_format(fuchsia_images2::PixelFormat::kB8G8R8A8)
                                 .size(fuchsia_math::wire::SizeU{kWidth, kHeight})
                                 .bytes_per_row(kStride)
                                 .Build();
  EXPECT_EQ(kHeight * kStride, ImageFormatImageSize(image_format_bgra32));
  auto pixel_format_and_modifier_bgra32 =
      PixelFormatAndModifierFromImageFormat(image_format_bgra32);
  EXPECT_EQ(1, ImageFormatSurfaceWidthMinDivisor(pixel_format_and_modifier_bgra32));
  EXPECT_EQ(1, ImageFormatSurfaceHeightMinDivisor(pixel_format_and_modifier_bgra32));
  EXPECT_EQ(4, ImageFormatSampleAlignment(pixel_format_and_modifier_bgra32));

  auto image_format_nv12 = fuchsia_images2::wire::ImageFormat::Builder(allocator)
                               .pixel_format(fuchsia_images2::PixelFormat::kNv12)
                               .size(fuchsia_math::wire::SizeU{kWidth, kHeight})
                               .bytes_per_row(kStride)
                               .Build();
  EXPECT_EQ(kHeight * kStride * 3 / 2, ImageFormatImageSize(image_format_nv12));
  auto pixel_format_and_modifier_nv12 = PixelFormatAndModifierFromImageFormat(image_format_nv12);
  EXPECT_EQ(2, ImageFormatSurfaceWidthMinDivisor(pixel_format_and_modifier_nv12));
  EXPECT_EQ(2, ImageFormatSurfaceHeightMinDivisor(pixel_format_and_modifier_nv12));
  EXPECT_EQ(2, ImageFormatSampleAlignment(pixel_format_and_modifier_nv12));
}

#if FUCHSIA_API_LEVEL_AT_LEAST(32)
TEST(ImageFormat, BasicSizesChecked_V2_wire) {
  fidl::Arena allocator;
  constexpr uint32_t kWidth = 64;
  constexpr uint32_t kHeight = 128;
  constexpr uint32_t kStride = kWidth * 6;

  auto image_format_bgra32 = fuchsia_images2::wire::ImageFormat::Builder(allocator)
                                 .pixel_format(fuchsia_images2::PixelFormat::kB8G8R8A8)
                                 .size(fuchsia_math::wire::SizeU{kWidth, kHeight})
                                 .bytes_per_row(kStride)
                                 .Build();
  auto size_checked_bgra32 = ImageFormatImageSizeChecked(image_format_bgra32);
  EXPECT_TRUE(size_checked_bgra32.IsValid());
  EXPECT_EQ(kHeight * kStride, size_checked_bgra32.ValueOrDie());
  auto pixel_format_and_modifier_bgra32 =
      PixelFormatAndModifierFromImageFormat(image_format_bgra32);
  EXPECT_EQ(1, ImageFormatSurfaceWidthMinDivisor(pixel_format_and_modifier_bgra32));
  EXPECT_EQ(1, ImageFormatSurfaceHeightMinDivisor(pixel_format_and_modifier_bgra32));
  EXPECT_EQ(4, ImageFormatSampleAlignment(pixel_format_and_modifier_bgra32));

  auto image_format_nv12 = fuchsia_images2::wire::ImageFormat::Builder(allocator)
                               .pixel_format(fuchsia_images2::PixelFormat::kNv12)
                               .size(fuchsia_math::wire::SizeU{kWidth, kHeight})
                               .bytes_per_row(kStride)
                               .Build();
  auto size_checked_nv12 = ImageFormatImageSizeChecked(image_format_nv12);
  EXPECT_TRUE(size_checked_nv12.IsValid());
  EXPECT_EQ(kHeight * kStride * 3 / 2, size_checked_nv12.ValueOrDie());
  auto pixel_format_and_modifier_nv12 = PixelFormatAndModifierFromImageFormat(image_format_nv12);
  EXPECT_EQ(2, ImageFormatSurfaceWidthMinDivisor(pixel_format_and_modifier_nv12));
  EXPECT_EQ(2, ImageFormatSurfaceHeightMinDivisor(pixel_format_and_modifier_nv12));
  EXPECT_EQ(2, ImageFormatSampleAlignment(pixel_format_and_modifier_nv12));
}
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)

TEST(ImageFormat, BasicSizes_V1_wire) {
  constexpr uint32_t kWidth = 64;
  constexpr uint32_t kHeight = 128;
  constexpr uint32_t kStride = 256;

  sysmem_v1::wire::ImageFormat2 image_format_bgra32 = {
      .pixel_format =
          {
              .type = sysmem_v1::wire::PixelFormatType::kBgra32,
          },
      .coded_width = kWidth,
      .coded_height = kHeight,
      .bytes_per_row = kStride,
  };
  EXPECT_EQ(kHeight * kStride, ImageFormatImageSize(image_format_bgra32));
  EXPECT_EQ(1, ImageFormatCodedWidthMinDivisor(image_format_bgra32.pixel_format));
  EXPECT_EQ(1, ImageFormatCodedHeightMinDivisor(image_format_bgra32.pixel_format));
  EXPECT_EQ(4, ImageFormatSampleAlignment(image_format_bgra32.pixel_format));

  sysmem_v1::wire::ImageFormat2 image_format_nv12 = {
      .pixel_format =
          {
              .type = sysmem_v1::wire::PixelFormatType::kNv12,
          },
      .coded_width = kWidth,
      .coded_height = kHeight,
      .bytes_per_row = kStride,
  };
  EXPECT_EQ(kHeight * kStride * 3 / 2, ImageFormatImageSize(image_format_nv12));
  EXPECT_EQ(2, ImageFormatCodedWidthMinDivisor(image_format_nv12.pixel_format));
  EXPECT_EQ(2, ImageFormatCodedHeightMinDivisor(image_format_nv12.pixel_format));
  EXPECT_EQ(2, ImageFormatSampleAlignment(image_format_nv12.pixel_format));
}

#if FUCHSIA_API_LEVEL_AT_LEAST(32)
TEST(ImageFormat, BasicSizesChecked_V1_wire) {
  constexpr uint32_t kWidth = 64;
  constexpr uint32_t kHeight = 128;
  constexpr uint32_t kStride = 256;

  sysmem_v1::wire::ImageFormat2 image_format_bgra32 = {
      .pixel_format =
          {
              .type = sysmem_v1::wire::PixelFormatType::kBgra32,
          },
      .coded_width = kWidth,
      .coded_height = kHeight,
      .bytes_per_row = kStride,
  };
  auto size_checked_bgra32 = ImageFormatImageSizeChecked(image_format_bgra32);
  EXPECT_TRUE(size_checked_bgra32.IsValid());
  EXPECT_EQ(kHeight * kStride, size_checked_bgra32.ValueOrDie());
  EXPECT_EQ(1, ImageFormatCodedWidthMinDivisor(image_format_bgra32.pixel_format));
  EXPECT_EQ(1, ImageFormatCodedHeightMinDivisor(image_format_bgra32.pixel_format));
  EXPECT_EQ(4, ImageFormatSampleAlignment(image_format_bgra32.pixel_format));

  sysmem_v1::wire::ImageFormat2 image_format_nv12 = {
      .pixel_format =
          {
              .type = sysmem_v1::wire::PixelFormatType::kNv12,
          },
      .coded_width = kWidth,
      .coded_height = kHeight,
      .bytes_per_row = kStride,
  };
  auto size_checked_nv12 = ImageFormatImageSizeChecked(image_format_nv12);
  EXPECT_TRUE(size_checked_nv12.IsValid());
  EXPECT_EQ(kHeight * kStride * 3 / 2, size_checked_nv12.ValueOrDie());
  EXPECT_EQ(2, ImageFormatCodedWidthMinDivisor(image_format_nv12.pixel_format));
  EXPECT_EQ(2, ImageFormatCodedHeightMinDivisor(image_format_nv12.pixel_format));
  EXPECT_EQ(2, ImageFormatSampleAlignment(image_format_nv12.pixel_format));
}
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)

TEST(ImageFormat, AfbcFlagFormats_V2) {
  PixelFormatAndModifier format;
  format.pixel_format = fuchsia_images2::PixelFormat::kB8G8R8A8;
  format.pixel_format_modifier =
      fuchsia_images2::PixelFormatModifier::kArmAfbc16X16SplitBlockSparseYuvTe;

  EXPECT_FALSE(ImageFormatCompatibleWithProtectedMemory(format));

  sysmem_v2::ImageFormatConstraints constraints;
  constraints.pixel_format() = format.pixel_format;
  constraints.pixel_format_modifier() = format.pixel_format_modifier;
  constraints.min_size() = {12u, 12u};
  constraints.max_size() = {100u, 100u};
  constraints.bytes_per_row_divisor().emplace(4u * 8u);
  constraints.max_bytes_per_row().emplace(100000u);

  auto image_format_result = ImageConstraintsToFormat(constraints, 18, 17);
  EXPECT_TRUE(image_format_result.is_ok());

  constraints.pixel_format_modifier() =
      fuchsia_images2::PixelFormatModifier::kArmAfbc16X16SplitBlockSparseYuvTiledHeader;

  image_format_result = ImageConstraintsToFormat(constraints, 18, 17);
  EXPECT_TRUE(image_format_result.is_ok());
  auto image_format = image_format_result.take_value();
  constexpr uint32_t kMinHeaderOffset = 4096u;
  constexpr uint32_t kMinWidth = 128;
  constexpr uint32_t kMinHeight = 128;
  EXPECT_EQ(kMinHeaderOffset + kMinWidth * kMinHeight * 4, ImageFormatImageSize(image_format));
}

#if FUCHSIA_API_LEVEL_AT_LEAST(32)
TEST(ImageFormat, AfbcFlagFormatsChecked_V2) {
  PixelFormatAndModifier format;
  format.pixel_format = fuchsia_images2::PixelFormat::kB8G8R8A8;
  format.pixel_format_modifier =
      fuchsia_images2::PixelFormatModifier::kArmAfbc16X16SplitBlockSparseYuvTe;

  EXPECT_FALSE(ImageFormatCompatibleWithProtectedMemory(format));

  sysmem_v2::ImageFormatConstraints constraints;
  constraints.pixel_format() = format.pixel_format;
  constraints.pixel_format_modifier() = format.pixel_format_modifier;
  constraints.min_size() = {12u, 12u};
  constraints.max_size() = {100u, 100u};
  constraints.bytes_per_row_divisor().emplace(4u * 8u);
  constraints.max_bytes_per_row().emplace(100000u);

  auto image_format_result = ImageConstraintsToFormat(constraints, 18, 17);
  EXPECT_TRUE(image_format_result.is_ok());

  constraints.pixel_format_modifier() =
      fuchsia_images2::PixelFormatModifier::kArmAfbc16X16SplitBlockSparseYuvTiledHeader;

  image_format_result = ImageConstraintsToFormat(constraints, 18, 17);
  EXPECT_TRUE(image_format_result.is_ok());
  auto image_format = image_format_result.take_value();
  constexpr uint32_t kMinHeaderOffset = 4096u;
  constexpr uint32_t kMinWidth = 128;
  constexpr uint32_t kMinHeight = 128;
  auto size_checked = ImageFormatImageSizeChecked(image_format);
  EXPECT_TRUE(size_checked.IsValid());
  EXPECT_EQ(kMinHeaderOffset + kMinWidth * kMinHeight * 4, size_checked.ValueOrDie());
}
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)

TEST(ImageFormat, AfbcFlagFormats_V2_wire) {
  fidl::Arena allocator;
  PixelFormatAndModifier format;
  format.pixel_format = fuchsia_images2::wire::PixelFormat::kB8G8R8A8;
  format.pixel_format_modifier =
      fuchsia_images2::wire::PixelFormatModifier::kArmAfbc16X16SplitBlockSparseYuvTe;

  EXPECT_FALSE(ImageFormatCompatibleWithProtectedMemory(format));

  auto make_constraints = [&](fuchsia_images2::wire::PixelFormatModifier modifier) {
    return sysmem_v2::wire::ImageFormatConstraints::Builder(allocator)
        .pixel_format(format.pixel_format)
        .pixel_format_modifier(modifier)
        .min_size(fuchsia_math::wire::SizeU{12u, 12u})
        .max_size(fuchsia_math::wire::SizeU{100u, 100u})
        .bytes_per_row_divisor(4u * 8u)
        .max_bytes_per_row(100000u)
        .Build();
  };

  auto constraints = make_constraints(format.pixel_format_modifier);
  auto image_format_result = ImageConstraintsToFormat(allocator, constraints, 18, 17);
  EXPECT_TRUE(image_format_result.is_ok());

  constraints = make_constraints(
      fuchsia_images2::wire::PixelFormatModifier::kArmAfbc16X16SplitBlockSparseYuvTiledHeader);

  image_format_result = ImageConstraintsToFormat(allocator, constraints, 18, 17);
  EXPECT_TRUE(image_format_result.is_ok());
  auto image_format = image_format_result.take_value();
  constexpr uint32_t kMinHeaderOffset = 4096u;
  constexpr uint32_t kMinWidth = 128;
  constexpr uint32_t kMinHeight = 128;
  EXPECT_EQ(kMinHeaderOffset + kMinWidth * kMinHeight * 4, ImageFormatImageSize(image_format));
}

#if FUCHSIA_API_LEVEL_AT_LEAST(32)
TEST(ImageFormat, AfbcFlagFormatsChecked_V2_wire) {
  fidl::Arena allocator;
  PixelFormatAndModifier format;
  format.pixel_format = fuchsia_images2::wire::PixelFormat::kB8G8R8A8;
  format.pixel_format_modifier =
      fuchsia_images2::wire::PixelFormatModifier::kArmAfbc16X16SplitBlockSparseYuvTe;

  EXPECT_FALSE(ImageFormatCompatibleWithProtectedMemory(format));

  auto make_constraints = [&](fuchsia_images2::wire::PixelFormatModifier modifier) {
    return sysmem_v2::wire::ImageFormatConstraints::Builder(allocator)
        .pixel_format(format.pixel_format)
        .pixel_format_modifier(modifier)
        .min_size(fuchsia_math::wire::SizeU{12u, 12u})
        .max_size(fuchsia_math::wire::SizeU{100u, 100u})
        .bytes_per_row_divisor(4u * 8u)
        .max_bytes_per_row(100000u)
        .Build();
  };

  auto constraints = make_constraints(format.pixel_format_modifier);
  auto image_format_result = ImageConstraintsToFormat(allocator, constraints, 18, 17);
  EXPECT_TRUE(image_format_result.is_ok());

  constraints = make_constraints(
      fuchsia_images2::wire::PixelFormatModifier::kArmAfbc16X16SplitBlockSparseYuvTiledHeader);

  image_format_result = ImageConstraintsToFormat(allocator, constraints, 18, 17);
  EXPECT_TRUE(image_format_result.is_ok());
  auto image_format = image_format_result.take_value();
  constexpr uint32_t kMinHeaderOffset = 4096u;
  constexpr uint32_t kMinWidth = 128;
  constexpr uint32_t kMinHeight = 128;
  auto size_checked = ImageFormatImageSizeChecked(image_format);
  EXPECT_TRUE(size_checked.IsValid());
  EXPECT_EQ(kMinHeaderOffset + kMinWidth * kMinHeight * 4, size_checked.ValueOrDie());
}
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)

TEST(ImageFormat, AfbcFlagFormats_V1_wire) {
  sysmem_v1::wire::PixelFormat format = {
      .type = sysmem_v1::wire::PixelFormatType::kBgra32,
      .has_format_modifier = true,
      .format_modifier =
          {
              .value = sysmem_v1::wire::kFormatModifierArmAfbc16X16SplitBlockSparseYuvTe,
          },
  };

  EXPECT_FALSE(ImageFormatCompatibleWithProtectedMemory(format));

  sysmem_v1::wire::ImageFormatConstraints constraints = {
      .pixel_format = format,
      .min_coded_width = 12,
      .max_coded_width = 100,
      .min_coded_height = 12,
      .max_coded_height = 100,
      .max_bytes_per_row = 100000,
      .bytes_per_row_divisor = 4 * 8,
  };

  auto optional_format = ImageConstraintsToFormat(constraints, 18, 17);
  EXPECT_TRUE(optional_format);

  sysmem_v1::wire::PixelFormat tiled_format = {
      .type = sysmem_v1::wire::PixelFormatType::kBgra32,
      .has_format_modifier = true,
      .format_modifier =
          {
              .value = sysmem_v1::wire::kFormatModifierArmAfbc16X16SplitBlockSparseYuvTiledHeader,
          },
  };

  constraints.pixel_format = tiled_format;

  optional_format = ImageConstraintsToFormat(constraints, 18, 17);
  EXPECT_TRUE(optional_format);
  auto& image_format = optional_format.value();
  constexpr uint32_t kMinHeaderOffset = 4096u;
  constexpr uint32_t kMinWidth = 128;
  constexpr uint32_t kMinHeight = 128;
  EXPECT_EQ(kMinHeaderOffset + kMinWidth * kMinHeight * 4, ImageFormatImageSize(image_format));
}

#if FUCHSIA_API_LEVEL_AT_LEAST(32)
TEST(ImageFormat, AfbcFlagFormatsChecked_V1_wire) {
  sysmem_v1::wire::PixelFormat format = {
      .type = sysmem_v1::wire::PixelFormatType::kBgra32,
      .has_format_modifier = true,
      .format_modifier =
          {
              .value = sysmem_v1::wire::kFormatModifierArmAfbc16X16SplitBlockSparseYuvTe,
          },
  };

  EXPECT_FALSE(ImageFormatCompatibleWithProtectedMemory(format));

  sysmem_v1::wire::ImageFormatConstraints constraints = {
      .pixel_format = format,
      .min_coded_width = 12,
      .max_coded_width = 100,
      .min_coded_height = 12,
      .max_coded_height = 100,
      .max_bytes_per_row = 100000,
      .bytes_per_row_divisor = 4 * 8,
  };

  auto optional_format = ImageConstraintsToFormat(constraints, 18, 17);
  EXPECT_TRUE(optional_format);

  sysmem_v1::wire::PixelFormat tiled_format = {
      .type = sysmem_v1::wire::PixelFormatType::kBgra32,
      .has_format_modifier = true,
      .format_modifier =
          {
              .value = sysmem_v1::wire::kFormatModifierArmAfbc16X16SplitBlockSparseYuvTiledHeader,
          },
  };

  constraints.pixel_format = tiled_format;

  optional_format = ImageConstraintsToFormat(constraints, 18, 17);
  EXPECT_TRUE(optional_format);
  auto& image_format = optional_format.value();
  constexpr uint32_t kMinHeaderOffset = 4096u;
  constexpr uint32_t kMinWidth = 128;
  constexpr uint32_t kMinHeight = 128;
  auto size_checked = ImageFormatImageSizeChecked(image_format);
  EXPECT_TRUE(size_checked.IsValid());
  EXPECT_EQ(kMinHeaderOffset + kMinWidth * kMinHeight * 4, size_checked.ValueOrDie());
}
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)

TEST(ImageFormat, R8G8Formats_V1_wire) {
  sysmem_v1::wire::PixelFormat format = {
      .type = sysmem_v1::wire::PixelFormatType::kR8G8,
      .has_format_modifier = true,
      .format_modifier =
          {
              .value = sysmem_v1::wire::kFormatModifierLinear,
          },
  };

  sysmem_v1::wire::ImageFormatConstraints constraints = {
      .pixel_format = format,
      .min_coded_width = 12,
      .max_coded_width = 100,
      .min_coded_height = 12,
      .max_coded_height = 100,
      .max_bytes_per_row = 100000,
      .bytes_per_row_divisor = 1,
  };

  {
    auto optional_format = ImageConstraintsToFormat(constraints, 18, 17);
    EXPECT_TRUE(optional_format);
    auto& image_format = optional_format.value();
    EXPECT_EQ(18u * 2, image_format.bytes_per_row);
    EXPECT_EQ(18u * 17u * 2, ImageFormatImageSize(image_format));
  }

  constraints.pixel_format.type = sysmem_v1::wire::PixelFormatType::kR8;

  {
    auto optional_format = ImageConstraintsToFormat(constraints, 18, 17);
    EXPECT_TRUE(optional_format);
    auto& image_format = optional_format.value();
    EXPECT_EQ(18u * 1, image_format.bytes_per_row);
    EXPECT_EQ(18u * 17u * 1, ImageFormatImageSize(image_format));
  }
}

TEST(ImageFormat, R8G8Formats_V2) {
  sysmem_v2::ImageFormatConstraints constraints;
  constraints.pixel_format() = fuchsia_images2::PixelFormat::kR8G8;
  constraints.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kLinear;
  constraints.min_size() = {12u, 12u};
  constraints.max_size() = {100u, 100u};
  constraints.bytes_per_row_divisor().emplace(1u);
  constraints.max_bytes_per_row().emplace(100000u);

  {
    auto optional_format = ImageConstraintsToFormat(constraints, 18, 17);
    EXPECT_TRUE(optional_format.is_ok());
    auto& image_format = optional_format.value();
    EXPECT_EQ(18u * 2, image_format.bytes_per_row());
    EXPECT_EQ(18u * 17u * 2, ImageFormatImageSize(image_format));
  }

  constraints.pixel_format() = fuchsia_images2::PixelFormat::kR8;

  {
    auto optional_format = ImageConstraintsToFormat(constraints, 18, 17);
    EXPECT_TRUE(optional_format.is_ok());
    auto& image_format = optional_format.value();
    EXPECT_EQ(18u * 1, image_format.bytes_per_row());
    EXPECT_EQ(18u * 17u * 1, ImageFormatImageSize(image_format));
  }
}

#if FUCHSIA_API_LEVEL_AT_LEAST(32)
TEST(ImageFormat, R8G8FormatsChecked_V2) {
  sysmem_v2::ImageFormatConstraints constraints;
  constraints.pixel_format() = fuchsia_images2::PixelFormat::kR8G8;
  constraints.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kLinear;
  constraints.min_size() = {12u, 12u};
  constraints.max_size() = {100u, 100u};
  constraints.bytes_per_row_divisor().emplace(1u);
  constraints.max_bytes_per_row().emplace(100000u);

  {
    auto optional_format = ImageConstraintsToFormat(constraints, 18, 17);
    EXPECT_TRUE(optional_format.is_ok());
    auto& image_format = optional_format.value();
    auto bytes_per_row_checked = ImageFormatMinimumRowBytesChecked(constraints, 18);
    EXPECT_TRUE(bytes_per_row_checked.IsValid());
    EXPECT_EQ(18u * 2, bytes_per_row_checked.ValueOrDie());
    auto size_checked = ImageFormatImageSizeChecked(image_format);
    EXPECT_TRUE(size_checked.IsValid());
    EXPECT_EQ(18u * 17u * 2, size_checked.ValueOrDie());
  }

  constraints.pixel_format() = fuchsia_images2::PixelFormat::kR8;

  {
    auto optional_format = ImageConstraintsToFormat(constraints, 18, 17);
    EXPECT_TRUE(optional_format.is_ok());
    auto& image_format = optional_format.value();
    auto bytes_per_row_checked = ImageFormatMinimumRowBytesChecked(constraints, 18);
    EXPECT_TRUE(bytes_per_row_checked.IsValid());
    EXPECT_EQ(18u * 1, bytes_per_row_checked.ValueOrDie());
    auto size_checked = ImageFormatImageSizeChecked(image_format);
    EXPECT_TRUE(size_checked.IsValid());
    EXPECT_EQ(18u * 17u * 1, size_checked.ValueOrDie());
  }
}
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)

TEST(ImageFormat, R8G8Formats_V2_wire) {
  fidl::Arena allocator;
  auto make_constraints = [&](fuchsia_images2::wire::PixelFormat pixel_format) {
    return sysmem_v2::wire::ImageFormatConstraints::Builder(allocator)
        .pixel_format(pixel_format)
        .pixel_format_modifier(fuchsia_images2::wire::PixelFormatModifier::kLinear)
        .min_size(fuchsia_math::wire::SizeU{12u, 12u})
        .max_size(fuchsia_math::wire::SizeU{100u, 100u})
        .bytes_per_row_divisor(1u)
        .max_bytes_per_row(100000u)
        .Build();
  };

  auto constraints = make_constraints(fuchsia_images2::wire::PixelFormat::kR8G8);
  {
    auto optional_format = ImageConstraintsToFormat(allocator, constraints, 18, 17);
    EXPECT_TRUE(optional_format.is_ok());
    auto& image_format = optional_format.value();
    EXPECT_EQ(18u * 2, image_format.bytes_per_row());
    EXPECT_EQ(18u * 17u * 2, ImageFormatImageSize(image_format));
  }

  constraints = make_constraints(fuchsia_images2::wire::PixelFormat::kR8);

  {
    auto optional_format = ImageConstraintsToFormat(allocator, constraints, 18, 17);
    EXPECT_TRUE(optional_format.is_ok());
    auto& image_format = optional_format.value();
    EXPECT_EQ(18u * 1, image_format.bytes_per_row());
    EXPECT_EQ(18u * 17u * 1, ImageFormatImageSize(image_format));
  }
}

#if FUCHSIA_API_LEVEL_AT_LEAST(32)
TEST(ImageFormat, R8G8FormatsChecked_V2_wire) {
  fidl::Arena allocator;
  auto make_constraints = [&](fuchsia_images2::wire::PixelFormat pixel_format) {
    return sysmem_v2::wire::ImageFormatConstraints::Builder(allocator)
        .pixel_format(pixel_format)
        .pixel_format_modifier(fuchsia_images2::wire::PixelFormatModifier::kLinear)
        .min_size(fuchsia_math::wire::SizeU{12u, 12u})
        .max_size(fuchsia_math::wire::SizeU{100u, 100u})
        .bytes_per_row_divisor(1u)
        .max_bytes_per_row(100000u)
        .Build();
  };

  auto constraints = make_constraints(fuchsia_images2::wire::PixelFormat::kR8G8);
  {
    auto optional_format = ImageConstraintsToFormat(allocator, constraints, 18, 17);
    EXPECT_TRUE(optional_format.is_ok());
    auto& image_format = optional_format.value();
    auto bytes_per_row_checked = ImageFormatMinimumRowBytesChecked(constraints, 18);
    EXPECT_TRUE(bytes_per_row_checked.IsValid());
    EXPECT_EQ(18u * 2, bytes_per_row_checked.ValueOrDie());
    auto size_checked = ImageFormatImageSizeChecked(image_format);
    EXPECT_TRUE(size_checked.IsValid());
    EXPECT_EQ(18u * 17u * 2, size_checked.ValueOrDie());
  }

  constraints = make_constraints(fuchsia_images2::wire::PixelFormat::kR8);

  {
    auto optional_format = ImageConstraintsToFormat(allocator, constraints, 18, 17);
    EXPECT_TRUE(optional_format.is_ok());
    auto& image_format = optional_format.value();
    auto bytes_per_row_checked = ImageFormatMinimumRowBytesChecked(constraints, 18);
    EXPECT_TRUE(bytes_per_row_checked.IsValid());
    EXPECT_EQ(18u * 1, bytes_per_row_checked.ValueOrDie());
    auto size_checked = ImageFormatImageSizeChecked(image_format);
    EXPECT_TRUE(size_checked.IsValid());
    EXPECT_EQ(18u * 17u * 1, size_checked.ValueOrDie());
  }
}
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)

#if FUCHSIA_API_LEVEL_AT_LEAST(32)
TEST(ImageFormat, R8G8FormatsChecked_V1_wire) {
  sysmem_v1::wire::PixelFormat format = {
      .type = sysmem_v1::wire::PixelFormatType::kR8G8,
      .has_format_modifier = true,
      .format_modifier =
          {
              .value = sysmem_v1::wire::kFormatModifierLinear,
          },
  };

  sysmem_v1::wire::ImageFormatConstraints constraints = {
      .pixel_format = format,
      .min_coded_width = 12,
      .max_coded_width = 100,
      .min_coded_height = 12,
      .max_coded_height = 100,
      .max_bytes_per_row = 100000,
      .bytes_per_row_divisor = 1,
  };

  {
    auto optional_format = ImageConstraintsToFormat(constraints, 18, 17);
    EXPECT_TRUE(optional_format);
    auto& image_format = optional_format.value();
    auto bytes_per_row_checked = ImageFormatMinimumRowBytesChecked(constraints, 18);
    EXPECT_TRUE(bytes_per_row_checked.IsValid());
    EXPECT_EQ(18u * 2, bytes_per_row_checked.ValueOrDie());
    auto size_checked = ImageFormatImageSizeChecked(image_format);
    EXPECT_TRUE(size_checked.IsValid());
    EXPECT_EQ(18u * 17u * 2, size_checked.ValueOrDie());
  }

  constraints.pixel_format.type = sysmem_v1::wire::PixelFormatType::kR8;

  {
    auto optional_format = ImageConstraintsToFormat(constraints, 18, 17);
    EXPECT_TRUE(optional_format);
    auto& image_format = optional_format.value();
    auto bytes_per_row_checked = ImageFormatMinimumRowBytesChecked(constraints, 18);
    EXPECT_TRUE(bytes_per_row_checked.IsValid());
    EXPECT_EQ(18u * 1, bytes_per_row_checked.ValueOrDie());
    auto size_checked = ImageFormatImageSizeChecked(image_format);
    EXPECT_TRUE(size_checked.IsValid());
    EXPECT_EQ(18u * 17u * 1, size_checked.ValueOrDie());
  }
}
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)

TEST(ImageFormat, A2R10G10B10_Formats_V1_wire) {
  for (const auto& pixel_format_type : {sysmem_v1::wire::PixelFormatType::kA2R10G10B10,
                                        sysmem_v1::wire::PixelFormatType::kA2B10G10R10}) {
    sysmem_v1::wire::PixelFormat format = {
        .type = pixel_format_type,
        .has_format_modifier = true,
        .format_modifier =
            {
                .value = sysmem_v1::wire::kFormatModifierLinear,
            },
    };

    sysmem_v1::wire::ImageFormatConstraints constraints = {
        .pixel_format = format,
        .min_coded_width = 12,
        .max_coded_width = 100,
        .min_coded_height = 12,
        .max_coded_height = 100,
        .max_bytes_per_row = 100000,
        .bytes_per_row_divisor = 1,
    };

    auto optional_format = ImageConstraintsToFormat(constraints, 18, 17);
    EXPECT_TRUE(optional_format);
    auto& image_format = optional_format.value();
    EXPECT_EQ(18u * 4, image_format.bytes_per_row);
    EXPECT_EQ(18u * 17u * 4, ImageFormatImageSize(image_format));
    EXPECT_EQ(1, ImageFormatCodedWidthMinDivisor(image_format.pixel_format));
    EXPECT_EQ(1, ImageFormatCodedHeightMinDivisor(image_format.pixel_format));
    EXPECT_EQ(4, ImageFormatSampleAlignment(image_format.pixel_format));
  }
}

TEST(ImageFormat, A2R10G10B10_Formats_V2) {
  for (const auto& pixel_format :
       {fuchsia_images2::PixelFormat::kA2R10G10B10, fuchsia_images2::PixelFormat::kA2B10G10R10}) {
    sysmem_v2::ImageFormatConstraints constraints;
    constraints.pixel_format() = pixel_format;
    constraints.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kLinear;
    constraints.min_size() = {12u, 12u};
    constraints.max_size() = {100u, 100u};
    constraints.bytes_per_row_divisor().emplace(1u);
    constraints.max_bytes_per_row().emplace(100000u);

    auto optional_format = ImageConstraintsToFormat(constraints, 18, 17);
    EXPECT_TRUE(optional_format.is_ok());
    auto& image_format = optional_format.value();
    EXPECT_EQ(18u * 4, image_format.bytes_per_row());
    EXPECT_EQ(18u * 17u * 4, ImageFormatImageSize(image_format));
    auto pixel_format_and_modifier = PixelFormatAndModifierFromImageFormat(image_format);
    EXPECT_EQ(1u, ImageFormatSurfaceWidthMinDivisor(pixel_format_and_modifier));
    EXPECT_EQ(1u, ImageFormatSurfaceHeightMinDivisor(pixel_format_and_modifier));
    EXPECT_EQ(4, ImageFormatSampleAlignment(pixel_format_and_modifier));
  }
}

TEST(ImageFormat, A2R10G10B10_Formats_V2_wire) {
  for (const auto& pixel_format : {fuchsia_images2::wire::PixelFormat::kA2R10G10B10,
                                   fuchsia_images2::wire::PixelFormat::kA2B10G10R10}) {
    fidl::Arena allocator;
    auto constraints =
        sysmem_v2::wire::ImageFormatConstraints::Builder(allocator)
            .pixel_format(pixel_format)
            .pixel_format_modifier(fuchsia_images2::wire::PixelFormatModifier::kLinear)
            .min_size(fuchsia_math::wire::SizeU{12u, 12u})
            .max_size(fuchsia_math::wire::SizeU{100u, 100u})
            .bytes_per_row_divisor(1u)
            .max_bytes_per_row(100000u)
            .Build();

    auto optional_format = ImageConstraintsToFormat(allocator, constraints, 18, 17);
    EXPECT_TRUE(optional_format.is_ok());
    auto& image_format = optional_format.value();
    EXPECT_EQ(18u * 4, image_format.bytes_per_row());
    EXPECT_EQ(18u * 17u * 4, ImageFormatImageSize(image_format));
    auto pixel_format_and_modifier = PixelFormatAndModifierFromImageFormat(image_format);
    EXPECT_EQ(1u, ImageFormatSurfaceWidthMinDivisor(pixel_format_and_modifier));
    EXPECT_EQ(1u, ImageFormatSurfaceHeightMinDivisor(pixel_format_and_modifier));
    EXPECT_EQ(4, ImageFormatSampleAlignment(pixel_format_and_modifier));
  }
}

#if FUCHSIA_API_LEVEL_AT_LEAST(32)
TEST(ImageFormat, A2R10G10B10_FormatsChecked_V2) {
  for (const auto& pixel_format :
       {fuchsia_images2::PixelFormat::kA2R10G10B10, fuchsia_images2::PixelFormat::kA2B10G10R10}) {
    sysmem_v2::ImageFormatConstraints constraints;
    constraints.pixel_format() = pixel_format;
    constraints.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kLinear;
    constraints.min_size() = {12u, 12u};
    constraints.max_size() = {100u, 100u};
    constraints.bytes_per_row_divisor().emplace(1u);
    constraints.max_bytes_per_row().emplace(100000u);

    auto optional_format = ImageConstraintsToFormat(constraints, 18, 17);
    EXPECT_TRUE(optional_format.is_ok());
    auto& image_format = optional_format.value();
    auto bytes_per_row_checked = ImageFormatMinimumRowBytesChecked(constraints, 18);
    EXPECT_TRUE(bytes_per_row_checked.IsValid());
    EXPECT_EQ(18u * 4, bytes_per_row_checked.ValueOrDie());
    auto size_checked = ImageFormatImageSizeChecked(image_format);
    EXPECT_TRUE(size_checked.IsValid());
    EXPECT_EQ(18u * 17u * 4, size_checked.ValueOrDie());
    auto pixel_format_and_modifier = PixelFormatAndModifierFromImageFormat(image_format);
    EXPECT_EQ(1u, ImageFormatSurfaceWidthMinDivisor(pixel_format_and_modifier));
    EXPECT_EQ(1u, ImageFormatSurfaceHeightMinDivisor(pixel_format_and_modifier));
    EXPECT_EQ(4, ImageFormatSampleAlignment(pixel_format_and_modifier));
  }
}

TEST(ImageFormat, A2R10G10B10_FormatsChecked_V2_wire) {
  for (const auto& pixel_format : {fuchsia_images2::wire::PixelFormat::kA2R10G10B10,
                                   fuchsia_images2::wire::PixelFormat::kA2B10G10R10}) {
    fidl::Arena allocator;
    auto constraints =
        sysmem_v2::wire::ImageFormatConstraints::Builder(allocator)
            .pixel_format(pixel_format)
            .pixel_format_modifier(fuchsia_images2::wire::PixelFormatModifier::kLinear)
            .min_size(fuchsia_math::wire::SizeU{12u, 12u})
            .max_size(fuchsia_math::wire::SizeU{100u, 100u})
            .bytes_per_row_divisor(1u)
            .max_bytes_per_row(100000u)
            .Build();

    auto optional_format = ImageConstraintsToFormat(allocator, constraints, 18, 17);
    EXPECT_TRUE(optional_format.is_ok());
    auto& image_format = optional_format.value();
    auto bytes_per_row_checked = ImageFormatMinimumRowBytesChecked(constraints, 18);
    EXPECT_TRUE(bytes_per_row_checked.IsValid());
    EXPECT_EQ(18u * 4, bytes_per_row_checked.ValueOrDie());
    auto size_checked = ImageFormatImageSizeChecked(image_format);
    EXPECT_TRUE(size_checked.IsValid());
    EXPECT_EQ(18u * 17u * 4, size_checked.ValueOrDie());
    auto pixel_format_and_modifier = PixelFormatAndModifierFromImageFormat(image_format);
    EXPECT_EQ(1u, ImageFormatSurfaceWidthMinDivisor(pixel_format_and_modifier));
    EXPECT_EQ(1u, ImageFormatSurfaceHeightMinDivisor(pixel_format_and_modifier));
    EXPECT_EQ(4, ImageFormatSampleAlignment(pixel_format_and_modifier));
  }
}

TEST(ImageFormat, A2R10G10B10_FormatsChecked_V1_wire) {
  for (const auto& pixel_format_type : {sysmem_v1::wire::PixelFormatType::kA2R10G10B10,
                                        sysmem_v1::wire::PixelFormatType::kA2B10G10R10}) {
    sysmem_v1::wire::PixelFormat format = {
        .type = pixel_format_type,
        .has_format_modifier = true,
        .format_modifier =
            {
                .value = sysmem_v1::wire::kFormatModifierLinear,
            },
    };

    sysmem_v1::wire::ImageFormatConstraints constraints = {
        .pixel_format = format,
        .min_coded_width = 12,
        .max_coded_width = 100,
        .min_coded_height = 12,
        .max_coded_height = 100,
        .max_bytes_per_row = 100000,
        .bytes_per_row_divisor = 1,
    };

    auto optional_format = ImageConstraintsToFormat(constraints, 18, 17);
    EXPECT_TRUE(optional_format);
    auto& image_format = optional_format.value();
    auto bytes_per_row_checked = ImageFormatMinimumRowBytesChecked(constraints, 18);
    EXPECT_TRUE(bytes_per_row_checked.IsValid());
    EXPECT_EQ(18u * 4, bytes_per_row_checked.ValueOrDie());
    auto size_checked = ImageFormatImageSizeChecked(image_format);
    EXPECT_TRUE(size_checked.IsValid());
    EXPECT_EQ(18u * 17u * 4, size_checked.ValueOrDie());
    EXPECT_EQ(1, ImageFormatCodedWidthMinDivisor(image_format.pixel_format));
    EXPECT_EQ(1, ImageFormatCodedHeightMinDivisor(image_format.pixel_format));
    EXPECT_EQ(4, ImageFormatSampleAlignment(image_format.pixel_format));
  }
}
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)

TEST(ImageFormat, GoldfishOptimal_V2) {
  constexpr uint32_t kWidth = 64;
  constexpr uint32_t kHeight = 128;
  constexpr uint32_t kStride = kWidth * 6;

  sysmem_v2::ImageFormatConstraints linear_constraints;
  linear_constraints.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  linear_constraints.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kLinear;
  linear_constraints.min_bytes_per_row() = kStride;
  auto linear_result = ImageConstraintsToFormat(linear_constraints, kWidth, kHeight);
  ASSERT_TRUE(linear_result.is_ok());
  auto linear_image_format_bgra32 = linear_result.take_value();

  sysmem_v2::ImageFormatConstraints goldfish_constraints;
  goldfish_constraints.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  goldfish_constraints.pixel_format_modifier() =
      fuchsia_images2::PixelFormatModifier::kGoogleGoldfishOptimal;
  goldfish_constraints.min_bytes_per_row() = kStride;
  auto goldfish_result = ImageConstraintsToFormat(goldfish_constraints, kWidth, kHeight);
  ASSERT_TRUE(goldfish_result.is_ok());
  auto goldfish_optimal_image_format_bgra32 = goldfish_result.take_value();

  EXPECT_EQ(kStride, goldfish_optimal_image_format_bgra32.bytes_per_row());
  EXPECT_EQ(ImageFormatImageSize(linear_image_format_bgra32),
            ImageFormatImageSize(goldfish_optimal_image_format_bgra32));
  auto pixel_format_and_modifier_linear_bgra32 =
      PixelFormatAndModifierFromImageFormat(linear_image_format_bgra32);
  auto pixel_format_and_modifier_goldfish_bgra32 =
      PixelFormatAndModifierFromImageFormat(goldfish_optimal_image_format_bgra32);
  EXPECT_EQ(ImageFormatSurfaceWidthMinDivisor(pixel_format_and_modifier_linear_bgra32),
            ImageFormatSurfaceWidthMinDivisor(pixel_format_and_modifier_goldfish_bgra32));
  EXPECT_EQ(ImageFormatSurfaceHeightMinDivisor(pixel_format_and_modifier_linear_bgra32),
            ImageFormatSurfaceHeightMinDivisor(pixel_format_and_modifier_goldfish_bgra32));
  EXPECT_EQ(ImageFormatSampleAlignment(pixel_format_and_modifier_linear_bgra32),
            ImageFormatSampleAlignment(pixel_format_and_modifier_goldfish_bgra32));

  // Multi-plane YUV formats on GOOGLE_GOLDFISH_OPTIMAL match LINEAR plane offsets and row bytes.
  for (auto yuv_format :
       {fuchsia_images2::PixelFormat::kNv12, fuchsia_images2::PixelFormat::kI420}) {
    SCOPED_TRACE(std::to_string(fidl::ToUnderlying(yuv_format)));
    linear_constraints.pixel_format() = yuv_format;
    goldfish_constraints.pixel_format() = yuv_format;
    auto linear_yuv_result = ImageConstraintsToFormat(linear_constraints, kWidth, kHeight);
    ASSERT_TRUE(linear_yuv_result.is_ok());
    auto linear_yuv = linear_yuv_result.take_value();
    auto goldfish_yuv_result = ImageConstraintsToFormat(goldfish_constraints, kWidth, kHeight);
    ASSERT_TRUE(goldfish_yuv_result.is_ok());
    auto goldfish_yuv = goldfish_yuv_result.take_value();
    EXPECT_EQ(ImageFormatImageSize(linear_yuv), ImageFormatImageSize(goldfish_yuv));
    for (uint32_t plane = 0; plane <= 3; ++plane) {
      SCOPED_TRACE("plane=" + std::to_string(plane));
      uint64_t linear_offset = 0, goldfish_offset = 0;
      uint32_t linear_row_bytes = 0, goldfish_row_bytes = 0;
      bool linear_has_offset = ImageFormatPlaneByteOffset(linear_yuv, plane, &linear_offset);
      bool goldfish_has_offset = ImageFormatPlaneByteOffset(goldfish_yuv, plane, &goldfish_offset);
      EXPECT_EQ(linear_has_offset, goldfish_has_offset);
      if (linear_has_offset) {
        EXPECT_EQ(linear_offset, goldfish_offset);
      }
      bool linear_has_row = ImageFormatPlaneRowBytes(linear_yuv, plane, &linear_row_bytes);
      bool goldfish_has_row = ImageFormatPlaneRowBytes(goldfish_yuv, plane, &goldfish_row_bytes);
      EXPECT_EQ(linear_has_row, goldfish_has_row);
      if (linear_has_row) {
        EXPECT_EQ(linear_row_bytes, goldfish_row_bytes);
      }
    }
  }
}

TEST(ImageFormat, GoldfishOptimal_V2_wire) {
  fidl::Arena allocator;
  constexpr uint32_t kWidth = 64;
  constexpr uint32_t kHeight = 128;
  constexpr uint32_t kStride = kWidth * 6;

  auto linear_constraints =
      sysmem_v2::wire::ImageFormatConstraints::Builder(allocator)
          .pixel_format(fuchsia_images2::wire::PixelFormat::kB8G8R8A8)
          .pixel_format_modifier(fuchsia_images2::wire::PixelFormatModifier::kLinear)
          .min_bytes_per_row(kStride)
          .Build();
  auto linear_result = ImageConstraintsToFormat(allocator, linear_constraints, kWidth, kHeight);
  ASSERT_TRUE(linear_result.is_ok());
  auto linear_image_format_bgra32 = linear_result.take_value();

  auto goldfish_constraints =
      sysmem_v2::wire::ImageFormatConstraints::Builder(allocator)
          .pixel_format(fuchsia_images2::wire::PixelFormat::kB8G8R8A8)
          .pixel_format_modifier(fuchsia_images2::wire::PixelFormatModifier::kGoogleGoldfishOptimal)
          .min_bytes_per_row(kStride)
          .Build();
  auto goldfish_result = ImageConstraintsToFormat(allocator, goldfish_constraints, kWidth, kHeight);
  ASSERT_TRUE(goldfish_result.is_ok());
  auto goldfish_optimal_image_format_bgra32 = goldfish_result.take_value();

  EXPECT_EQ(ImageFormatImageSize(linear_image_format_bgra32),
            ImageFormatImageSize(goldfish_optimal_image_format_bgra32));
  auto linear_pixel_format_and_modifier =
      PixelFormatAndModifierFromImageFormat(linear_image_format_bgra32);
  auto goldfish_optimal_pixel_format_and_modifier =
      PixelFormatAndModifierFromImageFormat(goldfish_optimal_image_format_bgra32);
  EXPECT_EQ(ImageFormatSurfaceWidthMinDivisor(linear_pixel_format_and_modifier),
            ImageFormatSurfaceWidthMinDivisor(goldfish_optimal_pixel_format_and_modifier));
  EXPECT_EQ(ImageFormatSurfaceHeightMinDivisor(linear_pixel_format_and_modifier),
            ImageFormatSurfaceHeightMinDivisor(goldfish_optimal_pixel_format_and_modifier));
  EXPECT_EQ(ImageFormatSampleAlignment(linear_pixel_format_and_modifier),
            ImageFormatSampleAlignment(goldfish_optimal_pixel_format_and_modifier));
}

TEST(ImageFormat, GoldfishOptimal_V1_wire) {
  constexpr uint32_t kWidth = 64;
  constexpr uint32_t kHeight = 128;
  constexpr uint32_t kStride = kWidth * 6;

  sysmem_v1::wire::ImageFormatConstraints linear_constraints = {
      .pixel_format =
          {
              .type = sysmem_v1::wire::PixelFormatType::kBgra32,
              .has_format_modifier = true,
              .format_modifier = {.value = sysmem_v1::wire::kFormatModifierLinear},
          },
      .min_bytes_per_row = kStride,
  };
  auto linear_result = ImageConstraintsToFormat(linear_constraints, kWidth, kHeight);
  ASSERT_TRUE(linear_result.is_ok());
  auto linear_image_format_bgra32 = linear_result.take_value();

  sysmem_v1::wire::ImageFormatConstraints goldfish_constraints = {
      .pixel_format =
          {
              .type = sysmem_v1::wire::PixelFormatType::kBgra32,
              .has_format_modifier = true,
              .format_modifier = {.value = sysmem_v1::wire::kFormatModifierGoogleGoldfishOptimal},
          },
      .min_bytes_per_row = kStride,
  };
  auto goldfish_result = ImageConstraintsToFormat(goldfish_constraints, kWidth, kHeight);
  ASSERT_TRUE(goldfish_result.is_ok());
  auto goldfish_optimal_image_format_bgra32 = goldfish_result.take_value();

  EXPECT_EQ(ImageFormatImageSize(linear_image_format_bgra32),
            ImageFormatImageSize(goldfish_optimal_image_format_bgra32));
  EXPECT_EQ(ImageFormatCodedWidthMinDivisor(linear_image_format_bgra32.pixel_format),
            ImageFormatCodedWidthMinDivisor(goldfish_optimal_image_format_bgra32.pixel_format));
  EXPECT_EQ(ImageFormatCodedHeightMinDivisor(linear_image_format_bgra32.pixel_format),
            ImageFormatCodedHeightMinDivisor(goldfish_optimal_image_format_bgra32.pixel_format));
  EXPECT_EQ(ImageFormatSampleAlignment(linear_image_format_bgra32.pixel_format),
            ImageFormatSampleAlignment(goldfish_optimal_image_format_bgra32.pixel_format));
}

#if FUCHSIA_API_LEVEL_AT_LEAST(32)
TEST(ImageFormat, GoldfishOptimalChecked_V2) {
  constexpr uint32_t kWidth = 64;
  constexpr uint32_t kHeight = 128;
  constexpr uint32_t kStride = kWidth * 6;

  sysmem_v2::ImageFormatConstraints linear_constraints;
  linear_constraints.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  linear_constraints.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kLinear;
  linear_constraints.min_bytes_per_row() = kStride;
  auto linear_result = ImageConstraintsToFormat(linear_constraints, kWidth, kHeight);
  ASSERT_TRUE(linear_result.is_ok());
  auto linear_image_format_bgra32 = linear_result.take_value();

  sysmem_v2::ImageFormatConstraints goldfish_constraints;
  goldfish_constraints.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  goldfish_constraints.pixel_format_modifier() =
      fuchsia_images2::PixelFormatModifier::kGoogleGoldfishOptimal;
  goldfish_constraints.min_bytes_per_row() = kStride;
  auto goldfish_result = ImageConstraintsToFormat(goldfish_constraints, kWidth, kHeight);
  ASSERT_TRUE(goldfish_result.is_ok());
  auto goldfish_optimal_image_format_bgra32 = goldfish_result.take_value();

  auto linear_size = ImageFormatImageSizeChecked(linear_image_format_bgra32);
  EXPECT_TRUE(linear_size.IsValid());
  auto goldfish_size = ImageFormatImageSizeChecked(goldfish_optimal_image_format_bgra32);
  EXPECT_TRUE(goldfish_size.IsValid());
  EXPECT_EQ(linear_size.ValueOrDie(), goldfish_size.ValueOrDie());

  auto pixel_format_and_modifier_linear_bgra32 =
      PixelFormatAndModifierFromImageFormat(linear_image_format_bgra32);
  auto pixel_format_and_modifier_goldfish_bgra32 =
      PixelFormatAndModifierFromImageFormat(goldfish_optimal_image_format_bgra32);
  EXPECT_EQ(ImageFormatSurfaceWidthMinDivisor(pixel_format_and_modifier_linear_bgra32),
            ImageFormatSurfaceWidthMinDivisor(pixel_format_and_modifier_goldfish_bgra32));
  EXPECT_EQ(ImageFormatSurfaceHeightMinDivisor(pixel_format_and_modifier_linear_bgra32),
            ImageFormatSurfaceHeightMinDivisor(pixel_format_and_modifier_goldfish_bgra32));
  EXPECT_EQ(ImageFormatSampleAlignment(pixel_format_and_modifier_linear_bgra32),
            ImageFormatSampleAlignment(pixel_format_and_modifier_goldfish_bgra32));
}

TEST(ImageFormat, GoldfishOptimalChecked_V2_wire) {
  fidl::Arena allocator;
  constexpr uint32_t kWidth = 64;
  constexpr uint32_t kHeight = 128;
  constexpr uint32_t kStride = kWidth * 6;

  auto linear_constraints =
      sysmem_v2::wire::ImageFormatConstraints::Builder(allocator)
          .pixel_format(fuchsia_images2::wire::PixelFormat::kB8G8R8A8)
          .pixel_format_modifier(fuchsia_images2::wire::PixelFormatModifier::kLinear)
          .min_bytes_per_row(kStride)
          .Build();
  auto linear_result = ImageConstraintsToFormat(allocator, linear_constraints, kWidth, kHeight);
  ASSERT_TRUE(linear_result.is_ok());
  auto linear_image_format_bgra32 = linear_result.take_value();

  auto goldfish_constraints =
      sysmem_v2::wire::ImageFormatConstraints::Builder(allocator)
          .pixel_format(fuchsia_images2::wire::PixelFormat::kB8G8R8A8)
          .pixel_format_modifier(fuchsia_images2::wire::PixelFormatModifier::kGoogleGoldfishOptimal)
          .min_bytes_per_row(kStride)
          .Build();
  auto goldfish_result = ImageConstraintsToFormat(allocator, goldfish_constraints, kWidth, kHeight);
  ASSERT_TRUE(goldfish_result.is_ok());
  auto goldfish_optimal_image_format_bgra32 = goldfish_result.take_value();

  auto linear_size = ImageFormatImageSizeChecked(linear_image_format_bgra32);
  EXPECT_TRUE(linear_size.IsValid());
  auto goldfish_size = ImageFormatImageSizeChecked(goldfish_optimal_image_format_bgra32);
  EXPECT_TRUE(goldfish_size.IsValid());
  EXPECT_EQ(linear_size.ValueOrDie(), goldfish_size.ValueOrDie());

  auto linear_pixel_format_and_modifier =
      PixelFormatAndModifierFromImageFormat(linear_image_format_bgra32);
  auto goldfish_optimal_pixel_format_and_modifier =
      PixelFormatAndModifierFromImageFormat(goldfish_optimal_image_format_bgra32);
  EXPECT_EQ(ImageFormatSurfaceWidthMinDivisor(linear_pixel_format_and_modifier),
            ImageFormatSurfaceWidthMinDivisor(goldfish_optimal_pixel_format_and_modifier));
  EXPECT_EQ(ImageFormatSurfaceHeightMinDivisor(linear_pixel_format_and_modifier),
            ImageFormatSurfaceHeightMinDivisor(goldfish_optimal_pixel_format_and_modifier));
  EXPECT_EQ(ImageFormatSampleAlignment(linear_pixel_format_and_modifier),
            ImageFormatSampleAlignment(goldfish_optimal_pixel_format_and_modifier));
}

TEST(ImageFormat, GoldfishOptimalChecked_V1_wire) {
  constexpr uint32_t kWidth = 64;
  constexpr uint32_t kHeight = 128;
  constexpr uint32_t kStride = kWidth * 6;

  sysmem_v1::wire::ImageFormatConstraints linear_constraints = {
      .pixel_format =
          {
              .type = sysmem_v1::wire::PixelFormatType::kBgra32,
              .has_format_modifier = true,
              .format_modifier = {.value = sysmem_v1::wire::kFormatModifierLinear},
          },
      .min_bytes_per_row = kStride,
  };
  auto linear_result = ImageConstraintsToFormat(linear_constraints, kWidth, kHeight);
  ASSERT_TRUE(linear_result.is_ok());
  auto linear_image_format_bgra32 = linear_result.take_value();

  sysmem_v1::wire::ImageFormatConstraints goldfish_constraints = {
      .pixel_format =
          {
              .type = sysmem_v1::wire::PixelFormatType::kBgra32,
              .has_format_modifier = true,
              .format_modifier = {.value = sysmem_v1::wire::kFormatModifierGoogleGoldfishOptimal},
          },
      .min_bytes_per_row = kStride,
  };
  auto goldfish_result = ImageConstraintsToFormat(goldfish_constraints, kWidth, kHeight);
  ASSERT_TRUE(goldfish_result.is_ok());
  auto goldfish_optimal_image_format_bgra32 = goldfish_result.take_value();

  auto linear_size = ImageFormatImageSizeChecked(linear_image_format_bgra32);
  EXPECT_TRUE(linear_size.IsValid());
  auto goldfish_size = ImageFormatImageSizeChecked(goldfish_optimal_image_format_bgra32);
  EXPECT_TRUE(goldfish_size.IsValid());
  EXPECT_EQ(linear_size.ValueOrDie(), goldfish_size.ValueOrDie());

  EXPECT_EQ(ImageFormatCodedWidthMinDivisor(linear_image_format_bgra32.pixel_format),
            ImageFormatCodedWidthMinDivisor(goldfish_optimal_image_format_bgra32.pixel_format));
  EXPECT_EQ(ImageFormatCodedHeightMinDivisor(linear_image_format_bgra32.pixel_format),
            ImageFormatCodedHeightMinDivisor(goldfish_optimal_image_format_bgra32.pixel_format));
  EXPECT_EQ(ImageFormatSampleAlignment(linear_image_format_bgra32.pixel_format),
            ImageFormatSampleAlignment(goldfish_optimal_image_format_bgra32.pixel_format));
}
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)

TEST(ImageFormat, CorrectModifiers) {
  EXPECT_EQ(sysmem_v1::kFormatModifierArmAfbc16X16YuvTiledHeader,
            sysmem_v1::kFormatModifierArmAfbc16X16YuvTiledHeader);
  EXPECT_EQ(sysmem_v1::kFormatModifierArmAfbc16X16YuvTiledHeader,
            sysmem_v1::kFormatModifierArmAfbc16X16 | sysmem_v1::kFormatModifierArmYuvBit |
                sysmem_v1::kFormatModifierArmTiledHeaderBit);
  // V2 changes the value to be less likely to collide.
  EXPECT_NE(sysmem_v1::kFormatModifierGoogleGoldfishOptimal,
            fidl::ToUnderlying(fuchsia_images2::PixelFormatModifier::kGoogleGoldfishOptimal));
}

TEST(ImageFormat, CorrectModifiers_wire) {
  EXPECT_EQ(sysmem_v1::wire::kFormatModifierArmAfbc16X16YuvTiledHeader,
            sysmem_v1::wire::kFormatModifierArmAfbc16X16YuvTiledHeader);
  EXPECT_EQ(sysmem_v1::wire::kFormatModifierArmAfbc16X16YuvTiledHeader,
            sysmem_v1::wire::kFormatModifierArmAfbc16X16 |
                sysmem_v1::wire::kFormatModifierArmYuvBit |
                sysmem_v1::wire::kFormatModifierArmTiledHeaderBit);
  // V2 changes the value to be less likely to collide.
  EXPECT_NE(sysmem_v1::wire::kFormatModifierGoogleGoldfishOptimal,
            fidl::ToUnderlying(fuchsia_images2::wire::PixelFormatModifier::kGoogleGoldfishOptimal));
}

TEST(ImageFormat, RoundUpWidthForCallers) {
  sysmem_v2::ImageFormatConstraints constraints;
  constraints.pixel_format() = fuchsia_images2::PixelFormat::kNv12;
  constraints.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kLinear;
  constraints.min_size() = {12u, 1u};
  constraints.max_size() = {100u, 0xFFFFFFFF};

  // Later we specify a width that isn't already aligned to size_alignment.width.
  constraints.size_alignment() = {8, 1};

  // Ensure that ImageFormatMinimumRowBytes rounds up for the caller.
  uint32_t minimum_row_bytes;
  bool result = ImageFormatMinimumRowBytes(constraints, 17, &minimum_row_bytes);
  ASSERT_TRUE(result);
  // Lowest value that's 17 or larger and divisible by 8.
  EXPECT_EQ(24, minimum_row_bytes);
}

#if FUCHSIA_API_LEVEL_AT_LEAST(32)
TEST(ImageFormat, RoundUpWidthForCallersChecked) {
  sysmem_v2::ImageFormatConstraints constraints;
  constraints.pixel_format() = fuchsia_images2::PixelFormat::kNv12;
  constraints.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kLinear;
  constraints.min_size() = {12u, 1u};
  constraints.max_size() = {100u, 0xFFFFFFFF};

  // Later we specify a width that isn't already aligned to size_alignment.width.
  constraints.size_alignment() = {8, 1};

  // Ensure that ImageFormatMinimumRowBytesChecked rounds up for the caller.
  auto minimum_row_bytes = ImageFormatMinimumRowBytesChecked(constraints, 17);
  ASSERT_TRUE(minimum_row_bytes.IsValid());
  // Lowest value that's 17 or larger and divisible by 8.
  EXPECT_EQ(24u, minimum_row_bytes.ValueOrDie());
}

TEST(ImageFormat, RoundUpNearLimitChecked) {
  sysmem_v2::ImageFormatConstraints constraints;
  constraints.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  constraints.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kLinear;
  constraints.min_size() = {1u, 1u};
  constraints.max_size() = {0xFFFFFFFF, 0xFFFFFFFF};
  constraints.bytes_per_row_divisor() = 344064;

  // Stride is 4 bytes for B8G8R8A8.
  // 1073737728 * 4 = 4294950912 (0xFFFC0000), which is a multiple of 344064.
  // Previously CheckRoundUp computed val + multiple - 1, which was
  // 4294950912 + 344063 = 4295294975 > 0xFFFFFFFF, causing a false overflow.
  constexpr uint32_t kAlignedWidth = 1073737728;
  auto aligned_row_bytes = ImageFormatMinimumRowBytesChecked(constraints, kAlignedWidth);
  ASSERT_TRUE(aligned_row_bytes.IsValid());
  EXPECT_EQ(4294950912u, aligned_row_bytes.ValueOrDie());

  // Non-aligned width that rounds up to 4294950912.
  constexpr uint32_t kNonAlignedWidth = 1073737727;
  auto non_aligned_row_bytes = ImageFormatMinimumRowBytesChecked(constraints, kNonAlignedWidth);
  ASSERT_TRUE(non_aligned_row_bytes.IsValid());
  EXPECT_EQ(4294950912u, non_aligned_row_bytes.ValueOrDie());

  // Width that actually overflows when rounded up.
  constexpr uint32_t kOverflowWidth = 1073737729;
  auto overflow_row_bytes = ImageFormatMinimumRowBytesChecked(constraints, kOverflowWidth);
  EXPECT_FALSE(overflow_row_bytes.IsValid());
}
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)

#if FUCHSIA_API_LEVEL_AT_LEAST(30)
TEST(ImageFormat, IsNonTiledSinglePlaneTest) {
  PixelFormatAndModifier p010(fuchsia_images2::PixelFormat::kP010,
                              fuchsia_images2::PixelFormatModifier::kLinear);
  EXPECT_FALSE(ImageFormatIsNonTiledSinglePlane(p010));

  PixelFormatAndModifier yuy2(fuchsia_images2::PixelFormat::kYuy2,
                              fuchsia_images2::PixelFormatModifier::kLinear);
  EXPECT_TRUE(ImageFormatIsNonTiledSinglePlane(yuy2));

  PixelFormatAndModifier bgra32(fuchsia_images2::PixelFormat::kB8G8R8A8,
                                fuchsia_images2::PixelFormatModifier::kLinear);
  EXPECT_TRUE(ImageFormatIsNonTiledSinglePlane(bgra32));
}
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(30)

// The image-format asserts are enabled in all build types, so misuse crashes consistently instead
// of returning a meaningless value in release builds.
TEST(ImageFormat, UnsupportedPixelFormatPropertiesCrash) {
  PixelFormatAndModifier unsupported(fuchsia_images2::PixelFormat::kB8G8R8A8,
                                     fuchsia_images2::PixelFormatModifier::kDoNotCare);
  ASSERT_FALSE(ImageFormatIsSupported(unsupported));
  ASSERT_DEATH([&] { ImageFormatBitsPerPixel(unsupported); });
  ASSERT_DEATH([&] { ImageFormatStrideBytesPerWidthPixel(unsupported); });
  ASSERT_DEATH([&] { ImageFormatSurfaceWidthMinDivisor(unsupported); });
  ASSERT_DEATH([&] { ImageFormatSurfaceHeightMinDivisor(unsupported); });
  ASSERT_DEATH([&] { ImageFormatSampleAlignment(unsupported); });
#if FUCHSIA_API_LEVEL_AT_LEAST(NEXT)
  ASSERT_DEATH([&] { ImageFormatRowBytesMinDivisor(unsupported); });

  sysmem_v1::wire::PixelFormat unsupported_v1 = {
      .type = sysmem_v1::wire::PixelFormatType::kBgra32,
      .has_format_modifier = true,
      .format_modifier = {.value = sysmem_v1::wire::kFormatModifierInvalid},
  };
  ASSERT_FALSE(ImageFormatIsSupported(unsupported_v1));
  ASSERT_DEATH([&] { ImageFormatRowBytesMinDivisor(unsupported_v1); });
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(NEXT)
}

TEST(ImageFormat, LinearImageSizeWithoutBytesPerRowCrashes) {
  fuchsia_images2::ImageFormat image_format;
  image_format.pixel_format() = fuchsia_images2::PixelFormat::kNv12;
  image_format.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kLinear;
  image_format.size() = {64u, 64u};
  ASSERT_DEATH([&] { ImageFormatImageSize(image_format); });
}

#if FUCHSIA_API_LEVEL_AT_LEAST(33)
TEST(ImageFormat, PvricFormats_V2) {
  EXPECT_EQ(fuchsia_images2::kFormatModifierVendorPvr, 0x9200000000000000ul);
  EXPECT_EQ(fidl::ToUnderlying(fuchsia_images2::PixelFormatModifier::kPvrFbcdc8X8V14),
            0x9200000000000022ul);
  EXPECT_EQ(fidl::ToUnderlying(fuchsia_images2::PixelFormatModifier::kPvrFbcdc16X4V14),
            0x9200000000000027ul);

  const fuchsia_images2::PixelFormat kSupported32BppFormats[] = {
      fuchsia_images2::PixelFormat::kR8G8B8A8,    fuchsia_images2::PixelFormat::kR8G8B8X8,
      fuchsia_images2::PixelFormat::kB8G8R8A8,    fuchsia_images2::PixelFormat::kB8G8R8X8,
      fuchsia_images2::PixelFormat::kA2R10G10B10, fuchsia_images2::PixelFormat::kA2B10G10R10,
  };
  const fuchsia_images2::PixelFormatModifier kPvricModifiers[] = {
      fuchsia_images2::PixelFormatModifier::kPvrFbcdc8X8V14,
      fuchsia_images2::PixelFormatModifier::kPvrFbcdc16X4V14,
  };

  for (auto modifier : kPvricModifiers) {
    for (auto format : kSupported32BppFormats) {
      PixelFormatAndModifier pfm(format, modifier);
      EXPECT_TRUE(ImageFormatIsSupported(pfm));
      EXPECT_FALSE(ImageFormatIsNonTiledSinglePlane(pfm));
      EXPECT_TRUE(ImageFormatCompatibleWithProtectedMemory(pfm));
      EXPECT_EQ(32u, ImageFormatBitsPerPixel(pfm));
      EXPECT_EQ(4u, ImageFormatStrideBytesPerWidthPixel(pfm));
      EXPECT_EQ(1u, ImageFormatSurfaceWidthMinDivisor(pfm));
      EXPECT_EQ(1u, ImageFormatSurfaceHeightMinDivisor(pfm));
      EXPECT_EQ(4u, ImageFormatSampleAlignment(pfm));
    }

    PixelFormatAndModifier rgb565(fuchsia_images2::PixelFormat::kR5G6B5, modifier);
    EXPECT_TRUE(ImageFormatIsSupported(rgb565));
    EXPECT_FALSE(ImageFormatIsNonTiledSinglePlane(rgb565));
    EXPECT_TRUE(ImageFormatCompatibleWithProtectedMemory(rgb565));
    EXPECT_EQ(16u, ImageFormatBitsPerPixel(rgb565));
    EXPECT_EQ(2u, ImageFormatStrideBytesPerWidthPixel(rgb565));
    EXPECT_EQ(1u, ImageFormatSurfaceWidthMinDivisor(rgb565));
    EXPECT_EQ(1u, ImageFormatSurfaceHeightMinDivisor(rgb565));
    EXPECT_EQ(2u, ImageFormatSampleAlignment(rgb565));

    EXPECT_FALSE(ImageFormatIsSupported(
        PixelFormatAndModifier(fuchsia_images2::PixelFormat::kNv12, modifier)));
    EXPECT_FALSE(ImageFormatIsSupported(
        PixelFormatAndModifier(fuchsia_images2::PixelFormat::kB8G8R8, modifier)));
  }

  // Test 16x4 V14 with 32-bpp (bytes_per_row_per_tile = 64, tile_rows = 4).
  // For 18x17 with divisor 32: lcm(32, 64) = 64. 18 * 4 = 72 -> rounds up to 128 bytes/row.
  // width_in_tiles = 128 / 64 = 2.
  // height_in_tiles = ceil(17 / 4) = 5.
  // num_tiles = 2 * 5 = 10.
  // header_size = RoundUp(10, 256) = 256.
  // data_size = 10 * 256 = 2560.
  // total_size = 256 + 2560 = 2816.
  {
    sysmem_v2::ImageFormatConstraints constraints;
    constraints.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
    constraints.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kPvrFbcdc16X4V14;
    constraints.min_size() = {12u, 12u};
    constraints.max_size() = {100u, 100u};
    constraints.bytes_per_row_divisor() = 32u;
    constraints.max_bytes_per_row() = 100000u;

    auto image_format_result = ImageConstraintsToFormat(constraints, 18, 17);
    ASSERT_TRUE(image_format_result.is_ok());
    auto image_format = image_format_result.take_value();
    EXPECT_EQ(128u, image_format.bytes_per_row().value());
    EXPECT_EQ(2816u, ImageFormatImageSize(image_format));

    auto size_checked = ImageFormatImageSizeChecked(image_format);
    ASSERT_TRUE(size_checked.IsValid());
    EXPECT_EQ(2816u, size_checked.ValueOrDie());

    uint64_t plane_offset = 0;
    EXPECT_TRUE(ImageFormatPlaneByteOffset(image_format, 0, &plane_offset));
    EXPECT_EQ(256u, plane_offset);
    EXPECT_FALSE(ImageFormatPlaneByteOffset(image_format, 1, &plane_offset));
    EXPECT_FALSE(ImageFormatPlaneByteOffset(image_format, 2, &plane_offset));
    EXPECT_TRUE(ImageFormatPlaneByteOffset(image_format, 3, &plane_offset));
    EXPECT_EQ(0u, plane_offset);
    EXPECT_FALSE(ImageFormatPlaneByteOffset(image_format, 4, &plane_offset));

    EXPECT_EQ(256u, ImageFormatPlaneByteOffsetChecked(image_format, 0).ValueOrDie());
    EXPECT_FALSE(ImageFormatPlaneByteOffsetChecked(image_format, 1).IsValid());
    EXPECT_FALSE(ImageFormatPlaneByteOffsetChecked(image_format, 2).IsValid());
    EXPECT_EQ(0u, ImageFormatPlaneByteOffsetChecked(image_format, 3).ValueOrDie());
    EXPECT_FALSE(ImageFormatPlaneByteOffsetChecked(image_format, 4).IsValid());

    uint32_t row_bytes = 0;
    EXPECT_TRUE(ImageFormatPlaneRowBytes(image_format, 0, &row_bytes));
    EXPECT_EQ(128u, row_bytes);
    EXPECT_FALSE(ImageFormatPlaneRowBytes(image_format, 1, &row_bytes));
    EXPECT_FALSE(ImageFormatPlaneRowBytes(image_format, 2, &row_bytes));
    EXPECT_TRUE(ImageFormatPlaneRowBytes(image_format, 3, &row_bytes));
    EXPECT_EQ(2u, row_bytes);
    EXPECT_FALSE(ImageFormatPlaneRowBytes(image_format, 4, &row_bytes));

    EXPECT_EQ(128u, ImageFormatPlaneRowBytesChecked(image_format, 0).ValueOrDie());
    EXPECT_FALSE(ImageFormatPlaneRowBytesChecked(image_format, 1).IsValid());
    EXPECT_FALSE(ImageFormatPlaneRowBytesChecked(image_format, 2).IsValid());
    EXPECT_EQ(2u, ImageFormatPlaneRowBytesChecked(image_format, 3).ValueOrDie());
    EXPECT_FALSE(ImageFormatPlaneRowBytesChecked(image_format, 4).IsValid());
  }

  // Test 8x8 V14 with 32-bpp at 1920x1080 (bytes_per_row_per_tile = 32, tile_rows = 8).
  // bytes_per_row = 1920 * 4 = 7680.
  // width_in_tiles = 7680 / 32 = 240.
  // height_in_tiles = 1080 / 8 = 135.
  // num_tiles = 240 * 135 = 32400.
  // header_size = RoundUp(32400, 256) = 32512.
  // data_size = 32400 * 256 = 8294400.
  // total_size = 32512 + 8294400 = 8326912.
  {
    sysmem_v2::ImageFormatConstraints constraints;
    constraints.pixel_format() = fuchsia_images2::PixelFormat::kR8G8B8A8;
    constraints.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kPvrFbcdc8X8V14;
    constraints.min_size() = {1u, 1u};
    constraints.max_size() = {4096u, 4096u};

    auto image_format_result = ImageConstraintsToFormat(constraints, 1920, 1080);
    ASSERT_TRUE(image_format_result.is_ok());
    auto image_format = image_format_result.take_value();
    EXPECT_EQ(7680u, image_format.bytes_per_row().value());
    EXPECT_EQ(8326912u, ImageFormatImageSize(image_format));
    EXPECT_EQ(32512u, ImageFormatPlaneByteOffsetChecked(image_format, 0).ValueOrDie());
    EXPECT_EQ(0u, ImageFormatPlaneByteOffsetChecked(image_format, 3).ValueOrDie());
    EXPECT_EQ(7680u, ImageFormatPlaneRowBytesChecked(image_format, 0).ValueOrDie());
    EXPECT_EQ(240u, ImageFormatPlaneRowBytesChecked(image_format, 3).ValueOrDie());
  }

  // Test 16x4 V14 with 16-bpp R5G6B5 (bytes_per_row_per_tile = 64, tile_rows = 4).
  // For width = 18 (36 bytes): rounds up to 64 bytes/row (1 tile wide = 32 pixels).
  // height = 17: ceil(17 / 4) = 5 tiles high.
  // num_tiles = 1 * 5 = 5.
  // header_size = 256, data_size = 1280, total_size = 1536.
  {
    sysmem_v2::ImageFormatConstraints constraints;
    constraints.pixel_format() = fuchsia_images2::PixelFormat::kR5G6B5;
    constraints.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kPvrFbcdc16X4V14;
    constraints.min_size() = {1u, 1u};
    constraints.max_size() = {4096u, 4096u};

    auto image_format_result = ImageConstraintsToFormat(constraints, 18, 17);
    ASSERT_TRUE(image_format_result.is_ok());
    auto image_format = image_format_result.take_value();
    EXPECT_EQ(64u, image_format.bytes_per_row().value());
    EXPECT_EQ(1536u, ImageFormatImageSize(image_format));
    EXPECT_EQ(256u, ImageFormatPlaneByteOffsetChecked(image_format, 0).ValueOrDie());
    EXPECT_EQ(0u, ImageFormatPlaneByteOffsetChecked(image_format, 3).ValueOrDie());
    EXPECT_EQ(64u, ImageFormatPlaneRowBytesChecked(image_format, 0).ValueOrDie());
    EXPECT_EQ(1u, ImageFormatPlaneRowBytesChecked(image_format, 3).ValueOrDie());
  }
}

TEST(ImageFormat, PvricFormats_V2_wire) {
  fidl::Arena allocator;
  auto constraints =
      sysmem_v2::wire::ImageFormatConstraints::Builder(allocator)
          .pixel_format(fuchsia_images2::wire::PixelFormat::kB8G8R8A8)
          .pixel_format_modifier(fuchsia_images2::wire::PixelFormatModifier::kPvrFbcdc16X4V14)
          .min_size(fuchsia_math::wire::SizeU{12u, 12u})
          .max_size(fuchsia_math::wire::SizeU{100u, 100u})
          .bytes_per_row_divisor(32u)
          .max_bytes_per_row(100000u)
          .Build();

  auto image_format_result = ImageConstraintsToFormat(allocator, constraints, 18, 17);
  ASSERT_TRUE(image_format_result.is_ok());
  auto image_format = image_format_result.take_value();
  EXPECT_EQ(128u, image_format.bytes_per_row());
  EXPECT_EQ(2816u, ImageFormatImageSize(image_format));
  EXPECT_EQ(2816u, ImageFormatImageSizeChecked(image_format).ValueOrDie());
  EXPECT_EQ(256u, ImageFormatPlaneByteOffsetChecked(image_format, 0).ValueOrDie());
  EXPECT_EQ(0u, ImageFormatPlaneByteOffsetChecked(image_format, 3).ValueOrDie());
  EXPECT_EQ(128u, ImageFormatPlaneRowBytesChecked(image_format, 0).ValueOrDie());
  EXPECT_EQ(2u, ImageFormatPlaneRowBytesChecked(image_format, 3).ValueOrDie());
}

TEST(ImageFormat, PvricFormats_V1_wire) {
  sysmem_v1::wire::PixelFormat format = {
      .type = sysmem_v1::wire::PixelFormatType::kBgra32,
      .has_format_modifier = true,
      .format_modifier =
          {
              .value = fidl::ToUnderlying(fuchsia_images2::PixelFormatModifier::kPvrFbcdc16X4V14),
          },
  };
  EXPECT_TRUE(ImageFormatIsSupported(format));
  EXPECT_TRUE(ImageFormatCompatibleWithProtectedMemory(format));

  sysmem_v1::wire::ImageFormatConstraints constraints = {
      .pixel_format = format,
      .min_coded_width = 12,
      .max_coded_width = 100,
      .min_coded_height = 12,
      .max_coded_height = 100,
      .max_bytes_per_row = 100000,
      .bytes_per_row_divisor = 32,
  };

  auto optional_format = ImageConstraintsToFormat(constraints, 18, 17);
  ASSERT_TRUE(optional_format.is_ok());
  auto& image_format = optional_format.value();
  EXPECT_EQ(128u, image_format.bytes_per_row);
  EXPECT_EQ(2816u, ImageFormatImageSize(image_format));
  EXPECT_EQ(2816u, ImageFormatImageSizeChecked(image_format).ValueOrDie());
  EXPECT_EQ(256u, ImageFormatPlaneByteOffsetChecked(image_format, 0).ValueOrDie());
  EXPECT_EQ(0u, ImageFormatPlaneByteOffsetChecked(image_format, 3).ValueOrDie());
  EXPECT_EQ(128u, ImageFormatPlaneRowBytesChecked(image_format, 0).ValueOrDie());
  EXPECT_EQ(2u, ImageFormatPlaneRowBytesChecked(image_format, 3).ValueOrDie());
}
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(33)

TEST(ImageFormat, ImageConstraintsToFormatDefaultsModifierToLinear) {
  sysmem_v2::ImageFormatConstraints constraints;
  constraints.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  ASSERT_FALSE(constraints.pixel_format_modifier().has_value());

  auto result = ImageConstraintsToFormat(constraints, 64u, 32u);
  ASSERT_TRUE(result.is_ok());
  auto image_format = result.take_value();
  EXPECT_EQ(fuchsia_images2::PixelFormat::kB8G8R8A8, image_format.pixel_format());
  EXPECT_EQ(fuchsia_images2::PixelFormatModifier::kLinear, image_format.pixel_format_modifier());
  EXPECT_EQ(64u * 4, image_format.bytes_per_row());
}

TEST(ImageFormat, ImageConstraintsToFormatZeroSizeAlignmentFails) {
  sysmem_v2::ImageFormatConstraints constraints;
  constraints.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  constraints.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kLinear;

  constraints.size_alignment() = {0u, 1u};
  EXPECT_FALSE(ImageConstraintsToFormat(constraints, 64u, 32u).is_ok());
  constraints.size_alignment() = {1u, 0u};
  EXPECT_FALSE(ImageConstraintsToFormat(constraints, 64u, 32u).is_ok());
  constraints.size_alignment() = {1u, 1u};
  EXPECT_TRUE(ImageConstraintsToFormat(constraints, 64u, 32u).is_ok());
}

TEST(ImageFormat, AfbcTePlaneOnlyWithTeModifier) {
  sysmem_v2::ImageFormatConstraints constraints;
  constraints.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;

  uint64_t offset;
  uint32_t row_bytes;
  constraints.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kArmAfbc16X16;
  auto non_te = ImageConstraintsToFormat(constraints, 1920u, 1080u).take_value();
  EXPECT_FALSE(ImageFormatPlaneByteOffset(non_te, 3, &offset));
  EXPECT_FALSE(ImageFormatPlaneRowBytes(non_te, 3, &row_bytes));

  constraints.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kArmAfbc16X16Te;
  auto with_te = ImageConstraintsToFormat(constraints, 1920u, 1080u).take_value();
  ASSERT_TRUE(ImageFormatPlaneByteOffset(with_te, 3, &offset));
  EXPECT_LT(offset, ImageFormatImageSize(with_te));
  EXPECT_TRUE(ImageFormatPlaneRowBytes(with_te, 3, &row_bytes));
}

TEST(ImageFormat, GoldfishOptimalSupportsOnlyLinearPixelFormats) {
  // Not a defined fuchsia_images2::PixelFormat enum member; exercises default: branches.
  constexpr auto kUnknownPixelFormat = static_cast<fuchsia_images2::PixelFormat>(12345);

  struct TestCase {
    fuchsia_images2::PixelFormat pixel_format;
    const char* name;
    bool supported;
  };
  constexpr TestCase kTestCases[] = {
      {fuchsia_images2::PixelFormat::kInvalid, "INVALID", false},
      {fuchsia_images2::PixelFormat::kR8G8B8A8, "R8G8B8A8", true},
      {fuchsia_images2::PixelFormat::kR8G8B8X8, "R8G8B8X8", true},
      {fuchsia_images2::PixelFormat::kB8G8R8A8, "B8G8R8A8", true},
      {fuchsia_images2::PixelFormat::kB8G8R8X8, "B8G8R8X8", true},
      {fuchsia_images2::PixelFormat::kI420, "I420", true},
      {fuchsia_images2::PixelFormat::kM420, "M420", true},
      {fuchsia_images2::PixelFormat::kNv12, "NV12", true},
      {fuchsia_images2::PixelFormat::kYuy2, "YUY2", true},
      {fuchsia_images2::PixelFormat::kMjpeg, "MJPEG", false},
      {fuchsia_images2::PixelFormat::kYv12, "YV12", true},
      {fuchsia_images2::PixelFormat::kB8G8R8, "B8G8R8", true},
      {fuchsia_images2::PixelFormat::kR5G6B5, "R5G6B5", true},
      {fuchsia_images2::PixelFormat::kR3G3B2, "R3G3B2", true},
      {fuchsia_images2::PixelFormat::kR2G2B2X2, "R2G2B2X2", true},
      {fuchsia_images2::PixelFormat::kL8, "L8", true},
      {fuchsia_images2::PixelFormat::kR8, "R8", true},
      {fuchsia_images2::PixelFormat::kR8G8, "R8G8", true},
      {fuchsia_images2::PixelFormat::kA2R10G10B10, "A2R10G10B10", true},
      {fuchsia_images2::PixelFormat::kA2B10G10R10, "A2B10G10R10", true},
      {fuchsia_images2::PixelFormat::kP010, "P010", true},
      {fuchsia_images2::PixelFormat::kR8G8B8, "R8G8B8", true},
      {fuchsia_images2::PixelFormat::kDoNotCare, "DO_NOT_CARE", false},
      {kUnknownPixelFormat, "UNKNOWN_12345", false},
  };

  constexpr uint32_t kWidth = 64;
  constexpr uint32_t kHeight = 64;

  for (const auto& tc : kTestCases) {
    SCOPED_TRACE(std::string(tc.name) + " (" + std::to_string(fidl::ToUnderlying(tc.pixel_format)) +
                 ")");

    const PixelFormatAndModifier linear(tc.pixel_format,
                                        fuchsia_images2::PixelFormatModifier::kLinear);
    const PixelFormatAndModifier arm_te(tc.pixel_format,
                                        fuchsia_images2::PixelFormatModifier::kArmLinearTe);
    const PixelFormatAndModifier goldfish(
        tc.pixel_format, fuchsia_images2::PixelFormatModifier::kGoogleGoldfishOptimal);

    EXPECT_EQ(tc.supported, ImageFormatIsSupported(linear));
    EXPECT_EQ(tc.supported, ImageFormatIsSupported(arm_te));
    EXPECT_EQ(tc.supported, ImageFormatIsSupported(goldfish));

    if (tc.supported) {
      const uint32_t bytes_per_row = ImageFormatStrideBytesPerWidthPixel(linear) * kWidth;

      fuchsia_images2::ImageFormat linear_image;
      linear_image.pixel_format() = tc.pixel_format;
      linear_image.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kLinear;
      linear_image.size() = {kWidth, kHeight};
      linear_image.bytes_per_row() = bytes_per_row;

      fuchsia_images2::ImageFormat goldfish_image = linear_image;
      goldfish_image.pixel_format_modifier() =
          fuchsia_images2::PixelFormatModifier::kGoogleGoldfishOptimal;

      const uint64_t goldfish_size = ImageFormatImageSize(goldfish_image);
      EXPECT_GT(goldfish_size, 0u);
      EXPECT_EQ(ImageFormatImageSize(linear_image), goldfish_size);
    }
  }
}

TEST(ImageFormat, P010BitsPerPixel) {
  PixelFormatAndModifier p010(fuchsia_images2::PixelFormat::kP010,
                              fuchsia_images2::PixelFormatModifier::kLinear);
  PixelFormatAndModifier nv12(fuchsia_images2::PixelFormat::kNv12,
                              fuchsia_images2::PixelFormatModifier::kLinear);
  EXPECT_EQ(12u, ImageFormatBitsPerPixel(nv12));
  // P010 has the same layout as NV12, with each sample in 2 bytes instead of 1.
  EXPECT_EQ(2 * ImageFormatBitsPerPixel(nv12), ImageFormatBitsPerPixel(p010));
  EXPECT_EQ(2 * ImageFormatStrideBytesPerWidthPixel(nv12),
            ImageFormatStrideBytesPerWidthPixel(p010));
}

TEST(ImageFormat, TenBitRgbColorSpaces) {
  for (auto pixel_format :
       {fuchsia_images2::PixelFormat::kA2B10G10R10, fuchsia_images2::PixelFormat::kA2R10G10B10}) {
    PixelFormatAndModifier format(pixel_format, fuchsia_images2::PixelFormatModifier::kLinear);
    EXPECT_TRUE(
        ImageFormatIsSupportedColorSpaceForPixelFormat(fuchsia_images2::ColorSpace::kSrgb, format));
    EXPECT_FALSE(ImageFormatIsSupportedColorSpaceForPixelFormat(
        fuchsia_images2::ColorSpace::kRec2020, format));
  }
}

TEST(ImageFormat, LinearMinimumRowBytesChecksAlignedWidth) {
  sysmem_v2::ImageFormatConstraints constraints;
  constraints.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  constraints.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kLinear;
  constraints.size_alignment() = {32u, 1u};
  uint32_t row_bytes;

  // 17 aligns up to 32, which is more than max_size.width.
  constraints.max_size() = {20u, 20u};
  EXPECT_FALSE(ImageFormatMinimumRowBytes(constraints, 17u, &row_bytes));

  constraints.max_size() = {32u, 32u};
  ASSERT_TRUE(ImageFormatMinimumRowBytes(constraints, 17u, &row_bytes));
  EXPECT_EQ(32u * 4, row_bytes);

  // 17 is less than min_size.width, but aligns up to 32, which isn't.
  constraints.min_size() = {20u, 1u};
  ASSERT_TRUE(ImageFormatMinimumRowBytes(constraints, 17u, &row_bytes));
  EXPECT_EQ(32u * 4, row_bytes);

  constraints.min_size() = {32u, 1u};
  ASSERT_TRUE(ImageFormatMinimumRowBytes(constraints, 17u, &row_bytes));
  EXPECT_EQ(32u * 4, row_bytes);

  // 17 aligns up to 32, which is still less than min_size.width.
  constraints.max_size() = {64u, 64u};
  constraints.min_size() = {33u, 1u};
  EXPECT_FALSE(ImageFormatMinimumRowBytes(constraints, 17u, &row_bytes));

  // Overflowing or zero size_alignment.width fails cleanly before checking min_size/max_size.
  EXPECT_FALSE(ImageFormatMinimumRowBytes(constraints, 0xFFFFFFE1u, &row_bytes));
  constraints.size_alignment() = {0u, 1u};
  EXPECT_FALSE(ImageFormatMinimumRowBytes(constraints, 17u, &row_bytes));
}

TEST(ImageFormat, ImageConstraintsToFormatZeroWidthFails) {
  sysmem_v2::ImageFormatConstraints constraints;
  constraints.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  constraints.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kLinear;

  // The minimum row bytes for width 0 is 0.
  auto row_bytes = ImageFormatMinimumRowBytesChecked(constraints, 0u);
  ASSERT_TRUE(row_bytes.IsValid());
  EXPECT_EQ(0u, row_bytes.ValueOrDie());

  EXPECT_FALSE(ImageConstraintsToFormat(constraints, 0u, 32u).is_ok());
}

TEST(ImageFormat, V1ConversionFailure) {
  sysmem_v1::wire::ImageFormatConstraints constraints = {
      .pixel_format =
          {
              .type = sysmem_v1::wire::PixelFormatType::kBgra32,
              .has_format_modifier = true,
              .format_modifier = {.value = sysmem_v1::wire::kFormatModifierLinear},
          },
      .layers = 2,
  };
  uint32_t row_bytes;
  EXPECT_FALSE(ImageFormatMinimumRowBytes(constraints, 64u, &row_bytes));
#if FUCHSIA_API_LEVEL_AT_LEAST(32)
  EXPECT_FALSE(ImageFormatMinimumRowBytesChecked(constraints, 64u).IsValid());
#endif
  EXPECT_FALSE(ImageConstraintsToFormat(constraints, 64u, 32u).is_ok());

  sysmem_v1::wire::ImageFormat2 image_format = {
      .pixel_format = constraints.pixel_format,
      .coded_width = 64,
      .coded_height = 32,
      .bytes_per_row = 256,
      .layers = 2,
  };
  uint64_t offset;
  EXPECT_FALSE(ImageFormatPlaneByteOffset(image_format, 0, &offset));
  EXPECT_FALSE(ImageFormatPlaneRowBytes(image_format, 0, &row_bytes));
#if FUCHSIA_API_LEVEL_AT_LEAST(32)
  EXPECT_FALSE(ImageFormatImageSizeChecked(image_format).IsValid());
#endif
  ASSERT_DEATH([&] { ImageFormatImageSize(image_format); });
}

TEST(ImageFormat, Linear420OddHeight) {
  fuchsia_images2::ImageFormat image_format;
  image_format.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kLinear;
  image_format.size() = {2u, 3u};
  image_format.bytes_per_row() = 4u;

  // 3 luma rows and 2 chroma rows.
  for (auto pixel_format :
       {fuchsia_images2::PixelFormat::kNv12, fuchsia_images2::PixelFormat::kP010,
        fuchsia_images2::PixelFormat::kM420, fuchsia_images2::PixelFormat::kI420,
        fuchsia_images2::PixelFormat::kYv12}) {
    SCOPED_TRACE(std::to_string(fidl::ToUnderlying(pixel_format)));
    image_format.pixel_format() = pixel_format;
    EXPECT_EQ(20u, ImageFormatImageSize(image_format));
  }

  // The second chroma plane of I420 and YV12 starts after 2 chroma rows of 2 bytes.
  for (auto pixel_format :
       {fuchsia_images2::PixelFormat::kI420, fuchsia_images2::PixelFormat::kYv12}) {
    SCOPED_TRACE(std::to_string(fidl::ToUnderlying(pixel_format)));
    image_format.pixel_format() = pixel_format;
    uint64_t offset;
    ASSERT_TRUE(ImageFormatPlaneByteOffset(image_format, 1, &offset));
    EXPECT_EQ(12u, offset);
    ASSERT_TRUE(ImageFormatPlaneByteOffset(image_format, 2, &offset));
    EXPECT_EQ(12u + 2 * (4 / 2), offset);
  }

  // An even height gives 1.5 times the luma size.
  image_format.size() = {2u, 4u};
  image_format.pixel_format() = fuchsia_images2::PixelFormat::kNv12;
  EXPECT_EQ(4u * 4 * 3 / 2, ImageFormatImageSize(image_format));
}

TEST(ImageFormat, AfbcMinimumRowBytesCoversAlignedWidth) {
  constexpr auto kAfbc32X8TiledHeader = static_cast<fuchsia_images2::PixelFormatModifier>(
      fidl::ToUnderlying(fuchsia_images2::PixelFormatModifier::kArmAfbc32X8) |
      fuchsia_images2::kFormatModifierArmTiledHeaderBit);
  struct TestCase {
    fuchsia_images2::PixelFormatModifier modifier;
    const char* name;
    uint32_t width_alignment;
  };
  constexpr TestCase kTestCases[] = {
      // Without a tiled header, the width is aligned to the block width.
      {fuchsia_images2::PixelFormatModifier::kArmAfbc16X16, "kArmAfbc16X16", 16u},
      {fuchsia_images2::PixelFormatModifier::kArmAfbc32X8, "kArmAfbc32X8", 32u},
      // With a tiled header (8x8 blocks), the width is aligned to 128 pixels (16x16) or 256 pixels
      // (32x8).
      {fuchsia_images2::PixelFormatModifier::kArmAfbc16X16YuvTiledHeader,
       "kArmAfbc16X16YuvTiledHeader", 128u},
      {kAfbc32X8TiledHeader, "kArmAfbc32X8|kFormatModifierArmTiledHeaderBit", 256u},
  };

  sysmem_v2::ImageFormatConstraints constraints;
  constraints.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  uint32_t row_bytes = 0;

  for (const auto& tc : kTestCases) {
    SCOPED_TRACE(tc.name);
    constraints.pixel_format_modifier() = tc.modifier;

    ASSERT_TRUE(ImageFormatMinimumRowBytes(constraints, 1u, &row_bytes));
    EXPECT_EQ(tc.width_alignment * 4u, row_bytes);

    ASSERT_TRUE(ImageFormatMinimumRowBytes(constraints, tc.width_alignment, &row_bytes));
    EXPECT_EQ(tc.width_alignment * 4u, row_bytes);

    ASSERT_TRUE(ImageFormatMinimumRowBytes(constraints, tc.width_alignment + 1u, &row_bytes));
    EXPECT_EQ(2u * tc.width_alignment * 4u, row_bytes);
  }

  // Check the uint32_t upper bound for a 256-pixel-aligned tiled-header modifier:
  // 0x3FFFFF00 * 4 = 0xFFFFFC00 fits in uint32_t, whereas 0x3FFFFF01 rounds up to 0x40000000
  // pixels (0x100000000 bytes) and overflows uint32_t.
  constraints.pixel_format_modifier() = kAfbc32X8TiledHeader;
  ASSERT_TRUE(ImageFormatMinimumRowBytes(constraints, 0x3FFFFF00u, &row_bytes));
  EXPECT_EQ(0xFFFFFC00u, row_bytes);
  EXPECT_FALSE(ImageFormatMinimumRowBytes(constraints, 0x3FFFFF01u, &row_bytes));
  EXPECT_FALSE(ImageFormatMinimumRowBytes(constraints, 0xFFFFFFFFu, &row_bytes));
}

TEST(ImageFormat, AfbcMinimumRowBytesAppliesRowConstraints) {
  sysmem_v2::ImageFormatConstraints constraints;
  constraints.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  constraints.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kArmAfbc16X16;
  uint32_t row_bytes;

  // 17 pixels is 32 pixels (2 blocks), so 128 bytes.
  ASSERT_TRUE(ImageFormatMinimumRowBytes(constraints, 17u, &row_bytes));
  EXPECT_EQ(128u, row_bytes);

  constraints.size_alignment() = {64u, 1u};
  ASSERT_TRUE(ImageFormatMinimumRowBytes(constraints, 17u, &row_bytes));
  EXPECT_EQ(64u * 4, row_bytes);
  constraints.size_alignment().reset();

  // min_bytes_per_row is rounded up to the 64-byte block width.
  constraints.min_bytes_per_row() = 1000u;
  ASSERT_TRUE(ImageFormatMinimumRowBytes(constraints, 17u, &row_bytes));
  EXPECT_EQ(1024u, row_bytes);
  constraints.min_bytes_per_row().reset();

  // bytes_per_row_divisor is combined with the 64-byte block width via lcm(100, 64) = 1600.
  constraints.bytes_per_row_divisor() = 100u;
  ASSERT_TRUE(ImageFormatMinimumRowBytes(constraints, 17u, &row_bytes));
  EXPECT_EQ(1600u, row_bytes);
  constraints.bytes_per_row_divisor() = 0xFFFFFFFFu;
  EXPECT_FALSE(ImageFormatMinimumRowBytes(constraints, 17u, &row_bytes));
  constraints.bytes_per_row_divisor() = 0u;
  EXPECT_FALSE(ImageFormatMinimumRowBytes(constraints, 17u, &row_bytes));
  constraints.bytes_per_row_divisor().reset();

  constraints.max_bytes_per_row() = 127u;
  EXPECT_FALSE(ImageFormatMinimumRowBytes(constraints, 17u, &row_bytes));
  constraints.max_bytes_per_row() = 128u;
  EXPECT_TRUE(ImageFormatMinimumRowBytes(constraints, 17u, &row_bytes));
  constraints.max_bytes_per_row().reset();

  // The size alignment is applied before the width range check.
  constraints.size_alignment() = {64u, 1u};
  constraints.max_size() = {40u, 40u};
  EXPECT_FALSE(ImageFormatMinimumRowBytes(constraints, 17u, &row_bytes));
}

TEST(ImageFormat, Afbc32X8ImageSize) {
  sysmem_v2::ImageFormatConstraints constraints;
  constraints.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  constraints.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kArmAfbc32X8;

  // 17x9 rounds up to 32x16 (1x2 = 2 32x8 blocks): 2 * 1024B body + RoundUp(2 * 16B, 1024B) = 3072.
  auto non_tiled = ImageConstraintsToFormat(constraints, 17u, 9u).take_value();
  EXPECT_EQ(128u, non_tiled.bytes_per_row());
  EXPECT_EQ(3072u, ImageFormatImageSize(non_tiled));

  // With TILED_HEADER, 17x9 rounds up to 256x64 (8x8 = 64 32x8 blocks):
  // 64 * 1024B body + RoundUp(64 * 16B, 4096B) = 65536 + 4096 = 69632.
  constraints.pixel_format_modifier() = static_cast<fuchsia_images2::PixelFormatModifier>(
      fidl::ToUnderlying(fuchsia_images2::PixelFormatModifier::kArmAfbc32X8) |
      fuchsia_images2::kFormatModifierArmTiledHeaderBit);
  auto tiled_header = ImageConstraintsToFormat(constraints, 17u, 9u).take_value();
  EXPECT_EQ(1024u, tiled_header.bytes_per_row());
  EXPECT_EQ(69632u, ImageFormatImageSize(tiled_header));
}

TEST(ImageFormat, AfbcPlane0RowBytesAndStrideScaling) {
  sysmem_v2::ImageFormatConstraints constraints;
  constraints.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  constraints.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kArmAfbc16X16Te;

  // 17x16 -> 32x16 (2x1 = 2 blocks): NonTESize = 2 * 1024 + 1024 = 3072.
  // TE uses logical size 17x16 -> 1 TE chunk -> 64B TE row; TE buffer = 64 + 4 * 64 = 320;
  // total = 3392.
  auto base_format = ImageConstraintsToFormat(constraints, 17u, 16u).take_value();
  EXPECT_EQ(128u, base_format.bytes_per_row());
  uint64_t offset;
  uint32_t row_bytes;
  ASSERT_TRUE(ImageFormatPlaneByteOffset(base_format, 0, &offset));
  EXPECT_EQ(0u, offset);
  ASSERT_TRUE(ImageFormatPlaneRowBytes(base_format, 0, &row_bytes));
  EXPECT_EQ(128u, row_bytes);
  EXPECT_EQ(3392u, ImageFormatImageSize(base_format));
  ASSERT_TRUE(ImageFormatPlaneByteOffset(base_format, 3, &offset));
  EXPECT_EQ(3072u, offset);
  uint32_t base_te_row = 0;
  ASSERT_TRUE(ImageFormatPlaneRowBytes(base_format, 3, &base_te_row));
  EXPECT_EQ(64u, base_te_row);

  // 512B / 4 = 128 px (8x1 = 8 blocks): NonTESize = 8 * 1024 + 1024 = 9216.
  // TE uses logical size 17x16 -> 64B TE row; TE buffer = 64 + 4 * 64 = 320; total = 9536.
  constraints.min_bytes_per_row() = 512u;
  auto padded_format = ImageConstraintsToFormat(constraints, 17u, 16u).take_value();
  EXPECT_EQ(512u, padded_format.bytes_per_row());
  ASSERT_TRUE(ImageFormatPlaneRowBytes(padded_format, 0, &row_bytes));
  EXPECT_EQ(512u, row_bytes);
  EXPECT_EQ(9536u, ImageFormatImageSize(padded_format));
  ASSERT_TRUE(ImageFormatPlaneByteOffset(padded_format, 3, &offset));
  EXPECT_EQ(9216u, offset);
  uint32_t padded_te_row = 0;
  ASSERT_TRUE(ImageFormatPlaneRowBytes(padded_format, 3, &padded_te_row));
  EXPECT_EQ(64u, padded_te_row);

  // 1024B / 4 = 256 px (16x1 = 16 blocks): NonTESize = 16 * 1024 + 1024 = 17408.
  // Plane 3's byte offset advances with bytes_per_row to 17408, while TE row bytes and TE buffer
  // size follow logical size 17x16 (64B TE row, 320B TE buffer; total = 17408 + 320 = 17728).
  constraints.min_bytes_per_row() = 1024u;
  auto wide_format = ImageConstraintsToFormat(constraints, 17u, 16u).take_value();
  EXPECT_EQ(1024u, wide_format.bytes_per_row());
  ASSERT_TRUE(ImageFormatPlaneRowBytes(wide_format, 0, &row_bytes));
  EXPECT_EQ(1024u, row_bytes);
  EXPECT_EQ(17728u, ImageFormatImageSize(wide_format));
  ASSERT_TRUE(ImageFormatPlaneByteOffset(wide_format, 3, &offset));
  EXPECT_EQ(17408u, offset);
  uint32_t wide_te_row = 0;
  ASSERT_TRUE(ImageFormatPlaneRowBytes(wide_format, 3, &wide_te_row));
  EXPECT_EQ(64u, wide_te_row);

  // Increasing logical width to 129 px (same 1024B bytes_per_row and 17408B NonTESize) widens TE
  // from 128 px (64B TE row) to 3 64x64 chunks (128B TE row, 576B TE buffer; total = 17984).
  auto wide_logical_format = ImageConstraintsToFormat(constraints, 129u, 16u).take_value();
  EXPECT_EQ(1024u, wide_logical_format.bytes_per_row());
  ASSERT_TRUE(ImageFormatPlaneByteOffset(wide_logical_format, 3, &offset));
  EXPECT_EQ(17408u, offset);
  ASSERT_TRUE(ImageFormatPlaneRowBytes(wide_logical_format, 3, &row_bytes));
  EXPECT_EQ(128u, row_bytes);
  EXPECT_EQ(17984u, ImageFormatImageSize(wide_logical_format));

  // 32x8 + TILED_HEADER + TE at 17x9 aligns plane 0 width to 256 px (NonTESize = 69632), while TE
  // uses logical 17x9 (64B TE row, 320B TE buffer; total = 69952).
  sysmem_v2::ImageFormatConstraints tiled_te_constraints;
  tiled_te_constraints.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  tiled_te_constraints.pixel_format_modifier() = static_cast<fuchsia_images2::PixelFormatModifier>(
      fidl::ToUnderlying(fuchsia_images2::PixelFormatModifier::kArmAfbc32X8) |
      fuchsia_images2::kFormatModifierArmTiledHeaderBit | fuchsia_images2::kFormatModifierArmTeBit);
  auto tiled_te_format = ImageConstraintsToFormat(tiled_te_constraints, 17u, 9u).take_value();
  ASSERT_TRUE(ImageFormatPlaneByteOffset(tiled_te_format, 3, &offset));
  EXPECT_EQ(69632u, offset);
  ASSERT_TRUE(ImageFormatPlaneRowBytes(tiled_te_format, 3, &row_bytes));
  EXPECT_EQ(64u, row_bytes);
  EXPECT_EQ(69952u, ImageFormatImageSize(tiled_te_format));

  // Hand-built unaligned bytes_per_row (200 >= 128 minimum) rounds up to 256 (4 blocks).
  fuchsia_images2::ImageFormat unaligned_format;
  unaligned_format.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  unaligned_format.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kArmAfbc16X16;
  unaligned_format.size() = {17u, 16u};
  unaligned_format.bytes_per_row() = 200u;
  ASSERT_TRUE(ImageFormatPlaneRowBytes(unaligned_format, 0, &row_bytes));
  EXPECT_EQ(256u, row_bytes);

  sysmem_v2::ImageFormatConstraints aligned_256_constraints;
  aligned_256_constraints.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  aligned_256_constraints.pixel_format_modifier() =
      fuchsia_images2::PixelFormatModifier::kArmAfbc16X16;
  aligned_256_constraints.min_bytes_per_row() = 256u;
  auto aligned_256_format =
      ImageConstraintsToFormat(aligned_256_constraints, 17u, 16u).take_value();
  EXPECT_EQ(ImageFormatImageSize(aligned_256_format), ImageFormatImageSize(unaligned_format));
}

TEST(ImageFormat, ArmLinearTeRowBytesScaling) {
  sysmem_v2::ImageFormatConstraints bgra_constraints;
  bgra_constraints.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  bgra_constraints.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kArmLinearTe;
  bgra_constraints.min_bytes_per_row() = 1024u;

  // Plane 0 linear size = 10 * 1024 = 10240 (plane 3 byte offset = 10240), while TE uses logical
  // size 10x10 -> 1 TE chunk -> 64B TE row; TE buffer = 64 + 4 * 64 = 320; total = 10560.
  auto bgra_padded = ImageConstraintsToFormat(bgra_constraints, 10u, 10u).take_value();
  uint64_t offset = 0;
  uint32_t row_bytes = 0;
  ASSERT_TRUE(ImageFormatPlaneByteOffset(bgra_padded, 3, &offset));
  EXPECT_EQ(10240u, offset);
  ASSERT_TRUE(ImageFormatPlaneRowBytes(bgra_padded, 3, &row_bytes));
  EXPECT_EQ(64u, row_bytes);
  EXPECT_EQ(10560u, ImageFormatImageSize(bgra_padded));

  // Logical width 129 px with the same 1024B bytes_per_row keeps plane 3 at offset 10240 while
  // widening TE to 128B per row (TE buffer = 64 + 4 * 128 = 576; total = 10816).
  auto bgra_wide_logical = ImageConstraintsToFormat(bgra_constraints, 129u, 10u).take_value();
  ASSERT_TRUE(ImageFormatPlaneByteOffset(bgra_wide_logical, 3, &offset));
  EXPECT_EQ(10240u, offset);
  ASSERT_TRUE(ImageFormatPlaneRowBytes(bgra_wide_logical, 3, &row_bytes));
  EXPECT_EQ(128u, row_bytes);
  EXPECT_EQ(10816u, ImageFormatImageSize(bgra_wide_logical));

  // 3-byte format (kR8G8B8): unaligned bytes_per_row (385B -> 3850B plane 0) rounds plane 3's byte
  // offset up to 64B (3904B) while TE row bytes stays 64B for logical width 10 (total = 4224B).
  sysmem_v2::ImageFormatConstraints rgb_constraints;
  rgb_constraints.pixel_format() = fuchsia_images2::PixelFormat::kR8G8B8;
  rgb_constraints.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kArmLinearTe;
  rgb_constraints.min_bytes_per_row() = 384u;
  auto rgb_384 = ImageConstraintsToFormat(rgb_constraints, 10u, 10u).take_value();
  ASSERT_TRUE(ImageFormatPlaneByteOffset(rgb_384, 3, &offset));
  EXPECT_EQ(3840u, offset);
  ASSERT_TRUE(ImageFormatPlaneRowBytes(rgb_384, 3, &row_bytes));
  EXPECT_EQ(64u, row_bytes);
  EXPECT_EQ(4160u, ImageFormatImageSize(rgb_384));

  rgb_constraints.min_bytes_per_row() = 385u;
  auto rgb_385 = ImageConstraintsToFormat(rgb_constraints, 10u, 10u).take_value();
  ASSERT_TRUE(ImageFormatPlaneByteOffset(rgb_385, 3, &offset));
  EXPECT_EQ(3904u, offset);
  ASSERT_TRUE(ImageFormatPlaneRowBytes(rgb_385, 3, &row_bytes));
  EXPECT_EQ(64u, row_bytes);
  EXPECT_EQ(4224u, ImageFormatImageSize(rgb_385));
}

#if FUCHSIA_API_LEVEL_AT_LEAST(32)
TEST(ImageFormat, UndersizedBytesPerRowIsInvalid) {
  // AFBC 16x16 at 1920x1080 requires at least RoundUp(1920, 16) * 4 = 7680 bytes_per_row.
  fuchsia_images2::ImageFormat afbc;
  afbc.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  afbc.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kArmAfbc16X16;
  afbc.size() = {1920u, 1080u};
  for (uint32_t bad_bpr : {0u, 1000u, 7679u}) {
    afbc.bytes_per_row() = bad_bpr;
    EXPECT_FALSE(ImageFormatImageSizeChecked(afbc).IsValid());
    EXPECT_FALSE(ImageFormatPlaneRowBytesChecked(afbc, 0).IsValid());
    EXPECT_EQ(0u, ImageFormatPlaneByteOffsetChecked(afbc, 0).ValueOrDie());
  }
  afbc.bytes_per_row() = 7680u;
  EXPECT_TRUE(ImageFormatImageSizeChecked(afbc).IsValid());

  // AFBC 16x16 at 17x16 requires at least RoundUp(17, 16) * 4 = 128 bytes_per_row.
  afbc.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kArmAfbc16X16Te;
  afbc.size() = {17u, 16u};
  afbc.bytes_per_row() = 68u;
  EXPECT_FALSE(ImageFormatImageSizeChecked(afbc).IsValid());
  EXPECT_FALSE(ImageFormatPlaneRowBytesChecked(afbc, 0).IsValid());
  EXPECT_FALSE(ImageFormatPlaneByteOffsetChecked(afbc, 3).IsValid());
  EXPECT_FALSE(ImageFormatPlaneRowBytesChecked(afbc, 3).IsValid());
  afbc.bytes_per_row() = 128u;
  EXPECT_TRUE(ImageFormatImageSizeChecked(afbc).IsValid());

  // V1 ImageFormat2: both 0 and non-zero undersized bytes_per_row return invalid/false on fallible
  // overloads (while non-Checked ImageFormatImageSize still panics).
  sysmem_v1::wire::ImageFormat2 v1_afbc = {
      .pixel_format =
          {
              .type = sysmem_v1::wire::PixelFormatType::kBgra32,
              .has_format_modifier = true,
              .format_modifier = {.value = sysmem_v1::wire::kFormatModifierArmAfbc16X16},
          },
      .coded_width = 1920u,
      .coded_height = 1080u,
      .bytes_per_row = 0u,
  };
  for (uint32_t bad_bpr : {0u, 1000u}) {
    v1_afbc.bytes_per_row = bad_bpr;
    EXPECT_FALSE(ImageFormatImageSizeChecked(v1_afbc).IsValid());
    EXPECT_FALSE(ImageFormatPlaneRowBytesChecked(v1_afbc, 0).IsValid());
    uint32_t v1_row_bytes = 0;
    EXPECT_FALSE(ImageFormatPlaneRowBytes(v1_afbc, 0, &v1_row_bytes));
    uint64_t v1_offset = 1;
    EXPECT_TRUE(ImageFormatPlaneByteOffset(v1_afbc, 0, &v1_offset));
    EXPECT_EQ(0u, v1_offset);
  }
  v1_afbc.bytes_per_row = 0u;
  ASSERT_DEATH([&] { (void)ImageFormatImageSize(v1_afbc); });

  // Intel X-tiled RGBA at 1921x1080 requires RoundUp(1921 * 4, 512) = 8192 bytes_per_row.
  fuchsia_images2::ImageFormat intel;
  intel.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  intel.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kIntelI915XTiled;
  intel.size() = {1921u, 1080u};
  for (uint32_t bad_bpr : {0u, 7684u, 8191u}) {
    intel.bytes_per_row() = bad_bpr;
    EXPECT_FALSE(ImageFormatImageSizeChecked(intel).IsValid());
    EXPECT_FALSE(ImageFormatPlaneRowBytesChecked(intel, 0).IsValid());
    EXPECT_EQ(0u, ImageFormatPlaneByteOffsetChecked(intel, 0).ValueOrDie());
  }
  intel.bytes_per_row() = 8192u;
  EXPECT_TRUE(ImageFormatImageSizeChecked(intel).IsValid());

  // Intel Y-tiled NV12 at 1920x1080 requires RoundUp(1920, 128) = 1920 bytes_per_row.
  intel.pixel_format() = fuchsia_images2::PixelFormat::kNv12;
  intel.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kIntelI915YTiled;
  intel.size() = {1920u, 1080u};
  intel.bytes_per_row() = 1919u;
  EXPECT_FALSE(ImageFormatPlaneByteOffsetChecked(intel, 1).IsValid());

#if FUCHSIA_API_LEVEL_AT_LEAST(33)
  // PVRIC 8x8 at 17x16 requires RoundUp(17 * 4, 32) = 96 bytes_per_row.
  fuchsia_images2::ImageFormat pvric;
  pvric.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  pvric.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kPvrFbcdc8X8V14;
  pvric.size() = {17u, 16u};
  pvric.bytes_per_row() = 68u;
  EXPECT_FALSE(ImageFormatImageSizeChecked(pvric).IsValid());
  EXPECT_FALSE(ImageFormatPlaneByteOffsetChecked(pvric, 0).IsValid());
  EXPECT_FALSE(ImageFormatPlaneRowBytesChecked(pvric, 0).IsValid());
  EXPECT_FALSE(ImageFormatPlaneRowBytesChecked(pvric, 3).IsValid());
  EXPECT_EQ(0u, ImageFormatPlaneByteOffsetChecked(pvric, 3).ValueOrDie());
  pvric.bytes_per_row() = 96u;
  EXPECT_TRUE(ImageFormatImageSizeChecked(pvric).IsValid());
#endif

  // Linear, Goldfish, and ARM linear TE at 1920x1080 BGRA require at least 1920 * 4 = 7680.
  for (auto modifier : {fuchsia_images2::PixelFormatModifier::kLinear,
                        fuchsia_images2::PixelFormatModifier::kGoogleGoldfishOptimal,
                        fuchsia_images2::PixelFormatModifier::kArmLinearTe}) {
    fuchsia_images2::ImageFormat fmt;
    fmt.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
    fmt.pixel_format_modifier() = modifier;
    fmt.size() = {1920u, 1080u};
    for (uint32_t bad_bpr : {0u, 7679u}) {
      fmt.bytes_per_row() = bad_bpr;
      EXPECT_FALSE(ImageFormatImageSizeChecked(fmt).IsValid());
      EXPECT_FALSE(ImageFormatPlaneRowBytesChecked(fmt, 0).IsValid());
      if (modifier == fuchsia_images2::PixelFormatModifier::kArmLinearTe) {
        EXPECT_FALSE(ImageFormatPlaneByteOffsetChecked(fmt, 3).IsValid());
        EXPECT_FALSE(ImageFormatPlaneRowBytesChecked(fmt, 3).IsValid());
      }
    }
    fmt.bytes_per_row() = 7680u;
    EXPECT_TRUE(ImageFormatImageSizeChecked(fmt).IsValid());
  }

  // Multi-plane linear NV12 at 1920x1080 requires at least 1920 bytes_per_row.
  fuchsia_images2::ImageFormat nv12;
  nv12.pixel_format() = fuchsia_images2::PixelFormat::kNv12;
  nv12.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kLinear;
  nv12.size() = {1920u, 1080u};
  nv12.bytes_per_row() = 1919u;
  EXPECT_FALSE(ImageFormatImageSizeChecked(nv12).IsValid());
  EXPECT_EQ(0u, ImageFormatPlaneByteOffsetChecked(nv12, 0).ValueOrDie());
  EXPECT_FALSE(ImageFormatPlaneByteOffsetChecked(nv12, 1).IsValid());
  EXPECT_FALSE(ImageFormatPlaneRowBytesChecked(nv12, 0).IsValid());
  EXPECT_FALSE(ImageFormatPlaneRowBytesChecked(nv12, 1).IsValid());
}
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)

TEST(ImageFormat, PlaneLayoutRequiresSizeAndBytesPerRowWhenUsed) {
  fuchsia_images2::ImageFormat linear_no_size;
  linear_no_size.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  linear_no_size.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kLinear;
  linear_no_size.bytes_per_row() = 40u;
  uint32_t row_bytes;
  ASSERT_DEATH([&] { ImageFormatPlaneRowBytes(linear_no_size, 0, &row_bytes); });

  fuchsia_images2::ImageFormat te_no_size;
  te_no_size.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  te_no_size.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kArmLinearTe;
  te_no_size.bytes_per_row() = 40u;
  uint64_t offset;
  ASSERT_DEATH([&] { ImageFormatPlaneByteOffset(te_no_size, 3, &offset); });
  ASSERT_DEATH([&] { ImageFormatPlaneRowBytes(te_no_size, 0, &row_bytes); });
  ASSERT_DEATH([&] { ImageFormatPlaneRowBytes(te_no_size, 3, &row_bytes); });

  fuchsia_images2::ImageFormat te_no_bpr;
  te_no_bpr.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  te_no_bpr.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kArmLinearTe;
  te_no_bpr.size() = {10u, 10u};
  ASSERT_DEATH([&] { ImageFormatPlaneRowBytes(te_no_bpr, 3, &row_bytes); });

  fuchsia_images2::ImageFormat afbc_no_size;
  afbc_no_size.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  afbc_no_size.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kArmAfbc16X16Te;
  afbc_no_size.bytes_per_row() = 128u;
  ASSERT_DEATH([&] { ImageFormatPlaneRowBytes(afbc_no_size, 0, &row_bytes); });
  ASSERT_DEATH([&] { ImageFormatPlaneRowBytes(afbc_no_size, 3, &row_bytes); });

  fuchsia_images2::ImageFormat afbc_no_bpr;
  afbc_no_bpr.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  afbc_no_bpr.pixel_format_modifier() = fuchsia_images2::PixelFormatModifier::kArmAfbc16X16Te;
  afbc_no_bpr.size() = {16u, 16u};
  ASSERT_DEATH([&] { ImageFormatImageSize(afbc_no_bpr); });
  ASSERT_DEATH([&] { ImageFormatPlaneRowBytes(afbc_no_bpr, 0, &row_bytes); });
  ASSERT_DEATH([&] { ImageFormatPlaneByteOffset(afbc_no_bpr, 3, &offset); });
  ASSERT_DEATH([&] { ImageFormatPlaneRowBytes(afbc_no_bpr, 3, &row_bytes); });

  fuchsia_images2::ImageFormat intel_no_size;
  intel_no_size.pixel_format() = fuchsia_images2::PixelFormat::kB8G8R8A8;
  intel_no_size.pixel_format_modifier() =
      fuchsia_images2::PixelFormatModifier::kIntelI915YfTiledCcs;
  intel_no_size.bytes_per_row() = 256u;
  ASSERT_DEATH([&] { ImageFormatPlaneByteOffset(intel_no_size, 3, &offset); });
}

TEST(ImageFormat, V1PixelFormatEquality) {
  sysmem_v1::wire::PixelFormat bgra_linear = {
      .type = sysmem_v1::wire::PixelFormatType::kBgra32,
      .has_format_modifier = true,
      .format_modifier = {.value = sysmem_v1::wire::kFormatModifierLinear},
  };
  EXPECT_TRUE(ImageFormatIsPixelFormatEqual(bgra_linear, bgra_linear));

  sysmem_v1::wire::PixelFormat other = bgra_linear;
  other.type = sysmem_v1::wire::PixelFormatType::kR8G8B8A8;
  EXPECT_FALSE(ImageFormatIsPixelFormatEqual(bgra_linear, other));
  EXPECT_FALSE(ImageFormatIsPixelFormatEqual(other, bgra_linear));

  other = bgra_linear;
  other.format_modifier.value = sysmem_v1::wire::kFormatModifierIntelI915XTiled;
  EXPECT_FALSE(ImageFormatIsPixelFormatEqual(bgra_linear, other));
  EXPECT_FALSE(ImageFormatIsPixelFormatEqual(other, bgra_linear));
}

TEST(ImageFormat, V1PixelFormatWithoutModifierIsLinear) {
  // Without has_format_modifier, format_modifier is ignored and the modifier is LINEAR.
  sysmem_v1::wire::PixelFormat no_modifier = {
      .type = sysmem_v1::wire::PixelFormatType::kBgra32,
      .has_format_modifier = false,
      .format_modifier = {.value = sysmem_v1::wire::kFormatModifierIntelI915XTiled},
  };
  sysmem_v1::wire::PixelFormat linear = {
      .type = sysmem_v1::wire::PixelFormatType::kBgra32,
      .has_format_modifier = true,
      .format_modifier = {.value = sysmem_v1::wire::kFormatModifierLinear},
  };
  EXPECT_TRUE(ImageFormatIsPixelFormatEqual(no_modifier, linear));
  EXPECT_TRUE(ImageFormatIsSupported(no_modifier));
  EXPECT_TRUE(ImageFormatIsNonTiledSinglePlane(sysmem::V2CopyFromV1PixelFormat(no_modifier)));
  EXPECT_TRUE(ImageFormatCompatibleWithProtectedMemory(no_modifier));
  EXPECT_EQ(32u, ImageFormatBitsPerPixel(no_modifier));
  EXPECT_EQ(4u, ImageFormatStrideBytesPerWidthPixel(no_modifier));
#if FUCHSIA_API_LEVEL_AT_LEAST(NEXT)
  EXPECT_EQ(1u, ImageFormatRowBytesMinDivisor(no_modifier));
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(NEXT)

  sysmem_v1::wire::ImageFormatConstraints constraints = {.pixel_format = no_modifier};
  uint32_t row_bytes;
  ASSERT_TRUE(ImageFormatMinimumRowBytes(constraints, 10u, &row_bytes));
  EXPECT_EQ(40u, row_bytes);
  auto image_format = ImageConstraintsToFormat(constraints, 10u, 10u);
  ASSERT_TRUE(image_format.is_ok());
  EXPECT_EQ(40u, image_format.value().bytes_per_row);
  EXPECT_EQ(400u, ImageFormatImageSize(image_format.value()));
}

#if FUCHSIA_API_LEVEL_AT_LEAST(NEXT)
TEST(ImageFormat, RowBytesMinDivisor) {
  EXPECT_EQ(1u, ImageFormatRowBytesMinDivisor(
                    PixelFormatAndModifier(fuchsia_images2::PixelFormat::kB8G8R8A8,
                                           fuchsia_images2::PixelFormatModifier::kLinear)));
  EXPECT_EQ(
      1u, ImageFormatRowBytesMinDivisor(PixelFormatAndModifier(
              fuchsia_images2::PixelFormat::kNv12, fuchsia_images2::PixelFormatModifier::kLinear)));
  EXPECT_EQ(1u, ImageFormatRowBytesMinDivisor(
                    PixelFormatAndModifier(fuchsia_images2::PixelFormat::kB8G8R8,
                                           fuchsia_images2::PixelFormatModifier::kLinear)));
  EXPECT_EQ(1u, ImageFormatRowBytesMinDivisor(PixelFormatAndModifier(
                    fuchsia_images2::PixelFormat::kB8G8R8A8,
                    fuchsia_images2::PixelFormatModifier::kGoogleGoldfishOptimal)));
  EXPECT_EQ(1u, ImageFormatRowBytesMinDivisor(
                    PixelFormatAndModifier(fuchsia_images2::PixelFormat::kB8G8R8A8,
                                           fuchsia_images2::PixelFormatModifier::kArmLinearTe)));
  EXPECT_EQ(512u, ImageFormatRowBytesMinDivisor(PixelFormatAndModifier(
                      fuchsia_images2::PixelFormat::kB8G8R8A8,
                      fuchsia_images2::PixelFormatModifier::kIntelI915XTiled)));
  EXPECT_EQ(128u, ImageFormatRowBytesMinDivisor(PixelFormatAndModifier(
                      fuchsia_images2::PixelFormat::kB8G8R8A8,
                      fuchsia_images2::PixelFormatModifier::kIntelI915YTiled)));
  EXPECT_EQ(128u, ImageFormatRowBytesMinDivisor(PixelFormatAndModifier(
                      fuchsia_images2::PixelFormat::kB8G8R8A8,
                      fuchsia_images2::PixelFormatModifier::kIntelI915YTiledCcs)));
  EXPECT_EQ(128u, ImageFormatRowBytesMinDivisor(PixelFormatAndModifier(
                      fuchsia_images2::PixelFormat::kB8G8R8A8,
                      fuchsia_images2::PixelFormatModifier::kIntelI915YfTiled)));
  EXPECT_EQ(128u, ImageFormatRowBytesMinDivisor(PixelFormatAndModifier(
                      fuchsia_images2::PixelFormat::kB8G8R8A8,
                      fuchsia_images2::PixelFormatModifier::kIntelI915YfTiledCcs)));
  EXPECT_EQ(128u, ImageFormatRowBytesMinDivisor(PixelFormatAndModifier(
                      fuchsia_images2::PixelFormat::kNv12,
                      fuchsia_images2::PixelFormatModifier::kIntelI915YTiled)));
  EXPECT_EQ(64u, ImageFormatRowBytesMinDivisor(
                     PixelFormatAndModifier(fuchsia_images2::PixelFormat::kB8G8R8A8,
                                            fuchsia_images2::PixelFormatModifier::kArmAfbc16X16)));
  EXPECT_EQ(128u, ImageFormatRowBytesMinDivisor(
                      PixelFormatAndModifier(fuchsia_images2::PixelFormat::kB8G8R8A8,
                                             fuchsia_images2::PixelFormatModifier::kArmAfbc32X8)));
  EXPECT_EQ(512u, ImageFormatRowBytesMinDivisor(PixelFormatAndModifier(
                      fuchsia_images2::PixelFormat::kB8G8R8A8,
                      fuchsia_images2::PixelFormatModifier::kArmAfbc16X16YuvTiledHeader)));
  EXPECT_EQ(1024u, ImageFormatRowBytesMinDivisor(PixelFormatAndModifier(
                       fuchsia_images2::PixelFormat::kB8G8R8A8,
                       static_cast<fuchsia_images2::PixelFormatModifier>(
                           fidl::ToUnderlying(fuchsia_images2::PixelFormatModifier::kArmAfbc32X8) |
                           fuchsia_images2::kFormatModifierArmTiledHeaderBit))));
  EXPECT_EQ(32u, ImageFormatRowBytesMinDivisor(PixelFormatAndModifier(
                     fuchsia_images2::PixelFormat::kB8G8R8A8,
                     fuchsia_images2::PixelFormatModifier::kPvrFbcdc8X8V14)));
  EXPECT_EQ(32u, ImageFormatRowBytesMinDivisor(PixelFormatAndModifier(
                     fuchsia_images2::PixelFormat::kR5G6B5,
                     fuchsia_images2::PixelFormatModifier::kPvrFbcdc8X8V14)));
  EXPECT_EQ(64u, ImageFormatRowBytesMinDivisor(PixelFormatAndModifier(
                     fuchsia_images2::PixelFormat::kB8G8R8A8,
                     fuchsia_images2::PixelFormatModifier::kPvrFbcdc16X4V14)));

  sysmem_v1::wire::PixelFormat v1_x_tiled = {
      .type = sysmem_v1::wire::PixelFormatType::kBgra32,
      .has_format_modifier = true,
      .format_modifier = {.value = sysmem_v1::wire::kFormatModifierIntelI915XTiled},
  };
  EXPECT_EQ(512u, ImageFormatRowBytesMinDivisor(v1_x_tiled));

  sysmem_v1::wire::PixelFormat v1_goldfish = {
      .type = sysmem_v1::wire::PixelFormatType::kBgra32,
      .has_format_modifier = true,
      .format_modifier = {.value = sysmem_v1::wire::kFormatModifierGoogleGoldfishOptimal},
  };
  EXPECT_EQ(1u, ImageFormatRowBytesMinDivisor(v1_goldfish));
}
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(NEXT)
