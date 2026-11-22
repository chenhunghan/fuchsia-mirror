// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "lib/fidl/cpp/unknown_interactions_hlcpp.h"

namespace fidl {
namespace internal {

static const fidl_union_tag_t kUnknownMethodFrameworkErrTag = 3;

static const size_t kResponseOffset = sizeof(fidl_message_header_t);

::fidl::HLCPPOutgoingMessage EncodeUnknownMethodResponse(::fidl::MessageEncoder* encoder) {
  encoder->Alloc(sizeof(fidl_union_t));

  auto response_value = FrameworkErr::kUnknownMethod;
  if (::fidl::EncodingInlineSize<::fidl::FrameworkErr>(encoder) <=
      FIDL_ENVELOPE_INLINING_SIZE_THRESHOLD) {
    ::fidl::Encode(encoder, &response_value, kResponseOffset + offsetof(fidl_union_t, envelope));

    fidl_union_t* encoded = encoder->GetPtr<fidl_union_t>(kResponseOffset);
    encoded->tag = kUnknownMethodFrameworkErrTag;
    encoded->envelope.num_handles = 0;
    encoded->envelope.flags = FIDL_ENVELOPE_FLAGS_INLINING_MASK;
  } else {
    const size_t length_before = encoder->CurrentLength();
    ::fidl::Encode(
        encoder, &response_value,
        encoder->Alloc(::fidl::EncodingInlineSize<::fidl::FrameworkErr, ::fidl::Encoder>(encoder)));

    fidl_union_t* encoded = encoder->GetPtr<fidl_union_t>(kResponseOffset);
    encoded->tag = kUnknownMethodFrameworkErrTag;
    encoded->envelope.num_bytes = static_cast<uint32_t>(encoder->CurrentLength() - length_before);
    encoded->envelope.num_handles = 0;
    encoded->envelope.flags = 0;
  }

  return encoder->GetMessage();
}

}  // namespace internal
}  // namespace fidl
