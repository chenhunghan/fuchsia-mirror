// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#include <fidl/fuchsia.hardware.pci/cpp/wire.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/driver/mmio/cpp/mmio-buffer.h>
#include <lib/fidl/cpp/wire/string_view.h>
#include <lib/pci/constants.h>
#include <lib/pci/hw.h>
#include <zircon/errors.h>
#include <zircon/fidl.h>
#include <zircon/status.h>
#include <zircon/types.h>

#include <algorithm>
#include <cstring>
#include <memory>
#include <utility>
#include <vector>

#include "src/devices/pci/drivers/pci/bus.h"

namespace pci {

static_assert(PciFidl::wire::kBaseConfigSize == pci::kBaseConfigSize);
static_assert(PciFidl::wire::kExtendedConfigSize == pci::kExtendedConfigSize);

void Bus::GetRootInfo(GetRootInfoCompleter::Sync& completer) {
  PciFidl::wire::RootInfo info = {
      .name = fidl::StringView::FromExternal(info_.name()),
      .start_bus_number = info_.start_bus_num(),
      .end_bus_number = info_.end_bus_num(),
      .segment_group = info_.segment_group(),
  };
  completer.Reply(info);
}

void Bus::GetDevices(GetDevicesRequestView request, GetDevicesCompleter::Sync& completer) {
  auto iter = std::make_unique<DeviceIterator>(this);
  DeviceIterator* ptr = iter.get();
  iterator_bindings_.AddBinding(dispatcher_, std::move(request->iterator), ptr,
                                [iter = std::move(iter)](fidl::UnbindInfo) {});
}

void Bus::DeviceIterator::GetNext(GetNextCompleter::Sync& completer) {
  if (bus_->NextDevices(&last_bdf_, completer)) {
    bus_->iterator_bindings_.RemoveBindings(this);
  }
}

bool Bus::NextDevices(std::optional<PciFidl::wire::Bdf>* last_bdf,
                      DeviceIterator::GetNextCompleter::Sync& completer) {
  // Snapshot the batch so devices_lock_ isn't held while reading config space, which would
  // stall legacy interrupt handling.
  std::vector<std::pair<fbl::RefPtr<pci::Device>, uint32_t>> batch;
  {
    fbl::AutoLock devices_lock(&devices_lock_);
    size_t msg_bytes = sizeof(fidl_message_header_t) + sizeof(fidl_vector_t);
    auto it = last_bdf->has_value() ? devices_.upper_bound(**last_bdf) : devices_.begin();
    for (; it != devices_.end(); ++it) {
      // Only an extended ECAM can reach offsets past 256. Proxied config access goes through
      // pciroot, which may alias or reject extended offsets.
      const bool has_ext_cfg = it->second->is_pcie() && ecam_.has_value() && is_extended_;
      const uint32_t cfg_size =
          has_ext_cfg ? PciFidl::wire::kExtendedConfigSize : PciFidl::wire::kBaseConfigSize;
      const size_t entry_bytes =
          FIDL_ALIGN(sizeof(PciFidl::wire::DeviceSnapshot)) + FIDL_ALIGN(cfg_size);
      if (msg_bytes + entry_bytes > ZX_CHANNEL_MAX_MSG_BYTES) {
        break;
      }
      msg_bytes += entry_bytes;
      batch.emplace_back(it->second, cfg_size);
      *last_bdf = it->first;
    }
  }

  fidl::Arena<> arena;
  fidl::VectorView<PciFidl::wire::DeviceSnapshot> devices(arena, batch.size());
  for (size_t dev_idx = 0; dev_idx < batch.size(); dev_idx++) {
    const auto& [device, cfg_size] = batch[dev_idx];
    fbl::AutoLock device_lock(device->dev_lock());
    auto& out = devices[dev_idx];
    out.bdf = device->config()->bdf();
    out.bar_sizes = {};
    for (size_t i = 0; i < device->bar_count(); i++) {
      if (const auto& bar = device->bars()[i]; bar.has_value()) {
        out.bar_sizes[i] = bar->size;
      }
    }

    fidl::VectorView<uint8_t> config(arena, cfg_size);
    for (uint32_t offset = 0; offset < cfg_size; offset += sizeof(uint32_t)) {
      uint32_t val = device->config()->Read<uint32_t>(offset);
      std::memcpy(&config[offset], &val, sizeof(val));
    }
    out.config = config;
  }

  completer.Reply(devices);
  return batch.empty();
}

void Bus::ReadBar(ReadBarRequestView request, ReadBarCompleter::Sync& completer) {
  const fuchsia_hardware_pci::wire::Bdf bdf = request->device;
  uint8_t bar_id = request->bar_id;

  // Hold a reference so devices_lock_ isn't held across the BAR mapping and read.
  fbl::RefPtr<pci::Device> device;
  {
    fbl::AutoLock devices_lock(&devices_lock_);
    auto it = devices_.find(bdf);
    if (it == devices_.end()) {
      fdf::debug("could not find device {:02x}:{:02x}.{:1x}", bdf.bus_id, bdf.device_id,
                 bdf.function_id);
      completer.ReplyError(ZX_ERR_NOT_FOUND);
      return;
    }
    device = it->second;
  }

  if (device->bar_count() <= bar_id) {
    fdf::debug("invalid BAR id {}", bar_id);
    completer.ReplyError(ZX_ERR_INVALID_ARGS);
    return;
  }

  fbl::AutoLock dev_lock(device->dev_lock());
  auto& bar = device->bars()[bar_id];
  if (!bar.has_value()) {
    fdf::debug("no BAR {} found for device", bar_id);
    completer.ReplyError(ZX_ERR_NOT_FOUND);
    return;
  }

  // The subtraction can't wrap because offset <= bar->size.
  if (request->size == 0 || request->offset > bar->size ||
      request->size > bar->size - request->offset) {
    completer.ReplyError(ZX_ERR_INVALID_ARGS);
    return;
  }

  // Only MMIO is supported.
  if (!bar->is_mmio) {
    completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
    return;
  }

  // Unallocated BARs have no backing address space.
  if (!bar->allocation) {
    fdf::error("BAR {} is not allocated", bar_id);
    completer.ReplyError(ZX_ERR_BAD_STATE);
    return;
  }

  // Reads to a BAR with memory decode disabled can raise an SError on some platforms.
  if (!config::Command.ReadFrom(device->config().get()).memory_space()) {
    fdf::error("memory decode is disabled for BAR {}", bar_id);
    completer.ReplyError(ZX_ERR_BAD_STATE);
    return;
  }

  auto result = bar->allocation->CreateVmo();
  if (result.is_error()) {
    fdf::error("failed to create VMO: {}", result);
    completer.ReplyError(result.error_value());
    return;
  }

  const size_t size = std::min<uint64_t>(request->size, PciFidl::wire::kReadbarMaxSize);

  zx::result<fdf::MmioBuffer> mmio = fdf::MmioBuffer::Create(
      request->offset, size, std::move(result.value()), ZX_CACHE_POLICY_UNCACHED_DEVICE);
  if (mmio.is_error()) {
    fdf::debug("failed to create MmioBuffer: {}", mmio);
    completer.ReplyError(mmio.status_value());
    return;
  }

  std::vector<uint8_t> buffer(size);
  mmio->ReadBuffer(0, buffer.data(), size);
  completer.ReplySuccess(::fidl::VectorView<uint8_t>::FromExternal(buffer));
}

}  // namespace pci
