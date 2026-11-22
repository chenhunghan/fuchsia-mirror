// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "zircon/system/ulib/image-format/include/lib/image-format/image_format.h"

#include <fidl/fuchsia.images2/cpp/fidl.h>
#include <fidl/fuchsia.sysmem2/cpp/fidl.h>

#if defined(FIDL_ALLOW_DEPRECATED_C_BINDINGS)
#include <fuchsia/sysmem/c/fidl.h>
#endif
#include <lib/fidl/cpp/wire_natural_conversions.h>
#include <lib/sysmem-version/sysmem-version.h>
#include <lib/zbi-format/graphics.h>
#include <zircon/assert.h>
#include <zircon/availability.h>

#include <algorithm>
#include <cinttypes>
#include <map>
#include <numeric>
#include <set>

#include <fbl/algorithm.h>
#include <safemath/safe_math.h>

using safemath::CheckAdd;
using safemath::CheckDiv;
using safemath::CheckedNumeric;
using safemath::CheckMin;
using safemath::CheckMod;
using safemath::CheckMul;
using safemath::CheckSub;

namespace {

using ColorSpace = fuchsia_images2::ColorSpace;
using ImageFormat = fuchsia_images2::ImageFormat;
using ImageFormatConstraints = fuchsia_sysmem2::ImageFormatConstraints;
using PixelFormat = fuchsia_images2::PixelFormat;

using ColorSpaceWire = fuchsia_images2::wire::ColorSpace;
using ImageFormatWire = fuchsia_images2::wire::ImageFormat;
using ImageFormatConstraintsWire = fuchsia_sysmem2::wire::ImageFormatConstraints;
using PixelFormatWire = fuchsia_images2::wire::PixelFormat;

// There are two aspects of the ColorSpaceWire and PixelFormatWire that we care about:
//   * bits-per-sample - bits per primary sample (R, G, B, or Y)
//   * RGB vs. YUV - whether the system supports the ColorSpaceWire or PixelFormatWire
//     representing RGB data or YUV data.  Any given ColorSpaceWire only supports
//     one or the other. Currently any given PixelFormatWire only supports one or
//     the other and this isn't likely to change.
// While we could just list all the ColorSpaceWire(s) that each PixelFormatWire could
// plausibly support, expressing in terms of bits-per-sample and RGB vs. YUV is
// perhaps easier to grok.

enum ColorType { kColorType_NONE, kColorType_RGB, kColorType_YUV };

struct SamplingInfo {
  std::set<uint32_t> possible_bits_per_sample;
  std::set<ColorType> color_types;
};

const std::map<ColorSpaceWire, SamplingInfo> kColorSpaceSamplingInfo = {
    {ColorSpace::kSrgb, {{8, 10, 12, 16}, {kColorType_RGB}}},
    {ColorSpace::kRec601Ntsc, {{8, 10}, {kColorType_YUV}}},
    {ColorSpace::kRec601NtscFullRange, {{8, 10}, {kColorType_YUV}}},
    {ColorSpace::kRec601Pal, {{8, 10}, {kColorType_YUV}}},
    {ColorSpace::kRec601PalFullRange, {{8, 10}, {kColorType_YUV}}},
    {ColorSpace::kRec709, {{8, 10}, {kColorType_YUV}}},
    {ColorSpace::kRec2020, {{10, 12}, {kColorType_YUV}}},
    {ColorSpace::kRec2100, {{10, 12}, {kColorType_YUV}}},
};
const std::map<PixelFormatWire, SamplingInfo> kPixelFormatSamplingInfo = {
    {PixelFormat::kR8G8B8A8, {{8}, {kColorType_RGB}}},
    {PixelFormat::kR8G8B8X8, {{8}, {kColorType_RGB}}},
    {PixelFormat::kB8G8R8A8, {{8}, {kColorType_RGB}}},
    {PixelFormat::kB8G8R8X8, {{8}, {kColorType_RGB}}},
    {PixelFormat::kI420, {{8}, {kColorType_YUV}}},
    {PixelFormat::kM420, {{8}, {kColorType_YUV}}},
    {PixelFormat::kNv12, {{8}, {kColorType_YUV}}},
    {PixelFormat::kP010, {{10}, {kColorType_YUV}}},
    {PixelFormat::kYuy2, {{8}, {kColorType_YUV}}},
    // 8 bits RGB when uncompressed - in this context, MJPEG is essentially
    // pretending to be uncompressed.
    {PixelFormat::kMjpeg, {{8}, {kColorType_RGB}}},
    {PixelFormat::kYv12, {{8}, {kColorType_YUV}}},
    {PixelFormat::kB8G8R8, {{8}, {kColorType_RGB}}},
    {PixelFormat::kR8G8B8, {{8}, {kColorType_RGB}}},

    // These use the same colorspaces as regular 8-bit-per-component formats
    {PixelFormat::kR5G6B5, {{8}, {kColorType_RGB}}},
    {PixelFormat::kR3G3B2, {{8}, {kColorType_RGB}}},
    {PixelFormat::kR2G2B2X2, {{8}, {kColorType_RGB}}},

    // Expands to RGB or YUV
    {PixelFormat::kL8, {{8}, {kColorType_RGB, kColorType_YUV}}},
    {PixelFormat::kR8, {{8}, {kColorType_RGB, kColorType_YUV}}},

    {PixelFormat::kR8G8, {{8}, {kColorType_RGB}}},
    {PixelFormat::kA2B10G10R10, {{10}, {kColorType_RGB}}},
    {PixelFormat::kA2R10G10B10, {{10}, {kColorType_RGB}}},
};

constexpr CheckedNumeric<uint32_t> kTransactionEliminationAlignment = 64;
// The transaction elimination buffer is always reported as plane 3.
constexpr uint32_t kTransactionEliminationPlane = 3;

constexpr CheckedNumeric<uint32_t> kInvalidCheckedNumeric32 =
    CheckAdd(CheckedNumeric(std::numeric_limits<uint32_t>::max()), 1);
static_assert(!kInvalidCheckedNumeric32.IsValid());
constexpr CheckedNumeric<uint64_t> kInvalidCheckedNumeric64 =
    CheckAdd(CheckedNumeric(std::numeric_limits<uint64_t>::max()), 1);
static_assert(!kInvalidCheckedNumeric64.IsValid());

template <typename T, typename U>
CheckedNumeric<T> CheckRoundUp(CheckedNumeric<T> val, CheckedNumeric<U> multiple) {
  // We could do template things to make the call site fail to find a template instantiation if
  // these aren't true, but this gives more helpful compiler error messages.
  static_assert(std::is_unsigned_v<T>);
  static_assert(std::is_unsigned_v<U>);
  static_assert(sizeof(T) >= sizeof(U));
  auto div = CheckDiv(val, multiple);
  auto mod = CheckMod(val, multiple);
  auto has_remainder = CheckMin(mod, decltype(div)(1));
  return CheckMul(CheckAdd(div, has_remainder), multiple);
}

// ARM Mali Transaction Elimination (TE) plane 3 layout:
//
// Fuchsia's `kFormatModifierArmTeBit` (on AFBC) and `kArmLinearTe` modifiers are Fuchsia/sysmem
// conventions for appending the ARM Mali GPU's per-surface Transaction Elimination CRC signature
// buffer at the end of the shared VMO (reported as plane 3) so CRC state persists across processes.
// TE is a Mali render-output feature independent of the AFBC bitstream format.
//
// Plane 3 memory layout:
//   1. Starts at `RoundUp(start, 64)` (`kTransactionEliminationAlignment`), matching Mali's 64-byte
//      CRC base-address alignment after the non-TE image data of size `start`.
//   2. Begins with a 64-byte cache-line-aligned software "CRC valid" flag header placed ahead of
//      the hardware CRC base address by the Mali Vulkan ICD so CPU/GPU updates to the valid flag do
//      not share a cache line with hardware-written CRC entries.
//   3. Followed by a 2D array of 64-bit (8-byte) CRC signatures, one per 16x16 pixel render tile.
//      Mali v12+ GPUs prefetch CRC entries in 64x64 pixel (4x4 tile) chunks (earlier Bifrost and
//      Valhall GPUs use 32x32 chunks, so rounding up to 64x64 satisfies all generations):
//        - Horizontally, each 64-pixel chunk spans 4 tiles (4 * 8 = 32 bytes per CRC row), and each
//          CRC row stride is rounded up to a 64-byte cache line:
//          `RoundUp(ceil(width / 64) * 32, 64)` (`64 * ceil(width / 128)` bytes).
//        - Vertically, each 64-pixel chunk spans 4 tile rows, giving `4 * ceil(height / 64)` CRC
//          rows after the 64-byte header.
//
// Why `width` and `height` are logical `ImageFormat.size` dimensions (not `bytes_per_row`):
//   In Mali's Multi-Frame Buffer Descriptor and Depth/CRC Buffer Descriptor, rendered 16x16 tile
//   coordinates and `crc_row_stride` are configured from the logical framebuffer extent
//   (`size.width`, `size.height`) independently of plane 0's `color_row_stride` or
//   `afbc_line_stride`, and the Mali Vulkan ICD always sizes and strides plane 3 from `size.width`
//   and `size.height` (ignoring plane 3 `bytes_per_row` while requiring plane 3's byte offset to be
//   `RoundUp(plane0_size, 64)`). Plane 0's size `start` (and therefore plane 3's byte offset) still
//   follows `ImageFormat.bytes_per_row` (and AFBC `width_alignment`).
CheckedNumeric<uint64_t> arm_transaction_elimination_row_size(CheckedNumeric<uint32_t> width) {
  constexpr CheckedNumeric<uint32_t> kTileSize = 64;
  constexpr CheckedNumeric<uint32_t> kBytesPerTilePerRow = 32;
  CheckedNumeric<uint32_t> width_in_tiles = CheckRoundUp(width, kTileSize) / kTileSize;
  return CheckRoundUp(width_in_tiles * kBytesPerTilePerRow, kTransactionEliminationAlignment);
}

CheckedNumeric<uint64_t> arm_transaction_elimination_buffer_size(CheckedNumeric<uint64_t> start,
                                                                 CheckedNumeric<uint32_t> width,
                                                                 CheckedNumeric<uint32_t> height) {
  constexpr CheckedNumeric<uint32_t> kTileSize = 64;
  CheckedNumeric<uint64_t> end = start;
  end = CheckRoundUp(end, kTransactionEliminationAlignment);
  constexpr CheckedNumeric<uint32_t> kHeaderSize = kTransactionEliminationAlignment;
  end += kHeaderSize;
  CheckedNumeric<uint32_t> height_in_tiles = CheckRoundUp(height, kTileSize) / kTileSize;
  end += arm_transaction_elimination_row_size(width) * 4 * height_in_tiles;
  return end - start;
}

class ImageFormatSet {
 public:
  virtual const char* Name() const = 0;
  virtual bool IsSupported(const PixelFormatAndModifier& pixel_format) const = 0;
  virtual CheckedNumeric<uint64_t> ImageFormatImageSize(const ImageFormat& image_format) const = 0;
  virtual CheckedNumeric<uint64_t> ImageFormatPlaneByteOffset(const ImageFormat& image_format,
                                                              uint32_t plane) const = 0;
  virtual CheckedNumeric<uint32_t> ImageFormatPlaneRowBytes(const ImageFormat& image_format,
                                                            uint32_t plane) const = 0;
  virtual bool ImageFormatIsNonTiledSinglePlane(
      const PixelFormatAndModifier& pixel_format_and_modifier) const = 0;
  virtual uint32_t ImageFormatRowBytesMinDivisor(
      const PixelFormatAndModifier& pixel_format_and_modifier) const = 0;
  virtual CheckedNumeric<uint32_t> ImageFormatMinimumRowBytes(
      const fuchsia_sysmem2::ImageFormatConstraints& constraints,
      CheckedNumeric<uint32_t> width) const = 0;
};

class IntelTiledFormats : public ImageFormatSet {
 public:
  const char* Name() const override { return "IntelTiledFormats"; }

  bool IsSupported(const PixelFormatAndModifier& pixel_format) const override {
    if (!Is8888Rgb(pixel_format.pixel_format) && pixel_format.pixel_format != PixelFormat::kNv12) {
      return false;
    }
    switch (pixel_format.pixel_format_modifier) {
      case fuchsia_images2::PixelFormatModifier::kIntelI915XTiled:
      case fuchsia_images2::PixelFormatModifier::kIntelI915YTiled:
      case fuchsia_images2::PixelFormatModifier::kIntelI915YfTiled:
        return true;
      // X-tiled CCS is not supported. Y-tiled and Yf-tiled CCS (Gen9-11 render compression) are
      // only supported for the 8:8:8:8 RGB formats (drm_fourcc.h; Gen9 display and <32-bpp / YUV
      // hardware restrictions), not planar formats such as NV12.
      case fuchsia_images2::PixelFormatModifier::kIntelI915YTiledCcs:
      case fuchsia_images2::PixelFormatModifier::kIntelI915YfTiledCcs:
        return Is8888Rgb(pixel_format.pixel_format);
      default:
        return false;
    }
  }

  CheckedNumeric<uint64_t> ImageFormatImageSize(const ImageFormat& image_format) const override {
    auto pixel_format_and_modifier = PixelFormatAndModifierFromImageFormat(image_format);
    ZX_ASSERT(IsSupported(PixelFormatAndModifier(pixel_format_and_modifier)));

    CheckedNumeric<uint32_t> width_in_tiles, height_in_tiles;
    uint32_t num_of_planes = FormatNumOfPlanes(image_format.pixel_format().value());
    CheckedNumeric<uint64_t> size = 0u;

    for (uint32_t plane_idx = 0; plane_idx < num_of_planes; plane_idx += 1) {
      GetSizeInTiles(image_format, plane_idx, &width_in_tiles, &height_in_tiles);
      size += (width_in_tiles * height_in_tiles * kIntelTileByteSize);
    }

    if (FormatHasCcs(pixel_format_and_modifier)) {
      // CCS is only supported for single-plane formats, so this is the size of the only plane.
      ZX_ASSERT(num_of_planes == 1);
      size += CcsSize(width_in_tiles, height_in_tiles);
    }

    return size;
  }

  CheckedNumeric<uint64_t> ImageFormatPlaneByteOffset(const ImageFormat& image_format,
                                                      uint32_t plane) const override {
    const auto pixel_format_and_modifier = PixelFormatAndModifierFromImageFormat(image_format);
    ZX_ASSERT(IsSupported(pixel_format_and_modifier));

    uint32_t num_of_planes = FormatNumOfPlanes(image_format.pixel_format().value());

    uint32_t end_plane;

    // For image data planes, calculate the size of all previous the image data planes
    if (plane < num_of_planes) {
      end_plane = plane;
    } else if (plane == kCcsPlane && FormatHasCcs(pixel_format_and_modifier)) {
      // If requesting the CCS Aux plane, calculate the size of all the image data planes
      end_plane = num_of_planes;
    } else {  // Plane is out of bounds, return !IsValid
      return kInvalidCheckedNumeric64;
    }

    CheckedNumeric<uint64_t> offset = 0u;
    for (uint32_t plane_idx = 0u; plane_idx < end_plane; plane_idx += 1u) {
      CheckedNumeric<uint32_t> width_in_tiles, height_in_tiles;
      GetSizeInTiles(image_format, plane_idx, &width_in_tiles, &height_in_tiles);
      offset += (width_in_tiles * height_in_tiles * kIntelTileByteSize);
    }
    // The offset may have overflowed, in which case it's returned as invalid. A valid offset is
    // always a whole number of tiles.
    ZX_ASSERT(!offset.IsValid() || ((offset % kIntelTileByteSize).ValueOrDie() == 0));
    return offset;
  }

  CheckedNumeric<uint32_t> ImageFormatPlaneRowBytes(const ImageFormat& image_format,
                                                    uint32_t plane) const override {
    auto pixel_format_and_modifier = PixelFormatAndModifierFromImageFormat(image_format);
    ZX_ASSERT(IsSupported(pixel_format_and_modifier));

    uint32_t num_of_planes = FormatNumOfPlanes(image_format.pixel_format().value());

    if (plane < num_of_planes) {
      CheckedNumeric<uint32_t> width_in_tiles, height_in_tiles;
      GetSizeInTiles(image_format, plane, &width_in_tiles, &height_in_tiles);
      const auto& tiling_data = GetTilingData(GetTilingTypeForPixelFormat(PixelFormatAndModifier(
          image_format.pixel_format().value(), image_format.pixel_format_modifier().value())));
      return width_in_tiles * tiling_data.bytes_per_row_per_tile;
    }

    if (plane == kCcsPlane && FormatHasCcs(pixel_format_and_modifier)) {
      CheckedNumeric<uint32_t> width_in_tiles, height_in_tiles;
      // Since we only care about the width, just use the first plane
      GetSizeInTiles(image_format, 0, &width_in_tiles, &height_in_tiles);
      return CcsWidthInTiles(width_in_tiles) * GetTilingData(TilingType::kY).bytes_per_row_per_tile;
    }

    // invalid plane param
    return kInvalidCheckedNumeric32;
  }

  bool ImageFormatIsNonTiledSinglePlane(
      const PixelFormatAndModifier& pixel_format_and_modifier) const override {
    // this class only supports tiled formats
    return false;
  }

  uint32_t ImageFormatRowBytesMinDivisor(
      const PixelFormatAndModifier& pixel_format_and_modifier) const override {
    ZX_ASSERT(IsSupported(pixel_format_and_modifier));
    return GetTilingData(GetTilingTypeForPixelFormat(pixel_format_and_modifier))
        .bytes_per_row_per_tile.ValueOrDie();
  }

  CheckedNumeric<uint32_t> ImageFormatMinimumRowBytes(
      const fuchsia_sysmem2::ImageFormatConstraints& constraints,
      CheckedNumeric<uint32_t> width) const override {
    auto pixel_format_and_modifier = PixelFormatAndModifierFromConstraints(constraints);
    ZX_ASSERT(IsSupported(pixel_format_and_modifier));

    // Caller must set pixel_format.
    ZX_ASSERT(constraints.pixel_format().has_value());

    if (constraints.size_alignment().has_value()) {
      width = CheckRoundUp(width, safemath::CheckedNumeric(constraints.size_alignment()->width()));
    }

    if (!width.IsValid()) {
      return kInvalidCheckedNumeric32;
    }

    if ((constraints.min_size().has_value() &&
         width.ValueOrDie() < constraints.min_size()->width()) ||
        (constraints.max_size().has_value() &&
         width.ValueOrDie() > constraints.max_size()->width())) {
      return kInvalidCheckedNumeric32;
    }
    CheckedNumeric<uint32_t> constraints_min_bytes_per_row =
        constraints.min_bytes_per_row().has_value() ? constraints.min_bytes_per_row().value() : 0;
    uint32_t bytes_per_row_divisor = constraints.bytes_per_row_divisor().has_value()
                                         ? constraints.bytes_per_row_divisor().value()
                                         : 1;
    if (bytes_per_row_divisor == 0) {
      return kInvalidCheckedNumeric32;
    }

    // The row bytes must be a multiple of both the constraints' divisor and the tile width in
    // bytes, so use their least common multiple.
    const uint32_t bytes_per_row_per_tile =
        ImageFormatRowBytesMinDivisor(pixel_format_and_modifier);
    CheckedNumeric<uint32_t> constraints_bytes_per_row_divisor =
        CheckMul(bytes_per_row_divisor / std::gcd(bytes_per_row_divisor, bytes_per_row_per_tile),
                 bytes_per_row_per_tile);

    // This code should match the code in garnet/public/rust/fuchsia-framebuffer/src/sysmem.rs.
    CheckedNumeric<uint32_t> non_padding_bytes_per_row =
        CheckedNumeric(ImageFormatStrideBytesPerWidthPixel(pixel_format_and_modifier)) * width;
    if (!non_padding_bytes_per_row.IsValid()) {
      return kInvalidCheckedNumeric32;
    }

    CheckedNumeric<uint32_t> minimum_row_bytes =
        CheckRoundUp(safemath::CheckMax(non_padding_bytes_per_row, constraints_min_bytes_per_row),
                     constraints_bytes_per_row_divisor);
    if (!minimum_row_bytes.IsValid()) {
      return kInvalidCheckedNumeric32;
    }
    if (constraints.max_bytes_per_row().has_value() &&
        minimum_row_bytes.ValueOrDie() > constraints.max_bytes_per_row().value()) {
      return kInvalidCheckedNumeric32;
    }
    return minimum_row_bytes;
  }

 private:
  struct TilingData {
    CheckedNumeric<uint32_t> tile_rows;
    CheckedNumeric<uint32_t> bytes_per_row_per_tile;
  };

  // These are base Intel tilings, with no aux buffers.
  enum class TilingType { kX, kY, kYf };

  // See
  // https://01.org/sites/default/files/documentation/intel-gfx-prm-osrc-skl-vol05-memory_views.pdf
  static constexpr CheckedNumeric<uint32_t> kIntelTileByteSize = 4096;
  static constexpr TilingData kTilingData[] = {
      {
          // kX
          .tile_rows = 8,
          .bytes_per_row_per_tile = 512,
      },
      {
          // kY
          .tile_rows = 32,
          .bytes_per_row_per_tile = 128,
      },
      {
          // kYf
          .tile_rows = 32,
          .bytes_per_row_per_tile = 128,
      },
  };

  // Auxiliary buffers use fixed plane index 3 across image-format (matching
  // kTransactionEliminationPlane) so callers do not depend on the number of color planes. Must
  // remain 3 for ABI compatibility with callers (e.g. Magma).
  static constexpr uint32_t kCcsPlane = 3;

  // See https://01.org/sites/default/files/documentation/intel-gfx-prm-osrc-kbl-vol12-display.pdf
  // for a description of the color control surface. The CCS is always Y-tiled. A CCS cache-line
  // (64 bytes, so 2 fit horizontally in a tile) represents 16 horizontal cache line pairs (so 16
  // tiles) and 16 pixels tall.
  static constexpr CheckedNumeric<uint32_t> kCcsTileWidthRatio = 2 * 16;
  static constexpr CheckedNumeric<uint32_t> kCcsTileHeightRatio = 16;

  static TilingType GetTilingTypeForPixelFormat(const PixelFormatAndModifier& pixel_format) {
    auto modifier_without_ccs_bit = static_cast<fuchsia_images2::PixelFormatModifier>(
        fidl::ToUnderlying(pixel_format.pixel_format_modifier) &
        ~fuchsia_images2::kFormatModifierIntelCcsBit);
    switch (modifier_without_ccs_bit) {
      case fuchsia_images2::PixelFormatModifier::kIntelI915XTiled:
        return TilingType::kX;

      case fuchsia_images2::PixelFormatModifier::kIntelI915YTiled:
        return TilingType::kY;

      case fuchsia_images2::PixelFormatModifier::kIntelI915YfTiled:
        return TilingType::kYf;
      default:
        ZX_PANIC("Not an Intel tiled modifier: 0x%" PRIx64,
                 fidl::ToUnderlying(pixel_format.pixel_format_modifier));
    }
  }

  static const TilingData& GetTilingData(TilingType type) {
    static_assert(static_cast<size_t>(TilingType::kYf) < std::size(kTilingData));
    ZX_ASSERT(static_cast<uint32_t>(type) < std::size(kTilingData));
    return kTilingData[static_cast<uint32_t>(type)];
  }

  // Gets the total size (in tiles) of image data for non-aux planes
  static void GetSizeInTiles(const ImageFormat& image_format, uint32_t plane,
                             CheckedNumeric<uint32_t>* width_out,
                             CheckedNumeric<uint32_t>* height_out) {
    ZX_ASSERT(width_out);
    ZX_ASSERT(height_out);
    *width_out = kInvalidCheckedNumeric32;
    *height_out = kInvalidCheckedNumeric32;

    auto pixel_format_and_modifier = PixelFormatAndModifierFromImageFormat(image_format);
    const auto& tiling_data = GetTilingData(GetTilingTypeForPixelFormat(pixel_format_and_modifier));
    ZX_ASSERT(image_format.bytes_per_row().has_value());
    ZX_ASSERT(image_format.size().has_value());
    CheckedNumeric<uint32_t> bytes_per_row = image_format.bytes_per_row().value();

    const auto& bytes_per_row_per_tile = tiling_data.bytes_per_row_per_tile;
    const auto& tile_rows = tiling_data.tile_rows;

    CheckedNumeric<uint32_t> min_bytes_per_row =
        CheckRoundUp(CheckedNumeric<uint64_t>(image_format.size()->width()) *
                         ImageFormatStrideBytesPerWidthPixel(pixel_format_and_modifier),
                     CheckedNumeric<uint64_t>(bytes_per_row_per_tile));
    if (!min_bytes_per_row.IsValid() ||
        image_format.bytes_per_row().value() < min_bytes_per_row.ValueOrDie()) {
      return;
    }

    switch (pixel_format_and_modifier.pixel_format) {
      case PixelFormat::kR8G8B8A8:
      case PixelFormat::kR8G8B8X8:
      case PixelFormat::kB8G8R8A8:
      case PixelFormat::kB8G8R8X8: {
        // Format only has one plane
        ZX_ASSERT(plane == 0);

        *width_out = CheckRoundUp(bytes_per_row, bytes_per_row_per_tile) / bytes_per_row_per_tile;
        *height_out =
            CheckRoundUp(CheckedNumeric(image_format.size()->height()), tile_rows) / tile_rows;
      } break;
      // Since NV12 is a biplanar format we must handle the size for each plane separately. From
      // https://github.com/intel/gmmlib/blob/e1f634c5d5a41ac48756b25697ea499605711747/Source/GmmLib/Texture/GmmTextureAlloc.cpp#L1192:
      // "For Tiled Planar surfaces, the planes must be tile-boundary aligned." Meaning that each
      // plane must be separately tiled aligned.
      case PixelFormat::kNv12:
        if (plane == 0) {
          // Calculate the Y plane size (8 bpp)
          *width_out = CheckRoundUp(bytes_per_row, bytes_per_row_per_tile) / bytes_per_row_per_tile;
          *height_out =
              CheckRoundUp(CheckedNumeric(image_format.size()->height()), tile_rows) / tile_rows;
        } else if (plane == 1) {
          // Calculate the UV plane size (4 bpp)
          // We effectively have 1/2 the height of our original image since we are subsampled at
          // 4:2:0. Since width of the Y plane must match the width of the UV plane we divide the
          // height of the Y plane by 2 to calculate the height of the UV plane (aligned on tile
          // height boundaries). Ensure the height is aligned 2 before dividing.
          CheckedNumeric<uint32_t> adjusted_height =
              CheckRoundUp<uint32_t, uint32_t>(image_format.size()->height(), 2) / 2;

          *width_out = CheckRoundUp(bytes_per_row, bytes_per_row_per_tile) / bytes_per_row_per_tile;
          *height_out = CheckRoundUp(adjusted_height, tile_rows) / tile_rows;
        } else {
          ZX_PANIC("Invalid NV12 plane: %u", plane);
        }
        break;
      default:
        ZX_PANIC("Unsupported pixel format: %u",
                 sysmem::fidl_underlying_cast(pixel_format_and_modifier.pixel_format));
    }

    // if either of width_out or height_out is invalid at this point, ensure both are invalid
    if (!width_out->IsValid()) {
      *height_out = kInvalidCheckedNumeric32;
    }
    if (!height_out->IsValid()) {
      *width_out = kInvalidCheckedNumeric32;
    }
  }

  static bool Is8888Rgb(PixelFormat pixel_format) {
    switch (pixel_format) {
      case PixelFormat::kR8G8B8A8:
      case PixelFormat::kR8G8B8X8:
      case PixelFormat::kB8G8R8A8:
      case PixelFormat::kB8G8R8X8:
        return true;
      default:
        return false;
    }
  }

  static bool FormatHasCcs(const PixelFormatAndModifier& pixel_format) {
    return fidl::ToUnderlying(pixel_format.pixel_format_modifier) &
           fuchsia_images2::kFormatModifierIntelCcsBit;
  }

  // Does not include aux planes
  static uint32_t FormatNumOfPlanes(const PixelFormat& pixel_format) {
    if (Is8888Rgb(pixel_format)) {
      return 1u;
    }
    if (pixel_format == PixelFormat::kNv12) {
      return 2u;
    }
    ZX_PANIC("Unsupported pixel format: %u", sysmem::fidl_underlying_cast(pixel_format));
  }

  static CheckedNumeric<uint64_t> CcsWidthInTiles(
      CheckedNumeric<uint32_t> main_plane_width_in_tiles) {
    return CheckRoundUp(main_plane_width_in_tiles, kCcsTileWidthRatio) / kCcsTileWidthRatio;
  }

  static CheckedNumeric<uint64_t> CcsSize(CheckedNumeric<uint32_t> width_in_tiles,
                                          CheckedNumeric<uint32_t> height_in_tiles) {
    CheckedNumeric<uint32_t> height_in_ccs_tiles =
        CheckRoundUp(height_in_tiles, kCcsTileHeightRatio) / kCcsTileHeightRatio;
    return CcsWidthInTiles(width_in_tiles) * height_in_ccs_tiles * kIntelTileByteSize;
  }
};

class AfbcFormats : public ImageFormatSet {
 public:
  const char* Name() const override { return "AfbcFormats"; }

  static constexpr uint64_t kAfbcModifierMask =
      fuchsia_images2::kFormatModifierArmTeBit | fuchsia_images2::kFormatModifierArmSplitBlockBit |
      fuchsia_images2::kFormatModifierArmSparseBit | fuchsia_images2::kFormatModifierArmYuvBit |
      fuchsia_images2::kFormatModifierArmBchBit | fuchsia_images2::kFormatModifierArmTiledHeaderBit;
  static constexpr uint32_t kBytesPerPixel = 4;
  bool IsSupported(const PixelFormatAndModifier& pixel_format) const override {
    if (pixel_format.pixel_format != PixelFormatWire::kR8G8B8A8 &&
        pixel_format.pixel_format != PixelFormatWire::kR8G8B8X8 &&
        pixel_format.pixel_format != PixelFormatWire::kB8G8R8A8 &&
        pixel_format.pixel_format != PixelFormatWire::kB8G8R8X8) {
      return false;
    }
    auto modifier_without_afbc = static_cast<fuchsia_images2::PixelFormatModifier>(
        fidl::ToUnderlying(pixel_format.pixel_format_modifier) & ~kAfbcModifierMask);
    switch (modifier_without_afbc) {
      case fuchsia_images2::PixelFormatModifier::kArmAfbc16X16:
      case fuchsia_images2::PixelFormatModifier::kArmAfbc32X8:
        return true;
      default:
        return false;
    }
  }

  struct BlockLayout {
    uint32_t block_width;
    uint32_t block_height;
    uint32_t width_alignment;
    uint32_t height_alignment;
    bool tiled_header;
  };

  static BlockLayout GetBlockLayout(const PixelFormatAndModifier& pixel_format) {
    // For <=32-bpp formats (all formats supported by `AfbcFormats`),
    // `kFormatModifierArmTiledHeaderBit` groups 8x8 blocks into each header tile.
    constexpr uint32_t kTiledHeaderWidthInBlocks = 8;
    constexpr uint32_t kTiledHeaderHeightInBlocks = 8;
    bool tiled_header = fidl::ToUnderlying(pixel_format.pixel_format_modifier) &
                        fuchsia_images2::kFormatModifierArmTiledHeaderBit;
    uint32_t width_blocks = tiled_header ? kTiledHeaderWidthInBlocks : 1u;
    uint32_t height_blocks = tiled_header ? kTiledHeaderHeightInBlocks : 1u;
    auto modifier_without_afbc = static_cast<fuchsia_images2::PixelFormatModifier>(
        fidl::ToUnderlying(pixel_format.pixel_format_modifier) & ~kAfbcModifierMask);
    switch (modifier_without_afbc) {
      case fuchsia_images2::PixelFormatModifier::kArmAfbc16X16:
        return BlockLayout{
            .block_width = 16,
            .block_height = 16,
            .width_alignment = 16u * width_blocks,
            .height_alignment = 16u * height_blocks,
            .tiled_header = tiled_header,
        };
      case fuchsia_images2::PixelFormatModifier::kArmAfbc32X8:
        return BlockLayout{
            .block_width = 32,
            .block_height = 8,
            .width_alignment = 32u * width_blocks,
            .height_alignment = 8u * height_blocks,
            .tiled_header = tiled_header,
        };
      default:
        ZX_PANIC("Unsupported AFBC modifier: 0x%" PRIx64,
                 fidl::ToUnderlying(pixel_format.pixel_format_modifier));
    }
  }

  // Whether the image has a transaction elimination buffer (plane 3).
  static bool HasTe(const ImageFormat& image_format) {
    return fidl::ToUnderlying(image_format.pixel_format_modifier().value()) &
           fuchsia_images2::kFormatModifierArmTeBit;
  }

  // Computed AFBC layout follows `bytes_per_row` (matching `IntelTiledFormats` and `PvricFormats`
  // when `bytes_per_row` is populated by `ImageConstraintsToFormat()` or sysmem), rounding any
  // unaligned `bytes_per_row` at or above the minimum for `size.width` up to
  // `width_alignment * kBytesPerPixel`. Hardware without a compressed row-stride register, such as
  // the Amlogic VIU OSD, would not match a padded `bytes_per_row`, and nothing enforces no padding
  // today. Sysmem is the place to enforce no stride padding, if it is ever modeled.
  static CheckedNumeric<uint32_t> AlignedRowBytes(const ImageFormat& image_format,
                                                  const BlockLayout& layout) {
    ZX_ASSERT(image_format.size().has_value());
    ZX_ASSERT(image_format.bytes_per_row().has_value());
    CheckedNumeric<uint32_t> aligned_min_width =
        CheckRoundUp(CheckedNumeric<uint32_t>(image_format.size()->width()),
                     CheckedNumeric<uint32_t>(layout.width_alignment));
    CheckedNumeric<uint32_t> min_bytes_per_row = CheckMul(aligned_min_width, kBytesPerPixel);
    if (!min_bytes_per_row.IsValid() ||
        image_format.bytes_per_row().value() < min_bytes_per_row.ValueOrDie()) {
      return kInvalidCheckedNumeric32;
    }
    CheckedNumeric<uint32_t> row_bytes_divisor = layout.width_alignment * kBytesPerPixel;
    return CheckRoundUp(CheckedNumeric<uint32_t>(image_format.bytes_per_row().value()),
                        row_bytes_divisor);
  }

  // Returns the horizontal pixel span implied by `AlignedRowBytes()`, including
  // `layout.width_alignment` (so 32x8 with `TILED_HEADER` aligns width to 256 pixels) and any
  // `bytes_per_row` padding, for computing plane 0's `NonTESize()` (and therefore plane 3's byte
  // offset when TE is present). Plane 3's own CRC row stride and size depend only on logical
  // `size.width` and `size.height`; see `arm_transaction_elimination_buffer_size()`.
  static CheckedNumeric<uint32_t> AlignedWidth(const ImageFormat& image_format,
                                               const BlockLayout& layout) {
    return AlignedRowBytes(image_format, layout) / kBytesPerPixel;
  }

  // Calculate the size of the Raw AFBC image without a transaction elimination buffer.
  CheckedNumeric<uint64_t> NonTESize(const ImageFormat& image_format) const {
    // See
    // https://android.googlesource.com/device/linaro/hikey/+/android-o-preview-3/gralloc960/alloc_device.cpp
    constexpr CheckedNumeric<uint64_t> kAfbcBodyAlignment = 1024u;
    constexpr CheckedNumeric<uint64_t> kTiledAfbcBodyAlignment = 4096u;
    constexpr CheckedNumeric<uint64_t> kBytesPerBlockHeader = 16;

    auto pixel_format_and_modifier = PixelFormatAndModifierFromImageFormat(image_format);
    ZX_ASSERT(IsSupported(pixel_format_and_modifier));
    BlockLayout layout = GetBlockLayout(pixel_format_and_modifier);
    CheckedNumeric<uint64_t> body_alignment =
        layout.tiled_header ? kTiledAfbcBodyAlignment : kAfbcBodyAlignment;

    ZX_ASSERT(image_format.size().has_value());
    CheckedNumeric<uint64_t> aligned_width =
        static_cast<CheckedNumeric<uint64_t>>(AlignedWidth(image_format, layout));
    CheckedNumeric<uint64_t> aligned_height =
        CheckRoundUp(CheckedNumeric<uint64_t>(image_format.size()->height()),
                     CheckedNumeric<uint64_t>(layout.height_alignment));
    CheckedNumeric<uint64_t> block_count =
        aligned_width / layout.block_width * aligned_height / layout.block_height;
    return (block_count * layout.block_width * layout.block_height * kBytesPerPixel) +
           CheckRoundUp(block_count * kBytesPerBlockHeader, body_alignment);
  }

  CheckedNumeric<uint64_t> ImageFormatImageSize(const ImageFormat& image_format) const override {
    CheckedNumeric<uint64_t> size = NonTESize(image_format);
    if (HasTe(image_format)) {
      // Plane 0's size `size` (and therefore plane 3's byte offset) follows `AlignedWidth()` /
      // `bytes_per_row`, while the TE CRC table itself is sized from logical `size.width` and
      // `size.height`; see `arm_transaction_elimination_buffer_size()`.
      size += arm_transaction_elimination_buffer_size(size, image_format.size()->width(),
                                                      image_format.size()->height());
    }

    return size;
  }

  CheckedNumeric<uint64_t> ImageFormatPlaneByteOffset(const ImageFormat& image_format,
                                                      uint32_t plane) const override {
    auto pixel_format_and_modifier = PixelFormatAndModifierFromImageFormat(image_format);
    ZX_ASSERT(IsSupported(pixel_format_and_modifier));
    if (plane == 0) {
      return 0;
    }
    if (plane == kTransactionEliminationPlane && HasTe(image_format)) {
      return CheckRoundUp(NonTESize(image_format), kTransactionEliminationAlignment);
    }
    // invalid plane param value
    return kInvalidCheckedNumeric64;
  }
  CheckedNumeric<uint32_t> ImageFormatPlaneRowBytes(const ImageFormat& image_format,
                                                    uint32_t plane) const override {
    auto pixel_format_and_modifier = PixelFormatAndModifierFromImageFormat(image_format);
    ZX_ASSERT(IsSupported(pixel_format_and_modifier));
    BlockLayout layout = GetBlockLayout(pixel_format_and_modifier);
    if (plane == 0) {
      return AlignedRowBytes(image_format, layout);
    }
    if (plane == kTransactionEliminationPlane && HasTe(image_format)) {
      // Validate `size` and `bytes_per_row` consistently with all other layout queries, even though
      // plane 3's CRC row stride depends only on logical `size.width`.
      if (!AlignedRowBytes(image_format, layout).IsValid()) {
        return kInvalidCheckedNumeric32;
      }
      return arm_transaction_elimination_row_size(image_format.size()->width());
    }
    // invalid plane param value
    return kInvalidCheckedNumeric32;
  }
  bool ImageFormatIsNonTiledSinglePlane(
      const PixelFormatAndModifier& pixel_format_and_modifier) const override {
    ZX_ASSERT(IsSupported(pixel_format_and_modifier));
    // only tiled formats are supported by this class
    return false;
  }
  uint32_t ImageFormatRowBytesMinDivisor(
      const PixelFormatAndModifier& pixel_format_and_modifier) const override {
    ZX_ASSERT(IsSupported(pixel_format_and_modifier));
    return GetBlockLayout(pixel_format_and_modifier).width_alignment * kBytesPerPixel;
  }
  CheckedNumeric<uint32_t> ImageFormatMinimumRowBytes(
      const fuchsia_sysmem2::ImageFormatConstraints& constraints,
      CheckedNumeric<uint32_t> width) const override {
    auto pixel_format_and_modifier = PixelFormatAndModifierFromConstraints(constraints);
    ZX_ASSERT(IsSupported(pixel_format_and_modifier));
    if (constraints.size_alignment().has_value()) {
      width = CheckRoundUp(width, CheckedNumeric(constraints.size_alignment()->width()));
    }
    if (!width.IsValid()) {
      return kInvalidCheckedNumeric32;
    }

    if (constraints.min_size().has_value() &&
        width.ValueOrDie() < constraints.min_size()->width()) {
      return kInvalidCheckedNumeric32;
    }
    if (constraints.max_size().has_value() &&
        width.ValueOrDie() > constraints.max_size()->width()) {
      return kInvalidCheckedNumeric32;
    }

    // The image is laid out in whole blocks, and with a tiled header the width is aligned further
    // to a whole number of header tiles.
    BlockLayout layout = GetBlockLayout(pixel_format_and_modifier);
    const uint32_t block_row_bytes_divisor =
        ImageFormatRowBytesMinDivisor(pixel_format_and_modifier);
    CheckedNumeric<uint32_t> width_in_pixels =
        CheckRoundUp(width, CheckedNumeric<uint32_t>(layout.width_alignment));
    CheckedNumeric<uint32_t> width_in_bytes = CheckMul(width_in_pixels, kBytesPerPixel);

    // Apply the row constraints, as for other tiled formats: the row bytes must be a multiple of
    // both the constraints' divisor and the block/tiled-header width in bytes, so use their least
    // common multiple.
    CheckedNumeric<uint32_t> constraints_min_bytes_per_row =
        constraints.min_bytes_per_row().has_value() ? constraints.min_bytes_per_row().value() : 0;
    uint32_t bytes_per_row_divisor = constraints.bytes_per_row_divisor().has_value()
                                         ? constraints.bytes_per_row_divisor().value()
                                         : 1;
    if (bytes_per_row_divisor == 0) {
      return kInvalidCheckedNumeric32;
    }
    CheckedNumeric<uint32_t> effective_divisor =
        CheckMul(bytes_per_row_divisor / std::gcd(bytes_per_row_divisor, block_row_bytes_divisor),
                 block_row_bytes_divisor);
    CheckedNumeric<uint32_t> minimum_row_bytes = CheckRoundUp(
        safemath::CheckMax(width_in_bytes, constraints_min_bytes_per_row), effective_divisor);
    if (!minimum_row_bytes.IsValid()) {
      return kInvalidCheckedNumeric32;
    }
    if (constraints.max_bytes_per_row().has_value() &&
        minimum_row_bytes.ValueOrDie() > constraints.max_bytes_per_row().value()) {
      return kInvalidCheckedNumeric32;
    }
    return minimum_row_bytes;
  }
};

// In 4:2:0 YUV formats, each row of chroma samples covers 2 rows of luma samples. An odd last luma
// row still needs a chroma row, so the chroma row count is rounded up.
constexpr CheckedNumeric<uint64_t> kYuv420LumaRowsPerChromaRow = 2;

CheckedNumeric<uint64_t> yuv420_chroma_rows(CheckedNumeric<uint64_t> luma_rows) {
  return CheckRoundUp(luma_rows, kYuv420LumaRowsPerChromaRow) / kYuv420LumaRowsPerChromaRow;
}

CheckedNumeric<uint64_t> linear_size(CheckedNumeric<uint32_t> surface_height_param,
                                     CheckedNumeric<uint32_t> bytes_per_row, PixelFormat type) {
  CheckedNumeric<uint64_t> surface_height = surface_height_param;
  switch (type) {
    case PixelFormat::kR8G8B8A8:
    case PixelFormat::kR8G8B8X8:
    case PixelFormat::kB8G8R8A8:
    case PixelFormat::kB8G8R8X8:
    case PixelFormat::kB8G8R8:
    case PixelFormat::kR8G8B8:
    case PixelFormat::kR5G6B5:
    case PixelFormat::kR3G3B2:
    case PixelFormat::kR2G2B2X2:
    case PixelFormat::kL8:
    case PixelFormat::kR8:
    case PixelFormat::kR8G8:
    case PixelFormat::kA2B10G10R10:
    case PixelFormat::kA2R10G10B10:
    case PixelFormat::kYuy2:
      return surface_height * bytes_per_row;
    case PixelFormat::kI420:
    case PixelFormat::kM420:
    case PixelFormat::kNv12:
    case PixelFormat::kYv12:
    case PixelFormat::kP010: {
      // 4:2:0: each chroma row takes bytes_per_row bytes (for I420 and YV12, two planes of
      // bytes_per_row / 2 bytes). For an even height, the size is 1.5 times the luma size.
      CheckedNumeric<uint64_t> chroma_rows = yuv420_chroma_rows(surface_height);
      return (surface_height + chroma_rows) * bytes_per_row;
    }
    default:
      return 0u;
  }
}

CheckedNumeric<uint32_t> linear_minimum_row_bytes(
    const fuchsia_sysmem2::ImageFormatConstraints& constraints, CheckedNumeric<uint32_t> width) {
  // Caller must set pixel_format.
  ZX_ASSERT(constraints.pixel_format().has_value());

  // We don't enforce that width is already aligned up by the caller by the time of this call, but
  // if the caller is a producer, the caller is nonetheless expected to conform to the
  // size_alignment.width divisibility requirement for any fuchsia.sysmem.ImageFormat2.coded_width
  // or fuchsia.images2.ImageFormat.size.width values the caller generates.
  //
  // The aligned width is what the row bytes are computed for, so it's the aligned width that must
  // be within min_size and max_size.
  if (constraints.size_alignment().has_value()) {
    width = CheckRoundUp(width, CheckedNumeric(constraints.size_alignment()->width()));
  }
  if (!width.IsValid()) {
    return kInvalidCheckedNumeric32;
  }

  if ((constraints.min_size().has_value() &&
       width.ValueOrDie() < constraints.min_size()->width()) ||
      (constraints.max_size().has_value() &&
       width.ValueOrDie() > constraints.max_size()->width())) {
    return kInvalidCheckedNumeric32;
  }

  CheckedNumeric<uint32_t> constraints_min_bytes_per_row =
      constraints.min_bytes_per_row().has_value() ? constraints.min_bytes_per_row().value() : 0;
  CheckedNumeric<uint32_t> constraints_bytes_per_row_divisor =
      constraints.bytes_per_row_divisor().has_value() ? constraints.bytes_per_row_divisor().value()
                                                      : 1;

  // This code should match the code in garnet/public/rust/fuchsia-framebuffer/src/sysmem.rs.
  CheckedNumeric<uint32_t> minimum_row_bytes = CheckRoundUp(
      safemath::CheckMax(
          ImageFormatStrideBytesPerWidthPixel(PixelFormatAndModifierFromConstraints(constraints)) *
              width,
          constraints_min_bytes_per_row),
      constraints_bytes_per_row_divisor);
  if (!minimum_row_bytes.IsValid()) {
    return kInvalidCheckedNumeric32;
  }
  if (constraints.max_bytes_per_row().has_value() &&
      minimum_row_bytes.ValueOrDie() > constraints.max_bytes_per_row().value()) {
    return kInvalidCheckedNumeric32;
  }
  return minimum_row_bytes;
}

// The pixel formats that have a linear layout (see linear_size()).
bool linear_is_supported_pixel_format(fuchsia_images2::PixelFormat pixel_format) {
  switch (pixel_format) {
    case PixelFormat::kInvalid:
    case PixelFormat::kDoNotCare:
    case PixelFormat::kMjpeg:
      return false;
    case PixelFormat::kR8G8B8A8:
    case PixelFormat::kR8G8B8X8:
    case PixelFormat::kB8G8R8A8:
    case PixelFormat::kB8G8R8X8:
    case PixelFormat::kB8G8R8:
    case PixelFormat::kR8G8B8:
    case PixelFormat::kI420:
    case PixelFormat::kM420:
    case PixelFormat::kNv12:
    case PixelFormat::kP010:
    case PixelFormat::kYuy2:
    case PixelFormat::kYv12:
    case PixelFormat::kR5G6B5:
    case PixelFormat::kR3G3B2:
    case PixelFormat::kR2G2B2X2:
    case PixelFormat::kL8:
    case PixelFormat::kR8:
    case PixelFormat::kR8G8:
    case PixelFormat::kA2B10G10R10:
    case PixelFormat::kA2R10G10B10:
      return true;
    default:
      return false;
  }
}

bool linear_is_single_plane(fuchsia_images2::PixelFormat pixel_format) {
  switch (pixel_format) {
    case fuchsia_images2::PixelFormat::kR8G8B8A8:
    case fuchsia_images2::PixelFormat::kR8G8B8X8:
    case fuchsia_images2::PixelFormat::kB8G8R8A8:
    case fuchsia_images2::PixelFormat::kB8G8R8X8:
    case fuchsia_images2::PixelFormat::kB8G8R8:
    case fuchsia_images2::PixelFormat::kR5G6B5:
    case fuchsia_images2::PixelFormat::kR3G3B2:
    case fuchsia_images2::PixelFormat::kR2G2B2X2:
    case fuchsia_images2::PixelFormat::kL8:
    case fuchsia_images2::PixelFormat::kR8:
    case fuchsia_images2::PixelFormat::kR8G8:
    case fuchsia_images2::PixelFormat::kA2R10G10B10:
    case fuchsia_images2::PixelFormat::kA2B10G10R10:
    case fuchsia_images2::PixelFormat::kYuy2:
    case fuchsia_images2::PixelFormat::kR8G8B8:
      return true;
    case fuchsia_images2::PixelFormat::kInvalid:
    case fuchsia_images2::PixelFormat::kDoNotCare:
    case fuchsia_images2::PixelFormat::kI420:
    // M420 has only plane 0 (its chroma rows are interleaved with its luma rows), but its rows
    // aren't all the same kind, so it isn't treated as a simple single-plane format.
    case fuchsia_images2::PixelFormat::kM420:
    case fuchsia_images2::PixelFormat::kNv12:
    case fuchsia_images2::PixelFormat::kP010:
    case fuchsia_images2::PixelFormat::kMjpeg:
    case fuchsia_images2::PixelFormat::kYv12:
    default:
      return false;
  }
}

CheckedNumeric<uint32_t> linear_validated_bytes_per_row(
    const ImageFormat& image_format, const PixelFormatAndModifier& pixel_format_and_modifier) {
  ZX_ASSERT(image_format.size().has_value());
  ZX_ASSERT(image_format.bytes_per_row().has_value());
  CheckedNumeric<uint32_t> min_bytes_per_row =
      CheckedNumeric<uint64_t>(image_format.size()->width()) *
      ImageFormatStrideBytesPerWidthPixel(pixel_format_and_modifier);
  if (!min_bytes_per_row.IsValid() ||
      image_format.bytes_per_row().value() < min_bytes_per_row.ValueOrDie()) {
    return kInvalidCheckedNumeric32;
  }
  return image_format.bytes_per_row().value();
}

class LinearFormats : public ImageFormatSet {
 public:
  const char* Name() const override { return "LinearFormats"; }

  bool IsSupported(const PixelFormatAndModifier& pixel_format) const override {
    if (pixel_format.pixel_format_modifier != fuchsia_images2::PixelFormatModifier::kLinear) {
      return false;
    }
    return linear_is_supported_pixel_format(pixel_format.pixel_format);
  }

  CheckedNumeric<uint64_t> ImageFormatImageSize(const ImageFormat& image_format) const override {
    ZX_ASSERT(image_format.pixel_format().has_value());
    auto pixel_format_and_modifier = PixelFormatAndModifierFromImageFormat(image_format);
    ZX_ASSERT(IsSupported(pixel_format_and_modifier));
    CheckedNumeric<uint32_t> bytes_per_row =
        linear_validated_bytes_per_row(image_format, pixel_format_and_modifier);
    CheckedNumeric<uint32_t> surface_height = image_format.size()->height();
    return linear_size(surface_height, bytes_per_row, pixel_format_and_modifier.pixel_format);
  }

  CheckedNumeric<uint64_t> ImageFormatPlaneByteOffset(const ImageFormat& image_format,
                                                      uint32_t plane) const override {
    if (plane == 0) {
      return 0;
    }
    auto pixel_format_and_modifier = PixelFormatAndModifierFromImageFormat(image_format);
    if (plane == 1) {
      switch (pixel_format_and_modifier.pixel_format) {
        case PixelFormat::kP010:
        case PixelFormat::kNv12:
        case PixelFormat::kI420:
        case PixelFormat::kYv12: {
          CheckedNumeric<uint32_t> bytes_per_row =
              linear_validated_bytes_per_row(image_format, pixel_format_and_modifier);
          return CheckedNumeric<uint64_t>(image_format.size()->height()) * bytes_per_row;
        }
        default:
          return kInvalidCheckedNumeric64;
      }
    }
    if (plane == 2) {
      switch (pixel_format_and_modifier.pixel_format) {
        case PixelFormat::kI420:
        case PixelFormat::kYv12: {
          CheckedNumeric<uint32_t> bytes_per_row =
              linear_validated_bytes_per_row(image_format, pixel_format_and_modifier);
          CheckedNumeric<uint64_t> height = image_format.size()->height();
          CheckedNumeric<uint64_t> luma_bytes = height * bytes_per_row;
          CheckedNumeric<uint64_t> one_chroma_plane_bytes =
              yuv420_chroma_rows(height) * (CheckedNumeric<uint64_t>(bytes_per_row) / 2);
          CheckedNumeric<uint64_t> offset_just_past_luma_and_one_chroma =
              luma_bytes + one_chroma_plane_bytes;
          // 2nd chroma plane is just past luma and 1st chroma plane
          return offset_just_past_luma_and_one_chroma;
        }
        default:
          return kInvalidCheckedNumeric64;
      }
    }

    // invalid plane param value
    return kInvalidCheckedNumeric64;
  }

  CheckedNumeric<uint32_t> ImageFormatPlaneRowBytes(const ImageFormat& image_format,
                                                    uint32_t plane) const override {
    auto pixel_format_and_modifier = PixelFormatAndModifierFromImageFormat(image_format);
    if (plane == 0) {
      return linear_validated_bytes_per_row(image_format, pixel_format_and_modifier);
    }
    if (plane == 1) {
      switch (pixel_format_and_modifier.pixel_format) {
        case PixelFormat::kNv12:
        case PixelFormat::kP010:
          return linear_validated_bytes_per_row(image_format, pixel_format_and_modifier);
        case PixelFormat::kI420:
        case PixelFormat::kYv12:
          return linear_validated_bytes_per_row(image_format, pixel_format_and_modifier) / 2;
        default:
          return kInvalidCheckedNumeric32;
      }
    } else if (plane == 2) {
      switch (pixel_format_and_modifier.pixel_format) {
        case PixelFormat::kI420:
        case PixelFormat::kYv12:
          return linear_validated_bytes_per_row(image_format, pixel_format_and_modifier) / 2;
        default:
          return kInvalidCheckedNumeric32;
      }
    }
    return kInvalidCheckedNumeric32;
  }

  bool ImageFormatIsNonTiledSinglePlane(
      const PixelFormatAndModifier& pixel_format_and_modifier) const override {
    return linear_is_single_plane(pixel_format_and_modifier.pixel_format);
  }

  uint32_t ImageFormatRowBytesMinDivisor(
      const PixelFormatAndModifier& pixel_format_and_modifier) const override {
    ZX_ASSERT(IsSupported(pixel_format_and_modifier));
    return 1u;
  }

  CheckedNumeric<uint32_t> ImageFormatMinimumRowBytes(
      const fuchsia_sysmem2::ImageFormatConstraints& constraints,
      CheckedNumeric<uint32_t> width) const override {
    return linear_minimum_row_bytes(constraints, width);
  }
};

constexpr LinearFormats kLinearFormats;

class GoldfishFormats : public ImageFormatSet {
 public:
  const char* Name() const override { return "GoldfishFormats"; }

  bool IsSupported(const PixelFormatAndModifier& pixel_format) const override {
    if (pixel_format.pixel_format_modifier !=
        fuchsia_images2::PixelFormatModifier::kGoogleGoldfishOptimal) {
      return false;
    }
    // The goldfish-optimal layout is host-managed and opaque, but guest-side image size, plane
    // layout, and minimum row bytes are modeled as linear, so only pixel formats with a linear
    // layout are supported.
    return linear_is_supported_pixel_format(pixel_format.pixel_format);
  }
  CheckedNumeric<uint64_t> ImageFormatImageSize(const ImageFormat& image_format) const override {
    ZX_ASSERT(image_format.pixel_format().has_value());
    auto pixel_format_and_modifier = PixelFormatAndModifierFromImageFormat(image_format);
    ZX_ASSERT(IsSupported(pixel_format_and_modifier));
    CheckedNumeric<uint32_t> bytes_per_row =
        linear_validated_bytes_per_row(image_format, pixel_format_and_modifier);
    CheckedNumeric<uint32_t> surface_height = image_format.size()->height();
    return linear_size(surface_height, bytes_per_row, pixel_format_and_modifier.pixel_format);
  }
  CheckedNumeric<uint64_t> ImageFormatPlaneByteOffset(const ImageFormat& image_format,
                                                      uint32_t plane) const override {
    ZX_ASSERT(IsSupported(PixelFormatAndModifierFromImageFormat(image_format)));
    return kLinearFormats.ImageFormatPlaneByteOffset(image_format, plane);
  }
  CheckedNumeric<uint32_t> ImageFormatPlaneRowBytes(const ImageFormat& image_format,
                                                    uint32_t plane) const override {
    ZX_ASSERT(IsSupported(PixelFormatAndModifierFromImageFormat(image_format)));
    return kLinearFormats.ImageFormatPlaneRowBytes(image_format, plane);
  }
  bool ImageFormatIsNonTiledSinglePlane(
      const PixelFormatAndModifier& pixel_format_and_modifier) const override {
    // The host-managed layout is opaque and may be tiled. Returning false here causes sysmem to
    // reject pad_for_block_size > 1 in either dimension, which is acceptable for now since
    // Goldfish buffer size is host-managed anyway and no guest participant should be demanding
    // pad_for_block_size adjustments.
    return false;
  }
  uint32_t ImageFormatRowBytesMinDivisor(
      const PixelFormatAndModifier& pixel_format_and_modifier) const override {
    ZX_ASSERT(IsSupported(pixel_format_and_modifier));
    return 1u;
  }
  CheckedNumeric<uint32_t> ImageFormatMinimumRowBytes(
      const fuchsia_sysmem2::ImageFormatConstraints& constraints,
      CheckedNumeric<uint32_t> width) const override {
    // Although the GOOGLE_GOLDFISH_OPTIMAL layout is host-managed, GoldfishFormats models the
    // guest-side image size and plane layout as linear (requiring ImageFormat.bytes_per_row).
    // Computing minimum row bytes using the linear rules keeps GoldfishFormats internally
    // consistent, enables ImageConstraintsToFormat(), and lets sysmem validate and size
    // GOOGLE_GOLDFISH_OPTIMAL uniformly alongside all other supported formats.
    return linear_minimum_row_bytes(constraints, width);
  }
};

class ArmTELinearFormats : public ImageFormatSet {
 public:
  const char* Name() const override { return "ArmTELinearFormats"; }

  bool IsSupported(const PixelFormatAndModifier& pixel_format) const override {
    if (pixel_format.pixel_format_modifier != fuchsia_images2::PixelFormatModifier::kArmLinearTe) {
      return false;
    }
    return linear_is_supported_pixel_format(pixel_format.pixel_format);
  }

  CheckedNumeric<uint64_t> ImageFormatImageSize(
      const fuchsia_images2::ImageFormat& image_format) const override {
    auto pixel_format_and_modifier = PixelFormatAndModifierFromImageFormat(image_format);
    ZX_ASSERT(IsSupported(pixel_format_and_modifier));
    CheckedNumeric<uint32_t> bytes_per_row =
        linear_validated_bytes_per_row(image_format, pixel_format_and_modifier);
    CheckedNumeric<uint64_t> size = linear_size(image_format.size()->height(), bytes_per_row,
                                                pixel_format_and_modifier.pixel_format);
    // Plane 0's linear byte size `size` (and therefore plane 3's byte offset) follows
    // `bytes_per_row`, while the TE CRC table itself is sized from logical `size.width` and
    // `size.height`; see `arm_transaction_elimination_buffer_size()`.
    CheckedNumeric<uint64_t> crc_size = arm_transaction_elimination_buffer_size(
        size, image_format.size()->width(), image_format.size()->height());
    return size + crc_size;
  }

  CheckedNumeric<uint64_t> ImageFormatPlaneByteOffset(
      const fuchsia_images2::ImageFormat& image_format, uint32_t plane) const override {
    if (plane < kTransactionEliminationPlane) {
      return kLinearFormats.ImageFormatPlaneByteOffset(image_format, plane);
    }
    if (plane == kTransactionEliminationPlane) {
      auto pixel_format_and_modifier = PixelFormatAndModifierFromImageFormat(image_format);
      ZX_ASSERT(IsSupported(pixel_format_and_modifier));
      CheckedNumeric<uint32_t> bytes_per_row =
          linear_validated_bytes_per_row(image_format, pixel_format_and_modifier);
      CheckedNumeric<uint64_t> size = linear_size(image_format.size()->height(), bytes_per_row,
                                                  pixel_format_and_modifier.pixel_format);
      return CheckRoundUp(size, CheckedNumeric<uint64_t>(64));
    }

    return kInvalidCheckedNumeric64;
  }

  CheckedNumeric<uint32_t> ImageFormatPlaneRowBytes(
      const fuchsia_images2::ImageFormat& image_format, uint32_t plane) const override {
    if (plane < kTransactionEliminationPlane) {
      return kLinearFormats.ImageFormatPlaneRowBytes(image_format, plane);
    }
    if (plane == kTransactionEliminationPlane) {
      auto pixel_format_and_modifier = PixelFormatAndModifierFromImageFormat(image_format);
      ZX_ASSERT(IsSupported(pixel_format_and_modifier));
      // Validate `size` and `bytes_per_row` consistently with all other layout queries, even though
      // plane 3's CRC row stride depends only on logical `size.width`.
      if (!linear_validated_bytes_per_row(image_format, pixel_format_and_modifier).IsValid()) {
        return kInvalidCheckedNumeric32;
      }
      return arm_transaction_elimination_row_size(image_format.size()->width());
    }
    return kInvalidCheckedNumeric32;
  }

  bool ImageFormatIsNonTiledSinglePlane(
      const PixelFormatAndModifier& pixel_format_and_modifier) const override {
    // The TE plane is a plane other than plane 0, so plane count is always at least 2.
    return false;
  }

  uint32_t ImageFormatRowBytesMinDivisor(
      const PixelFormatAndModifier& pixel_format_and_modifier) const override {
    ZX_ASSERT(IsSupported(pixel_format_and_modifier));
    return 1u;
  }

  CheckedNumeric<uint32_t> ImageFormatMinimumRowBytes(
      const fuchsia_sysmem2::ImageFormatConstraints& constraints,
      CheckedNumeric<uint32_t> width) const override {
    return linear_minimum_row_bytes(constraints, width);
  }
};

#if FUCHSIA_API_LEVEL_AT_LEAST(33)
class PvricFormats : public ImageFormatSet {
 public:
  const char* Name() const override { return "PvricFormats"; }

  bool IsSupported(const PixelFormatAndModifier& pixel_format) const override {
    switch (pixel_format.pixel_format) {
      case PixelFormat::kR8G8B8A8:
      case PixelFormat::kR8G8B8X8:
      case PixelFormat::kB8G8R8A8:
      case PixelFormat::kB8G8R8X8:
      case PixelFormat::kA2R10G10B10:
      case PixelFormat::kA2B10G10R10:
      case PixelFormat::kR5G6B5:
        break;
      default:
        return false;
    }
    switch (pixel_format.pixel_format_modifier) {
      case fuchsia_images2::PixelFormatModifier::kPvrFbcdc8X8V14:
      case fuchsia_images2::PixelFormatModifier::kPvrFbcdc16X4V14:
        return true;
      default:
        return false;
    }
  }

  CheckedNumeric<uint64_t> ImageFormatImageSize(const ImageFormat& image_format) const override {
    auto pixel_format_and_modifier = PixelFormatAndModifierFromImageFormat(image_format);
    ZX_ASSERT(IsSupported(pixel_format_and_modifier));

    CheckedNumeric<uint32_t> width_in_tiles, height_in_tiles;
    GetSizeInTiles(image_format, &width_in_tiles, &height_in_tiles);
    return HeaderSize(width_in_tiles, height_in_tiles) + DataSize(width_in_tiles, height_in_tiles);
  }

  CheckedNumeric<uint64_t> ImageFormatPlaneByteOffset(const ImageFormat& image_format,
                                                      uint32_t plane) const override {
    auto pixel_format_and_modifier = PixelFormatAndModifierFromImageFormat(image_format);
    ZX_ASSERT(IsSupported(pixel_format_and_modifier));

    if (plane == kPvricTilesPlane) {
      CheckedNumeric<uint32_t> width_in_tiles, height_in_tiles;
      GetSizeInTiles(image_format, &width_in_tiles, &height_in_tiles);
      return HeaderSize(width_in_tiles, height_in_tiles);
    }
    if (plane == kPvricHeaderPlane) {
      return 0;
    }
    return kInvalidCheckedNumeric64;
  }

  CheckedNumeric<uint32_t> ImageFormatPlaneRowBytes(const ImageFormat& image_format,
                                                    uint32_t plane) const override {
    auto pixel_format_and_modifier = PixelFormatAndModifierFromImageFormat(image_format);
    ZX_ASSERT(IsSupported(pixel_format_and_modifier));

    if (plane == kPvricTilesPlane) {
      CheckedNumeric<uint32_t> width_in_tiles, height_in_tiles;
      GetSizeInTiles(image_format, &width_in_tiles, &height_in_tiles);
      const auto& tiling_data =
          GetTilingData(GetTilingTypeForPixelFormat(pixel_format_and_modifier));
      return width_in_tiles * tiling_data.bytes_per_row_per_tile;
    }
    if (plane == kPvricHeaderPlane) {
      CheckedNumeric<uint32_t> width_in_tiles, height_in_tiles;
      GetSizeInTiles(image_format, &width_in_tiles, &height_in_tiles);
      return width_in_tiles;
    }
    return kInvalidCheckedNumeric32;
  }

  bool ImageFormatIsNonTiledSinglePlane(
      const PixelFormatAndModifier& pixel_format_and_modifier) const override {
    return false;
  }

  uint32_t ImageFormatRowBytesMinDivisor(
      const PixelFormatAndModifier& pixel_format_and_modifier) const override {
    ZX_ASSERT(IsSupported(pixel_format_and_modifier));
    return GetTilingData(GetTilingTypeForPixelFormat(pixel_format_and_modifier))
        .bytes_per_row_per_tile.ValueOrDie();
  }

  CheckedNumeric<uint32_t> ImageFormatMinimumRowBytes(
      const fuchsia_sysmem2::ImageFormatConstraints& constraints,
      CheckedNumeric<uint32_t> width) const override {
    auto pixel_format_and_modifier = PixelFormatAndModifierFromConstraints(constraints);
    ZX_ASSERT(IsSupported(pixel_format_and_modifier));

    // Caller must set pixel_format.
    ZX_ASSERT(constraints.pixel_format().has_value());

    if (constraints.size_alignment().has_value()) {
      width = CheckRoundUp(width, safemath::CheckedNumeric(constraints.size_alignment()->width()));
    }

    if (!width.IsValid()) {
      return kInvalidCheckedNumeric32;
    }

    if ((constraints.min_size().has_value() &&
         width.ValueOrDie() < constraints.min_size()->width()) ||
        (constraints.max_size().has_value() &&
         width.ValueOrDie() > constraints.max_size()->width())) {
      return kInvalidCheckedNumeric32;
    }
    uint32_t bytes_per_row_divisor = constraints.bytes_per_row_divisor().has_value()
                                         ? constraints.bytes_per_row_divisor().value()
                                         : 1;
    if (bytes_per_row_divisor == 0) {
      return kInvalidCheckedNumeric32;
    }

    // The row bytes must be a multiple of both the constraints' divisor and the tile width in
    // bytes, so use their least common multiple.
    const uint32_t bytes_per_row_per_tile =
        ImageFormatRowBytesMinDivisor(pixel_format_and_modifier);
    CheckedNumeric<uint32_t> constraints_bytes_per_row_divisor =
        CheckMul(bytes_per_row_divisor / std::gcd(bytes_per_row_divisor, bytes_per_row_per_tile),
                 bytes_per_row_per_tile);

    CheckedNumeric<uint32_t> non_padding_bytes_per_row =
        CheckedNumeric(ImageFormatStrideBytesPerWidthPixel(pixel_format_and_modifier)) * width;
    if (!non_padding_bytes_per_row.IsValid()) {
      return kInvalidCheckedNumeric32;
    }

    CheckedNumeric<uint32_t> constraints_min_bytes_per_row =
        constraints.min_bytes_per_row().has_value() ? constraints.min_bytes_per_row().value() : 0;
    CheckedNumeric<uint32_t> minimum_row_bytes =
        CheckRoundUp(safemath::CheckMax(non_padding_bytes_per_row, constraints_min_bytes_per_row),
                     constraints_bytes_per_row_divisor);
    if (!minimum_row_bytes.IsValid()) {
      return kInvalidCheckedNumeric32;
    }
    if (constraints.max_bytes_per_row().has_value() &&
        minimum_row_bytes.ValueOrDie() > constraints.max_bytes_per_row().value()) {
      return kInvalidCheckedNumeric32;
    }
    return minimum_row_bytes;
  }

 private:
  struct TilingData {
    CheckedNumeric<uint32_t> tile_rows;
    CheckedNumeric<uint32_t> bytes_per_row_per_tile;
  };

  enum class TilingType { k8x8, k16x4 };

  // Each PVRIC tile represents 256 bytes of uncompressed pixel data, with a 1-byte header per tile
  // in the header section aligned to 256 bytes.
  static constexpr CheckedNumeric<uint64_t> kPvricTileByteSize = 256;
  static constexpr CheckedNumeric<uint64_t> kPvricHeaderAlignment = 256;
  static constexpr uint32_t kPvricTilesPlane = 0;
  static constexpr uint32_t kPvricHeaderPlane = 3;

  static constexpr TilingData kTilingData[] = {
      {
          // k8x8: 8x8 at 32-bpp, 16x8 at 16-bpp
          .tile_rows = 8,
          .bytes_per_row_per_tile = 32,
      },
      {
          // k16x4: 16x4 at 32-bpp, 32x4 at 16-bpp
          .tile_rows = 4,
          .bytes_per_row_per_tile = 64,
      },
  };

  static TilingType GetTilingTypeForPixelFormat(const PixelFormatAndModifier& pixel_format) {
    switch (pixel_format.pixel_format_modifier) {
      case fuchsia_images2::PixelFormatModifier::kPvrFbcdc8X8V14:
        return TilingType::k8x8;
      case fuchsia_images2::PixelFormatModifier::kPvrFbcdc16X4V14:
        return TilingType::k16x4;
      default:
        ZX_PANIC("Not a PVRIC modifier: 0x%" PRIx64,
                 fidl::ToUnderlying(pixel_format.pixel_format_modifier));
    }
  }

  static const TilingData& GetTilingData(TilingType type) {
    static_assert(static_cast<size_t>(TilingType::k16x4) < std::size(kTilingData));
    ZX_ASSERT(static_cast<uint32_t>(type) < std::size(kTilingData));
    return kTilingData[static_cast<uint32_t>(type)];
  }

  static void GetSizeInTiles(const ImageFormat& image_format, CheckedNumeric<uint32_t>* width_out,
                             CheckedNumeric<uint32_t>* height_out) {
    ZX_ASSERT(width_out);
    ZX_ASSERT(height_out);
    *width_out = kInvalidCheckedNumeric32;
    *height_out = kInvalidCheckedNumeric32;

    auto pixel_format_and_modifier = PixelFormatAndModifierFromImageFormat(image_format);
    const auto& tiling_data = GetTilingData(GetTilingTypeForPixelFormat(pixel_format_and_modifier));
    ZX_ASSERT(image_format.bytes_per_row().has_value());
    ZX_ASSERT(image_format.size().has_value());
    CheckedNumeric<uint32_t> bytes_per_row = image_format.bytes_per_row().value();

    const auto& bytes_per_row_per_tile = tiling_data.bytes_per_row_per_tile;
    const auto& tile_rows = tiling_data.tile_rows;

    CheckedNumeric<uint32_t> min_bytes_per_row =
        CheckRoundUp(CheckedNumeric<uint64_t>(image_format.size()->width()) *
                         ImageFormatStrideBytesPerWidthPixel(pixel_format_and_modifier),
                     CheckedNumeric<uint64_t>(bytes_per_row_per_tile));
    if (!min_bytes_per_row.IsValid() ||
        image_format.bytes_per_row().value() < min_bytes_per_row.ValueOrDie()) {
      return;
    }

    *width_out = CheckRoundUp(bytes_per_row, bytes_per_row_per_tile) / bytes_per_row_per_tile;
    *height_out =
        CheckRoundUp(CheckedNumeric(image_format.size()->height()), tile_rows) / tile_rows;

    if (!width_out->IsValid()) {
      *height_out = kInvalidCheckedNumeric32;
    }
    if (!height_out->IsValid()) {
      *width_out = kInvalidCheckedNumeric32;
    }
  }

  static CheckedNumeric<uint64_t> HeaderSize(CheckedNumeric<uint32_t> width_in_tiles,
                                             CheckedNumeric<uint32_t> height_in_tiles) {
    CheckedNumeric<uint64_t> num_tiles = CheckedNumeric<uint64_t>(width_in_tiles) * height_in_tiles;
    return CheckRoundUp(num_tiles, kPvricHeaderAlignment);
  }

  static CheckedNumeric<uint64_t> DataSize(CheckedNumeric<uint32_t> width_in_tiles,
                                           CheckedNumeric<uint32_t> height_in_tiles) {
    CheckedNumeric<uint64_t> num_tiles = CheckedNumeric<uint64_t>(width_in_tiles) * height_in_tiles;
    return num_tiles * kPvricTileByteSize;
  }
};
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(33)

constexpr IntelTiledFormats kIntelFormats;
constexpr AfbcFormats kAfbcFormats;
constexpr ArmTELinearFormats kArmTELinearFormats;
constexpr GoldfishFormats kGoldfishFormats;
#if FUCHSIA_API_LEVEL_AT_LEAST(33)
constexpr PvricFormats kPvricFormats;
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(33)

constexpr const ImageFormatSet* kImageFormats[] = {
    &kLinearFormats, &kIntelFormats, &kAfbcFormats, &kArmTELinearFormats, &kGoldfishFormats,
#if FUCHSIA_API_LEVEL_AT_LEAST(33)
    &kPvricFormats,
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(33)
};

// Converts a V1 `ImageFormat2` struct to a V2 `ImageFormat` table for layout queries.
//
// `sysmem::V2CopyFromV1ImageFormat()` leaves `bytes_per_row()` unset when `v1.bytes_per_row == 0`
// because general V1-to-V2 table conversion treats `0` scalar fields as unset, whereas `size()` is
// always populated from `{coded_width, coded_height}`. Because `fuchsia.sysmem/ImageFormat_2` is a
// struct where `bytes_per_row` is always present on the wire (with `0` as default), we explicitly
// populate `bytes_per_row() = wire_image_format_v1.bytes_per_row` here so fallible V1 layout
// overloads (`*Checked` and `bool`-returning overloads) treat `0` as an undersized value and return
// invalid/`false` instead of aborting on `ZX_ASSERT(image_format.bytes_per_row().has_value())`
// (while the infallible `ImageFormatImageSize(v1)` overload still panics via `!size.IsValid()`).
fpromise::result<ImageFormat> V2ImageFormatFromV1ForLayout(
    const fuchsia_sysmem::wire::ImageFormat2& wire_image_format_v1) {
  auto image_format_v2_result =
      sysmem::V2CopyFromV1ImageFormat(fidl::ToNatural(wire_image_format_v1));
  if (!image_format_v2_result.is_ok()) {
    return fpromise::error();
  }
  ImageFormat image_format_v2 = image_format_v2_result.take_value();
  image_format_v2.bytes_per_row() = wire_image_format_v1.bytes_per_row;
  return fpromise::ok(std::move(image_format_v2));
}

}  // namespace

bool ImageFormatIsPixelFormatEqual(const PixelFormatAndModifier& a,
                                   const PixelFormatAndModifier& b) {
  if (a.pixel_format != b.pixel_format) {
    return false;
  }
  if (a.pixel_format_modifier != b.pixel_format_modifier) {
    return false;
  }
  return true;
}

bool ImageFormatIsPixelFormatEqual(const fuchsia_sysmem::wire::PixelFormat& wire_a_v1,
                                   const fuchsia_sysmem::wire::PixelFormat& wire_b_v1) {
  auto a_v1 = fidl::ToNatural(wire_a_v1);
  auto b_v1 = fidl::ToNatural(wire_b_v1);
  auto a_v2 = sysmem::V2CopyFromV1PixelFormat(a_v1);
  auto b_v2 = sysmem::V2CopyFromV1PixelFormat(b_v1);
  return ImageFormatIsPixelFormatEqual(a_v2, b_v2);
}

bool ImageFormatIsSupportedColorSpaceForPixelFormat(const fuchsia_images2::ColorSpace& color_space,
                                                    const PixelFormatAndModifier& pixel_format) {
  if (color_space == ColorSpace::kPassthrough) {
    return true;
  }
  // Ignore pixel format modifier - assume it has already been checked.
  auto color_space_sampling_info_iter = kColorSpaceSamplingInfo.find(color_space);
  if (color_space_sampling_info_iter == kColorSpaceSamplingInfo.end()) {
    return false;
  }
  auto pixel_format_sampling_info_iter = kPixelFormatSamplingInfo.find(pixel_format.pixel_format);
  if (pixel_format_sampling_info_iter == kPixelFormatSamplingInfo.end()) {
    return false;
  }
  const SamplingInfo& color_space_sampling_info = color_space_sampling_info_iter->second;
  const SamplingInfo& pixel_format_sampling_info = pixel_format_sampling_info_iter->second;
  std::vector<ColorType> color_type_intersection;
  std::set_intersection(
      color_space_sampling_info.color_types.begin(), color_space_sampling_info.color_types.end(),
      pixel_format_sampling_info.color_types.begin(), pixel_format_sampling_info.color_types.end(),
      std::back_inserter(color_type_intersection));
  if (color_type_intersection.empty()) {
    return false;
  }
  bool is_bits_per_sample_match_found = false;
  for (uint32_t bits_per_sample : color_space_sampling_info.possible_bits_per_sample) {
    auto pixel_format_bits_per_sample_iter =
        pixel_format_sampling_info.possible_bits_per_sample.find(bits_per_sample);
    if (pixel_format_bits_per_sample_iter !=
        pixel_format_sampling_info.possible_bits_per_sample.end()) {
      is_bits_per_sample_match_found = true;
      break;
    }
  }
  return is_bits_per_sample_match_found;
}

bool ImageFormatIsSupportedColorSpaceForPixelFormat(
    const fuchsia_sysmem::wire::ColorSpace& wire_color_space_v1,
    const fuchsia_sysmem::wire::PixelFormat& wire_pixel_format_v1) {
  auto color_space_v1 = fidl::ToNatural(wire_color_space_v1);
  auto pixel_format_v1 = fidl::ToNatural(wire_pixel_format_v1);
  auto color_space_v2 = sysmem::V2CopyFromV1ColorSpace(color_space_v1);
  auto pixel_format_v2 = sysmem::V2CopyFromV1PixelFormat(pixel_format_v1);
  return ImageFormatIsSupportedColorSpaceForPixelFormat(color_space_v2, pixel_format_v2);
}

bool ImageFormatIsSupported(const PixelFormatAndModifier& pixel_format) {
  return std::any_of(std::begin(kImageFormats), std::end(kImageFormats),
                     [pixel_format](const ImageFormatSet* format_set) {
                       return format_set->IsSupported(pixel_format);
                     });
}

bool ImageFormatIsSupported(const fuchsia_sysmem::wire::PixelFormat& wire_pixel_format_v1) {
  auto pixel_format_v1 = fidl::ToNatural(wire_pixel_format_v1);
  auto pixel_format_v2 = sysmem::V2CopyFromV1PixelFormat(pixel_format_v1);
  return ImageFormatIsSupported(pixel_format_v2);
}

uint32_t ImageFormatBitsPerPixel(const PixelFormatAndModifier& pixel_format) {
  ZX_ASSERT(ImageFormatIsSupported(pixel_format));
  switch (pixel_format.pixel_format) {
    case PixelFormat::kInvalid:
    case PixelFormat::kDoNotCare:
    case PixelFormat::kMjpeg:
      // impossible; checked previously.
      ZX_PANIC("Unsupported pixel format: %u",
               sysmem::fidl_underlying_cast(pixel_format.pixel_format));
    case PixelFormat::kR8G8B8A8:
    case PixelFormat::kR8G8B8X8:
      return 4u * 8u;
    case PixelFormat::kB8G8R8A8:
    case PixelFormat::kB8G8R8X8:
      return 4u * 8u;
    case PixelFormat::kB8G8R8:
    case PixelFormat::kR8G8B8:
      return 3u * 8u;
    case PixelFormat::kI420:
      return 12u;
    case PixelFormat::kM420:
      return 12u;
    case PixelFormat::kNv12:
      return 12u;
    case PixelFormat::kP010:
      // Each 10-bit sample is stored in 16 bits, with 4:2:0 subsampling: 16 * 1.5.
      return 24u;
    case PixelFormat::kYuy2:
      return 2u * 8u;
    case PixelFormat::kYv12:
      return 12u;
    case PixelFormat::kR5G6B5:
      return 16u;
    case PixelFormat::kR3G3B2:
    case PixelFormat::kR2G2B2X2:
    case PixelFormat::kL8:
    case PixelFormat::kR8:
      return 8u;
    case PixelFormat::kR8G8:
      return 16u;
    case PixelFormat::kA2B10G10R10:
    case PixelFormat::kA2R10G10B10:
      return 2u + (3u * 10u);
    default:
      ZX_PANIC("Unknown Pixel Format: %u", sysmem::fidl_underlying_cast(pixel_format.pixel_format));
  }
}

uint32_t ImageFormatBitsPerPixel(const fuchsia_sysmem::wire::PixelFormat& wire_pixel_format_v1) {
  auto pixel_format_v1 = fidl::ToNatural(wire_pixel_format_v1);
  auto pixel_format_v2 = sysmem::V2CopyFromV1PixelFormat(pixel_format_v1);
  return ImageFormatBitsPerPixel(pixel_format_v2);
}

uint32_t ImageFormatStrideBytesPerWidthPixel(const PixelFormatAndModifier& pixel_format) {
  ZX_ASSERT(ImageFormatIsSupported(pixel_format));
  // This list should match the one in garnet/public/rust/fuchsia-framebuffer/src/sysmem.rs.
  switch (pixel_format.pixel_format) {
    case PixelFormat::kInvalid:
    case PixelFormat::kDoNotCare:
    case PixelFormat::kMjpeg:
      // impossible; checked previously.
      ZX_PANIC("Unsupported pixel format: %u",
               sysmem::fidl_underlying_cast(pixel_format.pixel_format));
    case PixelFormat::kR8G8B8A8:
    case PixelFormat::kR8G8B8X8:
      return 4u;
    case PixelFormat::kB8G8R8A8:
    case PixelFormat::kB8G8R8X8:
      return 4u;
    case PixelFormat::kB8G8R8:
    case PixelFormat::kR8G8B8:
      return 3u;
    case PixelFormat::kI420:
      return 1u;
    case PixelFormat::kM420:
      return 1u;
    case PixelFormat::kNv12:
      return 1u;
    case PixelFormat::kP010:
      return 2u;
    case PixelFormat::kYuy2:
      return 2u;
    case PixelFormat::kYv12:
      return 1u;
    case PixelFormat::kR5G6B5:
      return 2u;
    case PixelFormat::kR3G3B2:
      return 1u;
    case PixelFormat::kR2G2B2X2:
      return 1u;
    case PixelFormat::kL8:
      return 1u;
    case PixelFormat::kR8:
      return 1u;
    case PixelFormat::kR8G8:
      return 2u;
    case PixelFormat::kA2B10G10R10:
      return 4u;
    case PixelFormat::kA2R10G10B10:
      return 4u;
    default:
      ZX_PANIC("Unknown Pixel Format: %u", sysmem::fidl_underlying_cast(pixel_format.pixel_format));
  }
}

uint32_t ImageFormatStrideBytesPerWidthPixel(
    const fuchsia_sysmem::wire::PixelFormat& wire_pixel_format_v1) {
  auto pixel_format_v1 = fidl::ToNatural(wire_pixel_format_v1);
  auto pixel_format_v2 = sysmem::V2CopyFromV1PixelFormat(pixel_format_v1);
  return ImageFormatStrideBytesPerWidthPixel(pixel_format_v2);
}

#if FUCHSIA_API_LEVEL_AT_LEAST(32)

safemath::CheckedNumeric<uint64_t> ImageFormatImageSizeChecked(
    const fuchsia_images2::ImageFormat& image_format) {
  ZX_ASSERT(image_format.pixel_format().has_value());
  auto pixel_format_and_modifier = PixelFormatAndModifierFromImageFormat(image_format);
  for (auto format_set : kImageFormats) {
    if (format_set->IsSupported(pixel_format_and_modifier)) {
      return format_set->ImageFormatImageSize(image_format);
    }
  }
  ZX_PANIC("Unknown Pixel Format: %u",
           sysmem::fidl_underlying_cast(image_format.pixel_format().value()));
}

safemath::CheckedNumeric<uint64_t> ImageFormatImageSizeChecked(
    const fuchsia_images2::wire::ImageFormat& image_format) {
  return ImageFormatImageSizeChecked(fidl::ToNatural(image_format));
}

safemath::CheckedNumeric<uint64_t> ImageFormatImageSizeChecked(
    const fuchsia_sysmem::wire::ImageFormat2& wire_image_format_v1) {
  auto image_format_v2_result = V2ImageFormatFromV1ForLayout(wire_image_format_v1);
  if (!image_format_v2_result.is_ok()) {
    return kInvalidCheckedNumeric64;
  }
  return ImageFormatImageSizeChecked(image_format_v2_result.value());
}

#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)

uint64_t ImageFormatImageSize(const fuchsia_images2::ImageFormat& image_format) {
  ZX_ASSERT(image_format.pixel_format().has_value());
  auto pixel_format_and_modifier = PixelFormatAndModifierFromImageFormat(image_format);
  for (auto format_set : kImageFormats) {
    if (format_set->IsSupported(pixel_format_and_modifier)) {
      CheckedNumeric<uint64_t> size = format_set->ImageFormatImageSize(image_format);
      if (!size.IsValid()) {
        // Switch to ImageFormatImageSizeChecked instead.
        ZX_PANIC("!size.IsValid()");
      }
      return size.ValueOrDie();
    }
  }
  ZX_PANIC("Unknown Pixel Format: %u",
           sysmem::fidl_underlying_cast(image_format.pixel_format().value()));
}

uint64_t ImageFormatImageSize(const fuchsia_images2::wire::ImageFormat& image_format) {
  return ImageFormatImageSize(fidl::ToNatural(image_format));
}

uint64_t ImageFormatImageSize(const fuchsia_sysmem::wire::ImageFormat2& wire_image_format_v1) {
  auto image_format_v2_result = V2ImageFormatFromV1ForLayout(wire_image_format_v1);
  if (!image_format_v2_result.is_ok()) {
    // Like an invalid size in the V2 overload; switch to ImageFormatImageSizeChecked instead.
    ZX_PANIC("V2CopyFromV1ImageFormat() failed");
  }
  return ImageFormatImageSize(image_format_v2_result.value());
}

uint32_t ImageFormatSurfaceWidthMinDivisor(const PixelFormatAndModifier& pixel_format) {
  ZX_ASSERT(ImageFormatIsSupported(pixel_format));
  switch (pixel_format.pixel_format) {
    case PixelFormat::kInvalid:
    case PixelFormat::kDoNotCare:
    case PixelFormat::kMjpeg:
      // impossible; checked previously.
      ZX_PANIC("Unsupported pixel format: %u",
               sysmem::fidl_underlying_cast(pixel_format.pixel_format));
    case PixelFormat::kR8G8B8A8:
    case PixelFormat::kR8G8B8X8:
      return 1u;
    case PixelFormat::kB8G8R8A8:
    case PixelFormat::kB8G8R8X8:
      return 1u;
    case PixelFormat::kB8G8R8:
    case PixelFormat::kR8G8B8:
      return 1u;
    case PixelFormat::kI420:
      return 2u;
    case PixelFormat::kM420:
      return 2u;
    case PixelFormat::kNv12:
      return 2u;
    case PixelFormat::kP010:
      return 2u;
    case PixelFormat::kYuy2:
      return 2u;
    case PixelFormat::kYv12:
      return 2u;
    case PixelFormat::kR5G6B5:
      return 1u;
    case PixelFormat::kR3G3B2:
      return 1u;
    case PixelFormat::kR2G2B2X2:
      return 1u;
    case PixelFormat::kL8:
      return 1u;
    case PixelFormat::kR8:
      return 1u;
    case PixelFormat::kR8G8:
      return 1u;
    case PixelFormat::kA2B10G10R10:
      return 1u;
    case PixelFormat::kA2R10G10B10:
      return 1u;
    default:
      ZX_PANIC("Unknown Pixel Format: %u", sysmem::fidl_underlying_cast(pixel_format.pixel_format));
  }
}

uint32_t ImageFormatCodedWidthMinDivisor(
    const fuchsia_sysmem::wire::PixelFormat& wire_pixel_format_v1) {
  auto pixel_format_v1 = fidl::ToNatural(wire_pixel_format_v1);
  auto pixel_format_v2 = sysmem::V2CopyFromV1PixelFormat(pixel_format_v1);
  return ImageFormatSurfaceWidthMinDivisor(pixel_format_v2);
}

uint32_t ImageFormatSurfaceHeightMinDivisor(const PixelFormatAndModifier& pixel_format) {
  ZX_ASSERT(ImageFormatIsSupported(pixel_format));
  switch (pixel_format.pixel_format) {
    case PixelFormat::kInvalid:
    case PixelFormat::kDoNotCare:
    case PixelFormat::kMjpeg:
      // impossible; checked previously.
      ZX_PANIC("Unsupported pixel format: %u",
               sysmem::fidl_underlying_cast(pixel_format.pixel_format));
    case PixelFormat::kR8G8B8A8:
    case PixelFormat::kR8G8B8X8:
      return 1u;
    case PixelFormat::kB8G8R8A8:
    case PixelFormat::kB8G8R8X8:
      return 1u;
    case PixelFormat::kB8G8R8:
    case PixelFormat::kR8G8B8:
      return 1u;
    case PixelFormat::kI420:
      return 2u;
    case PixelFormat::kM420:
      return 2u;
    case PixelFormat::kNv12:
      return 2u;
    case PixelFormat::kP010:
      return 2u;
    case PixelFormat::kYuy2:
      return 2u;
    case PixelFormat::kYv12:
      return 2u;
    case PixelFormat::kR5G6B5:
      return 1u;
    case PixelFormat::kR3G3B2:
      return 1u;
    case PixelFormat::kR2G2B2X2:
      return 1u;
    case PixelFormat::kL8:
      return 1u;
    case PixelFormat::kR8:
      return 1u;
    case PixelFormat::kR8G8:
      return 1u;
    case PixelFormat::kA2B10G10R10:
      return 1u;
    case PixelFormat::kA2R10G10B10:
      return 1u;
    default:
      ZX_PANIC("Unknown Pixel Format: %u", sysmem::fidl_underlying_cast(pixel_format.pixel_format));
  }
}

uint32_t ImageFormatCodedHeightMinDivisor(
    const fuchsia_sysmem::wire::PixelFormat& wire_pixel_format_v1) {
  auto pixel_format_v1 = fidl::ToNatural(wire_pixel_format_v1);
  auto pixel_format_v2 = sysmem::V2CopyFromV1PixelFormat(pixel_format_v1);
  return ImageFormatSurfaceHeightMinDivisor(pixel_format_v2);
}

uint32_t ImageFormatSampleAlignment(const PixelFormatAndModifier& pixel_format) {
  ZX_ASSERT(ImageFormatIsSupported(pixel_format));
  switch (pixel_format.pixel_format) {
    case PixelFormat::kInvalid:
    case PixelFormat::kDoNotCare:
    case PixelFormat::kMjpeg:
      // impossible; checked previously.
      ZX_PANIC("Unsupported pixel format: %u",
               sysmem::fidl_underlying_cast(pixel_format.pixel_format));
    case PixelFormat::kR8G8B8A8:
    case PixelFormat::kR8G8B8X8:
      return 4u;
    case PixelFormat::kB8G8R8A8:
    case PixelFormat::kB8G8R8X8:
      return 4u;
    case PixelFormat::kB8G8R8:
    case PixelFormat::kR8G8B8:
      return 1u;
    case PixelFormat::kI420:
      return 2u;
    case PixelFormat::kM420:
      return 2u;
    case PixelFormat::kNv12:
      return 2u;
    case PixelFormat::kP010:
      return 4u;
    case PixelFormat::kYuy2:
      return 2u;
    case PixelFormat::kYv12:
      return 2u;
    case PixelFormat::kR5G6B5:
      return 2u;
    case PixelFormat::kR3G3B2:
      return 1u;
    case PixelFormat::kR2G2B2X2:
      return 1u;
    case PixelFormat::kL8:
      return 1u;
    case PixelFormat::kR8:
      return 1u;
    case PixelFormat::kR8G8:
      return 2u;
    case PixelFormat::kA2B10G10R10:
      return 4u;
    case PixelFormat::kA2R10G10B10:
      return 4u;
    default:
      ZX_PANIC("Unknown Pixel Format: %u", sysmem::fidl_underlying_cast(pixel_format.pixel_format));
  }
}

uint32_t ImageFormatSampleAlignment(const fuchsia_sysmem::wire::PixelFormat& wire_pixel_format_v1) {
  auto pixel_format_v1 = fidl::ToNatural(wire_pixel_format_v1);
  auto pixel_format_v2 = sysmem::V2CopyFromV1PixelFormat(pixel_format_v1);
  return ImageFormatSampleAlignment(pixel_format_v2);
}

#if FUCHSIA_API_LEVEL_AT_LEAST(NEXT)

uint32_t ImageFormatRowBytesMinDivisor(const PixelFormatAndModifier& pixel_format) {
  for (auto* format_set : kImageFormats) {
    if (format_set->IsSupported(pixel_format)) {
      return format_set->ImageFormatRowBytesMinDivisor(pixel_format);
    }
  }
  ZX_PANIC("Unsupported Pixel Format/Modifier: %u / 0x%" PRIx64,
           sysmem::fidl_underlying_cast(pixel_format.pixel_format),
           fidl::ToUnderlying(pixel_format.pixel_format_modifier));
}

uint32_t ImageFormatRowBytesMinDivisor(const fuchsia_sysmem::wire::PixelFormat& pixel_format) {
  auto pixel_format_v1 = fidl::ToNatural(pixel_format);
  auto pixel_format_v2 = sysmem::V2CopyFromV1PixelFormat(pixel_format_v1);
  return ImageFormatRowBytesMinDivisor(pixel_format_v2);
}

#endif  // FUCHSIA_API_LEVEL_AT_LEAST(NEXT)

// This is not under FUCHSIA_API_LEVEL_AT_LEAST(32) due to being used by ImageConstraintsToFormat
// which is available in prior API levels.
safemath::CheckedNumeric<uint32_t> ImageFormatMinimumRowBytesChecked(
    const fuchsia_sysmem2::ImageFormatConstraints& constraints,
    safemath::CheckedNumeric<uint32_t> width) {
  if (!width.IsValid()) {
    return kInvalidCheckedNumeric32;
  }
  // Caller must set pixel_format.
  ZX_ASSERT(constraints.pixel_format().has_value());
  auto pixel_format_and_modifier = PixelFormatAndModifierFromConstraints(constraints);
  for (auto& format_set : kImageFormats) {
    if (format_set->IsSupported(pixel_format_and_modifier)) {
      return format_set->ImageFormatMinimumRowBytes(constraints, width);
    }
  }
  return kInvalidCheckedNumeric32;
}

#if FUCHSIA_API_LEVEL_AT_LEAST(32)

safemath::CheckedNumeric<uint32_t> ImageFormatMinimumRowBytesChecked(
    const fuchsia_sysmem2::wire::ImageFormatConstraints& wire_constraints,
    safemath::CheckedNumeric<uint32_t> width) {
  return ImageFormatMinimumRowBytesChecked(fidl::ToNatural(wire_constraints), width);
}

safemath::CheckedNumeric<uint32_t> ImageFormatMinimumRowBytesChecked(
    const fuchsia_sysmem::wire::ImageFormatConstraints& constraints,
    safemath::CheckedNumeric<uint32_t> width) {
  auto image_format_constraints_v1 = fidl::ToNatural(constraints);
  auto image_format_constraints_v2_result =
      sysmem::V2CopyFromV1ImageFormatConstraints(image_format_constraints_v1);
  if (!image_format_constraints_v2_result.is_ok()) {
    return kInvalidCheckedNumeric32;
  }
  auto image_format_constraints_v2 = image_format_constraints_v2_result.take_value();
  return ImageFormatMinimumRowBytesChecked(image_format_constraints_v2, width);
}

#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)

bool ImageFormatMinimumRowBytes(const fuchsia_sysmem2::ImageFormatConstraints& constraints,
                                uint32_t width, uint32_t* minimum_row_bytes_out) {
  ZX_ASSERT(width != 0);
  ZX_ASSERT(minimum_row_bytes_out);
  // Caller must set pixel_format.
  ZX_ASSERT(constraints.pixel_format().has_value());
  auto pixel_format_and_modifier = PixelFormatAndModifierFromConstraints(constraints);
  for (auto& format_set : kImageFormats) {
    if (format_set->IsSupported(pixel_format_and_modifier)) {
      CheckedNumeric<uint32_t> minimum_row_bytes =
          format_set->ImageFormatMinimumRowBytes(constraints, width);
      ZX_ASSERT(!minimum_row_bytes.IsValid() || minimum_row_bytes.ValueOrDie() != 0);
      if (!minimum_row_bytes.IsValid()) {
        return false;
      }
      *minimum_row_bytes_out = minimum_row_bytes.ValueOrDie();
      return true;
    }
  }
  return false;
}

bool ImageFormatMinimumRowBytes(
    const fuchsia_sysmem2::wire::ImageFormatConstraints& wire_constraints, uint32_t width,
    uint32_t* minimum_row_bytes_out) {
  return ImageFormatMinimumRowBytes(fidl::ToNatural(wire_constraints), width,
                                    minimum_row_bytes_out);
}

bool ImageFormatMinimumRowBytes(
    const fuchsia_sysmem::wire::ImageFormatConstraints& wire_image_format_constraints_v1,
    uint32_t width, uint32_t* minimum_row_bytes_out) {
  auto image_format_constraints_v1 = fidl::ToNatural(wire_image_format_constraints_v1);
  auto image_format_constraints_v2_result =
      sysmem::V2CopyFromV1ImageFormatConstraints(image_format_constraints_v1);
  if (!image_format_constraints_v2_result.is_ok()) {
    return false;
  }
  auto image_format_constraints_v2 = image_format_constraints_v2_result.take_value();
  return ImageFormatMinimumRowBytes(image_format_constraints_v2, width, minimum_row_bytes_out);
}

fpromise::result<fuchsia_images2::wire::PixelFormat> ImageFormatConvertZbiToSysmemPixelFormat_v2(
    zbi_pixel_format_t zx_pixel_format) {
  switch (zx_pixel_format) {
    case ZBI_PIXEL_FORMAT_RGB_565:
      return fpromise::ok(PixelFormat::kR5G6B5);
    case ZBI_PIXEL_FORMAT_RGB_332:
      return fpromise::ok(PixelFormat::kR3G3B2);
    case ZBI_PIXEL_FORMAT_RGB_2220:
      return fpromise::ok(PixelFormat::kR2G2B2X2);
    case ZBI_PIXEL_FORMAT_ARGB_8888:
      // Switching to using alpha.
    case ZBI_PIXEL_FORMAT_RGB_X888:
      // TODO(b/329142833): Change this to B8G8R8X8 when relevant participants support R8G8B8X8. The
      // main benefit is ensuring that consumer participants reliably ignore the X/A byte, treating
      // the RGB portion as fully opaque regardless of the contents of the X/A byte. Currently this
      // happens to work out ok, but really we should ensure that participants agree re. X vs. A.
      return fpromise::ok(PixelFormat::kB8G8R8A8);
    case ZBI_PIXEL_FORMAT_MONO_8:
      return fpromise::ok(PixelFormat::kL8);
    case ZBI_PIXEL_FORMAT_I420:
      return fpromise::ok(PixelFormat::kI420);
    case ZBI_PIXEL_FORMAT_NV12:
      return fpromise::ok(PixelFormat::kNv12);
    case ZBI_PIXEL_FORMAT_RGB_888:
      return fpromise::ok(PixelFormat::kB8G8R8);
    case ZBI_PIXEL_FORMAT_ABGR_8888:
      // Switching to using alpha.
    case ZBI_PIXEL_FORMAT_BGR_888_X:
      return fpromise::ok(PixelFormat::kR8G8B8A8);
    case ZBI_PIXEL_FORMAT_ARGB_2_10_10_10:
      return fpromise::ok(PixelFormat::kA2R10G10B10);
    case ZBI_PIXEL_FORMAT_ABGR_2_10_10_10:
      return fpromise::ok(PixelFormat::kA2B10G10R10);
    default:
      return fpromise::error();
  }
}

fpromise::result<ImageFormat> ImageConstraintsToFormat(const ImageFormatConstraints& constraints,
                                                       uint32_t width, uint32_t height) {
  if ((constraints.min_size().has_value() && width < constraints.min_size()->width()) ||
      (constraints.max_size().has_value() && width > constraints.max_size()->width())) {
    return fpromise::error();
  }
  if ((constraints.min_size().has_value() && height < constraints.min_size()->height()) ||
      (constraints.max_size().has_value() && height > constraints.max_size()->height())) {
    return fpromise::error();
  }
  if (constraints.size_alignment().has_value()) {
    if (constraints.size_alignment()->width() == 0 || constraints.size_alignment()->height() == 0) {
      return fpromise::error();
    }
    if ((width % constraints.size_alignment()->width()) != 0) {
      return fpromise::error();
    }
    if ((height % constraints.size_alignment()->height()) != 0) {
      return fpromise::error();
    }
  }
  ImageFormat result;
  CheckedNumeric<uint32_t> minimum_row_bytes =
      ImageFormatMinimumRowBytesChecked(constraints, width);
  // A width of 0 can have a valid minimum row bytes of 0, but isn't a valid image.
  if (!minimum_row_bytes.IsValid() || minimum_row_bytes.ValueOrDie() == 0) {
    return fpromise::error();
  }
  result.bytes_per_row() = minimum_row_bytes.ValueOrDie();
  // An unset pixel_format_modifier means LINEAR, as in ImageFormatMinimumRowBytesChecked() above.
  auto pixel_format_and_modifier = PixelFormatAndModifierFromConstraints(constraints);
  result.pixel_format() = pixel_format_and_modifier.pixel_format;
  result.pixel_format_modifier() = pixel_format_and_modifier.pixel_format_modifier;
  result.size() = {width, height};
  // We intentionally default to x, y == 0, 0 here, since that's by far the most common.  The caller
  // can fix this up.
  result.display_rect() = {0, 0, width, height};
  if (constraints.color_spaces().has_value() && !constraints.color_spaces()->empty()) {
    result.color_space() = constraints.color_spaces()->at(0);
  }
  // result's pixel_aspect_ratio field remains un-set which is equivalent to 1,1
  return fpromise::ok(std::move(result));
}

fpromise::result<ImageFormatWire> ImageConstraintsToFormat(
    fidl::AnyArena& allocator, const ImageFormatConstraintsWire& wire_constraints, uint32_t width,
    uint32_t height) {
  auto constraints = fidl::ToNatural(wire_constraints);
  auto result = ImageConstraintsToFormat(constraints, width, height);
  if (!result.is_ok()) {
    return fpromise::error();
  }
  return fpromise::ok(fidl::ToWire(allocator, result.take_value()));
}

fpromise::result<fuchsia_sysmem::wire::ImageFormat2> ImageConstraintsToFormat(
    const fuchsia_sysmem::wire::ImageFormatConstraints& wire_image_format_constraints_v1,
    uint32_t width, uint32_t height) {
  auto image_format_constraints_v1 = fidl::ToNatural(wire_image_format_constraints_v1);
  auto image_format_constraints_v2_result =
      sysmem::V2CopyFromV1ImageFormatConstraints(image_format_constraints_v1);
  if (!image_format_constraints_v2_result.is_ok()) {
    return fpromise::error();
  }
  auto image_format_constraints_v2 = image_format_constraints_v2_result.take_value();
  auto v2_out_result = ImageConstraintsToFormat(image_format_constraints_v2, width, height);
  if (!v2_out_result.is_ok()) {
    return fpromise::error();
  }
  auto v2_out = v2_out_result.take_value();
  auto v1_out_result = sysmem::V1CopyFromV2ImageFormat(v2_out);
  if (!v1_out_result) {
    return fpromise::error();
  }
  // This arena isn't relied upon by the returned value, because kMaxOutOfLine == 0.
  fidl::Arena arena;
  ZX_ASSERT(fidl::TypeTraits<fuchsia_sysmem::wire::ImageFormat2>::kMaxOutOfLine == 0);
  auto wire_v1 = fidl::ToWire(arena, v1_out_result.take_value());
  return fpromise::ok(wire_v1);
}

#if FUCHSIA_API_LEVEL_AT_LEAST(32)

safemath::CheckedNumeric<uint64_t> ImageFormatPlaneByteOffsetChecked(
    const fuchsia_images2::ImageFormat& image_format, uint32_t plane) {
  auto pixel_format_and_modifier = PixelFormatAndModifierFromImageFormat(image_format);
  for (auto& format_set : kImageFormats) {
    if (format_set->IsSupported(pixel_format_and_modifier)) {
      return format_set->ImageFormatPlaneByteOffset(image_format, plane);
    }
  }
  return kInvalidCheckedNumeric64;
}

safemath::CheckedNumeric<uint64_t> ImageFormatPlaneByteOffsetChecked(
    const fuchsia_images2::wire::ImageFormat& image_format, uint32_t plane) {
  return ImageFormatPlaneByteOffsetChecked(fidl::ToNatural(image_format), plane);
}

safemath::CheckedNumeric<uint64_t> ImageFormatPlaneByteOffsetChecked(
    const fuchsia_sysmem::wire::ImageFormat2& image_format, uint32_t plane) {
  auto image_format_v2_result = V2ImageFormatFromV1ForLayout(image_format);
  if (!image_format_v2_result.is_ok()) {
    return kInvalidCheckedNumeric64;
  }
  return ImageFormatPlaneByteOffsetChecked(image_format_v2_result.value(), plane);
}

#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)

bool ImageFormatPlaneByteOffset(const ImageFormat& image_format, uint32_t plane,
                                uint64_t* offset_out) {
  ZX_ASSERT(offset_out);
  auto pixel_format_and_modifier = PixelFormatAndModifierFromImageFormat(image_format);
  for (auto& format_set : kImageFormats) {
    if (format_set->IsSupported(pixel_format_and_modifier)) {
      safemath::CheckedNumeric<uint64_t> plane_offset =
          format_set->ImageFormatPlaneByteOffset(image_format, plane);
      if (!plane_offset.IsValid()) {
        return false;
      }
      *offset_out = plane_offset.ValueOrDie();
      return true;
    }
  }
  return false;
}

bool ImageFormatPlaneByteOffset(const ImageFormatWire& image_format, uint32_t plane,
                                uint64_t* offset_out) {
  return ImageFormatPlaneByteOffset(fidl::ToNatural(image_format), plane, offset_out);
}

bool ImageFormatPlaneByteOffset(const fuchsia_sysmem::wire::ImageFormat2& wire_image_format_v1,
                                uint32_t plane, uint64_t* offset_out) {
  ZX_ASSERT(offset_out);
  auto image_format_v2_result = V2ImageFormatFromV1ForLayout(wire_image_format_v1);
  if (!image_format_v2_result.is_ok()) {
    return false;
  }
  return ImageFormatPlaneByteOffset(image_format_v2_result.value(), plane, offset_out);
}

#if FUCHSIA_API_LEVEL_AT_LEAST(32)

safemath::CheckedNumeric<uint32_t> ImageFormatPlaneRowBytesChecked(
    const fuchsia_images2::ImageFormat& image_format, uint32_t plane) {
  auto pixel_format_and_modifier = PixelFormatAndModifierFromImageFormat(image_format);
  for (auto& format_set : kImageFormats) {
    if (format_set->IsSupported(pixel_format_and_modifier)) {
      return format_set->ImageFormatPlaneRowBytes(image_format, plane);
    }
  }
  return kInvalidCheckedNumeric32;
}

safemath::CheckedNumeric<uint32_t> ImageFormatPlaneRowBytesChecked(
    const fuchsia_images2::wire::ImageFormat& image_format, uint32_t plane) {
  return ImageFormatPlaneRowBytesChecked(fidl::ToNatural(image_format), plane);
}

safemath::CheckedNumeric<uint32_t> ImageFormatPlaneRowBytesChecked(
    const fuchsia_sysmem::wire::ImageFormat2& image_format, uint32_t plane) {
  auto image_format_v2_result = V2ImageFormatFromV1ForLayout(image_format);
  if (!image_format_v2_result.is_ok()) {
    return kInvalidCheckedNumeric32;
  }
  return ImageFormatPlaneRowBytesChecked(image_format_v2_result.value(), plane);
}

#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)

bool ImageFormatPlaneRowBytes(const ImageFormat& image_format, uint32_t plane,
                              uint32_t* row_bytes_out) {
  ZX_ASSERT(row_bytes_out);
  auto pixel_format_and_modifier = PixelFormatAndModifierFromImageFormat(image_format);
  for (auto& format_set : kImageFormats) {
    if (format_set->IsSupported(pixel_format_and_modifier)) {
      safemath::CheckedNumeric<uint32_t> plane_row_bytes =
          format_set->ImageFormatPlaneRowBytes(image_format, plane);
      if (!plane_row_bytes.IsValid()) {
        return false;
      }
      *row_bytes_out = plane_row_bytes.ValueOrDie();
      return true;
    }
  }
  return false;
}

bool ImageFormatPlaneRowBytes(const ImageFormatWire& wire_image_format, uint32_t plane,
                              uint32_t* row_bytes_out) {
  return ImageFormatPlaneRowBytes(fidl::ToNatural(wire_image_format), plane, row_bytes_out);
}

bool ImageFormatPlaneRowBytes(const fuchsia_sysmem::wire::ImageFormat2& wire_image_format_v1,
                              uint32_t plane, uint32_t* row_bytes_out) {
  ZX_ASSERT(row_bytes_out);
  auto image_format_v2_result = V2ImageFormatFromV1ForLayout(wire_image_format_v1);
  if (!image_format_v2_result.is_ok()) {
    return false;
  }
  return ImageFormatPlaneRowBytes(image_format_v2_result.value(), plane, row_bytes_out);
}

bool ImageFormatCompatibleWithProtectedMemory(const PixelFormatAndModifier& pixel_format) {
  // AKA kFormatModifierLinear
  if (pixel_format.pixel_format_modifier == fuchsia_images2::PixelFormatModifier::kLinear) {
    return true;
  }
  constexpr uint64_t kArmLinearFormat = 0x0800000000000000ul;
  switch (fidl::ToUnderlying(pixel_format.pixel_format_modifier) &
          ~AfbcFormats::kAfbcModifierMask) {
    case kArmLinearFormat:
    case fidl::ToUnderlying(fuchsia_images2::PixelFormatModifier::kArmAfbc16X16):
    case fidl::ToUnderlying(fuchsia_images2::PixelFormatModifier::kArmAfbc32X8):
      // TE formats occasionally need CPU writes to the TE buffer.
      return !(fidl::ToUnderlying(pixel_format.pixel_format_modifier) &
               fuchsia_images2::kFormatModifierArmTeBit);

    default:
      return true;
  }
}

bool ImageFormatCompatibleWithProtectedMemory(
    const fuchsia_sysmem::wire::PixelFormat& wire_pixel_format_v1) {
  auto pixel_format_v1 = fidl::ToNatural(wire_pixel_format_v1);
  auto pixel_format_v2 = sysmem::V2CopyFromV1PixelFormat(pixel_format_v1);
  return ImageFormatCompatibleWithProtectedMemory(pixel_format_v2);
}

#if FUCHSIA_API_LEVEL_AT_LEAST(30)
bool ImageFormatIsNonTiledSinglePlane(const PixelFormatAndModifier& pixel_format_and_modifier) {
  for (auto& format_set : kImageFormats) {
    if (format_set->IsSupported(pixel_format_and_modifier)) {
      return format_set->ImageFormatIsNonTiledSinglePlane(pixel_format_and_modifier);
    }
  }
  return false;
}
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(30)
