// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/fidl/cpp/natural_coding_traits.h>
#include <lib/fidl/cpp/wire/incoming_message.h>
#include <lib/fidl/cpp/wire/wire_coding_traits.h>

namespace fidl::internal {

template <typename IntType>
void NaturalEncodeNonZero(NaturalEncoder* encoder, const IntType* value, size_t offset) {
  if (unlikely(*value == 0)) {
    encoder->SetError(kCodingErrorInvalidNonZero);
    return;
  }
  *encoder->template GetPtr<IntType>(offset) = *value;
}

template <typename IntType>
void NaturalDecodeNonZero(NaturalDecoder* decoder, IntType* value, size_t offset) {
  IntType val = *decoder->template GetPtr<IntType>(offset);
  if (unlikely(val == 0)) {
    decoder->SetError(kCodingErrorInvalidNonZero);
    return;
  }
  *value = val;
}

template void NaturalEncodeNonZero<uint8_t>(NaturalEncoder*, const uint8_t*, size_t);
template void NaturalEncodeNonZero<uint16_t>(NaturalEncoder*, const uint16_t*, size_t);
template void NaturalEncodeNonZero<uint32_t>(NaturalEncoder*, const uint32_t*, size_t);
template void NaturalEncodeNonZero<uint64_t>(NaturalEncoder*, const uint64_t*, size_t);

template void NaturalDecodeNonZero<uint8_t>(NaturalDecoder*, uint8_t*, size_t);
template void NaturalDecodeNonZero<uint16_t>(NaturalDecoder*, uint16_t*, size_t);
template void NaturalDecodeNonZero<uint32_t>(NaturalDecoder*, uint32_t*, size_t);
template void NaturalDecodeNonZero<uint64_t>(NaturalDecoder*, uint64_t*, size_t);

fidl::Status NaturalDecode(::fidl::WireFormatMetadata metadata, size_t inline_size,
                           NaturalTopLevelDecodeFn decode_fn, ::fidl::EncodedMessage& message,
                           void* value) {
  if (fidl::Status status = EnsureSupportedWireFormat(metadata); !status.ok()) {
    std::move(message).CloseHandles();
    return status;
  }

  size_t message_byte_actual = message.bytes().size();
  uint32_t message_handle_actual = message.num_handles();
  ::fidl::internal::NaturalDecoder decoder(std::move(message), metadata.wire_format_version());
  size_t offset;
  if (unlikely(!decoder.Alloc(inline_size, &offset))) {
    return ::fidl::Error::DecodeError(decoder.status(), decoder.error());
  }

  decode_fn(&decoder, value, offset);
  if (unlikely(decoder.status() != ZX_OK)) {
    return ::fidl::Error::DecodeError(decoder.status(), decoder.error());
  }
  if (unlikely(decoder.CurrentLength() != message_byte_actual)) {
    return ::fidl::Error::DecodeError(ZX_ERR_INTERNAL, kCodingErrorNotAllBytesConsumed);
  }
  if (unlikely(decoder.CurrentHandleCount() != message_handle_actual)) {
    return ::fidl::Error::DecodeError(ZX_ERR_INTERNAL, kCodingErrorNotAllHandlesConsumed);
  }
  return ::fidl::Status::Ok();
}

}  // namespace fidl::internal
