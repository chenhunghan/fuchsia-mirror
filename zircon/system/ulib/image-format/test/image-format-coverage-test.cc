// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Broad coverage of the image-format API.
//
// Besides hand-written expectations, most of the grids here compare against golden values that
// were generated from the C++ implementation of image-format. The goldens make it possible to
// validate alternate implementations of the same API (such as a Rust-backed implementation)
// against the C++ implementation. If a golden comparison fails, the test writes the actual values
// as a replacement image-format-coverage-golden-<name>.inc file to /custom_artifacts.
// update-coverage-goldens.py runs the test and copies those files over the goldens. The
// `ImageLayout` and `Constraints` tests also write uncompared per-case `.txt` dumps to
// `/custom_artifacts` on every run so a fingerprint (`all=`) mismatch can be localized by diffing
// against a C++ run.

#include <lib/fidl/cpp/wire_natural_conversions.h>
#include <lib/image-format/image_format.h>
#include <lib/sysmem-version/sysmem-version.h>
#include <lib/zbi-format/graphics.h>
#include <zircon/compiler.h>

#include <algorithm>
#include <cinttypes>
#include <cstdarg>
#include <cstdio>
#include <optional>
#include <span>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include <fbl/string_printf.h>
#include <safemath/checked_math.h>
#include <zxtest/zxtest.h>

namespace {

using fuchsia_images2::ColorSpace;
using fuchsia_images2::PixelFormat;
using fuchsia_images2::PixelFormatModifier;
using safemath::CheckedNumeric;

// A modifier from an unknown vendor, so no ImageFormatSet supports it with any pixel format.
constexpr PixelFormatModifier kUnknownModifier{0x0500000000000001ull};
constexpr PixelFormat kUnknownPixelFormat{200u};
constexpr ColorSpace kUnknownColorSpace{100u};

struct NamedPixelFormat {
  PixelFormat value;
  const char* name;
};

const NamedPixelFormat kAllPixelFormats[] = {
    {PixelFormat::kInvalid, "INVALID"},
    {PixelFormat::kR8G8B8A8, "R8G8B8A8"},
    {PixelFormat::kR8G8B8X8, "R8G8B8X8"},
    {PixelFormat::kB8G8R8A8, "B8G8R8A8"},
    {PixelFormat::kB8G8R8X8, "B8G8R8X8"},
    {PixelFormat::kI420, "I420"},
    {PixelFormat::kM420, "M420"},
    {PixelFormat::kNv12, "NV12"},
    {PixelFormat::kYuy2, "YUY2"},
    {PixelFormat::kMjpeg, "MJPEG"},
    {PixelFormat::kYv12, "YV12"},
    {PixelFormat::kB8G8R8, "B8G8R8"},
    {PixelFormat::kR5G6B5, "R5G6B5"},
    {PixelFormat::kR3G3B2, "R3G3B2"},
    {PixelFormat::kR2G2B2X2, "R2G2B2X2"},
    {PixelFormat::kL8, "L8"},
    {PixelFormat::kR8, "R8"},
    {PixelFormat::kR8G8, "R8G8"},
    {PixelFormat::kA2R10G10B10, "A2R10G10B10"},
    {PixelFormat::kA2B10G10R10, "A2B10G10R10"},
    {PixelFormat::kP010, "P010"},
    {PixelFormat::kR8G8B8, "R8G8B8"},
    {PixelFormat::kDoNotCare, "DO_NOT_CARE"},
    {kUnknownPixelFormat, "UNKNOWN_PF_200"},
};

struct NamedModifier {
  PixelFormatModifier value;
  const char* name;
};

const NamedModifier kAllModifiers[] = {
    {PixelFormatModifier::kLinear, "LINEAR"},
    {PixelFormatModifier::kIntelI915XTiled, "INTEL_X"},
    {PixelFormatModifier::kIntelI915YTiled, "INTEL_Y"},
    {PixelFormatModifier::kIntelI915YfTiled, "INTEL_YF"},
    {PixelFormatModifier::kIntelI915YTiledCcs, "INTEL_Y_CCS"},
    {PixelFormatModifier::kIntelI915YfTiledCcs, "INTEL_YF_CCS"},
    // X-tiled CCS isn't a named modifier and isn't supported.
    {PixelFormatModifier{0x0100000001000001ull}, "INTEL_X_CCS"},
    {PixelFormatModifier{0x0100000000000004ull}, "INTEL_UNKNOWN_TILING"},
    {PixelFormatModifier::kArmAfbc16X16, "AFBC_16X16"},
    {PixelFormatModifier::kArmAfbc32X8, "AFBC_32X8"},
    {PixelFormatModifier::kArmLinearTe, "ARM_LINEAR_TE"},
    {PixelFormatModifier::kArmAfbc16X16Te, "AFBC_16X16_TE"},
    {PixelFormatModifier::kArmAfbc32X8Te, "AFBC_32X8_TE"},
    {PixelFormatModifier::kArmAfbc16X16YuvTiledHeader, "AFBC_16X16_YUV_TH"},
    {PixelFormatModifier::kArmAfbc16X16SplitBlockSparseYuv, "AFBC_16X16_SB_SP_YUV"},
    {PixelFormatModifier::kArmAfbc16X16SplitBlockSparseYuvTe, "AFBC_16X16_SB_SP_YUV_TE"},
    {PixelFormatModifier::kArmAfbc16X16SplitBlockSparseYuvTiledHeader, "AFBC_16X16_SB_SP_YUV_TH"},
    {PixelFormatModifier::kArmAfbc16X16SplitBlockSparseYuvTeTiledHeader,
     "AFBC_16X16_SB_SP_YUV_TE_TH"},
    // Un-named combinations of AFBC bits.
    {PixelFormatModifier{0x0800000000002002ull}, "AFBC_32X8_TH"},
    {PixelFormatModifier{0x0800000000003002ull}, "AFBC_32X8_TE_TH"},
    {PixelFormatModifier{0x0800000000000801ull}, "AFBC_16X16_BCH"},
    // The ARM linear base, with and without AFBC bits (neither is AFBC or ARM_LINEAR_TE).
    {PixelFormatModifier{0x0800000000001010ull}, "ARM_LINEAR_TE_YUV"},
    {PixelFormatModifier{0x0800000000000000ull}, "ARM_LINEAR"},
    // Not a known AFBC block size.
    {PixelFormatModifier{0x0800000000000003ull}, "AFBC_UNKNOWN_BLOCK"},
    {PixelFormatModifier::kGoogleGoldfishOptimal, "GOLDFISH"},
    {PixelFormatModifier::kPvrFbcdc8X8V14, "PVR_FBCDC_8X8_V14"},
    {PixelFormatModifier::kPvrFbcdc16X4V14, "PVR_FBCDC_16X4_V14"},
    {PixelFormatModifier{0x9200000000000001ull}, "PVR_UNKNOWN"},
    {PixelFormatModifier::kDoNotCare, "MOD_DO_NOT_CARE"},
    {PixelFormatModifier::kInvalid, "MOD_INVALID"},
    {kUnknownModifier, "UNKNOWN_VENDOR"},
};

struct NamedColorSpace {
  ColorSpace value;
  const char* name;
};

const NamedColorSpace kAllColorSpaces[] = {
    {ColorSpace::kInvalid, "INVALID"},
    {ColorSpace::kSrgb, "SRGB"},
    {ColorSpace::kRec601Ntsc, "REC601_NTSC"},
    {ColorSpace::kRec601NtscFullRange, "REC601_NTSC_FULL_RANGE"},
    {ColorSpace::kRec601Pal, "REC601_PAL"},
    {ColorSpace::kRec601PalFullRange, "REC601_PAL_FULL_RANGE"},
    {ColorSpace::kRec709, "REC709"},
    {ColorSpace::kRec2020, "REC2020"},
    {ColorSpace::kRec2100, "REC2100"},
    {ColorSpace::kPassthrough, "PASSTHROUGH"},
    {ColorSpace::kDoNotCare, "DO_NOT_CARE"},
    {kUnknownColorSpace, "UNKNOWN_CS_100"},
};

// FNV-1a over a sequence of 64-bit values, recorded as `all=<16-hex>` on each line of the
// `image-layout` and `constraints` goldens so every point in the grid is checked while keeping each
// golden line to one readable 1920x1080 sample:
//  * `ImageLayout` hashes, for each entry in `kImageSizes`, `image_size` followed by `offset` and
//    `plane_row_bytes` for each plane in 0..kPlaneCountToCheck-1 (each `CheckedNumeric` adds its
//    `IsValid()` flag followed by its value or 0).
//  * `Constraints` hashes, for each entry in `kConstraintsVariants`,
//    `ImageFormatMinimumRowBytesChecked` across `kWidths` followed by `ImageConstraintsToFormat`
//    `is_ok()` and (when ok) `AddImageFormat()` across `kToFormatSizes`.
//
// To localize an `all=` mismatch without instrumenting the test, `ImageLayout` and `Constraints`
// write uncompared per-case `.txt` files (`image-format-coverage-image-layout-cases.txt` and
// `image-format-coverage-constraints-cases.txt`) to `/custom_artifacts` on every run. Run
// `image-format-test` (C++) and the alternate implementation's test with `fx test --outdir <dir>`
// and diff the two `.txt` files to see the exact input and result that differed.
class Fingerprint {
 public:
  void Add(uint64_t value) {
    for (unsigned i = 0; i < 8; ++i) {
      hash_ ^= (value >> (i * 8)) & 0xFFu;
      hash_ *= 0x100000001b3ull;
    }
  }
  template <typename T>
  void AddChecked(CheckedNumeric<T> value) {
    Add(value.IsValid() ? 1 : 0);
    Add(value.IsValid() ? static_cast<uint64_t>(value.ValueOrDie()) : 0);
  }
  uint64_t value() const { return hash_; }

 private:
  uint64_t hash_ = 0xcbf29ce484222325ull;
};

template <typename T>
std::string CheckedToString(CheckedNumeric<T> value) {
  if (!value.IsValid()) {
    return "-";
  }
  return std::to_string(static_cast<uint64_t>(value.ValueOrDie()));
}

std::string Format(const char* format, ...) __PRINTFLIKE(1, 2);
std::string Format(const char* format, ...) {
  va_list args;
  va_start(args, format);
  fbl::String formatted = fbl::StringVPrintf(format, args);
  va_end(args);
  return std::string(formatted.data(), formatted.size());
}

// Writes uncompared per-case output to /custom_artifacts/<filename> so `fx test --outdir <dir>`
// collects it for diffing across implementations when an `all=` fingerprint mismatches.
void WriteCustomArtifactFile(const char* filename, std::string_view contents) {
  std::string path = std::string("/custom_artifacts/") + filename;
  FILE* file = fopen(path.c_str(), "w");
  if (file == nullptr) {
    printf("Couldn't write %s\n", path.c_str());
    return;
  }
  size_t written = fwrite(contents.data(), 1, contents.size(), file);
  if (written != contents.size()) {
    printf("Short write to %s (%zu of %zu bytes)\n", path.c_str(), written, contents.size());
  }
  fclose(file);
}

// Writes |lines| as a replacement golden file to /custom_artifacts, where `fx test` collects it
// (see update-coverage-goldens.py).
void WriteActualGolden(const char* name, const std::vector<std::string>& lines) {
  std::string path = std::string("/custom_artifacts/image-format-coverage-golden-") + name + ".inc";
  FILE* file = fopen(path.c_str(), "w");
  if (file == nullptr) {
    printf("Couldn't write %s\n", path.c_str());
    return;
  }
  fprintf(file,
          "// Copyright 2026 The Fuchsia Authors. All rights reserved.\n"
          "// Use of this source code is governed by a BSD-style license that can be\n"
          "// found in the LICENSE file.\n"
          "\n"
          "// Generated from the C++ image-format implementation; see "
          "image-format-coverage-test.cc.\n"
          "\n"
          "// clang-format off\n");
  for (const auto& line : lines) {
    fprintf(file, "    \"%s\",\n", line.c_str());
  }
  fprintf(file, "// clang-format on\n");
  fclose(file);
  printf("Wrote the actual %s values to %s\n", name, path.c_str());
}

// Compares |actual| to |golden| line by line. On mismatch, reports the first few differences and
// writes the full |actual| set of lines as a replacement golden file.
void ExpectMatchesGolden(const char* name, const std::vector<std::string>& actual,
                         std::span<const std::string_view> golden) {
  size_t mismatch_count = 0;
  size_t max_lines = std::max(actual.size(), golden.size());
  for (size_t i = 0; i < max_lines; ++i) {
    std::string_view actual_line = i < actual.size() ? std::string_view(actual[i]) : "<missing>";
    std::string_view golden_line = i < golden.size() ? golden[i] : "<missing>";
    if (actual_line != golden_line) {
      if (mismatch_count < 10) {
        ADD_FAILURE("%s golden mismatch at line %zu:\n  actual: %.*s\n  golden: %.*s", name, i,
                    static_cast<int>(actual_line.size()), actual_line.data(),
                    static_cast<int>(golden_line.size()), golden_line.data());
      }
      ++mismatch_count;
    }
  }
  if (mismatch_count != 0) {
    ADD_FAILURE("%s: %zu mismatched lines (actual %zu lines, golden %zu lines)", name,
                mismatch_count, actual.size(), golden.size());
    WriteActualGolden(name, actual);
  }
}

constexpr std::string_view kGoldenPixelFormatProperties[] = {
#include "image-format-coverage-golden-pixel-format-properties.inc"
};

constexpr std::string_view kGoldenColorSpaces[] = {
#include "image-format-coverage-golden-color-spaces.inc"
};

constexpr std::string_view kGoldenZbi[] = {
#include "image-format-coverage-golden-zbi.inc"
};

constexpr std::string_view kGoldenImageLayout[] = {
#include "image-format-coverage-golden-image-layout.inc"
};

constexpr std::string_view kGoldenConstraints[] = {
#include "image-format-coverage-golden-constraints.inc"
};

TEST(ImageFormatCoverage, PixelFormatProperties) {
  std::vector<std::string> lines;
  for (const auto& pf : kAllPixelFormats) {
    for (const auto& mod : kAllModifiers) {
      SCOPED_TRACE(Format("%s %s", pf.name, mod.name));
      PixelFormatAndModifier pfm(pf.value, mod.value);
      bool supported = ImageFormatIsSupported(pfm);
      std::string line =
          Format("%s %s: supported=%d protected=%d non_tiled_single_plane=%d", pf.name, mod.name,
                 supported, ImageFormatCompatibleWithProtectedMemory(pfm),
                 ImageFormatIsNonTiledSinglePlane(pfm));
      if (supported) {
        line +=
            Format(" bpp=%u stride=%u width_div=%u height_div=%u align=%u row_div=%u",
                   ImageFormatBitsPerPixel(pfm), ImageFormatStrideBytesPerWidthPixel(pfm),
                   ImageFormatSurfaceWidthMinDivisor(pfm), ImageFormatSurfaceHeightMinDivisor(pfm),
                   ImageFormatSampleAlignment(pfm), ImageFormatRowBytesMinDivisor(pfm));
      }
      lines.push_back(std::move(line));

      // The equality check only looks at pixel_format and pixel_format_modifier.
      EXPECT_TRUE(ImageFormatIsPixelFormatEqual(pfm, pfm));
      PixelFormatAndModifier other_modifier(pf.value, mod.value == PixelFormatModifier::kLinear
                                                          ? PixelFormatModifier::kIntelI915XTiled
                                                          : PixelFormatModifier::kLinear);
      EXPECT_FALSE(ImageFormatIsPixelFormatEqual(pfm, other_modifier));
    }
  }
  ExpectMatchesGolden("pixel-format-properties", lines, kGoldenPixelFormatProperties);
}

TEST(ImageFormatCoverage, PixelFormatPropertiesV1Consistency) {
  // For pixel formats that can be represented in V1, the V1 overloads should agree with the V2
  // overloads.
  size_t checked_count = 0;
  for (const auto& pf : kAllPixelFormats) {
    for (const auto& mod : kAllModifiers) {
      SCOPED_TRACE(Format("%s %s", pf.name, mod.name));
      PixelFormatAndModifier pfm(pf.value, mod.value);
      if (pf.value.IsUnknown()) {
        continue;
      }
      auto v1 = sysmem::V1CopyFromV2PixelFormat(pfm);
      auto round_trip = sysmem::V2CopyFromV1PixelFormat(v1);
      if (!ImageFormatIsPixelFormatEqual(round_trip, pfm)) {
        // Not representable in V1.
        continue;
      }
      fidl::Arena arena;
      auto v1_wire = fidl::ToWire(arena, v1);
      ++checked_count;
      bool supported = ImageFormatIsSupported(pfm);
      EXPECT_EQ(supported, ImageFormatIsSupported(v1_wire));
      EXPECT_EQ(ImageFormatCompatibleWithProtectedMemory(pfm),
                ImageFormatCompatibleWithProtectedMemory(v1_wire));
      EXPECT_TRUE(ImageFormatIsPixelFormatEqual(v1_wire, v1_wire));
      if (supported) {
        EXPECT_EQ(ImageFormatBitsPerPixel(pfm), ImageFormatBitsPerPixel(v1_wire));
        EXPECT_EQ(ImageFormatStrideBytesPerWidthPixel(pfm),
                  ImageFormatStrideBytesPerWidthPixel(v1_wire));
        EXPECT_EQ(ImageFormatSurfaceWidthMinDivisor(pfm), ImageFormatCodedWidthMinDivisor(v1_wire));
        EXPECT_EQ(ImageFormatSurfaceHeightMinDivisor(pfm),
                  ImageFormatCodedHeightMinDivisor(v1_wire));
        EXPECT_EQ(ImageFormatSampleAlignment(pfm), ImageFormatSampleAlignment(v1_wire));
        EXPECT_EQ(ImageFormatRowBytesMinDivisor(pfm), ImageFormatRowBytesMinDivisor(v1_wire));
      }
    }
  }
  // Guard against every case being skipped as not representable in V1.
  EXPECT_GT(checked_count, 0u);
}

TEST(ImageFormatCoverage, ColorSpaces) {
  std::vector<std::string> lines;
  for (const auto& cs : kAllColorSpaces) {
    std::string line = Format("%s:", cs.name);
    for (const auto& pf : kAllPixelFormats) {
      SCOPED_TRACE(Format("%s %s", cs.name, pf.name));
      PixelFormatAndModifier pfm(pf.value, PixelFormatModifier::kLinear);
      bool compatible = ImageFormatIsSupportedColorSpaceForPixelFormat(cs.value, pfm);
      line += compatible ? " 1" : " 0";
      // The modifier is ignored.
      for (const auto& mod : kAllModifiers) {
        SCOPED_TRACE(mod.name);
        PixelFormatAndModifier other(pf.value, mod.value);
        EXPECT_EQ(compatible, ImageFormatIsSupportedColorSpaceForPixelFormat(cs.value, other));
      }
      // V1 overload agrees where the color space and pixel format are representable in V1.
      if (!pf.value.IsUnknown() && !cs.value.IsUnknown()) {
        auto v1_pixel_format = sysmem::V1CopyFromV2PixelFormat(pfm);
        auto v1_color_space = sysmem::V1CopyFromV2ColorSpace(cs.value);
        if (ImageFormatIsPixelFormatEqual(sysmem::V2CopyFromV1PixelFormat(v1_pixel_format), pfm) &&
            sysmem::V2CopyFromV1ColorSpace(v1_color_space) == cs.value) {
          fidl::Arena arena;
          EXPECT_EQ(compatible,
                    ImageFormatIsSupportedColorSpaceForPixelFormat(
                        fidl::ToWire(arena, v1_color_space), fidl::ToWire(arena, v1_pixel_format)));
        }
      }
    }
    lines.push_back(std::move(line));
  }
  ExpectMatchesGolden("color-spaces", lines, kGoldenColorSpaces);
}

TEST(ImageFormatCoverage, ZbiPixelFormats) {
  std::vector<std::string> lines;
  const zbi_pixel_format_t kZbiFormats[] = {
      ZBI_PIXEL_FORMAT_NONE,
      ZBI_PIXEL_FORMAT_RGB_565,
      ZBI_PIXEL_FORMAT_RGB_332,
      ZBI_PIXEL_FORMAT_RGB_2220,
      ZBI_PIXEL_FORMAT_ARGB_8888,
      ZBI_PIXEL_FORMAT_RGB_X888,
      ZBI_PIXEL_FORMAT_MONO_8,
      ZBI_PIXEL_FORMAT_NV12,
      ZBI_PIXEL_FORMAT_I420,
      ZBI_PIXEL_FORMAT_RGB_888,
      ZBI_PIXEL_FORMAT_ABGR_8888,
      ZBI_PIXEL_FORMAT_BGR_888_X,
      ZBI_PIXEL_FORMAT_ARGB_2_10_10_10,
      ZBI_PIXEL_FORMAT_ABGR_2_10_10_10,
      // Not defined.
      0x00010001u,
      0x0004000eu,
      0xFFFFFFFFu,
  };
  for (zbi_pixel_format_t zbi_format : kZbiFormats) {
    auto result = ImageFormatConvertZbiToSysmemPixelFormat_v2(zbi_format);
    if (result.is_ok()) {
      lines.push_back(Format("0x%08x: %u", zbi_format, fidl::ToUnderlying(result.value())));
    } else {
      lines.push_back(Format("0x%08x: error", zbi_format));
    }
  }
  ExpectMatchesGolden("zbi", lines, kGoldenZbi);
}

struct ImageSize {
  uint32_t width;
  uint32_t height;
  uint32_t bytes_per_row;
};

const ImageSize kImageSizes[] = {
    {0, 0, 0},
    {1, 1, 4},
    {17, 13, 68},
    {64, 64, 256},
    {1920, 1080, 7680},
    {3440, 1440, 13760},
    {4096, 4096, 16384},
    {65535, 65535, 262144},
    {1, 0xFFFFFFFF, 0x80000000},
    {0x80000000, 2, 0xFFFFFFFF},
    {0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF},
    {0xFFFFFFFF, 1, 0xFFFFFF80},
};
// Planes 0..3 are the real planes (up to 3 YUV planes, plus plane 3 for auxiliary/header planes);
// plane 4 checks the out-of-range result.
constexpr uint32_t kPlaneCountToCheck = 5;

TEST(ImageFormatCoverage, ImageLayout) {
  std::vector<std::string> lines;
  std::string case_dump;
  case_dump.reserve(size_t{192} * 1024);
  for (const auto& pf : kAllPixelFormats) {
    for (const auto& mod : kAllModifiers) {
      SCOPED_TRACE(Format("%s %s", pf.name, mod.name));
      PixelFormatAndModifier pfm(pf.value, mod.value);
      if (!ImageFormatIsSupported(pfm)) {
        fuchsia_images2::ImageFormat unsupported_format;
        unsupported_format.pixel_format() = pf.value;
        unsupported_format.pixel_format_modifier() = mod.value;
        unsupported_format.size() = {1920, 1080};
        unsupported_format.bytes_per_row() = 7680;
        fidl::Arena arena;
        auto wire_unsupported_format = fidl::ToWire(arena, unsupported_format);
        for (uint32_t plane = 0; plane < kPlaneCountToCheck; ++plane) {
          SCOPED_TRACE(Format("plane=%u", plane));
          EXPECT_FALSE(ImageFormatPlaneByteOffsetChecked(unsupported_format, plane).IsValid());
          EXPECT_FALSE(ImageFormatPlaneByteOffsetChecked(wire_unsupported_format, plane).IsValid());
          uint64_t offset_out = 0;
          EXPECT_FALSE(ImageFormatPlaneByteOffset(unsupported_format, plane, &offset_out));
          EXPECT_FALSE(ImageFormatPlaneByteOffset(wire_unsupported_format, plane, &offset_out));
          EXPECT_FALSE(ImageFormatPlaneRowBytesChecked(unsupported_format, plane).IsValid());
          EXPECT_FALSE(ImageFormatPlaneRowBytesChecked(wire_unsupported_format, plane).IsValid());
          uint32_t row_bytes_out = 0;
          EXPECT_FALSE(ImageFormatPlaneRowBytes(unsupported_format, plane, &row_bytes_out));
          EXPECT_FALSE(ImageFormatPlaneRowBytes(wire_unsupported_format, plane, &row_bytes_out));
        }
        // ImageFormatImageSize*() aborts for unsupported formats (tested in
        // DeathUnsupportedImageSize).
        continue;
      }
      Fingerprint fingerprint;
      std::string reported;
      for (const auto& size : kImageSizes) {
        SCOPED_TRACE(Format("%ux%u bpr=%u", size.width, size.height, size.bytes_per_row));
        fuchsia_images2::ImageFormat image_format;
        image_format.pixel_format() = pf.value;
        image_format.pixel_format_modifier() = mod.value;
        image_format.size() = {size.width, size.height};
        image_format.bytes_per_row() = size.bytes_per_row;
        fidl::Arena arena;
        auto wire_image_format = fidl::ToWire(arena, image_format);

        auto image_size = ImageFormatImageSizeChecked(image_format);
        fingerprint.AddChecked(image_size);
        EXPECT_EQ(image_size.IsValid(), ImageFormatImageSizeChecked(wire_image_format).IsValid());
        if (image_size.IsValid()) {
          EXPECT_EQ(image_size.ValueOrDie(), ImageFormatImageSize(image_format));
          EXPECT_EQ(image_size.ValueOrDie(), ImageFormatImageSize(wire_image_format));
          EXPECT_EQ(image_size.ValueOrDie(),
                    ImageFormatImageSizeChecked(wire_image_format).ValueOrDie());
        }

        std::string offsets;
        std::string row_bytes;
        for (uint32_t plane = 0; plane < kPlaneCountToCheck; ++plane) {
          SCOPED_TRACE(Format("plane=%u", plane));
          auto offset = ImageFormatPlaneByteOffsetChecked(image_format, plane);
          fingerprint.AddChecked(offset);
          auto offset_wire = ImageFormatPlaneByteOffsetChecked(wire_image_format, plane);
          EXPECT_EQ(CheckedToString(offset), CheckedToString(offset_wire));
          uint64_t offset_out = 0;
          EXPECT_EQ(offset.IsValid(), ImageFormatPlaneByteOffset(image_format, plane, &offset_out));
          if (offset.IsValid()) {
            EXPECT_EQ(offset.ValueOrDie(), offset_out);
          }
          uint64_t offset_wire_out = 0;
          EXPECT_EQ(offset.IsValid(),
                    ImageFormatPlaneByteOffset(wire_image_format, plane, &offset_wire_out));
          if (offset.IsValid()) {
            EXPECT_EQ(offset.ValueOrDie(), offset_wire_out);
          }

          auto plane_row_bytes = ImageFormatPlaneRowBytesChecked(image_format, plane);
          fingerprint.AddChecked(plane_row_bytes);
          EXPECT_EQ(CheckedToString(plane_row_bytes),
                    CheckedToString(ImageFormatPlaneRowBytesChecked(wire_image_format, plane)));
          uint32_t row_bytes_out = 0;
          EXPECT_EQ(plane_row_bytes.IsValid(),
                    ImageFormatPlaneRowBytes(image_format, plane, &row_bytes_out));
          if (plane_row_bytes.IsValid()) {
            EXPECT_EQ(plane_row_bytes.ValueOrDie(), row_bytes_out);
          }
          uint32_t row_bytes_wire_out = 0;
          EXPECT_EQ(plane_row_bytes.IsValid(),
                    ImageFormatPlaneRowBytes(wire_image_format, plane, &row_bytes_wire_out));
          if (plane_row_bytes.IsValid()) {
            EXPECT_EQ(plane_row_bytes.ValueOrDie(), row_bytes_wire_out);
          }

          offsets += (plane == 0 ? "" : ",") + CheckedToString(offset);
          row_bytes += (plane == 0 ? "" : ",") + CheckedToString(plane_row_bytes);
        }
        std::string summary =
            Format("%ux%u size=%s offsets=[%s] row_bytes=[%s]", size.width, size.height,
                   CheckedToString(image_size).c_str(), offsets.c_str(), row_bytes.c_str());
        case_dump +=
            Format("%s %s bpr=%u %s\n", pf.name, mod.name, size.bytes_per_row, summary.c_str());
        if (size.width == 1920 && size.height == 1080) {
          reported = std::move(summary);
        }
      }
      lines.push_back(Format("%s %s: %s all=%016" PRIx64, pf.name, mod.name, reported.c_str(),
                             fingerprint.value()));
    }
  }
  WriteCustomArtifactFile("image-format-coverage-image-layout-cases.txt", case_dump);
  ExpectMatchesGolden("image-layout", lines, kGoldenImageLayout);
}

TEST(ImageFormatCoverage, ImageLayoutV1Consistency) {
  // For image formats that can be represented in V1, the V1 overloads should agree with the V2
  // overloads.
  size_t checked_count = 0;
  for (const auto& pf : kAllPixelFormats) {
    for (const auto& mod : kAllModifiers) {
      SCOPED_TRACE(Format("%s %s", pf.name, mod.name));
      PixelFormatAndModifier pfm(pf.value, mod.value);
      if (!ImageFormatIsSupported(pfm) || pf.value.IsUnknown()) {
        continue;
      }
      for (const auto& size : kImageSizes) {
        SCOPED_TRACE(Format("%ux%u bpr=%u", size.width, size.height, size.bytes_per_row));
        fuchsia_images2::ImageFormat image_format;
        image_format.pixel_format() = pf.value;
        image_format.pixel_format_modifier() = mod.value;
        image_format.color_space() = ColorSpace::kSrgb;
        image_format.size() = {size.width, size.height};
        image_format.bytes_per_row() = size.bytes_per_row;
        auto v1_result = sysmem::V1CopyFromV2ImageFormat(image_format);
        if (v1_result.is_error()) {
          continue;
        }
        auto round_trip = sysmem::V2CopyFromV1ImageFormat(v1_result.value());
        if (round_trip.is_error()) {
          continue;
        }
        // General V1-to-V2 conversion leaves bytes_per_row unset when 0, whereas image-format's V1
        // layout overloads populate bytes_per_row from the V1 struct even when 0.
        round_trip.value().bytes_per_row() = v1_result.value().bytes_per_row();
        if (!ImageFormatIsPixelFormatEqual(
                PixelFormatAndModifierFromImageFormat(round_trip.value()), pfm) ||
            round_trip.value().bytes_per_row() != image_format.bytes_per_row()) {
          continue;
        }
        fidl::Arena arena;
        auto v1_wire = fidl::ToWire(arena, v1_result.value());
        ++checked_count;

        auto image_size = ImageFormatImageSizeChecked(round_trip.value());
        EXPECT_EQ(CheckedToString(image_size),
                  CheckedToString(ImageFormatImageSizeChecked(v1_wire)));
        if (image_size.IsValid()) {
          EXPECT_EQ(image_size.ValueOrDie(), ImageFormatImageSize(v1_wire));
        }
        for (uint32_t plane = 0; plane < kPlaneCountToCheck; ++plane) {
          SCOPED_TRACE(Format("plane=%u", plane));
          auto offset = ImageFormatPlaneByteOffsetChecked(round_trip.value(), plane);
          EXPECT_EQ(CheckedToString(offset),
                    CheckedToString(ImageFormatPlaneByteOffsetChecked(v1_wire, plane)));
          uint64_t offset_out = 0;
          EXPECT_EQ(offset.IsValid(), ImageFormatPlaneByteOffset(v1_wire, plane, &offset_out));
          if (offset.IsValid()) {
            EXPECT_EQ(offset.ValueOrDie(), offset_out);
          }
          auto plane_row_bytes = ImageFormatPlaneRowBytesChecked(round_trip.value(), plane);
          EXPECT_EQ(CheckedToString(plane_row_bytes),
                    CheckedToString(ImageFormatPlaneRowBytesChecked(v1_wire, plane)));
          uint32_t row_bytes_out = 0;
          EXPECT_EQ(plane_row_bytes.IsValid(),
                    ImageFormatPlaneRowBytes(v1_wire, plane, &row_bytes_out));
          if (plane_row_bytes.IsValid()) {
            EXPECT_EQ(plane_row_bytes.ValueOrDie(), row_bytes_out);
          }
        }
      }
    }
  }
  // Guard against every case being skipped as not representable in V1.
  EXPECT_GT(checked_count, 0u);
}

// A set of constraints to apply on top of a pixel format and modifier.
struct ConstraintsVariant {
  const char* name;
  std::optional<fuchsia_math::SizeU> min_size;
  std::optional<fuchsia_math::SizeU> max_size;
  std::optional<fuchsia_math::SizeU> size_alignment;
  std::optional<uint32_t> min_bytes_per_row;
  std::optional<uint32_t> max_bytes_per_row;
  std::optional<uint32_t> bytes_per_row_divisor;
  std::vector<ColorSpace> color_spaces;
};

const ConstraintsVariant kConstraintsVariants[] = {
    {.name = "none"},
    {.name = "typical",
     .min_size = fuchsia_math::SizeU{12, 12},
     .max_size = fuchsia_math::SizeU{100, 100},
     .max_bytes_per_row = 100000,
     .bytes_per_row_divisor = 32},
    {.name = "align",
     .size_alignment = fuchsia_math::SizeU{8, 2},
     .min_bytes_per_row = 1000,
     .bytes_per_row_divisor = 3},
    {.name = "divisor_zero", .bytes_per_row_divisor = 0},
    {.name = "alignment_zero", .size_alignment = fuchsia_math::SizeU{0, 0}},
    {.name = "small_max_row",
     .min_size = fuchsia_math::SizeU{1, 1},
     .max_size = fuchsia_math::SizeU{0xFFFFFFFF, 0xFFFFFFFF},
     .max_bytes_per_row = 64},
    {.name = "color_spaces",
     .size_alignment = fuchsia_math::SizeU{2, 2},
     .color_spaces = {ColorSpace::kRec709, ColorSpace::kSrgb}},
    {.name = "huge_min_row", .min_bytes_per_row = 0xFFFFFFFF, .bytes_per_row_divisor = 64},
    {.name = "odd_divisor", .max_bytes_per_row = 0xFFFFFFFF, .bytes_per_row_divisor = 7},
};

fuchsia_sysmem2::ImageFormatConstraints MakeConstraints(PixelFormat pixel_format,
                                                        PixelFormatModifier modifier,
                                                        const ConstraintsVariant& variant) {
  fuchsia_sysmem2::ImageFormatConstraints constraints;
  constraints.pixel_format() = pixel_format;
  constraints.pixel_format_modifier() = modifier;
  constraints.min_size() = variant.min_size;
  constraints.max_size() = variant.max_size;
  constraints.size_alignment() = variant.size_alignment;
  constraints.min_bytes_per_row() = variant.min_bytes_per_row;
  constraints.max_bytes_per_row() = variant.max_bytes_per_row;
  constraints.bytes_per_row_divisor() = variant.bytes_per_row_divisor;
  if (!variant.color_spaces.empty()) {
    constraints.color_spaces() = variant.color_spaces;
  }
  return constraints;
}

const uint32_t kWidths[] = {0, 1, 17, 128, 1920, 0x40000000, 0xFFFFFFFF};

const fuchsia_math::SizeU kToFormatSizes[] = {
    {0, 1}, {1, 1}, {16, 14}, {17, 13}, {128, 64}, {1920, 1080}, {0xFFFFFFFF, 1},
};

void AddImageFormat(Fingerprint& fingerprint, const fuchsia_images2::ImageFormat& image_format) {
  fingerprint.Add(image_format.pixel_format().has_value());
  fingerprint.Add(fidl::ToUnderlying(image_format.pixel_format().value_or(PixelFormat::kInvalid)));
  fingerprint.Add(image_format.pixel_format_modifier().has_value());
  fingerprint.Add(fidl::ToUnderlying(
      image_format.pixel_format_modifier().value_or(PixelFormatModifier::kLinear)));
  fingerprint.Add(image_format.color_space().has_value());
  fingerprint.Add(fidl::ToUnderlying(image_format.color_space().value_or(ColorSpace::kInvalid)));
  fingerprint.Add(image_format.size().has_value());
  if (image_format.size().has_value()) {
    fingerprint.Add(image_format.size()->width());
    fingerprint.Add(image_format.size()->height());
  }
  fingerprint.Add(image_format.bytes_per_row().has_value());
  fingerprint.Add(image_format.bytes_per_row().value_or(0));
  fingerprint.Add(image_format.display_rect().has_value());
  if (image_format.display_rect().has_value()) {
    fingerprint.Add(image_format.display_rect()->x());
    fingerprint.Add(image_format.display_rect()->y());
    fingerprint.Add(image_format.display_rect()->width());
    fingerprint.Add(image_format.display_rect()->height());
  }
  fingerprint.Add(image_format.valid_size().has_value());
  fingerprint.Add(image_format.pixel_aspect_ratio().has_value());
}

// Formats every field hashed by `AddImageFormat()` so any fingerprint difference shows up in the
// per-case `.txt` dump.
std::string FormatImageFormat(const fuchsia_images2::ImageFormat& image_format) {
  std::string out = image_format.pixel_format().has_value()
                        ? Format("pf=%u", fidl::ToUnderlying(*image_format.pixel_format()))
                        : "pf=-";
  out += image_format.pixel_format_modifier().has_value()
             ? Format(" mod=0x%" PRIx64, fidl::ToUnderlying(*image_format.pixel_format_modifier()))
             : " mod=-";
  out += image_format.color_space().has_value()
             ? Format(" cs=%u", fidl::ToUnderlying(*image_format.color_space()))
             : " cs=-";
  out += image_format.size().has_value()
             ? Format(" size=%ux%u", image_format.size()->width(), image_format.size()->height())
             : " size=-";
  out += image_format.bytes_per_row().has_value() ? Format(" bpr=%u", *image_format.bytes_per_row())
                                                  : " bpr=-";
  out += image_format.display_rect().has_value()
             ? Format(" display_rect=%u,%u,%ux%u", image_format.display_rect()->x(),
                      image_format.display_rect()->y(), image_format.display_rect()->width(),
                      image_format.display_rect()->height())
             : " display_rect=-";
  out += Format(" valid_size=%d pixel_aspect_ratio=%d", image_format.valid_size().has_value(),
                image_format.pixel_aspect_ratio().has_value());
  return out;
}

TEST(ImageFormatCoverage, Constraints) {
  std::vector<std::string> lines;
  std::string case_dump;
  case_dump.reserve(size_t{1024} * 1024);
  for (const auto& pf : kAllPixelFormats) {
    for (const auto& mod : kAllModifiers) {
      SCOPED_TRACE(Format("%s %s", pf.name, mod.name));
      PixelFormatAndModifier pfm(pf.value, mod.value);
      bool supported = ImageFormatIsSupported(pfm);
      Fingerprint fingerprint;
      std::string reported;
      for (const auto& variant : kConstraintsVariants) {
        SCOPED_TRACE(variant.name);
        auto constraints = MakeConstraints(pf.value, mod.value, variant);
        fidl::Arena arena;
        auto wire_constraints = fidl::ToWire(arena, constraints);
        for (uint32_t width : kWidths) {
          SCOPED_TRACE(Format("width=%u", width));
          auto row_bytes = ImageFormatMinimumRowBytesChecked(constraints, width);
          if (!supported) {
            EXPECT_FALSE(row_bytes.IsValid());
          }
          fingerprint.AddChecked(row_bytes);
          if (supported) {
            case_dump += Format("%s %s variant=%s min_row_bytes(%u)=%s\n", pf.name, mod.name,
                                variant.name, width, CheckedToString(row_bytes).c_str());
          }
          EXPECT_EQ(CheckedToString(row_bytes),
                    CheckedToString(ImageFormatMinimumRowBytesChecked(wire_constraints, width)));
          if (width != 0) {
            uint32_t row_bytes_out = 0;
            EXPECT_EQ(row_bytes.IsValid(),
                      ImageFormatMinimumRowBytes(constraints, width, &row_bytes_out));
            if (row_bytes.IsValid()) {
              EXPECT_EQ(row_bytes.ValueOrDie(), row_bytes_out);
            }
            uint32_t row_bytes_wire_out = 0;
            EXPECT_EQ(row_bytes.IsValid(),
                      ImageFormatMinimumRowBytes(wire_constraints, width, &row_bytes_wire_out));
            if (row_bytes.IsValid()) {
              EXPECT_EQ(row_bytes.ValueOrDie(), row_bytes_wire_out);
            }
          }
          if (&variant == &kConstraintsVariants[0] && width == 1920) {
            reported = Format("min_row_bytes(1920)=%s", CheckedToString(row_bytes).c_str());
          }
        }
        // An invalid width results in an invalid row bytes.
        EXPECT_FALSE(
            ImageFormatMinimumRowBytesChecked(constraints, CheckedNumeric<uint32_t>(0xFFFFFFFF) + 1)
                .IsValid());

        for (const auto& size : kToFormatSizes) {
          SCOPED_TRACE(Format("%ux%u", size.width(), size.height()));
          auto result = ImageConstraintsToFormat(constraints, size.width(), size.height());
          if (!supported) {
            EXPECT_FALSE(result.is_ok());
          }
          fingerprint.Add(result.is_ok());
          auto wire_result =
              ImageConstraintsToFormat(arena, wire_constraints, size.width(), size.height());
          EXPECT_EQ(result.is_ok(), wire_result.is_ok());
          if (!result.is_ok()) {
            if (supported) {
              case_dump += Format("%s %s variant=%s to_format(%ux%u)=error\n", pf.name, mod.name,
                                  variant.name, size.width(), size.height());
            }
            continue;
          }
          AddImageFormat(fingerprint, result.value());
          if (supported) {
            case_dump +=
                Format("%s %s variant=%s to_format(%ux%u)=ok %s\n", pf.name, mod.name, variant.name,
                       size.width(), size.height(), FormatImageFormat(result.value()).c_str());
          }
          if (wire_result.is_ok()) {
            EXPECT_TRUE(fidl::ToNatural(wire_result.value()) == result.value());
          }
          if (&variant == &kConstraintsVariants[0] && size.width() == 1920 &&
              size.height() == 1080) {
            reported += Format(" to_format(1920x1080).bytes_per_row=%u",
                               result.value().bytes_per_row().value());
          }
        }
      }
      if (!supported) {
        continue;
      }
      lines.push_back(Format("%s %s: %s all=%016" PRIx64, pf.name, mod.name, reported.c_str(),
                             fingerprint.value()));
    }
  }
  WriteCustomArtifactFile("image-format-coverage-constraints-cases.txt", case_dump);
  ExpectMatchesGolden("constraints", lines, kGoldenConstraints);
}

TEST(ImageFormatCoverage, ConstraintsV1Consistency) {
  // For constraints that can be represented in V1, the V1 overloads should agree with the V2
  // overloads.
  size_t checked_count = 0;
  for (const auto& pf : kAllPixelFormats) {
    for (const auto& mod : kAllModifiers) {
      SCOPED_TRACE(Format("%s %s", pf.name, mod.name));
      if (pf.value.IsUnknown()) {
        continue;
      }
      for (const auto& variant : kConstraintsVariants) {
        SCOPED_TRACE(variant.name);
        auto constraints = MakeConstraints(pf.value, mod.value, variant);
        if (!constraints.color_spaces().has_value()) {
          constraints.color_spaces() = {ColorSpace::kSrgb};
        }
        auto v1_result = sysmem::V1CopyFromV2ImageFormatConstraints(constraints);
        if (v1_result.is_error()) {
          continue;
        }
        auto round_trip = sysmem::V2CopyFromV1ImageFormatConstraints(v1_result.value());
        if (round_trip.is_error()) {
          continue;
        }
        fidl::Arena arena;
        auto v1_wire = fidl::ToWire(arena, v1_result.value());
        ++checked_count;
        for (uint32_t width : kWidths) {
          SCOPED_TRACE(Format("width=%u", width));
          auto row_bytes = ImageFormatMinimumRowBytesChecked(round_trip.value(), width);
          EXPECT_EQ(CheckedToString(row_bytes),
                    CheckedToString(ImageFormatMinimumRowBytesChecked(v1_wire, width)));
          if (width != 0) {
            uint32_t row_bytes_out = 0;
            EXPECT_EQ(row_bytes.IsValid(),
                      ImageFormatMinimumRowBytes(v1_wire, width, &row_bytes_out));
            if (row_bytes.IsValid()) {
              EXPECT_EQ(row_bytes.ValueOrDie(), row_bytes_out);
            }
          }
        }
        for (const auto& size : kToFormatSizes) {
          SCOPED_TRACE(Format("%ux%u", size.width(), size.height()));
          auto v2_result =
              ImageConstraintsToFormat(round_trip.value(), size.width(), size.height());
          auto v1_out_result = ImageConstraintsToFormat(v1_wire, size.width(), size.height());
          if (!v2_result.is_ok()) {
            EXPECT_FALSE(v1_out_result.is_ok());
            continue;
          }
          auto expected_v1 = sysmem::V1CopyFromV2ImageFormat(v2_result.value());
          EXPECT_EQ(expected_v1.is_ok(), v1_out_result.is_ok());
          if (expected_v1.is_ok() && v1_out_result.is_ok()) {
            EXPECT_TRUE(fidl::ToNatural(v1_out_result.value()) == expected_v1.value());
          }
        }
      }
    }
  }
  // Guard against every case being skipped as not representable in V1.
  EXPECT_GT(checked_count, 0u);
}

// Hand-computed expectations and death tests for specific behaviors, including the width of
// intermediate arithmetic (where overflow is detected) and whether invalid inputs return invalid /
// error vs. abort. Where an edge case goes beyond what <lib/image-format/image_format.h> promises,
// these tests pin the current C++ behavior so a drop-in alternate implementation matches C++ unless
// an intentional contract change is decided.

TEST(ImageFormatCoverage, LinearImageSizeOverflow) {
  fuchsia_images2::ImageFormat image_format;
  image_format.pixel_format() = PixelFormat::kB8G8R8A8;
  image_format.pixel_format_modifier() = PixelFormatModifier::kLinear;
  image_format.size() = {0xFFFFFFFF, 0xFFFFFFFF};
  image_format.bytes_per_row() = 0xFFFFFFFF;
  // At 4 bytes per pixel, width 0xFFFFFFFF requires more than 0xFFFFFFFF bytes_per_row.
  EXPECT_FALSE(ImageFormatImageSizeChecked(image_format).IsValid());

  // With a width whose 4-byte minimum row bytes fits in bytes_per_row, height * bytes_per_row is
  // computed in 64 bits and fits.
  image_format.size() = {0x3FFFFFFF, 0xFFFFFFFF};
  auto size = ImageFormatImageSizeChecked(image_format);
  ASSERT_TRUE(size.IsValid());
  EXPECT_EQ(0xFFFFFFFFull * 0xFFFFFFFFull, size.ValueOrDie());
  EXPECT_EQ(0xFFFFFFFFull * 0xFFFFFFFFull, ImageFormatImageSize(image_format));

  // NV12 has 1 stride byte per width pixel (so width 0xFFFFFFFF fits in bytes_per_row 0xFFFFFFFF),
  // and adds ceil(height / 2) chroma rows, which overflows 64 bits.
  image_format.pixel_format() = PixelFormat::kNv12;
  image_format.size() = {0xFFFFFFFF, 0xFFFFFFFF};
  EXPECT_FALSE(ImageFormatImageSizeChecked(image_format).IsValid());

  image_format.size() = {1u << 20, 1u << 20};
  image_format.bytes_per_row() = 1u << 20;
  size = ImageFormatImageSizeChecked(image_format);
  ASSERT_TRUE(size.IsValid());
  EXPECT_EQ((1ull << 40) * 3 / 2, size.ValueOrDie());
}

TEST(ImageFormatCoverage, IntelTileProductIs32Bit) {
  // Y-tiled: 128 bytes per row per tile, 32 rows per tile, 4096 bytes per tile. The per-plane
  // width_in_tiles * height_in_tiles * 4096 product is computed in 32 bits, so 2^20 tiles is
  // invalid even though the size would fit in 64 bits.
  fuchsia_images2::ImageFormat image_format;
  image_format.pixel_format() = PixelFormat::kB8G8R8A8;
  image_format.pixel_format_modifier() = PixelFormatModifier::kIntelI915YTiled;
  image_format.bytes_per_row() = (1u << 20) * 128;
  image_format.size() = {(1u << 20) * 32, 32};
  EXPECT_FALSE(ImageFormatImageSizeChecked(image_format).IsValid());

  // One tile fewer fits in 32 bits.
  image_format.bytes_per_row() = ((1u << 20) - 1) * 128;
  image_format.size() = {((1u << 20) - 1) * 32, 32};
  auto size = ImageFormatImageSizeChecked(image_format);
  ASSERT_TRUE(size.IsValid());
  EXPECT_EQ(uint64_t{(1u << 20) - 1} * 4096, size.ValueOrDie());
}

TEST(ImageFormatCoverage, AfbcMinimumRowBytesOverflow) {
  fuchsia_sysmem2::ImageFormatConstraints constraints;
  constraints.pixel_format() = PixelFormat::kB8G8R8A8;
  constraints.pixel_format_modifier() = PixelFormatModifier::kArmAfbc16X16;
  // ceil(0xFFFFFFFF / 16) * 16 * 4 = 2^34, which doesn't fit in 32 bits.
  EXPECT_FALSE(ImageFormatMinimumRowBytesChecked(constraints, 0xFFFFFFFF).IsValid());
  // 2^28 / 16 * 16 * 4 = 2^30.
  auto row_bytes = ImageFormatMinimumRowBytesChecked(constraints, 1u << 28);
  ASSERT_TRUE(row_bytes.IsValid());
  EXPECT_EQ(1u << 30, row_bytes.ValueOrDie());
  // Rounded up to the 16 pixel block width.
  row_bytes = ImageFormatMinimumRowBytesChecked(constraints, 17);
  ASSERT_TRUE(row_bytes.IsValid());
  EXPECT_EQ(32u * 4, row_bytes.ValueOrDie());

  // Tiled headers align the width to 128 pixels (16x16) or 256 pixels (32x8).
  constraints.pixel_format_modifier() = PixelFormatModifier::kArmAfbc16X16YuvTiledHeader;
  row_bytes = ImageFormatMinimumRowBytesChecked(constraints, 17);
  ASSERT_TRUE(row_bytes.IsValid());
  EXPECT_EQ(128u * 4, row_bytes.ValueOrDie());
  constraints.pixel_format_modifier() = PixelFormatModifier{0x0800000000002002ull};
  row_bytes = ImageFormatMinimumRowBytesChecked(constraints, 17);
  ASSERT_TRUE(row_bytes.IsValid());
  EXPECT_EQ(256u * 4, row_bytes.ValueOrDie());

  // AFBC applies size_alignment (before the min/max width checks), min_bytes_per_row,
  // lcm(bytes_per_row_divisor, block_row_bytes_divisor) and max_bytes_per_row like the other tiled
  // formats do.
  constraints.pixel_format_modifier() = PixelFormatModifier::kArmAfbc16X16;
  constraints.size_alignment() = {64, 64};
  row_bytes = ImageFormatMinimumRowBytesChecked(constraints, 17);
  ASSERT_TRUE(row_bytes.IsValid());
  EXPECT_EQ(64u * 4, row_bytes.ValueOrDie());
  // lcm(1000, 64) = 8000.
  constraints.bytes_per_row_divisor() = 1000;
  row_bytes = ImageFormatMinimumRowBytesChecked(constraints, 17);
  ASSERT_TRUE(row_bytes.IsValid());
  EXPECT_EQ(8000u, row_bytes.ValueOrDie());
  constraints.min_bytes_per_row() = 100000;
  row_bytes = ImageFormatMinimumRowBytesChecked(constraints, 17);
  ASSERT_TRUE(row_bytes.IsValid());
  EXPECT_EQ(104000u, row_bytes.ValueOrDie());
  constraints.max_bytes_per_row() = 103999;
  EXPECT_FALSE(ImageFormatMinimumRowBytesChecked(constraints, 17).IsValid());
  constraints.max_bytes_per_row().reset();
  // The least common multiple is computed in 32 bits.
  constraints.bytes_per_row_divisor() = 0xFFFFFFFF;
  EXPECT_FALSE(ImageFormatMinimumRowBytesChecked(constraints, 17).IsValid());
  constraints.bytes_per_row_divisor().reset();
  constraints.max_size() = {63, 63};
  EXPECT_FALSE(ImageFormatMinimumRowBytesChecked(constraints, 17).IsValid());
}

TEST(ImageFormatCoverage, LinearMinimumRowBytes) {
  fuchsia_sysmem2::ImageFormatConstraints constraints;
  constraints.pixel_format() = PixelFormat::kB8G8R8;
  constraints.pixel_format_modifier() = PixelFormatModifier::kLinear;

  // 3 bytes per pixel, no rounding.
  auto row_bytes = ImageFormatMinimumRowBytesChecked(constraints, 17);
  ASSERT_TRUE(row_bytes.IsValid());
  EXPECT_EQ(51u, row_bytes.ValueOrDie());

  // Rounded up to bytes_per_row_divisor.
  constraints.bytes_per_row_divisor() = 16;
  row_bytes = ImageFormatMinimumRowBytesChecked(constraints, 17);
  ASSERT_TRUE(row_bytes.IsValid());
  EXPECT_EQ(64u, row_bytes.ValueOrDie());

  // min_bytes_per_row wins when larger, then gets rounded up to bytes_per_row_divisor.
  constraints.min_bytes_per_row() = 100;
  row_bytes = ImageFormatMinimumRowBytesChecked(constraints, 17);
  ASSERT_TRUE(row_bytes.IsValid());
  EXPECT_EQ(112u, row_bytes.ValueOrDie());

  // max_bytes_per_row applies after rounding.
  constraints.max_bytes_per_row() = 111;
  EXPECT_FALSE(ImageFormatMinimumRowBytesChecked(constraints, 17).IsValid());
  constraints.max_bytes_per_row() = 112;
  EXPECT_TRUE(ImageFormatMinimumRowBytesChecked(constraints, 17).IsValid());

  // size_alignment.width rounds up the width, before the min/max width checks.
  constraints.min_bytes_per_row().reset();
  constraints.max_bytes_per_row().reset();
  constraints.bytes_per_row_divisor().reset();
  constraints.max_size() = {17, 17};
  constraints.size_alignment() = {8, 1};
  EXPECT_FALSE(ImageFormatMinimumRowBytesChecked(constraints, 17).IsValid());
  constraints.max_size() = {24, 24};
  row_bytes = ImageFormatMinimumRowBytesChecked(constraints, 17);
  ASSERT_TRUE(row_bytes.IsValid());
  EXPECT_EQ(24u * 3, row_bytes.ValueOrDie());

  // A bytes_per_row_divisor of zero is invalid rather than a division by zero.
  constraints.bytes_per_row_divisor() = 0;
  EXPECT_FALSE(ImageFormatMinimumRowBytesChecked(constraints, 17).IsValid());
  uint32_t row_bytes_out = 0;
  EXPECT_FALSE(ImageFormatMinimumRowBytes(constraints, 17, &row_bytes_out));

  // Stride times width overflowing 32 bits is invalid.
  constraints.bytes_per_row_divisor().reset();
  constraints.max_size().reset();
  EXPECT_FALSE(ImageFormatMinimumRowBytesChecked(constraints, 0x60000000).IsValid());
}

TEST(ImageFormatCoverage, IntelMinimumRowBytes) {
  fuchsia_sysmem2::ImageFormatConstraints constraints;
  constraints.pixel_format() = PixelFormat::kB8G8R8A8;
  constraints.pixel_format_modifier() = PixelFormatModifier::kIntelI915XTiled;
  // X tiles are 512 bytes wide, so row bytes are rounded up to a multiple of 512.
  auto row_bytes = ImageFormatMinimumRowBytesChecked(constraints, 17);
  ASSERT_TRUE(row_bytes.IsValid());
  EXPECT_EQ(512u, row_bytes.ValueOrDie());
  // The row bytes are a multiple of the least common multiple of bytes_per_row_divisor and the tile
  // width in bytes.
  constraints.bytes_per_row_divisor() = 100;
  row_bytes = ImageFormatMinimumRowBytesChecked(constraints, 17);
  ASSERT_TRUE(row_bytes.IsValid());
  EXPECT_EQ(12800u, row_bytes.ValueOrDie());
  constraints.bytes_per_row_divisor() = 1000;
  row_bytes = ImageFormatMinimumRowBytesChecked(constraints, 17);
  ASSERT_TRUE(row_bytes.IsValid());
  EXPECT_EQ(64000u, row_bytes.ValueOrDie());
  constraints.bytes_per_row_divisor() = 1024;
  row_bytes = ImageFormatMinimumRowBytesChecked(constraints, 17);
  ASSERT_TRUE(row_bytes.IsValid());
  EXPECT_EQ(1024u, row_bytes.ValueOrDie());
  // The least common multiple is computed in 32 bits.
  constraints.bytes_per_row_divisor() = 0xFFFFFFFF;
  EXPECT_FALSE(ImageFormatMinimumRowBytesChecked(constraints, 17).IsValid());

  // size_alignment.width rounds up the width, before the min/max width checks.
  constraints.bytes_per_row_divisor().reset();
  constraints.size_alignment() = {32, 1};
  constraints.max_size() = {20, 20};
  EXPECT_FALSE(ImageFormatMinimumRowBytesChecked(constraints, 17).IsValid());
  constraints.max_size() = {32, 32};
  EXPECT_TRUE(ImageFormatMinimumRowBytesChecked(constraints, 17).IsValid());
}

TEST(ImageFormatCoverage, GoldfishMinimumRowBytesMatchesLinear) {
  fuchsia_sysmem2::ImageFormatConstraints constraints;
  constraints.pixel_format() = PixelFormat::kB8G8R8A8;
  constraints.pixel_format_modifier() = PixelFormatModifier::kGoogleGoldfishOptimal;
  auto row_bytes = ImageFormatMinimumRowBytesChecked(constraints, 64);
  ASSERT_TRUE(row_bytes.IsValid());
  EXPECT_EQ(64u * 4, row_bytes.ValueOrDie());
  auto format = ImageConstraintsToFormat(constraints, 64, 64);
  ASSERT_TRUE(format.is_ok());
  EXPECT_EQ(64u * 4, format.value().bytes_per_row());
}

TEST(ImageFormatCoverage, ImageConstraintsToFormatFields) {
  fuchsia_sysmem2::ImageFormatConstraints constraints;
  constraints.pixel_format() = PixelFormat::kNv12;
  constraints.pixel_format_modifier() = PixelFormatModifier::kLinear;
  constraints.color_spaces() = {ColorSpace::kRec709, ColorSpace::kRec601Pal};
  constraints.bytes_per_row_divisor() = 64;
  constraints.min_size() = {2, 2};
  constraints.max_size() = {4096, 4096};
  constraints.size_alignment() = {2, 2};

  auto result = ImageConstraintsToFormat(constraints, 1920, 1080);
  ASSERT_TRUE(result.is_ok());
  const auto& image_format = result.value();
  EXPECT_EQ(PixelFormat::kNv12, image_format.pixel_format());
  EXPECT_EQ(PixelFormatModifier::kLinear, image_format.pixel_format_modifier());
  EXPECT_EQ(ColorSpace::kRec709, image_format.color_space());
  ASSERT_TRUE(image_format.size().has_value());
  EXPECT_EQ(1920u, image_format.size()->width());
  EXPECT_EQ(1080u, image_format.size()->height());
  EXPECT_EQ(1920u, image_format.bytes_per_row());
  ASSERT_TRUE(image_format.display_rect().has_value());
  EXPECT_EQ(0u, image_format.display_rect()->x());
  EXPECT_EQ(0u, image_format.display_rect()->y());
  EXPECT_EQ(1920u, image_format.display_rect()->width());
  EXPECT_EQ(1080u, image_format.display_rect()->height());
  EXPECT_FALSE(image_format.valid_size().has_value());
  EXPECT_FALSE(image_format.pixel_aspect_ratio().has_value());

  // No color space when color_spaces is empty or absent.
  constraints.color_spaces() = std::vector<ColorSpace>{};
  result = ImageConstraintsToFormat(constraints, 1920, 1080);
  ASSERT_TRUE(result.is_ok());
  EXPECT_FALSE(result.value().color_space().has_value());
  constraints.color_spaces().reset();
  result = ImageConstraintsToFormat(constraints, 1920, 1080);
  ASSERT_TRUE(result.is_ok());
  EXPECT_FALSE(result.value().color_space().has_value());

  // Out of range or unaligned sizes fail.
  EXPECT_FALSE(ImageConstraintsToFormat(constraints, 1, 1080).is_ok());
  EXPECT_FALSE(ImageConstraintsToFormat(constraints, 1920, 1).is_ok());
  EXPECT_FALSE(ImageConstraintsToFormat(constraints, 8192, 1080).is_ok());
  EXPECT_FALSE(ImageConstraintsToFormat(constraints, 1920, 8192).is_ok());
  EXPECT_FALSE(ImageConstraintsToFormat(constraints, 1921, 1080).is_ok());
  EXPECT_FALSE(ImageConstraintsToFormat(constraints, 1920, 1081).is_ok());
}

TEST(ImageFormatCoverage, ArmTransactionEliminationLayout) {
  // Linear + TE: the TE plane (plane 3) follows the linear image data, aligned to 64 bytes. The
  // buffer includes a 64 byte header, then 4 rows of CRC data per 64 pixel high tile, where each
  // row has 32 bytes per 64 pixel wide tile, rounded up to 64 bytes.
  fuchsia_images2::ImageFormat image_format;
  image_format.pixel_format() = PixelFormat::kB8G8R8A8;
  image_format.pixel_format_modifier() = PixelFormatModifier::kArmLinearTe;
  image_format.size() = {100, 70};
  image_format.bytes_per_row() = 401;

  constexpr uint64_t kLinearSize = 401ull * 70;
  constexpr uint64_t kTeOffset = (kLinearSize + 63) / 64 * 64;
  // 100 pixels -> 2 tiles wide -> 64 bytes per CRC row; 70 pixels -> 2 tiles high -> 8 CRC rows.
  constexpr uint64_t kTeSize = (kTeOffset - kLinearSize) + 64 + (64ull * 8);
  auto offset = ImageFormatPlaneByteOffsetChecked(image_format, 3);
  ASSERT_TRUE(offset.IsValid());
  EXPECT_EQ(kTeOffset, offset.ValueOrDie());
  auto row_bytes = ImageFormatPlaneRowBytesChecked(image_format, 3);
  ASSERT_TRUE(row_bytes.IsValid());
  EXPECT_EQ(64u, row_bytes.ValueOrDie());
  auto size = ImageFormatImageSizeChecked(image_format);
  ASSERT_TRUE(size.IsValid());
  EXPECT_EQ(kLinearSize + kTeSize, size.ValueOrDie());

  // Planes 0..2 match the corresponding linear format, while plane 3 is the TE plane and plane 4
  // is out of range.
  for (PixelFormat pf : {PixelFormat::kB8G8R8A8, PixelFormat::kI420}) {
    image_format.pixel_format() = pf;
    fuchsia_images2::ImageFormat linear = image_format;
    linear.pixel_format_modifier() = PixelFormatModifier::kLinear;
    for (uint32_t plane = 0; plane < 3; ++plane) {
      EXPECT_EQ(CheckedToString(ImageFormatPlaneByteOffsetChecked(linear, plane)),
                CheckedToString(ImageFormatPlaneByteOffsetChecked(image_format, plane)));
      EXPECT_EQ(CheckedToString(ImageFormatPlaneRowBytesChecked(linear, plane)),
                CheckedToString(ImageFormatPlaneRowBytesChecked(image_format, plane)));
    }
    EXPECT_TRUE(ImageFormatPlaneByteOffsetChecked(image_format, 3).IsValid());
    EXPECT_FALSE(ImageFormatPlaneByteOffsetChecked(image_format, 4).IsValid());
  }
}

TEST(ImageFormatCoverage, IntelCcsLayout) {
  // Y-tiled CCS: one CCS tile covers 32 main-surface tiles horizontally and 16 vertically.
  fuchsia_sysmem2::ImageFormatConstraints constraints;
  constraints.pixel_format() = PixelFormat::kB8G8R8A8;
  constraints.pixel_format_modifier() = PixelFormatModifier::kIntelI915YTiledCcs;
  // 33 tiles wide (128 bytes each), 17 tiles high (32 rows each).
  auto result = ImageConstraintsToFormat(constraints, 33 * 32, 17 * 32);
  ASSERT_TRUE(result.is_ok());
  auto image_format = result.take_value();
  EXPECT_EQ(33u * 128, image_format.bytes_per_row());
  constexpr uint64_t kMainSize = 33ull * 17 * 4096;
  constexpr uint64_t kCcsSize = 2ull * 2 * 4096;
  auto size = ImageFormatImageSizeChecked(image_format);
  ASSERT_TRUE(size.IsValid());
  EXPECT_EQ(kMainSize + kCcsSize, size.ValueOrDie());
  auto offset = ImageFormatPlaneByteOffsetChecked(image_format, 3);
  ASSERT_TRUE(offset.IsValid());
  EXPECT_EQ(kMainSize, offset.ValueOrDie());
  auto row_bytes = ImageFormatPlaneRowBytesChecked(image_format, 3);
  ASSERT_TRUE(row_bytes.IsValid());
  EXPECT_EQ(2u * 128, row_bytes.ValueOrDie());
  // Plane 1 doesn't exist for a single-plane format.
  EXPECT_FALSE(ImageFormatPlaneByteOffsetChecked(image_format, 1).IsValid());
  EXPECT_FALSE(ImageFormatPlaneRowBytesChecked(image_format, 1).IsValid());
}

TEST(ImageFormatCoverage, PvricLayout) {
  // PVRIC 8x8: 32 bytes per row per tile, 8 rows per tile, 256 bytes per tile. Header plane (plane
  // 3) is at offset 0 (1 byte per tile, rounded up to 256 bytes), and tiles plane (plane 0)
  // follows at offset header_size.
  fuchsia_sysmem2::ImageFormatConstraints constraints;
  constraints.pixel_format() = PixelFormat::kB8G8R8A8;
  constraints.pixel_format_modifier() = PixelFormatModifier::kPvrFbcdc8X8V14;
  // 10 tiles wide (32 bytes each), 5 tiles high (8 rows each) -> 50 tiles.
  auto result = ImageConstraintsToFormat(constraints, 10 * 8, 5 * 8);
  ASSERT_TRUE(result.is_ok());
  auto image_format = result.take_value();
  EXPECT_EQ(10u * 32, image_format.bytes_per_row());
  constexpr uint64_t kHeaderSize = 256;
  constexpr uint64_t kDataSize = 10ull * 5 * 256;
  auto size = ImageFormatImageSizeChecked(image_format);
  ASSERT_TRUE(size.IsValid());
  EXPECT_EQ(kHeaderSize + kDataSize, size.ValueOrDie());
  auto offset0 = ImageFormatPlaneByteOffsetChecked(image_format, 0);
  ASSERT_TRUE(offset0.IsValid());
  EXPECT_EQ(kHeaderSize, offset0.ValueOrDie());
  auto offset3 = ImageFormatPlaneByteOffsetChecked(image_format, 3);
  ASSERT_TRUE(offset3.IsValid());
  EXPECT_EQ(0u, offset3.ValueOrDie());
  auto row_bytes0 = ImageFormatPlaneRowBytesChecked(image_format, 0);
  ASSERT_TRUE(row_bytes0.IsValid());
  EXPECT_EQ(10u * 32, row_bytes0.ValueOrDie());
  auto row_bytes3 = ImageFormatPlaneRowBytesChecked(image_format, 3);
  ASSERT_TRUE(row_bytes3.IsValid());
  EXPECT_EQ(10u, row_bytes3.ValueOrDie());
  // Planes 1 and 2 don't exist.
  EXPECT_FALSE(ImageFormatPlaneByteOffsetChecked(image_format, 1).IsValid());
  EXPECT_FALSE(ImageFormatPlaneRowBytesChecked(image_format, 1).IsValid());
  EXPECT_FALSE(ImageFormatPlaneByteOffsetChecked(image_format, 2).IsValid());
  EXPECT_FALSE(ImageFormatPlaneRowBytesChecked(image_format, 2).IsValid());
}

TEST(ImageFormatCoverage, MissingOptionalFields) {
  // pixel_format_modifier defaults to LINEAR when not set.
  fuchsia_images2::ImageFormat image_format;
  image_format.pixel_format() = PixelFormat::kB8G8R8A8;
  image_format.size() = {10, 10};
  image_format.bytes_per_row() = 40;
  auto size = ImageFormatImageSizeChecked(image_format);
  ASSERT_TRUE(size.IsValid());
  EXPECT_EQ(400u, size.ValueOrDie());

  // An unset pixel_format_modifier is LINEAR for the plane functions too.
  for (PixelFormat pixel_format : {PixelFormat::kNv12, PixelFormat::kI420}) {
    fuchsia_images2::ImageFormat unset_modifier;
    unset_modifier.pixel_format() = pixel_format;
    unset_modifier.size() = {16, 10};
    unset_modifier.bytes_per_row() = 16;
    fuchsia_images2::ImageFormat linear = unset_modifier;
    linear.pixel_format_modifier() = PixelFormatModifier::kLinear;
    for (uint32_t plane = 0; plane < 4; ++plane) {
      EXPECT_EQ(CheckedToString(ImageFormatPlaneByteOffsetChecked(linear, plane)),
                CheckedToString(ImageFormatPlaneByteOffsetChecked(unset_modifier, plane)));
      EXPECT_EQ(CheckedToString(ImageFormatPlaneRowBytesChecked(linear, plane)),
                CheckedToString(ImageFormatPlaneRowBytesChecked(unset_modifier, plane)));
    }
    EXPECT_TRUE(ImageFormatPlaneByteOffsetChecked(unset_modifier, 1).IsValid());
  }

  // Plane 0 offset doesn't need any fields other than pixel_format.
  fuchsia_images2::ImageFormat pixel_format_only;
  pixel_format_only.pixel_format() = PixelFormat::kNv12;
  auto offset = ImageFormatPlaneByteOffsetChecked(pixel_format_only, 0);
  ASSERT_TRUE(offset.IsValid());
  EXPECT_EQ(0u, offset.ValueOrDie());
  EXPECT_FALSE(ImageFormatPlaneByteOffsetChecked(pixel_format_only, 3).IsValid());

  // Constraints: pixel_format_modifier defaults to LINEAR for MinimumRowBytes.
  fuchsia_sysmem2::ImageFormatConstraints constraints;
  constraints.pixel_format() = PixelFormat::kB8G8R8A8;
  auto row_bytes = ImageFormatMinimumRowBytesChecked(constraints, 10);
  ASSERT_TRUE(row_bytes.IsValid());
  EXPECT_EQ(40u, row_bytes.ValueOrDie());
  // And for ImageConstraintsToFormat.
  auto result = ImageConstraintsToFormat(constraints, 10, 10);
  ASSERT_TRUE(result.is_ok());
  EXPECT_EQ(PixelFormatModifier::kLinear, result.value().pixel_format_modifier());
  EXPECT_EQ(40u, result.value().bytes_per_row());
}

TEST(ImageFormatCoverage, UnsupportedIsInvalidNotPanic) {
  // The per-plane functions and minimum row bytes return invalid for unsupported formats.
  fuchsia_images2::ImageFormat image_format;
  image_format.pixel_format() = PixelFormat::kB8G8R8A8;
  image_format.pixel_format_modifier() = kUnknownModifier;
  image_format.size() = {10, 10};
  image_format.bytes_per_row() = 40;
  EXPECT_FALSE(ImageFormatPlaneByteOffsetChecked(image_format, 0).IsValid());
  EXPECT_FALSE(ImageFormatPlaneRowBytesChecked(image_format, 0).IsValid());
  uint64_t offset = 0;
  EXPECT_FALSE(ImageFormatPlaneByteOffset(image_format, 0, &offset));
  uint32_t row_bytes = 0;
  EXPECT_FALSE(ImageFormatPlaneRowBytes(image_format, 0, &row_bytes));
  EXPECT_FALSE(
      ImageFormatIsNonTiledSinglePlane(PixelFormatAndModifierFromImageFormat(image_format)));

  fuchsia_sysmem2::ImageFormatConstraints constraints;
  constraints.pixel_format() = PixelFormat::kB8G8R8A8;
  constraints.pixel_format_modifier() = kUnknownModifier;
  EXPECT_FALSE(ImageFormatMinimumRowBytesChecked(constraints, 10).IsValid());
  EXPECT_FALSE(ImageFormatMinimumRowBytes(constraints, 10, &row_bytes));
  EXPECT_FALSE(ImageConstraintsToFormat(constraints, 10, 10).is_ok());
}

TEST(ImageFormatCoverage, UnknownPixelFormatWithGoldfish) {
  // The goldfish format set only supports the pixel formats that have a linear layout.
  fuchsia_images2::ImageFormat image_format;
  image_format.pixel_format() = kUnknownPixelFormat;
  image_format.pixel_format_modifier() = PixelFormatModifier::kGoogleGoldfishOptimal;
  image_format.size() = {10, 10};
  image_format.bytes_per_row() = 40;
  EXPECT_FALSE(ImageFormatIsSupported(PixelFormatAndModifierFromImageFormat(image_format)));
  EXPECT_FALSE(ImageFormatPlaneByteOffsetChecked(image_format, 0).IsValid());
}

TEST(ImageFormatCoverage, DeathMissingPixelFormat) {
  fuchsia_images2::ImageFormat image_format;
  image_format.size() = {10, 10};
  image_format.bytes_per_row() = 40;
  ASSERT_DEATH([&image_format] { ImageFormatImageSizeChecked(image_format); });
  ASSERT_DEATH([&image_format] { ImageFormatPlaneByteOffsetChecked(image_format, 0); });
  ASSERT_DEATH([&image_format] { ImageFormatPlaneRowBytesChecked(image_format, 0); });

  fuchsia_sysmem2::ImageFormatConstraints constraints;
  ASSERT_DEATH([&constraints] { ImageFormatMinimumRowBytesChecked(constraints, 10); });
  ASSERT_DEATH([&constraints] { ImageConstraintsToFormat(constraints, 10, 10); });
}

TEST(ImageFormatCoverage, DeathMissingSize) {
  fuchsia_images2::ImageFormat linear_no_size;
  linear_no_size.pixel_format() = PixelFormat::kB8G8R8A8;
  linear_no_size.pixel_format_modifier() = PixelFormatModifier::kLinear;
  linear_no_size.bytes_per_row() = 40;
  ASSERT_DEATH([&linear_no_size] { ImageFormatPlaneRowBytesChecked(linear_no_size, 0); });

  fuchsia_images2::ImageFormat te_no_size;
  te_no_size.pixel_format() = PixelFormat::kB8G8R8A8;
  te_no_size.pixel_format_modifier() = PixelFormatModifier::kArmLinearTe;
  te_no_size.bytes_per_row() = 40;
  ASSERT_DEATH([&te_no_size] { ImageFormatPlaneByteOffsetChecked(te_no_size, 3); });
  ASSERT_DEATH([&te_no_size] { ImageFormatPlaneRowBytesChecked(te_no_size, 0); });
  ASSERT_DEATH([&te_no_size] { ImageFormatPlaneRowBytesChecked(te_no_size, 3); });

  fuchsia_images2::ImageFormat afbc_no_size;
  afbc_no_size.pixel_format() = PixelFormat::kB8G8R8A8;
  afbc_no_size.pixel_format_modifier() = PixelFormatModifier::kArmAfbc16X16Te;
  afbc_no_size.bytes_per_row() = 128;
  ASSERT_DEATH([&afbc_no_size] { ImageFormatPlaneRowBytesChecked(afbc_no_size, 0); });
  ASSERT_DEATH([&afbc_no_size] { ImageFormatPlaneRowBytesChecked(afbc_no_size, 3); });
}

TEST(ImageFormatCoverage, DeathMissingBytesPerRow) {
  fuchsia_images2::ImageFormat te_no_bpr;
  te_no_bpr.pixel_format() = PixelFormat::kB8G8R8A8;
  te_no_bpr.pixel_format_modifier() = PixelFormatModifier::kArmLinearTe;
  te_no_bpr.size() = {10, 10};
  ASSERT_DEATH([&te_no_bpr] { ImageFormatPlaneRowBytesChecked(te_no_bpr, 3); });

  fuchsia_images2::ImageFormat afbc_no_bpr;
  afbc_no_bpr.pixel_format() = PixelFormat::kB8G8R8A8;
  afbc_no_bpr.pixel_format_modifier() = PixelFormatModifier::kArmAfbc16X16Te;
  afbc_no_bpr.size() = {16, 16};
  ASSERT_DEATH([&afbc_no_bpr] { ImageFormatImageSizeChecked(afbc_no_bpr); });
  ASSERT_DEATH([&afbc_no_bpr] { ImageFormatPlaneRowBytesChecked(afbc_no_bpr, 0); });
  ASSERT_DEATH([&afbc_no_bpr] { ImageFormatPlaneByteOffsetChecked(afbc_no_bpr, 3); });
  ASSERT_DEATH([&afbc_no_bpr] { ImageFormatPlaneRowBytesChecked(afbc_no_bpr, 3); });
}

TEST(ImageFormatCoverage, DeathUnsupportedImageSize) {
  fuchsia_images2::ImageFormat image_format;
  image_format.pixel_format() = PixelFormat::kB8G8R8A8;
  image_format.pixel_format_modifier() = kUnknownModifier;
  image_format.size() = {10, 10};
  image_format.bytes_per_row() = 40;
  ASSERT_DEATH([&image_format] { ImageFormatImageSizeChecked(image_format); });
  ASSERT_DEATH([&image_format] { ImageFormatImageSize(image_format); });
}

TEST(ImageFormatCoverage, DeathImageSizeOverflow) {
  fuchsia_images2::ImageFormat image_format;
  image_format.pixel_format() = PixelFormat::kNv12;
  image_format.pixel_format_modifier() = PixelFormatModifier::kLinear;
  image_format.size() = {0xFFFFFFFF, 0xFFFFFFFF};
  image_format.bytes_per_row() = 0xFFFFFFFF;
  ASSERT_DEATH([&image_format] { ImageFormatImageSize(image_format); });
}

TEST(ImageFormatCoverage, DeathMinimumRowBytesZeroWidth) {
  fuchsia_sysmem2::ImageFormatConstraints constraints;
  constraints.pixel_format() = PixelFormat::kB8G8R8A8;
  constraints.pixel_format_modifier() = PixelFormatModifier::kLinear;
  uint32_t row_bytes = 0;
  ASSERT_DEATH(
      ([&constraints, &row_bytes] { ImageFormatMinimumRowBytes(constraints, 0, &row_bytes); }));
}

TEST(ImageFormatCoverage, DeathUnsupportedPixelFormatProperties) {
  // The per-pixel-format properties are only defined for supported formats.
  PixelFormatAndModifier pfm(kUnknownPixelFormat, PixelFormatModifier::kLinear);
  ASSERT_DEATH([&pfm] { ImageFormatBitsPerPixel(pfm); });
  ASSERT_DEATH([&pfm] { ImageFormatStrideBytesPerWidthPixel(pfm); });
  ASSERT_DEATH([&pfm] { ImageFormatSurfaceWidthMinDivisor(pfm); });
  ASSERT_DEATH([&pfm] { ImageFormatSurfaceHeightMinDivisor(pfm); });
  ASSERT_DEATH([&pfm] { ImageFormatSampleAlignment(pfm); });
  ASSERT_DEATH([&pfm] { ImageFormatRowBytesMinDivisor(pfm); });
}

TEST(ImageFormatCoverage, DeathUnsupportedModifierProperties) {
  // A supported pixel format with a modifier it isn't supported with.
  PixelFormatAndModifier pfm(PixelFormat::kNv12, PixelFormatModifier::kArmAfbc16X16);
  ASSERT_DEATH([&pfm] { ImageFormatBitsPerPixel(pfm); });
  ASSERT_DEATH([&pfm] { ImageFormatStrideBytesPerWidthPixel(pfm); });
  ASSERT_DEATH([&pfm] { ImageFormatSurfaceWidthMinDivisor(pfm); });
  ASSERT_DEATH([&pfm] { ImageFormatSurfaceHeightMinDivisor(pfm); });
  ASSERT_DEATH([&pfm] { ImageFormatSampleAlignment(pfm); });
  ASSERT_DEATH([&pfm] { ImageFormatRowBytesMinDivisor(pfm); });
}

}  // namespace
