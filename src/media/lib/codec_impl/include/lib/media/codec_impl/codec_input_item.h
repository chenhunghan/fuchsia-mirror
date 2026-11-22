// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_LIB_CODEC_IMPL_INCLUDE_LIB_MEDIA_CODEC_IMPL_CODEC_INPUT_ITEM_H_
#define SRC_MEDIA_LIB_CODEC_IMPL_INCLUDE_LIB_MEDIA_CODEC_IMPL_CODEC_INPUT_ITEM_H_

#include <fidl/fuchsia.media/cpp/fidl.h>
#include <fuchsia/media/cpp/fidl.h>

#include <memory>

class CodecPacket;

class CodecInputItem {
 public:
  // Move-only.
  //
  // Defaulting either (or both) of these auto-deletes implicit copy and
  // implicit assign.  In other words, defaulting counts as user-declared.
  CodecInputItem(CodecInputItem&& from) = default;
  CodecInputItem& operator=(CodecInputItem&& from) = default;

  static CodecInputItem Invalid();
  static CodecInputItem FormatDetails(
      std::shared_ptr<const fuchsia_media::FormatDetails> format_details);
  // Deprecated HLCPP soft-transition overload; prefer the natural overload above.
  static CodecInputItem FormatDetails(const fuchsia::media::FormatDetails& format_details);
  static CodecInputItem Packet(const CodecPacket* packet);
  static CodecInputItem EndOfStream();

  bool is_valid() const;
  bool is_format_details() const;
  bool is_packet() const;
  bool is_end_of_stream() const;

  const fuchsia_media::FormatDetails& format_details2() const;
  const std::shared_ptr<const fuchsia_media::FormatDetails>& format_details_ptr() const;
  // Deprecated HLCPP soft-transition accessor; prefer format_details2() above.
  const fuchsia::media::FormatDetails& format_details();
  const CodecPacket* packet() const;

 private:
  // !is_valid()
  CodecInputItem();
  explicit CodecInputItem(std::shared_ptr<const fuchsia_media::FormatDetails> format_details);
  explicit CodecInputItem(const CodecPacket* packet);

  // The fields of this class are relied upon to be move-able.

  bool is_valid_ = true;
  std::shared_ptr<const fuchsia_media::FormatDetails> format_details_;
  // Lazily populated only for callers still using deprecated HLCPP format_details().
  mutable std::unique_ptr<fuchsia::media::FormatDetails> hlcpp_format_details_;
  // If nullptr, is_end_of_stream() is true.
  const CodecPacket* packet_ = nullptr;

  // Lack of format_details_ and lack of packet_ means is_end_of_stream().
};

#endif  // SRC_MEDIA_LIB_CODEC_IMPL_INCLUDE_LIB_MEDIA_CODEC_IMPL_CODEC_INPUT_ITEM_H_
