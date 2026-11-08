// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/lib/usb-endpoint/include/usb-endpoint/usb-endpoint-server.h"

#include <lib/fit/defer.h>
#include <lib/io-buffer/phys-iter.h>

#include <mutex>
#include <variant>

namespace usb {
namespace fendpoint = fuchsia_hardware_usb_endpoint;
namespace frequest = fuchsia_hardware_usb_request;

namespace {

static const size_t kPageSize = zx_system_get_page_size();

io_buffer::PhysIter phys_iter(uint64_t* phys_list, size_t phys_count, zx_off_t length,
                              zx_off_t vmo_offset, size_t max_length) {
  static_assert(sizeof(phys_iter_sg_entry_t) == sizeof(sg_entry_t) &&
                offsetof(phys_iter_sg_entry_t, length) == offsetof(sg_entry_t, length) &&
                offsetof(phys_iter_sg_entry_t, offset) == offsetof(sg_entry_t, offset));
  const size_t page_idx = vmo_offset / kPageSize;
  const size_t remaining_count = phys_count - page_idx;
  const zx_off_t sub_offset = vmo_offset & (kPageSize - 1);
  phys_iter_buffer_t buf = {
      .phys = phys_list + page_idx,
      .phys_count = remaining_count,
      .length = length,
      .vmo_offset = sub_offset,
      .sg_list = nullptr,
      .sg_count = 0,
  };
  return io_buffer::PhysIter(buf, max_length);
}

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
      zxlogf(ERROR, "Error sending event: %s", status.error_value().status_string());
    }
    return;
  }

  auto iter = completions.begin();
  while (iter != completions.end()) {
    size_t chunk_size =
        std::min(static_cast<size_t>(std::distance(iter, completions.end())), kMaxBatchSize);
    std::vector<fendpoint::Completion> batch(std::make_move_iterator(iter),
                                             std::make_move_iterator(iter + chunk_size));
    iter += chunk_size;
    auto status = fidl::SendEvent(target)->OnCompletion(std::move(batch));
    if (status.is_error()) {
      zxlogf(ERROR, "Error sending event: %s", status.error_value().status_string());
      break;
    }
  }
}

}  // namespace

zx::result<std::vector<io_buffer::PhysIter>> EndpointServer::get_iter(RequestVariant& req,
                                                                      size_t max_length) const {
  std::vector<io_buffer::PhysIter> iters;
  if (std::holds_alternative<usb::BorrowedRequest<void>>(req)) {
    auto& borrowed = std::get<usb::BorrowedRequest<void>>(req);
    auto iter = borrowed.phys_iter(max_length);
    if (sg_support_ == ScatterGatherSupport::kUnsupported &&
        borrowed.request()->header.length > 0) {
      auto it = iter.begin();
      if (it == iter.end() || (*it).second < borrowed.request()->header.length) {
        return zx::error(ZX_ERR_NOT_SUPPORTED);
      }
    }
    iters.push_back(iter);
  } else {
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
          auto iter = phys_iter(registered_vmo->phys_list, registered_vmo->phys_count, req_size,
                                vmo_offset, max_length);
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
          zxlogf(ERROR, "Not supported buffer type");
          return zx::error(ZX_ERR_NOT_SUPPORTED);
      }
      i++;
    }
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
    zxlogf(INFO, "Client disconnected");
  } else {
    zxlogf(ERROR, "Server error: %s", info.ToError().status_string());
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
        zxlogf(ERROR, "VMO ID %lu already registered", id);
        continue;
      }

      zx::vmo vmo;
      auto status = zx::vmo::create(size, 0, &vmo);
      if (status != ZX_OK) {
        zxlogf(ERROR, "Failed to pin registered VMO %d", status);
        continue;
      }

      // Pin VMO. Abusing usb_request_physmap
      usb_request_t req = {
          .vmo_handle = vmo.get(),
          .size = size,
          .offset = 0,
          .pmt = ZX_HANDLE_INVALID,
          .phys_list = nullptr,
          .phys_count = 0,
      };
      status = usb_request_physmap(&req, bti_.get());
      if (status != ZX_OK) {
        zxlogf(ERROR, "Failed to pin registered VMO %d", status);
        continue;
      }

      zx_vaddr_t virt_addr = 0;
      const size_t kPageSize = zx_system_get_page_size();
      const size_t map_size = (size + kPageSize - 1) & ~(kPageSize - 1);
      if (map_size > 0) {
        status = zx::vmar::root_self()->map(ZX_VM_PERM_READ | ZX_VM_PERM_WRITE, 0, vmo, 0, map_size,
                                            &virt_addr);
        if (status != ZX_OK) {
          zxlogf(ERROR, "zx::vmar::map(): %s", zx_status_get_string(status));
          zx_pmt_unpin(req.pmt);
          free(req.phys_list);
          continue;
        }
      }

      // Save
      vmos.emplace_back(std::move(fendpoint::VmoHandle().id(id).vmo(std::move(vmo))));
      registered_vmos_.emplace(
          id, std::make_shared<RegisteredVmo>(req.pmt, req.phys_list, req.phys_count, size,
                                              virt_addr, map_size));
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
  free(phys_list);
}

void EndpointServer::UnregisterVmos(UnregisterVmosRequest& request,
                                    UnregisterVmosCompleter::Sync& completer) {
  std::vector<zx_status_t> errors;
  std::vector<uint64_t> failed_vmo_ids;
  std::vector<std::shared_ptr<RegisteredVmo>> vmos_to_unmap;
  {
    std::lock_guard<std::mutex> lock(lock_);
    for (const auto& id : request.vmo_ids()) {
      auto registered_vmo = registered_vmos_.extract(id);
      if (registered_vmo.empty()) {
        failed_vmo_ids.emplace_back(id);
        errors.emplace_back(ZX_ERR_NOT_FOUND);
        continue;
      }
      if (registered_vmo.mapped().use_count() > 1) {
        unregistered_in_flight_vmos_.emplace_back(id, registered_vmo.mapped());
      }
      vmos_to_unmap.push_back(std::move(registered_vmo.mapped()));
    }
  }
  vmos_to_unmap.clear();
  completer.Reply({std::move(failed_vmo_ids), std::move(errors)});
}

void EndpointServer::RequestComplete(zx_status_t status, size_t actual, RequestVariant request,
                                     std::optional<zx::eventpair> wake_lease) {
  if (std::holds_alternative<usb::BorrowedRequest<void>>(request)) {
    std::get<usb::BorrowedRequest<void>>(request).Complete(status, actual);
    return;
  }
  auto& req = std::get<usb::FidlRequest>(request);

  auto defer_completion = *req->defer_completion();
  auto fidl_req = req.take_request();

  std::vector<fendpoint::Completion> completions;
  std::optional<fidl::ServerBindingRef<fendpoint::Endpoint>> binding;

  {
    std::lock_guard<std::mutex> lock(lock_);
    completions_.emplace_back(std::move(fendpoint::Completion()
                                            .request(std::move(fidl_req))
                                            .status(status)
                                            .transfer_size(actual)
                                            .wake_lease(std::move(wake_lease))));
    if (defer_completion && status == ZX_OK) {
      return;
    }

    if (binding_ref_) {
      completions.swap(completions_);
      binding = *binding_ref_;
    }
  }

  if (binding) {
    SendCompletionsBatch(*binding, std::move(completions));
  }
}

EndpointServer::~EndpointServer() {
  std::lock_guard<std::mutex> lock(lock_);
  registered_vmos_.clear();
}

}  // namespace usb
