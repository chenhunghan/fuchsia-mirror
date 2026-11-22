// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.media/cpp/hlcpp_conversion.h>
#include <lib/fidl/cpp/clone.h>
#include <lib/media/codec_impl/codec_input_item.h>
#include <zircon/assert.h>

CodecInputItem::CodecInputItem() : is_valid_(false) {
  // nothing else to do here
}

CodecInputItem::CodecInputItem(std::shared_ptr<const fuchsia_media::FormatDetails> format_details)
    : format_details_(std::move(format_details)) {
  ZX_DEBUG_ASSERT(format_details_);
}

CodecInputItem::CodecInputItem(const CodecPacket* packet) : packet_(packet) {
  // nothing else do to here
}

CodecInputItem CodecInputItem::Invalid() { return CodecInputItem(); }

CodecInputItem CodecInputItem::FormatDetails(
    std::shared_ptr<const fuchsia_media::FormatDetails> format_details) {
  return CodecInputItem(std::move(format_details));
}

CodecInputItem CodecInputItem::FormatDetails(const fuchsia::media::FormatDetails& format_details) {
  return CodecInputItem(std::make_shared<const fuchsia_media::FormatDetails>(
      fidl::HLCPPToNatural(fidl::Clone(format_details))));
}

CodecInputItem CodecInputItem::Packet(const CodecPacket* packet) {
  ZX_DEBUG_ASSERT(packet);
  return CodecInputItem(packet);
}

CodecInputItem CodecInputItem::EndOfStream() { return CodecInputItem(nullptr); }

bool CodecInputItem::is_valid() const { return is_valid_; }

bool CodecInputItem::is_format_details() const { return is_valid() && !!format_details_; }

bool CodecInputItem::is_packet() const { return is_valid() && !!packet_; }

bool CodecInputItem::is_end_of_stream() const { return is_valid() && !format_details_ && !packet_; }

const fuchsia_media::FormatDetails& CodecInputItem::format_details2() const {
  ZX_DEBUG_ASSERT(is_format_details());
  return *format_details_;
}

const std::shared_ptr<const fuchsia_media::FormatDetails>& CodecInputItem::format_details_ptr()
    const {
  ZX_DEBUG_ASSERT(is_format_details());
  return format_details_;
}

const fuchsia::media::FormatDetails& CodecInputItem::format_details() {
  ZX_DEBUG_ASSERT(is_format_details());
  if (!hlcpp_format_details_) {
    hlcpp_format_details_ = std::make_unique<fuchsia::media::FormatDetails>(
        fidl::NaturalToHLCPP(fuchsia_media::FormatDetails(*format_details_)));
  }
  return *hlcpp_format_details_;
}

const CodecPacket* CodecInputItem::packet() const {
  ZX_DEBUG_ASSERT(is_packet());
  return packet_;
}
