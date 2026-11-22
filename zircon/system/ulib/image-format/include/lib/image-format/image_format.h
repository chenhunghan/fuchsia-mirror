// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_IMAGE_FORMAT_IMAGE_FORMAT_H_
#define LIB_IMAGE_FORMAT_IMAGE_FORMAT_H_

#include <fidl/fuchsia.images2/cpp/fidl.h>
#include <fidl/fuchsia.sysmem/cpp/fidl.h>
#include <fidl/fuchsia.sysmem2/cpp/fidl.h>
#if defined(FIDL_ALLOW_DEPRECATED_C_BINDINGS)
#include <fuchsia/sysmem/c/fidl.h>
#endif
#include <lib/fpromise/result.h>
#include <lib/sysmem-version/sysmem-version.h>
#include <zircon/availability.h>

#if FUCHSIA_API_LEVEL_AT_LEAST(32)
#include <safemath/safe_math.h>
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)

// Forward-declared; defined in <lib/zbi-format/graphics.h>.
using zbi_pixel_format_t = uint32_t;

// Iff this returns true, the two pixel formats are equal.
bool ImageFormatIsPixelFormatEqual(const PixelFormatAndModifier& a,
                                   const PixelFormatAndModifier& b);
bool ImageFormatIsPixelFormatEqual(const fuchsia_sysmem::wire::PixelFormat& a,
                                   const fuchsia_sysmem::wire::PixelFormat& b);

// true - The color_space is potentially compatible with the PixelFormat,
// assuming the correct variant of the ColorSpace is used (with correct bpp).
//
// false - The pixel_format bpp is not supported by the given ColorSpace.  For
// example BT.2100 specifies 10 or 12 bpp, while NV12 specifies 8 bpp, so NV12
// is not compatible with BT.2100.  Or, the system does not support the
// combination of ColorSpace and PixelFormat (even if they are hypothetically
// compatible; in this case support might be added later).
//
// The first parameter can also accept fuchsia_images2::wire::ColorSpace since
// the type is the same.
bool ImageFormatIsSupportedColorSpaceForPixelFormat(const fuchsia_images2::ColorSpace& color_space,
                                                    const PixelFormatAndModifier& pixel_format);
bool ImageFormatIsSupportedColorSpaceForPixelFormat(
    const fuchsia_sysmem::wire::ColorSpace& color_space,
    const fuchsia_sysmem::wire::PixelFormat& pixel_format);

// If this returns true, the remainder of the functions in this header can be
// called with pixel_format.  If this returns false:
//  * Per-pixel-format property functions (ImageFormatBitsPerPixel,
//    ImageFormatStrideBytesPerWidthPixel, ImageFormat*WidthMinDivisor,
//    ImageFormat*HeightMinDivisor, ImageFormatSampleAlignment,
//    ImageFormatRowBytesMinDivisor) and ImageFormatImageSize* abort().
//  * ImageFormatMinimumRowBytes*, ImageConstraintsToFormat,
//    ImageFormatPlaneByteOffset*, and ImageFormatPlaneRowBytes* return invalid,
//    false, or error (provided `pixel_format` is set), and
//    ImageFormatIsNonTiledSinglePlane returns false.
bool ImageFormatIsSupported(const PixelFormatAndModifier& pixel_format);
bool ImageFormatIsSupported(const fuchsia_sysmem::wire::PixelFormat& pixel_format);

// Returns the number of bits per pixel for the given PixelFormat.  This is the
// bits per pixel (RGB pixel or Y pixel) in the overall image across all bytes
// that contain pixel data.
//
// For example, NV12 is 12 bits per pixel.  This accounts for sub-sampling in
// both horizontal and vertical.  This counts all the bits of the bytes that
// contain samples, including padding bits, so P010 (10-bit samples stored in
// 16 bits each) is 24 bits per pixel.
uint32_t ImageFormatBitsPerPixel(const PixelFormatAndModifier& pixel_format);
uint32_t ImageFormatBitsPerPixel(const fuchsia_sysmem::wire::PixelFormat& pixel_format);

// This is the number of stride bytes per pixel of width (RGB pixel width or Y
// pixel width) of plane 0.  For formats such as NV12 that separate Y and UV
// data, this is the number of stride bytes per pixel of width of the Y plane
// (plane 0).  NV12 has the same stride for the UV data.  For tiled or
// block-compressed formats, this is the number of bytes per pixel of width in a
// linearized plane 0 row (for example, 4 for R8G8B8A8 or 1 for NV12), not the
// whole row's byte stride (see ImageFormatMinimumRowBytes and
// ImageFormatRowBytesMinDivisor for whole-row byte stride and tile/block
// alignment).  This function doesn't return stride information for planes
// beyond plane 0.
uint32_t ImageFormatStrideBytesPerWidthPixel(const PixelFormatAndModifier& pixel_format);
uint32_t ImageFormatStrideBytesPerWidthPixel(const fuchsia_sysmem::wire::PixelFormat& pixel_format);

// This would be height * stride, if it weren't for formats like NV12, where it
// isn't.  The return value is in bytes.
//
// Requires `image_format.pixel_format`, `image_format.size`, and
// `image_format.bytes_per_row` to be set (for all supported formats, including
// AFBC and ARM linear TE; an unset `pixel_format_modifier` defaults to LINEAR),
// and aborts if the format/modifier is not supported (or, for the non-Checked
// overload, if the result is not valid).  Fails if `bytes_per_row` is smaller
// than the format's minimum row bytes for `size.width`.
#if FUCHSIA_API_LEVEL_AT_LEAST(32)
safemath::CheckedNumeric<uint64_t> ImageFormatImageSizeChecked(
    const fuchsia_images2::ImageFormat& image_format);
safemath::CheckedNumeric<uint64_t> ImageFormatImageSizeChecked(
    const fuchsia_images2::wire::ImageFormat& image_format);
safemath::CheckedNumeric<uint64_t> ImageFormatImageSizeChecked(
    const fuchsia_sysmem::wire::ImageFormat2& image_format);
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)
uint64_t ImageFormatImageSize(const fuchsia_images2::ImageFormat& image_format);
uint64_t ImageFormatImageSize(const fuchsia_images2::wire::ImageFormat& image_format);
uint64_t ImageFormatImageSize(const fuchsia_sysmem::wire::ImageFormat2& image_format);

uint32_t ImageFormatSurfaceWidthMinDivisor(const PixelFormatAndModifier& pixel_format);
uint32_t ImageFormatCodedWidthMinDivisor(const fuchsia_sysmem::wire::PixelFormat& pixel_format);

uint32_t ImageFormatSurfaceHeightMinDivisor(const PixelFormatAndModifier& pixel_format);
uint32_t ImageFormatCodedHeightMinDivisor(const fuchsia_sysmem::wire::PixelFormat& pixel_format);

// The start of image data must be at least this aligned.
//
// The plane 0 stride is required to be divisible by this alignment.  Generally
// the stride of planes beyond plane 0 (if any) will have a known fixed
// relationship with the plane 0 stride.
uint32_t ImageFormatSampleAlignment(const PixelFormatAndModifier& pixel_format);
uint32_t ImageFormatSampleAlignment(const fuchsia_sysmem::wire::PixelFormat& pixel_format);

#if FUCHSIA_API_LEVEL_AT_LEAST(NEXT)
// Gets the minimum divisor that ImageFormat.bytes_per_row must be a multiple of
// for a supported pixel_format and modifier (for example, the tile or block
// width in bytes for tiled formats, or 1 for linear formats).
//
// Note that sysmem also requires bytes_per_row to be a multiple of
// ImageFormatSampleAlignment(pixel_format) (and ImageFormatStrideBytesPerWidthPixel
// when require_bytes_per_row_at_pixel_boundary is true).
uint32_t ImageFormatRowBytesMinDivisor(const PixelFormatAndModifier& pixel_format);
uint32_t ImageFormatRowBytesMinDivisor(const fuchsia_sysmem::wire::PixelFormat& pixel_format);
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(NEXT)

// Gets the minimum number of bytes per row possible for an image with a
// specific width and specific constraints.
//
// Requires `constraints.pixel_format` to be set (an unset
// `pixel_format_modifier` defaults to LINEAR). Returns an invalid
// CheckedNumeric if the format/modifier is not supported, if `width` is invalid
// or outside `constraints` (after `size_alignment.width` rounding), or if row
// byte calculation overflows uint32_t.
#if FUCHSIA_API_LEVEL_AT_LEAST(32)
safemath::CheckedNumeric<uint32_t> ImageFormatMinimumRowBytesChecked(
    const fuchsia_sysmem2::ImageFormatConstraints& constraints,
    safemath::CheckedNumeric<uint32_t> width);
safemath::CheckedNumeric<uint32_t> ImageFormatMinimumRowBytesChecked(
    const fuchsia_sysmem2::wire::ImageFormatConstraints& constraints,
    safemath::CheckedNumeric<uint32_t> width);
safemath::CheckedNumeric<uint32_t> ImageFormatMinimumRowBytesChecked(
    const fuchsia_sysmem::wire::ImageFormatConstraints& constraints,
    safemath::CheckedNumeric<uint32_t> width);
#endif
// Non-checked is deprecated; switch to ImageFormatMinimumRowBytesChecked above.
// Requires `width != 0` and `minimum_row_bytes_out != nullptr`; returns false
// if the format/modifier is not supported or `width` is not valid for
// `constraints`.
bool ImageFormatMinimumRowBytes(const fuchsia_sysmem2::ImageFormatConstraints& constraints,
                                uint32_t width, uint32_t* minimum_row_bytes_out);
bool ImageFormatMinimumRowBytes(const fuchsia_sysmem2::wire::ImageFormatConstraints& constraints,
                                uint32_t width, uint32_t* minimum_row_bytes_out);
bool ImageFormatMinimumRowBytes(const fuchsia_sysmem::wire::ImageFormatConstraints& constraints,
                                uint32_t width, uint32_t* minimum_row_bytes_out);

fpromise::result<fuchsia_images2::wire::PixelFormat> ImageFormatConvertZbiToSysmemPixelFormat_v2(
    zbi_pixel_format_t zbi_pixel_format);

// Requires `constraints.pixel_format` to be set (an unset
// `pixel_format_modifier` defaults to LINEAR). Returns error if the
// format/modifier is not supported or if `width` or `height` (which must be
// non-zero) does not satisfy `constraints`.
// Though these are used within the .cc file, they do not need to be exposed for stable API levels.
fpromise::result<fuchsia_images2::ImageFormat> ImageConstraintsToFormat(
    const fuchsia_sysmem2::ImageFormatConstraints& constraints, uint32_t width, uint32_t height);
fpromise::result<fuchsia_images2::wire::ImageFormat> ImageConstraintsToFormat(
    fidl::AnyArena& allocator, const fuchsia_sysmem2::wire::ImageFormatConstraints& constraints,
    uint32_t width, uint32_t height);
fpromise::result<fuchsia_sysmem::wire::ImageFormat2> ImageConstraintsToFormat(
    const fuchsia_sysmem::wire::ImageFormatConstraints& constraints, uint32_t width,
    uint32_t height);

// Gets the byte offset of `plane` within the image buffer, or invalid / false
// if the format/modifier is not supported or `plane` is not present.
//
// Requires `image_format.pixel_format` to be set (an unset
// `pixel_format_modifier` defaults to LINEAR), and `offset_out != nullptr` for
// the bool overloads. Planes whose offset depends on prior plane sizes (such as
// YUV planes 1..2, PVRIC plane 0, and plane 3 for Intel CCS, AFBC TE, and ARM
// linear TE) also require `image_format.size` and `image_format.bytes_per_row`
// to be set, and fail if `bytes_per_row` is smaller than the format's minimum
// row bytes for `size.width`.
#if FUCHSIA_API_LEVEL_AT_LEAST(32)
safemath::CheckedNumeric<uint64_t> ImageFormatPlaneByteOffsetChecked(
    const fuchsia_images2::ImageFormat& image_format, uint32_t plane);
safemath::CheckedNumeric<uint64_t> ImageFormatPlaneByteOffsetChecked(
    const fuchsia_images2::wire::ImageFormat& image_format, uint32_t plane);
safemath::CheckedNumeric<uint64_t> ImageFormatPlaneByteOffsetChecked(
    const fuchsia_sysmem::wire::ImageFormat2& image_format, uint32_t plane);
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)
bool ImageFormatPlaneByteOffset(const fuchsia_images2::ImageFormat& image_format, uint32_t plane,
                                uint64_t* offset_out);
bool ImageFormatPlaneByteOffset(const fuchsia_images2::wire::ImageFormat& image_format,
                                uint32_t plane, uint64_t* offset_out);
bool ImageFormatPlaneByteOffset(const fuchsia_sysmem::wire::ImageFormat2& image_format,
                                uint32_t plane, uint64_t* offset_out);

// Gets the byte stride of a single row in `plane`, or invalid / false if the
// format/modifier is not supported or `plane` is not present.
//
// For color planes (plane 0, and YUV planes 1..2), this returns the row byte
// stride (for tiled or block-compressed formats, the tile/block-aligned
// linearized row byte width).
//
// For auxiliary or header plane 3, this returns the byte stride of one
// horizontal memory row of the auxiliary plane's own structure (Intel CCS: one
// internal row of the Y-tiled CCS surface; PVRIC header: one row of 1-byte
// tile headers; ARM transaction elimination: one 64-byte-aligned row of 8-byte
// CRC blocks).
//
// Requires `image_format.pixel_format` to be set (an unset
// `pixel_format_modifier` defaults to LINEAR), and `row_bytes_out != nullptr`
// for the bool overloads. Valid planes also require `image_format.size` and
// `image_format.bytes_per_row` to be set (including AFBC planes 0 and 3 and ARM
// linear TE plane 3), and fail if `bytes_per_row` is smaller than the format's
// minimum row bytes for `size.width`.
#if FUCHSIA_API_LEVEL_AT_LEAST(32)
safemath::CheckedNumeric<uint32_t> ImageFormatPlaneRowBytesChecked(
    const fuchsia_images2::ImageFormat& image_format, uint32_t plane);
safemath::CheckedNumeric<uint32_t> ImageFormatPlaneRowBytesChecked(
    const fuchsia_images2::wire::ImageFormat& image_format, uint32_t plane);
safemath::CheckedNumeric<uint32_t> ImageFormatPlaneRowBytesChecked(
    const fuchsia_sysmem::wire::ImageFormat2& image_format, uint32_t plane);
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(32)
bool ImageFormatPlaneRowBytes(const fuchsia_images2::ImageFormat& image_format, uint32_t plane,
                              uint32_t* row_bytes_out);
bool ImageFormatPlaneRowBytes(const fuchsia_images2::wire::ImageFormat& image_format,
                              uint32_t plane, uint32_t* row_bytes_out);
bool ImageFormatPlaneRowBytes(const fuchsia_sysmem::wire::ImageFormat2& image_format,
                              uint32_t plane, uint32_t* row_bytes_out);

bool ImageFormatCompatibleWithProtectedMemory(const PixelFormatAndModifier& pixel_format);
bool ImageFormatCompatibleWithProtectedMemory(
    const fuchsia_sysmem::wire::PixelFormat& pixel_format);

#if FUCHSIA_API_LEVEL_AT_LEAST(30)
// This intentionally does not define how planes are indexed or how they're counted, only whether a
// format is both non-tiled and has a single plane with uniform rows. Data like TE or CCS or the
// like imply not being single-plane, as do multi-plane YCbCr formats, and row-interleaved YCbCr
// formats like M420 do not count despite exposing only plane 0. MJPEG doesn't count as non-tiled
// and single-plane.
bool ImageFormatIsNonTiledSinglePlane(const PixelFormatAndModifier& pixel_format_and_modifier);
#endif

#endif  // LIB_IMAGE_FORMAT_IMAGE_FORMAT_H_
