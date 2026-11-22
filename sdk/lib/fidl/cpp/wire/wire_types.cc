// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/fidl/cpp/wire/coding_errors.h>
#include <lib/fidl/cpp/wire/status.h>
#include <lib/fidl/cpp/wire/wire_coding_traits.h>
#include <lib/fidl/cpp/wire/wire_types.h>
#include <lib/fidl/cpp/wire_format_metadata.h>
#include <lib/fidl/internal.h>
#include <zircon/assert.h>
#include <zircon/compiler.h>
#include <zircon/fidl.h>
#include <zircon/types.h>

#include <algorithm>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <limits>
#include <tuple>

namespace fidl::internal {

std::vector<uint8_t> ConcatMetadataAndMessage(fidl::WireFormatMetadata metadata,
                                              fidl::OutgoingMessage& message) {
  std::vector<uint8_t> result;
  result.reserve(sizeof(fidl_opaque_wire_format_metadata_t) + message.CountBytes());
  fidl_opaque_wire_format_metadata_t opaque = metadata.ToOpaque();
  std::array<uint8_t, 8> metadata_bytes;
  static_assert(sizeof(opaque.metadata) == sizeof(metadata_bytes));
  memcpy(metadata_bytes.data(), &opaque.metadata, sizeof(opaque.metadata));
  // Copy the wire format metadata.
  result.insert(result.end(), metadata_bytes.begin(), metadata_bytes.end());
  // Copy the encoded bytes.
  auto bytes = message.CopyBytes();
  result.insert(result.end(), bytes.data(), bytes.data() + bytes.size());
  return result;
}

fit::result<fidl::Error, cpp20::span<uint8_t>> PersistToBuffer(size_t inline_size,
                                                               TopLevelEncodeFn encode_fn,
                                                               void* value,
                                                               cpp20::span<uint8_t> buffer) {
  if (unlikely(!FidlIsAligned(buffer.data()))) {
    return fit::error(
        fidl::Status::EncodeError(ZX_ERR_INVALID_ARGS, "Bytes must be aligned to FIDL_ALIGNMENT"));
  }
  constexpr size_t kMetadataSize = sizeof(fidl_opaque_wire_format_metadata_t);
  const uint64_t min_size = kMetadataSize + FIDL_ALIGN(inline_size);
  if (unlikely(buffer.size() < min_size)) {
    return fit::error(
        fidl::Status::EncodeError(ZX_ERR_BUFFER_TOO_SMALL, kCodingErrorBackingBufferSizeExceeded));
  }

  const size_t max_payload =
      std::min<size_t>(buffer.size() - kMetadataSize, std::numeric_limits<uint32_t>::max());
  zx_channel_iovec_t iovec = {};
  fit::result<fidl::Error, WireEncoder::Result> encoded =
      WireEncode(inline_size, encode_fn, &kNullCodingConfig, value, &iovec, /*iovec_capacity=*/1u,
                 /*handles=*/nullptr, /*handle_metadata=*/nullptr, /*handle_capacity=*/0u,
                 buffer.data() + kMetadataSize, max_payload);
  if (unlikely(encoded.is_error())) {
    return encoded.take_error();
  }

  const fidl_opaque_wire_format_metadata_t opaque =
      WireFormatMetadataForVersion(kLLCPPWireFormatVersion).ToOpaque();
  static_assert(sizeof(opaque.metadata) == kMetadataSize);
  std::memcpy(buffer.data(), &opaque.metadata, kMetadataSize);

  ZX_DEBUG_ASSERT(encoded->iovec_actual == 1u);
  ZX_DEBUG_ASSERT(iovec.buffer == buffer.data() + kMetadataSize);
  return fit::ok(buffer.subspan(0, kMetadataSize + iovec.capacity));
}

fit::result<fidl::Error, std::tuple<fidl::WireFormatMetadata, cpp20::span<uint8_t>>>
SplitMetadataAndMessage(cpp20::span<uint8_t> persisted) {
  if (persisted.size() < sizeof(fidl_opaque_wire_format_metadata_t)) {
    return fit::error(fidl::Status::DecodeError(ZX_ERR_BUFFER_TOO_SMALL, kCodingErrorDataTooShort));
  }
  cpp20::span<uint8_t> metadata_span =
      persisted.subspan(0, sizeof(fidl_opaque_wire_format_metadata_t));
  cpp20::span<uint8_t> payload_span = persisted.subspan(sizeof(fidl_opaque_wire_format_metadata_t));
  fidl_opaque_wire_format_metadata opaque;
  memcpy(&opaque.metadata, metadata_span.data(), metadata_span.size());
  return fit::ok(std::make_tuple(fidl::WireFormatMetadata::FromOpaque(opaque), payload_span));
}

}  // namespace fidl::internal
