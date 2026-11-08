// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_USB_LIB_USB_INCLUDE_USB_INTERNAL_REQUEST_FIDL_H_
#define SRC_DEVICES_USB_LIB_USB_INCLUDE_USB_INTERNAL_REQUEST_FIDL_H_

#include <fidl/fuchsia.hardware.usb.request/cpp/fidl.h>

// Figure out which logging runtime and function to use, and bind it to the FDF_LOG() macro where
// necessary. The possible cases are:
//   1. DFv2 runtime using the comapt shim. To link against the compat shim, build with
//      -DDFV2_COMPAT_LOGGING.
//   2. DFv1 runtime using the DDK. To use link against the DDK, build with -DDFV1_LOGGING.
//   3. (default) DFv2 runtime using the native DFv2 logging runtime. With Neither flag in effect,
//      link against the native DFv2 runtime.
//
// If building against the SDK, the binary must be linked against the native DFv2 logging runtime as
// neither the DDK nor compat shim are baked into the SDK.
#ifdef DFV2_COMPAT_LOGGING
#include <lib/driver/compat/cpp/logging.h>  // nogncheck
#elif defined(DFV1_LOGGING)
#include <lib/ddk/debug.h>  // nogncheck
#define FDF_LOG(args, ...) zxlogf(args, __VA_ARGS__)
#else
#include "sdk/lib/driver/logging/cpp/logger.h"  // nogncheck
#endif

#include <lib/fit/defer.h>
#include <lib/zx/bti.h>
#include <lib/zx/result.h>
#include <lib/zx/vmar.h>
#include <zircon/status.h>

#include <cstring>
#include <functional>
#include <limits>
#include <map>
#include <memory>
#include <optional>
#include <queue>
#include <span>
#include <utility>
#include <vector>

#include "src/devices/usb/lib/usb/align.h"

namespace usb::internal {

// EndpointType: One of 4 endpoint types as defined by USB.
enum EndpointType : uint8_t {
  UNKNOWN,

  CONTROL,
  BULK,
  ISOCHRONOUS,
  INTERRUPT,
};

// MappedVmo is a container that keeps track of the virtual address and size of the VMO mapped.
struct MappedVmo {
  // addr: virtual address.
  zx_vaddr_t addr;
  // size: size of VMO that was mapped to this virtual address.
  size_t size;
};

// FidlRequest is a wrapper around a fuchsia_hardware_usb_request::Request implementing common
// functionality. Especially, FidlRequest keeps track of VMOs that were pinned upon PhysMap() and
// unpins them upon destruction.
template <typename PhysIterType>
class FidlRequest {
 public:
  using get_mapped_func_t = std::function<zx::result<std::optional<MappedVmo>>(
      const fuchsia_hardware_usb_request::Buffer& buffer)>;

  FidlRequest() = default;
  explicit FidlRequest(EndpointType ep_type) {
    switch (ep_type) {
      case CONTROL:
        set_control();
        break;
      case BULK:
        set_bulk();
        break;
      case ISOCHRONOUS:
        set_isochronous();
        break;
      case INTERRUPT:
        set_interrupt();
        break;
      default:
        ZX_ASSERT(false);
    };
  }
  explicit FidlRequest(fuchsia_hardware_usb_request::Request request)
      : request_(std::move(request)) {}

  // Disallow copy and assign, allow move.
  FidlRequest(FidlRequest&& request) = default;
  FidlRequest& operator=(FidlRequest&& request) = delete;
  FidlRequest(const FidlRequest&) = delete;
  FidlRequest& operator=(const FidlRequest&) = delete;

  ~FidlRequest() { Unpin(); }

  FidlRequest& set_control(fuchsia_hardware_usb_descriptor::UsbSetup setup = {}) {
    request_.information(fuchsia_hardware_usb_request::RequestInfo::WithControl(
        fuchsia_hardware_usb_request::ControlRequestInfo().setup(std::move(setup))));
    return *this;
  }
  FidlRequest& set_bulk() {
    request_.information(fuchsia_hardware_usb_request::RequestInfo::WithBulk(
        fuchsia_hardware_usb_request::BulkRequestInfo()));
    return *this;
  }
  FidlRequest& set_isochronous(uint64_t frame_id = 0) {
    request_.information(fuchsia_hardware_usb_request::RequestInfo::WithIsochronous(
        fuchsia_hardware_usb_request::IsochronousRequestInfo().frame_id(frame_id)));
    return *this;
  }
  FidlRequest& set_interrupt() {
    request_.information(fuchsia_hardware_usb_request::RequestInfo::WithInterrupt(
        fuchsia_hardware_usb_request::InterruptRequestInfo()));
    return *this;
  }

  FidlRequest& add_vmo_id(uint64_t vmo_id, size_t size = 0, size_t offset = 0) {
    _length.reset();
    if (!request_.data().has_value()) {
      request_.data().emplace();
    }

    request_.data()
        ->emplace_back()
        .buffer(fuchsia_hardware_usb_request::Buffer::WithVmoId(vmo_id))
        .offset(offset)
        .size(size);
    return *this;
  }
  // Appends an inline kData BufferRegion to the request. If `size` is omitted, infers the region
  // size as `data.size() - offset` (requiring `offset < data.size()`). Enforces a non-zero region
  // size (`actual_size > 0`).
  FidlRequest& add_data(std::vector<uint8_t> data, std::optional<size_t> size = std::nullopt,
                        size_t offset = 0) {
    _length.reset();
    if (!request_.data().has_value()) {
      request_.data().emplace();
    }

    size_t actual_size;
    if (size.has_value()) {
      actual_size = *size;
    } else {
      ZX_ASSERT(offset < data.size());
      actual_size = data.size() - offset;
    }
    ZX_ASSERT(actual_size > 0);

    size_t required_size;
    if (!__builtin_add_overflow(offset, actual_size, &required_size) &&
        required_size <= fuchsia_hardware_usb_request::kMaxTransferSize) {
      if (data.size() < required_size) {
        data.resize(required_size);
      }
    }

    request_.data()
        ->emplace_back()
        .buffer(fuchsia_hardware_usb_request::Buffer::WithData(std::move(data)))
        .offset(offset)
        .size(actual_size);
    return *this;
  }
  FidlRequest& clear_buffers() {
    _length.reset();
    if (!request_.data().has_value()) {
      return *this;
    }
    for (auto& d : *request_.data()) {
      d.offset(0);
      d.size(0);
      if (d.buffer().has_value() &&
          d.buffer()->Which() == fuchsia_hardware_usb_request::Buffer::Tag::kData &&
          d.buffer()->data().has_value()) {
        d.buffer()->data()->clear();
      }
    }
    return *this;
  }
  FidlRequest& reset_buffers(const get_mapped_func_t& get_mapped) {
    _length.reset();
    if (!request_.data().has_value()) {
      return *this;
    }
    for (auto& d : *request_.data()) {
      d.offset(0);
      if (!d.buffer().has_value()) {
        d.size(0);
        continue;
      }
      switch (d.buffer()->Which()) {
        case fuchsia_hardware_usb_request::Buffer::Tag::kVmoId: {
          auto mapped = get_mapped(*d.buffer());
          if (mapped.is_ok() && mapped.value().has_value()) {
            d.size(mapped.value()->size);
          } else {
            d.size(0);
          }
        } break;
        case fuchsia_hardware_usb_request::Buffer::Tag::kData: {
          const size_t kMaxTransfer =
              static_cast<size_t>(fuchsia_hardware_usb_request::kMaxTransferSize);
          size_t cap = kMaxTransfer;
          if (d.buffer()->data().has_value() && d.buffer()->data()->capacity() > 0) {
            cap = std::min(d.buffer()->data()->capacity(), kMaxTransfer);
          }
          d.size(cap);
        } break;
        default:
          d.size(0);
          break;
      }
    }
    return *this;
  }

  // RegionCapacity returns the writable capacity of a BufferRegion from its region offset.
  // When `d.size() > 0`, `d.size()` bounds the region capacity to the configured transfer size.
  // When `d.size() == 0` (such as after `clear_buffers()`), capacity resolves to the backing VMO
  // or `kMaxTransferSize` limit. `CopyTo` expands unpopulated (`d.size() == 0`) regions to this
  // capacity only when writing at local offset 0, and skips them as 0-length at positive offsets.
  size_t RegionCapacity(const fuchsia_hardware_usb_request::BufferRegion& d,
                        const get_mapped_func_t& get_mapped) const {
    if (!d.buffer().has_value()) {
      return 0;
    }
    std::optional<MappedVmo> mapped_vmo;
    if (d.buffer()->Which() == fuchsia_hardware_usb_request::Buffer::Tag::kVmoId) {
      auto mapped = get_mapped(*d.buffer());
      if (mapped.is_error()) {
        return 0;
      }
      mapped_vmo = mapped.value();
    }
    return RegionCapacity(d, mapped_vmo);
  }

  // CopyTo: tries to copy `size` bytes from `buffer` to contiguous `request` buffers from
  // `offset`. Returns the number of bytes copied for each buffer.
  //
  // Cache management is the responsibility of the user. See `CachedCopyTo` below for a
  // version of this method that performs cache management on the user's behalf.
  std::vector<size_t> CopyTo(size_t offset, const void* buffer, size_t size,
                             const get_mapped_func_t& get_mapped) {
    if (!request_.data().has_value()) {
      return {};
    }
    std::vector<size_t> cp_sizes(request_.data()->size(), 0);
    if (size == 0 || buffer == nullptr) {
      return cp_sizes;
    }
    size_t end_offset;
    if (__builtin_add_overflow(offset, size, &end_offset)) {
      return cp_sizes;
    }

    const uint8_t* start = static_cast<const uint8_t*>(buffer);
    size_t todo = size;
    size_t cur_offset = offset;
    for (size_t i = 0; i < request_.data()->size(); ++i) {
      auto& d = (*request_.data())[i];
      if (!d.buffer().has_value()) {
        continue;
      }
      if (d.size().value_or(0) == 0 && cur_offset > 0) {
        continue;
      }

      std::optional<MappedVmo> mapped;
      if (d.buffer()->Which() == fuchsia_hardware_usb_request::Buffer::Tag::kVmoId) {
        auto mapped_res = get_mapped(*d.buffer());
        if (mapped_res.is_error()) {
          return cp_sizes;
        }
        mapped = mapped_res.value();
      }

      const size_t reg_offset = d.offset().value_or(0);
      const size_t reg_cap = RegionCapacity(d, mapped);
      if (cur_offset >= reg_cap) {
        cur_offset -= reg_cap;
        continue;
      }

      const size_t cp_size = std::min(todo, reg_cap - cur_offset);
      uint8_t* dst_ptr = nullptr;
      switch (d.buffer()->Which()) {
        case fuchsia_hardware_usb_request::Buffer::Tag::kVmoId: {
          if (mapped.has_value()) {
            dst_ptr = reinterpret_cast<uint8_t*>(static_cast<uintptr_t>(mapped->addr)) +
                      reg_offset + cur_offset;
          }
          break;
        }
        case fuchsia_hardware_usb_request::Buffer::Tag::kData: {
          if (d.buffer()->data().has_value()) {
            const size_t needed = reg_offset + cur_offset + cp_size;
            if (d.buffer()->data()->size() < needed) {
              d.buffer()->data()->resize(needed);
            }
            dst_ptr = d.buffer()->data()->data() + reg_offset + cur_offset;
          }
          break;
        }
        default:
          break;
      }

      if (dst_ptr == nullptr) {
        break;
      }

      std::memcpy(dst_ptr, start, cp_size);
      const size_t written_end = cur_offset + cp_size;
      if (d.size().value_or(0) < written_end) {
        d.size(written_end);
        _length.reset();
      }
      cp_sizes[i] = cp_size;
      start += cp_size;
      todo -= cp_size;

      cur_offset = 0;
      if (todo == 0) {
        break;
      }
    }

    return cp_sizes;
  }

  std::vector<size_t> CopyTo(size_t offset, std::span<const uint8_t> buffer,
                             const get_mapped_func_t& get_mapped) {
    return CopyTo(offset, buffer.data(), buffer.size(), get_mapped);
  }

  // CopyFrom: tries to copy `size` bytes from `request` (starting from `offset`) to `buffer`.
  // Returns the number of bytes copied for each buffer.
  //
  // Cache management is the responsibility of the user. See `CachedCopyFrom` below for a
  // version of this method that performs cache management on the user's behalf.
  std::vector<size_t> CopyFrom(size_t offset, void* buffer, size_t size,
                               const get_mapped_func_t& get_mapped) {
    if (!request_.data().has_value()) {
      return {};
    }
    std::vector<size_t> cp_sizes(request_.data()->size(), 0);
    if (size == 0 || buffer == nullptr) {
      return cp_sizes;
    }
    size_t end_offset;
    if (__builtin_add_overflow(offset, size, &end_offset)) {
      return cp_sizes;
    }

    uint8_t* start = static_cast<uint8_t*>(buffer);
    size_t todo = size;
    size_t cur_offset = offset;
    for (size_t i = 0; i < request_.data()->size(); ++i) {
      const auto& d = (*request_.data())[i];
      if (!d.buffer().has_value()) {
        continue;
      }
      const size_t reg_size = d.size().value_or(0);
      if (cur_offset >= reg_size) {
        cur_offset -= reg_size;
        continue;
      }

      auto mapped = get_mapped(*d.buffer());
      if (mapped.is_error()) {
        return cp_sizes;
      }

      const size_t reg_offset = d.offset().value_or(0);
      size_t avail_cap = 0;
      const uint8_t* src_ptr = nullptr;
      switch (d.buffer()->Which()) {
        case fuchsia_hardware_usb_request::Buffer::Tag::kVmoId: {
          if (mapped.value().has_value() && reg_offset < mapped.value()->size) {
            avail_cap = mapped.value()->size - reg_offset;
            src_ptr =
                reinterpret_cast<const uint8_t*>(static_cast<uintptr_t>(mapped.value()->addr)) +
                reg_offset;
          }
          break;
        }
        case fuchsia_hardware_usb_request::Buffer::Tag::kData: {
          if (d.buffer()->data().has_value()) {
            const auto& vec = d.buffer()->data().value();
            if (reg_offset < vec.size()) {
              avail_cap = vec.size() - reg_offset;
              src_ptr = vec.data() + reg_offset;
            }
          }
          break;
        }
        default:
          break;
      }

      if (src_ptr == nullptr || avail_cap == 0) {
        break;
      }

      const size_t buffer_size = std::min(reg_size, avail_cap);
      if (cur_offset >= buffer_size) {
        break;
      }

      const size_t cp_size = std::min(todo, buffer_size - cur_offset);
      std::memcpy(start, src_ptr + cur_offset, cp_size);
      cp_sizes[i] = cp_size;
      start += cp_size;
      todo -= cp_size;

      cur_offset = 0;
      if (todo == 0) {
        break;
      }
    }

    return cp_sizes;
  }

  std::vector<size_t> CopyFrom(size_t offset, std::span<uint8_t> buffer,
                               const get_mapped_func_t& get_mapped) {
    return CopyFrom(offset, buffer.data(), buffer.size(), get_mapped);
  }

  // About CacheFlush and CacheFlushInvalidate: CacheFlush or CacheFlush invalidate MUST always be
  // called after data is written to the VMO and before the data is processed on schedule (see
  // comment in endpoint.fidl on `RequestQueue`).
  // CacheFlush should be called for operations where data is to be written out, but not read in
  // and CacheFlushInvalidate should be called for operations where data needs to be read in.
  // CacheFlush and CacheFlushInvalidate flush and invalidate cache for all buffer regions of a
  // request.
  zx_status_t CacheFlushInvalidate(get_mapped_func_t get_mapped) {
    return CacheRangeHelper(0, length(), ZX_CACHE_FLUSH_DATA | ZX_CACHE_FLUSH_INVALIDATE,
                            get_mapped);
  }

  zx_status_t CacheFlushInvalidate(get_mapped_func_t get_mapped, size_t offset, size_t size) {
    return CacheRangeHelper(offset, size, ZX_CACHE_FLUSH_DATA | ZX_CACHE_FLUSH_INVALIDATE,
                            get_mapped);
  }

  zx_status_t CacheFlush(get_mapped_func_t get_mapped) {
    return CacheRangeHelper(0, length(), ZX_CACHE_FLUSH_DATA, get_mapped);
  }

  zx_status_t CacheFlush(get_mapped_func_t get_mapped, size_t offset, size_t size) {
    return CacheRangeHelper(offset, size, ZX_CACHE_FLUSH_DATA, get_mapped);
  }

  // Similar to `CopyTo`, copies `size` bytes from `buffer` to contiguous request buffers starting
  // at `offset`, and flushes the cache for `[offset, offset + size)` after copying.
  //
  // For multi-part writes into a request buffer (such as copying a header followed by payload
  // data), call `CopyTo` for intermediate writes and call `CachedCopyTo` on the final write (or
  // call `CacheFlush` at the end) to avoid flushing cache lines multiple times.
  //
  // For multi-region scatter-gather requests, `CopyTo` expands unpopulated (`d.size() == 0`)
  // regions to capacity only when writing into them at local offset 0. Both `CopyTo` and
  // `CacheFlush` treat unpopulated regions as 0-length when skipping past them at a positive
  // `offset`.
  //
  // Parameters:
  //  * offset: Byte offset into the request buffers to start writing.
  //  * buffer: Source buffer to read data from.
  //  * size: Number of bytes to copy.
  //  * get_mapped: Callback to resolve virtual mapping information for buffer tags.
  // Returns a vector of byte counts copied to each request buffer on success, or an error status
  // if `CacheFlush` fails, mapping resolution fails, or `offset` is out of bounds
  // (`ZX_ERR_OUT_OF_RANGE`).
  zx::result<std::vector<size_t>> CachedCopyTo(size_t offset, const void* buffer, size_t size,
                                               const get_mapped_func_t& get_mapped) {
    if (!request_.data().has_value()) {
      return zx::ok(std::vector<size_t>{});
    }
    if (size == 0) {
      return zx::ok(std::vector<size_t>(request_.data()->size(), 0));
    }
    auto cp_sizes = CopyTo(offset, buffer, size, get_mapped);
    if (zx_status_t status = CacheFlush(get_mapped, offset, size); status != ZX_OK) {
      FDF_LOG(ERROR, "CacheFlush(): %s", zx_status_get_string(status));
      return zx::error(status);
    }
    return zx::ok(std::move(cp_sizes));
  }

  // Similar to `CopyFrom`, copies `size` bytes from contiguous request buffers starting at
  // `offset` into `buffer`. To ensure cache coherency, this method invalidates the cache before
  // copying.
  //
  // Only the range `[offset, offset + size)` is invalidated.
  //
  // For multi-part reads from a request buffer (such as reading a header followed by payload
  // data), call `CachedCopyFrom` on the first read (or use an explicit `CacheFlushInvalidate`
  // before reading) and use `CopyFrom` for subsequent reads to avoid invalidating cache lines
  // multiple times.
  //
  // Parameters:
  //  * offset: Byte offset into the request buffers to start reading.
  //  * buffer: Destination buffer to write data into.
  //  * size: Number of bytes to copy.
  //  * get_mapped: Callback to resolve virtual mapping information for buffer tags.
  //
  // Returns a vector of byte counts copied from each request buffer on success, or an error status
  // if `CacheFlushInvalidate` fails, mapping resolution fails, or `offset` is out of bounds
  // (`ZX_ERR_OUT_OF_RANGE`).
  zx::result<std::vector<size_t>> CachedCopyFrom(size_t offset, void* buffer, size_t size,
                                                 const get_mapped_func_t& get_mapped) {
    if (!request_.data().has_value()) {
      return zx::ok(std::vector<size_t>{});
    }
    if (size == 0) {
      return zx::ok(std::vector<size_t>(request_.data()->size(), 0));
    }
    if (offset >= length()) {
      return zx::error(ZX_ERR_OUT_OF_RANGE);
    }
    if (zx_status_t status = CacheFlushInvalidate(get_mapped, offset, size); status != ZX_OK) {
      FDF_LOG(ERROR, "CacheFlushInvalidate(): %s", zx_status_get_string(status));
      return zx::error(status);
    }
    return zx::ok(CopyFrom(offset, buffer, size, get_mapped));
  }

  zx::result<std::vector<size_t>> CachedCopyTo(size_t offset, std::span<const uint8_t> buffer,
                                               const get_mapped_func_t& get_mapped) {
    return CachedCopyTo(offset, buffer.data(), buffer.size(), get_mapped);
  }

  zx::result<std::vector<size_t>> CachedCopyFrom(size_t offset, std::span<uint8_t> buffer,
                                                 const get_mapped_func_t& get_mapped) {
    return CachedCopyFrom(offset, buffer.data(), buffer.size(), get_mapped);
  }

  // CacheHelper flushes and invalidates cache for a slice of a specified buffer region.
  zx_status_t CacheHelper(const fuchsia_hardware_usb_request::BufferRegion& buffer,
                          size_t offset_in_region, size_t flush_size, uint32_t options,
                          const get_mapped_func_t& get_mapped) {
    if (flush_size == 0 || buffer.size().value_or(0) == 0 || !buffer.buffer().has_value()) {
      return ZX_OK;
    }
    auto mapped = get_mapped(*buffer.buffer());
    if (mapped.is_error()) {
      return mapped.error_value();
    }
    if (!mapped.value().has_value()) {
      return ZX_OK;
    }

    const size_t reg_offset = buffer.offset().value_or(0);
    size_t flush_offset;
    size_t flush_end;
    if (__builtin_add_overflow(reg_offset, offset_in_region, &flush_offset) ||
        __builtin_add_overflow(flush_offset, flush_size, &flush_end) ||
        flush_end > mapped.value()->size) {
      return ZX_ERR_OUT_OF_RANGE;
    }

    zx_status_t status = zx_cache_flush(
        reinterpret_cast<void*>(static_cast<uintptr_t>(mapped.value()->addr) + flush_offset),
        flush_size, options);
    if (status != ZX_OK) {
      return status;
    }
    return ZX_OK;
  }

  // CacheRangeHelper iterates across populated `BufferRegion` segments (`d.size() > 0`) in the
  // request and performs cache maintenance (flushing or invalidation) on the byte slice
  // `[offset, offset + size)`. Ranges starting within `length()` that extend past the end of the
  // populated regions are clamped to the available bytes.
  zx_status_t CacheRangeHelper(size_t offset, size_t size, uint32_t options,
                               const get_mapped_func_t& get_mapped) {
    if (!request_.data().has_value()) {
      return ZX_OK;
    }
    for (const auto& d : *request_.data()) {
      if (d.buffer().has_value() && (!d.size().has_value() || !d.offset().has_value())) {
        return ZX_ERR_INVALID_ARGS;
      }
    }
    if (size == 0) {
      return ZX_OK;
    }
    if (offset >= length()) {
      return ZX_ERR_OUT_OF_RANGE;
    }
    zx_status_t ret_status = ZX_OK;
    size_t todo = size;
    size_t cur_offset = offset;
    for (const auto& d : *request_.data()) {
      const size_t reg_size = d.size().value_or(0);
      if (reg_size == 0) {
        continue;
      }
      if (cur_offset >= reg_size) {
        cur_offset -= reg_size;
        continue;
      }
      size_t flush_size = std::min(todo, reg_size - cur_offset);
      zx_status_t status = CacheHelper(d, cur_offset, flush_size, options, get_mapped);
      if (status != ZX_OK) {
        ret_status = status;
      }
      cur_offset = 0;
      todo -= flush_size;
      if (todo == 0) {
        break;
      }
    }
    return ret_status;
  }

  // Pins VMOs if needed. For
  //  - fuchsia_hardware_usb_request::Buffer::Tag::kVmoId -- Uses preregistered VMO. Does nothing.
  //  - fuchsia_hardware_usb_request::Buffer::Tag::kVmo   -- Pins VMO.
  //  - fuchsia_hardware_usb_request::Buffer::Tag::kData  -- Creates, maps, pins, VMO. Copies data
  //                                                         to VMO. (Unmapped and unpinned on
  //                                                         Unpin()).
  // Returns ZX_ERR_BAD_STATE if kData regions are already pinned without a prior Unpin() call.
  zx_status_t PhysMap(const zx::bti& bti) {
    if (!pinned_vmos_.empty()) {
      return ZX_ERR_BAD_STATE;
    }
    if (!request_.data().has_value()) {
      return ZX_OK;
    }
    auto cleanup = fit::defer([this]() { Unpin(); });

    int64_t idx = -1;
    for (auto& d : *request_.data()) {
      idx++;
      if (!d.buffer().has_value()) {
        continue;
      }
      if (!d.size().has_value() || !d.offset().has_value()) {
        return ZX_ERR_INVALID_ARGS;
      }
      zx::vmo vmo;
      zx_vaddr_t mapped_addr = 0;
      const size_t req_size = d.size().value();
      const size_t reg_offset = d.offset().value();
      size_t end_offset;
      if (__builtin_add_overflow(reg_offset, req_size, &end_offset)) {
        return ZX_ERR_INVALID_ARGS;
      }
      switch (d.buffer()->Which()) {
        case fuchsia_hardware_usb_request::Buffer::Tag::kVmoId:
          // Pre-registered and already pinned. Does not need to be pinned again.
          continue;
        case fuchsia_hardware_usb_request::Buffer::Tag::kData: {
          if (req_size == 0) {
            continue;
          }
          if (end_offset > fuchsia_hardware_usb_request::kMaxTransferSize) {
            return ZX_ERR_INVALID_ARGS;
          }
          if (d.buffer()->data().has_value() && d.buffer()->data()->size() < end_offset) {
            d.buffer()->data()->resize(end_offset);
          }
          zx_status_t status = zx::vmo::create(end_offset, 0, &vmo);
          if (status != ZX_OK) {
            return status;
          }

          uintptr_t mapped = 0;
          status = zx::vmar::root_self()->map(ZX_VM_PERM_READ | ZX_VM_PERM_WRITE, 0, vmo, 0,
                                              end_offset, &mapped);
          if (status != ZX_OK) {
            return status;
          }

          if (d.buffer()->data().has_value() && !d.buffer()->data()->empty()) {
            const size_t available = d.buffer()->data()->size() > reg_offset
                                         ? d.buffer()->data()->size() - reg_offset
                                         : 0;
            const size_t copy_bytes = std::min(req_size, available);
            if (copy_bytes > 0) {
              std::memcpy(reinterpret_cast<void*>(mapped + reg_offset),
                          d.buffer()->data()->data() + reg_offset, copy_bytes);
            }
          }
          zx_cache_flush(reinterpret_cast<void*>(mapped + reg_offset), req_size,
                         ZX_CACHE_FLUSH_DATA | ZX_CACHE_FLUSH_INVALIDATE);
          mapped_addr = mapped;
        } break;
        default:
          return ZX_ERR_NOT_SUPPORTED;
      }

      // Record the VMAR mapping before zx_bti_pin so cleanup can unmap it if pinning fails.
      pinned_vmos_[idx] = {
          ZX_HANDLE_INVALID,
          nullptr,
          0,
          {mapped_addr, end_offset},
      };

      uint64_t page_offset = USB_ROUNDDOWN(reg_offset, kPageSize);
      uint64_t page_length = (reg_offset + req_size) - page_offset;
      uint64_t pages = USB_ROUNDUP(page_length, kPageSize) / kPageSize;

      std::unique_ptr<zx_paddr_t[]> paddrs{new zx_paddr_t[pages]};
      const size_t sub_offset = reg_offset & (kPageSize - 1);
      const size_t pin_offset = page_offset;
      const size_t pin_length = pages * kPageSize;

      zx_handle_t pmt = ZX_HANDLE_INVALID;
      uint32_t options = ZX_BTI_PERM_READ | ZX_BTI_PERM_WRITE;
      zx_status_t status = zx_bti_pin(bti.get(), options, vmo.get(), pin_offset, pin_length,
                                      paddrs.get(), pages, &pmt);
      if (status != ZX_OK) {
        FDF_LOG(ERROR, "zx_bti_pin(): %s", zx_status_get_string(status));
        return status;
      }

      // Account for the initial misalignment if any
      paddrs.get()[0] += sub_offset;

      pinned_vmos_[idx].pmt = pmt;
      pinned_vmos_[idx].phys_list = paddrs.release();
      pinned_vmos_[idx].phys_count = pages;
    }

    cleanup.cancel();
    return ZX_OK;
  }

  // Unpins VMOs pinned by PhysMap.
  zx_status_t Unpin() {
    auto pinned_vmos = std::move(pinned_vmos_);
    for (const auto& [idx, pinned] : pinned_vmos) {
      if (request_.data().has_value() && idx < request_.data()->size()) {
        auto& d = (*request_.data())[idx];
        if (d.buffer().has_value() &&
            d.buffer()->Which() == fuchsia_hardware_usb_request::Buffer::Tag::kData) {
          if (d.buffer()->data().has_value()) {
            const size_t reg_offset = d.offset().value_or(0);
            const size_t vec_avail = (d.buffer()->data()->size() > reg_offset)
                                         ? d.buffer()->data()->size() - reg_offset
                                         : 0;
            const size_t mapped_avail =
                (pinned.mapped.size > reg_offset) ? pinned.mapped.size - reg_offset : 0;
            const size_t copy_size = std::min({d.size().value_or(0), mapped_avail, vec_avail});
            if (copy_size > 0) {
              const void* mapped_src = reinterpret_cast<const void*>(
                  static_cast<uintptr_t>(pinned.mapped.addr) + reg_offset);
              zx_cache_flush(mapped_src, copy_size,
                             ZX_CACHE_FLUSH_DATA | ZX_CACHE_FLUSH_INVALIDATE);
              std::memcpy(d.buffer()->data()->data() + reg_offset, mapped_src, copy_size);
            }
          }
        }
      }
      if (pinned.mapped.addr != 0) {
        zx_status_t status = zx::vmar::root_self()->unmap(
            reinterpret_cast<uintptr_t>(pinned.mapped.addr), pinned.mapped.size);
        ZX_DEBUG_ASSERT(status == ZX_OK);
      }

      if (pinned.pmt != ZX_HANDLE_INVALID) {
        zx_status_t status = zx_pmt_unpin(pinned.pmt);
        ZX_DEBUG_ASSERT(status == ZX_OK);
      }
      delete[] pinned.phys_list;
    }
    shared_contexts_.clear();
    return ZX_OK;
  }

  void add_shared_context(std::shared_ptr<void> ctx) { shared_contexts_.push_back(std::move(ctx)); }

  // Gets a PhysIterType for the buffer at request.data()->at(idx). PhysIterTypes return a
  // std::pair<zx_paddr_t, size_t> on iteration. For template specialization, see the public
  // headers outside the internal namespace.
  PhysIterType phys_iter(size_t idx, size_t max_length) const;

  fuchsia_hardware_usb_request::Request* operator->() {
    _length.reset();
    return &request_;
  }
  const fuchsia_hardware_usb_request::Request* operator->() const { return &request_; }
  const fuchsia_hardware_usb_request::Request& request() const { return request_; }
  // Ensures any removal of `request` is intentional and `Unpin` is called.
  fuchsia_hardware_usb_request::Request take_request() {
    zx_status_t status = Unpin();
    ZX_DEBUG_ASSERT(status == ZX_OK);
    _length.reset();
    return std::move(request_);
  }
  // Returns the total length of all data in the request. Saves to a variable for future use.
  size_t length() const {
    if (!_length.has_value()) {
      size_t len = 0;
      if (request_.data().has_value()) {
        for (const auto& d : *request_.data()) {
          size_t next_len;
          if (__builtin_add_overflow(len, d.size().value_or(0), &next_len)) {
            len = std::numeric_limits<size_t>::max();
            break;
          }
          len = next_len;
        }
      }
      _length = len;
    }
    return *_length;
  }

  // Returns the total capacity of all buffers in the request.
  size_t capacity(const get_mapped_func_t& get_mapped) const {
    size_t cap = 0;
    if (!request_.data().has_value()) {
      return 0;
    }
    for (const auto& d : *request_.data()) {
      size_t next_cap;
      if (__builtin_add_overflow(cap, RegionCapacity(d, get_mapped), &next_cap)) {
        return std::numeric_limits<size_t>::max();
      }
      cap = next_cap;
    }
    return cap;
  }

 private:
  size_t RegionCapacity(const fuchsia_hardware_usb_request::BufferRegion& d,
                        const std::optional<MappedVmo>& mapped) const {
    if (!d.buffer().has_value()) {
      return 0;
    }
    const size_t reg_offset = d.offset().value_or(0);
    switch (d.buffer()->Which()) {
      case fuchsia_hardware_usb_request::Buffer::Tag::kVmoId: {
        if (!mapped.has_value()) {
          return 0;
        }
        const size_t mapped_size = mapped->size;
        if (reg_offset >= mapped_size) {
          return 0;
        }
        const size_t vmo_avail = mapped_size - reg_offset;
        if (d.size().has_value() && d.size().value() > 0) {
          return std::min(d.size().value(), vmo_avail);
        }
        return vmo_avail;
      }
      case fuchsia_hardware_usb_request::Buffer::Tag::kData: {
        const size_t kMaxTransfer =
            static_cast<size_t>(fuchsia_hardware_usb_request::kMaxTransferSize);
        if (reg_offset >= kMaxTransfer) {
          return 0;
        }
        const size_t max_avail = kMaxTransfer - reg_offset;
        if (d.size().has_value() && d.size().value() > 0) {
          return std::min(d.size().value(), max_avail);
        }
        if (d.buffer()->data().has_value() && !d.buffer()->data()->empty()) {
          if (d.buffer()->data()->size() > reg_offset) {
            return std::min(d.buffer()->data()->size() - reg_offset, max_avail);
          }
          return 0;
        }
        return max_avail;
      }
      default:
        return 0;
    }
  }

  const size_t kPageSize = zx_system_get_page_size();

  // request_: FIDL request object.
  fuchsia_hardware_usb_request::Request request_;

  struct pinned_vmo_t {
    zx_handle_t pmt;
    uint64_t* phys_list;
    size_t phys_count;
    // mapped: only used for fuchsia_hardware_usb_request::Buffer::Tag::kData.
    MappedVmo mapped;
  };
  // pinned_vmos_: VMOs that were pinned by this request when PhysMap() was called. Will be
  // unpinned by when this request is destructed or when Unpin() is called. Indexed in the same
  // order as request_.data(), where only buffers that are
  // fuchsia_hardware_usb_request::Buffer::Tag::kVmoId are left empty.
  std::map<size_t, pinned_vmo_t> pinned_vmos_ = {};

  // _length: Total length saved so calculation doesn't have to be done multiple times.
  mutable std::optional<size_t> _length = std::nullopt;

  // shared_contexts_: Reference-counted external objects (such as registered VMOs) whose lifetime
  // must extend until this request is unpinned or destroyed.
  std::vector<std::shared_ptr<void>> shared_contexts_;
};

// FidlRequestPool: pool of FidlRequests.
template <typename RequestType>
class FidlRequestPool {
 public:
  // Add: called when adding a new request to the pool.
  void Add(RequestType&& request) __TA_EXCLUDES(mutex_) {
    std::scoped_lock _(mutex_);
    size_++;
    PutLocked(std::move(request));
  }

  // Remove: called when removing a request from the pool.
  std::optional<RequestType> Remove() __TA_EXCLUDES(mutex_) {
    std::scoped_lock _(mutex_);
    auto req = GetLocked();
    if (req.has_value()) {
      size_--;
    }
    return req;
  }

  std::optional<RequestType> Get() __TA_EXCLUDES(mutex_) {
    std::scoped_lock _(mutex_);
    return GetLocked();
  }

  // Put: called when a request (originally obtained from `get`) is returned to the pool.
  void Put(RequestType&& request) __TA_EXCLUDES(mutex_) {
    std::scoped_lock _(mutex_);
    PutLocked(std::move(request));
  }

  bool Full() __TA_EXCLUDES(mutex_) {
    std::scoped_lock _(mutex_);
    return free_reqs_.size() == size_;
  }

  bool Empty() __TA_EXCLUDES(mutex_) {
    std::scoped_lock _(mutex_);
    return free_reqs_.empty();
  }

  size_t GetInFlightCount() __TA_EXCLUDES(mutex_) {
    std::scoped_lock _(mutex_);
    return size_ - free_reqs_.size();
  }

  size_t GetTotalCount() __TA_EXCLUDES(mutex_) {
    std::scoped_lock _(mutex_);
    return size_;
  }

  std::pair<size_t, size_t> GetPoolCounts() __TA_EXCLUDES(mutex_) {
    std::scoped_lock _(mutex_);
    return {size_ - free_reqs_.size(), size_};
  }

 private:
  std::optional<RequestType> GetLocked() __TA_REQUIRES(mutex_) {
    if (free_reqs_.empty()) {
      return std::nullopt;
    }

    auto req = std::move(free_reqs_.front());
    free_reqs_.pop();
    return std::move(req);
  }

  void PutLocked(RequestType&& request) __TA_REQUIRES(mutex_) {
    free_reqs_.emplace(std::move(request));
    ZX_DEBUG_ASSERT(free_reqs_.size() <= size_);
  }

  std::mutex mutex_;
  std::queue<RequestType> free_reqs_ __TA_GUARDED(mutex_);
  uint32_t size_ __TA_GUARDED(mutex_) = 0;
};

}  // namespace usb::internal

#endif  // SRC_DEVICES_USB_LIB_USB_INCLUDE_USB_INTERNAL_REQUEST_FIDL_H_
