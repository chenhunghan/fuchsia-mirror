// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_USB_LIB_USB_INCLUDE_USB_REQUEST_FIDL_H_
#define SRC_DEVICES_USB_LIB_USB_INCLUDE_USB_REQUEST_FIDL_H_

#include <lib/io-buffer/phys-iter.h>

#include "src/devices/usb/lib/usb/include/usb/internal/request-fidl.h"

namespace usb {

// Exposed to header users.
using internal::EndpointType;
using internal::MappedVmo;

using FidlRequest = internal::FidlRequest<io_buffer::PhysIter>;
using FidlRequestPool = internal::FidlRequestPool<FidlRequest>;

// Template specialization for internal::FidlRequest<io_buffer::PhysIter>.
template <>
inline zx::result<io_buffer::PhysIter> FidlRequest::phys_iter(size_t idx, size_t max_length) const {
  if (!request_.data().has_value() || idx >= request_.data()->size()) {
    return zx::error(ZX_ERR_OUT_OF_RANGE);
  }
  const auto& region = request_.data()->at(idx);
  if (!region.size().has_value() || *region.size() == 0) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  const auto pinned_it = pinned_vmos_.find(idx);
  if (pinned_it == pinned_vmos_.end()) {
    return zx::error(ZX_ERR_NOT_FOUND);
  }
  const auto& pinned = pinned_it->second;
  if (pinned.page_list == nullptr || pinned.phys_count == 0) {
    return zx::error(ZX_ERR_BAD_STATE);
  }
  const uint64_t length = *region.size();
  const uint64_t offset = region.offset().value_or(0);
  // phys_iter is only called for Buffer::Tag::kData via PhysMap pinned_vmos_.
  // PhysMap pins page-aligned physical pages for [0, pinned.mapped.size) within a single page
  // (kMaxTransferSize < kPageSize); enforce bounds before applying the sub-page vmo_offset.
  if (offset >= kPageSize || offset > pinned.mapped.size || length > pinned.mapped.size - offset) {
    return zx::error(ZX_ERR_OUT_OF_RANGE);
  }
  phys_iter_buffer_t buf = {.phys = pinned.page_list,
                            .phys_count = pinned.phys_count,
                            .length = static_cast<size_t>(length),
                            .vmo_offset = offset,
                            .sg_list = nullptr,
                            .sg_count = 0};
  return zx::ok(io_buffer::PhysIter(buf, max_length));
}

}  // namespace usb

#endif  // SRC_DEVICES_USB_LIB_USB_INCLUDE_USB_REQUEST_FIDL_H_
