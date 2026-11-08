// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async/dispatcher.h>
#include <lib/dma-buffer/phys-iter.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/fit/defer.h>
#include <zircon/assert.h>
#include <zircon/status.h>

#include <algorithm>
#include <memory>
#include <mutex>
#include <vector>

#include <usb/sdk/request-fidl.h>

#include "src/devices/usb/lib/usb-endpoint/include/usb-endpoint/sdk/usb-endpoint-server.h"

namespace usb {
namespace fendpoint = fuchsia_hardware_usb_endpoint;
namespace frequest = fuchsia_hardware_usb_request;

static const size_t kPageSize = zx_system_get_page_size();

zx::result<std::vector<dma_buffer::PhysIter>> EndpointServer::get_iter(RequestVariant& req,
                                                                       size_t max_length) const {
  std::vector<dma_buffer::PhysIter> iters;
  auto& fidl_request = std::get<usb::FidlRequest>(req);
  if (!fidl_request->data().has_value()) {
    return zx::ok(std::move(iters));
  }
  if (sg_support_ == ScatterGatherSupport::kUnsupported && fidl_request->data()->size() > 1) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }
  size_t i = 0;
  std::lock_guard<std::mutex> lock(lock_);
  for (const auto& d : *fidl_request->data()) {
    if (!d.buffer().has_value() || !d.size().has_value() || *d.size() == 0) {
      return zx::error(ZX_ERR_INVALID_ARGS);
    }
    switch (d.buffer()->Which()) {
      case fuchsia_hardware_usb_request::Buffer::Tag::kVmoId: {
        if (!d.buffer()->vmo_id().has_value()) {
          return zx::error(ZX_ERR_INVALID_ARGS);
        }
        auto it = registered_vmos_.find(d.buffer()->vmo_id().value());
        if (it == registered_vmos_.end()) {
          return zx::error(ZX_ERR_NOT_FOUND);
        }
        const auto& registered_vmo = it->second;
        const zx_off_t vmo_offset = d.offset().value_or(0);
        const uint64_t req_size = *d.size();
        if (vmo_offset >= registered_vmo->size || req_size > registered_vmo->size - vmo_offset) {
          return zx::error(ZX_ERR_OUT_OF_RANGE);
        }
        const size_t page_idx = vmo_offset / kPageSize;
        const size_t remaining_count = registered_vmo->phys_count - page_idx;
        const zx_off_t sub_offset = vmo_offset & (kPageSize - 1);
        auto iter = dma_buffer::PhysIter{registered_vmo->phys_list.get() + page_idx,
                                         remaining_count,
                                         kPageSize,
                                         sub_offset,
                                         req_size,
                                         max_length,
                                         /*merge=*/true};
        if (sg_support_ == ScatterGatherSupport::kUnsupported && req_size > 0) {
          auto it_p = iter.begin();
          if (it_p == iter.end() || (*it_p).second < req_size) {
            return zx::error(ZX_ERR_NOT_SUPPORTED);
          }
        }
        fidl_request.add_shared_context(registered_vmo);
        iters.push_back(iter);
        break;
      }
      case fuchsia_hardware_usb_request::Buffer::Tag::kData: {
        auto iter = fidl_request.phys_iter(i, max_length);
        if (sg_support_ == ScatterGatherSupport::kUnsupported && *d.size() > 0) {
          auto it_p = iter.begin();
          if (it_p == iter.end() || (*it_p).second < *d.size()) {
            return zx::error(ZX_ERR_NOT_SUPPORTED);
          }
        }
        iters.push_back(iter);
        break;
      }
      default:
        fdf::error("Not supported buffer type");
        return zx::error(ZX_ERR_NOT_SUPPORTED);
    }
    i++;
  }
  return zx::success(std::move(iters));
}

// NOLINTNEXTLINE(readability-convert-member-functions-to-static)
zx::result<std::optional<usb::MappedVmo>> EndpointServer::GetMapped(
    const fuchsia_hardware_usb_request::Buffer& buffer) const {
  std::scoped_lock lock(lock_);
  if (buffer.data().has_value()) {
    return zx::ok(std::nullopt);
  }
  if (buffer.vmo_id().has_value()) {
    auto it = registered_vmos_.find(buffer.vmo_id().value());
    if (it != registered_vmos_.end() && it->second->virt_addr != 0) {
      return zx::ok(usb::MappedVmo{it->second->virt_addr, it->second->size});
    }

    auto prune_it = unregistered_in_flight_vmos_.begin();
    while (prune_it != unregistered_in_flight_vmos_.end()) {
      if (prune_it->second.expired()) {
        prune_it = unregistered_in_flight_vmos_.erase(prune_it);
      } else {
        ++prune_it;
      }
    }
    for (auto rit = unregistered_in_flight_vmos_.rbegin();
         rit != unregistered_in_flight_vmos_.rend(); ++rit) {
      if (rit->first == buffer.vmo_id().value()) {
        if (auto locked = rit->second.lock()) {
          if (locked->virt_addr != 0) {
            return zx::ok(usb::MappedVmo{locked->virt_addr, locked->size});
          }
        }
      }
    }
    return zx::error(ZX_ERR_NOT_FOUND);
  }
  return zx::error(ZX_ERR_NOT_SUPPORTED);
}

void EndpointServer::Connect(async_dispatcher_t* dispatcher,
                             fidl::ServerEnd<fendpoint::Endpoint> server_end) {
  std::lock_guard<std::mutex> lock(lock_);
  binding_ref_.emplace(fidl::BindServer(dispatcher, std::move(server_end), this,
                                        std::mem_fn(&EndpointServer::OnUnbound)));
}

namespace {
// Maximum number of completions sent in a single FIDL event to avoid exceeding channel message
// size limits and reduce serialization latency.
constexpr size_t kMaxBatchSize = fendpoint::kMaxCompletionsPerEvent;

template <typename Target>
void SendCompletionsBatch(const Target& target, std::vector<fendpoint::Completion> completions) {
  if (completions.empty()) {
    return;
  }

  // Fast path: send directly without vector chunking allocations if within limit.
  if (completions.size() <= kMaxBatchSize) {
    auto status = fidl::SendEvent(target)->OnCompletion(std::move(completions));
    if (status.is_error()) {
      fdf::error("Error sending event: {}", status.error_value().status_string());
    }
    return;
  }

  auto iter = completions.begin();
  while (iter != completions.end()) {
    size_t chunk_size =
        std::min(static_cast<size_t>(std::distance(iter, completions.end())), kMaxBatchSize);
    std::vector<fendpoint::Completion> chunk(std::make_move_iterator(iter),
                                             std::make_move_iterator(iter + chunk_size));
    iter += chunk_size;

    auto status = fidl::SendEvent(target)->OnCompletion(std::move(chunk));
    if (status.is_error()) {
      fdf::error("Error sending event: {}", status.error_value().status_string());
      break;
    }
  }
}
}  // namespace

void EndpointServer::OnUnbound(fidl::UnbindInfo info,
                               fidl::ServerEnd<fendpoint::Endpoint> server_end) {
  std::vector<fendpoint::Completion> completions;
  std::map<frequest::VmoId, std::shared_ptr<RegisteredVmo>> registered_vmos;
  {
    std::lock_guard<std::mutex> lock(lock_);
    completions = std::move(completions_);
    for (const auto& [id, vmo] : registered_vmos_) {
      if (vmo.use_count() > 1) {
        unregistered_in_flight_vmos_.emplace_back(id, vmo);
      }
    }
    registered_vmos = std::move(registered_vmos_);
    binding_ref_.reset();
  }

  SendCompletionsBatch(server_end, std::move(completions));

  // Unregister VMOs (dropped when registered_vmos goes out of scope or when in-flight requests
  // complete).
  registered_vmos.clear();

  if (info.is_user_initiated()) {
    return;
  }

  if (info.is_peer_closed()) {
    fdf::info(
        "EndpointServer ep(0x{:x}) fuchsia.hardware.usb.endpoint.Endpoint client disconnected",
        ep_addr_);
  } else {
    fdf::error("Server error: {}", info.ToError().status_string());
  }
}

void EndpointServer::RegisterVmos(RegisterVmosRequest& request,
                                  RegisterVmosCompleter::Sync& completer) {
  std::vector<fendpoint::VmoHandle> vmos;
  {
    std::lock_guard<std::mutex> lock(lock_);
    for (const auto& info : request.vmo_ids()) {
      ZX_ASSERT(info.id());
      ZX_ASSERT(info.size());
      auto id = *info.id();
      auto size = *info.size();

      if (registered_vmos_.find(id) != registered_vmos_.end()) {
        fdf::error("VMO ID {} already registered", id);
        continue;
      }

      zx::vmo vmo;
      auto status = zx::vmo::create(size, 0, &vmo);
      if (status != ZX_OK) {
        fdf::error("Failed to pin registered VMO {}", zx_status_get_string(status));
        continue;
      }

      zx_handle_t pmt;
      size_t num_addrs = USB_ROUNDUP(size, kPageSize) / kPageSize;

      std::unique_ptr<zx_paddr_t[]> paddrs{new zx_paddr_t[num_addrs]};

      uint64_t vmo_size;
      vmo.get_size(&vmo_size);

      status = zx_bti_pin(bti_.get(), ZX_BTI_PERM_READ | ZX_BTI_PERM_WRITE, vmo.get(), 0, vmo_size,
                          paddrs.get(), num_addrs, &pmt);

      if (status != ZX_OK) {
        fdf::error("zx_bti_pin(): {}", zx_status_get_string(status));
        continue;
      }

      zx_vaddr_t virt_addr = 0;
      const size_t map_size = USB_ROUNDUP(size, kPageSize);
      if (map_size > 0) {
        status = zx::vmar::root_self()->map(ZX_VM_PERM_READ | ZX_VM_PERM_WRITE, 0, vmo, 0, map_size,
                                            &virt_addr);
        if (status != ZX_OK) {
          fdf::error("zx::vmar::map(): {}", zx_status_get_string(status));
          zx_pmt_unpin(pmt);
          continue;
        }
      }

      // Save
      vmos.emplace_back(std::move(fendpoint::VmoHandle().id(id).vmo(std::move(vmo))));
      registered_vmos_.emplace(
          id, std::make_shared<RegisteredVmo>(pmt, std::move(paddrs), num_addrs, size, virt_addr,
                                              map_size));
    }
  }

  completer.Reply({std::move(vmos)});
}

EndpointServer::RegisteredVmo::~RegisteredVmo() {
  if (virt_addr != 0 && mapped_size != 0) {
    zx_status_t status = zx::vmar::root_self()->unmap(virt_addr, mapped_size);
    ZX_DEBUG_ASSERT(status == ZX_OK);
  }
  if (pmt != ZX_HANDLE_INVALID) {
    zx_status_t status = zx_pmt_unpin(pmt);
    ZX_DEBUG_ASSERT(status == ZX_OK);
  }
}

void EndpointServer::UnregisterVmos(UnregisterVmosRequest& request,
                                    UnregisterVmosCompleter::Sync& completer) {
  std::vector<zx_status_t> errors;
  std::vector<uint64_t> failed_vmo_ids;
  std::vector<std::shared_ptr<RegisteredVmo>> vmos_to_unmap;
  {
    std::lock_guard<std::mutex> lock(lock_);
    for (const auto& id : request.vmo_ids()) {
      auto node = registered_vmos_.extract(id);
      if (node.empty()) {
        failed_vmo_ids.emplace_back(id);
        errors.emplace_back(ZX_ERR_NOT_FOUND);
        continue;
      }
      if (node.mapped().use_count() > 1) {
        unregistered_in_flight_vmos_.emplace_back(id, node.mapped());
      }
      vmos_to_unmap.push_back(std::move(node.mapped()));
    }
  }
  vmos_to_unmap.clear();
  completer.Reply({std::move(failed_vmo_ids), std::move(errors)});
}

void EndpointServer::RequestComplete(zx_status_t status, size_t actual, RequestVariant request,
                                     bool send_now) {
  auto& req = std::get<usb::FidlRequest>(request);

  auto defer_completion = *req->defer_completion();
  auto fidl_req = req.take_request();

  {
    std::lock_guard<std::mutex> lock(lock_);
    completions_.emplace_back(std::move(
        fendpoint::Completion().request(std::move(fidl_req)).status(status).transfer_size(actual)));
    if ((defer_completion && status == ZX_OK) || !send_now || !binding_ref_) {
      return;
    }
  }

  SendCompletions();
}

void EndpointServer::SendCompletions() {
  std::vector<fendpoint::Completion> completions;
  std::optional<fidl::ServerBindingRef<fendpoint::Endpoint>> binding;
  {
    std::lock_guard lock(lock_);
    if (!binding_ref_ || completions_.empty()) {
      return;
    }
    completions.swap(completions_);
    binding = *binding_ref_;
  }

  SendCompletionsBatch(*binding, std::move(completions));
}

std::vector<EndpointServer::VmoInfo> EndpointServer::GetRegisteredVmosInfo() const {
  std::lock_guard<std::mutex> lock(lock_);
  std::vector<VmoInfo> info;
  for (const auto& [id, vmo] : registered_vmos_) {
    info.push_back({id, vmo->size});
  }
  return info;
}

EndpointServer::~EndpointServer() {
  std::lock_guard<std::mutex> lock(lock_);
  registered_vmos_.clear();
}

}  // namespace usb
